//! Closed-family frontend controls. These tests do not execute rule analysis.
use rush_starlark::{parse, BzlLoader, Evaluator, GlobalBzlCache, Value};
use std::sync::{Arc, Mutex};

const UNKNOWN_MODULE: &str = r#"
def implementation(ctx):
    return []
projection = rule(
    implementation = implementation,
    attrs = {"original_deps": attr.label_list(allow_files=True, allow_empty=True, providers=[CcInfo])},
)
alias = projection
"#;

fn evaluator(root: &std::path::Path) -> Evaluator {
    let mut evaluator = Evaluator::new();
    evaluator.register_native_namespace();
    evaluator.set_current_package_dir(Some(root.to_owned()));
    evaluator
}

#[test]
fn projection_refusal_is_opaque_lazy_and_survives_callable_aliases() {
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), || {
        let mut e = evaluator(root.path());
        e.eval_program(&parse(UNKNOWN_MODULE).unwrap()).unwrap();
        assert!(matches!(
            e.get_var("projection"),
            Some(Value::ProjectionRule(_))
        ));
        e.eval_program(
            &parse(
                "alias(name='bad',original_deps=[':real'])\nnative.filegroup(name='unrelated')\n",
            )
            .unwrap(),
        )
        .unwrap();
        let calls = e.get_rule_calls();
        let binding = calls[0]
            .cc_projection
            .as_ref()
            .expect("refusal cannot become legacy None");
        assert!(binding.kind().is_none());
        assert!(binding.refusal_reason().unwrap().contains("origin"));
        assert_eq!(
            calls[0].kwargs["original_deps"],
            Value::List(vec![Value::String(":real".into())])
        );
        assert!(calls[1].cc_projection.is_none());
        assert!(!calls[0]
            .kwargs
            .keys()
            .any(|key| key.contains("rush") || key.contains("projection")));
        let error = e
            .eval_program(&parse("bad=json.encode(projection)\n").unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("not JSON"));
    });
}

#[test]
fn forged_attribute_dictionary_and_same_named_foreign_provider_cannot_qualify() {
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), || {
        for descriptor in [
            "{'_type':'attr.label_list','allow_files':True,'allow_empty':True,'providers':[CcInfo]}",
            "attr.label_list(allow_files=True,allow_empty=True,providers=[CcInfo])",
        ] {
            let mut e = evaluator(root.path());
            let source = format!("{}def f(ctx):\n    return []\nr=rule(implementation=f,attrs={{'original_deps':{descriptor}}})\nr(name='bad')\n", if descriptor.starts_with("attr.") { "CcInfo=provider(fields=[])\n" } else { "" });
            e.eval_program(&parse(&source).unwrap()).unwrap();
            // A foreign provider may retain the generic unsupported path; it
            // must never be admitted as a native projection by its spelling.
            assert!(e.get_rule_calls()[0].cc_projection.as_ref().is_none_or(|decl| decl.kind().is_none() && decl.refusal_reason().is_some()));
        }
    });
}

#[test]
fn unrelated_rule_schemas_keep_legacy_dispatch_and_cannot_carry_proof_attributes() {
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), || {
        let mut e = evaluator(root.path());
        e.eval_program(&parse("def f(ctx):\n    return []\nr=rule(implementation=f,attrs={'srcs':attr.label_list()})\nr(name='other',__rush_custom_rule_kind='headers')\n").unwrap()).unwrap();
        assert_eq!(e.get_rule_calls()[0].rule_type, "CustomRule");
        assert!(e.get_rule_calls()[0].cc_projection.is_none());
        assert!(e
            .eval_program(&parse("x=attr.label_list(['bad'])\n").unwrap())
            .is_err());
    });
}

#[test]
fn local_and_global_module_cache_replay_transitive_source_edits() {
    for global in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("WORKSPACE"), "").unwrap();
        std::fs::write(root.path().join("child.bzl"), "value='first'\n").unwrap();
        std::fs::write(
            root.path().join("parent.bzl"),
            "load(':child.bzl','value')\ndef read():\n    return value\n",
        )
        .unwrap();
        let loader = Arc::new(Mutex::new(if global {
            BzlLoader::new_with_global_cache(root.path().to_owned(), GlobalBzlCache::new())
        } else {
            BzlLoader::new(root.path().to_owned())
        }));
        let read = || {
            rush_incremental::tracked::test_scope(root.path(), || {
                let mut e = evaluator(root.path());
                e.set_bzl_loader(loader.clone());
                e.eval_program(&parse("load('//:parent.bzl','read')\nresult=read()\n").unwrap())
                    .unwrap();
                e.get_var("result").unwrap()
            })
        };
        assert_eq!(read(), Value::String("first".into()));
        assert_eq!(read(), Value::String("first".into()));
        std::fs::write(root.path().join("child.bzl"), "value='edited'\n").unwrap();
        assert_eq!(read(), Value::String("edited".into()));
        std::fs::write(root.path().join("child.bzl"), "value='first'\n").unwrap();
        assert_eq!(read(), Value::String("first".into()));
    }
}

#[test]
fn loaded_candidate_does_not_retain_its_loader() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("WORKSPACE"), "").unwrap();
    std::fs::write(root.path().join("defs.bzl"), UNKNOWN_MODULE).unwrap();
    let loader = Arc::new(Mutex::new(BzlLoader::new(root.path().to_owned())));
    let weak = Arc::downgrade(&loader);
    let retained = rush_incremental::tracked::test_scope(root.path(), || {
        BzlLoader::load_bzl_from_evaluator(
            loader.clone(),
            "//:defs.bzl",
            &[("projection".into(), "projection".into())],
        )
        .unwrap()
    });
    assert!(matches!(retained["projection"], Value::ProjectionRule(_)));
    drop(loader);
    assert!(
        weak.upgrade().is_none(),
        "opaque origin must not close a loader ownership cycle"
    );
    drop(retained);
}

/// Actual pinned TF sources/cache are supplied by the root-owned runner. Only an
/// owned copy of the defining module is edited; original source/cache is read-only.
#[cfg(unix)]
#[test]
#[ignore = "requires RUSH_TF_PROJECTION_WORKSPACE, RUSH_TF_PROJECTION_EXTERNAL and RUSH_TF_PROJECTION_BAZEL pointing to retained genuine TF2.17 inputs"]
fn genuine_tsl_projection_aliases_owners_warm_requalification_and_edits() {
    use bytes::Bytes;
    use rush_command_core::CommandSemantics;
    use rush_incremental::{tracked, RealTrackedIo};
    use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
    use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use rush_starlark::CcProjectionKind;
    use sha2::{Digest, Sha256};
    use std::{
        os::unix::fs::{symlink, MetadataExt},
        path::PathBuf,
    };

    let workspace = PathBuf::from(
        std::env::var_os("RUSH_TF_PROJECTION_WORKSPACE").expect("genuine TF source root"),
    );
    let external = PathBuf::from(
        std::env::var_os("RUSH_TF_PROJECTION_EXTERNAL").expect("genuine cached source roots"),
    );
    let bazel = PathBuf::from(
        std::env::var_os("RUSH_TF_PROJECTION_BAZEL").expect("selected genuine analyzer"),
    );
    assert!(workspace.is_absolute() && external.is_absolute() && bazel.is_absolute());
    // The private runner supplies these exact owned roots. Process environment
    // is not an implicit fallback for a tracked analyzer subprocess.
    let case = workspace.parent().expect("fresh case/source fixture");
    assert_eq!(workspace.file_name().unwrap(), "source");
    // Test-only ownership check; this does not grant production authority.
    let uid = unsafe { libc::geteuid() };
    let case_metadata = std::fs::metadata(case).unwrap();
    assert_eq!(std::fs::canonicalize(case).unwrap(), case);
    assert_eq!(case_metadata.uid(), uid);
    assert_eq!(
        case_metadata.mode() & 0o077,
        0,
        "fixture case must be private"
    );
    let mut command_environment = Vec::new();
    for (name, suffix) in [
        ("HOME", "home"),
        ("TMPDIR", "tmp"),
        ("RUSH_WORKSPACE_CACHE_ROOT", "workcache"),
    ] {
        let value = PathBuf::from(std::env::var_os(name).expect("owned test environment root"));
        assert_eq!(
            value,
            case.join(suffix),
            "{name} must belong to the fresh fixture"
        );
        assert_eq!(
            std::fs::canonicalize(&value).unwrap(),
            value,
            "{name} cannot be redirected"
        );
        let metadata = std::fs::metadata(&value).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(metadata.uid(), uid, "{name} must be owned by the test user");
        assert_eq!(metadata.mode() & 0o077, 0, "{name} must be private");
        command_environment.push(EnvironmentVariable {
            name: Bytes::from(name.to_owned()),
            value: Bytes::from(value.to_str().unwrap().to_owned()),
        });
    }
    let path = std::env::var("PATH").expect("explicit test PATH");
    assert_eq!(
        path, "/usr/bin:/bin",
        "the test admits only the reviewed system tool PATH"
    );
    command_environment.push(EnvironmentVariable {
        name: Bytes::from_static(b"PATH"),
        value: Bytes::from(path),
    });
    command_environment.push(EnvironmentVariable {
        name: Bytes::from_static(b"RUSH_BAZEL_PATH"),
        value: Bytes::from(bazel.to_str().unwrap().to_owned()),
    });
    let genuine_xla = workspace.join("third_party/xla");
    let genuine_module = genuine_xla.join("xla/tsl/tsl.bzl");
    let source = std::fs::read(&genuine_module).unwrap();
    assert_eq!(
        hex::encode(Sha256::digest(&source)),
        "ae8424430a809d30b371e3c4c11a0783c1c4d087a014a73d8439a4977119f918"
    );
    let owned = tempfile::tempdir().unwrap();
    let overlay = owned.path().join("xla-root");
    std::fs::create_dir_all(overlay.join("xla/tsl")).unwrap();
    for (source_dir, target_dir, excluded) in [
        (genuine_xla.clone(), overlay.clone(), "xla"),
        (genuine_xla.join("xla"), overlay.join("xla"), "tsl"),
        (
            genuine_xla.join("xla/tsl"),
            overlay.join("xla/tsl"),
            "tsl.bzl",
        ),
    ] {
        for entry in std::fs::read_dir(source_dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() != excluded {
                symlink(entry.path(), target_dir.join(entry.file_name())).unwrap();
            }
        }
    }
    let module = overlay.join("xla/tsl/tsl.bzl");
    std::fs::write(&module, &source).unwrap();
    let scope = |body: &mut dyn FnMut()| {
        let request = RunRequest {
            auth_cookie: Bytes::new(),
            argv: vec![
                Bytes::from_static(b"rush"),
                Bytes::from_static(b"build"),
                Bytes::from_static(b"//:test"),
            ],
            working_directory: Bytes::from(workspace.to_str().unwrap().to_owned()),
            environment: command_environment.clone(),
            block_for_lock: true,
            client_build_id: "cc-projection-genuine".into(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            workspace_config: vec![],
        };
        let io = Arc::new(RealTrackedIo::new(
            CommandSemantics::from_run_request(&request).unwrap(),
            &workspace,
        ));
        tracked::scope(io, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let _guard = runtime.enter();
            body();
        });
    };
    for global in [false, true] {
        let mut raw =
            BzlLoader::new_with_all(workspace.clone(), None, global.then(GlobalBzlCache::new));
        for entry in std::fs::read_dir(&external).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_dir() {
                raw.add_external_repo(
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.path(),
                );
            }
        }
        raw.add_external_repo("local_xla".into(), overlay.clone());
        raw.add_external_repo("local_tsl".into(), genuine_xla.join("third_party/tsl"));
        raw.add_external_repo("projection_other".into(), overlay.clone());
        let loader = Arc::new(Mutex::new(raw));
        let evaluate = |label: &str| {
            let mut result = None;
            scope(&mut || {
                let mut e = evaluator(&workspace);
                e.set_current_package("consumer".into());
                e.set_bzl_loader(loader.clone());
                e.eval_program(&parse(&format!("load('{label}', helper='cc_header_only_library')\nalias=helper\nalias(name='test_headers',deps=[':caller_dep'],extra_deps=[':separate_extra'])\n")).unwrap()).unwrap();
                let bindings = e
                    .get_rule_calls()
                    .iter()
                    .filter_map(|call| {
                        call.cc_projection
                            .clone()
                            .map(|binding| (call.kwargs.clone(), binding))
                    })
                    .collect::<Vec<_>>();
                result = Some((e, bindings));
            });
            result.unwrap()
        };
        // The real public TF reexport chain preserves the original TSL identity.
        let (_, first) = evaluate("//tensorflow:tensorflow.default.bzl");
        assert_eq!(first.len(), 2);
        assert_eq!(
            first
                .iter()
                .filter_map(|(_, d)| d.kind())
                .collect::<Vec<_>>(),
            [
                CcProjectionKind::HeadersV1,
                CcProjectionKind::CompilationParametersV1
            ]
        );
        assert!(first.iter().all(|(_, d)| d.refusal_reason().is_none()));
        assert_eq!(
            first[1].0["original_deps"],
            Value::List(vec![Value::String(":caller_dep".into())])
        );
        let (_, warm) = evaluate("//tensorflow:tensorflow.default.bzl");
        assert_eq!(
            warm[0].1.definition_identity(),
            first[0].1.definition_identity()
        );
        scope(&mut || {
            for (_, declaration) in &warm {
                declaration.revalidate(loader.clone()).unwrap();
            }
        });
        let (_, other) = evaluate("@@projection_other//xla/tsl:tsl.bzl");
        assert_eq!(other.len(), 2);
        assert!(other.iter().all(|(_, d)| d.kind().is_some()));
        assert_ne!(
            other[0].1.definition_identity(),
            first[0].1.definition_identity(),
            "same physical source does not collapse logical owners"
        );
        // Source mutation is confined to this test's real owned copy.
        let edited = String::from_utf8(source.clone()).unwrap().replace(
            "defines = depset(direct = defines.to_list()),",
            "defines = depset(),",
        );
        assert_ne!(edited.as_bytes(), source.as_slice());
        std::fs::write(&module, edited).unwrap();
        let (_, changed) = evaluate("//tensorflow:tensorflow.default.bzl");
        assert_eq!(changed.len(), 2);
        assert!(changed
            .iter()
            .all(|(_, d)| d.kind().is_none() && d.refusal_reason().is_some()));
        std::fs::write(&module, &source).unwrap();
        let (restored_evaluator, restored) = evaluate("//tensorflow:tensorflow.default.bzl");
        assert!(restored.iter().all(|(_, d)| d.kind().is_some()));
        // The exact source digest alone cannot bless a changed live helper closure.
        let Value::Function { closure, .. } = restored_evaluator.get_var("helper").unwrap() else {
            panic!("real TSL macro must stay a function");
        };
        let helper = closure.get("_get_transitive_headers").unwrap();
        let mut shared = closure.as_ref().clone();
        shared.define("_get_transitive_headers".into(), Value::None);
        let (_, altered_closure) = evaluate("//tensorflow:tensorflow.default.bzl");
        assert!(altered_closure
            .iter()
            .any(|(_, d)| d.kind().is_none() && d.refusal_reason().is_some()));
        shared.define("_get_transitive_headers".into(), helper);
        let (_, recovered) = evaluate("//tensorflow:tensorflow.default.bzl");
        assert!(recovered.iter().all(|(_, d)| d.kind().is_some()));
    }
    assert_eq!(
        std::fs::read(&genuine_module).unwrap(),
        source,
        "original inputs unchanged"
    );
}
