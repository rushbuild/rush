//! BUILD caller repository identity, independently of module Label ownership.
use rush_starlark::{parse, BzlLoader, Evaluator, Value};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

fn write(path: &Path, source: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn build_evaluator(root: &Path, repo: Option<&str>, loader: Arc<Mutex<BzlLoader>>) -> Evaluator {
    let mut evaluator = Evaluator::new();
    evaluator.register_native_namespace();
    evaluator.set_current_repository(repo.map(str::to_owned));
    evaluator.set_current_package("pkg".into());
    evaluator.set_current_package_dir(Some(root.join("pkg")));
    evaluator.set_bzl_loader(loader);
    evaluator
}

#[test]
fn repository_name_uses_build_caller_across_shared_owners_nested_macros_and_warm_modules() {
    let root = tempfile::tempdir().unwrap();
    write(
        &root.path().join("WORKSPACE"),
        "workspace(name=\"named_main\")\n",
    );
    let shared = root.path().join("shared");
    write(
        &shared.join("defs/module.bzl"),
        r#"
DEFINITION = Label(":definition")
caller = native.repository_name
def pkg_path_name():
    # Exercise nsync/bazel/pkg_path_name.bzl's package-path expression.
    return "./" + Label(native.repository_name() + "//" + native.package_name() + ":nsync").workspace_root + "/" + native.package_name()
def report():
    return [native.repository_name(), caller(), str(DEFINITION), str(Label(":inside")), pkg_path_name()]
"#,
    );
    write(&root.path().join("bridge.bzl"),"load(\"@owner_a//defs:module.bzl\", _a=\"report\")\nload(\"@owner_b//defs:module.bzl\", _b=\"report\")\ndef a():\n    return _a()\ndef b():\n    return _b()\n");
    let mut loader = BzlLoader::new(root.path().to_owned());
    for owner in ["owner_a", "owner_b", "libmod~override"] {
        loader.add_external_repo(owner.into(), shared.clone());
        loader.set_repo_mapping_for(
            owner,
            std::collections::HashMap::from([
                ("owner_a".into(), "owner_a".into()),
                ("owner_b".into(), "owner_b".into()),
                ("libmod~override".into(), "libmod~override".into()),
            ]),
        );
    }
    let loader = Arc::new(Mutex::new(loader));
    rush_incremental::tracked::test_scope(root.path(), || {
        for _ in 0..2 {
            for owner in [
                None,
                Some("owner_a"),
                Some("owner_b"),
                Some("libmod~override"),
            ] {
                let mut evaluator = build_evaluator(root.path(), owner, loader.clone());
                evaluator
                    .eval_program(
                        &parse(
                            r#"
load("@@//:bridge.bzl","a","b")
left = a()
right = b()
bare = repository_name()
namespace = native.repository_name()
empty_unpack = native.repository_name(*[], **{})
"#,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                let name = format!("@{}", owner.unwrap_or(""));
                for (variable, defining_owner) in [("left", "owner_a"), ("right", "owner_b")] {
                    let path = if let Some(owner) = owner {
                        format!("./external/{owner}/pkg")
                    } else {
                        ".//pkg".into()
                    };
                    assert_eq!(
                        evaluator.get_var(variable),
                        Some(Value::List(vec![
                            Value::String(name.clone()),
                            Value::String(name.clone()),
                            Value::String(format!("@@{defining_owner}//defs:definition")),
                            Value::String(format!("@@{defining_owner}//defs:inside")),
                            Value::String(path),
                        ]))
                    );
                }
                for name_var in ["bare", "namespace", "empty_unpack"] {
                    assert_eq!(
                        evaluator.get_var(name_var),
                        Some(Value::String(name.clone()))
                    );
                }
            }
        }
    });
}

#[test]
fn repository_name_refuses_nonbuild_phases_bare_module_names_and_arguments() {
    let root = tempfile::tempdir().unwrap();
    write(&root.path().join("WORKSPACE"), "");
    write(
        &root.path().join("native_top.bzl"),
        "VALUE = native.repository_name()\n",
    );
    write(
        &root.path().join("bare_top.bzl"),
        "VALUE = repository_name()\n",
    );
    write(
        &root.path().join("bare_function.bzl"),
        "def report():\n    return repository_name()\n",
    );
    write(
        &root.path().join("allowed.bzl"),
        "caller=native.repository_name\ndef report():\n    return caller()\n",
    );
    for (name,source) in [
        ("unused_default", "def unused(value=native.repository_name()):\n    return value\nVALUE=1\n"),
        ("indirect_default", "def helper():\n    return native.repository_name()\ndef unused(value=helper()):\n    return value\nVALUE=1\n"),
        ("lambda_default", "unused=lambda value=native.repository_name():value\nVALUE=1\n"),
    ] { write(&root.path().join(format!("{name}.bzl")),source); }

    let mut registered = BzlLoader::new(root.path().to_owned());
    for owner in ["owner_a", "owner_b"] {
        let source = root.path().join(owner);
        write(&source.join("pkg/BUILD"), "");
        registered.add_external_repo(owner.into(), source);
        registered.set_repo_mapping_for(
            owner,
            std::collections::HashMap::from([
                ("owner_a".into(), "owner_a".into()),
                ("owner_b".into(), "owner_b".into()),
            ]),
        );
    }
    let loader = Arc::new(Mutex::new(registered));
    rush_incremental::tracked::test_scope(root.path(), || {
        for (file, needle) in [
            ("native_top.bzl", "requires a BUILD loading context"),
            ("bare_top.bzl", "Undefined variable"),
        ] {
            let mut evaluator = build_evaluator(root.path(), None, loader.clone());
            let error = evaluator
                .eval_program(&parse(&format!("load(\"//:{file}\",\"VALUE\")\n")).unwrap())
                .unwrap_err();
            assert!(format!("{error:#}").contains(needle), "{error:#}");
        }
        let mut evaluator = build_evaluator(root.path(), None, loader.clone());
        let error = evaluator
            .eval_program(&parse("load(\"//:bare_function.bzl\",\"report\")\nreport()\n").unwrap())
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("Undefined variable"),
            "{error:#}"
        );
        for source in [
            "native.repository_name(1)",
            "native.repository_name(name=\"x\")",
            "native.repository_name(*[1])",
            "native.repository_name(**{\"x\":1})",
            "repository_name(1)",
        ] {
            let mut evaluator = build_evaluator(root.path(), None, loader.clone());
            let error = evaluator.eval_program(&parse(source).unwrap()).unwrap_err();
            assert!(
                format!("{error:#}").contains("accepts no arguments"),
                "{source}: {error:#}"
            );
        }
        for name in ["unused_default", "indirect_default", "lambda_default"] {
            let mut evaluator = build_evaluator(root.path(), None, loader.clone());
            let error = evaluator
                .eval_program(&parse(&format!("load(\"//:{name}.bzl\",\"VALUE\")\n")).unwrap())
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("requires a BUILD loading context"),
                "{name}: {error:#}"
            );
            assert!(
                evaluator.get_var("VALUE").is_none(),
                "unused invalid defaults cannot publish module exports"
            );
        }
        // A real BUILD-defined default runs in BUILD phase, once at its
        // definition, while the function body still sees its later caller.
        let mut evaluator = build_evaluator(root.path(), Some("owner_a"), loader.clone());
        evaluator.eval_program(&parse("def local(value=repository_name()):\n    return (value,native.repository_name())\n").unwrap()).unwrap();
        evaluator.set_current_repository(Some("owner_b".into()));
        evaluator
            .eval_program(&parse("captured=local()\noverridden=local(\"explicit\")\n").unwrap())
            .unwrap();
        assert_eq!(
            evaluator.get_var("captured"),
            Some(Value::Tuple(vec![
                Value::String("@owner_a".into()),
                Value::String("@owner_b".into())
            ]))
        );
        assert_eq!(
            evaluator.get_var("overridden"),
            Some(Value::Tuple(vec![
                Value::String("explicit".into()),
                Value::String("@owner_b".into())
            ]))
        );
        let mut workspace = Evaluator::new();
        workspace.register_workspace_native_namespace();
        workspace.set_workspace_repository_context(root.path().to_owned());
        workspace.set_bzl_loader(loader.clone());
        let error = workspace
            .eval_program(&parse("load(\"//:allowed.bzl\",\"report\")\nreport()\n").unwrap())
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("requires a BUILD loading context"),
            "{error:#}"
        );
        let direct = workspace
            .eval_program(&parse("native.repository_name()\n").unwrap())
            .unwrap_err();
        assert!(
            format!("{direct:#}").contains("requires a BUILD loading context"),
            "{direct:#}"
        );
        assert!(workspace.get_var("repository_name").is_none());
        // Neither registering a namespace nor setting a repository creates a
        // BUILD thread. This also models a rule-analysis evaluator's phase.
        let mut module = Evaluator::new();
        module.register_native_namespace();
        module.set_current_repository(Some("owner_a".into()));
        let error = module
            .eval_program(&parse("native.repository_name()\n").unwrap())
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("requires a BUILD loading context"),
            "{error:#}"
        );
        assert!(module.get_var("repository_name").is_none());
    });
}

#[test]
fn changing_build_package_preserves_user_binding_and_keeps_native_alias_dynamic() {
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), || {
        let mut e = Evaluator::new();
        e.register_native_namespace();
        e.set_current_repository(Some("owner_a".into()));
        e.set_current_package_dir(Some(root.path().join("a")));
        e.eval_program(&parse("saved_native=native.repository_name\ndef repository_name():\n    return \"custom\"\ndef wrapper():\n    return repository_name()\n").unwrap()).unwrap();
        e.set_current_repository(Some("owner_b".into()));
        e.set_current_package_dir(Some(root.path().join("b")));
        e.eval_program(
            &parse("user=repository_name()\nwrapped=wrapper()\nnative=saved_native()\n").unwrap(),
        )
        .unwrap();
        assert_eq!(e.get_var("user"), Some(Value::String("custom".into())));
        assert_eq!(e.get_var("wrapped"), Some(Value::String("custom".into())));
        assert_eq!(e.get_var("native"), Some(Value::String("@owner_b".into())));
    });
}
