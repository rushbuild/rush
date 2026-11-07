// Rush Build - Per-package BUILD-file parse timing (phase 5.C.4 commit 88)
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 commit 88: per-package BUILD-file parse
//! timing instrumentation.
//!
//! Pro's commit-88 directive: *"Add per-package timing
//! inside the build_file_parse phase. Record repo, package,
//! BUILD file path, total duration, parse/eval invocation
//! count, unique vs repeated load count, target labels
//! requested, rule count. Make the report show both
//! UNIQUE BUILD files parsed and TOTAL BUILD-file parse
//! calls."*
//!
//! This module is the load-bearing piece that answers
//! Pro's commit-87 follow-up question:
//!
//!   "114 build_file_parse items for 4 resulting targets —
//!    parsing 114 unique files, or one file 114 times?"
//!
//! Per-package timing captures BOTH the unique count
//! (`packages.len()`) and the total invocation count
//! (`total_invocations`) so the gap between them surfaces
//! immediately.
//!
//! ## Cost model
//!
//! - Disabled: zero. Observations short-circuit on the
//!   parse-timing-enabled gate.
//! - Enabled: one HashMap insert per parse call + one
//!   Duration field update. The orchestrator's parse loop
//!   already iterates packages in O(n); this is O(1) per
//!   step.

use std::collections::HashMap;
use std::time::Duration;

/// Phase 5.C.4 commit 88: one per-package parse-timing
/// record.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct PackageParseEntry {
    /// External-repo name (empty string for the main
    /// workspace).
    pub repo: String,
    /// Package label without leading `//` (e.g.
    /// `tensorflow/core/platform`).
    pub package: String,
    /// Resolved path to the BUILD file.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub build_file: String,
    /// Cumulative wall-clock time spent parsing this
    /// package. Serialized as milliseconds.
    #[serde(serialize_with = "ser_duration_ms")]
    pub total_duration: Duration,
    /// How many times the orchestrator called parse for
    /// this package. With proper memoization this is 1;
    /// values > 1 indicate duplicate parsing.
    pub invocation_count: u64,
    /// Rule instances produced by the most recent parse.
    /// Optional because some parse calls are skipped early
    /// (e.g. test-filter rejection) without producing a
    /// rule count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_count: Option<u64>,
    /// Phase 5.C.4 commit 90: per-subphase timing breakdown.
    /// `None` when subphase recording is disabled.
    /// Pro's commit-90 directive: "Break build_file_parse
    /// into smaller phase timers."
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subphases: Option<ParseSubphaseTimings>,
    /// Phase 5.C.4 commit 91: load-provenance summary.
    /// Counts per [`LoadReason`] for this package — if
    /// the same package is loaded multiple times for
    /// different reasons, each reason gets its own
    /// counter. Empty when provenance recording is
    /// disabled.
    ///
    /// Pro's commit-91 directive: "Add 'why was this
    /// package loaded?' provenance."
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub load_reasons: Vec<LoadReasonCount>,
}

/// Phase 5.C.4 commit 91: why a package was loaded.
///
/// Pro's commit-91 spec:
///
/// ```text
/// target_pattern     — explicit user target on the CLI
/// direct_dependency  — named via `deps = [...]`
/// implicit_dependency — synthesized header / glob expansion
/// external_repo_dependency — @repo//path/to:target
/// alias_resolution   — alias target redirect
/// unknown            — fallback (provenance not threaded
///                       through this code path yet)
/// ```
///
/// JSON shape (commit 54b style):
///   "target_pattern"
///   "direct_dependency"
///   ...
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadReason {
    /// Explicit user target pattern on the CLI.
    TargetPattern,
    /// Named via `deps = [...]` on another rule.
    DirectDependency,
    /// Synthesized (e.g. system_cc_library header rule
    /// for an unmodeled `.h` file).
    ImplicitDependency,
    /// External repo path (`@repo//...`).
    ExternalRepoDependency,
    /// Alias target redirect.
    AliasResolution,
    /// Provenance not threaded through this code path
    /// yet. Fallback variant.
    Unknown,
}

impl LoadReason {
    /// Short label matching the JSON encoding.
    pub fn label(&self) -> &'static str {
        match self {
            Self::TargetPattern => "target_pattern",
            Self::DirectDependency => "direct_dependency",
            Self::ImplicitDependency => "implicit_dependency",
            Self::ExternalRepoDependency => "external_repo_dependency",
            Self::AliasResolution => "alias_resolution",
            Self::Unknown => "unknown",
        }
    }
}

/// Phase 5.C.4 commit 91: counted load-reason entry,
/// embedded in [`PackageParseEntry::load_reasons`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LoadReasonCount {
    /// Why the package was loaded.
    pub reason: LoadReason,
    /// How many times this reason fired.
    pub count: u64,
}

/// Phase 5.C.4 commit 90: per-subphase timing breakdown
/// for one BUILD-file parse invocation.
///
/// Pro suggested either a 9-step breakdown
/// (read/starlark_parse/bzl_load_resolution/eval/macro_expand/
/// glob/instantiate/attr_normalize/finalize) or a coarser
/// 5-step (read/parse/eval_expand/instantiate/finalize).
///
/// We ship the coarser 2-step split that maps directly to
/// what the orchestrator can observe today without
/// rush-starlark-side instrumentation:
///
///   - `starlark_eval` — parse_build_file_cached
///                       (lexer + parser + .bzl loads +
///                        Starlark eval + macro expansion +
///                        glob resolution all happen here;
///                        commit 89's cache counters confirm
///                        .bzl loading dominates this phase)
///   - `instantiate`   — build_file_to_instances
///                       (converts parsed BuildFile into
///                        rush_rules::RuleInstance objects)
///
/// Finer breakdown requires rush-starlark-side timing
/// hooks (lexer / parser / .bzl-load / eval / macro /
/// glob) which is a follow-up. The 2-step split is enough
/// to answer "is the cost in Starlark or in instance
/// creation?" — and commit 89's data plus this split will
/// pin "in Starlark, specifically .bzl loading."
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ParseSubphaseTimings {
    /// Time inside `parse_build_file_cached`: Starlark
    /// lex/parse, .bzl load resolution, Starlark
    /// evaluation, macro expansion, glob resolution.
    #[serde(serialize_with = "ser_duration_ms")]
    pub starlark_eval: Duration,
    /// Time inside `build_file_to_instances`: converting
    /// parsed BuildFile rules to `RuleInstance` objects.
    #[serde(serialize_with = "ser_duration_ms")]
    pub instantiate: Duration,
}

fn ser_duration_ms<S: serde::Serializer>(
    d: &Duration,
    s: S,
) -> Result<S::Ok, S::Error> {
    s.serialize_u64(d.as_millis() as u64)
}

/// Phase 5.C.4 commit 88: aggregate per-package
/// parse-timing report.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PackageParseTimingSummary {
    /// Schema version for the JSON wire shape. Bumped on
    /// breaking changes; CI consumers gate on equality.
    pub schema_version: u32,
    /// Number of DISTINCT (repo, package) pairs parsed.
    /// This is the "would-be-cached" count.
    pub unique_packages: u64,
    /// Number of TOTAL parse invocations across all
    /// packages. With proper memoization this equals
    /// `unique_packages`; values larger indicate
    /// duplicate work.
    pub total_invocations: u64,
    /// Difference between total and unique. Zero is
    /// healthy; any positive value is the duplicate-work
    /// signal Pro's commit-87 follow-up asked about.
    pub duplicate_invocations: u64,
    /// Top-N entries by total_duration (descending).
    /// Deterministic tie-break: total_invocations
    /// descending, then (repo, package) lexicographic.
    pub top_packages_by_duration: Vec<PackageParseEntry>,
    /// Top-N entries by invocation_count (descending).
    /// Surfaces packages that are parsed repeatedly even
    /// if each call is fast — these are the ones a
    /// memoization fix would help most.
    pub top_packages_by_invocations: Vec<PackageParseEntry>,
    /// Top-N parameter applied.
    pub top_n: usize,
}

/// Phase 5.C.4 commit 88: schema version for the parse-
/// timing JSON wire format.
pub const PACKAGE_PARSE_TIMING_SCHEMA_VERSION: u32 = 1;

/// Default top-N when callers don't override.
pub const DEFAULT_TOP_N: usize = 20;

/// Phase 5.C.4 commit 88: builder used by the
/// orchestrator's parse loop.
#[derive(Debug, Default)]
pub struct PackageParseTimingBuilder {
    by_package: HashMap<(String, String), PackageParseEntry>,
    total_invocations: u64,
}

impl PackageParseTimingBuilder {
    /// Build a fresh builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one parse invocation for a package.
    ///
    /// `repo` is the external-repo name or empty for the
    /// main workspace. `package` is the package label
    /// (without leading `//`). `build_file` is the
    /// resolved BUILD or BUILD.bazel path. `duration` is
    /// the wall-clock cost of the parse call.
    /// `rule_count` is the number of RuleInstances
    /// produced (None when the caller doesn't track this).
    pub fn observe_parse(
        &mut self,
        repo: &str,
        package: &str,
        build_file: &str,
        duration: Duration,
        rule_count: Option<u64>,
    ) {
        self.observe_parse_with_subphases(
            repo, package, build_file, duration, rule_count, None,
        );
    }

    /// Phase 5.C.4 commit 91: record one parse invocation
    /// with optional subphase + load-reason provenance.
    /// Wraps [`observe_parse_with_subphases`] with an
    /// extra reason argument.
    pub fn observe_parse_full(
        &mut self,
        repo: &str,
        package: &str,
        build_file: &str,
        duration: Duration,
        rule_count: Option<u64>,
        subphases: Option<ParseSubphaseTimings>,
        reason: Option<LoadReason>,
    ) {
        self.observe_parse_with_subphases(
            repo, package, build_file, duration, rule_count, subphases,
        );
        if let Some(r) = reason {
            let entry = self
                .by_package
                .get_mut(&(repo.to_string(), package.to_string()))
                .expect("observe_parse_with_subphases inserted entry");
            if let Some(existing) =
                entry.load_reasons.iter_mut().find(|e| e.reason == r)
            {
                existing.count += 1;
            } else {
                entry.load_reasons.push(LoadReasonCount {
                    reason: r,
                    count: 1,
                });
            }
        }
    }

    /// Phase 5.C.4 commit 90: record one parse invocation
    /// with optional subphase breakdown.
    ///
    /// `subphases.starlark_eval + subphases.instantiate`
    /// should approximately equal `duration` (small slack
    /// for measurement overhead is expected). Pro's
    /// commit-90 test:
    /// `subphase_timings_sum_or_are_bounded_by_parent`
    /// pins this invariant.
    pub fn observe_parse_with_subphases(
        &mut self,
        repo: &str,
        package: &str,
        build_file: &str,
        duration: Duration,
        rule_count: Option<u64>,
        subphases: Option<ParseSubphaseTimings>,
    ) {
        self.total_invocations += 1;
        let entry = self
            .by_package
            .entry((repo.to_string(), package.to_string()))
            .or_insert_with(|| PackageParseEntry {
                repo: repo.to_string(),
                package: package.to_string(),
                build_file: build_file.to_string(),
                total_duration: Duration::ZERO,
                invocation_count: 0,
                rule_count: None,
                subphases: None,
                load_reasons: Vec::new(),
            });
        entry.total_duration += duration;
        entry.invocation_count += 1;
        if let Some(c) = rule_count {
            // Last-write-wins on rule_count — the most
            // recent parse's count is the authoritative
            // figure for the package state today.
            entry.rule_count = Some(c);
        }
        if let Some(s) = subphases {
            // Accumulate subphase durations: if the same
            // package is parsed multiple times,
            // subphase totals sum just like the parent
            // total_duration.
            match &mut entry.subphases {
                Some(existing) => {
                    existing.starlark_eval += s.starlark_eval;
                    existing.instantiate += s.instantiate;
                }
                None => entry.subphases = Some(s),
            }
        }
    }

    /// Finalize: produce the immutable summary with
    /// top-N sorting applied. `top_n = 0` keeps all
    /// entries.
    pub fn finish(self, top_n: usize) -> PackageParseTimingSummary {
        let unique_packages = self.by_package.len() as u64;
        let total_invocations = self.total_invocations;
        let duplicate_invocations =
            total_invocations.saturating_sub(unique_packages);

        // Sort by total_duration desc. Tie-break:
        // invocation_count desc, then (repo, package)
        // lexicographic.
        let mut by_duration: Vec<PackageParseEntry> =
            self.by_package.values().cloned().collect();
        by_duration.sort_by(|a, b| {
            b.total_duration
                .cmp(&a.total_duration)
                .then(b.invocation_count.cmp(&a.invocation_count))
                .then(a.repo.cmp(&b.repo))
                .then(a.package.cmp(&b.package))
        });
        if top_n > 0 && by_duration.len() > top_n {
            by_duration.truncate(top_n);
        }

        // Sort by invocation_count desc (surfaces packages
        // parsed repeatedly). Tie-break same as above
        // minus the leading duration check.
        let mut by_invocations: Vec<PackageParseEntry> =
            self.by_package.into_values().collect();
        by_invocations.sort_by(|a, b| {
            b.invocation_count
                .cmp(&a.invocation_count)
                .then(b.total_duration.cmp(&a.total_duration))
                .then(a.repo.cmp(&b.repo))
                .then(a.package.cmp(&b.package))
        });
        if top_n > 0 && by_invocations.len() > top_n {
            by_invocations.truncate(top_n);
        }

        PackageParseTimingSummary {
            schema_version: PACKAGE_PARSE_TIMING_SCHEMA_VERSION,
            unique_packages,
            total_invocations,
            duplicate_invocations,
            top_packages_by_duration: by_duration,
            top_packages_by_invocations: by_invocations,
            top_n,
        }
    }
}

impl PackageParseTimingSummary {
    /// True iff no observations were recorded.
    pub fn is_empty(&self) -> bool {
        self.total_invocations == 0
    }

    /// Render as human-readable stderr lines.
    pub fn format_human(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push(format!(
            "package-parse timing (schema_version={}, top_n={}):",
            self.schema_version, self.top_n
        ));
        out.push(format!(
            "  unique_packages:     {}",
            self.unique_packages
        ));
        out.push(format!(
            "  total_invocations:   {}",
            self.total_invocations
        ));
        // The duplicate signal. Zero is healthy; positive
        // is the "we parsed the same thing N times" tell
        // that Pro flagged.
        out.push(format!(
            "  duplicate_invocations: {}{}",
            self.duplicate_invocations,
            if self.duplicate_invocations > 0 {
                "  ← duplicate parsing detected"
            } else {
                ""
            },
        ));
        if !self.top_packages_by_duration.is_empty() {
            out.push("  top_packages_by_duration:".to_string());
            for e in &self.top_packages_by_duration {
                out.push(format!(
                    "    {:60} {} ms  (× {} calls{})",
                    format_pkg_label(&e.repo, &e.package),
                    e.total_duration.as_millis(),
                    e.invocation_count,
                    e.rule_count
                        .map(|c| format!(", {} rules", c))
                        .unwrap_or_default(),
                ));
                // Phase 5.C.4 commit 90: render subphase
                // breakdown on the next line when present.
                if let Some(s) = &e.subphases {
                    out.push(format!(
                        "      ↳ starlark_eval: {} ms / instantiate: {} ms",
                        s.starlark_eval.as_millis(),
                        s.instantiate.as_millis(),
                    ));
                }
                // Phase 5.C.4 commit 91: render load
                // provenance counts inline.
                if !e.load_reasons.is_empty() {
                    let reasons: Vec<String> = e
                        .load_reasons
                        .iter()
                        .map(|r| format!("{}={}", r.reason.label(), r.count))
                        .collect();
                    out.push(format!(
                        "      ↳ loaded via: {}",
                        reasons.join(", ")
                    ));
                }
            }
        }
        if !self.top_packages_by_invocations.is_empty() {
            out.push("  top_packages_by_invocations:".to_string());
            for e in &self.top_packages_by_invocations {
                out.push(format!(
                    "    {:60} × {} calls  ({} ms total{})",
                    format_pkg_label(&e.repo, &e.package),
                    e.invocation_count,
                    e.total_duration.as_millis(),
                    e.rule_count
                        .map(|c| format!(", {} rules", c))
                        .unwrap_or_default(),
                ));
            }
        }
        out
    }
}

fn format_pkg_label(repo: &str, package: &str) -> String {
    if repo.is_empty() {
        format!("//{}", package)
    } else {
        format!("@{}//{}", repo, package)
    }
}

/// Phase 5.C.4 commit 88: parse-timing-enabled gate.
///
/// `RUSH_ANALYZE_PARSE_TIMING=1` enables per-package
/// recording. Implies `RUSH_ANALYZE_TIMING=1` from
/// commit 82 (the broader analyze-pass timing gate),
/// which the orchestrator already checks. Operators can
/// enable per-package timing without the JSON variant.
pub fn parse_timing_enabled() -> bool {
    std::env::var("RUSH_ANALYZE_PARSE_TIMING").as_deref() == Ok("1")
}

/// Phase 5.C.4 commit 88: parse-timing JSON gate.
pub fn parse_timing_json_enabled() -> bool {
    parse_timing_enabled()
        && std::env::var("RUSH_ANALYZE_PARSE_TIMING_JSON").as_deref()
            == Ok("1")
}

/// Phase 5.C.4 commit 88: top-N from env, defaulting to
/// [`DEFAULT_TOP_N`].
pub fn top_n_from_env() -> usize {
    std::env::var("RUSH_ANALYZE_PARSE_TIMING_TOP_N")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_TOP_N)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pro's commit-88 directive: per-package timing
    /// groups observations by (repo, package).
    #[test]
    fn per_package_timing_groups_by_repo_package() {
        let mut b = PackageParseTimingBuilder::new();
        b.observe_parse(
            "",
            "tensorflow/core",
            "tensorflow/core/BUILD",
            Duration::from_millis(120),
            Some(80),
        );
        b.observe_parse(
            "",
            "tensorflow/core/platform",
            "tensorflow/core/platform/BUILD",
            Duration::from_millis(96000),
            Some(126),
        );
        b.observe_parse(
            "com_google_protobuf",
            "src",
            "external/com_google_protobuf/src/BUILD",
            Duration::from_millis(40),
            Some(12),
        );
        let s = b.finish(DEFAULT_TOP_N);
        assert_eq!(s.unique_packages, 3);
        assert_eq!(s.total_invocations, 3);
        assert_eq!(s.duplicate_invocations, 0);
        // Top by duration: platform first.
        assert_eq!(
            s.top_packages_by_duration[0].package,
            "tensorflow/core/platform"
        );
        assert_eq!(
            s.top_packages_by_duration[0].total_duration,
            Duration::from_millis(96000)
        );
    }

    /// Pro's commit-88 directive: repeated package loads
    /// accumulate count, not duplicate entries.
    /// CRITICAL: this is the data shape that surfaces the
    /// "parsing 114 unique vs 114 calls of one file" gap.
    #[test]
    fn repeated_package_loads_accumulate_count() {
        let mut b = PackageParseTimingBuilder::new();
        for _ in 0..5 {
            b.observe_parse(
                "",
                "tensorflow/core/platform",
                "tensorflow/core/platform/BUILD",
                Duration::from_millis(20),
                Some(126),
            );
        }
        let s = b.finish(DEFAULT_TOP_N);
        // ONE unique package; FIVE total invocations.
        assert_eq!(s.unique_packages, 1);
        assert_eq!(s.total_invocations, 5);
        assert_eq!(s.duplicate_invocations, 4);
        // Cumulative duration = 5 × 20 ms = 100 ms.
        assert_eq!(
            s.top_packages_by_duration[0].total_duration,
            Duration::from_millis(100)
        );
        assert_eq!(
            s.top_packages_by_duration[0].invocation_count,
            5
        );
    }

    /// Pro's commit-88 directive: top_n_deterministic.
    /// Out-of-order observations must produce identical
    /// ordering.
    #[test]
    fn top_n_deterministic() {
        let mut b1 = PackageParseTimingBuilder::new();
        let mut b2 = PackageParseTimingBuilder::new();
        let observations = [
            ("", "a", Duration::from_millis(10), 5),
            ("", "b", Duration::from_millis(30), 5),
            ("", "c", Duration::from_millis(20), 5),
            ("", "d", Duration::from_millis(30), 1), // ties with b on duration
            ("", "e", Duration::from_millis(10), 5),
        ];
        for &(r, p, d, c) in &observations {
            b1.observe_parse(r, p, "", d, Some(c));
        }
        // Reverse order for b2.
        for &(r, p, d, c) in observations.iter().rev() {
            b2.observe_parse(r, p, "", d, Some(c));
        }
        let s1 = b1.finish(DEFAULT_TOP_N);
        let s2 = b2.finish(DEFAULT_TOP_N);
        // Byte-identical top-N: deterministic regardless
        // of observation order.
        let p1: Vec<String> = s1
            .top_packages_by_duration
            .iter()
            .map(|e| e.package.clone())
            .collect();
        let p2: Vec<String> = s2
            .top_packages_by_duration
            .iter()
            .map(|e| e.package.clone())
            .collect();
        assert_eq!(p1, p2);
        // Concrete check: d ties with b at 30 ms; b has
        // 5 invocations vs d's 1, so b sorts ahead.
        assert_eq!(p1[0], "b");
        assert_eq!(p1[1], "d");
        // Next: c (20 ms). Then a + e tied at 10 ms;
        // alphabetical tie-break — a before e.
        assert_eq!(p1[2], "c");
        assert_eq!(p1[3], "a");
        assert_eq!(p1[4], "e");
    }

    /// Phase 5.C.4 commit 88: top_n trimming.
    #[test]
    fn top_n_trims_to_requested_length() {
        let mut b = PackageParseTimingBuilder::new();
        for i in 0..30 {
            b.observe_parse(
                "",
                &format!("pkg_{:02}", i),
                "",
                Duration::from_millis(i as u64 + 1),
                Some(1),
            );
        }
        let s = b.finish(7);
        assert_eq!(s.top_packages_by_duration.len(), 7);
        assert_eq!(s.unique_packages, 30);
    }

    /// Phase 5.C.4 commit 88: external-repo prefix shows
    /// up in the formatter.
    #[test]
    fn external_repo_label_formats_with_at_prefix() {
        let mut b = PackageParseTimingBuilder::new();
        b.observe_parse(
            "com_google_protobuf",
            "src",
            "",
            Duration::from_millis(10),
            None,
        );
        let s = b.finish(DEFAULT_TOP_N);
        let lines = s.format_human();
        assert!(
            lines.iter().any(|l| l.contains("@com_google_protobuf//src")),
            "expected external-repo label with @ prefix; got {:?}",
            lines
        );
    }

    /// Phase 5.C.4 commit 88: JSON shape pins
    /// schema_version=1 + all expected fields.
    #[test]
    fn json_serialization_schema_v1() {
        let mut b = PackageParseTimingBuilder::new();
        b.observe_parse(
            "",
            "a",
            "a/BUILD",
            Duration::from_millis(50),
            Some(3),
        );
        b.observe_parse(
            "",
            "a",
            "a/BUILD",
            Duration::from_millis(70),
            Some(3),
        );
        let s = b.finish(DEFAULT_TOP_N);
        let json = serde_json::to_string(&s).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["unique_packages"], 1);
        assert_eq!(parsed["total_invocations"], 2);
        assert_eq!(parsed["duplicate_invocations"], 1);
        assert_eq!(
            parsed["top_packages_by_duration"][0]["total_duration"],
            120
        );
        assert_eq!(
            parsed["top_packages_by_duration"][0]["invocation_count"],
            2
        );
        assert_eq!(
            parsed["top_packages_by_duration"][0]["rule_count"],
            3
        );
    }

    /// Phase 5.C.4 commit 88: duplicate-invocations
    /// signal appears in human output when triggered.
    #[test]
    fn duplicate_parsing_signal_appears_in_human_output() {
        let mut b = PackageParseTimingBuilder::new();
        b.observe_parse("", "a", "", Duration::from_millis(10), Some(1));
        b.observe_parse("", "a", "", Duration::from_millis(10), Some(1));
        let s = b.finish(DEFAULT_TOP_N);
        let lines = s.format_human();
        assert!(
            lines.iter().any(|l| l.contains("duplicate parsing detected")),
            "expected duplicate-parsing warning in human output; got {:?}",
            lines
        );
    }

    /// Phase 5.C.4 commit 88: zero duplicates → no
    /// warning suffix.
    #[test]
    fn no_duplicate_signal_when_unique() {
        let mut b = PackageParseTimingBuilder::new();
        b.observe_parse("", "a", "", Duration::from_millis(10), Some(1));
        b.observe_parse("", "b", "", Duration::from_millis(10), Some(1));
        let s = b.finish(DEFAULT_TOP_N);
        let lines = s.format_human();
        assert!(
            !lines.iter().any(|l| l.contains("duplicate parsing detected")),
            "duplicate warning should not appear when all unique; got {:?}",
            lines
        );
    }

    /// Phase 5.C.4 commit 90: subphase timings sum or are
    /// bounded by parent. The orchestrator records
    /// starlark_eval + instantiate separately; their sum
    /// must be at most the parent total (with small slack
    /// for measurement overhead).
    #[test]
    fn subphase_timings_sum_or_are_bounded_by_parent() {
        let mut b = PackageParseTimingBuilder::new();
        let parent = Duration::from_millis(100);
        b.observe_parse_with_subphases(
            "",
            "p",
            "",
            parent,
            Some(5),
            Some(ParseSubphaseTimings {
                starlark_eval: Duration::from_millis(80),
                instantiate: Duration::from_millis(15),
            }),
        );
        let s = b.finish(DEFAULT_TOP_N);
        let e = &s.top_packages_by_duration[0];
        let subphases = e.subphases.as_ref().unwrap();
        let sum = subphases.starlark_eval + subphases.instantiate;
        // Subphase sum must not exceed parent duration
        // (allow 1ms slack for measurement overhead).
        assert!(
            sum <= parent + Duration::from_millis(1),
            "subphase sum {:?} exceeded parent {:?}",
            sum,
            parent
        );
    }

    /// Phase 5.C.4 commit 90: subphase timings persist
    /// across repeated parses (sum, not replace).
    #[test]
    fn subphase_timings_accumulate_across_repeats() {
        let mut b = PackageParseTimingBuilder::new();
        for _ in 0..3 {
            b.observe_parse_with_subphases(
                "",
                "p",
                "",
                Duration::from_millis(50),
                Some(1),
                Some(ParseSubphaseTimings {
                    starlark_eval: Duration::from_millis(40),
                    instantiate: Duration::from_millis(8),
                }),
            );
        }
        let s = b.finish(DEFAULT_TOP_N);
        let e = &s.top_packages_by_duration[0];
        let subphases = e.subphases.as_ref().unwrap();
        // 3 × 40 ms = 120 ms total in starlark_eval.
        assert_eq!(subphases.starlark_eval, Duration::from_millis(120));
        assert_eq!(subphases.instantiate, Duration::from_millis(24));
    }

    /// Phase 5.C.4 commit 90: JSON output contains
    /// subphase breakdown when present; absent when None.
    #[test]
    fn json_contains_subphase_breakdown_when_enabled() {
        let mut b = PackageParseTimingBuilder::new();
        b.observe_parse_with_subphases(
            "",
            "with_sub",
            "",
            Duration::from_millis(100),
            Some(1),
            Some(ParseSubphaseTimings {
                starlark_eval: Duration::from_millis(80),
                instantiate: Duration::from_millis(15),
            }),
        );
        b.observe_parse(
            "",
            "without_sub",
            "",
            Duration::from_millis(50),
            Some(1),
        );
        let s = b.finish(DEFAULT_TOP_N);
        let json = serde_json::to_string(&s).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        let entries = parsed["top_packages_by_duration"]
            .as_array()
            .expect("array");
        // The "with_sub" entry should have subphases set;
        // "without_sub" should not.
        let with_sub = entries
            .iter()
            .find(|e| e["package"] == "with_sub")
            .expect("with_sub entry");
        assert!(with_sub["subphases"].is_object());
        assert_eq!(with_sub["subphases"]["starlark_eval"], 80);
        assert_eq!(with_sub["subphases"]["instantiate"], 15);
        let without_sub = entries
            .iter()
            .find(|e| e["package"] == "without_sub")
            .expect("without_sub entry");
        assert!(
            without_sub.get("subphases").map_or(true, |v| v.is_null()),
            "subphases must be absent when None; got {:?}",
            without_sub
        );
    }

    /// Phase 5.C.4 commit 91: target_pattern reason
    /// recorded on outer-loop parse calls.
    #[test]
    fn target_pattern_load_records_reason() {
        let mut b = PackageParseTimingBuilder::new();
        b.observe_parse_full(
            "",
            "pkg",
            "",
            Duration::from_millis(10),
            Some(1),
            None,
            Some(LoadReason::TargetPattern),
        );
        let s = b.finish(DEFAULT_TOP_N);
        let e = &s.top_packages_by_duration[0];
        assert_eq!(e.load_reasons.len(), 1);
        assert_eq!(e.load_reasons[0].reason, LoadReason::TargetPattern);
        assert_eq!(e.load_reasons[0].count, 1);
    }

    /// Phase 5.C.4 commit 91: repeated loads with the
    /// SAME reason accumulate count, not duplicate
    /// entries.
    #[test]
    fn repeated_loads_preserve_reason_counts() {
        let mut b = PackageParseTimingBuilder::new();
        for _ in 0..4 {
            b.observe_parse_full(
                "",
                "pkg",
                "",
                Duration::from_millis(10),
                Some(1),
                None,
                Some(LoadReason::TargetPattern),
            );
        }
        let s = b.finish(DEFAULT_TOP_N);
        let e = &s.top_packages_by_duration[0];
        assert_eq!(e.load_reasons.len(), 1);
        assert_eq!(e.load_reasons[0].count, 4);
    }

    /// Phase 5.C.4 commit 91: same package loaded for
    /// different reasons keeps both reasons (separately
    /// counted).
    #[test]
    fn mixed_reasons_track_separate_counts() {
        let mut b = PackageParseTimingBuilder::new();
        b.observe_parse_full(
            "",
            "pkg",
            "",
            Duration::from_millis(10),
            Some(1),
            None,
            Some(LoadReason::TargetPattern),
        );
        b.observe_parse_full(
            "",
            "pkg",
            "",
            Duration::from_millis(10),
            Some(1),
            None,
            Some(LoadReason::DirectDependency),
        );
        b.observe_parse_full(
            "",
            "pkg",
            "",
            Duration::from_millis(10),
            Some(1),
            None,
            Some(LoadReason::DirectDependency),
        );
        let s = b.finish(DEFAULT_TOP_N);
        let e = &s.top_packages_by_duration[0];
        assert_eq!(e.load_reasons.len(), 2);
        let tp = e
            .load_reasons
            .iter()
            .find(|r| r.reason == LoadReason::TargetPattern)
            .unwrap();
        assert_eq!(tp.count, 1);
        let dd = e
            .load_reasons
            .iter()
            .find(|r| r.reason == LoadReason::DirectDependency)
            .unwrap();
        assert_eq!(dd.count, 2);
    }

    /// Phase 5.C.4 commit 91: LoadReason JSON encoding
    /// uses snake_case matching the rest of the schema.
    #[test]
    fn load_reason_json_encoding_is_snake_case() {
        let reasons = [
            (LoadReason::TargetPattern, "target_pattern"),
            (LoadReason::DirectDependency, "direct_dependency"),
            (LoadReason::ImplicitDependency, "implicit_dependency"),
            (
                LoadReason::ExternalRepoDependency,
                "external_repo_dependency",
            ),
            (LoadReason::AliasResolution, "alias_resolution"),
            (LoadReason::Unknown, "unknown"),
        ];
        for (r, label) in &reasons {
            assert_eq!(r.label(), *label);
            let json = serde_json::to_string(r).unwrap();
            assert_eq!(json, format!("\"{}\"", label));
        }
    }

    /// Phase 5.C.4 commit 88: env-var gates.
    #[test]
    fn parse_timing_gates_disabled_by_default() {
        let prior = std::env::var("RUSH_ANALYZE_PARSE_TIMING").ok();
        std::env::remove_var("RUSH_ANALYZE_PARSE_TIMING");
        assert!(!parse_timing_enabled());
        std::env::set_var("RUSH_ANALYZE_PARSE_TIMING", "1");
        assert!(parse_timing_enabled());
        match prior {
            Some(v) => std::env::set_var("RUSH_ANALYZE_PARSE_TIMING", v),
            None => std::env::remove_var("RUSH_ANALYZE_PARSE_TIMING"),
        }
    }
}
