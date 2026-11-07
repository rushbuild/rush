//! Bazel-style depset over T.
//!
//! A [`NestedSet`] is a directed acyclic graph of items: each node owns
//! a slice of "direct" items plus a slice of nested-set children
//! (transitive deps' sets). Sets are immutable after construction and
//! shared by `Arc`. Walking yields every reachable item, deduped on
//! Arc identity (so the same transitive set reached through two
//! different paths is visited once, not twice — eliminating the
//! O(N²) blow-up that flattening into per-target `Vec<T>`s causes
//! when many consumers share a popular dep).
//!
//! Memory is O(unique items + unique nested-set nodes); flatten is
//! O(unique items + unique edges) thanks to Arc-pointer dedup at walk
//! time.
//!
//! See `docs/memory-pressure-analysis.md` (patch set 2b) for why this
//! shape replaces the `HashMap<Label, Arc<Vec<Artifact>>>` from
//! patch set 2a.

use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::sync::Arc;

/// Fast build-local hasher for compact IDs and Arc pointer identities. These
/// values are trusted process-local data, so keyed SipHash adds no
/// collision-defense value and dominates short traversals.
#[derive(Default)]
pub(crate) struct PointerHasher(u64);

impl PointerHasher {
    #[inline]
    fn mix(mut value: u64) -> u64 {
        // SplitMix64 finalizer: aligned allocation addresses need a full
        // avalanche before hashbrown consumes their low bucket bits.
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

impl std::hash::Hasher for PointerHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        if bytes.len() == std::mem::size_of::<usize>() {
            let mut raw = [0u8; std::mem::size_of::<u64>()];
            raw[..bytes.len()].copy_from_slice(bytes);
            self.0 = Self::mix(u64::from_ne_bytes(raw));
            return;
        }
        for &byte in bytes {
            self.0 = (self.0.rotate_left(5) ^ u64::from(byte))
                .wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.0 = Self::mix(value as u64);
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.0 = Self::mix(u64::from(value));
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.0 = Self::mix(value);
    }
}

pub(crate) type FastHashSet<T> = HashSet<T, BuildHasherDefault<PointerHasher>>;

static NEXT_NESTED_SET_ID: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[derive(Clone, Copy)]
struct NestedSetNodePtr<T>(*const NestedSet<T>);

impl<T> std::ops::Deref for NestedSetNodePtr<T> {
    type Target = NestedSet<T>;

    fn deref(&self) -> &Self::Target {
        // SAFETY: NestedSetIter retains its root Arc for its entire lifetime.
        // Every pointer placed on the stack is reachable through that root's
        // immutable transitive Arc DAG, so the pointee remains alive until the
        // iterator (and therefore the stack) is dropped.
        unsafe { &*self.0 }
    }
}

#[derive(Debug)]
pub struct NestedSet<T> {
    direct: Box<[T]>,
    transitive: Box<[Arc<NestedSet<T>>]>,
    node_id: usize,
    min_descendant_id: usize,
}

impl<T> NestedSet<T> {
    /// An empty set — useful for rules that produce no artifacts and
    /// have no deps (alias-shaped targets, header-only proto stubs).
    pub fn empty() -> Arc<Self> {
        let node_id = NEXT_NESTED_SET_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Arc::new(Self {
            direct: Vec::new().into_boxed_slice(),
            transitive: Vec::new().into_boxed_slice(),
            node_id,
            min_descendant_id: node_id,
        })
    }

    /// Build a set from `direct` items plus `transitive` children sets.
    /// Children are stored by reference (Arc); no copying.
    pub fn new(direct: Vec<T>, transitive: Vec<Arc<NestedSet<T>>>) -> Arc<Self> {
        let node_id = NEXT_NESTED_SET_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let min_descendant_id = transitive
            .iter()
            .map(|child| child.min_descendant_id)
            .min()
            .unwrap_or(node_id);
        Arc::new(Self {
            direct: direct.into_boxed_slice(),
            transitive: transitive.into_boxed_slice(),
            node_id,
            min_descendant_id,
        })
    }

    /// Direct items only (slice into this node's own contributions).
    /// Cheap; no walk.
    pub fn direct(&self) -> &[T] {
        &self.direct
    }

    /// Direct children sets (slice). Each is shared via Arc.
    pub fn transitive(&self) -> &[Arc<NestedSet<T>>] {
        &self.transitive
    }

    /// #39 graph-RSS structural census: accumulate Arc-identity-deduped
    /// stats over the DAG rooted at `self`. `seen` carries `Arc` pointer
    /// addresses ACROSS multiple roots so shared sub-DAGs are counted
    /// once — this is the whole point: it distinguishes correct Arc
    /// sharing (unique_nodes ≪ naive total) from a flatten bug (huge
    /// `direct` slices, `max_direct_len` growing with target size).
    /// Element bytes are NOT summed here (T is generic); callers
    /// multiply `sum_unique_direct` by `size_of::<T>()`.
    pub fn accumulate_stats(
        self: &Arc<Self>,
        seen: &mut std::collections::HashSet<usize>,
        stats: &mut NestedSetStats,
    ) {
        let key = Arc::as_ptr(self) as usize;
        if !seen.insert(key) {
            return; // already counted this shared node
        }
        stats.unique_nodes += 1;
        let d = self.direct.len();
        stats.sum_unique_direct += d;
        if d > stats.max_direct_len {
            stats.max_direct_len = d;
        }
        stats.transitive_edges += self.transitive.len();
        for child in self.transitive.iter() {
            child.accumulate_stats(seen, stats);
        }
    }
}

/// #39 graph-RSS structural census result for one nested-set field,
/// accumulated across every provider in `built_providers`. The
/// decisive comparison (per Codex 019e8ee5): if `sum_unique_direct`
/// stays small and `max_direct_len` does not grow with target size,
/// Arc sharing is working and CcInfo is NOT the 82 GB hog; if
/// `max_direct_len` is large/growing, a merge path is flattening
/// dependency contributions into per-consumer `direct` slices.
#[derive(Default, Debug, Clone)]
pub struct NestedSetStats {
    /// Distinct nested-set nodes reachable across all roots (Arc-ptr deduped).
    pub unique_nodes: usize,
    /// Sum of `direct().len()` over the distinct nodes (the real retained
    /// element storage, counting each shared node once).
    pub sum_unique_direct: usize,
    /// Largest single `direct()` slice seen — a flatten-bug smell when large.
    pub max_direct_len: usize,
    /// Total transitive Arc edges over distinct nodes (DAG fan-out).
    pub transitive_edges: usize,
}

impl<T: Clone> NestedSet<T> {
    /// Walk every item reachable from `self`, yielding each `T` (by
    /// cloning out of the boxed slice). Nested-set nodes are visited
    /// once each — Arc-pointer dedup catches diamond/repeated edges.
    /// Order is preorder DFS: a node's direct items, then each
    /// transitive child in declaration order.
    pub fn iter(self: &Arc<Self>) -> NestedSetIter<T> {
        NestedSetIter::new(Arc::clone(self))
    }

    /// Materialize all reachable items into a Vec. Use sparingly —
    /// link/aggregation only. The whole point of NestedSet is to
    /// avoid this.
    pub fn flatten(self: &Arc<Self>) -> Vec<T> {
        self.iter().collect()
    }

    /// Visit every item once across MANY roots, borrowing rather than cloning.
    ///
    /// `iter()`/`flatten()` dedup nodes by identity within ONE traversal, so
    /// walking N roots re-walks every shared subtree N times and clones each
    /// item it yields. The post-build ledger recapture does exactly that over
    /// ~5.5k roots whose closures overlap almost completely: it produced
    /// 1.19M artifact clones for 5.7k distinct paths on the Modular corpus,
    /// then sorted and deduped 1.19M entries to get there.
    ///
    /// One `seen_nodes` set shared across every root makes the whole forest
    /// cost O(distinct nodes) instead of O(sum of closures), and `f` receives
    /// a reference so callers clone only what they keep.
    ///
    /// Iterative on purpose: a deep DAG would otherwise recurse as deep as
    /// the dependency chain.
    pub fn visit_unique<F>(
        self: &Arc<Self>,
        seen_nodes: &mut std::collections::HashSet<usize>,
        mut f: F,
    ) where
        F: FnMut(&T),
    {
        let mut stack: Vec<Arc<NestedSet<T>>> = vec![Arc::clone(self)];
        while let Some(node) = stack.pop() {
            if !seen_nodes.insert(node.node_id) {
                continue;
            }
            for item in node.direct.iter() {
                f(item);
            }
            for child in node.transitive.iter() {
                stack.push(Arc::clone(child));
            }
        }
    }
}

/// Preorder DFS iterator with Arc-identity dedup.
pub struct NestedSetIter<T> {
    /// Stack of (node, position into its direct slice). Top of stack
    /// is the currently-yielding node.
    stack: Vec<(NestedSetNodePtr<T>, usize)>,
    /// Dense identity bitmap over the root's descendant ID range.
    seen_base: usize,
    seen_bits: Vec<u64>,
    /// Owns the complete immutable DAG addressed by `stack` and `seen`.
    _root: Arc<NestedSet<T>>,
}

impl<T: Clone> NestedSetIter<T> {
    fn new(root: Arc<NestedSet<T>>) -> Self {
        let initial_capacity = root.transitive.len().saturating_add(1);
        let seen_base = root.min_descendant_id;
        let seen_range = root.node_id - seen_base + 1;
        let root_ptr = NestedSetNodePtr(Arc::as_ptr(&root));
        let mut s = Self {
            stack: Vec::with_capacity(initial_capacity),
            seen_base,
            seen_bits: vec![0; (seen_range + 63) / 64],
            _root: root,
        };
        s.push_if_new(root_ptr);
        s
    }

    fn push_if_new(&mut self, node: NestedSetNodePtr<T>) {
        let bit = node.node_id - self.seen_base;
        let word = &mut self.seen_bits[bit / 64];
        let mask = 1_u64 << (bit & 63);
        if *word & mask == 0 {
            *word |= mask;
            self.stack.push((node, 0));
        }
    }
}

impl<T: Clone> Iterator for NestedSetIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        loop {
            // Peek the top frame.
            let (node, idx) = match self.stack.last_mut() {
                Some(frame) => frame,
                None => return None,
            };

            // Yield next direct item if any.
            if *idx < node.direct.len() {
                let item = node.direct[*idx].clone();
                *idx += 1;
                return Some(item);
            }

            // Direct exhausted — pop, then push each transitive child
            // (skipping already-seen ones).
            let popped = self.stack.pop().expect("stack non-empty above");
            // We need to drop the mutable borrow before pushing. The root Arc
            // retained by the iterator keeps every child pointer alive.
            // Push in REVERSE so that the first declared child ends up on
            // top of the stack and is visited first — preorder DFS in
            // declaration order, which is what link order expects (a
            // dep's earlier-listed transitive deps should resolve before
            // later-listed ones, matching how Bazel orders the link line).
            for child in popped.0.transitive().iter().rev() {
                self.push_if_new(NestedSetNodePtr(Arc::as_ptr(child)));
            }
        }
    }
}

/// Order-preserving sibling of [`NestedSet`] for fields where
/// declaration order is correctness-bearing.
///
/// Same DAG shape as [`NestedSet`] (direct + transitive children,
/// `Arc`-identity dedup on nodes so a diamond visits a shared
/// child once). The difference is purely at the **type level**:
///
/// - `NestedSet<T>` is for fields where the consumer is free to
///   collect into a hash set and dedup by value (e.g. header
///   `ArtifactId`s — seeing the same header via two paths is the
///   same header; one entry is correct).
/// - `OrderedNestedSet<T>` is for fields where the consumer MUST
///   iterate in declaration order and MUST NOT blind-dedup by
///   value (e.g. include directories — `-Iearly` then `-Ilate`
///   has different header-search semantics from `-Ilate` then
///   `-Iearly`; link flags — repeated `-lfoo` inside a linker
///   group can be intentional).
///
/// Walk semantics are identical to `NestedSet`: preorder DFS in
/// declaration order, with `Arc`-identity dedup of nodes. The
/// type signals to readers and to call sites that they're
/// holding ordering-bearing data — the compiler doesn't enforce
/// non-dedup, but having a distinct type makes "I must not call
/// `.collect::<HashSet<_>>()` on this" obvious at the call site.
///
/// Implementation: thin wrapper around an `Arc<NestedSet<T>>`.
/// One extra Arc indirection compared to NestedSet, but zero
/// duplication of the DAG-walking logic and no `unsafe`.
///
/// See ChatGPT review §3 of `docs/item-5-typed-providers-plan.md`
/// for the original observation: a universal deduping
/// `NestedSet<PathBuf>` for libraries is a correctness bug
/// waiting for a larger target to expose it.
#[derive(Debug)]
pub struct OrderedNestedSet<T> {
    inner: Arc<NestedSet<T>>,
}

impl<T> OrderedNestedSet<T> {
    /// An empty ordered set.
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            inner: NestedSet::empty(),
        })
    }

    /// Build an ordered set from `direct` items plus `transitive`
    /// children. Each child must itself be an `OrderedNestedSet`.
    pub fn new(direct: Vec<T>, transitive: Vec<Arc<OrderedNestedSet<T>>>) -> Arc<Self> {
        let inner_transitive: Vec<Arc<NestedSet<T>>> =
            transitive.iter().map(|c| Arc::clone(&c.inner)).collect();
        Arc::new(Self {
            inner: NestedSet::new(direct, inner_transitive),
        })
    }

    /// Direct items (this node's own contributions). Cheap.
    pub fn direct(&self) -> &[T] {
        self.inner.direct()
    }

    /// The underlying `NestedSet` DAG node. Lets structural tooling
    /// (the #39 RUSH_MEM_CENSUS walker) reach `accumulate_stats` /
    /// `transitive()` without duplicating the DAG-walk logic here.
    pub fn inner(&self) -> &Arc<NestedSet<T>> {
        &self.inner
    }
}

impl<T: Clone> OrderedNestedSet<T> {
    /// Walk every reachable item in **declaration order** with
    /// `Arc`-identity dedup of nodes (no value dedup). The
    /// consumer is responsible for whatever flatten/collect
    /// strategy preserves their semantics.
    pub fn iter(self: &Arc<Self>) -> NestedSetIter<T> {
        self.inner.iter()
    }

    /// Materialize all reachable items into a `Vec`, preserving
    /// declaration order. Use sparingly — call once per target
    /// when constructing the compile/link command line, never
    /// per-source-file. See the v2 plan's "compile-command
    /// flattening warning" for why repeated `flatten()` per
    /// translation unit is the next O(N²) hazard.
    pub fn flatten(self: &Arc<Self>) -> Vec<T> {
        self.iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_set_yields_nothing() {
        let s: Arc<NestedSet<i32>> = NestedSet::empty();
        let v: Vec<i32> = s.iter().collect();
        assert!(v.is_empty());
    }

    #[test]
    fn direct_only() {
        let s = NestedSet::new(vec![1, 2, 3], vec![]);
        let v: Vec<i32> = s.iter().collect();
        assert_eq!(v, vec![1, 2, 3]);
    }

    #[test]
    fn transitive_walk_order() {
        let leaf_a = NestedSet::new(vec![1, 2], vec![]);
        let leaf_b = NestedSet::new(vec![3, 4], vec![]);
        let root = NestedSet::new(vec![10], vec![leaf_a, leaf_b]);
        let v: Vec<i32> = root.iter().collect();
        // Preorder: root direct first, then leaf_a, then leaf_b.
        assert_eq!(v, vec![10, 1, 2, 3, 4]);
    }

    #[test]
    fn diamond_dedup_on_arc_identity() {
        // Diamond: root → {left, right}, both → leaf. leaf must only
        // be visited once, so its direct items should appear once total
        // across the walk.
        let leaf = NestedSet::new(vec![100, 200], vec![]);
        let left = NestedSet::new(vec![1], vec![Arc::clone(&leaf)]);
        let right = NestedSet::new(vec![2], vec![Arc::clone(&leaf)]);
        let root = NestedSet::new(vec![0], vec![left, right]);
        let v: Vec<i32> = root.iter().collect();
        // Order: root.direct (0), left.direct (1), leaf.direct (100, 200),
        // then right.direct (2). leaf is NOT re-visited.
        assert_eq!(v, vec![0, 1, 100, 200, 2]);
    }

    // ---------------------------------------------------------------------
    // OrderedNestedSet: the type-level distinct sibling for fields where
    // order matters and value-dedup would be a correctness bug.

    #[test]
    fn ordered_empty_yields_nothing() {
        let s: Arc<OrderedNestedSet<i32>> = OrderedNestedSet::empty();
        let v: Vec<i32> = s.flatten();
        assert!(v.is_empty());
    }

    #[test]
    fn ordered_direct_only_preserves_order() {
        let s = OrderedNestedSet::new(vec![3, 1, 4, 1, 5, 9, 2, 6], vec![]);
        // Direct slice is intentionally not deduped (the 1 appears
        // twice). OrderedNestedSet is order-and-multiplicity faithful.
        assert_eq!(s.flatten(), vec![3, 1, 4, 1, 5, 9, 2, 6]);
    }

    #[test]
    fn ordered_transitive_walk_in_declaration_order() {
        // Shape mirrors NestedSet's `transitive_walk_order` test so
        // we can confirm the wrapper preserves the declaration-order
        // walk that NestedSet's iterator already implements.
        let leaf_a = OrderedNestedSet::new(vec![1, 2], vec![]);
        let leaf_b = OrderedNestedSet::new(vec![3, 4], vec![]);
        let root = OrderedNestedSet::new(vec![10], vec![leaf_a, leaf_b]);
        assert_eq!(root.flatten(), vec![10, 1, 2, 3, 4]);
    }

    #[test]
    fn ordered_diamond_dedups_nodes_not_values() {
        // The whole point of OrderedNestedSet vs NestedSet:
        // - shared NESTED-SET NODES are still deduped (Arc-identity),
        //   so diamonds don't blow up to O(N²);
        // - VALUES that happen to repeat across distinct nodes are
        //   NOT deduped — that's the consumer's call, and for
        //   include_dirs / link_flags it's correctness-bearing.
        //
        // Build:
        //   root         → [0]
        //     left       → [1, "shared_value"]   (shared_value = 99)
        //     right      → [2, "shared_value"]   (shared_value = 99)
        // Both `left` and `right` are distinct nodes but they each
        // CONTAIN the value 99 in their direct slice. A blindly-deduping
        // walk would yield 99 once; the order-preserving walk yields
        // it twice, once at the left position and once at the right.
        let left = OrderedNestedSet::new(vec![1, 99], vec![]);
        let right = OrderedNestedSet::new(vec![2, 99], vec![]);
        let root = OrderedNestedSet::new(vec![0], vec![left, right]);
        let v = root.flatten();
        assert_eq!(v, vec![0, 1, 99, 2, 99]);
        // And explicitly: the 99 appears TWICE, not once.
        assert_eq!(v.iter().filter(|&&x| x == 99).count(), 2);
    }

    #[test]
    fn ordered_diamond_dedups_arc_identity() {
        // Same diamond shape as NestedSet's test, but with
        // OrderedNestedSet. The shared `leaf` NODE is visited
        // exactly once; its values appear once across the walk
        // even though the node is reachable through both `left`
        // and `right`. This dedup is on Arc identity, NOT on the
        // values themselves — see `ordered_diamond_dedups_nodes_not_values`
        // for the value-non-dedup contract.
        let leaf = OrderedNestedSet::new(vec![100, 200], vec![]);
        let left = OrderedNestedSet::new(vec![1], vec![Arc::clone(&leaf)]);
        let right = OrderedNestedSet::new(vec![2], vec![Arc::clone(&leaf)]);
        let root = OrderedNestedSet::new(vec![0], vec![left, right]);
        assert_eq!(root.flatten(), vec![0, 1, 100, 200, 2]);
    }
}
