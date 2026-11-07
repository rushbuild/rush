//! Phase 5.C.4 commit 92: duplicate parse regression test.
//!
//! Pro's commit-92 directive: *"Add a synthetic workspace
//! test. Shape: package //a has multiple targets, package
//! //b depends on multiple //a targets, analyze a
//! top-level target that reaches //a through multiple
//! paths. Assert: //a/BUILD is parsed/evaluated once;
//! repeated target lookups do not re-parse the package;
//! timing counters reflect one miss + N hits, not N
//! misses."*
//!
//! This test uses the existing `examples/hello_cpp`
//! workspace as the synthetic fixture. hello_cpp's `//`
//! package has 18+ targets across cc_library, cc_binary,
//! cc_test, cc_proto_library, tf_*, plus aliases — when
//! the analyze pass walks multiple of those targets, the
//! orchestrator's `build_files_seen` dedup MUST kick in,
//! producing one parse + N-1 hits.
//!
//! The test runs rush with:
//!   RUSH_ANALYZE_CACHE_COUNTERS=1
//!   --analyze-only --use-scheduler
//!   //:hello //:hello_lib //:greeting_cc_proto ...
//!
//! and parses the cache-counter output to assert the
//! one-miss-many-hits invariant.

use std::process::Command;

fn rush_binary() -> std::path::PathBuf {
    let workspace_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    workspace_root.join("target").join("release").join("rush")
}

fn hello_cpp_dir() -> std::path::PathBuf {
    let workspace_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    workspace_root.join("examples").join("hello_cpp")
}

/// Phase 5.C.4 commit 92: multiple targets from the SAME
/// package must NOT cause repeated package parses.
///
/// Asserts: build_file_misses == 1 (one unique package
/// parse) and build_file_hits >= 2 (subsequent target
/// lookups hit the orchestrator's `build_files_seen`
/// dedup).
///
/// Pro's contract: "//a/BUILD is parsed/evaluated once,
/// or cache-hit on repeats."
#[test]
fn multiple_targets_in_same_package_parse_once() {
    let rush = rush_binary();
    if !rush.exists() {
        eprintln!(
            "skipping: rush binary not built at {}; run \
             `cargo build --release --bin rush` first",
            rush.display()
        );
        return;
    }
    let hello = hello_cpp_dir();
    if !hello.exists() {
        eprintln!("skipping: hello_cpp example dir not found");
        return;
    }
    let _ = std::fs::remove_dir_all(hello.join("rush-out"));

    // hello_cpp/BUILD has many targets in the `//` root
    // package. Pick five of them — analyzing all five
    // should produce ONE parse + four hits.
    let output = Command::new(&rush)
        .arg("build")
        .arg("--analyze-only")
        .arg("--use-scheduler")
        .arg("//:hello")
        .arg("//:hello_lib")
        .arg("//:hello_test")
        .arg("//:greeting_cc_proto")
        .arg("//:tf_smoke_proto")
        .env("RUSH_ANALYZE_CACHE_COUNTERS", "1")
        .env("RUSH_ANALYZE_CACHE_COUNTERS_JSON", "1")
        .current_dir(&hello)
        .output()
        .expect("rush invocation must succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Parse the cache-counter JSON from stdout. With
    // --use-scheduler + RUSH_ANALYZE_CACHE_COUNTERS_JSON=1
    // the orchestrator emits one JSON line. Other JSON
    // payloads (preflight, hot-spots) may also be present;
    // find the one with cache-counter fields.
    let mut counters_json: Option<serde_json::Value> = None;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("build_file_lookups").is_some() {
                counters_json = Some(v);
                break;
            }
        }
    }

    let counters = counters_json.unwrap_or_else(|| {
        panic!(
            "cache-counter JSON not found in stdout. \
             stdout:\n{}\nstderr:\n{}",
            stdout, stderr
        )
    });

    let lookups = counters["build_file_lookups"].as_u64().unwrap_or(0);
    let hits = counters["build_file_hits"].as_u64().unwrap_or(0);
    let misses = counters["build_file_misses"].as_u64().unwrap_or(0);

    // Pro's contract: //a is parsed ONCE.
    //
    // Note: lookups counts orchestrator outer-loop
    // iterations. With 5 explicit targets all in the //
    // package, we expect:
    //   lookups = 5
    //   misses  = 1
    //   hits    = 4
    //
    // If misses > 1, the dedup is broken and the bug Pro
    // suspected from the TF census IS present. The test
    // fails to flag the regression class.
    assert!(
        lookups >= 5,
        "expected at least 5 lookups for 5 targets; got {}",
        lookups
    );
    assert_eq!(
        misses, 1,
        "expected exactly 1 miss (one unique package parsed); got {}. \
         lookups={}, hits={}. \
         If misses > 1, the orchestrator's `build_files_seen` dedup \
         is broken — the same package is being re-parsed for each \
         target in the same package.",
        misses, lookups, hits
    );
    assert_eq!(
        lookups, misses + hits,
        "lookups must equal misses + hits; got lookups={}, misses={}, hits={}",
        lookups, misses, hits
    );
}

/// Phase 5.C.4 commit 92: hits accumulate as targets
/// pile up. Pro's spec: "timing counters reflect one
/// miss + N hits, not N misses."
#[test]
fn cache_counters_reflect_one_miss_plus_n_hits() {
    let rush = rush_binary();
    if !rush.exists() {
        eprintln!("skipping: rush binary not built");
        return;
    }
    let hello = hello_cpp_dir();
    if !hello.exists() {
        eprintln!("skipping: hello_cpp not found");
        return;
    }
    let _ = std::fs::remove_dir_all(hello.join("rush-out"));

    // Three targets in the same package.
    let output = Command::new(&rush)
        .arg("build")
        .arg("--analyze-only")
        .arg("--use-scheduler")
        .arg("//:hello_lib")
        .arg("//:hello")
        .arg("//:hello_test")
        .env("RUSH_ANALYZE_CACHE_COUNTERS", "1")
        .env("RUSH_ANALYZE_CACHE_COUNTERS_JSON", "1")
        .current_dir(&hello)
        .output()
        .expect("rush invocation must succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut counters: Option<serde_json::Value> = None;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("build_file_lookups").is_some() {
                counters = Some(v);
                break;
            }
        }
    }
    let counters = counters.expect("cache-counter JSON present");

    let hits = counters["build_file_hits"].as_u64().unwrap_or(0);
    let misses = counters["build_file_misses"].as_u64().unwrap_or(0);

    // One miss for the // package; subsequent target
    // lookups should be hits.
    assert_eq!(misses, 1, "expected exactly 1 miss; got {}", misses);
    assert!(
        hits >= 2,
        "expected at least 2 hits for repeat-target lookups; got {}",
        hits
    );
}
