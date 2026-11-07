//! Phase 5.C.4 commit 54a: stdout/stderr discipline for the
//! build orchestrator.
//!
//! Pro's post-batch directive: "JSON modes must emit
//! parseable JSON on stdout with no human progress lines
//! interleaved. stdout = machine-readable requested output;
//! stderr = logs, progress, tracing, human status."
//!
//! This module provides a tiny set of free functions that
//! make the destination explicit at every call site. The
//! orchestrator was previously mixing 100+ `println!`
//! progress lines onto stdout where the JSON outputs from
//! commits 46/47/53 also live, so CI consumers piping rush
//! to `jq` had to grep around emoji progress markers like
//! medieval monks copying corrupted manuscripts.
//!
//! Four functions, four routing rules:
//!
//!   - [`ui_progress`]: human progress (📋/🔨/✅/🎉/🧭).
//!     Goes to stderr.
//!   - [`ui_status`]: terse banners (workspace path, target
//!     list). Goes to stderr.
//!   - [`ui_warning`]: warnings the operator should see.
//!     Goes to stderr.
//!   - [`ui_json_line`]: a single JSON object/line for CI
//!     consumption. Goes to stdout.
//!
//! Module-level free functions (no state) so call sites
//! don't need to thread a reporter handle through every
//! function signature in the orchestrator. Future extension
//! (e.g. `--quiet` flag, structured progress events) can
//! introduce a struct-with-state without changing the call
//! sites. Pro's guidance:
//!
//!   "Do not turn this into a full logging framework rewrite.
//!    Do not touch unrelated user-facing formatting.
//!    Do not bikeshed emojis."

/// Phase 5.C.4 commit 54a: human-readable progress message
/// (e.g. "📋 Loading WORKSPACE", "🔨 Building //:foo").
/// Routes to stderr so JSON-mode stdout stays clean.
pub fn ui_progress(msg: impl AsRef<str>) {
    eprintln!("{}", msg.as_ref());
}

/// Phase 5.C.4 commit 54a: terse banner / status line
/// (workspace path, target patterns, header lines).
/// Routes to stderr.
pub fn ui_status(msg: impl AsRef<str>) {
    eprintln!("{}", msg.as_ref());
}

/// Phase 5.C.4 commit 54a: warning the operator should see.
/// Routes to stderr.
pub fn ui_warning(msg: impl AsRef<str>) {
    eprintln!("{}", msg.as_ref());
}

/// Phase 5.C.4 commit 54a: structured JSON output line.
/// Routes to stdout. Used by:
///   - RUSH_SCHEDULER_PREFLIGHT_JSON=1 (commit 46)
///   - RUSH_LEGACY_BREAKDOWN_JSON=1 (commit 47)
///   - RUSH_EXPLAIN_ACTION_KEYS_JSON=1 (commit 53)
///
/// Caller is responsible for producing valid JSON; this
/// function just routes.
pub fn ui_json_line(msg: impl AsRef<str>) {
    println!("{}", msg.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    // The functions write to actual stdout/stderr so unit
    // tests can't easily assert their output. The contract
    // (which destination each function uses) is verified by
    // an integration test (`tests/json_mode_stdout.rs`) that
    // runs the binary and inspects its captured streams.
    //
    // These unit tests just verify the functions accept the
    // expected argument shapes (String, &str, AsRef<str>)
    // without panicking, so refactors that change the
    // signatures break the build instead of the integration
    // test.

    #[test]
    fn accepts_str_literal() {
        ui_progress("a");
        ui_status("b");
        ui_warning("c");
        ui_json_line("{}");
    }

    #[test]
    fn accepts_owned_string() {
        ui_progress(String::from("a"));
        ui_status(String::from("b"));
        ui_warning(String::from("c"));
        ui_json_line(String::from("{}"));
    }

    #[test]
    fn accepts_format_macro_result() {
        let n = 42;
        ui_progress(format!("count: {}", n));
        ui_json_line(format!("{{\"n\": {}}}", n));
    }
}
