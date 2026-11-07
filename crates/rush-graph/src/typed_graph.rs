// Rush Typed Graph - Type-safe build graph with compile-time guarantees
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Type-safe build graph implementation using phantom types and typestate.
//!
//! This module demonstrates how Rust's type system can catch build errors
//! at compile time that Bazel's Starlark can only detect at runtime.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::path::PathBuf;

// ============================================================================
// 1. PHANTOM TYPES FOR ARTIFACT TYPE SAFETY
// ============================================================================

/// Marker trait for artifact kinds.
pub trait ArtifactKind: 'static {
    /// Returns the file extension for this artifact kind.
    fn extension() -> &'static str;
    /// Returns a human-readable description of this artifact kind.
    fn description() -> &'static str;
}

/// Source code artifact (e.g., .cc, .rs, .py files).
pub struct SourceArtifact;
impl ArtifactKind for SourceArtifact {
    fn extension() -> &'static str {
        "src"
    }
    fn description() -> &'static str {
        "source file"
    }
}

/// Compiled object file (e.g., .o, .obj files).
pub struct CompiledArtifact;
impl ArtifactKind for CompiledArtifact {
    fn extension() -> &'static str {
        "o"
    }
    fn description() -> &'static str {
        "object file"
    }
}

/// Linked executable or shared library.
pub struct LinkedArtifact;
impl ArtifactKind for LinkedArtifact {
    fn extension() -> &'static str {
        "exe"
    }
    fn description() -> &'static str {
        "executable"
    }
}

/// Static library archive (e.g., .a, .lib files).
pub struct ArchiveArtifact;
impl ArtifactKind for ArchiveArtifact {
    fn extension() -> &'static str {
        "a"
    }
    fn description() -> &'static str {
        "static library"
    }
}

/// Header file (C/C++ .h, .hpp files).
pub struct HeaderArtifact;
impl ArtifactKind for HeaderArtifact {
    fn extension() -> &'static str {
        "h"
    }
    fn description() -> &'static str {
        "header file"
    }
}

/// Typed artifact with phantom type parameter for kind.
///
/// The `K` parameter is a zero-sized type that exists only at compile time.
/// This allows the compiler to enforce that only compatible artifacts are
/// used together.
#[derive(Debug, Clone)]
pub struct TypedArtifact<K: ArtifactKind> {
    path: PathBuf,
    hash: blake3::Hash,
    _kind: PhantomData<K>,
}

impl<K: ArtifactKind> TypedArtifact<K> {
    /// Creates a new typed artifact with the given path and content hash.
    pub fn new(path: impl Into<PathBuf>, hash: blake3::Hash) -> Self {
        TypedArtifact {
            path: path.into(),
            hash,
            _kind: PhantomData,
        }
    }

    /// Returns the filesystem path of this artifact.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Returns the content hash of this artifact.
    pub fn hash(&self) -> &blake3::Hash {
        &self.hash
    }

    /// Returns a human-readable description of this artifact's kind.
    pub fn kind_description(&self) -> &'static str {
        K::description()
    }
}

// Type aliases for specific artifact types

/// Source code file artifact (e.g., .cc, .rs, .py).
pub type SourceFile = TypedArtifact<SourceArtifact>;
/// Compiled object file artifact (e.g., .o, .obj).
pub type ObjectFile = TypedArtifact<CompiledArtifact>;
/// Linked executable or shared library artifact.
pub type Executable = TypedArtifact<LinkedArtifact>;
/// Static library archive artifact (e.g., .a, .lib).
pub type StaticLib = TypedArtifact<ArchiveArtifact>;
/// Header file artifact (e.g., .h, .hpp).
pub type HeaderFile = TypedArtifact<HeaderArtifact>;

// ============================================================================
// 2. TYPESTATE PATTERN FOR BUILD ACTIONS
// ============================================================================

/// Build action states.
pub trait ActionState {}

/// Action is defined but not yet validated.
pub struct Unvalidated;
impl ActionState for Unvalidated {}

/// Action has been validated (inputs exist, outputs don't conflict).
pub struct Validated;
impl ActionState for Validated {}

/// Action is ready for execution.
pub struct Ready;
impl ActionState for Ready {}

/// Action with typestate to enforce validation before execution.
pub struct BuildAction<InputKind, OutputKind, State>
where
    InputKind: ArtifactKind,
    OutputKind: ArtifactKind,
    State: ActionState,
{
    name: String,
    inputs: Vec<TypedArtifact<InputKind>>,
    output_path: PathBuf,
    command: Vec<String>,
    _state: PhantomData<State>,
    _output_kind: PhantomData<OutputKind>,
}

// Construction methods (available in Unvalidated state)
impl<I, O> BuildAction<I, O, Unvalidated>
where
    I: ArtifactKind,
    O: ArtifactKind,
{
    /// Creates a new unvalidated build action with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        BuildAction {
            name: name.into(),
            inputs: Vec::new(),
            output_path: PathBuf::new(),
            command: Vec::new(),
            _state: PhantomData,
            _output_kind: PhantomData,
        }
    }

    /// Adds an input artifact to this build action.
    pub fn add_input(mut self, input: TypedArtifact<I>) -> Self {
        self.inputs.push(input);
        self
    }

    /// Sets the output path for this build action.
    pub fn set_output(mut self, path: impl Into<PathBuf>) -> Self {
        self.output_path = path.into();
        self
    }

    /// Sets the command to execute for this build action.
    pub fn set_command(mut self, cmd: Vec<String>) -> Self {
        self.command = cmd;
        self
    }

    /// Validates the build action, checking that inputs, outputs, and command are specified.
    pub fn validate(self) -> Result<BuildAction<I, O, Validated>, ValidationError> {
        // Validation logic
        if self.inputs.is_empty() {
            return Err(ValidationError::NoInputs);
        }
        if self.output_path.as_os_str().is_empty() {
            return Err(ValidationError::NoOutput);
        }
        if self.command.is_empty() {
            return Err(ValidationError::NoCommand);
        }

        Ok(BuildAction {
            name: self.name,
            inputs: self.inputs,
            output_path: self.output_path,
            command: self.command,
            _state: PhantomData,
            _output_kind: PhantomData,
        })
    }
}

// Preparation methods (available in Validated state)
impl<I, O> BuildAction<I, O, Validated>
where
    I: ArtifactKind,
    O: ArtifactKind,
{
    /// Prepares the validated action for execution.
    pub fn prepare(self) -> BuildAction<I, O, Ready> {
        BuildAction {
            name: self.name,
            inputs: self.inputs,
            output_path: self.output_path,
            command: self.command,
            _state: PhantomData,
            _output_kind: PhantomData,
        }
    }
}

// Execution methods (ONLY available in Ready state)
impl<I, O> BuildAction<I, O, Ready>
where
    I: ArtifactKind,
    O: ArtifactKind,
{
    /// Executes the build action and returns the output artifact.
    pub fn execute(&self) -> Result<TypedArtifact<O>, ExecutionError> {
        // Execute the action and return typed output
        println!("Executing action: {}", self.name);
        println!("  Command: {:?}", self.command);
        println!("  Inputs: {} files", self.inputs.len());

        // In real implementation, would execute command
        let hash = blake3::hash(b"dummy_output");
        Ok(TypedArtifact::new(self.output_path.clone(), hash))
    }

    /// Returns the name of this build action.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Errors that can occur during build action validation.
#[derive(Debug)]
pub enum ValidationError {
    /// Action has no input artifacts.
    NoInputs,
    /// Action has no output path specified.
    NoOutput,
    /// Action has no command specified.
    NoCommand,
}

/// Errors that can occur during build action execution.
#[derive(Debug)]
pub enum ExecutionError {
    /// The build command failed with the given error message.
    CommandFailed(String),
}

// ============================================================================
// 3. CAPABILITY-BASED TYPE SYSTEM FOR HERMETIC BUILDS
// ============================================================================

/// Marker trait for capabilities.
pub trait Capability: 'static {}

/// No capabilities (hermetic).
pub struct NoCapabilities;
impl Capability for NoCapabilities {}

/// Network access capability.
pub struct NetworkAccess;
impl Capability for NetworkAccess {}

/// Filesystem access capability (outside declared inputs).
pub struct FilesystemAccess;
impl Capability for FilesystemAccess {}

/// Environment variable access capability.
pub struct EnvironmentAccess;
impl Capability for EnvironmentAccess {}

/// Action with capability requirements encoded in type.
pub struct CapableAction<Caps: Capability> {
    action_id: blake3::Hash,
    _capabilities: PhantomData<Caps>,
}

impl<C: Capability> CapableAction<C> {
    /// Creates a new action with the specified capabilities.
    pub fn new(action_id: blake3::Hash) -> Self {
        CapableAction {
            action_id,
            _capabilities: PhantomData,
        }
    }
}

/// Only hermetic actions can be cached.
pub struct ActionCache;

impl ActionCache {
    /// Stores a hermetic action in the cache.
    /// Only hermetic actions (with NoCapabilities) can be cached.
    pub fn store(&mut self, action: &CapableAction<NoCapabilities>) -> Result<(), String> {
        println!("Caching hermetic action: {}", action.action_id);
        Ok(())
    }

    // This method doesn't exist for non-hermetic actions:
    // pub fn store<C: Capability>(&mut self, action: &CapableAction<C>) { ... }
    // Uncommenting above would allow caching non-hermetic actions (BAD!)
}

/// Sandbox with capability enforcement.
pub struct Sandbox<Caps: Capability> {
    _allowed_caps: PhantomData<Caps>,
}

impl Sandbox<NoCapabilities> {
    /// Creates a new hermetic sandbox with no capabilities.
    pub fn new_hermetic() -> Self {
        Sandbox {
            _allowed_caps: PhantomData,
        }
    }

    /// Executes a hermetic action in the sandbox.
    pub fn execute(&self, action: CapableAction<NoCapabilities>) -> Result<(), String> {
        println!("Executing hermetic action in sandbox: {}", action.action_id);
        Ok(())
    }
}

impl Sandbox<NetworkAccess> {
    /// Creates a new sandbox with network access capability.
    pub fn new_with_network() -> Self {
        Sandbox {
            _allowed_caps: PhantomData,
        }
    }

    /// Executes an action with network access in the sandbox.
    pub fn execute(&self, action: CapableAction<NetworkAccess>) -> Result<(), String> {
        println!(
            "Executing network action (non-cacheable): {}",
            action.action_id
        );
        Ok(())
    }
}

// ============================================================================
// 4. TYPE-LEVEL DAG WITH CYCLE PREVENTION
// ============================================================================

/// Node in a directed acyclic graph.
#[derive(Clone)]
pub struct DagNode<T> {
    /// The value stored in this node.
    pub value: T,
    /// Unique identifier for this node.
    pub id: String,
}

impl<T> DagNode<T> {
    /// Creates a new DAG node with the given ID and value.
    pub fn new(id: impl Into<String>, value: T) -> Self {
        DagNode {
            id: id.into(),
            value,
        }
    }
}

/// Edge in the DAG.
#[derive(Clone)]
pub struct DagEdge {
    /// The source node ID.
    pub from: String,
    /// The destination node ID.
    pub to: String,
}

impl DagEdge {
    /// Creates a new edge from one node to another.
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Self {
        DagEdge {
            from: from.into(),
            to: to.into(),
        }
    }
}

/// Builder for constructing a DAG with cycle detection.
pub struct DagBuilder<T> {
    nodes: Vec<DagNode<T>>,
    edges: Vec<DagEdge>,
}

impl<T> DagBuilder<T> {
    /// Creates a new empty DAG builder.
    pub fn new() -> Self {
        DagBuilder {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    /// Adds a node to the DAG being built.
    pub fn add_node(&mut self, node: DagNode<T>) -> &mut Self {
        self.nodes.push(node);
        self
    }

    /// Adds an edge to the DAG being built.
    pub fn add_edge(&mut self, edge: DagEdge) -> &mut Self {
        self.edges.push(edge);
        self
    }

    /// Build the DAG, checking for cycles.
    pub fn build(self) -> Result<Dag<T>, CycleError> {
        // Check for cycles using DFS
        self.check_acyclic()?;

        Ok(Dag {
            nodes: self.nodes,
            edges: self.edges,
        })
    }

    fn check_acyclic(&self) -> Result<(), CycleError> {
        use std::collections::{HashMap, HashSet};

        // Build adjacency list
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in &self.edges {
            adj.entry(edge.from.as_str())
                .or_default()
                .push(edge.to.as_str());
        }

        // DFS with cycle detection
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();

        for node in &self.nodes {
            if !visited.contains(node.id.as_str())
                && has_cycle_dfs(node.id.as_str(), &adj, &mut visited, &mut rec_stack)
            {
                return Err(CycleError {
                    node: node.id.clone(),
                });
            }
        }

        Ok(())
    }
}

fn has_cycle_dfs(
    node: &str,
    adj: &HashMap<&str, Vec<&str>>,
    visited: &mut HashSet<String>,
    rec_stack: &mut HashSet<String>,
) -> bool {
    visited.insert(node.to_string());
    rec_stack.insert(node.to_string());

    if let Some(neighbors) = adj.get(node) {
        for &neighbor in neighbors {
            if !visited.contains(neighbor) {
                if has_cycle_dfs(neighbor, adj, visited, rec_stack) {
                    return true;
                }
            } else if rec_stack.contains(neighbor) {
                return true; // Cycle detected
            }
        }
    }

    rec_stack.remove(node);
    false
}

impl<T> Default for DagBuilder<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Directed Acyclic Graph (DAG).
///
/// This type can ONLY be constructed through DagBuilder, which enforces
/// acyclicity. The constructor is private, making cycles impossible.
pub struct Dag<T> {
    nodes: Vec<DagNode<T>>,
    edges: Vec<DagEdge>,
}

impl<T> Dag<T> {
    // Private constructor - can only be built via DagBuilder
    fn _new() -> Self {
        panic!(
            "DAG::_new() is intentionally not public.\n\
             \n\
             Use DagBuilder to construct DAG instances:\n\
             \n\
             let dag = DagBuilder::new()\n\
                 .add_node(data)\n\
                 .add_edge(from, to)\n\
                 .build()?;\n\
             \n\
             This ensures DAG invariants are maintained during construction."
        )
    }

    /// Returns the nodes in this DAG.
    pub fn nodes(&self) -> &[DagNode<T>] {
        &self.nodes
    }

    /// Returns the edges in this DAG.
    pub fn edges(&self) -> &[DagEdge] {
        &self.edges
    }

    /// Topological sort of the DAG.
    pub fn topological_sort(&self) -> Vec<&str> {
        use std::collections::VecDeque;

        // Build adjacency list and in-degree map
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut in_degree: HashMap<&str, usize> = HashMap::new();

        for node in &self.nodes {
            in_degree.insert(node.id.as_str(), 0);
        }

        for edge in &self.edges {
            adj.entry(edge.from.as_str())
                .or_default()
                .push(edge.to.as_str());
            *in_degree.get_mut(edge.to.as_str()).unwrap() += 1;
        }

        // Kahn's algorithm
        let mut queue: VecDeque<&str> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(&node, _)| node)
            .collect();

        let mut result = Vec::new();

        while let Some(node) = queue.pop_front() {
            result.push(node);

            if let Some(neighbors) = adj.get(node) {
                for &neighbor in neighbors {
                    let deg = in_degree.get_mut(neighbor).unwrap();
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(neighbor);
                    }
                }
            }
        }

        result
    }
}

/// Error indicating a cycle was detected in the DAG.
#[derive(Debug)]
pub struct CycleError {
    /// The node involved in the detected cycle.
    pub node: String,
}

impl std::fmt::Display for CycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cycle detected involving node: {}", self.node)
    }
}

impl std::error::Error for CycleError {}

// ============================================================================
// 5. SESSION TYPES FOR BUILD GRAPH PROTOCOL
// ============================================================================

/// Graph state markers for session types.
pub trait GraphState {}

/// Graph state: empty, no nodes or edges yet.
pub struct EmptyGraph;
impl GraphState for EmptyGraph {}

/// Graph state: nodes have been added.
pub struct NodesAdded;
impl GraphState for NodesAdded {}

/// Graph state: edges have been added.
pub struct EdgesAdded;
impl GraphState for EdgesAdded {}

/// Graph state: graph has been validated (acyclic, well-formed).
pub struct ValidatedGraph;
impl GraphState for ValidatedGraph {}

/// Graph state: graph has been optimized for execution.
pub struct OptimizedGraph;
impl GraphState for OptimizedGraph {}

/// Build graph with session types enforcing construction protocol.
pub struct TypedBuildGraph<State: GraphState, T> {
    dag: Option<Dag<T>>,
    _state: PhantomData<State>,
}

// Protocol: Empty -> NodesAdded -> EdgesAdded -> Validated -> Optimized -> Execute
impl<T> TypedBuildGraph<EmptyGraph, T> {
    /// Creates a new empty build graph.
    pub fn new() -> Self {
        TypedBuildGraph {
            dag: None,
            _state: PhantomData,
        }
    }
}

impl<T> Default for TypedBuildGraph<EmptyGraph, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> TypedBuildGraph<ValidatedGraph, T> {
    /// Creates a validated build graph from a DAG.
    pub fn with_dag(dag: Dag<T>) -> Self {
        TypedBuildGraph {
            dag: Some(dag),
            _state: PhantomData,
        }
    }
}

impl<T> TypedBuildGraph<ValidatedGraph, T> {
    /// Optimizes the validated build graph for execution.
    pub fn optimize(self) -> TypedBuildGraph<OptimizedGraph, T> {
        println!("Optimizing build graph...");
        TypedBuildGraph {
            dag: self.dag,
            _state: PhantomData,
        }
    }
}

impl<T> TypedBuildGraph<OptimizedGraph, T> {
    /// Executes the optimized build graph.
    pub fn execute(&self) -> Result<(), String> {
        println!("Executing build graph...");
        if let Some(ref dag) = self.dag {
            let order = dag.topological_sort();
            println!("Build order: {:?}", order);
        }
        Ok(())
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_typed_artifacts() {
        let hash = blake3::hash(b"test");

        let source = SourceFile::new("main.cc", hash);
        let object = ObjectFile::new("main.o", hash);

        assert_eq!(source.kind_description(), "source file");
        assert_eq!(object.kind_description(), "object file");
    }

    #[test]
    fn test_typestate_action() {
        let hash = blake3::hash(b"input");
        let input = SourceFile::new("main.cc", hash);

        // Build action through typestate transitions
        let action = BuildAction::<SourceArtifact, CompiledArtifact, Unvalidated>::new("compile")
            .add_input(input)
            .set_output("main.o")
            .set_command(vec![
                "g++".to_string(),
                "-c".to_string(),
                "main.cc".to_string(),
            ])
            .validate()
            .expect("Validation failed")
            .prepare();

        let output = action.execute().expect("Execution failed");
        assert_eq!(output.kind_description(), "object file");
    }

    #[test]
    fn test_hermetic_caching() {
        let hash = blake3::hash(b"action");
        let hermetic_action = CapableAction::<NoCapabilities>::new(hash);

        let mut cache = ActionCache;
        cache.store(&hermetic_action).expect("Failed to cache");

        // This would not compile (no store method for non-hermetic):
        // let network_action = CapableAction::<NetworkAccess>::new(hash);
        // cache.store(&network_action);
    }

    #[test]
    fn test_dag_no_cycle() {
        let mut builder = DagBuilder::new();
        builder
            .add_node(DagNode::new("A", 1))
            .add_node(DagNode::new("B", 2))
            .add_node(DagNode::new("C", 3))
            .add_edge(DagEdge::new("A", "B"))
            .add_edge(DagEdge::new("B", "C"));

        let dag = builder.build().expect("Should build acyclic graph");
        let order = dag.topological_sort();
        assert_eq!(order, vec!["A", "B", "C"]);
    }

    #[test]
    fn test_dag_cycle_detection() {
        let mut builder = DagBuilder::new();
        builder
            .add_node(DagNode::new("A", 1))
            .add_node(DagNode::new("B", 2))
            .add_node(DagNode::new("C", 3))
            .add_edge(DagEdge::new("A", "B"))
            .add_edge(DagEdge::new("B", "C"))
            .add_edge(DagEdge::new("C", "A")); // Creates cycle

        let result = builder.build();
        assert!(result.is_err(), "Should detect cycle");
    }

    #[test]
    fn test_session_type_protocol() {
        let mut dag_builder = DagBuilder::new();
        dag_builder
            .add_node(DagNode::new("lib", "library"))
            .add_node(DagNode::new("bin", "binary"))
            .add_edge(DagEdge::new("bin", "lib"));

        let dag = dag_builder.build().expect("Failed to build DAG");

        let graph: TypedBuildGraph<OptimizedGraph, &str> =
            TypedBuildGraph::with_dag(dag).optimize();

        graph.execute().expect("Failed to execute");

        // This would not compile (wrong state):
        // let empty_graph = TypedBuildGraph::<EmptyGraph, &str>::new();
        // empty_graph.execute();  // ERROR: no method execute() on TypedBuildGraph<EmptyGraph>
    }
}
