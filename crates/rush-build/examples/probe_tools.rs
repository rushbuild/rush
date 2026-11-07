// Phase 5.C.4 commit 190 (Batch Y): host tooling audit.
//
// Operator-facing one-shot that runs the audit pass and
// prints both the human-readable cache dump and the
// markdown table suitable for pasting into
// docs/external-tooling-blockers.md.
//
// Run with:
//   cargo run -p rush-build --example probe_tools
//
// The audit is read-only and side-effect-free. It does
// NOT install any tools.

fn main() {
    // Operator audit: always probe fresh. Point the runtime-probe
    // memo cache at a unique temp dir so each invocation re-execs
    // the `--version` probes rather than reading a stale workspace
    // cache.
    let cache_dir = std::env::temp_dir().join(format!(
        "rush-probe-tools-{}",
        std::process::id()
    ));
    let cache =
        rush_build::build_orchestrator_ext::probe_well_known_tools(&cache_dir);
    println!("# Tool audit ({})", chrono_or_date());
    println!();
    println!("## Human dump");
    println!();
    for line in rush_resource::format_probe_cache(&cache) {
        println!("{}", line);
    }
    println!();
    println!("## Markdown table");
    println!();
    for line in rush_build::build_orchestrator_ext::format_probe_cache_as_markdown_table(&cache) {
        println!("{}", line);
    }
}

fn chrono_or_date() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "(date unavailable)".to_string())
}
