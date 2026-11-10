//! Built-in repository source authority, without executing an analyzer.
#![cfg(unix)]
use bytes::Bytes;
use rush_command_core::CommandSemantics;
use rush_incremental::{tracked, RealTrackedIo};
use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use rush_starlark::{BzlLoader, Value};
use std::path::Path;
use std::sync::Arc;

fn analysis(root: &Path, analyzer: Option<&Path>, body: impl FnOnce()) {
    use std::os::unix::ffi::OsStrExt;
    let request = RunRequest {
        auth_cookie: Bytes::new(),
        argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"build"), Bytes::from_static(b"//:test")],
        working_directory: Bytes::copy_from_slice(root.as_os_str().as_bytes()),
        environment: analyzer.into_iter().map(|path| EnvironmentVariable {
            name: Bytes::from_static(b"RUSH_BAZEL_PATH"),
            value: Bytes::copy_from_slice(path.as_os_str().as_bytes()),
        }).collect(),
        block_for_lock: true, client_build_id: "builtin-tools-source-test".into(),
        protocol_major: PROTOCOL_MAJOR, protocol_minor: PROTOCOL_MINOR, workspace_config: vec![],
    };
    let io = Arc::new(RealTrackedIo::new(CommandSemantics::from_run_request(&request).unwrap(), root));
    tracked::scope(io, || {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let _entered = runtime.enter();
        body();
    });
}

#[test]
fn explicit_builtin_source_preserves_owner_authority_without_analyzer() {
    let root = tempfile::tempdir().unwrap();
    let supplied = root.path().join("explicit-source");
    std::fs::create_dir(&supplied).unwrap();
    std::fs::write(supplied.join("defs.bzl"), "VALUE = 42\n").unwrap();
    let mut loader = BzlLoader::new(root.path().to_owned());
    loader.add_implicit_system_adapter("bazel_tools".into(), supplied.clone());
    loader.add_external_repo("bazel_tools".into(), supplied.clone());
    analysis(root.path(), Some(&root.path().join("absent-analyzer")), || {
        assert_eq!(loader.admit_builtin_repository("bazel_tools", Some(&supplied)).unwrap(), Some(supplied.clone()));
        let exports = loader.load_bzl("@bazel_tools//:defs.bzl", &[("VALUE".into(), "VALUE".into())], None).unwrap();
        assert_eq!(exports["VALUE"], Value::Int(42));
    });
}

#[test]
fn automatic_builtin_without_analyzer_keeps_native_compatibility() {
    let root = tempfile::tempdir().unwrap();
    let compatibility = root.path().join("native-compatibility");
    std::fs::create_dir(&compatibility).unwrap();
    std::fs::write(compatibility.join("defs.bzl"), "VALUE = 41\n").unwrap();
    let mut loader = BzlLoader::new(root.path().to_owned());
    loader.add_implicit_system_adapter("bazel_tools".into(), compatibility.clone());
    analysis(root.path(), None, || {
        let exports = loader.load_bzl("@bazel_tools//:defs.bzl", &[("VALUE".into(), "VALUE".into())], None).unwrap();
        assert_eq!(exports["VALUE"], Value::Int(41));
    });
}

#[test]
fn selecting_invalid_analyzer_refuses_automatic_builtin_exports() {
    let root = tempfile::tempdir().unwrap();
    let compatibility = root.path().join("native-compatibility");
    std::fs::create_dir(&compatibility).unwrap();
    std::fs::write(compatibility.join("defs.bzl"), "VALUE = 41\n").unwrap();
    let mut loader = BzlLoader::new(root.path().to_owned());
    loader.add_implicit_system_adapter("bazel_tools".into(), compatibility.clone());
    analysis(root.path(), Some(&root.path().join("absent-analyzer")), || {
        let error = loader.load_bzl("@bazel_tools//:defs.bzl", &[("VALUE".into(), "VALUE".into())], None).unwrap_err();
        assert!(format!("{error:#}").contains("RUSH_BAZEL_PATH points to a missing"), "{error:#}");
        assert_eq!(loader.external_repo_path("bazel_tools"), Some(compatibility.clone()));
    });
}

#[test]
fn automatic_builtin_mirror_cannot_replace_active_or_explicit_source() {
    let root = tempfile::tempdir().unwrap();
    let stale = root.path().join("stale-automatic");
    let active = root.path().join("active-automatic");
    let explicit = root.path().join("explicit-source");
    let mut loader = BzlLoader::new(root.path().to_owned());
    loader.add_implicit_system_adapter("bazel_tools".into(), stale.clone());
    loader.add_implicit_system_adapter("bazel_tools".into(), active.clone());
    assert_eq!(loader.preferred_registered_repository_path("bazel_tools", Some(&stale)), Some(active));
    loader.add_external_repo("bazel_tools".into(), explicit.clone());
    assert_eq!(loader.preferred_registered_repository_path("bazel_tools", Some(&stale)), Some(explicit.clone()));
    let clone = loader.clone_for_parallel_parsing();
    assert_eq!(clone.external_repo_path("bazel_tools"), Some(explicit));
}
