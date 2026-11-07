// Rush verified-core — C++ test link-PLAN assembly model
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Pure model for the `link-plan-v1` verified scope.
//!
//! A `tf_cc_test` (and every `cc_test`/`cc_binary`) link line is assembled by
//! [`CcTestInstance::link_binary`] in `crates/rush-rules/src/rules_cc.rs`
//! (≈ line 3289-3403). After the object files, the dep libraries are
//! partitioned into two buckets and emitted in the precise order GNU `ld`
//! (single-pass) needs:
//!
//!   1. **whole-archive (alwayslink) libs FIRST**, each wrapped in a
//!      `-Wl,--whole-archive <lib> -Wl,--no-whole-archive` triple, so they
//!      define their shared symbols up front;
//!   2. **then the normal libs** inside a single `-Wl,--start-group` /
//!      `-Wl,--end-group` (emitted iff there is ≥1 normal lib), so a member
//!      referenced by an earlier archive on the line is still resolved;
//!   3. **then the pass-through flags** (`-lgtest`, `-labsl_*`, `-L…`, …) that
//!      the archives above reference, INCLUDING the tf_cc_test benchmark arm.
//!
//! The benchmark arm is the load-bearing detail: `tsl/platform:test_main`
//! (linked by every TF `cc_test`) unconditionally calls
//! `benchmark::RunSpecifiedBenchmarks`, but `-lbenchmark` reaches this link via
//! no dep CcInfo link_fragment — so when `test_main` is in the closure the
//! assembler appends `-lbenchmark -lpthread` to the pass-through region (ONLY
//! `-lbenchmark`, NEVER `-lbenchmark_main`, which would redefine `main()` and
//! collide with `test_main.o`). The ABSENCE of that arm was a real, latent
//! `-lbenchmark` link gap — this verified scope is the cage that catches it.
//!
//! This module extracts the I/O-free CORE of that assembly: given the object
//! files, the dep libs each tagged `(path, alwayslink)`, the pass-through
//! flags, and a `links_test_main` bit, produce the ordered link-arg sequence.
//! It is the pure twin the production `link_binary` should delegate to (see the
//! crate's wire-up note). The downstream `dedup_archive_link_flags`
//! (link-dedup-v1) collapses any path that this assembly lists twice — it is a
//! separate, already-verified kernel and is NOT re-modelled here.
//!
//! REAL-SEMANTICS NOTES (the spec follows the production code, not a naive
//! guess — a falsely-assumed invariant is documented, never asserted):
//!
//!   * **GROUP CONDITION**: production emits the `--start-group`/`--end-group`
//!     bracket iff there is AT LEAST ONE normal lib (`if !normal_libs
//!     .is_empty()`), NOT ≥2. This mirror uses `>= 1` EXACTLY.
//!   * **ALWAYSLINK-WINS**: a path advertised `alwayslink` by ANY dep is
//!     whole-archived and dropped from the normal bucket
//!     (`normal_libs.retain(|p| !whole_archive_libs.contains(p))`), so it is
//!     never double-listed even when another dep advertised the same path plain.
//!   * **FIRST-BUCKET-WINS de-dup**: within each bucket the first occurrence of
//!     a path wins (the `!contains` guards), so each distinct path appears at
//!     most once per bucket.
//!
//! ## Pearlite-vs-oracle split
//!
//! The Pearlite contract below (under `--features creusot`) carries the two
//! tractable, position-level invariants:
//!
//!   * **whole-archive-before-normal ordering** — every emitted
//!     `-Wl,--whole-archive` token precedes every `-Wl,--start-group` token
//!     (P3, the WA-region-before-normal-region edge); and
//!   * the [`is_whole_archive_open`] / [`is_group_start`] token predicates used
//!     to state it.
//!
//! The heavier set-level conjuncts — P2 alwayslink-wins / no-double-list, P5
//! per-path completeness, and the exact P4 group-iff-≥1 and P6 benchmark-iff
//! biconditionals — are oracle-only (`tests/link_plan_prop.rs`), the same split
//! as `link_dedup` / `topo_roots` / `nested_set_walk`.

#[allow(unused_imports)]
use crate::contract_shim::*;

/// `-Wl,--whole-archive` — opens a whole-archive (alwayslink) emission.
const WHOLE_ARCHIVE_OPEN: &str = "-Wl,--whole-archive";
/// `-Wl,--no-whole-archive` — closes a whole-archive emission.
const WHOLE_ARCHIVE_CLOSE: &str = "-Wl,--no-whole-archive";
/// `-Wl,--start-group` — opens the lazy normal-archive group.
const GROUP_START: &str = "-Wl,--start-group";
/// `-Wl,--end-group` — closes the lazy normal-archive group.
const GROUP_END: &str = "-Wl,--end-group";
/// The tf_cc_test benchmark arm: Google Benchmark, appended when `test_main` is
/// in the closure. ONLY `-lbenchmark` (never `-lbenchmark_main`).
const BENCHMARK: &str = "-lbenchmark";
/// The pthread companion of the benchmark arm.
const BENCH_PTHREAD: &str = "-lpthread";

/// A dependency library on the test's link line: an archive `path` and whether
/// it was advertised `alwayslink` (force-include via `--whole-archive`).
///
/// Mirrors the `(artifact_path, LinkerInput::alwayslink)` pair production reads
/// from each dep's `CcInfo` linker_inputs. The same `path` can appear more than
/// once across `deps` with DIFFERENT `alwayslink` flags (one dep advertises it
/// alwayslink, another plain) — the alwayslink-wins rule resolves that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepLib {
    /// The archive path (e.g. `bazel-out/k8/bin/libsparse_tensor.a`).
    pub path: String,
    /// Whether this dep advertised the archive as `alwayslink=1`.
    pub alwayslink: bool,
}

impl DepLib {
    /// Construct a dep lib.
    pub fn new(path: impl Into<String>, alwayslink: bool) -> Self {
        DepLib { path: path.into(), alwayslink }
    }
}

/// Whether `s` is the whole-archive open marker.
pub fn is_whole_archive_open(s: &str) -> bool {
    s == WHOLE_ARCHIVE_OPEN
}

/// Whether `s` is the normal-group start marker.
pub fn is_group_start(s: &str) -> bool {
    s == GROUP_START
}

/// Assemble the PURE link-argument sequence for a C++ test binary.
///
/// This is the I/O-free twin of the partition + retain + order + group +
/// benchmark-arm logic in `crates/rush-rules/src/rules_cc.rs`'s
/// `CcTestInstance::link_binary` (≈ line 3289-3403). Semantics, preserved
/// EXACTLY:
///
///   1. **objects** are emitted first, in order.
///   2. **bucket partition** (alwayslink-wins): a dep path is placed in the
///      whole-archive bucket on first sight if it is alwayslink in THAT dep,
///      else the normal bucket; both buckets keep first-occurrence order and
///      de-dup within themselves. A path that is alwayslink in ANY dep is then
///      removed from the normal bucket (`retain`), so it is never double-listed.
///   3. **whole-archive emission FIRST**: each whole-archive path as a
///      `-Wl,--whole-archive <path> -Wl,--no-whole-archive` triple, in order.
///   4. **normal group**: iff ≥1 normal lib, `-Wl,--start-group`, the normal
///      libs in order, `-Wl,--end-group`.
///   5. **pass-through region**: the caller's pass-through flags in order, then
///      — iff `links_test_main` and `-lbenchmark` is not already present —
///      `-lbenchmark -lpthread` appended (the tf_cc_test benchmark arm).
///
/// The returned vector is the link-arg TAIL after the objects; the production
/// path then appends `-o <output>` and runs [`dedup_archive_link_flags`]
/// (link-dedup-v1) over the whole line. Output args are intentionally NOT part
/// of this kernel (they carry no ordering subtlety).
#[cfg_attr(feature = "creusot",
    ensures(whole_archive_precedes_group(result@)))]
pub fn assemble_link_plan(
    objects: Vec<String>,
    deps: Vec<DepLib>,
    passthrough: Vec<String>,
    links_test_main: bool,
) -> Vec<String> {
    // --- bucket partition (mirrors the linker_inputs loop) ------------------
    let mut whole_archive_libs: Vec<String> = Vec::new();
    let mut normal_libs: Vec<String> = Vec::new();
    for dep in &deps {
        if dep.alwayslink {
            if !whole_archive_libs.iter().any(|p| p == &dep.path) {
                whole_archive_libs.push(dep.path.clone());
            }
        } else if !normal_libs.iter().any(|p| p == &dep.path) {
            normal_libs.push(dep.path.clone());
        }
    }

    // --- alwayslink-wins: drop any whole-archived path from the normal bucket
    // (`normal_libs.retain(|p| !whole_archive_libs.contains(p))`). A path
    // advertised alwayslink by one dep and plain by another is whole-archived
    // only — never double-listed.
    normal_libs.retain(|p| !whole_archive_libs.iter().any(|w| w == p));

    // --- emit -------------------------------------------------------------
    let mut args: Vec<String> = Vec::with_capacity(
        objects.len() + whole_archive_libs.len() * 3 + normal_libs.len() + passthrough.len() + 4,
    );

    // objects first
    for obj in &objects {
        args.push(obj.clone());
    }

    // whole-archive (alwayslink) libs FIRST, each wrapped in a triple.
    for lib in &whole_archive_libs {
        args.push(WHOLE_ARCHIVE_OPEN.to_string());
        args.push(lib.clone());
        args.push(WHOLE_ARCHIVE_CLOSE.to_string());
    }

    // normal libs in a --start-group/--end-group, iff ≥1 (the REAL condition).
    if !normal_libs.is_empty() {
        args.push(GROUP_START.to_string());
        for lib in &normal_libs {
            args.push(lib.clone());
        }
        args.push(GROUP_END.to_string());
    }

    // pass-through flags, then the tf_cc_test benchmark arm.
    let mut passthrough_flags = passthrough;
    if links_test_main && !passthrough_flags.iter().any(|f| f == BENCHMARK) {
        passthrough_flags.push(BENCHMARK.to_string());
        passthrough_flags.push(BENCH_PTHREAD.to_string());
    }
    for f in &passthrough_flags {
        args.push(f.clone());
    }

    proof_assert!(true);
    args
}

/// Logic-level statement that every whole-archive open marker precedes every
/// normal-group start marker in `out` — the WA-region-before-normal-region edge
/// of P3. Defined as a Creusot `logic` fn so the `ensures` on
/// [`assemble_link_plan`] can refer to it; erased entirely in the default build.
///
/// This is the tractable, position-level slice of the ordering property that
/// Pearlite can carry without a set-membership fixpoint: there is no index `i`
/// holding a `--whole-archive` open that sits at or after an index `j` holding a
/// `--start-group`. (Since this assembly emits at most one `--start-group`, this
/// pins all whole-archive triples before the entire normal group.) The full
/// three-phase order, the bucket-membership conjuncts (P1/P2), completeness
/// (P5), and the group/benchmark biconditionals (P4/P6) are the heavier
/// obligations left to the proptest oracle (`tests/link_plan_prop.rs`).
#[cfg(feature = "creusot")]
#[logic]
fn whole_archive_precedes_group(out: Seq<String>) -> bool {
    pearlite! {
        forall(|i: Int, j: Int|
            0 <= i && i < out.len() && 0 <= j && j < out.len()
                && out[i] == "-Wl,--whole-archive"@
                && out[j] == "-Wl,--start-group"@
                ==> i < j)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn d(path: &str, alwayslink: bool) -> DepLib {
        DepLib::new(path, alwayslink)
    }

    #[test]
    fn objects_only_no_deps() {
        let out = assemble_link_plan(v(&["a.o", "b.o"]), vec![], vec![], false);
        assert_eq!(out, v(&["a.o", "b.o"]));
    }

    #[test]
    fn whole_archive_first_then_group_then_pass() {
        let out = assemble_link_plan(
            v(&["t.o"]),
            vec![d("libforce.a", true), d("libnorm.a", false)],
            v(&["-lgtest"]),
            false,
        );
        assert_eq!(
            out,
            v(&[
                "t.o",
                "-Wl,--whole-archive",
                "libforce.a",
                "-Wl,--no-whole-archive",
                "-Wl,--start-group",
                "libnorm.a",
                "-Wl,--end-group",
                "-lgtest",
            ])
        );
    }

    #[test]
    fn group_present_for_one_normal_lib() {
        // The REAL condition is >= 1, not >= 2: a single normal lib still gets
        // the --start-group/--end-group bracket.
        let out = assemble_link_plan(vec![], vec![d("libnorm.a", false)], vec![], false);
        assert_eq!(
            out,
            v(&["-Wl,--start-group", "libnorm.a", "-Wl,--end-group"])
        );
    }

    #[test]
    fn no_group_when_no_normal_libs() {
        let out = assemble_link_plan(vec![], vec![d("libforce.a", true)], vec![], false);
        assert_eq!(
            out,
            v(&["-Wl,--whole-archive", "libforce.a", "-Wl,--no-whole-archive"])
        );
    }

    #[test]
    fn alwayslink_wins_no_double_list() {
        // Same path advertised alwayslink by one dep and plain by another: it is
        // whole-archived ONLY, never in the normal group (the real retain()).
        let out = assemble_link_plan(
            vec![],
            vec![d("libdual.a", true), d("libdual.a", false), d("libplain.a", false)],
            vec![],
            false,
        );
        assert_eq!(
            out,
            v(&[
                "-Wl,--whole-archive",
                "libdual.a",
                "-Wl,--no-whole-archive",
                "-Wl,--start-group",
                "libplain.a",
                "-Wl,--end-group",
            ])
        );
        // libdual.a appears exactly once.
        assert_eq!(out.iter().filter(|t| *t == "libdual.a").count(), 1);
    }

    #[test]
    fn plain_first_then_alwayslink_still_whole_archived() {
        // Order of advertisement does not matter: plain-first then alwayslink
        // still ends whole-archived only (retain runs after the partition).
        let out = assemble_link_plan(
            vec![],
            vec![d("libdual.a", false), d("libdual.a", true)],
            vec![],
            false,
        );
        assert_eq!(
            out,
            v(&["-Wl,--whole-archive", "libdual.a", "-Wl,--no-whole-archive"])
        );
    }

    #[test]
    fn test_main_appends_benchmark_arm() {
        let out = assemble_link_plan(
            v(&["test_main.o"]),
            vec![d("libnorm.a", false)],
            v(&["-lgtest"]),
            true,
        );
        // benchmark arm lands in the pass-through region, after the group.
        assert_eq!(
            out,
            v(&[
                "test_main.o",
                "-Wl,--start-group",
                "libnorm.a",
                "-Wl,--end-group",
                "-lgtest",
                "-lbenchmark",
                "-lpthread",
            ])
        );
        // Never -lbenchmark_main.
        assert!(!out.iter().any(|t| t == "-lbenchmark_main"));
    }

    #[test]
    fn no_benchmark_arm_without_test_main() {
        let out = assemble_link_plan(vec![], vec![], v(&["-lgtest"]), false);
        assert_eq!(out, v(&["-lgtest"]));
        assert!(!out.iter().any(|t| t == "-lbenchmark"));
    }

    #[test]
    fn benchmark_not_duplicated_when_already_present() {
        // If the pass-through already carries -lbenchmark, the arm is not added
        // again (mirrors the `!passthrough_flags.iter().any(...)` guard).
        let out = assemble_link_plan(vec![], vec![], v(&["-lbenchmark"]), true);
        assert_eq!(out.iter().filter(|t| *t == "-lbenchmark").count(), 1);
        // …and -lpthread is NOT appended in that case (production gates BOTH
        // pushes on the same guard).
        assert!(!out.iter().any(|t| t == "-lpthread"));
    }

    #[test]
    fn duplicate_paths_deduped_within_bucket() {
        let out = assemble_link_plan(
            vec![],
            vec![d("libn.a", false), d("libn.a", false), d("libw.a", true), d("libw.a", true)],
            vec![],
            false,
        );
        assert_eq!(
            out,
            v(&[
                "-Wl,--whole-archive",
                "libw.a",
                "-Wl,--no-whole-archive",
                "-Wl,--start-group",
                "libn.a",
                "-Wl,--end-group",
            ])
        );
    }

    #[test]
    fn whole_archive_open_precedes_group_start() {
        let out = assemble_link_plan(
            vec![],
            vec![d("libw.a", true), d("libn.a", false)],
            vec![],
            false,
        );
        let wa = out.iter().position(|t| is_whole_archive_open(t)).unwrap();
        let gs = out.iter().position(|t| is_group_start(t)).unwrap();
        assert!(wa < gs);
    }
}
