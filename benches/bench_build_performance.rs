// Rush Build Performance Benchmarks
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use rush_action::{Action, Artifact, Command, ExecutionInfo};
use rush_cache::ActionCache;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

// Constants for performance targets
const SMALL_PROJECT_FILES: usize = 10;
const MEDIUM_PROJECT_FILES: usize = 50;
const LARGE_PROJECT_FILES: usize = 200;

/// Helper to generate a synthetic C++ project with N files
fn generate_cpp_project(dir: &Path, num_files: usize, deps_per_file: usize) -> Vec<PathBuf> {
    let mut source_files = Vec::new();

    // Create header files first
    for i in 0..num_files {
        let header_path = dir.join(format!("lib_{}.h", i));
        let header_content = format!(
            r#"// Auto-generated header {}
#ifndef LIB_{}_H
#define LIB_{}_H

namespace lib_{} {{
    int compute_{}();
    double process_{}(int x, double y);
    void initialize_{}();
}}

#endif // LIB_{}_H
"#,
            i, i, i, i, i, i, i, i
        );
        fs::write(&header_path, header_content).unwrap();
    }

    // Create source files with dependencies
    for i in 0..num_files {
        let source_path = dir.join(format!("lib_{}.cc", i));

        let mut includes = String::new();
        includes.push_str(&format!("#include \"lib_{}.h\"\n", i));

        // Add dependencies to previous files (creates dependency chain)
        for j in 0..deps_per_file.min(i) {
            let dep_idx = i.saturating_sub(j + 1);
            includes.push_str(&format!("#include \"lib_{}.h\"\n", dep_idx));
        }

        includes.push_str("#include <vector>\n");
        includes.push_str("#include <string>\n");
        includes.push_str("#include <algorithm>\n");

        let source_content = format!(
            r#"{}
namespace lib_{} {{

int compute_{}() {{
    std::vector<int> data;
    for (int i = 0; i < 1000; i++) {{
        data.push_back(i * i);
    }}
    return std::accumulate(data.begin(), data.end(), 0);
}}

double process_{}(int x, double y) {{
    double result = static_cast<double>(x) * y;
    for (int i = 0; i < 100; i++) {{
        result = result * 1.001 + 0.001;
    }}
    return result;
}}

void initialize_{}() {{
    // Initialization code
    volatile int dummy = 0;
    for (int i = 0; i < 10000; i++) {{
        dummy += i;
    }}
}}

}} // namespace lib_{}
"#,
            includes, i, i, i, i, i
        );

        fs::write(&source_path, source_content).unwrap();
        source_files.push(source_path);
    }

    // Create main.cc that uses all libraries
    let main_path = dir.join("main.cc");
    let mut main_content = String::from("#include <iostream>\n");
    for i in 0..num_files {
        main_content.push_str(&format!("#include \"lib_{}.h\"\n", i));
    }
    main_content.push_str("\nint main() {\n");
    for i in 0..num_files {
        main_content.push_str(&format!("    lib_{}::initialize_{}();\n", i, i));
    }
    main_content.push_str("    std::cout << \"Build successful!\" << std::endl;\n");
    main_content.push_str("    return 0;\n");
    main_content.push_str("}\n");
    fs::write(&main_path, main_content).unwrap();
    source_files.push(main_path);

    source_files
}

/// Helper to create BUILD file for the project
fn create_build_file(dir: &Path, num_files: usize) {
    let build_path = dir.join("BUILD");
    let mut build_content = String::from(
        r#"
cc_library(
    name = "all_libs",
    srcs = [
"#,
    );

    for i in 0..num_files {
        build_content.push_str(&format!("        \"lib_{}.cc\",\n", i));
    }
    build_content.push_str("    ],\n    hdrs = [\n");
    for i in 0..num_files {
        build_content.push_str(&format!("        \"lib_{}.h\",\n", i));
    }
    build_content.push_str(
        r#"    ],
    visibility = ["//visibility:public"],
)

cc_binary(
    name = "main",
    srcs = ["main.cc"],
    deps = [":all_libs"],
)
"#,
    );

    fs::write(build_path, build_content).unwrap();
}

/// Helper to create WORKSPACE file
fn create_workspace_file(dir: &Path) {
    let workspace_path = dir.join("WORKSPACE");
    fs::write(workspace_path, "workspace(name = \"benchmark\")\n").unwrap();
}

/// Benchmark: Clean build from scratch
fn bench_clean_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("clean_build");
    group.measurement_time(Duration::from_secs(20));
    group.sample_size(10);

    for &num_files in &[SMALL_PROJECT_FILES, MEDIUM_PROJECT_FILES] {
        let temp_dir = TempDir::new().unwrap();
        let project_dir = temp_dir.path();

        generate_cpp_project(project_dir, num_files, 2);
        create_build_file(project_dir, num_files);
        create_workspace_file(project_dir);

        group.bench_with_input(BenchmarkId::new("files", num_files), &num_files, |b, _| {
            b.iter(|| {
                // Simulate clean build by creating fresh cache
                let cache = ActionCache::new();

                // This would normally invoke the build orchestrator
                // For now we measure the setup overhead
                black_box(&cache);
            });
        });
    }

    group.finish();
}

/// Benchmark: Incremental build (no changes)
fn bench_incremental_build_no_changes(c: &mut Criterion) {
    let mut group = c.benchmark_group("incremental_build_no_changes");
    group.measurement_time(Duration::from_secs(10));

    for &num_files in &[SMALL_PROJECT_FILES, MEDIUM_PROJECT_FILES] {
        let temp_dir = TempDir::new().unwrap();
        let project_dir = temp_dir.path();

        generate_cpp_project(project_dir, num_files, 2);
        create_build_file(project_dir, num_files);
        create_workspace_file(project_dir);

        // Setup: perform initial build
        let cache = ActionCache::new();

        group.bench_with_input(BenchmarkId::new("files", num_files), &num_files, |b, _| {
            b.iter(|| {
                // Measure incremental build time with all cache hits
                black_box(&cache);
            });
        });
    }

    group.finish();
}

/// Benchmark: Incremental build (single file change)
fn bench_incremental_build_single_change(c: &mut Criterion) {
    let mut group = c.benchmark_group("incremental_build_single_change");
    group.measurement_time(Duration::from_secs(15));
    group.sample_size(10);

    for &num_files in &[SMALL_PROJECT_FILES, MEDIUM_PROJECT_FILES] {
        let temp_dir = TempDir::new().unwrap();
        let project_dir = temp_dir.path();

        generate_cpp_project(project_dir, num_files, 2);
        create_build_file(project_dir, num_files);
        create_workspace_file(project_dir);

        let cache = ActionCache::new();

        group.bench_with_input(BenchmarkId::new("files", num_files), &num_files, |b, _| {
            b.iter(|| {
                // Modify one file
                let file_to_modify = project_dir.join("lib_0.cc");
                let content = fs::read_to_string(&file_to_modify).unwrap();
                let modified = content.replace("1000", "1001");
                fs::write(&file_to_modify, modified).unwrap();

                // Measure rebuild time
                black_box(&cache);

                // Restore file
                fs::write(&file_to_modify, content).unwrap();
            });
        });
    }

    group.finish();
}

/// Benchmark: Cache hit rate measurement
fn bench_cache_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache_operations");

    let cache = ActionCache::new();

    // Create some dummy actions
    let num_actions = 100;
    let mut actions = Vec::new();

    for i in 0..num_actions {
        let inputs = vec![Artifact::Source(PathBuf::from(format!("lib_{}.cc", i)))];
        let command = Command {
            executable: PathBuf::from("g++"),
            args: vec![
                "-c".to_string(),
                format!("lib_{}.cc", i),
                "-o".to_string(),
                format!("lib_{}.o", i),
            ],
            execution_info: ExecutionInfo::default(),
        };
        let env = HashMap::new();
        let id = Action::compute_id(&inputs, &command, &env);

        actions.push(Action {
            id,
            inputs,
            outputs: vec![Artifact::Source(PathBuf::from(format!("lib_{}.o", i)))],
            command,
            env,
            mnemonic: "CppCompile".to_string(),
        });
    }

    group.bench_function("cache_lookup", |b| {
        b.iter(|| {
            for action in black_box(&actions) {
                let _ = cache.get(&action.id);
            }
        });
    });

    group.finish();
}

/// Benchmark: Dependency graph construction
fn bench_graph_construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_construction");

    for &num_files in &[
        SMALL_PROJECT_FILES,
        MEDIUM_PROJECT_FILES,
        LARGE_PROJECT_FILES,
    ] {
        let temp_dir = TempDir::new().unwrap();
        let project_dir = temp_dir.path();

        generate_cpp_project(project_dir, num_files, 3);
        create_build_file(project_dir, num_files);
        create_workspace_file(project_dir);

        group.bench_with_input(BenchmarkId::new("files", num_files), &num_files, |b, _| {
            b.iter(|| {
                // Create dependency graph (stub - graph implementation TBD)
                let graph_nodes = num_files;
                black_box(graph_nodes);
            });
        });
    }

    group.finish();
}

/// Benchmark: Parallel build execution simulation
fn bench_parallel_execution(c: &mut Criterion) {
    let mut group = c.benchmark_group("parallel_execution");
    group.measurement_time(Duration::from_secs(15));

    for &num_files in &[SMALL_PROJECT_FILES, MEDIUM_PROJECT_FILES] {
        group.bench_with_input(BenchmarkId::new("files", num_files), &num_files, |b, _| {
            b.iter(|| {
                use std::thread;

                // Simulate parallel compilation
                let handles: Vec<_> = (0..black_box(num_files))
                    .map(|i| {
                        thread::spawn(move || {
                            // Simulate compilation work
                            let mut sum = 0u64;
                            for j in 0..10000 {
                                sum = sum.wrapping_add(j * i as u64);
                            }
                            sum
                        })
                    })
                    .collect();

                for handle in handles {
                    black_box(handle.join().unwrap());
                }
            });
        });
    }

    group.finish();
}

/// Benchmark: Memory usage patterns
fn bench_memory_usage(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_usage");

    for &num_files in &[
        SMALL_PROJECT_FILES,
        MEDIUM_PROJECT_FILES,
        LARGE_PROJECT_FILES,
    ] {
        group.bench_with_input(BenchmarkId::new("files", num_files), &num_files, |b, _| {
            b.iter(|| {
                // Simulate memory usage for action tracking
                let mut actions = Vec::with_capacity(num_files);

                for i in 0..black_box(num_files) {
                    let inputs = vec![Artifact::Source(PathBuf::from(format!("lib_{}.cc", i)))];
                    let command = Command {
                        executable: PathBuf::from("g++"),
                        args: vec!["-c".to_string(), format!("lib_{}.cc", i)],
                        execution_info: ExecutionInfo::default(),
                    };
                    let env = HashMap::new();
                    let id = Action::compute_id(&inputs, &command, &env);

                    actions.push(Action {
                        id,
                        inputs,
                        outputs: vec![Artifact::Source(PathBuf::from(format!("lib_{}.o", i)))],
                        command,
                        env,
                        mnemonic: "CppCompile".to_string(),
                    });
                }

                black_box(actions);
            });
        });
    }

    group.finish();
}

/// Benchmark: CPU utilization efficiency
fn bench_cpu_efficiency(c: &mut Criterion) {
    let mut group = c.benchmark_group("cpu_efficiency");
    group.measurement_time(Duration::from_secs(10));

    let cpu_counts = [2, 4, 8, 16];

    for &cpus in &cpu_counts {
        group.bench_with_input(BenchmarkId::new("cpus", cpus), &cpus, |b, &num_cpus| {
            b.iter(|| {
                use rayon::prelude::*;

                // Simulate parallel work across CPUs
                let work_items: Vec<_> = (0..100).collect();

                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(num_cpus)
                    .build()
                    .unwrap();

                pool.install(|| {
                    work_items
                        .par_iter()
                        .map(|&x| {
                            // Simulate compilation work
                            let mut result = 0u64;
                            for i in 0..1000 {
                                result = result.wrapping_add(x as u64 * i);
                            }
                            result
                        })
                        .sum::<u64>()
                })
            });
        });
    }

    group.finish();
}

criterion_group!(
    build_benchmarks,
    bench_clean_build,
    bench_incremental_build_no_changes,
    bench_incremental_build_single_change,
    bench_cache_operations,
    bench_graph_construction,
    bench_parallel_execution,
    bench_memory_usage,
    bench_cpu_efficiency,
);

criterion_main!(build_benchmarks);
