//! Exercise the delegated analyzer API with real tracked client contexts.
//! Temporary executable fixtures test protocol/freshness behavior; they do not
//! certify a Bazel release or claim that Rush implements Bazel's BUILD semantics.
#![cfg(unix)]

use bytes::Bytes;
use rush_command_core::CommandSemantics;
use rush_incremental::{
    tracked, Dependency, DependencyManifest, RealTrackedIo, Staleness, TrackedIo,
};
use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use rush_starlark::{parse, BzlLoader, Evaluator, GlobalBzlCache, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier, Mutex};

fn analysis<R>(
    root: &Path,
    environment: &[(&str, String)],
    body: impl FnOnce() -> R,
) -> (R, DependencyManifest, CommandSemantics) {
    let request = RunRequest {
        auth_cookie: Bytes::new(),
        argv: vec![
            Bytes::from_static(b"rush"),
            Bytes::from_static(b"build"),
            Bytes::from_static(b"//:test"),
        ],
        working_directory: Bytes::from(root.to_str().unwrap().to_string()),
        environment: environment
            .iter()
            .map(|(name, value)| EnvironmentVariable {
                name: Bytes::from(name.to_string()),
                value: Bytes::from(value.clone()),
            })
            .collect(),
        block_for_lock: true,
        client_build_id: "workspace-analyzer-version-test".into(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        workspace_config: vec![],
    };
    let semantics = CommandSemantics::from_run_request(&request).unwrap();
    let io = Arc::new(RealTrackedIo::new(semantics.clone(), root));
    let result = tracked::scope(io.clone(), || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        body()
    });
    (result, io.manifest(), semantics)
}

fn tool(root: &Path, name: &str, body: &str) -> PathBuf {
    let path = root.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --version ] || exit 91\n{body}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn environment(tool: &Path, version: &str) -> Vec<(&'static str, String)> {
    vec![
        ("RUSH_BAZEL_PATH", tool.to_str().unwrap().into()),
        ("ANALYZER_VERSION", version.into()),
    ]
}

fn loader(root: &Path, global: Option<GlobalBzlCache>, workspace: bool) -> Arc<Mutex<BzlLoader>> {
    let mut loader = BzlLoader::new_with_all(root.to_path_buf(), None, global);
    loader.set_workspace_repository_context(workspace.then(|| root.to_path_buf()));
    Arc::new(Mutex::new(loader))
}

fn evaluate(
    loader: Option<&Arc<Mutex<BzlLoader>>>,
    root: &Path,
    workspace: bool,
    source: &str,
) -> anyhow::Result<Value> {
    let mut evaluator = Evaluator::new();
    evaluator.register_native_namespace();
    if workspace {
        evaluator.set_workspace_repository_context(root.to_path_buf());
    }
    if let Some(loader) = loader {
        evaluator.set_bzl_loader(loader.clone());
    }
    evaluator.eval_program(&parse(source)?)?;
    Ok(evaluator.get_var("result").unwrap())
}

#[test]
fn workspace_api_is_lazy_and_build_namespace_has_no_version() {
    let root = tempfile::tempdir().unwrap();
    analysis(root.path(), &[], || {
        assert_eq!(evaluate(None, root.path(), true,
            "alias = native\nresult = [hasattr(alias, \"bazel_version\"), \"bazel_version\" in dir(alias), getattr(alias, \"missing\", \"fallback\")]\n").unwrap(),
            Value::List(vec![Value::Bool(true), Value::Bool(true), Value::String("fallback".into())]));
        assert_eq!(evaluate(None, root.path(), false,
            "alias = native\nresult = [hasattr(alias, \"bazel_version\"), \"bazel_version\" in dir(alias), getattr(alias, \"bazel_version\", \"absent\")]\n").unwrap(),
            Value::List(vec![Value::Bool(false), Value::Bool(false), Value::String("absent".into())]));
        let error =
            evaluate(None, root.path(), true, "result = native.bazel_version\n").unwrap_err();
        assert!(format!("{error:#}").contains("neither RUSH_BAZEL_PATH nor PATH"));
    });
}

#[test]
fn imported_macros_namespace_aliases_and_getattr_read_actual_client_tool() {
    let root = tempfile::tempdir().unwrap();
    let analyzer = tool(
        root.path(),
        "analyzer",
        "printf 'bazel %s\\n' \"$ANALYZER_VERSION\"",
    );
    std::fs::write(root.path().join("WORKSPACE"), "").unwrap();
    std::fs::write(root.path().join("defs.bzl"),
        "alias = native\ndef read():\n    return [native.bazel_version, alias.bazel_version, getattr(alias, \"bazel_version\"), type(alias)]\n").unwrap();
    std::fs::write(
        root.path().join("bridge.bzl"),
        "load(\"@@//:defs.bzl\", _read = \"read\")\nread = _read\n",
    )
    .unwrap();
    let loader = loader(root.path(), Some(GlobalBzlCache::new()), true);
    let env = environment(&analyzer, "10.0.0-pre.20261023.1");
    let (value, manifest, _) = analysis(root.path(), &env, || {
        evaluate(
            Some(&loader),
            root.path(),
            true,
            "load(\"@@//:bridge.bzl\", \"read\")\nresult = read()\n",
        )
        .unwrap()
    });
    assert_eq!(
        value,
        Value::List(
            vec![Value::String("10.0.0-pre.20261023.1".into()); 3]
                .into_iter()
                .chain([Value::String("namespace".into())])
                .collect()
        )
    );
    assert!(manifest.deps().iter().any(|dependency| matches!(dependency,
        Dependency::ProcessProbe { program, args, .. } if program == &analyzer && args == &[b"--version".to_vec()])));
    assert!(manifest.deps().iter().any(|dependency| matches!(dependency,
        Dependency::FileContent { path, .. } if path == &analyzer)));
}

#[test]
fn local_and_global_caches_keep_workspace_and_build_module_phases_distinct() {
    for global in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("WORKSPACE"), "").unwrap();
        std::fs::write(
            root.path().join("phase.bzl"),
            "phase = hasattr(native, \"bazel_version\")\ndef read():\n    return phase\n",
        )
        .unwrap();
        let loader = loader(root.path(), global.then(GlobalBzlCache::new), false);
        analysis(root.path(), &[], || {
            for _ in 0..3 {
                for workspace in [false, true] {
                    loader.lock().unwrap().set_workspace_repository_context(
                        workspace.then(|| root.path().to_path_buf()),
                    );
                    let value = evaluate(
                        Some(&loader),
                        root.path(),
                        workspace,
                        "load(\"@@//:phase.bzl\", \"read\")\nresult = read()\n",
                    )
                    .unwrap();
                    assert_eq!(value, Value::Bool(workspace));
                }
            }
        });
        assert_eq!(loader.lock().unwrap().cache_stats().symbol_cache_entries, 2);
    }
}

#[test]
fn cached_top_level_version_invalidates_on_client_environment_and_tool_changes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("WORKSPACE"), "").unwrap();
    std::fs::write(
        root.path().join("version.bzl"),
        "version = native.bazel_version\n",
    )
    .unwrap();
    let analyzer = tool(
        root.path(),
        "analyzer",
        "printf 'bazel %s\\n' \"$ANALYZER_VERSION\"",
    );
    let loader = loader(root.path(), Some(GlobalBzlCache::new()), true);
    let source = "load(\"@@//:version.bzl\", \"version\")\nresult = version\n";
    let first_env = environment(&analyzer, "5.9.0");
    let (first, manifest, _) = analysis(root.path(), &first_env, || {
        evaluate(Some(&loader), root.path(), true, source).unwrap()
    });
    assert_eq!(first, Value::String("5.9.0".into()));
    let next_env = environment(&analyzer, "10.0.0-pre.20261023.1");
    let (next, next_manifest, semantics) = analysis(root.path(), &next_env, || {
        evaluate(Some(&loader), root.path(), true, source).unwrap()
    });
    assert_eq!(next, Value::String("10.0.0-pre.20261023.1".into()));
    assert!(matches!(
        manifest.validate(&semantics).staleness,
        Staleness::Stale { .. }
    ));
    tool(root.path(), "analyzer", "printf 'bazel 7.2.1rc3\\n'");
    assert!(matches!(
        next_manifest.validate(&semantics).staleness,
        Staleness::Stale { .. }
    ));
    let (edited, _, _) = analysis(root.path(), &next_env, || {
        evaluate(Some(&loader), root.path(), true, source).unwrap()
    });
    assert_eq!(edited, Value::String("7.2.1rc3".into()));
}

#[test]
fn path_selected_analyzer_and_optional_version_inputs_are_tracked() {
    let root = tempfile::tempdir().unwrap();
    tool(root.path(), "bazel", "printf 'bazel 8.1.0\\n'");
    let env = vec![("PATH", root.path().to_str().unwrap().to_string())];
    let (version, manifest, semantics) = analysis(root.path(), &env, || {
        evaluate(None, root.path(), true, "result = native.bazel_version\n").unwrap()
    });
    assert_eq!(version, Value::String("8.1.0".into()));
    std::fs::write(root.path().join(".bazelversion"), "8.1.0\n").unwrap();
    assert!(matches!(
        manifest.validate(&semantics).staleness,
        Staleness::Stale { .. }
    ));
}

#[test]
fn workspace_namespace_stays_nonserializable_and_is_not_a_rule_attribute() {
    let root = tempfile::tempdir().unwrap();
    analysis(root.path(), &[], || {
        let value = evaluate(None, root.path(), true, "result = native\n").unwrap();
        assert_eq!(value.to_string(), "<namespace>");
        assert!(rush_starlark::starlark_to_rule_value(&value).is_err());
        assert!(evaluate(None, root.path(), true, "result = json.encode(native)\n").is_err());
        let other = evaluate(None, root.path(), true, "result = native\n").unwrap();
        assert_eq!(value, other);
        let other_root = root.path().join("different-context");
        let different = evaluate(None, &other_root, true, "result = native\n").unwrap();
        assert_ne!(value, different);
        assert!(value.estimate_size() > 0);
    });
}

#[test]
fn measured_release_and_prerelease_versions_keep_their_exact_values() {
    let root = tempfile::tempdir().unwrap();
    let analyzer = tool(
        root.path(),
        "analyzer",
        "printf 'bazel %s\\n' \"$ANALYZER_VERSION\"",
    );
    for version in [
        "6.5.0",
        "7.2.1rc3",
        "10.0.0-pre.20261023.1",
        "10.0.0-pre.20261023.1+build.7",
    ] {
        let env = environment(&analyzer, version);
        let (actual, _, _) = analysis(root.path(), &env, || {
            evaluate(None, root.path(), true, "result = native.bazel_version\n").unwrap()
        });
        assert_eq!(actual, Value::String(version.into()));
    }
}

#[test]
fn version_failures_remain_errors_even_with_getattr_default() {
    let root = tempfile::tempdir().unwrap();
    for body in [
        "printf 'bazel unknown\\n'",
        "printf 'bazel 6.5.0\\nbazel 7.0.0\\n'",
        "exit 7",
    ] {
        let analyzer = tool(root.path(), "analyzer", body);
        let env = environment(&analyzer, "unused");
        analysis(root.path(), &env, || {
            for expression in [
                "native.bazel_version",
                "getattr(native, \"bazel_version\", \"fallback\")",
            ] {
                assert!(
                    evaluate(None, root.path(), true, &format!("result = {expression}\n")).is_err()
                );
            }
        });
    }
}

#[test]
fn concurrent_phase_loads_do_not_share_waits_or_placeholders() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("WORKSPACE"), "").unwrap();
    std::fs::write(
        root.path().join("phase.bzl"),
        "phase = hasattr(native, \"bazel_version\")\n",
    )
    .unwrap();
    let cache = GlobalBzlCache::new();
    let start = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for workspace in [false, true] {
        let root = root.path().to_path_buf();
        let loader = loader(&root, Some(cache.clone()), workspace);
        let start = start.clone();
        workers.push(std::thread::spawn(move || {
            analysis(&root, &[], || {
                start.wait();
                for _ in 0..3 {
                    assert_eq!(
                        evaluate(
                            Some(&loader),
                            &root,
                            workspace,
                            "load(\"@@//:phase.bzl\", \"phase\")\nresult = phase\n"
                        )
                        .unwrap(),
                        Value::Bool(workspace)
                    );
                }
            })
        }));
    }
    start.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(cache.cache_stats().symbol_cache_entries, 2);
}
