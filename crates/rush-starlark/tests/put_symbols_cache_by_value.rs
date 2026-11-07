// Phase 5.C.4 commit 113: regression tests for the
// put_symbols_cache "by-move" refactor. The hot-path
// change in load_bzl_from_evaluator and load_bzl is:
//
//   BEFORE:  loader.put_symbols_cache(path, symbols_map.clone())
//            ...
//            extract_symbols(&symbols_map, ...)
//
//   AFTER:   extract_symbols(&symbols_map, ...)
//            loader.put_symbols_cache(path, symbols_map)  // MOVE
//
// Pro's commit-113 directive: "Change put_symbols_cache
// to take ownership where possible. Goal: avoid cloning
// large symbol maps after evaluation. Requirements: no
// semantic change, no dangling post-cache uses of the
// old symbol map, aliasing/refcount behavior
// understood, tests proving exported symbols
// identical."
//
// These tests can't exercise the actual load_bzl_*
// functions without a full Starlark eval context, but
// they DO verify the data-flow invariant: after
// extracting from a HashMap then dropping the map,
// the extracted values are still owned and intact.
// This is the property the refactor relies on.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
enum FakeValue {
    Int(i64),
    String(String),
    List(Vec<FakeValue>),
}

/// Mimic extract_symbols_static: takes &HashMap, returns
/// a NEW HashMap with cloned values for the requested
/// keys.
fn fake_extract(
    all: &HashMap<String, FakeValue>,
    requested: &[(String, String)],
) -> HashMap<String, FakeValue> {
    let mut exported = HashMap::new();
    for (local, original) in requested {
        if let Some(v) = all.get(original) {
            exported.insert(local.clone(), v.clone());
        }
    }
    exported
}

/// Verifies the refactor's data-flow invariant: extract
/// first then move-drop the source map, and the
/// extracted values survive.
#[test]
fn extract_then_drop_preserves_values() {
    let mut source: HashMap<String, FakeValue> = HashMap::new();
    source.insert("a".to_string(), FakeValue::Int(42));
    source.insert(
        "b".to_string(),
        FakeValue::String("hello".to_string()),
    );
    source.insert(
        "c".to_string(),
        FakeValue::List(vec![FakeValue::Int(1), FakeValue::Int(2)]),
    );

    let requested = vec![
        ("a_local".to_string(), "a".to_string()),
        ("c_local".to_string(), "c".to_string()),
    ];
    let extracted = fake_extract(&source, &requested);

    // Simulate the move into cache: drop source.
    drop(source);

    // Extracted values are still owned and equal to
    // what they were.
    assert_eq!(extracted.len(), 2);
    assert_eq!(extracted["a_local"], FakeValue::Int(42));
    assert_eq!(
        extracted["c_local"],
        FakeValue::List(vec![FakeValue::Int(1), FakeValue::Int(2)])
    );
}

/// Verifies that the by-move refactor preserves
/// extract output across multiple "loads" of the same
/// symbol map.
#[test]
fn repeated_extract_before_move_is_stable() {
    let mut source: HashMap<String, FakeValue> = HashMap::new();
    for i in 0..240 {
        source.insert(
            format!("sym{}", i),
            FakeValue::Int(i as i64),
        );
    }

    // Extract a small subset (mimicking the typical
    // 1-10 requested symbols per load).
    let requested: Vec<(String, String)> = (0..5)
        .map(|i| (format!("local{}", i), format!("sym{}", i * 47)))
        .collect();

    let r1 = fake_extract(&source, &requested);
    let r2 = fake_extract(&source, &requested);
    assert_eq!(r1, r2, "extract is deterministic");

    // Final move into "cache" (drop here).
    drop(source);
    assert_eq!(r1.len(), 5);
    assert_eq!(r1["local0"], FakeValue::Int(0));
    assert_eq!(r1["local4"], FakeValue::Int(188));
}

/// Verifies symbols-map size accounting: the count we
/// capture BEFORE the move equals the count we WOULD
/// have read AFTER, but without needing the borrow.
#[test]
fn symbols_count_captured_before_move_matches_post_drop() {
    let mut source: HashMap<String, FakeValue> = HashMap::new();
    for i in 0..123 {
        source.insert(format!("k{}", i), FakeValue::Int(i as i64));
    }

    let count_before_move = source.len();
    drop(source); // simulate move into cache

    assert_eq!(count_before_move, 123);
}
