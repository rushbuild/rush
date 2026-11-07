//! Phase 5.C.4 commit 32: ActionKeySpec dry-run data model.
//!
//! ChatGPT Pro green-lit this as a dry-run-only sketch: the
//! ActionKeySpec shape exists, NormalizedCommandSpec converts
//! into it, host-ambient inputs (e.g. `-I/usr/include`,
//! `/usr/bin/grpc_cpp_plugin`) get explicitly classified as
//! non-hermetic — but NO cache reads, NO cache writes, NO
//! AC/CAS implementation, NO remote cache.
//!
//! Pro's commit-32 directive (verbatim):
//!
//! ```text
//! Allowed:
//!   ActionKeySpec data model
//!   conversion from verified scheduler actions to ActionKeySpec
//!   action-key explain / dump
//!   non-hermetic classification
//!   no cache reads
//!   no cache writes
//!   no AC/CAS implementation
//!   no remote cache
//!
//! Not allowed:
//!   using ActionKeySpec for reuse
//!   claiming cache correctness
//!   hashing host-ambient paths without classification
//! ```
//!
//! Why dry-run before 5.C.5: cache key correctness depends on
//! the action graph being the source of truth. Today the
//! action-graph + verifier-verified rules are stable enough to
//! sketch what an action key WOULD include without committing
//! to keys yet. When real cache work starts (post-5.C.5,
//! per Pro's earlier guidance), this module's classification
//! and shape inform what becomes part of the cache key vs
//! what gets marked `cacheable_local_only` / `non_hermetic` /
//! `requires_host_sysroot_fingerprint`.
//!
//! Pro's "build a fast system for reusing wrong answers"
//! warning still applies — this module is deliberately
//! diagnostic. Calling it "the cache" would be premature.

use crate::command_spec::NormalizedCommandSpec;
use std::path::{Path, PathBuf};

/// Phase 5.C.4 commit 32: hermeticity classification for one
/// input or tool reference inside an `ActionKeySpec`.
///
/// The classification decides how (or whether) the path
/// participates in cache keys at the future-cache stage. At
/// the dry-run stage today, it just informs the explain/dump
/// output so users and CI can SEE which inputs would be
/// hermetic-cacheable vs which would force `local_only` / fail
/// hermetic verification.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HermeticityClass {
    /// Workspace-local source artifact (e.g. `<workspace>/foo.cc`)
    /// or generated artifact (e.g. `<output_dir>/foo.o`). Fully
    /// participates in cache keys; remote-cacheable in principle.
    WorkspaceLocal,
    /// Host-system include path. Examples: `/usr/include`,
    /// `/usr/include/jsoncpp`. Non-hermetic — content varies by
    /// host distro/version. Future cache: would require an
    /// explicit host-sysroot fingerprint OR demotion to
    /// `cacheable_local_only`.
    HostAmbientInclude,
    /// Host-installed tool path. Examples:
    /// `/usr/bin/grpc_cpp_plugin`, `/usr/bin/protoc`,
    /// `/usr/bin/ar`. Non-hermetic — version varies by host.
    /// Future cache: would require typed `ToolchainInfo` with
    /// version pinning OR `cacheable_local_only` demotion.
    HostToolPath,
    /// User-supplied tool path via env var override (e.g.
    /// `RUSH_GRPC_CPP_PLUGIN`). Non-hermetic by definition —
    /// the user explicitly opted into a non-standard tool
    /// location. Future cache: same handling as `HostToolPath`.
    EnvOverrideToolPath,
    /// Path doesn't fall into any classified category.
    /// Conservatively treated as non-hermetic for cache-key
    /// purposes. Catch-all for unrecognized paths so the
    /// classifier stays honest.
    Unclassified,
}

impl HermeticityClass {
    /// Phase 5.C.4 commit 32 / 35 / 58: cacheability class
    /// this hermeticity class warrants in isolation.
    ///
    /// Returns the bare [`CacheabilityClass`] (not the full
    /// [`Cacheability`] struct) — the action-level
    /// aggregation in [`ActionKeySpec::cacheability`]
    /// constructs the struct with accumulated reasons.
    pub fn cacheability_class(&self) -> CacheabilityClass {
        match self {
            HermeticityClass::WorkspaceLocal => CacheabilityClass::RemoteCacheable,
            HermeticityClass::HostAmbientInclude => {
                CacheabilityClass::LocalHostCacheable
            }
            HermeticityClass::HostToolPath => CacheabilityClass::LocalHostCacheable,
            HermeticityClass::EnvOverrideToolPath => {
                CacheabilityClass::LocalHostCacheable
            }
            HermeticityClass::Unclassified => CacheabilityClass::NonCacheable,
        }
    }

    /// Phase 5.C.4 commit 58: which [`CacheabilityReason`]
    /// (if any) does this class contribute when it appears
    /// on an input or executable? Returned alongside the
    /// class by [`ActionKeySpec::cacheability`].
    ///
    /// The mapping:
    ///   WorkspaceLocal       → no reason (it's the safe case)
    ///   HostAmbientInclude   → Some(HostAmbientInclude)
    ///   HostToolPath         → Some(HostToolPath)
    ///   EnvOverrideToolPath  → Some(EnvOverrideToolPath)
    ///   Unclassified         → Some(UnclassifiedInput) by
    ///     default; callers passing the class in an
    ///     executable context can map to UnclassifiedExecutable
    ///     instead.
    pub fn cacheability_reason(&self) -> Option<CacheabilityReason> {
        match self {
            HermeticityClass::WorkspaceLocal => None,
            HermeticityClass::HostAmbientInclude => {
                Some(CacheabilityReason::HostAmbientInclude)
            }
            HermeticityClass::HostToolPath => Some(CacheabilityReason::HostToolPath),
            HermeticityClass::EnvOverrideToolPath => {
                Some(CacheabilityReason::EnvOverrideToolPath)
            }
            HermeticityClass::Unclassified => {
                Some(CacheabilityReason::UnclassifiedInput)
            }
        }
    }

    /// Phase 5.C.4 commit 32: would this class participate in
    /// remote cache keys today (if cache existed)?
    ///
    /// Phase 5.C.4 commit 35 / 58: kept as a thin wrapper.
    /// Returns true iff `cacheability_class() == RemoteCacheable`.
    pub fn is_remote_cacheable(&self) -> bool {
        matches!(self.cacheability_class(), CacheabilityClass::RemoteCacheable)
    }

    /// Phase 5.C.4 commit 32: short label for diagnostic
    /// output. Used by `format_action_key_spec`.
    pub fn label(&self) -> &'static str {
        match self {
            HermeticityClass::WorkspaceLocal => "WorkspaceLocal",
            HermeticityClass::HostAmbientInclude => "HostAmbientInclude",
            HermeticityClass::HostToolPath => "HostToolPath",
            HermeticityClass::EnvOverrideToolPath => "EnvOverrideToolPath",
            HermeticityClass::Unclassified => "Unclassified",
        }
    }
}

/// Phase 5.C.4 commit 35: 3-class cacheability tier for
/// actions. Pro's directive: "is_remote_cacheable() as a
/// boolean will become too crude. Use:
///
/// ```rust
/// pub enum Cacheability {
///     RemoteCacheable,
///     LocalHostCacheable,
///     NonCacheable,
/// }
/// ```
///
/// This avoids a false dichotomy. Some actions are not
/// remote-cache safe but may still be safe for a local disk
/// cache if the host state has not changed. Bazel can get
/// away with a lot because it has toolchain and sandboxing
/// machinery. Rush is still growing those teeth, so
/// classify honestly."
///
/// At the dry-run stage today, this enum is diagnostic-only.
/// Real cache work (post-5.C.5) reads it for routing:
///   - `RemoteCacheable` → AC + remote upload candidate
///   - `LocalHostCacheable` → local disk cache only;
///     never uploaded to remote AC
///   - `NonCacheable` → never cached; re-execute every time
///
/// Pro's "fast system for reusing wrong answers" warning
/// still applies. This commit defines the shape; the routing
/// policy lives in future cache work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheabilityClass {
    /// Hermetic across hosts. Content of inputs is the only
    /// variable; cache key is portable. Today: only
    /// `WorkspaceLocal` paths qualify.
    RemoteCacheable,
    /// Cacheable on the SAME host (host state assumed stable
    /// across builds), but not safe to share across hosts.
    /// Host-ambient includes, host-installed tools, env-
    /// override tool paths all land here.
    LocalHostCacheable,
    /// Not safe to cache. Unclassified paths land here
    /// conservatively — when we don't know what something is,
    /// we can't say anything about its cache safety.
    NonCacheable,
}

impl CacheabilityClass {
    /// Phase 5.C.4 commit 35 / 58: combine two cacheability
    /// classes — return the WORST of the two (lowest cache
    /// safety).
    ///
    /// Lattice (worst → best):
    ///   NonCacheable < LocalHostCacheable < RemoteCacheable
    pub fn combine(self, other: CacheabilityClass) -> CacheabilityClass {
        use CacheabilityClass::*;
        match (self, other) {
            (NonCacheable, _) | (_, NonCacheable) => NonCacheable,
            (LocalHostCacheable, _) | (_, LocalHostCacheable) => LocalHostCacheable,
            (RemoteCacheable, RemoteCacheable) => RemoteCacheable,
        }
    }

    /// Phase 5.C.4 commit 35 / 58: short label for diagnostic
    /// output.
    pub fn label(&self) -> &'static str {
        match self {
            CacheabilityClass::RemoteCacheable => "RemoteCacheable",
            CacheabilityClass::LocalHostCacheable => "LocalHostCacheable",
            CacheabilityClass::NonCacheable => "NonCacheable",
        }
    }
}

/// Phase 5.C.4 commit 58: structured reason explaining why
/// an action's cacheability ended up where it is.
///
/// Pro's directive: "Replace bool-style cacheability with
/// `Cacheability { class, reasons }`." Reasons accumulate
/// across an action's classified inputs / executable / env
/// / outputs, so a single dump tells operators EVERY
/// host-ambient-include and EVERY env-override tool that
/// blocks remote-cache safety, not just one.
///
/// JSON shape (commit 54b style):
///   {"kind": "host_ambient_include", "data": null}
///   {"kind": "host_path_env", "data": null}
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum CacheabilityReason {
    /// An input path classified as `HostAmbientInclude`
    /// (e.g. `/usr/include`). Demotes to LocalHostCacheable.
    HostAmbientInclude,
    /// An input or executable path classified as
    /// `HostToolPath` (e.g. `/usr/bin/protoc`). Demotes to
    /// LocalHostCacheable.
    HostToolPath,
    /// A path matched against an env-override list (e.g.
    /// `RUSH_GRPC_CPP_PLUGIN`). Demotes to LocalHostCacheable.
    EnvOverrideToolPath,
    /// An input path the classifier returned `Unclassified`
    /// for. Demotes to NonCacheable — we don't know what it
    /// is.
    UnclassifiedInput,
    /// An output path the classifier returned
    /// `OutputPathPolicy::Unclassified` for. Forces
    /// NonCacheable.
    UnclassifiedOutput,
    /// The executable path classified as `Unclassified`.
    /// Distinct from `UnclassifiedInput` because tools and
    /// inputs have different recovery stories (toolchain
    /// identity vs source-file content). Forces
    /// NonCacheable.
    UnclassifiedExecutable,
    /// An output is non-normalizable (commit 41's
    /// `OutputPathPolicy::Unclassified` in the post-rewrite
    /// sense). Future cache work: blocks remote cache key
    /// stability across hosts. Forces NonCacheable today.
    NonNormalizableOutput,
    /// The action's env carries an unclassified env var —
    /// we don't have an allowlist entry telling us this var
    /// is safe to ignore for cache purposes. Forces
    /// NonCacheable conservatively.
    UnclassifiedEnv,
    /// The action's env carries a host-path-sensitive env
    /// var (e.g. `PATH`, `LD_LIBRARY_PATH`, `HOME`,
    /// `TMPDIR`, `CC`, `CXX`). Demotes to LocalHostCacheable.
    HostPathEnv,
    /// Phase 5.C.4 commit 66: the action's
    /// [`crate::ids::ActionEnvPolicy`] is `InheritHost`,
    /// meaning the runner spawns with the parent process's
    /// full env inherited (plus declared overlay). Forces
    /// NonCacheable: an unbounded inherited env can't be
    /// fingerprinted into a cache key, so even
    /// LocalHostCacheable would be lying about
    /// reproducibility.
    ///
    /// Pro's directive: "If `ActionKeySpec.env` says `[]`
    /// but `tokio::process::Command` inherits the host
    /// environment, then the diagnostic model is lying. A
    /// cache model that forgets the action's real
    /// environment is not a cache model, it is a cheerful
    /// fraud with serialization."
    InheritedHostEnv,
}

impl CacheabilityReason {
    /// Phase 5.C.4 commit 58: which [`CacheabilityClass`]
    /// does this reason demote toward? Used by
    /// [`Cacheability::add_reason`] to update the class
    /// alongside accumulating the reason.
    pub fn class(&self) -> CacheabilityClass {
        use CacheabilityReason::*;
        match self {
            HostAmbientInclude
            | HostToolPath
            | EnvOverrideToolPath
            | HostPathEnv => CacheabilityClass::LocalHostCacheable,
            UnclassifiedInput
            | UnclassifiedOutput
            | UnclassifiedExecutable
            | NonNormalizableOutput
            | UnclassifiedEnv
            | InheritedHostEnv => CacheabilityClass::NonCacheable,
        }
    }
}

/// Phase 5.C.4 commit 72: counted summary of a
/// [`CacheabilityReason`].
///
/// Pairs a reason variant with the number of times it
/// fired during cacheability aggregation. Used by human +
/// JSON output to collapse "HostToolPath × 12" spam into
/// one line while preserving the count.
///
/// Pro's commit-72 directive: "Internal model should
/// preserve enough information to count repetition, but
/// human output should not spam."
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct CacheabilityReasonSummary {
    /// The reason variant.
    pub reason: CacheabilityReason,
    /// Number of times this reason fired (>= 1).
    pub count: usize,
}

/// Phase 5.C.4 commit 58 / 72: structured cacheability with
/// reasons.
///
/// Pro's directive: "Do not hash. Do not cache. Just report
/// honestly." Today this is purely diagnostic — it tells
/// callers + future cache code WHY an action ended up
/// non-remote-cacheable, not just THAT it did. Operators
/// reading the explain dump see exactly which input or env
/// var blocked remote-cache safety.
///
/// `class` is the worst-of-reasons aggregation. Adding a
/// new reason via `add_reason` updates `class` to the
/// stricter of the current class and the reason's
/// implied class.
///
/// Commit 72: `reasons` (the raw Vec) is preserved for
/// internal callers that want per-instance accounting; new
/// callers should prefer [`Cacheability::summarized_reasons`]
/// for human/JSON output so duplicates collapse.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct Cacheability {
    /// Aggregated class (worst across all reasons).
    pub class: CacheabilityClass,
    /// All reasons that contributed to non-remote-cacheable
    /// state. Empty when `class == RemoteCacheable`.
    /// Order: deterministic per
    /// [`ActionKeySpec::cacheability`] which iterates
    /// executable → declared_inputs → env → declared_outputs.
    ///
    /// Raw repetitions preserved here; commit 72's
    /// [`Cacheability::summarized_reasons`] collapses for
    /// display.
    pub reasons: Vec<CacheabilityReason>,
}

impl Cacheability {
    /// Phase 5.C.4 commit 58: start with the safe case
    /// (RemoteCacheable, no reasons).
    pub fn safe() -> Self {
        Self {
            class: CacheabilityClass::RemoteCacheable,
            reasons: Vec::new(),
        }
    }

    /// Phase 5.C.4 commit 58: add a reason and update class
    /// to the stricter (worse) of the current class and the
    /// reason's implied class.
    ///
    /// Reasons accumulate in order; duplicates are
    /// preserved (so operators see "HostAmbientInclude × 12
    /// inputs" via the count).
    pub fn add_reason(&mut self, reason: CacheabilityReason) {
        self.class = self.class.combine(reason.class());
        self.reasons.push(reason);
    }

    /// Phase 5.C.4 commit 58: convenience for
    /// `is_remote_cacheable` callers.
    pub fn is_remote_cacheable(&self) -> bool {
        matches!(self.class, CacheabilityClass::RemoteCacheable)
    }

    /// Phase 5.C.4 commit 72: collapse duplicate reasons
    /// into counted summary entries.
    ///
    /// Iterates `self.reasons` in order, accumulating
    /// counts per variant. The resulting `Vec` is ordered
    /// by first-occurrence (matches the original reason
    /// stream's deterministic walk:
    /// executable → declared_inputs → env →
    /// declared_outputs).
    ///
    /// Returns an empty Vec when `reasons` is empty (which
    /// is the `RemoteCacheable` case).
    ///
    /// Pro's commit-72 contract: "HostToolPath × 12 /
    /// HostAmbientInclude × 3 / InheritedHostEnv × 1" —
    /// one line per distinct reason variant.
    pub fn summarized_reasons(&self) -> Vec<CacheabilityReasonSummary> {
        let mut summary: Vec<CacheabilityReasonSummary> = Vec::new();
        for r in &self.reasons {
            if let Some(existing) =
                summary.iter_mut().find(|s| &s.reason == r)
            {
                existing.count += 1;
            } else {
                summary.push(CacheabilityReasonSummary {
                    reason: r.clone(),
                    count: 1,
                });
            }
        }
        summary
    }

    /// Phase 5.C.4 commit 35 / 58: short label that mirrors
    /// the class label (used by format_action_key_spec
    /// inline). Reasons render separately.
    pub fn label(&self) -> &'static str {
        self.class.label()
    }
}

/// Phase 5.C.4 commit 58: classify an env var by its key
/// for cacheability purposes.
///
/// Returns `None` when the var is on the safe-allowlist
/// (today: empty — Pro's "False caution is better than
/// reusing poisoned cache outputs"). Returns
/// `Some(HostPathEnv)` for known host-path-sensitive vars
/// (PATH, LD_LIBRARY_PATH, HOME, TMPDIR, CC, CXX, AR, LD,
/// CFLAGS, CXXFLAGS, LDFLAGS, PKG_CONFIG_PATH). Returns
/// `Some(EnvOverrideToolPath)` for env vars matching the
/// `RUSH_*_PLUGIN` / `RUSH_*_PATH` / `RUSH_*_OVERRIDE`
/// patterns. Returns `Some(UnclassifiedEnv)` for everything
/// else (we don't have data telling us the var is cache-
/// safe).
pub fn classify_env_var(key: &str) -> Option<CacheabilityReason> {
    // Pro's directive: start strict. Future commit can grow
    // an explicit safe-allowlist (e.g. LANG, LC_*) once the
    // cacheability story is firmer.
    const HOST_PATH_KEYS: &[&str] = &[
        "PATH",
        "LD_LIBRARY_PATH",
        "LIBRARY_PATH",
        "HOME",
        "TMPDIR",
        "TEMP",
        "TMP",
        "CC",
        "CXX",
        "AR",
        "LD",
        "CFLAGS",
        "CXXFLAGS",
        "LDFLAGS",
        "PKG_CONFIG_PATH",
    ];
    if HOST_PATH_KEYS.contains(&key) {
        return Some(CacheabilityReason::HostPathEnv);
    }
    if key.starts_with("RUSH_")
        && (key.ends_with("_PLUGIN")
            || key.ends_with("_PATH")
            || key.ends_with("_OVERRIDE"))
    {
        return Some(CacheabilityReason::EnvOverrideToolPath);
    }
    Some(CacheabilityReason::UnclassifiedEnv)
}

/// Phase 5.C.4 commit 32: classify a path for hermeticity.
///
/// `workspace_root` is the rush workspace root absolute path.
/// Paths under it (or under the build's `output_dir`) are
/// `WorkspaceLocal`. Specific host-system locations get
/// dedicated classes. Unknown paths fall to `Unclassified`.
///
/// `env_override_paths` is a list of paths declared via env
/// vars (e.g. `RUSH_GRPC_CPP_PLUGIN`). Matching against this
/// list classifies the path as `EnvOverrideToolPath` —
/// distinct from `HostToolPath` because the user explicitly
/// opted into the override.
pub fn classify_path(
    path: &Path,
    workspace_root: &Path,
    env_override_paths: &[PathBuf],
) -> HermeticityClass {
    // Env override match takes precedence — the user
    // explicitly named this path via RUSH_GRPC_CPP_PLUGIN or
    // similar. Want this classification BEFORE the workspace
    // check so a "rush in /home/user/.cache/..." workspace
    // doesn't accidentally absorb env-override tools.
    for ovr in env_override_paths {
        if ovr == path {
            return HermeticityClass::EnvOverrideToolPath;
        }
    }
    // Workspace-local: under the workspace root. Includes
    // both source files and build outputs (rush-out/ lives
    // under the workspace).
    if path.starts_with(workspace_root) {
        return HermeticityClass::WorkspaceLocal;
    }
    // Host system locations.
    let path_str = path.to_string_lossy();
    if path_str.starts_with("/usr/include")
        || path_str.starts_with("/usr/local/include")
    {
        return HermeticityClass::HostAmbientInclude;
    }
    if path_str.starts_with("/usr/bin/")
        || path_str.starts_with("/usr/local/bin/")
        || path_str.starts_with("/bin/")
    {
        return HermeticityClass::HostToolPath;
    }
    // Bare tool names like "protoc" (no leading slash) —
    // PATH-resolved at exec time. Treat as host tool — the
    // resolution happens against $PATH which is host-ambient.
    if !path.is_absolute() {
        let parent = path.parent();
        if parent.is_none() || parent == Some(Path::new("")) {
            return HermeticityClass::HostToolPath;
        }
    }
    HermeticityClass::Unclassified
}

/// Phase 5.C.4 commit 32: classified input record. Pairs a
/// path with its hermeticity classification.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ClassifiedInput {
    pub path: PathBuf,
    pub class: HermeticityClass,
}

/// Phase 5.C.4 commit 57: tool reference (executable) with
/// hermeticity classification.
///
/// Pro's directive: "A compiler or plugin is a tool, not an
/// input in the provider/action sense. Rename it."
///
/// Same fields as [`ClassifiedInput`] (path +
/// HermeticityClass) but the type name makes the contract
/// explicit at every use site. ActionKeySpec.executable is
/// a ClassifiedTool; declared_inputs are ClassifiedInputs.
/// Future cacheability code can reason about tool
/// classification distinctly from input classification
/// (e.g. "remote-cacheable only if BOTH inputs are
/// workspace-local AND tool is workspace-local or
/// hermetic-toolchain-classified").
///
/// The split is conceptual rather than mechanical — both
/// types serialize identically (path + class). Future
/// commits can extend either side independently
/// (e.g. ClassifiedTool gaining a digest or version field
/// without ClassifiedInput needing the same).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ClassifiedTool {
    /// Tool path (resolved at action-key construction time;
    /// may be relative when the tool is `$PATH`-resolved at
    /// exec time, absolute when an env override or
    /// distro-fallback resolved it).
    pub path: PathBuf,
    /// Hermeticity classification — the tool's path is
    /// classified the same way as inputs (the
    /// `classify_path` function is shared).
    pub class: HermeticityClass,
}

/// Phase 5.C.4 commit 41: output-path policy classification.
///
/// Pro's pivot directive #10: "Cache-key stability across hosts
/// requires output paths to normalize the same way regardless
/// of who's building. Today's `declared_outputs: Vec<PathBuf>`
/// in `ActionKeySpec` carries host-rooted absolute paths
/// (e.g. `/home/userA/workspace/rush-out/foo.o`). Two hosts
/// running the same build produce different cache keys for
/// 'identical' actions because the output paths differ in
/// their /home/ prefix. The fix is a typed policy that
/// describes how each output path should be normalized for
/// cache-key purposes — mirror to `HermeticityClass` for
/// inputs, but on the output side."
///
/// Today's commit ships the data model + classifier + tests
/// per Pro's "no cache reuse, no AC/CAS" guard from commit 32.
/// Future cache work consumes the policy to:
///   - rewrite WorkspaceRelativeOutput paths to
///     `<rush-out>/<package>/<file>` form (no /home/ prefix)
///   - rewrite GenfilesOutput paths to
///     `genfiles/<package>/<file>` form
///   - rewrite ExternalRepoOutput paths to
///     `external/<repo>/<package>/<file>` form
///   - reject Unclassified outputs from remote cache
///
/// The classifier is path-shape-only — it doesn't read or
/// rewrite paths today, just classifies them. Rewriting is
/// post-5.C.5 cache work.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum OutputPathPolicy {
    /// Under `<workspace>/rush-out/<package>/<file>` (or any
    /// other workspace-relative output dir). The most common
    /// shape; cache-key normalization rewrites the path to
    /// `rush-out/<package>/<file>` form.
    WorkspaceRelativeOutput,
    /// Under `<workspace>/rush-out/genfiles/<package>/<file>` —
    /// generated source/header outputs from protoc, tblgen,
    /// etc. Subset of WorkspaceRelativeOutput; tracked
    /// separately because genfiles are inputs to downstream
    /// compile actions and their normalization matters for
    /// the consumer's cache key.
    GenfilesOutput,
    /// External repo output (e.g.
    /// `<workspace>/external/com_google_absl/absl/foo.o`).
    /// Normalization rewrites to
    /// `external/<repo>/<package>/<file>`.
    ExternalRepoOutput {
        /// Repo name (e.g. "com_google_absl").
        repo: String,
    },
    /// Path didn't match any known output shape — cache key
    /// MUST treat this action as non-cacheable. Diagnostic
    /// only at the dry-run stage.
    Unclassified,
}

impl OutputPathPolicy {
    /// Phase 5.C.4 commit 41: human-readable label for the
    /// policy variant. Used by `format_action_key_spec` /
    /// future `--action-key-explain` output.
    pub fn label(&self) -> String {
        match self {
            OutputPathPolicy::WorkspaceRelativeOutput => {
                "WorkspaceRelativeOutput".to_string()
            }
            OutputPathPolicy::GenfilesOutput => "GenfilesOutput".to_string(),
            OutputPathPolicy::ExternalRepoOutput { repo } => {
                format!("ExternalRepoOutput({})", repo)
            }
            OutputPathPolicy::Unclassified => "Unclassified".to_string(),
        }
    }

    /// Phase 5.C.4 commit 41: would this output participate
    /// in a stable cache key across hosts?
    ///
    /// Returns true for WorkspaceRelative + Genfiles + External
    /// (all have a known normalization rewrite). Returns false
    /// for Unclassified — those force non-cacheability of the
    /// owning action.
    pub fn is_normalizable(&self) -> bool {
        !matches!(self, OutputPathPolicy::Unclassified)
    }
}

/// Phase 5.C.4 commit 41: classify an output path by its
/// shape relative to workspace + external-repo roots.
///
/// Order of checks (most-specific first):
///   1. `<workspace>/rush-out/genfiles/...` → GenfilesOutput
///      (sub-shape of WorkspaceRelativeOutput; precedence
///      because the consumer-side cache-key impact differs)
///   2. `<workspace>/external/<repo>/...` → ExternalRepoOutput
///   3. `<external_repo_paths[r]>/...` → ExternalRepoOutput(r)
///      (when the repo is laid out at a non-`<workspace>/external`
///      path, e.g. a sibling directory used by some toolchain
///      managers)
///   4. `<workspace>/...` → WorkspaceRelativeOutput
///   5. anything else → Unclassified
///
/// The genfiles directory name is hardcoded today
/// (`rush-out/genfiles`); a future commit can lift this to a
/// configurable build-out layout if needed. For now Rush's
/// output layout is fixed.
pub fn classify_output_path(
    path: &Path,
    workspace_root: &Path,
    external_repo_paths: &std::collections::HashMap<String, PathBuf>,
) -> OutputPathPolicy {
    // 1. Genfiles (most-specific check; comes before plain
    // workspace-relative match).
    let genfiles_root = workspace_root.join("rush-out").join("genfiles");
    if path.starts_with(&genfiles_root) {
        return OutputPathPolicy::GenfilesOutput;
    }
    // 2. External repo under <workspace>/external/<repo>/...
    let external_root = workspace_root.join("external");
    if let Ok(rel) = path.strip_prefix(&external_root) {
        if let Some(first) = rel.components().next() {
            if let std::path::Component::Normal(name) = first {
                return OutputPathPolicy::ExternalRepoOutput {
                    repo: name.to_string_lossy().into_owned(),
                };
            }
        }
    }
    // 3. External repo at a non-standard location (mapped via
    // external_repo_paths). Iterate stable order to keep
    // classification deterministic when paths overlap (rare,
    // but we want test-reproducible output).
    let mut sorted: Vec<(&String, &PathBuf)> = external_repo_paths.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    for (name, root) in sorted {
        if path.starts_with(root) {
            return OutputPathPolicy::ExternalRepoOutput {
                repo: name.clone(),
            };
        }
    }
    // 4. Workspace-relative (any path under workspace_root not
    // already classified above).
    if path.starts_with(workspace_root) {
        return OutputPathPolicy::WorkspaceRelativeOutput;
    }
    // 5. Anything else.
    OutputPathPolicy::Unclassified
}

/// Phase 5.C.4 commit 41: classified output record. Pairs a
/// path with its OutputPathPolicy classification. Mirror of
/// `ClassifiedInput` on the output side.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ClassifiedOutput {
    pub path: PathBuf,
    pub policy: OutputPathPolicy,
}

/// Phase 5.C.4 commit 32: dry-run ActionKeySpec.
///
/// Captures the conceptual cache-key-bearing inputs of an
/// action without committing to a hash function or to the
/// "this action will be cached" semantics. Pro's strict limits
/// apply: this is diagnostic-only, no cache reuse, no
/// AC/CAS implementation.
///
/// Future cache work (post-5.C.5) consumes this shape:
///   - Hash the WorkspaceLocal inputs by content digest.
///   - Hash the argv + env + cwd (already in this struct).
///   - Mark actions with HostAmbient* inputs as
///     `cacheable_local_only` until a hermetic-toolchain
///     story exists.
///   - Reject Unclassified inputs from remote cache (or fail
///     hermetic verification).
///
/// Today's commit doesn't do any of that — it just makes the
/// Current action-key explain JSON schema version. Bumped on
/// breaking JSON shape changes; CI consumers gate on equality.
pub const ACTION_KEY_EXPLAIN_SCHEMA_VERSION: u32 = 1;

/// Phase 5.C.4 commit 54 / 69: schema-versioned wrapper
/// around `ActionKeySpec` (and optionally the
/// `CanonicalActionKeyInput` + readiness report) for JSON
/// explain output.
///
/// Default shape (commit 54, view=spec):
///
/// ```json
/// {"schema_version": 1, "spec": {<ActionKeySpec...>}}
/// ```
///
/// Extended shape (commit 69, view=both):
///
/// ```json
/// {
///   "schema_version": 1,
///   "target": "//pkg:lib",
///   "action": "CppCompile:foo.o",
///   "spec": {<ActionKeySpec...>},
///   "canonical": {<CanonicalActionKeyInput...>},
///   "readiness": {<CanonicalizationReadiness...>}
/// }
/// ```
///
/// `target` / `action` / `canonical` / `readiness` are
/// optional — fields are omitted when not set, preserving
/// back-compat with consumers that read only the commit-54
/// shape.
///
/// `spec` is also optional in commit 69's wider model
/// (view=canonical omits spec). Schema_version stays at 1
/// because the changes are additive — existing CI gates on
/// schema_version=1 continue to pass; consumers that want
/// the canonical fields opt into reading them.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActionKeyExplainEntry<'a> {
    /// Schema version (currently 1).
    pub schema_version: u32,
    /// Phase 5.C.4 commit 69: optional target label
    /// (`//pkg:lib`). Omitted when callers don't have a
    /// label to attach (commit 54 callers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Phase 5.C.4 commit 69: optional action label
    /// (`mnemonic:primary_output_basename`). Omitted by
    /// callers that don't have one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// The action-key spec body. `None` when
    /// view=canonical (commit 69).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<&'a ActionKeySpec>,
    /// Phase 5.C.4 commit 69: canonical action-key input
    /// shape. `Some` when view includes canonical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical: Option<
        &'a crate::canonical_action_key::CanonicalActionKeyInput,
    >,
    /// Phase 5.C.4 commit 69: canonicalization readiness
    /// report (paired with `canonical`). `Some` when
    /// view includes canonical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness: Option<
        &'a crate::canonical_action_key::CanonicalizationReadiness,
    >,
    /// Phase 5.C.4 commit 84: optional cacheability
    /// payload — class + raw reasons + summary.
    ///
    /// Pro's commit-84 directive: "Add cacheability to
    /// ActionKeyExplainEntry as an optional field. Keep
    /// schema_version = 1 (additive optional JSON field;
    /// existing consumers can ignore it)."
    ///
    /// CI consumers (cf.
    /// `tools/ci/rush_scheduler_gate.sh`) parse this
    /// field directly instead of re-deriving from
    /// `spec.env_policy`. Today the script's env_policy
    /// heuristic is 1:1 with cacheability (every
    /// InheritHost is NonCacheable via
    /// `CacheabilityReason::InheritedHostEnv` from
    /// commit 66), but commit 85 prefers this direct
    /// signal for forward-compat once per-rule
    /// `ExplicitOnly` opt-in lands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cacheability: Option<ActionKeyCacheabilityView>,
}

/// Phase 5.C.4 commit 84: serialized cacheability payload
/// embedded in [`ActionKeyExplainEntry`].
///
/// Shape:
/// ```json
/// {
///   "class": "non_cacheable",
///   "reasons": [{"kind": "inherited_host_env"}, ...],
///   "reason_summary": [
///     {"reason": {"kind": "host_tool_path"}, "count": 12},
///     {"reason": {"kind": "inherited_host_env"}, "count": 1}
///   ]
/// }
/// ```
///
/// `class` uses snake_case (matches the existing
/// `CacheabilityClass` JSON encoding via the label).
/// `reasons` carries the RAW Vec from
/// [`Cacheability::reasons`] for callers that want
/// per-instance detail; `reason_summary` carries the
/// counted summary from
/// [`Cacheability::summarized_reasons`] (commit 72) for
/// human-friendly inspection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ActionKeyCacheabilityView {
    /// Aggregated cacheability class:
    /// `"remote_cacheable"`, `"local_host_cacheable"`, or
    /// `"non_cacheable"`.
    pub class: String,
    /// Raw reasons Vec (preserves per-instance detail).
    /// Empty when `class == "remote_cacheable"`.
    pub reasons: Vec<CacheabilityReason>,
    /// Counted summary (commit 72). Empty when reasons is
    /// empty.
    pub reason_summary: Vec<CacheabilityReasonSummary>,
}

impl ActionKeyCacheabilityView {
    /// Phase 5.C.4 commit 84: build the view from a
    /// `Cacheability` (typically the result of
    /// [`ActionKeySpec::cacheability`]). Pure data; no
    /// I/O.
    pub fn from_cacheability(c: &Cacheability) -> Self {
        // Class label in snake_case — matches the JSON
        // encoding consistency Pro flagged in commit 54b.
        let class = match c.class {
            CacheabilityClass::RemoteCacheable => "remote_cacheable",
            CacheabilityClass::LocalHostCacheable => "local_host_cacheable",
            CacheabilityClass::NonCacheable => "non_cacheable",
        }
        .to_string();
        Self {
            class,
            reasons: c.reasons.clone(),
            reason_summary: c.summarized_reasons(),
        }
    }
}

impl<'a> ActionKeyExplainEntry<'a> {
    /// Phase 5.C.4 commit 54: build a spec-only entry.
    /// Back-compat constructor — produces the same JSON
    /// shape the commit-54 surface always has.
    pub fn new(spec: &'a ActionKeySpec) -> Self {
        Self {
            schema_version: ACTION_KEY_EXPLAIN_SCHEMA_VERSION,
            target: None,
            action: None,
            spec: Some(spec),
            canonical: None,
            readiness: None,
            cacheability: None,
        }
    }

    /// Phase 5.C.4 commit 69: build a spec-only entry
    /// labeled with target + action. Same JSON shape as
    /// [`Self::new`] plus the optional label fields.
    pub fn spec_only_labeled(
        spec: &'a ActionKeySpec,
        target: String,
        action: String,
    ) -> Self {
        Self {
            schema_version: ACTION_KEY_EXPLAIN_SCHEMA_VERSION,
            target: Some(target),
            action: Some(action),
            spec: Some(spec),
            canonical: None,
            readiness: None,
            cacheability: None,
        }
    }

    /// Phase 5.C.4 commit 69: build a canonical-only entry
    /// (view=canonical). `spec` is omitted; `canonical` +
    /// `readiness` are populated.
    pub fn canonical_only_labeled(
        canonical: &'a crate::canonical_action_key::CanonicalActionKeyInput,
        readiness: &'a crate::canonical_action_key::CanonicalizationReadiness,
        target: String,
        action: String,
    ) -> Self {
        Self {
            schema_version: ACTION_KEY_EXPLAIN_SCHEMA_VERSION,
            target: Some(target),
            action: Some(action),
            spec: None,
            canonical: Some(canonical),
            readiness: Some(readiness),
            cacheability: None,
        }
    }

    /// Phase 5.C.4 commit 69: build a both-views entry
    /// (view=both). All fields populated.
    pub fn both_views_labeled(
        spec: &'a ActionKeySpec,
        canonical: &'a crate::canonical_action_key::CanonicalActionKeyInput,
        readiness: &'a crate::canonical_action_key::CanonicalizationReadiness,
        target: String,
        action: String,
    ) -> Self {
        Self {
            schema_version: ACTION_KEY_EXPLAIN_SCHEMA_VERSION,
            target: Some(target),
            action: Some(action),
            spec: Some(spec),
            canonical: Some(canonical),
            readiness: Some(readiness),
            cacheability: None,
        }
    }

    /// Phase 5.C.4 commit 84: attach a `cacheability`
    /// view to an existing entry.
    ///
    /// Builder-style: returns `self` after setting the
    /// field so the orchestrator can chain construction +
    /// attachment in a single expression. Calling twice
    /// replaces the prior value.
    pub fn with_cacheability(mut self, view: ActionKeyCacheabilityView) -> Self {
        self.cacheability = Some(view);
        self
    }
}

/// Phase 5.C.4 commit 69: which view of the action-key
/// explain output to emit.
///
/// `Spec` (default) preserves the commit-54 shape — just
/// the `ActionKeySpec`. `Canonical` emits the
/// `CanonicalActionKeyInput` + readiness report.
/// `Both` emits both.
///
/// Pro's commit-69 directive: "Default `spec`. Do not
/// double output volume by default."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExplainActionKeysView {
    /// Show only ActionKeySpec (default; commit-54
    /// behavior preserved).
    Spec,
    /// Show only CanonicalActionKeyInput + readiness.
    Canonical,
    /// Show both. Doubles output volume.
    Both,
}

impl ExplainActionKeysView {
    /// Parse a CLI/env-var string into a view. Returns
    /// `Some` on a recognized value, `None` otherwise.
    /// Recognized: `"spec"`, `"canonical"`, `"both"`
    /// (case-insensitive).
    pub fn from_str_ci(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "spec" => Some(Self::Spec),
            "canonical" => Some(Self::Canonical),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    /// Returns true iff the spec view is included.
    pub fn includes_spec(self) -> bool {
        matches!(self, Self::Spec | Self::Both)
    }

    /// Returns true iff the canonical view is included.
    pub fn includes_canonical(self) -> bool {
        matches!(self, Self::Canonical | Self::Both)
    }
}

impl Default for ExplainActionKeysView {
    fn default() -> Self {
        Self::Spec
    }
}

/// shape explicit and lets diagnostic surfaces show what the
/// future hash inputs WOULD be.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActionKeySpec {
    /// Mnemonic of the action (CppCompile, CcArchive, etc.).
    pub mnemonic: String,
    /// Executable path (the tool being invoked). Phase 5.C.4
    /// commit 57: classified as a [`ClassifiedTool`] (was
    /// ClassifiedInput) so the type makes the
    /// "tool, not an input" contract explicit.
    pub executable: ClassifiedTool,
    /// argv elements in order, post-NormalizedCommandSpec
    /// normalization.
    pub argv: Vec<String>,
    /// Env (key, value) pairs sorted by key (matches
    /// NormalizedCommandSpec sort).
    pub env: Vec<(String, String)>,
    /// Working directory (None = exec root).
    pub cwd: Option<ClassifiedInput>,
    /// Declared input paths, each classified.
    pub declared_inputs: Vec<ClassifiedInput>,
    /// Declared output paths. Not part of the cache key
    /// (cache outputs come from the cache; we don't hash the
    /// future-output names). Included for diagnostic
    /// completeness.
    pub declared_outputs: Vec<PathBuf>,
    /// Phase 5.C.4 commit 42: classified output paths.
    /// Parallel to `declared_outputs`; same Vec length, same
    /// order. Each entry pairs the path with its
    /// `OutputPathPolicy` so future cache-key normalization
    /// (post-5.C.5) can route normalized vs unclassified
    /// outputs differently.
    ///
    /// `declared_outputs` is preserved alongside this field
    /// rather than replaced because: (a) commit 32's API
    /// shipped `Vec<PathBuf>` and no production code depends
    /// on `Vec<ClassifiedOutput>` yet — keeping the existing
    /// field stable lets downstream consumers migrate
    /// independently; (b) the policy is path-derived data,
    /// not authoritative — `declared_outputs` remains the
    /// source of truth for the spec.
    pub classified_outputs: Vec<ClassifiedOutput>,
    /// Phase 5.C.4 commit 66: env-inheritance policy.
    /// Carried from the source `NormalizedCommandSpec` so
    /// the cacheability calculation can demote to
    /// [`CacheabilityClass::NonCacheable`] when the runner
    /// inherits the host env.
    ///
    /// Pro's commit-66 directive: the diagnostic model must
    /// not lie about real execution semantics. With
    /// `InheritHost`, `cacheability()` adds
    /// [`CacheabilityReason::InheritedHostEnv`] regardless
    /// of how clean the declared env list looks.
    pub env_policy: crate::ids::ActionEnvPolicy,
}

impl ActionKeySpec {
    /// Phase 5.C.4 commit 32: build an `ActionKeySpec` from a
    /// `NormalizedCommandSpec` by classifying each path
    /// against the workspace root + env overrides.
    ///
    /// Pure data conversion — no I/O, no hashing, no cache
    /// state. Future cache work calls this then hashes the
    /// result; today the orchestrator just dumps it for
    /// diagnostic purposes.
    pub fn from_normalized_command_spec(
        spec: &NormalizedCommandSpec,
        workspace_root: &Path,
        env_override_paths: &[PathBuf],
        external_repo_paths: &std::collections::HashMap<String, PathBuf>,
    ) -> Self {
        let executable = ClassifiedTool {
            path: spec.executable.clone(),
            class: classify_path(
                &spec.executable,
                workspace_root,
                env_override_paths,
            ),
        };
        let cwd = spec.cwd.as_ref().map(|p| ClassifiedInput {
            path: p.clone(),
            class: classify_path(p, workspace_root, env_override_paths),
        });
        let declared_inputs = spec
            .declared_inputs
            .iter()
            .map(|p| ClassifiedInput {
                path: p.clone(),
                class: classify_path(p, workspace_root, env_override_paths),
            })
            .collect();
        // Phase 5.C.4 commit 42: classify each declared output
        // by its OutputPathPolicy. Same input set as
        // declared_outputs, parallel order — the two Vecs
        // index together by position.
        let classified_outputs = spec
            .declared_outputs
            .iter()
            .map(|p| ClassifiedOutput {
                path: p.clone(),
                policy: classify_output_path(
                    p,
                    workspace_root,
                    external_repo_paths,
                ),
            })
            .collect();
        Self {
            mnemonic: spec.mnemonic.clone(),
            executable,
            argv: spec.argv.clone(),
            env: spec.env.clone(),
            cwd,
            declared_inputs,
            declared_outputs: spec.declared_outputs.clone(),
            classified_outputs,
            env_policy: spec.env_policy,
        }
    }

    /// Phase 5.C.4 commit 42: do all declared outputs classify
    /// into a normalizable shape? Returns false if any output
    /// is `Unclassified` (forces the action to be
    /// non-cacheable in the future cache layer).
    pub fn outputs_all_normalizable(&self) -> bool {
        self.classified_outputs.iter().all(|c| c.policy.is_normalizable())
    }

    /// Phase 5.C.4 commit 32 / 35: aggregate cacheability
    /// across all classified inputs + executable + cwd. The
    /// action's overall cacheability is the WORST tier across
    /// every path (any NonCacheable input demotes the action;
    /// any LocalHostCacheable input prevents remote-cacheable).
    ///
    /// Today this is diagnostic; nothing acts on the result.
    /// Real cache work (post-5.C.5) routes per the result:
    /// RemoteCacheable → AC + remote, LocalHostCacheable →
    /// local disk only, NonCacheable → re-execute.
    pub fn cacheability(&self) -> Cacheability {
        let mut c = Cacheability::safe();
        // Executable: Unclassified → UnclassifiedExecutable
        // (commit 58 — distinct from UnclassifiedInput so
        // operators can tell tools from inputs in the
        // reasons list).
        if self.executable.class == HermeticityClass::Unclassified {
            c.add_reason(CacheabilityReason::UnclassifiedExecutable);
        } else if let Some(reason) = self.executable.class.cacheability_reason() {
            c.add_reason(reason);
        }
        // cwd: same logic as inputs.
        if let Some(cwd) = &self.cwd {
            if let Some(reason) = cwd.class.cacheability_reason() {
                c.add_reason(reason);
            }
        }
        // Declared inputs: classify each.
        for inp in &self.declared_inputs {
            if let Some(reason) = inp.class.cacheability_reason() {
                c.add_reason(reason);
            }
        }
        // Phase 5.C.4 commit 66: env-policy demotion.
        // Pro's directive: "If `ActionKeySpec.env` says `[]`
        // but `tokio::process::Command` inherits the host
        // environment, then the diagnostic model is lying."
        // `InheritHost` means the spawned process gets the
        // FULL parent env, not just `self.env`. We can't
        // fingerprint an unbounded inherited env, so demote
        // unconditionally to NonCacheable. This fires BEFORE
        // per-key env classification because the host-env
        // demotion is the dominant signal — if we're
        // inheriting, the per-key analysis on the declared
        // overlay is a side detail.
        if matches!(
            self.env_policy,
            crate::ids::ActionEnvPolicy::InheritHost
        ) {
            c.add_reason(CacheabilityReason::InheritedHostEnv);
        }
        // Phase 5.C.4 commit 58: env classification.
        // Pro's directive: "Mark action non-remote-cacheable
        // if env contains unclassified or host-path-sensitive
        // variables." Each env entry contributes one reason
        // via `classify_env_var`.
        for (key, _value) in &self.env {
            if let Some(reason) = classify_env_var(key) {
                c.add_reason(reason);
            }
        }
        // Declared outputs: Unclassified → UnclassifiedOutput.
        for co in &self.classified_outputs {
            if matches!(co.policy, OutputPathPolicy::Unclassified) {
                c.add_reason(CacheabilityReason::UnclassifiedOutput);
            }
        }
        c
    }

    /// Phase 5.C.4 commit 32 / 58: thin wrapper. Returns
    /// true iff the aggregated cacheability class is
    /// RemoteCacheable.
    pub fn is_remote_cacheable(&self) -> bool {
        matches!(
            self.cacheability().class,
            CacheabilityClass::RemoteCacheable
        )
    }

    /// Phase 5.C.4 commit 59: per-field divergence diff
    /// against another `ActionKeySpec`.
    ///
    /// Returns a typed `Vec<ActionKeyDivergence>` (was
    /// `Vec<String>` in commit 51). Empty Vec means the two
    /// specs are equivalent. CI consumers parse the typed
    /// model; humans use [`format_action_key_divergences`]
    /// to render strings.
    ///
    /// Field ordering (top-down in the result Vec):
    ///   1. mnemonic
    ///   2. executable
    ///   3. cwd
    ///   4. argv (per-index)
    ///   5. env (per-key, sorted)
    ///   6. declared_inputs (per-path, set-diff)
    ///   7. declared_outputs (per-path, set-diff)
    ///   8. classified_outputs (per-path policy compare)
    ///   9. cacheability class
    ///  10. outputs_all_normalizable
    pub fn diff(&self, other: &Self) -> Vec<ActionKeyDivergence> {
        let mut out: Vec<ActionKeyDivergence> = Vec::new();
        if self.mnemonic != other.mnemonic {
            out.push(ActionKeyDivergence::Mnemonic {
                legacy: self.mnemonic.clone(),
                scheduler: other.mnemonic.clone(),
            });
        }
        if self.executable.path != other.executable.path
            || self.executable.class != other.executable.class
        {
            out.push(ActionKeyDivergence::Executable {
                legacy: format!(
                    "{} [{}]",
                    self.executable.path.display(),
                    self.executable.class.label()
                ),
                scheduler: format!(
                    "{} [{}]",
                    other.executable.path.display(),
                    other.executable.class.label()
                ),
            });
        }
        match (&self.cwd, &other.cwd) {
            (None, None) => {}
            (Some(a), Some(b)) if a == b => {}
            (Some(a), None) => out.push(ActionKeyDivergence::Cwd {
                legacy: format!(
                    "{} [{}]",
                    a.path.display(),
                    a.class.label()
                ),
                scheduler: "<exec_root>".to_string(),
            }),
            (None, Some(b)) => out.push(ActionKeyDivergence::Cwd {
                legacy: "<exec_root>".to_string(),
                scheduler: format!(
                    "{} [{}]",
                    b.path.display(),
                    b.class.label()
                ),
            }),
            (Some(a), Some(b)) => out.push(ActionKeyDivergence::Cwd {
                legacy: format!(
                    "{} [{}]",
                    a.path.display(),
                    a.class.label()
                ),
                scheduler: format!(
                    "{} [{}]",
                    b.path.display(),
                    b.class.label()
                ),
            }),
        }
        // argv: emit one Argv per first differing index.
        // When lengths differ, the "extra" entries appear as
        // legacy=None / scheduler=None. The first diverging
        // index sets first_index; subsequent indexes are
        // skipped (same as commit 51's behavior — operators
        // resync expectations after the first divergence).
        let max_argv = self.argv.len().max(other.argv.len());
        for i in 0..max_argv {
            let a = self.argv.get(i);
            let b = other.argv.get(i);
            if a != b {
                out.push(ActionKeyDivergence::Argv {
                    first_index: i,
                    legacy: a.cloned(),
                    scheduler: b.cloned(),
                });
                break;
            }
        }
        // env: compare by key (sorted iteration). Pro's
        // commit-59 spec: per-key Env { key, legacy, scheduler }.
        let mut self_env: std::collections::BTreeMap<&str, &str> =
            std::collections::BTreeMap::new();
        for (k, v) in &self.env {
            self_env.insert(k.as_str(), v.as_str());
        }
        let mut other_env: std::collections::BTreeMap<&str, &str> =
            std::collections::BTreeMap::new();
        for (k, v) in &other.env {
            other_env.insert(k.as_str(), v.as_str());
        }
        let all_keys: std::collections::BTreeSet<&str> = self_env
            .keys()
            .chain(other_env.keys())
            .copied()
            .collect();
        for key in all_keys {
            let a = self_env.get(key);
            let b = other_env.get(key);
            if a != b {
                out.push(ActionKeyDivergence::Env {
                    key: key.to_string(),
                    legacy: a.map(|s| s.to_string()),
                    scheduler: b.map(|s| s.to_string()),
                });
            }
        }
        // declared_inputs: compare by path (set-diff). Same
        // path on both sides with different class →
        // ClassifiedInput. Path on one side only →
        // DeclaredInput { side }.
        let self_inputs: std::collections::BTreeMap<PathBuf, &HermeticityClass> =
            self.declared_inputs
                .iter()
                .map(|c| (c.path.clone(), &c.class))
                .collect();
        let other_inputs: std::collections::BTreeMap<PathBuf, &HermeticityClass> =
            other
                .declared_inputs
                .iter()
                .map(|c| (c.path.clone(), &c.class))
                .collect();
        let all_input_paths: std::collections::BTreeSet<&PathBuf> = self_inputs
            .keys()
            .chain(other_inputs.keys())
            .collect();
        for p in all_input_paths {
            match (self_inputs.get(p), other_inputs.get(p)) {
                (Some(a), Some(b)) if a == b => {}
                (Some(a), Some(b)) => {
                    out.push(ActionKeyDivergence::ClassifiedInput {
                        path: p.display().to_string(),
                        legacy: a.label().to_string(),
                        scheduler: b.label().to_string(),
                    });
                }
                (Some(_), None) => {
                    out.push(ActionKeyDivergence::DeclaredInput {
                        path: p.display().to_string(),
                        side: DiffSide::LegacyOnly,
                    });
                }
                (None, Some(_)) => {
                    out.push(ActionKeyDivergence::DeclaredInput {
                        path: p.display().to_string(),
                        side: DiffSide::SchedulerOnly,
                    });
                }
                (None, None) => unreachable!(),
            }
        }
        // declared_outputs + classified_outputs: same
        // set-diff approach. Track classified_outputs as the
        // policy-bearing structure (declared_outputs are
        // path-only; classified_outputs run in lockstep by
        // construction).
        let self_outs: std::collections::BTreeMap<PathBuf, &OutputPathPolicy> =
            self.classified_outputs
                .iter()
                .map(|c| (c.path.clone(), &c.policy))
                .collect();
        let other_outs: std::collections::BTreeMap<PathBuf, &OutputPathPolicy> =
            other
                .classified_outputs
                .iter()
                .map(|c| (c.path.clone(), &c.policy))
                .collect();
        let all_output_paths: std::collections::BTreeSet<&PathBuf> =
            self_outs.keys().chain(other_outs.keys()).collect();
        for p in all_output_paths {
            match (self_outs.get(p), other_outs.get(p)) {
                (Some(a), Some(b)) if a == b => {}
                (Some(a), Some(b)) => {
                    out.push(ActionKeyDivergence::ClassifiedOutput {
                        path: p.display().to_string(),
                        legacy: a.label(),
                        scheduler: b.label(),
                    });
                }
                (Some(_), None) => {
                    out.push(ActionKeyDivergence::DeclaredOutput {
                        path: p.display().to_string(),
                        side: DiffSide::LegacyOnly,
                    });
                }
                (None, Some(_)) => {
                    out.push(ActionKeyDivergence::DeclaredOutput {
                        path: p.display().to_string(),
                        side: DiffSide::SchedulerOnly,
                    });
                }
                (None, None) => unreachable!(),
            }
        }
        let cl = self.cacheability();
        let cr = other.cacheability();
        if cl.class != cr.class {
            out.push(ActionKeyDivergence::Cacheability {
                legacy: cl.class.label().to_string(),
                scheduler: cr.class.label().to_string(),
            });
        }
        if self.outputs_all_normalizable() != other.outputs_all_normalizable() {
            out.push(ActionKeyDivergence::OutputsNormalizable {
                legacy: self.outputs_all_normalizable(),
                scheduler: other.outputs_all_normalizable(),
            });
        }
        // Phase 5.C.4 commit 68: typed env_policy divergence.
        // Pairs with the same-named field in
        // `command_spec::diff` (commit 66) — the verifier
        // surface now has a typed variant to match.
        if self.env_policy != other.env_policy {
            out.push(ActionKeyDivergence::EnvPolicy {
                legacy: self.env_policy,
                scheduler: other.env_policy,
            });
        }
        out
    }
}

/// Phase 5.C.4 commit 59: which side of a legacy-vs-
/// scheduler comparison a path or value appears on.
///
/// Used by [`ActionKeyDivergence::DeclaredInput`] and
/// `DeclaredOutput` variants when a path exists on one
/// side only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    /// Path/value present in legacy spec only (scheduler
    /// missing it).
    LegacyOnly,
    /// Path/value present in scheduler spec only (legacy
    /// missing it).
    SchedulerOnly,
}

/// Phase 5.C.4 commit 59: typed per-field divergence
/// between two `ActionKeySpec` values.
///
/// Replaces commit 51's `Vec<String>` diff output with a
/// structured typed model. Pro's directive: "Strings are
/// fine for human display. They are trash for CI. Very
/// popular trash, yes, but still trash."
///
/// CI consumers parse this enum directly via serde_json;
/// humans use [`format_action_key_divergences`] to render
/// the same strings the legacy Vec<String> output produced.
///
/// Variant set follows Pro's commit-59 spec.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ActionKeyDivergence {
    /// Mnemonic strings differ.
    Mnemonic {
        legacy: String,
        scheduler: String,
    },
    /// Executable path or class differ. legacy/scheduler
    /// strings carry "<path> [<class>]".
    Executable {
        legacy: String,
        scheduler: String,
    },
    /// cwd differs (None vs Some, or paths/classes
    /// differ). legacy/scheduler are "<path> [<class>]" or
    /// "<exec_root>".
    Cwd {
        legacy: String,
        scheduler: String,
    },
    /// argv differs at `first_index`. When one side is
    /// shorter, the corresponding side's value is None.
    Argv {
        first_index: usize,
        legacy: Option<String>,
        scheduler: Option<String>,
    },
    /// env entry for `key` differs. None means the side
    /// doesn't have the key.
    Env {
        key: String,
        legacy: Option<String>,
        scheduler: Option<String>,
    },
    /// Declared input path appears on one side only.
    DeclaredInput {
        path: String,
        side: DiffSide,
    },
    /// Declared output path appears on one side only.
    DeclaredOutput {
        path: String,
        side: DiffSide,
    },
    /// Same input path, different hermeticity class.
    ClassifiedInput {
        path: String,
        legacy: String,
        scheduler: String,
    },
    /// Same output path, different OutputPathPolicy.
    ClassifiedOutput {
        path: String,
        legacy: String,
        scheduler: String,
    },
    /// Cacheability class differs.
    Cacheability {
        legacy: String,
        scheduler: String,
    },
    /// outputs_all_normalizable bool differs.
    OutputsNormalizable {
        legacy: bool,
        scheduler: bool,
    },
    /// Phase 5.C.4 commit 68: `ActionEnvPolicy` differs
    /// between legacy and scheduler. Builder claims
    /// hermetic env (`ExplicitOnly`) but runner inherits
    /// (`InheritHost`), or vice versa — silent corruption
    /// risk. Fatal in `--verify-scheduler-commands`.
    ///
    /// Pro's commit-68 directive: "Commit 66 added
    /// env_policy mismatch detection in command_spec::diff,
    /// but the typed ActionKeyDivergence model does not
    /// yet have an env_policy variant. Otherwise one part
    /// of the system speaks typed diagnostics and this one
    /// corner goes back to stringly-shaped cave scribbles."
    EnvPolicy {
        legacy: crate::ids::ActionEnvPolicy,
        scheduler: crate::ids::ActionEnvPolicy,
    },
}

/// Phase 5.C.4 commit 59: severity tier for an
/// `ActionKeyDivergence`.
///
/// Today every variant in `--verify-scheduler-commands`
/// mode is `Fatal`. The type exists so a future commit can
/// demote specific variants to `Warning` once the
/// migration story is firmer (Pro's "demonstrate it can
/// proceed inside a clearly bounded lane"). NOT used by
/// the verifier today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceSeverity {
    /// Fatal — fails verification.
    Fatal,
    /// Warning — surfaces in output but doesn't fail.
    Warning,
}

/// Phase 5.C.4 commit 59: render typed divergences as
/// human-readable strings (matches commit 51's old
/// `Vec<String>` shape one-for-one).
///
/// Used by the verifier (commit 51's all_diffs site) to
/// preserve operator-readable output while the typed model
/// is what flows through the structured surfaces.
pub fn format_action_key_divergences(
    divergences: &[ActionKeyDivergence],
) -> Vec<String> {
    divergences
        .iter()
        .map(|d| match d {
            ActionKeyDivergence::Mnemonic { legacy, scheduler } => {
                format!("mnemonic: {} vs {}", legacy, scheduler)
            }
            ActionKeyDivergence::Executable { legacy, scheduler } => {
                format!("executable: {} vs {}", legacy, scheduler)
            }
            ActionKeyDivergence::Cwd { legacy, scheduler } => {
                format!("cwd: {} vs {}", legacy, scheduler)
            }
            ActionKeyDivergence::Argv {
                first_index,
                legacy,
                scheduler,
            } => match (legacy, scheduler) {
                (Some(a), Some(b)) => {
                    format!("argv[{}]: {} vs {}", first_index, a, b)
                }
                (Some(a), None) => {
                    format!("argv[{}]: {} vs <missing>", first_index, a)
                }
                (None, Some(b)) => {
                    format!("argv[{}]: <missing> vs {}", first_index, b)
                }
                (None, None) => unreachable!(
                    "argv divergence with both sides None is impossible"
                ),
            },
            ActionKeyDivergence::Env {
                key,
                legacy,
                scheduler,
            } => match (legacy, scheduler) {
                (Some(a), Some(b)) => {
                    format!("env[{}]: {} vs {}", key, a, b)
                }
                (Some(a), None) => {
                    format!("env[{}]: {} vs <missing>", key, a)
                }
                (None, Some(b)) => {
                    format!("env[{}]: <missing> vs {}", key, b)
                }
                (None, None) => unreachable!(),
            },
            ActionKeyDivergence::DeclaredInput { path, side } => match side {
                DiffSide::LegacyOnly => {
                    format!("declared_inputs: {} (legacy only)", path)
                }
                DiffSide::SchedulerOnly => {
                    format!("declared_inputs: {} (scheduler only)", path)
                }
            },
            ActionKeyDivergence::DeclaredOutput { path, side } => match side {
                DiffSide::LegacyOnly => {
                    format!("declared_outputs: {} (legacy only)", path)
                }
                DiffSide::SchedulerOnly => {
                    format!("declared_outputs: {} (scheduler only)", path)
                }
            },
            ActionKeyDivergence::ClassifiedInput {
                path,
                legacy,
                scheduler,
            } => format!(
                "declared_inputs[{}]: class {} vs {}",
                path, legacy, scheduler
            ),
            ActionKeyDivergence::ClassifiedOutput {
                path,
                legacy,
                scheduler,
            } => format!(
                "classified_outputs[{}].policy: {} vs {}",
                path, legacy, scheduler
            ),
            ActionKeyDivergence::Cacheability { legacy, scheduler } => {
                format!("cacheability: {} vs {}", legacy, scheduler)
            }
            ActionKeyDivergence::OutputsNormalizable { legacy, scheduler } => {
                format!(
                    "outputs_all_normalizable: {} vs {}",
                    legacy, scheduler
                )
            }
            ActionKeyDivergence::EnvPolicy { legacy, scheduler } => {
                format!(
                    "env_policy: {} vs {}",
                    env_policy_label(*legacy),
                    env_policy_label(*scheduler)
                )
            }
        })
        .collect()
}

/// Phase 5.C.4 commit 68: short human label for
/// [`crate::ids::ActionEnvPolicy`]. Mirrors the
/// snake_case JSON encoding so operator output reads the
/// same as machine output.
fn env_policy_label(p: crate::ids::ActionEnvPolicy) -> &'static str {
    match p {
        crate::ids::ActionEnvPolicy::InheritHost => "inherit_host",
        crate::ids::ActionEnvPolicy::ExplicitOnly => "explicit_only",
    }
}

/// Phase 5.C.4 commit 32: human-readable dump of an
/// `ActionKeySpec`. Pro's "action-key explain / dump" surface.
///
/// One block per action. Lists each classified path with its
/// hermeticity label, the argv, env, cwd, and a final
/// `would_remote_cache` line. Caller decides whether to
/// print, log, capture for tests, etc.
pub fn format_action_key_spec(spec: &ActionKeySpec) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    out.push(format!("ActionKeySpec[{}]:", spec.mnemonic));
    out.push(format!(
        "  executable: {} [{}]",
        spec.executable.path.display(),
        spec.executable.class.label()
    ));
    if let Some(cwd) = &spec.cwd {
        out.push(format!(
            "  cwd: {} [{}]",
            cwd.path.display(),
            cwd.class.label()
        ));
    } else {
        out.push("  cwd: <exec_root>".to_string());
    }
    out.push(format!("  argv ({} elements):", spec.argv.len()));
    for (i, a) in spec.argv.iter().enumerate() {
        out.push(format!("    [{}] {}", i, a));
    }
    out.push(format!("  env ({} entries):", spec.env.len()));
    for (k, v) in &spec.env {
        out.push(format!("    {}={}", k, v));
    }
    out.push(format!(
        "  declared_inputs ({} entries):",
        spec.declared_inputs.len()
    ));
    for inp in &spec.declared_inputs {
        out.push(format!(
            "    {} [{}]",
            inp.path.display(),
            inp.class.label()
        ));
    }
    out.push(format!(
        "  declared_outputs ({} entries):",
        spec.declared_outputs.len()
    ));
    // Phase 5.C.4 commit 42: dump each declared output with
    // its OutputPathPolicy classification. Falls back to the
    // raw path when classified_outputs is empty (legacy specs
    // built before commit 42's wire-up landed; should be rare
    // since from_normalized_command_spec always populates).
    if spec.classified_outputs.is_empty() {
        for outp in &spec.declared_outputs {
            out.push(format!("    {}", outp.display()));
        }
    } else {
        for co in &spec.classified_outputs {
            out.push(format!(
                "    {} [{}]",
                co.path.display(),
                co.policy.label()
            ));
        }
    }
    let cacheability = spec.cacheability();
    out.push(format!("  cacheability: {}", cacheability.label()));
    // Phase 5.C.4 commit 58 / 72: dump the reasons that
    // contributed to non-RemoteCacheable state, collapsed
    // by variant with counts. Pro's commit-72 directive:
    // "HostToolPath × 12 / HostAmbientInclude × 3" — one
    // line per distinct reason variant, no spam.
    let summary = cacheability.summarized_reasons();
    if !summary.is_empty() {
        let total: usize = summary.iter().map(|s| s.count).sum();
        out.push(format!(
            "  cacheability_reasons ({} total, {} distinct):",
            total,
            summary.len()
        ));
        for s in &summary {
            if s.count == 1 {
                out.push(format!("    {:?}", s.reason));
            } else {
                out.push(format!("    {:?} × {}", s.reason, s.count));
            }
        }
    }
    out.push(format!(
        "  outputs_all_normalizable: {}",
        spec.outputs_all_normalizable()
    ));
    out.push(format!(
        "  would_remote_cache: {}",
        spec.is_remote_cacheable()
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> PathBuf {
        PathBuf::from("/home/user/workspace")
    }

    #[test]
    fn classify_workspace_source() {
        let p = PathBuf::from("/home/user/workspace/src/foo.cc");
        let c = classify_path(&p, &ws(), &[]);
        assert_eq!(c, HermeticityClass::WorkspaceLocal);
        assert!(c.is_remote_cacheable());
    }

    #[test]
    fn classify_workspace_generated_output() {
        let p = PathBuf::from("/home/user/workspace/rush-out/obj/foo.o");
        assert_eq!(
            classify_path(&p, &ws(), &[]),
            HermeticityClass::WorkspaceLocal
        );
    }

    #[test]
    fn classify_usr_include_as_host_ambient() {
        let p = PathBuf::from("/usr/include/stdio.h");
        let c = classify_path(&p, &ws(), &[]);
        assert_eq!(c, HermeticityClass::HostAmbientInclude);
        assert!(!c.is_remote_cacheable());
    }

    #[test]
    fn classify_usr_local_include_as_host_ambient() {
        let p = PathBuf::from("/usr/local/include/foo.h");
        assert_eq!(
            classify_path(&p, &ws(), &[]),
            HermeticityClass::HostAmbientInclude
        );
    }

    #[test]
    fn classify_usr_bin_tool_as_host_tool() {
        let p = PathBuf::from("/usr/bin/protoc");
        let c = classify_path(&p, &ws(), &[]);
        assert_eq!(c, HermeticityClass::HostToolPath);
        assert!(!c.is_remote_cacheable());
    }

    #[test]
    fn classify_usr_local_bin_tool_as_host_tool() {
        let p = PathBuf::from("/usr/local/bin/grpc_cpp_plugin");
        assert_eq!(
            classify_path(&p, &ws(), &[]),
            HermeticityClass::HostToolPath
        );
    }

    #[test]
    fn classify_bare_tool_name_as_host_tool() {
        // `protoc` (no path) — PATH-resolved at exec, host-ambient.
        let p = PathBuf::from("protoc");
        assert_eq!(
            classify_path(&p, &ws(), &[]),
            HermeticityClass::HostToolPath
        );
    }

    #[test]
    fn classify_env_override_takes_precedence_over_workspace() {
        // RUSH_GRPC_CPP_PLUGIN points at a path inside the
        // workspace (unusual but possible). Env override
        // classification should win — the user explicitly
        // declared this as a tool override, so it gets the
        // EnvOverrideToolPath class.
        let env_path = PathBuf::from("/home/user/workspace/tools/grpc_cpp_plugin");
        assert_eq!(
            classify_path(&env_path, &ws(), &[env_path.clone()]),
            HermeticityClass::EnvOverrideToolPath
        );
    }

    #[test]
    fn classify_unknown_path_falls_to_unclassified() {
        let p = PathBuf::from("/opt/some/random/tool");
        let c = classify_path(&p, &ws(), &[]);
        assert_eq!(c, HermeticityClass::Unclassified);
        assert!(!c.is_remote_cacheable());
    }

    #[test]
    fn from_spec_classifies_all_paths() {
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec!["-c".to_string(), "src.cc".to_string()],
            env: vec![("CC".to_string(), "g++".to_string())],
            declared_inputs: vec![
                PathBuf::from("/home/user/workspace/src.cc"),
                PathBuf::from("/usr/include/stdio.h"),
            ],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/obj.o",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(&spec, &ws(), &[], &empty_external());
        assert_eq!(aks.mnemonic, "CppCompile");
        assert_eq!(aks.executable.class, HermeticityClass::HostToolPath);
        assert_eq!(
            aks.declared_inputs[0].class,
            HermeticityClass::WorkspaceLocal
        );
        assert_eq!(
            aks.declared_inputs[1].class,
            HermeticityClass::HostAmbientInclude
        );
        // Mixed inputs → not remote-cacheable.
        assert!(!aks.is_remote_cacheable());
    }

    #[test]
    fn fully_workspace_local_action_is_remote_cacheable() {
        // Hypothetical: all-workspace-local action (e.g.
        // ar archiving local .o files with a workspace-local
        // ar binary — not realistic on standard Linux but
        // possible in a hermetic setup).
        let spec = NormalizedCommandSpec {
            mnemonic: "CcArchive".to_string(),
            executable: PathBuf::from("/home/user/workspace/tools/ar"),
            cwd: None,
            argv: vec!["rcs".to_string()],
            env: Vec::new(),
            declared_inputs: vec![
                PathBuf::from("/home/user/workspace/rush-out/obj/foo.o"),
                PathBuf::from("/home/user/workspace/rush-out/obj/bar.o"),
            ],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/lib/libfoo.a",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(&spec, &ws(), &[], &empty_external());
        assert!(
            aks.is_remote_cacheable(),
            "all-workspace-local action should be remote-cacheable"
        );
    }

    #[test]
    fn format_dump_includes_classifications() {
        let spec = NormalizedCommandSpec {
            mnemonic: "ProtocCompile".to_string(),
            executable: PathBuf::from("/usr/bin/protoc"),
            cwd: None,
            argv: vec!["--cpp_out=.".to_string()],
            env: Vec::new(),
            declared_inputs: vec![PathBuf::from("/home/user/workspace/foo.proto")],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/foo.pb.h",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(&spec, &ws(), &[], &empty_external());
        let dump = format_action_key_spec(&aks).join("\n");
        assert!(dump.contains("ActionKeySpec[ProtocCompile]"));
        assert!(dump.contains("/usr/bin/protoc [HostToolPath]"));
        assert!(dump.contains("foo.proto [WorkspaceLocal]"));
        assert!(dump.contains("would_remote_cache: false"));
    }

    // Phase 5.C.4 commit 35: Cacheability enum + aggregation
    // tests. Pro: "is_remote_cacheable() as a boolean will
    // become too crude. Use 3-class enum."

    #[test]
    fn cacheability_class_workspace_local_is_remote_cacheable() {
        assert_eq!(
            HermeticityClass::WorkspaceLocal.cacheability_class(),
            CacheabilityClass::RemoteCacheable
        );
    }

    #[test]
    fn cacheability_class_host_ambient_include_is_local_host() {
        assert_eq!(
            HermeticityClass::HostAmbientInclude.cacheability_class(),
            CacheabilityClass::LocalHostCacheable
        );
    }

    #[test]
    fn cacheability_class_host_tool_path_is_local_host() {
        assert_eq!(
            HermeticityClass::HostToolPath.cacheability_class(),
            CacheabilityClass::LocalHostCacheable
        );
    }

    #[test]
    fn cacheability_class_env_override_is_local_host() {
        assert_eq!(
            HermeticityClass::EnvOverrideToolPath.cacheability_class(),
            CacheabilityClass::LocalHostCacheable
        );
    }

    #[test]
    fn cacheability_class_unclassified_is_non_cacheable() {
        assert_eq!(
            HermeticityClass::Unclassified.cacheability_class(),
            CacheabilityClass::NonCacheable
        );
    }

    #[test]
    fn cacheability_class_combine_lattice() {
        use CacheabilityClass::*;
        assert_eq!(NonCacheable.combine(RemoteCacheable), NonCacheable);
        assert_eq!(RemoteCacheable.combine(NonCacheable), NonCacheable);
        assert_eq!(NonCacheable.combine(LocalHostCacheable), NonCacheable);
        assert_eq!(
            LocalHostCacheable.combine(RemoteCacheable),
            LocalHostCacheable
        );
        assert_eq!(
            RemoteCacheable.combine(LocalHostCacheable),
            LocalHostCacheable
        );
        assert_eq!(
            RemoteCacheable.combine(RemoteCacheable),
            RemoteCacheable
        );
    }

    #[test]
    fn action_cacheability_demoted_by_host_include() {
        // CppCompile reading /usr/include/stdio.h: workspace
        // src + host include → LocalHostCacheable.
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/home/user/workspace/tools/g++"), // hermetic
            cwd: None,
            argv: vec!["-c".to_string()],
            env: Vec::new(),
            declared_inputs: vec![
                PathBuf::from("/home/user/workspace/src.cc"),
                PathBuf::from("/usr/include/stdio.h"),
            ],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/obj.o",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(&spec, &ws(), &[], &empty_external());
        assert_eq!(aks.cacheability().class, CacheabilityClass::LocalHostCacheable);
        assert!(!aks.is_remote_cacheable());
    }

    #[test]
    fn action_cacheability_demoted_to_non_cacheable_by_unclassified() {
        // Workspace src + unclassified path → NonCacheable.
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/home/user/workspace/tools/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![
                PathBuf::from("/home/user/workspace/src.cc"),
                PathBuf::from("/opt/strange-tool/random.h"),
            ],
            declared_outputs: vec![],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(&spec, &ws(), &[], &empty_external());
        assert_eq!(aks.cacheability().class, CacheabilityClass::NonCacheable);
    }

    /// Phase 5.C.4 commit 72: `summarized_reasons()` counts
    /// duplicate reason variants. Pro's spec example:
    /// HostToolPath × 12 / HostAmbientInclude × 3 /
    /// InheritedHostEnv × 1.
    #[test]
    fn cacheability_reason_summary_counts_duplicates() {
        let mut c = Cacheability::safe();
        c.add_reason(CacheabilityReason::HostToolPath);
        c.add_reason(CacheabilityReason::HostToolPath);
        c.add_reason(CacheabilityReason::HostToolPath);
        c.add_reason(CacheabilityReason::HostAmbientInclude);
        c.add_reason(CacheabilityReason::InheritedHostEnv);
        let summary = c.summarized_reasons();
        assert_eq!(summary.len(), 3, "3 distinct reasons");
        // Order: first-occurrence (HostToolPath, then
        // HostAmbientInclude, then InheritedHostEnv).
        assert_eq!(
            summary[0].reason,
            CacheabilityReason::HostToolPath
        );
        assert_eq!(summary[0].count, 3);
        assert_eq!(
            summary[1].reason,
            CacheabilityReason::HostAmbientInclude
        );
        assert_eq!(summary[1].count, 1);
        assert_eq!(
            summary[2].reason,
            CacheabilityReason::InheritedHostEnv
        );
        assert_eq!(summary[2].count, 1);
    }

    /// Phase 5.C.4 commit 72: summarized_reasons preserves
    /// the raw `reasons` Vec — internal callers still see
    /// per-instance accounting if they need it. Pro: "Keep
    /// raw reasons if useful internally."
    #[test]
    fn cacheability_raw_reasons_preserved_alongside_summary() {
        let mut c = Cacheability::safe();
        c.add_reason(CacheabilityReason::HostToolPath);
        c.add_reason(CacheabilityReason::HostToolPath);
        // Raw count is 2.
        assert_eq!(c.reasons.len(), 2);
        // Summary collapses to 1 entry with count 2.
        let summary = c.summarized_reasons();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].count, 2);
    }

    /// Phase 5.C.4 commit 72: human output (via
    /// format_action_key_spec) collapses duplicate reasons.
    /// Pin "HostToolPath × N" appears instead of N
    /// separate HostToolPath lines.
    #[test]
    fn human_output_collapses_duplicate_reasons() {
        // Build a spec where multiple inputs share the
        // same host-tool-path classification — produces
        // multiple HostToolPath reasons that should
        // collapse for display.
        let spec = NormalizedCommandSpec {
            mnemonic: "ProtocCompile".to_string(),
            executable: PathBuf::from("/usr/bin/protoc"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![
                PathBuf::from("/usr/include/lib_a.h"),
                PathBuf::from("/usr/include/lib_b.h"),
                PathBuf::from("/usr/include/lib_c.h"),
            ],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/foo.pb.cc",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        let dump = format_action_key_spec(&aks).join("\n");
        // The summary line should mention "× 3" for
        // HostAmbientInclude (the three /usr/include
        // inputs).
        assert!(
            dump.contains("HostAmbientInclude × 3"),
            "expected collapsed `HostAmbientInclude × 3` line; \
             got dump:\n{}",
            dump
        );
        // The header should reflect total + distinct counts.
        assert!(
            dump.contains("4 total, 2 distinct")
                || dump.contains("4 total, 3 distinct"),
            "expected `N total, M distinct` header; got:\n{}",
            dump
        );
    }

    /// Phase 5.C.4 commit 72: JSON output preserves both
    /// the raw `reasons` Vec AND the summary via the
    /// `summarized_reasons()` accessor. Pro: "JSON output
    /// preserves reason counts."
    #[test]
    fn json_output_preserves_reason_counts() {
        let mut c = Cacheability::safe();
        c.add_reason(CacheabilityReason::HostToolPath);
        c.add_reason(CacheabilityReason::HostToolPath);
        c.add_reason(CacheabilityReason::InheritedHostEnv);
        // Raw Vec serializes as before (back-compat).
        let json = serde_json::to_string(&c).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        let raw = parsed["reasons"].as_array().expect("reasons array");
        assert_eq!(raw.len(), 3, "raw Vec preserves duplicates");
        // Summary serializes through a separate call —
        // callers wanting counts use .summarized_reasons()
        // and serialize the result.
        let summary = c.summarized_reasons();
        let summary_json =
            serde_json::to_string(&summary).expect("serialize");
        let summary_parsed: serde_json::Value =
            serde_json::from_str(&summary_json).expect("parse");
        let arr = summary_parsed.as_array().expect("summary array");
        assert_eq!(arr.len(), 2, "summary collapses to 2 distinct");
        // Find HostToolPath entry; count should be 2.
        let host_tool = arr
            .iter()
            .find(|v| v["reason"]["kind"] == "host_tool_path")
            .expect("HostToolPath summary entry");
        assert_eq!(host_tool["count"], 2);
    }

    /// Phase 5.C.4 commit 72: empty reasons (RemoteCacheable)
    /// produces an empty summary.
    #[test]
    fn empty_reasons_yields_empty_summary() {
        let c = Cacheability::safe();
        assert!(c.reasons.is_empty());
        assert!(c.summarized_reasons().is_empty());
    }

    /// Phase 5.C.4 commit 66: with `env_policy = InheritHost`,
    /// even an otherwise pure-workspace action MUST demote to
    /// `NonCacheable` and surface
    /// [`CacheabilityReason::InheritedHostEnv`]. This pins
    /// the diagnostic-honesty contract: if the runner inherits
    /// host env, the cache layer can't fingerprint it, and the
    /// action-key must say so.
    #[test]
    fn action_cacheability_inherit_host_demotes_pure_workspace_to_non_cacheable() {
        let spec = NormalizedCommandSpec {
            mnemonic: "CcArchive".to_string(),
            executable: PathBuf::from("/home/user/workspace/tools/ar"),
            cwd: None,
            argv: vec!["rcs".to_string()],
            env: Vec::new(),
            declared_inputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/foo.o",
            )],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/libfoo.a",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::InheritHost,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        let c = aks.cacheability();
        assert_eq!(
            c.class,
            CacheabilityClass::NonCacheable,
            "InheritHost must demote even pure-workspace actions to NonCacheable"
        );
        assert!(
            c.reasons.contains(&CacheabilityReason::InheritedHostEnv),
            "InheritedHostEnv reason must surface; got {:?}",
            c.reasons
        );
    }

    /// Phase 5.C.4 commit 66: with `env_policy = ExplicitOnly`,
    /// the same pure-workspace action remains
    /// `RemoteCacheable`. Pin that the new reason ONLY fires
    /// for the inheriting case.
    #[test]
    fn action_cacheability_explicit_only_does_not_emit_inherited_host_env() {
        let spec = NormalizedCommandSpec {
            mnemonic: "CcArchive".to_string(),
            executable: PathBuf::from("/home/user/workspace/tools/ar"),
            cwd: None,
            argv: vec!["rcs".to_string()],
            env: Vec::new(),
            declared_inputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/foo.o",
            )],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/libfoo.a",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        let c = aks.cacheability();
        assert_eq!(c.class, CacheabilityClass::RemoteCacheable);
        assert!(
            !c.reasons.contains(&CacheabilityReason::InheritedHostEnv),
            "ExplicitOnly must NOT emit InheritedHostEnv; got {:?}",
            c.reasons
        );
    }

    #[test]
    fn action_cacheability_pure_workspace_is_remote_cacheable() {
        let spec = NormalizedCommandSpec {
            mnemonic: "CcArchive".to_string(),
            executable: PathBuf::from("/home/user/workspace/tools/ar"),
            cwd: None,
            argv: vec!["rcs".to_string()],
            env: Vec::new(),
            declared_inputs: vec![
                PathBuf::from("/home/user/workspace/rush-out/foo.o"),
            ],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/libfoo.a",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(&spec, &ws(), &[], &empty_external());
        assert_eq!(aks.cacheability().class, CacheabilityClass::RemoteCacheable);
        assert!(aks.is_remote_cacheable());
    }

    #[test]
    fn action_cacheability_in_format_dump() {
        let spec = NormalizedCommandSpec {
            mnemonic: "ProtocCompile".to_string(),
            executable: PathBuf::from("/usr/bin/protoc"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![PathBuf::from(
                "/home/user/workspace/foo.proto",
            )],
            declared_outputs: vec![],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(&spec, &ws(), &[], &empty_external());
        let dump = format_action_key_spec(&aks).join("\n");
        assert!(
            dump.contains("cacheability: LocalHostCacheable"),
            "dump should label aggregate cacheability: {}",
            dump
        );
    }

    #[test]
    fn unsupported_grpc_action_excluded_from_dump_via_eligibility() {
        // Pro: "For grpc-shaped actions, report:
        //   grpc shape: skipped / unsupported / tool unavailable"
        // The exclusion logic lives in the orchestrator (tasks
        // not on the eligibility allowlist don't reach this
        // module). This test pins that the data model has no
        // implicit "always include" behavior — every spec
        // passed in becomes an ActionKeySpec; the caller is
        // responsible for not feeding unsupported shapes here.
        // Future commit wires the orchestrator skip path.
        let spec = NormalizedCommandSpec {
            mnemonic: "ProtocCompileGrpc".to_string(),
            executable: PathBuf::from("/usr/bin/grpc_cpp_plugin"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(&spec, &ws(), &[], &empty_external());
        // Module accepts the spec; classification still runs.
        // Caller (orchestrator) skips before reaching us when
        // the rule's shape is UnsupportedShape.
        assert_eq!(aks.mnemonic, "ProtocCompileGrpc");
        assert_eq!(aks.executable.class, HermeticityClass::HostToolPath);
    }

    // Phase 5.C.4 commit 41: OutputPathPolicy classifier tests.
    // These pin the path-shape decisions so future cache-key
    // normalization (post-5.C.5) has a stable contract to
    // depend on. Each test exercises one branch of
    // classify_output_path.

    use std::collections::HashMap;

    fn empty_external() -> HashMap<String, PathBuf> {
        HashMap::new()
    }

    #[test]
    fn output_policy_classifies_workspace_relative_compile_output() {
        // The most common shape: rush-out/<package>/<file>.o
        let p = PathBuf::from("/home/user/workspace/rush-out/foo/bar.o");
        let policy = classify_output_path(&p, &ws(), &empty_external());
        assert_eq!(policy, OutputPathPolicy::WorkspaceRelativeOutput);
    }

    #[test]
    fn output_policy_classifies_genfiles_output_more_specifically() {
        // Genfiles wins over plain workspace-relative.
        let p = PathBuf::from(
            "/home/user/workspace/rush-out/genfiles/foo/bar.pb.h",
        );
        let policy = classify_output_path(&p, &ws(), &empty_external());
        assert_eq!(policy, OutputPathPolicy::GenfilesOutput);
    }

    #[test]
    fn output_policy_classifies_workspace_external_repo_output() {
        // <workspace>/external/<repo>/<package>/<file> shape.
        let p = PathBuf::from(
            "/home/user/workspace/external/com_google_absl/absl/foo.o",
        );
        let policy = classify_output_path(&p, &ws(), &empty_external());
        assert_eq!(
            policy,
            OutputPathPolicy::ExternalRepoOutput {
                repo: "com_google_absl".to_string()
            }
        );
    }

    #[test]
    fn output_policy_classifies_explicit_external_repo_path() {
        // External repo registered at a non-standard location
        // (e.g. sibling of workspace).
        let mut external = HashMap::new();
        external.insert(
            "absl_sibling".to_string(),
            PathBuf::from("/opt/absl_sibling"),
        );
        let p = PathBuf::from("/opt/absl_sibling/absl/foo.o");
        let policy = classify_output_path(&p, &ws(), &external);
        assert_eq!(
            policy,
            OutputPathPolicy::ExternalRepoOutput {
                repo: "absl_sibling".to_string()
            }
        );
    }

    #[test]
    fn output_policy_returns_unclassified_for_host_path_outside_workspace() {
        // /tmp/foo.o is neither workspace-relative nor any
        // registered external repo → Unclassified.
        let p = PathBuf::from("/tmp/some_random.o");
        let policy = classify_output_path(&p, &ws(), &empty_external());
        assert_eq!(policy, OutputPathPolicy::Unclassified);
    }

    #[test]
    fn output_policy_unclassified_blocks_normalization() {
        // Unclassified should report is_normalizable() == false.
        // Future cache work uses this to gate the action's
        // remote-cacheability.
        let p = OutputPathPolicy::Unclassified;
        assert!(!p.is_normalizable());
    }

    #[test]
    fn output_policy_workspace_genfiles_external_all_normalizable() {
        // Every classified shape is normalizable; only
        // Unclassified blocks it. Pin the contract.
        assert!(OutputPathPolicy::WorkspaceRelativeOutput.is_normalizable());
        assert!(OutputPathPolicy::GenfilesOutput.is_normalizable());
        assert!(OutputPathPolicy::ExternalRepoOutput {
            repo: "x".to_string()
        }
        .is_normalizable());
    }

    #[test]
    fn output_policy_label_round_trip_includes_repo_name() {
        // Label for ExternalRepoOutput must carry the repo
        // name so format_action_key_spec output is human-
        // readable.
        let p = OutputPathPolicy::ExternalRepoOutput {
            repo: "com_google_absl".to_string(),
        };
        assert_eq!(p.label(), "ExternalRepoOutput(com_google_absl)");
    }

    #[test]
    fn output_policy_label_simple_variants() {
        assert_eq!(
            OutputPathPolicy::WorkspaceRelativeOutput.label(),
            "WorkspaceRelativeOutput"
        );
        assert_eq!(
            OutputPathPolicy::GenfilesOutput.label(),
            "GenfilesOutput"
        );
        assert_eq!(OutputPathPolicy::Unclassified.label(), "Unclassified");
    }

    #[test]
    fn output_policy_external_takes_precedence_over_workspace_relative() {
        // <workspace>/external/<repo>/... should classify as
        // ExternalRepoOutput, NOT WorkspaceRelativeOutput,
        // even though the path technically starts with the
        // workspace root. The external check runs before the
        // workspace-relative check in classify_output_path.
        let p = PathBuf::from(
            "/home/user/workspace/external/some_repo/file.o",
        );
        let policy = classify_output_path(&p, &ws(), &empty_external());
        match policy {
            OutputPathPolicy::ExternalRepoOutput { repo } => {
                assert_eq!(repo, "some_repo");
            }
            other => panic!(
                "expected ExternalRepoOutput, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn output_policy_genfiles_takes_precedence_over_workspace_relative() {
        // genfiles is a sub-shape of workspace-relative; the
        // sub-shape check fires first so cache-key
        // normalization can route them differently.
        let p = PathBuf::from(
            "/home/user/workspace/rush-out/genfiles/foo.pb.h",
        );
        let policy = classify_output_path(&p, &ws(), &empty_external());
        assert_eq!(policy, OutputPathPolicy::GenfilesOutput);

        let p2 = PathBuf::from("/home/user/workspace/rush-out/foo.o");
        let policy2 = classify_output_path(&p2, &ws(), &empty_external());
        assert_eq!(policy2, OutputPathPolicy::WorkspaceRelativeOutput);
    }

    #[test]
    fn classified_output_round_trips_path_and_policy() {
        // ClassifiedOutput just pairs the two; pin the field
        // names + visibility so future format functions can
        // depend on them.
        let p = PathBuf::from("/home/user/workspace/rush-out/x.o");
        let co = ClassifiedOutput {
            path: p.clone(),
            policy: OutputPathPolicy::WorkspaceRelativeOutput,
        };
        assert_eq!(co.path, p);
        assert_eq!(co.policy, OutputPathPolicy::WorkspaceRelativeOutput);
    }

    // Phase 5.C.4 commit 57: ClassifiedTool tests. Pin the
    // type's existence + field names + that ActionKeySpec's
    // executable field is a ClassifiedTool (not a
    // ClassifiedInput).

    #[test]
    fn classified_tool_round_trips_path_and_class() {
        let p = PathBuf::from("/usr/bin/g++");
        let t = ClassifiedTool {
            path: p.clone(),
            class: HermeticityClass::HostToolPath,
        };
        assert_eq!(t.path, p);
        assert_eq!(t.class, HermeticityClass::HostToolPath);
    }

    #[test]
    fn action_key_spec_executable_is_classified_tool() {
        // Phase 5.C.4 commit 57 contract: ActionKeySpec
        // .executable's TYPE is ClassifiedTool, distinct
        // from ClassifiedInput. The fields are identical;
        // the type ensures the call site can't accidentally
        // mix tools with declared inputs in a future
        // refactor.
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        // Construct a ClassifiedTool from the populated
        // executable; type-check fails at compile time if
        // the field type drifted.
        let exec_check: &ClassifiedTool = &aks.executable;
        assert_eq!(exec_check.path, PathBuf::from("/usr/bin/g++"));
        assert_eq!(exec_check.class, HermeticityClass::HostToolPath);
    }

    #[test]
    fn classified_tool_serializes_with_named_fields() {
        // Phase 5.C.4 commit 57: ClassifiedTool's serde
        // shape matches ClassifiedInput's (path + class).
        let t = ClassifiedTool {
            path: PathBuf::from("/usr/bin/g++"),
            class: HermeticityClass::HostToolPath,
        };
        let json = serde_json::to_string(&t).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        assert!(parsed["path"].is_string());
        assert!(parsed["class"].is_string());
        // Phase 5.C.4 commit 54b: snake_case enum.
        assert_eq!(parsed["class"], "host_tool_path");
    }

    // Phase 5.C.4 commit 42: ActionKeySpec.classified_outputs
    // wire-up tests. These pin the contract that
    // from_normalized_command_spec populates the parallel
    // Vec, that order matches declared_outputs, that
    // outputs_all_normalizable() returns the expected value,
    // and that format_action_key_spec includes the policy
    // labels in its dump.

    #[test]
    fn from_spec_populates_classified_outputs_in_order() {
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec!["-c".to_string()],
            env: Vec::new(),
            declared_inputs: vec![PathBuf::from("/home/user/workspace/src.cc")],
            declared_outputs: vec![
                PathBuf::from("/home/user/workspace/rush-out/foo.o"),
                PathBuf::from(
                    "/home/user/workspace/rush-out/genfiles/bar.pb.h",
                ),
            ],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        // Length matches.
        assert_eq!(aks.classified_outputs.len(), 2);
        assert_eq!(aks.declared_outputs.len(), 2);
        // Order matches.
        assert_eq!(aks.classified_outputs[0].path, aks.declared_outputs[0]);
        assert_eq!(aks.classified_outputs[1].path, aks.declared_outputs[1]);
        // Each output classified per OutputPathPolicy rules.
        assert_eq!(
            aks.classified_outputs[0].policy,
            OutputPathPolicy::WorkspaceRelativeOutput
        );
        assert_eq!(
            aks.classified_outputs[1].policy,
            OutputPathPolicy::GenfilesOutput
        );
    }

    #[test]
    fn outputs_all_normalizable_is_true_for_all_classified() {
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![
                PathBuf::from("/home/user/workspace/rush-out/a.o"),
                PathBuf::from(
                    "/home/user/workspace/rush-out/genfiles/b.pb.h",
                ),
            ],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        assert!(aks.outputs_all_normalizable());
    }

    #[test]
    fn outputs_all_normalizable_is_false_when_any_unclassified() {
        // /tmp/foo.o is outside the workspace + not in any
        // registered external repo → Unclassified → trips the
        // outputs_all_normalizable() = false.
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![
                PathBuf::from("/home/user/workspace/rush-out/a.o"),
                PathBuf::from("/tmp/something_weird.o"),
            ],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        assert!(!aks.outputs_all_normalizable());
        // Position 0 is fine; position 1 is the problem.
        assert!(aks.classified_outputs[0].policy.is_normalizable());
        assert!(!aks.classified_outputs[1].policy.is_normalizable());
    }

    #[test]
    fn outputs_all_normalizable_is_true_for_empty_outputs() {
        // Vacuous: no outputs → all of zero outputs are
        // normalizable → true. The .all() iterator returns
        // true for empty input, which matches what callers
        // need (an action with no declared outputs shouldn't
        // be blocked by output classification).
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        assert!(aks.outputs_all_normalizable());
        assert!(aks.classified_outputs.is_empty());
    }

    #[test]
    fn from_spec_classifies_external_repo_output() {
        let mut external = HashMap::new();
        external.insert(
            "absl_sibling".to_string(),
            PathBuf::from("/opt/absl_sibling"),
        );
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![PathBuf::from(
                "/opt/absl_sibling/absl/foo.o",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &external,
        );
        match &aks.classified_outputs[0].policy {
            OutputPathPolicy::ExternalRepoOutput { repo } => {
                assert_eq!(repo, "absl_sibling");
            }
            other => panic!("expected ExternalRepoOutput, got {:?}", other),
        }
    }

    #[test]
    fn format_dump_includes_output_policy_labels() {
        // Pin that the format_action_key_spec output includes
        // the policy label in brackets after each output path,
        // and that outputs_all_normalizable shows up in the
        // summary section.
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![
                PathBuf::from("/home/user/workspace/rush-out/foo.o"),
                PathBuf::from(
                    "/home/user/workspace/rush-out/genfiles/bar.pb.h",
                ),
            ],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        let dump = format_action_key_spec(&aks).join("\n");
        assert!(
            dump.contains("foo.o [WorkspaceRelativeOutput]"),
            "dump:\n{}",
            dump
        );
        assert!(
            dump.contains("bar.pb.h [GenfilesOutput]"),
            "dump:\n{}",
            dump
        );
        assert!(
            dump.contains("outputs_all_normalizable: true"),
            "dump:\n{}",
            dump
        );
    }

    #[test]
    fn format_dump_marks_unclassified_outputs_in_summary() {
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![PathBuf::from("/tmp/random.o")],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        let dump = format_action_key_spec(&aks).join("\n");
        assert!(
            dump.contains("random.o [Unclassified]"),
            "dump:\n{}",
            dump
        );
        assert!(
            dump.contains("outputs_all_normalizable: false"),
            "dump:\n{}",
            dump
        );
    }

    // Phase 5.C.4 commit 51: ActionKeySpec::diff tests. Pin
    // per-field divergence reporting + the empty-when-equal
    // contract.

    fn make_aks(
        argv: Vec<&str>,
        inputs: Vec<&str>,
        outputs: Vec<&str>,
    ) -> ActionKeySpec {
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
            declared_inputs: inputs.iter().map(PathBuf::from).collect(),
            declared_outputs: outputs.iter().map(PathBuf::from).collect(),
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        )
    }

    #[test]
    fn diff_returns_empty_for_identical_specs() {
        let a = make_aks(
            vec!["-c", "src.cc"],
            vec!["/home/user/workspace/src.cc"],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let b = a.clone();
        let d = a.diff(&b);
        assert!(d.is_empty(), "expected empty diff, got {:?}", d);
    }

    #[test]
    fn diff_reports_argv_first_index() {
        // Phase 5.C.4 commit 59: typed Argv variant with
        // first_index payload. Per Pro: "argv_divergence_reports_first_index".
        let a = make_aks(
            vec!["-c", "src.cc"],
            vec![],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let b = make_aks(
            vec!["-c", "other.cc"],
            vec![],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let d = a.diff(&b);
        assert!(d.iter().any(|x| matches!(
            x,
            ActionKeyDivergence::Argv { first_index: 1, legacy: Some(l), scheduler: Some(s) }
                if l == "src.cc" && s == "other.cc"
        )));
    }

    #[test]
    fn diff_reports_argv_with_one_side_missing() {
        // Length mismatch: legacy has 2, scheduler has 3.
        // Argv variant fires at first_index=2 with
        // legacy=None, scheduler=Some.
        let a = make_aks(
            vec!["-c", "src.cc"],
            vec![],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let b = make_aks(
            vec!["-c", "src.cc", "-O2"],
            vec![],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let d = a.diff(&b);
        assert!(d.iter().any(|x| matches!(
            x,
            ActionKeyDivergence::Argv { first_index: 2, legacy: None, scheduler: Some(s) }
                if s == "-O2"
        )));
    }

    #[test]
    fn diff_reports_input_classification_change_via_typed_variant() {
        let mut a = make_aks(
            vec![],
            vec!["/home/user/workspace/src.cc"],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let mut b = a.clone();
        b.declared_inputs[0].class = HermeticityClass::HostAmbientInclude;
        a.declared_inputs[0].class = HermeticityClass::WorkspaceLocal;
        let d = a.diff(&b);
        assert!(d.iter().any(|x| matches!(
            x,
            ActionKeyDivergence::ClassifiedInput { path, legacy, scheduler }
                if path.contains("src.cc")
                    && legacy == "WorkspaceLocal"
                    && scheduler == "HostAmbientInclude"
        )));
    }

    #[test]
    fn diff_reports_executable_path_or_class_change() {
        let mut a = make_aks(
            vec![],
            vec![],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let mut b = a.clone();
        b.executable.path = PathBuf::from("/usr/bin/clang++");
        let d = a.diff(&b);
        assert!(d.iter().any(|x| matches!(
            x,
            ActionKeyDivergence::Executable { legacy, scheduler }
                if legacy.contains("g++") && scheduler.contains("clang++")
        )));
        // Reset, change just the class.
        b = a.clone();
        b.executable.class = HermeticityClass::WorkspaceLocal;
        a.executable.class = HermeticityClass::HostToolPath;
        let d2 = a.diff(&b);
        assert!(d2.iter().any(|x| matches!(
            x,
            ActionKeyDivergence::Executable { legacy, scheduler }
                if legacy.contains("HostToolPath")
                    && scheduler.contains("WorkspaceLocal")
        )));
    }

    #[test]
    fn diff_reports_cwd_some_vs_none() {
        let a = make_aks(
            vec![],
            vec![],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let mut b = a.clone();
        b.cwd = Some(ClassifiedInput {
            path: PathBuf::from("/home/user/workspace"),
            class: HermeticityClass::WorkspaceLocal,
        });
        let d = a.diff(&b);
        assert!(d.iter().any(|x| matches!(
            x,
            ActionKeyDivergence::Cwd { legacy, scheduler }
                if legacy == "<exec_root>" && scheduler.contains("WorkspaceLocal")
        )));
    }

    #[test]
    fn diff_reports_cacheability_when_inputs_diverge() {
        let mut a = make_aks(
            vec![],
            vec!["/home/user/workspace/src.cc"],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let mut b = a.clone();
        a.declared_inputs[0].class = HermeticityClass::WorkspaceLocal;
        a.executable.class = HermeticityClass::WorkspaceLocal;
        b.declared_inputs[0].class = HermeticityClass::Unclassified;
        b.executable.class = HermeticityClass::WorkspaceLocal;
        let d = a.diff(&b);
        assert!(d.iter().any(|x| matches!(
            x,
            ActionKeyDivergence::Cacheability { .. }
        )));
    }

    #[test]
    fn diff_reports_declared_output_side_only() {
        // Phase 5.C.4 commit 59: per Pro, paths-on-one-side-
        // only emit DeclaredOutput { path, side: ... }.
        let a = make_aks(
            vec![],
            vec![],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let b = make_aks(
            vec![],
            vec![],
            vec![
                "/home/user/workspace/rush-out/foo.o",
                "/home/user/workspace/rush-out/bar.o",
            ],
        );
        let d = a.diff(&b);
        assert!(d.iter().any(|x| matches!(
            x,
            ActionKeyDivergence::DeclaredOutput { path, side: DiffSide::SchedulerOnly }
                if path.contains("bar.o")
        )));
    }

    #[test]
    fn diff_reports_env_value_change_via_key() {
        // Phase 5.C.4 commit 59: per Pro,
        // "env_divergence_reports_key".
        let mut a = make_aks(
            vec![],
            vec![],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let mut b = a.clone();
        a.env.push(("CC".to_string(), "g++".to_string()));
        b.env.push(("CC".to_string(), "clang++".to_string()));
        let d = a.diff(&b);
        assert!(d.iter().any(|x| matches!(
            x,
            ActionKeyDivergence::Env { key, legacy: Some(l), scheduler: Some(s) }
                if key == "CC" && l == "g++" && s == "clang++"
        )));
    }

    #[test]
    fn diff_reports_env_key_only_on_one_side() {
        let a = make_aks(
            vec![],
            vec![],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let mut b = a.clone();
        b.env.push(("EXTRA".to_string(), "1".to_string()));
        let d = a.diff(&b);
        assert!(d.iter().any(|x| matches!(
            x,
            ActionKeyDivergence::Env { key, legacy: None, scheduler: Some(s) }
                if key == "EXTRA" && s == "1"
        )));
    }

    #[test]
    fn diff_serializes_to_json_per_pro_spec() {
        // Phase 5.C.4 commit 59: per Pro,
        // "cacheability_divergence_serializes" +
        // "json_divergence_output_stable".
        let div = ActionKeyDivergence::Cacheability {
            legacy: "RemoteCacheable".to_string(),
            scheduler: "NonCacheable".to_string(),
        };
        let json = serde_json::to_string(&div).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["kind"], "cacheability");
        assert_eq!(parsed["data"]["legacy"], "RemoteCacheable");
        assert_eq!(parsed["data"]["scheduler"], "NonCacheable");

        let argv = ActionKeyDivergence::Argv {
            first_index: 3,
            legacy: Some("foo".to_string()),
            scheduler: None,
        };
        let json2 = serde_json::to_string(&argv).expect("serialize");
        let parsed2: serde_json::Value =
            serde_json::from_str(&json2).expect("parse");
        assert_eq!(parsed2["kind"], "argv");
        assert_eq!(parsed2["data"]["first_index"], 3);
        assert_eq!(parsed2["data"]["legacy"], "foo");
        assert!(parsed2["data"]["scheduler"].is_null());

        let dout = ActionKeyDivergence::DeclaredOutput {
            path: "/p/x.o".to_string(),
            side: DiffSide::LegacyOnly,
        };
        let json3 = serde_json::to_string(&dout).expect("serialize");
        let parsed3: serde_json::Value =
            serde_json::from_str(&json3).expect("parse");
        assert_eq!(parsed3["kind"], "declared_output");
        assert_eq!(parsed3["data"]["path"], "/p/x.o");
        assert_eq!(parsed3["data"]["side"], "legacy_only");
    }

    /// Phase 5.C.4 commit 68: env_policy divergence appears
    /// in the typed `ActionKeyDivergence::diff` output.
    /// Pro's commit-68 directive: "Otherwise one part of
    /// the system speaks typed diagnostics and this one
    /// corner goes back to stringly-shaped cave scribbles."
    #[test]
    fn env_policy_divergence_is_typed() {
        let mut a = make_aks(
            vec!["-c"],
            vec![],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let mut b = a.clone();
        a.env_policy = crate::ids::ActionEnvPolicy::InheritHost;
        b.env_policy = crate::ids::ActionEnvPolicy::ExplicitOnly;
        let d = a.diff(&b);
        assert!(d.iter().any(|x| matches!(
            x,
            ActionKeyDivergence::EnvPolicy {
                legacy: crate::ids::ActionEnvPolicy::InheritHost,
                scheduler: crate::ids::ActionEnvPolicy::ExplicitOnly,
            }
        )));
    }

    /// Phase 5.C.4 commit 68: `ActionKeyDivergence::EnvPolicy`
    /// serializes with the JSON shape we promise CI consumers.
    /// Pro: schema_version=1, snake_case, tag/content.
    #[test]
    fn json_serializes_env_policy_divergence() {
        let div = ActionKeyDivergence::EnvPolicy {
            legacy: crate::ids::ActionEnvPolicy::InheritHost,
            scheduler: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let json = serde_json::to_string(&div).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["kind"], "env_policy");
        assert_eq!(parsed["data"]["legacy"], "inherit_host");
        assert_eq!(parsed["data"]["scheduler"], "explicit_only");
    }

    /// Phase 5.C.4 commit 68: human formatter prints the
    /// env_policy variant in the same snake_case form as the
    /// JSON encoding — so operator output matches machine
    /// output line-for-line.
    #[test]
    fn format_action_key_divergences_renders_env_policy() {
        let divs = vec![ActionKeyDivergence::EnvPolicy {
            legacy: crate::ids::ActionEnvPolicy::InheritHost,
            scheduler: crate::ids::ActionEnvPolicy::ExplicitOnly,
        }];
        let rendered = format_action_key_divergences(&divs);
        assert_eq!(rendered.len(), 1);
        assert!(
            rendered[0].contains("env_policy:")
                && rendered[0].contains("inherit_host")
                && rendered[0].contains("explicit_only"),
            "expected env_policy line; got {:?}",
            rendered[0]
        );
    }

    /// Phase 5.C.4 commit 68: matching env_policy on both
    /// sides yields no `EnvPolicy` divergence. Negative
    /// pin.
    #[test]
    fn env_policy_match_no_divergence_in_typed_diff() {
        let a = make_aks(
            vec!["-c"],
            vec![],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let b = a.clone();
        // both default to whatever make_aks sets — same on
        // both sides
        let d = a.diff(&b);
        assert!(!d
            .iter()
            .any(|x| matches!(x, ActionKeyDivergence::EnvPolicy { .. })));
    }

    #[test]
    fn format_action_key_divergences_renders_human_strings() {
        let divs = vec![
            ActionKeyDivergence::Mnemonic {
                legacy: "A".to_string(),
                scheduler: "B".to_string(),
            },
            ActionKeyDivergence::Argv {
                first_index: 1,
                legacy: Some("foo".to_string()),
                scheduler: Some("bar".to_string()),
            },
            ActionKeyDivergence::Argv {
                first_index: 2,
                legacy: None,
                scheduler: Some("-O2".to_string()),
            },
            ActionKeyDivergence::Env {
                key: "CC".to_string(),
                legacy: Some("g++".to_string()),
                scheduler: None,
            },
            ActionKeyDivergence::DeclaredInput {
                path: "/p/x.cc".to_string(),
                side: DiffSide::SchedulerOnly,
            },
            ActionKeyDivergence::Cacheability {
                legacy: "RemoteCacheable".to_string(),
                scheduler: "NonCacheable".to_string(),
            },
        ];
        let lines = format_action_key_divergences(&divs);
        assert_eq!(lines.len(), 6);
        assert!(lines[0].contains("mnemonic: A vs B"));
        assert!(lines[1].contains("argv[1]: foo vs bar"));
        assert!(lines[2].contains("argv[2]: <missing> vs -O2"));
        assert!(lines[3].contains("env[CC]: g++ vs <missing>"));
        assert!(lines[4].contains("declared_inputs: /p/x.cc (scheduler only)"));
        assert!(lines[5].contains("cacheability: RemoteCacheable vs NonCacheable"));
    }

    // Phase 5.C.4 commit 53: serde::Serialize derive tests.
    // Pin: ActionKeySpec + its component types serialize
    // without errors and produce JSON with the expected
    // top-level keys.

    #[test]
    fn serde_serializes_hermeticity_class_as_snake_case_variant() {
        // Phase 5.C.4 commit 54b: rename_all = "snake_case"
        // means JSON consumers see "workspace_local" rather
        // than "WorkspaceLocal". Pin the contract.
        let json = serde_json::to_string(&HermeticityClass::WorkspaceLocal)
            .expect("serialize");
        assert_eq!(json, "\"workspace_local\"");
        let json2 = serde_json::to_string(&HermeticityClass::HostToolPath)
            .expect("serialize");
        assert_eq!(json2, "\"host_tool_path\"");
        let json3 = serde_json::to_string(&HermeticityClass::HostAmbientInclude)
            .expect("serialize");
        assert_eq!(json3, "\"host_ambient_include\"");
        let json4 =
            serde_json::to_string(&HermeticityClass::EnvOverrideToolPath)
                .expect("serialize");
        assert_eq!(json4, "\"env_override_tool_path\"");
        let json5 = serde_json::to_string(&HermeticityClass::Unclassified)
            .expect("serialize");
        assert_eq!(json5, "\"unclassified\"");
    }

    #[test]
    fn serde_serializes_cacheability_class_as_snake_case_variant() {
        // Phase 5.C.4 commit 54b: CacheabilityClass also
        // normalized to snake_case.
        // Phase 5.C.4 commit 58: was Cacheability (enum) →
        // now CacheabilityClass (the enum), wrapped by the
        // new Cacheability struct.
        let json = serde_json::to_string(&CacheabilityClass::RemoteCacheable)
            .expect("serialize");
        assert_eq!(json, "\"remote_cacheable\"");
        let json2 = serde_json::to_string(&CacheabilityClass::LocalHostCacheable)
            .expect("serialize");
        assert_eq!(json2, "\"local_host_cacheable\"");
        let json3 = serde_json::to_string(&CacheabilityClass::NonCacheable)
            .expect("serialize");
        assert_eq!(json3, "\"non_cacheable\"");
    }

    // Phase 5.C.4 commit 58: Cacheability struct + reasons
    // tests.

    #[test]
    fn cacheability_safe_starts_clean() {
        let c = Cacheability::safe();
        assert_eq!(c.class, CacheabilityClass::RemoteCacheable);
        assert!(c.reasons.is_empty());
        assert!(c.is_remote_cacheable());
    }

    #[test]
    fn cacheability_add_reason_demotes_class() {
        let mut c = Cacheability::safe();
        c.add_reason(CacheabilityReason::HostAmbientInclude);
        assert_eq!(c.class, CacheabilityClass::LocalHostCacheable);
        c.add_reason(CacheabilityReason::UnclassifiedInput);
        assert_eq!(c.class, CacheabilityClass::NonCacheable);
        // Adding RemoteCacheable-class-implying reason
        // doesn't reverse the worse class.
        let mut c2 = Cacheability::safe();
        c2.add_reason(CacheabilityReason::HostToolPath);
        c2.add_reason(CacheabilityReason::HostAmbientInclude);
        assert_eq!(c2.class, CacheabilityClass::LocalHostCacheable);
        assert_eq!(c2.reasons.len(), 2);
    }

    #[test]
    fn classify_env_var_categorizes_known_keys() {
        assert_eq!(
            classify_env_var("PATH"),
            Some(CacheabilityReason::HostPathEnv)
        );
        assert_eq!(
            classify_env_var("LD_LIBRARY_PATH"),
            Some(CacheabilityReason::HostPathEnv)
        );
        assert_eq!(
            classify_env_var("CXX"),
            Some(CacheabilityReason::HostPathEnv)
        );
        assert_eq!(
            classify_env_var("RUSH_GRPC_CPP_PLUGIN"),
            Some(CacheabilityReason::EnvOverrideToolPath)
        );
        assert_eq!(
            classify_env_var("RUSH_FOO_PATH"),
            Some(CacheabilityReason::EnvOverrideToolPath)
        );
        // Unknown vars classify as Unclassified (strict
        // default per Pro: "False caution is better than
        // reusing poisoned cache outputs").
        assert_eq!(
            classify_env_var("MY_RANDOM_VAR"),
            Some(CacheabilityReason::UnclassifiedEnv)
        );
    }

    #[test]
    fn action_cacheability_demoted_by_env_unclassified() {
        // Pro's directive: "Mark action non-remote-cacheable
        // if env contains unclassified or host-path-sensitive
        // variables."
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/home/user/workspace/tools/g++"),
            cwd: None,
            argv: vec![],
            env: vec![("FOO".to_string(), "bar".to_string())],
            declared_inputs: vec![PathBuf::from(
                "/home/user/workspace/src.cc",
            )],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/obj.o",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        let c = aks.cacheability();
        // Unclassified env → NonCacheable (strict default).
        assert_eq!(c.class, CacheabilityClass::NonCacheable);
        assert!(c.reasons.contains(&CacheabilityReason::UnclassifiedEnv));
    }

    #[test]
    fn action_cacheability_demoted_by_host_path_env() {
        // Env carrying PATH demotes to LocalHostCacheable
        // (not NonCacheable like UnclassifiedEnv would).
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/home/user/workspace/tools/g++"),
            cwd: None,
            argv: vec![],
            env: vec![("PATH".to_string(), "/usr/bin".to_string())],
            declared_inputs: vec![PathBuf::from(
                "/home/user/workspace/src.cc",
            )],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/obj.o",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        let c = aks.cacheability();
        assert_eq!(c.class, CacheabilityClass::LocalHostCacheable);
        assert!(c.reasons.contains(&CacheabilityReason::HostPathEnv));
    }

    #[test]
    fn action_cacheability_pure_workspace_with_empty_env_is_remote() {
        // No env entries at all → no env-derived reasons →
        // pure-workspace action is RemoteCacheable.
        let spec = NormalizedCommandSpec {
            mnemonic: "CcArchive".to_string(),
            executable: PathBuf::from("/home/user/workspace/tools/ar"),
            cwd: None,
            argv: vec!["rcs".to_string()],
            env: Vec::new(),
            declared_inputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/foo.o",
            )],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/libfoo.a",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        let c = aks.cacheability();
        assert_eq!(c.class, CacheabilityClass::RemoteCacheable);
        assert!(c.reasons.is_empty());
    }

    #[test]
    fn serde_serializes_output_path_policy_with_tagged_payload() {
        // Phase 5.C.4 commit 54b: tag = "kind", content = "data"
        // on a payload enum produces:
        //   unit variant:   {"kind": "...", "data": null}
        //   struct variant: {"kind": "...", "data": {<fields>}}
        let policy = OutputPathPolicy::ExternalRepoOutput {
            repo: "absl".to_string(),
        };
        let json = serde_json::to_string(&policy).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["kind"], "external_repo_output");
        assert_eq!(parsed["data"]["repo"], "absl");

        let unit = OutputPathPolicy::WorkspaceRelativeOutput;
        let json2 = serde_json::to_string(&unit).expect("serialize");
        let parsed2: serde_json::Value =
            serde_json::from_str(&json2).expect("parse");
        assert_eq!(parsed2["kind"], "workspace_relative_output");
        // Unit variants serialize with data = null under the
        // tag/content combo.
        assert!(parsed2["data"].is_null());

        let unit2 = OutputPathPolicy::GenfilesOutput;
        let json3 = serde_json::to_string(&unit2).expect("serialize");
        let parsed3: serde_json::Value =
            serde_json::from_str(&json3).expect("parse");
        assert_eq!(parsed3["kind"], "genfiles_output");

        let unit3 = OutputPathPolicy::Unclassified;
        let json4 = serde_json::to_string(&unit3).expect("serialize");
        let parsed4: serde_json::Value =
            serde_json::from_str(&json4).expect("parse");
        assert_eq!(parsed4["kind"], "unclassified");
    }

    #[test]
    fn serde_serializes_full_action_key_spec() {
        let spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec!["-c".to_string(), "src.cc".to_string()],
            env: vec![("CC".to_string(), "g++".to_string())],
            declared_inputs: vec![PathBuf::from("/home/user/workspace/src.cc")],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/foo.o",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec,
            &ws(),
            &[],
            &empty_external(),
        );
        let json =
            serde_json::to_string(&aks).expect("ActionKeySpec serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("re-parse");
        assert_eq!(parsed["mnemonic"], "CppCompile");
        assert!(parsed["executable"].is_object());
        assert!(parsed["argv"].is_array());
        assert!(parsed["declared_inputs"].is_array());
        assert!(parsed["declared_outputs"].is_array());
        assert!(parsed["classified_outputs"].is_array());
        // Each input is {path, class}. class is a snake_case
        // string (commit 54b).
        assert!(parsed["declared_inputs"][0]["path"].is_string());
        assert!(parsed["declared_inputs"][0]["class"].is_string());
        // Each classified output is {path, policy}. Phase
        // 5.C.4 commit 54b: policy is now a tagged enum
        // object {kind, data}, not a bare string.
        assert!(parsed["classified_outputs"][0]["path"].is_string());
        assert!(parsed["classified_outputs"][0]["policy"].is_object());
        assert!(parsed["classified_outputs"][0]["policy"]["kind"]
            .is_string());
    }

    #[test]
    fn serde_action_key_explain_entry_includes_schema_version() {
        // Phase 5.C.4 commit 54: the wrapper used by the
        // orchestrator's RUSH_EXPLAIN_ACTION_KEYS_JSON=1 path
        // adds schema_version + nests the spec under .spec.
        let spec_in = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/foo.o",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec_in,
            &ws(),
            &[],
            &empty_external(),
        );
        let entry = ActionKeyExplainEntry::new(&aks);
        let json = serde_json::to_string(&entry).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("re-parse");
        assert_eq!(parsed["schema_version"], 1);
        assert!(parsed["spec"].is_object());
        assert_eq!(parsed["spec"]["mnemonic"], "CppCompile");
    }

    /// Phase 5.C.4 commit 69: `ExplainActionKeysView` parses
    /// the CLI/env-var string forms. Pin the
    /// case-insensitive matching + default behavior.
    #[test]
    fn explain_view_parses_recognized_strings() {
        assert_eq!(
            ExplainActionKeysView::from_str_ci("spec"),
            Some(ExplainActionKeysView::Spec)
        );
        assert_eq!(
            ExplainActionKeysView::from_str_ci("Spec"),
            Some(ExplainActionKeysView::Spec)
        );
        assert_eq!(
            ExplainActionKeysView::from_str_ci("canonical"),
            Some(ExplainActionKeysView::Canonical)
        );
        assert_eq!(
            ExplainActionKeysView::from_str_ci("BOTH"),
            Some(ExplainActionKeysView::Both)
        );
        assert_eq!(ExplainActionKeysView::from_str_ci(""), None);
        assert_eq!(ExplainActionKeysView::from_str_ci("garbage"), None);
        assert_eq!(
            ExplainActionKeysView::default(),
            ExplainActionKeysView::Spec
        );
    }

    /// Phase 5.C.4 commit 69: includes_spec / includes_canonical
    /// helpers route output emission. Pin the truth table.
    #[test]
    fn explain_view_inclusion_predicates() {
        let s = ExplainActionKeysView::Spec;
        assert!(s.includes_spec());
        assert!(!s.includes_canonical());
        let c = ExplainActionKeysView::Canonical;
        assert!(!c.includes_spec());
        assert!(c.includes_canonical());
        let b = ExplainActionKeysView::Both;
        assert!(b.includes_spec());
        assert!(b.includes_canonical());
    }

    /// Phase 5.C.4 commit 69: spec_only_labeled produces
    /// the commit-54 shape plus target/action labels;
    /// canonical/readiness fields are absent.
    #[test]
    fn explain_entry_spec_only_labeled_outputs_spec_with_label() {
        let spec_in = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/foo.o",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec_in,
            &ws(),
            &[],
            &empty_external(),
        );
        let entry = ActionKeyExplainEntry::spec_only_labeled(
            &aks,
            "//pkg:lib".to_string(),
            "CppCompile:foo.o".to_string(),
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&entry).unwrap()).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["target"], "//pkg:lib");
        assert_eq!(parsed["action"], "CppCompile:foo.o");
        assert!(parsed["spec"].is_object());
        assert!(parsed.get("canonical").is_none());
        assert!(parsed.get("readiness").is_none());
    }

    /// Phase 5.C.4 commit 69: canonical_only_labeled
    /// produces canonical + readiness, omits spec.
    #[test]
    fn explain_entry_canonical_only_labeled_omits_spec() {
        let spec_in = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/foo.o",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec_in,
            &ws(),
            &[],
            &empty_external(),
        );
        let (canonical, readiness) =
            crate::canonical_action_key::CanonicalActionKeyInput::from_action_key_spec(&aks);
        let entry = ActionKeyExplainEntry::canonical_only_labeled(
            &canonical,
            &readiness,
            "//pkg:lib".to_string(),
            "CppCompile:foo.o".to_string(),
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&entry).unwrap()).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["target"], "//pkg:lib");
        assert!(parsed.get("spec").is_none(), "view=canonical must omit spec");
        assert!(parsed["canonical"].is_object());
        assert!(parsed["readiness"].is_object());
    }

    /// Phase 5.C.4 commit 69: both_views_labeled populates
    /// every field. Pin that the JSON is parseable +
    /// carries all three views.
    #[test]
    fn explain_entry_both_views_labeled_outputs_spec_and_canonical() {
        let spec_in = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/foo.o",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec_in,
            &ws(),
            &[],
            &empty_external(),
        );
        let (canonical, readiness) =
            crate::canonical_action_key::CanonicalActionKeyInput::from_action_key_spec(&aks);
        let entry = ActionKeyExplainEntry::both_views_labeled(
            &aks,
            &canonical,
            &readiness,
            "//pkg:lib".to_string(),
            "CppCompile:foo.o".to_string(),
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&entry).unwrap()).unwrap();
        assert!(parsed["spec"].is_object());
        assert!(parsed["canonical"].is_object());
        assert!(parsed["readiness"].is_object());
        assert_eq!(parsed["target"], "//pkg:lib");
    }

    /// Phase 5.C.4 commit 84: with_cacheability attaches an
    /// `ActionKeyCacheabilityView` to an existing entry.
    /// JSON output gains a `cacheability` object with
    /// `class`, `reasons`, and `reason_summary` fields.
    /// Schema_version stays at 1 (Pro: additive optional).
    #[test]
    fn action_key_explain_json_contains_cacheability_when_requested() {
        let spec_in = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/foo.o",
            )],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::InheritHost,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec_in,
            &ws(),
            &[],
            &empty_external(),
        );
        let cv = ActionKeyCacheabilityView::from_cacheability(
            &aks.cacheability(),
        );
        let entry = ActionKeyExplainEntry::spec_only_labeled(
            &aks,
            "//pkg:lib".to_string(),
            "CppCompile:foo.o".to_string(),
        )
        .with_cacheability(cv);
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&entry).unwrap())
                .unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert!(parsed["cacheability"].is_object());
        // InheritHost forces NonCacheable via commit 66.
        assert_eq!(parsed["cacheability"]["class"], "non_cacheable");
        // reasons + reason_summary present.
        assert!(parsed["cacheability"]["reasons"].is_array());
        assert!(parsed["cacheability"]["reason_summary"].is_array());
        // At least one reason should be InheritedHostEnv.
        let reasons = parsed["cacheability"]["reasons"]
            .as_array()
            .expect("reasons array");
        assert!(reasons
            .iter()
            .any(|r| r["kind"] == "inherited_host_env"));
    }

    /// Phase 5.C.4 commit 84: existing spec-only shape
    /// (no cacheability attached) still serializes as
    /// before — the `cacheability` field is absent when
    /// not set. Pro's back-compat contract.
    #[test]
    fn existing_spec_only_shape_still_valid() {
        let spec_in = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec_in,
            &ws(),
            &[],
            &empty_external(),
        );
        // No with_cacheability call — field stays None.
        let entry = ActionKeyExplainEntry::new(&aks);
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&entry).unwrap())
                .unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert!(parsed["spec"].is_object());
        assert!(
            parsed.get("cacheability").is_none(),
            "cacheability field must be absent when not set; got {:?}",
            parsed
        );
    }

    /// Phase 5.C.4 commit 84: `ActionKeyCacheabilityView::from_cacheability`
    /// maps `CacheabilityClass` to snake_case labels. Pin
    /// each variant.
    #[test]
    fn cacheability_view_class_labels_are_snake_case() {
        let mut c = Cacheability::safe();
        let v = ActionKeyCacheabilityView::from_cacheability(&c);
        assert_eq!(v.class, "remote_cacheable");
        c.add_reason(CacheabilityReason::HostToolPath);
        let v = ActionKeyCacheabilityView::from_cacheability(&c);
        assert_eq!(v.class, "local_host_cacheable");
        c.add_reason(CacheabilityReason::InheritedHostEnv);
        let v = ActionKeyCacheabilityView::from_cacheability(&c);
        assert_eq!(v.class, "non_cacheable");
    }

    /// Phase 5.C.4 commit 84: `reason_summary` field
    /// collapses duplicate reasons. Pins commit 72's
    /// summary semantics flow through to the JSON shape.
    #[test]
    fn cacheability_view_reason_summary_collapses_duplicates() {
        let mut c = Cacheability::safe();
        c.add_reason(CacheabilityReason::HostToolPath);
        c.add_reason(CacheabilityReason::HostToolPath);
        c.add_reason(CacheabilityReason::HostToolPath);
        c.add_reason(CacheabilityReason::InheritedHostEnv);
        let v = ActionKeyCacheabilityView::from_cacheability(&c);
        let json = serde_json::to_string(&v).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        // Raw reasons: 4 entries.
        assert_eq!(parsed["reasons"].as_array().unwrap().len(), 4);
        // Summary: 2 distinct entries.
        let summary = parsed["reason_summary"].as_array().unwrap();
        assert_eq!(summary.len(), 2);
        let host = summary
            .iter()
            .find(|s| s["reason"]["kind"] == "host_tool_path")
            .unwrap();
        assert_eq!(host["count"], 3);
    }

    /// Phase 5.C.4 commit 69: back-compat — the commit-54
    /// `new(spec)` constructor still produces the original
    /// shape (no target/action/canonical/readiness fields).
    #[test]
    fn explain_entry_back_compat_new_constructor_unchanged() {
        let spec_in = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: vec![],
            env: Vec::new(),
            declared_inputs: vec![],
            declared_outputs: vec![],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        };
        let aks = ActionKeySpec::from_normalized_command_spec(
            &spec_in,
            &ws(),
            &[],
            &empty_external(),
        );
        let entry = ActionKeyExplainEntry::new(&aks);
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&entry).unwrap()).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert!(parsed["spec"].is_object());
        assert!(parsed.get("target").is_none());
        assert!(parsed.get("action").is_none());
        assert!(parsed.get("canonical").is_none());
        assert!(parsed.get("readiness").is_none());
    }

    #[test]
    fn diff_reports_classified_output_policy_change() {
        let mut a = make_aks(
            vec![],
            vec![],
            vec!["/home/user/workspace/rush-out/foo.o"],
        );
        let mut b = a.clone();
        a.classified_outputs[0].policy = OutputPathPolicy::WorkspaceRelativeOutput;
        b.classified_outputs[0].policy = OutputPathPolicy::ExternalRepoOutput {
            repo: "fake_repo".to_string(),
        };
        let d = a.diff(&b);
        assert!(d.iter().any(|x| matches!(
            x,
            ActionKeyDivergence::ClassifiedOutput { path, legacy, scheduler }
                if path.contains("foo.o")
                    && legacy == "WorkspaceRelativeOutput"
                    && scheduler.contains("ExternalRepoOutput")
        )));
    }
}
