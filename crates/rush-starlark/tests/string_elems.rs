// SPDX-License-Identifier: Apache-2.0
// Bazel 6.5 StringModule.java elems() returns a frozen StarlarkList.
// https://github.com/bazelbuild/bazel/blob/6.5.0/src/main/java/net/starlark/java/eval/StringModule.java
// ASCII is represented exactly. Non-ASCII byte strings require a separate
// representation change and deliberately fail instead of being replaced.
use rush_starlark::{parse, Evaluator, Value};

fn scoped(test: impl FnOnce()) {
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), test);
}

fn evaluate(source: &str) -> anyhow::Result<Evaluator> {
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
fn string_elems_returns_exact_ascii_frozen_list_with_sequence_reads() {
    scoped(|| {
        let evaluator = evaluate(
            r#"
e = "1.18.4".elems()
shape = type(e) == "list" and len(e) == 6 and e == ["1", ".", "1", "8", ".", "4"]
indexing = e[0] == "1" and e[-1] == "4" and e[1:3] == [".", "1"]
iteration = [element for element in e] == list(e)
sequence = tuple(e) == ("1", ".", "1", "8", ".", "4") and "8" in e
json_works = json.decode(json.encode(e)) == ["1", ".", "1", "8", ".", "4"]
empty = type("".elems()) == "list" and not "".elems() and len("".elems()) == 0
copy = list(e)
copy.append("copy")
sliced = e[:]
sliced.append("slice")
copies = len(copy) == 7 and len(sliced) == 7 and len(e) == 6
"#,
        )
        .unwrap();
        expect_true(
            &evaluator,
            &[
                "shape",
                "indexing",
                "iteration",
                "sequence",
                "json_works",
                "empty",
                "copies",
            ],
        );
    });
}

#[test]
fn string_elems_bound_alias_getattr_and_zero_arity_contract() {
    scoped(|| {
        let evaluator = evaluate(
            r#"
method = "ab".elems
alias = method
bound = alias() == ["a", "b"]
attributes = hasattr("ab", "elems") and getattr("ab", "elems")() == ["a", "b"]
empty_unpack = "ab".elems(*[], **{}) == ["a", "b"]
def elements(*args):
    return args
star = elements(*"ab".elems()) == ("a", "b")
"#,
        )
        .unwrap();
        expect_true(&evaluator, &["bound", "attributes", "empty_unpack", "star"]);
        for expression in [
            "\"ab\".elems(1)",
            "\"ab\".elems(self = \"other\")",
            "\"ab\".elems(extra = True)",
            "\"ab\".elems(*[1])",
            "\"ab\".elems(**{\"extra\": True})",
            "getattr(\"ab\", \"elems\")(1)",
        ] {
            let error = evaluate(expression)
                .err()
                .expect("extra argument must fail");
            assert!(
                error.to_string().contains("elems() accepts no arguments"),
                "{expression}: {error:#}"
            );
        }
    });
}

#[test]
fn string_elems_results_reject_in_place_mutation() {
    scoped(|| {
        for statement in [
            "e.append(\"x\")",
            "e.clear()",
            "e.pop()",
            "e[0] = \"x\"",
            "e += [\"x\"]",
            "\"abc\".elems().append(\"x\")",
        ] {
            let source = format!("e = \"abc\".elems()\n{statement}\n");
            assert!(evaluate(&source).is_err(), "must reject {statement}");
        }
    });
}

#[test]
fn string_elems_refuses_unrepresentable_non_ascii_byte_elements() {
    scoped(|| {
        for text in ["é", "😀", "ASCIIé", "é"] {
            let source = format!("result = \"{text}\".elems()\n");
            let error = evaluate(&source)
                .err()
                .expect("non-ASCII byte model must fail closed");
            assert!(
                error.to_string().contains("non-ASCII byte elements"),
                "{text:?}: {error:#}"
            );
        }
    });
}

#[test]
fn authentic_rules_go_sdk_parser_accepts_required_ascii_versions() {
    use sha2::{Digest, Sha256};
    assert_eq!(hex::encode(Sha256::digest(include_bytes!("fixtures/tensorflow_2_17_0_rules_go/parse_version.bzl"))),
        "0805255c92106c1e55e0f752c0ea83577d71b2d61e7e65c935e0465cabe14768");
    assert_eq!(hex::encode(Sha256::digest(include_bytes!("fixtures/tensorflow_2_17_0_rules_go/LICENSE.txt"))),
        "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30");
    scoped(|| {
        let source = format!(
            "{}\n{}",
            include_str!("fixtures/tensorflow_2_17_0_rules_go/parse_version.bzl"),
            r#"
required = _parse_version("1.18.4") == (1, 18, 4)
minor = _parse_version("1.18") == (1, 18, 0)
prerelease = _parse_version("1.18.4rc1") == (1, 18, 4, "rc1")
invalid = _parse_version("1..4") == None and _parse_version(".18.4") == None and _parse_version("1") == None
"#
        );
        let evaluator = evaluate(&source).unwrap();
        expect_true(&evaluator, &["required", "minor", "prerelease", "invalid"]);
    });
}
