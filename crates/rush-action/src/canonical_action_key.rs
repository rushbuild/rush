// Rush Action - Canonical action-key skeleton (phase 5.C.4 commit 67)
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 commit 67: `CanonicalActionKeyInput` type
//! skeleton.
//!
//! Pro's commit-67 directive: "Define the canonical action-key
//! input as a typed shape so the future cache layer adapts
//! into it. Skeleton only — no hashing, no file reads, no
//! cache state. Just the shape, plus a readiness report that
//! says which pieces of an `ActionKeySpec` need cleanup
//! before the action is safe to canonicalize."
//!
//! ## Why a skeleton?
//!
//! [`super::action_key::ActionKeySpec`] is the diagnostic
//! shape: every field is typed in a way that's useful for
//! humans and for the verifier, but it carries values
//! (absolute paths, raw env vars, host-classified inputs)
//! that the cache key cannot use directly. The cache-key
//! derivation needs:
//!
//!   - paths normalized relative to a stable root,
//!   - tools identified by content digest + toolchain
//!     identity (not host paths),
//!   - inputs identified by content digest,
//!   - env reduced to declared content (no host inheritance),
//!   - config + platform identity captured explicitly.
//!
//! This module declares the SHAPE of those values. The
//! conversion from `ActionKeySpec` returns a structured
//! readiness report — we explicitly do NOT compute digests
//! (Pro's hard stop) or normalize content. This commit gives
//! the future cache work the type to adapt into; the actual
//! adapter lands post-5.C.5.
//!
//! ## What this is NOT
//!
//! - Not a hash function. There is intentionally no `pub fn
//!   hash(&self) -> ...` here, and the [`Digest`] type has
//!   no constructor that reads files or computes anything.
//!   Pro's directive: "Do not hash."
//! - Not a cache lookup. No `lookup(&self) -> Option<...>`.
//!   No AC/CAS integration. Pro's hard stop.
//! - Not a final schema. The skeleton is intentionally
//!   permissive (Vec where a sorted set may end up; PathBuf
//!   inside NormalizedPath rather than a tighter
//!   workspace-relative type) so post-5.C.5 work can tighten
//!   without breaking diagnostic surfaces.

use std::path::PathBuf;

use crate::action_key::ActionKeySpec;
use crate::ids::ActionEnvPolicy;

/// Phase 5.C.4 commit 67: a content digest, declared as a
/// type with no constructor that reads files.
///
/// This is intentionally a marker type today — the cache-key
/// work will replace the inner repr with an actual content
/// digest (BLAKE3 / SHA-256 / etc.) when Pro greenlights
/// hashing. Today the only constructor is
/// [`Digest::placeholder`] which carries an opaque marker
/// string; it does NOT hash anything.
///
/// Pro's directive: "Do not hash. Do not read files. Just
/// declare the shape."
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct Digest {
    /// Placeholder repr. Today this is just an opaque string
    /// that explains why no real digest is here yet ("not
    /// computed", "absent — needs file content"). The
    /// post-5.C.5 cache-key work replaces this with a real
    /// content-digest representation (e.g. BLAKE3 32-byte
    /// array).
    pub marker: String,
}

impl Digest {
    /// Build a placeholder digest. Today this is the ONLY
    /// constructor. There is intentionally no
    /// `Digest::of_file` or `Digest::compute` — the
    /// commit-67 contract is "shape, not behavior."
    pub fn placeholder(marker: impl Into<String>) -> Self {
        Self {
            marker: marker.into(),
        }
    }
}

/// Phase 5.C.4 commit 67: a path expressed in a form the
/// cache key can use.
///
/// Today this just wraps a `PathBuf` plus a kind tag — a
/// real cache-key path needs to be workspace-relative (so
/// the same repo on host A and host B produces the same
/// key). The kind tag is a hint to the future normalizer
/// about which root the path is relative to.
///
/// Pro's directive: "Do not normalize. Just declare the
/// shape." The skeleton's `kind` field is enough to let the
/// readiness report flag absolute paths as
/// [`CanonicalizationBlocker::AbsolutePathInArgv`] without
/// committing to a specific normalization rule today.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct NormalizedPath {
    /// The path as observed from the source `ActionKeySpec`.
    /// Today this may be absolute (which the readiness
    /// report flags as a blocker); future work normalizes
    /// it.
    pub raw: PathBuf,
    /// Hint about which root this path SHOULD be relative
    /// to. The future normalizer uses this to apply the
    /// right strip-prefix.
    pub kind: NormalizedPathKind,
}

/// Phase 5.C.4 commit 67: which root a [`NormalizedPath`]
/// is meant to be relative to.
///
/// JSON shape (commit 54b style):
///   {"kind": "workspace_relative", "data": null}
///   {"kind": "external_repo", "data": "com_google_absl"}
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum NormalizedPathKind {
    /// Path is relative to (or under) the workspace root.
    /// Future normalization strips the workspace_root
    /// prefix.
    WorkspaceRelative,
    /// Path is an output under `rush-out/genfiles`.
    /// Distinct from workspace-relative because the
    /// genfiles tree has its own normalization rules
    /// (the action that produced it determines the
    /// "real" path identity).
    GenfilesRelative,
    /// Path lives under an external repo. The data is
    /// the repo name (matches
    /// `OutputPathPolicy::ExternalRepoOutput::repo`).
    ExternalRepo(String),
    /// Path looks like a host system path
    /// (`/usr/bin/...`, `/usr/include/...`). The cache
    /// key cannot accept these without a hermetic
    /// toolchain story.
    HostPath,
    /// Path doesn't classify into any of the above.
    /// Forces the action's canonicalization to fail
    /// readiness.
    Unclassified,
}

/// Phase 5.C.4 commit 67: identity of the tool that
/// executes the action.
///
/// The cache-key derivation needs to fingerprint the tool
/// so that swapping `gcc-12` for `gcc-13` invalidates all
/// affected actions. Today this skeleton carries a
/// [`NormalizedPath`] for the executable + an optional
/// toolchain identity string + a [`Digest`] placeholder.
///
/// Pro's directive: "No content reads." So the digest is
/// always [`Digest::placeholder`] today; the future cache
/// layer fills in the real content digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ToolIdentity {
    /// Path to the tool, normalized.
    pub path: NormalizedPath,
    /// Free-form identity string (`gcc-12.2.0`, `clang 18`,
    /// `protoc 3.21.12`). Optional — when missing, the
    /// readiness report flags
    /// [`CanonicalizationBlocker::NoToolchainIdentity`].
    pub toolchain_identity: Option<String>,
    /// Content digest of the tool binary. Today always a
    /// placeholder; future cache work computes the real
    /// digest.
    pub content_digest: Digest,
}

/// Phase 5.C.4 commit 67: stable identity for the build
/// configuration this action runs under.
///
/// Mirrors [`crate::ids::ConfigId`] but is declared
/// separately here because the cache-key shape and the
/// in-process registry handle have different stability
/// contracts:
///   - `ids::ConfigId(u32)` is per-orchestrator and not
///     stable across runs (it's an interner index).
///   - `ConfigId` here is meant to be a cross-run stable
///     identity (a hash of the config's content,
///     eventually). Today it carries the same numeric
///     value as a placeholder.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ConfigId {
    /// Today: the source `ids::ConfigId.0`. Future cache
    /// work replaces this with a stable hash of the
    /// configuration's content (toolchain, opt-mode,
    /// platform, …).
    pub raw: u32,
}

/// Phase 5.C.4 commit 67: stable identity for the target
/// platform.
///
/// Today a placeholder string ("default" when no platform
/// is set). The post-5.C.5 cache layer will populate this
/// from the resolved target platform's content.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct PlatformId {
    /// Free-form platform identifier. Today always
    /// `"default"` until platform analysis lands.
    pub raw: String,
}

/// Phase 5.C.4 commit 67: stable identity for the
/// toolchain. Distinct from [`ToolIdentity`] which
/// identifies a SINGLE tool — `ToolchainId` identifies the
/// SET of tools (cc + ar + ld + …) the action's rule
/// resolved.
///
/// Today optional; rule-side toolchain identity isn't
/// threaded into `NormalizedCommandSpec` yet (the field
/// is `toolchain_identity: Option<String>` reserved for
/// future).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ToolchainId {
    /// Free-form toolchain identifier. None when the rule
    /// didn't carry one.
    pub raw: Option<String>,
}

/// Phase 5.C.4 commit 67: the canonical action-key input
/// shape.
///
/// This is what the future cache-key derivation will hash.
/// Today it's a pure-data structure with no hash function;
/// it exists so that:
///
///   1. The diagnostic surfaces (`--explain-action-keys`,
///      `--explain-action-graph`) can show what the cache
///      key WOULD comprise.
///   2. The readiness report can flag actions that are not
///      ready to be canonicalized (e.g. host-ambient
///      inputs, inherited host env, missing toolchain
///      identity).
///   3. The future cache layer adapts directly into this
///      shape without re-deriving the structure.
///
/// Pro's commit-67 directive: "Skeleton only. Do not hash.
/// Do not read files. Do not lookup."
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct CanonicalActionKeyInput {
    /// Action mnemonic, unchanged from `ActionKeySpec`.
    pub mnemonic: String,
    /// Tool that executes the action.
    pub tool: ToolIdentity,
    /// argv elements in spawn order. Order significant.
    /// Argv elements that look like paths SHOULD be
    /// normalized; the readiness report flags any element
    /// that contains an absolute host-path-shaped string.
    pub argv: Vec<String>,
    /// Declared env (key, value) pairs sorted by key. ONLY
    /// the declared content — never the inherited host
    /// env. The readiness report flags
    /// [`CanonicalizationBlocker::InheritedHostEnv`] when
    /// the source `env_policy` is `InheritHost`.
    pub env: Vec<(String, String)>,
    /// Declared input paths, each normalized + digested.
    /// Today digests are placeholders.
    pub declared_inputs: Vec<(NormalizedPath, Digest)>,
    /// Declared output paths, normalized.
    pub declared_outputs: Vec<NormalizedPath>,
    /// Build configuration identity.
    pub config: ConfigId,
    /// Target platform identity.
    pub platform: PlatformId,
    /// Toolchain identity (set of tools).
    pub toolchain: ToolchainId,
}

/// Phase 5.C.4 commit 67: a readiness blocker — a reason an
/// `ActionKeySpec` cannot yet be safely canonicalized into a
/// `CanonicalActionKeyInput`.
///
/// JSON shape (commit 54b style):
///   {"kind": "absolute_path_in_argv", "data": "/abs/p"}
///   {"kind": "host_ambient_input", "data": "/usr/include/x.h"}
///   {"kind": "unclassified_path", "data": "..."}
///   {"kind": "inherited_host_env", "data": null}
///   {"kind": "no_toolchain_identity", "data": null}
///   {"kind": "missing_input_digest", "data": "src.cc"}
///   {"kind": "missing_tool_digest", "data": "/usr/bin/g++"}
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum CanonicalizationBlocker {
    /// An argv element looked like an absolute host path.
    /// Carries the offending element so operators can see
    /// which one.
    AbsolutePathInArgv(String),
    /// A declared input classified as host-ambient (e.g.
    /// `/usr/include/...`). The cache key cannot use it
    /// without hermetic-toolchain support.
    HostAmbientInput(PathBuf),
    /// A path didn't classify into any known kind. Carries
    /// the offending path.
    UnclassifiedPath(PathBuf),
    /// The source action's `env_policy` is `InheritHost`
    /// — the spawned process gets the parent's env, which
    /// can't be fingerprinted.
    InheritedHostEnv,
    /// The action's spec carries no toolchain identity
    /// string. The cache key needs a stable toolchain ID
    /// so swapping `gcc-12` for `gcc-13` invalidates.
    NoToolchainIdentity,
    /// A declared input has no content digest. Carries the
    /// path. Today's skeleton always emits placeholder
    /// digests, so this fires for every input — that's
    /// expected behavior for the skeleton phase.
    MissingInputDigest(PathBuf),
    /// The tool has no content digest.
    MissingToolDigest(PathBuf),
}

/// Phase 5.C.4 commit 67: a readiness report attached to a
/// `CanonicalActionKeyInput`.
///
/// `Ready` means the input is structurally complete enough
/// to be safely canonicalized AND digested when the cache
/// layer lands. `NotReady` carries one or more
/// [`CanonicalizationBlocker`]s explaining what's missing.
///
/// Today every input emits at least
/// `MissingInputDigest` / `MissingToolDigest` placeholders
/// (no real digests are computed by the skeleton), so
/// `NotReady` is the expected steady-state output. The
/// `Ready` variant exists for the eventual post-5.C.5
/// cache-key work; the skeleton doesn't produce it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum CanonicalizationReadiness {
    /// All blockers cleared. The input is safe to
    /// canonicalize. Today the skeleton never produces
    /// this variant (digests are always placeholders),
    /// but the type exists so the future cache layer can.
    Ready,
    /// One or more blockers prevent safe canonicalization.
    /// Order: as observed during the
    /// [`CanonicalActionKeyInput::from_action_key_spec`]
    /// walk (executable → argv → env → inputs → outputs →
    /// toolchain).
    NotReady(Vec<CanonicalizationBlocker>),
}

impl CanonicalizationReadiness {
    /// Returns `true` iff every blocker has been cleared.
    pub fn is_ready(&self) -> bool {
        matches!(self, CanonicalizationReadiness::Ready)
    }

    /// Number of blockers (0 when `Ready`).
    pub fn blocker_count(&self) -> usize {
        match self {
            CanonicalizationReadiness::Ready => 0,
            CanonicalizationReadiness::NotReady(b) => b.len(),
        }
    }

    /// Phase 5.C.4 commit 73: collapse duplicate blockers
    /// into counted summary entries.
    ///
    /// Mirrors commit 72's `Cacheability::summarized_reasons`
    /// pattern. First-occurrence ordering — operators see
    /// blockers in the same order the analysis walked them
    /// (executable → argv → env → inputs → outputs →
    /// toolchain). The summary collapses repetitive
    /// `MissingInputDigest` / `MissingToolDigest` /
    /// `HostAmbientInput` runs into one line per variant.
    ///
    /// Returns empty Vec when `Ready` (no blockers).
    ///
    /// Pro's commit-73 directive: "Humans do not need to
    /// read 'MissingInputDigest' 900 times to understand
    /// 'no digests yet.'"
    pub fn summarized_blockers(&self) -> Vec<CanonicalizationBlockerSummary> {
        let blockers = match self {
            CanonicalizationReadiness::Ready => return Vec::new(),
            CanonicalizationReadiness::NotReady(b) => b,
        };
        let mut summary: Vec<CanonicalizationBlockerSummary> = Vec::new();
        for b in blockers {
            // Match on variant identity, not payload —
            // operators want to see "MissingInputDigest × 42"
            // not 42 separate lines distinguished by path.
            // The raw `blockers` Vec retains the per-path
            // detail for callers that need it.
            if let Some(existing) = summary
                .iter_mut()
                .find(|s| same_blocker_variant(&s.blocker, b))
            {
                existing.count += 1;
            } else {
                summary.push(CanonicalizationBlockerSummary {
                    blocker: b.clone(),
                    count: 1,
                });
            }
        }
        summary
    }
}

/// Phase 5.C.4 commit 73: a counted summary of a
/// [`CanonicalizationBlocker`].
///
/// Pairs the blocker variant with the number of times it
/// fired during readiness analysis. The `blocker` field
/// carries the FIRST observed payload for variants that
/// take one (e.g. the first `HostAmbientInput(path)` —
/// subsequent same-variant blockers contribute to `count`
/// without preserving their individual payloads in the
/// summary). Callers that need per-instance payload still
/// have access to the raw `Vec` in
/// `CanonicalizationReadiness::NotReady`.
///
/// Pro's commit-73 directive: "Use the same first-
/// occurrence-order approach as cacheability reasons."
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct CanonicalizationBlockerSummary {
    /// The blocker variant (first-occurrence payload).
    pub blocker: CanonicalizationBlocker,
    /// Number of times this variant fired (>= 1).
    pub count: usize,
}

/// Phase 5.C.4 commit 73: variant-identity comparison for
/// blockers (ignores payload). Two `HostAmbientInput`s
/// with different paths compare equal here so the summary
/// collapses them.
fn same_blocker_variant(
    a: &CanonicalizationBlocker,
    b: &CanonicalizationBlocker,
) -> bool {
    use CanonicalizationBlocker::*;
    matches!(
        (a, b),
        (AbsolutePathInArgv(_), AbsolutePathInArgv(_))
            | (HostAmbientInput(_), HostAmbientInput(_))
            | (UnclassifiedPath(_), UnclassifiedPath(_))
            | (InheritedHostEnv, InheritedHostEnv)
            | (NoToolchainIdentity, NoToolchainIdentity)
            | (MissingInputDigest(_), MissingInputDigest(_))
            | (MissingToolDigest(_), MissingToolDigest(_))
    )
}

impl CanonicalActionKeyInput {
    /// Phase 5.C.4 commit 67: build a `CanonicalActionKeyInput`
    /// from an `ActionKeySpec` and report what's blocking
    /// canonicalization.
    ///
    /// Pure data conversion. No I/O. No hashing. No file
    /// reads. The returned tuple is `(input, readiness)` —
    /// the input is always populated (with placeholders for
    /// fields the skeleton can't fill), the readiness
    /// report enumerates what's blocked.
    ///
    /// Pro's contract: "Just declare the shape and report
    /// honestly." Operators reading the report see exactly
    /// which classifier output, env policy, or missing
    /// metadata is preventing canonicalization for an
    /// action.
    pub fn from_action_key_spec(
        spec: &ActionKeySpec,
    ) -> (Self, CanonicalizationReadiness) {
        use crate::action_key::{HermeticityClass, OutputPathPolicy};

        let mut blockers: Vec<CanonicalizationBlocker> = Vec::new();

        // ---- Tool identity -----------------------------------
        let tool_path = NormalizedPath {
            raw: spec.executable.path.clone(),
            kind: hermeticity_to_path_kind(spec.executable.class.clone()),
        };
        if matches!(tool_path.kind, NormalizedPathKind::Unclassified) {
            blockers.push(CanonicalizationBlocker::UnclassifiedPath(
                spec.executable.path.clone(),
            ));
        }
        // Always: the skeleton does not compute digests, so
        // every tool path carries a missing-digest blocker
        // until the post-5.C.5 cache layer fills it in.
        blockers.push(CanonicalizationBlocker::MissingToolDigest(
            spec.executable.path.clone(),
        ));
        let tool = ToolIdentity {
            path: tool_path,
            toolchain_identity: None,
            content_digest: Digest::placeholder("not computed"),
        };

        // ---- Argv: flag absolute-looking elements ------------
        let argv = spec.argv.clone();
        for arg in &argv {
            if arg.starts_with('/') {
                blockers.push(
                    CanonicalizationBlocker::AbsolutePathInArgv(arg.clone()),
                );
            }
        }

        // ---- Env: inherited-host-env demotion ----------------
        let env = spec.env.clone();
        if matches!(spec.env_policy, ActionEnvPolicy::InheritHost) {
            blockers.push(CanonicalizationBlocker::InheritedHostEnv);
        }

        // ---- Declared inputs ---------------------------------
        let declared_inputs: Vec<(NormalizedPath, Digest)> = spec
            .declared_inputs
            .iter()
            .map(|inp| {
                let np = NormalizedPath {
                    raw: inp.path.clone(),
                    kind: hermeticity_to_path_kind(inp.class.clone()),
                };
                if matches!(inp.class, HermeticityClass::HostAmbientInclude) {
                    blockers.push(
                        CanonicalizationBlocker::HostAmbientInput(
                            inp.path.clone(),
                        ),
                    );
                } else if matches!(
                    inp.class,
                    HermeticityClass::Unclassified
                ) {
                    blockers.push(CanonicalizationBlocker::UnclassifiedPath(
                        inp.path.clone(),
                    ));
                }
                blockers.push(CanonicalizationBlocker::MissingInputDigest(
                    inp.path.clone(),
                ));
                (np, Digest::placeholder("not computed"))
            })
            .collect();

        // ---- Declared outputs --------------------------------
        let declared_outputs: Vec<NormalizedPath> = spec
            .classified_outputs
            .iter()
            .map(|co| {
                let kind = match &co.policy {
                    OutputPathPolicy::WorkspaceRelativeOutput => {
                        NormalizedPathKind::WorkspaceRelative
                    }
                    OutputPathPolicy::GenfilesOutput => {
                        NormalizedPathKind::GenfilesRelative
                    }
                    OutputPathPolicy::ExternalRepoOutput { repo } => {
                        NormalizedPathKind::ExternalRepo(repo.clone())
                    }
                    OutputPathPolicy::Unclassified => {
                        blockers.push(
                            CanonicalizationBlocker::UnclassifiedPath(
                                co.path.clone(),
                            ),
                        );
                        NormalizedPathKind::Unclassified
                    }
                };
                NormalizedPath {
                    raw: co.path.clone(),
                    kind,
                }
            })
            .collect();

        // ---- Toolchain identity (always missing today) -------
        let toolchain = ToolchainId { raw: None };
        blockers.push(CanonicalizationBlocker::NoToolchainIdentity);

        let input = CanonicalActionKeyInput {
            mnemonic: spec.mnemonic.clone(),
            tool,
            argv,
            env,
            declared_inputs,
            declared_outputs,
            config: ConfigId { raw: 0 },
            platform: PlatformId {
                raw: "default".to_string(),
            },
            toolchain,
        };

        let readiness = if blockers.is_empty() {
            CanonicalizationReadiness::Ready
        } else {
            CanonicalizationReadiness::NotReady(blockers)
        };
        (input, readiness)
    }
}

/// Map a `HermeticityClass` to the corresponding
/// `NormalizedPathKind`. Helper for
/// [`CanonicalActionKeyInput::from_action_key_spec`].
fn hermeticity_to_path_kind(
    class: crate::action_key::HermeticityClass,
) -> NormalizedPathKind {
    use crate::action_key::HermeticityClass::*;
    match class {
        WorkspaceLocal => NormalizedPathKind::WorkspaceRelative,
        HostAmbientInclude | HostToolPath => NormalizedPathKind::HostPath,
        EnvOverrideToolPath => NormalizedPathKind::HostPath,
        Unclassified => NormalizedPathKind::Unclassified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action_key::{
        ActionKeySpec, ClassifiedInput, ClassifiedOutput, ClassifiedTool,
        HermeticityClass, OutputPathPolicy,
    };

    fn aks_with(
        env_policy: ActionEnvPolicy,
        executable_class: HermeticityClass,
    ) -> ActionKeySpec {
        ActionKeySpec {
            mnemonic: "CppCompile".to_string(),
            executable: ClassifiedTool {
                path: PathBuf::from("/usr/bin/g++"),
                class: executable_class,
            },
            argv: vec!["-c".to_string(), "src.cc".to_string()],
            env: Vec::new(),
            cwd: None,
            declared_inputs: vec![ClassifiedInput {
                path: PathBuf::from("/home/user/workspace/src.cc"),
                class: HermeticityClass::WorkspaceLocal,
            }],
            declared_outputs: vec![PathBuf::from(
                "/home/user/workspace/rush-out/foo.o",
            )],
            classified_outputs: vec![ClassifiedOutput {
                path: PathBuf::from("/home/user/workspace/rush-out/foo.o"),
                policy: OutputPathPolicy::WorkspaceRelativeOutput,
            }],
            env_policy,
        }
    }

    /// Phase 5.C.4 commit 67: the skeleton's structural
    /// contract — a freshly-built input from a
    /// "clean" ActionKeySpec still has placeholder
    /// digests, so readiness is `NotReady` with
    /// `MissingInputDigest` + `MissingToolDigest` +
    /// `NoToolchainIdentity` as the expected blockers.
    /// This pins that the skeleton does NOT compute
    /// anything beyond shape.
    #[test]
    fn from_action_key_spec_reports_missing_digests_today() {
        let spec = aks_with(
            ActionEnvPolicy::ExplicitOnly,
            HermeticityClass::WorkspaceLocal,
        );
        let (_input, readiness) =
            CanonicalActionKeyInput::from_action_key_spec(&spec);
        assert!(
            !readiness.is_ready(),
            "skeleton must never report Ready (no digests yet)"
        );
        let blockers = match readiness {
            CanonicalizationReadiness::Ready => unreachable!(),
            CanonicalizationReadiness::NotReady(b) => b,
        };
        assert!(blockers.iter().any(|b| matches!(
            b,
            CanonicalizationBlocker::MissingToolDigest(p) if p == &PathBuf::from("/usr/bin/g++")
        )));
        assert!(blockers.iter().any(|b| matches!(
            b,
            CanonicalizationBlocker::MissingInputDigest(p)
                if p == &PathBuf::from("/home/user/workspace/src.cc")
        )));
        assert!(blockers
            .iter()
            .any(|b| matches!(b, CanonicalizationBlocker::NoToolchainIdentity)));
    }

    /// Phase 5.C.4 commit 67: `InheritHost` env_policy
    /// surfaces `InheritedHostEnv` as a canonicalization
    /// blocker. Mirrors commit 66's `CacheabilityReason`
    /// at the canonical-key layer.
    #[test]
    fn inherit_host_env_policy_is_canonicalization_blocker() {
        let spec = aks_with(
            ActionEnvPolicy::InheritHost,
            HermeticityClass::WorkspaceLocal,
        );
        let (_input, readiness) =
            CanonicalActionKeyInput::from_action_key_spec(&spec);
        let blockers = match readiness {
            CanonicalizationReadiness::Ready => panic!("expected NotReady"),
            CanonicalizationReadiness::NotReady(b) => b,
        };
        assert!(blockers
            .iter()
            .any(|b| matches!(b, CanonicalizationBlocker::InheritedHostEnv)));
    }

    /// Phase 5.C.4 commit 67: `ExplicitOnly` env_policy
    /// does NOT emit `InheritedHostEnv`. Pin the contract
    /// in the negative direction.
    #[test]
    fn explicit_only_env_policy_no_inherited_host_env_blocker() {
        let spec = aks_with(
            ActionEnvPolicy::ExplicitOnly,
            HermeticityClass::WorkspaceLocal,
        );
        let (_input, readiness) =
            CanonicalActionKeyInput::from_action_key_spec(&spec);
        let blockers = match readiness {
            CanonicalizationReadiness::Ready => panic!("expected NotReady"),
            CanonicalizationReadiness::NotReady(b) => b,
        };
        assert!(!blockers
            .iter()
            .any(|b| matches!(b, CanonicalizationBlocker::InheritedHostEnv)));
    }

    /// Phase 5.C.4 commit 67: a host-tool executable
    /// (e.g. `/usr/bin/g++`) emits `MissingToolDigest`
    /// AND classifies the tool path as `HostPath`. Pins
    /// that the path-kind classification feeds the
    /// readiness report deterministically.
    #[test]
    fn host_tool_executable_classifies_as_host_path() {
        let spec = aks_with(
            ActionEnvPolicy::ExplicitOnly,
            HermeticityClass::HostToolPath,
        );
        let (input, _readiness) =
            CanonicalActionKeyInput::from_action_key_spec(&spec);
        assert_eq!(input.tool.path.kind, NormalizedPathKind::HostPath);
        assert_eq!(input.tool.path.raw, PathBuf::from("/usr/bin/g++"));
    }

    /// Phase 5.C.4 commit 67: argv elements that look like
    /// absolute host paths are flagged. Pins the
    /// "argv hides paths" detection.
    #[test]
    fn argv_absolute_path_flagged_as_blocker() {
        let mut spec = aks_with(
            ActionEnvPolicy::ExplicitOnly,
            HermeticityClass::WorkspaceLocal,
        );
        spec.argv = vec![
            "-c".to_string(),
            "/abs/path/src.cc".to_string(), // absolute → blocker
            "-o".to_string(),
            "obj.o".to_string(), // relative → not flagged
        ];
        let (_input, readiness) =
            CanonicalActionKeyInput::from_action_key_spec(&spec);
        let blockers = match readiness {
            CanonicalizationReadiness::Ready => panic!("expected NotReady"),
            CanonicalizationReadiness::NotReady(b) => b,
        };
        assert!(blockers.iter().any(|b| matches!(
            b,
            CanonicalizationBlocker::AbsolutePathInArgv(s)
                if s == "/abs/path/src.cc"
        )));
    }

    /// Phase 5.C.4 commit 67: blocker count + is_ready
    /// helper functions. Operators / CI consumers may
    /// gate on counts.
    #[test]
    fn readiness_helpers_blocker_count_and_is_ready() {
        let r = CanonicalizationReadiness::Ready;
        assert!(r.is_ready());
        assert_eq!(r.blocker_count(), 0);
        let r = CanonicalizationReadiness::NotReady(vec![
            CanonicalizationBlocker::InheritedHostEnv,
            CanonicalizationBlocker::NoToolchainIdentity,
        ]);
        assert!(!r.is_ready());
        assert_eq!(r.blocker_count(), 2);
    }

    /// Phase 5.C.4 commit 67: the skeleton has NO public
    /// hash function. This test compiles iff we have NOT
    /// added a hashing API. (Compile-time pin: if a future
    /// commit adds `pub fn hash` or `pub fn digest_of` on
    /// `CanonicalActionKeyInput`, the test below would
    /// need updating, surfacing the policy decision.)
    ///
    /// Pro's directive: "Do not hash." This test exists
    /// to make a future "let's just sneak in a hash
    /// function" PR visible.
    #[test]
    fn no_hash_api_exists_yet() {
        // We construct the input and only call methods that
        // EXIST. The set of methods this test calls is the
        // set of methods that must continue to exist for
        // the skeleton contract to hold.
        let spec = aks_with(
            ActionEnvPolicy::ExplicitOnly,
            HermeticityClass::WorkspaceLocal,
        );
        let (input, readiness) =
            CanonicalActionKeyInput::from_action_key_spec(&spec);
        // Allowed: read fields.
        assert_eq!(input.mnemonic, "CppCompile");
        // Allowed: ask readiness.
        let _ = readiness.is_ready();
        // Allowed: serialize for diagnostics.
        let _ = serde_json::to_string(&input).unwrap();
        // FORBIDDEN today (these methods do not exist):
        //   input.hash()
        //   input.digest()
        //   input.cache_key()
        // The compile error if a reviewer adds them is the
        // tripwire.
    }

    /// Phase 5.C.4 commit 67: `Digest::placeholder` is the
    /// only constructor. Pins that the skeleton has no
    /// content-reading constructor (no `Digest::of_file`,
    /// no `Digest::compute`).
    #[test]
    fn digest_only_constructor_is_placeholder() {
        let d = Digest::placeholder("not computed");
        assert_eq!(d.marker, "not computed");
        let d2 = Digest::placeholder("absent — needs file content");
        assert_ne!(d, d2);
    }

    /// Phase 5.C.4 commit 73: `summarized_blockers()`
    /// collapses repeated variants. Today's skeleton emits
    /// one `MissingInputDigest` per input + one
    /// `MissingToolDigest` + one `NoToolchainIdentity`,
    /// plus per-policy blockers. Pin that duplicates
    /// collapse with count.
    #[test]
    fn blocker_summary_counts_duplicates() {
        let r = CanonicalizationReadiness::NotReady(vec![
            CanonicalizationBlocker::MissingInputDigest(PathBuf::from("a.cc")),
            CanonicalizationBlocker::MissingInputDigest(PathBuf::from("b.cc")),
            CanonicalizationBlocker::MissingInputDigest(PathBuf::from("c.cc")),
            CanonicalizationBlocker::MissingToolDigest(PathBuf::from(
                "/usr/bin/g++",
            )),
            CanonicalizationBlocker::InheritedHostEnv,
            CanonicalizationBlocker::NoToolchainIdentity,
        ]);
        let summary = r.summarized_blockers();
        assert_eq!(summary.len(), 4, "4 distinct variants");
        // First entry: MissingInputDigest × 3.
        assert!(matches!(
            summary[0].blocker,
            CanonicalizationBlocker::MissingInputDigest(_)
        ));
        assert_eq!(summary[0].count, 3);
        // Second: MissingToolDigest × 1.
        assert!(matches!(
            summary[1].blocker,
            CanonicalizationBlocker::MissingToolDigest(_)
        ));
        assert_eq!(summary[1].count, 1);
        // Third: InheritedHostEnv × 1.
        assert!(matches!(
            summary[2].blocker,
            CanonicalizationBlocker::InheritedHostEnv
        ));
        // Fourth: NoToolchainIdentity × 1.
        assert!(matches!(
            summary[3].blocker,
            CanonicalizationBlocker::NoToolchainIdentity
        ));
    }

    /// Phase 5.C.4 commit 73: first-occurrence ordering
    /// preserved. Pro: "Use the same first-occurrence-order
    /// approach as cacheability reasons."
    #[test]
    fn blocker_summary_preserves_first_occurrence_order() {
        let r = CanonicalizationReadiness::NotReady(vec![
            CanonicalizationBlocker::InheritedHostEnv,
            CanonicalizationBlocker::MissingInputDigest(PathBuf::from("a.cc")),
            CanonicalizationBlocker::InheritedHostEnv, // duplicate
            CanonicalizationBlocker::MissingInputDigest(PathBuf::from("b.cc")),
        ]);
        let summary = r.summarized_blockers();
        assert_eq!(summary.len(), 2);
        // InheritedHostEnv came FIRST — must be position 0.
        assert!(matches!(
            summary[0].blocker,
            CanonicalizationBlocker::InheritedHostEnv
        ));
        assert_eq!(summary[0].count, 2);
        assert!(matches!(
            summary[1].blocker,
            CanonicalizationBlocker::MissingInputDigest(_)
        ));
        assert_eq!(summary[1].count, 2);
    }

    /// Phase 5.C.4 commit 73: `Ready` produces an empty
    /// summary. Negative pin.
    #[test]
    fn ready_yields_empty_blocker_summary() {
        let r = CanonicalizationReadiness::Ready;
        assert!(r.summarized_blockers().is_empty());
    }

    /// Phase 5.C.4 commit 73: blocker variants with
    /// different payloads still collapse (the summary
    /// keeps the first observed payload + the count). The
    /// raw `Vec` in `NotReady` retains per-path detail.
    #[test]
    fn host_ambient_input_collapses_with_first_payload() {
        let r = CanonicalizationReadiness::NotReady(vec![
            CanonicalizationBlocker::HostAmbientInput(PathBuf::from(
                "/usr/include/a.h",
            )),
            CanonicalizationBlocker::HostAmbientInput(PathBuf::from(
                "/usr/include/b.h",
            )),
            CanonicalizationBlocker::HostAmbientInput(PathBuf::from(
                "/usr/include/c.h",
            )),
        ]);
        let summary = r.summarized_blockers();
        assert_eq!(summary.len(), 1);
        // First observed payload preserved.
        assert!(matches!(
            &summary[0].blocker,
            CanonicalizationBlocker::HostAmbientInput(p)
                if p == &PathBuf::from("/usr/include/a.h")
        ));
        assert_eq!(summary[0].count, 3);
    }

    /// Phase 5.C.4 commit 73: serializing the summary
    /// produces a parseable JSON array each entry having
    /// {blocker, count}. Pro: "JSON can include either raw
    /// blockers and summary, or just summary under a
    /// separate field. Prefer both."
    #[test]
    fn blocker_summary_serializes_with_count() {
        let r = CanonicalizationReadiness::NotReady(vec![
            CanonicalizationBlocker::MissingInputDigest(PathBuf::from(
                "a.cc",
            )),
            CanonicalizationBlocker::MissingInputDigest(PathBuf::from(
                "b.cc",
            )),
            CanonicalizationBlocker::InheritedHostEnv,
        ]);
        let summary = r.summarized_blockers();
        let json = serde_json::to_string(&summary).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        let arr = parsed.as_array().expect("summary array");
        assert_eq!(arr.len(), 2);
        // First entry: MissingInputDigest, count 2.
        assert_eq!(arr[0]["blocker"]["kind"], "missing_input_digest");
        assert_eq!(arr[0]["count"], 2);
        // Second: InheritedHostEnv, count 1.
        assert_eq!(arr[1]["blocker"]["kind"], "inherited_host_env");
        assert_eq!(arr[1]["count"], 1);
    }
}
