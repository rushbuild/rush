// Rush Action - Normalized command spec for legacy/scheduler verification
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 verifier: shared comparison shape.
//!
//! ChatGPT Pro's review of the 5.C.4 landing called for command
//! verification as a migration gate: before a rule kind is added
//! to the scheduler-verified allowlist, run the legacy and
//! scheduler argv-construction paths and diff the result. This
//! module is the comparison primitive both paths produce.
//!
//! ## Design choices
//!
//! - **argv order is significant** — link-library ordering and
//!   include-path ordering are observable to the build outcome,
//!   so diffs preserve order.
//! - **env / declared inputs / declared outputs / param files are
//!   sorted for deterministic comparison** — these don't have
//!   semantic ordering, and sorting eliminates spurious diffs
//!   from HashMap iteration order. Output paths are sorted at
//!   normalization time, but rule-side construction may want to
//!   sort earlier; the canonical comparison is post-sort.
//! - **paths are kept owned PathBuf** — the verifier compares
//!   absolute paths today; commit 3+ may add an execroot-relative
//!   normalization once the spec builders exist for both paths.
//! - **toolchain identity is a free-form string** — `gcc-12.2.0`,
//!   `ar 2.40`, etc. Builders that don't have a toolchain identity
//!   set it to None; mismatch surfaces both sides for the reviewer.
//!
//! ## What this is NOT
//!
//! - Not a hashing primitive. The cache-key derivation is OKR 4
//!   work and uses different normalization (e.g. content digests
//!   rather than path equality). That layer adapts NormalizedCommandSpec
//!   into ActionKeySpec when it lands.
//! - Not aware of legacy vs scheduler internals. Both sides
//!   construct one of these and pass to `diff`. Construction
//!   helpers (`CcCompileCommandSpec → NormalizedCommandSpec`)
//!   land in commit 3.

use crate::ids::{ActionEnvPolicy, ArtifactRegistry, DeclaredActionId};
use std::path::PathBuf;

/// One side of a legacy-vs-scheduler comparison. Constructed by
/// both paths from their respective argv-assembly logic, then
/// diffed via [`diff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedCommandSpec {
    /// Action mnemonic (`CppCompile`, `CcArchive`, `CcLink`).
    pub mnemonic: String,
    /// Resolved executable path.
    pub executable: PathBuf,
    /// Working directory. `None` means the build's exec root.
    pub cwd: Option<PathBuf>,
    /// Argv in spawn order. Order significant.
    pub argv: Vec<String>,
    /// Env (key, value) pairs sorted by key. Last-write-wins on
    /// duplicate keys is the responsibility of the spec builder;
    /// this is what gets compared.
    pub env: Vec<(String, String)>,
    /// Declared input paths sorted for deterministic comparison.
    /// Order is NOT significant (compiles can be reordered for
    /// the same target without changing semantics).
    pub declared_inputs: Vec<PathBuf>,
    /// Declared output paths sorted for deterministic comparison.
    pub declared_outputs: Vec<PathBuf>,
    /// Free-form toolchain identity (`gcc-12.2.0`, `clang 18`).
    /// `None` when the builder doesn't carry one.
    pub toolchain_identity: Option<String>,
    /// Param files: (path, contents-digest hex string). Sorted by
    /// path. Empty when the action doesn't use param files.
    pub param_files: Vec<(PathBuf, String)>,
    /// Phase 5.C.4 commit 66: env-inheritance policy carried
    /// through from `MaterializedCommand`. The runner consults
    /// this to decide whether to call `env_clear()` before
    /// applying declared env vars; the action-key
    /// cacheability layer consults it to demote when set to
    /// [`ActionEnvPolicy::InheritHost`].
    ///
    /// Default `InheritHost` matches today's
    /// `tokio::process::Command::new` behavior. Specs built
    /// without a real source action (test fixtures, future
    /// hermetic builders) can opt into `ExplicitOnly`.
    pub env_policy: ActionEnvPolicy,
}

impl NormalizedCommandSpec {
    /// Sort env / declared_inputs / declared_outputs / param_files
    /// in place. Builders SHOULD call this before passing to
    /// [`diff`], but [`diff`] doesn't require it (the sort is
    /// idempotent, and pre-sorting lets builders detect duplicate
    /// keys before normalization).
    pub fn normalize(&mut self) {
        self.env.sort_by(|a, b| a.0.cmp(&b.0));
        self.declared_inputs.sort();
        self.declared_outputs.sort();
        self.param_files.sort_by(|a, b| a.0.cmp(&b.0));
    }

    /// Phase 5.C.4 commit 43: build a `NormalizedCommandSpec`
    /// from a registered (DeclaredAction + ExecutableAction)
    /// pair. Returns `None` when the action's
    /// `ExecutableAction` slot is empty (legacy-only action,
    /// not yet migrated to the typed execution path).
    ///
    /// Resolves declared_inputs / declared_outputs by walking
    /// `DeclaredAction.inputs` / `.outputs` (Box<[ArtifactId]>)
    /// through `registry.artifact_path(id)`. The result is a
    /// pure-data spec suitable for diagnostic dumps,
    /// ActionKeySpec construction, or future cache-key
    /// computation.
    ///
    /// Pro's commit-32 dry-run guard still applies — this is
    /// the bridge from the registry's typed action graph to
    /// the diagnostic spec; no cache reuse happens here.
    ///
    /// `toolchain_identity` and `param_files` stay default
    /// (None / empty) because the registry doesn't carry them
    /// today; future commits can extend the registry to
    /// thread those through. Their absence doesn't affect the
    /// diagnostic dump shape.
    pub fn from_registry_action(
        reg: &ArtifactRegistry,
        action_id: DeclaredActionId,
    ) -> Option<Self> {
        let exec = reg.executable_action(action_id)?;
        let mat = exec.materialize(reg);
        let action = reg.declared_action(action_id);
        let mnemonic = reg.strings().str(action.mnemonic).to_string();
        let declared_inputs: Vec<PathBuf> = action
            .inputs
            .iter()
            .map(|aid| reg.artifact_path(*aid).to_path_buf())
            .collect();
        let declared_outputs: Vec<PathBuf> = action
            .outputs
            .iter()
            .map(|aid| reg.artifact_path(*aid).to_path_buf())
            .collect();
        let mut spec = NormalizedCommandSpec {
            mnemonic,
            executable: mat.executable,
            cwd: mat.working_dir,
            argv: mat.argv,
            env: mat.env,
            declared_inputs,
            declared_outputs,
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: mat.env_policy,
        };
        spec.normalize();
        Some(spec)
    }
}

/// Phase 5.C.4 commit 26: argv well-formedness check. The
/// verifier's [`diff`] function tells you whether legacy and
/// scheduler specs MATCH; well-formedness tells you whether
/// the matched command is structurally valid before spawn.
///
/// Pro's commit-26 directive: "the verifier compares specs but
/// not their well-formedness. Two identical bad commands are
/// still bad." The empty `-I` bug from commit 25 was an
/// instance: both spec sides emitted `-I` with no path, the
/// verifier said "match legacy," and `g++` rejected at exec
/// time. A well-formedness check catches that class of bug
/// before spawn.
///
/// Conservative checks only:
///   - argv elements are non-empty
///   - argv elements contain no embedded NUL bytes
///   - separated flags that require an operand
///     (`-I`, `-D`, `-U`, `-L`, `-isystem`, `-iquote`,
///      `-include`, `-o`, `-MF`, `-MT`, `-MQ`) are followed
///     by a non-empty operand
///   - joined forms (`-Ifoo`, `-DNAME=value`, `-Lpath`) are
///     allowed
///
/// Per Pro's directive, this DOES NOT:
///   - hard-error duplicate flags (sometimes intentional)
///   - reject shell metacharacters (argv-array execution
///     means *, ;, $, spaces are not inherently unsafe)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandWellFormednessError {
    /// argv index where the problem was detected.
    pub argv_index: usize,
    /// The offending argv element (or the flag whose operand
    /// is missing).
    pub offending_arg: String,
    /// What's wrong with it.
    pub reason: WellFormednessReason,
}

/// Reasons a [`NormalizedCommandSpec`] argv element fails the
/// well-formedness check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WellFormednessReason {
    /// The argv element was empty (zero-length string).
    EmptyArg,
    /// The argv element contained an embedded NUL byte. NUL
    /// in argv terminates the C string at exec time, so an
    /// embedded NUL truncates the argument silently.
    EmbeddedNul,
    /// A flag that requires an operand was followed by an
    /// empty argv element. Example: `["-I", ""]` instead of
    /// `["-I", "/path"]` or the joined form `["-I/path"]`.
    MissingOperand { flag: String },
    /// A flag that requires an operand was the LAST argv
    /// element with no following operand. Example: argv ends
    /// with `["-I"]` and there's nothing after it.
    OperandTruncated { flag: String },
}

/// Set of separated flags that require a following operand
/// (i.e. when the flag appears as a standalone argv element,
/// the NEXT argv element is the operand). Joined forms like
/// `-Ifoo` / `-DNAME=value` are NOT in this set — they don't
/// need a following element.
const FLAGS_REQUIRING_OPERAND: &[&str] = &[
    "-I", "-D", "-U", "-L", "-isystem", "-iquote", "-include",
    "-o", "-MF", "-MT", "-MQ",
];

/// Phase 5.C.4 commit 26: validate argv structural sanity.
/// Returns the first detected well-formedness error, or
/// `None` if the argv is structurally valid.
///
/// Returns at most one error per call. Callers can iterate
/// by fixing one error and re-checking, but in practice if
/// any well-formedness error exists, the spec is broken
/// enough that one error is informative enough.
pub fn validate_command_well_formed(
    spec: &NormalizedCommandSpec,
) -> Option<CommandWellFormednessError> {
    let argv = &spec.argv;
    for (i, arg) in argv.iter().enumerate() {
        if arg.is_empty() {
            return Some(CommandWellFormednessError {
                argv_index: i,
                offending_arg: arg.clone(),
                reason: WellFormednessReason::EmptyArg,
            });
        }
        if arg.contains('\0') {
            return Some(CommandWellFormednessError {
                argv_index: i,
                offending_arg: arg.clone(),
                reason: WellFormednessReason::EmbeddedNul,
            });
        }
        if FLAGS_REQUIRING_OPERAND.contains(&arg.as_str()) {
            // This is a separated flag — the operand is the
            // NEXT argv element. Must exist and be non-empty.
            match argv.get(i + 1) {
                None => {
                    return Some(CommandWellFormednessError {
                        argv_index: i,
                        offending_arg: arg.clone(),
                        reason: WellFormednessReason::OperandTruncated {
                            flag: arg.clone(),
                        },
                    });
                }
                Some(next) if next.is_empty() => {
                    return Some(CommandWellFormednessError {
                        argv_index: i + 1,
                        offending_arg: next.clone(),
                        reason: WellFormednessReason::MissingOperand {
                            flag: arg.clone(),
                        },
                    });
                }
                Some(_) => {}
            }
        }
    }
    None
}

/// Format a [`CommandWellFormednessError`] as a human-readable
/// string for diagnostics. The output is one line, newline-free.
pub fn format_well_formedness_error(
    err: &CommandWellFormednessError,
) -> String {
    match &err.reason {
        WellFormednessReason::EmptyArg => {
            format!("argv[{}] is empty", err.argv_index)
        }
        WellFormednessReason::EmbeddedNul => {
            format!(
                "argv[{}] {:?} contains an embedded NUL byte",
                err.argv_index, err.offending_arg
            )
        }
        WellFormednessReason::MissingOperand { flag } => {
            format!(
                "argv[{}] empty operand for flag {:?}",
                err.argv_index, flag
            )
        }
        WellFormednessReason::OperandTruncated { flag } => {
            format!(
                "argv[{}] flag {:?} has no following operand (argv ends here)",
                err.argv_index, flag
            )
        }
    }
}

/// Per-field divergence between legacy and scheduler command
/// specs. Each field is `Some` when that field disagreed.
#[derive(Debug, Default)]
pub struct CommandSpecDiff {
    pub mnemonic: Option<(String, String)>,
    pub executable: Option<(PathBuf, PathBuf)>,
    pub cwd: Option<(Option<PathBuf>, Option<PathBuf>)>,
    pub argv: Option<ArgvDiff>,
    /// Per-key env divergences. Empty Vec means no env diff.
    pub env: Vec<EnvDiffEntry>,
    /// Inputs only on one side. Empty Vec means inputs match.
    pub inputs: Vec<PathDiffEntry>,
    /// Outputs only on one side. Empty Vec means outputs match.
    pub outputs: Vec<PathDiffEntry>,
    pub toolchain_identity: Option<(Option<String>, Option<String>)>,
    /// Param-file divergences (path missing or digest mismatch).
    pub param_files: Vec<ParamFileDiffEntry>,
    /// Phase 5.C.4 commit 66: env-policy divergence. `Some`
    /// when one side declares `ExplicitOnly` and the other
    /// `InheritHost`. Pro's commit-66 directive: a builder
    /// that says "hermetic env" but executes through a runner
    /// that inherits is silent corruption — surface the
    /// disagreement.
    pub env_policy: Option<(ActionEnvPolicy, ActionEnvPolicy)>,
}

impl CommandSpecDiff {
    /// True if every field matched (no divergence found).
    pub fn is_empty(&self) -> bool {
        self.mnemonic.is_none()
            && self.executable.is_none()
            && self.cwd.is_none()
            && self.argv.is_none()
            && self.env.is_empty()
            && self.inputs.is_empty()
            && self.outputs.is_empty()
            && self.toolchain_identity.is_none()
            && self.param_files.is_empty()
            && self.env_policy.is_none()
    }
}

/// Argv divergence detail. `first_differing_index` is the first
/// index where the two argv slices disagreed (the link-library
/// ordering bug class hides exactly here).
#[derive(Debug)]
pub struct ArgvDiff {
    pub first_differing_index: usize,
    pub legacy_argv: Vec<String>,
    pub scheduler_argv: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSide {
    LegacyOnly,
    SchedulerOnly,
}

#[derive(Debug)]
pub struct EnvDiffEntry {
    pub key: String,
    pub legacy_value: Option<String>,
    pub scheduler_value: Option<String>,
}

#[derive(Debug)]
pub struct PathDiffEntry {
    pub path: PathBuf,
    pub side: DiffSide,
}

#[derive(Debug)]
pub struct ParamFileDiffEntry {
    pub path: PathBuf,
    pub legacy_digest: Option<String>,
    pub scheduler_digest: Option<String>,
}

/// Compare two normalized command specs. Returns a
/// [`CommandSpecDiff`] populated with every field that
/// disagreed. Empty diff = perfect match.
///
/// Pre-conditions: both specs SHOULD have been
/// [`NormalizedCommandSpec::normalize`]'d before calling this so
/// the env / inputs / outputs / param_files lists are in
/// canonical sort order. The diff is correct either way (it
/// re-sorts internally for the set-difference logic), but
/// pre-normalization makes argv comparison cheaper.
pub fn diff(legacy: &NormalizedCommandSpec, scheduler: &NormalizedCommandSpec) -> CommandSpecDiff {
    let mut d = CommandSpecDiff::default();

    if legacy.mnemonic != scheduler.mnemonic {
        d.mnemonic = Some((legacy.mnemonic.clone(), scheduler.mnemonic.clone()));
    }
    if legacy.executable != scheduler.executable {
        d.executable = Some((legacy.executable.clone(), scheduler.executable.clone()));
    }
    if legacy.cwd != scheduler.cwd {
        d.cwd = Some((legacy.cwd.clone(), scheduler.cwd.clone()));
    }

    // Argv: order-significant. Find first divergent index.
    if legacy.argv != scheduler.argv {
        let first_idx = legacy
            .argv
            .iter()
            .zip(scheduler.argv.iter())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| legacy.argv.len().min(scheduler.argv.len()));
        d.argv = Some(ArgvDiff {
            first_differing_index: first_idx,
            legacy_argv: legacy.argv.clone(),
            scheduler_argv: scheduler.argv.clone(),
        });
    }

    // Env: per-key set difference. Both sides should be sorted
    // by key. Walk both as merge-sort to find disagreements.
    {
        let l: std::collections::BTreeMap<&str, &str> =
            legacy.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let s: std::collections::BTreeMap<&str, &str> = scheduler
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let all_keys: std::collections::BTreeSet<&str> =
            l.keys().chain(s.keys()).copied().collect();
        for k in all_keys {
            let lv = l.get(k).copied();
            let sv = s.get(k).copied();
            if lv != sv {
                d.env.push(EnvDiffEntry {
                    key: k.to_string(),
                    legacy_value: lv.map(|x| x.to_string()),
                    scheduler_value: sv.map(|x| x.to_string()),
                });
            }
        }
    }

    // Inputs / outputs: set difference (treat as unordered).
    fn set_diff(legacy: &[PathBuf], scheduler: &[PathBuf]) -> Vec<PathDiffEntry> {
        let l: std::collections::BTreeSet<&PathBuf> = legacy.iter().collect();
        let s: std::collections::BTreeSet<&PathBuf> = scheduler.iter().collect();
        let mut out = Vec::new();
        for p in l.difference(&s) {
            out.push(PathDiffEntry {
                path: (*p).clone(),
                side: DiffSide::LegacyOnly,
            });
        }
        for p in s.difference(&l) {
            out.push(PathDiffEntry {
                path: (*p).clone(),
                side: DiffSide::SchedulerOnly,
            });
        }
        out
    }
    d.inputs = set_diff(&legacy.declared_inputs, &scheduler.declared_inputs);
    d.outputs = set_diff(&legacy.declared_outputs, &scheduler.declared_outputs);

    if legacy.toolchain_identity != scheduler.toolchain_identity {
        d.toolchain_identity = Some((
            legacy.toolchain_identity.clone(),
            scheduler.toolchain_identity.clone(),
        ));
    }

    if legacy.env_policy != scheduler.env_policy {
        d.env_policy = Some((legacy.env_policy, scheduler.env_policy));
    }

    // Param files: by path, compare digests.
    {
        let l: std::collections::BTreeMap<&PathBuf, &str> = legacy
            .param_files
            .iter()
            .map(|(p, d)| (p, d.as_str()))
            .collect();
        let s: std::collections::BTreeMap<&PathBuf, &str> = scheduler
            .param_files
            .iter()
            .map(|(p, d)| (p, d.as_str()))
            .collect();
        let all_paths: std::collections::BTreeSet<&PathBuf> =
            l.keys().chain(s.keys()).copied().collect();
        for p in all_paths {
            let lv = l.get(p).copied();
            let sv = s.get(p).copied();
            if lv != sv {
                d.param_files.push(ParamFileDiffEntry {
                    path: (*p).clone(),
                    legacy_digest: lv.map(|x| x.to_string()),
                    scheduler_digest: sv.map(|x| x.to_string()),
                });
            }
        }
    }

    d
}

/// Format a [`CommandSpecDiff`] for human consumption — one
/// line per disagreement, suitable for stderr surfacing during
/// `--verify-scheduler-commands` runs. Empty diff produces
/// nothing.
pub fn format_diff(d: &CommandSpecDiff) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some((l, s)) = &d.mnemonic {
        lines.push(format!("  mnemonic: legacy={} scheduler={}", l, s));
    }
    if let Some((l, s)) = &d.executable {
        lines.push(format!(
            "  executable: legacy={} scheduler={}",
            l.display(),
            s.display()
        ));
    }
    if let Some((l, s)) = &d.cwd {
        lines.push(format!("  cwd: legacy={:?} scheduler={:?}", l, s));
    }
    if let Some(a) = &d.argv {
        lines.push(format!(
            "  argv: first divergent index = {} (legacy len {}, scheduler len {})",
            a.first_differing_index,
            a.legacy_argv.len(),
            a.scheduler_argv.len()
        ));
        // Show a small window around the divergence so reviewer
        // can see what's around it without dumping the whole list.
        let from = a.first_differing_index.saturating_sub(2);
        let to_l = (a.first_differing_index + 3).min(a.legacy_argv.len());
        let to_s = (a.first_differing_index + 3).min(a.scheduler_argv.len());
        lines.push(format!(
            "    legacy[{}..{}]: {:?}",
            from, to_l, &a.legacy_argv[from..to_l]
        ));
        lines.push(format!(
            "    scheduler[{}..{}]: {:?}",
            from, to_s, &a.scheduler_argv[from..to_s]
        ));
    }
    for e in &d.env {
        lines.push(format!(
            "  env[{}]: legacy={:?} scheduler={:?}",
            e.key, e.legacy_value, e.scheduler_value
        ));
    }
    for p in &d.inputs {
        lines.push(format!("  input ({:?}-only): {}", p.side, p.path.display()));
    }
    for p in &d.outputs {
        lines.push(format!("  output ({:?}-only): {}", p.side, p.path.display()));
    }
    if let Some((l, s)) = &d.toolchain_identity {
        lines.push(format!(
            "  toolchain_identity: legacy={:?} scheduler={:?}",
            l, s
        ));
    }
    for p in &d.param_files {
        lines.push(format!(
            "  param_file[{}]: legacy_digest={:?} scheduler_digest={:?}",
            p.path.display(),
            p.legacy_digest,
            p.scheduler_digest
        ));
    }
    if let Some((l, s)) = &d.env_policy {
        lines.push(format!(
            "  env_policy: legacy={:?} scheduler={:?}",
            l, s
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(argv: &[&str]) -> NormalizedCommandSpec {
        NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
            declared_inputs: vec![PathBuf::from("src.cc")],
            declared_outputs: vec![PathBuf::from("obj.o")],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: ActionEnvPolicy::InheritHost,
        }
    }

    #[test]
    fn matching_specs_yield_empty_diff() {
        let a = spec(&["-c", "src.cc", "-o", "obj.o"]);
        let b = spec(&["-c", "src.cc", "-o", "obj.o"]);
        let d = diff(&a, &b);
        assert!(d.is_empty());
        assert!(format_diff(&d).is_empty());
    }

    #[test]
    fn argv_divergence_reports_first_differing_index() {
        let a = spec(&["-c", "-O2", "src.cc", "-o", "obj.o"]);
        let b = spec(&["-c", "-O3", "src.cc", "-o", "obj.o"]);
        let d = diff(&a, &b);
        assert!(!d.is_empty());
        let argv = d.argv.as_ref().expect("argv diff");
        assert_eq!(argv.first_differing_index, 1);
        let lines = format_diff(&d);
        assert!(lines.iter().any(|l| l.contains("argv: first divergent")));
    }

    #[test]
    fn env_divergence_per_key() {
        let mut a = spec(&[]);
        a.env = vec![("PATH".to_string(), "/usr/bin".to_string())];
        let mut b = spec(&[]);
        b.env = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("LANG".to_string(), "C".to_string()),
        ];
        let d = diff(&a, &b);
        assert_eq!(d.env.len(), 1);
        assert_eq!(d.env[0].key, "LANG");
        assert!(d.env[0].legacy_value.is_none());
        assert_eq!(d.env[0].scheduler_value.as_deref(), Some("C"));
    }

    #[test]
    fn inputs_outputs_treated_as_sets() {
        let mut a = spec(&[]);
        a.declared_inputs = vec![PathBuf::from("a.h"), PathBuf::from("b.h")];
        let mut b = spec(&[]);
        b.declared_inputs = vec![PathBuf::from("b.h"), PathBuf::from("a.h")]; // reversed
        // Inputs should match despite order difference (set semantics).
        let d = diff(&a, &b);
        assert!(d.inputs.is_empty(), "input order shouldn't matter");
    }

    #[test]
    fn missing_input_on_scheduler_side_surfaces() {
        let mut a = spec(&[]);
        a.declared_inputs = vec![PathBuf::from("a.h"), PathBuf::from("b.h")];
        let mut b = spec(&[]);
        b.declared_inputs = vec![PathBuf::from("a.h")]; // missing b.h
        let d = diff(&a, &b);
        assert_eq!(d.inputs.len(), 1);
        assert_eq!(d.inputs[0].path, PathBuf::from("b.h"));
        assert_eq!(d.inputs[0].side, DiffSide::LegacyOnly);
    }

    #[test]
    fn mnemonic_executable_cwd_diffs() {
        let mut a = spec(&[]);
        let mut b = spec(&[]);
        b.mnemonic = "CcLink".to_string();
        b.executable = PathBuf::from("/usr/bin/clang++");
        b.cwd = Some(PathBuf::from("/build"));
        // a stays default: same mnemonic CppCompile, /usr/bin/g++, no cwd
        a.mnemonic = "CppCompile".to_string();
        let d = diff(&a, &b);
        assert!(d.mnemonic.is_some());
        assert!(d.executable.is_some());
        assert!(d.cwd.is_some());
    }

    /// Phase 5.C.4 commit 66: env_policy mismatch surfaces
    /// in the diff. A builder that says "hermetic env"
    /// against a runner that inherits is silent corruption;
    /// the verifier MUST surface the disagreement.
    #[test]
    fn env_policy_mismatch_surfaces_in_diff() {
        let mut a = spec(&[]);
        let mut b = spec(&[]);
        a.env_policy = ActionEnvPolicy::InheritHost;
        b.env_policy = ActionEnvPolicy::ExplicitOnly;
        let d = diff(&a, &b);
        assert!(!d.is_empty());
        let (l, s) = d.env_policy.expect("env_policy diff");
        assert_eq!(l, ActionEnvPolicy::InheritHost);
        assert_eq!(s, ActionEnvPolicy::ExplicitOnly);
        let lines = format_diff(&d);
        assert!(
            lines.iter().any(|l| l.contains("env_policy:")),
            "format_diff must surface env_policy line; got {:?}",
            lines
        );
    }

    /// Phase 5.C.4 commit 66: matching env_policy on both
    /// sides yields no env_policy divergence in the diff.
    #[test]
    fn env_policy_match_yields_no_divergence() {
        let mut a = spec(&[]);
        let mut b = spec(&[]);
        a.env_policy = ActionEnvPolicy::ExplicitOnly;
        b.env_policy = ActionEnvPolicy::ExplicitOnly;
        let d = diff(&a, &b);
        assert!(d.env_policy.is_none());
    }

    #[test]
    fn normalize_sorts_env_inputs_outputs() {
        let mut s = NormalizedCommandSpec {
            mnemonic: "X".to_string(),
            executable: PathBuf::from("/bin/true"),
            cwd: None,
            argv: vec!["a".to_string(), "b".to_string()],
            env: vec![
                ("Z".to_string(), "1".to_string()),
                ("A".to_string(), "2".to_string()),
            ],
            declared_inputs: vec![PathBuf::from("z.h"), PathBuf::from("a.h")],
            declared_outputs: vec![PathBuf::from("z.o"), PathBuf::from("a.o")],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: ActionEnvPolicy::InheritHost,
        };
        s.normalize();
        assert_eq!(s.env[0].0, "A");
        assert_eq!(s.env[1].0, "Z");
        assert_eq!(s.declared_inputs[0], PathBuf::from("a.h"));
        assert_eq!(s.declared_inputs[1], PathBuf::from("z.h"));
        assert_eq!(s.declared_outputs[0], PathBuf::from("a.o"));
        // argv NOT sorted (order significant).
        assert_eq!(s.argv, vec!["a".to_string(), "b".to_string()]);
    }

    // Phase 5.C.4 commit 26: argv well-formedness checks.

    #[test]
    fn well_formed_normal_argv_passes() {
        let s = spec(&["-c", "-O2", "src.cc", "-o", "obj.o"]);
        assert!(validate_command_well_formed(&s).is_none());
    }

    #[test]
    fn well_formed_joined_flag_forms_pass() {
        // -Ifoo, -DNAME=value, -Ldir are valid joined forms.
        let s = spec(&[
            "-c", "-Ipath/to/dir", "-DENABLE_X=1", "-Llib", "src.cc",
            "-o", "obj.o",
        ]);
        assert!(validate_command_well_formed(&s).is_none());
    }

    #[test]
    fn well_formed_separated_flag_with_operand_passes() {
        // -I path is the separated form; valid when followed by a
        // non-empty operand.
        let s = spec(&["-c", "-I", "path/to/dir", "-o", "obj.o"]);
        assert!(validate_command_well_formed(&s).is_none());
    }

    #[test]
    fn well_formed_empty_arg_rejected() {
        let s = spec(&["-c", "", "src.cc", "-o", "obj.o"]);
        let err = validate_command_well_formed(&s).expect("empty arg should fail");
        assert_eq!(err.argv_index, 1);
        assert_eq!(err.reason, WellFormednessReason::EmptyArg);
    }

    #[test]
    fn well_formed_empty_operand_for_separated_flag_rejected() {
        // commit 25 bug 1 in test form: -I followed by "".
        let s = spec(&["-c", "src.cc", "-o", "obj.o", "-I", ""]);
        let err = validate_command_well_formed(&s)
            .expect("-I followed by empty operand should fail");
        assert_eq!(err.argv_index, 5);
        assert_eq!(
            err.reason,
            WellFormednessReason::MissingOperand {
                flag: "-I".to_string(),
            }
        );
    }

    #[test]
    fn well_formed_truncated_separated_flag_rejected() {
        // -I as the last argv element with nothing after it.
        let s = spec(&["-c", "src.cc", "-o", "obj.o", "-I"]);
        let err = validate_command_well_formed(&s)
            .expect("-I as last argv element should fail");
        assert_eq!(err.argv_index, 4);
        assert_eq!(
            err.reason,
            WellFormednessReason::OperandTruncated {
                flag: "-I".to_string(),
            }
        );
    }

    #[test]
    fn well_formed_embedded_nul_rejected() {
        let s = spec(&["-c", "src\0injected", "-o", "obj.o"]);
        let err = validate_command_well_formed(&s)
            .expect("NUL byte in argv should fail");
        assert_eq!(err.argv_index, 1);
        assert_eq!(err.reason, WellFormednessReason::EmbeddedNul);
    }

    #[test]
    fn well_formed_does_not_hard_error_on_duplicates() {
        // Pro: "Duplicate -I, -D, -L can be intentional or at least
        // tolerated. Report duplicates as optional diagnostics later,
        // not a commit-26 hard failure."
        let s = spec(&[
            "-c",
            "-I/a",
            "-I/b",
            "-I/a", // duplicate -I
            "-DFLAG=1",
            "-DFLAG=1", // duplicate -D
            "src.cc",
            "-o",
            "obj.o",
        ]);
        assert!(validate_command_well_formed(&s).is_none());
    }

    #[test]
    fn well_formed_does_not_reject_shell_metacharacters() {
        // argv-array execution: shell meta is not inherently unsafe.
        // Pro: "argv-array execution, characters like *, ;, $, and
        // spaces are not inherently unsafe. Rejecting them could break
        // valid paths."
        let s = spec(&[
            "-c",
            "/path/with $special* chars; semicolon",
            "-o",
            "obj.o",
        ]);
        assert!(validate_command_well_formed(&s).is_none());
    }

    #[test]
    fn well_formed_covers_all_separated_flags() {
        // All 11 flags in FLAGS_REQUIRING_OPERAND should fire when
        // followed by an empty operand.
        for flag in &[
            "-I", "-D", "-U", "-L", "-isystem", "-iquote", "-include",
            "-o", "-MF", "-MT", "-MQ",
        ] {
            let argv: Vec<String> =
                vec!["-c".to_string(), flag.to_string(), "".to_string()];
            let mut s = spec(&[]);
            s.argv = argv;
            let err = validate_command_well_formed(&s)
                .unwrap_or_else(|| panic!("flag {} should require operand", flag));
            match &err.reason {
                WellFormednessReason::MissingOperand { flag: f } if f == flag => {
                    // ok
                }
                other => panic!(
                    "expected MissingOperand for {}, got {:?}",
                    flag, other
                ),
            }
        }
    }

    // Phase 5.C.4 commit 43: from_registry_action tests.
    // Bridges DeclaredAction + ExecutableAction → NormalizedCommandSpec.
    // Pin: presence/absence of ExecutableAction routes to
    // Some/None; declared_inputs/outputs resolve through the
    // registry's path interner; mnemonic resolves through
    // the string interner; result is normalized in place
    // (sorted env + inputs + outputs).

    use crate::ids::{
        ActionDeclarationStatus, ArtifactKey, ArtifactKind, ConfigId,
        DeclaredAction, ExecutableAction, TargetId,
    };
    use std::path::Path;

    fn mk_artifact(
        reg: &mut ArtifactRegistry,
        name: &str,
        kind: ArtifactKind,
    ) -> crate::ids::ArtifactId {
        let p = reg.intern_path(Path::new(name));
        reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: p,
            },
            kind,
            matches!(kind, ArtifactKind::Source),
            TargetId(1),
            None,
        )
    }

    #[test]
    fn from_registry_action_returns_none_when_no_executable_action() {
        // Action is Complete but no set_executable_action call
        // — typical for legacy-only rules. from_registry_action
        // must return None so callers (e.g. orchestrator's
        // explain-action-keys path) skip those entries.
        let mut reg = ArtifactRegistry::new();
        let src = mk_artifact(&mut reg, "src.cc", ArtifactKind::Source);
        let obj = mk_artifact(&mut reg, "obj.o", ArtifactKind::Object);
        let m = reg.intern_string("CppCompile");
        let id = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m,
                inputs: Box::new([src]),
                outputs: Box::new([obj]),
                status: ActionDeclarationStatus::Complete,
            })
            .expect("declare ok");
        let result = NormalizedCommandSpec::from_registry_action(&reg, id);
        assert!(result.is_none());
    }

    #[test]
    fn from_registry_action_resolves_paths_argv_env_via_interners() {
        let mut reg = ArtifactRegistry::new();
        let src = mk_artifact(
            &mut reg,
            "/home/user/workspace/src.cc",
            ArtifactKind::Source,
        );
        let obj = mk_artifact(
            &mut reg,
            "/home/user/workspace/rush-out/obj.o",
            ArtifactKind::Object,
        );
        let m = reg.intern_string("CppCompile");
        let id = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m,
                inputs: Box::new([src]),
                outputs: Box::new([obj]),
                status: ActionDeclarationStatus::Complete,
            })
            .expect("declare ok");
        let exe = reg.intern_path(Path::new("/usr/bin/g++"));
        let arg_c = reg.intern_string("-c");
        let arg_src = reg.intern_string("src.cc");
        let env_k = reg.intern_string("CC");
        let env_v = reg.intern_string("g++");
        reg.set_executable_action(
            id,
            ExecutableAction {
                executable: exe,
                argv: Box::new([arg_c, arg_src]),
                env: Box::new([(env_k, env_v)]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );
        let spec = NormalizedCommandSpec::from_registry_action(&reg, id)
            .expect("Some when ExecutableAction is set");
        assert_eq!(spec.mnemonic, "CppCompile");
        assert_eq!(spec.executable, PathBuf::from("/usr/bin/g++"));
        assert_eq!(spec.argv, vec!["-c".to_string(), "src.cc".to_string()]);
        assert_eq!(
            spec.env,
            vec![("CC".to_string(), "g++".to_string())]
        );
        assert_eq!(
            spec.declared_inputs,
            vec![PathBuf::from("/home/user/workspace/src.cc")]
        );
        assert_eq!(
            spec.declared_outputs,
            vec![PathBuf::from("/home/user/workspace/rush-out/obj.o")]
        );
        assert_eq!(spec.cwd, None);
        assert_eq!(spec.toolchain_identity, None);
        assert!(spec.param_files.is_empty());
    }

    #[test]
    fn from_registry_action_normalizes_in_place() {
        // Multiple inputs registered out-of-order; the
        // resulting spec should have declared_inputs sorted
        // (per NormalizedCommandSpec::normalize contract).
        let mut reg = ArtifactRegistry::new();
        let z = mk_artifact(&mut reg, "z.cc", ArtifactKind::Source);
        let a = mk_artifact(&mut reg, "a.cc", ArtifactKind::Source);
        let m_ = mk_artifact(&mut reg, "m.cc", ArtifactKind::Source);
        let obj = mk_artifact(&mut reg, "obj.o", ArtifactKind::Object);
        let mn = reg.intern_string("CppCompile");
        let id = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: mn,
                inputs: Box::new([z, a, m_]),
                outputs: Box::new([obj]),
                status: ActionDeclarationStatus::Complete,
            })
            .expect("declare ok");
        let exe = reg.intern_path(Path::new("/usr/bin/g++"));
        reg.set_executable_action(
            id,
            ExecutableAction {
                executable: exe,
                argv: Box::new([]),
                env: Box::new([]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );
        let spec = NormalizedCommandSpec::from_registry_action(&reg, id)
            .expect("Some");
        assert_eq!(
            spec.declared_inputs,
            vec![
                PathBuf::from("a.cc"),
                PathBuf::from("m.cc"),
                PathBuf::from("z.cc"),
            ]
        );
    }

    #[test]
    fn from_registry_action_handles_working_dir_and_empty_inputs() {
        // Action with no declared inputs (e.g. a pure
        // codegen action that reads no files), explicit
        // working_dir set. Both shapes should round-trip.
        let mut reg = ArtifactRegistry::new();
        let out = mk_artifact(
            &mut reg,
            "/home/user/workspace/rush-out/gen.h",
            ArtifactKind::Header,
        );
        let m = reg.intern_string("ProtocCompile");
        let id = reg
            .declare_action(DeclaredAction {
                owner_target: TargetId(1),
                mnemonic: m,
                inputs: Box::new([]),
                outputs: Box::new([out]),
                status: ActionDeclarationStatus::Complete,
            })
            .expect("declare ok");
        let exe = reg.intern_path(Path::new("/usr/bin/protoc"));
        let cwd = reg.intern_path(Path::new("/home/user/workspace"));
        reg.set_executable_action(
            id,
            ExecutableAction {
                executable: exe,
                argv: Box::new([]),
                env: Box::new([]),
                working_dir: Some(cwd),
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );
        let spec = NormalizedCommandSpec::from_registry_action(&reg, id)
            .expect("Some");
        assert!(spec.declared_inputs.is_empty());
        assert_eq!(
            spec.declared_outputs,
            vec![PathBuf::from("/home/user/workspace/rush-out/gen.h")]
        );
        assert_eq!(
            spec.cwd,
            Some(PathBuf::from("/home/user/workspace"))
        );
    }
}
