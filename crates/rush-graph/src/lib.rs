// Rush Graph - Incremental evaluation engine
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Incremental evaluation engine inspired by Bazel's Skyframe.
//!
//! This crate provides the core dependency graph evaluation with memoization,
//! change pruning, and parallel async execution via tokio.

#![warn(missing_docs)]

pub mod typed_graph;

use async_trait::async_trait;
use dashmap::DashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A key in the evaluation graph.
///
/// Keys uniquely identify nodes in the dependency graph. Each key has an
/// associated value type that will be computed by a GraphFunction.
pub trait GraphKey: Hash + Eq + Clone + Debug + Send + Sync + 'static {
    /// The value type associated with this key.
    type Value: GraphValue;
}

/// A value in the evaluation graph.
///
/// Values are the results of evaluating a GraphKey via a GraphFunction.
/// They must be cloneable for caching and thread-safe.
pub trait GraphValue: Clone + Debug + Send + Sync + 'static {}

/// The evaluation environment provided to GraphFunctions.
///
/// This allows functions to request dependencies (other GraphValues)
/// during their computation.
pub struct Environment {
    _private: (),
}

impl Environment {
    /// Request the value for a dependency key.
    ///
    /// If the dependency is not yet available, this will trigger its
    /// evaluation and return None, causing the current function to restart
    /// later once the dependency is ready.
    pub async fn get<K: GraphKey>(&self, _key: &K) -> Option<K::Value> {
        // TODO: Implement dependency resolution
        None
    }
}

/// A function that computes a value for a given key.
///
/// GraphFunctions are the computation logic that produces GraphValues from
/// GraphKeys. They can request dependencies via the Environment.
#[async_trait]
pub trait GraphFunction<K: GraphKey>: Send + Sync + 'static {
    /// Compute the value for the given key.
    ///
    /// May request dependencies via env.get(). Returns None if a dependency
    /// is not yet available (causing a restart later), or Some(value) if
    /// computation completes.
    async fn compute(&self, key: &K, env: &Environment) -> anyhow::Result<K::Value>;
}

/// Version number for tracking graph changes.
///
/// Each evaluation has a version number. Nodes track when they were last
/// evaluated and when their value last changed, enabling change pruning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64);

impl Version {
    /// Create a new version.
    pub fn new(v: u64) -> Self {
        Version(v)
    }

    /// Get the version number.
    pub fn get(&self) -> u64 {
        self.0
    }
}

/// A node in the evaluation graph.
///
/// Stores the computed value, dependencies, version information, and
/// dirty state for incremental evaluation.
struct _Node<V: GraphValue> {
    /// The computed value (None if not yet evaluated or dirty).
    value: Option<V>,

    /// Version when this node was last evaluated.
    eval_version: Version,

    /// Version when the value last changed.
    change_version: Version,

    /// Whether this node is dirty and needs re-evaluation.
    dirty: AtomicBool,
}

impl<V: GraphValue> _Node<V> {
    fn _new() -> Self {
        _Node {
            value: None,
            eval_version: Version::new(0),
            change_version: Version::new(0),
            dirty: AtomicBool::new(true),
        }
    }
}

/// Type-erased node storage key.
///
/// Since we need to store nodes of different types in the same map,
/// we use a type-erased key wrapper.
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
struct NodeKey {
    type_id: std::any::TypeId,
    key_bytes: Vec<u8>,
}

impl NodeKey {
    fn new<K: GraphKey>(key: &K) -> Self {
        // Simple serialization using Debug representation
        // In production, would use proper serialization
        let key_bytes = format!("{:?}", key).into_bytes();
        NodeKey {
            type_id: std::any::TypeId::of::<K>(),
            key_bytes,
        }
    }
}

/// Type-erased node storage.
struct AnyNode {
    _value: Option<Vec<u8>>, // Serialized value
    _eval_version: Version,
    _change_version: Version,
    dirty: AtomicBool,
}

/// The incremental evaluation graph.
///
/// This is the core of the Rush build system, providing Skyframe-like
/// incremental evaluation with memoization and change pruning.
pub struct EvaluationGraph {
    /// Current graph version (incremented on each evaluation).
    version: AtomicU64,

    /// Node storage (type-erased for heterogeneous keys).
    nodes: DashMap<NodeKey, AnyNode>,
}

impl EvaluationGraph {
    /// Create a new evaluation graph.
    pub fn new() -> Self {
        EvaluationGraph {
            version: AtomicU64::new(1),
            nodes: DashMap::new(),
        }
    }

    /// Get the current graph version.
    pub fn current_version(&self) -> Version {
        Version::new(self.version.load(Ordering::SeqCst))
    }

    /// Increment the graph version for a new evaluation cycle.
    pub fn next_version(&self) -> Version {
        Version::new(self.version.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// Mark a key as dirty (needs re-evaluation).
    pub fn invalidate<K: GraphKey>(&self, key: &K) {
        let node_key = NodeKey::new(key);
        if let Some(node) = self.nodes.get(&node_key) {
            node.dirty.store(true, Ordering::SeqCst);
        }
    }

    /// Get the number of nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

impl Default for EvaluationGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct TestKey(u32);
    #[allow(dead_code)]
    #[derive(Clone, Debug)]
    struct TestValue(String);

    impl GraphValue for TestValue {}
    impl GraphKey for TestKey {
        type Value = TestValue;
    }

    #[test]
    fn test_version_ordering() {
        assert!(Version::new(0) < Version::new(1));
        assert!(Version::new(1) > Version::new(0));
        assert_eq!(Version::new(5), Version::new(5));
    }

    #[test]
    fn test_graph_version_management() {
        let graph = EvaluationGraph::new();
        assert_eq!(graph.current_version(), Version::new(1));
        assert_eq!(graph.next_version(), Version::new(2));
        assert_eq!(graph.current_version(), Version::new(2));
        assert_eq!(graph.next_version(), Version::new(3));
        assert_eq!(graph.current_version(), Version::new(3));
    }
}
