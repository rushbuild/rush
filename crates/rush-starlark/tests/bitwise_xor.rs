use rush_starlark::{parse, Evaluator, Lexer, TokenType, Value};

// rules_jvm_external 4.3, coursier.bzl:289, as loaded by TensorFlow 2.17.
// https://github.com/bazelbuild/rules_jvm_external/blob/4.3/coursier.bzl#L289
// Source SHA256: f9412be6299e432537a787097fe8af6ceba48926d63306ced97555223347c790.
const COURSIER_SIGNATURE_EXPRESSION: &str =
    "hash(repr(sorted(artifact_inputs))) ^ hash(repr(sorted(repositories)))";

fn evaluate(source: &str) -> Evaluator {
    let mut evaluator = Evaluator::new();
    evaluator.eval_program(&parse(source).unwrap()).unwrap();
    evaluator
}

#[test]
fn xor_has_its_own_precedence_and_left_associativity() {
    for (source, grouped) in [
        ("1 | 2 ^ 3", "1 | (2 ^ 3)"),
        ("1 ^ 1 + 1", "1 ^ (1 + 1)"),
        ("2 * 3 ^ 1", "(2 * 3) ^ 1"),
        ("5 ^ 3 == 6", "(5 ^ 3) == 6"),
        ("not 5 ^ 3", "not (5 ^ 3)"),
        ("0 or 1 ^ 3", "0 or (1 ^ 3)"),
        ("7 ^ 7 and 13", "(7 ^ 7) and 13"),
        ("1 ^ 2 ^ 3", "(1 ^ 2) ^ 3"),
        ("-1 ^ 3", "(-1) ^ 3"),
    ] {
        assert_eq!(
            parse(&format!("result = {source}\n")).unwrap(),
            parse(&format!("result = {grouped}\n")).unwrap(),
            "{source}"
        );
    }
}

#[test]
fn xor_reaches_the_existing_integer_evaluator() {
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), || {
        for (source, expected) in [
            ("1 | 2 ^ 3", Value::Int(1)),
            ("1 ^ 1 + 1", Value::Int(3)),
            ("2 * 3 ^ 1", Value::Int(7)),
            ("5 ^ 3 == 6", Value::Bool(true)),
            ("not 5 ^ 3", Value::Bool(false)),
            ("0 or 1 ^ 3", Value::Int(2)),
            ("7 ^ 7 and 13", Value::Int(0)),
            ("(1 | 2) ^ 3", Value::Int(0)),
            ("-1 ^ 3", Value::Int(-4)),
        ] {
            let evaluator = evaluate(&format!("result = {source}\n"));
            assert_eq!(evaluator.get_var("result"), Some(expected), "{source}");
        }
    });
}

#[test]
fn xor_rejects_noninteger_operands_without_coercion() {
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), || {
        for (source, types) in [
            ("1.5 ^ 2", "float and int"),
            ("2 ^ 1.5", "int and float"),
            ("True ^ 2", "bool and int"),
            ("2 ^ False", "int and bool"),
            ("[1] ^ [2]", "list and list"),
            ("[1] ^ 2", "list and int"),
            ("2 ^ [1]", "int and list"),
            ("\"1\" ^ 2", "string and int"),
            ("2 ^ \"1\"", "int and string"),
        ] {
            let mut evaluator = Evaluator::new();
            let error = evaluator
                .eval_program(&parse(&format!("result = {source}\n")).unwrap())
                .unwrap_err()
                .to_string();
            assert!(error.contains("Operator ^ requires integer operands"), "{error}");
            assert!(error.contains(types), "{error}");
            assert!(evaluator.get_var("result").is_none());
        }
    });
}

#[test]
fn coursier_signature_expression_parses_and_executes_unchanged() {
    let source = format!(
        "def signature(artifact_inputs, repositories):\n    return {COURSIER_SIGNATURE_EXPRESSION}\n\
         artifacts = [\"z:b:1\", \"a:b:2\"]\n\
         repositories = [\"https://repo.example/maven2\"]\n\
         left = hash(repr(sorted(artifacts)))\n\
         right = hash(repr(sorted(repositories)))\n\
         result = signature(artifacts, repositories)\n\
         reordered = signature([\"a:b:2\", \"z:b:1\"], repositories)\n"
    );
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), || {
        let evaluator = evaluate(&source);
        let Some(Value::Int(left)) = evaluator.get_var("left") else {
            panic!("artifact signature must be an integer");
        };
        let Some(Value::Int(right)) = evaluator.get_var("right") else {
            panic!("repository signature must be an integer");
        };
        assert_eq!(evaluator.get_var("result"), Some(Value::Int(left ^ right)));
        assert_eq!(evaluator.get_var("result"), evaluator.get_var("reordered"));
    });
}

#[test]
fn missing_xor_operand_returns_a_parse_error() {
    assert!(parse("result = 1 ^\n").is_err());
    assert!(parse("result = ^ 1\n").is_err());
    let mut lexer = Lexer::new("^");
    assert_eq!(lexer.next_token().token_type, TokenType::Caret);
    assert_eq!(lexer.next_token().token_type, TokenType::Eof);
}
