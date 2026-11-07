// Rush Top - Process monitor for Rush builds
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! rushtop: htop-like monitor for Rush processes and PSI metrics
//!
//! Displays:
//! - All running Rush processes (PID, memory, CPU, command)
//! - PSI (Pressure Stall Information) metrics for memory, CPU, and IO
//! - GlobalBzlCache statistics (if available)
//! - System resource usage

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process;
use std::thread;
use std::time::Duration;

/// PSI metrics for a single resource (memory, CPU, or IO)
#[derive(Debug, Default)]
struct PsiMetrics {
    some10: f64,
    some60: f64,
    some300: f64,
    some_total: u64,
    full10: f64,
    full60: f64,
    full300: f64,
    full_total: u64,
}

impl PsiMetrics {
    fn parse(content: &str) -> Self {
        let mut metrics = Self::default();
        for line in content.lines() {
            if line.starts_with("some") {
                // Format: some avg10=0.00 avg60=0.00 avg300=0.00 total=0
                for part in line.split_whitespace() {
                    if let Some(val) = part.strip_prefix("avg10=") {
                        metrics.some10 = val.parse().unwrap_or(0.0);
                    } else if let Some(val) = part.strip_prefix("avg60=") {
                        metrics.some60 = val.parse().unwrap_or(0.0);
                    } else if let Some(val) = part.strip_prefix("avg300=") {
                        metrics.some300 = val.parse().unwrap_or(0.0);
                    } else if let Some(val) = part.strip_prefix("total=") {
                        metrics.some_total = val.parse().unwrap_or(0);
                    }
                }
            } else if line.starts_with("full") {
                // Format: full avg10=0.00 avg60=0.00 avg300=0.00 total=0
                for part in line.split_whitespace() {
                    if let Some(val) = part.strip_prefix("avg10=") {
                        metrics.full10 = val.parse().unwrap_or(0.0);
                    } else if let Some(val) = part.strip_prefix("avg60=") {
                        metrics.full60 = val.parse().unwrap_or(0.0);
                    } else if let Some(val) = part.strip_prefix("avg300=") {
                        metrics.full300 = val.parse().unwrap_or(0.0);
                    } else if let Some(val) = part.strip_prefix("total=") {
                        metrics.full_total = val.parse().unwrap_or(0);
                    }
                }
            }
        }
        metrics
    }
}

/// Information about a running Rush process
#[derive(Debug)]
struct RushProcess {
    pid: u32,
    memory_kb: u64,
    cpu_percent: f64,
    command: String,
}

impl RushProcess {
    fn from_pid(pid: u32) -> Option<Self> {
        let status_path = format!("/proc/{}/status", pid);
        let cmdline_path = format!("/proc/{}/cmdline", pid);

        let status = fs::read_to_string(&status_path).ok()?;
        let cmdline = fs::read_to_string(&cmdline_path).ok()?;

        // Parse memory from status
        let mut memory_kb = 0;
        for line in status.lines() {
            if line.starts_with("VmRSS:") {
                // Format: VmRSS:       12345 kB
                if let Some(val) = line.split_whitespace().nth(1) {
                    memory_kb = val.parse().unwrap_or(0);
                }
                break;
            }
        }

        // Parse command from cmdline (null-separated)
        let command = cmdline.replace('\0', " ").trim().to_string();

        // TODO: Calculate CPU percentage (would need to read /proc/[pid]/stat twice)
        let cpu_percent = 0.0;

        Some(RushProcess {
            pid,
            memory_kb,
            cpu_percent,
            command,
        })
    }
}

/// Find all Rush processes
fn find_rush_processes() -> Vec<RushProcess> {
    let mut processes = Vec::new();

    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Ok(file_name) = entry.file_name().into_string() {
                if let Ok(pid) = file_name.parse::<u32>() {
                    let cmdline_path = format!("/proc/{}/cmdline", pid);
                    if let Ok(cmdline) = fs::read_to_string(&cmdline_path) {
                        // Check if this is a Rush process
                        if cmdline.contains("rush")
                            || cmdline.contains("target/release/rush")
                            || cmdline.contains("target/debug/rush")
                        {
                            if let Some(proc) = RushProcess::from_pid(pid) {
                                processes.push(proc);
                            }
                        }
                    }
                }
            }
        }
    }

    processes.sort_by_key(|p| p.pid);
    processes
}

/// Read PSI metrics from /proc/pressure/
fn read_psi_memory() -> Option<PsiMetrics> {
    fs::read_to_string("/proc/pressure/memory")
        .ok()
        .map(|content| PsiMetrics::parse(&content))
}

fn read_psi_cpu() -> Option<PsiMetrics> {
    fs::read_to_string("/proc/pressure/cpu")
        .ok()
        .map(|content| PsiMetrics::parse(&content))
}

fn read_psi_io() -> Option<PsiMetrics> {
    fs::read_to_string("/proc/pressure/io")
        .ok()
        .map(|content| PsiMetrics::parse(&content))
}

/// Get system memory info from /proc/meminfo
fn read_system_memory() -> (u64, u64, u64) {
    let mut total_kb = 0;
    let mut available_kb = 0;
    let mut free_kb = 0;

    if let Ok(content) = fs::read_to_string("/proc/meminfo") {
        for line in content.lines() {
            if line.starts_with("MemTotal:") {
                if let Some(val) = line.split_whitespace().nth(1) {
                    total_kb = val.parse().unwrap_or(0);
                }
            } else if line.starts_with("MemAvailable:") {
                if let Some(val) = line.split_whitespace().nth(1) {
                    available_kb = val.parse().unwrap_or(0);
                }
            } else if line.starts_with("MemFree:") {
                if let Some(val) = line.split_whitespace().nth(1) {
                    free_kb = val.parse().unwrap_or(0);
                }
            }
        }
    }

    (total_kb, available_kb, free_kb)
}

/// ANSI color codes
const RESET: &str = "\x1B[0m";
const BOLD: &str = "\x1B[1m";
const _RED: &str = "\x1B[31m";
const GREEN: &str = "\x1B[32m";
const _YELLOW: &str = "\x1B[33m";
const _BLUE: &str = "\x1B[34m";
const CYAN: &str = "\x1B[36m";
const BOLD_RED: &str = "\x1B[1;31m";
const _BOLD_GREEN: &str = "\x1B[1;32m";
const BOLD_YELLOW: &str = "\x1B[1;33m";
const BOLD_CYAN: &str = "\x1B[1;36m";

/// Color code a percentage value based on severity
fn color_percent(value: f64, low_warn: f64, high_crit: f64) -> String {
    if value >= high_crit {
        format!("{}{:6.2}%{}", BOLD_RED, value, RESET)
    } else if value >= low_warn {
        format!("{}{:6.2}%{}", BOLD_YELLOW, value, RESET)
    } else {
        format!("{}{:6.2}%{}", GREEN, value, RESET)
    }
}

/// Color code memory usage percentage
fn color_memory_percent(percent: u64) -> String {
    if percent >= 90 {
        format!("{}{}%{}", BOLD_RED, percent, RESET)
    } else if percent >= 75 {
        format!("{}{}%{}", BOLD_YELLOW, percent, RESET)
    } else {
        format!("{}{}%{}", GREEN, percent, RESET)
    }
}

/// Clear screen and move cursor to top
fn clear_screen() {
    print!("\x1B[2J\x1B[1;1H");
    io::stdout().flush().unwrap();
}

/// Display the rushtop UI
fn display_ui() {
    clear_screen();

    println!(
        "{}╔═══════════════════════════════════════════════════════════════════════════════╗",
        BOLD_CYAN
    );
    println!("║  RUSHTOP - Rush Build System Process Monitor                                 ║");
    println!(
        "╚═══════════════════════════════════════════════════════════════════════════════╝{}",
        RESET
    );
    println!();

    // System Memory
    let (total_kb, available_kb, _free_kb) = read_system_memory();
    let total_mb = total_kb / 1024;
    let available_mb = available_kb / 1024;
    let used_mb = total_mb - available_mb;
    let used_percent = if total_mb > 0 {
        (used_mb * 100) / total_mb
    } else {
        0
    };

    println!(
        "{}┌─ System Memory ────────────────────────────────────────────────────────────────┐{}",
        CYAN, RESET
    );
    println!(
        "│ Total: {} MB  |  Used: {} MB ({})  |  Available: {} MB              │",
        total_mb,
        used_mb,
        color_memory_percent(used_percent),
        available_mb
    );
    println!("└────────────────────────────────────────────────────────────────────────────────┘");
    println!();

    // PSI Metrics
    println!(
        "{}┌─ PSI (Pressure Stall Information) ─────────────────────────────────────────────┐{}",
        CYAN, RESET
    );

    if let Some(mem_psi) = read_psi_memory() {
        println!(
            "│ {}Memory Pressure:{}                                                              │",
            BOLD, RESET
        );
        println!(
            "│   some: 10s={}  60s={}  300s={}  (total={} µs)          │",
            color_percent(mem_psi.some10, 5.0, 15.0),
            color_percent(mem_psi.some60, 2.0, 5.0),
            color_percent(mem_psi.some300, 1.0, 3.0),
            mem_psi.some_total
        );
        println!(
            "│   full: 10s={}  60s={}  300s={}  (total={} µs)          │",
            color_percent(mem_psi.full10, 5.0, 10.0),
            color_percent(mem_psi.full60, 2.0, 4.0),
            color_percent(mem_psi.full300, 1.0, 2.0),
            mem_psi.full_total
        );

        // Highlight critical pressure
        if mem_psi.full10 > 10.0 || mem_psi.full60 > 2.0 {
            println!(
                "│   {}⚠ ⚠ ⚠  CRITICAL MEMORY PRESSURE DETECTED!  ⚠ ⚠ ⚠{}                        │",
                BOLD_RED, RESET
            );
        } else if mem_psi.full10 > 5.0 || mem_psi.full60 > 1.0 {
            println!(
                "│   {}⚠  High memory pressure - build may slow down{}                           │",
                BOLD_YELLOW, RESET
            );
        }
    } else {
        println!(
            "│ Memory Pressure: N/A (requires Linux kernel 4.20+)                           │"
        );
    }

    println!("│                                                                                │");

    if let Some(cpu_psi) = read_psi_cpu() {
        println!(
            "│ {}CPU Pressure:{}                                                                 │",
            BOLD, RESET
        );
        println!(
            "│   some: 10s={}  60s={}  300s={}  (total={} µs)          │",
            color_percent(cpu_psi.some10, 20.0, 50.0),
            color_percent(cpu_psi.some60, 10.0, 30.0),
            color_percent(cpu_psi.some300, 5.0, 15.0),
            cpu_psi.some_total
        );
    } else {
        println!(
            "│ CPU Pressure: N/A                                                             │"
        );
    }

    println!("│                                                                                │");

    if let Some(io_psi) = read_psi_io() {
        println!(
            "│ {}I/O Pressure:{}                                                                 │",
            BOLD, RESET
        );
        println!(
            "│   some: 10s={}  60s={}  300s={}  (total={} µs)          │",
            io_psi.some10, io_psi.some60, io_psi.some300, io_psi.some_total
        );
        println!(
            "│   full: 10s={}  60s={}  300s={}  (total={} µs)          │",
            color_percent(io_psi.full10, 10.0, 30.0),
            color_percent(io_psi.full60, 5.0, 15.0),
            color_percent(io_psi.full300, 2.0, 10.0),
            io_psi.full_total
        );
    } else {
        println!(
            "│ I/O Pressure: N/A                                                             │"
        );
    }

    println!("└────────────────────────────────────────────────────────────────────────────────┘");
    println!();

    // Rush Processes
    let processes = find_rush_processes();

    println!(
        "{}┌─ Rush Processes ({}) ──────────────────────────────────────────────────────────┐{}",
        CYAN,
        processes.len(),
        RESET
    );
    println!(
        "│ {}{:>6}{} │ {}{:>10}{} │ {}{:>6}{} │ Command                                     │",
        BOLD, "PID", RESET, BOLD, "Memory", RESET, BOLD, "CPU%", RESET
    );
    println!("├────────┼────────────┼────────┼─────────────────────────────────────────────────┤");

    if processes.is_empty() {
        println!(
            "│ No Rush processes found                                                       │"
        );
    } else {
        for proc in processes.iter().take(10) {
            let memory_mb = proc.memory_kb / 1024;
            let command_short = if proc.command.len() > 45 {
                format!("{}...", &proc.command[..42])
            } else {
                proc.command.clone()
            };
            println!(
                "│ {:>6} │ {:>8} MB │ {:>5.1}% │ {:<45} │",
                proc.pid, memory_mb, proc.cpu_percent, command_short
            );
        }

        if processes.len() > 10 {
            println!(
                "│ ... and {} more processes                                                  │",
                processes.len() - 10
            );
        }
    }

    println!("└────────────────────────────────────────────────────────────────────────────────┘");
    println!();

    println!("Press Ctrl+C to exit. Refreshing every 1 second...");
}

fn main() {
    // Check if we're on Linux
    if !cfg!(target_os = "linux") {
        eprintln!("Error: rushtop only works on Linux (requires /proc filesystem and PSI)");
        process::exit(1);
    }

    // Check if PSI is available
    if !Path::new("/proc/pressure/memory").exists() {
        eprintln!("Warning: PSI not available. Requires Linux kernel 4.20+ and CONFIG_PSI=y");
        eprintln!("Some metrics will not be available.");
        println!();
    }

    // Set up Ctrl+C handler to clean up terminal
    ctrlc::set_handler(move || {
        println!();
        println!("Exiting rushtop...");
        process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");

    // Main loop
    loop {
        display_ui();
        thread::sleep(Duration::from_secs(1));
    }
}
