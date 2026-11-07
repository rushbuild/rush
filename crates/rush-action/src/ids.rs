// Rush Action - ID skeleton + artifact registry
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

// Phase 5.A.5 is a skeleton module: types and a registry, no
// consumers yet. The crate-level `#![warn(missing_docs)]` would
// fire on every public field/method here; individual docstrings
// will come when phase 5.B/5.C wire these into rule provider
// construction and the action graph and the contract solidifies.
// Until then, the module-level doc explains intent and the field
// names are self-evident.
#![allow(missing_docs)]

//! ID-based artifact / path / string skeleton (phase 5.A.5).
//!
//! This module introduces `Copy`-able 32-bit handles for the four
//! things the build system passes around in volume: artifacts, file
//! paths, strings, and build configurations. The shapes follow the
//! plan in `docs/item-5-typed-providers-plan.md` §5.A.5:
//!
//! - [`ArtifactId`], [`PathId`], [`StringId`] — `u32` indices into
//!   per-orchestrator registries; `Copy`, 4 bytes, hashable.
//! - [`ConfigId`], [`TargetId`], `ActionId` (already in
//!   `rush-action::lib`) — same shape, semantic role differs.
//! - [`ArtifactRegistry`] — owns the path/string interners and the
//!   artifact table; lives on `BuildOrchestrator` (or per-build
//!   session), **not** as a `static`. ChatGPT review §3 was
//!   explicit on this — global state is annoying for tests and
//!   harms reuse across builds.
//! - [`ArtifactKey`] = (`ConfigId`, `PathId`) so two configurations
//!   with the same exec path don't collide their generated outputs.
//!
//! ## What this phase does NOT do
//!
//! - Rule providers don't use these IDs yet. That's phase 5.B
//!   (typed `CcCompilationContext` / `CcLinkingContext`).
//! - Rule `build()` impls don't go through the registry yet. That's
//!   phase 5.C (action graph, declared inputs/outputs).
//! - The existing `rush-action::Artifact` (Source/Derived) and
//!   `rush-rules::Artifact { path, artifact_type }` are unchanged.
//!   5.A.5 just stands up the new types alongside; later phases
//!   migrate consumers.
//!
//! Phase 5.A.5 is therefore a **skeleton-only** commit. The proof
//! that it's structurally correct is unit tests that exercise:
//! interner dedup, collision-safe `ArtifactKey`, kind-stored-once
//! semantics, source-vs-derived owner_action invariant.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Stable handle for a build configuration (toolchain, compilation
/// mode, target platform, …). Distinct `ConfigId`s isolate artifact
/// outputs that share an exec path but were produced under different
/// settings — without this in [`ArtifactKey`], a Linux opt build
/// and a Linux dbg build would collide on `obj/foo.o`.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default)]
pub struct ConfigId(pub u32);

impl ConfigId {
    /// Sentinel for the unconfigured / single-config case. Phase
    /// 5.C will introduce a real configuration table; until then
    /// every artifact uses this default and the registry behaves
    /// like a path-keyed table.
    pub const DEFAULT: ConfigId = ConfigId(0);
}

/// Stable handle for a build target (the result of analyzing a rule
/// instance — `//foo:bar` becomes a `TargetId`). Distinct from
/// `ActionId`: a target produces zero-or-more actions.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct TargetId(pub u32);

/// Stable handle into the [`PathInterner`]. 4 bytes, `Copy`, cheap
/// to hash. Resolves back to a `&Path` via `PathInterner::path`.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct PathId(pub u32);

/// Stable handle into the [`StringInterner`]. Used for short
/// repeated strings (defines, link flags, env keys) where
/// allocating a `String` per occurrence is wasteful and where
/// equality is compared often.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct StringId(pub u32);

/// Stable handle for a registered artifact. The `ArtifactKey` tuple
/// ensures uniqueness per (configuration, exec path); duplicates
/// hand back the same `ArtifactId`.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct ArtifactId(pub u32);

/// Stable handle for a registered linker input. References from
/// providers (via NestedSet/OrderedNestedSet) carry just the 4-byte
/// `LinkerInputId`; the rich `LinkerInputData` lives once in the
/// registry. This shape is ChatGPT review §4: future-proof linker
/// inputs without a callsite-churn migration when fields like
/// `additional_inputs` and `per_input_link_flags` become needed.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct LinkerInputId(u32);

/// Stable handle for a declared action (phase 5.C0).
///
/// Distinct from `ActionId` (the content-addressed BLAKE3-based
/// identifier in `rush_action::lib`): `DeclaredActionId` is a
/// per-build arena index, used only during analysis to track
/// producer/consumer relationships. The content-addressed
/// `ActionId` comes from phase 5.C when the scheduler hashes the
/// action for caching.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct DeclaredActionId(pub u32);

/// What kind of file an artifact represents. Stored ONCE per
/// artifact (in [`ArtifactData`]) — references to the artifact
/// pass `ArtifactId` only, so the kind isn't duplicated across
/// every NestedSet node that references the same header. This is
/// the shape ChatGPT review §2 called out: don't model artifacts
/// as `{ id, kind }` everywhere; the kind belongs in the registry.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum ArtifactKind {
    /// Source file present in the repository before the build.
    Source,
    /// Header file (.h, .hpp). May be source or generated.
    Header,
    /// Object file produced by a compile (.o).
    Object,
    /// Static library (.a).
    StaticLibrary,
    /// Shared library (.so / .dylib / .dll).
    SharedLibrary,
    /// Executable binary.
    Executable,
    /// Generated text/code/proto file produced by a tool action.
    Generated,
    /// Tree artifact — a directory whose individual entries aren't
    /// declared. Used for things like protoc output dirs where the
    /// rule knows the dir but not every file.
    Directory,
}

/// Identity tuple for [`ArtifactRegistry::intern_artifact`]. Two
/// artifacts with the same exec path but different `ConfigId` get
/// distinct `ArtifactId`s — this is the collision protection
/// ChatGPT review §2 specifically called out.
#[derive(Clone, Eq, PartialEq, Hash, Debug)]
pub struct ArtifactKey {
    pub config: ConfigId,
    pub exec_path: PathId,
}

/// Registry-side data for one artifact. Pulled out of the
/// `Artifact` reference type (which is just `ArtifactId`) so a
/// `NestedSet<ArtifactId>` carries 4-byte handles, not (path +
/// kind) tuples replicated per node.
#[derive(Clone, Debug)]
pub struct ArtifactData {
    /// Path under the workspace exec root.
    pub exec_path: PathId,
    /// What kind of file this is (source/header/object/…). Phase
    /// 5.C0 P3: this field describes file *role*, not origin.
    /// `Header` no longer implies "source-on-disk" — the
    /// `is_source` flag is the only authoritative origin signal.
    pub kind: ArtifactKind,
    /// Phase 5.C0 P3: TRUE for files registered via
    /// `register_source_artifact` (files that exist in the repo
    /// before the build). FALSE for files registered via
    /// `register_generated_artifact` (build outputs of some
    /// action). The previous "skip if kind == Header" heuristic
    /// in producer validation was a blind spot for the
    /// `array_ops.h` class of bug — generated headers are exactly
    /// the artifact class we need validation to catch, not skip.
    /// Validation now keys off `is_source` instead.
    pub is_source: bool,
    /// Which target's analysis registered it.
    pub owner_target: TargetId,
    /// Phase 5.C content-addressed action ID. Reserved for the
    /// future action scheduler when the action graph is finalized
    /// and each action gets its BLAKE3 content hash. Currently
    /// `None` for everything — populated in phase 5.C.
    pub owner_action: Option<crate::ActionId>,
    /// Phase 5.C0 lightweight declared-action handle. Populated
    /// by `ArtifactRegistry::declare_action` when a rule's
    /// analyze() declares the action that produces this artifact.
    /// `None` for source artifacts and for derived artifacts
    /// whose producer hasn't been declared yet (the latter is a
    /// validation error — caught by post-analysis pass 2).
    pub owner_action_declared: Option<DeclaredActionId>,
}

/// Optional extra data on a `LinkerInputData`. Wrapped in
/// `Arc<Option<...>>` so the common case (a plain library with no
/// per-input flags or extra inputs) costs zero extra bytes.
///
/// `additional_inputs` covers things like linkstamp objects, version
/// scripts, or any non-library file the linker must see when this
/// input is consumed. `per_input_link_flags` covers Bazel-style
/// per-LinkerInput user link flags (TF doesn't use these heavily but
/// the shape exists for parity).
#[derive(Debug)]
pub struct LinkerInputExtras {
    pub additional_inputs: Box<[ArtifactId]>,
    pub per_input_link_flags: Box<[StringId]>,
}

/// Registry-side data for one linker input. Same shape principle as
/// `ArtifactData`: every reference is a 4-byte `LinkerInputId`; the
/// rich payload lives once. Per ChatGPT review §4 we add `owner`
/// (TargetId) up front so future producer-validation work (phase
/// 5.C0) can attribute LinkerInputs to their declaring target
/// without a fresh callsite migration.
#[derive(Debug)]
pub struct LinkerInputData {
    /// Target that declared this linker input.
    pub owner: TargetId,
    /// The library/archive artifact (.a, .so).
    pub artifact: ArtifactId,
    /// `-Wl,--whole-archive` / `--no-whole-archive` wrapping.
    pub alwayslink: bool,
    /// Optional rich extras. `None` for the common case (plain
    /// library with no per-input flags); `Some(Arc<...>)` for
    /// linkstamps, version scripts, etc. Using `Arc` so the same
    /// extras can be shared across multiple LinkerInputs that
    /// derive from the same source target.
    pub extras: Option<std::sync::Arc<LinkerInputExtras>>,
}

/// Path interner. Maps `PathBuf -> PathId` with reverse lookup.
/// Same path interned twice gives the same `PathId`.
#[derive(Default, Debug)]
pub struct PathInterner {
    by_path: HashMap<PathBuf, PathId>,
    paths: Vec<PathBuf>,
}

impl PathInterner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern a path. Returns the existing `PathId` if already
    /// registered, or assigns a new one.
    pub fn intern(&mut self, p: &Path) -> PathId {
        if let Some(id) = self.by_path.get(p) {
            return *id;
        }
        let id = PathId(self.paths.len() as u32);
        self.paths.push(p.to_path_buf());
        self.by_path.insert(p.to_path_buf(), id);
        id
    }

    /// Resolve a `PathId` back to its `&Path`. Panics on an ID
    /// that wasn't issued by this interner — a programming error.
    pub fn path(&self, id: PathId) -> &Path {
        &self.paths[id.0 as usize]
    }

    /// Non-mutating reverse lookup: given a path, return its
    /// `PathId` if it has been interned, or `None` otherwise.
    /// Distinct from [`intern`](Self::intern) — does not insert.
    /// Used by debug / query tooling that needs to ask "do we
    /// know about this path?" without growing the interner.
    pub fn lookup(&self, p: &Path) -> Option<PathId> {
        self.by_path.get(p).copied()
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Sum of interned path byte lengths (RUSH_MEM_CENSUS accounting).
    pub fn total_path_bytes(&self) -> usize {
        self.paths.iter().map(|p| p.as_os_str().len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// String interner. Same shape as [`PathInterner`] but for
/// `String` (defines, link flags, env values, mnemonic names).
#[derive(Default, Debug)]
pub struct StringInterner {
    by_str: HashMap<String, StringId>,
    strs: Vec<String>,
}

impl StringInterner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern(&mut self, s: &str) -> StringId {
        if let Some(id) = self.by_str.get(s) {
            return *id;
        }
        let id = StringId(self.strs.len() as u32);
        self.strs.push(s.to_string());
        self.by_str.insert(s.to_string(), id);
        id
    }

    pub fn str(&self, id: StringId) -> &str {
        &self.strs[id.0 as usize]
    }

    pub fn len(&self) -> usize {
        self.strs.len()
    }

    /// Sum of interned string byte lengths (RUSH_MEM_CENSUS accounting).
    pub fn total_str_bytes(&self) -> usize {
        self.strs.iter().map(|s| s.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.strs.is_empty()
    }
}

/// Status of a declared action — phase 5.C0 P4.
///
/// `Complete` means the action's declared inputs and outputs are
/// the full set the scheduler will need to execute it. The
/// scheduler can derive dep edges, hash for caching, and run.
///
/// `Incomplete` is for actions whose declaration is missing
/// something — typically cross-target inputs that haven't been
/// plumbed through the typed-provider path yet (cc_shared_library's
/// link inputs at the time of writing). The validation report
/// counts these separately, and the eventual phase 5.C scheduler
/// will refuse to execute Incomplete actions; legacy inline
/// `build()` execution still runs for them as a fallback.
#[derive(Clone, Debug)]
pub enum ActionDeclarationStatus {
    Complete,
    Incomplete {
        /// Short reason string (e.g. "cross-target link inputs
        /// deferred to phase 5.C"). Surfaces in producer-
        /// validation reports.
        reason: &'static str,
    },
}

/// Phase 5.C0 declared-action skeleton. Carries enough metadata
/// for producer/consumer validation but no command line — that's
/// phase 5.C territory. argv/env/tools all stay on the legacy
/// inline-execution path until the action scheduler lands.
#[derive(Debug)]
pub struct DeclaredAction {
    /// Target whose analyze() declared this action.
    pub owner_target: TargetId,
    /// Action mnemonic (e.g. "CppCompile", "CcLink") interned as
    /// a string ID so census output and validation errors can
    /// resolve it without holding an owned String per action.
    pub mnemonic: StringId,
    /// Declared input artifacts. For 5.C0 these are an advisory
    /// (validation pass: every generated input must have a known
    /// producer). 5.C will use them as the action-graph dep edges.
    pub inputs: Box<[ArtifactId]>,
    /// Declared output artifacts. Each output gets its
    /// `owner_action` slot in the registry set to this action's
    /// `DeclaredActionId`; a duplicate registration is an error.
    pub outputs: Box<[ArtifactId]>,
    /// Phase 5.C0 P4: completeness marker. `Complete` means the
    /// scheduler can execute this action; `Incomplete` means it
    /// can't (something's missing in the declaration) and the
    /// scheduler must fall back to the legacy `build()` path.
    pub status: ActionDeclarationStatus,
}

/// Phase 5.C.4: per-action executable payload.
///
/// Side-table companion to [`DeclaredAction`]. Where DeclaredAction
/// carries the analysis-time skeleton (mnemonic, declared inputs,
/// declared outputs, status), `ExecutableAction` carries what the
/// scheduler needs to actually run the action: the executable
/// path, argv, env, working directory.
///
/// Stored in [`ArtifactRegistry`] as `Arc<ExecutableAction>` keyed
/// by [`DeclaredActionId`] in a sparse side table (only populated
/// for actions whose declaring rule has migrated to the typed
/// execution path). Rules opt in by calling
/// [`ArtifactRegistry::set_executable_action`] during analyze();
/// the phase 5.C.4 scheduler refuses to execute actions whose
/// `executable_action()` slot is empty and falls through to the
/// legacy `rule.build()` path for those.
///
/// All fields use registry-side compact IDs (`PathId`, `StringId`)
/// rather than owned `PathBuf` / `String` per OKR 6's compact-ID
/// principle. The materialized [`crate::Action`] type resolves
/// these IDs back to owned strings at the cache-key /
/// ExecutionStrategy boundary; that resolution is a one-time
/// per-action cost, not per-reference.
///
/// Phase 5.C.5 may collapse this side table into `DeclaredAction`
/// fields (Option A in `docs/item-5-phase-c-status.md`) once every
/// rule populates it; for the migration window the side table
/// keeps `DeclaredAction`'s footprint stable.
#[derive(Clone, Debug)]
pub struct ExecutableAction {
    /// Path to the binary to run (g++, ar, protoc, mlir-tblgen,
    /// …). Interned through the registry's path interner.
    pub executable: PathId,
    /// Command-line arguments as interned strings, in argv order.
    /// Empty if the executable takes no arguments (rare for build
    /// actions).
    pub argv: Box<[StringId]>,
    /// Environment variables as interned (key, value) pairs.
    /// Order is registration order; duplicate keys are not
    /// deduplicated (the scheduler picks one when materializing
    /// — typically the last write wins, matching `Command::env`).
    pub env: Box<[(StringId, StringId)]>,
    /// Working directory for the action. `None` means the build's
    /// exec root (the default for most actions). Used by rules
    /// like protoc that prefer to run from a specific directory
    /// to keep relative `--cpp_out=` paths stable.
    pub working_dir: Option<PathId>,
    /// Phase 5.C.4 commit 66: how the spawned process should
    /// receive its environment. Default `InheritHost` matches
    /// today's `tokio::process::Command::new` behavior (parent
    /// env inherited, declared env overlaid). `ExplicitOnly`
    /// makes the runner call `env_clear()` first, producing a
    /// hermetic env consisting only of declared vars.
    ///
    /// The cacheability calculation in
    /// `ActionKeySpec::cacheability` consults this policy:
    /// `InheritHost` adds [`CacheabilityReason::InheritedHostEnv`]
    /// and demotes the action to `NonCacheable` (we can't
    /// fingerprint an unbounded inherited env). `ExplicitOnly`
    /// allows higher cacheability tiers based on the declared
    /// env contents alone.
    pub env_policy: ActionEnvPolicy,
}

/// Phase 5.C.4 commit 66: how an action's spawned process
/// receives its environment.
///
/// Pro's commit-66 directive: "If `ActionKeySpec.env` says `[]`
/// but `tokio::process::Command` inherits the host environment,
/// then the diagnostic model is lying. A cache model that
/// forgets the action's real environment is not a cache
/// model, it is a cheerful fraud with serialization."
///
/// This enum makes the env-inheritance contract EXPLICIT at
/// every layer (registry, materialized command, spec, runner).
/// The runner consults the policy to decide whether to call
/// `env_clear()`. The diagnostic / cacheability layer
/// consults the policy to decide whether to demote
/// cacheability for inherited host env.
///
/// JSON shape (commit 54b style — snake_case unit-variant
/// enum):
///   "inherit_host"
///   "explicit_only"
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionEnvPolicy {
    /// Process inherits the host environment, plus any
    /// declared env vars overlay it. Today's default
    /// behavior of `tokio::process::Command::new`. Forces
    /// non-remote-cacheability via
    /// `CacheabilityReason::InheritedHostEnv`.
    InheritHost,
    /// Process gets ONLY the declared env vars
    /// (`env_clear()` called before populating). Hermetic
    /// w.r.t. the host's environment; cacheability flows
    /// from the declared env content.
    ExplicitOnly,
}

impl Default for ActionEnvPolicy {
    /// Default: `InheritHost`. Today's behavior is to inherit;
    /// rules that want hermetic env opt into `ExplicitOnly`
    /// explicitly. This matches the principle "make the
    /// existing behavior the default; require opt-in for
    /// stricter semantics."
    fn default() -> Self {
        ActionEnvPolicy::InheritHost
    }
}

/// Phase 5.C.4: an `ExecutableAction` resolved through the
/// registry's path / string interners into owned types ready to
/// hand to a process spawn API. Produced by
/// [`ExecutableAction::materialize`].
///
/// Distinct from the in-registry compact form so the materialization
/// cost (path lookups + string copies) is one-time per scheduled
/// action, not per consumer query. The fields mirror what
/// `tokio::process::Command` (and the existing `rush_action::Command`
/// shape) needs.
#[derive(Clone, Debug)]
pub struct MaterializedCommand {
    /// Resolved executable path.
    pub executable: PathBuf,
    /// Explicit argv[0], or `None` to use the executable path.
    pub argv0: Option<String>,
    /// Resolved argv strings, in argv order.
    pub argv: Vec<String>,
    /// Resolved env (key, value) pairs in registration order.
    /// Duplicate keys are preserved — caller decides last-write-
    /// wins when applying to a process spawn API.
    pub env: Vec<(String, String)>,
    /// Resolved working directory, if set.
    pub working_dir: Option<PathBuf>,
    /// Phase 5.C.4 commit 66: env-inheritance policy. Carried
    /// from the source `ExecutableAction` so the runner
    /// (`LocalActionRunner` and friends) can honor it via
    /// `env_clear()` when set to `ExplicitOnly`.
    pub env_policy: ActionEnvPolicy,
}

impl ExecutableAction {
    /// Resolve compact IDs through the registry's interners into
    /// owned types. Phase 5.C.4 dry-run + execution use this at
    /// the scheduler boundary — a per-action materialization,
    /// not a per-reference one.
    ///
    /// Panics on an ID not issued by this registry (programming
    /// error in the producer rule's analyze() — e.g. an argv
    /// `StringId` that doesn't come from this registry's string
    /// interner). Production-ready material always satisfies this
    /// invariant by construction (the same registry that
    /// registered the action issued every ID it carries).
    pub fn materialize(&self, reg: &ArtifactRegistry) -> MaterializedCommand {
        let executable = reg.paths().path(self.executable).to_path_buf();
        let argv: Vec<String> = self
            .argv
            .iter()
            .map(|sid| reg.strings().str(*sid).to_string())
            .collect();
        let env: Vec<(String, String)> = self
            .env
            .iter()
            .map(|(k, v)| {
                (
                    reg.strings().str(*k).to_string(),
                    reg.strings().str(*v).to_string(),
                )
            })
            .collect();
        let working_dir = self
            .working_dir
            .map(|pid| reg.paths().path(pid).to_path_buf());
        MaterializedCommand {
            executable,
            argv0: None,
            argv,
            env,
            working_dir,
            env_policy: self.env_policy,
        }
    }
}

/// Errors from `ArtifactRegistry::declare_action` and the
/// post-analysis validation passes. Surfaces the artifact /
/// action identity so the orchestrator can format a useful
/// message ("target X declared an action that produces Y, but
/// target Z's earlier action already produced Y" — phase 5.C0
/// catches the array_ops.h class of bug at analysis time
/// instead of three layers deep at link time).
#[derive(Debug, Clone)]
pub enum ProducerError {
    /// Two actions claim the same generated `ArtifactId` as an
    /// output. Pass 1 of the four producer/consumer invariants.
    DuplicateProducer {
        artifact: ArtifactId,
        existing: DeclaredActionId,
        attempted: DeclaredActionId,
    },
    /// An action declared a source artifact (`owner_action == None`
    /// in `ArtifactData`) as one of its outputs. Pass 4. Source
    /// files are inputs only; emitting them as outputs would
    /// confuse the action scheduler about producer ownership.
    SourceArtifactAsOutput {
        artifact: ArtifactId,
        action: DeclaredActionId,
    },
}

impl std::fmt::Display for ProducerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProducerError::DuplicateProducer {
                artifact,
                existing,
                attempted,
            } => write!(
                f,
                "duplicate producer: artifact {:?} already produced by action {:?}, \
                 attempted re-registration by action {:?}",
                artifact, existing, attempted
            ),
            ProducerError::SourceArtifactAsOutput { artifact, action } => write!(
                f,
                "action {:?} declared source artifact {:?} as an output (sources have no producer)",
                action, artifact
            ),
        }
    }
}

impl std::error::Error for ProducerError {}

/// Per-build artifact + path + string registry.
///
/// Owns [`PathInterner`] / [`StringInterner`] and the artifact
/// table, plus the linker-input arena and (phase 5.C0) the
/// declared-action arena. One registry per build invocation —
/// phase 5.B/5.C will thread an `Arc<Mutex<ArtifactRegistry>>`
/// (or `&mut`) through the orchestrator analysis loop. **Not** a
/// `static`: tests need fresh registries, and the registry's
/// lifetime is the build's.
#[derive(Default, Debug)]
pub struct ArtifactRegistry {
    paths: PathInterner,
    strings: StringInterner,
    by_key: HashMap<ArtifactKey, ArtifactId>,
    artifacts: Vec<ArtifactData>,
    linker_inputs: Vec<LinkerInputData>,
    /// Phase 5.C0: declared actions. `actions[id.0]` is the
    /// action with handle `DeclaredActionId(id.0)`. Push-only
    /// during analysis; never reordered.
    actions: Vec<DeclaredAction>,
    /// Declared-action ids per owning target. The wavefront's pruned
    /// skip path asked "what did target T declare?" and the only answer
    /// was a scan of EVERY action under the registry lock - O(targets x
    /// actions) per skip wave, serial, on the driver's critical path.
    actions_by_owner: std::collections::HashMap<TargetId, Vec<DeclaredActionId>>,
    /// Phase 5.C.4: side table of `ExecutableAction` payloads
    /// keyed by `DeclaredActionId`. Sparse — only populated for
    /// actions whose declaring rule's analyze() has called
    /// `set_executable_action`. Phase 5.C.4's scheduler refuses
    /// to execute any DeclaredAction whose entry is missing here
    /// and falls through to legacy `build()` for those.
    executable_actions: HashMap<DeclaredActionId, Arc<ExecutableAction>>,
}

impl ArtifactRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn paths(&self) -> &PathInterner {
        &self.paths
    }

    pub fn paths_mut(&mut self) -> &mut PathInterner {
        &mut self.paths
    }

    pub fn strings(&self) -> &StringInterner {
        &self.strings
    }

    pub fn strings_mut(&mut self) -> &mut StringInterner {
        &mut self.strings
    }

    /// Intern a path under the registry's path interner.
    pub fn intern_path(&mut self, p: &Path) -> PathId {
        self.paths.intern(p)
    }

    /// Intern a string under the registry's string interner.
    pub fn intern_string(&mut self, s: &str) -> StringId {
        self.strings.intern(s)
    }

    /// Register an artifact. If the (config, exec_path) key has
    /// already been registered, returns the existing `ArtifactId`
    /// without modifying its `ArtifactData` — registration is
    /// idempotent on the key, not on the kind/owner.
    ///
    /// Phase 5.C0 P3: `is_source` is the authoritative origin
    /// flag. Pass `true` for files that exist in the repo before
    /// the build (cc_library hdrs, .cc srcs); pass `false` for
    /// build outputs (CcArchive .a, ProtocCompile .pb.h, etc.).
    /// Producer validation keys off this flag, not `kind`, so
    /// generated headers are no longer hidden behind the
    /// "Header is ambiguous" skip.
    pub fn intern_artifact(
        &mut self,
        key: ArtifactKey,
        kind: ArtifactKind,
        is_source: bool,
        owner_target: TargetId,
        owner_action: Option<crate::ActionId>,
    ) -> ArtifactId {
        if let Some(id) = self.by_key.get(&key) {
            return *id;
        }
        let id = ArtifactId(self.artifacts.len() as u32);
        self.artifacts.push(ArtifactData {
            exec_path: key.exec_path,
            kind,
            is_source,
            owner_target,
            owner_action,
            owner_action_declared: None,
        });
        self.by_key.insert(key, id);
        id
    }

    /// Look up an artifact's data by ID. Panics on an ID not
    /// issued by this registry — a programming error.
    pub fn artifact(&self, id: ArtifactId) -> &ArtifactData {
        &self.artifacts[id.0 as usize]
    }

    /// Reverse lookup: given an exec-path and a config, return
    /// the `ArtifactId` if one has been registered. Phase 5.C.3
    /// follow-up; OKR 3 KR 5 "owner of artifact path" debug
    /// helper.
    ///
    /// Together with [`producer`](Self::producer) and
    /// [`consumers_of_artifact`](Self::consumers_of_artifact),
    /// this gives the three lookups the OKR doc calls out:
    ///   - path → artifact (this method)
    ///   - artifact → producer action
    ///   - artifact → consumer actions
    ///
    /// `None` for paths the registry doesn't know about (not
    /// interned) or paths interned but never registered as an
    /// artifact (e.g. include-search-path entries — those are
    /// just `PathId`s, not artifacts).
    pub fn find_artifact_by_path(&self, p: &Path, config: ConfigId) -> Option<ArtifactId> {
        let path_id = self.paths.lookup(p)?;
        let key = ArtifactKey {
            config,
            exec_path: path_id,
        };
        self.by_key.get(&key).copied()
    }

    /// Look up the path of an artifact directly (convenience).
    pub fn artifact_path(&self, id: ArtifactId) -> &Path {
        self.paths.path(self.artifacts[id.0 as usize].exec_path)
    }

    /// Allocate a new linker-input handle. No dedup — every call
    /// gets a fresh `LinkerInputId`. Dedup at the consumer level
    /// happens via `OrderedNestedSet`'s Arc-identity at the node
    /// level; if a target registers the same library twice, that's
    /// the rule's call (matters for circular static-archive
    /// resolution where intentional duplicates are valid).
    pub fn intern_linker_input(&mut self, data: LinkerInputData) -> LinkerInputId {
        let id = LinkerInputId(self.linker_inputs.len() as u32);
        self.linker_inputs.push(data);
        id
    }

    /// Declare an action (phase 5.C0). Registers the action's
    /// outputs in the producer registry — every output's
    /// `ArtifactData.owner_action` slot is set to this action's
    /// `DeclaredActionId`.
    ///
    /// Enforces two of the four producer/consumer invariants
    /// at registration time:
    ///   - **Pass 1** (no duplicate producer): if an output
    ///     `ArtifactId` already has an `owner_action` from a
    ///     prior `declare_action` call, fails with
    ///     `ProducerError::DuplicateProducer`. The artifact
    ///     registry's `intern_artifact` previously allowed
    ///     multiple registrations of the same key without
    ///     conflict (idempotent on `(config, path)`); this method
    ///     adds the producer-uniqueness contract on top.
    ///   - **Pass 4** (source artifacts have no producer): if
    ///     an output `ArtifactId` resolves to an `ArtifactData`
    ///     with `kind == Source`, fails with
    ///     `ProducerError::SourceArtifactAsOutput`. Sources are
    ///     inputs to the build, not outputs of it.
    ///
    /// Passes 2 and 3 (every generated provider artifact and
    /// every generated action input has a producer) are
    /// post-analysis sweeps over the full graph; the orchestrator
    /// runs those after all `analyze()` calls return.
    pub fn declare_action(
        &mut self,
        action: DeclaredAction,
    ) -> Result<DeclaredActionId, ProducerError> {
        let new_id = DeclaredActionId(self.actions.len() as u32);

        // Pre-validate before mutating any state, so a failed
        // registration leaves the registry in the same state it
        // was in before the call.
        for &out_aid in action.outputs.iter() {
            let data = &self.artifacts[out_aid.0 as usize];
            if data.kind == ArtifactKind::Source {
                return Err(ProducerError::SourceArtifactAsOutput {
                    artifact: out_aid,
                    action: new_id,
                });
            }
            if let Some(existing_action_id) = data.owner_action_declared {
                return Err(ProducerError::DuplicateProducer {
                    artifact: out_aid,
                    existing: existing_action_id,
                    attempted: new_id,
                });
            }
        }

        // Validation passed — register the action and stamp
        // owner_action on each output's ArtifactData.
        for &out_aid in action.outputs.iter() {
            self.artifacts[out_aid.0 as usize].owner_action_declared = Some(new_id);
        }
        self.actions_by_owner
            .entry(action.owner_target)
            .or_default()
            .push(new_id);
        self.actions.push(action);
        Ok(new_id)
    }

    /// The actions a target declared, in declaration order. Empty slice
    /// for a target that declared none.
    pub fn actions_of(&self, target: TargetId) -> &[DeclaredActionId] {
        self.actions_by_owner
            .get(&target)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Look up the producer of an artifact by `DeclaredActionId`,
    /// if one has been declared. `None` for source artifacts and
    /// for derived artifacts whose producing action hasn't been
    /// declared yet (typically: rule has been analyzed but no
    /// action was declared — phase 5.C0 makes declaration
    /// optional, phase 5.C0 follow-up will require it for every
    /// generated artifact).
    pub fn producer(&self, aid: ArtifactId) -> Option<DeclaredActionId> {
        self.artifacts[aid.0 as usize].owner_action_declared
    }

    /// Number of declared actions across the build. Useful for
    /// census output and `--analyze-only` reporting.
    pub fn declared_action_count(&self) -> usize {
        self.actions.len()
    }

    /// Phase 5.C0 P4: number of declared actions whose status is
    /// `Incomplete` — these can't be scheduled by phase 5.C and
    /// stay on the legacy inline `build()` execution path.
    pub fn incomplete_action_count(&self) -> usize {
        self.actions
            .iter()
            .filter(|a| matches!(a.status, ActionDeclarationStatus::Incomplete { .. }))
            .count()
    }

    /// Iterate every `Incomplete` declared action with its reason
    /// string. Phase 5.C.4 migration progress is the count
    /// trending toward zero; the listing is what tells you
    /// *which* rules still need work. Used by the
    /// producer-validation pass and `--analyze-only` to print a
    /// detail block when the count is non-zero.
    pub fn incomplete_actions(
        &self,
    ) -> impl Iterator<Item = (DeclaredActionId, &DeclaredAction, &'static str)> {
        self.actions.iter().enumerate().filter_map(|(i, a)| {
            if let ActionDeclarationStatus::Incomplete { reason } = a.status {
                Some((DeclaredActionId(i as u32), a, reason))
            } else {
                None
            }
        })
    }

    /// Find every declared action that consumes `aid` as an
    /// input. Phase 5.C.3 follow-up. The action DAG already
    /// derives action→action edges via the artifact's producer,
    /// but a query by artifact (rather than by producer-action)
    /// is what diagnostic tooling and OKR 3 KR 5's "consumers
    /// of generated artifact" report needs.
    ///
    /// Linear scan over `self.actions` — fine for diagnostic
    /// queries; phase 5.C.4's scheduler will precompute an index
    /// if hot-path use shows it's needed.
    pub fn consumers_of_artifact(&self, aid: ArtifactId) -> Vec<DeclaredActionId> {
        self.actions
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                if a.inputs.iter().any(|&x| x == aid) {
                    Some(DeclaredActionId(i as u32))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Look up a declared action by handle. Panics on an ID not
    /// issued by this registry — a programming error.
    pub fn declared_action(&self, id: DeclaredActionId) -> &DeclaredAction {
        &self.actions[id.0 as usize]
    }

    /// Iterate every declared action. Used by orchestrator-side
    /// passes 2 and 3 (the post-analysis "every generated input
    /// has a producer" sweep).
    pub fn declared_actions(&self) -> impl Iterator<Item = (DeclaredActionId, &DeclaredAction)> {
        self.actions
            .iter()
            .enumerate()
            .map(|(i, a)| (DeclaredActionId(i as u32), a))
    }

    /// Every SOURCE artifact's path the registry interned during analysis.
    ///
    /// Used by the persistent action-freshness ledger (rush-build Phase-2) to
    /// over-collect the input side of the no-op freshness proof: a touched
    /// existing source (`.cc`/`.h`/`.proto`/genrule `srcs`/…) does not change a
    /// glob dir-listing, so `manifest.validate()` stays Fresh — the ledger MUST
    /// re-stat the source files themselves to catch such an edit. Source
    /// artifacts are the analysis-time record of every file the build reads as
    /// input, including the inputs of legacy rules that never declared a full
    /// action into the registry. Over-collection is safe (a false positive just
    /// forces the full wavefront); a missed input would serve a stale build.
    pub fn source_artifact_paths(&self) -> impl Iterator<Item = &Path> {
        self.artifacts
            .iter()
            .filter(|a| a.is_source || matches!(a.kind, ArtifactKind::Source))
            .map(|a| self.paths.path(a.exec_path))
    }

    /// Phase 5.C.4: attach an `ExecutableAction` payload to a
    /// declared action so the scheduler can run it without
    /// falling through to legacy `build()`. Stored as
    /// `Arc<ExecutableAction>` so the materialized
    /// [`crate::Action`] (built at scheduler time) can hold a
    /// cheap clone without copying argv/env.
    ///
    /// Idempotent in the sense that a second call for the same
    /// `DeclaredActionId` overwrites the prior payload — the
    /// last `set_executable_action` wins. That's deliberate: a
    /// rule's analyze() builds up the action incrementally
    /// (collect inputs → declare action → fill argv after deps
    /// are resolved) and the orchestrator treats the final
    /// payload as authoritative. If a rule needs append-style
    /// argv assembly it can read the current payload via
    /// `executable_action()` and re-set with the extension.
    ///
    /// Panics if `id` was not issued by this registry — a
    /// programming error in the caller.
    pub fn set_executable_action(&mut self, id: DeclaredActionId, exec: ExecutableAction) {
        assert!(
            (id.0 as usize) < self.actions.len(),
            "DeclaredActionId {:?} out of range (registry has {} actions)",
            id,
            self.actions.len()
        );
        self.executable_actions.insert(id, Arc::new(exec));
    }

    /// Phase 5.C.4: look up an action's `ExecutableAction`
    /// payload, if its declaring rule has populated one. `None`
    /// means the action is "declaration-only" — phase 5.C.4's
    /// scheduler must defer to the legacy `build()` path for
    /// this action's owner_target.
    ///
    /// Returns the registry's `Arc<ExecutableAction>` directly
    /// (not cloned) so callers can compare Arc identity if they
    /// want; clone via `Arc::clone(reg.executable_action(id)?)`
    /// for owned handles.
    pub fn executable_action(&self, id: DeclaredActionId) -> Option<&Arc<ExecutableAction>> {
        self.executable_actions.get(&id)
    }

    /// Phase 5.C.4: how many declared actions have an
    /// `ExecutableAction` payload attached. Migration progress
    /// metric — when this equals `declared_action_count() -
    /// incomplete_action_count()` (every Complete action has a
    /// payload), every rule has been migrated and the legacy
    /// `build()` fallback can be retired (phase 5.C.5).
    pub fn executable_action_count(&self) -> usize {
        self.executable_actions.len()
    }

    /// Look up linker-input data by ID. Panics on an ID not issued
    /// by this registry.
    pub fn linker_input(&self, id: LinkerInputId) -> &LinkerInputData {
        &self.linker_inputs[id.0 as usize]
    }

    /// Number of registered linker inputs. Useful for census output.
    pub fn linker_input_count(&self) -> usize {
        self.linker_inputs.len()
    }

    /// Number of registered artifacts. Useful for census output.
    pub fn artifact_count(&self) -> usize {
        self.artifacts.len()
    }

    /// #39 graph-RSS reduction (RUSH_MEM_CENSUS): a coarse memory accounting of
    /// this registry's sub-structures, to name the dominant consumer before
    /// optimizing (Codex 019e88db ranked the action/artifact registry as the
    /// likely #1 hog). Reports element counts plus approximate heap bytes:
    /// interner string/path payloads are counted ONCE for the `Vec` and ONCE
    /// for the dedup `HashMap` key (each interner stores both), and action
    /// payloads count their `Box<[Id]>` slices (4 bytes/id) plus the 24-byte
    /// `ArtifactData`/`DeclaredAction` struct stride. This is an estimate, not
    /// an allocator-exact figure — enough to size the hog.
    pub fn mem_census(&self) -> String {
        let path_bytes: usize = self.paths.total_path_bytes();
        let str_bytes: usize = self.strings.total_str_bytes();
        let action_id_elems: usize = self
            .actions
            .iter()
            .map(|a| a.inputs.len() + a.outputs.len())
            .sum();
        let exec_elems: usize = self
            .executable_actions
            .values()
            .map(|e| e.argv.len() + e.env.len() * 2)
            .sum();
        let mb = |b: usize| (b as f64) / (1024.0 * 1024.0);
        format!(
            "ArtifactRegistry mem census:\n  \
             paths={} (~{:.1} MB strings, x2 for dedup map = ~{:.1} MB)\n  \
             strings={} (~{:.1} MB strings, x2 = ~{:.1} MB)\n  \
             by_key entries={}\n  \
             artifacts={} (~{:.1} MB @24B)\n  \
             linker_inputs={}\n  \
             actions={} ({} input+output ids, ~{:.1} MB @4B)\n  \
             executable_actions={} ({} argv+env*2 ids, ~{:.1} MB @4B)",
            self.paths.len(),
            mb(path_bytes),
            mb(path_bytes * 2),
            self.strings.len(),
            mb(str_bytes),
            mb(str_bytes * 2),
            self.by_key.len(),
            self.artifacts.len(),
            mb(self.artifacts.len() * 24),
            self.linker_inputs.len(),
            self.actions.len(),
            action_id_elems,
            mb(action_id_elems * 4),
            self.executable_actions.len(),
            exec_elems,
            mb(exec_elems * 4),
        )
    }

    pub fn path_count(&self) -> usize {
        self.paths.len()
    }

    pub fn string_count(&self) -> usize {
        self.strings.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_interner_dedupes() {
        let mut interner = PathInterner::new();
        let id1 = interner.intern(Path::new("foo/bar.h"));
        let id2 = interner.intern(Path::new("foo/bar.h"));
        let id3 = interner.intern(Path::new("foo/baz.h"));
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
        assert_eq!(interner.len(), 2);
    }

    #[test]
    fn path_interner_round_trip() {
        let mut interner = PathInterner::new();
        let p = Path::new("/abs/path/file.cc");
        let id = interner.intern(p);
        assert_eq!(interner.path(id), p);
    }

    #[test]
    fn string_interner_dedupes() {
        let mut interner = StringInterner::new();
        let id1 = interner.intern("FOO=1");
        let id2 = interner.intern("FOO=1");
        let id3 = interner.intern("BAR=2");
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
        assert_eq!(interner.len(), 2);
        assert_eq!(interner.str(id1), "FOO=1");
    }

    #[test]
    fn artifact_registry_dedupes_on_key() {
        let mut reg = ArtifactRegistry::new();
        let p = reg.intern_path(Path::new("obj/foo.o"));
        let key = ArtifactKey {
            config: ConfigId::DEFAULT,
            exec_path: p,
        };
        let id1 = reg.intern_artifact(key.clone(), ArtifactKind::Object, false, TargetId(1), None);
        let id2 = reg.intern_artifact(key, ArtifactKind::Object, false, TargetId(1), None);
        assert_eq!(id1, id2);
        assert_eq!(reg.artifact_count(), 1);
    }

    #[test]
    fn artifact_registry_distinguishes_configs() {
        // Same exec path, different ConfigId: must produce
        // distinct ArtifactIds. Otherwise an opt build's `obj/foo.o`
        // would alias a dbg build's `obj/foo.o` — exactly the
        // collision ChatGPT review §2 flagged.
        let mut reg = ArtifactRegistry::new();
        let p = reg.intern_path(Path::new("obj/foo.o"));
        let id_opt = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId(1),
                exec_path: p,
            },
            ArtifactKind::Object,
            false,
            TargetId(1),
            None,
        );
        let id_dbg = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId(2),
                exec_path: p,
            },
            ArtifactKind::Object,
            false,
            TargetId(1),
            None,
        );
        assert_ne!(id_opt, id_dbg);
        assert_eq!(reg.artifact_count(), 2);
    }

    #[test]
    fn artifact_kind_stored_once() {
        // The whole point of the registry: two NestedSet nodes
        // both referencing the same header artifact only carry
        // the 4-byte ArtifactId. The kind lives ONCE, in
        // ArtifactData. Verify the data round-trip.
        let mut reg = ArtifactRegistry::new();
        let p = reg.intern_path(Path::new("include/foo.h"));
        let id = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: p,
            },
            ArtifactKind::Header,
            false,
            TargetId(7),
            None,
        );
        let data = reg.artifact(id);
        assert_eq!(data.kind, ArtifactKind::Header);
        assert_eq!(data.owner_target, TargetId(7));
        assert!(data.owner_action.is_none());
        assert_eq!(reg.artifact_path(id), Path::new("include/foo.h"));
    }

    #[test]
    fn source_vs_derived_owner_action_invariant() {
        // Source artifacts have owner_action = None; derived
        // artifacts have Some(action). Phase 5.C0 will validate
        // this; the skeleton just makes sure the field carries.
        let mut reg = ArtifactRegistry::new();
        let src_path = reg.intern_path(Path::new("src/main.cc"));
        let obj_path = reg.intern_path(Path::new("obj/main.o"));
        let action = crate::ActionId::new(blake3::hash(b"compile main.cc"));

        let src_id = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: src_path,
            },
            ArtifactKind::Source,
            true,
            TargetId(1),
            None,
        );
        let obj_id = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: obj_path,
            },
            ArtifactKind::Object,
            false,
            TargetId(1),
            Some(action.clone()),
        );

        assert!(reg.artifact(src_id).owner_action.is_none());
        assert_eq!(
            reg.artifact(obj_id).owner_action.as_ref().unwrap().hash(),
            action.hash()
        );
    }

    #[test]
    fn artifact_id_is_copy_and_4_bytes() {
        // The whole memory motivation for ArtifactId is that it's
        // 4 bytes Copy, replacing PathBuf's ~50 bytes. Verify the
        // size invariant so a future struct-field addition doesn't
        // silently regress this.
        assert_eq!(std::mem::size_of::<ArtifactId>(), 4);
        assert_eq!(std::mem::size_of::<PathId>(), 4);
        assert_eq!(std::mem::size_of::<StringId>(), 4);
        assert_eq!(std::mem::size_of::<ConfigId>(), 4);
        assert_eq!(std::mem::size_of::<TargetId>(), 4);
        assert_eq!(std::mem::size_of::<LinkerInputId>(), 4);
        // Copy-able sanity check (compile-time): if these weren't
        // Copy, `let _ = id; let _ = id;` wouldn't typecheck.
        let id = ArtifactId(0);
        let _a = id;
        let _b = id;
        let lid = LinkerInputId(0);
        let _l1 = lid;
        let _l2 = lid;
    }

    #[test]
    fn linker_input_arena_round_trip() {
        // Register a LinkerInput, get back a LinkerInputId, look up
        // the rich data through it. The whole point of the arena:
        // providers carry the 4-byte ID, the data lives once.
        let mut reg = ArtifactRegistry::new();
        let lib_path = reg.intern_path(Path::new("lib/libfoo.a"));
        let lib_artifact = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: lib_path,
            },
            ArtifactKind::StaticLibrary,
            false,
            TargetId(1),
            None,
        );
        let id = reg.intern_linker_input(LinkerInputData {
            owner: TargetId(1),
            artifact: lib_artifact,
            alwayslink: true,
            extras: None,
        });
        let data = reg.linker_input(id);
        assert_eq!(data.owner, TargetId(1));
        assert_eq!(data.artifact, lib_artifact);
        assert!(data.alwayslink);
        assert!(data.extras.is_none());
        assert_eq!(reg.linker_input_count(), 1);
    }

    #[test]
    fn linker_input_arena_does_not_dedup() {
        // Two LinkerInputs with identical (owner, artifact, alwayslink)
        // get DISTINCT LinkerInputIds. The OrderedNestedSet that
        // references them dedupes at the Arc-node level, not at the
        // value level — important for circular static-archive
        // resolution where intentional duplicates are valid in a
        // --start-group / --end-group block.
        let mut reg = ArtifactRegistry::new();
        let lib_path = reg.intern_path(Path::new("lib/libfoo.a"));
        let lib_artifact = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: lib_path,
            },
            ArtifactKind::StaticLibrary,
            false,
            TargetId(1),
            None,
        );
        let id1 = reg.intern_linker_input(LinkerInputData {
            owner: TargetId(1),
            artifact: lib_artifact,
            alwayslink: false,
            extras: None,
        });
        let id2 = reg.intern_linker_input(LinkerInputData {
            owner: TargetId(1),
            artifact: lib_artifact,
            alwayslink: false,
            extras: None,
        });
        assert_ne!(id1, id2);
        assert_eq!(reg.linker_input_count(), 2);
    }

    #[test]
    fn linker_input_extras_arc_shareable() {
        // LinkerInputExtras is Arc-wrapped so the same extras can
        // be shared across multiple LinkerInputs (e.g. when a target
        // exports both the library and a libtool-style group of
        // additional inputs to multiple downstream consumers). Verify
        // Arc::ptr_eq works on the extras handle.
        use std::sync::Arc;
        let mut reg = ArtifactRegistry::new();
        let lib_path = reg.intern_path(Path::new("lib/libfoo.a"));
        let lib_artifact = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: lib_path,
            },
            ArtifactKind::StaticLibrary,
            false,
            TargetId(1),
            None,
        );
        let extras = Arc::new(LinkerInputExtras {
            additional_inputs: Vec::new().into_boxed_slice(),
            per_input_link_flags: Vec::new().into_boxed_slice(),
        });
        let id1 = reg.intern_linker_input(LinkerInputData {
            owner: TargetId(1),
            artifact: lib_artifact,
            alwayslink: false,
            extras: Some(Arc::clone(&extras)),
        });
        let id2 = reg.intern_linker_input(LinkerInputData {
            owner: TargetId(1),
            artifact: lib_artifact,
            alwayslink: false,
            extras: Some(Arc::clone(&extras)),
        });
        let e1 = reg.linker_input(id1).extras.as_ref().unwrap();
        let e2 = reg.linker_input(id2).extras.as_ref().unwrap();
        assert!(Arc::ptr_eq(e1, e2));
    }

    // ---------------------------------------------------------------------
    // Phase 5.C0: declared-action arena + producer-validation tests.

    fn make_obj(reg: &mut ArtifactRegistry, name: &str, target: u32) -> ArtifactId {
        let p = reg.intern_path(Path::new(name));
        reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: p,
            },
            ArtifactKind::Object,
            false,
            TargetId(target),
            None,
        )
    }

    fn make_src(reg: &mut ArtifactRegistry, name: &str, target: u32) -> ArtifactId {
        let p = reg.intern_path(Path::new(name));
        reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: p,
            },
            ArtifactKind::Source,
            true,
            TargetId(target),
            None,
        )
    }

    #[test]
    fn declare_action_round_trips_through_arena() {
        let mut reg = ArtifactRegistry::new();
        let mnemonic = reg.intern_string("CppCompile");
        let src = make_src(&mut reg, "src/main.cc", 1);
        let obj = make_obj(&mut reg, "obj/main.o", 1);
        let id = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic,
                inputs: Box::new([src]),
                outputs: Box::new([obj]),
                status: ActionDeclarationStatus::Complete,
            })
            .expect("first declaration should succeed");
        let action = reg.declared_action(id);
        assert_eq!(action.owner_target, TargetId(1));
        assert_eq!(action.inputs.as_ref(), &[src]);
        assert_eq!(action.outputs.as_ref(), &[obj]);
        assert_eq!(reg.declared_action_count(), 1);
        // Producer registry: the obj's owner_action_declared now
        // points at our DeclaredActionId.
        assert_eq!(reg.producer(obj), Some(id));
        // Source artifact has no producer.
        assert_eq!(reg.producer(src), None);
    }

    // Phase 5.C.4 commit 75: fake gentbl downstream
    // consumer test fixtures.
    //
    // Pro's commit-75 directive: "Target shape: fake
    // gentbl rule produces .h.inc; downstream cc_library or
    // tf_cc_library includes .h.inc; CppCompile declares
    // .h.inc as generated input; producer chain points to
    // fake tblgen action. Verifier: zero-diff for fake
    // shape only."
    //
    // The fake-tool plumbing lives in
    // rush-resource::tool_probe (the `fake_mlir_tblgen`
    // helper from commit 63). These tests exercise the
    // STRUCTURAL shape: artifact registry producer-chain
    // semantics for a gentbl-shaped action graph. They do
    // NOT invoke the real or fake tblgen tool — that's
    // execution-layer concern. Pro's "fake tests prove the
    // producer/consumer shape, not real tool invocation"
    // is exactly what these tests verify.

    fn make_generated_h_inc(
        reg: &mut ArtifactRegistry,
        name: &str,
        target: u32,
    ) -> ArtifactId {
        let p = reg.intern_path(Path::new(name));
        reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: p,
            },
            ArtifactKind::Header,
            false, // not a source — generated
            TargetId(target),
            None,
        )
    }

    /// Phase 5.C.4 commit 75: a fake gentbl action declares
    /// an `.h.inc` output. Pins that the producer side of
    /// the chain is structurally valid — the declared
    /// output has the `.h.inc` extension and is registered
    /// with the rule's target.
    #[test]
    fn fake_gentbl_declares_h_inc_output() {
        let mut reg = ArtifactRegistry::new();
        let mnemonic = reg.intern_string("CodegenTblgen");
        let h_inc = make_generated_h_inc(
            &mut reg,
            "rush-out/genfiles/fake/op_attrs.h.inc",
            7,
        );
        let id = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(7),
                mnemonic,
                inputs: Box::new([]),
                outputs: Box::new([h_inc]),
                status: ActionDeclarationStatus::Complete,
            })
            .expect("fake gentbl action declares cleanly");
        let path = reg.artifact_path(h_inc).to_path_buf();
        assert!(
            path.to_string_lossy().ends_with(".h.inc"),
            "fake gentbl output must end in `.h.inc`; got {:?}",
            path
        );
        let action = reg.declared_action(id);
        assert_eq!(action.outputs.as_ref(), &[h_inc]);
        assert_eq!(action.inputs.len(), 0, "fake gentbl has no inputs");
    }

    /// Phase 5.C.4 commit 75: a downstream CppCompile action
    /// declares the `.h.inc` as a generated input. Pins
    /// that the consumer side of the chain is structurally
    /// valid.
    #[test]
    fn fake_gentbl_downstream_compile_has_generated_input() {
        let mut reg = ArtifactRegistry::new();
        let tblgen_mnemonic = reg.intern_string("CodegenTblgen");
        let compile_mnemonic = reg.intern_string("CppCompile");
        let h_inc = make_generated_h_inc(
            &mut reg,
            "rush-out/genfiles/fake/op_attrs.h.inc",
            7,
        );
        let src = make_src(&mut reg, "fake_consumer.cc", 8);
        let obj = make_obj(&mut reg, "rush-out/fake/consumer.o", 8);
        // Producer: fake gentbl action emitting the .h.inc.
        reg.declare_action(DeclaredAction {
            owner_target: TargetId(7),
            mnemonic: tblgen_mnemonic,
            inputs: Box::new([]),
            outputs: Box::new([h_inc]),
            status: ActionDeclarationStatus::Complete,
        })
        .expect("producer registers");
        // Consumer: CppCompile declaring src + h_inc as
        // inputs, obj as output.
        let consumer_id = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(8),
                mnemonic: compile_mnemonic,
                inputs: Box::new([src, h_inc]),
                outputs: Box::new([obj]),
                status: ActionDeclarationStatus::Complete,
            })
            .expect("consumer registers");
        let consumer = reg.declared_action(consumer_id);
        assert!(
            consumer.inputs.contains(&h_inc),
            "downstream CppCompile must declare generated \
             .h.inc as input; inputs were {:?}",
            consumer.inputs
        );
        assert!(
            consumer.inputs.contains(&src),
            "downstream CppCompile must declare source .cc \
             as input; inputs were {:?}",
            consumer.inputs
        );
    }

    /// Phase 5.C.4 commit 75: registry's producer chain
    /// resolves an `.h.inc` artifact back to the fake
    /// gentbl action that emitted it. Pins that the
    /// scheduler can answer "who produced this header?"
    /// for a fake-gentbl-shaped graph.
    #[test]
    fn fake_gentbl_producer_chain_valid() {
        let mut reg = ArtifactRegistry::new();
        let tblgen_mnemonic = reg.intern_string("CodegenTblgen");
        let compile_mnemonic = reg.intern_string("CppCompile");
        let h_inc = make_generated_h_inc(
            &mut reg,
            "rush-out/genfiles/fake/op_attrs.h.inc",
            7,
        );
        let src = make_src(&mut reg, "fake_consumer.cc", 8);
        let obj = make_obj(&mut reg, "rush-out/fake/consumer.o", 8);
        let producer_id = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(7),
                mnemonic: tblgen_mnemonic,
                inputs: Box::new([]),
                outputs: Box::new([h_inc]),
                status: ActionDeclarationStatus::Complete,
            })
            .expect("producer registers");
        reg.declare_action(DeclaredAction {
            owner_target: TargetId(8),
            mnemonic: compile_mnemonic,
            inputs: Box::new([src, h_inc]),
            outputs: Box::new([obj]),
            status: ActionDeclarationStatus::Complete,
        })
        .expect("consumer registers");
        // Producer chain: querying who produced h_inc
        // returns the fake gentbl action.
        assert_eq!(
            reg.producer(h_inc),
            Some(producer_id),
            "producer chain must point .h.inc → fake gentbl action"
        );
        // Source files have no producer (sanity check).
        assert_eq!(reg.producer(src), None);
        // The compile's output (obj) has its own producer
        // — the consumer action.
        assert!(
            reg.producer(obj).is_some(),
            "obj must have a registered producer"
        );
        assert_ne!(
            reg.producer(obj),
            Some(producer_id),
            "obj producer must NOT be the gentbl action (would \
             cross targets)"
        );
    }

    #[test]
    fn declare_action_rejects_duplicate_producer() {
        // Pass 1 of producer/consumer validation: two actions
        // can't both produce the same generated artifact.
        let mut reg = ArtifactRegistry::new();
        let mnemonic = reg.intern_string("CppCompile");
        let obj = make_obj(&mut reg, "obj/main.o", 1);
        let first = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic,
                inputs: Box::new([]),
                outputs: Box::new([obj]),
                status: ActionDeclarationStatus::Complete,
            })
            .expect("first registration should succeed");
        let err = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(2),
                mnemonic,
                inputs: Box::new([]),
                outputs: Box::new([obj]),
                status: ActionDeclarationStatus::Complete,
            })
            .expect_err("second registration should fail");
        match err {
            ProducerError::DuplicateProducer {
                artifact,
                existing,
                attempted,
            } => {
                assert_eq!(artifact, obj);
                assert_eq!(existing, first);
                assert_ne!(attempted, first);
            }
            _ => panic!("expected DuplicateProducer, got {:?}", err),
        }
        // Registry is unchanged after the failed registration —
        // only the first action exists.
        assert_eq!(reg.declared_action_count(), 1);
        assert_eq!(reg.producer(obj), Some(first));
    }

    #[test]
    fn declare_action_rejects_source_artifact_as_output() {
        // Pass 4: source artifacts must not appear in any
        // action's outputs. Sources are inputs to the build, not
        // outputs of it. A rule that declared a source as an
        // output would confuse the scheduler about producer
        // ownership.
        let mut reg = ArtifactRegistry::new();
        let mnemonic = reg.intern_string("BogusAction");
        let src = make_src(&mut reg, "src/main.cc", 1);
        let err = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic,
                inputs: Box::new([]),
                outputs: Box::new([src]),
                status: ActionDeclarationStatus::Complete,
            })
            .expect_err("source-as-output should fail");
        match err {
            ProducerError::SourceArtifactAsOutput { artifact, .. } => {
                assert_eq!(artifact, src);
            }
            _ => panic!("expected SourceArtifactAsOutput, got {:?}", err),
        }
        // Registry is unchanged.
        assert_eq!(reg.declared_action_count(), 0);
        assert_eq!(reg.producer(src), None);
    }

    #[test]
    fn declared_action_id_is_copy_and_4_bytes() {
        // Same memory invariant as the other ID types — important
        // because DeclaredActionId is going to live in NestedSet
        // children (action graph in 5.C) and on every Action
        // reference.
        assert_eq!(std::mem::size_of::<DeclaredActionId>(), 4);
        let id = DeclaredActionId(0);
        let _a = id;
        let _b = id;
    }

    #[test]
    fn executable_action_round_trips() {
        // Declare an action, attach an ExecutableAction, look it
        // back up via the side table. Verify Arc identity is
        // preserved (no extra clone path).
        let mut reg = ArtifactRegistry::new();
        let m = reg.intern_string("CppCompile");
        let src = make_src(&mut reg, "src/main.cc", 1);
        let obj = make_obj(&mut reg, "obj/main.o", 1);
        let id = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m,
                inputs: Box::new([src]),
                outputs: Box::new([obj]),
                status: ActionDeclarationStatus::Complete,
            })
            .unwrap();

        // Before set: no payload.
        assert!(reg.executable_action(id).is_none());
        assert_eq!(reg.executable_action_count(), 0);

        let executable = reg.intern_path(Path::new("/usr/bin/g++"));
        let arg_c = reg.intern_string("-c");
        let arg_o = reg.intern_string("-o");
        let env_key = reg.intern_string("PATH");
        let env_val = reg.intern_string("/usr/bin");
        reg.set_executable_action(
            id,
            ExecutableAction {
                executable,
                argv: Box::new([arg_c, arg_o]),
                env: Box::new([(env_key, env_val)]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );

        let retrieved = reg
            .executable_action(id)
            .expect("set then get should resolve");
        assert_eq!(retrieved.executable, executable);
        assert_eq!(retrieved.argv.as_ref(), &[arg_c, arg_o]);
        assert_eq!(retrieved.env.as_ref(), &[(env_key, env_val)]);
        assert!(retrieved.working_dir.is_none());
        assert_eq!(reg.executable_action_count(), 1);
    }

    #[test]
    fn executable_action_materialize_resolves_ids() {
        // Build an ExecutableAction with interned IDs, then
        // materialize through the same registry. Verify every
        // PathId / StringId resolves to its original content
        // (round-trip through the interners).
        let mut reg = ArtifactRegistry::new();
        let exe = reg.intern_path(Path::new("/usr/bin/ar"));
        let arg_rcs = reg.intern_string("rcs");
        let arg_lib = reg.intern_string("lib/libfoo.a");
        let arg_obj = reg.intern_string("obj/foo.o");
        let env_key = reg.intern_string("LANG");
        let env_val = reg.intern_string("C");
        let wd = reg.intern_path(Path::new("/build"));

        let action = ExecutableAction {
            executable: exe,
            argv: Box::new([arg_rcs, arg_lib, arg_obj]),
            env: Box::new([(env_key, env_val)]),
            working_dir: Some(wd),
            env_policy: ActionEnvPolicy::InheritHost,
        };

        let mat = action.materialize(&reg);
        assert_eq!(mat.executable, PathBuf::from("/usr/bin/ar"));
        assert_eq!(mat.argv, vec!["rcs", "lib/libfoo.a", "obj/foo.o"]);
        assert_eq!(mat.env, vec![("LANG".to_string(), "C".to_string())]);
        assert_eq!(mat.working_dir, Some(PathBuf::from("/build")));
    }

    #[test]
    fn executable_action_materialize_handles_empty_argv_env() {
        // No argv, no env, no working_dir: still materializes
        // cleanly (just an executable path with empty Vecs).
        let mut reg = ArtifactRegistry::new();
        let exe = reg.intern_path(Path::new("/bin/true"));
        let action = ExecutableAction {
            executable: exe,
            argv: Box::new([]),
            env: Box::new([]),
            working_dir: None,
            env_policy: ActionEnvPolicy::InheritHost,
        };
        let mat = action.materialize(&reg);
        assert_eq!(mat.executable, PathBuf::from("/bin/true"));
        assert!(mat.argv.is_empty());
        assert!(mat.env.is_empty());
        assert!(mat.working_dir.is_none());
    }

    #[test]
    fn executable_action_overwrite_replaces_payload() {
        // Second set_executable_action for the same DeclaredActionId
        // replaces the prior payload. Documented "last write wins"
        // semantics; rules use this to extend argv after deps
        // resolve.
        let mut reg = ArtifactRegistry::new();
        let m = reg.intern_string("X");
        let o = make_obj(&mut reg, "o.o", 1);
        let id = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m,
                inputs: Box::new([]),
                outputs: Box::new([o]),
                status: ActionDeclarationStatus::Complete,
            })
            .unwrap();

        let exe = reg.intern_path(Path::new("/bin/true"));
        let a1 = reg.intern_string("-x");
        let a2 = reg.intern_string("-y");
        reg.set_executable_action(
            id,
            ExecutableAction {
                executable: exe,
                argv: Box::new([a1]),
                env: Box::new([]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );
        reg.set_executable_action(
            id,
            ExecutableAction {
                executable: exe,
                argv: Box::new([a1, a2]),
                env: Box::new([]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );

        // Only one entry; the second payload wins.
        assert_eq!(reg.executable_action_count(), 1);
        assert_eq!(
            reg.executable_action(id).unwrap().argv.as_ref(),
            &[a1, a2]
        );
    }

    #[test]
    fn executable_action_missing_for_declaration_only_action() {
        // Declared but not migrated: executable_action() returns
        // None. This is the signal phase 5.C.4's scheduler uses
        // to fall through to legacy build().
        let mut reg = ArtifactRegistry::new();
        let m = reg.intern_string("LegacyMnemonic");
        let o = make_obj(&mut reg, "o.o", 1);
        let id = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m,
                inputs: Box::new([]),
                outputs: Box::new([o]),
                status: ActionDeclarationStatus::Complete,
            })
            .unwrap();
        assert!(reg.executable_action(id).is_none());
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn executable_action_panics_on_unknown_id() {
        // Setting an ExecutableAction for a DeclaredActionId
        // that wasn't issued by this registry is a programming
        // error — panic, don't silently allocate.
        let mut reg = ArtifactRegistry::new();
        let exe = reg.intern_path(Path::new("/bin/sh"));
        reg.set_executable_action(
            DeclaredActionId(99),
            ExecutableAction {
                executable: exe,
                argv: Box::new([]),
                env: Box::new([]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );
    }

    #[test]
    fn incomplete_actions_iter_returns_only_incomplete() {
        // Mix Complete and Incomplete declarations; verify the
        // iterator yields exactly the Incomplete ones with their
        // reason strings.
        let mut reg = ArtifactRegistry::new();
        let m = reg.intern_string("X");
        let o1 = make_obj(&mut reg, "a.o", 1);
        let o2 = make_obj(&mut reg, "b.o", 1);
        let o3 = make_obj(&mut reg, "c.o", 1);

        let _id1 = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m,
                inputs: Box::new([]),
                outputs: Box::new([o1]),
                status: ActionDeclarationStatus::Complete,
            })
            .unwrap();
        let id2 = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m,
                inputs: Box::new([]),
                outputs: Box::new([o2]),
                status: ActionDeclarationStatus::Incomplete {
                    reason: "deferred to 5.C",
                },
            })
            .unwrap();
        let _id3 = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m,
                inputs: Box::new([]),
                outputs: Box::new([o3]),
                status: ActionDeclarationStatus::Complete,
            })
            .unwrap();

        let collected: Vec<_> = reg
            .incomplete_actions()
            .map(|(id, _, reason)| (id, reason))
            .collect();
        assert_eq!(collected, vec![(id2, "deferred to 5.C")]);
        assert_eq!(reg.incomplete_action_count(), 1);
    }

    #[test]
    fn path_interner_lookup_does_not_insert() {
        let mut interner = PathInterner::new();
        // Lookup before intern: None.
        assert_eq!(interner.lookup(Path::new("foo.cc")), None);
        let id = interner.intern(Path::new("foo.cc"));
        // Lookup after intern: Some(id).
        assert_eq!(interner.lookup(Path::new("foo.cc")), Some(id));
        // Different path: still None even though interner is
        // populated.
        assert_eq!(interner.lookup(Path::new("bar.cc")), None);
        // Lookup didn't grow the interner — only the explicit
        // intern() call did.
        assert_eq!(interner.len(), 1);
    }

    #[test]
    fn find_artifact_by_path_round_trips() {
        let mut reg = ArtifactRegistry::new();
        let p = reg.intern_path(Path::new("obj/main.o"));
        let aid = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: p,
            },
            ArtifactKind::Object,
            false,
            TargetId(1),
            None,
        );
        // Roundtrip: same path, same config → same ArtifactId.
        assert_eq!(
            reg.find_artifact_by_path(Path::new("obj/main.o"), ConfigId::DEFAULT),
            Some(aid)
        );
    }

    #[test]
    fn find_artifact_by_path_distinguishes_configs() {
        // Same path, different ConfigId → different ArtifactId.
        // Verifies the lookup respects the config dimension of
        // ArtifactKey (the collision protection ChatGPT review §2
        // called out for opt vs dbg builds).
        let mut reg = ArtifactRegistry::new();
        let p = reg.intern_path(Path::new("obj/main.o"));
        let id_opt = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId(1),
                exec_path: p,
            },
            ArtifactKind::Object,
            false,
            TargetId(1),
            None,
        );
        let id_dbg = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId(2),
                exec_path: p,
            },
            ArtifactKind::Object,
            false,
            TargetId(1),
            None,
        );
        assert_eq!(
            reg.find_artifact_by_path(Path::new("obj/main.o"), ConfigId(1)),
            Some(id_opt)
        );
        assert_eq!(
            reg.find_artifact_by_path(Path::new("obj/main.o"), ConfigId(2)),
            Some(id_dbg)
        );
        // Wrong config returns None — wouldn't fall back to a
        // different config's artifact.
        assert_eq!(
            reg.find_artifact_by_path(Path::new("obj/main.o"), ConfigId(99)),
            None,
        );
    }

    #[test]
    fn find_artifact_by_path_returns_none_for_unregistered_path() {
        let mut reg = ArtifactRegistry::new();
        // Intern the path but don't register an artifact.
        // find_artifact_by_path should return None — interning
        // a path doesn't make it an artifact.
        let _path_id = reg.intern_path(Path::new("include/foo.h"));
        assert_eq!(
            reg.find_artifact_by_path(Path::new("include/foo.h"), ConfigId::DEFAULT),
            None
        );
        // A path that was never seen at all also returns None.
        assert_eq!(
            reg.find_artifact_by_path(Path::new("never/seen.h"), ConfigId::DEFAULT),
            None
        );
    }

    #[test]
    fn consumers_of_artifact_finds_action_inputs() {
        // Two actions consume the same intermediate object as
        // input. The lookup must return both, in registration
        // order, and exclude the producer itself.
        let mut reg = ArtifactRegistry::new();
        let m_compile = reg.intern_string("CppCompile");
        let m_archive = reg.intern_string("CcArchive");
        let m_link = reg.intern_string("CcLink");
        let src = make_src(&mut reg, "src.cc", 1);
        let obj = make_obj(&mut reg, "obj.o", 1);
        let lib = {
            let p = reg.intern_path(Path::new("lib.a"));
            reg.intern_artifact(
                ArtifactKey {
                    config: ConfigId::DEFAULT,
                    exec_path: p,
                },
                ArtifactKind::StaticLibrary,
                false,
                TargetId(1),
                None,
            )
        };
        let bin = {
            let p = reg.intern_path(Path::new("bin"));
            reg.intern_artifact(
                ArtifactKey {
                    config: ConfigId::DEFAULT,
                    exec_path: p,
                },
                ArtifactKind::Executable,
                false,
                TargetId(1),
                None,
            )
        };

        let _compile = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m_compile,
                inputs: Box::new([src]),
                outputs: Box::new([obj]),
                status: ActionDeclarationStatus::Complete,
            })
            .unwrap();
        let archive = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m_archive,
                inputs: Box::new([obj]),
                outputs: Box::new([lib]),
                status: ActionDeclarationStatus::Complete,
            })
            .unwrap();
        let link = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m_link,
                inputs: Box::new([obj]),
                outputs: Box::new([bin]),
                status: ActionDeclarationStatus::Complete,
            })
            .unwrap();

        let consumers = reg.consumers_of_artifact(obj);
        assert_eq!(consumers, vec![archive, link]);

        // Source artifact has no consumer? Wrong — src is
        // consumed by the compile action. Verify.
        let src_consumers = reg.consumers_of_artifact(src);
        assert_eq!(src_consumers.len(), 1);

        // An artifact that nobody consumes (the binary) has an
        // empty consumer list.
        let bin_consumers = reg.consumers_of_artifact(bin);
        assert!(bin_consumers.is_empty());
    }

    /// actions_of must agree with a full declared_actions scan for every
    /// owner - the pruned skip path now trusts the index instead of the
    /// scan, so divergence would silently under-report a target's outputs
    /// and send it to the conservative-rebuild arm.
    #[test]
    fn actions_of_agrees_with_full_scan() {
        let mut reg = ArtifactRegistry::new();
        let m = reg.intern_string("Test");
        let mut declare = |reg: &mut ArtifactRegistry, owner: u32, out_name: &str| {
            let out = make_obj(reg, out_name, owner);
            reg.declare_action(DeclaredAction {
                owner_target: TargetId(owner),
                mnemonic: m,
                inputs: Box::new([]),
                outputs: Box::new([out]),
                status: ActionDeclarationStatus::Complete,
            })
            .expect("declares")
        };
        declare(&mut reg, 1, "rush-out/a.o");
        declare(&mut reg, 2, "rush-out/b.o");
        declare(&mut reg, 1, "rush-out/c.o");
        for owner in [1u32, 2, 3] {
            let scanned: Vec<DeclaredActionId> = reg
                .declared_actions()
                .filter(|(_, a)| a.owner_target == TargetId(owner))
                .map(|(id, _)| id)
                .collect();
            assert_eq!(reg.actions_of(TargetId(owner)), scanned.as_slice());
        }
    }

    #[test]
    fn declared_actions_iter_yields_in_registration_order() {
        let mut reg = ArtifactRegistry::new();
        let m = reg.intern_string("X");
        let o1 = make_obj(&mut reg, "o1.o", 1);
        let o2 = make_obj(&mut reg, "o2.o", 1);
        let o3 = make_obj(&mut reg, "o3.o", 1);
        let id1 = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m,
                inputs: Box::new([]),
                outputs: Box::new([o1]),
                status: ActionDeclarationStatus::Complete,
            })
            .unwrap();
        let id2 = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m,
                inputs: Box::new([]),
                outputs: Box::new([o2]),
                status: ActionDeclarationStatus::Complete,
            })
            .unwrap();
        let id3 = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m,
                inputs: Box::new([]),
                outputs: Box::new([o3]),
                status: ActionDeclarationStatus::Complete,
            })
            .unwrap();
        let observed: Vec<_> = reg.declared_actions().map(|(id, _)| id).collect();
        assert_eq!(observed, vec![id1, id2, id3]);
    }
}
