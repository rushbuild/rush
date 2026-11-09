// SPDX-License-Identifier: Apache-2.0
// Native introspection contract: Bazel 6.5 StarlarkNativeModule.java and
// StarlarkNativeModuleApi.java; verified with the pinned WORKSPACE/BUILD oracle.
use rush_starlark::{parse, BzlLoader, Evaluator, GlobalBzlCache, Value};
use std::sync::{Arc, Mutex};

fn scoped(test: impl FnOnce()) {
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), test);
}

fn eval(source: &str) -> anyhow::Result<Evaluator> {
    let mut evaluator = Evaluator::new();
    evaluator.register_native_namespace();
    evaluator.eval_program(&parse(source)?)?;
    Ok(evaluator)
}

fn expect_true(evaluator: &Evaluator, names: &[&str]) {
    for name in names {
        assert_eq!(evaluator.get_var(name), Some(Value::Bool(true)), "{name}");
    }
}

#[test]
fn existing_rule_exact_name_arity_type_and_alias_calls() {
    scoped(|| {
        let evaluator = eval(
            r#"
native.filegroup(name = "present")
lookup = native.existing_rule
present = lookup(*["present"])["name"] == "present"
missing = lookup("missing") == None
empty = lookup("") == None
colon = lookup(":present") == None
absolute = lookup("//:present") == None
"#,
        )
        .unwrap();
        expect_true(
            &evaluator,
            &["present", "missing", "empty", "colon", "absolute"],
        );
        for expression in [
            "native.existing_rule()",
            "native.existing_rule(1)",
            "native.existing_rule(None)",
            "native.existing_rule(True)",
            "native.existing_rule(\"x\", \"y\")",
            "native.existing_rule(name = \"x\")",
            "native.existing_rule(\"x\", name = \"x\")",
            "native.existing_rule(**{\"name\": \"x\"})",
            "native.existing_rules(\"x\")",
            "native.existing_rules(name = \"x\")",
        ] {
            let error = eval(expression).err().expect("invalid lookup must fail");
            assert!(
                error.to_string().contains("existing_rule"),
                "{expression}: {error:#}"
            );
        }
    });
}

#[test]
fn existing_rule_metadata_views_are_identity_backed_and_read_only() {
    scoped(|| {
        let evaluator = eval(r#"
native.filegroup(name = "present", srcs = ["data.txt"], tags = ["data.txt"], testonly = True)
r = native.existing_rule("present")
a = r
p = native.existing_rules()
map_type = type(r) == "Map" and type(p) == "Map"
identity = a == r and r != native.existing_rule("present") and r != p["present"]
fields = r["name"] == "present" and r["kind"] == "filegroup" and r["testonly"]
labels = r["srcs"] == (":data.txt",) and r["tags"] == ("data.txt",)
defaults = r["data"] == () and type(r["srcs"]) == "tuple"
methods = dir(r) == ["get", "items", "keys", "values"]
read_methods = r.get("absent", default = 17) == 17 and r.get(7) == None
members = "name" in r and "absent" not in r and 7 not in r
list_results = type(r.keys()) == "list" and type(r.values()) == "list" and type(r.items()) == "list"
rows = type(r.items()[0]) == "tuple"
iterations = len([key for key in r]) == len(r) and "present" in [value["name"] for value in p.values()]
get_alias = getattr(r, "get")("name") == "present" and hasattr(r, "keys") and not hasattr(r, "update")
"#).unwrap();
        expect_true(
            &evaluator,
            &[
                "map_type",
                "identity",
                "fields",
                "labels",
                "defaults",
                "methods",
                "read_methods",
                "members",
                "list_results",
                "rows",
                "iterations",
                "get_alias",
            ],
        );
    });
}

#[test]
fn existing_rule_snapshots_copy_and_source_file_boundary() {
    scoped(|| {
        let evaluator = eval(
            r#"
native.filegroup(name = "present", srcs = ["data.txt"])
native.exports_files(["data.txt"])
snapshot = native.existing_rules()
r = native.existing_rule("present")
copied = dict(r)
copied["name"] = "changed"
keys = list(r)
keys.append("copy-only")
sliced = r.keys()[:]
sliced.append("slice-only")
joined = r.keys() + ["joined-only"]
joined.append("mutable")
repeated = r.keys() * 2
repeated.append("mutable")
native.filegroup(name = "later")
original = r["name"] == "present" and native.existing_rule("present")["name"] == "present"
stable = "later" not in snapshot and "later" in native.existing_rules()
source_excluded = native.existing_rule("data.txt") == None and "data.txt" not in snapshot
copy_works = copied["name"] == "changed" and "copy-only" not in r and len(sliced) == len(r) + 1
encoded = json.decode(json.encode(r))
json_works = encoded["name"] == "present" and encoded["srcs"] == [":data.txt"]
"#,
        )
        .unwrap();
        expect_true(
            &evaluator,
            &[
                "original",
                "stable",
                "source_excluded",
                "copy_works",
                "json_works",
            ],
        );
    });
}

#[test]
fn existing_rule_all_exposed_collections_reject_mutation() {
    scoped(|| {
        for statement in [
            "r[\"name\"] = \"changed\"",
            "r.update({\"name\": \"changed\"})",
            "r[\"srcs\"].append(\"changed\")",
            "r[\"srcs\"][0] = \"changed\"",
            "r.keys().append(\"changed\")",
            "r.values().pop()",
            "r.items().clear()",
            "k = r.keys()\nk[0] = \"changed\"",
            "k = r.keys()\nk += [\"changed\"]",
            "p[\"present\"] = None",
            "p[\"present\"][\"name\"] = \"changed\"",
        ] {
            let source = format!("native.filegroup(name = \"present\", srcs = [\"data.txt\"])\nr = native.existing_rule(\"present\")\np = native.existing_rules()\n{statement}\n");
            assert!(eval(&source).is_err(), "must refuse: {statement}");
        }
        let mut evaluator = Evaluator::new();
        evaluator.register_workspace_native_namespace();
        for statement in [
            "r[\"repo_mapping\"][\"dep\"] = \"other\"",
            "r[\"repo_mapping\"].update({\"dep\": \"other\"})",
        ] {
            let source = format!("native.local_repository(name=\"present\", path=\"dep\", repo_mapping={{}})\nr=native.existing_rule(\"present\")\n{statement}\n");
            assert!(
                evaluator.eval_program(&parse(&source).unwrap()).is_err(),
                "{statement}"
            );
        }
    });
}

#[test]
fn existing_rule_frozen_dictionary_read_methods_make_mutable_copies() {
    scoped(|| {
        let mut evaluator = Evaluator::new();
        evaluator.register_workspace_native_namespace();
        evaluator
            .eval_program(
                &parse(
                    r#"
native.local_repository(name = "present", path = "dep", repo_mapping = {})
r = native.existing_rule("present")
kind = type(r["repo_mapping"]) == "dict"
keys = r["repo_mapping"].keys()
values = r["repo_mapping"].values()
items = r["repo_mapping"].items()
keys.append("copy-key")
values.append("copy-value")
items.append(("copy-key", "copy-value"))
copies = keys == ["copy-key"] and values == ["copy-value"] and items == [("copy-key", "copy-value")]
original = len(r["repo_mapping"]) == 0
"#,
                )
                .unwrap(),
            )
            .unwrap();
        expect_true(&evaluator, &["kind", "copies", "original"]);
    });
}

#[test]
fn authentic_tensorflow_apple_guard_preserves_existing_and_declares_absent_repo() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("WORKSPACE"),
        "workspace(name = \"main\")\n",
    )
    .unwrap();
    let apple = root.path().join("apple_source");
    let tools = root.path().join("tools_source");
    std::fs::create_dir_all(apple.join("apple")).unwrap();
    std::fs::create_dir_all(tools.join("tools/build_defs/repo")).unwrap();
    std::fs::write(apple.join("apple/BUILD"), "").unwrap();
    std::fs::write(
        apple.join("apple/repositories.bzl"),
        include_bytes!("fixtures/tensorflow_2_17_0_rules_apple/repositories.bzl"),
    )
    .unwrap();
    // Declaration-only unit seam: the authentic guard remains unchanged. This
    // test performs neither a download nor a repository implementation call.
    std::fs::write(tools.join("tools/build_defs/repo/BUILD"), "").unwrap();
    std::fs::write(
        tools.join("tools/build_defs/repo/http.bzl"),
        "http_archive = native.http_archive\n",
    )
    .unwrap();
    std::fs::write(root.path().join("BUILD"), "").unwrap();
    std::fs::write(root.path().join("bridge.bzl"), "load(\"@apple_rules//apple:repositories.bzl\", _original = \"apple_rules_dependencies\")\nrenamed = _original\n").unwrap();
    let mut loader =
        BzlLoader::new_with_global_cache(root.path().to_path_buf(), GlobalBzlCache::new());
    loader.set_workspace_repository_context(Some(root.path().to_path_buf()));
    loader.add_external_repo("apple_rules".to_string(), apple);
    loader.add_external_repo("bazel_tools".to_string(), tools);
    let loader = Arc::new(Mutex::new(loader));
    rush_incremental::tracked::test_scope(root.path(), || {
        for present in [true, false] {
            for _warm in 0..2 {
                let mut evaluator = Evaluator::new();
                evaluator.register_workspace_native_namespace();
                evaluator.set_bzl_loader(loader.clone());
                let prefix = if present {
                    "native.http_archive(name = \"bazel_skylib\", urls = [\"original-pin\"])\n"
                } else {
                    ""
                };
                let source =
                    format!("load(\"//:bridge.bzl\", alias = \"renamed\")\n{prefix}alias()\n");
                evaluator.eval_program(&parse(&source).unwrap()).unwrap();
                let calls: Vec<_> = evaluator
                    .get_rule_calls()
                    .iter()
                    .filter(|call| {
                        call.kwargs.get("name") == Some(&Value::String("bazel_skylib".into()))
                    })
                    .collect();
                assert_eq!(calls.len(), 1, "present={present}; warm={_warm}");
                let Some(Value::List(urls)) = calls[0].kwargs.get("urls") else {
                    panic!("real declaration URLs missing")
                };
                if present {
                    assert_eq!(urls, &vec![Value::String("original-pin".into())]);
                } else {
                    assert!(urls
                        .iter()
                        .any(|value| value.to_string().contains("bazel-skylib-1.3.0.tar.gz")));
                }
            }
        }
    });
}

#[test]
fn existing_rules_accessors_create_fresh_rule_views_but_assigned_aliases_keep_identity() {
    scoped(|| {
        let evaluator = eval(
            r#"
native.filegroup(name = "present")
p = native.existing_rules()
a = p["present"]
alias = a
aliases = alias == a
indexes = p["present"] != p["present"]
gets = p.get("present") != p.get("present")
values = p.values()[0] != p.values()[0]
items = p.items()[0][1] != p.items()[0][1]
copied = dict(p)
other = dict(p)
copies = copied["present"] != other["present"] and copied["present"] == copied["present"]
default_alias = p.get("missing", a) == a
metadata = p["present"]["name"] == a["name"] and p.get("present")["kind"] == a["kind"]
"#,
        )
        .unwrap();
        expect_true(
            &evaluator,
            &[
                "aliases",
                "indexes",
                "gets",
                "values",
                "items",
                "copies",
                "default_alias",
                "metadata",
            ],
        );
    });
}

#[test]
fn existing_rule_frozen_dictionary_expansion_and_typed_bridge_keep_dictionary_values() {
    scoped(|| {
        let mut evaluator = Evaluator::new();
        evaluator.register_workspace_native_namespace();
        // Match the production WORKSPACE loader's registered bare-rule path.
        evaluator.register_rule_function("local_repository".to_string());
        evaluator
            .eval_program(
                &parse(
                    r#"
native.local_repository(name = "present", path = "dep", repo_mapping = {"dep": "@owner"})
r = native.existing_rule("present")
m = r["repo_mapping"]
def unpacked(**kwargs):
    return kwargs
copied = dict(**m)
called = unpacked(**m)
literal = {**m}
expansion = copied["dep"] == "@owner" and called["dep"] == "@owner" and literal["dep"] == "@owner"
copied["dep"] = "@changed"
unchanged = m["dep"] == "@owner"
native.local_repository(name = "copy", path = "other", repo_mapping = m)
local_repository(name = "bare_copy", path = "other", repo_mapping = m)
p = native.existing_rules()
"#,
                )
                .unwrap(),
            )
            .unwrap();
        expect_true(&evaluator, &["expansion", "unchanged"]);
        let mapping = evaluator.get_var("m").unwrap();
        assert_eq!(
            rush_starlark::starlark_to_rule_value(&mapping).unwrap(),
            rush_starlark::RuleValue::Dict(std::collections::HashMap::from([(
                "dep".to_string(),
                rush_starlark::RuleValue::String("@owner".to_string())
            ),])),
        );
        for name in ["copy", "bare_copy"] {
            let declaration = evaluator
                .get_rule_calls()
                .iter()
                .find(|call| call.kwargs.get("name") == Some(&Value::String(name.to_string())))
                .unwrap();
            assert!(
                matches!(declaration.kwargs.get("repo_mapping"), Some(Value::Dict(values))
                if values.get("dep") == Some(&Value::String("@owner".to_string()))),
                "{name}"
            );
        }
        for name in ["r", "p"] {
            let error = rush_starlark::starlark_to_rule_value(&evaluator.get_var(name).unwrap())
                .unwrap_err();
            assert!(error.to_string().contains("opaque"));
        }
    });
}

#[test]
fn existing_rule_map_keyword_expansion_is_read_only_and_rejects_duplicate_names() {
    scoped(|| {
        let evaluator = eval(
            r#"
native.filegroup(name = "present")
r = native.existing_rule("present")
p = native.existing_rules()
def consume(**kwargs):
    return kwargs
copied = dict(**r)
called = consume(**r)
positive = copied["name"] == "present" and called["name"] == "present"
plural_copy = dict(**p)
plural_alias = plural_copy["present"] == plural_copy["present"]
plural_fresh = dict(**p)["present"] != dict(**p)["present"]
"#,
        )
        .unwrap();
        expect_true(&evaluator, &["positive", "plural_alias", "plural_fresh"]);
        for expression in [
            "dict(name = \"explicit\", **r)",
            "dict(**r, **{\"name\": \"late\"})",
            "dict(**{\"name\": \"early\"}, **r)",
            "consume(name = \"explicit\", **r)",
            "consume(**r, **{\"name\": \"late\"})",
            "named(\"positional\", **r)",
            "native.filegroup(name = \"explicit\", **r)",
        ] {
            let source = format!("native.filegroup(name = \"present\")\nr = native.existing_rule(\"present\")\ndef consume(**kwargs):\n    return kwargs\ndef named(name, **kwargs):\n    return name\n{expression}\n");
            let error = eval(&source).err().expect("duplicate keyword must fail");
            assert!(
                error.to_string().contains("multiple values"),
                "{expression}: {error:#}"
            );
        }
        let mut evaluator = Evaluator::new();
        evaluator.register_workspace_native_namespace();
        let source = "native.local_repository(name = \"present\", path = \"dep\", repo_mapping = {\"dep\": \"@owner\"})\nm = native.existing_rule(\"present\")[\"repo_mapping\"]\ndict(dep = \"duplicate\", **m)\n";
        assert!(evaluator
            .eval_program(&parse(source).unwrap())
            .unwrap_err()
            .to_string()
            .contains("multiple values"));
    });
}

#[test]
fn existing_rule_frozen_dictionary_update_sources_copy_without_mutating_the_snapshot() {
    scoped(|| {
        let mut evaluator = Evaluator::new();
        evaluator.register_workspace_native_namespace();
        evaluator
            .eval_program(
                &parse(
                    r#"
native.local_repository(name = "present", path = "dep", repo_mapping = {"dep": "@owner"})
r = native.existing_rule("present")
m = r["repo_mapping"]
positional = {"dep": "@old", "keep": "retained"}
positional.update(m, added = "keyword")
expanded = {"dep": "@old", "keep": "retained"}
expanded.update(added = "keyword", **m)
starred = {}
starred.update(*[m])
ordinary = {"dep": "@old", "keep": "retained"}
ordinary.update({"dep": "@owner"}, added = "keyword")
ordinary_expanded = {"dep": "@old", "keep": "retained"}
ordinary_expanded.update(added = "keyword", **{"dep": "@owner"})
copies_match = positional == ordinary and expanded == ordinary_expanded and starred == {"dep": "@owner"}
fields_retained = positional["dep"] == "@owner" and positional["keep"] == "retained" and positional["added"] == "keyword"
positional["dep"] = "@changed"
source_unchanged = m["dep"] == "@owner" and r["repo_mapping"]["dep"] == "@owner" and native.existing_rule("present")["repo_mapping"]["dep"] == "@owner"
rule_copy = {}
rule_copy.update(r)
rule_fields = rule_copy["name"] == "present" and rule_copy["path"] == "dep"
plural_copy = {}
plural_copy.update(native.existing_rules())
child = plural_copy["present"]
child_alias = child == plural_copy["present"] and child != native.existing_rules()["present"]
"#,
                )
                .unwrap(),
            )
            .unwrap();
        expect_true(
            &evaluator,
            &[
                "copies_match",
                "fields_retained",
                "source_unchanged",
                "rule_fields",
                "child_alias",
            ],
        );
    });
}

#[test]
fn existing_rule_frozen_dictionary_update_source_errors_leave_target_unchanged() {
    scoped(|| {
        for (statement, expected) in [
            ("target.update(m, m)", "at most one positional"),
            ("target.update(7)", "argument must be a dict"),
            ("target.update([m])", "argument must be a dict"),
            ("target.update(**7)", "Cannot unpack non-dict"),
            (
                "target.update(dep = 'duplicate', **m)",
                "multiple values for keyword argument",
            ),
            (
                "target.update(**m, **{'dep': 'duplicate'})",
                "multiple values for keyword argument",
            ),
            (
                "target.update(**{'dep': 'duplicate'}, **m)",
                "multiple values for keyword argument",
            ),
            (
                "native.local_repository(name='malformed', path='dep', repo_mapping={7: 'bad-key'})\nbad=native.existing_rule('malformed')['repo_mapping']\ntarget.update(**bad)",
                "keyword argument names must be strings",
            ),
        ] {
            let mut evaluator = Evaluator::new();
            evaluator.register_workspace_native_namespace();
            evaluator
                .eval_program(
                    &parse(
                        r#"
native.local_repository(name = "present", path = "dep", repo_mapping = {"dep": "@owner"})
m = native.existing_rule("present")["repo_mapping"]
target = {"keep": "retained"}
"#,
                    )
                    .unwrap(),
                )
                .unwrap();
            let error = evaluator
                .eval_program(&parse(statement).unwrap())
                .unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "{statement}: {error:#}"
            );
            evaluator
                .eval_program(
                    &parse(
                        "target_unchanged = target == {'keep': 'retained'}\nsource_unchanged = m['dep'] == '@owner'\n",
                    )
                    .unwrap(),
                )
                .unwrap();
            expect_true(&evaluator, &["target_unchanged", "source_unchanged"]);
        }
        for statement in ["m.update({})", "m.update(**{})"] {
            let mut evaluator = Evaluator::new();
            evaluator.register_workspace_native_namespace();
            evaluator
                .eval_program(
                    &parse(
                        "native.local_repository(name='present', path='dep', repo_mapping={})\nm=native.existing_rule('present')['repo_mapping']\n",
                    )
                    .unwrap(),
                )
                .unwrap();
            assert!(
                evaluator
                    .eval_program(&parse(statement).unwrap())
                    .unwrap_err()
                    .to_string()
                    .contains("cannot mutate"),
                "{statement}"
            );
        }
    });
}

#[test]
fn existing_rule_frozen_dictionary_update_keeps_the_ordinary_bound_method_boundary() {
    scoped(|| {
        let mut evaluator = Evaluator::new();
        evaluator.register_workspace_native_namespace();
        // Expression and aliased BoundMethod updates validate arguments but do
        // not mutate even for ordinary Dict sources. Keep that separate limit
        // explicit rather than treating source support as a mutation repair.
        evaluator
            .eval_program(
                &parse(
                    r#"
native.local_repository(name = "present", path = "dep", repo_mapping = {"dep": "@owner"})
m = native.existing_rule("present")["repo_mapping"]
ordinary = {}
from_frozen = {}
expanded = {}
aliased = {}
a = ordinary.update({"dep": "@owner"})
b = from_frozen.update(m)
c = expanded.update(**m)
update = aliased.update
d = update(m)
returns_none = a == None and b == None and c == None and d == None
same_boundary = ordinary == {} and from_frozen == {} and expanded == {} and aliased == {}
source_unchanged = m["dep"] == "@owner"
"#,
                )
                .unwrap(),
            )
            .unwrap();
        expect_true(
            &evaluator,
            &["returns_none", "same_boundary", "source_unchanged"],
        );
    });
}
