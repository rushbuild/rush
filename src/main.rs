// Rush - Blazingly fast build system
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_cli::Cli;
use tracing_subscriber::{fmt, EnvFilter};

#[cfg(feature = "heap-profile")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        // Unfold anyhow's full context chain — `Debug` (the default
        // tokio::main returner) only shows the outermost cause, which
        // hides every `with_context(…)` we add at deeper layers. Print
        // the alternate `Display` form so the failing-action context
        // bubbles up where the user can see it.
        eprintln!("Error: {:#}", err);
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    #[cfg(feature = "heap-profile")]
    let _profiler = dhat::Profiler::new_heap();
    // Print memory profiling information if RUSH_MEM_PROFILE env var is set
    if std::env::var("RUSH_MEM_PROFILE").is_ok() {
        rush_starlark::mem_profile::print_type_sizes();
    }

    // CRITICAL: Check RAM floor BEFORE any initialization to prevent thrashing
    // This prevents Rush from starting when the system is already under pressure
    #[cfg(target_os = "linux")]
    {
        if let Ok((total_mb, available_mb)) = rush_resource::ResourceManager::read_system_memory() {
            let (ram_floor_exceeded, available_pct) =
                rush_resource::check_ram_floor_critical(Some((total_mb, available_mb)));

            if ram_floor_exceeded {
                eprintln!("╔════════════════════════════════════════════════════════════════╗");
                eprintln!("║ INSUFFICIENT RAM - CANNOT START BUILD                          ║");
                eprintln!("╚════════════════════════════════════════════════════════════════╝");
                eprintln!(
                    "Available RAM: {} MB / {} MB ({:.1}% free)",
                    available_mb, total_mb, available_pct
                );
                eprintln!("HARD MINIMUM: 4 GB (4096 MB) available RAM required");
                eprintln!();
                eprintln!("System is under memory pressure from other processes.");
                eprintln!("Free up memory and try again.");
                eprintln!();
                eprintln!("This strict check prevents Rush from thrashing your system.");
                std::process::exit(137); // Exit code 137 = killed for memory
            }
        }
    }

    // Initialize tracing subscriber to enable info!, warn!, debug! macros
    // Respects RUST_LOG environment variable (default: info)
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Phase 5.C.4 commit 54: tracing INFO/WARN/ERROR/DEBUG
    // logs go to STDERR. stdout is reserved for structured
    // build output (JSON dumps from
    // RUSH_SCHEDULER_PREFLIGHT_JSON / _LEGACY_BREAKDOWN_JSON
    // / _EXPLAIN_ACTION_KEYS_JSON modes). CI consumers pipe
    // stdout to jq; tracing logs would pollute that stream
    // without the explicit writer redirect.
    fmt()
        .with_env_filter(filter)
        .with_target(false) // Don't show module paths by default
        .with_writer(std::io::stderr)
        .init();

    Cli::run().await
}
