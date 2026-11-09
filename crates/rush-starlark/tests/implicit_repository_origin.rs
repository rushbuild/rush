//! Origin policy for explicitly registered sources versus Rush compatibility
//! adapters. These controls never invoke Bazel or fabricate fetched sources.
#![cfg(unix)]
use bytes::Bytes;
use rush_command_core::CommandSemantics;
use rush_incremental::{tracked, RealTrackedIo};
use rush_protocol::command::v1::RunRequest;
use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use rush_starlark::{BzlLoader, Value};
use std::path::Path;
use std::sync::Arc;

fn analysis(root: &Path, body: impl FnOnce()) {
    let request = RunRequest {
        auth_cookie: Bytes::new(),
        argv: vec![
            Bytes::from_static(b"rush"),
            Bytes::from_static(b"build"),
            Bytes::from_static(b"//:test"),
        ],
        working_directory: Bytes::from(root.to_str().unwrap().to_owned()),
        environment: vec![],
        block_for_lock: true,
        client_build_id: "implicit-origin-test".into(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        workspace_config: vec![],
    };
    let io = Arc::new(RealTrackedIo::new(
        CommandSemantics::from_run_request(&request).unwrap(),
        root,
    ));
    tracked::scope(io, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        body();
    });
}

#[test]
fn implicit_workspace_adapter_refuses_missing_analyzer_without_export_fallback() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("WORKSPACE"), "").unwrap();
    let adapter = root.path().join("adapter");
    std::fs::create_dir(&adapter).unwrap();
    std::fs::write(adapter.join("defs.bzl"), "VALUE = 99\n").unwrap();
    analysis(root.path(), || {
        let mut loader = BzlLoader::new(root.path().to_owned());
        loader.add_implicit_system_adapter("zlib".into(), adapter.clone());
        assert!(loader.is_implicit_system_adapter("zlib", &adapter));
        let mut clone = loader.clone_for_parallel_parsing();
        let error = clone
            .load_bzl(
                "@zlib//:defs.bzl",
                &[("VALUE".into(), "VALUE".into())],
                None,
            )
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("neither RUSH_BAZEL_PATH nor PATH"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read_to_string(adapter.join("defs.bzl")).unwrap(),
            "VALUE = 99\n"
        );
        // The same physical path can be explicitly registered by its owner.
        // It is the registration API, never a path/name heuristic, that grants
        // adapter origin. Clearing that origin is shared with loader clones.
        loader.add_external_repo("zlib".into(), adapter.clone());
        assert!(!clone.is_implicit_system_adapter("zlib", &adapter));
        let value = clone
            .load_bzl(
                "@zlib//:defs.bzl",
                &[("VALUE".into(), "VALUE".into())],
                None,
            )
            .unwrap();
        assert_eq!(value["VALUE"], Value::Int(99));
    });
}

#[test]
fn implicit_adapter_keeps_no_workspace_compatibility() {
    let root = tempfile::tempdir().unwrap();
    let adapter = root.path().join("adapter");
    std::fs::create_dir(&adapter).unwrap();
    std::fs::write(adapter.join("defs.bzl"), "VALUE = 42\n").unwrap();
    analysis(root.path(), || {
        let mut loader = BzlLoader::new(root.path().to_owned());
        loader.add_implicit_system_adapter("zlib".into(), adapter.clone());
        let value = loader
            .load_bzl(
                "@zlib//:defs.bzl",
                &[("VALUE".into(), "VALUE".into())],
                None,
            )
            .unwrap();
        assert_eq!(value["VALUE"], Value::Int(42));
    });
}

#[test]
fn automatically_created_repository_reenters_admission_when_workspace_appears() {
    let root = tempfile::tempdir().unwrap();
    let mut loader = BzlLoader::new(root.path().to_owned());
    analysis(root.path(), || {
        let exports = loader.load_bzl(
            "@local_config_remote_execution//:remote_execution.bzl",
            &[("gpu_test_tags".into(), "gpu_test_tags".into())], None,
        ).unwrap();
        assert!(matches!(exports["gpu_test_tags"], Value::Function { .. }));
        let path = loader.external_repo_path("local_config_remote_execution").unwrap();
        assert!(loader.is_implicit_system_adapter("local_config_remote_execution", &path));
    });
    std::fs::write(root.path().join("WORKSPACE"), "workspace(name = \"new_owner\")\n").unwrap();
    analysis(root.path(), || {
        loader.clear_cache();
        let error = loader.load_bzl(
            "@local_config_remote_execution//:remote_execution.bzl",
            &[("gpu_test_tags".into(), "gpu_test_tags".into())], None,
        ).unwrap_err();
        assert!(format!("{error:#}").contains("neither RUSH_BAZEL_PATH nor PATH"), "{error:#}");
    });
}

#[test]
fn managed_source_origin_is_shared_and_refuses_context_free_reuse() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("WORKSPACE"), "workspace(name = \"managed_origin\")\n").unwrap();
    let source = root.path().join("previous-command-cache/source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("defs.bzl"), "VALUE = 99\n").unwrap();
    analysis(root.path(), || {
        let loader = BzlLoader::new(root.path().to_owned());
        loader.add_managed_workspace_source("source".into(), source.clone());
        let mut clone = loader.clone_for_parallel_parsing();
        assert!(clone.is_managed_workspace_source("source", &source));
        let error = clone.load_bzl("@source//:defs.bzl", &[("VALUE".into(), "VALUE".into())], None).unwrap_err();
        assert!(format!("{error:#}").contains("neither RUSH_BAZEL_PATH nor PATH"), "{error:#}");
        assert_eq!(std::fs::read_to_string(source.join("defs.bzl")).unwrap(), "VALUE = 99\n");
        // An explicit owner registration of exactly that logical path can
        // reclassify it. Internal mirrors use the managed registration API.
        let mut explicit = loader;
        explicit.add_external_repo("source".into(), source.clone());
        assert!(!clone.is_managed_workspace_source("source", &source));
        let value = clone.load_bzl("@source//:defs.bzl", &[("VALUE".into(), "VALUE".into())], None).unwrap();
        assert_eq!(value["VALUE"], Value::Int(99));
    });
}

#[test]
fn explicit_source_under_backend_directory_is_not_inferred_managed() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("WORKSPACE"), "workspace(name = \"explicit_backend_path\")\n").unwrap();
    let source = root.path().join(".cache/rush/bazel_output_base/external/source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("defs.bzl"), "VALUE = 42\n").unwrap();
    analysis(root.path(), || {
        let mut loader = BzlLoader::new(root.path().to_owned());
        loader.add_external_repo("source".into(), source.clone());
        assert!(!loader.is_managed_workspace_source("source", &source));
        let value = loader.load_bzl("@source//:defs.bzl", &[("VALUE".into(), "VALUE".into())], None).unwrap();
        assert_eq!(value["VALUE"], Value::Int(42));
    });
}
