//! Production admission and capture orchestration for external build graphs.
//!
//! This controller is deliberately analyzer-neutral. It selects one managed
//! snapshot generation, replays an admitted graph without invoking its
//! analyzer, or opens one exact-path clean capture transaction. A successful
//! analyzer run is adopted and published directly; it is never replayed a
//! second time in the same invocation.

use crate::external_action_identity::ExternalCaptureWitnessReceipt;
use crate::external_analysis_snapshot::{
    ExternalAnalysisSnapshot, ExternalSnapshotAdmission, ExternalSnapshotDecision,
};
use crate::external_capture_adoption::stage_external_capture_results;
use crate::external_generation::{
    ExternalCaptureTransactionIdentity, ExternalGenerationStore, ManagedCaptureRecovery,
};
use crate::external_replay::{ExternalReplayEngine, ExternalReplayStatus};
use anyhow::{Context, Result};
use rush_cache::cas::CasStore;
use rush_cache::ActionCache;
use rush_command_core::CommandSemantics;
use rush_observe::ProcessCaptureStreamIdentity;
use rush_resource::ResourceManager;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

/// Controller-owned inputs for one exact-path analyzer capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCaptureRequest {
    /// Fresh directory at the final logical output path.
    pub output_root: PathBuf,
    /// Identity that the observer must bind and return on certification.
    pub transaction_identity: ExternalCaptureTransactionIdentity,
}

impl ExternalCaptureRequest {
    /// Identity that the native process observer must seal into its stream.
    pub fn process_capture_identity(&self) -> ProcessCaptureStreamIdentity {
        ProcessCaptureStreamIdentity::new(
            self.transaction_identity.capture_id(),
            self.transaction_identity.context_digest(),
        )
    }
}

/// Analyzer completion returned to the generic external-build controller.
#[derive(Debug)]
pub enum ExternalCaptureCompletion<T> {
    /// The analyzer failed, so its fresh output tree must be discarded.
    Failed(T),
    /// The analyzer succeeded, but its observation cannot be reused safely.
    Uncertified {
        /// Analyzer-specific successful result returned to the caller.
        result: T,
        /// Stable diagnostic explaining why no generation may be published.
        reason: String,
    },
    /// The analyzer and observer produced a complete reusable generation.
    Certified {
        /// Analyzer-specific successful result returned to the caller.
        result: T,
        /// Fully validated captured graph and analysis dependencies.
        snapshot: Box<ExternalAnalysisSnapshot>,
        /// Versioned read-time values bound to the exact snapshot.
        witness_receipt: ExternalCaptureWitnessReceipt,
        /// Controller transaction bound into the analyzer observation.
        transaction_identity: ExternalCaptureTransactionIdentity,
    },
}

/// One external-build invocation outcome with explicit admission telemetry.
#[derive(Debug)]
pub enum ExternalBuildOutcome<T> {
    /// A persisted generation was admitted and routed through normal replay.
    Replayed {
        /// Snapshot admission decision made before replay.
        decision: ExternalSnapshotDecision,
        /// Restart repair completed before admission.
        recovery: ManagedCaptureRecovery,
        /// Action/cache work performed by the admitted graph.
        replay: ExternalReplayStatus,
    },
    /// The analyzer failed and the prior managed tree was restored.
    CaptureFailed {
        /// Miss decision that required the capture.
        decision: ExternalSnapshotDecision,
        /// Admission diagnostic that caused recapture.
        admission_reason: String,
        /// Restart repair completed before admission.
        recovery: ManagedCaptureRecovery,
        /// Analyzer-specific failure result.
        result: T,
    },
    /// The analyzer succeeded, but replay remains durably invalidated.
    CaptureUncertified {
        /// Miss decision that required the capture.
        decision: ExternalSnapshotDecision,
        /// Admission diagnostic that caused recapture.
        admission_reason: String,
        /// Restart repair completed before admission.
        recovery: ManagedCaptureRecovery,
        /// Analyzer-specific successful result.
        result: T,
        /// Stable reason no reusable generation was published.
        reason: String,
    },
    /// The successful analyzer output was adopted without a second build.
    Captured {
        /// Miss decision that required the capture.
        decision: ExternalSnapshotDecision,
        /// Admission diagnostic that caused recapture.
        admission_reason: String,
        /// Restart repair completed before admission.
        recovery: ManagedCaptureRecovery,
        /// Analyzer-specific successful result.
        result: T,
        /// Reusable captured actions published to the action cache.
        adopted_actions: usize,
        /// Deliberately uncacheable or always-run captured actions.
        skipped_actions: usize,
    },
    /// Policy forbids replay and reusable capture for this build mode.
    Unsupported {
        /// Restart repair completed before admission.
        recovery: ManagedCaptureRecovery,
        /// Stable policy diagnostic.
        reason: String,
    },
}

/// Thin controller over generic generation, adoption, cache, and replay APIs.
pub struct ExternalBuildController {
    store: ExternalGenerationStore,
    cache: Arc<ActionCache>,
    cas: Arc<CasStore>,
    semantics: CommandSemantics,
}

impl ExternalBuildController {
    /// Create a controller for one exact analysis key and managed output path.
    pub fn new(
        store: ExternalGenerationStore,
        cache: Arc<ActionCache>,
        cas: Arc<CasStore>,
        semantics: CommandSemantics,
    ) -> Self {
        Self {
            store,
            cache,
            cas,
            semantics,
        }
    }

    /// Admit replay or run exactly one clean capture at the managed path.
    ///
    /// `capture` is called only for a missing, stale, invalid, or mismatched
    /// continuation generation.
    /// Its request names the final logical output path after the old tree has
    /// been moved aside and the identity that observation must bind. Returning
    /// an error rolls the tree back before propagating the error. Process death
    /// instead leaves the durable journal for the next invocation's recovery
    /// pass.
    pub async fn build<T, F, Fut>(&self, capture: F) -> Result<ExternalBuildOutcome<T>>
    where
        F: FnOnce(ExternalCaptureRequest) -> Fut,
        Fut: Future<Output = Result<ExternalCaptureCompletion<T>>>,
    {
        let recovery = self
            .store
            .recover_managed_capture_with_cache(&self.cache, &self.cas)
            .await
            .context("recover managed external capture")?;
        let admission = self.store.load_active_if_fresh(&self.semantics);
        let decision = admission.decision();
        let (decision, admission_reason) = match admission {
            ExternalSnapshotAdmission::Hit(snapshot) => {
                let replay = ExternalReplayEngine::with_cas(
                    snapshot,
                    self.cache.clone(),
                    ResourceManager::new()?,
                    self.cas.clone(),
                )?
                .replay()
                .await?;
                match replay {
                    ExternalReplayStatus::Complete { .. } => {
                        return Ok(ExternalBuildOutcome::Replayed {
                            decision,
                            recovery,
                            replay,
                        });
                    }
                    ExternalReplayStatus::AnalysisMismatch { barrier_index, .. } => (
                        decision,
                        format!(
                            "captured analysis continuation checkpoint {barrier_index} changed"
                        ),
                    ),
                }
            }
            ExternalSnapshotAdmission::ContinuationRequired(snapshot) => {
                // A continuation checkpoint describes an intermediate output
                // state. The populated tree also contains outputs from blocked
                // actions, so observing it in place can neither reproduce that
                // state nor preserve the old tree if a producer executes before
                // a mismatch. Replay in the existing clean managed transaction:
                // publish the fresh tree only after every checkpoint matches,
                // and restore the prior tree before recapture otherwise.
                let replay = ExternalReplayEngine::with_cas(
                    snapshot.clone(),
                    self.cache.clone(),
                    ResourceManager::new()?,
                    self.cas.clone(),
                )?;
                let transaction = self
                    .store
                    .begin_clean_managed_capture()
                    .context("begin clean external continuation replay")?;
                let replay = match replay.replay().await {
                    Ok(replay) => replay,
                    Err(error) => {
                        transaction
                            .rollback()
                            .context("roll back failed external continuation replay")?;
                        return Err(error).context("replay external analysis continuation");
                    }
                };
                match replay {
                    ExternalReplayStatus::Complete { .. } => {
                        transaction
                            .commit_replay(&snapshot)
                            .context("commit clean external continuation replay")?;
                        return Ok(ExternalBuildOutcome::Replayed {
                            decision,
                            recovery,
                            replay,
                        });
                    }
                    ExternalReplayStatus::AnalysisMismatch { barrier_index, .. } => {
                        transaction
                            .rollback()
                            .context("restore managed output after continuation mismatch")?;
                        (
                            decision,
                            format!(
                                "captured analysis continuation checkpoint {barrier_index} changed"
                            ),
                        )
                    }
                }
            }
            ExternalSnapshotAdmission::Unsupported(reason) => {
                return Ok(ExternalBuildOutcome::Unsupported { recovery, reason });
            }
            ExternalSnapshotAdmission::Missing(admission_reason)
            | ExternalSnapshotAdmission::Stale(admission_reason)
            | ExternalSnapshotAdmission::Invalid(admission_reason) => (decision, admission_reason),
        };
        self.store
            .claim_managed_output()
            .context("claim managed external output")?;
        let transaction = self
            .store
            .begin_clean_managed_capture()
            .context("begin clean managed external capture")?;
        let stage_directory = transaction.action_stage_directory();
        let request = ExternalCaptureRequest {
            output_root: self.store_output_root(),
            transaction_identity: transaction.identity(),
        };
        let expected_transaction_identity = request.transaction_identity;
        let completion = match capture(request).await {
            Ok(completion) => completion,
            Err(error) => {
                transaction
                    .rollback()
                    .context("roll back failed external capture callback")?;
                return Err(error).context("external capture callback failed");
            }
        };
        match completion {
            ExternalCaptureCompletion::Failed(result) => {
                transaction.rollback()?;
                Ok(ExternalBuildOutcome::CaptureFailed {
                    decision,
                    admission_reason,
                    recovery,
                    result,
                })
            }
            ExternalCaptureCompletion::Uncertified { result, reason } => {
                let reason = normalized_uncertified_reason(reason);
                transaction.keep_uncertified(&reason)?;
                Ok(ExternalBuildOutcome::CaptureUncertified {
                    decision,
                    admission_reason,
                    recovery,
                    result,
                    reason,
                })
            }
            ExternalCaptureCompletion::Certified {
                result,
                snapshot,
                witness_receipt,
                transaction_identity,
            } => {
                if transaction_identity != expected_transaction_identity {
                    let reason = "captured observer transaction identity does not match \
                                          this managed capture"
                        .to_string();
                    transaction.keep_uncertified(&reason)?;
                    return Ok(ExternalBuildOutcome::CaptureUncertified {
                        decision,
                        admission_reason,
                        recovery,
                        result,
                        reason,
                    });
                }
                let snapshot_identity = snapshot.validate_structure().and_then(|()| {
                    if &snapshot.key != self.store.expected_key() {
                        anyhow::bail!("captured snapshot key does not match this invocation");
                    }
                    Ok(())
                });
                if let Err(error) = snapshot_identity {
                    let reason = format!("captured snapshot certification failed: {error}");
                    transaction.keep_uncertified(&reason)?;
                    return Ok(ExternalBuildOutcome::CaptureUncertified {
                        decision,
                        admission_reason,
                        recovery,
                        result,
                        reason,
                    });
                }
                if let Err(error) = witness_receipt.validate_for(&snapshot) {
                    let reason = format!("captured witness receipt certification failed: {error}");
                    transaction.keep_uncertified(&reason)?;
                    return Ok(ExternalBuildOutcome::CaptureUncertified {
                        decision,
                        admission_reason,
                        recovery,
                        result,
                        reason,
                    });
                }
                let reusable_actions = snapshot
                    .graph
                    .actions
                    .iter()
                    .filter(|action| action.cacheable && !action.always_run)
                    .count();
                if reusable_actions == 0 {
                    let skipped_actions = snapshot.graph.actions.len();
                    transaction.commit(&snapshot)?;
                    return Ok(ExternalBuildOutcome::Captured {
                        decision,
                        admission_reason,
                        recovery,
                        result,
                        adopted_actions: 0,
                        skipped_actions,
                    });
                }
                let adoption = match stage_external_capture_results(
                    &snapshot,
                    witness_receipt.witnesses(),
                    &self.cas,
                    &stage_directory,
                ) {
                    Ok(adoption) => adoption,
                    Err(error) => {
                        let reason = format!("captured action result adoption failed: {error}");
                        transaction.keep_uncertified(&reason)?;
                        return Ok(ExternalBuildOutcome::CaptureUncertified {
                            decision,
                            admission_reason,
                            recovery,
                            result,
                            reason,
                        });
                    }
                };
                let adopted_actions = adoption.adopted_action_count();
                let skipped_actions = adoption.skipped_action_count();
                transaction
                    .commit_with_action_results(&snapshot, adoption, &self.cache, &self.cas)
                    .await?;
                Ok(ExternalBuildOutcome::Captured {
                    decision,
                    admission_reason,
                    recovery,
                    result,
                    adopted_actions,
                    skipped_actions,
                })
            }
        }
    }

    fn store_output_root(&self) -> PathBuf {
        // The output root is authenticated by the store's analysis key. Keep
        // the field private there so callers cannot substitute a different
        // path between admission and transaction creation.
        self.store.output_root().to_path_buf()
    }
}

fn normalized_uncertified_reason(reason: String) -> String {
    if reason.trim().is_empty() {
        "external capture observer did not provide a certification reason".to_string()
    } else {
        reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_action_identity::{
        ExternalActionCaptureWitness, ExternalExecutableWitness,
    };
    use crate::external_analysis_snapshot::{
        ExternalAction, ExternalActionGraph, ExternalAnalysisBarrier, ExternalAnalysisKey,
        ExternalAnalyzerIdentity, ExternalCommand, ExternalInput, ExternalOutput,
    };
    use rush_action::action_manifest::ManifestResourceClass;
    use rush_action::artifact_value::content_digest_of_file;
    use rush_action::external_action_key::{
        ExternalArgumentKey, ExternalCredentialState, ExternalInputKey, ExternalInputValue,
        ExternalOutputKind, ExternalPathKey,
    };
    use rush_cache::CacheConfig;
    use rush_incremental::DependencyManifest;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::Command as ProcessCommand;

    fn rooted(root: &str, path: &str) -> ExternalPathKey {
        ExternalPathKey {
            root: root.to_string(),
            path: path.as_bytes().to_vec(),
        }
    }

    #[cfg(target_os = "linux")]
    fn current_credentials() -> ExternalCredentialState {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| {
                    line.strip_prefix(name)
                        .and_then(|line| line.strip_prefix(':'))
                })
                .unwrap()
                .trim()
        };
        let identities = |name: &str| {
            field(name)
                .split_ascii_whitespace()
                .map(|value| value.parse::<u32>().unwrap())
                .collect::<Vec<_>>()
        };
        let users = identities("Uid");
        let groups = identities("Gid");
        let mut supplementary_groups = field("Groups")
            .split_ascii_whitespace()
            .map(|value| value.parse::<u32>().unwrap())
            .collect::<Vec<_>>();
        supplementary_groups.sort_unstable();
        let capabilities = |name: &str| u64::from_str_radix(field(name), 16).unwrap();
        ExternalCredentialState {
            real_user: users[0],
            effective_user: users[1],
            saved_user: users[2],
            filesystem_user: users[3],
            real_group: groups[0],
            effective_group: groups[1],
            saved_group: groups[2],
            filesystem_group: groups[3],
            supplementary_groups,
            inheritable_capabilities: capabilities("CapInh"),
            permitted_capabilities: capabilities("CapPrm"),
            effective_capabilities: capabilities("CapEff"),
            bounding_capabilities: capabilities("CapBnd"),
            ambient_capabilities: capabilities("CapAmb"),
            no_new_privileges: field("NoNewPrivs") == "1",
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn current_credentials() -> ExternalCredentialState {
        ExternalCredentialState::default()
    }

    fn captured_graph(
        root: &Path,
        goal: &[u8],
    ) -> (ExternalAnalysisSnapshot, ExternalActionCaptureWitness) {
        let source = root.join("src");
        let output = root.join("obj");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&output).unwrap();
        let tool = source.join("tool");
        if !tool.exists() {
            std::fs::write(
                &tool,
                b"#!/bin/sh\nprintf executed > \"${1:-captured.o}\"\n",
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let analyzer = ExternalAnalyzerIdentity {
            kind: "test-analyzer".to_string(),
            executable_digest: [7; 32],
            argv: vec![b"analyzer".to_vec(), goal.to_vec()],
            working_directory: source.clone(),
            environment: BTreeMap::new(),
            build_options: BTreeMap::new(),
            platform: BTreeMap::new(),
        };
        let key = ExternalAnalysisKey::new(
            source.clone(),
            output.clone(),
            &analyzer,
            vec![goal.to_vec()],
        );
        let action = ExternalAction {
            logical_id: "compile".to_string(),
            mnemonic: "KernelCompile".to_string(),
            commands: vec![ExternalCommand {
                executable: rooted("src", "tool"),
                argv0: ExternalArgumentKey::Path(rooted("src", "tool")),
                argv: Vec::new(),
                environment: None,
                working_directory: None,
            }],
            working_directory: rooted("obj", "."),
            environment: BTreeMap::new(),
            signal_state: Default::default(),
            file_creation_mask: 0o022,
            credentials: current_credentials(),
            normal_inputs: Vec::<ExternalInput>::new(),
            order_only_inputs: Vec::new(),
            outputs: vec![ExternalOutput {
                path: rooted("obj", "captured.o"),
                kind: ExternalOutputKind::File,
                mode: 0o644,
            }],
            dependencies: Vec::new(),
            config_deps: Vec::new(),
            depfile: None,
            restat: true,
            always_run: false,
            cacheable: true,
            local_only: true,
            sandbox: false,
            resource: ManifestResourceClass::KernelCompile,
            execution_properties: BTreeMap::new(),
        };
        let snapshot = ExternalAnalysisSnapshot::new(
            key,
            analyzer,
            DependencyManifest::from_deps(Vec::new()),
            ExternalActionGraph {
                roots: BTreeMap::from([("src".to_string(), source), ("obj".to_string(), output)]),
                kconfig: None,
                initial_inputs: Vec::new(),
                empty_output_inputs: Vec::new(),
                actions: vec![action],
                analysis_barriers: Vec::new(),
                requested_outputs: vec![rooted("obj", "captured.o")],
            },
        )
        .unwrap();
        let witness = ExternalActionCaptureWitness {
            logical_id: "compile".to_string(),
            executables: vec![ExternalExecutableWitness {
                path: rooted("src", "tool"),
                value: ExternalInputValue::File {
                    digest: content_digest_of_file(&tool).unwrap().0,
                    executable: true,
                },
            }],
            inputs: Vec::<ExternalInputKey>::new(),
        };
        (snapshot, witness)
    }

    fn cache(root: &Path) -> (Arc<ActionCache>, Arc<CasStore>) {
        let cache = Arc::new(
            ActionCache::with_disk(CacheConfig {
                cache_root: root.join("actions"),
                max_size: 0,
                ttl_seconds: 0,
            })
            .unwrap(),
        );
        let cas = Arc::new(CasStore::new(root).unwrap());
        (cache, cas)
    }

    fn have_tool(name: &str) -> bool {
        ProcessCommand::new("sh")
            .arg("-c")
            .arg(format!("command -v {name} >/dev/null 2>&1"))
            .status()
            .is_ok_and(|status| status.success())
    }

    fn mandatory_archive_graph(
        root: &Path,
    ) -> (ExternalAnalysisSnapshot, ExternalActionCaptureWitness) {
        let (mut snapshot, mut witness) = captured_graph(root, b"archive");
        let source = snapshot.key.workspace_root.clone();
        let tool = source.join("tool");
        std::fs::write(&tool, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let member_source = source.join("member.cc");
        let member_object = source.join("member.o");
        std::fs::write(&member_source, b"int member() { return 1; }\n").unwrap();
        assert!(ProcessCommand::new("g++")
            .args(["-c", member_source.to_str().unwrap(), "-o"])
            .arg(&member_object)
            .status()
            .unwrap()
            .success());

        let action = &mut snapshot.graph.actions[0];
        action.mnemonic = "CcArchive".to_string();
        action.commands[0].argv = vec![
            ExternalArgumentKey::Literal(b"rcs".to_vec()),
            ExternalArgumentKey::Path(rooted("obj", "captured.o")),
            ExternalArgumentKey::Literal(b"--thin".to_vec()),
            ExternalArgumentKey::Path(rooted("src", "member.o")),
        ];
        action.normal_inputs = vec![ExternalInput {
            path: rooted("src", "member.o"),
            kind: crate::external_analysis_snapshot::ExternalInputKind::File,
        }];
        action.always_run = true;
        witness.executables[0].value = ExternalInputValue::File {
            digest: content_digest_of_file(&tool).unwrap().0,
            executable: true,
        };
        witness.inputs = vec![ExternalInputKey {
            path: rooted("src", "member.o"),
            value: ExternalInputValue::File {
                digest: content_digest_of_file(&member_object).unwrap().0,
                executable: false,
            },
        }];
        snapshot.validate_structure().unwrap();
        (snapshot, witness)
    }

    fn controller(
        state: &Path,
        snapshot: &ExternalAnalysisSnapshot,
        cache: Arc<ActionCache>,
        cas: Arc<CasStore>,
    ) -> ExternalBuildController {
        ExternalBuildController::new(
            ExternalGenerationStore::new(state, snapshot.key.clone()),
            cache,
            cas,
            CommandSemantics::from_process(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn capture_is_adopted_once_then_replayed_without_analyzer() {
        let temp = tempfile::tempdir().unwrap();
        let (snapshot, witness) = captured_graph(temp.path(), b"all");
        let output = snapshot.key.output_root.clone();
        let (cache, cas) = cache(&temp.path().join("cache"));
        let controller = controller(&temp.path().join("state"), &snapshot, cache, cas);

        let captured_snapshot = snapshot.clone();
        let captured_witness = witness.clone();
        let first = controller
            .build(move |request| async move {
                let transaction_identity = request.transaction_identity;
                let managed_output = request.output_root;
                assert_eq!(managed_output, captured_snapshot.key.output_root);
                std::fs::write(managed_output.join("captured.o"), b"captured\n")?;
                let witness_receipt =
                    ExternalCaptureWitnessReceipt::new(&captured_snapshot, vec![captured_witness])?;
                Ok(ExternalCaptureCompletion::Certified {
                    result: 0,
                    snapshot: Box::new(captured_snapshot),
                    witness_receipt,
                    transaction_identity,
                })
            })
            .await
            .unwrap();
        assert!(matches!(
            first,
            ExternalBuildOutcome::Captured {
                decision: ExternalSnapshotDecision::SnapshotMissing,
                adopted_actions: 1,
                skipped_actions: 0,
                ..
            }
        ));

        let second = controller
            .build(|_| async {
                Err::<ExternalCaptureCompletion<i32>, _>(anyhow::anyhow!(
                    "analyzer must not run on a snapshot hit"
                ))
            })
            .await
            .unwrap();
        assert!(matches!(
            second,
            ExternalBuildOutcome::Replayed {
                decision: ExternalSnapshotDecision::SnapshotHit,
                replay: ExternalReplayStatus::Complete {
                    cache_hits: 1,
                    executed: 0
                },
                ..
            }
        ));

        std::fs::remove_file(output.join("captured.o")).unwrap();
        let restored = controller
            .build(|_| async {
                Err::<ExternalCaptureCompletion<i32>, _>(anyhow::anyhow!(
                    "analyzer must not run for CAS restoration"
                ))
            })
            .await
            .unwrap();
        assert!(matches!(
            restored,
            ExternalBuildOutcome::Replayed {
                replay: ExternalReplayStatus::Complete {
                    cache_hits: 1,
                    executed: 0
                },
                ..
            }
        ));
        assert_eq!(
            std::fs::read(output.join("captured.o")).unwrap(),
            b"captured\n"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mismatched_continuation_starts_one_clean_recapture() {
        let temp = tempfile::tempdir().unwrap();
        let (mut snapshot, witness) = captured_graph(temp.path(), b"all");
        let mut later = snapshot.graph.actions[0].clone();
        later.logical_id = "later".to_string();
        later.outputs[0].path = rooted("obj", "later.o");
        later.dependencies = vec!["compile".to_string()];
        snapshot.graph.actions.push(later);
        snapshot
            .graph
            .analysis_barriers
            .push(ExternalAnalysisBarrier {
                producer_closure: vec!["compile".to_string()],
                expected_inputs: vec![ExternalInputKey {
                    path: rooted("obj", "captured.o"),
                    value: ExternalInputValue::File {
                        digest: [9; 32],
                        executable: false,
                    },
                }],
                blocked_actions: vec!["later".to_string()],
            });
        snapshot.validate_structure().unwrap();
        let output = snapshot.key.output_root.clone();
        let (cache, cas) = cache(&temp.path().join("cache"));
        let controller = controller(&temp.path().join("state"), &snapshot, cache, cas);

        let captured_snapshot = snapshot.clone();
        let later_witness = ExternalActionCaptureWitness {
            logical_id: "later".to_string(),
            executables: witness.executables.clone(),
            inputs: witness.inputs.clone(),
        };
        let first = controller
            .build(move |request| async move {
                std::fs::write(request.output_root.join("captured.o"), b"captured\n")?;
                std::fs::write(request.output_root.join("later.o"), b"later\n")?;
                let witness_receipt = ExternalCaptureWitnessReceipt::new(
                    &captured_snapshot,
                    vec![witness, later_witness],
                )?;
                Ok(ExternalCaptureCompletion::Certified {
                    result: 0,
                    snapshot: Box::new(captured_snapshot),
                    witness_receipt,
                    transaction_identity: request.transaction_identity,
                })
            })
            .await
            .unwrap();
        assert!(matches!(first, ExternalBuildOutcome::Captured { .. }));

        let recaptured = controller
            .build(|request| async move {
                assert_eq!(request.output_root, output);
                Ok::<_, anyhow::Error>(ExternalCaptureCompletion::Failed(7))
            })
            .await
            .unwrap();
        assert!(matches!(
            recaptured,
            ExternalBuildOutcome::CaptureFailed {
                decision: ExternalSnapshotDecision::SnapshotContinuationRequired,
                admission_reason,
                result: 7,
                ..
            } if admission_reason.contains("checkpoint 0 changed")
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn continuation_replays_directory_checkpoint_in_a_clean_tree() {
        let temp = tempfile::tempdir().unwrap();
        let (mut snapshot, witness) = captured_graph(temp.path(), b"all");
        let checkpoint = temp.path().join("checkpoint");
        std::fs::create_dir(&checkpoint).unwrap();
        std::fs::write(checkpoint.join("captured.o"), b"captured\n").unwrap();
        let checkpoint_digest =
            crate::external_replay::digest_directory_listing(&checkpoint).unwrap();
        let mut later = snapshot.graph.actions[0].clone();
        later.logical_id = "later".to_string();
        later.commands[0].argv = vec![ExternalArgumentKey::Literal(b"later.o".to_vec())];
        later.outputs[0].path = rooted("obj", "later.o");
        later.dependencies = vec!["compile".to_string()];
        snapshot.graph.actions.push(later);
        snapshot
            .graph
            .analysis_barriers
            .push(ExternalAnalysisBarrier {
                producer_closure: vec!["compile".to_string()],
                expected_inputs: vec![ExternalInputKey {
                    path: rooted("obj", "."),
                    value: ExternalInputValue::DirectoryListing {
                        digest: checkpoint_digest,
                    },
                }],
                blocked_actions: vec!["later".to_string()],
            });
        snapshot.graph.requested_outputs = vec![rooted("obj", "later.o")];
        snapshot.validate_structure().unwrap();
        let output = snapshot.key.output_root.clone();
        let (cache, cas) = cache(&temp.path().join("cache"));
        let controller = controller(&temp.path().join("state"), &snapshot, cache, cas);

        let captured_snapshot = snapshot.clone();
        let later_witness = ExternalActionCaptureWitness {
            logical_id: "later".to_string(),
            executables: witness.executables.clone(),
            inputs: witness.inputs.clone(),
        };
        let first = controller
            .build(move |request| async move {
                std::fs::write(request.output_root.join("captured.o"), b"captured\n")?;
                std::fs::write(request.output_root.join("later.o"), b"later\n")?;
                let witness_receipt = ExternalCaptureWitnessReceipt::new(
                    &captured_snapshot,
                    vec![witness, later_witness],
                )?;
                Ok(ExternalCaptureCompletion::Certified {
                    result: 0,
                    snapshot: Box::new(captured_snapshot),
                    witness_receipt,
                    transaction_identity: request.transaction_identity,
                })
            })
            .await
            .unwrap();
        assert!(matches!(first, ExternalBuildOutcome::Captured { .. }));
        assert!(output.join("later.o").is_file());

        let replayed = controller
            .build(|_| async {
                Err::<ExternalCaptureCompletion<i32>, _>(anyhow::anyhow!(
                    "analyzer must not run for a matching continuation"
                ))
            })
            .await
            .unwrap();
        assert!(matches!(
            replayed,
            ExternalBuildOutcome::Replayed {
                decision: ExternalSnapshotDecision::SnapshotContinuationRequired,
                replay: ExternalReplayStatus::Complete {
                    cache_hits: 2,
                    executed: 0,
                },
                ..
            }
        ));
        assert_eq!(
            std::fs::read(output.join("captured.o")).unwrap(),
            b"captured\n"
        );
        assert_eq!(std::fs::read(output.join("later.o")).unwrap(), b"later\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restarted_controller_executes_mandatory_reusable_archive() {
        if !have_tool("g++") || !have_tool("ar") {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let (snapshot, witness) = mandatory_archive_graph(temp.path());
        let (cache, cas) = cache(&temp.path().join("cache"));

        let captured_snapshot = snapshot.clone();
        let captured_witness = witness.clone();
        let first = controller(&state, &snapshot, cache.clone(), cas.clone());
        let captured = first
            .build(move |request| async move {
                let transaction_identity = request.transaction_identity;
                let output = request.output_root;
                let archive = output.join("captured.o");
                let object = captured_snapshot.key.workspace_root.join("member.o");
                anyhow::ensure!(
                    ProcessCommand::new("ar")
                        .args(["rcs", archive.to_str().unwrap(), "--thin"])
                        .arg(object)
                        .status()?
                        .success(),
                    "ar failed"
                );
                let receipt =
                    ExternalCaptureWitnessReceipt::new(&captured_snapshot, vec![captured_witness])?;
                Ok(ExternalCaptureCompletion::Certified {
                    result: 0,
                    snapshot: Box::new(captured_snapshot),
                    witness_receipt: receipt,
                    transaction_identity,
                })
            })
            .await
            .unwrap();
        assert!(matches!(
            captured,
            ExternalBuildOutcome::Captured {
                adopted_actions: 0,
                skipped_actions: 1,
                ..
            }
        ));
        drop(first);

        let restarted = controller(&state, &snapshot, cache, cas);
        let replayed = restarted
            .build(|_| async {
                Err::<ExternalCaptureCompletion<i32>, _>(anyhow::anyhow!(
                    "analyzer must not run on a snapshot hit"
                ))
            })
            .await
            .unwrap();
        assert!(matches!(
            replayed,
            ExternalBuildOutcome::Replayed {
                decision: ExternalSnapshotDecision::SnapshotHit,
                replay: ExternalReplayStatus::Complete {
                    cache_hits: 0,
                    executed: 1
                },
                ..
            }
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_capture_restores_the_prior_exact_path_tree() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let (first_snapshot, first_witness) = captured_graph(temp.path(), b"all");
        let (cache, cas) = cache(&temp.path().join("cache"));
        let first = controller(&state, &first_snapshot, cache.clone(), cas.clone());
        first
            .build(move |request| async move {
                let transaction_identity = request.transaction_identity;
                let output = request.output_root;
                std::fs::write(output.join("captured.o"), b"prior\n")?;
                let witness_receipt =
                    ExternalCaptureWitnessReceipt::new(&first_snapshot, vec![first_witness])?;
                Ok(ExternalCaptureCompletion::Certified {
                    result: 0,
                    snapshot: Box::new(first_snapshot),
                    witness_receipt,
                    transaction_identity,
                })
            })
            .await
            .unwrap();

        let (next_snapshot, _) = captured_graph(temp.path(), b"other-goal");
        let next = controller(&state, &next_snapshot, cache, cas);
        let outcome = next
            .build(|request| async move {
                let output = request.output_root;
                std::fs::write(output.join("captured.o"), b"failed generation\n")?;
                Ok(ExternalCaptureCompletion::Failed(23))
            })
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            ExternalBuildOutcome::CaptureFailed {
                decision: ExternalSnapshotDecision::SnapshotMissing,
                result: 23,
                ..
            }
        ));
        assert_eq!(
            std::fs::read(temp.path().join("obj/captured.o")).unwrap(),
            b"prior\n"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_witness_keeps_make_output_but_invalidates_replay() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let (snapshot, witness) = captured_graph(temp.path(), b"all");
        let receipt = ExternalCaptureWitnessReceipt::new(&snapshot, vec![witness]).unwrap();
        let mut encoded = serde_json::to_value(receipt).unwrap();
        encoded["witnesses"][0]["logical_id"] = serde_json::json!("unknown");
        let bad_receipt: ExternalCaptureWitnessReceipt = serde_json::from_value(encoded).unwrap();
        let (cache, cas) = cache(&temp.path().join("cache"));
        let controller = controller(&state, &snapshot, cache, cas);
        let store = ExternalGenerationStore::new(&state, snapshot.key.clone());

        let outcome = controller
            .build(move |request| async move {
                let transaction_identity = request.transaction_identity;
                let output = request.output_root;
                std::fs::write(output.join("captured.o"), b"valid make output\n")?;
                Ok(ExternalCaptureCompletion::Certified {
                    result: 0,
                    snapshot: Box::new(snapshot),
                    witness_receipt: bad_receipt,
                    transaction_identity,
                })
            })
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            ExternalBuildOutcome::CaptureUncertified { result: 0, .. }
        ));
        assert_eq!(
            std::fs::read(temp.path().join("obj/captured.o")).unwrap(),
            b"valid make output\n"
        );
        assert!(matches!(
            store.load_active_if_fresh(&CommandSemantics::from_process()),
            ExternalSnapshotAdmission::Missing(_)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn substituted_snapshot_keeps_make_output_but_cannot_publish() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let (expected, _) = captured_graph(temp.path(), b"all");
        let (substitute, substitute_witness) = captured_graph(temp.path(), b"other-goal");
        let substitute_receipt =
            ExternalCaptureWitnessReceipt::new(&substitute, vec![substitute_witness]).unwrap();
        let (cache, cas) = cache(&temp.path().join("cache"));
        let controller = controller(&state, &expected, cache, cas);
        let store = ExternalGenerationStore::new(&state, expected.key.clone());

        let outcome = controller
            .build(move |request| async move {
                let transaction_identity = request.transaction_identity;
                let output = request.output_root;
                std::fs::write(output.join("captured.o"), b"substituted snapshot\n")?;
                Ok(ExternalCaptureCompletion::Certified {
                    result: 0,
                    snapshot: Box::new(substitute),
                    witness_receipt: substitute_receipt,
                    transaction_identity,
                })
            })
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            ExternalBuildOutcome::CaptureUncertified { result: 0, .. }
        ));
        assert_eq!(
            std::fs::read(temp.path().join("obj/captured.o")).unwrap(),
            b"substituted snapshot\n"
        );
        assert!(matches!(
            store.load_active_if_fresh(&CommandSemantics::from_process()),
            ExternalSnapshotAdmission::Missing(_)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn substituted_capture_transaction_cannot_publish() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let (snapshot, witness) = captured_graph(temp.path(), b"all");
        let (cache, cas) = cache(&temp.path().join("cache"));
        let controller = controller(&state, &snapshot, cache, cas);
        let store = ExternalGenerationStore::new(&state, snapshot.key.clone());

        let outcome = controller
            .build(move |request| async move {
                let observer_identity = request.process_capture_identity();
                assert_eq!(
                    observer_identity.capture_id(),
                    request.transaction_identity.capture_id()
                );
                assert_eq!(
                    observer_identity.context_digest(),
                    request.transaction_identity.context_digest()
                );
                std::fs::write(
                    request.output_root.join("captured.o"),
                    b"wrong transaction\n",
                )?;
                let witness_receipt = ExternalCaptureWitnessReceipt::new(&snapshot, vec![witness])?;
                let mut capture_id = request.transaction_identity.capture_id();
                capture_id[0] ^= 1;
                let transaction_identity = ExternalCaptureTransactionIdentity::new(
                    capture_id,
                    request.transaction_identity.context_digest(),
                );
                Ok(ExternalCaptureCompletion::Certified {
                    result: 0,
                    snapshot: Box::new(snapshot),
                    witness_receipt,
                    transaction_identity,
                })
            })
            .await
            .unwrap();
        match outcome {
            ExternalBuildOutcome::CaptureUncertified { reason, .. } => {
                assert!(reason.contains("observer transaction identity"));
            }
            _ => panic!("substituted capture transaction was not rejected"),
        }
        assert_eq!(
            std::fs::read(temp.path().join("obj/captured.o")).unwrap(),
            b"wrong transaction\n"
        );
        assert!(matches!(
            store.load_active_if_fresh(&CommandSemantics::from_process()),
            ExternalSnapshotAdmission::Missing(_)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_uncertified_reason_still_keeps_output_and_invalidates() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let (snapshot, _) = captured_graph(temp.path(), b"all");
        let (cache, cas) = cache(&temp.path().join("cache"));
        let controller = controller(&state, &snapshot, cache.clone(), cas.clone());

        let outcome = controller
            .build(|request| async move {
                let output = request.output_root;
                std::fs::write(output.join("captured.o"), b"uncertified\n")?;
                Ok(ExternalCaptureCompletion::Uncertified {
                    result: 0,
                    reason: "  ".to_string(),
                })
            })
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            ExternalBuildOutcome::CaptureUncertified { result: 0, .. }
        ));

        let store = ExternalGenerationStore::new(&state, snapshot.key.clone());
        assert!(matches!(
            store.load_active_if_fresh(&CommandSemantics::from_process()),
            ExternalSnapshotAdmission::Missing(_)
        ));
        assert_eq!(
            std::fs::read(temp.path().join("obj/captured.o")).unwrap(),
            b"uncertified\n"
        );
    }
}
