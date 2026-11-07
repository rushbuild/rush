//! Canonical action identity for replay and captured external builds.
//!
//! Rush owns every static part of an external action key. An observer may
//! certify only the executable and input values that existed when the action
//! read them. Capture adoption and live replay both pass those values through
//! the same strict constructor before an [`ActionId`](rush_action::ActionId)
//! can be used as cache authority.

use crate::external_analysis_snapshot::{
    ExternalAction, ExternalActionGraph, ExternalAnalysisSnapshot, ExternalInputKind,
};
use anyhow::{Context, Result};
use rush_action::external_action_key::{
    ExternalActionKey, ExternalCommandKey, ExternalInputKey, ExternalInputValue, ExternalOutputKey,
    ExternalPathKey,
};
use rush_action::ActionId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Schema included in every Rush-owned external action key.
pub const EXTERNAL_ACTION_SCHEMA: &str = "org.rushbuild.external-action.v5";

/// Stable schema for one complete observer witness receipt.
pub const EXTERNAL_CAPTURE_WITNESS_RECEIPT_SCHEMA: &str =
    "org.rushbuild.external-capture-witness-receipt";

/// Current persisted witness receipt layout.
pub const EXTERNAL_CAPTURE_WITNESS_RECEIPT_VERSION: u32 = 1;

/// Read-time value of one command executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalExecutableWitness {
    /// Stable rooted executable path.
    pub path: ExternalPathKey,
    /// Exact file digest and executable bit observed for this command.
    pub value: ExternalInputValue,
}

/// Observer-certified read-time values for one captured action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalActionCaptureWitness {
    /// Exporter-local action identity matched against the captured graph.
    pub logical_id: String,
    /// One executable value for every command, in command order.
    pub executables: Vec<ExternalExecutableWitness>,
    /// Exact normal and virtual input set, in any order.
    pub inputs: Vec<ExternalInputKey>,
}

/// Versioned observer values bound to one exact serialized snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCaptureWitnessReceipt {
    schema: String,
    version: u32,
    snapshot_digest: [u8; 32],
    analysis_key_hash: [u8; 32],
    witness_digest: [u8; 32],
    witnesses: Vec<ExternalActionCaptureWitness>,
}

impl ExternalCaptureWitnessReceipt {
    /// Seal a complete witness set for exactly one captured snapshot.
    pub fn new(
        snapshot: &ExternalAnalysisSnapshot,
        witnesses: Vec<ExternalActionCaptureWitness>,
    ) -> Result<Self> {
        snapshot.validate_structure()?;
        certify_external_action_capture_witnesses(&snapshot.graph, &witnesses)?;
        Ok(Self {
            schema: EXTERNAL_CAPTURE_WITNESS_RECEIPT_SCHEMA.to_string(),
            version: EXTERNAL_CAPTURE_WITNESS_RECEIPT_VERSION,
            snapshot_digest: serialized_snapshot_digest(snapshot)?,
            analysis_key_hash: snapshot.key.stable_hash(),
            witness_digest: serialized_witness_digest(&witnesses)?,
            witnesses,
        })
    }

    /// Revalidate decoded receipt identity and exact action-value coverage.
    pub fn validate_for(&self, snapshot: &ExternalAnalysisSnapshot) -> Result<()> {
        if self.schema != EXTERNAL_CAPTURE_WITNESS_RECEIPT_SCHEMA
            || self.version != EXTERNAL_CAPTURE_WITNESS_RECEIPT_VERSION
        {
            anyhow::bail!("external capture witness receipt has an unsupported schema");
        }
        snapshot.validate_structure()?;
        if self.snapshot_digest != serialized_snapshot_digest(snapshot)?
            || self.analysis_key_hash != snapshot.key.stable_hash()
        {
            anyhow::bail!("external capture witness receipt belongs to a different snapshot");
        }
        if self.witness_digest != serialized_witness_digest(&self.witnesses)? {
            anyhow::bail!("external capture witness receipt content digest changed");
        }
        certify_external_action_capture_witnesses(&snapshot.graph, &self.witnesses)?;
        Ok(())
    }

    /// Exact observer values after receipt validation against a snapshot.
    pub fn witnesses(&self) -> &[ExternalActionCaptureWitness] {
        &self.witnesses
    }
}

fn serialized_snapshot_digest(snapshot: &ExternalAnalysisSnapshot) -> Result<[u8; 32]> {
    Ok(*blake3::hash(&bincode::serialize(snapshot)?).as_bytes())
}

fn serialized_witness_digest(witnesses: &[ExternalActionCaptureWitness]) -> Result<[u8; 32]> {
    Ok(*blake3::hash(&bincode::serialize(witnesses)?).as_bytes())
}

/// Construct the canonical key from one graph action and its read-time values.
///
/// The observer cannot supply commands, arguments, environment, outputs, or
/// execution properties. Those fields always come from Rush's validated
/// [`ExternalAction`]. Missing, extra, duplicate, misplaced, or wrongly shaped
/// observed values fail closed.
pub fn external_action_key_from_values(
    action: &ExternalAction,
    executables: &[ExternalExecutableWitness],
    inputs: &[ExternalInputKey],
) -> Result<ExternalActionKey> {
    if executables.len() != action.commands.len() {
        anyhow::bail!(
            "external action {:?} has {} executable witnesses for {} commands",
            action.logical_id,
            executables.len(),
            action.commands.len()
        );
    }

    let commands = action
        .commands
        .iter()
        .zip(executables)
        .enumerate()
        .map(|(index, (command, witness))| {
            if witness.path != command.executable {
                anyhow::bail!(
                    "external action {:?} executable witness {index} has the wrong path",
                    action.logical_id
                );
            }
            let (executable_digest, executable_bit) = match &witness.value {
                ExternalInputValue::File { digest, executable } => (*digest, *executable),
                _ => anyhow::bail!(
                    "external action {:?} executable witness {index} has the wrong shape",
                    action.logical_id
                ),
            };
            Ok(ExternalCommandKey {
                executable: command.executable.clone(),
                executable_digest,
                executable_bit,
                argv0: command.argv0.clone(),
                argv: command.argv.clone(),
                environment: command
                    .environment
                    .as_ref()
                    .unwrap_or(&action.environment)
                    .clone(),
                working_directory: command
                    .working_directory
                    .as_ref()
                    .unwrap_or(&action.working_directory)
                    .clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut observed = BTreeMap::new();
    for input in inputs {
        if observed
            .insert(input.path.clone(), input.value.clone())
            .is_some()
        {
            anyhow::bail!(
                "external action {:?} has a duplicate input witness",
                action.logical_id
            );
        }
    }

    let mut canonical_inputs = Vec::with_capacity(
        action
            .normal_inputs
            .len()
            .saturating_add(action.config_deps.len()),
    );
    for input in &action.normal_inputs {
        let value = observed.remove(&input.path).with_context(|| {
            format!(
                "external action {:?} is missing a normal input witness",
                action.logical_id
            )
        })?;
        if !input_value_matches_kind(&value, input.kind) {
            anyhow::bail!(
                "external action {:?} normal input witness has the wrong shape",
                action.logical_id
            );
        }
        canonical_inputs.push(ExternalInputKey {
            path: input.path.clone(),
            value,
        });
    }
    for symbol in &action.config_deps {
        let name = symbol.as_bytes().to_vec();
        let path = ExternalPathKey {
            root: "kconfig".to_string(),
            path: name.clone(),
        };
        let value = observed.remove(&path).with_context(|| {
            format!(
                "external action {:?} is missing a Kconfig input witness",
                action.logical_id
            )
        })?;
        match &value {
            ExternalInputValue::Virtual {
                name: observed_name,
                ..
            } if observed_name == &name => {}
            _ => anyhow::bail!(
                "external action {:?} Kconfig input witness has the wrong shape",
                action.logical_id
            ),
        }
        canonical_inputs.push(ExternalInputKey { path, value });
    }
    if !observed.is_empty() {
        anyhow::bail!(
            "external action {:?} has an extra input witness",
            action.logical_id
        );
    }

    let outputs = action
        .outputs
        .iter()
        .map(|output| ExternalOutputKey {
            path: output.path.clone(),
            kind: output.kind,
            mode: output.mode,
        })
        .collect();
    let mut execution_properties = action.execution_properties.clone();
    execution_properties.insert("rush.local_only".to_string(), action.local_only.to_string());
    execution_properties.insert(
        "rush.resource".to_string(),
        format!("{:?}", action.resource),
    );
    execution_properties.insert("rush.sandbox".to_string(), action.sandbox.to_string());
    if let Some(depfile) = &action.depfile {
        execution_properties.insert("rush.depfile.format".to_string(), depfile.format.clone());
    }

    Ok(ExternalActionKey {
        schema: EXTERNAL_ACTION_SCHEMA.to_string(),
        commands,
        working_directory: action.working_directory.clone(),
        signal_state: action.signal_state,
        file_creation_mask: action.file_creation_mask,
        credentials: action.credentials.clone(),
        inputs: canonical_inputs,
        outputs,
        execution_properties,
    })
}

/// Certify exact witness coverage and calculate every captured action ID.
///
/// No action ID is returned unless the witness collection contains exactly one
/// valid witness for every graph action and no unknown action.
pub fn certify_external_action_capture_witnesses(
    graph: &ExternalActionGraph,
    witnesses: &[ExternalActionCaptureWitness],
) -> Result<BTreeMap<String, ActionId>> {
    let mut by_id = BTreeMap::new();
    for witness in witnesses {
        if by_id.insert(witness.logical_id.as_str(), witness).is_some() {
            anyhow::bail!(
                "duplicate capture witness for external action {:?}",
                witness.logical_id
            );
        }
    }

    let mut certified = BTreeMap::new();
    for action in &graph.actions {
        let witness = by_id.remove(action.logical_id.as_str()).with_context(|| {
            format!(
                "missing capture witness for external action {:?}",
                action.logical_id
            )
        })?;
        let key = external_action_key_from_values(action, &witness.executables, &witness.inputs)?;
        certified.insert(action.logical_id.clone(), key.compute_id()?);
    }
    if let Some((logical_id, _)) = by_id.into_iter().next() {
        anyhow::bail!(
            "capture witness references unknown external action {:?}",
            logical_id
        );
    }
    Ok(certified)
}

fn input_value_matches_kind(value: &ExternalInputValue, kind: ExternalInputKind) -> bool {
    matches!(
        (value, kind),
        (ExternalInputValue::File { .. }, ExternalInputKind::File)
            | (
                ExternalInputValue::Symlink { .. },
                ExternalInputKind::Symlink
            )
            | (ExternalInputValue::Tree { .. }, ExternalInputKind::Tree)
            | (ExternalInputValue::Missing, ExternalInputKind::Missing)
            | (
                ExternalInputValue::DirectoryListing { .. },
                ExternalInputKind::DirectoryListing
            )
            | (
                ExternalInputValue::Directory { .. },
                ExternalInputKind::Directory
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_analysis_snapshot::{
        ExternalCommand, ExternalDepfile, ExternalInput, ExternalOutput,
    };
    use rush_action::action_manifest::ManifestResourceClass;
    use rush_action::external_action_key::{ExternalArgumentKey, ExternalOutputKind};

    fn path(root: &str, value: &str) -> ExternalPathKey {
        ExternalPathKey {
            root: root.to_string(),
            path: value.as_bytes().to_vec(),
        }
    }

    fn action(logical_id: &str) -> ExternalAction {
        ExternalAction {
            logical_id: logical_id.to_string(),
            mnemonic: "KernelCompile".to_string(),
            commands: vec![ExternalCommand {
                executable: path("toolchain", "bin/cc"),
                argv0: ExternalArgumentKey::Literal(b"cc".to_vec()),
                argv: vec![
                    ExternalArgumentKey::Literal(b"-c".to_vec()),
                    ExternalArgumentKey::Path(path("src", "a.c")),
                ],
                environment: None,
                working_directory: None,
            }],
            working_directory: path("obj", "."),
            environment: BTreeMap::new(),
            signal_state: Default::default(),
            file_creation_mask: 0o022,
            credentials: Default::default(),
            normal_inputs: vec![ExternalInput {
                path: path("src", "a.c"),
                kind: ExternalInputKind::File,
            }],
            order_only_inputs: Vec::new(),
            outputs: vec![ExternalOutput {
                path: path("obj", "a.o"),
                kind: ExternalOutputKind::File,
                mode: 0o644,
            }],
            dependencies: Vec::new(),
            config_deps: vec!["CONFIG_CC_OPTIMIZE_FOR_SIZE".to_string()],
            depfile: None,
            restat: true,
            always_run: false,
            cacheable: true,
            local_only: true,
            sandbox: false,
            resource: ManifestResourceClass::KernelCompile,
            execution_properties: BTreeMap::new(),
        }
    }

    fn witness(logical_id: &str) -> ExternalActionCaptureWitness {
        let symbol = b"CONFIG_CC_OPTIMIZE_FOR_SIZE".to_vec();
        ExternalActionCaptureWitness {
            logical_id: logical_id.to_string(),
            executables: vec![ExternalExecutableWitness {
                path: path("toolchain", "bin/cc"),
                value: ExternalInputValue::File {
                    digest: [3; 32],
                    executable: true,
                },
            }],
            inputs: vec![
                ExternalInputKey {
                    path: path("src", "a.c"),
                    value: ExternalInputValue::File {
                        digest: [5; 32],
                        executable: false,
                    },
                },
                ExternalInputKey {
                    path: ExternalPathKey {
                        root: "kconfig".to_string(),
                        path: symbol.clone(),
                    },
                    value: ExternalInputValue::Virtual {
                        name: symbol,
                        value: b"y".to_vec(),
                    },
                },
            ],
        }
    }

    fn graph(actions: Vec<ExternalAction>) -> ExternalActionGraph {
        ExternalActionGraph {
            roots: BTreeMap::new(),
            kconfig: None,
            initial_inputs: Vec::new(),
            empty_output_inputs: Vec::new(),
            actions,
            analysis_barriers: Vec::new(),
            requested_outputs: Vec::new(),
        }
    }

    fn id(action: &ExternalAction, witness: &ExternalActionCaptureWitness) -> ActionId {
        external_action_key_from_values(action, &witness.executables, &witness.inputs)
            .unwrap()
            .compute_id()
            .unwrap()
    }

    #[test]
    fn capture_witness_and_live_values_share_one_identity() {
        let action = action("compile");
        let witness = witness("compile");
        let capture_id = certify_external_action_capture_witnesses(
            &graph(vec![action.clone()]),
            std::slice::from_ref(&witness),
        )
        .unwrap()["compile"]
            .clone();
        let live_id =
            external_action_key_from_values(&action, &witness.executables, &witness.inputs)
                .unwrap()
                .compute_id()
                .unwrap();

        assert_eq!(capture_id, live_id);
    }

    #[test]
    fn read_time_value_remains_the_capture_identity() {
        let action = action("compile");
        let captured = witness("compile");
        let captured_id =
            external_action_key_from_values(&action, &captured.executables, &captured.inputs)
                .unwrap()
                .compute_id()
                .unwrap();
        let mut later_live = captured.clone();
        later_live.inputs[0].value = ExternalInputValue::File {
            digest: [9; 32],
            executable: false,
        };
        let later_id =
            external_action_key_from_values(&action, &later_live.executables, &later_live.inputs)
                .unwrap()
                .compute_id()
                .unwrap();

        assert_ne!(captured_id, later_id);
        assert_eq!(
            captured_id,
            external_action_key_from_values(&action, &captured.executables, &captured.inputs)
                .unwrap()
                .compute_id()
                .unwrap()
        );
    }

    #[test]
    fn witness_coverage_is_exact() {
        let first = action("first");
        let second = action("second");
        let graph = graph(vec![first, second]);

        assert!(
            certify_external_action_capture_witnesses(&graph, &[witness("first")])
                .unwrap_err()
                .to_string()
                .contains("missing capture witness")
        );
        assert!(certify_external_action_capture_witnesses(
            &graph,
            &[witness("first"), witness("second"), witness("unknown")]
        )
        .unwrap_err()
        .to_string()
        .contains("unknown external action"));
        assert!(certify_external_action_capture_witnesses(
            &graph,
            &[witness("first"), witness("first"), witness("second")]
        )
        .unwrap_err()
        .to_string()
        .contains("duplicate capture witness"));
    }

    #[test]
    fn malformed_action_witnesses_fail_closed() {
        let action = action("compile");

        let mut missing = witness("compile");
        missing.inputs.pop();
        assert!(
            external_action_key_from_values(&action, &missing.executables, &missing.inputs)
                .is_err()
        );

        let mut extra = witness("compile");
        extra.inputs.push(ExternalInputKey {
            path: path("src", "extra.h"),
            value: ExternalInputValue::Missing,
        });
        assert!(
            external_action_key_from_values(&action, &extra.executables, &extra.inputs).is_err()
        );

        let mut duplicate = witness("compile");
        duplicate.inputs.push(duplicate.inputs[0].clone());
        assert!(external_action_key_from_values(
            &action,
            &duplicate.executables,
            &duplicate.inputs
        )
        .is_err());

        let mut mistyped = witness("compile");
        mistyped.inputs[0].value = ExternalInputValue::Missing;
        assert!(
            external_action_key_from_values(&action, &mistyped.executables, &mistyped.inputs)
                .is_err()
        );

        let mut wrong_executable = witness("compile");
        wrong_executable.executables[0].value = ExternalInputValue::Missing;
        assert!(external_action_key_from_values(
            &action,
            &wrong_executable.executables,
            &wrong_executable.inputs
        )
        .is_err());

        let mut missing_executable = witness("compile");
        missing_executable.executables.clear();
        assert!(external_action_key_from_values(
            &action,
            &missing_executable.executables,
            &missing_executable.inputs
        )
        .is_err());

        let mut misplaced_executable = witness("compile");
        misplaced_executable.executables[0].path = path("toolchain", "bin/other-cc");
        assert!(external_action_key_from_values(
            &action,
            &misplaced_executable.executables,
            &misplaced_executable.inputs
        )
        .is_err());

        let mut wrong_virtual_name = witness("compile");
        let last = wrong_virtual_name.inputs.last_mut().unwrap();
        last.value = ExternalInputValue::Virtual {
            name: b"CONFIG_OTHER".to_vec(),
            value: b"y".to_vec(),
        };
        assert!(external_action_key_from_values(
            &action,
            &wrong_virtual_name.executables,
            &wrong_virtual_name.inputs
        )
        .is_err());
    }

    #[test]
    fn capture_witness_round_trips_losslessly() {
        let witness = witness("compile");
        let bytes = bincode::serialize(&witness).unwrap();
        let decoded: ExternalActionCaptureWitness = bincode::deserialize(&bytes).unwrap();

        assert_eq!(decoded, witness);
    }

    fn receipt_snapshot(goal: &[u8]) -> (ExternalAnalysisSnapshot, ExternalActionCaptureWitness) {
        let mut action = action("compile");
        action.config_deps.clear();
        let mut witness = witness("compile");
        witness.inputs.pop();
        let source = std::path::PathBuf::from("/source");
        let output = std::path::PathBuf::from("/output");
        let analyzer = crate::external_analysis_snapshot::ExternalAnalyzerIdentity {
            kind: "test-analyzer".to_string(),
            executable_digest: [8; 32],
            argv: vec![b"analyzer".to_vec(), goal.to_vec()],
            working_directory: source.clone(),
            environment: BTreeMap::new(),
            build_options: BTreeMap::new(),
            platform: BTreeMap::new(),
        };
        let key = crate::external_analysis_snapshot::ExternalAnalysisKey::new(
            source.clone(),
            output.clone(),
            &analyzer,
            vec![goal.to_vec()],
        );
        let snapshot = ExternalAnalysisSnapshot::new(
            key,
            analyzer,
            rush_incremental::DependencyManifest::from_deps(Vec::new()),
            ExternalActionGraph {
                roots: BTreeMap::from([
                    ("src".to_string(), source),
                    ("obj".to_string(), output),
                    (
                        "toolchain".to_string(),
                        std::path::PathBuf::from("/toolchain"),
                    ),
                ]),
                kconfig: None,
                initial_inputs: Vec::new(),
                empty_output_inputs: Vec::new(),
                actions: vec![action],
                analysis_barriers: Vec::new(),
                requested_outputs: vec![path("obj", "a.o")],
            },
        )
        .unwrap();
        (snapshot, witness)
    }

    #[test]
    fn witness_receipt_round_trips_and_remains_snapshot_bound() {
        let (snapshot, witness) = receipt_snapshot(b"all");
        let receipt = ExternalCaptureWitnessReceipt::new(&snapshot, vec![witness]).unwrap();
        let bytes = serde_json::to_vec(&receipt).unwrap();
        let decoded: ExternalCaptureWitnessReceipt = serde_json::from_slice(&bytes).unwrap();

        decoded.validate_for(&snapshot).unwrap();
        assert_eq!(decoded, receipt);

        let (substitute, _) = receipt_snapshot(b"other-goal");
        assert!(decoded.validate_for(&substitute).is_err());
    }

    #[test]
    fn witness_receipt_rejects_unknown_or_modified_content() {
        let (snapshot, witness) = receipt_snapshot(b"all");
        let receipt = ExternalCaptureWitnessReceipt::new(&snapshot, vec![witness]).unwrap();
        let mut encoded = serde_json::to_value(&receipt).unwrap();
        encoded["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ExternalCaptureWitnessReceipt>(encoded).is_err());

        let mut changed = receipt;
        changed.witnesses[0].executables[0].value = ExternalInputValue::File {
            digest: [9; 32],
            executable: true,
        };
        assert!(changed.validate_for(&snapshot).is_err());
    }

    #[test]
    fn rush_owned_static_fields_change_identity() {
        let baseline_action = action("compile");
        let witness = witness("compile");
        let baseline = id(&baseline_action, &witness);

        let mut changed = baseline_action.clone();
        changed.commands[0]
            .argv
            .push(ExternalArgumentKey::Literal(b"-g".to_vec()));
        assert_ne!(baseline, id(&changed, &witness));

        let mut changed = baseline_action.clone();
        changed.working_directory = path("obj", "subdir");
        assert_ne!(baseline, id(&changed, &witness));

        let mut explicit_default = baseline_action.clone();
        explicit_default.commands[0].working_directory = Some(path("obj", "."));
        assert_eq!(baseline, id(&explicit_default, &witness));

        let mut changed = baseline_action.clone();
        changed.commands[0].working_directory = Some(path("obj", "subdir"));
        assert_ne!(baseline, id(&changed, &witness));

        let mut changed = baseline_action.clone();
        changed.environment.insert(
            b"LANG".to_vec(),
            ExternalArgumentKey::Literal(b"C".to_vec()),
        );
        assert_ne!(baseline, id(&changed, &witness));

        let mut explicit_default = baseline_action.clone();
        explicit_default.commands[0].environment = Some(BTreeMap::new());
        assert_eq!(baseline, id(&explicit_default, &witness));

        let mut changed = baseline_action.clone();
        changed.commands[0].environment = Some(BTreeMap::from([(
            b"STEP".to_vec(),
            ExternalArgumentKey::Literal(b"second".to_vec()),
        )]));
        assert_ne!(baseline, id(&changed, &witness));

        let mut changed = baseline_action.clone();
        changed.file_creation_mask = 0o077;
        assert_ne!(baseline, id(&changed, &witness));

        let mut changed = baseline_action.clone();
        changed.credentials.no_new_privileges = true;
        assert_ne!(baseline, id(&changed, &witness));

        let mut changed = baseline_action.clone();
        changed.outputs[0].mode = 0o600;
        assert_ne!(baseline, id(&changed, &witness));

        let mut changed = baseline_action.clone();
        changed
            .execution_properties
            .insert("target".to_string(), "other".to_string());
        assert_ne!(baseline, id(&changed, &witness));

        let mut changed = baseline_action.clone();
        changed.local_only = false;
        assert_ne!(baseline, id(&changed, &witness));

        let mut changed = baseline_action.clone();
        changed.sandbox = true;
        assert_ne!(baseline, id(&changed, &witness));

        let mut changed = baseline_action.clone();
        changed.resource = ManifestResourceClass::Archive;
        assert_ne!(baseline, id(&changed, &witness));

        let mut changed = baseline_action;
        changed.depfile = Some(ExternalDepfile {
            format: "kbuild_fixdep".to_string(),
            path: path("obj", "a.o"),
        });

        assert_ne!(baseline, id(&changed, &witness));
    }
}
