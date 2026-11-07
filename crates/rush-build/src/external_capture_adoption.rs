//! Private adoption of outputs produced by a certified external capture.
//!
//! A successful analyzer run is already the build. Adoption therefore records
//! its reusable action outputs without executing them again, but keeps every
//! resulting [`ActionResult`] private until the complete set is sealed.

use crate::external_action_identity::{
    certify_external_action_capture_witnesses, ExternalActionCaptureWitness,
};
use crate::external_analysis_snapshot::ExternalAnalysisSnapshot;
use crate::external_replay::{normalize_and_validate_outputs, resolve_path};
use crate::virtual_materializer::records_available;
use anyhow::{Context, Result};
use rush_action::artifact_value::ArtifactValueKind;
use rush_action::external_action_key::ExternalOutputKind;
use rush_action::{ActionId, ActionResult, Artifact};
use rush_cache::cas::CasStore;
use rush_cache::staging::{CertifiedActionCacheStage, StagedActionCache};
use std::path::Path;
use std::time::Duration;

/// A complete private action-result stage produced from one capture.
#[derive(Debug)]
pub struct ExternalCaptureAdoption {
    stage: CertifiedActionCacheStage,
    snapshot_digest: [u8; 32],
    adopted_action_count: usize,
    skipped_action_count: usize,
}

impl ExternalCaptureAdoption {
    /// Certified stage that remains invisible until a controller publishes it.
    pub fn stage(&self) -> &CertifiedActionCacheStage {
        &self.stage
    }

    /// Consume this receipt and return its certified private stage.
    pub fn into_stage(self) -> CertifiedActionCacheStage {
        self.stage
    }

    /// Digest of the exact snapshot whose action identities were certified.
    pub(crate) fn snapshot_digest(&self) -> [u8; 32] {
        self.snapshot_digest
    }

    /// Number of cache-reusable captured actions staged for publication.
    pub fn adopted_action_count(&self) -> usize {
        self.adopted_action_count
    }

    /// Number of uncacheable or always-run actions deliberately not staged.
    pub fn skipped_action_count(&self) -> usize {
        self.skipped_action_count
    }
}

/// Record a captured build's reusable outputs into CAS and a private stage.
///
/// Snapshot structure and the complete read-time witness set are certified
/// before the stage is created. Every declared output is normalized, recorded
/// durably in the supplied CAS, checked 1:1 against its declaration, and only
/// then placed in a zero-execution action result. Uncacheable and always-run
/// actions are intentionally skipped because replay cannot reuse them.
pub fn stage_external_capture_results(
    snapshot: &ExternalAnalysisSnapshot,
    witnesses: &[ExternalActionCaptureWitness],
    cas: &CasStore,
    stage_directory: &Path,
) -> Result<ExternalCaptureAdoption> {
    snapshot.validate_structure()?;
    let snapshot_digest = *blake3::hash(&bincode::serialize(snapshot)?).as_bytes();
    let action_ids = certify_external_action_capture_witnesses(&snapshot.graph, witnesses)?;
    let expected = snapshot
        .graph
        .actions
        .iter()
        .filter(|action| action.cacheable && !action.always_run)
        .map(|action| {
            action_ids
                .get(&action.logical_id)
                .cloned()
                .context("certified external action lacks an action ID")
        })
        .collect::<Result<Vec<ActionId>>>()?;
    let stage = StagedActionCache::create(stage_directory)?;
    let mut adopted_action_count = 0usize;
    let mut skipped_action_count = 0usize;

    for action in &snapshot.graph.actions {
        if !action.cacheable || action.always_run {
            skipped_action_count += 1;
            continue;
        }
        let action_id = action_ids
            .get(&action.logical_id)
            .cloned()
            .context("certified external action lacks an action ID")?;
        let resolved_outputs = action
            .outputs
            .iter()
            .map(|output| {
                Ok((
                    resolve_path(&snapshot.graph, &output.path)?,
                    output.kind,
                    output.mode,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        normalize_and_validate_outputs(&resolved_outputs).with_context(|| {
            format!(
                "normalize captured outputs for external action {:?}",
                action.logical_id
            )
        })?;

        let mut output_records = Vec::with_capacity(resolved_outputs.len());
        for (path, expected_kind, _) in &resolved_outputs {
            let record = cas.record_output_durable(path).with_context(|| {
                format!(
                    "record captured output for external action {:?}",
                    action.logical_id
                )
            })?;
            if record.path != *path || !output_kind_matches(record.kind, *expected_kind) {
                anyhow::bail!(
                    "captured output record does not match its declaration for action {:?}",
                    action.logical_id
                );
            }
            output_records.push(record);
        }
        if output_records.len() != action.outputs.len() || !records_available(cas, &output_records)
        {
            anyhow::bail!(
                "captured action {:?} does not have a complete durable output set",
                action.logical_id
            );
        }
        let outputs = resolved_outputs
            .iter()
            .map(|(path, _, _)| Artifact::Source(path.clone()))
            .collect();
        let result = ActionResult {
            action_id: action_id.clone(),
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            outputs,
            output_records,
            duration: Duration::ZERO,
            cached: false,
        };
        stage.put(action_id, &result)?;
        adopted_action_count += 1;
    }

    let stage = stage.certify(&expected)?;
    Ok(ExternalCaptureAdoption {
        stage,
        snapshot_digest,
        adopted_action_count,
        skipped_action_count,
    })
}

fn output_kind_matches(recorded: ArtifactValueKind, expected: ExternalOutputKind) -> bool {
    matches!(
        (recorded, expected),
        (ArtifactValueKind::RegularFile, ExternalOutputKind::File)
            | (ArtifactValueKind::Symlink, ExternalOutputKind::Symlink)
            | (ArtifactValueKind::Tree, ExternalOutputKind::Tree)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_action_identity::ExternalExecutableWitness;
    use crate::external_analysis_snapshot::{
        ExternalAction, ExternalActionGraph, ExternalAnalysisKey, ExternalAnalyzerIdentity,
        ExternalCommand, ExternalInput, ExternalInputKind, ExternalOutput,
    };
    use rush_action::action_manifest::ManifestResourceClass;
    use rush_action::artifact_value::content_digest_of_file;
    use rush_action::external_action_key::{
        ExternalArgumentKey, ExternalInputKey, ExternalInputValue, ExternalPathKey,
    };
    use rush_cache::{ActionCache, CacheConfig};
    use rush_incremental::{content_digest_of_regular_file, Dependency, DependencyManifest};
    use std::collections::BTreeMap;

    fn rooted(root: &str, path: &str) -> ExternalPathKey {
        ExternalPathKey {
            root: root.to_string(),
            path: path.as_bytes().to_vec(),
        }
    }

    fn fixture(root: &Path) -> (ExternalAnalysisSnapshot, ExternalActionCaptureWitness) {
        let src = root.join("src");
        let obj = root.join("obj");
        let toolchain = root.join("toolchain");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&obj).unwrap();
        std::fs::create_dir_all(toolchain.join("bin")).unwrap();
        std::fs::write(src.join("Makefile"), b"all:\n").unwrap();
        std::fs::write(src.join("a.c"), b"int a;\n").unwrap();
        std::fs::write(toolchain.join("bin/cc"), b"compiler bytes\n").unwrap();
        std::fs::write(obj.join("a.o"), b"captured object\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(obj.join("a.o"), std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }

        let analyzer = ExternalAnalyzerIdentity {
            kind: "test-analyzer".to_string(),
            executable_digest: [7; 32],
            argv: vec![b"analyzer".to_vec()],
            working_directory: src.clone(),
            environment: BTreeMap::new(),
            build_options: BTreeMap::new(),
            platform: BTreeMap::new(),
        };
        let key =
            ExternalAnalysisKey::new(src.clone(), obj.clone(), &analyzer, vec![b"all".to_vec()]);
        let makefile = src.join("Makefile");
        let manifest = DependencyManifest::from_deps(vec![Dependency::FileContent {
            path: makefile.clone(),
            digest: content_digest_of_regular_file(&makefile, 1 << 20).unwrap(),
        }]);
        let action = ExternalAction {
            logical_id: "compile".to_string(),
            mnemonic: "KernelCompile".to_string(),
            commands: vec![ExternalCommand {
                executable: rooted("toolchain", "bin/cc"),
                argv0: ExternalArgumentKey::Path(rooted("toolchain", "bin/cc")),
                argv: vec![
                    ExternalArgumentKey::Literal(b"-c".to_vec()),
                    ExternalArgumentKey::Path(rooted("src", "a.c")),
                ],
                environment: None,
                working_directory: None,
            }],
            working_directory: rooted("obj", "."),
            environment: BTreeMap::new(),
            signal_state: Default::default(),
            file_creation_mask: 0o022,
            credentials: Default::default(),
            normal_inputs: vec![ExternalInput {
                path: rooted("src", "a.c"),
                kind: ExternalInputKind::File,
            }],
            order_only_inputs: Vec::new(),
            outputs: vec![ExternalOutput {
                path: rooted("obj", "a.o"),
                kind: ExternalOutputKind::File,
                mode: 0o640,
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
            manifest,
            ExternalActionGraph {
                roots: BTreeMap::from([
                    ("src".to_string(), src.clone()),
                    ("obj".to_string(), obj.clone()),
                    ("toolchain".to_string(), toolchain.clone()),
                ]),
                kconfig: None,
                initial_inputs: Vec::new(),
                empty_output_inputs: Vec::new(),
                actions: vec![action],
                analysis_barriers: Vec::new(),
                requested_outputs: vec![rooted("obj", "a.o")],
            },
        )
        .unwrap();
        let witness = ExternalActionCaptureWitness {
            logical_id: "compile".to_string(),
            executables: vec![ExternalExecutableWitness {
                path: rooted("toolchain", "bin/cc"),
                value: file_value(&toolchain.join("bin/cc"), true),
            }],
            inputs: vec![ExternalInputKey {
                path: rooted("src", "a.c"),
                value: file_value(&src.join("a.c"), false),
            }],
        };
        (snapshot, witness)
    }

    fn file_value(path: &Path, executable: bool) -> ExternalInputValue {
        ExternalInputValue::File {
            digest: content_digest_of_file(path).unwrap().0,
            executable,
        }
    }

    fn disk_cache(root: &Path) -> ActionCache {
        ActionCache::with_disk(CacheConfig {
            cache_root: root.to_path_buf(),
            max_size: 0,
            ttl_seconds: 0,
        })
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adoption_records_without_execution_or_early_visibility() {
        let temp = tempfile::tempdir().unwrap();
        let (snapshot, witness) = fixture(temp.path());
        let cache_root = temp.path().join("cache");
        let cas = CasStore::new(&cache_root).unwrap();
        let stages = temp.path().join("stages");
        std::fs::create_dir(&stages).unwrap();
        let expected_id = certify_external_action_capture_witnesses(
            &snapshot.graph,
            std::slice::from_ref(&witness),
        )
        .unwrap()["compile"]
            .clone();
        let cache = disk_cache(&cache_root.join("actions"));

        let adoption =
            stage_external_capture_results(&snapshot, &[witness], &cas, &stages.join("capture"))
                .unwrap();

        assert_eq!(adoption.adopted_action_count(), 1);
        assert_eq!(adoption.skipped_action_count(), 0);
        assert_eq!(adoption.stage().receipt().entry_count(), 1);
        assert!(cache.get(&expected_id).is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(snapshot.key.output_root.join("a.o"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777,
                0o640
            );
        }

        adoption
            .stage()
            .publish_into_with_cas(&cache, &cas)
            .await
            .unwrap();
        let result = cache.get(&expected_id).unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.is_empty() && result.stderr.is_empty());
        assert_eq!(result.duration, Duration::ZERO);
        assert!(!result.cached);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.output_records.len(), 1);
        assert!(records_available(&cas, &result.output_records));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn adoption_restores_non_utf8_symlink_target_losslessly() {
        use crate::virtual_materializer::materialize_record;
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let temp = tempfile::tempdir().unwrap();
        let (mut snapshot, witness) = fixture(temp.path());
        let output = snapshot.key.output_root.join("a.o");
        std::fs::remove_file(&output).unwrap();
        let target = b"captured-target-\xff";
        std::os::unix::fs::symlink(OsStr::from_bytes(target), &output).unwrap();
        snapshot.graph.actions[0].outputs[0].kind = ExternalOutputKind::Symlink;
        snapshot.graph.actions[0].outputs[0].mode = 0o777;
        snapshot.validate_structure().unwrap();

        let expected_id = certify_external_action_capture_witnesses(
            &snapshot.graph,
            std::slice::from_ref(&witness),
        )
        .unwrap()["compile"]
            .clone();
        let cache_root = temp.path().join("cache");
        let cache = disk_cache(&cache_root.join("actions"));
        let cas = CasStore::new(&cache_root).unwrap();
        let stages = temp.path().join("stages");
        std::fs::create_dir(&stages).unwrap();
        let adoption =
            stage_external_capture_results(&snapshot, &[witness], &cas, &stages.join("capture"))
                .unwrap();
        adoption
            .stage()
            .publish_into_with_cas(&cache, &cas)
            .await
            .unwrap();
        let result = cache.get(&expected_id).unwrap();

        std::fs::remove_file(&output).unwrap();
        materialize_record(&cas, &result.output_records[0]).unwrap();
        let restored = std::fs::read_link(&output).unwrap();
        assert_eq!(restored.as_os_str().as_bytes(), target);
    }

    #[test]
    fn invalid_witness_set_creates_no_stage() {
        let temp = tempfile::tempdir().unwrap();
        let (snapshot, _) = fixture(temp.path());
        let cache_root = temp.path().join("cache");
        let cas = CasStore::new(&cache_root).unwrap();
        let stages = temp.path().join("stages");
        std::fs::create_dir(&stages).unwrap();
        let stage = stages.join("capture");

        assert!(stage_external_capture_results(&snapshot, &[], &cas, &stage).is_err());
        assert!(!stage.exists());
    }

    #[test]
    fn cas_failure_does_not_stage_an_action_result() {
        let temp = tempfile::tempdir().unwrap();
        let (snapshot, witness) = fixture(temp.path());
        let cache_root = temp.path().join("cache");
        let cas = CasStore::new(&cache_root).unwrap();
        let output = snapshot.key.output_root.join("a.o");
        let digest = content_digest_of_file(&output).unwrap();
        std::fs::write(
            cache_root.join("cas").join(&digest.to_hex()[..2]),
            b"blocked",
        )
        .unwrap();
        let stages = temp.path().join("stages");
        std::fs::create_dir(&stages).unwrap();
        let stage = stages.join("capture");

        assert!(stage_external_capture_results(&snapshot, &[witness], &cas, &stage).is_err());
        assert_eq!(std::fs::read_dir(stage).unwrap().count(), 0);
    }

    #[test]
    fn non_reusable_actions_are_deliberately_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let (mut snapshot, witness) = fixture(temp.path());
        snapshot.graph.actions[0].always_run = true;
        snapshot.validate_structure().unwrap();
        std::fs::remove_file(snapshot.key.output_root.join("a.o")).unwrap();
        let cache_root = temp.path().join("cache");
        let cas = CasStore::new(&cache_root).unwrap();
        let stages = temp.path().join("stages");
        std::fs::create_dir(&stages).unwrap();

        let adoption =
            stage_external_capture_results(&snapshot, &[witness], &cas, &stages.join("capture"))
                .unwrap();

        assert_eq!(adoption.adopted_action_count(), 0);
        assert_eq!(adoption.skipped_action_count(), 1);
        assert_eq!(adoption.stage().receipt().entry_count(), 0);
    }

    #[test]
    fn output_kind_mapping_is_exact() {
        assert!(output_kind_matches(
            ArtifactValueKind::RegularFile,
            ExternalOutputKind::File
        ));
        assert!(output_kind_matches(
            ArtifactValueKind::Symlink,
            ExternalOutputKind::Symlink
        ));
        assert!(output_kind_matches(
            ArtifactValueKind::Tree,
            ExternalOutputKind::Tree
        ));
        assert!(!output_kind_matches(
            ArtifactValueKind::Tree,
            ExternalOutputKind::File
        ));
    }
}
