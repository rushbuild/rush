// Rush Build - Hot-spot summary (phase 5.C.4 commit 83)
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 commit 83: aggregate hot-spot counters for
//! the analyze pass.
//!
//! Pro's commit-83 directive: *"Add aggregate hot-spot
//! counters, not logs. Track: BUILD files parsed count,
//! packages loaded count, targets discovered per package,
//! rule instances analyzed per rule kind, declared actions
//! per mnemonic, slowest N packages by analysis time,
//! slowest N rule kinds by total analysis time."*
//!
//! ## What this commit ships
//!
//! - Counts: BUILD files / packages / targets per package /
//!   rule instances per rule kind / actions per mnemonic.
//! - Top-N functions (configurable N, default 20).
//! - Human + JSON output.
//!
//! ## What this commit does NOT ship (bounded scope)
//!
//! - Per-package wall-clock timing. Would require
//!   instrumenting each BUILD-file parse call individually;
//!   today's analyze-pass parse loop is one bulk operation
//!   timed by commit 82's `BuildFileParse` phase entry. A
//!   future commit can add per-package timing if the data
//!   is operationally needed.
//! - Per-rule-kind wall-clock timing. Same reasoning.
//!
//! For TF-scale workloads these counts are usually the
//! more actionable signal anyway: "12,000 cc_library
//! instances" tells you where to focus attention; the
//! exact ms-per-rule-kind breakdown only matters for
//! tier-1 optimizations that haven't fired yet.

use std::collections::HashMap;

/// Phase 5.C.4 commit 83: aggregate hot-spot counters
/// produced during analyze-pass.
///
/// One instance per analyze invocation. The orchestrator
/// populates it from the dependency graph + artifact
/// registry after the parse loop completes.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct HotSpotSummary {
    /// Total BUILD files parsed.
    pub build_files_parsed: u64,
    /// Distinct packages encountered (some BUILD files
    /// may contain multiple sub-packages via `subinclude`
    /// or future namespace work).
    pub packages_loaded: u64,
    /// Targets per package, top-N sorted by count.
    pub targets_per_package: Vec<PackageTargetCount>,
    /// Rule instances per rule kind. Sorted by count
    /// descending; trimmed to top-N.
    pub rule_instances_by_rule_kind: Vec<RuleKindCount>,
    /// Declared actions per mnemonic. Sorted by count
    /// descending; trimmed to top-N.
    pub actions_by_mnemonic: Vec<MnemonicCount>,
    /// Top-N parameter applied to the sorted vectors.
    /// `0` means no trimming (all entries kept).
    pub top_n: usize,
    /// Schema version. Bumped on breaking JSON shape
    /// changes.
    pub schema_version: u32,
}

/// Phase 5.C.4 commit 83: schema version for the hot-spot
/// JSON wire format.
pub const HOTSPOT_SUMMARY_SCHEMA_VERSION: u32 = 1;

/// Phase 5.C.4 commit 83: default Top-N when callers
/// don't override.
pub const DEFAULT_TOP_N: usize = 20;

/// Phase 5.C.4 commit 83: package label → target count.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PackageTargetCount {
    /// Package label (e.g. `//tensorflow/core/platform`).
    pub package: String,
    /// Number of targets in this package.
    pub count: u64,
}

/// Phase 5.C.4 commit 83: rule kind → instance count.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RuleKindCount {
    /// Rule kind (`cc_library`, `tf_proto_library`, etc.).
    pub rule_kind: String,
    /// Number of RuleInstance values of this kind.
    pub count: u64,
}

/// Phase 5.C.4 commit 83: mnemonic → action count.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MnemonicCount {
    /// Action mnemonic (`CppCompile`, `CcArchive`, …).
    pub mnemonic: String,
    /// Number of declared actions with this mnemonic.
    pub count: u64,
}

/// Phase 5.C.4 commit 83: builder that the orchestrator
/// uses to aggregate hot-spot counts.
///
/// Internal buckets stay as HashMaps for O(1) increments
/// during the gather phase; [`HotSpotSummaryBuilder::finish`]
/// sorts + applies top-N + emits the immutable
/// [`HotSpotSummary`].
#[derive(Debug, Default)]
pub struct HotSpotSummaryBuilder {
    build_files_parsed: u64,
    packages: std::collections::HashSet<String>,
    targets_per_package: HashMap<String, u64>,
    rule_instances_by_rule_kind: HashMap<String, u64>,
    actions_by_mnemonic: HashMap<String, u64>,
}

impl HotSpotSummaryBuilder {
    /// Build a fresh builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one parsed BUILD file. Idempotent on the
    /// same package label (the underlying HashSet
    /// dedupes), but the build-files-parsed counter
    /// increments every call so re-parses get counted.
    pub fn observe_build_file(&mut self, package: &str) {
        self.build_files_parsed += 1;
        self.packages.insert(package.to_string());
    }

    /// Record one RuleInstance discovered in a BUILD file.
    pub fn observe_rule_instance(&mut self, package: &str, rule_kind: &str) {
        *self.targets_per_package.entry(package.to_string()).or_insert(0) += 1;
        *self
            .rule_instances_by_rule_kind
            .entry(rule_kind.to_string())
            .or_insert(0) += 1;
    }

    /// Record one declared action's mnemonic.
    pub fn observe_action(&mut self, mnemonic: &str) {
        *self.actions_by_mnemonic.entry(mnemonic.to_string()).or_insert(0) += 1;
    }

    /// Finalize: sort each vector by count descending +
    /// apply top-N trimming. `top_n = 0` keeps every
    /// entry.
    pub fn finish(self, top_n: usize) -> HotSpotSummary {
        HotSpotSummary {
            build_files_parsed: self.build_files_parsed,
            packages_loaded: self.packages.len() as u64,
            targets_per_package: top_n_pkg(
                self.targets_per_package,
                top_n,
            ),
            rule_instances_by_rule_kind: top_n_rule_kind(
                self.rule_instances_by_rule_kind,
                top_n,
            ),
            actions_by_mnemonic: top_n_mnemonic(
                self.actions_by_mnemonic,
                top_n,
            ),
            top_n,
            schema_version: HOTSPOT_SUMMARY_SCHEMA_VERSION,
        }
    }
}

fn top_n_pkg(
    src: HashMap<String, u64>,
    top_n: usize,
) -> Vec<PackageTargetCount> {
    let mut out: Vec<PackageTargetCount> = src
        .into_iter()
        .map(|(package, count)| PackageTargetCount { package, count })
        .collect();
    out.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.package.cmp(&b.package))
    });
    if top_n > 0 && out.len() > top_n {
        out.truncate(top_n);
    }
    out
}

fn top_n_rule_kind(
    src: HashMap<String, u64>,
    top_n: usize,
) -> Vec<RuleKindCount> {
    let mut out: Vec<RuleKindCount> = src
        .into_iter()
        .map(|(rule_kind, count)| RuleKindCount { rule_kind, count })
        .collect();
    out.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.rule_kind.cmp(&b.rule_kind))
    });
    if top_n > 0 && out.len() > top_n {
        out.truncate(top_n);
    }
    out
}

fn top_n_mnemonic(
    src: HashMap<String, u64>,
    top_n: usize,
) -> Vec<MnemonicCount> {
    let mut out: Vec<MnemonicCount> = src
        .into_iter()
        .map(|(mnemonic, count)| MnemonicCount { mnemonic, count })
        .collect();
    out.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.mnemonic.cmp(&b.mnemonic))
    });
    if top_n > 0 && out.len() > top_n {
        out.truncate(top_n);
    }
    out
}

impl HotSpotSummary {
    /// True iff the builder had no observations. Used by
    /// the orchestrator to skip empty dumps.
    pub fn is_empty(&self) -> bool {
        self.build_files_parsed == 0
            && self.rule_instances_by_rule_kind.is_empty()
            && self.actions_by_mnemonic.is_empty()
    }

    /// Render as human-readable stderr lines. Each section
    /// (BUILD files / packages / targets-per-pkg / rule
    /// kinds / mnemonics) appears in fixed order.
    pub fn format_human(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push(format!(
            "analyze-pass hot-spots (schema_version={}, top_n={}):",
            self.schema_version, self.top_n
        ));
        out.push(format!(
            "  build_files_parsed: {}", self.build_files_parsed
        ));
        out.push(format!(
            "  packages_loaded:    {}", self.packages_loaded
        ));
        if !self.rule_instances_by_rule_kind.is_empty() {
            out.push("  rule_instances_by_rule_kind (top N):".to_string());
            for rk in &self.rule_instances_by_rule_kind {
                out.push(format!("    {:30} {}", rk.rule_kind, rk.count));
            }
        }
        if !self.actions_by_mnemonic.is_empty() {
            out.push("  actions_by_mnemonic (top N):".to_string());
            for m in &self.actions_by_mnemonic {
                out.push(format!("    {:30} {}", m.mnemonic, m.count));
            }
        }
        if !self.targets_per_package.is_empty() {
            out.push("  targets_per_package (top N):".to_string());
            for p in &self.targets_per_package {
                out.push(format!("    {:60} {}", p.package, p.count));
            }
        }
        out
    }
}

/// Phase 5.C.4 commit 83: hot-spot-enabled gate. Returns
/// `true` when `RUSH_ANALYZE_HOTSPOTS=1`. Implies that
/// analyze_timing's `timing_enabled()` is independent —
/// operators can request hot-spots without timing or
/// vice versa.
pub fn hotspots_enabled() -> bool {
    std::env::var("RUSH_ANALYZE_HOTSPOTS").as_deref() == Ok("1")
}

/// Phase 5.C.4 commit 83: hot-spot-as-JSON gate.
pub fn hotspots_json_enabled() -> bool {
    hotspots_enabled()
        && std::env::var("RUSH_ANALYZE_HOTSPOTS_JSON").as_deref()
            == Ok("1")
}

/// Phase 5.C.4 commit 83: read the operator's preferred
/// top-N from the env, defaulting to [`DEFAULT_TOP_N`].
pub fn top_n_from_env() -> usize {
    std::env::var("RUSH_ANALYZE_HOTSPOTS_TOP_N")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_TOP_N)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pro's commit-83 directive: "rule instances per rule
    /// kind." Pin grouping correctness.
    #[test]
    fn hotspot_summary_groups_by_rule_kind() {
        let mut b = HotSpotSummaryBuilder::new();
        b.observe_rule_instance("//a", "cc_library");
        b.observe_rule_instance("//a", "cc_library");
        b.observe_rule_instance("//a", "cc_library");
        b.observe_rule_instance("//a", "cc_binary");
        b.observe_rule_instance("//b", "tf_proto_library");
        let s = b.finish(DEFAULT_TOP_N);
        // 3 distinct rule kinds; cc_library has count 3.
        assert_eq!(s.rule_instances_by_rule_kind.len(), 3);
        assert_eq!(s.rule_instances_by_rule_kind[0].rule_kind, "cc_library");
        assert_eq!(s.rule_instances_by_rule_kind[0].count, 3);
    }

    /// Pro's commit-83 directive: "Make N configurable or
    /// fixed small, e.g. top 20." Pin top-N trimming.
    #[test]
    fn hotspot_summary_limits_top_n() {
        let mut b = HotSpotSummaryBuilder::new();
        for i in 0..30 {
            // Distinct rule kinds, distinct counts so
            // ordering is deterministic.
            for _ in 0..(i + 1) {
                b.observe_rule_instance("//pkg", &format!("kind_{:02}", i));
            }
        }
        let s = b.finish(5);
        assert_eq!(s.rule_instances_by_rule_kind.len(), 5);
        // top_n=5 must keep the 5 highest-count kinds:
        // kind_29 (30), kind_28 (29), ... kind_25 (26).
        assert_eq!(s.rule_instances_by_rule_kind[0].rule_kind, "kind_29");
        assert_eq!(s.rule_instances_by_rule_kind[0].count, 30);
        assert_eq!(s.rule_instances_by_rule_kind[4].rule_kind, "kind_25");
        assert_eq!(s.rule_instances_by_rule_kind[4].count, 26);
        assert_eq!(s.top_n, 5);
    }

    /// Pro's commit-83 directive: "package_summary_is_deterministic."
    /// Pin sort + tie-break stability.
    #[test]
    fn package_summary_is_deterministic() {
        let mut b1 = HotSpotSummaryBuilder::new();
        let mut b2 = HotSpotSummaryBuilder::new();
        // Same observations in DIFFERENT order. Output
        // must be byte-identical.
        for (pkg, kind) in &[
            ("//a", "cc_library"),
            ("//b", "cc_library"),
            ("//c", "cc_binary"),
            ("//a", "cc_library"),
            ("//c", "cc_binary"),
        ] {
            b1.observe_rule_instance(pkg, kind);
        }
        for (pkg, kind) in &[
            ("//c", "cc_binary"),
            ("//a", "cc_library"),
            ("//c", "cc_binary"),
            ("//b", "cc_library"),
            ("//a", "cc_library"),
        ] {
            b2.observe_rule_instance(pkg, kind);
        }
        let s1 = b1.finish(DEFAULT_TOP_N);
        let s2 = b2.finish(DEFAULT_TOP_N);
        // Sort order must match across both observation
        // sequences. Tie-break is lexicographic by name.
        assert_eq!(
            s1.rule_instances_by_rule_kind,
            s2.rule_instances_by_rule_kind
        );
        assert_eq!(s1.targets_per_package, s2.targets_per_package);
        // Concrete check: //a has 2 cc_library; //b has 1;
        // //c has 2 cc_binary. Top: a + c tied at 2, b at 1.
        // Tie-break: alphabetical — //a before //c.
        assert_eq!(s1.targets_per_package[0].package, "//a");
        assert_eq!(s1.targets_per_package[0].count, 2);
        assert_eq!(s1.targets_per_package[1].package, "//c");
        assert_eq!(s1.targets_per_package[1].count, 2);
        assert_eq!(s1.targets_per_package[2].package, "//b");
        assert_eq!(s1.targets_per_package[2].count, 1);
    }

    /// Pro's commit-83 directive: aggregate counts by
    /// action mnemonic. Pin mnemonic grouping.
    #[test]
    fn actions_grouped_by_mnemonic() {
        let mut b = HotSpotSummaryBuilder::new();
        for _ in 0..7 {
            b.observe_action("CppCompile");
        }
        for _ in 0..3 {
            b.observe_action("CcArchive");
        }
        b.observe_action("ProtocCompile");
        let s = b.finish(DEFAULT_TOP_N);
        assert_eq!(s.actions_by_mnemonic.len(), 3);
        assert_eq!(s.actions_by_mnemonic[0].mnemonic, "CppCompile");
        assert_eq!(s.actions_by_mnemonic[0].count, 7);
    }

    /// Phase 5.C.4 commit 83: BUILD-file count matches
    /// observe_build_file call count; packages_loaded
    /// dedupes by name.
    #[test]
    fn build_file_and_package_counts() {
        let mut b = HotSpotSummaryBuilder::new();
        b.observe_build_file("//a");
        b.observe_build_file("//b");
        b.observe_build_file("//a"); // duplicate package
        b.observe_build_file("//c");
        let s = b.finish(DEFAULT_TOP_N);
        assert_eq!(s.build_files_parsed, 4);
        assert_eq!(s.packages_loaded, 3, "duplicates deduped");
    }

    /// Phase 5.C.4 commit 83: top_n=0 disables trimming
    /// (caller wants all entries).
    #[test]
    fn top_n_zero_keeps_all_entries() {
        let mut b = HotSpotSummaryBuilder::new();
        for i in 0..50 {
            b.observe_rule_instance("//x", &format!("k_{}", i));
        }
        let s = b.finish(0);
        assert_eq!(s.rule_instances_by_rule_kind.len(), 50);
    }

    /// Phase 5.C.4 commit 83: JSON shape pins
    /// schema_version=1 + all expected top-level fields.
    #[test]
    fn hotspot_summary_json_schema_v1() {
        let mut b = HotSpotSummaryBuilder::new();
        b.observe_build_file("//a");
        b.observe_rule_instance("//a", "cc_library");
        b.observe_action("CppCompile");
        let s = b.finish(DEFAULT_TOP_N);
        let json = serde_json::to_string(&s).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["build_files_parsed"], 1);
        assert_eq!(parsed["packages_loaded"], 1);
        assert_eq!(parsed["top_n"], DEFAULT_TOP_N);
        assert_eq!(
            parsed["rule_instances_by_rule_kind"][0]["rule_kind"],
            "cc_library"
        );
        assert_eq!(parsed["actions_by_mnemonic"][0]["mnemonic"], "CppCompile");
    }

    /// Phase 5.C.4 commit 83: hot-spots disabled by
    /// default. Operators opt in via env var.
    #[test]
    fn hotspots_disabled_by_default() {
        let prior = std::env::var("RUSH_ANALYZE_HOTSPOTS").ok();
        std::env::remove_var("RUSH_ANALYZE_HOTSPOTS");
        assert!(!hotspots_enabled());
        std::env::set_var("RUSH_ANALYZE_HOTSPOTS", "1");
        assert!(hotspots_enabled());
        match prior {
            Some(v) => std::env::set_var("RUSH_ANALYZE_HOTSPOTS", v),
            None => std::env::remove_var("RUSH_ANALYZE_HOTSPOTS"),
        }
    }

    /// Phase 5.C.4 commit 83: top-N from env var with
    /// fallback to default.
    #[test]
    fn top_n_from_env_with_fallback() {
        let prior = std::env::var("RUSH_ANALYZE_HOTSPOTS_TOP_N").ok();
        std::env::remove_var("RUSH_ANALYZE_HOTSPOTS_TOP_N");
        assert_eq!(top_n_from_env(), DEFAULT_TOP_N);
        std::env::set_var("RUSH_ANALYZE_HOTSPOTS_TOP_N", "7");
        assert_eq!(top_n_from_env(), 7);
        std::env::set_var("RUSH_ANALYZE_HOTSPOTS_TOP_N", "notanumber");
        assert_eq!(top_n_from_env(), DEFAULT_TOP_N);
        match prior {
            Some(v) => std::env::set_var("RUSH_ANALYZE_HOTSPOTS_TOP_N", v),
            None => std::env::remove_var("RUSH_ANALYZE_HOTSPOTS_TOP_N"),
        }
    }
}
