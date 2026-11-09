//! Run explicitly with real protoc, grpc_cpp_plugin, C++ headers/compiler,
//! GNU ar and nm. Set RUSH_PROTOC and RUSH_GRPC_CPP_PLUGIN to matching binaries;
//! select matching protobuf headers/runtime through RUSH_GLOBAL_CLONE_CACHE or
//! use the system installation with an empty task-specific clone-cache root.

use anyhow::{Context, Result};
use async_trait::async_trait;
use rush_action::ids::{ArtifactRegistry, ConfigId, TargetId};
use rush_action::{Action, ActionResult, Artifact as ActionArtifact};
use rush_rules::analysis_context::{AnalysisContext, IncludeResolver};
use rush_rules::context::{ActionManager, BuildContext, Toolchain};
use rush_rules::providers::LinkFragment;
use rush_rules::{BuildRule, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tensorflow_rules::TfProtoLibraryRule;

const PROTO: &str = r#"syntax = "proto3";
package rpc_fixture;
message Request { string message = 1; }
message Reply { string message = 1; }
service Echo { rpc Call(Request) returns (Reply); }
"#;

/// Execute each rule-emitted command and record its actual outputs. This
/// manager has no cache and never creates or substitutes output contents.
struct RecordingProcessExecutor {
    workspace: PathBuf,
    actions: Mutex<Vec<Action>>,
}

#[async_trait]
impl ActionManager for RecordingProcessExecutor {
    async fn execute(&self, action: Action) -> Result<ActionResult> {
        assert_eq!(action.id, Action::compute_id(&action.inputs, &action.command, &action.env)?);
        self.actions.lock().unwrap().push(action.clone());
        let started = Instant::now();
        let mut process = tokio::process::Command::new(&action.command.executable);
        process.args(&action.command.args).envs(&action.env)
            .current_dir(action.command.working_dir.as_ref().unwrap_or(&self.workspace))
            .kill_on_drop(true);
        let output = tokio::time::timeout(action.command.execution_info.timeout, process.output())
            .await.context("real rule command timed out")??;
        let mut output_records = Vec::new();
        if output.status.success() {
            for artifact in &action.outputs {
                let ActionArtifact::Source(path) = artifact else {
                    anyhow::bail!("unexpected derived output in this legacy rule test");
                };
                output_records.push(rush_action::artifact_value::record_output(path, None)?);
            }
        }
        Ok(ActionResult {
            action_id: action.id,
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
            outputs: action.outputs,
            output_records,
            duration: started.elapsed(),
            cached: false,
        })
    }
}

struct FixtureIncludeResolver(PathBuf);

impl IncludeResolver for FixtureIncludeResolver {
    fn resolve(&self, raw: &Path, package: &str, workspace: Option<&str>) -> PathBuf {
        assert!(workspace.is_none());
        if raw.is_absolute() { raw.to_path_buf() } else { self.0.join(package).join(raw) }
    }
}

async fn tool_output(tool: &str, arguments: &[&str], path: &Path) -> String {
    let output = tokio::process::Command::new(tool).args(arguments).arg(path)
        .output().await.unwrap_or_else(|error| panic!("cannot execute real {tool}: {error}"));
    assert!(output.status.success(), "{tool} failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

async fn check_case(has_services: bool, create_grpc_library: bool) {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    let output = temporary.path().join("out");
    std::fs::create_dir_all(workspace.join("pkg")).unwrap();
    std::fs::write(workspace.join("pkg/echo.proto"), PROTO).unwrap();
    let attributes = HashMap::from([
        ("name".to_string(), Value::String("echo_proto".to_string())),
        ("srcs".to_string(), Value::List(vec![Value::String("echo.proto".to_string())])),
        ("has_services".to_string(), Value::Bool(has_services)),
        ("create_grpc_library".to_string(), Value::Bool(create_grpc_library)),
        ("use_grpc_namespace".to_string(), Value::Bool(false)),
    ]);
    let rule = TfProtoLibraryRule.parse_attributes(&attributes).unwrap();
    let toolchain = Toolchain::default();
    let resolver = FixtureIncludeResolver(workspace.clone());
    let target = TargetId(1);
    let mut registry = ArtifactRegistry::new();
    let providers = {
        let mut context = AnalysisContext::new(target, ConfigId::DEFAULT, "pkg", None,
            &output, &mut registry, &resolver, &toolchain, &workspace, &[]);
        rule.analyze(&mut context, &[])
    };
    assert!(rule.scheduler_shape_unsupported().is_some());
    assert!(rule.scheduler_required_tools().contains(&rush_rules::RequiredTool::GrpcCppPlugin));
    let declared_outputs: Vec<_> = registry.actions_of(target).iter()
        .flat_map(|action| registry.declared_action(*action).outputs.iter())
        .map(|artifact| registry.artifact_path(*artifact).to_path_buf()).collect();
    for relative in ["genfiles/pkg/echo.grpc.pb.h", "genfiles/pkg/echo.grpc.pb.cc",
        "pkg/echo_proto/echo.grpc.pb.o", "pkg/echo_proto/libecho_proto.a"] {
        assert!(declared_outputs.contains(&output.join(relative)), "missing analyzed output {relative}: {declared_outputs:?}");
    }
    let cc_info = providers.get_cc_info().unwrap();
    let flags: Vec<_> = cc_info.linking.link_fragments.iter().filter_map(|fragment| {
        if let LinkFragment::Flag(flag) = fragment { Some(registry.strings().str(flag)) } else { None }
    }).collect();
    for flag in ["-lgrpc++", "-lgrpc", "-lgpr"] {
        assert!(flags.contains(&flag), "missing analyzed link flag {flag}: {flags:?}");
    }
    let executor = Arc::new(RecordingProcessExecutor {
        workspace: workspace.clone(), actions: Mutex::new(Vec::new()),
    });
    let context = BuildContext::new(workspace, output.clone())
        .with_package(PathBuf::from("pkg")).with_toolchain(toolchain)
        .with_materialize_static_libraries(true).with_action_manager(executor.clone());
    assert!(rule.build_legacy_command_specs(&context).await.is_empty());
    let artifacts = rule.build(&context).await.expect("real proto codegen, compile and archive");
    for relative in ["genfiles/pkg/echo.pb.h", "genfiles/pkg/echo.pb.cc",
        "genfiles/pkg/echo.grpc.pb.h", "genfiles/pkg/echo.grpc.pb.cc",
        "pkg/echo_proto/echo.grpc.pb.o", "pkg/echo_proto/libecho_proto.a"] {
        let path = output.join(relative);
        assert!(path.is_file(), "real tool did not create {relative}");
        assert!(artifacts.iter().any(|artifact| artifact.path == path), "build omitted {relative}");
    }
    let actions = executor.actions.lock().unwrap();
    let grpc_actions: Vec<_> = actions.iter().filter(|action| action.mnemonic == "GenGrpcProto").collect();
    assert_eq!(grpc_actions.len(), 1, "expected one real grpc generation command");
    let grpc_command = &grpc_actions[0].command;
    assert_eq!(grpc_command.executable, rush_rules::rules_system::protobuf_protoc_path());
    assert!(grpc_command.args.iter().any(|argument| argument.starts_with("--grpc_out=")));
    let plugin_argument = format!("--plugin=protoc-gen-grpc={}", std::env::var("RUSH_GRPC_CPP_PLUGIN").unwrap());
    assert!(grpc_command.args.contains(&plugin_argument));
    assert_eq!(actions.iter().filter(|action| action.mnemonic == "CppCompile").count(), 2);
    assert_eq!(actions.iter().filter(|action| action.mnemonic == "CppArchive").count(), 1);
    assert_eq!(grpc_command.args.iter().any(|argument| argument == "--grpc_opt=services_namespace=grpc"), create_grpc_library);
    drop(actions);

    let archive = output.join("pkg/echo_proto/libecho_proto.a");
    let members = tool_output("ar", &["t"], &archive).await;
    assert!(members.lines().any(|member| member.ends_with("echo.grpc.pb.o")), "grpc object absent from real archive: {members}");
    assert!(members.lines().any(|member| member.ends_with("echo.pb.o")), "message object absent from real archive: {members}");
    let symbols = tool_output("nm", &["-C", "--defined-only"], &archive).await;
    let namespace = if create_grpc_library { "rpc_fixture::grpc::Echo::Service::" } else { "rpc_fixture::Echo::Service::" };
    assert!(symbols.contains(namespace), "generated service namespace absent from compiled archive: {symbols}");
    if !create_grpc_library {
        assert!(!symbols.contains("rpc_fixture::grpc::Echo::Service::"), "has_services-only namespace changed");
    }
}

#[tokio::test]
#[ignore = "requires real matching protoc/grpc_cpp_plugin, protobuf+grpc++ headers, C++ compiler, GNU ar and nm; run explicitly with --ignored"]
async fn real_create_only_grpc_codegen_and_has_services_namespace_control() {
    for variable in ["RUSH_PROTOC", "RUSH_GRPC_CPP_PLUGIN"] {
        let executable = std::env::var_os(variable).unwrap_or_else(|| panic!("set {variable} to the real executable before opting in"));
        assert!(Path::new(&executable).is_absolute() && Path::new(&executable).is_file(), "{variable} must name an existing absolute executable");
    }
    check_case(false, true).await;
    check_case(true, false).await;
}
