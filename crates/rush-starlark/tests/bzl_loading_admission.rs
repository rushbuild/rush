#![cfg(target_os = "linux")]

use bytes::Bytes;
use rush_command_core::CommandSemantics;
use rush_incremental::{tracked, RealTrackedIo};
use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use rush_starlark::{parse, BzlLoader, Evaluator, GlobalBzlCache, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn analysis<R>(
    root: &Path,
    environment: &[(&str, &str)],
    runtime: u8,
    body: impl FnOnce() -> R,
) -> R {
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
                value: Bytes::from(value.to_string()),
            })
            .collect(),
        block_for_lock: true,
        client_build_id: "bzl-loading-admission-test".into(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        workspace_config: vec![],
    };
    let io = Arc::new(RealTrackedIo::new(
        CommandSemantics::from_run_request(&request).unwrap(),
        root,
    ));
    let runtime = match runtime {
        0 => None,
        1 => Some(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        ),
        2 => Some(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap(),
        ),
        _ => panic!("invalid test runtime"),
    };
    let _entered = runtime.as_ref().map(|runtime| runtime.enter());
    tracked::scope(io, body)
}

fn evaluator(root: &Path) -> Evaluator {
    let mut evaluator = Evaluator::new();
    evaluator.set_bzl_loader(Arc::new(Mutex::new(BzlLoader::new_with_global_cache(
        root.to_path_buf(),
        GlobalBzlCache::with_limits(1000, 64 * 1024 * 1024),
    ))));
    evaluator
}

fn modules(root: &Path, count: usize) -> String {
    std::fs::write(root.join("WORKSPACE"), "").unwrap();
    let mut source = String::new();
    for index in 0..count {
        let name = format!("module_{index}.bzl");
        std::fs::write(root.join(&name), format!("value = {index}\n")).unwrap();
        source.push_str(&format!(
            "load(\"@@//:{name}\", value_{index} = \"value\")\n"
        ));
    }
    source.push_str(&format!(
        "result = [{}]\n",
        (0..count)
            .map(|index| format!("value_{index}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    source
}

#[test]
fn measured_admission_loads_more_than_old_level_and_total_count_limits() {
    for runtime in [0, 1, 2] {
        for count in [60, 120] {
            let root = tempfile::tempdir().unwrap();
            let source = modules(root.path(), count);
            analysis(
                root.path(),
                &[("RUSH_BZL_MAX_CONCURRENT", "2")],
                runtime,
                || {
                    let mut evaluator = evaluator(root.path());
                    evaluator.eval_program(&parse(&source).unwrap()).unwrap();
                    assert_eq!(
                        evaluator.get_var("result"),
                        Some(Value::List(
                            (0..count).map(|value| Value::Int(value as i64)).collect()
                        ))
                    );
                },
            );
        }
    }
}

#[test]
fn failed_batch_and_nested_modules_never_bind_rule_function_markers() {
    for runtime in [0, 1, 2] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("WORKSPACE"), "").unwrap();
        std::fs::write(root.path().join("good.bzl"), "value = 1\n").unwrap();
        std::fs::write(
            root.path().join("bad.bzl"),
            "fail(\"intentional module failure\")\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("nested.bzl"),
            "load(\"@@//:bad.bzl\", \"py_rule\")\n",
        )
        .unwrap();
        analysis(root.path(), &[("RUSH_ENABLE_BZLMOD", "1")], runtime, || {
            for failing in ["bad.bzl", "nested.bzl"] {
                let mut evaluator = evaluator(root.path());
                let source = format!(
                    "load(\"@@//:good.bzl\", \"value\")\nload(\"@@//:{failing}\", \"py_rule\")\n"
                );
                let error = evaluator
                    .eval_program(&parse(&source).unwrap())
                    .unwrap_err();
                assert!(
                    format!("{error:#}").contains("intentional module failure"),
                    "{error:#}"
                );
                assert_eq!(evaluator.get_var("py_rule"), None);
            }
            let mut evaluator = evaluator(root.path());
            let error = evaluator
                .eval_program(&parse("load(\"@@//:good.bzl\", \"java_missing\")\n").unwrap())
                .unwrap_err();
            assert!(format!("{error:#}").contains("java_missing"), "{error:#}");
            assert_eq!(evaluator.get_var("java_missing"), None);
        });
    }
}

#[test]
fn partial_profile_and_invalid_concurrency_refuse_complete_analysis() {
    let root = tempfile::tempdir().unwrap();
    let source = modules(root.path(), 2);
    for environment in [
        vec![("RUSH_PROFILE_BZL_LIMIT", "1")],
        vec![("RUSH_BZL_MAX_CONCURRENT", "0")],
        vec![("RUSH_BZL_MAX_CONCURRENT", "invalid")],
    ] {
        analysis(root.path(), &environment, 0, || {
            let mut evaluator = evaluator(root.path());
            assert!(evaluator.eval_program(&parse(&source).unwrap()).is_err());
            assert_eq!(evaluator.get_var("value_0"), None);
            assert_eq!(evaluator.get_var("result"), None);
        });
    }
}

#[test]
fn configured_shared_budget_failure_is_not_a_peripheral_rule_stub() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("WORKSPACE"), "").unwrap();
    std::fs::write(root.path().join("good.bzl"), "py_rule = 7\n").unwrap();
    analysis(root.path(), &[("RUSH_ENABLE_BZLMOD", "1")], 1, || {
        // An explicitly zero shared budget refuses allocation without consuming
        // actual memory. It tests the real configured-budget error path.
        let budget = rush_resource::GlobalMemoryBudget::new(0, 90, 0).unwrap();
        let loader = BzlLoader::new_with_all_and_budget(
            root.path().to_path_buf(),
            None,
            Some(GlobalBzlCache::new()),
            Some(budget),
        );
        let mut denied = Evaluator::new();
        denied.set_bzl_loader(Arc::new(Mutex::new(loader)));
        let source = parse("load(\"@@//:good.bzl\", \"py_rule\")\n").unwrap();
        let error = denied.eval_program(&source).unwrap_err();
        assert!(
            format!("{error:#}").contains("Memory budget exceeded"),
            "{error:#}"
        );
        assert_eq!(denied.get_var("py_rule"), None);
        // Failed admission must release its system reservation/loading owner.
        let mut allowed = evaluator(root.path());
        allowed.eval_program(&source).unwrap();
        assert_eq!(allowed.get_var("py_rule"), Some(Value::Int(7)));
    });
}
