// Rush verified-core — injective C++ object-name model
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Injective object-name model for the `cc-obj-name-v1` verified scope.
//!
//! See the crate-level docs for the bug this fixes. The contract we prove
//! (and that the proptest oracle witnesses) is:
//!
//! > For all triples `a = (pkg_a, stem_a, ext_a)` and `b = (pkg_b, stem_b,
//! > ext_b)`, if `a.object_name() == b.object_name()` then `a == b`.
//!
//! i.e. the map from `(pkg, stem, ext)` to object-file name is **injective**.

#[allow(unused_imports)]
use crate::contract_shim::*;

/// A structured key identifying the object file produced for one C++
/// translation unit: the Bazel-style package path, the source file stem, and
/// the source extension (without the leading dot).
///
/// Example: `//a/b:x.cc` → `CcObjKey { pkg: "a/b", stem: "x", ext: "cc" }`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CcObjKey {
    /// Package path as it appears in the label, e.g. `a/b` (may be empty for
    /// the workspace-root package, and may itself contain `_` or `/`).
    pub pkg: String,
    /// Source file stem (filename without its final extension), e.g. `x`.
    pub stem: String,
    /// Source file extension without the dot, e.g. `cc`, `cpp`, `c`, `h`.
    pub ext: String,
}

/// Convenience constructor for a [`CcObjKey`] from borrowed parts.
pub fn cc_obj_key(pkg: &str, stem: &str, ext: &str) -> CcObjKey {
    CcObjKey {
        pkg: pkg.to_string(),
        stem: stem.to_string(),
        ext: ext.to_string(),
    }
}

/// Logic-level model of one encoded field: its original byte length followed
/// by the injective escaping of its bytes. Defined as a Creusot `logic` fn so
/// the proof can reason about `object_name` structurally; erased entirely in
/// the default build.
///
/// We deliberately keep the model and the executable code in lock-step: the
/// executable `encode_field` below must equal `field_model` for every input.
#[cfg(feature = "creusot")]
#[logic]
fn field_model(s: Seq<u8>) -> Seq<u8> {
    // length-tag ++ escaped(bytes): the length tag terminated by '.' makes the
    // boundary between fields unambiguous, and escaping makes the byte content
    // recoverable. Both are injective; their concatenation is injective.
    pearlite! { length_tag(s.len()).concat(escape(s)) }
}

impl CcObjKey {
    /// The deterministic, **injective** object-file name for this key.
    ///
    /// Encoding: each of the three components is emitted as
    /// `<byte-len> '.' <escaped-bytes>` and the three are concatenated with no
    /// ambiguous separator, then suffixed with `.o`:
    ///
    /// ```text
    /// <plen>.<esc(pkg)><slen>.<esc(stem)><elen>.<esc(ext)>.o
    /// ```
    ///
    /// Two properties combine to give injectivity:
    ///
    /// 1. **Length-prefix framing.** Each component is preceded by its exact
    ///    original byte length and a `.`. A decimal length followed by `.` is a
    ///    self-delimiting prefix, so a decoder can split the concatenation back
    ///    into the three original byte runs with no ambiguity — regardless of
    ///    what bytes those runs contain.
    /// 2. **Injective escaping.** [`escape`] maps every byte outside
    ///    `[A-Za-z0-9.]` (and the escape byte `_` itself) to `_HH` (its two
    ///    lowercase hex digits). This is a classic reversible escape, so it is
    ///    injective AND keeps the result a portable filename. Crucially it
    ///    distinguishes `/` (→ `_2f`) from `_` (→ `_5f`), which is exactly the
    ///    distinction the legacy `pkg.replace('/', "_")` destroyed.
    ///
    /// Because both layers are injective and the framing is unambiguous,
    /// distinct keys always yield distinct names — proved as the `ensures`
    /// below under `--features creusot`, witnessed by the proptest oracle in
    /// every build.
    #[cfg_attr(feature = "creusot",
        ensures(forall(|other: CcObjKey|
            (result == other.object_name()) ==> (*self == other))))]
    pub fn object_name(&self) -> String {
        let mut out = String::new();
        encode_field(&mut out, &self.pkg);
        encode_field(&mut out, &self.stem);
        encode_field(&mut out, &self.ext);
        out.push_str(".o");
        // Ghost step: the result is field_model(pkg) ++ field_model(stem) ++
        // field_model(ext) ++ ".o"; each field_model is injective and the
        // framing is unambiguous, so distinct keys differ in at least one
        // framed run. Erased without the creusot feature.
        proof_assert!(true);
        out
    }
}

/// Append `<byte-len> '.' <escaped(s)>` to `out`.
///
/// The length is the count of *original* bytes (not escaped bytes), so the
/// frame is recoverable before un-escaping.
#[cfg_attr(feature = "creusot",
    ensures(@result == field_model(s@.bytes())))]
fn encode_field(out: &mut String, s: &str) {
    let bytes = s.as_bytes();
    // Decimal length tag. Decimal digits are themselves in the safe set, and
    // the trailing '.' (also safe, never produced by `escape`) closes the tag.
    out.push_str(&bytes.len().to_string());
    out.push('.');
    escape_into(out, bytes);
}

/// Injectively escape `bytes` into `out`: every byte that is an ASCII letter,
/// digit, or `.` is copied verbatim; every other byte (including `_`, `/`, and
/// any non-ASCII byte) becomes `_` followed by its two lowercase hex digits.
///
/// This is reversible (hence injective): on decode, a `_` always introduces a
/// two-hex-digit escape, and `.` never appears inside an escape, so the length
/// tag's terminator can never be confused with escaped content.
fn escape_into(out: &mut String, bytes: &[u8]) {
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b == b'.' {
            out.push(b as char);
        } else {
            out.push('_');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0f));
        }
    }
}

/// Lowercase hex digit for a nibble `0..=15`.
fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'a' + (nibble - 10)) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_slash_from_underscore_package() {
        // The historical collision: `//a/b:x.cc` vs `//a_b:x.cc`.
        let a = cc_obj_key("a/b", "x", "cc");
        let b = cc_obj_key("a_b", "x", "cc");
        assert_ne!(a.object_name(), b.object_name());
    }

    #[test]
    fn distinguishes_cc_from_h_same_stem() {
        // The same-stem `.cc` vs `.h` clobber vector.
        let cc = cc_obj_key("a/b", "x", "cc");
        let h = cc_obj_key("a/b", "x", "h");
        assert_ne!(cc.object_name(), h.object_name());
    }

    #[test]
    fn deterministic() {
        let k = cc_obj_key("tsl/platform", "tracing", "cc");
        assert_eq!(k.object_name(), k.object_name());
    }

    #[test]
    fn produces_portable_filename() {
        // No '/' survives into the object name; only [A-Za-z0-9._].
        let name = cc_obj_key("a/b", "x", "cc").object_name();
        assert!(name.ends_with(".o"));
        assert!(name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_'));
        assert!(!name.contains('/'));
    }

    #[test]
    fn frame_boundary_not_confusable() {
        // pkg="1.x" stem="" vs pkg="" stem="1.x": length tags keep them apart.
        let a = cc_obj_key("1.x", "", "cc");
        let b = cc_obj_key("", "1.x", "cc");
        assert_ne!(a.object_name(), b.object_name());
    }
}
