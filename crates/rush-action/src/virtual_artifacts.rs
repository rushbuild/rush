// Rush Action - virtual CAS-backed artifact registry
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 4: virtual CAS-backed artifacts.
//!
//! When `RUSH_VIRTUAL_ARTIFACTS=1`, a cache hit whose outputs are absent
//! on disk may skip rematerialization entirely: the action manager
//! registers the full [`OutputRecord`]s here without publishing output-tree
//! metadata. Downstream action identities hash the verified registry value,
//! so cache lookups proceed without the bytes; a consumer that
//! actually MISSES materializes just the inputs it needs from the CAS at
//! execution time, and the outputs of the targets the user requested are
//! materialized at the end of the build.
//!
//! This module is deliberately CAS-free: it lives in `rush-action` so
//! both rule crates (post-execution `is_file()` checks) and the build
//! orchestrator can consult it. The CAS-aware materializer lives in
//! `rush-build::virtual_materializer`.
//!
//! The registry is process-global and rebuilt lazily each process: a
//! fresh daemon repopulates it as cached actions virtual-hit again
//! (producers always precede consumers in the wavefront), so no
//! persistence is needed.

use crate::artifact_value::OutputRecord;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

/// Pure decision function, unit-testable without process-global env
/// state. Virtual artifacts require the legacy execution path: the
/// scheduler runner is not action-cache aware, so its actions would
/// read virtual inputs without a JIT materialization seam.
pub fn compute_enabled(
    virtual_flag: Option<&str>,
    scheduler_flag: Option<&str>,
) -> bool {
    virtual_flag == Some("1") && scheduler_flag != Some("1")
}

/// 0 = unconfigured (fall back to this process's env), 1 = off, 2 = on.
static MODE: AtomicU8 = AtomicU8::new(0);

/// Configure the mode for the build that is starting and reset the
/// per-build counters.
///
/// `rushd` is long-lived and is spawned with whatever environment the
/// FIRST client happened to have, so reading the daemon's own `std::env`
/// would let one client's flag silently virtualize every later client's
/// build (and would make a later client unable to turn it off). Callers
/// pass the CLIENT's env — the same rule
/// [`rush_command_core::CommandSemantics`] exists to enforce.
pub fn begin_build(virtual_flag: Option<&str>, scheduler_flag: Option<&str>) {
    let on = compute_enabled(virtual_flag, scheduler_flag);
    if virtual_flag == Some("1") && !on {
        eprintln!(
            "⚠️  RUSH_VIRTUAL_ARTIFACTS=1 ignored: the scheduler path \
             (RUSH_USE_SCHEDULER=1) is not virtual-artifact aware"
        );
    }
    MODE.store(if on { 2 } else { 1 }, Ordering::SeqCst);
    reset_build_stats();
    if on {
        // A resident daemon changing on-disk behavior must never be
        // silent: interior outputs legitimately stop existing.
        eprintln!(
            "🪶 virtual artifacts ON: interior outputs stay CAS-backed \
             (RUSH_VIRTUAL_ARTIFACTS=1)"
        );
    }
}

/// Whether virtual artifacts are enabled for the current build. Falls
/// back to this process's env when no build configured the mode (unit
/// tests, and any entry point that predates the semantics plumbing).
pub fn enabled() -> bool {
    match MODE.load(Ordering::SeqCst) {
        2 => true,
        1 => false,
        _ => {
            static ENV_FALLBACK: OnceLock<bool> = OnceLock::new();
            *ENV_FALLBACK.get_or_init(|| {
                let v = std::env::var("RUSH_VIRTUAL_ARTIFACTS").ok();
                let s = std::env::var("RUSH_USE_SCHEDULER").ok();
                compute_enabled(v.as_deref(), s.as_deref())
            })
        }
    }
}

fn registry() -> &'static Mutex<HashMap<PathBuf, OutputRecord>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, OutputRecord>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<PathBuf, OutputRecord>> {
    registry().lock().unwrap_or_else(|p| p.into_inner())
}

/// Register the output records of a virtual cache hit so consumers can
/// materialize the bytes on demand.
pub fn register(records: &[OutputRecord]) {
    let mut map = lock();
    for record in records {
        map.insert(record.path.clone(), record.clone());
    }
}

/// Drop any virtual registration for `paths`.
///
/// Called when an action is about to EXECUTE: from that moment the
/// on-disk state is authoritative for its outputs, and a record from an
/// earlier build describes bytes this action is replacing. Without this,
/// a stale registration would (a) mask a "tool exited 0 but wrote
/// nothing" failure — [`present_or_virtual`] would keep returning true —
/// and (b) let a later sweep resurrect the PREVIOUS build's bytes under
/// the new action's name.
pub fn unregister(paths: &[PathBuf]) {
    let mut map = lock();
    for path in paths {
        map.remove(path);
    }
}

/// The full record for a virtually-hit output path, if any.
pub fn lookup(path: &Path) -> Option<OutputRecord> {
    lock().get(path).cloned()
}

/// Whether `path` was published as a virtual artifact this process.
/// Registration is not removed on materialization — the record stays
/// valid (it still describes the bytes now on disk).
pub fn is_registered(path: &Path) -> bool {
    lock().contains_key(path)
}

/// Post-execution output check for rules: an output is acceptable when
/// it exists on disk OR its producing action virtual-hit.
pub fn present_or_virtual(path: &Path) -> bool {
    path.is_file() || is_registered(path)
}

// ── instrumentation ─────────────────────────────────────────────────────

static VIRTUAL_HITS: AtomicU64 = AtomicU64::new(0);
static VIRTUAL_OUTPUTS: AtomicU64 = AtomicU64::new(0);
static BYTES_AVOIDED: AtomicU64 = AtomicU64::new(0);
static JIT_FILES: AtomicU64 = AtomicU64::new(0);
static JIT_BYTES: AtomicU64 = AtomicU64::new(0);
static PREBUILD_FILES: AtomicU64 = AtomicU64::new(0);
static PREBUILD_BYTES: AtomicU64 = AtomicU64::new(0);
static TOPLEVEL_FILES: AtomicU64 = AtomicU64::new(0);
static TOPLEVEL_BYTES: AtomicU64 = AtomicU64::new(0);

/// Why a materialization happened, for the counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializePurpose {
    /// A cache-missing action needed this input before executing.
    Jit,
    /// A rule that reads dependency files outside the action manager
    /// (genrule, py rules, ...) got its deps materialized up front.
    PreBuild,
    /// A user-requested target's output, materialized at end of build.
    TopLevel,
}

/// Record a virtual hit that skipped materializing `outputs` outputs
/// totalling `bytes` bytes.
pub fn note_virtual_hit(outputs: u64, bytes: u64) {
    VIRTUAL_HITS.fetch_add(1, Ordering::Relaxed);
    VIRTUAL_OUTPUTS.fetch_add(outputs, Ordering::Relaxed);
    BYTES_AVOIDED.fetch_add(bytes, Ordering::Relaxed);
}

fn materialized() -> &'static Mutex<HashSet<PathBuf>> {
    static MATERIALIZED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    MATERIALIZED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Record one materialized file of `bytes` bytes for `purpose`. `path`
/// is remembered for this build so the freshness ledger can account for
/// a write rush itself made to a path outside the active build slice.
///
/// Byte totals cover regular files; symlink and tree records carry no
/// size, so they count as zero bytes (they are not produced by the
/// virtual-eligible compile/archive/link mnemonics).
pub fn note_materialized(purpose: MaterializePurpose, path: &Path, bytes: u64) {
    let (files, total) = match purpose {
        MaterializePurpose::Jit => (&JIT_FILES, &JIT_BYTES),
        MaterializePurpose::PreBuild => (&PREBUILD_FILES, &PREBUILD_BYTES),
        MaterializePurpose::TopLevel => (&TOPLEVEL_FILES, &TOPLEVEL_BYTES),
    };
    files.fetch_add(1, Ordering::Relaxed);
    total.fetch_add(bytes, Ordering::Relaxed);
    materialized()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(path.to_path_buf());
}

/// Every path this build materialized from the CAS.
///
/// A pruned rebuild plans its ledger refresh / own-write path sets from
/// the ACTIVE target slice, but JIT materialization writes the outputs
/// of INACTIVE upstream targets. Those writes are rush's own, so the
/// caller folds this set into both, otherwise the next no-op reports
/// "path appeared" forever.
pub fn materialized_paths() -> Vec<PathBuf> {
    materialized()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// Zero the per-build counters and materialized-path set. The record
/// REGISTRY deliberately survives: an output that is CAS-backed on disk
/// stays CAS-backed across builds in a resident daemon, and a pruned
/// rebuild never re-runs the producer that would re-register it.
pub fn reset_build_stats() {
    for c in [
        &VIRTUAL_HITS,
        &VIRTUAL_OUTPUTS,
        &BYTES_AVOIDED,
        &JIT_FILES,
        &JIT_BYTES,
        &PREBUILD_FILES,
        &PREBUILD_BYTES,
        &TOPLEVEL_FILES,
        &TOPLEVEL_BYTES,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    materialized()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// One-line end-of-build summary, or `None` when nothing virtual
/// happened (keeps flag-off and virtual-quiet builds silent).
pub fn summary() -> Option<String> {
    let hits = VIRTUAL_HITS.load(Ordering::Relaxed);
    let jit = JIT_FILES.load(Ordering::Relaxed);
    let pre = PREBUILD_FILES.load(Ordering::Relaxed);
    let top = TOPLEVEL_FILES.load(Ordering::Relaxed);
    if hits == 0 && jit == 0 && pre == 0 && top == 0 {
        return None;
    }
    Some(format!(
        "🪶 virtual artifacts: {} hit(s) kept {} output(s) / {:.1} MiB \
         CAS-backed; materialized {} JIT ({:.1} MiB), {} pre-build \
         ({:.1} MiB), {} top-level ({:.1} MiB)",
        hits,
        VIRTUAL_OUTPUTS.load(Ordering::Relaxed),
        mib(BYTES_AVOIDED.load(Ordering::Relaxed)),
        jit,
        mib(JIT_BYTES.load(Ordering::Relaxed)),
        pre,
        mib(PREBUILD_BYTES.load(Ordering::Relaxed)),
        top,
        mib(TOPLEVEL_BYTES.load(Ordering::Relaxed)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_value::{ArtifactFingerprint, ArtifactValueKind};

    fn record(path: &str, size: u64) -> OutputRecord {
        OutputRecord {
            path: PathBuf::from(path),
            kind: ArtifactValueKind::RegularFile,
            artifact_fingerprint: ArtifactFingerprint([7u8; 32]),
            content_digest: None,
            size,
            executable: false,
            symlink_target: None,
            tree: Vec::new(),
        }
    }

    #[test]
    fn register_lookup_roundtrip() {
        let r = record("/vr-test/nonexistent/a.o", 42);
        register(std::slice::from_ref(&r));
        assert!(is_registered(Path::new("/vr-test/nonexistent/a.o")));
        assert_eq!(
            lookup(Path::new("/vr-test/nonexistent/a.o")).unwrap().size,
            42
        );
        assert!(!is_registered(Path::new("/vr-test/nonexistent/b.o")));
        // Absent from disk but registered ⇒ acceptable to rules.
        assert!(present_or_virtual(Path::new("/vr-test/nonexistent/a.o")));
        assert!(!present_or_virtual(Path::new("/vr-test/nonexistent/b.o")));
    }

    #[test]
    fn enablement_requires_flag_and_forbids_scheduler() {
        assert!(compute_enabled(Some("1"), None));
        assert!(compute_enabled(Some("1"), Some("0")));
        assert!(!compute_enabled(Some("1"), Some("1")));
        assert!(!compute_enabled(None, None));
        assert!(!compute_enabled(Some("0"), None));
    }
}
