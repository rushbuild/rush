use rush_starlark::{parse, Evaluator, Value};

#[test]
fn tensorflow_git_configure_multiline_call_slice_preserves_the_expression() {
    // TensorFlow v2.17.0, third_party/git/git_configure.bzl, _git_conf_impl.
    // Preserve the actual expression which failed during WORKSPACE loading.
    // https://github.com/tensorflow/tensorflow/blob/v2.17.0/third_party/git/git_configure.bzl
    let authentic = r#"
tensorflow_root_path = str(repository_ctx.path(
    Label("@org_tensorflow//:BUILD"),
))[:-len("BUILD")]
"#;
    let compact = r#"tensorflow_root_path = str(repository_ctx.path(Label("@org_tensorflow//:BUILD")))[:-len("BUILD")]"#;
    assert_eq!(parse(authentic).unwrap(), parse(compact).unwrap());
}

#[test]
fn completed_postfix_delimiters_allow_same_line_chains() {
    for (multiline, compact) in [
        (
            "result = factory(\n    0,\n)(1)[2].name\n",
            "result = factory(0)(1)[2].name\n",
        ),
        (
            "result = values[\n    0\n][1].name\n",
            "result = values[0][1].name\n",
        ),
        (
            "result = values[\n    1:\n][0]\n",
            "result = values[1:][0]\n",
        ),
        ("result = [\n    1,\n    2,\n][0]\n", "result = [1, 2][0]\n"),
        ("result = (\n    1,\n    2,\n)[0]\n", "result = (1, 2)[0]\n"),
        (
            "result = {\n    'key': 1,\n}['key']\n",
            "result = {'key': 1}['key']\n",
        ),
    ] {
        assert_eq!(
            parse(multiline).unwrap(),
            parse(compact).unwrap(),
            "{multiline}"
        );
    }
}

#[test]
fn newline_separated_calls_and_lists_remain_separate_statements() {
    for (multiline, compact) in [
        (
            "value = factory(\n    1,\n)\n[0]\n",
            "value = factory(1)\n[0]\n",
        ),
        (
            "value = factory(\n    1,\n)\n(0)\n",
            "value = factory(1)\n(0)\n",
        ),
        (
            "value = items[\n    0\n]\n(1,)\n",
            "value = items[0]\n(1,)\n",
        ),
        ("value = [\n    1,\n]\n[0]\n", "value = [1]\n[0]\n"),
        ("value = (\n    1,\n)\n(2,)\n", "value = (1,)\n(2,)\n"),
    ] {
        let parsed = parse(multiline).unwrap();
        assert_eq!(parsed.statements.len(), 2, "{multiline}");
        assert_eq!(parsed, parse(compact).unwrap(), "{multiline}");
    }
}

#[test]
fn enclosing_delimiters_allow_postfix_continuation_across_lines() {
    for (multiline, compact) in [
        (
            "result = (factory(\n    1,\n)\n[0])\n",
            "result = factory(1)[0]\n",
        ),
        (
            "result = [factory(\n    1,\n)\n(2)]\n",
            "result = [factory(1)(2)]\n",
        ),
    ] {
        assert_eq!(
            parse(multiline).unwrap(),
            parse(compact).unwrap(),
            "{multiline}"
        );
    }
}

#[test]
fn malformed_postfix_chains_still_fail_parsing() {
    for source in [
        "result = factory(\n    1,\n)[:\n",
        "result = factory(\n    1,\n)[1:2:3:4]\n",
        "result = factory(\n    1,\n).\n",
        "result = factory(\n    1,\n][0]\n",
        "result = values[\n    0\n][:\n",
    ] {
        assert!(
            parse(source).is_err(),
            "accepted malformed expression: {source}"
        );
    }
}

#[test]
fn multiline_call_index_attribute_slice_chain_executes() {
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), || {
        let mut evaluator = Evaluator::new();
        evaluator
            .eval_program(
                &parse(
                    r#"
result = "root/BUILD".split(
    "/",
)[
    -1
].lower(
)[:-len("ILD")]
"#,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            evaluator.get_var("result"),
            Some(Value::String("bu".into()))
        );
    });
}
