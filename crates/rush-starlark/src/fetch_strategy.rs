// Phase 5.C.4 commit 141 (Batch R): FetchStrategy
// enum + combined bazel-sync capability probe +
// combined executor.
//
// Commit 140 landed the FetchPlan data model
// (what each repo's classification is). This commit
// lands the EXECUTION side — given a list of
// "needs_fetch" repos, can we batch them into one
// bazel invocation?
//
// `bazel help sync` (bazel 6.x) lists the
// `--only` flag as "a string; may be used multiple
// times" — confirming that `bazel sync
// --only=A --only=B --only=C` runs one bazel
// startup against N repos. That's the combined
// strategy.
//
// This commit does NOT wire the strategy into the
// orchestrator's analyze path yet. The executor is
// a standalone function exercised by unit tests
// with a fake bazel binary. Commit 142 will wire
// the dispatcher and commit 143 measures all 5
// configurations.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

/// Phase 5.C.4 commit 141 (Batch R): fetch
/// strategy selector. Controls how Batch R's fetch
/// orchestration dispatches a list of repos to
/// `bazel sync`.
///
/// `Auto` lets the implementation pick the
/// best strategy from the probe + repo count
/// (1 repo → Serial, ≥2 repos + capability →
/// Combined). For now `Auto` is the same as
/// `Serial` because commit 141 doesn't yet pick
/// `Combined` automatically; commit 144's decision
/// doc will recommend whether to promote `Auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchStrategy {
    /// One `bazel sync --only=<repo>` invocation per
    /// repo. Default. Matches pre-Batch-R behavior.
    Serial,
    /// One `bazel sync --only=A --only=B ...`
    /// invocation for the whole list. Requires
    /// bazel capability (probed via `bazel help
    /// sync`).
    Combined,
    /// Multiple concurrent serial invocations,
    /// bounded by `RUSH_FETCH_CONCURRENCY`. Commit
    /// 142.
    Parallel,
    /// Auto-select. Commit 141 maps this to Serial;
    /// commit 144 may promote it.
    Auto,
}

impl FetchStrategy {
    /// Parse `RUSH_FETCH_STRATEGY`. Accepts
    /// case-insensitive `serial` / `combined` /
    /// `parallel` / `auto`. Anything else → `Serial`
    /// (default), with a warning emitted by the
    /// caller if they care to validate.
    pub fn from_env_value(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "serial" => FetchStrategy::Serial,
            "combined" => FetchStrategy::Combined,
            "parallel" => FetchStrategy::Parallel,
            "auto" => FetchStrategy::Auto,
            _ => FetchStrategy::Serial,
        }
    }

    /// Read `RUSH_FETCH_STRATEGY` from the
    /// environment, defaulting to `Serial`.
    pub fn from_env() -> Self {
        std::env::var("RUSH_FETCH_STRATEGY")
            .ok()
            .map(|v| Self::from_env_value(&v))
            .unwrap_or(FetchStrategy::Serial)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            FetchStrategy::Serial => "serial",
            FetchStrategy::Combined => "combined",
            FetchStrategy::Parallel => "parallel",
            FetchStrategy::Auto => "auto",
        }
    }
}

/// Phase 5.C.4 commit 141 (Batch R): result of
/// probing bazel for combined-fetch support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CombinedCapability {
    /// bazel reports `--only (a string; may be used
    /// multiple times)` for `sync` — combined mode
    /// is supported.
    Supported,
    /// `bazel help sync` ran but the `--only`
    /// repeat-marker was not found.
    NotSupported,
    /// `bazel help sync` itself failed (binary
    /// missing, wrong version, etc). Treated as
    /// `NotSupported` for fallback purposes; the
    /// reason is preserved for diagnostics.
    ProbeFailed(String),
}

impl CombinedCapability {
    pub fn is_supported(&self) -> bool {
        matches!(self, CombinedCapability::Supported)
    }
}

/// Phase 5.C.4 commit 141 (Batch R): probe whether
/// `bazel sync --only=` accepts multiple repos in
/// one invocation.
///
/// Runs `<bazel_binary> help sync` and grep-checks
/// for the literal:
///
///   `--only (a string; may be used multiple times)`
///
/// This text is bazel's stable
/// "this flag is repeatable" notation. If the marker
/// is present the combined strategy is safe; if
/// absent we fall back.
///
/// The probe is run once per process and the result
/// can be cached by the caller. Callers must NOT
/// invoke this on every fetch.
pub fn probe_combined_capability(bazel_binary: &Path) -> CombinedCapability {
    let out = StdCommand::new(bazel_binary)
        .arg("help")
        .arg("sync")
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let combined = String::from_utf8_lossy(&o.stdout)
                .to_string()
                + &String::from_utf8_lossy(&o.stderr);
            if combined.contains(
                "--only (a string; may be used multiple times)",
            ) {
                CombinedCapability::Supported
            } else {
                CombinedCapability::NotSupported
            }
        }
        Ok(o) => CombinedCapability::ProbeFailed(format!(
            "bazel help sync exited {}",
            o.status
        )),
        Err(e) => CombinedCapability::ProbeFailed(e.to_string()),
    }
}

/// Phase 5.C.4 commit 141 (Batch R): run one
/// `bazel sync --only=A --only=B ...` for the given
/// repo list. Returns `Ok(())` on success.
///
/// `forced` controls fallback semantics when the
/// capability probe says NotSupported:
///   - `forced = true`:  try the combined command
///     anyway and bubble up bazel's error if it
///     rejects the args. Used when the operator
///     explicitly passes `RUSH_FETCH_STRATEGY=combined`
///     and wants to see what happens.
///   - `forced = false`: returns Err so the caller
///     can fall back to Serial cleanly.
pub fn execute_combined_fetch(
    bazel_binary: &Path,
    output_base: &Path,
    workspace_root: &Path,
    repos: &[&str],
    capability: &CombinedCapability,
    forced: bool,
) -> Result<()> {
    if repos.is_empty() {
        return Ok(());
    }

    if !capability.is_supported() && !forced {
        return Err(anyhow!(
            "combined fetch unsupported (capability={:?}); \
             caller should fall back to Serial",
            capability,
        ));
    }

    let mut cmd = StdCommand::new(bazel_binary);
    cmd.arg("--output_base")
        .arg(output_base)
        .arg("sync");
    for r in repos {
        cmd.arg(format!("--only={}", r));
    }
    cmd.current_dir(workspace_root);

    let status = cmd.status().with_context(|| {
        format!(
            "Failed to launch combined `{} sync --only=...` for {} repos",
            bazel_binary.display(),
            repos.len()
        )
    })?;

    if !status.success() {
        return Err(anyhow!(
            "combined `{} sync` for {} repos failed with status {:?}: {}",
            bazel_binary.display(),
            repos.len(),
            status.code(),
            if forced {
                "forced combined mode — bazel rejected the args"
            } else {
                "bazel command reported failure"
            },
        ));
    }
    Ok(())
}

/// Phase 5.C.4 commit 141 (Batch R): for callers
/// that want a single typed dispatch entry point.
/// Given a chosen strategy + the probed capability,
/// returns the strategy that will actually run
/// (Serial fallback for Combined+NotSupported when
/// not forced).
pub fn resolve_runtime_strategy(
    requested: FetchStrategy,
    capability: &CombinedCapability,
    forced: bool,
) -> FetchStrategy {
    match requested {
        FetchStrategy::Combined if !capability.is_supported() && !forced => {
            eprintln!(
                "[fetch] combined strategy requested but unsupported \
                 ({:?}); falling back to serial",
                capability
            );
            FetchStrategy::Serial
        }
        // Auto maps to Combined when supported (commit
        // 144 may revisit), else Serial.
        FetchStrategy::Auto if capability.is_supported() => {
            FetchStrategy::Combined
        }
        FetchStrategy::Auto => FetchStrategy::Serial,
        other => other,
    }
}

// =============================================================
// Phase 5.C.4 commit 142 (Batch R): bounded parallel fetch.
// =============================================================

/// Default concurrency for parallel fetch when
/// `RUSH_FETCH_CONCURRENCY` is unset or invalid.
pub const DEFAULT_FETCH_CONCURRENCY: usize = 4;

/// Read `RUSH_FETCH_CONCURRENCY` from env, defaulting
/// to `DEFAULT_FETCH_CONCURRENCY`. Clamps to
/// `[1, 64]` — 0 is treated as 1 (run-but-serial),
/// and anything above 64 would just oversubscribe.
pub fn parse_concurrency_env() -> usize {
    std::env::var("RUSH_FETCH_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.clamp(1, 64))
        .unwrap_or(DEFAULT_FETCH_CONCURRENCY)
}

/// Phase 5.C.4 commit 142 (Batch R): per-repo
/// outcome of a parallel fetch dispatch.
#[derive(Debug)]
pub struct ParallelFetchResult {
    pub repo_name: String,
    pub outcome: Result<()>,
}

/// Phase 5.C.4 commit 142 (Batch R): run the input
/// repos through a bounded worker pool of size
/// `concurrency`. Each worker fetches one repo at a
/// time via `bazel sync --only=<repo>` — same
/// command as commit 141's Serial path, just
/// dispatched concurrently.
///
/// Guarantees per Pro's spec:
///   - one fetch per repo (input deduplicated by
///     name, preserving first-seen order)
///   - no two workers fetch the same repo
///     simultaneously (dedup happens BEFORE
///     dispatch; there's nothing to race on)
///   - bounded by `concurrency` (worker pool fed
///     by a shared mpsc channel — `concurrency`
///     workers wait on `recv()`, never more)
///   - results returned sorted by `repo_name` for
///     deterministic operator-visible reporting
///   - any per-repo failure is captured in
///     `ParallelFetchResult::outcome`; the dispatcher
///     does NOT short-circuit on first failure —
///     all repos are attempted, all errors reported
pub fn execute_parallel_fetch(
    bazel_binary: &Path,
    output_base: &Path,
    workspace_root: &Path,
    repos: &[&str],
    concurrency: usize,
) -> Vec<ParallelFetchResult> {
    // Dedup input preserving first-seen order.
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<String> = repos
        .iter()
        .filter_map(|r| {
            if seen.insert(r.to_string()) {
                Some(r.to_string())
            } else {
                None
            }
        })
        .collect();
    if unique.is_empty() {
        return Vec::new();
    }
    let concurrency = concurrency.max(1).min(unique.len());

    let (tx, rx) =
        std::sync::mpsc::channel::<String>();
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));

    // Each worker writes (repo_name, Result) into
    // results_tx; the main thread collects + sorts.
    let (results_tx, results_rx) = std::sync::mpsc::channel::<
        ParallelFetchResult,
    >();

    let bazel = bazel_binary.to_path_buf();
    let ob = output_base.to_path_buf();
    let ws = workspace_root.to_path_buf();

    let mut handles = Vec::new();
    for _ in 0..concurrency {
        let rx_clone = std::sync::Arc::clone(&rx);
        let tx_clone = results_tx.clone();
        let bazel = bazel.clone();
        let ob = ob.clone();
        let ws = ws.clone();
        let handle = std::thread::spawn(move || loop {
            // Pull the next repo. The lock is held
            // only across `recv()` — workers don't
            // serialize on bazel calls.
            let repo = {
                let guard = match rx_clone.lock() {
                    Ok(g) => g,
                    Err(_) => break, // poisoned → exit
                };
                match guard.recv() {
                    Ok(r) => r,
                    Err(_) => break, // channel closed → exit
                }
            };
            let outcome = run_one_serial_fetch(&bazel, &ob, &ws, &repo);
            let _ = tx_clone.send(ParallelFetchResult {
                repo_name: repo,
                outcome,
            });
        });
        handles.push(handle);
    }
    // Drop our copy of results_tx so the channel
    // closes once all workers exit.
    drop(results_tx);

    for r in &unique {
        if tx.send(r.clone()).is_err() {
            break;
        }
    }
    drop(tx);

    for h in handles {
        let _ = h.join();
    }

    let mut results: Vec<ParallelFetchResult> =
        results_rx.iter().collect();
    results.sort_by(|a, b| a.repo_name.cmp(&b.repo_name));
    results
}

/// Phase 5.C.4 commit 142 (Batch R): one serial
/// fetch, equivalent to the existing
/// `bzl_loader::fetch_repo_via_bazel` but extracted
/// here so the parallel executor can reuse it. The
/// command is bit-identical to the pre-Batch-R
/// shape.
fn run_one_serial_fetch(
    bazel_binary: &Path,
    output_base: &Path,
    workspace_root: &Path,
    repo: &str,
) -> Result<()> {
    let status = StdCommand::new(bazel_binary)
        .arg("--output_base")
        .arg(output_base)
        .arg("sync")
        .arg(format!("--only={}", repo))
        .current_dir(workspace_root)
        .status()
        .with_context(|| {
            format!(
                "Failed to launch `{} sync --only={}`",
                bazel_binary.display(),
                repo
            )
        })?;
    if !status.success() {
        return Err(anyhow!(
            "`{} sync --only={}` failed with status {:?}",
            bazel_binary.display(),
            repo,
            status.code(),
        ));
    }
    Ok(())
}

// =============================================================
// Phase 5.C.4 commit 143 (Batch R): orchestrator integration.
// =============================================================

/// Repos that have already been materialized by a
/// Batch R warmup pass (combined / parallel). The
/// per-repo `fetch_repo_via_bazel` path consults
/// this set and short-circuits — the repo is
/// already on disk under `bazel_output_base/external/`,
/// no per-repo invocation needed.
///
/// Populated by `perform_warmup_if_configured` at
/// the start of analyze, ONCE per process.
pub static WARMED_UP_REPOS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, PathBuf>>,
> = std::sync::OnceLock::new();

/// True iff `repo_name` has been pre-fetched via
/// the Batch R warmup pass. Used by the loader's
/// `fetch_repo_via_bazel` to skip duplicate work.
pub fn is_warmed_up(repo_name: &str) -> Option<PathBuf> {
    let lock = WARMED_UP_REPOS.get()?;
    let map = lock.lock().ok()?;
    map.get(repo_name).cloned()
}

// =============================================================
// Phase 5.C.4 commit 148 (Batch S): manifest-writer callback.
// =============================================================
//
// rush-starlark cannot depend on rush-build, but
// commit 148's manifest writer LIVES in rush-build
// (next to `cache_paths`). Same pattern as the
// commit-133 `GLOBAL_BZL_CACHE_FOR_FETCH_TIMING`
// bridge: rush-build registers a callback at
// orchestrator init; rush-starlark calls through
// the OnceLock when a fetch succeeds.
//
// Signature: (workspace_root, repo_name,
// external_path, bazel_command_shape) → outcome
// string (Written / Skipped / Failed). The string
// is the discriminant of
// `external_repo_manifest::ManifestWriteOutcome`.

pub type ManifestWriterFn = std::sync::Arc<
    dyn Fn(&Path, &str, &Path, &str) -> String + Send + Sync,
>;

#[doc(hidden)]
pub static MANIFEST_WRITER: std::sync::OnceLock<ManifestWriterFn> =
    std::sync::OnceLock::new();

/// Phase 5.C.4 commit 149 (Batch S): manifest-reader
/// callback. Same OnceLock pattern as the writer.
/// rush-build registers a function that consults
/// `<cache_root>/external-manifests/<repo>.json`
/// and returns the discriminant string:
///   "valid" | "missing" | "invalid" | "incomplete"
///   | "stale" | "unknown"
/// Returns "absent" if no reader has been registered
/// (unit-test contexts).
pub type ManifestReaderFn = std::sync::Arc<
    dyn Fn(&Path, &str) -> String + Send + Sync,
>;

#[doc(hidden)]
pub static MANIFEST_READER: std::sync::OnceLock<ManifestReaderFn> =
    std::sync::OnceLock::new();

#[doc(hidden)]
pub fn register_manifest_reader(f: ManifestReaderFn) {
    let _ = MANIFEST_READER.set(f);
}

/// Phase 5.C.4 commit 149 (Batch S): invoke the
/// registered manifest reader. Returns the
/// discriminant string for skip-decision logic.
/// "absent" when no reader is registered.
pub fn read_manifest_status_via_callback(
    workspace_root: &Path,
    repo_name: &str,
) -> String {
    match MANIFEST_READER.get() {
        Some(f) => f(workspace_root, repo_name),
        None => "absent".to_string(),
    }
}

/// Phase 5.C.4 commit 148 (Batch S): rush-build
/// calls this once during orchestrator init to
/// register its manifest writer. rush-starlark
/// then folds manifest writes into the fetch path
/// via `write_manifest_via_callback`.
#[doc(hidden)]
pub fn register_manifest_writer(f: ManifestWriterFn) {
    let _ = MANIFEST_WRITER.set(f);
}

/// Phase 5.C.4 commit 148 (Batch S): invoke the
/// registered manifest writer (if any). Returns
/// the outcome discriminant ("written" / "skipped_*"
/// / "failed") for counter wiring. Returns "absent"
/// if no writer has been registered (e.g., unit
/// test contexts).
pub fn write_manifest_via_callback(
    workspace_root: &Path,
    repo_name: &str,
    external_path: &Path,
    bazel_command_shape: &str,
) -> String {
    match MANIFEST_WRITER.get() {
        Some(f) => f(
            workspace_root,
            repo_name,
            external_path,
            bazel_command_shape,
        ),
        None => "absent".to_string(),
    }
}

/// Phase 5.C.4 commit 162 (Batch U): explicit
/// strategy-precedence helper. Decides which
/// FetchStrategy the unified warmup actually runs
/// given:
///   - `requested`: what `FetchStrategy::from_env()`
///     returned (may be Serial if the operator
///     didn't set RUSH_FETCH_STRATEGY).
///   - `explicit_env_set`: whether the env var was
///     present (distinguishes "operator wrote
///     `RUSH_FETCH_STRATEGY=serial`" from "operator
///     didn't set anything; from_env returned the
///     default Serial").
///   - `capability`: bazel's `sync --only`
///     repeatability probe result.
///
/// Precedence rules (per Pro's commit-162 letter):
///   1. If the operator explicitly set
///      `RUSH_FETCH_STRATEGY`, honor it — even if
///      that means using serial for the unified
///      warmup. Pro: "Operators occasionally do
///      things for debugging, and the tool should
///      not play 'I know better.'"
///   2. Otherwise (env unset), promote
///      Serial → Auto so the warmup gets the
///      combined speedup when bazel supports it.
///   3. Auto resolves to Combined when capability is
///      Supported; serial fallback otherwise.
pub fn resolve_warmup_strategy(
    requested: FetchStrategy,
    explicit_env_set: bool,
    capability: &CombinedCapability,
) -> FetchStrategy {
    let pre = if requested == FetchStrategy::Serial && !explicit_env_set
    {
        // Env unset → from_env returned the
        // default Serial. Promote to Auto so the
        // warmup picks combined when supported.
        FetchStrategy::Auto
    } else {
        // Operator was explicit (or selected
        // something other than Serial). Honor it.
        requested
    };
    resolve_runtime_strategy(pre, capability, false)
}

/// Phase 5.C.4 commit 161 (Batch U): typed source
/// attribution for a single repo in a unified
/// warmup request. Reported alongside per-repo
/// outcomes so operators can see which knob (plan
/// or env CSV) contributed each repo, and which
/// were declared by both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmupRepoSource {
    /// Repo came from the FetchWarmupPlan's
    /// `repos_required_for_fetch` only.
    PlanOnly,
    /// Repo came from
    /// `RUSH_FETCH_WARMUP_REPOS=<csv>` only.
    EnvOnly,
    /// Repo was listed in both.
    Both,
}

impl WarmupRepoSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            WarmupRepoSource::PlanOnly => "plan_only",
            WarmupRepoSource::EnvOnly => "env_only",
            WarmupRepoSource::Both => "both",
        }
    }
}

/// Phase 5.C.4 commit 170 (Batch V): structured
/// return shape for `perform_unified_warmup`. Adds
/// the resolved fetch strategy alongside the
/// per-repo results so commit 170's warmup-mode
/// report can name the strategy without scraping
/// stderr.
///
/// Behaves like a Vec via Deref-style helpers so
/// existing callers that only care about the
/// per-repo Vec stay terse.
#[derive(Debug, Clone)]
pub struct UnifiedWarmupOutcome {
    pub strategy: FetchStrategy,
    pub repos: Vec<(String, WarmupRepoSource, bool)>,
    pub wall_ms: u64,
}

impl UnifiedWarmupOutcome {
    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
    }
    pub fn len(&self) -> usize {
        self.repos.len()
    }
    pub fn iter(
        &self,
    ) -> std::slice::Iter<'_, (String, WarmupRepoSource, bool)> {
        self.repos.iter()
    }
}

/// Phase 5.C.4 commit 164 (Batch U): parse
/// `RUSH_FETCH_WARMUP_STRICT`. Accepts `1`, `true`,
/// `yes` (case-insensitive) as enable; anything else
/// (including unset) is the default best-effort
/// mode.
///
/// Pro's directive for commit 164: "Default behavior:
/// log a warning, clear WARMED_UP for that repo,
/// continue with normal analyze (slow path). Add an
/// opt-in RUSH_FETCH_WARMUP_STRICT=1 mode that turns
/// these failures fatal — useful for CI configs
/// that want a hard signal."
pub fn is_warmup_strict() -> bool {
    std::env::var("RUSH_FETCH_WARMUP_STRICT")
        .map(|v| {
            let lc = v.trim().to_ascii_lowercase();
            matches!(lc.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

/// Phase 5.C.4 commit 164 (Batch U): inspect a
/// unified-warmup report and report which repos
/// failed.
///
/// Returns the failed-repo names alpha-sorted by
/// construction (commit 161 sorted the input). The
/// orchestrator uses this to:
///   - emit a deterministic warning in non-strict mode
///   - build a deterministic Err message in strict
///     mode
pub fn warmup_report_failures(
    report: &[(String, WarmupRepoSource, bool)],
) -> Vec<String> {
    report
        .iter()
        .filter(|(_, _, ok)| !*ok)
        .map(|(name, _, _)| name.clone())
        .collect()
}

/// Phase 5.C.4 commit 161 (Batch U): unified warmup
/// request. Replaces commit 143's
/// `perform_warmup_if_configured` and commit 156's
/// `perform_warmup_from_plan` for the common case
/// where the orchestrator wants ONE fetch
/// invocation covering both plan and env sources.
///
/// Per Pro's commit-161 directive:
///   "Create one warmup request that merges plan
///    repos and RUSH_FETCH_WARMUP_REPOS CSV repos.
///    Dedupe repos before executing fetch. Preserve
///    source attribution. Run exactly one fetch
///    strategy invocation for the merged set."
///
/// The caller passes pre-categorized repo lists:
///   - `plan_repos`: from plan's
///     `repos_required_for_fetch`.
///   - `env_repos`: from
///     `RUSH_FETCH_WARMUP_REPOS=<csv>`.
///
/// This function:
///   1. Computes the union (deduped, alphabetized).
///   2. Records source attribution per repo
///      (PlanOnly / EnvOnly / Both).
///   3. Runs ONE combined/serial invocation.
///   4. Populates WARMED_UP_REPOS exactly as
///      commit 143's hook does.
///   5. Returns Vec<(repo_name, source, succeeded)>
///      sorted by repo_name for stable reporting.
pub fn perform_unified_warmup(
    workspace_root: &Path,
    bazel_binary: &Path,
    output_base: &Path,
    plan_repos: &[String],
    env_repos: &[String],
) -> Result<UnifiedWarmupOutcome> {
    use std::collections::BTreeMap;

    // Build source-attributed merged set.
    let mut sources: BTreeMap<String, WarmupRepoSource> =
        BTreeMap::new();
    for r in plan_repos {
        sources.insert(r.clone(), WarmupRepoSource::PlanOnly);
    }
    for r in env_repos {
        sources
            .entry(r.clone())
            .and_modify(|s| *s = WarmupRepoSource::Both)
            .or_insert(WarmupRepoSource::EnvOnly);
    }
    if sources.is_empty() {
        // Empty input → empty outcome. Strategy is
        // Serial as a no-op default; wall_ms is 0.
        return Ok(UnifiedWarmupOutcome {
            strategy: FetchStrategy::Serial,
            repos: Vec::new(),
            wall_ms: 0,
        });
    }
    let repos: Vec<&str> = sources.keys().map(|s| s.as_str()).collect();

    // No-op fast path: if every warmup repo is already materialized in the output base, skip the
    // bazel warmup entirely. The plan deliberately keeps already-materialized repos in the warmup
    // set for stability (no-op oscillation fix, commit d9cc0b1), but invoking bazel to "warm" repos
    // that are already present costs ~6s of JVM + repo-rule eval on a TF no-op and fetches nothing —
    // it was the single biggest cost of the ~7s no-op. (TF-scale no-op profiling 2026-06-24.)
    if !repos.is_empty()
        && repos
            .iter()
            .all(|r| output_base.join("external").join(r).exists())
    {
        let lock = WARMED_UP_REPOS
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
        if let Ok(mut map) = lock.lock() {
            for r in &repos {
                map.insert(r.to_string(), output_base.join("external").join(r));
            }
        }
        eprintln!(
            "[fetch warmup unified] all {} repo(s) already materialized — skipping bazel warmup",
            repos.len()
        );
        let out: Vec<(String, WarmupRepoSource, bool)> = sources
            .iter()
            .map(|(name, src)| (name.clone(), *src, true))
            .collect();
        return Ok(UnifiedWarmupOutcome {
            strategy: FetchStrategy::Serial,
            repos: out,
            wall_ms: 0,
        });
    }

    std::fs::create_dir_all(output_base)?;
    eprintln!(
        "[fetch warmup unified] repos={} (plan={} env={} both={})",
        sources.len(),
        sources
            .values()
            .filter(|s| matches!(s, WarmupRepoSource::PlanOnly))
            .count(),
        sources
            .values()
            .filter(|s| matches!(s, WarmupRepoSource::EnvOnly))
            .count(),
        sources
            .values()
            .filter(|s| matches!(s, WarmupRepoSource::Both))
            .count(),
    );
    let start = std::time::Instant::now();

    let cap = probe_combined_capability(bazel_binary);
    let requested = FetchStrategy::from_env();
    let explicit_env_set =
        std::env::var("RUSH_FETCH_STRATEGY").is_ok();
    let resolved =
        resolve_warmup_strategy(requested, explicit_env_set, &cap);

    let mut succeeded = Vec::new();
    let mut failed = Vec::new();
    match resolved {
        FetchStrategy::Combined => {
            match execute_combined_fetch(
                bazel_binary,
                output_base,
                workspace_root,
                &repos,
                &cap,
                false,
            ) {
                Ok(()) => {
                    for r in &repos {
                        succeeded.push(r.to_string());
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[fetch warmup unified] combined failed: {}; falling back to serial",
                        e
                    );
                    for r in &repos {
                        match run_one_serial_fetch(
                            bazel_binary,
                            output_base,
                            workspace_root,
                            r,
                        ) {
                            Ok(()) => succeeded.push(r.to_string()),
                            Err(e2) => {
                                failed.push((
                                    r.to_string(),
                                    format!("{}", e2),
                                ))
                            }
                        }
                    }
                }
            }
        }
        FetchStrategy::Parallel => {
            let conc = parse_concurrency_env();
            let results = execute_parallel_fetch(
                bazel_binary,
                output_base,
                workspace_root,
                &repos,
                conc,
            );
            for r in results {
                match r.outcome {
                    Ok(()) => succeeded.push(r.repo_name),
                    Err(e) => failed.push((r.repo_name, format!("{}", e))),
                }
            }
        }
        FetchStrategy::Serial | FetchStrategy::Auto => {
            for r in &repos {
                match run_one_serial_fetch(
                    bazel_binary,
                    output_base,
                    workspace_root,
                    r,
                ) {
                    Ok(()) => succeeded.push(r.to_string()),
                    Err(e) => {
                        failed.push((r.to_string(), format!("{}", e)))
                    }
                }
            }
        }
    }

    // Populate WARMED_UP_REPOS for the per-repo
    // fetch short-circuit (commit 143).
    let entries: std::collections::HashMap<String, PathBuf> = succeeded
        .iter()
        .map(|r| (r.clone(), output_base.join("external").join(r)))
        .collect();
    let lock = WARMED_UP_REPOS.get_or_init(|| {
        std::sync::Mutex::new(std::collections::HashMap::new())
    });
    if let Ok(mut map) = lock.lock() {
        for (k, v) in entries.iter() {
            map.insert(k.clone(), v.clone());
        }
    }

    // Write manifests for succeeded repos.
    let cmd_shape = format!("bazel sync --only=... ({})", resolved.as_str());
    for r in &succeeded {
        let ext = output_base.join("external").join(r);
        let _ = write_manifest_via_callback(
            workspace_root,
            r,
            &ext,
            &cmd_shape,
        );
    }

    let dur = start.elapsed();
    eprintln!(
        "[fetch warmup unified] strategy={} succeeded={} failed={} wall={} ms",
        resolved.as_str(),
        succeeded.len(),
        failed.len(),
        dur.as_millis()
    );
    if !failed.is_empty() {
        for (r, e) in &failed {
            eprintln!("[fetch warmup unified] FAIL {}: {}", r, e);
        }
    }

    let succeeded_set: std::collections::HashSet<&str> =
        succeeded.iter().map(|s| s.as_str()).collect();
    let mut out: Vec<(String, WarmupRepoSource, bool)> = sources
        .iter()
        .map(|(name, src)| {
            (name.clone(), *src, succeeded_set.contains(name.as_str()))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(UnifiedWarmupOutcome {
        strategy: resolved,
        repos: out,
        wall_ms: dur.as_millis() as u64,
    })
}

/// Phase 5.C.4 commit 156 (Batch T): plan-driven
/// warmup. Called by the orchestrator when
/// `RUSH_FETCH_WARMUP_PLAN=auto` discovered a
/// saved plan. The `repos` list comes from the
/// plan's `repos_required` field.
///
/// Unlike `perform_warmup_if_configured`
/// (commit 143), this entry point does NOT read
/// `RUSH_FETCH_WARMUP_REPOS` — the repo list is
/// the caller's responsibility. It also defaults
/// to `Combined` strategy when available (with
/// serial fallback) regardless of
/// `RUSH_FETCH_STRATEGY`, because the plan-driven
/// path is the entire point of auto mode.
///
/// Returns the strategy that actually ran so the
/// caller can log it.
pub fn perform_warmup_from_plan(
    workspace_root: &Path,
    bazel_binary: &Path,
    output_base: &Path,
    repos: &[&str],
) -> Result<FetchStrategy> {
    if repos.is_empty() {
        return Ok(FetchStrategy::Serial);
    }
    std::fs::create_dir_all(output_base)?;

    eprintln!(
        "[fetch warmup-plan] repos={} target_output_base={}",
        repos.len(),
        output_base.display()
    );
    let start = std::time::Instant::now();

    let cap = probe_combined_capability(bazel_binary);
    let resolved = resolve_runtime_strategy(
        FetchStrategy::Auto,
        &cap,
        false,
    );

    let mut succeeded = Vec::new();
    let mut failed = Vec::new();
    match resolved {
        FetchStrategy::Combined => {
            match execute_combined_fetch(
                bazel_binary,
                output_base,
                workspace_root,
                repos,
                &cap,
                false,
            ) {
                Ok(()) => {
                    for r in repos {
                        succeeded.push(r.to_string());
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[fetch warmup-plan] combined failed: {}; falling back to serial",
                        e
                    );
                    for r in repos {
                        match run_one_serial_fetch(
                            bazel_binary,
                            output_base,
                            workspace_root,
                            r,
                        ) {
                            Ok(()) => succeeded.push(r.to_string()),
                            Err(e2) => {
                                failed.push((
                                    r.to_string(),
                                    format!("{}", e2),
                                ))
                            }
                        }
                    }
                }
            }
        }
        // Auto + !capability → Serial.
        _ => {
            for r in repos {
                match run_one_serial_fetch(
                    bazel_binary,
                    output_base,
                    workspace_root,
                    r,
                ) {
                    Ok(()) => succeeded.push(r.to_string()),
                    Err(e) => {
                        failed.push((r.to_string(), format!("{}", e)))
                    }
                }
            }
        }
    }

    // Populate WARMED_UP_REPOS so per-repo fetch
    // short-circuits exactly as the env-driven
    // warmup path does (commit 143).
    let entries: std::collections::HashMap<String, PathBuf> = succeeded
        .iter()
        .map(|r| (r.clone(), output_base.join("external").join(r)))
        .collect();
    let lock = WARMED_UP_REPOS.get_or_init(|| {
        std::sync::Mutex::new(std::collections::HashMap::new())
    });
    if let Ok(mut map) = lock.lock() {
        for (k, v) in entries.iter() {
            map.insert(k.clone(), v.clone());
        }
    }

    // Write manifests for the warmed-up repos.
    let cmd_shape = format!(
        "bazel sync --only=... ({})",
        resolved.as_str()
    );
    for r in &succeeded {
        let ext = output_base.join("external").join(r);
        let _ = write_manifest_via_callback(
            workspace_root,
            r,
            &ext,
            &cmd_shape,
        );
    }

    let dur = start.elapsed();
    eprintln!(
        "[fetch warmup-plan] strategy={} succeeded={} failed={} wall={} ms",
        resolved.as_str(),
        succeeded.len(),
        failed.len(),
        dur.as_millis()
    );
    if !failed.is_empty() {
        for (r, e) in &failed {
            eprintln!("[fetch warmup-plan] FAIL {}: {}", r, e);
        }
    }
    Ok(resolved)
}

/// Phase 5.C.4 commit 143 (Batch R): orchestrator
/// pre-warmup hook. If `RUSH_FETCH_STRATEGY` is set
/// to anything other than `serial` AND
/// `RUSH_FETCH_WARMUP_REPOS` is a non-empty
/// comma-separated list, runs the appropriate
/// executor (Combined or Parallel) to materialize
/// the listed repos in one batch. Subsequent
/// per-repo calls into `fetch_repo_via_bazel` then
/// short-circuit via `is_warmed_up`.
///
/// Env contract (Pro's letter):
///   RUSH_FETCH_STRATEGY = serial | combined | parallel | auto
///   RUSH_FETCH_WARMUP_REPOS = repo1,repo2,repo3,...
///   RUSH_FETCH_CONCURRENCY = N (parallel only,
///                              default 4, clamped 1..=64)
///
/// Returns `Ok(())` on success or no-op (env unset,
/// empty list, or Serial). Errors propagate from
/// the executor; the orchestrator can choose to
/// fail analyze or warn-and-continue.
pub fn perform_warmup_if_configured(
    workspace_root: &Path,
    bazel_binary: &Path,
    output_base: &Path,
) -> Result<()> {
    let strategy = FetchStrategy::from_env();
    if matches!(strategy, FetchStrategy::Serial) {
        return Ok(());
    }
    let repos_env = std::env::var("RUSH_FETCH_WARMUP_REPOS")
        .ok()
        .unwrap_or_default();
    let repos: Vec<&str> = repos_env
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if repos.is_empty() {
        return Ok(());
    }

    std::fs::create_dir_all(output_base)?;

    eprintln!(
        "[fetch warmup] strategy={} repos={} target_output_base={}",
        strategy.as_str(),
        repos.len(),
        output_base.display()
    );
    let start = std::time::Instant::now();

    let cap = probe_combined_capability(bazel_binary);
    let resolved = resolve_runtime_strategy(strategy, &cap, false);

    let mut succeeded = Vec::new();
    let mut failed = Vec::new();

    match resolved {
        FetchStrategy::Combined => {
            match execute_combined_fetch(
                bazel_binary,
                output_base,
                workspace_root,
                &repos,
                &cap,
                false,
            ) {
                Ok(()) => {
                    for r in &repos {
                        succeeded.push(r.to_string());
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[fetch warmup] combined failed: {}; falling back to serial",
                        e
                    );
                    for r in &repos {
                        match run_one_serial_fetch(
                            bazel_binary,
                            output_base,
                            workspace_root,
                            r,
                        ) {
                            Ok(()) => succeeded.push(r.to_string()),
                            Err(e2) => failed.push((r.to_string(), format!("{}", e2))),
                        }
                    }
                }
            }
        }
        FetchStrategy::Parallel => {
            let conc = parse_concurrency_env();
            eprintln!(
                "[fetch warmup] parallel concurrency={}",
                conc
            );
            let results = execute_parallel_fetch(
                bazel_binary,
                output_base,
                workspace_root,
                &repos,
                conc,
            );
            for r in results {
                match r.outcome {
                    Ok(()) => succeeded.push(r.repo_name),
                    Err(e) => failed.push((r.repo_name, format!("{}", e))),
                }
            }
        }
        FetchStrategy::Serial | FetchStrategy::Auto => {
            // Auto-with-no-capability falls back here.
            for r in &repos {
                match run_one_serial_fetch(
                    bazel_binary,
                    output_base,
                    workspace_root,
                    r,
                ) {
                    Ok(()) => succeeded.push(r.to_string()),
                    Err(e) => failed.push((r.to_string(), format!("{}", e))),
                }
            }
        }
    }

    // Populate WARMED_UP_REPOS for the successful
    // ones so per-repo fetch can short-circuit.
    let entries: std::collections::HashMap<String, PathBuf> = succeeded
        .iter()
        .map(|r| {
            (
                r.clone(),
                output_base.join("external").join(r),
            )
        })
        .collect();
    let lock = WARMED_UP_REPOS.get_or_init(|| {
        std::sync::Mutex::new(std::collections::HashMap::new())
    });
    if let Ok(mut map) = lock.lock() {
        for (k, v) in entries.iter() {
            map.insert(k.clone(), v.clone());
        }
    }

    // Phase 5.C.4 commit 148 (Batch S): write a
    // sidecar manifest for each successfully
    // materialized repo. The writer (registered by
    // rush-build) checks for BUILD/BUILD.bazel
    // presence before writing; failures are logged
    // but do not abort the warmup.
    let cmd_shape = match resolved {
        FetchStrategy::Combined => "bazel sync --only=... (combined)".to_string(),
        FetchStrategy::Parallel => format!(
            "bazel sync --only=<one> (parallel c={})",
            parse_concurrency_env()
        ),
        _ => "bazel sync --only=<one>".to_string(),
    };
    for r in &succeeded {
        let ext = output_base.join("external").join(r);
        let _ = write_manifest_via_callback(
            workspace_root,
            r,
            &ext,
            &cmd_shape,
        );
    }

    let dur = start.elapsed();
    eprintln!(
        "[fetch warmup] strategy={} succeeded={} failed={} wall={} ms",
        resolved.as_str(),
        succeeded.len(),
        failed.len(),
        dur.as_millis()
    );
    if !failed.is_empty() {
        for (r, e) in &failed {
            eprintln!("[fetch warmup] FAIL {}: {}", r, e);
        }
        return Err(anyhow!(
            "{} of {} warmup fetches failed",
            failed.len(),
            failed.len() + succeeded.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Write a fake bazel script that records its
    /// argv into `argv_file` and exits with
    /// `exit_code`. If `help_sync_output` is given,
    /// `bazel help sync` prints it.
    fn write_fake_bazel(
        dir: &TempDir,
        argv_file: &Path,
        exit_code: i32,
        help_sync_output: Option<&str>,
    ) -> PathBuf {
        let path = dir.path().join("bazel");
        let body = format!(
            r#"#!/usr/bin/env bash
# fake bazel for commit 141 tests
# args are recorded to {argv_file} one per line.
if [ "$1" = "help" ] && [ "$2" = "sync" ]; then
  cat <<'EOF'
{help_text}
EOF
  exit 0
fi
printf '%s\n' "$@" >> {argv_file}
exit {exit_code}
"#,
            argv_file = argv_file.display(),
            help_text = help_sync_output.unwrap_or(
                "Usage: bazel sync [<option> ...]\nFlags:\n  --only (a string; may be used multiple times)\n"
            ),
            exit_code = exit_code,
        );
        fs::write(&path, body).expect("write fake bazel");
        let mut p = fs::metadata(&path).unwrap().permissions();
        p.set_mode(0o755);
        fs::set_permissions(&path, p).unwrap();
        path
    }

    fn make_tmp_workspace_and_output_base() -> (TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let output_base = dir.path().join("output_base");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(&output_base).unwrap();
        (dir, ws, output_base)
    }

    #[test]
    fn combined_strategy_invokes_single_bazel_command() {
        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        // help sync output with the "multiple times"
        // marker → Supported.
        let bazel = write_fake_bazel(&dir, &argv_file, 0, None);

        let cap = probe_combined_capability(&bazel);
        assert_eq!(cap, CombinedCapability::Supported);

        execute_combined_fetch(
            &bazel,
            &ob,
            &ws,
            &["repo_a", "repo_b", "repo_c"],
            &cap,
            false,
        )
        .expect("combined fetch should succeed");

        // The fake bazel records argv to argv_file —
        // a single invocation should produce ONE
        // batch of args containing the three
        // --only=X flags.
        let logged = fs::read_to_string(&argv_file)
            .expect("argv log");
        let only_args: Vec<&str> = logged
            .lines()
            .filter(|l| l.starts_with("--only="))
            .collect();
        assert_eq!(
            only_args,
            vec!["--only=repo_a", "--only=repo_b", "--only=repo_c"],
            "combined strategy must produce ONE bazel invocation with \
             repeated --only flags"
        );
        // Sanity: the argv log must contain the `sync`
        // verb exactly once. (One process = one log
        // append.)
        let sync_count = logged.lines().filter(|l| *l == "sync").count();
        assert_eq!(sync_count, 1, "exactly one bazel invocation");
    }

    #[test]
    fn unsupported_combined_falls_back_to_serial() {
        let (dir, _ws, _ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        // help sync output WITHOUT the marker →
        // NotSupported.
        let bazel = write_fake_bazel(
            &dir,
            &argv_file,
            0,
            Some(
                "Usage: bazel sync\nFlags:\n  --configure (a boolean)\n",
            ),
        );

        let cap = probe_combined_capability(&bazel);
        assert_eq!(cap, CombinedCapability::NotSupported);

        // resolve_runtime_strategy should fall back
        // when not forced.
        let resolved =
            resolve_runtime_strategy(FetchStrategy::Combined, &cap, false);
        assert_eq!(resolved, FetchStrategy::Serial);

        // execute_combined_fetch with forced=false must
        // return Err so the caller serializes.
        let err = execute_combined_fetch(
            &bazel,
            std::path::Path::new("/tmp/ob"),
            std::path::Path::new("/tmp/ws"),
            &["foo"],
            &cap,
            false,
        )
        .expect_err("must err on unsupported when not forced");
        assert!(
            format!("{}", err).contains("unsupported"),
            "error message must indicate unsupported: {}",
            err
        );
    }

    #[test]
    fn forced_combined_failure_reports_clear_error() {
        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        // help sync OK (Supported), but the sync
        // subcommand fails (exit 1) — represents
        // bazel rejecting the args (e.g., wrong
        // repo name).
        let bazel = write_fake_bazel(&dir, &argv_file, 1, None);
        let cap = probe_combined_capability(&bazel);
        assert_eq!(cap, CombinedCapability::Supported);

        // forced=true so we attempt anyway and
        // bubble up the failure.
        let err = execute_combined_fetch(
            &bazel,
            &ob,
            &ws,
            &["bad_repo"],
            &cap,
            true,
        )
        .expect_err("forced combined failure must err");
        let msg = format!("{}", err);
        assert!(
            msg.contains("combined") && msg.contains("failed"),
            "error must indicate combined fetch failed: {}",
            msg
        );
        assert!(
            msg.contains("forced combined mode"),
            "error must mention forced mode: {}",
            msg
        );
    }

    #[test]
    fn from_env_value_accepts_each_strategy() {
        assert_eq!(
            FetchStrategy::from_env_value("serial"),
            FetchStrategy::Serial
        );
        assert_eq!(
            FetchStrategy::from_env_value("Combined"),
            FetchStrategy::Combined
        );
        assert_eq!(
            FetchStrategy::from_env_value("PARALLEL"),
            FetchStrategy::Parallel
        );
        assert_eq!(
            FetchStrategy::from_env_value("auto"),
            FetchStrategy::Auto
        );
        // Unknown → default Serial (no panic).
        assert_eq!(
            FetchStrategy::from_env_value("turbo"),
            FetchStrategy::Serial
        );
    }

    #[test]
    fn empty_repos_combined_returns_ok() {
        // Edge case: no repos to fetch → no-op.
        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        let bazel = write_fake_bazel(&dir, &argv_file, 0, None);
        let cap = probe_combined_capability(&bazel);
        execute_combined_fetch(&bazel, &ob, &ws, &[], &cap, false)
            .expect("empty list must succeed without invoking bazel");
        // argv log should be empty (sync wasn't called).
        let logged = fs::read_to_string(&argv_file)
            .unwrap_or_else(|_| String::new());
        assert!(
            !logged.contains("sync"),
            "no bazel invocation expected for empty repo list"
        );
    }

    // =====================================================
    // Phase 5.C.4 commit 142 (Batch R): parallel fetch tests
    // =====================================================

    /// Fake bazel that records concurrency
    /// observations. Each invocation:
    ///   1. Acquires a flock on `lockfile`, increments
    ///      a counter file, sleeps briefly, releases.
    ///   2. Records the (start, end, repo, peak_seen)
    ///      tuple to argv_file.
    ///   3. Exits 0 (unless `fail_repos` matches).
    fn write_concurrency_aware_fake_bazel(
        dir: &TempDir,
        argv_file: &Path,
        inflight_dir: &Path,
        sleep_ms: u32,
        fail_for_repo: Option<&str>,
    ) -> PathBuf {
        fs::create_dir_all(inflight_dir).unwrap();
        let path = dir.path().join("bazel");
        let fail_clause = match fail_for_repo {
            Some(r) => format!(
                r#"for arg in "$@"; do
  if [ "$arg" = "--only={}" ]; then
    exit 1
  fi
done"#,
                r
            ),
            None => String::new(),
        };
        let body = format!(
            r#"#!/usr/bin/env bash
# fake bazel for commit 142 parallel tests
if [ "$1" = "help" ] && [ "$2" = "sync" ]; then
  printf 'Usage: bazel sync\nFlags:\n  --only (a string; may be used multiple times)\n'
  exit 0
fi
{fail_clause}
# Extract the --only=<repo> arg.
repo=""
for arg in "$@"; do
  case "$arg" in
    --only=*) repo="${{arg#--only=}}";;
  esac
done
# Mark in-flight: create a unique file in inflight_dir.
marker="{inflight_dir}/$$.${{repo}}"
touch "$marker"
# Count current in-flight repos.
inflight_count=$(ls "{inflight_dir}" | wc -l)
# Append START line atomically.
{{
  flock -x 200
  printf 'START repo=%s inflight=%s\n' "$repo" "$inflight_count" >> {argv_file}
}} 200>{argv_file}.lock
# Simulated fetch work.
sleep {sleep_ms_decimal}
{{
  flock -x 200
  printf 'END repo=%s\n' "$repo" >> {argv_file}
}} 200>{argv_file}.lock
rm -f "$marker"
exit 0
"#,
            fail_clause = fail_clause,
            inflight_dir = inflight_dir.display(),
            argv_file = argv_file.display(),
            sleep_ms_decimal = format!("{}.{:03}", sleep_ms / 1000, sleep_ms % 1000),
        );
        fs::write(&path, body).expect("write fake bazel");
        let mut p = fs::metadata(&path).unwrap().permissions();
        p.set_mode(0o755);
        fs::set_permissions(&path, p).unwrap();
        path
    }

    /// Parse the START lines to find peak observed
    /// concurrency.
    fn peak_inflight_from_log(log: &str) -> usize {
        log.lines()
            .filter_map(|l| {
                l.strip_prefix("START ")
                    .and_then(|rest| rest.split("inflight=").nth(1))
                    .and_then(|s| s.trim().parse::<usize>().ok())
            })
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn parallel_fetch_runs_independent_repos() {
        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        let inflight = dir.path().join("inflight");
        let bazel = write_concurrency_aware_fake_bazel(
            &dir,
            &argv_file,
            &inflight,
            10,
            None,
        );
        let results = execute_parallel_fetch(
            &bazel,
            &ob,
            &ws,
            &["alpha", "beta", "gamma"],
            2,
        );
        // Every repo must show up exactly once.
        assert_eq!(results.len(), 3);
        let names: Vec<&str> = results
            .iter()
            .map(|r| r.repo_name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
        // All succeeded.
        for r in &results {
            assert!(r.outcome.is_ok(), "{} failed: {:?}", r.repo_name, r.outcome);
        }
        // Each repo's START line appears in the log.
        let log = fs::read_to_string(&argv_file).unwrap();
        assert!(log.contains("START repo=alpha"));
        assert!(log.contains("START repo=beta"));
        assert!(log.contains("START repo=gamma"));
    }

    #[test]
    fn concurrency_limit_respected() {
        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        let inflight = dir.path().join("inflight");
        let bazel = write_concurrency_aware_fake_bazel(
            &dir,
            &argv_file,
            &inflight,
            // Long enough that the workers actually
            // overlap; short enough that the test is
            // fast.
            50,
            None,
        );
        let results = execute_parallel_fetch(
            &bazel,
            &ob,
            &ws,
            &["a", "b", "c", "d", "e", "f"],
            2,
        );
        assert_eq!(results.len(), 6);
        let log = fs::read_to_string(&argv_file).unwrap();
        let peak = peak_inflight_from_log(&log);
        assert!(
            peak <= 2,
            "peak inflight {} exceeded concurrency limit 2:\n{}",
            peak,
            log
        );
        // Sanity: we DID observe at least 2 overlapping
        // — otherwise the test isn't really exercising
        // concurrency.
        assert!(
            peak >= 2,
            "peak inflight {} < 2; concurrency not exercised:\n{}",
            peak,
            log
        );
    }

    #[test]
    fn duplicate_repo_fetches_deduped() {
        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        let inflight = dir.path().join("inflight");
        let bazel = write_concurrency_aware_fake_bazel(
            &dir,
            &argv_file,
            &inflight,
            5,
            None,
        );
        let results = execute_parallel_fetch(
            &bazel,
            &ob,
            &ws,
            &["x", "y", "x", "y", "x"],
            4,
        );
        // After dedup the unique set is {x, y} → 2 results.
        assert_eq!(results.len(), 2);
        let names: Vec<&str> = results
            .iter()
            .map(|r| r.repo_name.as_str())
            .collect();
        assert_eq!(names, vec!["x", "y"]);
        // The log should show each repo started exactly once.
        let log = fs::read_to_string(&argv_file).unwrap();
        let x_starts = log
            .lines()
            .filter(|l| l.contains("START repo=x"))
            .count();
        let y_starts = log
            .lines()
            .filter(|l| l.contains("START repo=y"))
            .count();
        assert_eq!(x_starts, 1, "duplicate of x fetched: {}", log);
        assert_eq!(y_starts, 1, "duplicate of y fetched: {}", log);
    }

    #[test]
    fn failure_report_order_deterministic() {
        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        let inflight = dir.path().join("inflight");
        // Make "broken" fail.
        let bazel = write_concurrency_aware_fake_bazel(
            &dir,
            &argv_file,
            &inflight,
            5,
            Some("broken"),
        );
        let results = execute_parallel_fetch(
            &bazel,
            &ob,
            &ws,
            // Intentionally out-of-order input — the
            // executor must sort by repo_name in the
            // output regardless.
            &["zebra", "broken", "alpha", "mango"],
            3,
        );
        assert_eq!(results.len(), 4);
        let names: Vec<&str> = results
            .iter()
            .map(|r| r.repo_name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["alpha", "broken", "mango", "zebra"],
            "results must be alpha-sorted regardless of input order"
        );
        // Only `broken` failed; the other three succeeded.
        for r in &results {
            if r.repo_name == "broken" {
                assert!(
                    r.outcome.is_err(),
                    "broken should have failed"
                );
            } else {
                assert!(
                    r.outcome.is_ok(),
                    "{} should have succeeded: {:?}",
                    r.repo_name,
                    r.outcome
                );
            }
        }
    }

    #[test]
    fn serial_default_unchanged() {
        // RUSH_FETCH_STRATEGY unset should yield Serial.
        let prior =
            std::env::var("RUSH_FETCH_STRATEGY").ok();
        std::env::remove_var("RUSH_FETCH_STRATEGY");
        assert_eq!(FetchStrategy::from_env(), FetchStrategy::Serial);

        // RUSH_FETCH_CONCURRENCY unset → default.
        let prior_c =
            std::env::var("RUSH_FETCH_CONCURRENCY").ok();
        std::env::remove_var("RUSH_FETCH_CONCURRENCY");
        assert_eq!(parse_concurrency_env(), DEFAULT_FETCH_CONCURRENCY);

        // Restore.
        match prior {
            Some(v) => std::env::set_var("RUSH_FETCH_STRATEGY", v),
            None => std::env::remove_var("RUSH_FETCH_STRATEGY"),
        }
        match prior_c {
            Some(v) => std::env::set_var("RUSH_FETCH_CONCURRENCY", v),
            None => std::env::remove_var("RUSH_FETCH_CONCURRENCY"),
        }
    }

    #[test]
    fn concurrency_env_parsing_clamps_and_defaults() {
        let prior =
            std::env::var("RUSH_FETCH_CONCURRENCY").ok();
        std::env::set_var("RUSH_FETCH_CONCURRENCY", "0");
        assert_eq!(parse_concurrency_env(), 1, "0 clamps up to 1");
        std::env::set_var("RUSH_FETCH_CONCURRENCY", "999");
        assert_eq!(parse_concurrency_env(), 64, "huge clamps down");
        std::env::set_var("RUSH_FETCH_CONCURRENCY", "garbage");
        assert_eq!(parse_concurrency_env(), DEFAULT_FETCH_CONCURRENCY);
        std::env::set_var("RUSH_FETCH_CONCURRENCY", "8");
        assert_eq!(parse_concurrency_env(), 8);
        match prior {
            Some(v) => std::env::set_var("RUSH_FETCH_CONCURRENCY", v),
            None => std::env::remove_var("RUSH_FETCH_CONCURRENCY"),
        }
    }

    // =====================================================
    // Phase 5.C.4 commit 161 (Batch U): unified warmup tests
    // =====================================================

    #[test]
    fn plan_and_env_repos_are_union_deduped() {
        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        let bazel = write_fake_bazel(&dir, &argv_file, 0, None);
        let plan = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let env = vec!["b".to_string(), "c".to_string(), "d".to_string()];
        let results = perform_unified_warmup(&ws, &bazel, &ob, &plan, &env)
            .expect("unified warmup must succeed");
        // Union = {a, b, c, d}, alpha-sorted.
        let names: Vec<&str> = results.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c", "d"]);
        // All succeeded.
        for (n, _, ok) in &results.repos {
            assert!(*ok, "{} failed", n);
        }
    }

    #[test]
    fn source_attribution_preserved() {
        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        let bazel = write_fake_bazel(&dir, &argv_file, 0, None);
        let plan = vec!["alpha".to_string(), "shared".to_string()];
        let env = vec!["beta".to_string(), "shared".to_string()];
        let results = perform_unified_warmup(&ws, &bazel, &ob, &plan, &env)
            .expect("unified warmup must succeed");
        let map: std::collections::HashMap<&str, WarmupRepoSource> = results
            .iter()
            .map(|(n, s, _)| (n.as_str(), *s))
            .collect();
        assert_eq!(map.get("alpha").copied(), Some(WarmupRepoSource::PlanOnly));
        assert_eq!(map.get("beta").copied(), Some(WarmupRepoSource::EnvOnly));
        assert_eq!(map.get("shared").copied(), Some(WarmupRepoSource::Both));
    }

    #[test]
    fn one_combined_fetch_invocation_for_merged_set() {
        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        let bazel = write_fake_bazel(&dir, &argv_file, 0, None);
        // The unified warmup function picks Auto when
        // RUSH_FETCH_STRATEGY is unset and the
        // capability is supported. The fake bazel
        // here advertises Supported via the default
        // help-sync output.
        std::env::remove_var("RUSH_FETCH_STRATEGY");
        let plan = vec!["a".to_string(), "b".to_string()];
        let env = vec!["c".to_string(), "d".to_string()];
        let _ = perform_unified_warmup(&ws, &bazel, &ob, &plan, &env)
            .expect("unified warmup must succeed");
        // The argv log should contain `sync` exactly
        // once (one combined invocation).
        let log = std::fs::read_to_string(&argv_file).unwrap();
        let sync_count = log.lines().filter(|l| *l == "sync").count();
        assert_eq!(
            sync_count, 1,
            "unified warmup must produce ONE combined sync, got {}:\n{}",
            sync_count, log
        );
        // The single invocation should have 4 --only flags.
        let only_args: Vec<&str> = log
            .lines()
            .filter(|l| l.starts_with("--only="))
            .collect();
        assert_eq!(only_args.len(), 4);
    }

    #[test]
    fn env_only_still_works() {
        // Empty plan + non-empty env → all env repos
        // get fetched, each marked EnvOnly.
        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        let bazel = write_fake_bazel(&dir, &argv_file, 0, None);
        std::env::remove_var("RUSH_FETCH_STRATEGY");
        let plan: Vec<String> = Vec::new();
        let env = vec!["only_env_a".to_string(), "only_env_b".to_string()];
        let results = perform_unified_warmup(&ws, &bazel, &ob, &plan, &env)
            .expect("unified warmup must succeed");
        assert_eq!(results.len(), 2);
        for (_, source, _) in &results.repos {
            assert_eq!(*source, WarmupRepoSource::EnvOnly);
        }
    }

    // =====================================================
    // Phase 5.C.4 commit 162 (Batch U): precedence tests
    // =====================================================

    #[test]
    fn explicit_serial_overrides_plan_auto() {
        // Operator explicitly set
        // RUSH_FETCH_STRATEGY=serial. Even with the
        // capability supported, the unified path
        // must run Serial.
        let cap = CombinedCapability::Supported;
        let resolved = resolve_warmup_strategy(
            FetchStrategy::Serial,
            /*explicit_env_set=*/ true,
            &cap,
        );
        assert_eq!(resolved, FetchStrategy::Serial);
    }

    #[test]
    fn explicit_parallel_overrides_plan_auto() {
        // Operator explicitly set
        // RUSH_FETCH_STRATEGY=parallel. Unified
        // warmup must honor it (parallel was
        // measured dead-on-arrival in Batch R, but
        // the operator may want it for debugging
        // bazel internal serialization).
        let cap = CombinedCapability::Supported;
        let resolved = resolve_warmup_strategy(
            FetchStrategy::Parallel,
            /*explicit_env_set=*/ true,
            &cap,
        );
        assert_eq!(resolved, FetchStrategy::Parallel);
    }

    #[test]
    fn unset_strategy_plan_auto_uses_combined_when_supported() {
        // Env unset (from_env returned default
        // Serial, explicit_env_set=false). The
        // unified path should promote to Auto →
        // Combined when supported.
        let cap = CombinedCapability::Supported;
        let resolved = resolve_warmup_strategy(
            FetchStrategy::Serial,
            /*explicit_env_set=*/ false,
            &cap,
        );
        assert_eq!(resolved, FetchStrategy::Combined);
    }

    #[test]
    fn precedence_combined_unsupported_falls_back_to_serial() {
        // Env unset + capability NotSupported →
        // Auto resolves to Serial.
        let cap = CombinedCapability::NotSupported;
        let resolved = resolve_warmup_strategy(
            FetchStrategy::Serial,
            /*explicit_env_set=*/ false,
            &cap,
        );
        assert_eq!(resolved, FetchStrategy::Serial);

        // Operator explicitly set Combined +
        // capability NotSupported → resolve_runtime_strategy
        // falls back to Serial (with warning).
        let resolved2 = resolve_warmup_strategy(
            FetchStrategy::Combined,
            /*explicit_env_set=*/ true,
            &cap,
        );
        assert_eq!(resolved2, FetchStrategy::Serial);
    }

    #[test]
    fn empty_plan_and_env_returns_empty() {
        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        let bazel = write_fake_bazel(&dir, &argv_file, 0, None);
        let results = perform_unified_warmup(&ws, &bazel, &ob, &[], &[])
            .expect("empty must succeed");
        assert!(results.is_empty());
    }

    #[test]
    fn resolve_runtime_strategy_auto_picks_combined_when_supported() {
        let cap = CombinedCapability::Supported;
        assert_eq!(
            resolve_runtime_strategy(FetchStrategy::Auto, &cap, false),
            FetchStrategy::Combined
        );
        let not_cap = CombinedCapability::NotSupported;
        assert_eq!(
            resolve_runtime_strategy(FetchStrategy::Auto, &not_cap, false),
            FetchStrategy::Serial
        );
    }

    // =====================================================
    // Phase 5.C.4 commit 164 (Batch U): best-effort
    // warmup failure policy tests. Names per Pro's
    // directive. is_warmup_strict() parses env;
    // warmup_report_failures() reads the report; the
    // orchestrator combines them to decide
    // abort-vs-continue. Tests cover the lower-level
    // primitives so they stay hermetic (no
    // BuildOrchestrator construction).
    // =====================================================

    #[test]
    fn warmup_failure_non_strict_falls_back_to_normal_analyze() {
        // Default mode (RUSH_FETCH_WARMUP_STRICT unset):
        //   - perform_unified_warmup() returns Ok with
        //     a report where failing repos have ok=false.
        //   - WARMED_UP_REPOS only contains succeeded
        //     repos, so the per-repo fetch hook
        //     (commit 143) will treat failing repos
        //     as not-warmed and re-fetch them via the
        //     normal analyze path.
        let prior_strategy = std::env::var("RUSH_FETCH_STRATEGY").ok();
        let prior_strict = std::env::var("RUSH_FETCH_WARMUP_STRICT").ok();
        std::env::remove_var("RUSH_FETCH_STRATEGY");
        std::env::remove_var("RUSH_FETCH_WARMUP_STRICT");

        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        // exit_code=1 → every actual sync invocation
        // fails; help sync still returns 0.
        let bazel = write_fake_bazel(&dir, &argv_file, 1, None);

        let plan = vec!["a".to_string(), "b".to_string()];
        let env: Vec<String> = vec![];
        let results =
            perform_unified_warmup(&ws, &bazel, &ob, &plan, &env)
                .expect(
                    "non-strict warmup must return Ok even on per-repo failure",
                );
        // Both repos in the report, both failed.
        assert_eq!(results.len(), 2);
        for (_, _, ok) in &results.repos {
            assert!(!*ok, "with failing bazel every repo must be ok=false");
        }
        // Non-strict mode should NOT panic / Err.
        assert!(!is_warmup_strict());
        let failed = warmup_report_failures(&results.repos);
        assert_eq!(failed, vec!["a".to_string(), "b".to_string()]);

        match prior_strategy {
            Some(v) => std::env::set_var("RUSH_FETCH_STRATEGY", v),
            None => std::env::remove_var("RUSH_FETCH_STRATEGY"),
        }
        match prior_strict {
            Some(v) => std::env::set_var("RUSH_FETCH_WARMUP_STRICT", v),
            None => std::env::remove_var("RUSH_FETCH_WARMUP_STRICT"),
        }
    }

    #[test]
    fn warmup_failure_strict_fails() {
        // Strict mode: warmup_report_failures() returns
        // non-empty AND is_warmup_strict() is true →
        // orchestrator returns Err. We can't drive the
        // orchestrator here without a real
        // BuildOrchestrator, but we CAN verify the two
        // primitives the orchestrator consults.
        let prior_strict = std::env::var("RUSH_FETCH_WARMUP_STRICT").ok();
        std::env::set_var("RUSH_FETCH_WARMUP_STRICT", "1");
        assert!(
            is_warmup_strict(),
            "RUSH_FETCH_WARMUP_STRICT=1 must enable strict mode"
        );

        // Other accepted values.
        std::env::set_var("RUSH_FETCH_WARMUP_STRICT", "true");
        assert!(is_warmup_strict());
        std::env::set_var("RUSH_FETCH_WARMUP_STRICT", "YES");
        assert!(is_warmup_strict(), "value parsing is case-insensitive");
        std::env::set_var("RUSH_FETCH_WARMUP_STRICT", "on");
        assert!(is_warmup_strict());

        // Unrecognized / off values disable strict.
        std::env::set_var("RUSH_FETCH_WARMUP_STRICT", "0");
        assert!(!is_warmup_strict());
        std::env::set_var("RUSH_FETCH_WARMUP_STRICT", "");
        assert!(!is_warmup_strict());
        std::env::remove_var("RUSH_FETCH_WARMUP_STRICT");
        assert!(!is_warmup_strict(), "unset must default to non-strict");

        // Now combine: strict ON + a report with
        // failures = the abort condition.
        std::env::set_var("RUSH_FETCH_WARMUP_STRICT", "1");
        let fake_report = vec![
            ("good".to_string(), WarmupRepoSource::PlanOnly, true),
            ("bad".to_string(), WarmupRepoSource::EnvOnly, false),
        ];
        let failed = warmup_report_failures(&fake_report);
        assert_eq!(failed, vec!["bad".to_string()]);
        assert!(
            is_warmup_strict() && !failed.is_empty(),
            "strict + failures = abort condition"
        );

        match prior_strict {
            Some(v) => std::env::set_var("RUSH_FETCH_WARMUP_STRICT", v),
            None => std::env::remove_var("RUSH_FETCH_WARMUP_STRICT"),
        }
    }

    #[test]
    fn stale_repo_in_plan_does_not_break_default_run() {
        // Plan-driven warmup with a repo that's stale
        // (e.g. WORKSPACE removed it, bazel sync
        // errors out for it). Non-strict default
        // must:
        //   - return Ok
        //   - mark the stale repo as failed in the report
        //   - NOT add it to WARMED_UP_REPOS (so the
        //     per-repo serial path during analyze can
        //     re-fetch / discover the rule is gone)
        let prior_strict = std::env::var("RUSH_FETCH_WARMUP_STRICT").ok();
        std::env::remove_var("RUSH_FETCH_WARMUP_STRICT");

        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        let bazel = write_fake_bazel(&dir, &argv_file, 1, None);

        let plan = vec!["stale_repo".to_string()];
        let env: Vec<String> = vec![];
        let results =
            perform_unified_warmup(&ws, &bazel, &ob, &plan, &env)
                .expect(
                    "stale repo in plan must NOT propagate as Err in default mode",
                );
        assert_eq!(results.len(), 1);
        let (name, source, ok) = &results.repos[0];
        assert_eq!(name, "stale_repo");
        assert_eq!(*source, WarmupRepoSource::PlanOnly);
        assert!(!*ok, "stale repo must be marked failed");

        // WARMED_UP_REPOS must NOT contain the failed
        // repo. Other parallel tests may share this
        // OnceLock, so we just assert this specific
        // failed repo is absent.
        let lock = WARMED_UP_REPOS.get_or_init(|| {
            std::sync::Mutex::new(std::collections::HashMap::new())
        });
        let map = lock.lock().expect("WARMED_UP_REPOS lock");
        assert!(
            !map.contains_key("stale_repo"),
            "WARMED_UP must NOT contain a repo that failed to fetch"
        );
        drop(map);

        match prior_strict {
            Some(v) => std::env::set_var("RUSH_FETCH_WARMUP_STRICT", v),
            None => std::env::remove_var("RUSH_FETCH_WARMUP_STRICT"),
        }
    }

    #[test]
    fn failure_report_is_deterministic() {
        // Pro: "deterministic" so operators can rely
        // on consistent error messages across runs.
        // perform_unified_warmup sorts its output by
        // repo name (commit 161 invariant);
        // warmup_report_failures preserves that order
        // via filter (filter is order-preserving on
        // Vec). Run the same workload twice and
        // assert identical ordering.
        let prior_strict = std::env::var("RUSH_FETCH_WARMUP_STRICT").ok();
        std::env::remove_var("RUSH_FETCH_WARMUP_STRICT");

        let (dir, ws, ob) = make_tmp_workspace_and_output_base();
        let argv_file = dir.path().join("argv.log");
        let bazel = write_fake_bazel(&dir, &argv_file, 1, None);

        // Interleave plan and env repos so the input
        // order is NOT already alpha-sorted: only the
        // function's internal sort produces the
        // alpha-sorted output.
        let plan = vec!["zeta".to_string(), "alpha".to_string()];
        let env = vec!["mu".to_string(), "beta".to_string()];

        let r1 = perform_unified_warmup(&ws, &bazel, &ob, &plan, &env)
            .expect("first run must succeed in non-strict mode");
        let r2 = perform_unified_warmup(&ws, &bazel, &ob, &plan, &env)
            .expect("second run must succeed in non-strict mode");

        // Order of (name, source, ok) triples must be
        // identical across runs.
        let names1: Vec<&str> =
            r1.iter().map(|(n, _, _)| n.as_str()).collect();
        let names2: Vec<&str> =
            r2.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(
            names1, names2,
            "report ordering must be deterministic across runs"
        );
        // And specifically alpha-sorted.
        assert_eq!(
            names1,
            vec!["alpha", "beta", "mu", "zeta"],
            "report must be alpha-sorted: {:?}",
            names1
        );

        // warmup_report_failures preserves that order.
        let f1 = warmup_report_failures(&r1.repos);
        let f2 = warmup_report_failures(&r2.repos);
        assert_eq!(f1, f2, "failures Vec must be deterministic");
        assert_eq!(
            f1,
            vec![
                "alpha".to_string(),
                "beta".to_string(),
                "mu".to_string(),
                "zeta".to_string(),
            ]
        );

        match prior_strict {
            Some(v) => std::env::set_var("RUSH_FETCH_WARMUP_STRICT", v),
            None => std::env::remove_var("RUSH_FETCH_WARMUP_STRICT"),
        }
    }
}
