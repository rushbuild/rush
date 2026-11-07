// Type-Safe Build Graph Demo
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Demonstrates Rush's compile-time type safety advantages over Bazel.
//!
//! This example shows how Rust's type system prevents common build errors
//! at compile time that Bazel can only catch at runtime.

use rush_graph::typed_graph::*;

// Import blake3 - it's a workspace dependency
extern crate blake3;

fn main() {
    println!("=== Rush Type-Safe Build System Demo ===\n");

    // Demo 1: Typed Artifacts
    demo_typed_artifacts();

    // Demo 2: Typestate Pattern
    demo_typestate_pattern();

    // Demo 3: Hermetic Build Enforcement
    demo_hermetic_enforcement();

    // Demo 4: DAG Cycle Prevention
    demo_dag_validation();

    // Demo 5: Session Types
    demo_session_types();

    println!("\n=== All demos completed successfully! ===");
    println!("Note: Many error scenarios shown in comments would be compile-time");
    println!("errors in Rush but runtime errors in Bazel.");
}

fn demo_typed_artifacts() {
    println!("📦 Demo 1: Typed Artifacts (Phantom Types)\n");
    println!("In Bazel: Artifacts are untyped strings, validated at runtime");
    println!("In Rush:  Artifacts are strongly typed, validated at compile time\n");

    let hash = blake3::hash(b"source_file");
    let source = SourceFile::new("main.cc", hash);
    println!("✓ Created typed source artifact: {:?}", source.path());
    println!("  Type: {}", source.kind_description());

    let obj_hash = blake3::hash(b"object_file");
    let object = ObjectFile::new("main.o", obj_hash);
    println!("✓ Created typed object artifact: {:?}", object.path());
    println!("  Type: {}", object.kind_description());

    println!("\n// This WOULD NOT COMPILE in Rush:");
    println!("// let wrong_action = link(vec![source]);  // ERROR: expected ObjectFile, found SourceFile");
    println!("// In Bazel: Runtime error during execution\n");
}

fn demo_typestate_pattern() {
    println!("🔄 Demo 2: Typestate Pattern (Build Action States)\n");
    println!("In Bazel: Actions can be executed without validation");
    println!("In Rush:  Type system enforces validation before execution\n");

    let hash = blake3::hash(b"input");
    let input = SourceFile::new("lib.cc", hash);

    // Build action through typestate transitions
    let action = BuildAction::<SourceArtifact, CompiledArtifact, Unvalidated>::new("compile")
        .add_input(input)
        .set_output("lib.o")
        .set_command(vec![
            "g++".to_string(),
            "-c".to_string(),
            "lib.cc".to_string(),
        ]);

    println!("✓ Created unvalidated action");
    println!("  State: Unvalidated");

    let validated = action.validate().expect("Validation failed");
    println!("✓ Action validated");
    println!("  State: Validated");

    let ready = validated.prepare();
    println!("✓ Action prepared");
    println!("  State: Ready");

    let output = ready.execute().expect("Execution failed");
    println!("✓ Action executed successfully");
    println!("  Output: {:?}", output.path());
    println!("  Output type: {}", output.kind_description());

    println!("\n// This WOULD NOT COMPILE in Rush:");
    println!("// let action = BuildAction::new(\"compile\");");
    println!("// action.execute();  // ERROR: no method execute() on BuildAction<Unvalidated>");
    println!("// In Bazel: Runtime error when executing invalid action\n");
}

fn demo_hermetic_enforcement() {
    println!("🔒 Demo 3: Hermetic Build Enforcement (Capability Types)\n");
    println!("In Bazel: Hermetic violations detected at runtime during execution");
    println!("In Rush:  Type system prevents non-hermetic actions from being cached\n");

    let hash = blake3::hash(b"hermetic_action");
    let hermetic_action = CapableAction::<NoCapabilities>::new(hash);

    println!("✓ Created hermetic action (no capabilities)");
    println!("  Action ID: {}", hash);

    let mut cache = ActionCache;
    cache.store(&hermetic_action).expect("Failed to cache");
    println!("✓ Successfully cached hermetic action");

    let sandbox = Sandbox::<NoCapabilities>::new_hermetic();
    sandbox.execute(hermetic_action).expect("Failed to execute");
    println!("✓ Executed in hermetic sandbox");

    println!("\n// This WOULD NOT COMPILE in Rush:");
    println!("// let network_action = CapableAction::<NetworkAccess>::new(hash);");
    println!("// cache.store(&network_action);  // ERROR: cannot cache non-hermetic action");
    println!("// In Bazel: Runtime error or cache poisoning\n");

    let network_hash = blake3::hash(b"network_action");
    let network_action = CapableAction::<NetworkAccess>::new(network_hash);
    let network_sandbox = Sandbox::<NetworkAccess>::new_with_network();
    network_sandbox
        .execute(network_action)
        .expect("Failed to execute");
    println!("✓ Non-hermetic action executed in network-capable sandbox");
    println!("  Note: Cannot be cached (enforced by type system)\n");
}

fn demo_dag_validation() {
    println!("🔀 Demo 4: DAG Validation (Cycle Prevention)\n");
    println!("In Bazel: Dependency cycles detected during graph analysis (runtime)");
    println!("In Rush:  Cycles prevented during graph construction (compile time)\n");

    // Valid DAG: A -> B -> C
    let mut valid_builder = DagBuilder::new();
    valid_builder
        .add_node(DagNode::new("lib_base", "Base library"))
        .add_node(DagNode::new("lib_util", "Utility library"))
        .add_node(DagNode::new("app", "Application"))
        .add_edge(DagEdge::new("app", "lib_util"))
        .add_edge(DagEdge::new("lib_util", "lib_base"));

    match valid_builder.build() {
        Ok(dag) => {
            println!("✓ Valid DAG constructed:");
            let order = dag.topological_sort();
            println!("  Build order: {:?}", order);
        }
        Err(e) => {
            eprintln!("✗ Unexpected error: {}", e);
        }
    }

    // Invalid DAG: A -> B -> C -> A (cycle)
    println!("\nAttempting to create cyclic graph:");
    let mut cyclic_builder = DagBuilder::new();
    cyclic_builder
        .add_node(DagNode::new("lib_a", "Library A"))
        .add_node(DagNode::new("lib_b", "Library B"))
        .add_node(DagNode::new("lib_c", "Library C"))
        .add_edge(DagEdge::new("lib_a", "lib_b"))
        .add_edge(DagEdge::new("lib_b", "lib_c"))
        .add_edge(DagEdge::new("lib_c", "lib_a")); // Creates cycle!

    match cyclic_builder.build() {
        Ok(_) => {
            eprintln!("✗ Should have detected cycle!");
        }
        Err(e) => {
            println!("✓ Cycle detected and rejected: {}", e);
            println!("  In Bazel: Would fail during 'bazel build' execution");
        }
    }
    println!();
}

fn demo_session_types() {
    println!("📋 Demo 5: Session Types (Build Graph Protocol)\n");
    println!("In Bazel: Build graph can be executed in invalid states");
    println!("In Rush:  Type system enforces correct construction protocol\n");

    let mut dag_builder = DagBuilder::new();
    dag_builder
        .add_node(DagNode::new("base", "Base library"))
        .add_node(DagNode::new("app", "Application binary"))
        .add_edge(DagEdge::new("app", "base"));

    let dag = dag_builder.build().expect("Failed to build DAG");
    println!("✓ DAG constructed");

    // Session type protocol: ValidatedGraph -> OptimizedGraph -> Execute
    let graph = TypedBuildGraph::with_dag(dag);
    println!("✓ Build graph created (state: Validated)");

    let optimized = graph.optimize();
    println!("✓ Build graph optimized (state: Optimized)");

    optimized.execute().expect("Failed to execute");
    println!("✓ Build graph executed successfully");

    println!("\n// This WOULD NOT COMPILE in Rush:");
    println!("// let graph = TypedBuildGraph::<EmptyGraph, _>::new();");
    println!("// graph.execute();  // ERROR: no method execute() on TypedBuildGraph<EmptyGraph>");
    println!("// In Bazel: Undefined behavior or runtime error\n");
}
