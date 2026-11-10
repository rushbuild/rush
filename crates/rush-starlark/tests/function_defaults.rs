//! Definition-time default values. This intentionally retains the evaluator's
//! existing owned list/dict cloning model; shared mutable identity and module
//! container freezing are separate limitations, not claims of these controls.
use rush_starlark::{parse, BzlLoader, Evaluator, Value};
use std::sync::{Arc,Mutex};

#[test]
fn function_and_lambda_defaults_capture_values_once_before_any_call() {
    let root=tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(),|| {
        let mut e=Evaluator::new();e.register_native_namespace();e.set_current_package_dir(Some(root.path().to_owned()));
        e.eval_program(&parse(r#"
def capture(name):
    native.filegroup(name=name)
    return name
def factory():
    captured="before"
    def named(a=capture("first"),b=capture("second"),global_value=captured,typed=(1,[2],{"k":3})):
        return (a,b,global_value,typed)
    captured="lambda_time"
    closure = lambda value=capture("lambda"), global_value=captured: (value,global_value)
    captured="after"
    return (named,closure)
functions=factory()
named=functions[0]
closure=functions[1]
"#).unwrap()).unwrap();
        assert_eq!(e.get_rule_calls().iter().map(|r|r.kwargs["name"].clone()).collect::<Vec<_>>(),[
            Value::String("first".into()),Value::String("second".into()),Value::String("lambda".into())]);
        e.eval_program(&parse("a=named()\nb=named()\noverride=named(a=\"replacement\")\nc=closure()\nd=closure(\"explicit\")\n").unwrap()).unwrap();
        let expected=Value::Tuple(vec![Value::String("first".into()),Value::String("second".into()),Value::String("before".into()),
            Value::Tuple(vec![Value::Int(1),Value::List(vec![Value::Int(2)]),Value::Dict(std::collections::HashMap::from([("k".into(),Value::Int(3))]))])]);
        assert_eq!(e.get_var("a"),Some(expected.clone()));assert_eq!(e.get_var("b"),Some(expected));
        let Value::Tuple(overridden)=e.get_var("override").unwrap() else {panic!("typed default tuple lost")};
        assert_eq!(overridden[0],Value::String("replacement".into()));
        assert_eq!(e.get_var("c"),Some(Value::Tuple(vec![Value::String("lambda".into()),Value::String("lambda_time".into())])));
        assert_eq!(e.get_var("d"),Some(Value::Tuple(vec![Value::String("explicit".into()),Value::String("lambda_time".into())])));
        assert_eq!(e.get_rule_calls().len(),3,"calls/overrides must not evaluate default AST again");
    });
}

#[test]
fn never_called_and_overridden_defaults_still_fail_at_definition_including_indirect_calls() {
    let root=tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(),|| {
    for source in [
        "def unused(value=fail(\"default failure\")):\n    return value\n",
        "def helper():\n    fail(\"default failure\")\ndef unused(value=helper()):\n    return value\n",
        "def unused(value=fail(\"default failure\")):\n    return value\nunused(\"override\")\n",
        "unused=lambda value=fail(\"default failure\"): value\n",
    ] {
        let mut e=Evaluator::new();
        let error=e.eval_program(&parse(source).unwrap()).unwrap_err();
        assert!(format!("{error:#}").contains("default failure"),"{error:#}");
        assert!(e.get_var("unused").is_none(),"a failed default cannot publish a function");
    }
    });
}

#[test]
fn defaults_resolve_definition_scope_before_parameter_binding_and_global_rebinding() {
    let root=tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(),|| {
    let mut e=Evaluator::new();
    e.eval_program(&parse(r#"
def factory():
    parameter="definition"
    def named(parameter="argument",value=parameter):
        return value
    parameter="changed"
    return named
named=factory()
result=named()
"#).unwrap()).unwrap();
    assert_eq!(e.get_var("result"),Some(Value::String("definition".into())));
    });
}

#[test]
fn loaded_module_default_failures_propagate_before_export_or_explicit_override() {
    let root=tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("WORKSPACE"),"").unwrap();
    for (name,source) in [
        ("direct","def unused(value=fail(\"module default failure\")):\n    return value\n"),
        ("indirect","def helper():\n    fail(\"module default failure\")\ndef unused(value=helper()):\n    return value\n"),
    ] {
        std::fs::write(root.path().join(format!("{name}.bzl")),source).unwrap();
        let loader=Arc::new(Mutex::new(BzlLoader::new(root.path().to_owned())));
        rush_incremental::tracked::test_scope(root.path(),|| {
            let mut e=Evaluator::new();e.register_native_namespace();e.set_current_package_dir(Some(root.path().to_owned()));e.set_bzl_loader(loader);
            let error=e.eval_program(&parse(&format!("load(\"//:{name}.bzl\",\"unused\")\nunused(\"override\")\n")).unwrap()).unwrap_err();
            assert!(format!("{error:#}").contains("module default failure"),"{error:#}");
            assert!(e.get_var("unused").is_none());
        });
    }
}

#[test]
fn stored_default_values_participate_in_size_estimate_and_value_census() {
    let root=tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(),|| {
    let mut e=Evaluator::new();
    e.eval_program(&parse("def named(value=\"default payload\"):\n    return value\nclosure=lambda value=\"lambda payload\":value\n").unwrap()).unwrap();
    let named=e.get_var("named").unwrap();let closure=e.get_var("closure").unwrap();
    let Value::Function {params,..}=&named else {panic!("function missing")};
    assert_eq!(params[0].1,Some(Value::String("default payload".into())));
    let Value::Lambda {params,..}=&closure else {panic!("lambda missing")};
    assert_eq!(params[0].1,Some(Value::String("lambda payload".into())));
    assert!(named.estimate_size()>"default payload".len());
    assert!(closure.estimate_size()>"lambda payload".len());
    // Census recursively includes defaults and remains cycle-safe while these
    // functions retain their shared module environment.
    let census=rush_starlark::evaluator::ValueCensus::of_values([&named,&closure].into_iter());
    assert!(census.string_bytes >= "default payload".len()+"lambda payload".len());
    });
}

#[test]
fn replacing_retained_functions_and_lambdas_preserves_default_value_representation_changes() {
    let root=tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(),|| {
        for declarations in [
            "def a(v=x[0]):\n    return type(v[0])\nx[0]=(1.0,)\ndef b(v=x[0]):\n    return type(v[0])\n",
            "a=lambda v=x[0]:type(v[0])\nx[0]=(1.0,)\nb=lambda v=x[0]:type(v[0])\n",
        ] {
            let mut e=Evaluator::new();
            e.eval_program(&parse(&format!("x=[(1,)]\n{declarations}semantic_equal=(a==b)\nholder=[a]\ndef replace(items):\n    items[0]=b\nreplace(holder)\nresult=holder[0]()\n")).unwrap()).unwrap();
            // Preserve the existing user equality contract. Internal mutation
            // detection must nevertheless distinguish the typed defaults.
            assert_eq!(e.get_var("semantic_equal"),Some(Value::Bool(true)));
            assert_eq!(e.get_var("result"),Some(Value::String("float".into())));
        }
    });
}
