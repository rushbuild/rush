// Rush Build - Graph Pruning
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build graph pruning for eliminating unnecessary work.
//!
//! Analyzes the build graph and removes nodes that don't contribute to
//! requested targets, dramatically reducing work in large mono-repos.

use anyhow::Result;
use std::collections::{HashMap, HashSet, VecDeque};

/// Build graph node.
#[derive(Debug, Clone)]
pub struct GraphNode {
    /// Node identifier.
    pub id: String,

    /// Dependencies (nodes this depends on).
    pub dependencies: Vec<String>,

    /// Reverse dependencies (nodes that depend on this).
    pub rdeps: Vec<String>,

    /// Node type (for filtering).
    pub node_type: NodeType,

    /// Estimated cost (for prioritization).
    pub estimated_cost: u64,
}

/// Node type in build graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeType {
    /// Source file.
    Source,

    /// Generated file.
    Generated,

    /// Compilation action.
    Compile,

    /// Link action.
    Link,

    /// Test target.
    Test,

    /// Library target.
    Library,

    /// Binary target.
    Binary,
}

/// Build graph pruner.
pub struct GraphPruner {
    /// All nodes in the graph.
    nodes: HashMap<String, GraphNode>,

    /// Statistics.
    stats: PruneStats,
}

/// Pruning statistics.
#[derive(Debug, Clone, Default)]
pub struct PruneStats {
    /// Total nodes before pruning.
    pub nodes_before: usize,

    /// Nodes after pruning.
    pub nodes_after: usize,

    /// Nodes pruned.
    pub nodes_pruned: usize,

    /// Estimated time saved (ms).
    pub time_saved_ms: u64,
}

impl PruneStats {
    /// Get pruning rate.
    pub fn prune_rate(&self) -> f64 {
        if self.nodes_before == 0 {
            return 0.0;
        }
        (self.nodes_pruned as f64 / self.nodes_before as f64) * 100.0
    }
}

impl GraphPruner {
    /// Create a new graph pruner.
    pub fn new() -> Self {
        GraphPruner {
            nodes: HashMap::new(),
            stats: PruneStats::default(),
        }
    }

    /// Add a node to the graph.
    pub fn add_node(&mut self, node: GraphNode) {
        self.nodes.insert(node.id.clone(), node);
        self.stats.nodes_before += 1;
    }

    /// Compute reverse dependencies.
    pub fn compute_rdeps(&mut self) {
        let node_ids: Vec<_> = self.nodes.keys().cloned().collect();

        for node_id in &node_ids {
            if let Some(node) = self.nodes.get(node_id) {
                let deps = node.dependencies.clone();

                for dep in deps {
                    if let Some(dep_node) = self.nodes.get_mut(&dep) {
                        if !dep_node.rdeps.contains(node_id) {
                            dep_node.rdeps.push(node_id.clone());
                        }
                    }
                }
            }
        }
    }

    /// Prune graph to only include nodes needed for targets.
    pub fn prune_to_targets(&mut self, targets: &[String]) -> Result<HashSet<String>> {
        // Backward traversal from targets
        let mut needed = HashSet::new();
        let mut queue = VecDeque::new();

        // Start with requested targets
        for target in targets {
            if self.nodes.contains_key(target) {
                needed.insert(target.clone());
                queue.push_back(target.clone());
            }
        }

        // BFS backward through dependencies
        while let Some(node_id) = queue.pop_front() {
            if let Some(node) = self.nodes.get(&node_id) {
                for dep in &node.dependencies {
                    if !needed.contains(dep) {
                        needed.insert(dep.clone());
                        queue.push_back(dep.clone());
                    }
                }
            }
        }

        // Remove unneeded nodes
        let all_nodes: Vec<_> = self.nodes.keys().cloned().collect();
        for node_id in all_nodes {
            if !needed.contains(&node_id) {
                if let Some(node) = self.nodes.remove(&node_id) {
                    self.stats.nodes_pruned += 1;
                    self.stats.time_saved_ms += node.estimated_cost;
                }
            }
        }

        self.stats.nodes_after = self.nodes.len();

        Ok(needed)
    }

    /// Prune test nodes (when not running tests).
    pub fn prune_tests(&mut self) -> usize {
        let test_nodes: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.node_type == NodeType::Test)
            .map(|(id, _)| id.clone())
            .collect();

        let mut pruned = 0;
        for node_id in test_nodes {
            // Remove test and its dependencies that aren't used elsewhere
            if self.can_prune_node(&node_id) {
                self.nodes.remove(&node_id);
                pruned += 1;
            }
        }

        self.stats.nodes_pruned += pruned;
        pruned
    }

    /// Check if a node can be pruned safely.
    fn can_prune_node(&self, node_id: &str) -> bool {
        if let Some(node) = self.nodes.get(node_id) {
            // Can prune if no reverse dependencies
            return node.rdeps.is_empty();
        }
        false
    }

    /// Remove duplicate nodes (deduplication).
    pub fn deduplicate(&mut self) -> usize {
        let mut seen_hashes: HashMap<String, String> = HashMap::new();
        let mut to_remove = Vec::new();

        for (id, node) in &self.nodes {
            let hash = self.compute_node_hash(node);

            if let Some(existing_id) = seen_hashes.get(&hash) {
                // Duplicate found - mark for removal and redirect dependencies
                to_remove.push((id.clone(), existing_id.clone()));
            } else {
                seen_hashes.insert(hash, id.clone());
            }
        }

        let count = to_remove.len();

        // Remove duplicates and redirect dependencies
        for (duplicate_id, canonical_id) in to_remove {
            // Redirect all rdeps to canonical node
            if let Some(dup_node) = self.nodes.get(&duplicate_id) {
                let rdeps = dup_node.rdeps.clone();

                for rdep in rdeps {
                    if let Some(rdep_node) = self.nodes.get_mut(&rdep) {
                        // Replace dependency
                        for dep in &mut rdep_node.dependencies {
                            if dep == &duplicate_id {
                                *dep = canonical_id.clone();
                            }
                        }
                    }
                }
            }

            self.nodes.remove(&duplicate_id);
        }

        self.stats.nodes_pruned += count;
        count
    }

    /// Compute hash for node deduplication.
    fn compute_node_hash(&self, node: &GraphNode) -> String {
        use blake3::Hasher;

        let mut hasher = Hasher::new();
        hasher.update(node.id.as_bytes());

        let mut sorted_deps = node.dependencies.clone();
        sorted_deps.sort();
        for dep in sorted_deps {
            hasher.update(dep.as_bytes());
        }

        hasher.finalize().to_hex().to_string()
    }

    /// Get pruned graph (as node IDs).
    pub fn get_pruned_graph(&self) -> Vec<String> {
        self.nodes.keys().cloned().collect()
    }

    /// Get statistics.
    pub fn stats(&self) -> &PruneStats {
        &self.stats
    }

    /// Get node count.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Topological sort of pruned graph.
    pub fn topological_sort(&self) -> Result<Vec<String>> {
        let mut result = Vec::new();
        let mut visited = HashSet::new();
        let mut temp_marks = HashSet::new();

        for node_id in self.nodes.keys() {
            if !visited.contains(node_id) {
                self.visit_node(node_id, &mut visited, &mut temp_marks, &mut result)?;
            }
        }

        // NO reverse: visit_node already appends a node AFTER recursing into
        // its dependencies, so `result` is dependencies-first — which is the
        // order a build must run in. Reversing produced dependents-first,
        // i.e. "link before you compile".
        Ok(result)
    }

    /// DFS visit for topological sort.
    fn visit_node(
        &self,
        node_id: &str,
        visited: &mut HashSet<String>,
        temp_marks: &mut HashSet<String>,
        result: &mut Vec<String>,
    ) -> Result<()> {
        if temp_marks.contains(node_id) {
            anyhow::bail!("Cycle detected in build graph at node: {}", node_id);
        }

        if visited.contains(node_id) {
            return Ok(());
        }

        temp_marks.insert(node_id.to_string());

        if let Some(node) = self.nodes.get(node_id) {
            for dep in &node.dependencies {
                self.visit_node(dep, visited, temp_marks, result)?;
            }
        }

        temp_marks.remove(node_id);
        visited.insert(node_id.to_string());
        result.push(node_id.to_string());

        Ok(())
    }

    /// Identify critical path in graph.
    pub fn critical_path(&self) -> Vec<String> {
        let mut longest_paths: HashMap<String, u64> = HashMap::new();

        // Compute longest path to each node
        for node_id in self.nodes.keys() {
            self.compute_longest_path(node_id, &mut longest_paths);
        }

        // Find node with longest path
        let mut max_length = 0;
        let mut end_node = String::new();

        for (node_id, length) in &longest_paths {
            if *length > max_length {
                max_length = *length;
                end_node = node_id.clone();
            }
        }

        // Reconstruct path
        self.reconstruct_critical_path(&end_node, &longest_paths)
    }

    /// Compute longest path to a node.
    fn compute_longest_path(&self, node_id: &str, cache: &mut HashMap<String, u64>) -> u64 {
        if let Some(&cached) = cache.get(node_id) {
            return cached;
        }

        let node = match self.nodes.get(node_id) {
            Some(n) => n,
            None => return 0,
        };

        let mut max_dep_length = 0;
        for dep in &node.dependencies {
            let dep_length = self.compute_longest_path(dep, cache);
            max_dep_length = max_dep_length.max(dep_length);
        }

        let length = max_dep_length + node.estimated_cost;
        cache.insert(node_id.to_string(), length);

        length
    }

    /// Reconstruct critical path.
    fn reconstruct_critical_path(
        &self,
        node_id: &str,
        lengths: &HashMap<String, u64>,
    ) -> Vec<String> {
        let path = vec![node_id.to_string()];

        let node = match self.nodes.get(node_id) {
            Some(n) => n,
            None => return path,
        };

        let node_length = lengths.get(node_id).copied().unwrap_or(0);

        // Find dependency that contributes to critical path
        for dep in &node.dependencies {
            let dep_length = lengths.get(dep).copied().unwrap_or(0);
            let dep_node = match self.nodes.get(dep) {
                Some(n) => n,
                None => continue,
            };

            if dep_length + dep_node.estimated_cost == node_length - node.estimated_cost {
                // This dependency is on the critical path
                let mut dep_path = self.reconstruct_critical_path(dep, lengths);
                dep_path.extend(path);
                return dep_path;
            }
        }

        path
    }
}

impl Default for GraphPruner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_graph_pruning() {
        let mut pruner = GraphPruner::new();

        // Add nodes
        pruner.add_node(GraphNode {
            id: "a".to_string(),
            dependencies: vec![],
            rdeps: vec![],
            node_type: NodeType::Source,
            estimated_cost: 10,
        });

        pruner.add_node(GraphNode {
            id: "b".to_string(),
            dependencies: vec!["a".to_string()],
            rdeps: vec![],
            node_type: NodeType::Compile,
            estimated_cost: 100,
        });

        pruner.add_node(GraphNode {
            id: "c".to_string(),
            dependencies: vec![],
            rdeps: vec![],
            node_type: NodeType::Source,
            estimated_cost: 10,
        });

        assert_eq!(pruner.node_count(), 3);

        // Prune to only target 'b'
        let needed = pruner.prune_to_targets(&["b".to_string()]).unwrap();

        assert_eq!(needed.len(), 2); // b and a
        assert!(needed.contains("a"));
        assert!(needed.contains("b"));
        assert!(!needed.contains("c")); // c was pruned

        let stats = pruner.stats();
        assert_eq!(stats.nodes_pruned, 1);
    }

    #[test]
    fn test_topological_sort() {
        let mut pruner = GraphPruner::new();

        pruner.add_node(GraphNode {
            id: "a".to_string(),
            dependencies: vec![],
            rdeps: vec![],
            node_type: NodeType::Source,
            estimated_cost: 1,
        });

        pruner.add_node(GraphNode {
            id: "b".to_string(),
            dependencies: vec!["a".to_string()],
            rdeps: vec![],
            node_type: NodeType::Compile,
            estimated_cost: 1,
        });

        pruner.add_node(GraphNode {
            id: "c".to_string(),
            dependencies: vec!["b".to_string()],
            rdeps: vec![],
            node_type: NodeType::Link,
            estimated_cost: 1,
        });

        let sorted = pruner.topological_sort().unwrap();

        // a should come before b, b before c
        let a_pos = sorted.iter().position(|x| x == "a").unwrap();
        let b_pos = sorted.iter().position(|x| x == "b").unwrap();
        let c_pos = sorted.iter().position(|x| x == "c").unwrap();

        assert!(a_pos < b_pos);
        assert!(b_pos < c_pos);
    }
}
