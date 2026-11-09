// Rush CLI - CLI interface
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! CLI interface.

#![warn(missing_docs)]

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use owo_colors::OwoColorize;
use rush_build::{
    action_manifest::{
        execute_action_manifest_with_lock_fd_and_output_mode, ActionManifestOutputMode,
    },
    bazel_compat::parse_bazel_bool, cache_paths, BazelRc, BuildOrchestrator, BuildRequest,
    CacheConfig, TestOutputMode,
};
use rush_parse::parse_build_file;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Internal transport for the fully ordered visibility-option stream.
///
/// The client recursively expands every `.bazelrc` `--config` at its exact
/// occurrence before publishing this value, so in-process and daemon analysis
/// consume identical Bazel precedence.
const EFFECTIVE_VISIBILITY_FLAGS_ENV: &str =
    "RUSH_EFFECTIVE_VISIBILITY_FLAGS";

const VISIBILITY_COMPATIBILITY_FLAG_NAMES: [&str; 4] = [
    "incompatible_enforce_config_setting_visibility",
    "incompatible_config_setting_private_default_visibility",
    "incompatible_package_group_has_public_syntax",
    "incompatible_fix_package_group_reporoot_syntax",
];

/// Detect if Rush was invoked as "bazel" (for drop-in replacement).
fn is_bazel_mode() -> bool {
    std::env::args()
        .next()
        .map(|arg0| arg0.ends_with("bazel") || arg0.ends_with("bazel.exe"))
        .unwrap_or(false)
}

/// Lossless bytes of an `OsStr`-ish value (argv/env). Unix paths/args are not
/// guaranteed UTF-8; this mirrors `CommandSemantics::from_process`'s capture so
/// the daemon reconstructs identical semantics from the wire request.
fn os_bytes(s: &std::ffi::OsString) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        s.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        s.to_string_lossy().into_owned().into_bytes()
    }
}

/// Lossless bytes of a path (the cwd) for the wire request.
fn path_bytes(p: &std::path::Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        p.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        p.to_string_lossy().into_owned().into_bytes()
    }
}

fn is_visibility_compatibility_flag(flag: &str) -> bool {
    VISIBILITY_COMPATIBILITY_FLAG_NAMES.iter().any(|name| {
        flag == format!("--{name}")
            || flag == format!("--no{name}")
            || flag.starts_with(&format!("--{name}="))
    })
}

fn publish_effective_visibility_flags(flags: &[String]) {
    if flags.is_empty() {
        std::env::remove_var(EFFECTIVE_VISIBILITY_FLAGS_ENV);
    } else {
        std::env::set_var(EFFECTIVE_VISIBILITY_FLAGS_ENV, flags.join("\n"));
    }
}

#[derive(Debug, PartialEq, Eq)]
struct BazelRcBuildExpansion {
    rc_flags: Vec<String>,
    effective_visibility_flags: Vec<String>,
}

fn expand_named_bazelrc_config(
    bazelrc: &BazelRc,
    name: &str,
    stack: &mut Vec<String>,
) -> Result<BazelRcBuildExpansion> {
    if name.is_empty() {
        return Err(anyhow!("--config requires a non-empty name"));
    }
    if let Some(cycle_start) = stack.iter().position(|entry| entry == name) {
        let mut cycle = stack[cycle_start..].to_vec();
        cycle.push(name.to_string());
        return Err(anyhow!(
            "recursive .bazelrc --config cycle: {}",
            cycle.join(" -> ")
        ));
    }
    let flags = bazelrc
        .named_config_flags("build", name)
        .ok_or_else(|| {
            anyhow!(
                "--config={} has no definition applicable to build",
                name
            )
        })?;
    stack.push(name.to_string());
    let expanded = expand_bazelrc_tokens(bazelrc, &flags, stack);
    stack.pop();
    expanded
}

fn expand_bazelrc_tokens(
    bazelrc: &BazelRc,
    tokens: &[String],
    stack: &mut Vec<String>,
) -> Result<BazelRcBuildExpansion> {
    let mut rc_flags = Vec::new();
    let mut effective_visibility_flags = Vec::new();
    let mut index = 0usize;
    while index < tokens.len() {
        let token = &tokens[index];
        if token == "--" {
            rc_flags.extend(tokens[index..].iter().cloned());
            break;
        }
        let config = if token == "--config" {
            index += 1;
            Some(tokens.get(index).ok_or_else(|| {
                anyhow!("--config requires a value")
            })?.as_str())
        } else {
            token.strip_prefix("--config=")
        };
        if let Some(name) = config {
            let expansion = expand_named_bazelrc_config(
                bazelrc,
                name,
                stack,
            )?;
            rc_flags.extend(expansion.rc_flags);
            effective_visibility_flags.extend(
                expansion.effective_visibility_flags,
            );
        } else {
            rc_flags.push(token.clone());
            if is_visibility_compatibility_flag(token) {
                effective_visibility_flags.push(token.clone());
            }
        }
        index += 1;
    }
    Ok(BazelRcBuildExpansion {
        rc_flags,
        effective_visibility_flags,
    })
}

fn expand_bazelrc_build(
    bazelrc: &BazelRc,
    cli_tokens: &[String],
) -> Result<BazelRcBuildExpansion> {
    let mut stack = Vec::new();
    let common = expand_bazelrc_tokens(
        bazelrc,
        &bazelrc.common_flags,
        &mut stack,
    )?;
    let build = expand_bazelrc_tokens(
        bazelrc,
        &bazelrc.build_flags,
        &mut stack,
    )?;
    let mut rc_flags = common.rc_flags;
    rc_flags.extend(build.rc_flags);
    let mut effective_visibility_flags =
        common.effective_visibility_flags;
    effective_visibility_flags.extend(build.effective_visibility_flags);
    let mut index = 0usize;
    while index < cli_tokens.len() {
        let token = &cli_tokens[index];
        if token == "--" {
            break;
        }
        let config = if token == "--config" {
            index += 1;
            Some(cli_tokens.get(index).ok_or_else(|| {
                anyhow!("--config requires a value")
            })?.as_str())
        } else {
            token.strip_prefix("--config=")
        };
        if let Some(name) = config {
            let flags = expand_named_bazelrc_config(
                bazelrc,
                name,
                &mut stack,
            )?;
            rc_flags.extend(flags.rc_flags);
            effective_visibility_flags.extend(
                flags.effective_visibility_flags,
            );
        } else if is_visibility_compatibility_flag(token) {
            effective_visibility_flags.push(token.clone());
        }
        index += 1;
    }
    Ok(BazelRcBuildExpansion {
        rc_flags,
        effective_visibility_flags,
    })
}

fn process_command_tokens() -> Vec<String> {
    std::env::args_os()
        .skip(2)
        .map(|token| token.to_string_lossy().into_owned())
        .collect()
}

fn load_bazelrc_build_expansion(
    workspace_root: &Path,
    cli_tokens: &[String],
    verbose: bool,
) -> Result<BazelRcBuildExpansion> {
    let bazelrc_path = workspace_root.join(".bazelrc");
    let bazelrc = if bazelrc_path.exists() {
        match BazelRc::parse(&bazelrc_path) {
            Ok(rc) => {
                if verbose {
                    eprintln!(
                        "📋 Loaded .bazelrc from: {}",
                        bazelrc_path.display()
                    );
                }
                rc
            }
            Err(error) => {
                return Err(anyhow!("Failed to parse .bazelrc: {error}"));
            }
        }
    } else {
        BazelRc {
            common_flags: Vec::new(),
            build_flags: Vec::new(),
            test_flags: Vec::new(),
            run_flags: Vec::new(),
            configs: HashMap::new(),
            startup_flags: Vec::new(),
        }
    };

    let expansion = expand_bazelrc_build(&bazelrc, cli_tokens)?;
    if verbose && !expansion.rc_flags.is_empty() {
        eprintln!("   Flags from .bazelrc: {:?}", expansion.rc_flags);
    }
    Ok(expansion)
}

fn normalized_daemon_build_argv(
    mut argv: Vec<bytes::Bytes>,
) -> Vec<bytes::Bytes> {
    if argv.is_empty() {
        argv.push(bytes::Bytes::from_static(b"rush"));
    }
    if argv.len() == 1 {
        argv.push(bytes::Bytes::from_static(b"build"));
    } else {
        argv[1] = bytes::Bytes::from_static(b"build");
    }

    let mut normalized = Vec::with_capacity(argv.len());
    let mut index = 0;
    let mut after_terminator = false;
    while index < argv.len() {
        let token = &argv[index];
        if index >= 2 && !after_terminator {
            if token.as_ref() == b"--" {
                after_terminator = true;
            } else if token.as_ref() == b"--daemon" {
                index += 2;
                continue;
            } else if token.starts_with(b"--daemon=") {
                index += 1;
                continue;
            }
        }
        normalized.push(token.clone());
        index += 1;
    }
    normalized
}

/// Per-workspace daemon discovery paths.
///
/// Both the daemon (server) and the `--daemon=auto` client derive the SAME
/// paths from the canonicalized workspace root, so a client finds the daemon
/// without a hardcoded socket name. Everything lives in a user-private 0700
/// directory under the runtime dir (`$XDG_RUNTIME_DIR`, else `/tmp/rush-<uid>`).
struct DaemonDiscovery {
    /// The 0700 directory holding the socket/cookie/metadata for this workspace.
    dir: PathBuf,
    /// The Unix-domain socket the daemon binds.
    socket_path: PathBuf,
    /// The 0600 auth-cookie file.
    cookie_path: PathBuf,
    /// The discovery metadata JSON the client reads.
    metadata_path: PathBuf,
}

#[derive(clap::Subcommand, Debug, Clone)]
pub enum CacheAction {
    /// Report cache roots, entry counts and sizes.
    Info {
        /// Emit JSON instead of a human summary.
        #[arg(long)]
        json: bool,
    },
    /// Delete cached state. Requires --all: the cache is shared by
    /// every workspace on this machine, so there is no per-workspace
    /// subset to clear.
    Clear {
        /// Confirm deletion of the MACHINE-GLOBAL cache.
        #[arg(long)]
        all: bool,
    },
}

impl DaemonDiscovery {
    /// Derive the discovery paths for `workspace_root`.
    ///
    /// The directory name embeds a STABLE hash of the canonicalized workspace
    /// root (FNV-1a — deterministic across process runs, unlike the
    /// random-seeded `DefaultHasher`) so two separate `rush` processes for the
    /// same workspace agree on the location. The socket path is kept short
    /// (the hash, not the full workspace path) to stay within the
    /// `sun_path` limit.
    fn for_workspace(workspace_root: &std::path::Path) -> Result<Self> {
        let real = std::fs::canonicalize(workspace_root)
            .unwrap_or_else(|_| workspace_root.to_path_buf());
        let hash = fnv1a_hex(path_bytes(&real).as_slice());

        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let uid = unsafe { libc::getuid() };
                std::env::temp_dir().join(format!("rush-{uid}"))
            });
        let protocol = format!(
            "v{}.{}",
            rush_protocol::PROTOCOL_MAJOR,
            rush_protocol::PROTOCOL_MINOR
        );
        let dir = base.join("rush-daemon").join(protocol).join(&hash);
        Ok(DaemonDiscovery {
            socket_path: dir.join("rushd.sock"),
            cookie_path: dir.join("cookie"),
            metadata_path: dir.join("metadata.json"),
            dir,
        })
    }

    /// Create the discovery directory with 0700 perms.
    fn ensure_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

/// Stable FNV-1a 64-bit hash rendered as lowercase hex. Deterministic across
/// process runs (the workspace-path → discovery-dir mapping must be stable).
fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Rush - Blazingly fast build system
///
/// A spiritual successor to Bazel, written in Rust.
#[derive(Parser, Debug)]
#[command(name = "rush")]
#[command(version = VERSION)]
#[command(about = "Blazingly fast build system - razing Bazel to the ground")]
#[command(long_about = "\
Rush - Blazingly fast build system

A spiritual successor to Bazel, written in Rust. Provides Bazel's powerful
semantics (hermetic builds, aggressive caching, distributed execution) without
the JVM overhead.

COMPATIBILITY:
  Rush is 90% compatible with Bazel. Most Bazel projects work with zero changes.
  Create a symlink: ln -s /usr/local/bin/rush /usr/local/bin/bazel

EXAMPLES:
  rush build //...                    Build all targets
  rush build //pkg:target             Build specific target
  rush test //...                     Run all tests
  rush run //cmd/app:main             Run a binary
  rush build --config=release //...   Optimized build

For detailed documentation, see: https://github.com/mcgrof/rush/blob/master/CLI.md
")]
pub struct Cli {
    /// Subcommand to execute
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ActionManifestErrorFormat {
    Human,
    Json,
}

/// Available commands
#[derive(Subcommand, Debug)]
enum Commands {
    /// Execute a declared file-based action through the standard Remote Execution API.
    #[command(visible_alias = "rexec")]
    RemoteExecute {
        /// JSON action description: argv, explicit inputs, outputs and platform.
        manifest: PathBuf,
        /// gRPC endpoint; use HTTPS or an authenticated loopback tunnel.
        #[arg(long)]
        endpoint: String,
        /// Local root for relative inputs and verified outputs.
        #[arg(long, default_value = ".")]
        execution_root: PathBuf,
        /// Backend instance name.
        #[arg(long, default_value = "")]
        instance: String,
        /// Total client deadline, including queue and transfer time.
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u32).range(1..))]
        deadline_seconds: u32,
        /// Save outcome metadata and captured streams as JSON.
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// Build specified targets
    #[command(visible_alias = "b")]
    #[command(after_help = "\
EXAMPLES:
  rush build //pkg:target              Build a specific target
  rush build //pkg:all                 Build all targets in package
  rush build //...                     Build everything
  rush build --config=release //...    Optimized build
  rush build --jobs=8 //...            Build with 8 parallel jobs

FLAGS:
  --config=<name>           Apply configuration from .bazelrc
  --compilation_mode=<mode> Compilation mode: fastbuild, opt, dbg
  -c <mode>                 Short form of --compilation_mode
  --jobs=<n>                Number of parallel jobs
  -j <n>                    Short form of --jobs
  --remote_executor=<url>   Remote execution endpoint
  --verbose, -v             Verbose output
")]
    Build {
        /// Target labels to build (e.g., //pkg:target, //..., //pkg:all)
        #[arg(required = true)]
        targets: Vec<String>,

        /// Apply named configurations from .bazelrc in occurrence order
        #[arg(long, action = clap::ArgAction::Append)]
        config: Vec<String>,

        /// Bazel-style build setting: --define KEY=VALUE (repeatable).
        ///
        /// Flows into `config_setting(define_values=...)` matching, which in
        /// turn drives `select()` — exactly like Bazel. It is NOT emitted as a
        /// compiler `-D`. Example: `--define tflite_with_xnnpack=false` drops
        /// the TFLite XNNPACK delegate from the build graph.
        #[arg(long = "define", value_name = "KEY=VALUE")]
        defines: Vec<String>,

        /// Compilation mode: fastbuild, opt, or dbg
        #[arg(long, short = 'c')]
        compilation_mode: Option<String>,

        /// Number of parallel jobs (default: auto)
        #[arg(long, short = 'j')]
        jobs: Option<u32>,

        /// Enable disk-backed cache
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        disk_cache: bool,

        /// Disk cache directory (default: ~/.cache/rush)
        #[arg(long)]
        disk_cache_dir: Option<PathBuf>,

        /// Remote executor endpoint (Remote Execution API v2)
        #[arg(long)]
        remote_executor: Option<String>,

        /// Worker platform property, including a pinned toolchain=VALUE.
        #[arg(long = "remote-platform", value_name = "KEY=VALUE", requires = "remote_executor")]
        remote_platform: Vec<String>,

        /// Remote Execution API instance name.
        #[arg(long, default_value = "", requires = "remote_executor")]
        remote_instance: String,

        /// Remote cache endpoint
        #[arg(long)]
        remote_cache: Option<String>,

        /// Show detailed benchmark results
        #[arg(long)]
        benchmark: bool,

        /// Enable build profiling (generates trace file)
        #[arg(long)]
        profile: Option<PathBuf>,

        /// Write build events to JSON file
        #[arg(long)]
        build_event_json: Option<PathBuf>,

        /// Verbose output
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Show progress bar
        #[arg(long)]
        progress: bool,

        /// Stream real-time build output
        #[arg(long)]
        streaming: bool,

        /// Export metrics to JSON file
        #[arg(long)]
        metrics_json: Option<PathBuf>,

        /// Export metrics to CSV file
        #[arg(long)]
        metrics_csv: Option<PathBuf>,

        /// Maximum memory usage in GB (abort build if exceeded)
        #[arg(long)]
        max_memory_gb: Option<u64>,

        /// Analyze targets without executing actions.
        ///
        /// Loads BUILD files, instantiates rules, resolves dependencies,
        /// topologically sorts, and prints a census of the resulting
        /// graph (node counts, rule-type distribution, edge counts,
        /// process RSS) — then exits successfully without compiling
        /// anything. Useful for measuring the memory cost of the
        /// analysis phase in isolation from action execution, and for
        /// catching parse/dependency errors quickly. Pair with
        /// `RUSH_MEM_CENSUS=1` to also emit per-phase RSS deltas.
        #[arg(long)]
        analyze_only: bool,

        /// Run the producer/consumer validation sweep after analysis.
        ///
        /// Phase 5.C0 producer registry is populated as rules'
        /// analyze() declare their compile/link/archive actions.
        /// After all targets are analyzed, validates that every
        /// generated artifact in the typed CcInfo contexts has a
        /// declared producer (pass 2), and that every declared
        /// action's generated input has a declared producer
        /// (pass 3). Logs warnings + a summary line; doesn't
        /// fail the build. Equivalent to setting
        /// `RUSH_VALIDATE_PRODUCERS=1`.
        #[arg(long)]
        validate_producers: bool,

        /// Phase 5.C.2: validate declared outputs on disk.
        ///
        /// After each target's build() succeeds, every Complete
        /// declared action's output ArtifactId paths are stat()ed.
        /// A missing output fails the build with a structured
        /// error naming the action mnemonic, the missing path,
        /// and the rule label. Incomplete actions (cross-target
        /// inputs deferred to phase 5.C) are skipped — their
        /// outputs may legitimately not exist when the legacy
        /// build path takes over. Gated until rules are audited;
        /// phase 5.C.3 flips the default. Equivalent to setting
        /// `RUSH_VALIDATE_OUTPUTS=1`.
        #[arg(long)]
        validate_outputs: bool,

        /// Phase 5.C.4: dispatch scheduler-eligible targets
        /// through the action graph instead of legacy build().
        ///
        /// A target is scheduler-eligible iff every Complete
        /// action it owns has an `ExecutableAction` populated by
        /// the rule's analyze() (i.e., the rule has been
        /// migrated). Eligible targets bypass `rule.build()`
        /// entirely; ineligible targets continue through the
        /// legacy path. Mixed builds work — scheduler and
        /// legacy targets interleave in topological order. Use
        /// `--validate-producers` to see scheduler-plan counts
        /// (`N ready, M legacy-complete, K legacy-incomplete`)
        /// and target eligibility before flipping this flag.
        /// Equivalent to setting `RUSH_USE_SCHEDULER=1`.
        #[arg(long)]
        use_scheduler: bool,

        /// Phase 5.C.4 commit 4: verify scheduler argv matches
        /// legacy.
        ///
        /// For every scheduler-eligible target, materialize both
        /// the legacy command spec (via the rule's
        /// build_legacy_command_specs) and the scheduler command
        /// spec (via ExecutableAction.materialize), normalize
        /// both, and diff per-field (mnemonic, executable, cwd,
        /// argv preserving order, env, declared inputs,
        /// declared outputs, toolchain identity, param files).
        /// Fails the build with a structured per-action report
        /// on any mismatch. Migration / CI gate; the
        /// verified-rule allowlist (commit 5) is what gates
        /// normal scheduler dispatch. Equivalent to setting
        /// `RUSH_VERIFY_SCHEDULER_COMMANDS=1`.
        #[arg(long)]
        verify_scheduler_commands: bool,

        /// Phase 5.C.4 commit 50: dump full ActionKeySpec per
        /// Ready action.
        ///
        /// For every Ready action in the SchedulerPlan,
        /// materialize a NormalizedCommandSpec, build an
        /// ActionKeySpec (classified inputs, classified
        /// outputs, cacheability), and dump via
        /// format_action_key_spec. Used to inspect what the
        /// future cache key would consider for each action —
        /// surfaces HostAmbientInclude / HostToolPath /
        /// Unclassified entries that block remote-caching.
        /// Equivalent to setting `RUSH_EXPLAIN_ACTION_KEYS=1`.
        ///
        /// Pair with `--validate-producers` (which is what
        /// runs the analyze pass that this hooks into).
        #[arg(long)]
        explain_action_keys: bool,

        /// Phase 5.C.4 commit 69: view selector for
        /// `--explain-action-keys`.
        ///
        /// Values: `spec` (default), `canonical`, `both`.
        ///
        /// - `spec`      → current ActionKeySpec output only
        /// - `canonical` → CanonicalActionKeyInput +
        ///                 readiness report only
        /// - `both`      → ActionKeySpec +
        ///                 CanonicalActionKeyInput +
        ///                 readiness
        ///
        /// Pro's commit-69 directive: "Do not double output
        /// volume by default." Equivalent to setting
        /// `RUSH_EXPLAIN_ACTION_KEYS_VIEW=spec|canonical|both`.
        #[arg(long, value_name = "VIEW")]
        explain_action_keys_view: Option<String>,

        /// Phase 5.C.4 commit 71: scheduler-preflight surface
        /// selector. Values: `off` (default), `human`, `json`.
        /// CLI overrides env. Env aliases:
        /// `RUSH_SCHEDULER_PREFLIGHT=1` (human),
        /// `RUSH_SCHEDULER_PREFLIGHT_JSON=1` (json).
        #[arg(long, value_name = "MODE")]
        scheduler_preflight: Option<String>,

        /// Phase 5.C.4 commit 71: legacy-breakdown surface
        /// selector. Values: `off`, `human` (default), `json`.
        /// CLI overrides env. Env alias:
        /// `RUSH_LEGACY_BREAKDOWN_JSON=1` (json).
        #[arg(long, value_name = "MODE")]
        legacy_breakdown: Option<String>,

        /// Phase 5.C.4 commit 71: format selector for
        /// `--explain-action-keys`. Values: `human` (default),
        /// `json`. Distinct from
        /// `--explain-action-keys-view` (body shape);
        /// this controls destination (stderr human vs
        /// stdout JSON). CLI overrides env. Env alias:
        /// `RUSH_EXPLAIN_ACTION_KEYS_JSON=1`.
        #[arg(long, value_name = "FORMAT")]
        explain_action_keys_format: Option<String>,

        /// Phase 5.C.4 commit 71: format selector for
        /// `--explain-action-graph`. Values: `human` (default),
        /// `json`. CLI overrides env. Env alias:
        /// `RUSH_EXPLAIN_ACTION_GRAPH_JSON=1`.
        #[arg(long, value_name = "FORMAT")]
        explain_action_graph_format: Option<String>,

        /// Phase 5.C.5a Lane A step A2 (Pro 2026-05-26):
        /// scheduler-authoritative mode. Values: `off`
        /// (default), `verified`.
        ///
        /// `off`: current behavior — scheduler optional,
        /// legacy fallback always available.
        ///
        /// `verified`: for targets whose rule_kind maps to
        /// a `VerifiedExecutionShape` (cc_library,
        /// cc_binary, cc_test, proto, system_cc_library,
        /// header-only, external-system-stub), legacy
        /// invocation is forbidden. The orchestrator
        /// bails with a structured per-target violation
        /// error if the legacy path is reached for a
        /// verified shape.
        ///
        /// Use with `--use-scheduler` so verified-shape
        /// targets dispatch through the scheduler path.
        ///
        /// CLI overrides env. Env alias:
        /// `RUSH_SCHEDULER_AUTHORITATIVE=off|verified`.
        #[arg(long, value_name = "MODE")]
        scheduler_authoritative: Option<String>,

        /// Phase 5.C.4 commit 124 (Batch P): tri-state
        /// selector for the experimental size-estimate
        /// reuse path on repeat-miss `.bzl` reloads.
        ///
        /// Values:
        ///   - `off` (default): disable reuse;
        ///     `estimate_symbols_size` recomputes on
        ///     every load.
        ///   - `on`: enable reuse. When the fingerprint
        ///     matches the prior load, skip the
        ///     recursive Value walk and use the
        ///     recorded estimate.
        ///   - `verify`: enable reuse AND recompute for
        ///     comparison. Mismatches counted but not
        ///     fatal. Use as canary mode.
        ///
        /// CLI wins over env. Env aliases:
        ///   `RUSH_BZL_REUSE_SIZE_ESTIMATE=1` → `on`
        ///   `RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY=1` → `verify`
        ///
        /// Marked EXPERIMENTAL: validated on
        /// `//tensorflow/core/platform:macros` only
        /// (commit 121: 1.91s wall, 43% CPU reduction,
        /// 87.5% reuse hit rate, 0 verification
        /// mismatches). Default stays off pending
        /// validation on a larger workload (see
        /// `docs/analysis/batch-o-tf-macros-timing.md`).
        #[arg(long, value_name = "MODE")]
        bzl_size_estimate_reuse: Option<String>,

        /// Phase 5.C.4 commit 61: dump per-target
        /// action-graph explain.
        ///
        /// Distinct from `--explain-action-keys`. Action-key
        /// explain shows per-action argv/inputs/cacheability
        /// (cache/debug). Action-graph explain shows per-
        /// target structure (correctness): rule kind,
        /// scheduler support level, required tools,
        /// dependencies, declared actions with verdict.
        ///
        /// Pair with `--analyze-only --use-scheduler`.
        /// Equivalent to setting `RUSH_EXPLAIN_ACTION_GRAPH=1`.
        /// JSON output via `RUSH_EXPLAIN_ACTION_GRAPH_JSON=1`.
        #[arg(long)]
        explain_action_graph: bool,

        /// Phase 5.C.4 commit 10: load test targets matched by
        /// wildcard patterns.
        ///
        /// By default, test targets (cc_test, py_test, sh_test,
        /// java_test, tf_cc_test, tf_py_test, test_suite, plus
        /// targets ending in `_test` / `_tests` / `_benchmark` /
        /// `_test_lib`) are skipped during BUILD-file loading
        /// to prevent transitive-dependency explosion / OOM on
        /// TF-scale workloads. `--include-tests` disables that
        /// filter so cc_test verification can run on
        /// hello_cpp-scale workloads.
        ///
        /// **Explicitly named test targets are always honored,
        /// regardless of this flag.** If you type
        /// `rush build //pkg:my_test`, the test loads even
        /// without `--include-tests`. The filter only applies
        /// to test targets pulled in by wildcard patterns
        /// (`//...`, `:all`, `/...`) or as transitive
        /// dependencies.
        ///
        /// Replaces the old `RUSH_INCLUDE_TESTS=1` env-var
        /// switch (which still works as internal plumbing).
        #[arg(long)]
        include_tests: bool,

        /// Enforce visibility on config_setting labels used by select().
        #[arg(
            long = "incompatible_enforce_config_setting_visibility",
            default_value_t = true,
            default_missing_value = "true",
            num_args = 0..=1,
            require_equals = true,
            value_parser = parse_bazel_bool,
            action = clap::ArgAction::Set,
            overrides_with = "incompatible_enforce_config_setting_visibility"
        )]
        incompatible_enforce_config_setting_visibility: bool,

        /// Bazel-compatible negated spelling for the preceding option.
        #[arg(
            long = "noincompatible_enforce_config_setting_visibility",
            action = clap::ArgAction::SetTrue,
            overrides_with = "noincompatible_enforce_config_setting_visibility"
        )]
        noincompatible_enforce_config_setting_visibility: bool,

        /// Make omitted config_setting visibility follow ordinary defaults.
        #[arg(
            long = "incompatible_config_setting_private_default_visibility",
            default_value_t = false,
            default_missing_value = "true",
            num_args = 0..=1,
            require_equals = true,
            value_parser = parse_bazel_bool,
            action = clap::ArgAction::Set,
            overrides_with =
                "incompatible_config_setting_private_default_visibility"
        )]
        incompatible_config_setting_private_default_visibility: bool,

        /// Bazel-compatible negated spelling for the preceding option.
        #[arg(
            long = "noincompatible_config_setting_private_default_visibility",
            action = clap::ArgAction::SetTrue,
            overrides_with =
                "noincompatible_config_setting_private_default_visibility"
        )]
        noincompatible_config_setting_private_default_visibility: bool,

        /// Accept public/private package_group package specifications.
        #[arg(
            long = "incompatible_package_group_has_public_syntax",
            default_value_t = true,
            default_missing_value = "true",
            num_args = 0..=1,
            require_equals = true,
            value_parser = parse_bazel_bool,
            action = clap::ArgAction::Set,
            overrides_with = "incompatible_package_group_has_public_syntax"
        )]
        incompatible_package_group_has_public_syntax: bool,

        /// Bazel-compatible negated spelling for the preceding option.
        #[arg(
            long = "noincompatible_package_group_has_public_syntax",
            action = clap::ArgAction::SetTrue,
            overrides_with = "noincompatible_package_group_has_public_syntax"
        )]
        noincompatible_package_group_has_public_syntax: bool,

        /// Keep //... package_group patterns local to their repository.
        #[arg(
            long = "incompatible_fix_package_group_reporoot_syntax",
            default_value_t = true,
            default_missing_value = "true",
            num_args = 0..=1,
            require_equals = true,
            value_parser = parse_bazel_bool,
            action = clap::ArgAction::Set,
            overrides_with = "incompatible_fix_package_group_reporoot_syntax"
        )]
        incompatible_fix_package_group_reporoot_syntax: bool,

        /// Bazel-compatible negated spelling for the preceding option.
        #[arg(
            long = "noincompatible_fix_package_group_reporoot_syntax",
            action = clap::ArgAction::SetTrue,
            overrides_with = "noincompatible_fix_package_group_reporoot_syntax"
        )]
        noincompatible_fix_package_group_reporoot_syntax: bool,

        /// Persistent-daemon routing: auto | on | off (default: auto).
        ///
        /// `auto` (the DEFAULT) discovers — or spawns on demand — the resident
        /// `rushd` for this workspace, sends the command over the Unix-domain
        /// socket, and streams the result back, so a SECOND `rush build`
        /// process reuses the daemon's in-memory analyzed graph (the warm
        /// no-op collapse). `on` is an alias for `auto`. On ANY daemon problem
        /// — no daemon, spawn failure, connect/handshake failure, protocol
        /// version mismatch, a daemon that dies mid-request, or stale
        /// metadata/cookie — it falls back SILENTLY to the in-process build
        /// and produces the identical result. `off` forces the in-process
        /// path (no daemon involved at all). The default is `auto`: the daemon
        /// is the default build path and can never break or slow a build
        /// relative to in-process, because every failure degrades to it.
        #[arg(long, default_value = "auto", value_name = "MODE")]
        daemon: String,

        /// Filesystem-watcher mode for the daemon's warm no-op: auto | off |
        /// require (default: auto).
        ///
        /// `auto` (the DEFAULT) uses the daemon's inotify dirty-set watcher on a
        /// known-good local filesystem so a healthy warm no-op validates ZERO
        /// paths (skips the manifest stat-walk), else falls back to a scan. `off`
        /// always scans (the pre-watcher behavior). `require` errors if the
        /// watcher is unavailable/unhealthy (CI / debugging). The flag is passed
        /// through to the daemon via argv; on the in-process path it is inert
        /// (the in-process build always scans). Bazel's equivalent is
        /// `--watchfs` (default off); rush's watcher is on by default.
        #[arg(long, default_value = "auto", value_name = "MODE")]
        watchfs: String,
    },

    /// Execute a complete external action graph without WORKSPACE or Starlark.
    #[command(after_help = "\
SUPPORTED MANIFEST CONTRACT:
  Executes every imported action once. Cache, depfile, restat, and always-run
  semantics are rejected. Commands run directly; Make is not a graph evaluator.

EXAMPLES:
  rush build-manifest kbuild-actions.jsonl -j 16
  rush build-manifest kbuild-actions.jsonl --error-format=json
")]
    BuildManifest {
        /// Versioned JSONL action-graph manifest.
        manifest: PathBuf,

        /// Maximum logical actions admitted concurrently.
        #[arg(long, short = 'j')]
        jobs: Option<usize>,

        /// Inherited fd for an obj-directory flock held across bootstrap/export.
        #[arg(long, value_name = "FD")]
        output_lock_fd: Option<i32>,

        /// Error rendering; JSON reserves stdout for its diagnostic object.
        #[arg(long, value_enum, default_value_t = ActionManifestErrorFormat::Human)]
        error_format: ActionManifestErrorFormat,
    },

    /// Run the persistent build daemon (rushd).
    ///
    /// Holds a long-lived BuildOrchestrator so the analyzed-graph snapshot
    /// cache persists across client `rush build --daemon=auto` requests, serving
    /// the rush.command.v1 RPC surface over a Unix-domain socket. Writes a
    /// discovery metadata file (socket path + auth cookie) so clients can find
    /// it without a hardcoded socket name. Serves until idle-timeout or an
    /// explicit shutdown RPC.
    #[command(after_help = "\
EXAMPLES:
  rush daemon                          Start rushd for the current workspace
  rush daemon --idle-timeout=600       Exit after 10 min idle

Clients reach the daemon with: rush build --daemon=auto //target
")]
    Daemon {
        /// Idle timeout in seconds before the daemon shuts itself down.
        /// 0 disables the idle timeout (serve until an explicit shutdown).
        #[arg(long, default_value_t = 3600)]
        idle_timeout: u64,

        /// Verbose output.
        #[arg(long, short = 'v')]
        verbose: bool,
    },

    /// Run tests for specified targets
    #[command(visible_alias = "t")]
    #[command(after_help = "\
EXAMPLES:
  rush test //...                        Run all tests
  rush test //pkg:all                    Run all tests in package
  rush test //pkg:mytest                 Run specific test
  rush test --test_output=all //...      Verbose test output
  rush test --jobs=8 //...               Run tests in parallel

FLAGS:
  --test_output=<mode>      Test output: summary, errors, all
  --test_filter=<pattern>   Filter tests by name
  --jobs=<n>                Number of parallel test jobs
  --verbose, -v             Verbose output
")]
    Test {
        /// Test target labels (e.g., //pkg:test, //..., //pkg:all)
        #[arg(required = true)]
        targets: Vec<String>,

        /// Test output mode: summary, errors, or all
        #[arg(long, default_value = "summary")]
        test_output: String,

        /// Filter tests by name pattern
        #[arg(long)]
        test_filter: Option<String>,

        /// Number of parallel test jobs
        #[arg(long, short = 'j')]
        jobs: Option<u32>,

        /// Verbose output
        #[arg(long, short = 'v')]
        verbose: bool,
    },

    /// Run a binary target
    #[command(visible_alias = "r")]
    #[command(after_help = "\
EXAMPLES:
  rush run //cmd/app:main              Run a binary
  rush run //cmd/app:main -- arg1      Run with arguments
  rush run --config=release //cmd/app  Run optimized binary

Arguments after -- are passed to the binary.
")]
    Run {
        /// Binary target label to run (e.g., //cmd/app:main)
        #[arg(required = true)]
        target: String,

        /// Arguments to pass to the binary (after --)
        #[arg(last = true)]
        args: Vec<String>,

        /// Apply named configuration from .bazelrc
        #[arg(long)]
        config: Option<String>,

        /// Verbose output
        #[arg(long, short = 'v')]
        verbose: bool,
    },

    /// Clean build outputs
    #[command(after_help = "\
EXAMPLES:
  rush clean                           Remove all build outputs
  rush clean --expunge                 Deep clean (including cache)
  rush clean --cache                   Clean disk cache only
  rush clean --async                   Async clean (fast)

FLAGS:
  --expunge                 Delete all Rush data (outputs + cache)
  --cache                   Clean disk cache only
  --async                   Async clean (non-blocking)
  --verbose, -v             Verbose output
")]
    Clean {
        /// Deep clean - remove all Rush data
        #[arg(long)]
        expunge: bool,

        /// Clean disk cache only
        #[arg(long)]
        cache: bool,

        /// Async clean (fast, non-blocking)
        #[arg(long)]
        async_clean: bool,

        /// Verbose output
        #[arg(long, short = 'v')]
        verbose: bool,
    },

    /// Inspect or clear rush's caches
    #[command(after_help = "\
DESCRIPTION:
  The action cache and content-addressed store are MACHINE-GLOBAL: every
  workspace on this machine shares them. `rush clean --expunge` therefore
  does NOT touch them — it only removes state owned by the current
  workspace. Destroying shared state is spelled out explicitly here.

EXAMPLES:
  rush cache info                 Show cache roots and sizes
  rush cache info --json          Machine-readable, for scripts
  rush cache clear --all          Delete the machine-global cache
")]
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },

    /// Show version information
    #[command(after_help = "\
Shows Rush version and build information.

In Bazel compatibility mode (when invoked as 'bazel'), also displays
compatibility status.
")]
    Version,

    /// Query the build graph and targets
    #[command(visible_alias = "q")]
    #[command(after_help = "\
EXAMPLES:
  rush query '//...'                       List all targets
  rush query '//pkg:all'                   List all targets in package
  rush query 'deps(//pkg:target)'          Show dependencies
  rush query 'rdeps(//pkg:target)'         Show reverse dependencies
  rush query 'kind(cc_test, //...)'        Filter by rule type

QUERY EXPRESSIONS:
  //...                     All targets in workspace
  //pkg:all                 All targets in a package
  //pkg/sub/...             All targets in pkg/sub and subdirs
  deps(pattern)             Dependencies of matching targets
  rdeps(pattern)            Reverse dependencies
  kind(rule_type, pattern)  Filter targets by rule type

OUTPUT FORMATS:
  --output=text            Human-readable text (default)
  --output=json            JSON format
  --output=proto           Protocol buffer format (not implemented)

FLAGS:
  --output=<format>        Output format
  --verbose, -v            Verbose output
")]
    Query {
        /// Query expression to evaluate
        #[arg(required = true)]
        query: String,

        /// Output format: text, json, or proto
        #[arg(long, default_value = "text")]
        output: String,

        /// Verbose output
        #[arg(long, short = 'v')]
        verbose: bool,
    },

    /// Watch mode - rebuild on file changes
    #[command(visible_alias = "w")]
    #[command(after_help = "\
DESCRIPTION:
  Watch mode continuously monitors your source files and automatically rebuilds
  when changes are detected. Perfect for rapid development iteration.

EXAMPLES:
  rush watch //pkg:target              Watch and rebuild target
  rush watch //...                     Watch and rebuild everything
  rush watch //pkg:binary --config=dbg Watch with debug config

HOW IT WORKS:
  - Monitors source files (*.cc, *.rs, *.py, BUILD files)
  - Debounces rapid changes (waits 500ms)
  - Rebuilds only affected targets
  - Shows clear feedback on what changed and build status
")]
    Watch {
        /// Target labels to build on changes
        #[arg(required = true)]
        targets: Vec<String>,

        /// Apply named configuration from .bazelrc
        #[arg(long)]
        config: Option<String>,

        /// Compilation mode: fastbuild, opt, or dbg
        #[arg(long, short = 'c')]
        compilation_mode: Option<String>,

        /// Number of parallel jobs
        #[arg(long, short = 'j')]
        jobs: Option<u32>,

        /// Verbose output
        #[arg(long, short = 'v')]
        verbose: bool,
    },

    /// Generate dependency graph visualization
    #[command(after_help = "\
DESCRIPTION:
  Generate a Graphviz DOT file representing the build dependency graph.
  Use with graphviz tools to create visual diagrams:
    rush graph //... > graph.dot && dot -Tpng graph.dot -o graph.png

EXAMPLES:
  rush graph //...                     Full workspace graph
  rush graph //pkg:target              Single target and its dependencies
  rush graph //pkg:target -o graph.dot Write to file

COLORS:
  - Light blue:   cc_library targets
  - Light green:  cc_binary targets
  - Light yellow: cc_test targets
")]
    Graph {
        /// Target to visualize (or //... for all)
        target: Option<String>,

        /// Output file (stdout if not specified)
        #[arg(long, short = 'o')]
        output: Option<String>,
    },

    /// Show workspace information and statistics
    #[command(after_help = "\
DESCRIPTION:
  Display information about the Rush workspace including target counts,
  build status, cache statistics, and incremental build state.

EXAMPLES:
  rush info                            Show workspace overview
  rush info --verbose                  Detailed workspace information
")]
    Info {
        /// Show detailed information
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Print one resolved path, unadorned, for scripts:
        /// cache-root | action-cache-root | cas-dir.
        ///
        /// A benchmark that wants a genuinely cold run must delete what
        /// rush actually opens; ask rush instead of reconstructing it.
        #[arg(long, value_name = "KEY")]
        path: Option<String>,
    },

    /// Initialize a new Rush workspace
    #[command(after_help = "\
DESCRIPTION:
  Create a new Rush workspace with WORKSPACE file and example projects.
  Can initialize in the current directory or create a new directory.

EXAMPLES:
  rush init                            Initialize in current directory
  rush init myproject                  Create and initialize myproject/
  rush init --template=cpp myproject   Initialize with C++ example project
")]
    Init {
        /// Directory to initialize (current dir if not specified)
        directory: Option<PathBuf>,

        /// Template to use: minimal, cpp, rust
        #[arg(long, default_value = "minimal")]
        template: String,

        /// Force initialization even if directory is not empty
        #[arg(long)]
        force: bool,
    },

    /// Import Bazel BUILD files to Rush format
    #[command(after_help = "\
DESCRIPTION:
  Convert Bazel BUILD files and WORKSPACE to Rush-compatible format.
  Useful for migrating existing Bazel projects to Rush.

EXAMPLES:
  rush import-bazel                    Import BUILD files in current workspace
  rush import-bazel --workspace=/path  Import from specific workspace
  rush import-bazel --validate         Import and validate conversion
  rush import-bazel --dry-run          Show what would be converted
")]
    ImportBazel {
        /// Workspace root (current dir if not specified)
        #[arg(long)]
        workspace: Option<PathBuf>,

        /// Output directory for converted files
        #[arg(long)]
        output_dir: Option<PathBuf>,

        /// Validate converted BUILD files
        #[arg(long)]
        validate: bool,

        /// Dry run - show what would be converted without writing
        #[arg(long)]
        dry_run: bool,

        /// Verbose output
        #[arg(long, short = 'v')]
        verbose: bool,
    },
}

impl Cli {
    /// Get CPU times (user, system) from /proc/self/stat on Linux.
    /// Returns None on non-Linux platforms or if reading fails.
    #[cfg(target_os = "linux")]
    fn get_cpu_times() -> Option<(f64, f64)> {
        use std::fs;

        let stat = fs::read_to_string("/proc/self/stat").ok()?;
        let fields: Vec<&str> = stat.split_whitespace().collect();

        // /proc/self/stat format (man 5 proc):
        // Field 14: utime (user mode jiffies)
        // Field 15: stime (kernel mode jiffies)
        // Clock ticks per second (usually 100 on Linux)
        let clock_ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;

        if fields.len() > 14 {
            let utime: u64 = fields[13].parse().ok()?;
            let stime: u64 = fields[14].parse().ok()?;

            let user_secs = utime as f64 / clock_ticks_per_sec;
            let sys_secs = stime as f64 / clock_ticks_per_sec;

            Some((user_secs, sys_secs))
        } else {
            None
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn get_cpu_times() -> Option<(f64, f64)> {
        None
    }

    /// Run the CLI
    pub async fn run() -> Result<()> {
        // Check if invoked as "bazel" for drop-in replacement.
        // Phase 5.C.4 commit 70: this is a build-startup mode
        // banner, not user-requested machine output — route to
        // stderr so JSON build modes get a clean stdout.
        if is_bazel_mode() {
            eprintln!("Rush v{} (running in Bazel compatibility mode)", VERSION);
            eprintln!("Detected invocation as 'bazel' - using Bazel CLI syntax");
        }

        let cli = Cli::parse();

        match cli.command {
            Some(Commands::RemoteExecute { manifest, endpoint, execution_root,
                instance, deadline_seconds, report }) => {
                use std::io::Write;
                let spec: rush_remote::reapi::ActionSpec =
                    serde_json::from_slice(&std::fs::read(manifest)?)?;
                let deadline = std::time::Duration::from_secs(deadline_seconds.into());
                let outcome = tokio::time::timeout(deadline, async {
                    let client = rush_remote::reapi::ReapiClient::connect(endpoint, instance).await?;
                    client.execute(&execution_root, &spec, deadline).await
                }).await.context("remote execution client deadline exceeded")??;
                std::io::stdout().write_all(&outcome.stdout)?;
                std::io::stderr().write_all(&outcome.stderr)?;
                if let Some(path) = report {
                    std::fs::write(path, serde_json::to_vec_pretty(&outcome)?)?;
                }
                eprintln!("remote action {}: exit={} cached={}", outcome.action_digest,
                    outcome.exit_code, outcome.cached);
                if outcome.exit_code != 0 {
                    std::process::exit(if (1..=255).contains(&outcome.exit_code) {
                        outcome.exit_code
                    } else { 1 });
                }
                ()
            }
            Some(Commands::Build {
                targets,
                config,
                defines,
                compilation_mode,
                jobs,
                disk_cache,
                disk_cache_dir,
                remote_executor,
                remote_platform,
                remote_instance,
                remote_cache,
                benchmark,
                profile,
                build_event_json,
                verbose,
                progress,
                streaming,
                metrics_json,
                metrics_csv,
                max_memory_gb,
                analyze_only,
                validate_producers,
                validate_outputs,
                use_scheduler,
                verify_scheduler_commands,
                explain_action_keys,
                explain_action_keys_view,
                scheduler_preflight,
                legacy_breakdown,
                explain_action_keys_format,
                explain_action_graph_format,
                bzl_size_estimate_reuse,
                explain_action_graph,
                include_tests,
                incompatible_enforce_config_setting_visibility: _,
                noincompatible_enforce_config_setting_visibility: _,
                incompatible_config_setting_private_default_visibility: _,
                noincompatible_config_setting_private_default_visibility: _,
                incompatible_package_group_has_public_syntax: _,
                noincompatible_package_group_has_public_syntax: _,
                incompatible_fix_package_group_reporoot_syntax: _,
                noincompatible_fix_package_group_reporoot_syntax: _,
                scheduler_authoritative,
                daemon,
                watchfs,
            }) => {
                // Validate --watchfs early (a bad value is an error). The flag is
                // forwarded to the daemon via argv (the orchestrator reads the
                // mode there per request); it is deliberately NOT published as an
                // env var, because env is folded into the config fingerprint and
                // the watcher mode must not perturb the reuse key. On the
                // in-process path the watcher is unarmed, so the flag is inert
                // (the in-process build always scans). The daemon always ARMS the
                // watcher (cheap); `--watchfs=off` is honored per request by the
                // orchestrator simply not consulting it.
                match watchfs.to_ascii_lowercase().as_str() {
                    "auto" | "off" | "require" | "on" => {}
                    other => {
                        return Err(anyhow::anyhow!(
                            "--watchfs: unknown mode {other:?} (expected: auto, off, require)"
                        ));
                    }
                }

                // Expand the workspace rc before daemon routing. Nested and
                // repeated --config occurrences are replaced in place among
                // raw CLI visibility flags, then the resulting ordered stream
                // travels in the client's environment snapshot.
                let workspace_root = rush_build::find_workspace_root()?;
                let bazelrc_expansion = load_bazelrc_build_expansion(
                    &workspace_root,
                    &process_command_tokens(),
                    verbose,
                )?;
                publish_effective_visibility_flags(
                    &bazelrc_expansion.effective_visibility_flags,
                );
                let bazelrc_flags = bazelrc_expansion.rc_flags;

                // Bazel `--define KEY=VALUE` build settings. Publish them via
                // the RUSH_DEFINES env var BEFORE any BUILD parsing so
                // rush_starlark::config_defines (lazily seeded from that env)
                // picks them up for config_setting(define_values=...) matching
                // and the select()s it drives. NOT emitted as a compiler -D.
                // Going through the env keeps rush-cli free of a direct
                // rush-starlark dependency and merges with any pre-set value.
                if !defines.is_empty() {
                    for d in &defines {
                        if d.split_once('=').is_none() {
                            return Err(anyhow::anyhow!(
                                "invalid --define {:?} (expected KEY=VALUE)",
                                d
                            ));
                        }
                    }
                    let mut merged = std::env::var("RUSH_DEFINES").unwrap_or_default();
                    for d in &defines {
                        if !merged.is_empty() {
                            merged.push(' ');
                        }
                        merged.push_str(d);
                    }
                    std::env::set_var("RUSH_DEFINES", merged);
                }

                // Phase 5.C.5a Lane A step A2: plumb the CLI
                // selector to the orchestrator via the
                // RUSH_SCHEDULER_AUTHORITATIVE env var.
                // AuthoritativeMode::resolve() in the
                // orchestrator reads CLI override first via
                // this env (CLI overrides any pre-existing
                // env). Validation happens here so an
                // invalid value fails before the orchestrator
                // boots.
                if let Some(ref mode) = scheduler_authoritative {
                    // Parse to validate; bail on invalid.
                    if mode != "off" && mode != "verified" {
                        return Err(anyhow::anyhow!(
                            "invalid --scheduler-authoritative={:?} \
                             (expected: off, verified)",
                            mode
                        ));
                    }
                    std::env::set_var(
                        "RUSH_SCHEDULER_AUTHORITATIVE",
                        mode,
                    );
                }
                // Persistent-daemon Run-wiring: branch BEFORE the in-process
                // orchestrator. `--daemon=auto` (the DEFAULT) and its alias
                // `on` discover — or spawn on demand — rushd, send the command
                // over the wire (with the client's argv/env/cwd +
                // workspace-config digest), stream the result back, and
                // propagate the exit code, so a SECOND process reuses the
                // daemon's retained graph.
                //
                // BULLETPROOF FALLBACK: `run_build_via_daemon` returns `Err`
                // ONLY when the daemon path is UNAVAILABLE — no daemon, spawn
                // failure, connect/handshake failure, protocol version
                // mismatch, a daemon that dies mid-request before reporting a
                // result, or stale metadata/cookie. In every such case we fall
                // through to the in-process build, which produces the identical
                // correct result. A daemon that actually RAN the build and
                // reported an exit code returns `Ok(code)` — a real build
                // result we honor (success or genuine build failure), never a
                // reason to fall back. So the daemon being the default can
                // never break or slow a build relative to in-process.
                //
                // The fallback is SILENT by default (a missing/cold daemon is
                // the normal first-build state, not an error to warn about);
                // `--verbose` surfaces the reason for diagnosis. `--daemon=off`
                // forces the in-process path with no daemon involvement.
                // Analysis-INTROSPECTION requests are not "real build"
                // workloads and are not served by the daemon's build verb:
                // `--analyze-only`, the `--explain-action-*` family, the
                // `--aquery` action-graph export (RUSH_AQUERY_OUT), and the
                // per-action HIT/MISS cache trace (RUSH_ACTION_TRACE). These
                // must run IN-PROCESS so they actually produce their
                // introspection output — routing them through the daemon (whose
                // build runs in a separate process and would not write the
                // client-expected trace/graph files) would silently drop it.
                // Force the in-process path for them regardless of
                // --daemon=auto. (This is not a fallback; it is the correct
                // home for these flags. `--daemon=off` is unaffected.)
                let introspection_request = analyze_only
                    || explain_action_keys
                    || explain_action_graph
                    || explain_action_keys_view.is_some()
                    || explain_action_keys_format.is_some()
                    || explain_action_graph_format.is_some()
                    || std::env::var_os("RUSH_AQUERY_OUT").is_some()
                    || std::env::var_os("RUSH_ACTION_TRACE").is_some();

                // Validate the requested mode FIRST (a bad --daemon=value is an
                // error even for an introspection request), THEN coerce an
                // introspection request to the in-process path.
                let daemon_mode = daemon.to_ascii_lowercase();
                match daemon_mode.as_str() {
                    "off" | "auto" | "on" => {}
                    other => {
                        return Err(anyhow::anyhow!(
                            "--daemon: unknown mode {other:?} (expected: auto, on, off)"
                        ));
                    }
                }
                let effective_mode = if introspection_request || remote_executor.is_some() { "off" } else { daemon_mode.as_str() };
                if matches!(effective_mode, "auto" | "on") {
                    match Self::run_build_via_daemon(verbose).await {
                        Ok(exit_code) => {
                            if exit_code != 0 {
                                std::process::exit(exit_code);
                            }
                            return Ok(());
                        }
                        Err(e) => {
                            if verbose {
                                eprintln!(
                                    "ℹ️  daemon path unavailable ({e}); building in-process"
                                );
                            }
                        }
                    }
                }
                Self::run_build(
                    targets,
                    config,
                    bazelrc_flags,
                    compilation_mode,
                    jobs,
                    disk_cache,
                    disk_cache_dir,
                    remote_executor,
                    remote_platform,
                    remote_instance,
                    remote_cache,
                    benchmark,
                    profile,
                    build_event_json,
                    verbose,
                    progress,
                    streaming,
                    metrics_json,
                    metrics_csv,
                    max_memory_gb,
                    analyze_only,
                    validate_producers,
                    validate_outputs,
                    use_scheduler,
                    verify_scheduler_commands,
                    explain_action_keys,
                    explain_action_keys_view,
                    scheduler_preflight,
                    legacy_breakdown,
                    explain_action_keys_format,
                    explain_action_graph_format,
                    bzl_size_estimate_reuse,
                    explain_action_graph,
                    include_tests,
                )
                .await?;
            }
            Some(Commands::BuildManifest {
                manifest,
                jobs,
                output_lock_fd,
                error_format,
            }) => {
                let jobs = jobs.unwrap_or_else(|| {
                    std::thread::available_parallelism()
                        .map(|count| count.get())
                        .unwrap_or(1)
                });
                let output_mode = match error_format {
                    ActionManifestErrorFormat::Human => ActionManifestOutputMode::Human,
                    ActionManifestErrorFormat::Json => ActionManifestOutputMode::JsonDiagnostic,
                };
                #[cfg(unix)]
                let (execution_result, signal_exit) = {
                    let mut terminate =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
                    let mut hangup =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
                    let mut quit =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())?;
                    let execution = execute_action_manifest_with_lock_fd_and_output_mode(
                        &manifest,
                        jobs,
                        output_lock_fd,
                        output_mode,
                    );
                    tokio::pin!(execution);
                    tokio::select! {
                        result = &mut execution => (Some(result), None),
                        interrupt = tokio::signal::ctrl_c() => {
                            interrupt?;
                            (None, Some(130))
                        }
                        _ = terminate.recv() => (None, Some(143)),
                        _ = hangup.recv() => (None, Some(129)),
                        _ = quit.recv() => (None, Some(131)),
                    }
                };
                #[cfg(not(unix))]
                let (execution_result, signal_exit) = {
                    let execution = execute_action_manifest_with_lock_fd_and_output_mode(
                        &manifest,
                        jobs,
                        output_lock_fd,
                        output_mode,
                    );
                    tokio::pin!(execution);
                    tokio::select! {
                        result = &mut execution => (Some(result), None),
                        interrupt = tokio::signal::ctrl_c() => {
                            interrupt?;
                            (None, Some(130))
                        }
                    }
                };
                if let Some(exit_code) = signal_exit {
                    eprintln!("external action-manifest execution interrupted");
                    std::process::exit(exit_code);
                }
                match execution_result.expect("completed manifest execution has a result") {
                    Ok(summary) => {
                        println!(
                            "Executed {} actions ({} commands); external manifest execution is \
                             cache-disabled",
                            summary.actions_executed, summary.commands_executed
                        );
                    }
                    Err(error) => match error_format {
                        ActionManifestErrorFormat::Human => return Err(error.into()),
                        ActionManifestErrorFormat::Json => {
                            println!("{}", error.to_json());
                            std::process::exit(2);
                        }
                    },
                }
            }
            Some(Commands::Daemon {
                idle_timeout,
                verbose,
            }) => {
                Self::run_daemon(idle_timeout, verbose).await?;
            }
            Some(Commands::Test {
                targets,
                test_output,
                test_filter,
                jobs,
                verbose,
            }) => {
                Self::run_test(targets, test_output, test_filter, jobs, verbose).await?;
            }
            Some(Commands::Run {
                target,
                args,
                config,
                verbose,
            }) => {
                Self::run_binary(target, args, config, verbose).await?;
            }
            Some(Commands::Clean {
                expunge,
                cache,
                async_clean,
                verbose,
            }) => {
                Self::run_clean(expunge, cache, async_clean, verbose).await?;
            }
            Some(Commands::Cache { action }) => {
                Self::run_cache(action)?;
            }
            Some(Commands::Version) => {
                Self::print_version_info();
            }
            Some(Commands::Query {
                query,
                output,
                verbose,
            }) => {
                Self::run_query(query, output, verbose).await?;
            }
            Some(Commands::Watch {
                targets,
                config,
                compilation_mode,
                jobs,
                verbose,
            }) => {
                Self::run_watch(targets, config, compilation_mode, jobs, verbose).await?;
            }
            Some(Commands::Graph { target, output }) => {
                Self::run_graph(target, output).await?;
            }
            Some(Commands::Info { verbose, path }) => {
                if let Some(key) = path {
                    let resolved = match key.as_str() {
                        "cache-root" => rush_build::cache_roots::cache_root(),
                        "action-cache-root" => rush_build::cache_roots::action_cache_root(),
                        "cas-dir" => rush_build::cache_roots::cas_blob_dir(),
                        other => anyhow::bail!(
                            "unknown info path key '{}': expected \
                             cache-root, action-cache-root or cas-dir",
                            other
                        ),
                    };
                    println!("{}", resolved.display());
                } else {
                    Self::run_info(verbose).await?;
                }
            }
            Some(Commands::Init {
                directory,
                template,
                force,
            }) => {
                Self::run_init(directory, template, force).await?;
            }
            Some(Commands::ImportBazel {
                workspace,
                output_dir,
                validate,
                dry_run,
                verbose,
            }) => {
                Self::run_import_bazel(workspace, output_dir, validate, dry_run, verbose).await?;
            }
            None => {
                println!();
                println!(
                    "{} {}",
                    "🚀 Rush".bold().cyan(),
                    format!("v{}", VERSION).green()
                );
                if is_bazel_mode() {
                    println!(
                        "{}",
                        "   (Invoked as 'bazel' - try 'bazel --help')".bright_black()
                    );
                } else {
                    println!(
                        "{}",
                        "   Run 'rush --help' for usage information".bright_black()
                    );
                    println!(
                        "{}",
                        "   Run 'rush version' for detailed build information".bright_black()
                    );
                }
                println!();
            }
        }

        // Print profiling report if profiling is enabled
        #[cfg(feature = "profile")]
        rush_profiling::print_report();

        Ok(())
    }

    /// Execute build command
    ///
    /// # Clippy Suppression
    ///
    /// `too_many_arguments`: This function takes 16 parameters directly from CLI argument parsing.
    /// While a BuildOptions struct would reduce the parameter count, it would add indirection
    /// for a function that directly maps CLI args to build configuration. The parameters are
    /// well-organized (targets, config options, output options, feature flags) and the function
    /// is called from a single location (CLI parsing match statement).
    #[allow(clippy::too_many_arguments)]
    async fn run_build(
        targets: Vec<String>,
        config: Vec<String>,
        bazelrc_flags: Vec<String>,
        compilation_mode: Option<String>,
        jobs: Option<u32>,
        disk_cache: bool,
        disk_cache_dir: Option<PathBuf>,
        remote_executor: Option<String>,
        remote_platform: Vec<String>,
        remote_instance: String,
        remote_cache: Option<String>,
        _benchmark: bool,
        profile: Option<PathBuf>,
        build_event_json: Option<PathBuf>,
        verbose: bool,
        progress: bool,
        streaming: bool,
        metrics_json: Option<PathBuf>,
        metrics_csv: Option<PathBuf>,
        max_memory_gb: Option<u64>,
        analyze_only: bool,
        validate_producers: bool,
        validate_outputs: bool,
        use_scheduler: bool,
        verify_scheduler_commands: bool,
        explain_action_keys: bool,
        explain_action_keys_view: Option<String>,
        scheduler_preflight: Option<String>,
        legacy_breakdown: Option<String>,
        explain_action_keys_format: Option<String>,
        explain_action_graph_format: Option<String>,
        bzl_size_estimate_reuse: Option<String>,
        explain_action_graph: bool,
        include_tests: bool,
    ) -> Result<()> {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Instant;

        // Record start time for timing summary
        let start_time = Instant::now();
        let start_cpu_times = Self::get_cpu_times();

        // Set up Ctrl+C signal handler
        let interrupted = Arc::new(AtomicBool::new(false));
        let interrupted_clone = Arc::clone(&interrupted);

        tokio::spawn(async move {
            match tokio::signal::ctrl_c().await {
                Ok(()) => {
                    interrupted_clone.store(true, Ordering::SeqCst);
                    eprintln!("\n\n⚠️  Interrupted by Ctrl+C");
                    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                    eprintln!("📊 BUILD TIMING SUMMARY (interrupted)");
                    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

                    let elapsed = start_time.elapsed();
                    let real_secs = elapsed.as_secs_f64();

                    if let Some((user_secs, sys_secs)) = Self::get_cpu_times() {
                        if let Some((start_user, start_sys)) = start_cpu_times {
                            let user_time = user_secs - start_user;
                            let sys_time = sys_secs - start_sys;
                            let total_cpu = user_time + sys_time;
                            let ratio = if real_secs > 0.0 {
                                total_cpu / real_secs
                            } else {
                                0.0
                            };

                            eprintln!("Real time:   {:8.2}s (wall clock)", real_secs);
                            eprintln!(
                                "User time:   {:8.2}s (CPU executing in user mode)",
                                user_time
                            );
                            eprintln!("System time: {:8.2}s (CPU executing in kernel)", sys_time);
                            eprintln!("Total CPU:   {:8.2}s (user + system)", total_cpu);
                            eprintln!("Ratio:       {:8.2}  (total_cpu / real_time)", ratio);
                            eprintln!();
                            eprintln!(
                                "Parallelization efficiency: {:.1}%",
                                (ratio / num_cpus::get() as f64) * 100.0
                            );
                            eprintln!("Cores utilized: {:.1} / {}", ratio, num_cpus::get());
                        }
                    } else {
                        eprintln!("Elapsed time: {:.2}s", real_secs);
                    }

                    // Count object files to show actual build progress
                    eprintln!();
                    if let Ok(cwd) = std::env::current_dir() {
                        let output_dirs = vec![
                            cwd.join("rush-out"),
                            cwd.join("bazel-out"),
                            cwd.join("bazel-bin"),
                        ];

                        let mut total_objects = 0;
                        let mut total_binaries = 0;
                        let mut total_libs = 0;

                        for output_dir in output_dirs {
                            if output_dir.exists() {
                                for entry in walkdir::WalkDir::new(&output_dir)
                                    .into_iter()
                                    .filter_map(|e| e.ok())
                                {
                                    if entry.file_type().is_file() {
                                        if let Some(ext) = entry.path().extension() {
                                            match ext.to_str() {
                                                Some("o") => total_objects += 1,
                                                Some("so") | Some("a") => total_libs += 1,
                                                _ => {
                                                    // Check if it's an executable (no extension, has execute bit)
                                                    if entry.path().extension().is_none() {
                                                        #[cfg(unix)]
                                                        {
                                                            use std::os::unix::fs::PermissionsExt;
                                                            if let Ok(metadata) = entry.metadata() {
                                                                if metadata.permissions().mode()
                                                                    & 0o111
                                                                    != 0
                                                                {
                                                                    total_binaries += 1;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        eprintln!("📦 BUILD ARTIFACTS:");
                        eprintln!("   Object files (.o):  {}", total_objects);
                        eprintln!("   Libraries (.a/.so): {}", total_libs);
                        eprintln!("   Binaries:           {}", total_binaries);
                    }

                    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                    std::process::exit(130); // Exit code 130 = terminated by Ctrl+C
                }
                Err(err) => {
                    eprintln!("Error setting up Ctrl+C handler: {}", err);
                }
            }
        });

        // Set memory limit if provided via CLI flag
        if let Some(max_gb) = max_memory_gb {
            std::env::set_var("RUSH_MEMORY_THRESHOLD_GB", max_gb.to_string());
            eprintln!("📊 Memory limit set: {} GB (via --max-memory-gb)", max_gb);
        }

        // Find workspace root
        let workspace_root = rush_build::find_workspace_root()?;

        // Acquire build lock to prevent concurrent builds that cause memory thrashing
        let mut _build_lock = rush_build::BuildLock::new(&workspace_root)?;
        if !_build_lock.wait_acquire(300)? {
            // Wait up to 5 minutes
            anyhow::bail!(
                "Could not acquire build lock after 5 minutes. Another Rush build may be stuck."
            );
        }
        // Lock will be automatically released when _build_lock is dropped (RAII)

        if verbose {
            eprintln!("{}", "🔧 Rush Build Configuration:".bold().cyan());
            if !config.is_empty() {
                eprintln!(
                    "   {} {}",
                    "Config:".bright_black(),
                    config.join(", ").green()
                );
            }
            if let Some(ref mode) = compilation_mode {
                eprintln!("   {} {}", "Compilation mode:".bright_black(), mode.cyan());
            }
            if let Some(j) = jobs {
                eprintln!(
                    "   {} {}",
                    "Parallel jobs:".bright_black(),
                    j.to_string().cyan()
                );
            }
            if disk_cache {
                eprintln!("   {} {}", "Disk cache:".bright_black(), "enabled".green());
                if let Some(ref dir) = disk_cache_dir {
                    eprintln!(
                        "   {} {}",
                        "Cache directory:".bright_black(),
                        dir.display().to_string().cyan()
                    );
                }
            }
            if let Some(ref executor) = remote_executor {
                eprintln!(
                    "   {} {}",
                    "Remote executor:".bright_black(),
                    executor.cyan()
                );
            }
            if let Some(ref cache) = remote_cache {
                eprintln!("   {} {}", "Remote cache:".bright_black(), cache.cyan());
            }
            if !bazelrc_flags.is_empty() {
                eprintln!(
                    "   {} {} {}",
                    ".bazelrc flags:".bright_black(),
                    bazelrc_flags.len().to_string().cyan(),
                    "applied".bright_black()
                );
            }
            eprintln!();
        }

        // Parse build configuration
        let build_config = if let Some(ref mode) = compilation_mode {
            rush_build::BuildConfig::parse(mode).ok_or_else(|| {
                anyhow::anyhow!(
                    "Invalid compilation mode: {}. Use 'fastbuild', 'opt', or 'dbg'",
                    mode
                )
            })?
        } else {
            rush_build::BuildConfig::Fastbuild
        };

        // Create orchestrator with appropriate cache and build config
        let mut orchestrator = if disk_cache {
            // --disk-cache-dir must move the BLOBS too. The CAS resolves
            // its root through rush_cache::roots, so publish the choice
            // there before anything opens a store; otherwise the flag
            // splits ActionResults and blobs across two roots.
            if let Some(ref dir) = disk_cache_dir {
                std::env::set_var(rush_build::cache_roots::CACHE_ROOT_ENV, dir);
            }
            let mut cache_config = CacheConfig::default();
            if let Some(dir) = disk_cache_dir {
                cache_config.cache_root = dir;
            }
            BuildOrchestrator::with_disk_cache_and_config(
                workspace_root.clone(),
                cache_config,
                build_config,
            )?
        } else {
            BuildOrchestrator::try_with_config(workspace_root.clone(), build_config)?
        };

        // Set maximum parallel jobs if specified
        if let Some(j) = jobs {
            orchestrator.set_max_jobs(j);
            if verbose {
                eprintln!("🔧 Parallel jobs limited to: {}", j);
            }
        }

        // Configure profiling before the first workspace fetch so external
        // workspace setup participates in the Chrome trace.
        if let Some(ref profile_path) = profile {
            orchestrator = orchestrator.with_profiler(true);
            if verbose {
                eprintln!("📊 Profiling enabled: {}", profile_path.display());
            }
        }

        // Configure remote execution if requested
        if let Some(ref endpoint) = remote_executor {
            let mut platform = std::collections::BTreeMap::new();
            for value in remote_platform {
                let (key, value) = value.split_once('=').ok_or_else(||
                    anyhow::anyhow!("--remote-platform requires KEY=VALUE"))?;
                anyhow::ensure!(!key.is_empty() && !value.is_empty() &&
                    platform.insert(key.to_string(), value.to_string()).is_none(),
                    "remote platform properties must be nonempty and unique");
            }
            orchestrator.set_reapi_executor(endpoint.clone(), remote_instance, platform)?;
            if verbose {
                eprintln!("🌐 Remote execution enabled: {}", endpoint);
            }
        }

        // Configure build event stream if requested
        if let Some(event_path) = build_event_json {
            orchestrator = orchestrator.with_event_file(&event_path)?;
            if verbose {
                eprintln!("📝 Build events: {}", event_path.display());
            }
        }

        // Enable progress bar if requested
        if progress {
            orchestrator.enable_progress_bar();
        }

        // Enable output streaming if requested
        if streaming {
            orchestrator.enable_streaming();
        }

        // --analyze-only: signal the orchestrator via env var. Avoids
        // plumbing a new field through every BuildOrchestrator
        // constructor and Clone impl. The orchestrator checks this var
        // after step 4 (topological sort) and prints a census without
        // entering step 5 (action execution).
        if analyze_only {
            std::env::set_var("RUSH_ANALYZE_ONLY", "1");
        }
        // Phase 5.C0: --validate-producers turns on the
        // post-analysis producer/consumer validation sweep. The
        // orchestrator's run_producer_validation_pass walks the
        // declared-action registry + typed CcInfo contexts and
        // reports artifacts whose producers haven't been declared.
        // Same plumbing pattern as analyze_only — sets the env
        // var the orchestrator already gates on.
        if validate_producers {
            std::env::set_var("RUSH_VALIDATE_PRODUCERS", "1");
        }
        // Phase 5.C.2: --validate-outputs turns on the per-target
        // declared-output stat() validation in PASS B of the
        // orchestrator's analyze/build split. Sets the env var
        // the orchestrator already gates on.
        if validate_outputs {
            std::env::set_var("RUSH_VALIDATE_OUTPUTS", "1");
        }
        // Phase 5.C.4: --use-scheduler turns on the action-graph
        // dispatch path for scheduler-eligible targets. Sets the
        // env var the orchestrator's PASS B inspects when
        // computing scheduler_eligible. Default off until every
        // rule the user's workload exercises has been migrated
        // to populate ExecutableAction; mixed eligibility works
        // (legacy and scheduler targets interleave) but only
        // eligible targets get the scheduler path.
        if use_scheduler {
            std::env::set_var("RUSH_USE_SCHEDULER", "1");
        }
        // Phase 5.C.4 commit 4: --verify-scheduler-commands
        // turns on the legacy-vs-scheduler argv diff harness.
        if verify_scheduler_commands {
            std::env::set_var("RUSH_VERIFY_SCHEDULER_COMMANDS", "1");
        }
        // Phase 5.C.4 commit 50: --explain-action-keys
        // promotes RUSH_EXPLAIN_ACTION_KEYS=1. Same env-var
        // pattern as the other Phase 5.C.4 toggles.
        if explain_action_keys {
            std::env::set_var("RUSH_EXPLAIN_ACTION_KEYS", "1");
        }
        // Phase 5.C.4 commit 69: --explain-action-keys-view
        // promotes RUSH_EXPLAIN_ACTION_KEYS_VIEW=<value>.
        // CLI overrides env. Recognized: spec | canonical |
        // both. Invalid values rejected here so operators
        // get a fast failure rather than a silent default.
        if let Some(view) = explain_action_keys_view.as_deref() {
            match view.to_ascii_lowercase().as_str() {
                "spec" | "canonical" | "both" => {
                    std::env::set_var("RUSH_EXPLAIN_ACTION_KEYS_VIEW", view);
                }
                other => {
                    anyhow::bail!(
                        "--explain-action-keys-view: unknown value {:?}; \
                         expected one of: spec, canonical, both",
                        other
                    );
                }
            }
        }
        // Phase 5.C.4 commit 61: --explain-action-graph
        // promotes RUSH_EXPLAIN_ACTION_GRAPH=1. Distinct
        // surface from action-key explain — see CLI doc.
        if explain_action_graph {
            std::env::set_var("RUSH_EXPLAIN_ACTION_GRAPH", "1");
        }
        // Phase 5.C.4 commit 71: report-specific CLI format
        // flags. CLI overrides env. Env vars remain as
        // aliases for back-compat. Pro: "Use report-specific
        // flags, not one global --output-format."
        if let Some(mode) = scheduler_preflight.as_deref() {
            match mode.to_ascii_lowercase().as_str() {
                "off" => {
                    // Active suppression — even if the env
                    // var was set, the CLI overrides.
                    std::env::remove_var("RUSH_SCHEDULER_PREFLIGHT");
                    std::env::remove_var("RUSH_SCHEDULER_PREFLIGHT_JSON");
                }
                "human" => {
                    std::env::set_var("RUSH_SCHEDULER_PREFLIGHT", "1");
                    std::env::remove_var("RUSH_SCHEDULER_PREFLIGHT_JSON");
                }
                "json" => {
                    std::env::set_var("RUSH_SCHEDULER_PREFLIGHT", "1");
                    std::env::set_var("RUSH_SCHEDULER_PREFLIGHT_JSON", "1");
                }
                other => {
                    anyhow::bail!(
                        "--scheduler-preflight: unknown value {:?}; \
                         expected one of: off, human, json",
                        other
                    );
                }
            }
        }
        if let Some(mode) = legacy_breakdown.as_deref() {
            match mode.to_ascii_lowercase().as_str() {
                "off" => {
                    std::env::set_var("RUSH_LEGACY_BREAKDOWN", "off");
                    std::env::remove_var("RUSH_LEGACY_BREAKDOWN_JSON");
                }
                "human" => {
                    std::env::set_var("RUSH_LEGACY_BREAKDOWN", "human");
                    std::env::remove_var("RUSH_LEGACY_BREAKDOWN_JSON");
                }
                "json" => {
                    std::env::set_var("RUSH_LEGACY_BREAKDOWN", "json");
                    std::env::set_var("RUSH_LEGACY_BREAKDOWN_JSON", "1");
                }
                other => {
                    anyhow::bail!(
                        "--legacy-breakdown: unknown value {:?}; \
                         expected one of: off, human, json",
                        other
                    );
                }
            }
        }
        if let Some(fmt) = explain_action_keys_format.as_deref() {
            match fmt.to_ascii_lowercase().as_str() {
                "human" => {
                    std::env::remove_var("RUSH_EXPLAIN_ACTION_KEYS_JSON");
                }
                "json" => {
                    std::env::set_var("RUSH_EXPLAIN_ACTION_KEYS_JSON", "1");
                }
                other => {
                    anyhow::bail!(
                        "--explain-action-keys-format: unknown value {:?}; \
                         expected one of: human, json",
                        other
                    );
                }
            }
        }
        if let Some(fmt) = explain_action_graph_format.as_deref() {
            match fmt.to_ascii_lowercase().as_str() {
                "human" => {
                    std::env::remove_var("RUSH_EXPLAIN_ACTION_GRAPH_JSON");
                }
                "json" => {
                    std::env::set_var("RUSH_EXPLAIN_ACTION_GRAPH_JSON", "1");
                }
                other => {
                    anyhow::bail!(
                        "--explain-action-graph-format: unknown value {:?}; \
                         expected one of: human, json",
                        other
                    );
                }
            }
        }
        // Phase 5.C.4 commit 124 (Batch P): tri-state
        // selector for the experimental size-estimate
        // reuse path on repeat-miss .bzl reloads. CLI
        // overrides env. Env aliases:
        //   RUSH_BZL_REUSE_SIZE_ESTIMATE=1        → on
        //   RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY=1 → verify
        apply_bzl_size_estimate_reuse(bzl_size_estimate_reuse.as_deref())?;
        // Phase 5.C.4 commit 10: --include-tests disables the
        // BUILD-loader's test-target filter. The filter exists
        // to prevent transitive-dependency explosion on TF-scale
        // workloads where loading every cc_test would OOM the
        // process. cc_test verification on hello_cpp-scale
        // workloads needs this off. Explicitly named test
        // targets are honored regardless of this flag — that
        // logic lives in the orchestrator's loader, not here.
        if include_tests {
            std::env::set_var("RUSH_INCLUDE_TESTS", "1");
        }

        // Execute build using RuleInstance path (enables action generation).
        //
        // Persistent-daemon Run-wiring: RUSH_REUSE_SELFTEST=1 runs the build
        // TWICE in-process and asserts (a) the 2nd run reused the analyzed graph
        // with ZERO analysis work (the invariant) and (b) the warm action graph
        // is byte-identical to the cold build's (the differential oracle). This
        // proves the reuse mechanism is correct before the daemon transport
        // routes real commands through it.
        // Cache maintenance ran only on the daemon path, so a --daemon=off
        // sweep never bounded the disk: a TF certification run wrote 415 GB
        // of CAS blobs and aborted on its own low-disk guard. Maintain
        // BEFORE building - a build that starts on a nearly-full disk needs
        // the space reclaimed now, not after it has failed.
        orchestrator.gc_disk_cache_if_needed().await;

        if std::env::var("RUSH_REUSE_SELFTEST").as_deref() == Ok("1") {
            orchestrator.reuse_selftest(targets).await?;
        } else {
            orchestrator.build_with_dependencies(targets).await?;
        }

        // Write compilation database
        orchestrator.write_compilation_database().await?;

        // Export metrics if requested
        if let Some(json_path) = metrics_json {
            orchestrator.export_metrics_json(&json_path)?;
        }

        if let Some(csv_path) = metrics_csv {
            orchestrator.export_metrics_csv(&csv_path)?;
        }

        // Export profiling trace if enabled
        if let Some(profile_path) = profile {
            orchestrator.export_profiler_trace(&profile_path)?;
        }

        // Display remote execution statistics if remote execution was used
        if remote_executor.is_some() {
            if let Some(stats) = orchestrator.remote_executor_stats().await {
                eprintln!();
                eprintln!("{}", "🌐 Remote Execution Statistics:".bold().cyan());
                eprintln!(
                    "   {} {}",
                    "Total actions:".bright_black(),
                    stats.total_actions.to_string().cyan()
                );
                eprintln!(
                    "   {} {}",
                    "Remote cache hits:".bright_black(),
                    stats.cache_hits.to_string().green()
                );
                eprintln!(
                    "   {} {}",
                    "Remote executions:".bright_black(),
                    stats.remote_executions.to_string().cyan()
                );
                eprintln!(
                    "   {} {}",
                    "Fallback to local:".bright_black(),
                    stats.failures.to_string().yellow()
                );

                if stats.total_actions > 0 {
                    let cache_hit_rate =
                        (stats.cache_hits as f64 / stats.total_actions as f64) * 100.0;
                    eprintln!(
                        "   {} {:.1}%",
                        "Cache hit rate:".bright_black(),
                        cache_hit_rate
                    );

                    let avg_wait_time = stats.total_wait_time_ms / stats.total_actions as u64;
                    eprintln!("   {} {}ms", "Avg wait time:".bright_black(), avg_wait_time);
                }
            }
        }

        Ok(())
    }

    /// Run the persistent build daemon (`rush daemon` / rushd).
    ///
    /// Holds a long-lived [`BuildOrchestrator`] whose snapshot cache persists
    /// across client requests, serves the rush.command.v1 RPC over a
    /// Unix-domain socket, and publishes a discovery metadata file (socket path +
    /// auth cookie) so clients find it without a hardcoded socket name. Serves
    /// until an explicit shutdown RPC or `idle_timeout` seconds of inactivity.
    async fn run_daemon(idle_timeout: u64, verbose: bool) -> Result<()> {
        use rush_transport_grpc::{serve_unix_with_shutdown, AuthCookie, DaemonMetadata, GrpcAdapter, LocalEndpoint};
        use std::sync::Arc;

        let workspace_root = rush_build::find_workspace_root()?;
        let disc = DaemonDiscovery::for_workspace(&workspace_root)?;
        disc.ensure_dir()?;

        // Build the long-lived orchestrator whose snapshot cache persists across
        // client requests. disk cache on by default, fastbuild config — matching
        // the CLI default build arm.
        //
        // We do NOT eagerly load the WORKSPACE here: the build path itself calls
        // `load_workspace` on the first request, so a pre-load would only delay
        // socket binding. Binding the socket FIRST (below) lets a spawning
        // client's liveness ping answer immediately, so the cold WORKSPACE load
        // happens inside the first build (the unavoidable cold cost) rather than
        // blocking the client's connect-retry — that is what keeps a cold build
        // from being penalized beyond the small spawn cost.
        // Disk-backed action cache in the daemon too: without it, daemon
        // builds populated only an in-memory cache, so every daemon
        // restart lost all action results and CAS restoration had no
        // ActionResult records to restore from (the repeat-cold path was
        // structurally impossible). Fall back to memory-only on error.
        let orchestrator = Arc::new(
            rush_build::BuildOrchestrator::with_disk_cache_and_config(
                workspace_root.clone(),
                rush_build::CacheConfig::default(),
                rush_build::BuildConfig::Fastbuild,
            )
            .or_else(|_| {
                // Disk-cache setup failed: fall back to memory-only. A
                // construction error that is NOT about the disk cache (a
                // protobuf version skew, a missing declared repository)
                // fails here too, and must surface as an error rather than
                // panic inside the fallback.
                rush_build::BuildOrchestrator::try_with_config(
                    workspace_root.clone(),
                    rush_build::BuildConfig::Fastbuild,
                )
            })?,
        );

        // Phase-2 dirty-set watcher: arm it over the workspace + external roots
        // BEFORE serving. A healthy watcher lets a warm no-op validate ZERO paths
        // (skip the ~20k-dep manifest stat-walk) when nothing it depends on
        // changed. Arming first establishes the continuity premise: every
        // snapshot analyzed after this instant is covered. On any failure the
        // watcher reports Broken and every no-op falls back to the full Phase-1
        // manifest validate — so arming can never cause a stale build. Honor
        // RUSH_WATCHFS=off to keep the pure-scan behavior.
        if std::env::var("RUSH_WATCHFS").as_deref() != Ok("off") {
            orchestrator.arm_watcher();
        }

        // A fresh cookie per daemon instance, persisted 0600 for clients.
        let cookie = AuthCookie::generate();
        cookie.write_to(&disc.cookie_path)?;

        // The service holds the orchestrator + a shutdown notifier the serve loop
        // awaits. The build verb routes through the reuse-or-analyze fast path.
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let service = Arc::new(rush_build::command_service::BuildCommandService::new(
            Arc::clone(&orchestrator),
            Arc::clone(&shutdown),
        ));
        // Handle to the service's last-activity counter — drives the true
        // inactivity-reset idle timeout below.
        let last_activity = service.last_activity_handle();

        let adapter = GrpcAdapter::new(service, cookie);

        // Publish discovery metadata atomically AFTER the cookie exists. The
        // socket is created by serve_unix_with_shutdown; we write metadata first
        // so a racing client either sees a complete record or none.
        let endpoint = LocalEndpoint::UnixSocket(disc.socket_path.clone());
        let meta = DaemonMetadata::new(
            &endpoint,
            &disc.cookie_path,
            rush_protocol::PROTOCOL_MAJOR,
            rush_protocol::PROTOCOL_MINOR,
        );

        // Shutdown future: either an explicit Shutdown RPC (via the notifier) or
        // a TRUE inactivity-reset idle timeout. Each client RPC bumps the
        // service's `last_activity` counter; the loop wakes periodically and
        // shuts the daemon down only after `idle_timeout` seconds with NO
        // activity. This is the correct behavior for an always-on default
        // daemon: an actively-used daemon never dies mid-session, while an
        // abandoned one reaps itself so it does not linger forever.
        let started = std::time::Instant::now();
        let shutdown_signal = {
            let shutdown = Arc::clone(&shutdown);
            let last_activity = Arc::clone(&last_activity);
            async move {
                if idle_timeout == 0 {
                    // Idle timeout disabled: serve until an explicit shutdown.
                    shutdown.notified().await;
                    return;
                }
                // Seed the activity clock at startup so a daemon that is never
                // contacted still reaps itself after one idle window.
                last_activity.store(0, std::sync::atomic::Ordering::Relaxed);
                // Poll at a fraction of the timeout (bounded 1..=60s) so the
                // shutdown latency after the last build is small but the poll
                // is cheap.
                let poll = std::cmp::max(1, std::cmp::min(idle_timeout / 4, 60));
                loop {
                    tokio::select! {
                        _ = shutdown.notified() => return,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(poll)) => {}
                    }
                    let now = started.elapsed().as_secs();
                    let last = last_activity.load(std::sync::atomic::Ordering::Relaxed);
                    if now.saturating_sub(last) >= idle_timeout {
                        eprintln!(
                            "⏱️  rushd idle timeout ({idle_timeout}s, no activity) — shutting down"
                        );
                        return;
                    }
                }
            }
        };

        let handle = serve_unix_with_shutdown(adapter, &disc.socket_path, shutdown_signal).await?;
        meta.write_atomic(&disc.metadata_path)?;

        eprintln!(
            "🟢 rushd serving workspace {} on {} (pid {})",
            workspace_root.display(),
            disc.socket_path.display(),
            std::process::id()
        );
        if verbose {
            eprintln!("   metadata: {}", disc.metadata_path.display());
            eprintln!("   cookie:   {}", disc.cookie_path.display());
        }

        // Serve until shutdown; then clean up the discovery files.
        let res = handle.join().await;
        let _ = std::fs::remove_file(&disc.metadata_path);
        let _ = std::fs::remove_file(&disc.socket_path);
        let _ = std::fs::remove_file(&disc.cookie_path);
        res.map_err(|e| anyhow::anyhow!("rushd serve error: {e}"))?;
        Ok(())
    }

    /// Route a build through rushd (`--daemon=auto`). Discovers a live daemon
    /// (spawning one if absent), sends the command, streams RunEvents, prints
    /// diagnostics, and returns the daemon-reported exit code.
    ///
    /// Returns `Err` only when the daemon path is UNAVAILABLE (no daemon, spawn
    /// failed, connect failed) so the caller can fall back to the in-process
    /// build. A daemon that runs the build and reports a non-zero exit returns
    /// `Ok(code)` — that is a real build result, not an unavailability.
    async fn run_build_via_daemon(verbose: bool) -> Result<i32> {
        use futures::StreamExt;
        use rush_command_core::RunRequestExt;
        use rush_protocol::command::v1::run_event::Event;
        use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};

        let workspace_root = rush_build::find_workspace_root()?;
        let disc = DaemonDiscovery::for_workspace(&workspace_root)?;

        // Discover-or-spawn. Try to connect to an existing daemon first; if the
        // metadata is absent or the connect fails, spawn rushd and retry.
        let mut client = match Self::connect_daemon(&disc).await {
            Ok(c) => c,
            Err(_) => {
                if verbose {
                    eprintln!("🚀 no live rushd; spawning one for {}", workspace_root.display());
                }
                Self::spawn_daemon(&workspace_root)?;
                Self::connect_daemon_with_retry(&disc).await?
            }
        };

        // Build the RunRequest from THIS client process's argv/env/cwd plus the
        // workspace-config digest. The daemon reconstructs the client's
        // CommandSemantics from exactly these fields — so the reuse key matches
        // the equivalent in-process command.
        let cwd = std::env::current_dir().unwrap_or_else(|_| workspace_root.clone());
        // Normalize both `rush build` and its visible `rush b` alias in place,
        // preserving every target and analysis flag. Remove only the
        // client-side daemon routing option (including its split-value
        // spelling) so it does not perturb the analysis fingerprint.
        let argv = normalized_daemon_build_argv(
            std::env::args_os()
                .map(|arg| bytes::Bytes::from(os_bytes(&arg)))
                .collect(),
        );

        let environment: Vec<EnvironmentVariable> = std::env::vars_os()
            .map(|(name, value)| EnvironmentVariable {
                name: bytes::Bytes::from(os_bytes(&name)),
                value: bytes::Bytes::from(os_bytes(&value)),
            })
            .collect();

        let request = RunRequest {
            auth_cookie: bytes::Bytes::new(), // injected by the client
            argv,
            working_directory: bytes::Bytes::from(path_bytes(&cwd)),
            environment,
            block_for_lock: true,
            client_build_id: format!("cli-{}", std::process::id()),
            protocol_major: rush_protocol::PROTOCOL_MAJOR,
            protocol_minor: rush_protocol::PROTOCOL_MINOR,
            workspace_config: Vec::new(),
        }
        .with_workspace_config_from(&cwd);

        let mut stream = client
            .run(request)
            .await
            .map_err(|e| anyhow::anyhow!("daemon run RPC failed: {e}"))?;

        // Stream + print events; propagate the finished exit code.
        //
        // Fallback discipline: a stream error BEFORE the daemon reports a
        // `Finished` event means the daemon did not deliver a build result
        // (e.g. it crashed mid-request, or rejected the handshake). We return
        // `Err` so the caller falls back to the in-process build — correct,
        // because the build is idempotent. ONCE a `Finished` event arrives the
        // daemon HAS delivered a result; we capture the exit code and return it
        // immediately, so a transport error on the tail of the stream (e.g. the
        // daemon tearing down its side after sending the result) can NEVER
        // trigger a spurious fallback re-build of an already-completed build.
        let mut exit_code = 0i32;
        let mut finished = false;
        if verbose {
            eprintln!("▶️  daemon command started");
        }
        while let Some(item) = stream.next().await {
            let event = match item {
                Ok(ev) => ev,
                Err(e) => {
                    if finished {
                        // Result already delivered; ignore tail transport noise.
                        break;
                    }
                    return Err(anyhow::anyhow!("daemon run stream error: {e}"));
                }
            };
            match event.event {
                Some(Event::Started(_)) => {}
                Some(Event::Stdout(bytes)) => {
                    use std::io::Write as _;
                    let _ = std::io::stdout().write_all(&bytes);
                }
                Some(Event::Stderr(bytes)) => {
                    use std::io::Write as _;
                    let _ = std::io::stderr().write_all(&bytes);
                }
                Some(Event::Diagnostic(d)) => {
                    eprintln!("{}", String::from_utf8_lossy(&d.message));
                }
                Some(Event::Progress(_)) => {}
                Some(Event::Finished(f)) => {
                    exit_code = f.exit_code;
                    finished = true;
                    // The daemon delivered its result; stop reading the stream.
                    break;
                }
                None => {}
            }
        }
        if !finished {
            // The stream ended without a `Finished` event — the daemon went
            // away before delivering a result. Fall back to in-process.
            return Err(anyhow::anyhow!(
                "daemon closed the stream without reporting a result"
            ));
        }
        Ok(exit_code)
    }

    /// Connect to the daemon described by the discovery metadata.
    async fn connect_daemon(
        disc: &DaemonDiscovery,
    ) -> Result<rush_transport_grpc::GrpcClient> {
        use rush_transport_grpc::{DaemonMetadata, GrpcClient};
        if !disc.metadata_path.exists() {
            return Err(anyhow::anyhow!("no daemon metadata at {}", disc.metadata_path.display()));
        }
        let metadata = DaemonMetadata::read(&disc.metadata_path).map_err(
            |error| anyhow::anyhow!("invalid daemon metadata: {error}"),
        )?;
        let expected_version = format!(
            "{}.{}",
            rush_protocol::PROTOCOL_MAJOR,
            rush_protocol::PROTOCOL_MINOR
        );
        if metadata.protocol_version != expected_version {
            return Err(anyhow::anyhow!(
                "daemon protocol {} does not match client protocol {}",
                metadata.protocol_version,
                expected_version
            ));
        }
        let mut client = GrpcClient::discover(&disc.metadata_path)
            .await
            .map_err(|e| anyhow::anyhow!("discover failed: {e}"))?;
        // The channel connection already rejects absent/stale sockets. Avoid a
        // second RPC on every warm command; keep the ping available as an
        // opt-in diagnostic for stale-daemon investigations.
        if std::env::var("RUSH_DAEMON_CONNECT_PING").as_deref() == Ok("1") {
            client
                .ping()
                .await
                .map_err(|e| anyhow::anyhow!("ping failed (stale daemon?): {e}"))?;
        }
        Ok(client)
    }

    /// Connect with a short retry loop (used right after spawning the daemon,
    /// while it binds its socket and writes metadata).
    async fn connect_daemon_with_retry(
        disc: &DaemonDiscovery,
    ) -> Result<rush_transport_grpc::GrpcClient> {
        // The daemon now binds its socket BEFORE the (lazy) workspace load, so
        // its liveness ping answers within a socket-bind window. Budget 30s of
        // 100ms polls to absorb a busy machine / slow process start; if the
        // daemon still hasn't come up we return Err and the caller falls back
        // to the in-process build (correct, just without daemon reuse).
        let mut last_err = None;
        for _ in 0..300 {
            match Self::connect_daemon(disc).await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("daemon did not come up")))
    }

    /// Spawn a detached `rush daemon` for `workspace_root`, inheriting the
    /// client's environment (so the daemon sees the same RUSH_* / cache config).
    fn spawn_daemon(workspace_root: &std::path::Path) -> Result<()> {
        let exe = std::env::current_exe()
            .map_err(|e| anyhow::anyhow!("cannot locate rush binary to spawn daemon: {e}"))?;

        // FULLY detach the daemon's stdio. If the daemon inherited the client's
        // stdout/stderr (e.g. a pipe to `tail`), the long-lived daemon would hold
        // that pipe open forever, so the client's pipeline would never see EOF
        // and would hang even after the client exits. Redirect the daemon's
        // stdout/stderr to a per-workspace log file (best-effort) and null its
        // stdin.
        let disc = DaemonDiscovery::for_workspace(workspace_root)?;
        disc.ensure_dir()?;
        let log_path = disc.dir.join("rushd.log");
        let (out, err) = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(f) => {
                let f2 = f.try_clone().map_err(|e| anyhow::anyhow!("clone log fd: {e}"))?;
                (std::process::Stdio::from(f), std::process::Stdio::from(f2))
            }
            Err(_) => (std::process::Stdio::null(), std::process::Stdio::null()),
        };

        let mut cmd = std::process::Command::new(exe);
        cmd.arg("daemon")
            .current_dir(workspace_root)
            .stdin(std::process::Stdio::null())
            .stdout(out)
            .stderr(err);

        // FULLY detach into a NEW SESSION before exec. Redirecting stdio is not
        // enough: a child that stays in the spawning shell's session/process
        // group keeps the controlling terminal (and an interactive parent such
        // as an `ssh host 'rush build ...'` invocation) waiting on it, so the
        // client appears to hang even after it exits. `setsid()` makes the
        // daemon a session leader with no controlling terminal, so the client
        // can exit immediately and the long-lived daemon survives independently.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            unsafe {
                cmd.pre_exec(|| {
                    // New session — detaches from the controlling terminal and
                    // the parent's process group. `setsid` only fails if the
                    // caller is already a process-group leader, which a freshly
                    // forked child is not, so this succeeds in practice; treat a
                    // failure as non-fatal (the daemon still runs).
                    libc::setsid();
                    Ok(())
                });
            }
        }

        cmd.spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn rushd: {e}"))?;
        Ok(())
    }

    /// Execute test command
    async fn run_test(
        targets: Vec<String>,
        test_output: String,
        test_filter: Option<String>,
        jobs: Option<u32>,
        verbose: bool,
    ) -> Result<()> {
        // Find workspace root
        let workspace_root = rush_build::find_workspace_root()?;

        // Parse output mode
        let output_mode = TestOutputMode::parse(&test_output)?;

        if verbose {
            println!("🔧 Rush Test Configuration:");
            println!("   Test output: {:?}", output_mode);
            if let Some(ref filter) = test_filter {
                println!("   Test filter: {}", filter);
            }
            if let Some(j) = jobs {
                println!("   Parallel jobs: {}", j);
            }
            println!();
        }

        // Create orchestrator
        let mut orchestrator = BuildOrchestrator::with_workspace(workspace_root);

        // Set maximum parallel jobs if specified
        if let Some(j) = jobs {
            orchestrator.set_max_jobs(j);
            if verbose {
                println!("   Parallel jobs: {}", j);
            }
        }

        // Create test request
        let request = BuildRequest { targets };

        // Execute tests
        let results = orchestrator
            .test(request, output_mode, test_filter.as_deref())
            .await?;

        // Print summary
        results.print_summary();

        // Exit with error if any tests failed
        if !results.all_passed() {
            std::process::exit(1);
        }

        Ok(())
    }

    /// Execute clean command
    /// `rush cache info|clear` — the caches are machine-global, so
    /// inspecting and destroying them is a first-class command rather
    /// than a flag on a workspace-scoped one.
    fn run_cache(action: CacheAction) -> Result<()> {
        let root = rush_build::cache_roots::cache_root();
        let cas = rush_build::cache_roots::cas_blob_dir();
        let count_and_bytes = |dir: &std::path::Path, ext: Option<&str>| {
            let mut n = 0u64;
            let mut bytes = 0u64;
            for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
                if !e.file_type().is_file() {
                    continue;
                }
                if let Some(ext) = ext {
                    if e.path().extension().and_then(|x| x.to_str()) != Some(ext) {
                        continue;
                    }
                }
                n += 1;
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
            (n, bytes)
        };
        match action {
            CacheAction::Info { json } => {
                let (entries, entry_bytes) = count_and_bytes(&root, Some("cache"));
                let (blobs, blob_bytes) = count_and_bytes(&cas, None);
                if json {
                    println!(
                        "{{\"root\":\"{}\",\"action_cache\":{{\"entries\":{},\"bytes\":{}}},\"cas\":{{\"dir\":\"{}\",\"blobs\":{},\"bytes\":{}}},\"scope\":\"machine-global\"}}",
                        root.display(),
                        entries,
                        entry_bytes,
                        cas.display(),
                        blobs,
                        blob_bytes
                    );
                } else {
                    println!("cache root:    {}", root.display());
                    println!(
                        "action cache:  {} entries, {:.1} MiB",
                        entries,
                        entry_bytes as f64 / 1048576.0
                    );
                    println!(
                        "CAS:           {} blobs, {:.1} MiB ({})",
                        blobs,
                        blob_bytes as f64 / 1048576.0,
                        cas.display()
                    );
                    println!("scope:         machine-global (shared by every workspace)");
                }
                Ok(())
            }
            CacheAction::Clear { all } => {
                if !all {
                    anyhow::bail!(
                        "refusing to clear: the cache at {} is shared by every \
                         workspace on this machine, so there is no \
                         per-workspace subset to clear. Re-run with \
                         `rush cache clear --all` to delete it.",
                        root.display()
                    );
                }
                if root.exists() {
                    std::fs::remove_dir_all(&root)?;
                }
                println!("removed machine-global cache: {}", root.display());
                Ok(())
            }
        }
    }

    async fn run_clean(
        expunge: bool,
        cache_only: bool,
        _async_clean: bool,
        verbose: bool,
    ) -> Result<()> {
        use std::fs;

        // Find workspace root
        let workspace_root = rush_build::find_workspace_root()?;

        if expunge {
            println!("{}", "🗑️  Expunging all Rush data...".bold().yellow());
            println!();

            let mut removed_count = 0;

            // Remove rush-out/
            let rush_out = cache_paths::workspace_output_root(&workspace_root);
            if rush_out.exists() {
                if verbose {
                    println!(
                        "   {} {}",
                        "Removing:".bright_black(),
                        rush_out.display().to_string().cyan()
                    );
                }
                fs::remove_dir_all(&rush_out)?;
                println!("   {} {}", "✓".green(), "Removed rush-out/".bright_white());
                removed_count += 1;
            }

            // Remove rush-bin symlink
            let rush_bin = cache_paths::workspace_bin_symlink(&workspace_root);
            if rush_bin.exists() || rush_bin.symlink_metadata().is_ok() {
                if verbose {
                    println!(
                        "   {} {}",
                        "Removing:".bright_black(),
                        rush_bin.display().to_string().cyan()
                    );
                }
                fs::remove_file(&rush_bin).ok();
                println!(
                    "   {} {}",
                    "✓".green(),
                    "Removed rush-bin symlink".bright_white()
                );
                removed_count += 1;
            }

            // The action cache and CAS are MACHINE-GLOBAL: every
            // workspace on this machine shares them. `clean --expunge`
            // run inside one workspace used to delete them, silently
            // invalidating every other workspace's cached work — which
            // is exactly what happened during the 2026-08-22 census,
            // from a throwaway scratch workspace. Expunge now covers
            // only what THIS workspace owns; destroying shared state is
            // spelled out as its own command.
            let global_root = rush_build::cache_roots::cache_root();
            if global_root.exists() {
                println!(
                    "   {} machine-global cache left intact at {}",
                    "•".bright_black(),
                    global_root.display().to_string().cyan()
                );
                println!(
                    "     {} to delete it: {}",
                    "→".bright_black(),
                    "rush cache clear --all".bright_white()
                );
            }

            println!();
            if removed_count > 0 {
                println!(
                    "{} {} items removed",
                    "✨".bold(),
                    removed_count.to_string().green().bold()
                );
            } else {
                println!("{}", "Nothing to clean".bright_black());
            }
        } else if cache_only {
            anyhow::bail!(
                "`rush clean --cache` deleted the MACHINE-GLOBAL cache at {}, \
                 which every workspace on this machine shares. Use \
                 `rush cache clear --all` if that is what you mean, or \
                 `rush cache info` to see what is there.",
                rush_build::cache_roots::cache_root().display()
            );
        } else {
            println!("{}", "🗑️  Cleaning build outputs...".bold().yellow());
            println!();

            let mut removed_count = 0;

            // Remove rush-out/
            let rush_out = cache_paths::workspace_output_root(&workspace_root);
            if rush_out.exists() {
                if verbose {
                    println!(
                        "   {} {}",
                        "Removing:".bright_black(),
                        rush_out.display().to_string().cyan()
                    );
                }
                fs::remove_dir_all(&rush_out)?;
                println!("   {} {}", "✓".green(), "Removed rush-out/".bright_white());
                removed_count += 1;
            }

            // Remove rush-bin symlink
            let rush_bin = cache_paths::workspace_bin_symlink(&workspace_root);
            if rush_bin.exists() || rush_bin.symlink_metadata().is_ok() {
                if verbose {
                    println!(
                        "   {} {}",
                        "Removing:".bright_black(),
                        rush_bin.display().to_string().cyan()
                    );
                }
                fs::remove_file(&rush_bin).ok();
                println!(
                    "   {} {}",
                    "✓".green(),
                    "Removed rush-bin symlink".bright_white()
                );
                removed_count += 1;
            }

            println!();
            if removed_count > 0 {
                println!(
                    "{} {} items removed",
                    "✨".bold(),
                    removed_count.to_string().green().bold()
                );
                println!(
                    "   {} {}",
                    "ℹ".cyan(),
                    "Disk cache preserved (use --cache to clean)".bright_black()
                );
            } else {
                println!("{}", "Nothing to clean".bright_black());
            }
        }

        Ok(())
    }

    /// Execute run command
    async fn run_binary(
        target: String,
        args: Vec<String>,
        config: Option<String>,
        verbose: bool,
    ) -> Result<()> {
        use std::process::Command;

        if verbose {
            println!("🏃 Rush Run Configuration:");
            println!("   Target: {}", target);
            if let Some(ref cfg) = config {
                println!("   Config: {}", cfg);
            }
            if !args.is_empty() {
                println!("   Arguments: {:?}", args);
            }
            println!();
        }

        // Find workspace root
        let workspace_root = rush_build::find_workspace_root()?;

        // Parse target label
        let (package, name) = Self::parse_target_label(&target)?;

        // Build the target first
        println!(
            "{} {}",
            "🔨 Building target:".bold().cyan(),
            target.bright_white()
        );
        let orchestrator = BuildOrchestrator::with_workspace(workspace_root.clone());
        orchestrator
            .build_with_dependencies(vec![target.clone()])
            .await?;

        // Write compilation database
        orchestrator.write_compilation_database().await?;

        // Determine executable path
        let output_tree =
            rush_build::OutputTree::new(workspace_root, rush_build::BuildConfig::Fastbuild);
        let exe_path = output_tree.bin_path(&package, &name);

        if !exe_path.exists() {
            anyhow::bail!("Executable not found: {}", exe_path.display());
        }

        println!(
            "\n{} {}",
            "🚀 Running:".bold().green(),
            exe_path.display().to_string().cyan()
        );
        if !args.is_empty() {
            println!("   {} {:?}", "Arguments:".bright_black(), args);
        }
        println!();

        // Execute the binary
        let status = Command::new(&exe_path).args(&args).status()?;

        if !status.success() {
            anyhow::bail!("Binary exited with code: {}", status.code().unwrap_or(-1));
        }

        Ok(())
    }

    /// Print detailed version information
    fn print_version_info() {
        println!();
        println!(
            "{} {}",
            "🚀 Rush Build System".bold().cyan(),
            format!("v{}", VERSION).green()
        );
        println!();

        if is_bazel_mode() {
            println!("{}", "⚡ Running in Bazel compatibility mode".yellow());
            println!();
        }

        println!("{}", "📦 Build Information:".bold());
        println!("   {} {}", "Version:".bright_black(), VERSION.cyan());

        // Package metadata
        println!(
            "   {} {}",
            "Package:".bright_black(),
            env!("CARGO_PKG_NAME").bright_black()
        );
        println!(
            "   {} {}",
            "Authors:".bright_black(),
            env!("CARGO_PKG_AUTHORS").bright_black()
        );
        println!(
            "   {} {}",
            "License:".bright_black(),
            env!("CARGO_PKG_LICENSE").bright_black()
        );

        // Build target
        let target = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
        println!("   {} {}", "Target:".bright_black(), target.bright_black());

        println!();
        println!(
            "{}",
            "⚡ Razing Bazel to the ground, one build at a time"
                .italic()
                .bright_black()
        );
        println!();
    }

    /// Parse target label into (package, name)
    /// Supports:
    /// - "//package:name" - Workspace-relative target
    /// - "@repo//package:name" - External repository target (repo prefix stripped)
    fn parse_target_label(label: &str) -> Result<(String, String)> {
        // Handle external repository labels: @repo//package:name
        let working_label = if label.starts_with('@') {
            // Find the '//' that separates repo from package
            if let Some(slash_pos) = label.find("//") {
                // Strip @repo, keep //package:name
                &label[slash_pos..]
            } else {
                anyhow::bail!(
                    "Invalid external repository label (missing '//'): {}",
                    label
                )
            }
        } else {
            label
        };

        if let Some(without_slashes) = working_label.strip_prefix("//") {
            if let Some(colon_pos) = without_slashes.find(':') {
                let package = without_slashes[..colon_pos].to_string();
                let name = without_slashes[colon_pos + 1..].to_string();
                Ok((package, name))
            } else {
                anyhow::bail!("Invalid target label (missing ':'): {}", label)
            }
        } else {
            anyhow::bail!(
                "Invalid target label (must start with '//' or '@'): {}",
                label
            )
        }
    }

    /// Execute query command
    async fn run_query(query: String, output_format: String, verbose: bool) -> Result<()> {
        // Find workspace root
        let workspace_root = rush_build::find_workspace_root()?;

        if verbose {
            println!("{}", "🔍 Rush Query".bold().cyan());
            println!("   {} {}", "Query:".bright_black(), query.bright_white());
            println!("   {} {}", "Format:".bright_black(), output_format.cyan());
            println!(
                "   {} {}",
                "Workspace:".bright_black(),
                workspace_root.display().to_string().bright_black()
            );
            println!();
        }

        // Check if query involves external repositories (starts with @)
        let needs_workspace = query.trim().starts_with('@');

        if needs_workspace {
            // Create orchestrator and load WORKSPACE for external repository access
            if verbose {
                println!("📦 Loading WORKSPACE for external repository access...");
            }
            let orchestrator = BuildOrchestrator::with_workspace(workspace_root.clone());
            orchestrator.load_workspace(verbose).await?;

            // Now execute query using the orchestrator
            let results = Self::execute_query_with_orchestrator(&orchestrator, &query, verbose)?;

            // Output results
            match output_format.as_str() {
                "text" => Self::output_text(&results),
                "json" => Self::output_json(&results)?,
                "proto" => anyhow::bail!("Protocol buffer output format not yet implemented"),
                _ => anyhow::bail!(
                    "Unknown output format: {}. Use text, json, or proto",
                    output_format
                ),
            }
        } else {
            // Parse and execute query without orchestrator (local workspace only)
            let results = Self::execute_query(&workspace_root, &query, verbose)?;

            // Output results in requested format
            match output_format.as_str() {
                "text" => Self::output_text(&results),
                "json" => Self::output_json(&results)?,
                "proto" => anyhow::bail!("Protocol buffer output format not yet implemented"),
                _ => anyhow::bail!(
                    "Unknown output format: {}. Use text, json, or proto",
                    output_format
                ),
            }
        }

        Ok(())
    }

    /// Execute a query using an orchestrator (for external repository support)
    fn execute_query_with_orchestrator(
        orchestrator: &BuildOrchestrator,
        query: &str,
        verbose: bool,
    ) -> Result<Vec<QueryResult>> {
        let query = query.trim();

        // For now, only support simple target patterns for external repos
        // Complex queries (deps, rdeps, etc.) on external repos not yet implemented
        if query.starts_with('@') {
            if verbose {
                println!("Expanding external repository targets: {}", query);
            }

            // Expand the pattern using orchestrator
            let targets = orchestrator.expand_targets(&[query.to_string()])?;
            let mut results = Vec::new();

            // For each expanded target, try to get its metadata
            for target in targets {
                // Parse the external repo target
                if let Some(slash_pos) = target.find("//") {
                    let _repo_and_package = &target[..slash_pos + 2];
                    let rest = &target[slash_pos + 2..];

                    if let Some(_colon_pos) = rest.find(':') {
                        // External repo target format: @repo//package:name
                        // For now, just return the target label with a generic type
                        // TODO: Actually parse the BUILD file to get real rule type
                        results.push(QueryResult {
                            label: target.clone(),
                            rule_type: "external_target".to_string(),
                            dependencies: vec![],
                        });
                    }
                }
            }

            Ok(results)
        } else {
            anyhow::bail!(
                "execute_query_with_orchestrator should only be called for external repo queries"
            )
        }
    }

    /// Execute a query expression and return matching targets
    fn execute_query(
        workspace_root: &Path,
        query: &str,
        verbose: bool,
    ) -> Result<Vec<QueryResult>> {
        let query = query.trim();

        // Parse and execute different query types
        if query.starts_with("deps(") && query.ends_with(')') {
            // deps(pattern) - show dependencies
            let pattern = &query[5..query.len() - 1];
            Self::query_deps(workspace_root, pattern, verbose)
        } else if query.starts_with("rdeps(") && query.ends_with(')') {
            // rdeps(pattern) - show reverse dependencies
            let pattern = &query[6..query.len() - 1];
            Self::query_rdeps(workspace_root, pattern, verbose)
        } else if query.starts_with("kind(") && query.ends_with(')') {
            // kind(rule_type, pattern) - filter by rule type
            let inner = &query[5..query.len() - 1];
            if let Some(comma_pos) = inner.find(',') {
                let rule_type = inner[..comma_pos].trim();
                let pattern = inner[comma_pos + 1..].trim();
                Self::query_kind(workspace_root, rule_type, pattern, verbose)
            } else {
                anyhow::bail!("Invalid kind() query. Expected: kind(rule_type, pattern)")
            }
        } else if query.starts_with("allpaths(") && query.ends_with(')') {
            // allpaths(from, to) - find all dependency paths
            let inner = &query[9..query.len() - 1];
            if let Some(comma_pos) = inner.find(',') {
                let from = inner[..comma_pos].trim();
                let to = inner[comma_pos + 1..].trim();
                Self::query_allpaths(workspace_root, from, to, verbose)
            } else {
                anyhow::bail!("Invalid allpaths() query. Expected: allpaths(from, to)")
            }
        } else if query.starts_with("somepath(") && query.ends_with(')') {
            // somepath(from, to) - find one dependency path
            let inner = &query[9..query.len() - 1];
            if let Some(comma_pos) = inner.find(',') {
                let from = inner[..comma_pos].trim();
                let to = inner[comma_pos + 1..].trim();
                Self::query_somepath(workspace_root, from, to, verbose)
            } else {
                anyhow::bail!("Invalid somepath() query. Expected: somepath(from, to)")
            }
        } else if query.starts_with("filter(") && query.ends_with(')') {
            // filter(pattern, expression) - filter targets
            let inner = &query[7..query.len() - 1];
            if let Some(comma_pos) = inner.find(',') {
                let pattern = inner[..comma_pos].trim();
                let expression = inner[comma_pos + 1..].trim();
                Self::query_filter(workspace_root, pattern, expression, verbose)
            } else {
                anyhow::bail!("Invalid filter() query. Expected: filter(pattern, expression)")
            }
        } else if query == "//..." || query.ends_with(":all") || query.ends_with("/...") {
            // Target pattern - list targets
            Self::query_targets(workspace_root, query, verbose)
        } else if query.starts_with("//") && query.contains(':') {
            // Single target
            Self::query_targets(workspace_root, query, verbose)
        } else {
            anyhow::bail!(
                "Invalid query expression: {}\n\
                Supported:\n  \
                  //..., //pkg:all, //pkg:target\n  \
                  deps(pattern) - Show dependencies\n  \
                  rdeps(pattern) - Show reverse dependencies\n  \
                  kind(type, pattern) - Filter by rule type\n  \
                  allpaths(from, to) - Find all paths between targets\n  \
                  somepath(from, to) - Find one path between targets\n  \
                  filter(pattern, expr) - Filter targets by attributes",
                query
            )
        }
    }

    /// Query targets matching a pattern
    fn query_targets(
        workspace_root: &Path,
        pattern: &str,
        verbose: bool,
    ) -> Result<Vec<QueryResult>> {
        if verbose {
            println!("Searching for targets matching: {}", pattern);
        }

        let targets = Self::expand_target_pattern(workspace_root, pattern)?;
        let mut results = Vec::new();

        for target in targets {
            let (package, name) = Self::parse_target_label(&target)?;

            // Use lightweight parsing to get rule type without loading .bzl files
            let build_path = workspace_root.join(&package).join("BUILD");
            if !build_path.exists() {
                continue;
            }

            if let Ok(target_info) = BuildOrchestrator::extract_target_info_lightweight(&build_path)
            {
                if let Some((_target_name, rule_type)) =
                    target_info.iter().find(|(n, _)| n == &name)
                {
                    // For simple queries, we don't need full dependency info
                    // Dependencies would require full .bzl loading which is expensive
                    results.push(QueryResult {
                        label: target.clone(),
                        rule_type: rule_type.clone(),
                        dependencies: vec![], // TODO: extract from AST without evaluation
                    });
                }
            }
        }

        Ok(results)
    }

    /// Query dependencies of matching targets
    fn query_deps(workspace_root: &Path, pattern: &str, verbose: bool) -> Result<Vec<QueryResult>> {
        if verbose {
            println!("Finding dependencies of: {}", pattern);
        }

        let targets = Self::expand_target_pattern(workspace_root, pattern)?;
        let mut all_deps = Vec::new();

        for target in targets {
            let deps = Self::get_target_deps(workspace_root, &target)?;
            all_deps.extend(deps);
        }

        // Remove duplicates
        all_deps.sort_by(|a, b| a.label.cmp(&b.label));
        all_deps.dedup_by(|a, b| a.label == b.label);

        Ok(all_deps)
    }

    /// Query reverse dependencies (what depends on these targets)
    fn query_rdeps(
        workspace_root: &Path,
        pattern: &str,
        verbose: bool,
    ) -> Result<Vec<QueryResult>> {
        if verbose {
            println!("Finding reverse dependencies of: {}", pattern);
        }

        // Get all targets in workspace
        let all_targets = Self::expand_target_pattern(workspace_root, "//...")?;
        let target_set: std::collections::HashSet<_> =
            Self::expand_target_pattern(workspace_root, pattern)?
                .into_iter()
                .collect();

        let mut rdeps = Vec::new();

        // Check each target to see if it depends on any of our targets
        for target in all_targets {
            let (package, name) = Self::parse_target_label(&target)?;
            let build_path = workspace_root.join(&package).join("BUILD");

            if !build_path.exists() {
                continue;
            }

            if let Ok(build_file) = rush_parse::parse_build_file(&build_path) {
                if let Some(rule) = build_file.rules.iter().find(|r| r.name == name) {
                    let deps = Self::extract_deps(rule);

                    // Check if this target depends on any of our targets
                    if deps.iter().any(|d| target_set.contains(d)) {
                        rdeps.push(QueryResult {
                            label: target.clone(),
                            rule_type: rule.rule_type.clone(),
                            dependencies: deps,
                        });
                    }
                }
            }
        }

        Ok(rdeps)
    }

    /// Query targets filtered by rule type
    fn query_kind(
        workspace_root: &Path,
        rule_type: &str,
        pattern: &str,
        verbose: bool,
    ) -> Result<Vec<QueryResult>> {
        if verbose {
            println!("Finding {} targets matching: {}", rule_type, pattern);
        }

        let all_targets = Self::query_targets(workspace_root, pattern, false)?;

        // Filter by rule type
        let filtered: Vec<_> = all_targets
            .into_iter()
            .filter(|t| t.rule_type == rule_type)
            .collect();

        Ok(filtered)
    }

    /// Find all paths from one target to another
    fn query_allpaths(
        workspace_root: &Path,
        from: &str,
        to: &str,
        verbose: bool,
    ) -> Result<Vec<QueryResult>> {
        use std::collections::{HashMap, VecDeque};

        if verbose {
            println!("Finding all paths from {} to {}", from, to);
        }

        // Build dependency graph
        let mut graph: HashMap<String, Vec<String>> = HashMap::new();
        let all_targets = Self::query_targets(workspace_root, "//...", false)?;

        for target in &all_targets {
            graph.insert(target.label.clone(), target.dependencies.clone());
        }

        // Find all paths using BFS with path tracking
        let mut paths = Vec::new();
        let mut queue = VecDeque::new();
        queue.push_back((from.to_string(), vec![from.to_string()]));

        while let Some((current, path)) = queue.pop_front() {
            if current == to {
                paths.push(path.clone());
                continue;
            }

            if let Some(deps) = graph.get(&current) {
                for dep in deps {
                    if !path.contains(dep) {
                        // Avoid cycles
                        let mut new_path = path.clone();
                        new_path.push(dep.clone());
                        queue.push_back((dep.clone(), new_path));
                    }
                }
            }
        }

        // Convert paths to QueryResult format
        let mut results = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            let path_str = path.join(" -> ");
            results.push(QueryResult {
                label: format!("path_{}: {}", i + 1, path_str),
                rule_type: "path".to_string(),
                dependencies: vec![],
            });
        }

        Ok(results)
    }

    /// Find one path from one target to another
    fn query_somepath(
        workspace_root: &Path,
        from: &str,
        to: &str,
        verbose: bool,
    ) -> Result<Vec<QueryResult>> {
        use std::collections::{HashMap, VecDeque};

        if verbose {
            println!("Finding a path from {} to {}", from, to);
        }

        // Build dependency graph
        let mut graph: HashMap<String, Vec<String>> = HashMap::new();
        let all_targets = Self::query_targets(workspace_root, "//...", false)?;

        for target in &all_targets {
            graph.insert(target.label.clone(), target.dependencies.clone());
        }

        // BFS to find shortest path
        let mut queue = VecDeque::new();
        let mut parent: HashMap<String, String> = HashMap::new();
        queue.push_back(from.to_string());

        while let Some(current) = queue.pop_front() {
            if current == to {
                // Reconstruct path
                let mut path = vec![current.clone()];
                let mut node = current.clone();
                while let Some(p) = parent.get(&node) {
                    path.push(p.clone());
                    node = p.clone();
                }
                path.reverse();

                let path_str = path.join(" -> ");
                return Ok(vec![QueryResult {
                    label: path_str,
                    rule_type: "path".to_string(),
                    dependencies: vec![],
                }]);
            }

            if let Some(deps) = graph.get(&current) {
                for dep in deps {
                    if !parent.contains_key(dep) && dep != from {
                        parent.insert(dep.clone(), current.clone());
                        queue.push_back(dep.clone());
                    }
                }
            }
        }

        Ok(vec![]) // No path found
    }

    /// Filter targets by attributes
    fn query_filter(
        workspace_root: &Path,
        pattern: &str,
        expression: &str,
        verbose: bool,
    ) -> Result<Vec<QueryResult>> {
        if verbose {
            println!("Filtering {} with expression: {}", pattern, expression);
        }

        let all_targets = Self::query_targets(workspace_root, pattern, false)?;

        // Parse filter expression
        // Supports: "has_srcs", "has_deps", "no_deps"
        let filtered: Vec<_> = all_targets
            .into_iter()
            .filter(|t| {
                match expression {
                    "has_deps" => !t.dependencies.is_empty(),
                    "no_deps" => t.dependencies.is_empty(),
                    expr if expr.starts_with("kind=") => {
                        let kind = &expr[5..];
                        t.rule_type == kind
                    }
                    _ => true, // Unknown filter, pass through
                }
            })
            .collect();

        Ok(filtered)
    }

    /// Get all dependencies of a target recursively
    fn get_target_deps(workspace_root: &Path, target: &str) -> Result<Vec<QueryResult>> {
        let (package, name) = Self::parse_target_label(target)?;
        let build_path = workspace_root.join(&package).join("BUILD");

        if !build_path.exists() {
            anyhow::bail!(
                "Cannot inspect dependencies for {} - BUILD file {} missing",
                target,
                build_path.display()
            );
        }

        let build_file = rush_parse::parse_build_file(&build_path)?;
        let rule = build_file
            .rules
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| anyhow::anyhow!("Target not found: {}", target))?;

        let mut all_deps = Vec::new();
        let deps = Self::extract_deps(rule);

        for dep in &deps {
            // Add the direct dependency
            if let Ok((dep_pkg, dep_name)) = Self::parse_target_label(dep) {
                let dep_build_path = workspace_root.join(&dep_pkg).join("BUILD");
                if dep_build_path.exists() {
                    if let Ok(dep_build_file) = rush_parse::parse_build_file(&dep_build_path) {
                        if let Some(dep_rule) =
                            dep_build_file.rules.iter().find(|r| r.name == dep_name)
                        {
                            all_deps.push(QueryResult {
                                label: dep.clone(),
                                rule_type: dep_rule.rule_type.clone(),
                                dependencies: Self::extract_deps(dep_rule),
                            });

                            // Recursively get transitive dependencies
                            let transitive = Self::get_target_deps(workspace_root, dep)?;
                            all_deps.extend(transitive);
                        }
                    }
                }
            }
        }

        // Remove duplicates
        all_deps.sort_by(|a, b| a.label.cmp(&b.label));
        all_deps.dedup_by(|a, b| a.label == b.label);

        Ok(all_deps)
    }

    /// Expand a target pattern to concrete target labels
    fn expand_target_pattern(workspace_root: &Path, pattern: &str) -> Result<Vec<String>> {
        let orchestrator = BuildOrchestrator::with_workspace(workspace_root.to_path_buf());
        orchestrator.expand_targets(&[pattern.to_string()])
    }

    /// Extract dependency labels from a rule
    fn extract_deps(rule: &rush_parse::Rule) -> Vec<String> {
        if let Some(rush_parse::Value::List(deps)) = rule.attributes.get("deps") {
            deps.iter()
                .filter_map(|v| match v {
                    rush_parse::Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Output results in text format
    fn output_text(results: &[QueryResult]) {
        use std::collections::HashMap;

        if results.is_empty() {
            println!("{}", "No targets found".yellow());
            return;
        }

        // Group results by rule type for organized display
        let mut by_type: HashMap<String, Vec<&QueryResult>> = HashMap::new();
        for result in results {
            by_type
                .entry(result.rule_type.clone())
                .or_default()
                .push(result);
        }

        println!();
        println!("{}", "🔍 Query Results:".bold().cyan());
        println!();

        // Display targets grouped by type with colors
        for (rule_type, targets) in by_type.iter() {
            let (icon, color) = match rule_type.as_str() {
                "cc_library" => ("📚", "blue"),
                "cc_binary" => ("⚙️", "green"),
                "cc_test" => ("🧪", "yellow"),
                _ => ("📦", "white"),
            };

            println!(
                "{} {} {} {}",
                icon,
                rule_type.bold(),
                "-".bright_black(),
                format!("{} target(s)", targets.len()).bright_black()
            );

            for result in targets {
                let label_colored = match color {
                    "blue" => result.label.blue().to_string(),
                    "green" => result.label.green().to_string(),
                    "yellow" => result.label.yellow().to_string(),
                    _ => result.label.bright_white().to_string(),
                };
                println!("   {}", label_colored);

                if !result.dependencies.is_empty() {
                    for dep in &result.dependencies {
                        println!("     {} {}", "→".bright_black(), dep.bright_black());
                    }
                }
            }
            println!();
        }

        // Statistics summary
        println!("{}", "📊 Summary:".bold().cyan());
        println!(
            "   {} {}",
            "Total targets:".bright_black(),
            results.len().to_string().cyan()
        );

        let total_deps: usize = results.iter().map(|r| r.dependencies.len()).sum();
        println!(
            "   {} {}",
            "Total dependencies:".bright_black(),
            total_deps.to_string().cyan()
        );

        let unique_types = by_type.len();
        println!(
            "   {} {}",
            "Rule types:".bright_black(),
            unique_types.to_string().cyan()
        );
    }

    /// Output results in JSON format
    fn output_json(results: &[QueryResult]) -> Result<()> {
        use std::io::Write;

        let json_results: Vec<_> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "label": r.label,
                    "rule_type": r.rule_type,
                    "dependencies": r.dependencies,
                })
            })
            .collect();

        let output = serde_json::json!({
            "targets": json_results,
            "count": results.len(),
        });

        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{}", serde_json::to_string_pretty(&output)?)?;

        Ok(())
    }

    /// Run watch mode - rebuild on file changes
    async fn run_watch(
        targets: Vec<String>,
        config: Option<String>,
        compilation_mode: Option<String>,
        jobs: Option<u32>,
        verbose: bool,
    ) -> Result<()> {
        use rush_build::watch::FileWatcher;

        let workspace_root = rush_build::find_workspace_root()?;
        let bazelrc_expansion = load_bazelrc_build_expansion(
            &workspace_root,
            &process_command_tokens(),
            verbose,
        )?;
        publish_effective_visibility_flags(
            &bazelrc_expansion.effective_visibility_flags,
        );
        let bazelrc_flags = bazelrc_expansion.rc_flags;
        let build_configs: Vec<String> = config.clone().into_iter().collect();

        // Create watcher
        let watcher = FileWatcher::new(workspace_root.clone(), targets.clone());

        // Initial build
        println!("{}", "🔨 Running initial build...".bold().cyan());
        Self::run_build(
            targets.clone(),
            build_configs.clone(),
            bazelrc_flags.clone(),
            compilation_mode.clone(),
            jobs,
            false, // disk_cache
            None,  // disk_cache_dir
            None,  // remote_executor
            Vec::new(), // remote_platform
            String::new(), // remote_instance
            None,  // remote_cache
            false, // benchmark
            None,  // profile
            None,  // build_event_json
            verbose,
            false, // progress
            false, // streaming
            None,  // metrics_json
            None,  // metrics_csv
            None,  // max_memory_gb
            false, // analyze_only
            false, // validate_producers
            false, // validate_outputs
            false, // use_scheduler
            false, // verify_scheduler_commands
            false, // explain_action_keys
            None,  // explain_action_keys_view
            None,  // scheduler_preflight
            None,  // legacy_breakdown
            None,  // explain_action_keys_format
            None,  // explain_action_graph_format
            None,  // bzl_size_estimate_reuse
            false, // explain_action_graph
            false, // include_tests
        )
        .await?;

        println!("\n{}", "✅ Initial build complete".green().bold());

        // Start watching
        watcher.watch(|_changed_files| {
            // Rebuild on changes
            let bazelrc_expansion = load_bazelrc_build_expansion(
                &workspace_root,
                &process_command_tokens(),
                verbose,
            )?;
            publish_effective_visibility_flags(
                &bazelrc_expansion.effective_visibility_flags,
            );
            let bazelrc_flags = bazelrc_expansion.rc_flags;
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                Self::run_build(
                    targets.clone(),
                    build_configs.clone(),
                    bazelrc_flags,
                    compilation_mode.clone(),
                    jobs,
                    false,
                    None,
                    None,
                    Vec::new(),
                    String::new(),
                    None,
                    false,
                    None,
                    None,
                    verbose,
                    false,
                    false,
                    None,
                    None,
                    None, // max_memory_gb
                    false, // analyze_only
                    false, // validate_producers
                    false, // validate_outputs
                    false, // use_scheduler
                    false, // verify_scheduler_commands
                    false, // explain_action_keys
                    None,  // explain_action_keys_view
                    None,  // scheduler_preflight
                    None,  // legacy_breakdown
                    None,  // explain_action_keys_format
                    None,  // explain_action_graph_format
                    None,  // bzl_size_estimate_reuse
                    false, // explain_action_graph
                    false, // include_tests
                )
                .await
            })
        })?;

        Ok(())
    }

    /// Generate and output dependency graph in DOT format
    async fn run_graph(target: Option<String>, output_file: Option<String>) -> Result<()> {
        use std::fs::File;
        use std::io::Write;

        let workspace_root = rush_build::find_workspace_root()?;
        let orchestrator = BuildOrchestrator::with_workspace(workspace_root);

        // Status message
        if let Some(ref t) = target {
            println!(
                "{} {}",
                "📊 Generating dependency graph for".bright_black(),
                t.cyan()
            );
        } else {
            println!(
                "{}",
                "📊 Generating full workspace dependency graph...".bright_black()
            );
        }

        // Generate DOT graph
        let dot = orchestrator.generate_dot_graph(target.as_deref())?;

        // Count nodes and edges for statistics
        let node_count = dot
            .lines()
            .filter(|l| l.trim().contains(" [label="))
            .count();
        let edge_count = dot.lines().filter(|l| l.trim().contains(" -> ")).count();

        // Write to file or stdout
        match output_file {
            Some(path) => {
                let mut file = File::create(&path)?;
                file.write_all(dot.as_bytes())?;
                println!();
                println!("{} {}", "✓".green(), "Graph written to:".bright_white());
                println!("  {}", path.cyan());
                println!();
                println!("{}", "📈 Graph Statistics:".bold());
                println!(
                    "   {} {}",
                    "Nodes:".bright_black(),
                    node_count.to_string().cyan()
                );
                println!(
                    "   {} {}",
                    "Edges:".bright_black(),
                    edge_count.to_string().cyan()
                );
                println!();
                println!("{}", "🎨 Visualization commands:".bold());
                println!(
                    "   {} {}",
                    "PNG:".bright_black(),
                    format!("dot -Tpng {} -o graph.png", path).green()
                );
                println!(
                    "   {} {}",
                    "SVG:".bright_black(),
                    format!("dot -Tsvg {} -o graph.svg", path).green()
                );
                println!(
                    "   {} {}",
                    "PDF:".bright_black(),
                    format!("dot -Tpdf {} -o graph.pdf", path).green()
                );
                println!(
                    "   {} {}",
                    "Interactive:".bright_black(),
                    format!("xdot {}", path).green()
                );
            }
            None => {
                // When outputting to stdout, just print the DOT content
                print!("{}", dot);
            }
        }

        Ok(())
    }

    /// Show workspace information
    async fn run_info(verbose: bool) -> Result<()> {
        use std::fs;

        let workspace_root = rush_build::find_workspace_root()?;
        let orchestrator = BuildOrchestrator::with_workspace(workspace_root.clone());

        println!("{}", "🏗️  Rush Workspace Information".bold().cyan());
        println!();
        println!(
            "{} {}",
            "📂 Workspace:".bold(),
            workspace_root.display().to_string().bright_white()
        );

        // Count targets
        let all_targets = orchestrator.expand_targets(&["//...".to_string()])?;
        let mut target_counts: HashMap<String, usize> = HashMap::new();

        for target in &all_targets {
            let (package, name) = match parse_target_label_simple(target) {
                Ok((p, n)) => (p, n),
                Err(_) => continue,
            };

            let build_path = workspace_root.join(&package).join("BUILD");
            if let Ok(build_file) = parse_build_file(&build_path) {
                if let Some(rule) = build_file.rules.iter().find(|r| r.name == name) {
                    *target_counts.entry(rule.rule_type.clone()).or_insert(0) += 1;
                }
            }
        }

        println!();
        println!("{}", "🎯 Targets:".bold());
        println!(
            "   {} {}",
            "Total:".bright_black(),
            all_targets.len().to_string().cyan()
        );

        // Sort by rule type for consistent output
        let mut sorted_counts: Vec<_> = target_counts.iter().collect();
        sorted_counts.sort_by_key(|(k, _)| *k);

        for (rule_type, count) in sorted_counts {
            let color_count = match rule_type.as_str() {
                "cc_binary" => count.to_string().green().to_string(),
                "cc_test" => count.to_string().yellow().to_string(),
                "cc_library" => count.to_string().blue().to_string(),
                _ => count.to_string().bright_white().to_string(),
            };
            println!("   {} {}", rule_type.bright_black(), color_count);
        }

        // Check for build outputs
        println!();
        println!("{}", "📦 Build Outputs:".bold());
        let rush_out = cache_paths::workspace_output_root(&workspace_root);
        if rush_out.exists() {
            if let Ok(_metadata) = fs::metadata(&rush_out) {
                println!(
                    "   {} {}",
                    "Directory:".bright_black(),
                    rush_out.display().to_string().cyan()
                );

                // Calculate size
                if verbose {
                    let size = calculate_dir_size(&rush_out).unwrap_or(0);
                    let size_mb = size / 1_000_000;
                    let size_text = if size_mb > 100 {
                        format!("{} MB", size_mb).yellow().to_string()
                    } else {
                        format!("{} MB", size_mb).bright_white().to_string()
                    };
                    println!("   {} {}", "Size:".bright_black(), size_text);
                }
            }

            // Check for build state
            println!();
            println!("{}", "📁 Incremental Build:".bold());
            let state_file = rush_out.join(".build-state.json");
            if state_file.exists() {
                if let Some(stats) = orchestrator.incremental_stats().await {
                    println!(
                        "   {} {}",
                        "Tracked files:".bright_black(),
                        stats.tracked_files.to_string().cyan()
                    );
                    println!(
                        "   {} {}",
                        "Tracked targets:".bright_black(),
                        stats.tracked_targets.to_string().cyan()
                    );

                    if stats.tracked_files > 0 {
                        println!("   {} {}", "Status:".bright_black(), "Active".green());
                    }
                }
            } else {
                println!(
                    "   {} {}",
                    "Status:".bright_black(),
                    "Clean workspace".bright_black()
                );
            }
        } else {
            println!(
                "   {} {}",
                "Status:".bright_black(),
                "Clean workspace".bright_black()
            );
        }

        // Check cache
        println!();
        println!("{}", "💾 Disk Cache:".bold());
        let cache_config = CacheConfig::default();
        if cache_config.cache_root.exists() {
            println!(
                "   {} {}",
                "Directory:".bright_black(),
                cache_config.cache_root.display().to_string().cyan()
            );

            if verbose {
                let size = calculate_dir_size(&cache_config.cache_root).unwrap_or(0);
                let size_mb = size / 1_000_000;
                let size_text = if size_mb > 1000 {
                    format!("{} MB", size_mb).yellow().to_string()
                } else if size_mb > 500 {
                    format!("{} MB", size_mb).bright_white().to_string()
                } else {
                    format!("{} MB", size_mb).green().to_string()
                };
                println!("   {} {}", "Size:".bright_black(), size_text);
            }
            println!("   {} {}", "Status:".bright_black(), "Enabled".green());
        } else {
            println!(
                "   {} {}",
                "Status:".bright_black(),
                "Not configured".bright_black()
            );
        }

        println!();
        Ok(())
    }

    /// Initialize a new workspace
    async fn run_init(directory: Option<PathBuf>, template: String, force: bool) -> Result<()> {
        use std::fs;

        // Determine target directory
        let target_dir = match &directory {
            Some(dir) => {
                println!("🚀 Initializing Rush workspace in: {}", dir.display());
                dir.clone()
            }
            None => {
                println!("🚀 Initializing Rush workspace in current directory");
                std::env::current_dir()?
            }
        };

        // Create directory if it doesn't exist
        if !target_dir.exists() {
            fs::create_dir_all(&target_dir)?;
            println!("   Created directory: {}", target_dir.display());
        }

        // Check if directory is empty (unless force)
        if !force {
            let entries: Vec<_> = fs::read_dir(&target_dir)?.filter_map(|e| e.ok()).collect();

            if !entries.is_empty() {
                anyhow::bail!(
                    "Directory is not empty. Use --force to initialize anyway.\nFound {} items in {}",
                    entries.len(),
                    target_dir.display()
                );
            }
        }

        // Create WORKSPACE file
        let workspace_file = target_dir.join("WORKSPACE");
        let workspace_content = format!(
            "# Rush Workspace\n\
            # Generated by Rush v{}\n\
            \n\
            workspace(\n\
                name = \"{}\",\n\
            )\n",
            VERSION,
            target_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("workspace")
        );
        fs::write(&workspace_file, workspace_content)?;
        println!("   ✅ Created WORKSPACE");

        // Create .gitignore
        let gitignore_file = target_dir.join(".gitignore");
        let gitignore_content = "\
# Rush build outputs\n\
rush-out/\n\
rush-bin\n\
\n\
# Rush cache\n\
.build-state.json\n\
\n\
# IDE\n\
.vscode/\n\
.idea/\n\
*.swp\n\
*~\n\
";
        fs::write(&gitignore_file, gitignore_content)?;
        println!("   ✅ Created .gitignore");

        // Create .bazelrc
        let bazelrc_file = target_dir.join(".bazelrc");
        let bazelrc_content = "\
# Rush configuration\n\
\n\
# Use disk cache by default\n\
build --disk-cache\n\
\n\
# Build configurations\n\
build:release --compilation_mode=opt\n\
build:debug --compilation_mode=dbg\n\
\n\
# Test configuration\n\
test --test_output=errors\n\
";
        fs::write(&bazelrc_file, bazelrc_content)?;
        println!("   ✅ Created .bazelrc");

        // Create template-specific content
        match template.as_str() {
            "minimal" => {
                // Just WORKSPACE file - no example code
                println!("\n✨ Minimal workspace initialized!");
            }
            "cpp" => {
                Self::create_cpp_template(&target_dir)?;
                println!("\n✨ C++ workspace initialized!");
            }
            "rust" => {
                println!("\n⚠️  Rust template not yet implemented");
                println!("   Created minimal workspace instead");
            }
            _ => {
                println!(
                    "\n⚠️  Unknown template '{}', created minimal workspace",
                    template
                );
            }
        }

        println!("\n📚 Next steps:");
        if directory.is_some() {
            println!("   cd {}", target_dir.display());
        }
        println!("   rush build //...");
        println!("   rush test //...");
        println!("\n📖 Learn more: https://github.com/your-org/rush");

        Ok(())
    }

    /// Create C++ template project
    fn create_cpp_template(dir: &Path) -> Result<()> {
        use std::fs;

        // Create hello package
        let hello_dir = dir.join("hello");
        fs::create_dir_all(&hello_dir)?;

        // hello/BUILD
        let build_content = "\
cc_library(\n\
    name = \"greet\",\n\
    srcs = [\"greet.cc\"],\n\
    hdrs = [\"greet.h\"],\n\
)\n\
\n\
cc_binary(\n\
    name = \"hello\",\n\
    srcs = [\"main.cc\"],\n\
    deps = [\":greet\"],\n\
)\n\
\n\
cc_test(\n\
    name = \"greet_test\",\n\
    srcs = [\"greet_test.cc\"],\n\
    deps = [\":greet\"],\n\
)\n\
";
        fs::write(hello_dir.join("BUILD"), build_content)?;

        // hello/greet.h
        let greet_h = "\
#ifndef HELLO_GREET_H_\n\
#define HELLO_GREET_H_\n\
\n\
#include <string>\n\
\n\
std::string greet(const std::string& name);\n\
\n\
#endif  // HELLO_GREET_H_\n\
";
        fs::write(hello_dir.join("greet.h"), greet_h)?;

        // hello/greet.cc
        let greet_cc = "\
#include \"greet.h\"\n\
\n\
std::string greet(const std::string& name) {\n\
    return \"Hello, \" + name + \"!\";\n\
}\n\
";
        fs::write(hello_dir.join("greet.cc"), greet_cc)?;

        // hello/main.cc
        let main_cc = "\
#include <iostream>\n\
#include \"greet.h\"\n\
\n\
int main() {\n\
    std::cout << greet(\"Rush\") << std::endl;\n\
    return 0;\n\
}\n\
";
        fs::write(hello_dir.join("main.cc"), main_cc)?;

        // hello/greet_test.cc
        let test_cc = "\
#include \"greet.h\"\n\
#include <cassert>\n\
#include <iostream>\n\
\n\
int main() {\n\
    std::string result = greet(\"World\");\n\
    assert(result == \"Hello, World!\");\n\
    std::cout << \"✓ greet test passed\" << std::endl;\n\
    return 0;\n\
}\n\
";
        fs::write(hello_dir.join("greet_test.cc"), test_cc)?;

        println!("   ✅ Created hello/ package with library, binary, and test");

        Ok(())
    }

    /// Import Bazel BUILD files to Rush format
    async fn run_import_bazel(
        workspace: Option<PathBuf>,
        output_dir: Option<PathBuf>,
        validate: bool,
        dry_run: bool,
        verbose: bool,
    ) -> Result<()> {
        use std::fs;
        use walkdir::WalkDir;

        // Determine workspace root
        let workspace_root = match workspace {
            Some(ws) => {
                if !ws.exists() {
                    anyhow::bail!("Workspace directory does not exist: {}", ws.display());
                }
                ws
            }
            None => std::env::current_dir()?,
        };

        // Check for Bazel WORKSPACE file
        let bazel_workspace = workspace_root.join("WORKSPACE");
        let has_bazel = bazel_workspace.exists();

        println!("{}", "📦 Bazel to Rush Import".bold().cyan());
        println!();
        println!(
            "   {} {}",
            "Workspace:".bright_black(),
            workspace_root.display().to_string().cyan()
        );
        println!(
            "   {} {}",
            "Bazel detected:".bright_black(),
            if has_bazel {
                "Yes".green().to_string()
            } else {
                "No".yellow().to_string()
            }
        );

        if dry_run {
            println!("   {} {}", "Mode:".bright_black(), "DRY RUN".yellow());
        }
        println!();

        if !has_bazel && verbose {
            println!(
                "   {} {}",
                "⚠".yellow(),
                "No WORKSPACE file found - assuming Bazel-compatible structure".bright_black()
            );
            println!();
        }

        // Find all BUILD files
        println!("{}", "🔍 Scanning for BUILD files...".bright_black());
        let mut build_files = Vec::new();
        let mut bazel_build_count = 0;

        for entry in WalkDir::new(&workspace_root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            // Skip rush-out and bazel-out directories
            if path
                .to_str()
                .map(|s| s.contains("rush-out") || s.contains("bazel-out"))
                .unwrap_or(false)
            {
                continue;
            }

            // Look for BUILD, BUILD.bazel files
            if file_name == "BUILD" || file_name == "BUILD.bazel" {
                build_files.push(path.to_path_buf());
                if file_name == "BUILD.bazel" {
                    bazel_build_count += 1;
                }
            }
        }

        println!(
            "   {} {} BUILD files",
            "Found:".bright_black(),
            build_files.len().to_string().cyan()
        );
        if bazel_build_count > 0 {
            println!(
                "   {} {} BUILD.bazel files",
                "".bright_black(),
                bazel_build_count.to_string().cyan()
            );
        }
        println!();

        if build_files.is_empty() {
            println!("{}", "⚠️  No BUILD files found in workspace".yellow());
            return Ok(());
        }

        // Process each BUILD file
        println!("{}", "🔄 Converting BUILD files...".bold().cyan());
        println!();

        let mut converted_count = 0;
        let mut error_count = 0;
        let mut skipped_count = 0;

        for build_file in &build_files {
            let relative = build_file
                .strip_prefix(&workspace_root)
                .unwrap_or(build_file);

            if verbose {
                println!(
                    "   {} {}",
                    "Processing:".bright_black(),
                    relative.display().to_string().cyan()
                );
            }

            // Read BUILD file
            let content = match fs::read_to_string(build_file) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "   {} Failed to read {}: {}",
                        "✗".red(),
                        relative.display(),
                        e
                    );
                    error_count += 1;
                    continue;
                }
            };

            // Check if conversion is needed
            let is_bazel_specific = content.contains("@")
                || content.contains("bazel_")
                || build_file.file_name().and_then(|n| n.to_str()) == Some("BUILD.bazel");

            if !is_bazel_specific && !dry_run {
                // Already Rush-compatible, potentially
                skipped_count += 1;
                if verbose {
                    println!(
                        "      {} {}",
                        "→".bright_black(),
                        "Already Rush-compatible (skipped)".bright_black()
                    );
                }
                continue;
            }

            // Parse with rush-parse to validate
            match parse_build_file(build_file) {
                Ok(parsed) => {
                    if verbose {
                        println!(
                            "      {} {} rules parsed",
                            "→".bright_black(),
                            parsed.rules.len().to_string().green()
                        );
                    }

                    // Convert Bazel-specific constructs to Rush equivalents
                    let converted_content = Self::convert_bazel_to_rush(&content);

                    // Determine output path
                    let output_path = if let Some(ref out_dir) = output_dir {
                        // Write to separate output directory, preserving structure
                        let rel_path = build_file.strip_prefix(&workspace_root)?;
                        out_dir.join(rel_path).with_file_name("BUILD")
                    } else {
                        // In-place conversion
                        build_file.with_file_name("BUILD")
                    };

                    if !dry_run {
                        // Create parent directory if needed
                        if let Some(parent) = output_path.parent() {
                            fs::create_dir_all(parent)?;
                        }

                        // Write converted BUILD file
                        fs::write(&output_path, converted_content)?;

                        // If original was BUILD.bazel, optionally remove it
                        if build_file.file_name().and_then(|n| n.to_str()) == Some("BUILD.bazel")
                            && output_dir.is_none()
                        {
                            // Remove BUILD.bazel after creating BUILD
                            let _ = fs::remove_file(build_file);
                        }
                    }

                    converted_count += 1;

                    if !verbose {
                        print!(".");
                        use std::io::Write;
                        std::io::stdout().flush().ok();
                    } else {
                        println!(
                            "      {} {}",
                            "✓".green(),
                            "Converted successfully".bright_white()
                        );
                    }
                }
                Err(e) => {
                    if verbose {
                        eprintln!("      {} Parse error: {}", "✗".red(), e);
                    } else {
                        eprintln!("\n   {} {} - {}", "✗".red(), relative.display(), e);
                    }
                    error_count += 1;
                }
            }
        }

        if !verbose && converted_count > 0 {
            println!(); // Newline after progress dots
        }

        // Summary
        println!();
        println!("{}", "📊 Import Summary:".bold().cyan());
        println!(
            "   {} {}",
            "Converted:".bright_black(),
            converted_count.to_string().green()
        );

        if skipped_count > 0 {
            println!(
                "   {} {}",
                "Skipped:".bright_black(),
                skipped_count.to_string().bright_black()
            );
        }

        if error_count > 0 {
            println!(
                "   {} {}",
                "Errors:".bright_black(),
                error_count.to_string().red()
            );
        }

        // Validation step
        if validate && !dry_run {
            println!();
            println!("{}", "✅ Validating converted BUILD files...".bold().cyan());

            let mut validation_errors = 0;

            for build_file in &build_files {
                let build_path = if let Some(ref out_dir) = output_dir {
                    let rel_path = build_file.strip_prefix(&workspace_root)?;
                    out_dir.join(rel_path).with_file_name("BUILD")
                } else {
                    build_file.with_file_name("BUILD")
                };

                if !build_path.exists() {
                    continue;
                }

                match parse_build_file(&build_path) {
                    Ok(_) => {
                        if verbose {
                            println!(
                                "   {} {}",
                                "✓".green(),
                                build_path
                                    .strip_prefix(&workspace_root)
                                    .unwrap_or(&build_path)
                                    .display()
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "   {} {} - {}",
                            "✗".red(),
                            build_path
                                .strip_prefix(&workspace_root)
                                .unwrap_or(&build_path)
                                .display(),
                            e
                        );
                        validation_errors += 1;
                    }
                }
            }

            if validation_errors == 0 {
                println!(
                    "   {} {}",
                    "✓".green(),
                    "All BUILD files are valid".bright_white()
                );
            } else {
                println!(
                    "   {} {} validation errors",
                    "✗".red(),
                    validation_errors.to_string().red()
                );
            }
        }

        // Show next steps
        if !dry_run && converted_count > 0 {
            println!();
            println!("{}", "🎉 Import complete!".green().bold());
            println!();
            println!("{}", "📚 Next steps:".bold());
            println!(
                "   {} {}",
                "1.".bright_black(),
                "Review converted BUILD files for accuracy".bright_white()
            );
            println!(
                "   {} {}",
                "2.".bright_black(),
                "Test builds: rush build //...".bright_white()
            );
            println!(
                "   {} {}",
                "3.".bright_black(),
                "Run tests: rush test //...".bright_white()
            );
        } else if dry_run {
            println!();
            println!("{}", "ℹ️  Dry run complete - no files were modified".cyan());
        }

        println!();
        Ok(())
    }

    /// Convert Bazel-specific constructs to Rush equivalents
    fn convert_bazel_to_rush(content: &str) -> String {
        let mut converted = content.to_string();

        // Common Bazel to Rush conversions
        // External dependencies: @repo//path:target -> //external/repo/path:target
        if converted.contains('@') {
            converted = Self::convert_external_deps(&converted);
        }

        // Bazel-specific attributes that need translation
        converted = converted
            .replace(
                "visibility = [\"//visibility:public\"]",
                "visibility = [\"public\"]",
            )
            .replace(
                "visibility = [\"//visibility:private\"]",
                "visibility = [\"private\"]",
            )
            .replace("alwayslink = True", "alwayslink = true")
            .replace("alwayslink = False", "alwayslink = false");

        // Convert common Bazel rule names to Rush equivalents if needed
        // (Most are compatible, but some might need mapping)

        converted
    }

    /// Convert external dependency references from Bazel to Rush format
    fn convert_external_deps(content: &str) -> String {
        use regex::Regex;

        // Convert @repo//path:target to //external/repo/path:target
        let external_re = Regex::new(r#"@([a-zA-Z0-9_-]+)//([^"':\s]*):([^"':\s]+)"#).unwrap();
        external_re
            .replace_all(content, "//external/$1/$2:$3")
            .to_string()
    }
}

/// Helper to parse target label (simple version for info command)
fn parse_target_label_simple(label: &str) -> Result<(String, String)> {
    if let Some(without_slashes) = label.strip_prefix("//") {
        if let Some(colon_pos) = without_slashes.find(':') {
            let package = without_slashes[..colon_pos].to_string();
            let name = without_slashes[colon_pos + 1..].to_string();
            Ok((package, name))
        } else {
            anyhow::bail!("Invalid target label")
        }
    } else {
        anyhow::bail!("Invalid target label")
    }
}

/// Calculate directory size recursively
fn calculate_dir_size(path: &Path) -> Result<u64> {
    use std::fs;

    let mut total = 0u64;

    if path.is_file() {
        return Ok(fs::metadata(path)?.len());
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;

        if metadata.is_dir() {
            total += calculate_dir_size(&entry.path())?;
        } else {
            total += metadata.len();
        }
    }

    Ok(total)
}

/// Result from a query operation
#[derive(Debug, Clone)]
struct QueryResult {
    /// Target label
    label: String,
    /// Rule type (e.g., "cc_library", "cc_binary")
    rule_type: String,
    /// List of dependency labels
    dependencies: Vec<String>,
}

/// Phase 5.C.4 commit 124 (Batch P): dispatch the
/// `--bzl-size-estimate-reuse=off|on|verify` tri-state
/// CLI flag into the env vars that
/// `GlobalBzlCache::put_symbols_with_label` reads.
///
/// CLI wins over env: an explicit `off` actively
/// REMOVES the env vars even if a wrapper script set
/// them. This is the same pattern as
/// `--scheduler-preflight` / `--legacy-breakdown`.
///
/// When `mode` is `None` (CLI flag not provided), the
/// env-var state is left as-is so wrapper scripts can
/// still opt in via `RUSH_BZL_REUSE_SIZE_ESTIMATE=1`.
///
/// Exposed at module scope (rather than method-local)
/// so the unit tests below can exercise the
/// dispatch behavior without a full Cli invocation.
fn apply_bzl_size_estimate_reuse(mode: Option<&str>) -> Result<()> {
    let Some(mode) = mode else {
        return Ok(());
    };
    match mode.to_ascii_lowercase().as_str() {
        "off" => {
            std::env::remove_var("RUSH_BZL_REUSE_SIZE_ESTIMATE");
            std::env::remove_var("RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY");
        }
        "on" => {
            std::env::set_var("RUSH_BZL_REUSE_SIZE_ESTIMATE", "1");
            std::env::remove_var("RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY");
        }
        "verify" => {
            std::env::set_var("RUSH_BZL_REUSE_SIZE_ESTIMATE", "1");
            std::env::set_var("RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY", "1");
        }
        other => {
            anyhow::bail!(
                "--bzl-size-estimate-reuse: unknown value {:?}; \
                 expected one of: off, on, verify",
                other
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod bzl_size_estimate_reuse_cli_tests {
    use super::apply_bzl_size_estimate_reuse;
    use std::sync::{Mutex, OnceLock};

    /// Serialize tests because env vars are
    /// process-global; cargo runs tests in parallel
    /// by default.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn clean_env() {
        std::env::remove_var("RUSH_BZL_REUSE_SIZE_ESTIMATE");
        std::env::remove_var("RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY");
    }

    /// Pro test #1: `--bzl-size-estimate-reuse=on`
    /// sets the reuse env var, leaves verify unset.
    #[test]
    fn cli_on_enables_reuse() {
        let _l = env_lock();
        clean_env();
        apply_bzl_size_estimate_reuse(Some("on")).unwrap();
        assert_eq!(
            std::env::var("RUSH_BZL_REUSE_SIZE_ESTIMATE").ok(),
            Some("1".into())
        );
        assert_eq!(
            std::env::var("RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY").ok(),
            None
        );
        clean_env();
    }

    /// Pro test #2: `--bzl-size-estimate-reuse=verify`
    /// sets BOTH env vars.
    #[test]
    fn cli_verify_enables_verify_mode() {
        let _l = env_lock();
        clean_env();
        apply_bzl_size_estimate_reuse(Some("verify")).unwrap();
        assert_eq!(
            std::env::var("RUSH_BZL_REUSE_SIZE_ESTIMATE").ok(),
            Some("1".into())
        );
        assert_eq!(
            std::env::var("RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY").ok(),
            Some("1".into())
        );
        clean_env();
    }

    /// Pro test #3: `--bzl-size-estimate-reuse=off`
    /// actively CLEARS env vars even when a wrapper
    /// script set them. CLI wins.
    #[test]
    fn cli_off_overrides_env() {
        let _l = env_lock();
        std::env::set_var("RUSH_BZL_REUSE_SIZE_ESTIMATE", "1");
        std::env::set_var("RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY", "1");
        apply_bzl_size_estimate_reuse(Some("off")).unwrap();
        assert_eq!(std::env::var("RUSH_BZL_REUSE_SIZE_ESTIMATE").ok(), None);
        assert_eq!(
            std::env::var("RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY").ok(),
            None
        );
        clean_env();
    }

    /// Pro test #4: an invalid value fails cleanly
    /// with a helpful error message listing valid
    /// values.
    #[test]
    fn invalid_value_fails_cleanly() {
        let _l = env_lock();
        clean_env();
        let err = apply_bzl_size_estimate_reuse(Some("maybe"))
            .expect_err("invalid value should error");
        let msg = format!("{}", err);
        assert!(msg.contains("--bzl-size-estimate-reuse"));
        assert!(msg.contains("off"));
        assert!(msg.contains("on"));
        assert!(msg.contains("verify"));
        assert!(msg.contains("maybe"));
        clean_env();
    }

    /// Bonus: case-insensitive matching.
    #[test]
    fn case_insensitive_matching() {
        let _l = env_lock();
        clean_env();
        apply_bzl_size_estimate_reuse(Some("ON")).unwrap();
        assert_eq!(
            std::env::var("RUSH_BZL_REUSE_SIZE_ESTIMATE").ok(),
            Some("1".into())
        );
        clean_env();
        apply_bzl_size_estimate_reuse(Some("Verify")).unwrap();
        assert_eq!(
            std::env::var("RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY").ok(),
            Some("1".into())
        );
        clean_env();
    }

    /// Bonus: None means "don't touch env state."
    #[test]
    fn none_preserves_env() {
        let _l = env_lock();
        std::env::set_var("RUSH_BZL_REUSE_SIZE_ESTIMATE", "1");
        apply_bzl_size_estimate_reuse(None).unwrap();
        // env var must still be set
        assert_eq!(
            std::env::var("RUSH_BZL_REUSE_SIZE_ESTIMATE").ok(),
            Some("1".into())
        );
        clean_env();
    }
}

#[cfg(test)]
mod visibility_compatibility_cli_tests {
    use super::{
        expand_bazelrc_build, normalized_daemon_build_argv, Cli,
    };
    use bytes::Bytes;
    use clap::Parser;
    use rush_build::{BazelRc, BazelRcNamedConfig};
    use std::collections::HashMap;

    #[test]
    fn accepts_bazel_boolean_visibility_flag_spellings() {
        let cases = [
            vec![
                "rush",
                "build",
                "--incompatible_enforce_config_setting_visibility",
                "//:target",
            ],
            vec![
                "rush",
                "build",
                concat!(
                    "--incompatible_config_setting_",
                    "private_default_visibility=false"
                ),
                "//:target",
            ],
            vec![
                "rush",
                "build",
                "--noincompatible_package_group_has_public_syntax",
                "--noincompatible_fix_package_group_reporoot_syntax",
                "//:target",
            ],
            vec![
                "rush",
                "build",
                "--incompatible_enforce_config_setting_visibility=false",
                "--incompatible_enforce_config_setting_visibility=true",
                "//:target",
            ],
            vec![
                "rush",
                "b",
                "--noincompatible_enforce_config_setting_visibility",
                "--incompatible_enforce_config_setting_visibility",
                "//:target",
            ],
        ];

        for arguments in cases {
            Cli::try_parse_from(arguments).unwrap();
        }

        for raw_value in [
            "true", "TRUE", "1", "yes", "Yes", "t", "T", "y", "Y",
            "false", "FALSE", "0", "no", "No", "f", "F", "n", "N",
        ] {
            let option = format!(
                "--incompatible_enforce_config_setting_visibility={raw_value}"
            );
            Cli::try_parse_from(["rush", "build", &option, "//:target"])
                .unwrap();
        }

        for raw_value in ["on", "off", "enabled"] {
            let option = format!(
                "--incompatible_enforce_config_setting_visibility={raw_value}"
            );
            assert!(
                Cli::try_parse_from([
                    "rush",
                    "build",
                    &option,
                    "//:target"
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn expands_repeated_and_nested_configs_in_place() {
        let rc = BazelRc {
            common_flags: vec![
                "--incompatible_enforce_config_setting_visibility=false"
                    .to_string(),
            ],
            build_flags: vec![
                "--incompatible_package_group_has_public_syntax"
                    .to_string(),
            ],
            test_flags: Vec::new(),
            run_flags: Vec::new(),
            configs: HashMap::from([
                (
                    "strict".to_string(),
                    BazelRcNamedConfig {
                        build_flags: vec![
                            concat!(
                                "--incompatible_enforce_config_setting_",
                                "visibility=true"
                            )
                            .to_string(),
                            "--config=nested".to_string(),
                        ],
                        ..Default::default()
                    },
                ),
                (
                    "nested".to_string(),
                    BazelRcNamedConfig {
                        build_flags: vec![concat!(
                            "--incompatible_config_setting_",
                            "private_default_visibility"
                        )
                        .to_string()],
                        ..Default::default()
                    },
                ),
                (
                    "legacy".to_string(),
                    BazelRcNamedConfig {
                        build_flags: vec![concat!(
                            "--noincompatible_enforce_config_setting_",
                            "visibility"
                        )
                        .to_string()],
                        ..Default::default()
                    },
                ),
            ]),
            startup_flags: Vec::new(),
        };

        let cli = vec![
            "--incompatible_enforce_config_setting_visibility=true"
                .to_string(),
            "--config=strict".to_string(),
            "--incompatible_enforce_config_setting_visibility=false"
                .to_string(),
            "--config".to_string(),
            "legacy".to_string(),
            "--incompatible_enforce_config_setting_visibility=true"
                .to_string(),
        ];
        let expansion = expand_bazelrc_build(&rc, &cli).unwrap();
        assert_eq!(
            expansion.effective_visibility_flags,
            vec![
                "--incompatible_enforce_config_setting_visibility=false",
                "--incompatible_package_group_has_public_syntax",
                "--incompatible_enforce_config_setting_visibility=true",
                "--incompatible_enforce_config_setting_visibility=true",
                concat!(
                    "--incompatible_config_setting_",
                    "private_default_visibility"
                ),
                "--incompatible_enforce_config_setting_visibility=false",
                "--noincompatible_enforce_config_setting_visibility",
                "--incompatible_enforce_config_setting_visibility=true",
            ]
        );
    }

    #[test]
    fn rejects_recursive_config_cycles() {
        let rc = BazelRc {
            common_flags: Vec::new(),
            build_flags: Vec::new(),
            test_flags: Vec::new(),
            run_flags: Vec::new(),
            configs: HashMap::from([
                (
                    "a".to_string(),
                    BazelRcNamedConfig {
                        build_flags: vec!["--config=b".to_string()],
                        ..Default::default()
                    },
                ),
                (
                    "b".to_string(),
                    BazelRcNamedConfig {
                        build_flags: vec!["--config=a".to_string()],
                        ..Default::default()
                    },
                ),
            ]),
            startup_flags: Vec::new(),
        };
        let error = expand_bazelrc_build(
            &rc,
            &["--config=a".to_string()],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("a -> b -> a"), "{error}");
    }

    #[test]
    fn common_terminator_does_not_hide_build_or_cli_flags() {
        let rc = BazelRc {
            common_flags: vec![
                concat!(
                    "--noincompatible_enforce_config_setting_",
                    "visibility"
                )
                .to_string(),
                "--".to_string(),
                "--config=not_expanded".to_string(),
                concat!(
                    "--incompatible_enforce_config_setting_",
                    "visibility"
                )
                .to_string(),
            ],
            build_flags: vec![concat!(
                "--incompatible_enforce_config_setting_",
                "visibility=true"
            )
            .to_string()],
            test_flags: Vec::new(),
            run_flags: Vec::new(),
            configs: HashMap::new(),
            startup_flags: Vec::new(),
        };
        let expansion = expand_bazelrc_build(
            &rc,
            &[
                concat!(
                    "--noincompatible_enforce_config_setting_",
                    "visibility"
                )
                .to_string(),
                "--".to_string(),
                concat!(
                    "--incompatible_package_group_has_",
                    "public_syntax"
                )
                .to_string(),
            ],
        )
        .unwrap();
        assert_eq!(
            expansion.effective_visibility_flags,
            vec![
                concat!(
                    "--noincompatible_enforce_config_setting_",
                    "visibility"
                ),
                concat!(
                    "--incompatible_enforce_config_setting_",
                    "visibility=true"
                ),
                concat!(
                    "--noincompatible_enforce_config_setting_",
                    "visibility"
                ),
            ]
        );
    }

    #[test]
    fn config_terminator_does_not_hide_later_cli_flags() {
        let rc = BazelRc {
            common_flags: Vec::new(),
            build_flags: Vec::new(),
            test_flags: Vec::new(),
            run_flags: Vec::new(),
            configs: HashMap::from([(
                "bounded".to_string(),
                BazelRcNamedConfig {
                    build_flags: vec![
                        concat!(
                            "--incompatible_enforce_config_setting_",
                            "visibility=true"
                        )
                        .to_string(),
                        "--".to_string(),
                        "--config=not_expanded".to_string(),
                        concat!(
                            "--incompatible_package_group_has_",
                            "public_syntax"
                        )
                        .to_string(),
                    ],
                    ..Default::default()
                },
            )]),
            startup_flags: Vec::new(),
        };
        let expansion = expand_bazelrc_build(
            &rc,
            &[
                "--config=bounded".to_string(),
                concat!(
                    "--noincompatible_enforce_config_setting_",
                    "visibility"
                )
                .to_string(),
            ],
        )
        .unwrap();
        assert_eq!(
            expansion.effective_visibility_flags,
            vec![
                concat!(
                    "--incompatible_enforce_config_setting_",
                    "visibility=true"
                ),
                concat!(
                    "--noincompatible_enforce_config_setting_",
                    "visibility"
                ),
            ]
        );
    }

    #[test]
    fn accepts_repeated_build_configs() {
        Cli::try_parse_from([
            "rush",
            "build",
            "--config=first",
            "--config",
            "second",
            "//:target",
        ])
        .unwrap();
    }

    #[test]
    fn daemon_alias_normalization_preserves_targets_and_flags() {
        let argv = vec![
            Bytes::from_static(b"rush"),
            Bytes::from_static(b"b"),
            Bytes::from_static(b"//pkg:target"),
            Bytes::from_static(
                b"--incompatible_enforce_config_setting_visibility=false",
            ),
            Bytes::from_static(b"--daemon=auto"),
            Bytes::from_static(b"--jobs=4"),
        ];

        assert_eq!(
            normalized_daemon_build_argv(argv),
            vec![
                Bytes::from_static(b"rush"),
                Bytes::from_static(b"build"),
                Bytes::from_static(b"//pkg:target"),
                Bytes::from_static(
                    b"--incompatible_enforce_config_setting_visibility=false",
                ),
                Bytes::from_static(b"--jobs=4"),
            ]
        );
    }

    #[test]
    fn daemon_split_value_is_removed_before_argument_terminator() {
        let argv = vec![
            Bytes::from_static(b"rush"),
            Bytes::from_static(b"b"),
            Bytes::from_static(b"--daemon"),
            Bytes::from_static(b"on"),
            Bytes::from_static(b"//pkg:target"),
            Bytes::from_static(b"--"),
            Bytes::from_static(b"--daemon=target-name"),
        ];

        assert_eq!(
            normalized_daemon_build_argv(argv),
            vec![
                Bytes::from_static(b"rush"),
                Bytes::from_static(b"build"),
                Bytes::from_static(b"//pkg:target"),
                Bytes::from_static(b"--"),
                Bytes::from_static(b"--daemon=target-name"),
            ]
        );
    }
}

#[cfg(test)]
mod daemon_protocol_rollout_tests {
    use super::{Cli, DaemonDiscovery};
    use rush_transport_grpc::{DaemonMetadata, LocalEndpoint};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rush-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn discovery_path_is_protocol_versioned() {
        let workspace = temp_dir("protocol-path");
        let discovery = DaemonDiscovery::for_workspace(&workspace).unwrap();
        let version = format!(
            "v{}.{}",
            rush_protocol::PROTOCOL_MAJOR,
            rush_protocol::PROTOCOL_MINOR
        );
        assert_eq!(
            discovery
                .dir
                .parent()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str()),
            Some(version.as_str())
        );
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test]
    async fn client_rejects_stale_daemon_metadata_before_connecting() {
        let root = temp_dir("protocol-metadata");
        let dir = root.join("daemon");
        std::fs::create_dir_all(&dir).unwrap();
        let discovery = DaemonDiscovery {
            socket_path: dir.join("rushd.sock"),
            cookie_path: dir.join("cookie"),
            metadata_path: dir.join("metadata.json"),
            dir,
        };
        let endpoint = LocalEndpoint::UnixSocket(
            discovery.socket_path.clone(),
        );
        let mut metadata = DaemonMetadata::new(
            &endpoint,
            &discovery.cookie_path,
            rush_protocol::PROTOCOL_MAJOR,
            rush_protocol::PROTOCOL_MINOR,
        );
        metadata.protocol_version = "1.0".to_string();
        metadata.write_atomic(&discovery.metadata_path).unwrap();

        let error = match Cli::connect_daemon(&discovery).await {
            Ok(_) => panic!("stale daemon metadata was accepted"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("does not match client protocol"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod reapi_cli_tests {
    use super::*;

    #[test]
    fn remote_selection_requires_an_endpoint() {
        assert!(Cli::try_parse_from(["rush", "build", "//:app",
            "--remote-platform", "toolchain=pinned"]).is_err());
        assert!(Cli::try_parse_from(["rush", "build", "//:app",
            "--remote-instance", "test"]).is_err());
        assert!(Cli::try_parse_from(["rush", "build", "//:app",
            "--remote-executor", "http://127.0.0.1:9000",
            "--remote-platform", "toolchain=pinned"]).is_ok());
        assert!(Cli::try_parse_from(["rush", "build", "//:app"]).is_ok());
    }

    #[test]
    fn reject_zero_client_deadline_and_accept_rexec_alias() {
        assert!(Cli::try_parse_from(["rush", "rexec", "action.json",
            "--endpoint", "http://127.0.0.1:9000"]).is_ok());
        assert!(Cli::try_parse_from(["rush", "remote-execute", "action.json",
            "--endpoint", "http://127.0.0.1:9000", "--deadline-seconds", "0"]).is_err());
    }
}
