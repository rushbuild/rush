// Rush verified-core — C++ test link-PLAN assembly ORACLE
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT
//
// spec-owned (oracle): this file is the functional oracle for the
// link-plan-v1 verified scope. It witnesses the assembly invariants P1-P6 on
// thousands of random (objects, tagged-dep, passthrough, links_test_main)
// inputs AND on the historical alwayslink/plain double-advertise case + the
// tonight `-lbenchmark` test_main arm, and proves the oracle is non-vacuous via
// deliberately-wrong assembly variants it catches. Deleting, #[ignore]-ing, or
// weakening these assertions is a spec-sensitive change (Spec-Change: oracle)
// and routes through the human Spec-Review gate. See ci/spec-owned.tsv.
//
//! Property/golden ORACLE for [`rush_verified_core::assemble_link_plan`]. On
//! every generated input it checks the six real invariants of the link-plan
//! assembly. Getting ANY wrong is a real link bug — a misplaced archive breaks
//! GNU ld's single pass, a double-listed alwayslink path re-extracts members
//! ("multiple definition"), a dropped path reintroduces undefined symbols, and
//! a missing benchmark arm is literally tonight's latent link gap:
//!
//!   * **P1 BUCKET** — every alwayslink path is emitted wrapped in a
//!     `--whole-archive <p> --no-whole-archive` triple; every normal-bucket
//!     path is emitted as a bare lib inside the group.
//!   * **P2 ALWAYSLINK-WINS / no-double-list** — a path that is alwayslink in
//!     ANY dep is whole-archived and does NOT also appear as a normal lib.
//!   * **P3 ORDER** — all whole-archive-region tokens precede all normal libs,
//!     which precede all pass-through flags. Objects lead the whole line.
//!   * **P4 GROUP** — the `--start-group`/`--end-group` bracket is present IFF
//!     there is ≥1 normal lib (the REAL `!normal_libs.is_empty()`), and brackets
//!     the normal libs when present.
//!   * **P5 COMPLETENESS** — every distinct dep path appears at least once.
//!   * **P6 TESTMAIN** — `-lbenchmark` is in the pass-through region IFF
//!     `links_test_main`, and `-lbenchmark_main` NEVER appears.
//!
//! Anti-vacuity is demonstrated three ways (each a deliberately-broken assembly
//! the oracle CATCHES):
//!   * `mutant_whole_archive_after_normal_caught` — emits whole-archive AFTER
//!     the normal group → caught by P3 ordering;
//!   * `mutant_keeps_alwayslink_in_normal_caught` — keeps an alwayslink path
//!     ALSO in the normal group → caught by P2 alwayslink-wins/no-double-list;
//!   * `mutant_omits_benchmark_arm_caught` — omits the benchmark arm when
//!     `links_test_main` → caught by P6. This last one is TONIGHT'S BUG: the
//!     oracle proves the verified kernel would have caught the `-lbenchmark` gap.

use proptest::prelude::*;
use rush_verified_core::{assemble_link_plan, DepLib};
use std::collections::HashSet;

const WHOLE_ARCHIVE_OPEN: &str = "-Wl,--whole-archive";
const WHOLE_ARCHIVE_CLOSE: &str = "-Wl,--no-whole-archive";
const GROUP_START: &str = "-Wl,--start-group";
const GROUP_END: &str = "-Wl,--end-group";
const BENCHMARK: &str = "-lbenchmark";

// ---------------------------------------------------------------------------
// Bucket projections over the INPUT deps — the abstract sets the output must
// realise. These mirror WholeArchiveSet/NormalSet/AllPaths from LinkPlan.dfy.
// ---------------------------------------------------------------------------

/// Distinct paths that should be whole-archived: alwayslink in ≥1 dep.
fn whole_archive_set(deps: &[DepLib]) -> HashSet<String> {
    deps.iter().filter(|d| d.alwayslink).map(|d| d.path.clone()).collect()
}

/// Distinct paths in the normal group: dep paths NOT whole-archived anywhere.
fn normal_set(deps: &[DepLib]) -> HashSet<String> {
    let wa = whole_archive_set(deps);
    deps.iter()
        .map(|d| d.path.clone())
        .filter(|p| !wa.contains(p))
        .collect()
}

/// All distinct dep paths.
fn all_paths(deps: &[DepLib]) -> HashSet<String> {
    deps.iter().map(|d| d.path.clone()).collect()
}

// ---------------------------------------------------------------------------
// Token classifiers over the OUTPUT.
// ---------------------------------------------------------------------------

fn is_marker(t: &str) -> bool {
    matches!(t, WHOLE_ARCHIVE_OPEN | WHOLE_ARCHIVE_CLOSE | GROUP_START | GROUP_END)
}

/// Index where a `--whole-archive <p> --no-whole-archive` triple is emitted.
fn wrapped_at(out: &[String], p: &str) -> Option<usize> {
    (0..out.len()).find(|&i| {
        i + 2 < out.len()
            && out[i] == WHOLE_ARCHIVE_OPEN
            && out[i + 1] == p
            && out[i + 2] == WHOLE_ARCHIVE_CLOSE
    })
}

/// Positions of every bare lib token of `p` (a `p` not inside a triple).
fn bare_lib_positions(out: &[String], p: &str) -> Vec<usize> {
    let mut wrapped = HashSet::new();
    let mut i = 0;
    while i < out.len() {
        if out[i] == WHOLE_ARCHIVE_OPEN
            && i + 2 < out.len()
            && out[i + 2] == WHOLE_ARCHIVE_CLOSE
        {
            wrapped.insert(i + 1);
            i += 3;
            continue;
        }
        i += 1;
    }
    out.iter()
        .enumerate()
        .filter(|(idx, t)| t.as_str() == p && !wrapped.contains(idx))
        .map(|(idx, _)| idx)
        .collect()
}

// ---------------------------------------------------------------------------
// Oracle checkers (shared by golden + property tests).
// ---------------------------------------------------------------------------

/// P1 BUCKET — every alwayslink path is wrapped; every normal path appears bare.
fn check_bucket(deps: &[DepLib], out: &[String]) -> Result<(), String> {
    for p in whole_archive_set(deps) {
        if wrapped_at(out, &p).is_none() {
            return Err(format!("P1: alwayslink path {:?} is not whole-archive-wrapped", p));
        }
    }
    for p in normal_set(deps) {
        if bare_lib_positions(out, &p).is_empty() {
            return Err(format!("P1: normal path {:?} not present as a bare lib", p));
        }
    }
    Ok(())
}

/// P2 ALWAYSLINK-WINS — a whole-archived path is NEVER also a bare (normal) lib.
fn check_alwayslink_wins(deps: &[DepLib], out: &[String]) -> Result<(), String> {
    let wa = whole_archive_set(deps);
    for p in &wa {
        if !bare_lib_positions(out, p).is_empty() {
            return Err(format!(
                "P2: alwayslink path {:?} double-listed (also appears as a bare normal lib)",
                p
            ));
        }
    }
    Ok(())
}

/// P3 ORDER — every whole-archive-region token precedes every normal lib, which
/// precedes every pass-through flag.
fn check_order(deps: &[DepLib], objects: &[String], out: &[String]) -> Result<(), String> {
    let wa = whole_archive_set(deps);
    let nm = normal_set(deps);

    // Region index of every token: 0=object, 1=whole-archive, 2=normal lib,
    // 3=pass-through (and markers ride with their region). Compute the last
    // whole-archive index, first/last normal-lib index, first pass index.
    let mut last_wa: i64 = -1;
    let mut first_normal: i64 = i64::MAX;
    let mut last_normal: i64 = -1;
    let mut first_pass: i64 = i64::MAX;

    let object_set: HashSet<&String> = objects.iter().collect();

    let mut i = 0usize;
    while i < out.len() {
        let t = &out[i];
        if *t == WHOLE_ARCHIVE_OPEN && i + 2 < out.len() && out[i + 2] == WHOLE_ARCHIVE_CLOSE {
            last_wa = (i + 2) as i64;
            i += 3;
            continue;
        }
        if *t == GROUP_START || *t == GROUP_END {
            i += 1;
            continue;
        }
        if nm.contains(t) && !bare_lib_positions(out, t).is_empty() {
            first_normal = first_normal.min(i as i64);
            last_normal = last_normal.max(i as i64);
            i += 1;
            continue;
        }
        // A pass-through flag = not a marker, not an object, not a known lib path.
        if !is_marker(t) && !object_set.contains(t) && !wa.contains(t) && !nm.contains(t) {
            first_pass = first_pass.min(i as i64);
        }
        i += 1;
    }

    if last_wa >= 0 && first_normal != i64::MAX && last_wa > first_normal {
        return Err(format!(
            "P3: a whole-archive token (idx {}) follows a normal lib (idx {})",
            last_wa, first_normal
        ));
    }
    if last_normal >= 0 && first_pass != i64::MAX && last_normal > first_pass {
        return Err(format!(
            "P3: a normal lib (idx {}) follows a pass-through flag (idx {})",
            last_normal, first_pass
        ));
    }
    if last_wa >= 0 && first_pass != i64::MAX && last_wa > first_pass {
        return Err(format!(
            "P3: a whole-archive token (idx {}) follows a pass-through flag (idx {})",
            last_wa, first_pass
        ));
    }
    Ok(())
}

/// P4 GROUP — start/end present IFF ≥1 normal lib, and bracketing them.
fn check_group(deps: &[DepLib], out: &[String]) -> Result<(), String> {
    let nm = normal_set(deps);
    let starts = out.iter().filter(|t| t.as_str() == GROUP_START).count();
    let ends = out.iter().filter(|t| t.as_str() == GROUP_END).count();
    let want = !nm.is_empty();
    if want && (starts != 1 || ends != 1) {
        return Err(format!(
            "P4: expected exactly one --start-group/--end-group for {} normal libs, got {}/{}",
            nm.len(), starts, ends
        ));
    }
    if !want && (starts != 0 || ends != 0) {
        return Err(format!(
            "P4: group bracket present with zero normal libs (starts {}, ends {})",
            starts, ends
        ));
    }
    if want {
        let gs = out.iter().position(|t| t == GROUP_START).unwrap();
        let ge = out.iter().position(|t| t == GROUP_END).unwrap();
        if gs >= ge {
            return Err("P4: --start-group does not precede --end-group".into());
        }
        for p in &nm {
            for pos in bare_lib_positions(out, p) {
                if !(gs < pos && pos < ge) {
                    return Err(format!(
                        "P4: normal lib {:?} at idx {} is outside the group ({}, {})",
                        p, pos, gs, ge
                    ));
                }
            }
        }
    }
    Ok(())
}

/// P5 COMPLETENESS — every distinct dep path appears at least once.
fn check_completeness(deps: &[DepLib], out: &[String]) -> Result<(), String> {
    for p in all_paths(deps) {
        let present = out.iter().any(|t| t == &p);
        if !present {
            return Err(format!("P5: dep path {:?} dropped from the link line", p));
        }
    }
    Ok(())
}

/// P6 TESTMAIN — -lbenchmark IFF links_test_main, and never -lbenchmark_main.
fn check_testmain(links_test_main: bool, out: &[String]) -> Result<(), String> {
    let has_bench = out.iter().any(|t| t == BENCHMARK);
    if has_bench != links_test_main {
        return Err(format!(
            "P6: -lbenchmark present={} but links_test_main={}",
            has_bench, links_test_main
        ));
    }
    if out.iter().any(|t| t == "-lbenchmark_main") {
        return Err("P6: -lbenchmark_main must never appear (would redefine main())".into());
    }
    Ok(())
}

/// Run every invariant; returns the first failure.
fn check_all(
    objects: &[String],
    deps: &[DepLib],
    links_test_main: bool,
    out: &[String],
) -> Result<(), String> {
    check_bucket(deps, out)?;
    check_alwayslink_wins(deps, out)?;
    check_order(deps, objects, out)?;
    check_group(deps, out)?;
    check_completeness(deps, out)?;
    check_testmain(links_test_main, out)?;
    Ok(())
}

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn d(path: &str, alwayslink: bool) -> DepLib {
    DepLib::new(path, alwayslink)
}

// ---------------------------------------------------------------------------
// Golden / specific-witness cases (always run, no randomness).
// ---------------------------------------------------------------------------

/// The real retain() case: `libsparse_tensor.a` advertised alwayslink by the
/// framework rollup dep AND plain by the standalone cc_library dep. It must be
/// whole-archived ONLY — no double-list — else GNU ld extracts the member twice
/// → "multiple definition".
#[test]
fn witness_alwayslink_and_plain_same_path() {
    let objects = v(&["test.o"]);
    let deps = vec![
        d("libsparse_tensor.a", true),
        d("libsparse_tensor.a", false),
        d("libother.a", false),
    ];
    let out = assemble_link_plan(objects.clone(), deps.clone(), vec![], false);
    check_all(&objects, &deps, false, &out).unwrap();
    // Whole-archived exactly once, never bare.
    assert!(wrapped_at(&out, "libsparse_tensor.a").is_some());
    assert!(bare_lib_positions(&out, "libsparse_tensor.a").is_empty());
}

/// Tonight's bug, as a positive witness: a tf_cc_test whose closure links
/// `test_main` gets `-lbenchmark -lpthread` in the pass-through region (after
/// the archives), and NEVER `-lbenchmark_main`.
#[test]
fn witness_test_main_benchmark_arm() {
    let objects = v(&["test_main.o", "mytest.o"]);
    let deps = vec![d("libtsl_platform_test_main.a", false), d("libcore.a", true)];
    let out = assemble_link_plan(objects.clone(), deps.clone(), v(&["-lgtest"]), true);
    check_all(&objects, &deps, true, &out).unwrap();
    assert!(out.iter().any(|t| t == "-lbenchmark"));
    assert!(out.iter().any(|t| t == "-lpthread"));
    assert!(!out.iter().any(|t| t == "-lbenchmark_main"));
    // benchmark arm is after the archives.
    let bench = out.iter().position(|t| t == "-lbenchmark").unwrap();
    let group_end = out.iter().position(|t| t == GROUP_END).unwrap();
    assert!(bench > group_end);
}

/// A single normal lib still gets a group (the REAL >= 1 condition, NOT >= 2).
#[test]
fn witness_single_normal_lib_gets_group() {
    let deps = vec![d("libsolo.a", false)];
    let out = assemble_link_plan(vec![], deps.clone(), vec![], false);
    check_all(&[], &deps, false, &out).unwrap();
    assert_eq!(out, v(&[GROUP_START, "libsolo.a", GROUP_END]));
}

// ---------------------------------------------------------------------------
// ANTI-VACUITY: deliberately-wrong assemblies the oracle MUST catch.
// ---------------------------------------------------------------------------

/// MUTANT A: emits the whole-archive triples AFTER the normal group. Caught by
/// P3 ordering.
fn mutant_whole_archive_after_normal(deps: &[DepLib]) -> Vec<String> {
    let wa = whole_archive_set(deps);
    let nm = normal_set(deps);
    let mut out = Vec::new();
    // normal group FIRST (wrong)
    if !nm.is_empty() {
        out.push(GROUP_START.to_string());
        for p in deps.iter().map(|d| &d.path) {
            if nm.contains(p) && !out.contains(p) {
                out.push(p.clone());
            }
        }
        out.push(GROUP_END.to_string());
    }
    // whole-archive AFTER (wrong)
    let mut emitted = HashSet::new();
    for p in deps.iter().filter(|d| d.alwayslink).map(|d| &d.path) {
        if wa.contains(p) && emitted.insert(p.clone()) {
            out.push(WHOLE_ARCHIVE_OPEN.to_string());
            out.push(p.clone());
            out.push(WHOLE_ARCHIVE_CLOSE.to_string());
        }
    }
    out
}

#[test]
fn mutant_whole_archive_after_normal_caught() {
    let deps = vec![d("libw.a", true), d("libn.a", false)];
    let bad = mutant_whole_archive_after_normal(&deps);
    assert!(
        check_all(&[], &deps, false, &bad).is_err(),
        "oracle went vacuous: accepted whole-archive emitted AFTER the normal group"
    );
    // The real function passes.
    let good = assemble_link_plan(vec![], deps.clone(), vec![], false);
    check_all(&[], &deps, false, &good).unwrap();
}

/// MUTANT B: keeps an alwayslink path ALSO in the normal group (skips the
/// retain). Caught by P2 alwayslink-wins / no-double-list.
fn mutant_keeps_alwayslink_in_normal(deps: &[DepLib]) -> Vec<String> {
    let mut out = Vec::new();
    let mut wa_emitted = HashSet::new();
    for p in deps.iter().filter(|d| d.alwayslink).map(|d| &d.path) {
        if wa_emitted.insert(p.clone()) {
            out.push(WHOLE_ARCHIVE_OPEN.to_string());
            out.push(p.clone());
            out.push(WHOLE_ARCHIVE_CLOSE.to_string());
        }
    }
    // BUG: normal bucket = every dep path advertised plain, WITHOUT retaining
    // away the alwayslink-wins ones → an alwayslink path advertised plain by
    // another dep double-lists.
    let mut normal: Vec<String> = Vec::new();
    for dep in deps {
        if !dep.alwayslink && !normal.contains(&dep.path) {
            normal.push(dep.path.clone());
        }
    }
    if !normal.is_empty() {
        out.push(GROUP_START.to_string());
        out.extend(normal);
        out.push(GROUP_END.to_string());
    }
    out
}

#[test]
fn mutant_keeps_alwayslink_in_normal_caught() {
    // libdual.a is alwayslink in one dep, plain in another. The mutant lists it
    // both whole-archived AND in the normal group.
    let deps = vec![d("libdual.a", true), d("libdual.a", false)];
    let bad = mutant_keeps_alwayslink_in_normal(&deps);
    assert!(
        bad.iter().filter(|t| t.as_str() == "libdual.a").count() >= 2,
        "mutant should have double-listed libdual.a"
    );
    assert!(
        check_all(&[], &deps, false, &bad).is_err(),
        "oracle went vacuous: accepted an alwayslink path double-listed in the normal group"
    );
    let good = assemble_link_plan(vec![], deps.clone(), vec![], false);
    check_all(&[], &deps, false, &good).unwrap();
    assert!(bare_lib_positions(&good, "libdual.a").is_empty());
}

/// MUTANT C — TONIGHT'S BUG: omits the benchmark arm even though
/// `links_test_main` is set. Caught by P6.
fn mutant_omits_benchmark_arm(
    objects: &[String],
    deps: &[DepLib],
    passthrough: &[String],
) -> Vec<String> {
    // Faithful assembly EXCEPT it never appends the -lbenchmark arm.
    let mut out = assemble_link_plan(
        objects.to_vec(),
        deps.to_vec(),
        passthrough.to_vec(),
        false, // BUG: pretend test_main is absent, so no benchmark arm
    );
    // (no -lbenchmark appended)
    out.retain(|t| t != BENCHMARK && t != "-lpthread");
    out
}

#[test]
fn mutant_omits_benchmark_arm_caught() {
    // This is the exact shape of tonight's latent gap: test_main IS in the
    // closure (links_test_main = true) but the assembly forgot -lbenchmark.
    let objects = v(&["test_main.o"]);
    let deps = vec![d("libcore.a", false)];
    let bad = mutant_omits_benchmark_arm(&objects, &deps, &v(&["-lgtest"]));
    assert!(!bad.iter().any(|t| t == "-lbenchmark"));
    assert!(
        check_all(&objects, &deps, true, &bad).is_err(),
        "oracle went vacuous: accepted a link plan MISSING the -lbenchmark arm \
         while links_test_main=true (this is tonight's bug)"
    );
    // The verified kernel emits it.
    let good = assemble_link_plan(objects.clone(), deps.clone(), v(&["-lgtest"]), true);
    check_all(&objects, &deps, true, &good).unwrap();
    assert!(good.iter().any(|t| t == "-lbenchmark"));
}

// ---------------------------------------------------------------------------
// Property: random inputs always satisfy all six invariants.
// ---------------------------------------------------------------------------

/// A dep lib drawn from a SMALL path pool so the same path frequently appears
/// with BOTH alwayslink flags (forcing the retain / alwayslink-wins path).
fn dep_lib() -> impl Strategy<Value = DepLib> {
    let path = prop_oneof![
        Just("liba.a".to_string()),
        Just("dir/libb.a".to_string()),
        Just("libtest_main.a".to_string()),
        Just("libc.a".to_string()),
        Just("libd.a".to_string()),
    ];
    (path, any::<bool>()).prop_map(|(p, aw)| DepLib::new(p, aw))
}

/// A pass-through flag (never an archive path, never a marker we'd misread).
fn pass_flag() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("-lgtest".to_string()),
        Just("-lgtest_main".to_string()),
        Just("-labsl_strings".to_string()),
        Just("-L/usr/lib".to_string()),
        Just("-ldouble-conversion".to_string()),
    ]
}

fn object() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("test.o".to_string()),
        Just("test_main.o".to_string()),
        Just("helper.o".to_string()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    /// All six invariants hold on every generated input.
    #[test]
    fn assemble_satisfies_all_invariants(
        objects in proptest::collection::vec(object(), 0..=4),
        deps in proptest::collection::vec(dep_lib(), 0..=10),
        passthrough in proptest::collection::vec(pass_flag(), 0..=5),
        links_test_main in any::<bool>(),
    ) {
        let out = assemble_link_plan(
            objects.clone(), deps.clone(), passthrough.clone(), links_test_main);
        if let Err(why) = check_all(&objects, &deps, links_test_main, &out) {
            prop_assert!(false, "invariant violated: {} on deps={:?} ltm={}", why, deps, links_test_main);
        }
    }

    /// Idempotence of the bucket de-dup: feeding the same deps twice (each path
    /// duplicated) yields the same plan as feeding them once.
    #[test]
    fn assemble_dedups_duplicate_deps(
        deps in proptest::collection::vec(dep_lib(), 0..=6),
        links_test_main in any::<bool>(),
    ) {
        let once = assemble_link_plan(vec![], deps.clone(), vec![], links_test_main);
        let doubled: Vec<DepLib> = deps.iter().chain(deps.iter()).cloned().collect();
        let twice = assemble_link_plan(vec![], doubled, vec![], links_test_main);
        prop_assert_eq!(once, twice);
    }
}
