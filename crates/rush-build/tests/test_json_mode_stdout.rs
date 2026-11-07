//! Phase 5.C.4 commit 54a integration test: JSON-mode
//! stdout cleanliness contract.
//!
//! Pro's directive: "JSON modes must emit parseable JSON on
//! stdout with no human progress lines interleaved."
//!
//! These tests run the rush binary against the hello_cpp
//! example with each JSON env var set, capture stdout,
//! and assert that stdout is parseable as JSON without
//! grepping around progress emojis.

use std::process::Command;

fn rush_binary() -> std::path::PathBuf {
    // Find the rush binary built by `cargo build --release`.
    // The integration tests run from
    // `crates/rush-build/`, so workspace_root is two levels up.
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

/// Phase 5.C.4 commit 54a: when RUSH_SCHEDULER_PREFLIGHT_JSON
/// is set, stdout must be a single parseable JSON object —
/// no progress emoji lines, no tracing INFO logs.
#[test]
fn preflight_json_stdout_has_no_progress_lines() {
    let rush = rush_binary();
    if !rush.exists() {
        eprintln!(
            "skipping: rush binary not built at {}; \
             run `cargo build --release --bin rush` first",
            rush.display()
        );
        return;
    }
    let hello = hello_cpp_dir();
    if !hello.exists() {
        eprintln!("skipping: hello_cpp example dir not found");
        return;
    }
    // Best-effort cleanup so we get a deterministic build.
    let _ = std::fs::remove_dir_all(hello.join("rush-out"));

    let output = Command::new(&rush)
        .arg("build")
        .arg("//:hello")
        .env("RUSH_USE_SCHEDULER", "1")
        .env("RUSH_SCHEDULER_PREFLIGHT", "1")
        .env("RUSH_SCHEDULER_PREFLIGHT_JSON", "1")
        .current_dir(&hello)
        .output()
        .expect("rush build invocation must succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Pro's contract: stdout must parse as JSON. If progress
    // lines polluted it, serde_json fails.
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .expect(&format!(
            "stdout must be parseable JSON; got:\n---START---\n{}\n---END---",
            stdout
        ));
    // Schema_version + dispatched are required fields per
    // commit 54.
    assert_eq!(parsed["schema_version"], 1);
    assert!(parsed["dispatched"].is_number());
}

/// Phase 5.C.4 commit 54a: human progress (📋/🔨/✅/🎉/🧭)
/// must go to stderr, not stdout.
#[test]
fn human_progress_goes_to_stderr() {
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

    // Run a regular build (no JSON env). Human progress
    // should appear on stderr, NOT stdout.
    let output = Command::new(&rush)
        .arg("build")
        .arg("//:hello")
        .current_dir(&hello)
        .output()
        .expect("rush build must succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Progress markers must NOT be on stdout.
    for marker in &["📋", "🔨", "🎉", "🧭"] {
        assert!(
            !stdout.contains(*marker),
            "stdout contains progress marker {} — should be stderr only.\nstdout was:\n{}",
            marker,
            stdout
        );
    }
    // At least one progress marker SHOULD be on stderr (a
    // hello_cpp build emits 🔨/✅ during build phase).
    let any_marker_on_stderr = ["📋", "🔨", "🎉"].iter().any(|m| stderr.contains(*m));
    assert!(
        any_marker_on_stderr,
        "expected at least one progress marker on stderr; \
         got stderr:\n{}",
        stderr
    );
}

/// Phase 5.C.4 commit 70: with `--verbose` + a JSON env
/// mode, the CLI's build-startup banners (Rush
/// Configuration block, Loading WORKSPACE, Jobs limit
/// messages, etc.) MUST NOT pollute stdout. The pre-
/// commit-70 versions of these lines were `println!`,
/// which would interleave with the JSON output and break
/// CI parsers.
///
/// Pro's commit-70 contract: "Keep stdout for JSON
/// reports. Move build-startup banners to stderr."
#[test]
fn json_build_with_cli_startup_banner_stdout_is_clean() {
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

    // `--verbose` activates the build-startup banner block
    // (Rush Build Configuration + flags from .bazelrc + jobs
    // limit + WORKSPACE loading + remote/profile/event
    // configuration messages). Pair with a JSON env mode to
    // pin: even with banners on, stdout stays parseable.
    let output = Command::new(&rush)
        .arg("build")
        .arg("--verbose")
        .arg("//:hello")
        .env("RUSH_USE_SCHEDULER", "1")
        .env("RUSH_SCHEDULER_PREFLIGHT", "1")
        .env("RUSH_SCHEDULER_PREFLIGHT_JSON", "1")
        .current_dir(&hello)
        .output()
        .expect("rush build must succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Pro's contract: stdout MUST parse as JSON. The
    // verbose banner block (🔧 Rush Build Configuration,
    // 📦 Loading WORKSPACE, etc.) must not be interleaved.
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!(
            "stdout must be parseable JSON under --verbose + JSON mode; \
             parse error: {}\nstdout was:\n---START---\n{}\n---END---",
            e, stdout
        ));
    assert_eq!(parsed["schema_version"], 1);

    // At least the verbose-banner marker SHOULD appear on
    // stderr — pins that the banner block is moved to
    // stderr rather than dropped entirely. Pre-commit-70
    // this would have been on stdout.
    let banner_on_stderr = stderr.contains("🔧 Rush Build Configuration")
        || stderr.contains("Loading WORKSPACE")
        || stderr.contains("Parallel jobs");
    assert!(
        banner_on_stderr,
        "expected at least one verbose startup banner on stderr; \
         got stderr:\n{}",
        stderr
    );
}

/// Phase 5.C.4 commit 71: `--scheduler-preflight=json`
/// outputs JSON on stdout. Equivalent to the env-var path
/// (`RUSH_SCHEDULER_PREFLIGHT_JSON=1`).
#[test]
fn cli_scheduler_preflight_json_outputs_json() {
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

    let output = Command::new(&rush)
        .arg("build")
        .arg("//:hello")
        .arg("--use-scheduler")
        .arg("--scheduler-preflight=json")
        .current_dir(&hello)
        .output()
        .expect("rush build must succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!(
            "--scheduler-preflight=json must produce parseable JSON on \
             stdout; parse error: {}\nstdout:\n{}",
            e, stdout
        ));
    assert_eq!(parsed["schema_version"], 1);
}

/// Phase 5.C.4 commit 71: CLI flag overrides env. If the env
/// says `RUSH_SCHEDULER_PREFLIGHT_JSON=1` but the CLI says
/// `--scheduler-preflight=off`, the CLI wins → no preflight
/// emission.
#[test]
fn cli_scheduler_preflight_off_overrides_env_json() {
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

    let output = Command::new(&rush)
        .arg("build")
        .arg("//:hello")
        .arg("--use-scheduler")
        .arg("--scheduler-preflight=off")
        .env("RUSH_SCHEDULER_PREFLIGHT", "1")
        .env("RUSH_SCHEDULER_PREFLIGHT_JSON", "1")
        .current_dir(&hello)
        .output()
        .expect("rush build must succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // CLI off → stdout should be empty (no preflight JSON).
    // If env had won, we'd see schema_version JSON.
    assert!(
        stdout.trim().is_empty(),
        "--scheduler-preflight=off must override env; stdout:\n{}",
        stdout
    );
}

/// Phase 5.C.4 commit 71: `--legacy-breakdown=off` suppresses
/// the legacy-breakdown stderr emission entirely.
#[test]
fn cli_legacy_breakdown_off_suppresses_emission() {
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

    let output = Command::new(&rush)
        .arg("build")
        .arg("//:hello")
        .arg("--use-scheduler")
        .arg("--legacy-breakdown=off")
        .current_dir(&hello)
        .output()
        .expect("rush build must succeed");

    let stderr = String::from_utf8_lossy(&output.stderr);
    // The legacy breakdown marker line shouldn't appear.
    // The orchestrator prefixes legacy-breakdown lines with
    // 🧭 — and the breakdown specifically mentions
    // "legacy" in its content. Conservative check: no
    // "📜 Legacy" / "rule-kind" lines.
    let has_legacy_rule_kind_lines = stderr.contains("by_rule_kind")
        || stderr.contains("legacy actions by mnemonic");
    assert!(
        !has_legacy_rule_kind_lines,
        "--legacy-breakdown=off must suppress legacy breakdown lines; \
         stderr:\n{}",
        stderr
    );
}

/// Phase 5.C.4 commit 71: `--explain-action-keys-format=json`
/// produces parseable JSON on stdout (equivalent to
/// RUSH_EXPLAIN_ACTION_KEYS_JSON=1).
#[test]
fn cli_explain_action_keys_format_json_outputs_json() {
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

    let output = Command::new(&rush)
        .arg("build")
        .arg("--analyze-only")
        .arg("//:hello")
        .arg("--explain-action-keys")
        .arg("--explain-action-keys-format=json")
        .current_dir(&hello)
        .output()
        .expect("rush build must succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Each line is one ActionKeyExplainEntry.
    let mut parsed_entries = 0;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!(
                "--explain-action-keys-format=json line must parse: {}\nline:\n{}",
                e, line
            ));
        assert_eq!(val["schema_version"], 1);
        parsed_entries += 1;
    }
    assert!(
        parsed_entries > 0,
        "expected at least one ActionKeyExplainEntry; got 0 lines"
    );
}

/// Phase 5.C.4 commit 54a: non-JSON-mode stdout should be
/// empty (or close to empty) — only RUSH_*_JSON modes emit
/// machine output. Human progress is stderr-only.
#[test]
fn regular_build_stdout_is_empty_or_near_empty() {
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

    let output = Command::new(&rush)
        .arg("build")
        .arg("//:hello")
        .current_dir(&hello)
        .output()
        .expect("rush build must succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // stdout should be empty (no JSON requested, no machine
    // output mode active). Progress went to stderr.
    assert!(
        stdout.trim().is_empty(),
        "regular build stdout should be empty; got:\n{}",
        stdout
    );
}
