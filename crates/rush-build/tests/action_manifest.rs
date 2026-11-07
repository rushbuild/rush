// Rush Build - External action manifest integration tests
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#![cfg(unix)]

use rush_action::action_manifest::{
    ActionManifestAction, ActionManifestActionV2, ActionManifestFinal, ActionManifestHeader,
    ActionManifestHeaderV2, ManifestCommand, ManifestDepfile, ManifestEnvPolicy,
    ManifestNamedEnvironment, ManifestOutput, ManifestOutputKind, ManifestPath, ManifestRecordKind,
    ManifestResourceClass, ManifestRoots, ManifestTypedPath, KBUILD_ACTION_MANIFEST_SCHEMA,
    KBUILD_ACTION_MANIFEST_VERSION, KBUILD_ACTION_MANIFEST_VERSION_2,
};
use rush_build::action_manifest::{
    execute_action_manifest, execute_imported_action_graph, load_action_manifest,
    ActionManifestError, ActionManifestErrorCode,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    src: PathBuf,
    obj: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        let obj = temp.path().join("obj");
        fs::create_dir(&src).unwrap();
        fs::create_dir(&obj).unwrap();
        Self { temp, src, obj }
    }

    fn write_manifest(&self, actions: &[ActionManifestAction]) -> PathBuf {
        self.write_manifest_with_preexisting(actions, Vec::new())
    }

    fn write_manifest_with_preexisting(
        &self,
        actions: &[ActionManifestAction],
        preexisting_obj_inputs: Vec<ManifestTypedPath>,
    ) -> PathBuf {
        let mut tools = BTreeMap::new();
        tools.insert("system".to_string(), PathBuf::from("/bin"));
        self.write_manifest_with_roots(
            actions,
            ManifestRoots {
                src: self.src.clone(),
                obj: self.obj.clone(),
                tools,
            },
            preexisting_obj_inputs,
        )
    }

    fn write_manifest_with_roots(
        &self,
        actions: &[ActionManifestAction],
        roots: ManifestRoots,
        preexisting_obj_inputs: Vec<ManifestTypedPath>,
    ) -> PathBuf {
        let header = ActionManifestHeader {
            record: ManifestRecordKind::Header,
            schema: KBUILD_ACTION_MANIFEST_SCHEMA.to_string(),
            version: KBUILD_ACTION_MANIFEST_VERSION,
            roots,
            preexisting_obj_inputs,
        };
        let final_record = ActionManifestFinal {
            record: ManifestRecordKind::Final,
            action_count: actions.len() as u64,
        };
        let mut lines = Vec::with_capacity(actions.len() + 2);
        lines.push(serde_json::to_string(&header).unwrap());
        lines.extend(
            actions
                .iter()
                .map(|action| serde_json::to_string(action).unwrap()),
        );
        lines.push(serde_json::to_string(&final_record).unwrap());

        let path = self.temp.path().join("actions.jsonl");
        fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }

    fn write_manifest_v2(
        &self,
        actions: &[ActionManifestActionV2],
        environments: Vec<ManifestNamedEnvironment>,
    ) -> PathBuf {
        let mut tools = BTreeMap::new();
        tools.insert("system".to_string(), PathBuf::from("/bin"));
        let header = ActionManifestHeaderV2 {
            record: ManifestRecordKind::Header,
            schema: KBUILD_ACTION_MANIFEST_SCHEMA.to_string(),
            version: KBUILD_ACTION_MANIFEST_VERSION_2,
            roots: ManifestRoots {
                src: self.src.clone(),
                obj: self.obj.clone(),
                tools,
            },
            preexisting_obj_inputs: Vec::new(),
            environments,
        };
        let final_record = ActionManifestFinal {
            record: ManifestRecordKind::Final,
            action_count: actions.len() as u64,
        };
        let mut lines = Vec::with_capacity(actions.len() + 2);
        lines.push(serde_json::to_string(&header).unwrap());
        lines.extend(
            actions
                .iter()
                .map(|action| serde_json::to_string(action).unwrap()),
        );
        lines.push(serde_json::to_string(&final_record).unwrap());
        let path = self.temp.path().join("actions-v2.jsonl");
        fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }

    fn replace_action_record(&self, path: &Path, action_json: String) {
        let contents = fs::read_to_string(path).unwrap();
        let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();
        assert_eq!(lines.len(), 3);
        lines[1] = action_json;
        fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
    }
}

fn rooted(root: &str, path: &str) -> ManifestPath {
    ManifestPath {
        root: root.to_string(),
        path: PathBuf::from(path),
    }
}

fn shell_program() -> ManifestPath {
    rooted("system", "sh")
}

fn shell_command(script: &str) -> ManifestCommand {
    ManifestCommand {
        program: shell_program(),
        argv: vec!["-c".to_string(), script.to_string()],
    }
}

fn shell_action(id: &str, script: &str, output: &str) -> ActionManifestAction {
    ActionManifestAction {
        record: ManifestRecordKind::Action,
        id: id.to_string(),
        mnemonic: "TestAction".to_string(),
        commands: vec![shell_command(script)],
        cwd: rooted("obj", "."),
        env: BTreeMap::new(),
        env_policy: ManifestEnvPolicy::InheritHost,
        normal_inputs: vec![shell_program()],
        order_only_inputs: Vec::new(),
        outputs: vec![ManifestOutput {
            path: rooted("obj", output),
            kind: ManifestOutputKind::File,
        }],
        resource: ManifestResourceClass::Other,
        depfile: None,
        config_deps: Vec::new(),
        restat: false,
        always_run: false,
        cacheable: false,
    }
}

fn shell_action_v2(id: &str, script: &str, output: &str, env_ref: &str) -> ActionManifestActionV2 {
    let action = shell_action(id, script, output);
    ActionManifestActionV2 {
        record: action.record,
        id: action.id,
        mnemonic: action.mnemonic,
        commands: action.commands,
        cwd: action.cwd,
        env_ref: env_ref.to_string(),
        env_policy: action.env_policy,
        normal_inputs: action.normal_inputs,
        order_only_inputs: action.order_only_inputs,
        outputs: action.outputs,
        resource: action.resource,
        depfile: action.depfile,
        config_deps: action.config_deps,
        restat: action.restat,
        always_run: action.always_run,
        cacheable: action.cacheable,
    }
}

fn assert_load_error(path: &Path, expected: ActionManifestErrorCode) {
    let error = load_error(path);
    assert_eq!(error.code(), expected, "unexpected diagnostic: {error}");
}

fn load_error(path: &Path) -> ActionManifestError {
    load_action_manifest(path).unwrap_err()
}

#[tokio::test]
async fn version_two_resolves_and_executes_named_environment() {
    let fixture = Fixture::new();
    let mut env = BTreeMap::new();
    env.insert("RUSH_MANIFEST_V2_VALUE".to_string(), "exact".to_string());
    let first = shell_action_v2(
        "v2",
        "test \"$RUSH_MANIFEST_V2_VALUE\" = exact && : > v2.out",
        "v2.out",
        "env-a",
    );
    let second = shell_action_v2(
        "v2-second",
        "test \"$RUSH_MANIFEST_V2_VALUE\" = exact && : > v2-second.out",
        "v2-second.out",
        "env-a",
    );
    let third = shell_action_v2(
        "v2-third",
        "test \"$RUSH_MANIFEST_V2_VALUE\" = distinct && : > v2-third.out",
        "v2-third.out",
        "env-b",
    );
    let mut distinct_env = BTreeMap::new();
    distinct_env.insert("RUSH_MANIFEST_V2_VALUE".to_string(), "distinct".to_string());
    let manifest = fixture.write_manifest_v2(
        &[first, second, third],
        vec![
            ManifestNamedEnvironment {
                id: "env-a".to_string(),
                env,
            },
            ManifestNamedEnvironment {
                id: "env-b".to_string(),
                env: distinct_env,
            },
        ],
    );

    let summary = execute_action_manifest(&manifest, 1).await.unwrap();
    assert_eq!(summary.actions_executed, 3);
    assert!(fixture.obj.join("v2.out").is_file());
    assert!(fixture.obj.join("v2-second.out").is_file());
    assert!(fixture.obj.join("v2-third.out").is_file());
}

#[test]
fn version_two_rejects_unknown_environment_reference() {
    let fixture = Fixture::new();
    let action = shell_action_v2("v2", ": > v2.out", "v2.out", "missing");
    let manifest = fixture.write_manifest_v2(&[action], Vec::new());
    let error = load_error(&manifest);
    assert_eq!(error.code(), ActionManifestErrorCode::InvalidAction);
    assert_eq!(error.diagnostic().line, Some(2));
    assert_eq!(error.diagnostic().action_id.as_deref(), Some("v2"));
    assert!(error
        .diagnostic()
        .message
        .contains("unknown environment reference \"missing\""));
}

#[test]
fn version_two_rejects_unused_environment_declaration() {
    let fixture = Fixture::new();
    let manifest = fixture.write_manifest_v2(
        &[],
        vec![
            ManifestNamedEnvironment {
                id: "unused-z".to_string(),
                env: BTreeMap::new(),
            },
            ManifestNamedEnvironment {
                id: "unused-a".to_string(),
                env: BTreeMap::new(),
            },
        ],
    );
    let error = load_error(&manifest);
    assert_eq!(error.code(), ActionManifestErrorCode::InvalidAction);
    assert_eq!(error.diagnostic().line, Some(1));
    assert_eq!(error.diagnostic().action_id, None);
    assert_eq!(
        error.diagnostic().message,
        "unused named environment \"unused-a\""
    );
}

#[test]
fn version_two_rejects_duplicate_environment_ids() {
    let fixture = Fixture::new();
    let action = shell_action_v2("v2", ": > v2.out", "v2.out", "env-a");
    let manifest = fixture.write_manifest_v2(
        &[action],
        vec![
            ManifestNamedEnvironment {
                id: "env-a".to_string(),
                env: BTreeMap::new(),
            },
            ManifestNamedEnvironment {
                id: "env-a".to_string(),
                env: BTreeMap::new(),
            },
        ],
    );
    let error = load_error(&manifest);
    assert_eq!(error.code(), ActionManifestErrorCode::InvalidAction);
    assert_eq!(error.diagnostic().line, Some(1));
    assert_eq!(error.diagnostic().action_id, None);
    assert_eq!(
        error.diagnostic().message,
        "duplicate named environment \"env-a\""
    );
}

#[test]
fn version_two_rejects_invalid_environment_ids() {
    for id in [String::new(), "env\nbad".to_string(), "x".repeat(4097)] {
        let fixture = Fixture::new();
        let action = shell_action_v2("v2", ": > v2.out", "v2.out", &id);
        let manifest = fixture.write_manifest_v2(
            &[action],
            vec![ManifestNamedEnvironment {
                id,
                env: BTreeMap::new(),
            }],
        );
        let error = load_error(&manifest);
        assert_eq!(error.code(), ActionManifestErrorCode::InvalidAction);
        assert_eq!(error.diagnostic().line, Some(1));
        assert_eq!(error.diagnostic().action_id, None);
        assert!(error.diagnostic().message.starts_with("environment id "));
    }
}

#[test]
fn version_two_rejects_invalid_environment_references() {
    for env_ref in [String::new(), "env\nbad".to_string(), "x".repeat(4097)] {
        let fixture = Fixture::new();
        let action = shell_action_v2("v2", ": > v2.out", "v2.out", &env_ref);
        let manifest = fixture.write_manifest_v2(
            &[action],
            vec![ManifestNamedEnvironment {
                id: "env-a".to_string(),
                env: BTreeMap::new(),
            }],
        );
        let error = load_error(&manifest);
        assert_eq!(error.code(), ActionManifestErrorCode::InvalidAction);
        assert_eq!(error.diagnostic().line, Some(2));
        assert_eq!(error.diagnostic().action_id.as_deref(), Some("v2"));
        assert!(error
            .diagnostic()
            .message
            .starts_with("environment reference "));
    }
}

#[test]
fn version_two_rejects_invalid_named_environment_contents() {
    let cases = [
        ("empty-key", "", "value", "invalid environment key"),
        ("equals-key", "BAD=KEY", "value", "invalid environment key"),
        ("nul-key", "BAD\0KEY", "value", "invalid environment key"),
        (
            "nul-value",
            "BAD_VALUE",
            "bad\0value",
            "environment value for \"BAD_VALUE\" contains NUL",
        ),
    ];

    for (id, key, value, expected_message) in cases {
        let fixture = Fixture::new();
        let action = shell_action_v2("v2", ": > v2.out", "v2.out", id);
        let mut env = BTreeMap::new();
        env.insert(key.to_string(), value.to_string());
        let manifest = fixture.write_manifest_v2(
            &[action],
            vec![ManifestNamedEnvironment {
                id: id.to_string(),
                env,
            }],
        );
        let error = load_error(&manifest);
        assert_eq!(error.code(), ActionManifestErrorCode::InvalidAction);
        assert_eq!(error.diagnostic().line, Some(1));
        assert_eq!(error.diagnostic().action_id, None);
        assert!(error
            .diagnostic()
            .message
            .starts_with(&format!("named environment {id:?}: ")));
        assert!(error.diagnostic().message.contains(expected_message));
    }
}

#[test]
fn version_two_rejects_oversized_named_environment() {
    let fixture = Fixture::new();
    let action = shell_action_v2("v2", ": > v2.out", "v2.out", "oversized");
    let env = (0..=16_384)
        .map(|index| (format!("KEY_{index:05}"), String::new()))
        .collect();
    let manifest = fixture.write_manifest_v2(
        &[action],
        vec![ManifestNamedEnvironment {
            id: "oversized".to_string(),
            env,
        }],
    );
    let error = load_error(&manifest);
    assert_eq!(error.code(), ActionManifestErrorCode::InvalidAction);
    assert_eq!(error.diagnostic().line, Some(1));
    assert_eq!(error.diagnostic().action_id, None);
    assert_eq!(
        error.diagnostic().message,
        "named environment \"oversized\": environment exceeds 16384 entries"
    );
}

#[test]
fn version_one_invalid_environment_retains_action_context() {
    let fixture = Fixture::new();
    let mut action = shell_action("v1", ": > v1.out", "v1.out");
    action.env.insert(String::new(), "value".to_string());
    let manifest = fixture.write_manifest(&[action]);
    let error = load_error(&manifest);
    assert_eq!(error.code(), ActionManifestErrorCode::InvalidAction);
    assert_eq!(error.diagnostic().line, Some(2));
    assert_eq!(error.diagnostic().action_id.as_deref(), Some("v1"));
    assert_eq!(error.diagnostic().message, "invalid environment key \"\"");
}

#[test]
fn version_one_rejects_version_two_action_shape() {
    let fixture = Fixture::new();
    let v1_action = shell_action("v1", ": > v1.out", "v1.out");
    let manifest = fixture.write_manifest(&[v1_action]);
    let v2_action = shell_action_v2("v2", ": > v2.out", "v2.out", "env-a");
    fixture.replace_action_record(&manifest, serde_json::to_string(&v2_action).unwrap());
    assert_load_error(&manifest, ActionManifestErrorCode::InvalidJson);
}

#[test]
fn version_two_rejects_version_one_action_shape() {
    let fixture = Fixture::new();
    let v2_action = shell_action_v2("v2", ": > v2.out", "v2.out", "env-a");
    let manifest = fixture.write_manifest_v2(
        &[v2_action],
        vec![ManifestNamedEnvironment {
            id: "env-a".to_string(),
            env: BTreeMap::new(),
        }],
    );
    let v1_action = shell_action("v1", ": > v1.out", "v1.out");
    fixture.replace_action_record(&manifest, serde_json::to_string(&v1_action).unwrap());
    assert_load_error(&manifest, ActionManifestErrorCode::InvalidJson);
}

#[test]
fn rejects_order_only_cycle() {
    let fixture = Fixture::new();
    let mut first = shell_action("first", ":", "first.out");
    first.order_only_inputs.push(rooted("obj", "second.out"));
    let mut second = shell_action("second", ":", "second.out");
    second.order_only_inputs.push(rooted("obj", "first.out"));

    let manifest = fixture.write_manifest(&[first, second]);
    assert_load_error(&manifest, ActionManifestErrorCode::Cycle);
}

#[test]
fn rejects_mixed_normal_and_order_only_cycle() {
    let fixture = Fixture::new();
    let mut first = shell_action("first", ":", "first.out");
    first.normal_inputs.push(rooted("obj", "second.out"));
    let mut second = shell_action("second", ":", "second.out");
    second.order_only_inputs.push(rooted("obj", "first.out"));

    let manifest = fixture.write_manifest(&[first, second]);
    assert_load_error(&manifest, ActionManifestErrorCode::Cycle);
}

#[test]
fn rejects_duplicate_output_producers() {
    let fixture = Fixture::new();
    let first = shell_action("first", ":", "shared.out");
    let second = shell_action("second", ":", "shared.out");

    let manifest = fixture.write_manifest(&[first, second]);
    assert_load_error(&manifest, ActionManifestErrorCode::DuplicateProducer);
}

#[test]
fn rejects_lexical_path_escape() {
    let fixture = Fixture::new();
    let action = shell_action("escape", ":", "../outside");

    let manifest = fixture.write_manifest(&[action]);
    assert_load_error(&manifest, ActionManifestErrorCode::PathEscape);
}

#[test]
fn rejects_symlink_path_escape() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let outside = fixture.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, fixture.obj.join("escape")).unwrap();
    let action = shell_action("escape", ":", "escape/result");

    let manifest = fixture.write_manifest(&[action]);
    assert_load_error(&manifest, ActionManifestErrorCode::PathEscape);
}

#[test]
fn rejects_contained_symlink_in_output_ancestry() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fs::create_dir(fixture.obj.join("real")).unwrap();
    symlink(fixture.obj.join("real"), fixture.obj.join("alias")).unwrap();
    let first = shell_action("alias", ":", "alias/shared.out");
    let second = shell_action("real", ":", "real/shared.out");

    let manifest = fixture.write_manifest(&[first, second]);
    assert_load_error(&manifest, ActionManifestErrorCode::PathEscape);
}

#[test]
fn rejects_preexisting_alias_of_an_output() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fs::create_dir(fixture.obj.join("real")).unwrap();
    fs::write(fixture.obj.join("real/bootstrap"), b"ready").unwrap();
    symlink(fixture.obj.join("real"), fixture.obj.join("alias")).unwrap();
    let preexisting = ManifestTypedPath {
        path: rooted("obj", "alias/bootstrap"),
        kind: ManifestOutputKind::File,
    };
    let output = shell_action("overwrite", ":", "real/bootstrap");

    let manifest = fixture.write_manifest_with_preexisting(&[output], vec![preexisting]);
    assert_load_error(&manifest, ActionManifestErrorCode::DuplicateProducer);
}

#[test]
fn rejects_file_output_as_another_outputs_ancestor() {
    let fixture = Fixture::new();
    let parent = shell_action("parent-file", ":", "generated");
    let nested = shell_action("nested-file", ":", "generated/result.out");

    let manifest = fixture.write_manifest(&[parent, nested]);
    assert_load_error(&manifest, ActionManifestErrorCode::OutputNamespaceConflict);
}

#[test]
fn requires_nested_output_to_order_on_directory_producer() {
    let fixture = Fixture::new();
    let mut directory = shell_action("directory", ":", "generated");
    directory.outputs[0].kind = ManifestOutputKind::Directory;
    let nested = shell_action("nested-file", ":", "generated/result.out");

    let manifest = fixture.write_manifest(&[directory, nested]);
    assert_load_error(&manifest, ActionManifestErrorCode::OutputNamespaceConflict);
}

#[test]
fn rejects_overlapping_source_and_output_roots() {
    let fixture = Fixture::new();
    let nested_obj = fixture.src.join("output");
    fs::create_dir(&nested_obj).unwrap();
    let mut tools = BTreeMap::new();
    tools.insert("system".to_string(), PathBuf::from("/bin"));
    let manifest = fixture.write_manifest_with_roots(
        &[],
        ManifestRoots {
            src: fixture.src.clone(),
            obj: nested_obj,
            tools,
        },
        Vec::new(),
    );

    assert_load_error(&manifest, ActionManifestErrorCode::InvalidRoot);
}

#[test]
fn rejects_tool_root_that_overlaps_the_output_root() {
    let fixture = Fixture::new();
    let mut tools = BTreeMap::new();
    tools.insert("system".to_string(), fixture.obj.clone());
    let manifest = fixture.write_manifest_with_roots(
        &[],
        ManifestRoots {
            src: fixture.src.clone(),
            obj: fixture.obj.clone(),
            tools,
        },
        Vec::new(),
    );

    assert_load_error(&manifest, ActionManifestErrorCode::InvalidRoot);
}

#[test]
fn rejects_non_normalized_absolute_root() {
    let fixture = Fixture::new();
    let mut tools = BTreeMap::new();
    tools.insert("system".to_string(), PathBuf::from("/bin"));
    let manifest = fixture.write_manifest_with_roots(
        &[],
        ManifestRoots {
            src: fixture.src.join("..").join("src"),
            obj: fixture.obj.clone(),
            tools,
        },
        Vec::new(),
    );

    assert_load_error(&manifest, ActionManifestErrorCode::InvalidRoot);
}

#[test]
fn rejects_symbolic_link_source_root() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let source_alias = fixture.temp.path().join("source-alias");
    symlink(&fixture.src, &source_alias).unwrap();
    let mut tools = BTreeMap::new();
    tools.insert("system".to_string(), PathBuf::from("/bin"));
    let manifest = fixture.write_manifest_with_roots(
        &[],
        ManifestRoots {
            src: source_alias,
            obj: fixture.obj.clone(),
            tools,
        },
        Vec::new(),
    );

    assert_load_error(&manifest, ActionManifestErrorCode::InvalidRoot);
}

#[test]
fn rejects_symbolic_link_output_root() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let output_alias = fixture.temp.path().join("output-alias");
    symlink(&fixture.obj, &output_alias).unwrap();
    let mut tools = BTreeMap::new();
    tools.insert("system".to_string(), PathBuf::from("/bin"));
    let manifest = fixture.write_manifest_with_roots(
        &[],
        ManifestRoots {
            src: fixture.src.clone(),
            obj: output_alias,
            tools,
        },
        Vec::new(),
    );

    assert_load_error(&manifest, ActionManifestErrorCode::InvalidRoot);
}

#[test]
fn rejects_control_character_in_tagged_path() {
    let fixture = Fixture::new();
    let action = shell_action("control-path", ":", "bad\nname");

    let manifest = fixture.write_manifest(&[action]);
    assert_load_error(&manifest, ActionManifestErrorCode::PathEscape);
}

#[test]
fn rejects_unlisted_preexisting_obj_input() {
    let fixture = Fixture::new();
    fs::write(fixture.obj.join("bootstrap.in"), b"ready\n").unwrap();
    let mut action = shell_action("consume", ":", "result.out");
    action.normal_inputs.push(rooted("obj", "bootstrap.in"));

    let manifest = fixture.write_manifest(&[action]);
    assert_load_error(&manifest, ActionManifestErrorCode::MissingProducer);
}

#[test]
fn rejects_preexisting_obj_input_with_wrong_kind() {
    let fixture = Fixture::new();
    fs::write(fixture.obj.join("bootstrap.in"), b"not a directory\n").unwrap();
    let preexisting = ManifestTypedPath {
        path: rooted("obj", "bootstrap.in"),
        kind: ManifestOutputKind::Directory,
    };

    let manifest = fixture.write_manifest_with_preexisting(&[], vec![preexisting]);
    assert_load_error(&manifest, ActionManifestErrorCode::MissingInput);
}

#[test]
fn accepts_declared_preexisting_obj_input() {
    let fixture = Fixture::new();
    fs::write(fixture.obj.join("bootstrap.in"), b"ready\n").unwrap();
    let preexisting = ManifestTypedPath {
        path: rooted("obj", "bootstrap.in"),
        kind: ManifestOutputKind::File,
    };
    let mut action = shell_action("consume", ":", "result.out");
    action.normal_inputs.push(rooted("obj", "bootstrap.in"));

    let manifest = fixture.write_manifest_with_preexisting(&[action], vec![preexisting]);
    load_action_manifest(&manifest).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn permits_unlisted_prepared_baseline_as_ambient_read_only_input() {
    let fixture = Fixture::new();
    fs::write(fixture.obj.join("ambient.in"), b"prepared\n").unwrap();
    let action = shell_action(
        "consume-ambient",
        "cat ambient.in > result.out",
        "result.out",
    );
    let manifest = fixture.write_manifest(&[action]);

    execute_action_manifest(&manifest, 1).await.unwrap();

    assert_eq!(
        fs::read(fixture.obj.join("ambient.in")).unwrap(),
        b"prepared\n"
    );
    assert_eq!(
        fs::read(fixture.obj.join("result.out")).unwrap(),
        b"prepared\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revalidates_preexisting_obj_input_after_acquiring_lock() {
    let fixture = Fixture::new();
    fs::write(fixture.obj.join("bootstrap.in"), b"ready\n").unwrap();
    let preexisting = ManifestTypedPath {
        path: rooted("obj", "bootstrap.in"),
        kind: ManifestOutputKind::File,
    };
    let mut action = shell_action("consume", ":", "result.out");
    action.normal_inputs.push(rooted("obj", "bootstrap.in"));
    let manifest = fixture.write_manifest_with_preexisting(&[action], vec![preexisting]);
    let graph = load_action_manifest(&manifest).unwrap();
    fs::remove_file(fixture.obj.join("bootstrap.in")).unwrap();

    let error = execute_imported_action_graph(graph, 1).await.unwrap_err();

    assert_eq!(error.code(), ActionManifestErrorCode::MissingInput);
    assert!(!fixture.obj.join("result.out").exists());
}

#[test]
fn rejects_output_that_collides_with_preexisting_obj_input() {
    let fixture = Fixture::new();
    fs::write(fixture.obj.join("bootstrap.in"), b"ready\n").unwrap();
    let preexisting = ManifestTypedPath {
        path: rooted("obj", "bootstrap.in"),
        kind: ManifestOutputKind::File,
    };
    let action = shell_action("replace", ":", "bootstrap.in");

    let manifest = fixture.write_manifest_with_preexisting(&[action], vec![preexisting]);
    assert_load_error(&manifest, ActionManifestErrorCode::DuplicateProducer);
}

#[test]
fn rejects_every_deferred_semantic_with_a_specific_code() {
    let fixture = Fixture::new();

    let mut depfile = shell_action("depfile", ":", "depfile.out");
    depfile.depfile = Some(ManifestDepfile {
        format: "kbuild_fixdep".to_string(),
        path: rooted("obj", "depfile.d"),
    });
    let manifest = fixture.write_manifest(&[depfile]);
    assert_load_error(&manifest, ActionManifestErrorCode::UnsupportedDepfile);

    let mut config = shell_action("config", ":", "config.out");
    config.config_deps.push("CONFIG_TEST".to_string());
    let manifest = fixture.write_manifest(&[config]);
    assert_load_error(&manifest, ActionManifestErrorCode::UnsupportedConfigDeps);

    let mut restat = shell_action("restat", ":", "restat.out");
    restat.restat = true;
    let manifest = fixture.write_manifest(&[restat]);
    assert_load_error(&manifest, ActionManifestErrorCode::UnsupportedRestat);

    let mut always = shell_action("always", ":", "always.out");
    always.always_run = true;
    let manifest = fixture.write_manifest(&[always]);
    assert_load_error(&manifest, ActionManifestErrorCode::UnsupportedAlwaysRun);

    let mut cache = shell_action("cache", ":", "cache.out");
    cache.cacheable = true;
    let manifest = fixture.write_manifest(&[cache]);
    assert_load_error(&manifest, ActionManifestErrorCode::UnsupportedCache);
}

#[test]
fn rejects_directory_membership_as_a_normal_value_input() {
    let fixture = Fixture::new();
    let mut directory = shell_action("mkdir", "mkdir generated", "generated");
    directory.outputs[0].kind = ManifestOutputKind::Directory;
    let mut consumer = shell_action("consume-tree", ":", "result.out");
    consumer.normal_inputs.push(rooted("obj", "generated"));

    let manifest = fixture.write_manifest(&[directory, consumer]);
    assert_load_error(
        &manifest,
        ActionManifestErrorCode::UnsupportedDirectoryMembership,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_directory_can_order_a_nested_working_directory() {
    let fixture = Fixture::new();
    let mut directory = shell_action("mkdir", "mkdir generated", "generated");
    directory.outputs[0].kind = ManifestOutputKind::Directory;
    let mut consumer = shell_action(
        "nested",
        "printf '%s' nested > result.out",
        "generated/result.out",
    );
    consumer.cwd = rooted("obj", "generated");
    consumer.order_only_inputs.push(rooted("obj", "generated"));

    let manifest = fixture.write_manifest(&[directory, consumer]);
    let summary = execute_action_manifest(&manifest, 2).await.unwrap();

    assert_eq!(summary.actions_executed, 2);
    assert_eq!(
        fs::read(fixture.obj.join("generated/result.out")).unwrap(),
        b"nested"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn schedules_normal_and_order_only_dependencies_after_producers() {
    let fixture = Fixture::new();
    let normal_producer = shell_action(
        "normal-producer",
        "sleep 0.15; printf '%s' normal > normal.ready",
        "normal.ready",
    );
    let mut normal_consumer = shell_action(
        "normal-consumer",
        "test -f normal.ready && printf '%s' consumed > normal.done",
        "normal.done",
    );
    normal_consumer
        .normal_inputs
        .push(rooted("obj", "normal.ready"));
    let order_producer = shell_action(
        "order-producer",
        "sleep 0.15; printf '%s' order > order.ready",
        "order.ready",
    );
    let mut order_consumer = shell_action(
        "order-consumer",
        "test -f order.ready && printf '%s' consumed > order.done",
        "order.done",
    );
    order_consumer
        .order_only_inputs
        .push(rooted("obj", "order.ready"));

    let manifest = fixture.write_manifest(&[
        normal_consumer,
        order_consumer,
        normal_producer,
        order_producer,
    ]);
    let summary = execute_action_manifest(&manifest, 4).await.unwrap();

    assert_eq!(summary.actions_executed, 4);
    assert_eq!(summary.commands_executed, 4);
    assert_eq!(
        fs::read(fixture.obj.join("normal.done")).unwrap(),
        b"consumed"
    );
    assert_eq!(
        fs::read(fixture.obj.join("order.done")).unwrap(),
        b"consumed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn jobs_is_a_hard_logical_action_admission_cap() {
    let fixture = Fixture::new();
    let first = shell_action(
        "first-slow",
        "sleep 0.2; printf first > first.out",
        "first.out",
    );
    let second = shell_action(
        "second-slow",
        "sleep 0.2; printf second > second.out",
        "second.out",
    );
    let manifest = fixture.write_manifest(&[first, second]);
    let start = Instant::now();

    let summary = execute_action_manifest(&manifest, 1).await.unwrap();

    assert_eq!(summary.actions_executed, 2);
    assert!(
        start.elapsed() >= Duration::from_millis(300),
        "-j1 admitted two sleeping actions concurrently"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn preserves_compound_shell_text_and_serial_command_order() {
    let fixture = Fixture::new();
    let mut action = shell_action(
        "exact-shell",
        r#"printf '%s\n' 'a b; $HOME "quoted"' > exact.out"#,
        "exact.out",
    );
    action.env_policy = ManifestEnvPolicy::ExplicitOnly;
    action.commands.push(shell_command(
        r#"printf '%s\n' 'second && untouched' >> exact.out"#,
    ));

    let manifest = fixture.write_manifest(&[action]);
    let summary = execute_action_manifest(&manifest, 2).await.unwrap();

    assert_eq!(summary.actions_executed, 1);
    assert_eq!(summary.commands_executed, 2);
    assert_eq!(
        fs::read(fixture.obj.join("exact.out")).unwrap(),
        b"a b; $HOME \"quoted\"\nsecond && untouched\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn preserves_distinct_and_repeated_environments_per_action() {
    let fixture = Fixture::new();
    let mut first = shell_action(
        "env-first",
        r#"printf '%s|%s|%s' "$ACTION_VALUE" "$EMPTY_VALUE" "${SECOND_ONLY-unset}" > first.env"#,
        "first.env",
    );
    first.env_policy = ManifestEnvPolicy::ExplicitOnly;
    first
        .env
        .insert("ACTION_VALUE".to_string(), "alpha".to_string());
    first.env.insert("EMPTY_VALUE".to_string(), String::new());

    let mut second = shell_action(
        "env-second",
        r#"printf '%s|%s|%s' "$ACTION_VALUE" "$EMPTY_VALUE" "${SECOND_ONLY-unset}" > second.env"#,
        "second.env",
    );
    second.env_policy = ManifestEnvPolicy::ExplicitOnly;
    second
        .env
        .insert("ACTION_VALUE".to_string(), "beta".to_string());
    second
        .env
        .insert("EMPTY_VALUE".to_string(), "populated".to_string());
    second
        .env
        .insert("SECOND_ONLY".to_string(), "yes".to_string());

    let mut repeated = first.clone();
    repeated.id = "env-repeated".to_string();
    repeated.commands = vec![shell_command(
        r#"printf '%s|%s|%s' "$ACTION_VALUE" "$EMPTY_VALUE" "${SECOND_ONLY-unset}" > repeated.env"#,
    )];
    repeated.outputs = vec![ManifestOutput {
        path: rooted("obj", "repeated.env"),
        kind: ManifestOutputKind::File,
    }];

    let manifest = fixture.write_manifest(&[first, second, repeated]);
    let summary = execute_action_manifest(&manifest, 3).await.unwrap();

    assert_eq!(summary.actions_executed, 3);
    assert_eq!(
        fs::read(fixture.obj.join("first.env")).unwrap(),
        b"alpha||unset"
    );
    assert_eq!(
        fs::read(fixture.obj.join("second.env")).unwrap(),
        b"beta|populated|yes"
    );
    assert_eq!(
        fs::read(fixture.obj.join("repeated.env")).unwrap(),
        b"alpha||unset"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn preserves_declared_tool_root_spelling_for_exec() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let fixture = Fixture::new();
    let real_tools = fixture.temp.path().join("real-tools");
    let declared_tools = fixture.temp.path().join("declared-tools");
    fs::create_dir(&real_tools).unwrap();
    fs::copy("/bin/sh", real_tools.join("runner")).unwrap();
    let mut permissions = fs::metadata(real_tools.join("runner"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(real_tools.join("runner"), permissions).unwrap();
    symlink(&real_tools, &declared_tools).unwrap();

    let mut action = shell_action(
        "declared-program",
        "printf '%s' \"$0\" > program-path.out",
        "program-path.out",
    );
    action.commands[0].program = rooted("system", "runner");
    action.normal_inputs = vec![rooted("system", "runner")];
    let mut tools = BTreeMap::new();
    tools.insert("system".to_string(), declared_tools.clone());
    let manifest = fixture.write_manifest_with_roots(
        &[action],
        ManifestRoots {
            src: fixture.src.clone(),
            obj: fixture.obj.clone(),
            tools,
        },
        Vec::new(),
    );

    execute_action_manifest(&manifest, 1).await.unwrap();

    assert_eq!(
        fs::read_to_string(fixture.obj.join("program-path.out")).unwrap(),
        declared_tools.join("runner").to_string_lossy()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_tool_root_alias_swapped_after_import() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let fixture = Fixture::new();
    let first_tools = fixture.temp.path().join("first-tools");
    let second_tools = fixture.temp.path().join("second-tools");
    let declared_tools = fixture.temp.path().join("declared-tools");
    for tools in [&first_tools, &second_tools] {
        fs::create_dir(tools).unwrap();
        fs::copy("/bin/sh", tools.join("runner")).unwrap();
        let mut permissions = fs::metadata(tools.join("runner")).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(tools.join("runner"), permissions).unwrap();
    }
    symlink(&first_tools, &declared_tools).unwrap();

    let mut action = shell_action(
        "swapped-program",
        "printf result > result.out",
        "result.out",
    );
    action.commands[0].program = rooted("system", "runner");
    action.normal_inputs = vec![rooted("system", "runner")];
    let mut tools = BTreeMap::new();
    tools.insert("system".to_string(), declared_tools.clone());
    let manifest = fixture.write_manifest_with_roots(
        &[action],
        ManifestRoots {
            src: fixture.src.clone(),
            obj: fixture.obj.clone(),
            tools,
        },
        Vec::new(),
    );
    let graph = load_action_manifest(&manifest).unwrap();
    fs::remove_file(&declared_tools).unwrap();
    symlink(&second_tools, &declared_tools).unwrap();

    let error = execute_imported_action_graph(graph, 1).await.unwrap_err();

    assert_eq!(error.code(), ActionManifestErrorCode::PathEscape);
    assert_eq!(
        error.diagnostic().action_id.as_deref(),
        Some("swapped-program")
    );
    assert!(!fixture.obj.join("result.out").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_tool_root_cwd_alias_swapped_after_import() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let first_cwd = fixture.temp.path().join("first-cwd");
    let second_cwd = fixture.temp.path().join("second-cwd");
    let declared_cwd = fixture.temp.path().join("declared-cwd");
    fs::create_dir(&first_cwd).unwrap();
    fs::create_dir(&second_cwd).unwrap();
    symlink(&first_cwd, &declared_cwd).unwrap();

    let mut action = shell_action("swapped-cwd", "printf result > \"$OUTPUT\"", "result.out");
    action.cwd = rooted("working", ".");
    action.env.insert(
        "OUTPUT".to_string(),
        fixture
            .obj
            .join("result.out")
            .to_string_lossy()
            .into_owned(),
    );
    let mut tools = BTreeMap::new();
    tools.insert("system".to_string(), PathBuf::from("/bin"));
    tools.insert("working".to_string(), declared_cwd.clone());
    let manifest = fixture.write_manifest_with_roots(
        &[action],
        ManifestRoots {
            src: fixture.src.clone(),
            obj: fixture.obj.clone(),
            tools,
        },
        Vec::new(),
    );
    let graph = load_action_manifest(&manifest).unwrap();
    fs::remove_file(&declared_cwd).unwrap();
    symlink(&second_cwd, &declared_cwd).unwrap();

    let error = execute_imported_action_graph(graph, 1).await.unwrap_err();

    assert_eq!(error.code(), ActionManifestErrorCode::PathEscape);
    assert_eq!(error.diagnostic().action_id.as_deref(), Some("swapped-cwd"));
    assert!(!fixture.obj.join("result.out").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_successful_command_that_omits_output() {
    let fixture = Fixture::new();
    let action = shell_action("missing", ":", "missing.out");
    let manifest = fixture.write_manifest(&[action]);

    let error = execute_action_manifest(&manifest, 1).await.unwrap_err();

    assert_eq!(error.code(), ActionManifestErrorCode::MissingOutput);
    assert_eq!(error.diagnostic().action_id.as_deref(), Some("missing"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_stale_output_before_starting_any_action() {
    let fixture = Fixture::new();
    fs::write(fixture.obj.join("stale.out"), b"must remain").unwrap();
    let action = shell_action("stale", ":", "stale.out");
    let manifest = fixture.write_manifest(&[action]);

    let error = execute_action_manifest(&manifest, 1).await.unwrap_err();

    assert_eq!(error.code(), ActionManifestErrorCode::OutputAlreadyExists);
    assert_eq!(
        fs::read(fixture.obj.join("stale.out")).unwrap(),
        b"must remain"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_output_precreated_by_an_earlier_directory_action() {
    let fixture = Fixture::new();
    let mut directory = shell_action(
        "directory",
        "mkdir generated; printf foreign > generated/result.out",
        "generated",
    );
    directory.outputs[0].kind = ManifestOutputKind::Directory;
    let mut child = shell_action("child", ":", "generated/result.out");
    child.order_only_inputs.push(rooted("obj", "generated"));
    let manifest = fixture.write_manifest(&[directory, child]);

    let error = execute_action_manifest(&manifest, 2).await.unwrap_err();

    assert_eq!(error.code(), ActionManifestErrorCode::OutputAlreadyExists);
    assert_eq!(error.diagnostic().action_id.as_deref(), Some("child"));
    assert_eq!(
        fs::read(fixture.obj.join("generated/result.out")).unwrap(),
        b"foreign"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rechecks_output_ancestry_after_the_command() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.obj.join("real")).unwrap();
    let action = shell_action(
        "late-symlink",
        "ln -s real alias; printf result > real/result.out",
        "alias/result.out",
    );
    let manifest = fixture.write_manifest(&[action]);

    let error = execute_action_manifest(&manifest, 1).await.unwrap_err();

    assert_eq!(error.code(), ActionManifestErrorCode::PathEscape);
    assert_eq!(
        error.diagnostic().action_id.as_deref(),
        Some("late-symlink")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_symlink_as_a_generated_directory_output() {
    let fixture = Fixture::new();
    let mut action = shell_action(
        "symlink-directory",
        "mkdir real; ln -s real generated",
        "generated",
    );
    action.outputs[0].kind = ManifestOutputKind::Directory;
    let manifest = fixture.write_manifest(&[action]);

    let error = execute_action_manifest(&manifest, 1).await.unwrap_err();

    assert_eq!(error.code(), ActionManifestErrorCode::WrongOutputKind);
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_output_with_wrong_filesystem_kind() {
    let fixture = Fixture::new();
    let mut action = shell_action("wrong-kind", ":", "wrong.out");
    let mkdir = rooted("system", "mkdir");
    action.commands = vec![ManifestCommand {
        program: mkdir.clone(),
        argv: vec!["wrong.out".to_string()],
    }];
    action.normal_inputs = vec![mkdir];
    action.env_policy = ManifestEnvPolicy::ExplicitOnly;
    let manifest = fixture.write_manifest(&[action]);

    let error = execute_action_manifest(&manifest, 1).await.unwrap_err();

    assert_eq!(error.code(), ActionManifestErrorCode::WrongOutputKind);
    assert_eq!(error.diagnostic().action_id.as_deref(), Some("wrong-kind"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_persistent_undeclared_sibling_file() {
    let fixture = Fixture::new();
    let action = shell_action(
        "side-file",
        "printf declared > result.out; printf side > .result.cmd",
        "result.out",
    );
    let manifest = fixture.write_manifest(&[action]);

    let error = execute_action_manifest(&manifest, 1).await.unwrap_err();

    assert_eq!(error.code(), ActionManifestErrorCode::UndeclaredObjWrite);
    assert_eq!(
        fs::read(fixture.obj.join("result.out")).unwrap(),
        b"declared"
    );
    assert_eq!(fs::read(fixture.obj.join(".result.cmd")).unwrap(), b"side");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_overwritten_bootstrap_input() {
    let fixture = Fixture::new();
    fs::write(fixture.obj.join("bootstrap.in"), b"original\n").unwrap();
    let preexisting = ManifestTypedPath {
        path: rooted("obj", "bootstrap.in"),
        kind: ManifestOutputKind::File,
    };
    let mut action = shell_action(
        "overwrite-bootstrap",
        "printf changed > bootstrap.in; printf result > result.out",
        "result.out",
    );
    action.normal_inputs.push(rooted("obj", "bootstrap.in"));
    let manifest = fixture.write_manifest_with_preexisting(&[action], vec![preexisting]);

    let error = execute_action_manifest(&manifest, 1).await.unwrap_err();

    assert_eq!(
        error.code(),
        ActionManifestErrorCode::PreexistingObjMutation
    );
    assert_eq!(
        fs::read(fixture.obj.join("bootstrap.in")).unwrap(),
        b"changed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_same_size_bootstrap_overwrite_with_restored_mtime() {
    let fixture = Fixture::new();
    fs::write(fixture.obj.join("bootstrap.in"), b"original\n").unwrap();
    let preexisting = ManifestTypedPath {
        path: rooted("obj", "bootstrap.in"),
        kind: ManifestOutputKind::File,
    };
    let mut action = shell_action(
        "overwrite-bootstrap-same-metadata",
        "stamp=$(stat -c '%y' bootstrap.in); printf 'changed!\\n' > bootstrap.in; touch -d \"$stamp\" bootstrap.in; printf result > result.out",
        "result.out",
    );
    action.normal_inputs.push(rooted("obj", "bootstrap.in"));
    let manifest = fixture.write_manifest_with_preexisting(&[action], vec![preexisting]);

    let error = execute_action_manifest(&manifest, 1).await.unwrap_err();

    assert_eq!(
        error.code(),
        ActionManifestErrorCode::PreexistingObjMutation
    );
    assert_eq!(
        fs::metadata(fixture.obj.join("bootstrap.in"))
            .unwrap()
            .len(),
        b"original\n".len() as u64
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_undeclared_empty_directory() {
    let fixture = Fixture::new();
    let action = shell_action(
        "extra-directory",
        "mkdir scratch; printf result > result.out",
        "result.out",
    );
    let manifest = fixture.write_manifest(&[action]);

    let error = execute_action_manifest(&manifest, 1).await.unwrap_err();

    assert_eq!(error.code(), ActionManifestErrorCode::UndeclaredObjWrite);
    assert!(fixture.obj.join("scratch").is_dir());
}

#[tokio::test(flavor = "multi_thread")]
async fn permits_new_directories_that_are_declared_output_ancestors() {
    let fixture = Fixture::new();
    let action = shell_action(
        "nested-output",
        "mkdir -p generated/deep; printf result > generated/deep/result.out",
        "generated/deep/result.out",
    );
    let manifest = fixture.write_manifest(&[action]);

    let summary = execute_action_manifest(&manifest, 1).await.unwrap();

    assert_eq!(summary.actions_executed, 1);
    assert_eq!(
        fs::read(fixture.obj.join("generated/deep/result.out")).unwrap(),
        b"result"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_action_never_starts_its_dependent() {
    let fixture = Fixture::new();
    let failing = shell_action("fail", "exit 7", "failed.out");
    let mut dependent = shell_action(
        "dependent",
        "printf '%s' started > dependent.out",
        "dependent.out",
    );
    dependent.normal_inputs.push(rooted("obj", "failed.out"));
    let manifest = fixture.write_manifest(&[dependent, failing]);

    let error = execute_action_manifest(&manifest, 2).await.unwrap_err();

    assert_eq!(error.code(), ActionManifestErrorCode::ExecutionFailed);
    assert_eq!(error.diagnostic().action_id.as_deref(), Some("fail"));
    assert!(!fixture.obj.join("dependent.out").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn failure_drains_running_action_without_admitting_ready_work() {
    let fixture = Fixture::new();
    let failing = shell_action("fail", "sleep 0.05; exit 9", "failed.out");
    let draining = shell_action(
        "already-running",
        "sleep 0.2; printf drained > draining.out",
        "draining.out",
    );
    let queued = shell_action(
        "ready-but-queued",
        "printf admitted > queued.out",
        "queued.out",
    );
    let manifest = fixture.write_manifest(&[failing, draining, queued]);

    let error = execute_action_manifest(&manifest, 2).await.unwrap_err();

    assert_eq!(error.code(), ActionManifestErrorCode::ExecutionFailed);
    assert_eq!(error.diagnostic().action_id.as_deref(), Some("fail"));
    assert_eq!(
        fs::read(fixture.obj.join("draining.out")).unwrap(),
        b"drained"
    );
    assert!(!fixture.obj.join("queued.out").exists());
}
