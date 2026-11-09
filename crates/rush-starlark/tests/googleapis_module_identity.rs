//! Keep the main TensorFlow Googleapis wrapper distinct from the external module
//! it imports, even though both files are named repository_rules.bzl.
use rush_starlark::{parse, BzlLoader, Evaluator, GlobalBzlCache, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

const TF_WRAPPER: &[u8] =
    include_bytes!("fixtures/tensorflow_2_17_0_googleapis/repository_rules.bzl");
const GOOGLEAPIS: &[u8] =
    include_bytes!("fixtures/tensorflow_2_17_0_googleapis/googleapis_repository_rules.bzl");
static LOAD_LOCK: Mutex<()> = Mutex::new(());

fn analysis(root: &Path, body: impl FnOnce()) {
    let _lock = LOAD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    rush_incremental::tracked::test_scope(root, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        body();
    });
}

fn setup(root: &Path) {
    std::fs::create_dir_all(root.join("third_party/googleapis")).unwrap();
    std::fs::create_dir_all(root.join("external_source")).unwrap();
    std::fs::write(root.join("alias_consumer.bzl"),
        "load(\"@googleapis_alias//:repository_rules.bzl\", _alias_external = \"switched_rules_by_language\")\nalias_external = _alias_external\n").unwrap();
    std::fs::write(
        root.join("WORKSPACE"),
        "workspace(name = \"org_tensorflow\")\n",
    )
    .unwrap();
}

fn loader(root: &Path, cache: Option<GlobalBzlCache>, workspace: bool) -> Arc<Mutex<BzlLoader>> {
    let mut loader = BzlLoader::new_with_all(root.to_path_buf(), None, cache);
    loader.add_external_repo("com_google_googleapis".into(), root.join("external_source"));
    loader.set_repo_mapping_for(
        "",
        HashMap::from([("googleapis_alias".into(), "com_google_googleapis".into())]),
    );
    loader.set_workspace_repository_context(workspace.then(|| root.to_path_buf()));
    Arc::new(Mutex::new(loader))
}

fn load(loader: &Arc<Mutex<BzlLoader>>, label: &str, original: &str) -> anyhow::Result<Value> {
    BzlLoader::load_bzl_from_evaluator(loader.clone(), label, &[("result".into(), original.into())])
        .map(|symbols| symbols["result"].clone())
}

fn assert_same_function_definition(imported: &Value, direct: &Value) {
    let Value::Function {
        params: imported_params,
        varargs_param: imported_varargs,
        kwargs_param: imported_kwargs,
        body: imported_body,
        ..
    } = imported
    else {
        panic!("the wrapper import must be the actual function declaration");
    };
    let Value::Function {
        params: direct_params,
        varargs_param: direct_varargs,
        kwargs_param: direct_kwargs,
        body: direct_body,
        ..
    } = direct
    else {
        panic!("the external module must export the actual function declaration");
    };
    // Compare source declarations, not cloned environment storage. Definition
    // owner/package and phase semantics are asserted by the executable Label
    // regressions below. Never Debug-format recursive Value::Function closures.
    assert_eq!(imported_params, direct_params);
    assert_eq!(imported_varargs, direct_varargs);
    assert_eq!(imported_kwargs, direct_kwargs);
    assert_eq!(imported_body, direct_body);
}

fn evaluate(loader: &Arc<Mutex<BzlLoader>>, source: &str) -> Value {
    let mut evaluator = Evaluator::new();
    evaluator.set_current_package("consumer".into());
    evaluator.set_bzl_loader(loader.clone());
    evaluator.eval_program(&parse(source).unwrap()).unwrap();
    evaluator.get_var("result").unwrap()
}

fn synthetic_modules(root: &Path, main: &str, external: &str) {
    std::fs::write(
        root.join("third_party/googleapis/repository_rules.bzl"),
        format!(
            r#"
load("@com_google_googleapis//:repository_rules.bzl", imported = "switched_rules_by_language")
LOCAL = Label(":definition")
PHASE = hasattr(native, "bazel_version")
def config_googleapis():
    return [{main:?}, str(LOCAL), str(Label(":inside")), PHASE, imported()]
"#
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("external_source/repository_rules.bzl"),
        format!(
            r#"
LOCAL = Label(":definition")
PHASE = hasattr(native, "bazel_version")
def switched_rules_by_language():
    return [{external:?}, str(LOCAL), str(Label(":inside")), PHASE]
"#
        ),
    )
    .unwrap();
}

fn expected_external(external: &str, workspace: bool) -> Value {
    Value::List(vec![
        Value::String(external.into()),
        Value::String("@@com_google_googleapis//:definition".into()),
        Value::String("@@com_google_googleapis//:inside".into()),
        Value::Bool(workspace),
    ])
}

fn expected(main: &str, external: &str, workspace: bool) -> Value {
    Value::List(vec![
        Value::String(main.into()),
        Value::String("@@//third_party/googleapis:definition".into()),
        Value::String("@@//third_party/googleapis:inside".into()),
        Value::Bool(workspace),
        expected_external(external, workspace),
    ])
}

#[test]
fn main_and_external_same_basename_keep_owners_across_cold_warm_reverse_and_phase_caches() {
    for global in [false, true] {
        for reverse in [false, true] {
            let root = tempfile::tempdir().unwrap();
            setup(root.path());
            synthetic_modules(root.path(), "main-v1", "external-v1");
            let cache = global.then(GlobalBzlCache::new);
            // Reuse one global cache across BUILD/WORKSPACE contexts and fresh
            // loader instances. PHASE makes accidental cross-phase reuse visible.
            for workspace in [false, true, false] {
                let loader = loader(root.path(), cache.clone(), workspace);
                analysis(root.path(), || {
                    for _ in 0..2 {
                        if reverse {
                            assert!(matches!(
                                load(
                                    &loader,
                                    "@com_google_googleapis//:repository_rules.bzl",
                                    "switched_rules_by_language"
                                )
                                .unwrap(),
                                Value::Function { .. }
                            ));
                        }
                        for label in [
                            "//third_party/googleapis:repository_rules.bzl",
                            "@@//third_party/googleapis:repository_rules.bzl",
                            "@org_tensorflow//third_party/googleapis:repository_rules.bzl",
                        ] {
                            let source = format!("load({label:?}, renamed = \"config_googleapis\")\nresult = renamed()\n");
                            assert_eq!(
                                evaluate(&loader, &source),
                                expected("main-v1", "external-v1", workspace)
                            );
                        }
                        // Apparent names belong to a caller's repository map;
                        // exercise the alias from a real main-owned .bzl frame.
                        assert_eq!(evaluate(&loader,
                            "load(\"@@//:alias_consumer.bzl\", \"alias_external\")\nresult = alias_external()\n"),
                            expected_external("external-v1", workspace));
                        assert!(load(
                            &loader,
                            "@com_google_googleapis//:repository_rules.bzl",
                            "config_googleapis"
                        )
                        .is_err());
                    }
                });
            }
        }
    }
}

#[test]
fn main_wrapper_and_external_edits_invalidate_cached_reexports_without_namespace_fallback() {
    for global in [false, true] {
        let root = tempfile::tempdir().unwrap();
        setup(root.path());
        synthetic_modules(root.path(), "main-v1", "external-v1");
        std::fs::write(root.path().join("bridge.bzl"), "load(\"//third_party/googleapis:repository_rules.bzl\", original = \"config_googleapis\")\nforwarded = original\n").unwrap();
        let loader = loader(root.path(), global.then(GlobalBzlCache::new), false);
        let source = "load(\"@@//:bridge.bzl\", alias = \"forwarded\")\nresult = alias()\n";
        analysis(root.path(), || {
            assert_eq!(
                evaluate(&loader, source),
                expected("main-v1", "external-v1", false)
            )
        });
        synthetic_modules(root.path(), "main-v2", "external-v1");
        // The local cache is command-scoped. Clear it at the next command
        // boundary; the shared global cache stays resident and must validate
        // transitive input edits through its producing dependency manifest.
        if !global {
            loader.lock().unwrap().clear_cache();
        }
        analysis(root.path(), || {
            assert_eq!(
                evaluate(&loader, source),
                expected("main-v2", "external-v1", false)
            )
        });
        synthetic_modules(root.path(), "main-v2", "external-v2");
        if !global {
            loader.lock().unwrap().clear_cache();
        }
        analysis(root.path(), || {
            assert_eq!(
                evaluate(&loader, source),
                expected("main-v2", "external-v2", false)
            )
        });
        std::fs::write(
            root.path()
                .join("third_party/googleapis/repository_rules.bzl"),
            "MAIN_ONLY = 42\n",
        )
        .unwrap();
        if !global {
            loader.lock().unwrap().clear_cache();
        }
        analysis(root.path(), || {
            assert!(load(
                &loader,
                "//third_party/googleapis:repository_rules.bzl",
                "config_googleapis"
            )
            .is_err());
            assert!(matches!(
                load(
                    &loader,
                    "@com_google_googleapis//:repository_rules.bzl",
                    "switched_rules_by_language"
                )
                .unwrap(),
                Value::Function { .. }
            ));
        });
    }
}

#[test]
fn authentic_tensorflow_googleapis_wrapper_loads_its_actual_external_module() {
    assert_eq!(
        hex::encode(Sha256::digest(TF_WRAPPER)),
        "11b2eb7d943f1c24462a9c88b4a84ba03be8751b826fd85aa34305074bd4fea7"
    );
    assert_eq!(
        hex::encode(Sha256::digest(GOOGLEAPIS)),
        "1ea9af576a42da2b02af67bc1209774fad566bc9cf4b2e5a697e854af808300e"
    );
    for (license, expected) in [
        (
            include_bytes!("fixtures/tensorflow_2_17_0_googleapis/LICENSE.tensorflow").as_slice(),
            "71c6915d04265772a0339bed47276942c678b45cc01534210ebe6984fd1aec65",
        ),
        (
            include_bytes!("fixtures/tensorflow_2_17_0_googleapis/LICENSE.googleapis").as_slice(),
            "c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4",
        ),
    ] {
        assert_eq!(hex::encode(Sha256::digest(license)), expected);
    }
    for workspace in [false, true] {
        let root = tempfile::tempdir().unwrap();
        setup(root.path());
        std::fs::write(
            root.path()
                .join("third_party/googleapis/repository_rules.bzl"),
            TF_WRAPPER,
        )
        .unwrap();
        std::fs::write(
            root.path().join("external_source/repository_rules.bzl"),
            GOOGLEAPIS,
        )
        .unwrap();
        let loader = loader(root.path(), Some(GlobalBzlCache::new()), workspace);
        analysis(root.path(), || {
            for _ in 0..2 {
                let wrapper = load(
                    &loader,
                    "//third_party/googleapis:repository_rules.bzl",
                    "config_googleapis",
                )
                .unwrap();
                let external = load(
                    &loader,
                    "@com_google_googleapis//:repository_rules.bzl",
                    "switched_rules_by_language",
                )
                .unwrap();
                let Value::Function { closure, .. } = wrapper else {
                    panic!("the actual wrapper must retain its real function");
                };
                let imported = closure
                    .get("switched_rules_by_language")
                    .expect("the real TensorFlow wrapper must retain its explicit external import");
                assert_same_function_definition(&imported, &external);
                assert!(load(
                    &loader,
                    "@com_google_googleapis//:repository_rules.bzl",
                    "config_googleapis"
                )
                .is_err());
            }
        });
    }
}
