// Rush verified-core — object-name injectivity ORACLE
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT
//
// spec-owned (oracle): this file is the functional oracle ("uDebug gate") for
// the cc-obj-name-v1 verified scope. Deleting, #[ignore]-ing, or weakening
// these assertions is a spec-sensitive change (Spec-Change: oracle) and routes
// through the human Spec-Review gate. See ci/spec-owned.tsv and the MACP
// agentic-maintenance extension doc.
//
//! Property/golden ORACLE proving the object-name map is collision-free
//! (injective) over realistic Bazel-style inputs. This is the independent
//! functional check that complements the Creusot proof: even with the verifier
//! uninstalled, this oracle witnesses the property on thousands of generated
//! cases AND on the specific historical collisions.
//!
//! Anti-vacuity is demonstrated two ways:
//!   * `legacy_encoding_actually_collides` shows the OLD `/`→`_` encoding fails
//!     the very property this oracle enforces — so the oracle is not trivially
//!     satisfiable; it catches the real bug.
//!   * the proptest generators deliberately emit `/`, `_`, digits, `.` and
//!     nested paths, so the search space contains the `a/b` vs `a_b` and
//!     `.cc` vs `.h` witnesses.

use proptest::prelude::*;
use rush_verified_core::cc_obj_key;

/// The legacy (BUGGY) encoding, reproduced verbatim from the pre-fix
/// `cc_obj_name_for_src` in crates/rush-rules/src/rules_cc.rs. Kept ONLY in
/// this test so we can prove the oracle rejects it (anti-vacuity witness).
fn legacy_object_name(pkg: &str, stem: &str, _ext: &str) -> String {
    if pkg.is_empty() {
        format!("{}.o", stem)
    } else {
        format!("{}_{}.o", pkg.replace('/', "_"), stem)
    }
}

// ---------------------------------------------------------------------------
// Golden / specific-witness cases (always run, no randomness).
// ---------------------------------------------------------------------------

#[test]
fn witness_slash_vs_underscore_package() {
    // //a/b:x.cc  vs  //a_b:x.cc  — the distinct collision vector Codex found.
    let a = cc_obj_key("a/b", "x", "cc");
    let b = cc_obj_key("a_b", "x", "cc");
    assert_ne!(
        a.object_name(),
        b.object_name(),
        "a/b and a_b must not collide"
    );
}

#[test]
fn witness_cc_vs_h_same_stem() {
    // Same package + stem, different extension: x.cc vs x.h.
    let cc = cc_obj_key("tsl/platform", "tracing", "cc");
    let h = cc_obj_key("tsl/platform", "tracing", "h");
    assert_ne!(cc.object_name(), h.object_name());
}

#[test]
fn witness_cross_package_same_stem() {
    // tracing.cc in `default` vs //tsl/platform:tracing.cc (the real TF case).
    let local = cc_obj_key("tsl/platform/default", "tracing", "cc");
    let cross = cc_obj_key("tsl/platform", "tracing", "cc");
    assert_ne!(local.object_name(), cross.object_name());
}

/// ANTI-VACUITY: the same collision-freedom property, when checked against the
/// OLD encoding, MUST fail — proving this oracle is meaningful. If the legacy
/// encoding ever stopped colliding here, the oracle would be vacuous and this
/// test would (correctly) fail to find the expected collision.
#[test]
fn legacy_encoding_actually_collides() {
    let a = legacy_object_name("a/b", "x", "cc");
    let b = legacy_object_name("a_b", "x", "cc");
    assert_eq!(
        a, b,
        "legacy `/`->`_` encoding is expected to collide a/b with a_b; \
         if this no longer holds, the injectivity oracle has gone vacuous"
    );
    // And the .cc/.h legacy clobber (extension ignored by the old code):
    let cc = legacy_object_name("p", "x", "cc");
    let h = legacy_object_name("p", "x", "h");
    assert_eq!(cc, h, "legacy encoding ignored the extension");
}

// ---------------------------------------------------------------------------
// Property: distinct keys => distinct object names (injectivity), over
// generators that exercise '/', '_', '.', digits and nested paths.
// ---------------------------------------------------------------------------

/// Bytes that recreate the historical hazards: path separators, the legacy
/// escape char, dots, and digits, plus ordinary letters.
fn component() -> impl Strategy<Value = String> {
    // 0..=4 symbols drawn from a hazard-rich alphabet.
    proptest::collection::vec(
        prop_oneof![
            Just('a'),
            Just('b'),
            Just('/'),
            Just('_'),
            Just('.'),
            Just('1'),
        ],
        0..=4,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

fn ext() -> impl Strategy<Value = String> {
    prop_oneof![Just("cc".to_string()), Just("cpp".to_string()), Just("c".to_string()), Just("h".to_string())]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    /// Core injectivity: two keys collide on object_name() IFF they are equal.
    #[test]
    fn injective(
        pkg_a in component(), stem_a in component(), ext_a in ext(),
        pkg_b in component(), stem_b in component(), ext_b in ext(),
    ) {
        let a = cc_obj_key(&pkg_a, &stem_a, &ext_a);
        let b = cc_obj_key(&pkg_b, &stem_b, &ext_b);
        let same_name = a.object_name() == b.object_name();
        let same_key = a == b;
        prop_assert_eq!(
            same_name, same_key,
            "object_name collision must imply key equality: \
             a=({:?},{:?},{:?}) b=({:?},{:?},{:?}) -> {} vs {}",
            pkg_a, stem_a, ext_a, pkg_b, stem_b, ext_b,
            a.object_name(), b.object_name()
        );
    }

    /// The output is always a portable filename ([A-Za-z0-9._], ends in .o).
    #[test]
    fn portable(pkg in component(), stem in component(), e in ext()) {
        let name = cc_obj_key(&pkg, &stem, &e).object_name();
        prop_assert!(name.ends_with(".o"));
        prop_assert!(
            name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_'),
            "non-portable char in {}", name
        );
    }
}
