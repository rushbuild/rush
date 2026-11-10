use rush_starlark::{parse, BzlLoader, Evaluator, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn scoped(test: impl FnOnce(&Path)) {
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), || test(root.path()));
}

fn evaluate(evaluator: &mut Evaluator, source: &str) -> anyhow::Result<()> {
    evaluator.eval_program(&parse(source)?)
}

fn expect_true(evaluator: &Evaluator, names: &[&str]) {
    for name in names {
        assert_eq!(evaluator.get_var(name), Some(Value::Bool(true)), "{name}");
    }
}

#[test]
fn local_functions_and_native_aliases_win_over_builtin_names() {
    scoped(|_| {
        let mut evaluator = Evaluator::new();
        evaluate(
            &mut evaluator,
            r#"
original_len = len
len = str
different_native_alias = len(123) == "123"
len = original_len
same_native_binding = len([1, 2]) == 2
def len(value, suffix = "default"):
    return value + ":" + suffix
local_alias = len
local_direct = len("direct", suffix = "keyword") == "direct:keyword"
local_indirect = local_alias("alias") == "alias:default"
def caller(len):
    return len("parameter", suffix = "scope")
parameter_binding = caller(local_alias) == "parameter:scope"
"#,
        )
        .unwrap();
        expect_true(
            &evaluator,
            &[
                "different_native_alias",
                "same_native_binding",
                "local_direct",
                "local_indirect",
                "parameter_binding",
            ],
        );
    });
}

#[test]
fn loaded_helpers_preserve_direct_alias_closure_and_configurable_options() {
    scoped(|root| {
        std::fs::write(root.join("WORKSPACE"), "workspace(name = \"main\")\n").unwrap();
        std::fs::write(root.join("BUILD"), "").unwrap();
        // Real loaded Starlark functions, not replacement native providers.
        // These names collide with the legacy TensorFlow compatibility builtins.
        std::fs::write(
            root.join("defs.bzl"),
            r#"
def tf_copts(option = "default", allow_exceptions = False):
    return [option] + ([] if allow_exceptions else ["-fno-exceptions"])
def if_mkl(if_true, if_false = []):
    return select({"//cfg:enabled": if_true, "//conditions:default": if_false})
extension_constructor = module_extension
def implementation(ctx):
    pass
def options_from_closure():
    return tf_copts("-DCLOSURE") + if_mkl(["-DSELECTED"], ["-DDEFAULT"])
"#,
        )
        .unwrap();
        let loader = Arc::new(Mutex::new(BzlLoader::new(root.to_path_buf())));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let mut evaluator = Evaluator::new();
        evaluator.register_native_namespace();
        evaluator.set_bzl_loader(loader);
        evaluate(
            &mut evaluator,
            r#"
load("//:defs.bzl", "tf_copts", "if_mkl", "options_from_closure", "implementation", module_extension = "extension_constructor")
extension = module_extension(implementation = implementation)
loaded_alias = tf_copts
direct = tf_copts("-DDIRECT", allow_exceptions = True) == ["-DDIRECT"]
indirect = loaded_alias() == ["default", "-fno-exceptions"]
conditional = if_mkl(["-DTRUE"], ["-DFALSE"])
closure_options = options_from_closure()
native.cc_library(name = "consumer", copts = closure_options)
"#,
        )
        .unwrap();
        expect_true(&evaluator, &["direct", "indirect"]);
        assert!(matches!(
            evaluator.get_var("extension"),
            Some(Value::ModuleExtension(_))
        ));
        let Some(Value::Select(branches)) = evaluator.get_var("conditional") else {
            panic!("loaded helper must retain its real conditional expression");
        };
        assert_eq!(
            branches["//cfg:enabled"],
            Value::List(vec![Value::String("-DTRUE".into())])
        );
        assert_eq!(
            branches["//conditions:default"],
            Value::List(vec![Value::String("-DFALSE".into())])
        );
        let Some(Value::Configurable { left, right, .. }) = evaluator.get_var("closure_options")
        else {
            panic!("module closure must use its loaded helpers");
        };
        assert_eq!(
            left.as_ref(),
            &Value::List(vec![
                Value::String("-DCLOSURE".into()),
                Value::String("-fno-exceptions".into())
            ])
        );
        assert!(
            matches!(right.as_ref(), Value::Select(branches) if branches.contains_key("//cfg:enabled") && branches.contains_key("//conditions:default"))
        );
        let calls = evaluator.get_rule_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].rule_type, "cc_library");
        assert_eq!(
            calls[0].kwargs.get("copts"),
            evaluator.get_var("closure_options").as_ref()
        );
    });
}

#[test]
fn noncallable_local_and_loaded_values_do_not_fall_back_to_builtins() {
    scoped(|root| {
        std::fs::write(root.join("WORKSPACE"), "workspace(name = \"main\")\n").unwrap();
        std::fs::write(root.join("BUILD"), "").unwrap();
        std::fs::write(
            root.join("not_callable.bzl"),
            "tf_copts = 7\nif_mkl = \"value\"\n",
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        for source in [
            "len = 7\nresult = len([])\n",
            "tf_copts = \"value\"\nresult = tf_copts()\n",
            "load(\"//:not_callable.bzl\", \"tf_copts\")\nresult = tf_copts()\n",
            "load(\"//:not_callable.bzl\", \"if_mkl\")\nresult = if_mkl([1])\n",
        ] {
            let mut evaluator = Evaluator::new();
            evaluator.set_bzl_loader(Arc::new(Mutex::new(BzlLoader::new(root.to_path_buf()))));
            assert!(evaluate(&mut evaluator, source).is_err(), "{source}");
            assert!(evaluator.get_rule_calls().is_empty());
        }
    });
}

#[test]
fn unbound_builtins_and_registered_rule_sentinels_keep_existing_routes() {
    scoped(|_| {
        let mut evaluator = Evaluator::new();
        evaluator.register_native_namespace();
        evaluate(
            &mut evaluator,
            r#"
unbound = len([1, 2]) == 2 and str(17) == "17"
legacy_helpers = tf_copts() == ["-DEIGEN_AVOID_STL_ARRAY", "-Wno-sign-compare", "-ftemplate-depth=900"] and if_mkl(["true"], ["false"]) == ["false"]
test_suite(name = "legacy_suite", tests = [])
cc_library(name = "captured_rule", srcs = [])
"#,
        )
        .unwrap();
        expect_true(&evaluator, &["unbound", "legacy_helpers"]);
        let calls = evaluator.get_rule_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].rule_type, "cc_library");
        assert_eq!(
            calls[0].kwargs.get("name"),
            Some(&Value::String("captured_rule".into()))
        );
    });
}

#[test]
fn native_glob_fastpath_stays_package_relative() {
    scoped(|root| {
        let package = root.join("pkg");
        std::fs::create_dir(&package).unwrap();
        std::fs::write(package.join("own.cc"), "").unwrap();
        std::fs::write(root.join("foreign.cc"), "").unwrap();
        let mut evaluator = Evaluator::new();
        evaluator.register_native_namespace();
        evaluator.set_current_package("pkg".into());
        evaluator.set_current_package_dir(Some(package));
        evaluate(
            &mut evaluator,
            "result = glob([\"*.cc\"]) == [\"own.cc\"]\n",
        )
        .unwrap();
        expect_true(&evaluator, &["result"]);
    });
}
