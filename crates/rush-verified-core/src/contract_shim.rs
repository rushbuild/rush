// Rush verified-core — contract shim
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Feature-gated bridge to Creusot's contract vocabulary.
//!
//! When built with `--features creusot`, the real `creusot_contracts`
//! attributes/macros (`requires`, `ensures`, `logic`, `proof_assert!`, …) are
//! re-exported and used directly on the functions in `cc_obj_name.rs`.
//!
//! When built WITHOUT the feature (the default — what `cargo test
//! -p rush-verified-core` runs in CI and on developer machines that have no
//! Creusot toolchain), the contract attributes are written as
//! `#[cfg_attr(feature = "creusot", ...)]` directly at the use site, so they
//! simply vanish. This module then only needs to provide the no-op forms of
//! the macro-style helpers (`proof_assert!`) so the source still type-checks.
//!
//! This is the mechanism that lets the PoC compile and its oracle run today,
//! while the Creusot proof becomes a drop-in second layer once the pinned
//! verifier in `ci/creusot-toolchain.lock` is installed. It is NOT a stub: the
//! default-build behavior (contracts erased, runtime code intact) is exactly
//! the intended, documented semantics.

#[cfg(feature = "creusot")]
pub(crate) use creusot_contracts::{ensures, logic, proof_assert, requires};

/// No-op `proof_assert!` for the non-Creusot build.
///
/// Under `--features creusot` this name is shadowed by the real
/// `creusot_contracts::proof_assert!`, which emits a proof obligation. Without
/// the feature it expands to nothing, so ghost proof steps in the source carry
/// zero runtime cost and do not require the verifier to be installed.
#[cfg(not(feature = "creusot"))]
macro_rules! proof_assert {
    ($($t:tt)*) => {};
}

#[cfg(not(feature = "creusot"))]
pub(crate) use proof_assert;
