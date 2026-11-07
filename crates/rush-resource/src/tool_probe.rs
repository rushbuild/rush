//! Phase 5.C.4 commit 36: tool probe cache skeleton.
//!
//! Pro's pivot directive #4: "Do not put tool probing inside
//! scheduler eligibility. Create a toolchain probe cache."
//!
//! Tools are not just strings in argv. They have identity,
//! existence, executability, dynamic-link dependencies,
//! versions, and sometimes horrifying environment requirements.
//! This module captures that structure as a per-build-invocation
//! cache so:
//!
//!   - Multiple call sites that need the same tool probe
//!     don't re-run the file/exec checks.
//!   - Tool-availability state surfaces via a typed `ProbeResult`
//!     with diagnostic detail (path, source, runtime status).
//!   - Future hermetic toolchain work (post-5.C.5) replaces
//!     the env-var/PATH probe with a typed `ToolchainConfig`
//!     reference; the cache shape stays useful.
//!   - Action-key classification (commit 32+35) consumes
//!     `ProbeResult.path` for `EnvOverrideToolPath` vs
//!     `HostToolPath` distinction.
//!   - The future scheduler preflight report (commit 38)
//!     dumps the probe cache state as part of the readiness
//!     overview.
//!
//! Per Pro's "no aggressive probing yet" constraint:
//! `runtime_status` is `Unknown` by default. The cache stores
//! whatever cheap probes (file exists + executable bit) are
//! reasonable; running the binary with `--version` to detect
//! ABI failures is opt-in (`probe_with_runtime_check`) for
//! diagnostic-mode callers.
//!
//! Today's commit ships the data model + lookup helpers + a
//! handful of well-known-tool keys. Wiring the cache into
//! tf_proto_library / tf_cc_library / etc. is a follow-up;
//! for now they continue to call their inline probes
//! (`probe_grpc_cpp_plugin`, etc.).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Phase 5.C.4 commit 36: identifier for a well-known tool.
/// Adding a new tool means extending this enum. Strings would
/// be more flexible but lose exhaustive match correctness —
/// the future preflight report wants to enumerate every known
/// tool, not "every tool the registry happens to have probed."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolId {
    /// `protoc` — Protocol Buffers compiler. Used by
    /// cc_proto_library, tf_proto_library.
    Protoc,
    /// `grpc_cpp_plugin` — gRPC code generator (protoc plugin).
    /// Used by tf_proto_library when has_services or
    /// create_grpc_library is set.
    GrpcCppPlugin,
    /// `mlir-tblgen` — MLIR table generator. Used by
    /// gentbl_cc_library (not yet allowlisted; commit 39's
    /// fake-tool follow-up will exercise it hermetically).
    MlirTblgen,
    /// `llvm-tblgen` — LLVM table generator. Sibling of
    /// MlirTblgen.
    LlvmTblgen,
    /// `cxx` — system C++ compiler (g++ / clang++). Used by
    /// every cc_library / cc_binary / cc_test / tf_cc_*.
    Cxx,
    /// `cc` — system C compiler (gcc / clang). Used for `.c`
    /// sources.
    Cc,
    /// `ar` — archiver. Used by every CcArchive.
    Ar,
    /// `ld` — linker (often invoked indirectly via cxx for
    /// CcLink, but ld_shared standalone calls it directly).
    Ld,
    /// Phase 5.C.4 commit 190 (Batch Y): `bazel` — the bazel
    /// binary itself. Used by the fetch warmup path to invoke
    /// `bazel sync --only=…`. Required for any external-repo
    /// fetch in the current scheme.
    Bazel,
}

impl ToolId {
    /// Phase 5.C.4 commit 36: short name for diagnostic output.
    /// Used by `format_probe_cache`.
    pub fn name(&self) -> &'static str {
        match self {
            ToolId::Protoc => "protoc",
            ToolId::GrpcCppPlugin => "grpc_cpp_plugin",
            ToolId::MlirTblgen => "mlir-tblgen",
            ToolId::LlvmTblgen => "llvm-tblgen",
            ToolId::Cxx => "cxx",
            ToolId::Cc => "cc",
            ToolId::Ar => "ar",
            ToolId::Ld => "ld",
            ToolId::Bazel => "bazel",
        }
    }

    /// Phase 5.C.4 commit 190 (Batch Y): every well-known tool
    /// the audit pass covers. Iteration order is stable
    /// (matches enum declaration order) so the audit report
    /// is deterministic across runs.
    pub fn all() -> &'static [ToolId] {
        &[
            ToolId::Protoc,
            ToolId::GrpcCppPlugin,
            ToolId::MlirTblgen,
            ToolId::LlvmTblgen,
            ToolId::Cxx,
            ToolId::Cc,
            ToolId::Ar,
            ToolId::Ld,
            ToolId::Bazel,
        ]
    }

    /// Phase 5.C.4 commit 190 (Batch Y): operator-visible env
    /// var that overrides this tool's discovery, if any. None
    /// means "no override — fall back to PATH + fixed
    /// locations." The audit pass consults this before doing
    /// PATH lookup.
    pub fn env_override(&self) -> Option<&'static str> {
        match self {
            ToolId::Protoc => Some("RUSH_PROTOC"),
            ToolId::GrpcCppPlugin => Some("RUSH_GRPC_CPP_PLUGIN"),
            ToolId::MlirTblgen => Some("RUSH_MLIR_TBLGEN"),
            ToolId::LlvmTblgen => Some("RUSH_LLVM_TBLGEN"),
            ToolId::Cxx => Some("CXX"),
            ToolId::Cc => Some("CC"),
            ToolId::Ar => Some("AR"),
            ToolId::Ld => Some("LD"),
            ToolId::Bazel => Some("RUSH_BAZEL_PATH"),
        }
    }
}

/// Phase 5.C.4 commit 190 (Batch Y): typed audit status per
/// Pro's commit-190 spec. Maps the lower-level ProbeResult
/// into the operator-facing classification.
///
///   Available     — found AND executable AND (runtime check
///                   succeeded OR not run).
///   Missing       — not found at all OR not executable.
///   AbiBroken     — found, executable, but ld.so reported
///                   the binary cannot load. Commit 31b's
///                   diagnostic patterns drive this
///                   classification.
///   WrongVersion  — found and executes, but a version
///                   probe reported an unsupported version.
///                   Not produced by the default audit pass;
///                   reserved for future per-rule version
///                   gates.
///   Unknown       — neither cheap nor runtime probes ran.
///                   Should not appear in a completed audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAuditStatus {
    Available,
    Missing,
    AbiBroken,
    WrongVersion,
    Unknown,
}

impl ToolAuditStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolAuditStatus::Available => "available",
            ToolAuditStatus::Missing => "missing",
            ToolAuditStatus::AbiBroken => "abi_broken",
            ToolAuditStatus::WrongVersion => "wrong_version",
            ToolAuditStatus::Unknown => "unknown",
        }
    }

    /// Phase 5.C.4 commit 190 (Batch Y): classify a ProbeResult
    /// into the typed audit status. Pure function — no
    /// side effects, deterministic.
    pub fn classify(result: &ProbeResult) -> Self {
        // Path missing or file gone → Missing.
        if result.path.is_none() || !result.exists {
            return ToolAuditStatus::Missing;
        }
        // File present but not executable → Missing (operator
        // can't run it).
        if !result.executable {
            return ToolAuditStatus::Missing;
        }
        // Runtime probe states.
        match &result.runtime_status {
            RuntimeStatus::Executes => ToolAuditStatus::Available,
            RuntimeStatus::FailsToExecute(diag) => {
                if diag.starts_with("ld.so error:") {
                    ToolAuditStatus::AbiBroken
                } else {
                    // Non-ld.so runtime failure. Could be
                    // a wrong-version exit code, a panic,
                    // or other. Without finer classification,
                    // treat as AbiBroken — the binary
                    // doesn't run.
                    ToolAuditStatus::AbiBroken
                }
            }
            // Runtime not probed; cheap checks passed.
            RuntimeStatus::Unknown => ToolAuditStatus::Available,
        }
    }
}

/// Phase 5.C.4 commit 36: how the tool was discovered.
/// Tracked separately from the path itself so the
/// action-key classifier (commit 32) can distinguish
/// `EnvOverrideToolPath` from `HostToolPath` without
/// re-walking the env / PATH at classification time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeSource {
    /// User-supplied via env var (e.g. `RUSH_GRPC_CPP_PLUGIN`).
    /// Carries the env var name for diagnostic output.
    EnvOverride(&'static str),
    /// Found by walking `$PATH`.
    Path,
    /// One of the hardcoded fallback locations
    /// (`/usr/bin/<name>`, `/usr/local/bin/<name>`).
    FixedFallback,
    /// Future: typed `ToolchainConfig` reference. Not used
    /// today; reserved for post-5.C.5 hermetic toolchain
    /// work.
    ToolchainConfig,
}

/// Phase 5.C.4 commit 36: runtime executability state.
/// Default `Unknown` — running the binary with `--version`
/// or similar to verify ld.so resolution is opt-in. Pro's
/// "no aggressive probing yet" — the runtime check has
/// latency and side effects (exec arbitrary binary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeStatus {
    /// Not yet probed at runtime. Default state.
    Unknown,
    /// Tool was successfully executed (e.g. `--version`
    /// returned 0). ld.so resolution succeeded.
    Executes,
    /// Tool exists and is file-mode executable, but failed
    /// at exec time. Carries the diagnostic message
    /// (typically containing ld.so error text from the
    /// commit-31b detection patterns).
    FailsToExecute(String),
}

/// Phase 5.C.4 commit 36: result of probing one tool.
/// Captures everything useful for diagnostic surfaces and
/// action-key classification.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    /// Resolved absolute path. `None` when the tool was not
    /// found in any searched location.
    pub path: Option<PathBuf>,
    /// Source of the resolved path. `None` when path is
    /// `None`.
    pub source: Option<ProbeSource>,
    /// File-system existence check. Always populated.
    pub exists: bool,
    /// Executable-bit check on Unix; `is_file()` on other
    /// platforms.
    pub executable: bool,
    /// Runtime status. Defaults to `Unknown` when the
    /// caller hasn't run the binary; `FailsToExecute` when
    /// a runtime check has been done and failed.
    pub runtime_status: RuntimeStatus,
    /// Free-form diagnostic, surfaces when path is `None`
    /// or runtime_status is `FailsToExecute`. Empty when
    /// nothing went wrong.
    pub diagnostic: String,
}

impl ProbeResult {
    /// Phase 5.C.4 commit 36: "tool not found" probe result.
    /// Used by the cache when a lookup turned up nothing.
    pub fn not_found(diagnostic: String) -> Self {
        Self {
            path: None,
            source: None,
            exists: false,
            executable: false,
            runtime_status: RuntimeStatus::Unknown,
            diagnostic,
        }
    }

    /// Phase 5.C.4 commit 36: "tool found via cheap probe"
    /// result. exists + executable populated; runtime_status
    /// stays Unknown until an explicit runtime probe runs.
    pub fn found(
        path: PathBuf,
        source: ProbeSource,
        exists: bool,
        executable: bool,
    ) -> Self {
        Self {
            path: Some(path),
            source: Some(source),
            exists,
            executable,
            runtime_status: RuntimeStatus::Unknown,
            diagnostic: String::new(),
        }
    }

    /// Phase 5.C.4 commit 36: is this tool ready for
    /// scheduler dispatch? True iff the tool was found AND
    /// is executable AND the runtime check (if performed)
    /// succeeded. The future `MissingTool` eligibility
    /// reason (commit 37) reads this.
    pub fn is_usable(&self) -> bool {
        if !self.exists || !self.executable {
            return false;
        }
        match &self.runtime_status {
            RuntimeStatus::FailsToExecute(_) => false,
            // Unknown counts as usable — the cheap probes
            // passed; we haven't actually run the binary.
            RuntimeStatus::Unknown | RuntimeStatus::Executes => true,
        }
    }
}

/// Phase 5.C.4 commit 36: per-build tool probe cache. Owned
/// by the orchestrator; passed by reference to call sites
/// that need tool resolution.
///
/// Today's API is read-only after construction; future
/// commits add `record_runtime_status` for opt-in runtime
/// probing.
#[derive(Debug, Default, Clone)]
pub struct ToolProbeCache {
    entries: HashMap<ToolId, ProbeResult>,
}

impl ToolProbeCache {
    /// Empty cache. Callers populate via `insert` or via the
    /// rule-specific probe functions (e.g. tf_proto_library's
    /// `probe_grpc_cpp_plugin` will eventually populate this
    /// cache rather than running the probe inline).
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a probe result for a tool. Last-write wins;
    /// callers should populate the cache once at orchestrator
    /// startup.
    pub fn insert(&mut self, tool: ToolId, result: ProbeResult) {
        self.entries.insert(tool, result);
    }

    /// Look up a tool's probe result. Returns None if the
    /// cache hasn't seen this tool yet — caller decides
    /// whether to run an inline probe or treat as missing.
    pub fn get(&self, tool: ToolId) -> Option<&ProbeResult> {
        self.entries.get(&tool)
    }

    /// Phase 5.C.4 commit 36: count tools by usability.
    /// Returns (usable, missing/broken). Used by future
    /// preflight report (commit 38) to summarize tool
    /// readiness.
    pub fn usability_summary(&self) -> (usize, usize) {
        let mut usable = 0;
        let mut broken = 0;
        for r in self.entries.values() {
            if r.is_usable() {
                usable += 1;
            } else {
                broken += 1;
            }
        }
        (usable, broken)
    }

    /// Iterate over (tool, result) entries.
    pub fn iter(&self) -> impl Iterator<Item = (&ToolId, &ProbeResult)> {
        self.entries.iter()
    }
}

/// Phase 5.C.4 commit 36: format the probe cache state as a
/// multi-line dump. Future preflight report (commit 38) uses
/// this for the tool-readiness section.
pub fn format_probe_cache(cache: &ToolProbeCache) -> Vec<String> {
    let mut out = Vec::new();
    let (usable, broken) = cache.usability_summary();
    out.push(format!(
        "Tool probe cache ({} entries; {} usable, {} broken/missing):",
        cache.entries.len(),
        usable,
        broken,
    ));
    let mut entries: Vec<_> = cache.entries.iter().collect();
    entries.sort_by_key(|(t, _)| t.name());
    for (tool, result) in entries {
        out.push(format!("  {}", tool.name()));
        match &result.path {
            Some(p) => out.push(format!("    path: {}", p.display())),
            None => out.push("    path: <not found>".to_string()),
        }
        match &result.source {
            Some(ProbeSource::EnvOverride(var)) => {
                out.push(format!("    source: EnvOverride({})", var));
            }
            Some(ProbeSource::Path) => {
                out.push("    source: $PATH".to_string());
            }
            Some(ProbeSource::FixedFallback) => {
                out.push("    source: FixedFallback".to_string());
            }
            Some(ProbeSource::ToolchainConfig) => {
                out.push("    source: ToolchainConfig".to_string());
            }
            None => {}
        }
        out.push(format!(
            "    exists: {}, executable: {}",
            result.exists, result.executable
        ));
        match &result.runtime_status {
            RuntimeStatus::Unknown => {}
            RuntimeStatus::Executes => {
                out.push("    runtime: Executes".to_string());
            }
            RuntimeStatus::FailsToExecute(diag) => {
                out.push(format!("    runtime: FailsToExecute({})", diag));
            }
        }
        if !result.diagnostic.is_empty() {
            out.push(format!("    diagnostic: {}", result.diagnostic));
        }
    }
    out
}

/// Phase 5.C.4 commit 39: opt-in runtime executability probe.
///
/// Runs the binary at `path` with the supplied `args` and a
/// short timeout, classifies the outcome:
///
///   - exit 0 → [`RuntimeStatus::Executes`].
///   - non-zero exit OR exec failure → [`RuntimeStatus::FailsToExecute`]
///     with diagnostic carrying combined stdout+stderr (trimmed).
///
/// The function detects the ld.so error patterns from commit 31b
/// (cannot-open-shared-object, while-loading-shared-libraries,
/// libprotoc.so) so the diagnostic explicitly names ABI failures
/// for downstream consumers (eligibility predicate's MissingTool
/// reason, future preflight tool-readiness section).
///
/// Pro's "no aggressive probing yet" gate remains in force: this
/// function is opt-in. Callers in commit 39 are tests + future
/// `--analyze-only --probe-tools` paths. The default
/// orchestrator flow doesn't run it.
///
/// Args defaults to `["--version"]` when caller passes empty.
/// `--version` is the cheapest CLI most codegen tools support
/// (protoc, grpc_cpp_plugin via protoc-as-host, mlir-tblgen,
/// llvm-tblgen all accept it; clang/g++ do too). When a tool
/// doesn't, callers can pass an alternative.
pub fn probe_runtime_status(path: &Path, args: &[&str]) -> RuntimeStatus {
    let probe_args: Vec<&str> = if args.is_empty() {
        vec!["--version"]
    } else {
        args.to_vec()
    };
    let output = match std::process::Command::new(path)
        .args(&probe_args)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            return RuntimeStatus::FailsToExecute(format!("spawn failed: {}", e));
        }
    };
    if output.status.success() {
        return RuntimeStatus::Executes;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{}{}", stderr.trim_end(), stdout.trim_end());
    let looks_like_ldso = stderr.contains("cannot open shared object file")
        || stderr.contains("while loading shared libraries")
        || stderr.contains("libprotoc.so")
        || stdout.contains("cannot open shared object file")
        || stdout.contains("while loading shared libraries");
    let diag = if looks_like_ldso {
        format!("ld.so error: {}", combined.trim())
    } else {
        format!("exit {}: {}", output.status, combined.trim())
    };
    RuntimeStatus::FailsToExecute(diag)
}

/// Disk-memoized variant of [`probe_runtime_status`].
///
/// The runtime status of a tool (executes cleanly vs. ABI/ld.so
/// failure) is invariant for a fixed binary content + probe args.
/// On a warm no-op rebuild, re-running `<binary> --version` for
/// every well-known tool (protoc, g++, gcc, ar, ld, bazel, …) is
/// pure overhead — the auditor traced one `bazel --version` exec
/// (bazelisk → real bazel) plus seven more on every TF no-op. The
/// bazel one is the most expensive (bazelisk dispatch). This
/// memoizes the result in `cache_dir`, keyed on the binary's
/// path + mtime + size + the exact probe args, so the subprocess
/// only re-runs when the binary actually changes (upgrade,
/// rebuild, replaced symlink) — which is exactly when the runtime
/// status could differ. A cold cache (first run) probes for real,
/// so clean builds stay correct.
///
/// The cache is best-effort: any I/O error (unreadable dir,
/// malformed entry, stat failure) falls back to a live probe and,
/// where possible, refreshes the cache. It never returns a stale
/// answer for a changed binary because the key embeds mtime+size.
pub fn probe_runtime_status_cached(
    path: &Path,
    args: &[&str],
    cache_dir: &Path,
) -> RuntimeStatus {
    // Stat the binary for the freshness key. If we can't stat it,
    // there's nothing to cache against — probe live.
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return probe_runtime_status(path, args),
    };
    let size = meta.len();
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe_args: Vec<&str> = if args.is_empty() {
        vec!["--version"]
    } else {
        args.to_vec()
    };
    let args_joined = probe_args.join(" ");

    // The cache file name is derived from a stable hash of the
    // resolved path so different tools / paths don't collide.
    let key_hash = stable_path_hash(path);
    let cache_file = cache_dir.join(format!("toolprobe-{:016x}.txt", key_hash));

    // Cache line format (single line, tab-separated):
    //   <mtime_ns> <size>\t<args_joined>\t<status_tag>\t<diag>
    // status_tag: "ok" | "fail". diag is empty for ok.
    if let Ok(contents) = std::fs::read_to_string(&cache_file) {
        if let Some(status) =
            parse_cached_probe(&contents, mtime_ns, size, &args_joined)
        {
            return status;
        }
    }

    // Miss / stale / malformed: probe live and refresh the cache.
    let status = probe_runtime_status(path, args);
    let (tag, diag) = match &status {
        RuntimeStatus::Executes => ("ok", String::new()),
        RuntimeStatus::FailsToExecute(d) => ("fail", d.replace('\n', " ")),
        // Unknown is never produced by probe_runtime_status, but
        // be exhaustive; don't cache an Unknown.
        RuntimeStatus::Unknown => return status,
    };
    // Best-effort write; ignore failures (read-only cache dir etc.).
    let _ = std::fs::create_dir_all(cache_dir);
    let line = format!(
        "{} {}\t{}\t{}\t{}\n",
        mtime_ns, size, args_joined, tag, diag
    );
    let _ = std::fs::write(&cache_file, line);
    status
}

/// Parse a cached probe line, returning the status iff the
/// freshness key (mtime+size+args) matches. Returns None on any
/// mismatch or malformed content so the caller re-probes.
fn parse_cached_probe(
    contents: &str,
    mtime_ns: u128,
    size: u64,
    args_joined: &str,
) -> Option<RuntimeStatus> {
    let line = contents.lines().next()?;
    let mut parts = line.splitn(4, '\t');
    let key = parts.next()?; // "<mtime_ns> <size>"
    let cached_args = parts.next()?;
    let tag = parts.next()?;
    let diag = parts.next().unwrap_or("");
    let mut key_parts = key.split_whitespace();
    let cached_mtime: u128 = key_parts.next()?.parse().ok()?;
    let cached_size: u64 = key_parts.next()?.parse().ok()?;
    if cached_mtime != mtime_ns || cached_size != size || cached_args != args_joined {
        return None;
    }
    match tag {
        "ok" => Some(RuntimeStatus::Executes),
        "fail" => Some(RuntimeStatus::FailsToExecute(diag.to_string())),
        _ => None,
    }
}

/// FNV-1a hash of a path's bytes — small, dependency-free, and
/// stable across runs (unlike DefaultHasher, whose seed varies).
/// Used only to derive a per-binary cache file name; collisions
/// are harmless because the cache line re-validates the full key.
fn stable_path_hash(path: &Path) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_id_name_round_trip() {
        // Spot-check a few; the full enum is small enough.
        assert_eq!(ToolId::Protoc.name(), "protoc");
        assert_eq!(ToolId::GrpcCppPlugin.name(), "grpc_cpp_plugin");
        assert_eq!(ToolId::MlirTblgen.name(), "mlir-tblgen");
        assert_eq!(ToolId::Cxx.name(), "cxx");
    }

    #[test]
    fn cached_probe_round_trips_and_invalidates_on_change() {
        // Use a real executable that supports --version.
        let sh = if Path::new("/bin/sh").exists() {
            PathBuf::from("/bin/sh")
        } else {
            PathBuf::from("/usr/bin/true")
        };
        // /bin/sh doesn't accept --version cleanly; use `true`
        // which exits 0 regardless of args.
        let tool = if Path::new("/usr/bin/true").exists() {
            PathBuf::from("/usr/bin/true")
        } else if Path::new("/bin/true").exists() {
            PathBuf::from("/bin/true")
        } else {
            sh
        };
        let dir = std::env::temp_dir().join(format!(
            "rush-toolprobe-test-{}-{}",
            std::process::id(),
            "a"
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // Cold: probes live, writes cache.
        let s1 = probe_runtime_status_cached(&tool, &["--version"], &dir);
        assert_eq!(s1, RuntimeStatus::Executes);

        // Warm: cache hit returns same status; the cache file
        // exists and validates the freshness key.
        let s2 = probe_runtime_status_cached(&tool, &["--version"], &dir);
        assert_eq!(s2, RuntimeStatus::Executes);
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "exactly one cache file written");

        // Different args → different freshness key → cache miss,
        // re-probe (still Executes for `true`), new file written.
        let _ = probe_runtime_status_cached(&tool, &["--help"], &dir);
        let entries2 = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(entries2, 1, "same tool path → same cache file, args validated within");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_cached_probe_rejects_stale_key() {
        // mtime/size mismatch → None (forces re-probe).
        let line = "100 200\t--version\tok\t";
        assert_eq!(parse_cached_probe(line, 100, 200, "--version"),
                   Some(RuntimeStatus::Executes));
        assert_eq!(parse_cached_probe(line, 999, 200, "--version"), None);
        assert_eq!(parse_cached_probe(line, 100, 999, "--version"), None);
        assert_eq!(parse_cached_probe(line, 100, 200, "--help"), None);
        // fail tag round-trips its diagnostic.
        let failline = "1 2\t--version\tfail\tld.so error: boom";
        assert_eq!(
            parse_cached_probe(failline, 1, 2, "--version"),
            Some(RuntimeStatus::FailsToExecute("ld.so error: boom".to_string()))
        );
        // malformed → None.
        assert_eq!(parse_cached_probe("garbage", 1, 2, "--version"), None);
    }

    #[test]
    fn probe_result_not_found_is_unusable() {
        let r = ProbeResult::not_found("nothing on PATH".to_string());
        assert!(!r.is_usable());
        assert_eq!(r.path, None);
        assert_eq!(r.diagnostic, "nothing on PATH");
    }

    #[test]
    fn probe_result_found_with_exec_is_usable() {
        let r = ProbeResult::found(
            PathBuf::from("/usr/bin/protoc"),
            ProbeSource::FixedFallback,
            true,
            true,
        );
        assert!(r.is_usable());
    }

    #[test]
    fn probe_result_found_but_not_executable_is_unusable() {
        let r = ProbeResult::found(
            PathBuf::from("/usr/bin/protoc"),
            ProbeSource::FixedFallback,
            true,
            false,
        );
        assert!(!r.is_usable());
    }

    #[test]
    fn probe_result_runtime_failure_is_unusable() {
        let mut r = ProbeResult::found(
            PathBuf::from("/some/grpc_cpp_plugin"),
            ProbeSource::EnvOverride("RUSH_GRPC_CPP_PLUGIN"),
            true,
            true,
        );
        r.runtime_status = RuntimeStatus::FailsToExecute(
            "libprotoc.so.31.1.0: cannot open shared object file".to_string(),
        );
        // Cheap probes passed but runtime failed — not usable.
        assert!(!r.is_usable());
    }

    #[test]
    fn probe_result_runtime_unknown_is_usable_when_cheap_probes_pass() {
        let r = ProbeResult::found(
            PathBuf::from("/usr/bin/ar"),
            ProbeSource::FixedFallback,
            true,
            true,
        );
        assert_eq!(r.runtime_status, RuntimeStatus::Unknown);
        assert!(r.is_usable());
    }

    #[test]
    fn cache_get_returns_none_for_unprobed_tool() {
        let cache = ToolProbeCache::new();
        assert!(cache.get(ToolId::Protoc).is_none());
    }

    #[test]
    fn cache_insert_and_get_round_trip() {
        let mut cache = ToolProbeCache::new();
        let r = ProbeResult::found(
            PathBuf::from("/usr/bin/protoc"),
            ProbeSource::FixedFallback,
            true,
            true,
        );
        cache.insert(ToolId::Protoc, r.clone());
        let got = cache.get(ToolId::Protoc).expect("should be in cache");
        assert_eq!(got.path, r.path);
    }

    #[test]
    fn cache_usability_summary_counts_correctly() {
        let mut cache = ToolProbeCache::new();
        cache.insert(
            ToolId::Protoc,
            ProbeResult::found(
                PathBuf::from("/usr/bin/protoc"),
                ProbeSource::FixedFallback,
                true,
                true,
            ),
        );
        cache.insert(
            ToolId::GrpcCppPlugin,
            ProbeResult::not_found("not on PATH".to_string()),
        );
        let mut grpc_failing = ProbeResult::found(
            PathBuf::from("/path/grpc_cpp_plugin"),
            ProbeSource::EnvOverride("RUSH_GRPC_CPP_PLUGIN"),
            true,
            true,
        );
        grpc_failing.runtime_status =
            RuntimeStatus::FailsToExecute("ABI mismatch".to_string());
        // Overwrite the not_found entry — last-write wins.
        cache.insert(ToolId::GrpcCppPlugin, grpc_failing);
        let (usable, broken) = cache.usability_summary();
        assert_eq!(usable, 1, "protoc usable");
        assert_eq!(broken, 1, "grpc_cpp_plugin runtime-fail");
    }

    #[test]
    fn format_probe_cache_includes_tool_names_and_paths() {
        let mut cache = ToolProbeCache::new();
        cache.insert(
            ToolId::Protoc,
            ProbeResult::found(
                PathBuf::from("/usr/bin/protoc"),
                ProbeSource::FixedFallback,
                true,
                true,
            ),
        );
        cache.insert(
            ToolId::GrpcCppPlugin,
            ProbeResult::not_found("nothing found".to_string()),
        );
        let dump = format_probe_cache(&cache).join("\n");
        assert!(dump.contains("protoc"));
        assert!(dump.contains("/usr/bin/protoc"));
        assert!(dump.contains("grpc_cpp_plugin"));
        assert!(dump.contains("<not found>"));
        assert!(dump.contains("FixedFallback"));
    }

    #[test]
    fn probe_source_env_override_carries_var_name() {
        // Future commit's diagnostic surfaces will rely on
        // this — when the env-override path fails, the
        // message can name the env var so the user knows
        // what to fix.
        let r = ProbeResult::found(
            PathBuf::from("/x/grpc_cpp_plugin"),
            ProbeSource::EnvOverride("RUSH_GRPC_CPP_PLUGIN"),
            true,
            true,
        );
        match r.source.as_ref().unwrap() {
            ProbeSource::EnvOverride(var) => {
                assert_eq!(*var, "RUSH_GRPC_CPP_PLUGIN");
            }
            other => panic!("expected EnvOverride, got {:?}", other),
        }
    }

    // Phase 5.C.4 commit 39: fake codegen tool tests.
    //
    // Pro's pivot directive #9: "Add a fake_grpc_cpp_plugin /
    // fake_mlir_tblgen test fixture. CI hosts won't have real
    // grpc / mlir; the runtime probe path needs to be
    // exercisable with synthetic binaries that mimic the
    // tool's exit-code shape. This protects the probe code
    // against regressions and lets future grpc-shape
    // migration land tests without bringing in real toolchains."
    //
    // The helper writes a shell-script binary on disk with
    // controllable exit behavior. Each test creates its own
    // fake (PID-suffixed path → no cross-test interference)
    // and removes it on completion (best-effort; /tmp gets
    // swept anyway).

    /// Phase 5.C.4 commit 63: write a temp shell-script that
    /// emulates `grpc_cpp_plugin`'s file-emission contract.
    ///
    /// Pro's directive: "Add fake grpc plugin test shape:
    /// fake_grpc_cpp_plugin: input .proto / emits .grpc.pb.h
    /// + .grpc.pb.cc. Goal: scheduler/provider/action
    /// correctness independent of host grpc install."
    ///
    /// Behavior:
    ///   - Parses `--grpc_out=DIR` from argv.
    ///   - Treats the last `*.proto` argument as the input.
    ///   - Writes minimal stub `.grpc.pb.h` and `.grpc.pb.cc`
    ///     into DIR.
    ///   - Exits 0 on success; non-zero with stderr message
    ///     on missing args.
    ///
    /// Used by integration tests that need to exercise
    /// downstream-of-grpc build paths without depending on
    /// a real grpc_cpp_plugin install. The host's real
    /// `protoc` may be absent or ABI-mismatched — the fake
    /// is hermetic (just writes files; no link to libprotoc).
    fn fake_grpc_cpp_plugin(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "rush-fake-grpc-cpp-plugin-{}-{}",
            std::process::id(),
            suffix
        ));
        let script = r#"#!/bin/sh
out_dir=""
proto_file=""
for arg in "$@"; do
  case "$arg" in
    --grpc_out=*)
      out_dir="${arg#--grpc_out=}"
      ;;
    --version)
      echo "fake_grpc_cpp_plugin 0.0.1 (rush test fixture)"
      exit 0
      ;;
    *.proto)
      proto_file="$arg"
      ;;
  esac
done
if [ -z "$out_dir" ] || [ -z "$proto_file" ]; then
  echo "fake_grpc_cpp_plugin: missing --grpc_out= or .proto input" >&2
  exit 1
fi
base=$(basename "$proto_file" .proto)
mkdir -p "$out_dir"
cat > "$out_dir/$base.grpc.pb.h" << 'GRPCH'
// Phase 5.C.4 commit 63: fake grpc generated header.
// Emitted by tests/fake_grpc_cpp_plugin to exercise
// scheduler/provider/action correctness independent of
// real grpc_cpp_plugin install.
#ifndef FAKE_GRPC_GUARD_INCLUDED
#define FAKE_GRPC_GUARD_INCLUDED
namespace fake_grpc {}
#endif
GRPCH
cat > "$out_dir/$base.grpc.pb.cc" << 'GRPCC'
// Phase 5.C.4 commit 63: fake grpc generated source.
namespace fake_grpc {}
GRPCC
exit 0
"#;
        std::fs::write(&path, script).expect("write fake grpc plugin");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    /// Phase 5.C.4 commit 63: write a temp shell-script that
    /// emulates `mlir-tblgen`'s `.h.inc` emission contract.
    ///
    /// Companion to `fake_grpc_cpp_plugin`. Pro: "If feasible:
    /// add fake mlir-tblgen too." Behavior:
    ///   - Parses `-o OUT_FILE` from argv.
    ///   - Treats the last `*.td` argument as the input.
    ///   - Writes a minimal `.h.inc` to OUT_FILE.
    ///   - Exits 0 on success.
    fn fake_mlir_tblgen(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "rush-fake-mlir-tblgen-{}-{}",
            std::process::id(),
            suffix
        ));
        let script = r#"#!/bin/sh
out_file=""
td_file=""
prev=""
for arg in "$@"; do
  case "$prev" in
    -o)
      out_file="$arg"
      ;;
  esac
  case "$arg" in
    --version)
      echo "fake_mlir_tblgen 0.0.1 (rush test fixture)"
      exit 0
      ;;
    *.td)
      td_file="$arg"
      ;;
  esac
  prev="$arg"
done
if [ -z "$out_file" ] || [ -z "$td_file" ]; then
  echo "fake_mlir_tblgen: missing -o OUT_FILE or .td input" >&2
  exit 1
fi
mkdir -p "$(dirname "$out_file")"
cat > "$out_file" << 'TDOUT'
// Phase 5.C.4 commit 63: fake mlir-tblgen generated h.inc.
// Emitted by tests/fake_mlir_tblgen.
#ifndef FAKE_MLIR_TBLGEN_GUARD
#define FAKE_MLIR_TBLGEN_GUARD
#endif
TDOUT
exit 0
"#;
        std::fs::write(&path, script).expect("write fake mlir-tblgen");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    /// Write a temp shell-script that exits with `exit_code`
    /// after optionally writing `stderr_msg` to stderr. The
    /// resulting file is mode 0o755 so file_is_executable +
    /// is_usable() pass.
    fn fake_codegen_tool(name: &str, exit_code: u8, stderr_msg: &str) -> PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "rush-fake-{}-{}-{:p}",
            name,
            std::process::id(),
            stderr_msg as *const str,
        ));
        let script = if stderr_msg.is_empty() {
            format!("#!/bin/sh\nexit {}\n", exit_code)
        } else {
            // Single-quote stderr_msg by escaping any embedded
            // single quotes; tests use plain strings anyway.
            let escaped = stderr_msg.replace('\'', "'\\''");
            format!(
                "#!/bin/sh\necho '{}' >&2\nexit {}\n",
                escaped, exit_code
            )
        };
        std::fs::write(&path, script).expect("write fake tool");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    #[test]
    fn runtime_probe_reports_executes_for_fake_grpc_cpp_plugin_that_exits_zero() {
        // Synthetic: fake_grpc_cpp_plugin behaves like the
        // real tool when called with --version (exits 0). The
        // probe should classify as Executes regardless of
        // whether stdout is empty.
        let fake = fake_codegen_tool("grpc_cpp_plugin", 0, "");
        let status = probe_runtime_status(&fake, &["--version"]);
        assert_eq!(status, RuntimeStatus::Executes);
        let _ = std::fs::remove_file(&fake);
    }

    #[test]
    fn runtime_probe_reports_executes_for_fake_mlir_tblgen_with_default_args() {
        // Default args (--version) when caller passes empty
        // slice. Fake mlir-tblgen exits 0; status = Executes.
        let fake = fake_codegen_tool("mlir-tblgen", 0, "");
        let status = probe_runtime_status(&fake, &[]);
        assert_eq!(status, RuntimeStatus::Executes);
        let _ = std::fs::remove_file(&fake);
    }

    #[test]
    fn runtime_probe_detects_ldso_failure_via_cannot_open_shared_object() {
        // Mimics the real-world rattler grpc_cpp_plugin ABI
        // mismatch from commit 28's discovery: the binary
        // exists, mode 0o755, but ld.so can't resolve its
        // libprotoc.so dependency. Our probe must surface this
        // as FailsToExecute with the ld.so prefix in the
        // diagnostic so downstream consumers (commit 31b's
        // grpc_plugin_runtime_failure_diagnostic, future
        // MissingTool eligibility wire-up) can distinguish it
        // from generic protoc failures.
        let fake = fake_codegen_tool(
            "grpc_cpp_plugin_ldso_fail",
            127,
            "/usr/lib/grpc_cpp_plugin: error while loading shared libraries: \
             libprotoc.so.31.1.0: cannot open shared object file: No such file \
             or directory",
        );
        let status = probe_runtime_status(&fake, &["--version"]);
        match status {
            RuntimeStatus::FailsToExecute(diag) => {
                assert!(
                    diag.starts_with("ld.so error:"),
                    "expected ld.so prefix, got {}",
                    diag
                );
                assert!(diag.contains("cannot open shared object file"));
                assert!(diag.contains("libprotoc.so.31.1.0"));
            }
            other => panic!("expected FailsToExecute, got {:?}", other),
        }
        let _ = std::fs::remove_file(&fake);
    }

    #[test]
    fn runtime_probe_reports_generic_failure_for_non_ldso_error() {
        // A fake that exits with a domain error (e.g. "unknown
        // flag") gets the generic 'exit <status>:' prefix —
        // not the ld.so prefix. This distinguishes the two
        // classes for downstream consumers.
        let fake = fake_codegen_tool(
            "grpc_cpp_plugin_bad_flag",
            1,
            "Unknown command line flag '--made-up-flag'",
        );
        let status = probe_runtime_status(&fake, &["--made-up-flag"]);
        match status {
            RuntimeStatus::FailsToExecute(diag) => {
                assert!(
                    diag.starts_with("exit "),
                    "expected exit-code prefix, got {}",
                    diag
                );
                assert!(!diag.contains("ld.so error:"));
                assert!(diag.contains("Unknown command line flag"));
            }
            other => panic!("expected FailsToExecute, got {:?}", other),
        }
        let _ = std::fs::remove_file(&fake);
    }

    #[test]
    fn runtime_probe_reports_spawn_failure_for_missing_path() {
        // Calling probe_runtime_status on a non-existent path
        // should return FailsToExecute with "spawn failed:" —
        // the spawn() syscall fails before we get to status
        // classification.
        let path = std::env::temp_dir().join(format!(
            "rush-fake-definitely-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let status = probe_runtime_status(&path, &["--version"]);
        match status {
            RuntimeStatus::FailsToExecute(diag) => {
                assert!(
                    diag.contains("spawn failed:"),
                    "expected spawn-failed diagnostic, got {}",
                    diag
                );
            }
            other => panic!("expected FailsToExecute, got {:?}", other),
        }
    }

    #[test]
    fn runtime_probe_then_cache_insert_marks_tool_unusable_on_ldso_fail() {
        // End-to-end: fake plugin → probe → cache.insert →
        // is_usable() == false. This pins the contract that
        // the future MissingTool eligibility wire-up depends
        // on: a tool whose runtime probe fails must NOT pass
        // is_usable().
        let fake = fake_codegen_tool(
            "fake_grpc_for_cache_test",
            127,
            "while loading shared libraries: libprotoc.so.31.1.0: cannot \
             open shared object file: No such file or directory",
        );
        let runtime_status = probe_runtime_status(&fake, &["--version"]);
        let mut result = ProbeResult::found(
            fake.clone(),
            ProbeSource::EnvOverride("RUSH_GRPC_CPP_PLUGIN"),
            true,
            true,
        );
        result.runtime_status = runtime_status;
        let mut cache = ToolProbeCache::new();
        cache.insert(ToolId::GrpcCppPlugin, result);
        let cached = cache.get(ToolId::GrpcCppPlugin).expect("cached");
        assert!(
            !cached.is_usable(),
            "ld.so failure must mark plugin unusable; got runtime_status={:?}",
            cached.runtime_status
        );
        let (usable, broken) = cache.usability_summary();
        assert_eq!((usable, broken), (0, 1));
        let _ = std::fs::remove_file(&fake);
    }

    #[test]
    fn runtime_probe_then_cache_insert_marks_tool_usable_on_clean_exec() {
        // End-to-end positive: fake exits 0 → Executes → cache
        // marks usable. Pins the happy-path contract.
        let fake = fake_codegen_tool("fake_grpc_for_clean_test", 0, "");
        let runtime_status = probe_runtime_status(&fake, &["--version"]);
        assert_eq!(runtime_status, RuntimeStatus::Executes);
        let mut result = ProbeResult::found(
            fake.clone(),
            ProbeSource::EnvOverride("RUSH_GRPC_CPP_PLUGIN"),
            true,
            true,
        );
        result.runtime_status = runtime_status;
        let mut cache = ToolProbeCache::new();
        cache.insert(ToolId::GrpcCppPlugin, result);
        let cached = cache.get(ToolId::GrpcCppPlugin).expect("cached");
        assert!(cached.is_usable());
        let (usable, broken) = cache.usability_summary();
        assert_eq!((usable, broken), (1, 0));
        let _ = std::fs::remove_file(&fake);
    }

    // Phase 5.C.4 commit 63: fake plugin emission tests.
    // Pro's directive: "Add fake grpc plugin test shape:
    // input .proto / emits .grpc.pb.h + .grpc.pb.cc.
    // If feasible: add fake mlir-tblgen too. Goal:
    // scheduler/provider/action correctness independent
    // of host grpc install."

    #[test]
    fn fake_grpc_cpp_plugin_emits_grpc_headers_and_sources() {
        let fake = fake_grpc_cpp_plugin("emit_test");
        let out_dir = std::env::temp_dir().join(format!(
            "rush-fake-grpc-out-{}-emit",
            std::process::id()
        ));
        let proto_input = std::env::temp_dir().join(format!(
            "rush-fake-grpc-input-{}-emit.proto",
            std::process::id()
        ));
        std::fs::write(&proto_input, "syntax = \"proto3\";").unwrap();
        let _ = std::fs::remove_dir_all(&out_dir);

        // Invoke the fake with grpc-style args.
        let output = std::process::Command::new(&fake)
            .arg(format!("--grpc_out={}", out_dir.display()))
            .arg(&proto_input)
            .output()
            .expect("invoke fake plugin");
        assert!(
            output.status.success(),
            "fake plugin failed: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify the expected output files exist with stub
        // content. The base name comes from the .proto
        // filename minus the .proto extension.
        let proto_basename = proto_input.file_stem().unwrap().to_string_lossy();
        let h_path = out_dir.join(format!("{}.grpc.pb.h", proto_basename));
        let cc_path = out_dir.join(format!("{}.grpc.pb.cc", proto_basename));
        assert!(h_path.is_file(), "expected {}", h_path.display());
        assert!(cc_path.is_file(), "expected {}", cc_path.display());
        let h_content = std::fs::read_to_string(&h_path).unwrap();
        let cc_content = std::fs::read_to_string(&cc_path).unwrap();
        assert!(h_content.contains("FAKE_GRPC_GUARD_INCLUDED"));
        assert!(cc_content.contains("namespace fake_grpc"));

        let _ = std::fs::remove_file(&fake);
        let _ = std::fs::remove_file(&proto_input);
        let _ = std::fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn fake_grpc_cpp_plugin_version_probe_executes() {
        // The fake supports --version per the runtime probe
        // contract (commit 39's probe_runtime_status). When
        // future commits route grpc through ToolProbeCache,
        // the cache's runtime probe step will succeed against
        // the fake.
        let fake = fake_grpc_cpp_plugin("version_probe");
        let status = probe_runtime_status(&fake, &["--version"]);
        assert_eq!(status, RuntimeStatus::Executes);
        let _ = std::fs::remove_file(&fake);
    }

    #[test]
    fn fake_grpc_cpp_plugin_fails_on_missing_args() {
        // Conservative: invoke without --grpc_out= → exit 1
        // with stderr error. The runtime probe (which uses
        // --version) must not regress.
        let fake = fake_grpc_cpp_plugin("missing_args");
        let output = std::process::Command::new(&fake)
            .output()
            .expect("invoke fake plugin");
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("missing --grpc_out"),
            "stderr: {}",
            stderr
        );
        let _ = std::fs::remove_file(&fake);
    }

    #[test]
    fn fake_mlir_tblgen_emits_h_inc_to_dash_o_path() {
        let fake = fake_mlir_tblgen("emit_test");
        let out_file = std::env::temp_dir().join(format!(
            "rush-fake-tblgen-out-{}-emit.h.inc",
            std::process::id()
        ));
        let td_input = std::env::temp_dir().join(format!(
            "rush-fake-tblgen-input-{}-emit.td",
            std::process::id()
        ));
        std::fs::write(&td_input, "// fake .td").unwrap();
        let _ = std::fs::remove_file(&out_file);

        let output = std::process::Command::new(&fake)
            .arg("-o")
            .arg(&out_file)
            .arg(&td_input)
            .output()
            .expect("invoke fake mlir-tblgen");
        assert!(
            output.status.success(),
            "fake mlir-tblgen failed: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(out_file.is_file());
        let content = std::fs::read_to_string(&out_file).unwrap();
        assert!(content.contains("FAKE_MLIR_TBLGEN_GUARD"));

        let _ = std::fs::remove_file(&fake);
        let _ = std::fs::remove_file(&td_input);
        let _ = std::fs::remove_file(&out_file);
    }

    #[test]
    fn fake_mlir_tblgen_version_probe_executes() {
        let fake = fake_mlir_tblgen("version_probe");
        let status = probe_runtime_status(&fake, &["--version"]);
        assert_eq!(status, RuntimeStatus::Executes);
        let _ = std::fs::remove_file(&fake);
    }

    // =====================================================
    // Phase 5.C.4 commit 190 (Batch Y): ToolAuditStatus
    // classifier tests. Pure-function tests over ProbeResult
    // shapes — no real binaries, no PATH lookup.
    // =====================================================

    #[test]
    fn audit_status_classify_not_found_is_missing() {
        let r = ProbeResult::not_found("nothing".to_string());
        assert_eq!(
            ToolAuditStatus::classify(&r),
            ToolAuditStatus::Missing
        );
    }

    #[test]
    fn audit_status_classify_found_executable_unknown_is_available() {
        // Cheap probes pass; runtime not run. The conservative
        // "presumed available" classification — matches
        // probe.is_usable() semantics.
        let r = ProbeResult::found(
            PathBuf::from("/usr/bin/protoc"),
            ProbeSource::Path,
            true,
            true,
        );
        assert_eq!(
            ToolAuditStatus::classify(&r),
            ToolAuditStatus::Available
        );
    }

    #[test]
    fn audit_status_classify_found_executable_executes_is_available() {
        let mut r = ProbeResult::found(
            PathBuf::from("/usr/bin/protoc"),
            ProbeSource::Path,
            true,
            true,
        );
        r.runtime_status = RuntimeStatus::Executes;
        assert_eq!(
            ToolAuditStatus::classify(&r),
            ToolAuditStatus::Available
        );
    }

    #[test]
    fn audit_status_classify_ldso_failure_is_abi_broken() {
        // commit 39's probe_runtime_status formats the ld.so
        // case with the "ld.so error:" prefix. The audit
        // classifier matches on that prefix.
        let mut r = ProbeResult::found(
            PathBuf::from("/usr/bin/grpc_cpp_plugin"),
            ProbeSource::Path,
            true,
            true,
        );
        r.runtime_status = RuntimeStatus::FailsToExecute(
            "ld.so error: libprotoc.so.30: cannot open shared object"
                .to_string(),
        );
        assert_eq!(
            ToolAuditStatus::classify(&r),
            ToolAuditStatus::AbiBroken
        );
    }

    #[test]
    fn audit_status_classify_non_ldso_runtime_failure_is_abi_broken() {
        // Non-ld.so runtime failure (e.g. SIGSEGV) also
        // classifies as AbiBroken — the binary doesn't run
        // for whatever reason. Operators who need finer
        // diagnostics read the underlying ProbeResult.
        let mut r = ProbeResult::found(
            PathBuf::from("/usr/bin/protoc"),
            ProbeSource::Path,
            true,
            true,
        );
        r.runtime_status = RuntimeStatus::FailsToExecute(
            "exit 134: SIGABRT".to_string(),
        );
        assert_eq!(
            ToolAuditStatus::classify(&r),
            ToolAuditStatus::AbiBroken
        );
    }

    #[test]
    fn audit_status_classify_not_executable_is_missing() {
        // File present but executable bit clear.
        let mut r = ProbeResult::found(
            PathBuf::from("/tmp/notexec"),
            ProbeSource::Path,
            true,
            false, // not executable
        );
        r.runtime_status = RuntimeStatus::Unknown;
        assert_eq!(
            ToolAuditStatus::classify(&r),
            ToolAuditStatus::Missing
        );
    }

    #[test]
    fn audit_status_str_labels_are_stable() {
        // Wire-format consumers rely on these.
        assert_eq!(ToolAuditStatus::Available.as_str(), "available");
        assert_eq!(ToolAuditStatus::Missing.as_str(), "missing");
        assert_eq!(ToolAuditStatus::AbiBroken.as_str(), "abi_broken");
        assert_eq!(
            ToolAuditStatus::WrongVersion.as_str(),
            "wrong_version"
        );
        assert_eq!(ToolAuditStatus::Unknown.as_str(), "unknown");
    }

    #[test]
    fn tool_id_all_covers_every_variant() {
        // Compile-time exhaustive: every ToolId is in all().
        let known = ToolId::all();
        // The 9 tools the audit pass covers as of commit 190.
        assert_eq!(known.len(), 9);
        assert!(known.contains(&ToolId::Protoc));
        assert!(known.contains(&ToolId::GrpcCppPlugin));
        assert!(known.contains(&ToolId::MlirTblgen));
        assert!(known.contains(&ToolId::LlvmTblgen));
        assert!(known.contains(&ToolId::Cxx));
        assert!(known.contains(&ToolId::Cc));
        assert!(known.contains(&ToolId::Ar));
        assert!(known.contains(&ToolId::Ld));
        assert!(known.contains(&ToolId::Bazel));
    }

    #[test]
    fn tool_id_env_override_well_known_names() {
        assert_eq!(
            ToolId::GrpcCppPlugin.env_override(),
            Some("RUSH_GRPC_CPP_PLUGIN")
        );
        assert_eq!(ToolId::Bazel.env_override(), Some("RUSH_BAZEL_PATH"));
        assert_eq!(ToolId::Cxx.env_override(), Some("CXX"));
        assert_eq!(ToolId::Cc.env_override(), Some("CC"));
        assert_eq!(ToolId::Ar.env_override(), Some("AR"));
    }
}
