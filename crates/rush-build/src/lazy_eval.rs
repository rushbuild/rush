// Rush Build - Lazy Evaluation Engine
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Lazy evaluation engine for avoiding unnecessary work.
//!
//! Only computes values when they are actually needed, dramatically reducing
//! wasted computation in large build graphs.

use anyhow::Result;
use dashmap::DashMap;
use parking_lot::RwLock;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;

/// Type alias for compute function in Lazy
type ComputeFn<K, V> = Arc<dyn Fn(&K) -> Result<V> + Send + Sync>;

/// Type alias for array compute function in LazyBuildGraph
type ArrayComputeFn<T> = Arc<dyn Fn(&[T]) -> Result<T> + Send + Sync>;

/// Type alias for memoization function
type MemoizeFn<In, Out> = Arc<dyn Fn(&In) -> Result<Out> + Send + Sync>;

/// Lazy value that computes on first access.
pub struct Lazy<K, V>
where
    K: Eq + Hash + Clone + Send + Sync,
    V: Clone + Send + Sync,
{
    /// Cached computed values.
    cache: Arc<DashMap<K, V>>,

    /// Computation function.
    compute_fn: ComputeFn<K, V>,

    /// Statistics.
    stats: Arc<RwLock<LazyStats>>,
}

/// Lazy evaluation statistics.
#[derive(Debug, Clone, Default)]
pub struct LazyStats {
    /// Cache hits.
    pub hits: usize,

    /// Cache misses (computed values).
    pub misses: usize,

    /// Total accesses.
    pub accesses: usize,
}

impl LazyStats {
    /// Get cache hit rate.
    pub fn hit_rate(&self) -> f64 {
        if self.accesses == 0 {
            return 0.0;
        }
        (self.hits as f64 / self.accesses as f64) * 100.0
    }
}

impl<K, V> Lazy<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + Debug,
    V: Clone + Send + Sync,
{
    /// Create a new lazy evaluator.
    pub fn new<F>(compute_fn: F) -> Self
    where
        F: Fn(&K) -> Result<V> + Send + Sync + 'static,
    {
        Lazy {
            cache: Arc::new(DashMap::new()),
            compute_fn: Arc::new(compute_fn),
            stats: Arc::new(RwLock::new(LazyStats::default())),
        }
    }

    /// Get or compute a value.
    pub fn get(&self, key: &K) -> Result<V> {
        {
            let mut stats = self.stats.write();
            stats.accesses += 1;
        }

        // Check cache first
        if let Some(value) = self.cache.get(key) {
            let mut stats = self.stats.write();
            stats.hits += 1;
            return Ok(value.clone());
        }

        // Compute value
        let value = (self.compute_fn)(key)?;

        // Cache it
        self.cache.insert(key.clone(), value.clone());

        {
            let mut stats = self.stats.write();
            stats.misses += 1;
        }

        Ok(value)
    }

    /// Check if value is cached.
    pub fn is_cached(&self, key: &K) -> bool {
        self.cache.contains_key(key)
    }

    /// Invalidate a cached value.
    pub fn invalidate(&self, key: &K) {
        self.cache.remove(key);
    }

    /// Clear all cached values.
    pub fn clear(&self) {
        self.cache.clear();
    }

    /// Get statistics.
    pub fn stats(&self) -> LazyStats {
        self.stats.read().clone()
    }

    /// Get number of cached values.
    pub fn cached_count(&self) -> usize {
        self.cache.len()
    }
}

/// Lazy build graph that only evaluates needed nodes.
pub struct LazyBuildGraph<T>
where
    T: Clone + Send + Sync,
{
    /// Node values.
    nodes: Arc<DashMap<String, T>>,

    /// Node dependencies.
    dependencies: Arc<DashMap<String, Vec<String>>>,

    /// Computation functions per node.
    compute_fns: Arc<DashMap<String, ArrayComputeFn<T>>>,

    /// Nodes that have been evaluated.
    evaluated: Arc<DashMap<String, bool>>,

    /// Statistics.
    stats: Arc<RwLock<LazyGraphStats>>,
}

/// Lazy graph statistics.
#[derive(Debug, Clone, Default)]
pub struct LazyGraphStats {
    /// Total nodes in graph.
    pub total_nodes: usize,

    /// Nodes actually evaluated.
    pub evaluated_nodes: usize,

    /// Nodes skipped due to lazy evaluation.
    pub skipped_nodes: usize,
}

impl LazyGraphStats {
    /// Get percentage of nodes that were skipped.
    pub fn skip_rate(&self) -> f64 {
        if self.total_nodes == 0 {
            return 0.0;
        }
        (self.skipped_nodes as f64 / self.total_nodes as f64) * 100.0
    }
}

impl<T> LazyBuildGraph<T>
where
    T: Clone + Send + Sync + Debug,
{
    /// Create a new lazy build graph.
    pub fn new() -> Self {
        LazyBuildGraph {
            nodes: Arc::new(DashMap::new()),
            dependencies: Arc::new(DashMap::new()),
            compute_fns: Arc::new(DashMap::new()),
            evaluated: Arc::new(DashMap::new()),
            stats: Arc::new(RwLock::new(LazyGraphStats::default())),
        }
    }

    /// Add a node with its computation function.
    pub fn add_node<F>(&self, name: String, deps: Vec<String>, compute_fn: F)
    where
        F: Fn(&[T]) -> Result<T> + Send + Sync + 'static,
    {
        self.dependencies.insert(name.clone(), deps);
        self.compute_fns.insert(name.clone(), Arc::new(compute_fn));

        let mut stats = self.stats.write();
        stats.total_nodes += 1;
    }

    /// Get a node's value, computing it lazily if needed.
    pub fn get(&self, name: &str) -> Result<T> {
        // Check if already evaluated
        if let Some(value) = self.nodes.get(name) {
            return Ok(value.clone());
        }

        // Mark as evaluated
        self.evaluated.insert(name.to_string(), true);

        // Get dependencies
        let deps = self
            .dependencies
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Node not found: {}", name))?
            .clone();

        // Recursively evaluate dependencies
        let mut dep_values = Vec::new();
        for dep in &deps {
            let dep_value = self.get(dep)?;
            dep_values.push(dep_value);
        }

        // Get compute function
        let compute_fn = self
            .compute_fns
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Compute function not found: {}", name))?
            .clone();

        // Compute value
        let value = compute_fn(&dep_values)?;

        // Cache it
        self.nodes.insert(name.to_string(), value.clone());

        {
            let mut stats = self.stats.write();
            stats.evaluated_nodes += 1;
        }

        Ok(value)
    }

    /// Check if a node has been evaluated.
    pub fn is_evaluated(&self, name: &str) -> bool {
        self.evaluated.contains_key(name)
    }

    /// Get statistics.
    pub fn stats(&self) -> LazyGraphStats {
        let mut stats = self.stats.read().clone();
        stats.skipped_nodes = stats.total_nodes.saturating_sub(stats.evaluated_nodes);
        stats
    }

    /// Clear all evaluations (but keep graph structure).
    pub fn reset(&self) {
        self.nodes.clear();
        self.evaluated.clear();

        let mut stats = self.stats.write();
        stats.evaluated_nodes = 0;
        stats.skipped_nodes = 0;
    }
}

impl<T> Default for LazyBuildGraph<T>
where
    T: Clone + Send + Sync + Debug,
{
    fn default() -> Self {
        Self::new()
    }
}

/// Memoization wrapper for expensive computations.
pub struct Memoize<In, Out>
where
    In: Eq + Hash + Clone + Send + Sync,
    Out: Clone + Send + Sync,
{
    cache: Arc<DashMap<In, Out>>,
    func: MemoizeFn<In, Out>,
}

impl<In, Out> Memoize<In, Out>
where
    In: Eq + Hash + Clone + Send + Sync,
    Out: Clone + Send + Sync,
{
    /// Create a new memoized function.
    pub fn new<F>(func: F) -> Self
    where
        F: Fn(&In) -> Result<Out> + Send + Sync + 'static,
    {
        Memoize {
            cache: Arc::new(DashMap::new()),
            func: Arc::new(func),
        }
    }

    /// Call the function, using cached result if available.
    pub fn call(&self, input: &In) -> Result<Out> {
        if let Some(output) = self.cache.get(input) {
            return Ok(output.clone());
        }

        let output = (self.func)(input)?;
        self.cache.insert(input.clone(), output.clone());

        Ok(output)
    }

    /// Clear the cache.
    pub fn clear(&self) {
        self.cache.clear();
    }

    /// Get cache size.
    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_lazy_evaluation() {
        let compute_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&compute_count);

        let lazy = Lazy::new(move |x: &i32| {
            count_clone.fetch_add(1, Ordering::SeqCst);
            Ok(x * 2)
        });

        // First access - should compute
        let result1 = lazy.get(&5).unwrap();
        assert_eq!(result1, 10);
        assert_eq!(compute_count.load(Ordering::SeqCst), 1);

        // Second access - should use cache
        let result2 = lazy.get(&5).unwrap();
        assert_eq!(result2, 10);
        assert_eq!(compute_count.load(Ordering::SeqCst), 1); // Still 1!

        let stats = lazy.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
    }

    #[test]
    fn test_lazy_build_graph() {
        let graph = LazyBuildGraph::<i32>::new();

        // Add leaf nodes
        graph.add_node("a".to_string(), vec![], |_deps| Ok(1));
        graph.add_node("b".to_string(), vec![], |_deps| Ok(2));

        // Add dependent node
        graph.add_node(
            "c".to_string(),
            vec!["a".to_string(), "b".to_string()],
            |deps| Ok(deps[0] + deps[1]),
        );

        // Only evaluate 'c' - should lazily evaluate 'a' and 'b'
        let result = graph.get("c").unwrap();
        assert_eq!(result, 3);

        let stats = graph.stats();
        assert_eq!(stats.total_nodes, 3);
        assert_eq!(stats.evaluated_nodes, 3); // All evaluated because we needed c
    }

    #[test]
    fn test_memoize() {
        let compute_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&compute_count);

        let memo = Memoize::new(move |x: &i32| {
            count_clone.fetch_add(1, Ordering::SeqCst);
            Ok(x * x)
        });

        assert_eq!(memo.call(&5).unwrap(), 25);
        assert_eq!(compute_count.load(Ordering::SeqCst), 1);

        // Second call - should use cache
        assert_eq!(memo.call(&5).unwrap(), 25);
        assert_eq!(compute_count.load(Ordering::SeqCst), 1);

        assert_eq!(memo.cache_size(), 1);
    }
}
