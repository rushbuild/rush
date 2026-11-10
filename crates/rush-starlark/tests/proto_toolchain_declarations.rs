//! Native proto declarations stay declarations; they do not fabricate providers.
use rush_starlark::{parse, Evaluator, Value};

#[test]
fn native_proto_toolchain_records_all_explicit_attributes_and_aliases() {
    let root=tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(),|| {
        let mut evaluator=Evaluator::new();
        evaluator.register_native_namespace();
        evaluator.eval_program(&parse(r#"
ctor = native.proto_lang_toolchain
ctor(name="language", command_line="--custom_out=$(OUT)", output_files="multiple",
     plugin_format_flag="--plugin=protoc-gen-custom=%s", plugin="@tools//:plugin",
     runtime=":runtime", blacklisted_protos=["@proto//:well_known"],
     toolchain_type="//:type", mnemonic="CustomProto", progress_message="Generate %{label}")
proto_lang_toolchain(name="direct", command_line="--cpp_out=$(OUT)")
cc_library(name="unrelated")
"#).unwrap()).unwrap();
        let calls=evaluator.get_rule_calls();
        assert_eq!(calls.iter().map(|r|r.rule_type.as_str()).collect::<Vec<_>>(),["proto_lang_toolchain","proto_lang_toolchain","cc_library"]);
        assert_eq!(calls[0].kwargs["plugin"],Value::String("@tools//:plugin".into()));
        assert_eq!(calls[0].kwargs["runtime"],Value::String(":runtime".into()));
        assert_eq!(calls[0].kwargs["blacklisted_protos"],Value::List(vec![Value::String("@proto//:well_known".into())]));
        assert_eq!(calls[0].kwargs["command_line"],Value::String("--custom_out=$(OUT)".into()));
        assert_eq!(calls[0].kwargs["output_files"],Value::String("multiple".into()));
        assert_eq!(calls[0].kwargs["plugin_format_flag"],Value::String("--plugin=protoc-gen-custom=%s".into()));
        assert_eq!(calls[0].kwargs["toolchain_type"],Value::String("//:type".into()));
        assert_eq!(calls[0].kwargs["mnemonic"],Value::String("CustomProto".into()));
        assert_eq!(calls[0].kwargs["progress_message"],Value::String("Generate %{label}".into()));
        assert!(evaluator.get_var("ProtoLangToolchainInfo").is_none(),"a declaration must not fabricate a configured provider");
    });
}

#[cfg(unix)]
#[test]
fn analyzer_selection_tracks_negative_candidates_and_explicit_invalidity_without_running_tools() {
    use bytes::Bytes;
    use rush_command_core::CommandSemantics;
    use rush_incremental::{RealTrackedIo,tracked};
    use rush_protocol::command::v1::{EnvironmentVariable,RunRequest};
    use std::{os::unix::ffi::OsStrExt,os::unix::fs::PermissionsExt,sync::Arc};
    let root=tempfile::tempdir().unwrap();
    let run=|explicit:bool| {
        let value=if explicit {root.path().join("missing")} else {root.path().to_owned()};
        let request=RunRequest { argv:vec![Bytes::from_static(b"rush"),Bytes::from_static(b"build")],
            environment:vec![EnvironmentVariable {name:Bytes::from_static(if explicit {b"RUSH_BAZEL_PATH"} else {b"PATH"}),value:Bytes::copy_from_slice(value.as_os_str().as_bytes())}],
            protocol_major:rush_protocol::PROTOCOL_MAJOR,protocol_minor:rush_protocol::PROTOCOL_MINOR,..Default::default() };
        let io=Arc::new(RealTrackedIo::new(CommandSemantics::from_run_request(&request).unwrap(),root.path()));
        let result=tracked::scope(io.clone(),||rush_starlark::BzlLoader::selected_repository_analysis_binary(root.path()));
        (io,result)
    };
    let (io,none)=run(false);assert_eq!(none.unwrap(),None);
    let manifest=io.manifest();
    assert!(run(true).1.unwrap_err().to_string().contains("RUSH_BAZEL_PATH points to a missing"));
    let binary=root.path().join("bazel");
    // This file must never execute: detection observes identity/type only.
    std::fs::write(&binary,"#!/bin/sh\nexit 99\n").unwrap();
    std::fs::set_permissions(&binary,std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(run(false).1.unwrap(),Some(binary));
    use rush_incremental::TrackedIo;
    assert!(matches!(io.validate_manifest(&manifest).staleness,rush_incremental::Staleness::Stale {..}));
}

#[cfg(unix)]
#[test]
fn analyzer_symlink_target_creation_and_executable_mode_invalidate_native_selection() {
    use bytes::Bytes;
    use rush_command_core::CommandSemantics;
    use rush_incremental::{Dependency, RealTrackedIo, TrackedIo, tracked};
    use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
    use std::{os::unix::ffi::OsStrExt, os::unix::fs::{symlink, PermissionsExt}, sync::Arc};
    for missing in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("selected-tool");
        let candidate = root.path().join("bazel");
        symlink("selected-tool", &candidate).unwrap();
        if !missing {
            std::fs::write(&target, "#!/bin/sh\nexit 99\n").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let request = RunRequest {
            argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"build")],
            environment: vec![EnvironmentVariable { name: Bytes::from_static(b"PATH"),
                value: Bytes::copy_from_slice(root.path().as_os_str().as_bytes()) }],
            protocol_major: rush_protocol::PROTOCOL_MAJOR, protocol_minor: rush_protocol::PROTOCOL_MINOR,
            ..Default::default()
        };
        let semantics = CommandSemantics::from_run_request(&request).unwrap();
        let io = Arc::new(RealTrackedIo::new(semantics.clone(), root.path()));
        let selected = tracked::scope(io.clone(), || rush_starlark::BzlLoader::selected_repository_analysis_binary(root.path()).unwrap());
        assert_eq!(selected, None);
        let manifest = io.manifest();
        assert!(manifest.deps().iter().any(|dep| matches!(dep,
            Dependency::Symlink { path, target: Some(value) } if path == &candidate && value == std::path::Path::new("selected-tool"))));
        if missing {
            // Prove that the *failed* canonicalization retained the exact
            // unresolved target dependency; an unchanged link cannot mask it.
            assert!(manifest.deps().iter().any(|dep| matches!(dep,
                Dependency::FileExistence {path, exists:false, ..} if path == &target)));
        } else {
            assert!(manifest.deps().iter().any(|dep| matches!(dep,
                Dependency::FileMetadata {path, mode, ..} if path == &target && *mode & 0o111 == 0)));
        }
        assert!(matches!(io.validate_manifest(&manifest).staleness, rush_incremental::Staleness::Fresh));
        if missing { std::fs::write(&target, "#!/bin/sh\nexit 99\n").unwrap(); }
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(std::fs::read_link(&candidate).unwrap(), std::path::Path::new("selected-tool"));
        assert!(matches!(io.validate_manifest(&manifest).staleness, rush_incremental::Staleness::Stale {..}));
        let fresh = Arc::new(RealTrackedIo::new(semantics, root.path()));
        assert_eq!(tracked::scope(fresh, || rush_starlark::BzlLoader::selected_repository_analysis_binary(root.path()).unwrap()), Some(candidate));
    }
}
