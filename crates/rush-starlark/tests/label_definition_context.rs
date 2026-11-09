use rush_starlark::{parse, BzlLoader, Evaluator, GlobalBzlCache, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

static MODULE_LOAD_LOCK: Mutex<()> = Mutex::new(());

const MODULE: &str = r#"
Ctor = Label

def clean_dep(target):
    label = Label(target)
    if label.package.startswith("legacy/"):
        return "@relocated//" + label.package + ":" + label.name
    return str(label)

def nested():
    def inner():
        return str(Ctor("//cfg:mode"))
    return inner()

def with_default(target = Label("//cfg:mode")):
    return str(target)

def from_lambda():
    function = lambda target = Label("//cfg:mode"): str(target)
    return function()

def labels():
    return [
        clean_dep("//cfg:mode"),
        str(Label(":local")),
        str(Ctor(*["//cfg:mode"])),
        str(Label("@dep//cfg:mode")),
        nested(),
        with_default(),
        from_lambda(),
    ]

def declare(name):
    native.filegroup(name = name, srcs = [":" + native.package_name() + "_input"])

def broken():
    fail("from-module")
"#;

fn source_tree() -> (tempfile::TempDir, Arc<Mutex<BzlLoader>>) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("WORKSPACE"), "workspace(name = \"main\")\n").unwrap();
    let mut loader = BzlLoader::new_with_global_cache(root.path().to_path_buf(), GlobalBzlCache::new());
    for (owner, dependency) in [("owner_a", "dep_a"), ("owner_b", "dep_b")] {
        let repository = root.path().join(owner);
        std::fs::create_dir_all(repository.join("defs")).unwrap();
        std::fs::write(repository.join("defs/BUILD"), "").unwrap();
        std::fs::write(repository.join("defs/module.bzl"), MODULE).unwrap();
        loader.add_external_repo(owner.to_string(), repository);
        loader.set_repo_mapping_for(owner, HashMap::from([("dep".to_string(), dependency.to_string())]));
    }
    std::fs::write(
        root.path().join("bridge.bzl"),
        "load(\"@owner_a//defs:module.bzl\", _forwarded = \"labels\", _failed = \"broken\")\nforwarded = _forwarded\nfailed = _failed\n",
    ).unwrap();
    let consumer = root.path().join("consumer_repository");
    std::fs::create_dir_all(consumer.join("consumer")).unwrap();
    std::fs::write(consumer.join("consumer/BUILD"), "").unwrap();
    loader.add_external_repo("consumer".to_string(), consumer);
    loader.set_repo_mapping_for("consumer", HashMap::from([("dep".to_string(), "caller_dep".to_string())]));
    (root, Arc::new(Mutex::new(loader)))
}

fn caller(loader: &Arc<Mutex<BzlLoader>>) -> Evaluator {
    let mut evaluator = Evaluator::new();
    evaluator.set_current_repository(Some("consumer".to_string()));
    evaluator.set_current_package("consumer".to_string());
    evaluator.register_native_namespace();
    evaluator.set_bzl_loader(loader.clone());
    evaluator
}

fn expected(owner: &str, dependency: &str) -> Value {
    let condition = format!("@@{owner}//cfg:mode");
    Value::List(vec![
        Value::String(condition.clone()),
        Value::String(format!("@@{owner}//defs:local")),
        Value::String(condition.clone()),
        Value::String(format!("@@{dependency}//cfg:mode")),
        Value::String(condition.clone()),
        Value::String(condition.clone()),
        Value::String(condition),
    ])
}

#[test]
fn loaded_functions_retain_label_owners_across_nested_calls_aliases_and_cache_hits() {
    let _guard = MODULE_LOAD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (root, loader) = source_tree();
    rush_incremental::tracked::test_scope(root.path(), || {
        for _ in 0..2 {
            let mut evaluator = caller(&loader);
            evaluator.eval_program(&parse(r#"
load("@owner_a//defs:module.bzl", a = "labels", declare_a = "declare", clean = "clean_dep")
load("@owner_b//defs:module.bzl", b = "labels")
load("@@//:bridge.bzl", forwarded = "forwarded")
result_a = a()
result_b = b()
result_forwarded = forwarded()
relocated = clean("//legacy/path:item")
declare_a(name = "declared_from_macro")
"#).unwrap()).unwrap();
            assert_eq!(evaluator.get_var("result_a"), Some(expected("owner_a", "dep_a")));
            assert_eq!(evaluator.get_var("result_b"), Some(expected("owner_b", "dep_b")));
            assert_eq!(evaluator.get_var("result_forwarded"), evaluator.get_var("result_a"));
            assert_eq!(evaluator.get_var("relocated"), Some(Value::String("@relocated//legacy/path:item".to_string())));
            let calls = evaluator.captured_rule_calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].rule_type, "filegroup");
            assert_eq!(calls[0].kwargs["srcs"], Value::List(vec![Value::String(":consumer_input".to_string())]));
        }
    });
}

#[test]
fn a_failed_loaded_function_restores_the_callers_label_context() {
    let _guard = MODULE_LOAD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (root, loader) = source_tree();
    rush_incremental::tracked::test_scope(root.path(), || {
        let mut evaluator = caller(&loader);
        let error = evaluator.eval_program(&parse(
            "load(\"@@//:bridge.bzl\", failed = \"failed\")\nbefore = 42\nfailed()\n",
        ).unwrap()).unwrap_err();
        assert!(error.to_string().contains("from-module"));
        evaluator.eval_program(&parse("after = str(Label(\":caller\"))\n").unwrap()).unwrap();
        assert_eq!(evaluator.get_var("before"), Some(Value::Int(42)));
        assert_eq!(evaluator.get_var("after"), Some(Value::String("@@consumer//consumer:caller".to_string())));
    });
}

#[test]
fn top_level_build_labels_use_the_build_owners_mapping() {
    let _guard = MODULE_LOAD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (root, loader) = source_tree();
    rush_incremental::tracked::test_scope(root.path(), || {
        let mut evaluator = caller(&loader);
        evaluator.eval_program(&parse(
            "constructor = Label\ndirect = str(Label(\"@dep//cfg:mode\"))\naliased = str(constructor(\"@dep//cfg:mode\"))\nlocal = str(Label(\":local\"))\n",
        ).unwrap()).unwrap();
        let mapped = Some(Value::String("@@caller_dep//cfg:mode".to_string()));
        assert_eq!(evaluator.get_var("direct"), mapped);
        assert_eq!(evaluator.get_var("aliased"), mapped);
        assert_eq!(evaluator.get_var("local"), Some(Value::String("@@consumer//consumer:local".to_string())));
    });
}

#[test]
fn evaluator_context_setters_refresh_new_labels_while_preserving_existing_closures() {
    let _guard = MODULE_LOAD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (root, loader) = source_tree();
    rush_incremental::tracked::test_scope(root.path(), || {
        let mut evaluator = Evaluator::new();
        evaluator.set_bzl_loader(loader);
        evaluator.set_current_repository(Some("owner_a".to_string()));
        evaluator.set_current_package("defs".to_string());
        evaluator.eval_program(&parse(r#"
def from_a():
    return [str(Label(":local")), str(Label("@dep//cfg:mode"))]
a_initial = from_a()
"#).unwrap()).unwrap();
        let a = Value::List(vec![
            Value::String("@@owner_a//defs:local".to_string()),
            Value::String("@@dep_a//cfg:mode".to_string()),
        ]);
        assert_eq!(evaluator.get_var("a_initial"), Some(a.clone()));

        // Change only the repository, so this setter must retire the memo.
        evaluator.set_current_repository(Some("owner_b".to_string()));
        evaluator.eval_program(&parse(r#"
b_direct = [str(Label(":local")), str(Label("@dep//cfg:mode"))]
def from_b():
    return [str(Label(":local")), str(Label("@dep//cfg:mode"))]
a_after_repository = from_a()
b_initial = from_b()
"#).unwrap()).unwrap();
        let b = Value::List(vec![
            Value::String("@@owner_b//defs:local".to_string()),
            Value::String("@@dep_b//cfg:mode".to_string()),
        ]);
        assert_eq!(evaluator.get_var("b_direct"), Some(b.clone()));
        assert_eq!(evaluator.get_var("b_initial"), Some(b.clone()));
        assert_eq!(evaluator.get_var("a_after_repository"), Some(a.clone()));

        // Change only the package; previously captured functions keep theirs.
        evaluator.set_current_package("other".to_string());
        evaluator.eval_program(&parse(r#"
other_direct = str(Label(":local"))
def from_other():
    return [str(Label(":local")), str(Label("@dep//cfg:mode"))]
b_after_package = from_b()
other_initial = from_other()
"#).unwrap()).unwrap();
        let other = Value::List(vec![
            Value::String("@@owner_b//other:local".to_string()),
            Value::String("@@dep_b//cfg:mode".to_string()),
        ]);
        assert_eq!(evaluator.get_var("other_direct"), Some(Value::String("@@owner_b//other:local".to_string())));
        assert_eq!(evaluator.get_var("other_initial"), Some(other.clone()));
        assert_eq!(evaluator.get_var("b_after_package"), Some(b.clone()));

        // Replace only the loader at the same owner/package with a new map.
        let mut replacement = BzlLoader::new(root.path().to_path_buf());
        replacement.add_external_repo("owner_b".to_string(), root.path().join("owner_b"));
        replacement.set_repo_mapping_for("owner_b", HashMap::from([
            ("dep".to_string(), "replacement_dep".to_string()),
        ]));
        evaluator.set_bzl_loader(Arc::new(Mutex::new(replacement)));
        evaluator.eval_program(&parse(r#"
replacement_direct = str(Label("@dep//cfg:mode"))
def from_replacement():
    return [str(Label(":local")), str(Label("@dep//cfg:mode"))]
replacement_initial = from_replacement()
a_after_loader = from_a()
b_after_loader = from_b()
other_after_loader = from_other()
"#).unwrap()).unwrap();
        assert_eq!(evaluator.get_var("replacement_direct"), Some(Value::String("@@replacement_dep//cfg:mode".to_string())));
        assert_eq!(evaluator.get_var("replacement_initial"), Some(Value::List(vec![
            Value::String("@@owner_b//other:local".to_string()),
            Value::String("@@replacement_dep//cfg:mode".to_string()),
        ])));
        assert_eq!(evaluator.get_var("a_after_loader"), Some(a));
        assert_eq!(evaluator.get_var("b_after_loader"), Some(b));
        assert_eq!(evaluator.get_var("other_after_loader"), Some(other));
    });
}

#[test]
fn package_relative_labels_use_the_build_caller_while_label_uses_the_defining_module() {
    let _guard = MODULE_LOAD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (root, loader) = source_tree();
    std::fs::write(root.path().join("owner_a/defs/caller.bzl"), r#"
BuildLabel = native.package_relative_label

def labels_for_caller():
    definition = Label(":local")
    caller = native.package_relative_label(":local")
    caller_alias = BuildLabel("//cfg:mode")
    caller_dependency = BuildLabel("@dep//cfg:mode")
    definition_dependency = Label("@dep//cfg:mode")
    main = BuildLabel("@//cfg:mode")
    canonical_main = BuildLabel("@@//cfg:mode")
    main_from_label = Label("@//cfg:mode")
    dictionary = {main_from_label: "main identity"}
    return [str(definition), str(caller), str(caller_alias), str(caller_dependency), str(definition_dependency), str(main), dictionary[main], dictionary[canonical_main], BuildLabel(definition) == definition]
"#).unwrap();
    rush_incremental::tracked::test_scope(root.path(), || {
        for _ in 0..2 {
            let mut evaluator = caller(&loader);
            evaluator.eval_program(&parse(r#"
load("@@owner_a//defs:caller.bzl", "labels_for_caller")
result = labels_for_caller()
"#).unwrap()).unwrap();
            assert_eq!(evaluator.get_var("result"), Some(Value::List(vec![
                Value::String("@@owner_a//defs:local".to_string()),
                Value::String("@@consumer//consumer:local".to_string()),
                Value::String("@@consumer//cfg:mode".to_string()),
                Value::String("@@caller_dep//cfg:mode".to_string()),
                Value::String("@@dep_a//cfg:mode".to_string()),
                Value::String("@@//cfg:mode".to_string()),
                Value::String("main identity".to_string()),
                Value::String("main identity".to_string()),
                Value::Bool(true),
            ])));
        }
    });
}
