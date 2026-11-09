//! Declaration-only compatibility for the actual pinned Bazel 6.5 .bzl API.
//! These tests never evaluate an extension implementation or fetch a repository.
use rush_starlark::{parse, starlark_to_rule_value, BzlLoader, Evaluator, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

const XCODE: &[u8] = include_bytes!("fixtures/bazel_6_5_0_module_extension/xcode_configure.bzl");
const LICENSE: &[u8] = include_bytes!("fixtures/bazel_6_5_0_module_extension/LICENSE");
const REPR: &str =
    "<unknown object com.google.devtools.build.lib.bazel.bzlmod.AutoValue_ModuleExtension>";
static LOAD_LOCK: Mutex<()> = Mutex::new(());

fn analysis(body: impl FnOnce(&Path)) {
    let _lock = LOAD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("WORKSPACE"), "").unwrap();
    rush_incremental::tracked::test_scope(root.path(), || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        body(root.path());
    });
}

fn loader(root: &Path, workspace: bool) -> Arc<Mutex<BzlLoader>> {
    let mut loader = BzlLoader::new(root.to_path_buf());
    loader.set_workspace_repository_context(workspace.then(|| root.to_path_buf()));
    Arc::new(Mutex::new(loader))
}

fn load(
    loader: &Arc<Mutex<BzlLoader>>,
    file: &str,
    symbols: &[&str],
) -> anyhow::Result<HashMap<String, Value>> {
    BzlLoader::load_bzl_from_evaluator(
        loader.clone(),
        &format!("@@//:{file}"),
        &symbols
            .iter()
            .map(|name| (name.to_string(), name.to_string()))
            .collect::<Vec<_>>(),
    )
}

fn evaluate(loader: &Arc<Mutex<BzlLoader>>, source: &str) -> anyhow::Result<Evaluator> {
    let mut evaluator = Evaluator::new();
    evaluator.register_native_namespace();
    evaluator.set_bzl_loader(loader.clone());
    evaluator.eval_program(&parse(source)?)?;
    Ok(evaluator)
}

#[test]
fn authentic_xcode_module_declares_extension_in_build_and_workspace_contexts() {
    assert_eq!(
        hex::encode(Sha256::digest(XCODE)),
        "2f3d8daf4b40a99800984c412681ca9cf415fadf167f8ac4643fc4edcfbceacd"
    );
    assert_eq!(
        hex::encode(Sha256::digest(LICENSE)),
        "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30"
    );
    for workspace in [false, true] {
        analysis(|root| {
            std::fs::write(root.join("xcode_configure.bzl"), XCODE).unwrap();
            let loader = loader(root, workspace);
            let first = load(
                &loader,
                "xcode_configure.bzl",
                &["xcode_configure_extension"],
            )
            .unwrap();
            let value = &first["xcode_configure_extension"];
            let Value::ModuleExtension(declaration) = value else {
                panic!("authentic Xcode API must preserve a typed declaration");
            };
            assert!(matches!(&declaration.implementation, Value::Lambda { .. }));
            assert!(declaration.tag_classes.is_empty());
            assert!(declaration.doc.is_empty());
            assert!(declaration.environ.is_empty());
            assert!(!declaration.os_dependent && !declaration.arch_dependent);
            assert!(declaration
                .source_context
                .as_deref()
                .is_some_and(|label| label.contains("xcode_configure.bzl")));
            assert_eq!(value.to_string(), REPR);
            assert!(value.estimate_size() > std::mem::size_of::<Value>());
            let warm = load(
                &loader,
                "xcode_configure.bzl",
                &["xcode_configure_extension"],
            )
            .unwrap();
            let Value::ModuleExtension(warm_declaration) = &warm["xcode_configure_extension"]
            else {
                unreachable!()
            };
            assert!(Arc::ptr_eq(declaration, warm_declaration));
        });
    }
}

#[test]
fn declarations_preserve_metadata_callable_identity_and_opaque_api() {
    analysis(|root| {
        std::fs::write(root.join("defs.bzl"), r#"
def impl(ctx):
    fail("the constructor must not execute this implementation")
ctor = module_extension
e = ctor(impl, tag_classes = {}, doc = "extension docs", environ = ("A", "B", "A"), os_dependent = True, arch_dependent = True)
alias = e
other = module_extension(impl)
builtin_impl = module_extension(len)
provider_impl = module_extension(provider())
constructor_impl = module_extension(module_extension)
info = [type(e), repr(e), dir(e), hasattr(e, "implementation"), getattr(e, "implementation", "absent"), e == alias, e == other]
def make():
    return module_extension(impl)
"#).unwrap();
        std::fs::write(
            root.join("reexport.bzl"),
            "load(\"@@//:defs.bzl\", original = \"e\")\nforwarded = original\n",
        )
        .unwrap();
        let loader = loader(root, false);
        let values = load(
            &loader,
            "defs.bzl",
            &[
                "e",
                "alias",
                "other",
                "builtin_impl",
                "provider_impl",
                "constructor_impl",
                "info",
            ],
        )
        .unwrap();
        assert_eq!(
            values["info"],
            Value::List(vec![
                Value::String("AutoValue_ModuleExtension".into()),
                Value::String(REPR.into()),
                Value::List(vec![]),
                Value::Bool(false),
                Value::String("absent".into()),
                Value::Bool(true),
                Value::Bool(false),
            ])
        );
        let Value::ModuleExtension(declaration) = &values["e"] else {
            unreachable!()
        };
        assert!(matches!(
            &declaration.implementation,
            Value::Function { .. }
        ));
        assert_eq!(declaration.doc, "extension docs");
        assert_eq!(declaration.environ, ["A", "B", "A"]);
        assert!(declaration.os_dependent && declaration.arch_dependent);
        assert!(
            matches!(&values["builtin_impl"], Value::ModuleExtension(decl) if matches!(&decl.implementation, Value::NativeFunction(name) if name == "len"))
        );
        assert!(
            matches!(&values["provider_impl"], Value::ModuleExtension(decl) if matches!(&decl.implementation, Value::Provider { .. }))
        );
        assert!(
            matches!(&values["constructor_impl"], Value::ModuleExtension(decl) if matches!(&decl.implementation, Value::NativeFunction(name) if name == "module_extension"))
        );
        let reexport = load(&loader, "reexport.bzl", &["forwarded"]).unwrap();
        assert_eq!(reexport["forwarded"], values["e"]);
        // Imported .bzl functions and an explicitly exported constructor retain
        // their real defining capability when called from BUILD.
        let imported = evaluate(
            &loader,
            "load(\"@@//:defs.bzl\", \"make\", \"ctor\")\na = make()\nb = ctor(len)\n",
        )
        .unwrap();
        assert!(matches!(
            imported.get_var("a"),
            Some(Value::ModuleExtension(_))
        ));
        assert!(matches!(
            imported.get_var("b"),
            Some(Value::ModuleExtension(_))
        ));
        assert!(starlark_to_rule_value(&values["e"])
            .unwrap_err()
            .to_string()
            .contains("module-extension declaration"));
    });
}

#[test]
fn constructor_is_predeclared_only_for_real_bzl_definitions() {
    analysis(|root| {
        let loader = loader(root, false);
        for prefix in [
            "",
            "label = Label(\"//:a\")\n",
            "def impl(ctx):\n    return None\n",
        ] {
            for call in [
                "value = module_extension\n",
                "value = module_extension(len)\n",
            ] {
                let error = evaluate(&loader, &format!("{prefix}{call}"))
                    .err()
                    .expect("direct BUILD must not expose the constructor");
                assert!(format!("{error:#}").contains("available only in .bzl definitions"));
            }
        }
        let mut workspace = Evaluator::new();
        workspace.set_workspace_repository_context(root.to_path_buf());
        let error = workspace
            .eval_program(&parse("value = module_extension(len)\n").unwrap())
            .unwrap_err();
        assert!(format!("{error:#}").contains("available only in .bzl definitions"));
        // User functions may shadow the predeclared name normally.
        std::fs::write(
            root.join("shadow.bzl"),
            "module_extension = lambda value: value\nresult = module_extension(42)\n",
        )
        .unwrap();
        assert_eq!(
            load(&loader, "shadow.bzl", &["result"]).unwrap()["result"],
            Value::Int(42)
        );
    });
}

#[test]
fn unsupported_execution_attributes_json_and_hashing_refuse_explicitly() {
    analysis(|root| {
        std::fs::write(root.join("defs.bzl"), "e = module_extension(len)\n").unwrap();
        let loader = loader(root, false);
        for (source, expected) in [
            ("e()", "module-extension declarations are not callable"),
            ("e.implementation", "have no attribute 'implementation'"),
            (
                "getattr(e, \"implementation\")",
                "have no attribute 'implementation'",
            ),
            (
                "json.encode(e)",
                "module-extension declarations are not JSON data values",
            ),
            ("{e: 1}", "unhashable type: 'AutoValue_ModuleExtension'"),
        ] {
            let error = evaluate(
                &loader,
                &format!("load(\"@@//:defs.bzl\", \"e\")\nresult = {source}\n"),
            )
            .err()
            .expect("unsupported operation must refuse");
            assert!(
                format!("{error:#}").contains(expected),
                "{source}: {error:#}"
            );
        }
    });
}

#[test]
fn constructor_validates_arguments_including_aliased_kwargs_without_last_wins() {
    analysis(|root| {
        let cases = [
            ("module_extension()", "missing required argument"),
            ("module_extension(1)", "must be a supported callable"),
            ("module_extension(len, len)", "keyword-only"),
            (
                "module_extension(len, unknown = True)",
                "unexpected keyword",
            ),
            (
                "module_extension(len, implementation = len)",
                "multiple values",
            ),
            (
                "module_extension(implementation = len, implementation = len)",
                "multiple values",
            ),
            (
                "module_extension(implementation = len, **{\"implementation\": len})",
                "multiple values",
            ),
            (
                "ctor(implementation = len, **{\"implementation\": len})",
                "multiple values",
            ),
            (
                "ctor(**{\"implementation\": len}, **{\"implementation\": len})",
                "multiple values",
            ),
            ("ctor(len, **{1: True})", "keyword names must be strings"),
            ("ctor(len, **[])", "kwargs must be a dict"),
            (
                "ctor(len, tag_classes = None)",
                "tag_classes must be a dict",
            ),
            (
                "ctor(len, tag_classes = {\"tag\": None})",
                "requires genuine tag_class",
            ),
            ("ctor(len, doc = 1)", "doc must be a string"),
            (
                "ctor(len, environ = None)",
                "environ must be a list or tuple",
            ),
            ("ctor(len, environ = [1])", "environ must contain strings"),
            ("ctor(len, os_dependent = 1)", "os_dependent must be a bool"),
            (
                "ctor(len, arch_dependent = \"True\")",
                "arch_dependent must be a bool",
            ),
            // A compatibility RuleFunction marker is not an implementation.
            (
                "ctor(rule(implementation = len))",
                "compatibility rule markers",
            ),
        ];
        let loader = loader(root, false);
        for (index, (expression, expected)) in cases.into_iter().enumerate() {
            let file = format!("bad_{index}.bzl");
            std::fs::write(
                root.join(&file),
                format!("ctor = module_extension\nresult = {expression}\n"),
            )
            .unwrap();
            let error = load(&loader, &file, &["result"]).unwrap_err();
            assert!(
                format!("{error:#}").contains(expected),
                "{expression}: {error:#}"
            );
        }
    });
}
