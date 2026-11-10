use anyhow::Result;
use async_trait::async_trait;
use rush_action::{Action, ActionResult};
use rush_cache::ActionCache;
use rush_exec::ExecutionStrategy;
use rush_resource::{ResourceManager, ResourceRequest, TaskType};
use rush_rules::context::ActionManager;
use std::sync::Arc;

/// A synchronous fence invoked immediately before an action may write files.
/// Retained cache hits do not cross this boundary.
pub(crate) type BeforeOutputMutation = dyn Fn() -> Result<()> + Send + Sync;

/// Action manager implementation that handles caching and execution.
///
/// Phase 5.C.4 commit 7: now acquires `ResourceManager` permits
/// before delegating to the strategy. Symmetric with the scheduler
/// path's `PermitAcquiringRunner` wrapper — every local subprocess
/// action goes through the same throttle, regardless of whether
/// it landed via legacy `rule.build()` or scheduler dispatch.
pub struct RushActionManager {
    cache: Arc<ActionCache>,
    strategy: Arc<dyn ExecutionStrategy>,
    resource_manager: Arc<ResourceManager>,
    /// Content-addressed blob store for output rematerialization. When
    /// present, committed outputs are teed into it and a cache hit whose
    /// outputs were deleted restores them instead of re-executing.
    cas: Option<Arc<rush_cache::cas::CasStore>>,
}

/// Per-action control over cache lookup and publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionCachePolicy {
    /// Permit an existing result to skip execution.
    ///
    /// False is a must-execute contract for every reuse path, not only an
    /// instruction to bypass the ordinary action-cache lookup.
    pub lookup: bool,
    /// Publish a successful execution to the action cache and retain its
    /// output bytes in the configured CAS.
    pub store: bool,
    /// Wait for configured cache backends before reporting success.
    pub durable: bool,
}

impl ActionCachePolicy {
    /// Standard cacheable action behavior.
    pub const CACHEABLE: Self = Self {
        lookup: true,
        store: true,
        durable: false,
    };

    /// Force execution, then replace the reusable cache entry.
    pub const ALWAYS_RUN: Self = Self {
        lookup: false,
        store: true,
        durable: false,
    };

    /// Execute without reading or writing an action-cache entry.
    pub const UNCACHEABLE: Self = Self {
        lookup: false,
        store: false,
        durable: false,
    };

    /// Cacheable behavior with a durable publication boundary.
    pub const DURABLE_CACHEABLE: Self = Self {
        lookup: true,
        store: true,
        durable: true,
    };

    /// Force execution and durably replace the reusable entry.
    pub const DURABLE_ALWAYS_RUN: Self = Self {
        lookup: false,
        store: true,
        durable: true,
    };

    /// True when no cache, retained result, or action-specific shortcut may
    /// replace this invocation's execution.
    pub const fn must_execute(self) -> bool {
        !self.lookup
    }
}

impl RushActionManager {
    /// Separate local diagnostic path: no lookup, CAS, virtual restoration,
    /// thin-archive shortcut, output records or cache publication occurs here.
    pub(crate) async fn execute_diagnostic_owned(
        &self,
        mut action: Action,
        pending: Option<rush_rules::context::PendingCompileDeclaration>,
        declared_id: rush_action::ids::DeclaredActionId,
        target: rush_action::ids::TargetId,
        declared_inputs: Vec<rush_action::ids::ArtifactId>,
        declared_outputs: Vec<rush_action::ids::ArtifactId>,
        mut environment: Vec<(String, String)>,
        workspace: std::path::PathBuf,
        cancellation: tokio::sync::watch::Receiver<bool>,
        scope: Box<dyn Send>,
    ) -> Result<ActionResult> {
        anyhow::ensure!(
            cfg!(target_os = "linux"),
            "owned diagnostic execution requires Linux"
        );
        anyhow::ensure!(
            !*cancellation.borrow(),
            "diagnostic execution cancelled before preparation"
        );
        crate::generated_cc_sharding::prepare(&mut action)?;
        let transport = crate::compiler_discovery::prepare_diagnostic_transport(
            &mut action,
            pending.as_ref(),
            &environment,
            &workspace,
        )?
        .map(|(directory, pending)| (Arc::new(directory), pending));
        for (key, value) in &action.env {
            environment.retain(|(k, _)| k != key);
            environment.push((key.clone(), value.clone()));
        }
        let paths = Self::output_paths(&action);
        anyhow::ensure!(
            !paths.is_empty(),
            "diagnostic action has no declared outputs"
        );
        for input in &action.inputs {
            let input = match input {
                rush_action::Artifact::Source(path) => path,
                rush_action::Artifact::Derived { output_path, .. } => output_path,
            };
            anyhow::ensure!(std::fs::metadata(input).is_ok_and(|meta|meta.is_file()),
                "diagnostic execution requires a real regular input, without virtual/CAS restoration: {}",input.display());
        }
        // Output invalidation is not a virtual hit or certificate publication.
        rush_action::virtual_artifacts::unregister(&paths);
        for path in &paths {
            anyhow::ensure!(
                !action.inputs.iter().any(|input| match input {
                    rush_action::Artifact::Source(input) => input == path,
                    rush_action::Artifact::Derived { output_path, .. } => output_path == path,
                }),
                "diagnostic output aliases an input"
            );
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match std::fs::remove_file(path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        let request = rush_exec::local_action_runner::ExecutionRequest {
            declared_action_id: declared_id,
            mnemonic: action.mnemonic.clone(),
            resource_class: None,
            owner_target: target,
            command: rush_action::ids::MaterializedCommand {
                executable: action.command.executable.clone(),
                argv0: None,
                argv: action.command.args.clone(),
                env: environment,
                working_dir: Some(
                    action
                        .command
                        .working_dir
                        .clone()
                        .unwrap_or_else(|| workspace.clone()),
                ),
                env_policy: rush_action::ids::ActionEnvPolicy::ExplicitOnly,
            },
            declared_inputs,
            declared_outputs,
            timeout: Some(action.command.execution_info.timeout),
            log_command_index: None,
            remove_stale_archive_before_execute: false,
            terminate_process_group_on_drop: true,
            stdin_null: true,
            signal_state: None,
            file_creation_mask: None,
            credentials: None,
            isolation: None,
        };
        let permit = self
            .resource_manager
            .acquire(self.resource_request_for_action(&action))
            .await?;
        let (cleaned, cleanup) = tokio::sync::oneshot::channel();
        let scope: Box<dyn Send> = Box::new(DiagnosticCleanupScope {
            _owned: Box::new((
                scope,
                transport.as_ref().map(|(directory, _)| directory.clone()),
            )),
            cleaned: Some(cleaned),
        });
        let result = rush_exec::local_action_runner::DefaultLocalActionRunner::execute_owned(
            request,
            cancellation,
            scope,
        )
        .await;
        // This worker is not aborted by caller cancellation. If the runner
        // cannot confirm cleanup it retains scope, hence this permit and the
        // workspace lease also remain withheld instead of authorizing writers.
        let _ = cleanup.await;
        permit.release_async().await;
        let result = result?;
        if result.exit_code == 0 {
            for path in &paths {
                anyhow::ensure!(
                    std::fs::symlink_metadata(path).is_ok(),
                    "diagnostic action did not create output {}",
                    path.display()
                );
            }
            if let Some((_, pending)) = &transport {
                crate::compiler_discovery::read_diagnostic_depfile(pending)?;
            }
            #[cfg(test)]
            crate::compiler_discovery::record_completed_diagnostic_process(
                &workspace,
                &action.mnemonic,
                declared_id,
            );
        }
        Ok(ActionResult {
            action_id: action.id,
            exit_code: result.exit_code,
            stdout: result.stdout_tail,
            stderr: result.stderr_tail,
            outputs: action.outputs,
            output_records: Vec::new(),
            duration: result.duration,
            cached: false,
        })
    }

    /// Create a new RushActionManager.
    pub fn new(
        cache: Arc<ActionCache>,
        strategy: Arc<dyn ExecutionStrategy>,
        resource_manager: Arc<ResourceManager>,
    ) -> Self {
        // A disk-backed action cache shares its root with the CAS, resolved by
        // the one authority in rush_cache::roots. Memory-only caching leaves
        // the CAS unopened; a disk-open failure degrades to metadata-only
        // caching and never fails construction.
        let cas = if cache.uses_local_disk() {
            rush_cache::cas::CasStore::new(&rush_cache::roots::cas_store_root())
                .ok()
                .map(Arc::new)
        } else {
            None
        };
        Self {
            cache,
            strategy,
            resource_manager,
            cas,
        }
    }

    /// Construct with an explicitly selected CAS store.
    ///
    /// External replay tests and embedders use this to keep the serialized
    /// action cache and its blobs under one caller-owned root.
    pub fn new_with_cas(
        cache: Arc<ActionCache>,
        strategy: Arc<dyn ExecutionStrategy>,
        resource_manager: Arc<ResourceManager>,
        cas: Option<Arc<rush_cache::cas::CasStore>>,
    ) -> Self {
        Self {
            cache,
            strategy,
            resource_manager,
            cas,
        }
    }

    fn output_paths(action: &Action) -> Vec<std::path::PathBuf> {
        action
            .outputs
            .iter()
            .map(|out| match out {
                rush_action::Artifact::Source(p) => p.clone(),
                rush_action::Artifact::Derived { output_path, .. } => output_path.clone(),
            })
            .collect()
    }

    /// Fingerprint every declared output and retain its value in the action
    /// result. No Rush metadata is written beside user-visible outputs.
    fn commit_outputs(
        &self,
        action: &Action,
        retain_blobs: bool,
    ) -> Vec<rush_action::artifact_value::OutputRecord> {
        let sink: Option<&dyn rush_action::artifact_value::BlobSink> = if retain_blobs {
            self.cas.as_deref().map(|cas| cas as _)
        } else {
            None
        };
        let mut recorded = Vec::new();
        for p in Self::output_paths(action) {
            match rush_action::artifact_value::record_output(&p, sink) {
                Ok(record) => recorded.push(record),
                Err(_) => {}
            }
        }
        recorded
    }

    /// Rematerialize a cached action's outputs from the CAS. Any failure
    /// (missing blob, digest mismatch, unsupported shape) returns false
    /// and the caller re-executes — uncertainty is a miss, never a hit.
    fn restore_outputs_from_cas(
        &self,
        result: &rush_exec::ActionResult,
        before_output_mutation: Option<&BeforeOutputMutation>,
        invalidate_virtual_outputs: bool,
    ) -> Result<bool> {
        let Some(cas) = self.cas.as_deref() else {
            return Ok(false);
        };
        if result.output_records.is_empty() {
            return Ok(false);
        }
        if let Some(before) = before_output_mutation {
            before()?;
        }
        if invalidate_virtual_outputs {
            rush_action::virtual_artifacts::unregister(
                &result.output_records.iter().map(|record| record.path.clone()).collect::<Vec<_>>(),
            );
        }
        for record in &result.output_records {
            if crate::virtual_materializer::materialize_record(cas, record).is_err() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Phase 4 virtual-hit gate: a cache hit whose outputs are ALL absent may
    /// publish verified in-process values and keep the bytes CAS-backed.
    /// Requires (1) the feature flag, (2) a producer mnemonic whose
    /// consumers are audited (see `virtual_materializer`), (3) records
    /// aligned 1:1 with declared outputs, and (4) every referenced
    /// blob PRESENT in the CAS — a consumer must always be able to get
    /// the bytes back. Mixed presence falls through to the full
    /// restore, which overwrites from the CAS: an on-disk file written by
    /// another configuration must never survive as this configuration's
    /// value (config skew).
    fn try_virtual_hit(&self, action: &Action, result: &rush_exec::ActionResult) -> bool {
        if !rush_action::virtual_artifacts::enabled()
            || !crate::virtual_materializer::virtual_mnemonic(&action.mnemonic)
        {
            return false;
        }
        let paths = Self::output_paths(action);
        if paths.iter().any(|p| p.exists()) {
            return false;
        }
        if result.output_records.len() != paths.len()
            || !paths
                .iter()
                .zip(result.output_records.iter())
                .all(|(p, r)| p == &r.path)
        {
            return false;
        }
        let Some(cas) = self.cas.as_deref() else {
            return false;
        };
        if !crate::virtual_materializer::records_available(cas, &result.output_records) {
            return false;
        }
        rush_action::virtual_artifacts::register(&result.output_records);
        rush_action::virtual_artifacts::note_virtual_hit(
            result.output_records.len() as u64,
            result.output_records.iter().map(|r| r.size).sum(),
        );
        true
    }

    /// True iff every declared output's live filesystem value equals the
    /// fingerprint this cached result recorded at commit. This preserves
    /// config-skew protection without trusting adjacent metadata. A read
    /// failure or record/output mismatch is a conservative miss.
    fn outputs_match_recorded(action: &Action, result: &rush_exec::ActionResult) -> bool {
        let paths = Self::output_paths(action);
        if result.output_records.len() != paths.len() {
            return false;
        }
        paths.iter().zip(result.output_records.iter()).all(
            |(path, record)| {
                if path != &record.path {
                    return false;
                }
                rush_action::artifact_value::fingerprint_path(path)
                    .map(|actual| actual == record.artifact_fingerprint)
                    .unwrap_or(false)
            },
        )
    }

    /// Phase 5.C.4 commit 7 + 21: derive a [`ResourceRequest`]
    /// from an `Action`. Routes through
    /// [`rush_resource::ResourceClass::from_mnemonic`] (commit
    /// 21 seam) for classification + per-class default; argv
    /// heuristics layer on top. Mirrors the policy in
    /// `rush_exec::local_action_runner::resource_request_for_request`
    /// so legacy and scheduler paths produce equivalent
    /// requests for equivalent actions.
    fn resource_request_for_action(&self, action: &Action) -> ResourceRequest {
        let argv = &action.command.args;
        let class = rush_resource::ResourceClass::from_mnemonic(&action.mnemonic);
        let mut req = class.default_request();
        match class {
            rush_resource::ResourceClass::CppCompile
            | rush_resource::ResourceClass::KernelCompile => {
                let has_lto = argv.iter().any(|a| a == "-flto" || a.contains("thinlto"));
                let has_debug = argv.iter().any(|a| a == "-g");
                if has_lto {
                    req.memory_mb = req.memory_mb.max(4096);
                }
                if has_debug {
                    req.memory_mb += 512;
                }
                if let Some(cpu_slots) = crate::generated_cc_sharding::critical_compile_cpu_slots(
                    argv,
                    self.resource_manager.hardware_cpu_slots(),
                ) {
                    req.cpu_slots = cpu_slots;
                }
            }
            rush_resource::ResourceClass::Link | rush_resource::ResourceClass::KernelLink => {
                let obj_count = argv
                    .iter()
                    .filter(|a| a.ends_with(".o") || a.ends_with(".obj"))
                    .count();
                if obj_count > 0 {
                    let extra_chunks = obj_count / 400;
                    let extra_mb = extra_chunks.saturating_mul(2048);
                    req.memory_mb = (req.memory_mb + extra_mb).min(16 * 1024);
                }
                let has_lto = argv
                    .iter()
                    .any(|a| a == "-flto" || a.contains("thinlto") || a.contains("-Wl,--thinlto"));
                if has_lto {
                    req.memory_mb = req.memory_mb.max(8 * 1024);
                }
            }
            // CodegenProtoc / CodegenTblgen / Archive / Other:
            // class default suffices; future per-class tuning
            // attaches here.
            _ => {}
        }
        req
    }

    fn action_outputs_exist(action: &Action) -> bool {
        action.outputs.iter().all(|out| match out {
            rush_action::Artifact::Source(p) => p.exists(),
            rush_action::Artifact::Derived { output_path, .. } => output_path.exists(),
        })
    }

    /// Bind root-independent cached output values to this invocation's
    /// declared destinations. Canonical external keys can be shared across
    /// workspaces, while historical OutputRecords contain absolute paths.
    fn retarget_cached_outputs(action: &Action, result: &mut ActionResult) -> bool {
        let paths = Self::output_paths(action);
        if paths.len() != result.output_records.len() {
            return false;
        }
        for (record, path) in result.output_records.iter_mut().zip(paths) {
            record.path = path;
        }
        result.outputs = action.outputs.clone();
        true
    }
}

struct DiagnosticCleanupScope {
    _owned: Box<dyn Send>,
    cleaned: Option<tokio::sync::oneshot::Sender<()>>,
}
impl Drop for DiagnosticCleanupScope {
    fn drop(&mut self) { if let Some(cleaned) = self.cleaned.take() { let _ = cleaned.send(()); } }
}

/// #1 roadmap P1 minimality instrumentation: when RUSH_ACTION_TRACE=<file> is
/// set, append one line per action — "HIT <output>" (cache hit, skipped) or
/// "MISS <output>" (executed) — so the differential-correctness harness can
/// prove MINIMALITY (after editing one source, only the dependent actions
/// MISS; everything else HITs). Short single-line O_APPEND writes are atomic on
/// Linux, so concurrent wavefront actions interleave safely. Debug-only.
fn trace_action_outcome(action: &Action, outcome: &str) {
    let path = match std::env::var("RUSH_ACTION_TRACE") {
        Ok(p) if !p.is_empty() => p,
        _ => return,
    };
    let out = action
        .outputs
        .first()
        .map(|o| match o {
            rush_action::Artifact::Source(p) => p.display().to_string(),
            rush_action::Artifact::Derived { output_path, .. } => output_path.display().to_string(),
        })
        .unwrap_or_default();
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        // ONE write() call for the whole record. `writeln!` formats
        // argument by argument, issuing several small writes, and
        // concurrent wavefront actions then interleave mid-record —
        // observed as "HITHITHIT" and spliced paths in a whole-repo
        // trace, which silently corrupts the minimality accounting.
        // A single O_APPEND write of a short line is atomic on Linux.
        let line = format!("{} {}\n", outcome, out);
        let _ = f.write_all(line.as_bytes());
    }
}

impl RushActionManager {
    /// Execute an action through the normal resource, CAS, and output-commit
    /// path while honoring an explicit cache policy.
    pub async fn execute_with_cache_policy(
        &self,
        action: Action,
        policy: ActionCachePolicy,
    ) -> Result<ActionResult> {
        self.execute_impl(action, policy, true, None).await
    }

    /// Execute with a replay-owned input fence at actual mutation boundaries.
    /// The callback is scoped to this invocation, never installed globally on
    /// the manager shared by other replays. It must not hold a lock over await.
    /// Captured commands and their external action identities are exact: native
    /// compiler overrides and generated-source sharding must not rewrite them.
    /// Inputs and outputs are filesystem values, never native virtual-artifact
    /// placeholders. Producers or verified CAS restoration materialize outputs;
    /// this path does not implicitly JIT-materialize a different input value.
    pub(crate) async fn execute_canonical_external_action(
        &self,
        action: Action,
        policy: ActionCachePolicy,
        before_output_mutation: Arc<BeforeOutputMutation>,
    ) -> Result<ActionResult> {
        self.execute_impl(action, policy, false, Some(before_output_mutation.as_ref())).await
    }

    async fn execute_impl(
        &self,
        mut action: Action,
        policy: ActionCachePolicy,
        prepare_native_command: bool,
        before_output_mutation: Option<&BeforeOutputMutation>,
    ) -> Result<ActionResult> {
        let action_t0 = std::time::Instant::now();
        if prepare_native_command {
            crate::generated_cc_sharding::prepare(&mut action)?;
        }
        // Cache lookup. The cache stores ActionResults keyed by
        // action.id (blake3 of inputs+command+env). If we get a hit we
        // STILL need to confirm the declared output files actually
        // exist on disk — a previous build invocation may have cached
        // the result in memory and then the user (or another tooling
        // step) wiped rush-out/. Returning a stale "success" then has
        // downstream rules read non-existent .o / .a files and ENOENT
        // in obscure places. Re-execute when any declared output is
        // missing so the file state matches the cached success claim.
        let lookup_t0 = std::time::Instant::now();
        if let Some(mut result) = policy.lookup.then(|| self.cache.get(&action.id)).flatten() {
            if !Self::retarget_cached_outputs(&action, &mut result) {
                result.output_records.clear();
            }
            let mut outputs_present = Self::action_outputs_exist(&action);
            // Phase 4: under RUSH_VIRTUAL_ARTIFACTS=1, an all-absent hit
            // for an audited mnemonic registers verified records only — no
            // bytes touch disk unless a
            // consumer actually needs them (JIT / pre-build / top-level
            // materialization in virtual_materializer).
            if prepare_native_command && !outputs_present && self.try_virtual_hit(&action, &result) {
                result.cached = true;
                trace_action_outcome(&action, "HIT-VIRTUAL");
                if crate::action_timing::enabled() {
                    crate::action_timing::record(
                        &action,
                        None,
                        action_t0.elapsed(),
                        Some(0),
                        true,
                    );
                }
                return Ok(result);
            }
            // Outputs deleted (or never present on this machine): a real
            // CAS can rebuild them from cached bytes instead of paying the
            // action again. Restoration verifies every blob digest; any
            // doubt falls through to re-execution.
            if !outputs_present && self.restore_outputs_from_cas(&result, before_output_mutation, !prepare_native_command)? {
                outputs_present = Self::action_outputs_exist(&action);
                if outputs_present {
                    trace_action_outcome(&action, "HIT-CAS-RESTORE");
                }
            }
            // The output FILE existing is necessary but NOT sufficient: a
            // different action (e.g. the same source under a different
            // `--define`, with a different `-D` copt and thus a different
            // action.id) may have last written this same output path. Require
            // the live value to match the cached output record so we never
            // serve another configuration's bytes as a hit.
            if outputs_present && Self::outputs_match_recorded(&action, &result) {
                result.cached = true;
                trace_action_outcome(&action, "HIT");
                if crate::action_timing::enabled() {
                    crate::action_timing::record(
                        &action,
                        None,
                        action_t0.elapsed(),
                        Some(0),
                        true,
                    );
                }
                return Ok(result);
            }
            // Fall through to re-execute; don't return the stale entry.
        }

        let lookup_elapsed = lookup_t0.elapsed();

        if prepare_native_command
            && !policy.must_execute()
            && matches!(action.mnemonic.as_str(), "CppArchive" | "CcArchive")
            && Self::action_outputs_exist(&action)
            && crate::thin_archive::can_reuse_cc_archive_command(
                &action.command.executable,
                &action.command.args,
            )
        {
            let duration = action_t0.elapsed();
            let output_records = self.commit_outputs(&action, policy.store);
            let result = ActionResult {
                action_id: action.id.clone(),
                exit_code: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
                outputs: action.outputs.clone(),
                output_records,
                duration,
                cached: true,
            };
            trace_action_outcome(&action, "HIT");
            if crate::action_timing::enabled() {
                crate::action_timing::record(&action, None, duration, Some(0), true);
            }
            if policy.store {
                if policy.durable {
                    self.cache
                        .put_durable(action.id.clone(), result.clone())
                        .await?;
                } else {
                    self.cache.put(action.id.clone(), result.clone());
                }
            }
            return Ok(result);
        }

        // Phase 4: this action is about to EXECUTE, so its inputs must
        // be real files. Any input that is a registered virtual
        // artifact (or a symlink chain ending in one) is materialized
        // from the CAS now — the only moment the bytes are truly
        // needed. Failure is a hard error: the producer already
        // virtual-hit, so re-producing it is not schedulable this
        // build (the CAS quarantines corrupt blobs, so the NEXT build
        // misses and re-executes).
        if prepare_native_command && rush_action::virtual_artifacts::enabled() {
            for input in &action.inputs {
                let path = match input {
                    rush_action::Artifact::Source(p) => p,
                    rush_action::Artifact::Derived { output_path, .. } => output_path,
                };
                crate::virtual_materializer::ensure_path_materialized(
                    path,
                    rush_action::virtual_artifacts::MaterializePurpose::Jit,
                )?;
            }
            // This action is about to WRITE its outputs, so any virtual
            // record for them (from an earlier build in a resident
            // daemon) is now stale. Dropping it keeps the rules'
            // "reported success but produced nothing" guards honest and
            // stops a later sweep resurrecting the previous bytes.
            rush_action::virtual_artifacts::unregister(&Self::output_paths(&action));
        }

        // Phase 5.C.4 commit 7: acquire ResourceManager permit
        // before spawning. Legacy `rule.build()` paths reach this
        // method via `context.execute_action(action)` — without
        // this acquisition they'd bypass the throttle that
        // protects against TF-scale child-compiler memory
        // pressure. Symmetric with PermitAcquiringRunner around
        // the scheduler path.
        let resource_request = self.resource_request_for_action(&action);
        let permit_t0 = std::time::Instant::now();
        let resource_handle = self.resource_manager.acquire(resource_request).await?;
        let resource_wait = permit_t0.elapsed();

        // Fence after resource admission, immediately before execution. Keeping
        // the fence active while queued detects earlier foreign input writes.
        let execution_timer = crate::action_timing::ExecutionTimer::start_if_enabled();
        let result = match before_output_mutation.map(|before| before()).transpose() {
            Ok(_) => {
                // Invalidate only output metadata: a later native sweep must
                // not resurrect a previous virtual value after this writer.
                // Canonical inputs remain exact filesystem values.
                if !prepare_native_command {
                    rush_action::virtual_artifacts::unregister(&Self::output_paths(&action));
                }
                self.strategy.execute(&action).await
            }
            Err(error) => Err(error),
        };
        let execution = execution_timer.map(|timer| timer.finish());

        // Release permit before returning, regardless of outcome.
        resource_handle.release_async().await;
        // The record is emitted AFTER the commit below, so the
        // post-execution hashing/CAS/stamping cost is inside the
        // measurement instead of after it. A failure path still reports
        // here, because there is no commit to wait for.
        if result.is_err() && crate::action_timing::enabled() {
            crate::action_timing::record_full(
                &action,
                None,
                action_t0.elapsed(),
                execution.clone(),
                crate::action_timing::PhaseBreakdown {
                    resource_wait: Some(resource_wait),
                    commit: None,
                    cache_lookup: Some(lookup_elapsed),
                },
                None,
                false,
            );
        }
        let result = result?;
        trace_action_outcome(&action, "MISS");

        // Cache result (if successful). For successful actions we don't
        // need stdout/stderr again — they were either silent (g++ etc.)
        // or got logged at execution time. Keeping them around just
        // bloats the in-memory ActionCache and explodes peak RSS at
        // TF-rung-11 scale (observed ~45 GB orchestrator RSS where
        // retained Vec<u8> output buffers are a top suspect, per the
        // 2026-04-26 ChatGPT Pro analysis in
        // docs/memory-pressure-analysis.md). Strip stdout/stderr before
        // storing; failure paths still return the full result to the
        // caller, who logs/displays it directly.
        let mut result = result;
        let mut commit_time = std::time::Duration::ZERO;
        if result.exit_code == 0 {
            // Fingerprint + stamp the outputs FIRST, then cache an entry
            // that records those fingerprints: the hit gate compares
            // stamps against the recorded values so a lookup can never
            // serve another configuration's bytes, while byte-identical
            // rebuilds keep downstream keys unchanged (early cutoff).
            let commit_t0 = std::time::Instant::now();
            result.output_records = self.commit_outputs(&action, policy.store);
            commit_time = commit_t0.elapsed();
            let mut cached_entry = result.clone();
            cached_entry.stdout = Vec::new();
            cached_entry.stderr = Vec::new();
            if policy.store {
                if policy.durable {
                    self.cache
                        .put_durable(action.id.clone(), cached_entry)
                        .await?;
                } else {
                    self.cache.put(action.id.clone(), cached_entry);
                }
            }
        }

        if crate::action_timing::enabled() {
            crate::action_timing::record_full(
                &action,
                None,
                action_t0.elapsed(),
                execution,
                crate::action_timing::PhaseBreakdown {
                    resource_wait: Some(resource_wait),
                    commit: Some(commit_time),
                    cache_lookup: Some(lookup_elapsed),
                },
                Some(result.exit_code),
                result.cached,
            );
        }

        Ok(result)
    }
}

#[async_trait]
impl ActionManager for RushActionManager {
    async fn execute(&self, action: Action) -> Result<ActionResult> {
        self.execute_impl(action, ActionCachePolicy::CACHEABLE, true, None)
            .await
    }
}

#[cfg(test)]
mod output_value_tests {
    use super::*;
    use rush_action::{Artifact, Command, ExecutionInfo};
    use rush_exec::ExecutionStrategy;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::process::Command as ProcessCommand;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingStrategy {
        calls: AtomicUsize,
    }

    struct PreservingCountingStrategy {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ExecutionStrategy for CountingStrategy {
        async fn execute(&self, action: &Action) -> Result<ActionResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            for output in RushActionManager::output_paths(action) {
                std::fs::write(output, b"result")?;
            }
            Ok(ActionResult {
                action_id: action.id.clone(),
                exit_code: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
                outputs: action.outputs.clone(),
                output_records: Vec::new(),
                duration: std::time::Duration::ZERO,
                cached: false,
            })
        }

        fn can_execute(&self, _action: &Action) -> bool {
            true
        }

        fn name(&self) -> &str {
            "counting"
        }
    }

    #[async_trait]
    impl ExecutionStrategy for PreservingCountingStrategy {
        async fn execute(&self, action: &Action) -> Result<ActionResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ActionResult {
                action_id: action.id.clone(),
                exit_code: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
                outputs: action.outputs.clone(),
                output_records: Vec::new(),
                duration: std::time::Duration::ZERO,
                cached: false,
            })
        }

        fn can_execute(&self, _action: &Action) -> bool {
            true
        }

        fn name(&self) -> &str {
            "preserving-counting"
        }
    }

    fn have_tool(name: &str) -> bool {
        ProcessCommand::new("sh")
            .arg("-c")
            .arg(format!("command -v {name} >/dev/null 2>&1"))
            .status()
            .is_ok_and(|status| status.success())
    }

    fn reusable_archive_action(dir: &std::path::Path) -> Result<Action> {
        let source = dir.join("member.cc");
        let object = dir.join("member.o");
        let archive = dir.join("libmember.a");
        std::fs::write(&source, b"int member() { return 1; }\n")?;
        anyhow::ensure!(
            ProcessCommand::new("g++")
                .args(["-c", source.to_str().unwrap(), "-o"])
                .arg(&object)
                .status()?
                .success(),
            "g++ failed"
        );
        anyhow::ensure!(
            ProcessCommand::new("ar")
                .args(["rcs", archive.to_str().unwrap(), "--thin"])
                .arg(&object)
                .status()?
                .success(),
            "ar failed"
        );
        let command = Command {
            executable: PathBuf::from("ar"),
            args: vec![
                "rcs".to_string(),
                archive.display().to_string(),
                "--thin".to_string(),
                object.display().to_string(),
            ],
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };
        let inputs = vec![Artifact::Source(object)];
        let id = Action::compute_id(&inputs, &command, &HashMap::new())?;
        Ok(Action {
            id,
            inputs,
            outputs: vec![Artifact::Source(archive)],
            command,
            env: HashMap::new(),
            mnemonic: "CcArchive".to_string(),
        })
    }

    fn compile_action(
        out: &std::path::Path,
        src: &std::path::Path,
        define: Option<&str>,
    ) -> Action {
        // A minimal CppCompile-shaped action: the same source + output, differing
        // ONLY by a `-D` copt — exactly the config-toggle case from the bug. The
        // define flows into the command args, so compute_id yields distinct ids.
        let mut args = vec!["-c".to_string(), "-fPIC".to_string()];
        if let Some(d) = define {
            args.push(format!("-D{}", d));
        }
        args.push("-o".to_string());
        args.push(out.to_string_lossy().to_string());
        args.push(src.to_string_lossy().to_string());
        let command = Command {
            executable: PathBuf::from("g++"),
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };
        let inputs = vec![Artifact::Source(src.to_path_buf())];
        let id = Action::compute_id(&inputs, &command, &HashMap::new()).unwrap();
        Action {
            id,
            inputs,
            outputs: vec![Artifact::Source(out.to_path_buf())],
            command,
            env: HashMap::new(),
            mnemonic: "CppCompile".to_string(),
        }
    }

    struct ExactCommandStrategy {
        expected: Action,
        calls: AtomicUsize,
        expect_missing_input: bool,
    }

    #[async_trait]
    impl ExecutionStrategy for ExactCommandStrategy {
        async fn execute(&self, action: &Action) -> Result<ActionResult> {
            assert_eq!(action.id, self.expected.id);
            assert_eq!(action.command.executable, self.expected.command.executable);
            assert_eq!(action.command.args, self.expected.command.args);
            assert_eq!(action.env, self.expected.env);
            if self.expect_missing_input {
                let input = match &action.inputs[0] {
                    Artifact::Source(path) => path,
                    Artifact::Derived { output_path, .. } => output_path,
                };
                assert!(!input.exists(), "canonical execution must not JIT-materialize an input");
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            CountingStrategy { calls: AtomicUsize::new(0) }.execute(action).await
        }
        fn can_execute(&self, _: &Action) -> bool { true }
        fn name(&self) -> &str { "exact-external-command" }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn canonical_external_commands_skip_native_sharding_and_fence_actual_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("rush-out/genfiles/tensorflow/cc/ops/math_ops.cc");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, r#"#include "ops.h"
namespace tensorflow {
namespace ops {
Foo::Foo() {}
Bar::Bar() {}
Baz::Baz() {}
Qux::Qux() {}
A::A() {}
B::B() {}
C::C() {}
D::D() {}
}  // namespace ops
}  // namespace tensorflow
"#).unwrap();
        let mut native = compile_action(&dir.path().join("native.o"), &source, None);
        let native_id = native.id.clone();
        assert!(crate::generated_cc_sharding::prepare(&mut native).unwrap());
        assert_ne!(native.id, native_id, "the fixture must trigger native preparation");

        let output = dir.path().join("exact.o");
        let action = compile_action(&output, &source, None);
        let strategy = Arc::new(ExactCommandStrategy { expected: action.clone(), calls: AtomicUsize::new(0), expect_missing_input: false });
        let cache = Arc::new(ActionCache::new());
        let cas = Arc::new(rush_cache::cas::CasStore::new(dir.path()).unwrap());
        let stale_output_record = if rush_action::virtual_artifacts::enabled() {
            let stale_path = dir.path().join("stale-native-output");
            std::fs::write(&stale_path, b"obsolete native bytes\n").unwrap();
            let mut record = cas.record_output_durable(&stale_path).unwrap();
            record.path = output.clone();
            rush_action::virtual_artifacts::register(std::slice::from_ref(&record));
            Some(record)
        } else {
            None
        };
        let manager = RushActionManager::new_with_cas(cache, strategy.clone(), ResourceManager::new().unwrap(), Some(cas.clone()));
        let mutations = Arc::new(AtomicUsize::new(0));
        let before: Arc<BeforeOutputMutation> = {
            let mutations = mutations.clone();
            Arc::new(move || { mutations.fetch_add(1, Ordering::SeqCst); Ok(()) })
        };
        let first = manager.execute_canonical_external_action(action.clone(), ActionCachePolicy::CACHEABLE, before.clone()).await.unwrap();
        assert!(!first.cached);
        assert_eq!(first.action_id, action.id);
        assert_eq!(mutations.load(Ordering::SeqCst), 1);
        assert!(!dir.path().join("exact.o.rush-shard0.cc").exists());
        assert!(!rush_action::virtual_artifacts::is_registered(&output), "execution must invalidate old output metadata");
        let retained = manager.execute_canonical_external_action(action.clone(), ActionCachePolicy::CACHEABLE, before.clone()).await.unwrap();
        assert!(retained.cached);
        assert_eq!(mutations.load(Ordering::SeqCst), 1, "pure retained hits must not disable generated memoization");
        std::fs::remove_file(&output).unwrap();
        if let Some(record) = &stale_output_record {
            rush_action::virtual_artifacts::register(std::slice::from_ref(record));
        }
        let restored = manager.execute_canonical_external_action(action.clone(), ActionCachePolicy::CACHEABLE, before.clone()).await.unwrap();
        assert!(restored.cached);
        assert_eq!(mutations.load(Ordering::SeqCst), 2);
        assert_eq!(strategy.calls.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&output).unwrap(), b"result");
        assert!(!rush_action::virtual_artifacts::is_registered(&output));

        // Corrupted CAS bytes cannot turn a missing output into a cache hit.
        let digest = first.output_records[0].content_digest.unwrap().to_hex();
        std::fs::write(dir.path().join("cas").join(&digest[..2]).join(&digest), b"corrupted").unwrap();
        std::fs::remove_file(&output).unwrap();
        let recovered = manager.execute_canonical_external_action(action.clone(), ActionCachePolicy::CACHEABLE, before).await.unwrap();
        assert!(!recovered.cached);
        assert_eq!(std::fs::read(&output).unwrap(), b"result");
        assert_eq!(strategy.calls.load(Ordering::SeqCst), 2);

        // A failed fence must prevent both CAS output restoration and execution.
        std::fs::remove_file(&output).unwrap();
        if let Some(record) = &stale_output_record {
            rush_action::virtual_artifacts::register(std::slice::from_ref(record));
        }
        let refused: Arc<BeforeOutputMutation> = Arc::new(|| anyhow::bail!("foreign input mutation"));
        assert!(manager.execute_canonical_external_action(action.clone(), ActionCachePolicy::CACHEABLE, refused.clone()).await.is_err());
        assert!(manager.execute_canonical_external_action(action.clone(), ActionCachePolicy::UNCACHEABLE, refused).await.is_err());
        assert!(!output.exists());
        assert_eq!(strategy.calls.load(Ordering::SeqCst), 2);
        if stale_output_record.is_some() {
            assert!(rush_action::virtual_artifacts::is_registered(&output), "a rejected fence must leave output metadata untouched");
        }

        if rush_action::virtual_artifacts::enabled() {
            // The subprocess below enables native virtual mode. Even then,
            // canonical external execution must preserve this missing path.
            let record = cas.record_output_durable(&source).unwrap();
            rush_action::virtual_artifacts::register(&[record]);
            std::fs::remove_file(&source).unwrap();
            let strategy = Arc::new(ExactCommandStrategy {
                expected: action.clone(), calls: AtomicUsize::new(0), expect_missing_input: true,
            });
            let manager = RushActionManager::new_with_cas(
                Arc::new(ActionCache::new()), strategy.clone(), ResourceManager::new().unwrap(), Some(cas),
            );
            manager.execute_canonical_external_action(action, ActionCachePolicy::UNCACHEABLE, Arc::new(|| Ok(()))).await.unwrap();
            assert_eq!(strategy.calls.load(Ordering::SeqCst), 1);
            assert!(!source.exists());
            assert!(rush_action::virtual_artifacts::is_registered(&source), "canonical execution must preserve input registrations");
            assert!(!rush_action::virtual_artifacts::is_registered(&output));
            rush_action::virtual_artifacts::unregister(&[source]);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn canonical_external_virtual_mode_does_not_virtualize_or_jit() {
        // Virtual mode and its fallback environment are process-global. Use an
        // isolated invocation of the exact regression rather than racing the
        // rest of the unit-test binary's native mode/cache settings.
        let temp = tempfile::tempdir().unwrap();
        let status = ProcessCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", "action_manager::output_value_tests::canonical_external_commands_skip_native_sharding_and_fence_actual_mutations", "--nocapture", "--test-threads=1"])
            .env("RUSH_VIRTUAL_ARTIFACTS", "1")
            .env("RUSH_USE_SCHEDULER", "0")
            .env(rush_cache::roots::CACHE_ROOT_ENV, temp.path())
            .status().unwrap();
        assert!(status.success());
    }

    #[test]
    fn live_values_preserve_config_skew_protection() {
        let dir = std::env::temp_dir().join(format!("rush-stamp-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("foo.cc");
        std::fs::write(&src, b"int foo(){return 0;}").unwrap();
        let obj = dir.join("foo.o");

        let fast = compile_action(&obj, &src, Some("USE_FAST"));
        let slow = compile_action(&obj, &src, None);
        assert_ne!(fast.id, slow.id, "define change must change the action id");

        fn test_commit(
            a: &Action,
        ) -> Vec<rush_action::artifact_value::OutputRecord> {
            let mut recorded = Vec::new();
            for out in &a.outputs {
                let p = match out {
                    rush_action::Artifact::Source(p) => p.clone(),
                    rush_action::Artifact::Derived { output_path, .. } => {
                        output_path.clone()
                    }
                };
                let record =
                    rush_action::artifact_value::record_output(&p, None).unwrap();
                recorded.push(record);
            }
            recorded
        }

        let base_result = |a: &Action, records| rush_exec::ActionResult {
            action_id: a.id.clone(),
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            outputs: a.outputs.clone(),
            output_records: records,
            duration: std::time::Duration::from_secs(0),
            cached: false,
        };

        // Simulate the SLOW config producing its bytes and committing.
        std::fs::write(&obj, b"OBJ-SLOW").unwrap();
        let slow_result =
            base_result(&slow, test_commit(&slow));

        // Then the FAST config overwrites the shared output path.
        std::fs::write(&obj, b"OBJ-FAST").unwrap();
        let fast_result =
            base_result(&fast, test_commit(&fast));

        // On-disk value now belongs to fast: fast HITs, slow must MISS —
        // the config-skew protection, now expressed through values.
        assert!(
            RushActionManager::outputs_match_recorded(&fast, &fast_result),
            "producing config must keep its HIT"
        );
        assert!(
            !RushActionManager::outputs_match_recorded(&slow, &slow_result),
            "the other config must not serve foreign bytes"
        );

        // Early cutoff corollary: if both configs produce IDENTICAL bytes,
        // both match — serving either is correct by value.
        std::fs::write(&obj, b"OBJ-SAME").unwrap();
        let fps_same = test_commit(&fast);
        let fast_same = base_result(&fast, fps_same.clone());
        let slow_same = base_result(&slow, fps_same);
        assert!(RushActionManager::outputs_match_recorded(&fast, &fast_same));
        assert!(RushActionManager::outputs_match_recorded(&slow, &slow_same));

        // A record/outputs length mismatch (legacy or executor-default
        // entry) is a conservative MISS.
        let empty = base_result(&fast, Vec::new());
        assert!(!RushActionManager::outputs_match_recorded(&fast, &empty));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn explicit_cache_policy_controls_lookup_and_publication() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        let action = Action {
            id: rush_action::ActionId::new(blake3::hash(b"cache-policy")),
            inputs: Vec::new(),
            outputs: vec![Artifact::Source(output)],
            command: Command {
                executable: PathBuf::from("unused"),
                args: Vec::new(),
                working_dir: None,
                execution_info: ExecutionInfo::default(),
            },
            env: HashMap::new(),
            mnemonic: "Test".to_string(),
        };
        let strategy = Arc::new(CountingStrategy {
            calls: AtomicUsize::new(0),
        });
        let manager = RushActionManager::new(
            Arc::new(ActionCache::new()),
            strategy.clone(),
            ResourceManager::new().unwrap(),
        );

        manager
            .execute_with_cache_policy(action.clone(), ActionCachePolicy::CACHEABLE)
            .await
            .unwrap();
        manager
            .execute_with_cache_policy(action.clone(), ActionCachePolicy::CACHEABLE)
            .await
            .unwrap();
        assert_eq!(strategy.calls.load(Ordering::SeqCst), 1);

        manager
            .execute_with_cache_policy(action.clone(), ActionCachePolicy::ALWAYS_RUN)
            .await
            .unwrap();
        manager
            .execute_with_cache_policy(action.clone(), ActionCachePolicy::ALWAYS_RUN)
            .await
            .unwrap();
        assert_eq!(strategy.calls.load(Ordering::SeqCst), 3);

        manager
            .execute_with_cache_policy(action.clone(), ActionCachePolicy::UNCACHEABLE)
            .await
            .unwrap();
        manager
            .execute_with_cache_policy(action, ActionCachePolicy::UNCACHEABLE)
            .await
            .unwrap();
        assert_eq!(strategy.calls.load(Ordering::SeqCst), 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mandatory_policies_execute_despite_reusable_archive_after_restart() {
        if !have_tool("g++") || !have_tool("ar") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let action = reusable_archive_action(dir.path()).unwrap();
        assert!(!crate::thin_archive::can_reuse_cc_archive_command(
            &action.command.executable,
            &action.command.args,
        ));
        let config = rush_cache::CacheConfig {
            cache_root: dir.path().join("cache"),
            max_size: 0,
            ttl_seconds: 0,
        };

        let seed_strategy = Arc::new(PreservingCountingStrategy {
            calls: AtomicUsize::new(0),
        });
        let seed = RushActionManager::new_with_cas(
            Arc::new(ActionCache::with_disk(config.clone()).unwrap()),
            seed_strategy.clone(),
            ResourceManager::new().unwrap(),
            None,
        );
        let reused = seed
            .execute_with_cache_policy(action.clone(), ActionCachePolicy::CACHEABLE)
            .await
            .unwrap();
        assert!(!reused.cached);
        assert_eq!(seed_strategy.calls.load(Ordering::SeqCst), 1);
        drop(seed);

        for policy in [
            ActionCachePolicy::ALWAYS_RUN,
            ActionCachePolicy::UNCACHEABLE,
            ActionCachePolicy::DURABLE_ALWAYS_RUN,
        ] {
            let strategy = Arc::new(PreservingCountingStrategy {
                calls: AtomicUsize::new(0),
            });
            let restarted = RushActionManager::new_with_cas(
                Arc::new(ActionCache::with_disk(config.clone()).unwrap()),
                strategy.clone(),
                ResourceManager::new().unwrap(),
                None,
            );
            let result = restarted
                .execute_with_cache_policy(action.clone(), policy)
                .await
                .unwrap();
            assert!(!result.cached, "{policy:?} must report execution");
            assert_eq!(
                strategy.calls.load(Ordering::SeqCst),
                1,
                "{policy:?} must invoke the execution strategy"
            );
        }
    }

    #[tokio::test]
    async fn uncacheable_execution_retains_neither_action_record_nor_cas_blob() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("private-output");
        let action = Action {
            id: rush_action::ActionId::new(blake3::hash(b"private-output")),
            inputs: Vec::new(),
            outputs: vec![Artifact::Source(output)],
            command: Command {
                executable: PathBuf::from("unused"),
                args: Vec::new(),
                working_dir: None,
                execution_info: ExecutionInfo::default(),
            },
            env: HashMap::new(),
            mnemonic: "Test".to_string(),
        };
        let cache = Arc::new(ActionCache::new());
        let cas = Arc::new(rush_cache::cas::CasStore::new(dir.path()).unwrap());
        let strategy = Arc::new(CountingStrategy {
            calls: AtomicUsize::new(0),
        });
        let manager = RushActionManager::new_with_cas(
            cache.clone(),
            strategy.clone(),
            ResourceManager::new().unwrap(),
            Some(cas.clone()),
        );

        let result = manager
            .execute_with_cache_policy(action.clone(), ActionCachePolicy::UNCACHEABLE)
            .await
            .unwrap();
        assert!(!result.cached);
        assert_eq!(strategy.calls.load(Ordering::SeqCst), 1);
        assert!(!cache.contains(&action.id));
        let digest = result.output_records[0].content_digest.unwrap();
        assert!(!cas.has(&digest));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn durable_policy_survives_an_immediate_manager_restart() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("input");
        let output = dir.path().join("output");
        std::fs::write(&src, b"input").unwrap();
        let action = compile_action(&output, &src, None);
        let config = rush_cache::CacheConfig {
            cache_root: dir.path().join("cache"),
            max_size: 0,
            ttl_seconds: 0,
        };

        let first_strategy = Arc::new(CountingStrategy {
            calls: AtomicUsize::new(0),
        });
        let first = RushActionManager::new_with_cas(
            Arc::new(ActionCache::with_disk(config.clone()).unwrap()),
            first_strategy.clone(),
            ResourceManager::new().unwrap(),
            None,
        );
        first
            .execute_with_cache_policy(action.clone(), ActionCachePolicy::DURABLE_CACHEABLE)
            .await
            .unwrap();
        assert_eq!(first_strategy.calls.load(Ordering::SeqCst), 1);
        drop(first);

        let second_strategy = Arc::new(CountingStrategy {
            calls: AtomicUsize::new(0),
        });
        let second = RushActionManager::new_with_cas(
            Arc::new(ActionCache::with_disk(config).unwrap()),
            second_strategy.clone(),
            ResourceManager::new().unwrap(),
            None,
        );
        let result = second
            .execute_with_cache_policy(action, ActionCachePolicy::DURABLE_CACHEABLE)
            .await
            .unwrap();
        assert!(result.cached);
        assert_eq!(second_strategy.calls.load(Ordering::SeqCst), 0);
    }
}
