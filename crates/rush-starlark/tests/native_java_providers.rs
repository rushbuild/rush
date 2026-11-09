use rush_starlark::{parse, BzlLoader, Evaluator, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const NATIVE: &[u8] = include_bytes!("fixtures/rules_java_5_3_5/native.bzl");
const DEFS: &[u8] = include_bytes!("fixtures/rules_java_5_3_5/defs.bzl");
static MODULE_LOAD_LOCK: Mutex<()> = Mutex::new(());

fn with_analysis(f: impl FnOnce()) {
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), f);
}

fn evaluate(source: &str) -> Evaluator {
    let mut evaluator = Evaluator::new();
    evaluator.eval_program(&parse(source).unwrap()).unwrap();
    evaluator
}

fn upstream_loader() -> (tempfile::TempDir, Arc<Mutex<BzlLoader>>) {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("external/rules_java");
    std::fs::create_dir_all(repo.join("java/private")).unwrap();
    std::fs::write(root.path().join("WORKSPACE"), "").unwrap();
    std::fs::write(repo.join("java/private/native.bzl"), NATIVE).unwrap();
    std::fs::write(repo.join("java/defs.bzl"), DEFS).unwrap();
    std::fs::write(repo.join("unrelated.bzl"), "answer = 42\n").unwrap();
    let mut loader = BzlLoader::new(root.path().to_path_buf());
    loader.add_external_repo("rules_java".to_string(), repo);
    (root, Arc::new(Mutex::new(loader)))
}

fn load(
    loader: &Arc<Mutex<BzlLoader>>,
    label: &str,
    symbols: &[(&str, &str)],
) -> HashMap<String, Value> {
    let requested: Vec<_> = symbols
        .iter()
        .map(|(local, original)| (local.to_string(), original.to_string()))
        .collect();
    BzlLoader::load_bzl_from_evaluator(Arc::clone(loader), label, &requested).unwrap()
}

#[test]
fn authentic_rules_java_exports_preserve_native_provider_identity() {
    with_analysis(|| {
        let _load_guard = MODULE_LOAD_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(
            hex::encode(Sha256::digest(NATIVE)),
            "2a77333abf00329e125993d66bee2b7ab8c46181b64e15f3aff27a75926ddfac"
        );
        assert_eq!(
            hex::encode(Sha256::digest(DEFS)),
            "2791e87e988e79090c0650b55a5a05e1902b551b5634ad9cf0b20b9d93c10337"
        );
        let (_root, loader) = upstream_loader();
        let native = load(
            &loader,
            "@rules_java//java/private:native.bzl",
            &[
                ("info", "NativeJavaInfo"),
                ("plugin", "NativeJavaPluginInfo"),
                ("common", "native_java_common"),
            ],
        );
        let public = load(
            &loader,
            "@rules_java//java:defs.bzl",
            &[
                ("info", "JavaInfo"),
                ("plugin", "JavaPluginInfo"),
                ("common", "java_common"),
            ],
        );
        let expected = evaluate("info = JavaInfo\nplugin = JavaPluginInfo\ncommon = java_common\n");
        for symbol in ["info", "plugin", "common"] {
            assert_eq!(native.get(symbol), public.get(symbol));
            assert_eq!(native.get(symbol), expected.get_var(symbol).as_ref());
        }
        let Value::Namespace(common) = &native["common"] else {
            panic!("java_common must remain a namespace");
        };
        assert_eq!(common.get("provider"), native.get("info"));
        assert_eq!(common.get("JavaPluginInfo"), native.get("plugin"));
        for (symbol, name) in [("info", "JavaInfo"), ("plugin", "JavaPluginInfo")] {
            assert!(matches!(
                &native[symbol],
                Value::Provider { name: actual, init: Some(_), .. } if actual == name
            ));
        }
    });
}

#[test]
fn java_predeclared_values_are_not_unassigned_module_exports() {
    with_analysis(|| {
        let _load_guard = MODULE_LOAD_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_root, loader) = upstream_loader();
        assert_eq!(
            load(
                &loader,
                "@rules_java//:unrelated.bzl",
                &[("answer", "answer")]
            )["answer"],
            Value::Int(42)
        );
        for symbol in ["JavaInfo", "JavaPluginInfo"] {
            let error = BzlLoader::load_bzl_from_evaluator(
                Arc::clone(&loader),
                "@rules_java//:unrelated.bzl",
                &[(symbol.to_string(), symbol.to_string())],
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(symbol), "{error}");
            assert!(error.contains("not found"), "{error}");
        }
    });
}

#[test]
fn java_provider_construction_rejects_unsupported_semantics_through_aliases() {
    with_analysis(|| {
        let _load_guard = MODULE_LOAD_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for (source, provider) in [
            ("JavaInfo()", "JavaInfo"),
            ("JavaPluginInfo()", "JavaPluginInfo"),
            ("Ctor = JavaInfo\nCtor(output_jar = None)", "JavaInfo"),
            ("Ctor = JavaPluginInfo\nCtor(**{})", "JavaPluginInfo"),
            ("java_common.provider()", "JavaInfo"),
            ("java_common.JavaPluginInfo()", "JavaPluginInfo"),
        ] {
            let mut evaluator = Evaluator::new();
            let error = evaluator
                .eval_program(&parse(source).unwrap())
                .unwrap_err()
                .to_string();
            assert!(error.contains(provider), "{error}");
            assert!(error.contains("construction is unsupported"), "{error}");
            assert!(evaluator.get_var("__rush_provider_initializer__").is_none());
        }
        let (_root, loader) = upstream_loader();
        let mut evaluator = Evaluator::new();
        evaluator.set_bzl_loader(loader);
        let error = evaluator
            .eval_program(
                &parse("load(\"@rules_java//java:defs.bzl\", Ctor = \"JavaInfo\")\nCtor()\n")
                    .unwrap(),
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("JavaInfo construction is unsupported"),
            "{error}"
        );
    });
}

#[test]
fn java_exports_do_not_shadow_existing_constructor_aliases_or_restore_placeholders() {
    with_analysis(|| {
        let mut evaluator = evaluate(
        "CcCtor = CcInfo\ncc = CcCtor()\nDefaultCtor = DefaultInfo\ndefault = DefaultCtor()\nProtoCtor = ProtoInfo\nproto = ProtoCtor()\n",
    );
        for (variable, provider, required_fields) in [
            (
                "cc",
                "CcInfo",
                &["compilation_context", "linking_context"][..],
            ),
            ("default", "DefaultInfo", &["files"][..]),
            (
                "proto",
                "ProtoInfo",
                &["direct_sources", "transitive_sources"][..],
            ),
        ] {
            let Value::ProviderInstance {
                provider_name,
                fields,
            } = evaluator.get_var(variable).unwrap()
            else {
                panic!("{variable} must retain its builtin constructor");
            };
            assert_eq!(provider_name, provider);
            for field in required_fields {
                assert!(fields.contains_key(*field), "{provider} is missing {field}");
            }
        }
        for symbol in [
            "_tf_py_test",
            "_ADDITIONAL_API_INDEXABLE_SETTINGS",
            "tf_openmp_copts",
        ] {
            let error = evaluator
                .eval_program(&parse(&format!("value = {symbol}\n")).unwrap())
                .unwrap_err()
                .to_string();
            assert!(error.contains("Undefined variable"), "{error}");
        }
    });
}

#[test]
fn authentic_java_rule_macros_retain_their_native_target_identity() {
    with_analysis(|| {
        let _load_guard = MODULE_LOAD_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_root, loader) = upstream_loader();
        let mut evaluator = Evaluator::new();
        evaluator.register_native_namespace();
        evaluator.set_bzl_loader(loader);
        evaluator
        .eval_program(
            &parse(
                "load(\"@rules_java//java:defs.bzl\", \"java_library\")\njava_library(name = \"java\", deps = [\":dependency\"])\n",
            )
            .unwrap(),
        )
        .unwrap();
        let calls = evaluator.captured_rule_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].rule_type, "java_library");
        assert_eq!(calls[0].kwargs["name"], Value::String("java".to_string()));
        assert_eq!(
            calls[0].kwargs["deps"],
            Value::List(vec![Value::String(":dependency".to_string())])
        );
        assert_eq!(
            calls[0].kwargs["tags"],
            Value::List(vec![Value::String(
                "__JAVA_RULES_MIGRATION_DO_NOT_USE_WILL_BREAK__".to_string()
            )])
        );
    });
}

#[test]
fn native_java_initializers_validate_unpacked_arguments() {
    with_analysis(|| {
        for (source, message) in [
            (
                "JavaInfo(output_jar = None, **{\"output_jar\": None})",
                "multiple values for argument 'output_jar'",
            ),
            (
                "Ctor = JavaPluginInfo\nCtor(**{\"data\": []}, **{\"data\": []})",
                "multiple values for argument 'data'",
            ),
            ("JavaInfo(**{1: None})", "argument names must be strings"),
            ("JavaInfo(**None)", "cannot unpack a non-dict argument"),
            ("JavaInfo(*None)", "Cannot unpack non-iterable value"),
            (
                "JavaInfo(**{\"output_jar\": None})",
                "JavaInfo construction is unsupported",
            ),
        ] {
            let mut evaluator = Evaluator::new();
            let error = evaluator
                .eval_program(&parse(source).unwrap())
                .unwrap_err()
                .to_string();
            assert!(error.contains(message), "{error}");
            assert!(evaluator.get_var("__rush_provider_initializer__").is_none());
        }
    });
}
