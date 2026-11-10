// Rush Parsing Benchmarks
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use rush_build::bazelrc::{BazelRc, CompilerFlags};
use rush_parse::{parse_build_file, translate_envoy_macro, Rule, Value};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use tempfile::{NamedTempFile, TempDir};

// Helper to create a test BUILD file
fn create_test_build_file(num_rules: usize) -> (TempDir, std::path::PathBuf) {
    let temp_dir = TempDir::new().unwrap();

    // Create WORKSPACE file
    let workspace_path = temp_dir.path().join("WORKSPACE");
    fs::write(&workspace_path, "workspace(name = \"test\")\n").unwrap();

    // Create BUILD file
    let build_path = temp_dir.path().join("BUILD");
    let mut content = String::new();

    for i in 0..num_rules {
        content.push_str(&format!(
            r#"
rust_library(
    name = "lib_{i}",
    srcs = ["lib_{i}.rs"],
    deps = [
        "//common:base",
        "//common:utils",
    ],
    visibility = ["//visibility:public"],
)
"#
        ));
    }

    fs::write(&build_path, content).unwrap();
    (temp_dir, build_path)
}

// Helper to create a test .bazelrc file
fn create_test_bazelrc(num_configs: usize) -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();

    writeln!(file, "# Test .bazelrc").unwrap();
    writeln!(file, "startup --host_jvm_args=-Xmx3g").unwrap();
    writeln!(file).unwrap();
    writeln!(file, "common --noenable_bzlmod").unwrap();
    writeln!(file, "build --color=yes").unwrap();
    writeln!(file, "build --cxxopt=-std=c++20").unwrap();
    writeln!(file, "build --cxxopt=-Wall").unwrap();
    writeln!(file, "build --action_env=CC=clang").unwrap();
    writeln!(file).unwrap();

    for i in 0..num_configs {
        writeln!(file, "build:config_{i} --compilation_mode=opt").unwrap();
        writeln!(file, "build:config_{i} --strip=always").unwrap();
        writeln!(file, "build:config_{i} --copt=-O3").unwrap();
        writeln!(file, "build:config_{i} --linkopt=-fuse-ld=lld").unwrap();
    }

    writeln!(file).unwrap();
    writeln!(file, "test --test_verbose_timeout_warnings").unwrap();
    writeln!(file, "test --test_output=errors").unwrap();

    file.flush().unwrap();
    file
}

// Helper to create Envoy-style rules
fn create_envoy_rules(count: usize) -> Vec<Rule> {
    let mut rules = Vec::new();

    for i in 0..count {
        let mut attrs = HashMap::new();
        attrs.insert(
            "srcs".to_string(),
            Value::List(vec![
                Value::String(format!("lib_{i}.cc")),
                Value::String(format!("lib_{i}.h")),
            ]),
        );
        attrs.insert(
            "deps".to_string(),
            Value::List(vec![
                Value::String("//source/common/common:lib".to_string()),
                Value::String("@envoy_api//envoy/config/core/v3:pkg".to_string()),
            ]),
        );
        attrs.insert(
            "visibility".to_string(),
            Value::List(vec![Value::String("//visibility:public".to_string())]),
        );

        rules.push(Rule {
            cc_projection: None,
            name: format!("envoy_lib_{i}"),
            rule_type: "envoy_cc_library".to_string(),
            attributes: attrs,
        });
    }

    rules
}

// Benchmark BUILD file parsing
fn bench_build_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("build_file_parsing");

    for num_rules in [10, 50, 100, 500].iter() {
        let (temp_dir, build_path) = create_test_build_file(*num_rules);

        group.bench_with_input(BenchmarkId::from_parameter(num_rules), num_rules, |b, _| {
            b.iter(|| parse_build_file(black_box(&build_path)).unwrap());
        });

        // Keep temp_dir alive until benchmark is done
        drop(temp_dir);
    }

    group.finish();
}

// Benchmark .bazelrc parsing
fn bench_bazelrc_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("bazelrc_parsing");

    for num_configs in [5, 10, 25, 50].iter() {
        let bazelrc = create_test_bazelrc(*num_configs);

        group.bench_with_input(
            BenchmarkId::from_parameter(num_configs),
            num_configs,
            |b, _| {
                b.iter(|| BazelRc::parse(black_box(bazelrc.path())).unwrap());
            },
        );
    }

    group.finish();
}

// Benchmark compiler flags extraction
fn bench_compiler_flags(c: &mut Criterion) {
    let mut group = c.benchmark_group("compiler_flags_extraction");

    let flags = vec![
        "--cxxopt=-std=c++20".to_string(),
        "--cxxopt=-Wall".to_string(),
        "--cxxopt=-Wextra".to_string(),
        "--copt=-O3".to_string(),
        "--copt=-march=native".to_string(),
        "--linkopt=-fuse-ld=lld".to_string(),
        "--linkopt=-Wl,--gc-sections".to_string(),
        "-c opt".to_string(),
        "--action_env=CC=clang".to_string(),
        "--action_env=CXX=clang++".to_string(),
        "--config=linux".to_string(),
    ];

    group.bench_function("extract_flags", |b| {
        b.iter(|| CompilerFlags::from_bazelrc_flags(black_box(&flags)));
    });

    group.finish();
}

// Benchmark Envoy macro translation
fn bench_envoy_translation(c: &mut Criterion) {
    let mut group = c.benchmark_group("envoy_macro_translation");

    for rule_count in [10, 50, 100, 500].iter() {
        let rules = create_envoy_rules(*rule_count);

        group.bench_with_input(
            BenchmarkId::from_parameter(rule_count),
            rule_count,
            |b, _| {
                b.iter(|| {
                    for rule in black_box(&rules) {
                        translate_envoy_macro(rule);
                    }
                });
            },
        );
    }

    group.finish();
}

// Benchmark individual Envoy macro types
fn bench_envoy_macro_types(c: &mut Criterion) {
    let mut group = c.benchmark_group("envoy_macro_types");

    let macro_types = vec![
        "envoy_cc_library",
        "envoy_cc_test",
        "envoy_cc_binary",
        "envoy_cc_extension",
        "envoy_cc_fuzz_test",
        "envoy_proto_library",
    ];

    for macro_type in macro_types {
        let mut attrs = HashMap::new();
        attrs.insert(
            "srcs".to_string(),
            Value::List(vec![Value::String("test.cc".to_string())]),
        );

        let rule = Rule {
            cc_projection: None,
            name: "test".to_string(),
            rule_type: macro_type.to_string(),
            attributes: attrs,
        };

        group.bench_function(macro_type, |b| {
            b.iter(|| translate_envoy_macro(black_box(&rule)));
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_build_parsing,
    bench_bazelrc_parsing,
    bench_compiler_flags,
    bench_envoy_translation,
    bench_envoy_macro_types
);

criterion_main!(benches);
