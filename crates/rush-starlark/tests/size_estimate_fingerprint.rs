// Phase 5.C.4 commit 119: tests for the
// SizeEstimateFingerprint that decides whether a
// prior estimate_symbols_size result can be reused
// on a repeat-miss reload.
//
// Pro: "The fingerprint must not be just `PathBuf`.
// You already saw identity weirdness with
// `new_sets.bzl`; don't invite that goblin back
// inside."
//
// Tests required by Pro's commit-119 directive:
//   1. same_fingerprint_reuses_record
//   2. mtime_change_invalidates_record
//   3. symbol_count_change_invalidates_record
//   4. different_canonical_label_does_not_reuse

use rush_starlark::bzl_loader::{
    SizeEstimateFingerprint, SizeEstimateRecord,
};
use std::path::PathBuf;

fn fp(
    label: Option<&str>,
    path: &str,
    file_len: u64,
    file_mtime_ns: Option<u128>,
    symbol_count: u64,
) -> SizeEstimateFingerprint {
    SizeEstimateFingerprint {
        canonical_label: label.map(String::from),
        resolved_path: PathBuf::from(path),
        file_len,
        file_mtime_ns,
        exported_symbol_count: symbol_count,
    }
}

/// Pro test #1: two equivalent fingerprints match
/// and the recorded estimate is reused.
#[test]
fn same_fingerprint_reuses_record() {
    let a = fp(Some("//x:foo.bzl"), "/abs/x/foo.bzl", 4096, Some(123_456), 17);
    let b = fp(Some("//x:foo.bzl"), "/abs/x/foo.bzl", 4096, Some(123_456), 17);
    assert!(a.matches(&b));

    // Build a record around `a` and check that `b`
    // would reuse it.
    let record = SizeEstimateRecord {
        fingerprint: a,
        estimated_bytes: 12_345,
    };
    assert!(record.fingerprint.matches(&b));
    assert_eq!(record.estimated_bytes, 12_345);
}

/// Pro test #2: an mtime change invalidates the
/// record (catches in-place file rewrites that
/// preserve length).
#[test]
fn mtime_change_invalidates_record() {
    let a = fp(Some("//x:foo.bzl"), "/abs/x/foo.bzl", 4096, Some(100), 17);
    let b = fp(Some("//x:foo.bzl"), "/abs/x/foo.bzl", 4096, Some(200), 17);
    assert!(
        !a.matches(&b),
        "different mtime must invalidate the fingerprint"
    );

    // One-sided Some/None is also a mismatch.
    let c = fp(Some("//x:foo.bzl"), "/abs/x/foo.bzl", 4096, None, 17);
    assert!(!a.matches(&c));
    assert!(!c.matches(&b));
}

/// Pro test #3: a symbol-count change invalidates
/// the record (catches semantic changes that don't
/// touch the file — e.g. a load() chain exporting a
/// different subset).
#[test]
fn symbol_count_change_invalidates_record() {
    let a = fp(Some("//x:foo.bzl"), "/abs/x/foo.bzl", 4096, Some(123), 17);
    let b = fp(Some("//x:foo.bzl"), "/abs/x/foo.bzl", 4096, Some(123), 18);
    assert!(
        !a.matches(&b),
        "different exported_symbol_count must invalidate"
    );
}

/// Pro test #4: two distinct canonical labels
/// resolving to the same path are NOT
/// interchangeable. This is the new_sets.bzl
/// identity-collision case from commit 105.
#[test]
fn different_canonical_label_does_not_reuse() {
    let bare = fp(Some(":new_sets.bzl"), "/abs/skylib/new_sets.bzl", 6144, Some(123), 9);
    let at = fp(
        Some("@bazel_skylib//lib:new_sets.bzl"),
        "/abs/skylib/new_sets.bzl",
        6144,
        Some(123),
        9,
    );
    assert!(
        !bare.matches(&at),
        "identity-collision: different raw labels must NOT share the record"
    );

    // One label vs no label is also distinct (None
    // is a separate identity state).
    let none = fp(None, "/abs/skylib/new_sets.bzl", 6144, Some(123), 9);
    assert!(!bare.matches(&none));
    assert!(!at.matches(&none));
}

/// Bonus: file_len change is caught.
#[test]
fn file_len_change_invalidates_record() {
    let a = fp(Some("//x:foo.bzl"), "/abs/x/foo.bzl", 4096, Some(123), 17);
    let b = fp(Some("//x:foo.bzl"), "/abs/x/foo.bzl", 8192, Some(123), 17);
    assert!(!a.matches(&b));
}

/// Bonus: resolved_path change is caught (even with
/// matching label string).
#[test]
fn resolved_path_change_invalidates_record() {
    let a = fp(Some("//x:foo.bzl"), "/abs/x/foo.bzl", 4096, Some(123), 17);
    let b = fp(Some("//x:foo.bzl"), "/different/x/foo.bzl", 4096, Some(123), 17);
    assert!(!a.matches(&b));
}
