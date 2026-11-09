use rush_starlark::{parse, Evaluator, Value};

// Constructor/sequence contract: Bazel 6.5.0 MethodLibrary.java (tuple,
// enumerate, zip), Starlark.java (toIterable, ordering), and Tuple.java.
// https://github.com/bazelbuild/bazel/blob/6.5.0/src/main/java/net/starlark/java/eval/MethodLibrary.java
// Strings are not StarlarkIterable. Tuple dictionary keys retain their kind;
// list values never become hashable merely because their AST resembles a tuple.
fn scoped(test: impl FnOnce()) {
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), test);
}

fn evaluate(source: &str) -> Evaluator {
    let mut evaluator = Evaluator::new();
    evaluator.eval_program(&parse(source).unwrap()).unwrap();
    evaluator
}

fn tuple(values: &[i64]) -> Value {
    Value::Tuple(values.iter().copied().map(Value::Int).collect())
}

#[test]
fn tuple_constructor_and_literals_preserve_sequence_kind() {
    scoped(|| {
        let evaluator = evaluate(
            r#"
empty = tuple()
one = tuple([1])
literal = (1,)
converted = tuple([1, 2])
unchanged = tuple(converted)
listed = list(converted)
tuple_type = type(converted)
list_type = type(listed)
different = converted != listed
empty_repr = repr(empty)
one_repr = repr(one)
many_repr = repr(converted)
truth = [bool(empty), bool(one), len(converted)]
"#,
        );
        for (name, expected) in [
            ("empty", tuple(&[])),
            ("one", tuple(&[1])),
            ("literal", tuple(&[1])),
            ("converted", tuple(&[1, 2])),
            ("unchanged", tuple(&[1, 2])),
            ("listed", Value::List(vec![Value::Int(1), Value::Int(2)])),
            ("tuple_type", Value::String("tuple".into())),
            ("list_type", Value::String("list".into())),
            ("different", Value::Bool(true)),
            ("empty_repr", Value::String("()".into())),
            ("one_repr", Value::String("(1,)".into())),
            ("many_repr", Value::String("(1, 2)".into())),
            (
                "truth",
                Value::List(vec![Value::Bool(false), Value::Bool(true), Value::Int(2)]),
            ),
        ] {
            assert_eq!(evaluator.get_var(name), Some(expected), "{name}");
        }
    });
}

#[test]
fn tuple_rejects_wrong_arity_keywords_noniterables_and_mutation() {
    scoped(|| {
        for (source, error) in [
            ("result = tuple([], [])", "at most 1"),
            ("result = tuple(x = [])", "keyword"),
            ("result = tuple(\"abc\")", "must be iterable"),
            ("result = tuple(None)", "must be iterable"),
            ("result = tuple(7)", "must be iterable"),
            ("result = tuple(True)", "must be iterable"),
            ("t = (1, 2)\nt[0] = 3", "indexed assignment"),
            ("t = (1,)\nt.append(2)", "Attribute access"),
            ("d = {tuple([[1]]): 0}", "unhashable type: 'list'"),
            ("result = (1,) + [2]", "Cannot add"),
            ("result = (1,) < [2]", "unsupported comparison"),
            (
                "result = ({\"a\": 1}, 0) < ({\"a\": 2}, 1)",
                "unsupported comparison",
            ),
            ("result = (None, 0) < (False, 1)", "unsupported comparison"),
            ("result = \"foo.py\".endswith([\".py\"])", "string or tuple"),
        ] {
            let mut evaluator = Evaluator::new();
            let actual = evaluator
                .eval_program(&parse(&format!("{source}\n")).unwrap())
                .unwrap_err()
                .to_string();
            assert!(actual.contains(error), "{source}: {actual}");
        }
    });
}

#[test]
fn tuple_iteration_operators_and_slices_retain_immutable_kind() {
    scoped(|| {
        let evaluator = evaluate(
            r#"
t = (1, 2, 3)
first = t[0]
last = t[-1]
sliced = t[1:]
reversed = t[::-1]
reverse_stop = t[2:0:-1]
empty_slice = t[-9:-9:-1]
concatenated = t + (4,)
repeated = 2 * (4, 5)
negative = t * -1
members = [2 in t, 4 not in t]
ordered = [(1, 2) < (1, 3), (1, 2) < (1, 2, 0), (2,) > (1, 99)]
equal_prefix = [(None, 1) < (None, 2), ({"a": 1}, 1) < ({"a": 1}, 2), ([1], 1) < ([1], 2), (1, 1) < (1.0, 2)]
comprehension = [v + 1 for v in t]
"#,
        );
        for (name, expected) in [
            ("first", Value::Int(1)),
            ("last", Value::Int(3)),
            ("sliced", tuple(&[2, 3])),
            ("reversed", tuple(&[3, 2, 1])),
            ("reverse_stop", tuple(&[3, 2])),
            ("empty_slice", tuple(&[])),
            ("concatenated", tuple(&[1, 2, 3, 4])),
            ("repeated", tuple(&[4, 5, 4, 5])),
            ("negative", tuple(&[])),
            ("members", Value::List(vec![Value::Bool(true); 2])),
            ("ordered", Value::List(vec![Value::Bool(true); 3])),
            ("equal_prefix", Value::List(vec![Value::Bool(true); 4])),
            (
                "comprehension",
                Value::List(vec![Value::Int(2), Value::Int(3), Value::Int(4)]),
            ),
        ] {
            assert_eq!(evaluator.get_var(name), Some(expected), "{name}");
        }
    });
}

#[test]
fn tuple_equality_uses_recursive_starlark_numeric_semantics() {
    scoped(|| {
        let evaluator = evaluate(
            r#"
equal = [(1,) == (1.0,), ((1,),) == ((1.0,),), ([1],) == ([1.0],), ({"n": 1},) == ({"n": 1.0},)]
different = [(True,) != (1,), (1,) != (2,), (1,) != (1, 2), (9007199254740993,) != (9007199254740992.0,)]
membership = [1.0 in (1,), (1.0,) in ((1,),), 2.0 not in (1,)]
"#,
        );
        for name in ["equal", "different"] {
            assert_eq!(
                evaluator.get_var(name),
                Some(Value::List(vec![Value::Bool(true); 4])),
                "{name}"
            );
        }
        assert_eq!(
            evaluator.get_var("membership"),
            Some(Value::List(vec![Value::Bool(true); 3]))
        );
    });
}

#[test]
fn tuple_dictionary_keys_round_trip_through_variables_and_iterables() {
    scoped(|| {
        let evaluator = evaluate(
            r#"
key = (1, ("é:2", None), Label("//pkg:target"))
d = {key: "present"}
lookup = d[tuple([1, ("é:2", None), Label("//pkg:target")])]
keys = tuple(d)
items = d.items()
present = key in d
comprehension = {k: v for k, v in d.items()}
comprehension_lookup = comprehension[key]
or_key = ("//cfg:a", "//conditions:default")
or_dict = {or_key: [":dep"]}
or_length = len(or_dict)
"#,
        );
        let key = evaluator.get_var("key").unwrap();
        assert_eq!(
            evaluator.get_var("comprehension_lookup"),
            Some(Value::String("present".into()))
        );
        assert_eq!(
            evaluator.get_var("lookup"),
            Some(Value::String("present".into()))
        );
        assert_eq!(
            evaluator.get_var("keys"),
            Some(Value::Tuple(vec![key.clone()]))
        );
        assert_eq!(
            evaluator.get_var("items"),
            Some(Value::List(vec![Value::Tuple(vec![
                key,
                Value::String("present".into())
            ])]))
        );
        assert_eq!(evaluator.get_var("present"), Some(Value::Bool(true)));
        assert_eq!(evaluator.get_var("or_length"), Some(Value::Int(1)));
    });
}

#[test]
fn tuple_starargs_and_standard_pair_iterators_keep_typed_tuples() {
    scoped(|| {
        let evaluator = evaluate(
            r#"
def collect(*args):
    return args
args = collect(*(1, 2))
empty_args = collect()
pairs = zip((1, 2), (3, 4))
numbered = enumerate((4, 5))
minimum = min((7, 2, 9))
maximum = max((7, 2, 9))
ordered = sorted((3, 1, 2))
truth = [all((1, True)), any((0, False))]
suffix = "main.py".endswith(tuple([".py", ".py3"]))
prefix = "main.py".startswith(("other", "main"))
joined = "-".join(("a", "b"))
formatted = "%s/%s" % ("a", "b")
"#,
        );
        assert_eq!(evaluator.get_var("args"), Some(tuple(&[1, 2])));
        assert_eq!(evaluator.get_var("empty_args"), Some(tuple(&[])));
        assert_eq!(
            evaluator.get_var("pairs"),
            Some(Value::List(vec![tuple(&[1, 3]), tuple(&[2, 4])]))
        );
        assert_eq!(
            evaluator.get_var("numbered"),
            Some(Value::List(vec![tuple(&[0, 4]), tuple(&[1, 5])]))
        );
        assert_eq!(evaluator.get_var("minimum"), Some(Value::Int(2)));
        assert_eq!(evaluator.get_var("maximum"), Some(Value::Int(9)));
        assert_eq!(
            evaluator.get_var("ordered"),
            Some(Value::List(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(3)
            ]))
        );
        assert_eq!(
            evaluator.get_var("truth"),
            Some(Value::List(vec![Value::Bool(true), Value::Bool(false)]))
        );
        for name in ["suffix", "prefix"] {
            assert_eq!(evaluator.get_var(name), Some(Value::Bool(true)));
        }
        assert_eq!(
            evaluator.get_var("joined"),
            Some(Value::String("a-b".into()))
        );
        assert_eq!(
            evaluator.get_var("formatted"),
            Some(Value::String("a/b".into()))
        );
    });
}

#[test]
fn skylib_1_3_0_parse_bazel_version_expression_runs_unchanged() {
    // TensorFlow 2.17 tensorflow/workspace3.bzl pins bazel-skylib 1.3.0:
    // archive SHA256 74d544d96f4a5bb630d465ca8bbcfe231e3594e5aae57e1edbf17a6eb3ca2506.
    // lib/versions.bzl SHA256 0342cb6a9e699b36c9ce5bc31c6568872a48fe99f55a6ff3e5326088fc18cb88.
    // https://github.com/bazelbuild/bazel-skylib/blob/1.3.0/lib/versions.bzl#L55
    // Preserve the actual failing return expression, not a list replacement.
    scoped(|| {
        let evaluator = evaluate(
            r#"
def parse_version(version):
    return tuple([int(n) for n in version.split(".")])
actual = parse_version("6.5.0")
minimum = actual >= parse_version("6.4.0")
maximum = actual < parse_version("7.0.0")
"#,
        );
        assert_eq!(evaluator.get_var("actual"), Some(tuple(&[6, 5, 0])));
        assert_eq!(evaluator.get_var("minimum"), Some(Value::Bool(true)));
        assert_eq!(evaluator.get_var("maximum"), Some(Value::Bool(true)));
    });
}

#[test]
fn tuple_equality_composes_through_containers_membership_and_removal() {
    scoped(|| {
        let evaluator = evaluate(
            r#"
a = (1,)
b = (1.0,)
comparisons = [[a] == [b], {"key": a} == {"key": b}, b in [a], b not in [(2,)]]
values = [a]
values.remove(b)
remaining = len(values)
"#,
        );
        assert_eq!(
            evaluator.get_var("comparisons"),
            Some(Value::List(vec![Value::Bool(true); 4]))
        );
        assert_eq!(evaluator.get_var("remaining"), Some(Value::Int(0)));
    });
}

#[test]
fn tuple_mixed_numeric_ordering_preserves_integer_precision() {
    scoped(|| {
        let evaluator = evaluate(
            r#"
ordered = [
    (9007199254740993,) > (9007199254740992.0,),
    (9007199254740992.0,) < (9007199254740993,),
    (1,) < (1.5,),
    (-1,) > (-1.5,),
    ([9007199254740993],) > ([9007199254740992.0],),
    (9223372036854775807,) < (9223372036854775808.0,),
    (-9223372036854775807 - 1,) == (-9223372036854775808.0,),
]
"#,
        );
        assert_eq!(
            evaluator.get_var("ordered"),
            Some(Value::List(vec![Value::Bool(true); 7]))
        );
    });
}

#[test]
fn tuple_slots_stay_immutable_while_contained_mutations_reach_callers() {
    scoped(|| {
        let evaluator = evaluate(
            r#"
list_holder = ([1],)
list_holder[0][0] = 2
dict_holder = ({"key": 1},)
dict_holder[0]["key"] = 3

def mutate_list(t):
    t[0][0] = 4

def mutate_dict(t):
    t[0]["key"] = 5

def change_element_type(t):
    t[0][0] = 1.0

mutate_list(list_holder)
mutate_dict(dict_holder)
list_result = list_holder[0][0]
dict_result = dict_holder[0]["key"]
typed_holder = ([1],)
change_element_type(typed_holder)
element_type = type(typed_holder[0][0])
"#,
        );
        assert_eq!(evaluator.get_var("list_result"), Some(Value::Int(4)));
        assert_eq!(evaluator.get_var("dict_result"), Some(Value::Int(5)));
        assert_eq!(
            evaluator.get_var("element_type"),
            Some(Value::String("float".into()))
        );
        let mut evaluator = Evaluator::new();
        let error = evaluator
            .eval_program(&parse("t = ((1,),)\nt[0][0] = 2\n").unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("indexed assignment"), "{error}");
    });
}

#[test]
fn exports_files_accepts_tuple_visibility_through_both_call_routes() {
    scoped(|| {
        let mut evaluator = Evaluator::new();
        evaluator.register_native_namespace();
        evaluator
            .eval_program(
                &parse(
                    r#"
visibility = ("//visibility:public",)
exports_files(("first.h",), visibility = visibility)
native.exports_files(("second.h",), visibility = visibility)
"#,
                )
                .unwrap(),
            )
            .unwrap();
        let calls = evaluator.get_rule_calls();
        assert_eq!(calls.len(), 2);
        for (call, name) in calls.iter().zip(["first.h", "second.h"]) {
            assert_eq!(call.kwargs.get("name"), Some(&Value::String(name.into())));
            assert_eq!(
                call.kwargs.get("visibility"),
                Some(&Value::List(vec![Value::String(
                    "//visibility:public".into()
                )]))
            );
        }
    });
}
