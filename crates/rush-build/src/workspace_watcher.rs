// Rush Build - Daemon dirty-set filesystem watcher (Phase 2 of the crush-Bazel plan)
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! An inotify-backed dirty-set watcher that lets a healthy warm no-op validate
//! ZERO paths instead of stat-walking all ~20k recorded [`Dependency`] records.
//!
//! ## Why this exists
//!
//! After Phase 1 (single-validate), the daemon's warm no-op is dominated by a
//! single manifest stat-walk: re-reading / re-statting / re-`readdir`-ing every
//! recorded analysis dependency (~20ms, LINEAR in dep count). Bazel's `--watchfs`
//! avoids that scan with a filesystem watcher; this module is rush's equivalent.
//! A healthy watcher that has observed NO change to any path/dir the snapshot's
//! manifest depends on lets execution admission skip the stat-walk entirely.
//!
//! ## Correctness is sacred — the watcher is a PERF HINT, never the source of truth
//!
//! A changed source/output MUST still rebuild. The watcher only ever *shrinks*
//! the validation work, and only when it can PROVE (kernel-ordered, flushed)
//! that nothing relevant moved. On ANY doubt we fall back to the full
//! [`DependencyManifest::validate`] — the exact Phase-1 path. Specifically:
//!
//! * **Real kernel barrier, not an async queue.** We own the inotify fd and
//!   [`drain`](WorkspaceWatcher::drain) it with non-blocking `read` until
//!   `EAGAIN` at the start of every request. The Linux inotify queue is ordered
//!   and a user-space edit that *happened-before* the client's connect is, by
//!   in-order delivery, already enqueued in the kernel when we drain — so reading
//!   to `EAGAIN` is a true flush barrier. We do NOT use `notify`'s background
//!   thread + channel here (its async hand-off has no such barrier — Codex
//!   review 019f02fa).
//! * **Three dirty concepts** (Codex 019f02fa): `dirty_paths` (the exact object
//!   touched), `dirty_entry_dirs` (the parent dir whose child-name SET changed —
//!   covers `glob()` / package `DirListing` deps and negative `FileExistence`
//!   probes), and `dirty_subtrees` (a directory create/delete/move/recreate whose
//!   descendants may have changed OR whose recursive watch coverage raced — the
//!   whole subtree is conservatively dirty).
//! * **Fallback (→ full validate) on:** queue overflow (`IN_Q_OVERFLOW`), a watch
//!   root that became un-watchable, an event we cannot classify, a watch-add
//!   failure while extending coverage to a new subdir, root delete/move, an
//!   unknown watch descriptor, any manifest filesystem dependency whose required
//!   parent/listing directory is not actually watched, or the watcher never having
//!   been armed before the snapshot was analyzed (no continuity — a restarted
//!   daemon). Global health failures flip the health to
//!   [`WatchHealth::Broken`]/[`Overflowed`](WatchHealth::Overflowed) and STAY
//!   there until re-armed; per-manifest coverage gaps make that snapshot fall
//!   back to full validation. Neither case authorizes a blind 0-path no-op.
//! * **Continuity gate.** A snapshot may only use the watcher if it was analyzed
//!   AFTER the watch was armed (`armed_at`). A snapshot from before the arm (or
//!   from a previous daemon) has no watcher continuity and must full-validate.
//! * **Concurrent-modification guard.** The caller captures
//!   [`epoch`](WorkspaceWatcher::epoch) before AND after the no-op decision; if it
//!   moved, an event landed during the decision and the caller re-drains / falls
//!   back rather than trusting the stale "clean" verdict.
//!
//! ## `--watchfs={auto,off,require}`
//!
//! * `auto` (default): use the watcher on a known-good local FS, else scan.
//! * `off`: never use the watcher — always the Phase-1 stat-walk (today's behavior).
//! * `require`: error if the watcher is unavailable/unhealthy (CI / debugging).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use rush_incremental::Dependency;

/// The `--watchfs` mode, parsed from the command's argv/env.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchFsMode {
    /// Use the watcher on a known-good local FS, else scan. The default.
    Auto,
    /// Always scan (the Phase-1 single manifest_validate). Today's behavior.
    Off,
    /// Error if the watcher is unavailable/unhealthy.
    Require,
}

impl WatchFsMode {
    /// Parse `--watchfs={auto,off,require}` (or `RUSH_WATCHFS=...`) from a
    /// command's argv tokens + an env lookup. argv form wins over env; the
    /// `--watchfs=VALUE` and `--watchfs VALUE` shapes are both accepted.
    /// Unknown / absent → `Auto`.
    pub fn parse(argv: &[bytes::Bytes], env: impl Fn(&[u8]) -> Option<Vec<u8>>) -> Self {
        // argv scan (space form and = form).
        let mut i = 0usize;
        while i < argv.len() {
            let tok = String::from_utf8_lossy(&argv[i]);
            if let Some(rest) = tok.strip_prefix("--watchfs") {
                let val = if let Some(eq) = rest.strip_prefix('=') {
                    Some(eq.to_string())
                } else if rest.is_empty() {
                    // space form: next token is the value
                    argv.get(i + 1).map(|b| String::from_utf8_lossy(b).into_owned())
                } else {
                    None
                };
                if let Some(v) = val {
                    return Self::from_str_value(&v);
                }
            }
            i += 1;
        }
        if let Some(v) = env(b"RUSH_WATCHFS") {
            return Self::from_str_value(&String::from_utf8_lossy(&v));
        }
        WatchFsMode::Auto
    }

    fn from_str_value(v: &str) -> Self {
        match v.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" | "no" => WatchFsMode::Off,
            "require" | "required" => WatchFsMode::Require,
            _ => WatchFsMode::Auto, // "auto", "on", "true", unknown
        }
    }
}

/// Health of the watcher. Only [`Healthy`](WatchHealth::Healthy) authorizes a
/// 0-path no-op; everything else forces a full manifest validate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchHealth {
    /// Armed, all roots watched, no overflow/error seen. The fast path is live.
    Healthy,
    /// The kernel inotify queue overflowed (`IN_Q_OVERFLOW`): events were
    /// dropped, so the dirty set is INCOMPLETE. Must full-validate until re-armed.
    Overflowed,
    /// An inotify error / un-watchable root / unclassifiable event / watch-add
    /// failure. Must full-validate until re-armed.
    Broken,
    /// The watcher was never armed (default state), or the platform/FS is not
    /// supported. Must full-validate.
    Unsupported,
}

/// Result of trying to admit a manifest through the watcher without a
/// filesystem stat-walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestWatchAdmission {
    /// No manifest dependency intersects the watcher dirty sets.
    Clean,
    /// Every dirty manifest dependency is an exact file path covered by a
    /// caller-supplied freshness proof, such as the action ledger.
    CoveredDirty(Vec<PathBuf>),
}

/// Precomputed manifest facts used to admit watcher-dirty snapshots without
/// scanning every dependency on the warm path.
#[derive(Debug, Clone, Default)]
pub struct ManifestWatchIndex {
    exact_file_paths: HashSet<PathBuf>,
    symlink_paths: HashSet<PathBuf>,
    dir_listing_paths: HashSet<PathBuf>,
    /// Every directory that must have an active inotify watch for this manifest:
    /// parents of file/symlink deps and the directory itself for `DirListing`.
    /// Besides dirty-entry intersection this is the persisted proof obligation
    /// for rejecting zero-stat admission on skipped/out-of-root dependencies.
    entry_sensitive_dirs: HashSet<PathBuf>,
    has_process_probe: bool,
    has_linux_proc_filesystems: bool,
    has_linux_proc_vm_overcommit_memory: bool,
    has_linux_cgroup_v2_cpu_max: bool,
}

impl ManifestWatchIndex {
    /// Build the watcher admission index for an immutable manifest.
    pub fn from_manifest(manifest: &rush_incremental::DependencyManifest) -> Self {
        let mut index = Self::default();
        for dep in manifest.deps() {
            match dep {
                Dependency::FileContent { path, .. }
                | Dependency::FileExistence { path, .. }
                | Dependency::FileMetadata { path, .. } => {
                    index.exact_file_paths.insert(path.clone());
                    if let Some(parent) = path.parent() {
                        index.entry_sensitive_dirs.insert(parent.to_path_buf());
                    }
                }
                Dependency::Symlink { path, .. } => {
                    index.symlink_paths.insert(path.clone());
                    if let Some(parent) = path.parent() {
                        index.entry_sensitive_dirs.insert(parent.to_path_buf());
                    }
                }
                Dependency::DirListing { path, .. } => {
                    index.dir_listing_paths.insert(path.clone());
                    index.entry_sensitive_dirs.insert(path.clone());
                }
                Dependency::Env { .. }
                | Dependency::Opt { .. }
                | Dependency::ConfigFingerprint { .. } => {}
                Dependency::ProcessProbe { .. } => {
                    index.has_process_probe = true;
                }
                Dependency::LinuxProcFilesystems { .. } => {
                    index.has_linux_proc_filesystems = true;
                }
                Dependency::LinuxProcVmOvercommitMemory { .. } => {
                    index.has_linux_proc_vm_overcommit_memory = true;
                }
                Dependency::LinuxCgroupV2CpuMax { .. } => {
                    index.has_linux_cgroup_v2_cpu_max = true;
                }
            }
        }
        index
    }
}

/// The dirty-set state, bumped each [`drain`](WorkspaceWatcher::drain).
#[derive(Debug, Clone)]
pub struct WatchState {
    /// Monotonic generation; bumped on every drain that observed >=1 event.
    pub epoch: u64,
    /// Current health.
    pub health: WatchHealth,
    /// When the watch was armed (continuity gate). `None` until armed.
    pub armed_at: Option<Instant>,
    /// Exact object paths an event touched (modify/attrib/delete-self/move-self,
    /// plus the full child path on create/delete/moved within a watched dir).
    pub dirty_paths: HashSet<PathBuf>,
    /// Parent directories whose immediate child-name SET changed (create / delete
    /// / moved_from / moved_to of a child). A `DirListing` dep on such a dir, or a
    /// negative `FileExistence` for a path whose parent is here, is dirty.
    pub dirty_entry_dirs: HashSet<PathBuf>,
    /// Directories whose entire SUBTREE is conservatively dirty: a dir was
    /// created/deleted/moved (descendants and/or recursive-watch coverage may
    /// have raced). Any dep at/under such a dir is dirty.
    pub dirty_subtrees: HashSet<PathBuf>,
    /// ── Phase 4: ledger (OUTPUT-universe) watch state ──────────────────────
    ///
    /// The freshness ledger re-stats the action graph's ~21k input+output paths
    /// each no-op to detect a deleted/modified output. Phase 4 lets a healthy
    /// watcher skip THAT stat-walk too. These fields track the OUTPUT universe
    /// (which the analysis-root watch deliberately skips — `rush-out`). They are
    /// SEPARATE from the analysis dirty sets above so a build's own output churn
    /// never poisons the manifest fast path and vice-versa.
    ///
    /// Health of the ledger watch: `Healthy` only when every tracked ledger
    /// parent dir is watched and no overflow/error/lost-coverage was seen.
    pub ledger_health: WatchHealth,
    /// The exact ledger paths we are tracking (the union of action inputs +
    /// outputs the ledger recorded). An event whose full child path is in this
    /// set marks [`ledger_dirty`](WatchState::ledger_dirty).
    pub ledger_paths: HashSet<PathBuf>,
    /// True once an EXTERNAL change to a tracked ledger path was observed since
    /// the baseline was installed (a `rm`/`touch`/rewrite of an output/input by
    /// something other than the daemon's own in-build writes). A dirty ledger
    /// watch refuses the 0-output-path no-op → the caller runs the full
    /// `ledger.verify()` (today's behavior). Reset to `false` only when a fresh
    /// baseline is installed after a fully-successful build.
    pub ledger_dirty: bool,
    /// When the ledger-watch baseline was installed (continuity gate for the
    /// ledger skip, mirroring `armed_at` for the manifest skip). `None` until a
    /// build installs it.
    pub ledger_baseline_at: Option<Instant>,
    /// The monotonic id of the CURRENT ledger baseline. The ledger watch is
    /// GLOBAL (one per watcher) but each snapshot has its OWN freshness ledger
    /// over its OWN output universe; `install_ledger_watch` OVERWRITES the global
    /// watch with the just-built snapshot's paths and mints a fresh id. A no-op
    /// may skip `ledger.verify()` ONLY if its snapshot holds THIS exact id —
    /// otherwise the global watch is covering a DIFFERENT snapshot's outputs and
    /// authorizing a skip would miss an external change to this snapshot's
    /// outputs (Codex 019f0368: pin to a ledger-baseline token, single fast
    /// snapshot; all others fall back to the full verify). `0` = no baseline.
    pub ledger_baseline_id: u64,
    /// True while a wavefront is executing: the daemon's OWN output writes fire
    /// events on the ledger watches and must NOT mark `ledger_dirty`. Set by
    /// [`begin_build`](WorkspaceWatcher::begin_build), cleared by
    /// [`end_build`](WorkspaceWatcher::end_build). The no-op fast path never runs
    /// a wavefront, so an output event observed while this is `false` is
    /// necessarily EXTERNAL (Codex 019f0368).
    pub build_active: bool,
    /// During a full wavefront (`None`), any tracked ledger-path event can be
    /// the daemon's own output write and is suppressed while `build_active`.
    /// During a pruned wavefront (`Some(paths)`), only these declared output
    /// paths are own writes; events for other tracked ledger paths remain
    /// external and keep `ledger_dirty` true.
    pub own_write_paths: Option<HashSet<PathBuf>>,
}

impl WatchState {
    fn unsupported() -> Self {
        WatchState {
            epoch: 0,
            health: WatchHealth::Unsupported,
            armed_at: None,
            dirty_paths: HashSet::new(),
            dirty_entry_dirs: HashSet::new(),
            dirty_subtrees: HashSet::new(),
            ledger_health: WatchHealth::Unsupported,
            ledger_paths: HashSet::new(),
            ledger_dirty: false,
            ledger_baseline_at: None,
            ledger_baseline_id: 0,
            build_active: false,
            own_write_paths: None,
        }
    }
}

/// Directory names never worth watching (build outputs / VCS / scratch / the
/// daemon's own cache). Package discovery and globs exclude these paths, and
/// recursing into a multi-GB `rush-out` would blow the inotify watch-count
/// limit. Output freshness is covered separately by the freshness ledger's own
/// stat of the recorded output paths. If an explicit tracked dependency (for
/// example a Bzlmod repository under `.cache`) does enter the analysis manifest,
/// the zero-stat admission path detects the missing analysis watch and falls
/// back to full manifest validation.
const SKIP_DIR_NAMES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "rush-out",
    "bazel-out",
    ".cache",
];

/// Output / scratch FILE base-names that rush (or Bazel) writes into the
/// workspace and that are NEVER an analysis source. An inotify event whose child
/// name is one of these must NOT dirty the parent's entry-set (it is not part of
/// the package-discovery / glob universe — `.bazelignore` lists exactly these),
/// matching how the recorded `DirListing` digest's relevance is scoped. This is
/// the shared ignore predicate Codex (019f02fa) prescribed over an ad-hoc
/// watcher blocklist: the SAME exclusion the package scanner applies.
const IGNORED_ENTRY_FILE_NAMES: &[&str] = &[
    "compile_commands.json",
    "bazel-bin",
    "bazel-genfiles",
    "bazel-testlogs",
];

/// True iff a directory ENTRY named `name` is a rush/Bazel output or VCS entry
/// that is never an analysis input. Used both to skip recursing a watch into it
/// AND (Codex 019f02fa) to ignore CREATE/DELETE/MOVED events about it so the
/// daemon's own output churn never spuriously dirties a `DirListing` dep. A
/// `bazel-<workspace>` convenience symlink also matches the `bazel-` prefix.
fn is_ignored_entry_name(name: &str) -> bool {
    SKIP_DIR_NAMES.contains(&name)
        || IGNORED_ENTRY_FILE_NAMES.contains(&name)
        || name.starts_with("bazel-")
}

/// The internal mutable watcher: the owned inotify fd + the watch bookkeeping +
/// the dirty state. Behind a `Mutex` on [`WorkspaceWatcher`].
struct WatcherInner {
    /// The owned inotify instance (non-blocking fd; `init()` sets `IN_NONBLOCK`).
    /// `None` when unarmed/unsupported.
    inotify: Option<Inotify>,
    /// Watch descriptor -> the absolute directory it watches. Lets an event's
    /// `wd` resolve to a path so we can compose full child paths.
    wd_to_dir: HashMap<WatchDescriptor, PathBuf>,
    /// Absolute dir -> its watch descriptor (so we don't double-add / can extend).
    dir_to_wd: HashMap<PathBuf, WatchDescriptor>,
    /// Directories covered continuously by the recursive analysis watcher.
    /// `dir_to_wd` also contains late, non-recursive ledger watches, which do
    /// not prove that an analysis dependency was watched when its snapshot was
    /// produced and therefore cannot authorize zero-stat manifest admission.
    analysis_dirs: HashSet<PathBuf>,
    /// The roots we recursively watch (workspace + external + others).
    roots: Vec<PathBuf>,
    /// Phase 4: the set of PARENT directories we watch on behalf of the ledger
    /// (deduped `parent(ledger_path)`). These live under `rush-out` (which the
    /// analysis roots skip) and are watched non-recursively — one watch per
    /// unique parent, bounded well below the ~21k path count. An event whose
    /// `wd` resolves to one of these is a ledger-universe event. A
    /// DELETE_SELF/MOVE_SELF/IGNORED on one of these → the tracked children lost
    /// coverage → the ledger watch is poisoned (Codex 019f0368).
    ledger_dirs: HashSet<PathBuf>,
    /// Monotonic source for [`WatchState::ledger_baseline_id`]; bumped once per
    /// successful `install_ledger_watch`. Never reused (so a stale snapshot's
    /// old id can never accidentally match a future baseline).
    next_ledger_baseline_id: u64,
    /// The dirty-set + health state.
    state: WatchState,
}

/// The daemon's filesystem watcher. Shared via `Arc` on the orchestrator; armed
/// once by the daemon and drained at the start of every request.
pub struct WorkspaceWatcher {
    inner: Mutex<WatcherInner>,
}

impl Default for WorkspaceWatcher {
    fn default() -> Self {
        Self::unarmed()
    }
}

impl WorkspaceWatcher {
    /// An unarmed watcher (the default for single-shot / in-process builds, which
    /// behave exactly as before: always full-validate). The daemon arms it.
    pub fn unarmed() -> Self {
        WorkspaceWatcher {
            inner: Mutex::new(WatcherInner {
                inotify: None,
                wd_to_dir: HashMap::new(),
                dir_to_wd: HashMap::new(),
                analysis_dirs: HashSet::new(),
                roots: Vec::new(),
                ledger_dirs: HashSet::new(),
                next_ledger_baseline_id: 1,
                state: WatchState::unsupported(),
            }),
        }
    }

    /// The mask we watch on every directory. We deliberately watch
    /// content/metadata/name-set/structure events and NOT noise like `ACCESS`/
    /// `OPEN`/`CLOSE_NOWRITE`.
    ///
    /// * `MODIFY` / `CLOSE_WRITE` — content of a watched file's child changed.
    /// * `ATTRIB` — chmod/owner/mtime (a `chmod +x` on a tool a content digest
    ///   would miss; the manifest records `FileMetadata` for exactly this).
    /// * `CREATE` / `DELETE` / `MOVED_FROM` / `MOVED_TO` — child-name set changed
    ///   (glob / package `DirListing`, negative existence).
    /// * `DELETE_SELF` / `MOVE_SELF` — the watched dir itself vanished/moved (root
    ///   move / dir delete → subtree dirty + possible re-arm).
    fn watch_mask() -> WatchMask {
        WatchMask::MODIFY
            | WatchMask::CLOSE_WRITE
            | WatchMask::ATTRIB
            | WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO
            | WatchMask::DELETE_SELF
            | WatchMask::MOVE_SELF
    }

    /// Arm the watcher over `roots` (each recursively watched). Replaces any
    /// previous state. On success the health is [`Healthy`](WatchHealth::Healthy)
    /// and `armed_at` is set; on ANY failure to establish coverage of a root the
    /// health is [`Broken`](WatchHealth::Broken) (so the caller always
    /// full-validates — never silently miss).
    ///
    /// Roots that do not exist yet are simply skipped (a not-yet-materialized
    /// external cache is not an error); a root that exists but cannot be watched
    /// IS an error (Broken). Establishing the recursive watch FIRST, then setting
    /// `armed_at`, is the continuity premise: any snapshot analyzed after this
    /// instant is covered.
    pub fn arm(&self, roots: &[PathBuf]) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        // Reset bookkeeping.
        inner.wd_to_dir.clear();
        inner.dir_to_wd.clear();
        inner.analysis_dirs.clear();
        inner.ledger_dirs.clear();
        inner.roots = roots.to_vec();

        let inotify = match Inotify::init() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("👁️  watcher: inotify_init failed ({e}); falling back to scan");
                inner.inotify = None;
                inner.state = WatchState::unsupported();
                return;
            }
        };
        inner.inotify = Some(inotify);

        let mut broken = false;
        // Canonicalize-free: we watch the roots as given (the caller passes
        // already-resolved absolute paths). Recursively add a watch on every dir.
        let roots_owned = inner.roots.clone();
        for root in &roots_owned {
            if !root.exists() {
                // Not-yet-materialized root (e.g. empty external cache): fine.
                continue;
            }
            if let Err(e) = Self::add_watch_recursive(&mut inner, root) {
                eprintln!(
                    "👁️  watcher: failed to watch root {} ({e}); falling back to scan",
                    root.display()
                );
                broken = true;
                break;
            }
        }

        inner.state = WatchState {
            epoch: 1,
            health: if broken {
                WatchHealth::Broken
            } else {
                WatchHealth::Healthy
            },
            armed_at: Some(Instant::now()),
            dirty_paths: HashSet::new(),
            dirty_entry_dirs: HashSet::new(),
            dirty_subtrees: HashSet::new(),
            // The ledger watch starts UNSUPPORTED on a fresh arm: no build has
            // installed a baseline yet, so a no-op must full-`ledger.verify()`
            // until the first successful build calls `install_ledger_watch`.
            ledger_health: WatchHealth::Unsupported,
            ledger_paths: HashSet::new(),
            ledger_dirty: false,
            ledger_baseline_at: None,
            ledger_baseline_id: 0,
            build_active: false,
            own_write_paths: None,
        };
        if !broken {
            eprintln!(
                "👁️  watcher armed: {} dir watch(es) over {} root(s) — warm no-ops will validate 0 paths when clean",
                inner.wd_to_dir.len(),
                roots_owned.len(),
            );
        }
    }

    /// Recursively add a watch on `dir` and every sub-directory (skipping
    /// [`SKIP_DIR_NAMES`]). Symlinked directories are NOT followed (we watch the
    /// link node's parent for the symlink dep; following could escape the roots
    /// or loop). Returns the first hard error.
    fn add_watch_recursive(inner: &mut WatcherInner, dir: &Path) -> std::io::Result<()> {
        // Iterative DFS to avoid deep recursion on large trees.
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            Self::add_one_watch(inner, &d)?;
            inner.analysis_dirs.insert(d.clone());
            let rd = match std::fs::read_dir(&d) {
                Ok(rd) => rd,
                // A dir that races away mid-walk: skip it (it'll surface as a
                // dirty subtree if it mattered). Not a hard error.
                Err(_) => continue,
            };
            for entry in rd.flatten() {
                // Use symlink_metadata so we do NOT descend through symlinks.
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if ft.is_dir() {
                    let name = entry.file_name();
                    if let Some(n) = name.to_str() {
                        if is_ignored_entry_name(n) {
                            continue;
                        }
                    }
                    stack.push(entry.path());
                }
            }
        }
        Ok(())
    }

    /// Add a single directory watch and record the bookkeeping (idempotent: an
    /// existing dir updates its mask, returning the same wd).
    fn add_one_watch(inner: &mut WatcherInner, dir: &Path) -> std::io::Result<()> {
        let Some(inotify) = inner.inotify.as_mut() else {
            return Ok(());
        };
        let wd = inotify.add_watch(dir, Self::watch_mask())?;
        inner.wd_to_dir.insert(wd.clone(), dir.to_path_buf());
        inner.dir_to_wd.insert(dir.to_path_buf(), wd);
        Ok(())
    }

    /// Drain all queued inotify events (non-blocking `read` until `EAGAIN` — the
    /// real kernel flush barrier), folding each into the dirty sets and bumping
    /// the epoch if anything was observed. Cheap (a single `read` returning
    /// `EAGAIN`) when nothing changed. A no-op when unarmed.
    ///
    /// Returns the post-drain epoch.
    pub fn drain(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.inotify.is_none() {
            return inner.state.epoch;
        }
        // Keep draining as long as EITHER the analysis watch OR the ledger watch
        // is still trustworthy (Healthy). The two share one inotify fd; if we
        // stopped draining because the ANALYSIS watch went Broken, a still-Healthy
        // LEDGER watch would silently miss external output events (it would report
        // clean while events sit undrained) → a stale build. Only short-circuit
        // when NEITHER watch can be authorized (both non-Healthy) — then the dirty
        // set is already untrustworthy on both sides until re-arm/re-baseline.
        let analysis_live = inner.state.health == WatchHealth::Healthy;
        let ledger_live = inner.state.ledger_health == WatchHealth::Healthy;
        if !analysis_live && !ledger_live {
            return inner.state.epoch;
        }

        let mut buf = [0u8; 16 * 1024];
        let mut observed = 0u64;
        loop {
            // SAFETY of the borrow: we hold `inner` exclusively. Read into a
            // local buffer; collect the events we need into owned data BEFORE
            // touching `inner` mutably again (the Events borrow `buf`).
            let collected: Vec<(Option<PathBuf>, EventMask, Option<String>, WatchDescriptor)> = {
                let inotify = inner.inotify.as_mut().unwrap();
                match inotify.read_events(&mut buf) {
                    Ok(events) => events
                        .map(|ev| {
                            (
                                None::<PathBuf>, // dir filled below via wd map
                                ev.mask,
                                ev.name.and_then(|n| n.to_str().map(|s| s.to_string())),
                                ev.wd.clone(),
                            )
                        })
                        .collect(),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // Drained: this is the barrier.
                        break;
                    }
                    Err(e) => {
                        eprintln!("👁️  watcher: read_events error ({e}); marking Broken");
                        inner.state.health = WatchHealth::Broken;
                        inner.state.ledger_health = WatchHealth::Broken;
                        inner.state.epoch += 1;
                        return inner.state.epoch;
                    }
                }
            };
            if collected.is_empty() {
                // read returned 0 events but not WouldBlock — treat as drained.
                break;
            }
            for (_, mask, name, wd) in collected {
                observed += 1;
                if !self_fold_event(&mut inner, mask, name, wd) {
                    // Unclassifiable / overflow / lost coverage → broken/overflow
                    // already set inside fold; stop folding (dirty set tainted).
                    inner.state.epoch += 1;
                    return inner.state.epoch;
                }
            }
        }
        if observed > 0 {
            inner.state.epoch += 1;
        }
        inner.state.epoch
    }

    /// The current epoch (without draining). Used by the concurrent-modification
    /// guard (capture before+after the decision).
    pub fn epoch(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state
            .epoch
    }

    /// The current health.
    pub fn health(&self) -> WatchHealth {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state
            .health
    }

    /// The instant the watch was armed, if any (continuity gate).
    pub fn armed_at(&self) -> Option<Instant> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state
            .armed_at
    }

    /// Decide whether the watcher can authorize a 0-path no-op for a manifest
    /// whose analysis completed at `analyzed_at`, against the dirty sets.
    ///
    /// Returns `Ok(())` if the watcher is HEALTHY, has continuity for this
    /// snapshot (`armed_at <= analyzed_at`), and NONE of the manifest's
    /// filesystem dependencies intersect the dirty sets — meaning the stat-walk
    /// can be skipped. Returns `Err(reason)` otherwise (the caller falls back to
    /// the full manifest validate). NEVER returns `Ok` on any doubt.
    pub fn clean_for(
        &self,
        manifest: &rush_incremental::DependencyManifest,
        analyzed_at: Instant,
    ) -> Result<(), String> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match inner.state.health {
            WatchHealth::Healthy => {}
            WatchHealth::Overflowed => return Err("watcher overflowed".into()),
            WatchHealth::Broken => return Err("watcher broken".into()),
            WatchHealth::Unsupported => return Err("watcher unsupported/unarmed".into()),
        }
        // Continuity: the snapshot must have been analyzed AFTER the watch armed,
        // or there is a coverage gap (events before arming were never seen).
        match inner.state.armed_at {
            Some(armed) if armed <= analyzed_at => {}
            _ => return Err("snapshot predates watch arm (no continuity)".into()),
        }
        // Intersect the manifest's filesystem deps with the dirty sets.
        for dep in manifest.deps() {
            ensure_dependency_watch_coverage(&inner, dep)?;
            if let Some(reason) = dep_dirty(&inner.state, dep) {
                return Err(reason);
            }
        }
        Ok(())
    }

    /// Admit a manifest without a filesystem stat-walk when the watcher can
    /// prove it is either clean or dirty only on exact file paths covered by
    /// another freshness proof.
    pub fn admit_manifest_with_covered_dirty_paths<F>(
        &self,
        manifest: &rush_incremental::DependencyManifest,
        analyzed_at: Instant,
        covers_path: F,
    ) -> Result<ManifestWatchAdmission, String>
    where
        F: FnMut(&Path) -> bool,
    {
        self.manifest_dirty_paths_covered_by_inner(manifest, analyzed_at, covers_path)
            .map(|paths| {
                if paths.is_empty() {
                    ManifestWatchAdmission::Clean
                } else {
                    ManifestWatchAdmission::CoveredDirty(paths)
                }
            })
    }

    /// Indexed variant of [`Self::admit_manifest_with_covered_dirty_paths`].
    /// The verdict is the same, but the common real-edit case intersects the
    /// watcher's small dirty sets with a snapshot-local index instead of scanning
    /// the entire dependency manifest.
    pub fn admit_manifest_indexed_with_covered_dirty_paths<F>(
        &self,
        index: &ManifestWatchIndex,
        analyzed_at: Instant,
        mut covers_path: F,
    ) -> Result<ManifestWatchAdmission, String>
    where
        F: FnMut(&Path) -> bool,
    {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match inner.state.health {
            WatchHealth::Healthy => {}
            WatchHealth::Overflowed => return Err("watcher overflowed".into()),
            WatchHealth::Broken => return Err("watcher broken".into()),
            WatchHealth::Unsupported => return Err("watcher unsupported/unarmed".into()),
        }
        match inner.state.armed_at {
            Some(armed) if armed <= analyzed_at => {}
            _ => return Err("snapshot predates watch arm (no continuity)".into()),
        }
        if index.has_process_probe {
            return Err("process-probe dep requires re-validation".into());
        }
        if index.has_linux_proc_filesystems {
            return Err("Linux filesystem registry requires re-validation".into());
        }
        if index.has_linux_proc_vm_overcommit_memory {
            return Err("Linux overcommit policy requires re-validation".into());
        }
        if index.has_linux_cgroup_v2_cpu_max {
            return Err("Linux cgroup-v2 CPU quota requires re-validation".into());
        }
        ensure_index_watch_coverage(&inner, index)?;
        if !inner.state.dirty_subtrees.is_empty() {
            return Err("dirty subtree requires full manifest validation".into());
        }

        for dir in &inner.state.dirty_entry_dirs {
            if index.entry_sensitive_dirs.contains(dir) {
                return Err(format!("dir entry-set changed: {}", dir.display()));
            }
        }

        let mut dirty_paths: Vec<PathBuf> = Vec::new();
        // RUSH_WATCH_DEBUG=1: report what the watcher actually observed, so a
        // wrong "clean" verdict can be attributed to the right stage instead
        // of guessed at.
        let watch_debug = std::env::var("RUSH_WATCH_DEBUG").as_deref() == Ok("1");
        if watch_debug {
            eprintln!(
                "[watch] dirty_paths={} dirty_entry_dirs={} dirty_subtrees={} \
                 manifest_exact={} ledger_dirty={}",
                inner.state.dirty_paths.len(),
                inner.state.dirty_entry_dirs.len(),
                inner.state.dirty_subtrees.len(),
                index.exact_file_paths.len(),
                inner.state.ledger_dirty,
            );
            for p in inner.state.dirty_paths.iter().take(8) {
                eprintln!(
                    "[watch]   dirty {} manifest_dep={} ledger_tracked={}",
                    p.display(),
                    index.exact_file_paths.contains(p),
                    covers_path(p),
                );
            }
        }
        for path in &inner.state.dirty_paths {
            if index.symlink_paths.contains(path) {
                return Err(format!("symlink dep changed: {}", path.display()));
            }
            if index.dir_listing_paths.contains(path) {
                return Err(format!("watched dir path dirty: {}", path.display()));
            }
            if index.exact_file_paths.contains(path) {
                if covers_path(path) {
                    dirty_paths.push(path.clone());
                } else {
                    return Err(format!(
                        "dirty manifest path is not ledger-tracked: {}",
                        path.display()
                    ));
                }
            }
        }

        dirty_paths.sort();
        dirty_paths.dedup();
        if dirty_paths.is_empty() {
            Ok(ManifestWatchAdmission::Clean)
        } else {
            Ok(ManifestWatchAdmission::CoveredDirty(dirty_paths))
        }
    }

    /// Decide whether a dirty watcher can still admit snapshot reuse because
    /// every dirty manifest dependency is an exact file path already covered by
    /// another freshness proof, such as the action ledger.
    ///
    /// This does NOT authorize directory-entry changes, dirty subtrees,
    /// symlinks, process probes, or untracked paths. Those all fall back to the
    /// full manifest validate, because they can change the analyzed graph shape.
    pub fn manifest_dirty_paths_covered_by<F>(
        &self,
        manifest: &rush_incremental::DependencyManifest,
        analyzed_at: Instant,
        covers_path: F,
    ) -> Result<Vec<PathBuf>, String>
    where
        F: FnMut(&Path) -> bool,
    {
        self.manifest_dirty_paths_covered_by_inner(manifest, analyzed_at, covers_path)
    }

    fn manifest_dirty_paths_covered_by_inner<F>(
        &self,
        manifest: &rush_incremental::DependencyManifest,
        analyzed_at: Instant,
        mut covers_path: F,
    ) -> Result<Vec<PathBuf>, String>
    where
        F: FnMut(&Path) -> bool,
    {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match inner.state.health {
            WatchHealth::Healthy => {}
            WatchHealth::Overflowed => return Err("watcher overflowed".into()),
            WatchHealth::Broken => return Err("watcher broken".into()),
            WatchHealth::Unsupported => return Err("watcher unsupported/unarmed".into()),
        }
        match inner.state.armed_at {
            Some(armed) if armed <= analyzed_at => {}
            _ => return Err("snapshot predates watch arm (no continuity)".into()),
        }

        let mut dirty_paths: HashSet<PathBuf> = HashSet::new();
        for dep in manifest.deps() {
            ensure_dependency_watch_coverage(&inner, dep)?;
            if let Some(path) = dep_exact_dirty_path(&inner.state, dep)? {
                if covers_path(path) {
                    dirty_paths.insert(path.to_path_buf());
                } else {
                    return Err(format!(
                        "dirty manifest path is not ledger-tracked: {}",
                        path.display()
                    ));
                }
            }
        }

        let mut out: Vec<PathBuf> = dirty_paths.into_iter().collect();
        out.sort();
        Ok(out)
    }

    // ── Phase 4: ledger (OUTPUT-universe) watch ─────────────────────────────

    /// Mark the start of a wavefront: the daemon is about to write its OWN
    /// outputs, and those writes fire inotify events on the ledger watches. While
    /// `build_active`, a ledger-path event is the daemon's own write and must NOT
    /// mark `ledger_dirty`. The no-op fast path never runs a wavefront, so an
    /// output event observed while NOT `build_active` is necessarily an EXTERNAL
    /// change (Codex 019f0368). Idempotent; safe to nest is NOT supported (one
    /// wavefront at a time on the single-threaded reuse path).
    pub fn begin_build(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.state.build_active = true;
        inner.state.own_write_paths = None;
    }

    /// Mark the start of a pruned wavefront. The caller has just verified the
    /// complete dirty path set and planned the affected slice, so clear the old
    /// dirty bit and start detecting NEW external movement. Only events for the
    /// affected slice's declared outputs are suppressed as daemon-owned writes;
    /// inactive ledger-path events stay dirty.
    pub fn begin_pruned_build(&self, own_write_paths: &[PathBuf]) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.state.build_active = true;
        inner.state.ledger_dirty = false;
        inner.state.own_write_paths = Some(own_write_paths.iter().cloned().collect());
    }

    /// Add paths to the current build's own-write set.
    ///
    /// A pruned build declares its own-write set up front from the active
    /// target slice, but rush can legitimately write OUTSIDE that slice
    /// during the build — Phase-4 materialization restores an inactive
    /// target's CAS-backed output when an active consumer needs it. Such a
    /// write is rush's, not an external change, so the caller registers it
    /// here. Events are classified when [`Self::drain`] runs, so a
    /// registration made before the next drain covers writes already
    /// queued. No-op outside a pruned build (a full build suppresses every
    /// tracked path already).
    pub fn note_own_write_paths(&self, paths: &[PathBuf]) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(own) = inner.state.own_write_paths.as_mut() {
            own.extend(paths.iter().cloned());
        }
    }

    /// Mark the end of a wavefront's own-write window. After this, any ledger
    /// event is treated as EXTERNAL again. The caller MUST have already drained
    /// the own-write backlog (a `drain()` WHILE `build_active`) BEFORE calling
    /// this, and then do one final `drain()` AFTER this so a change that lands
    /// post-window is caught (Codex 019f0368: the suppression window must close
    /// before the final capture-confirming drain).
    pub fn end_build(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.state.build_active = false;
        inner.state.own_write_paths = None;
    }

    /// Install (or refresh) the ledger-watch baseline over `ledger_paths` after a
    /// fully-successful build. Adds a non-recursive inotify watch on each unique
    /// PARENT directory of a tracked path (deduped — bounded well below the path
    /// count), records the tracked path set, resets `ledger_dirty`, and stamps
    /// `ledger_baseline_at` for the continuity gate. On ANY failure to establish
    /// coverage (add-watch error, no inotify fd, ENOSPC) the ledger watch is set
    /// [`Unsupported`](WatchHealth::Unsupported) so every no-op falls back to the
    /// full `ledger.verify()` — correct, just not flat.
    ///
    /// MUST be called only with the suppression window OPEN (`build_active`):
    /// adding a watch can immediately deliver events for files already present,
    /// and any own-write backlog must still be suppressed. The caller closes the
    /// window (`end_build`) and does the authoritative final `drain()` afterward.
    ///
    /// Returns the minted [`ledger_baseline_id`](WatchState::ledger_baseline_id)
    /// on success, which the caller stores beside the snapshot's freshness ledger
    /// and later passes to [`ledger_clean`](WorkspaceWatcher::ledger_clean) so the
    /// (global) ledger watch only authorizes a skip for the EXACT snapshot it is
    /// currently baselined over. Returns `0` on any failure to install (→ no
    /// snapshot can claim a fast ledger skip; all full-`verify()`).
    pub fn install_ledger_watch(&self, ledger_paths: &[PathBuf]) -> u64 {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());

        // If the base watcher has no inotify fd (unarmed/unsupported), the ledger
        // watch cannot exist either.
        if inner.inotify.is_none() {
            inner.state.ledger_health = WatchHealth::Unsupported;
            inner.state.ledger_baseline_id = 0;
            return 0;
        }

        // Compute the deduped parent-dir set of all tracked ledger paths.
        let mut parents: HashSet<PathBuf> = HashSet::new();
        for p in ledger_paths {
            if let Some(parent) = p.parent() {
                parents.insert(parent.to_path_buf());
            }
        }

        // Add a (non-recursive) watch on every parent dir not already covered.
        // Parents under the analysis roots may already be watched recursively
        // (e.g. a source file's dir) — `add_one_watch` is idempotent (updates the
        // mask, same wd), so we never double-count. We track the ledger parents
        // separately so a DELETE_SELF on one poisons the LEDGER watch (not the
        // manifest one).
        let mut broken = false;
        let parents_owned: Vec<PathBuf> = parents.iter().cloned().collect();
        for dir in &parents_owned {
            if !dir.is_dir() {
                // The parent dir of a recorded output does not exist: the output
                // itself cannot be present, so this baseline is not trustworthy.
                // (build_complete + the required-present assertion in the caller
                // already guard this; be conservative here regardless.)
                broken = true;
                break;
            }
            if let Err(e) = Self::add_one_watch(&mut inner, dir) {
                eprintln!(
                    "👁️  ledger-watch: failed to watch output dir {} ({e}); falling back to full ledger.verify()",
                    dir.display()
                );
                broken = true;
                break;
            }
        }

        if broken {
            inner.ledger_dirs.clear();
            inner.state.ledger_health = WatchHealth::Unsupported;
            inner.state.ledger_paths.clear();
            inner.state.ledger_baseline_at = None;
            inner.state.ledger_baseline_id = 0;
            return 0;
        }

        let id = inner.next_ledger_baseline_id;
        inner.next_ledger_baseline_id += 1;
        inner.ledger_dirs = parents;
        inner.state.ledger_paths = ledger_paths.iter().cloned().collect();
        inner.state.ledger_dirty = false;
        inner.state.ledger_health = WatchHealth::Healthy;
        inner.state.ledger_baseline_at = Some(Instant::now());
        inner.state.ledger_baseline_id = id;
        eprintln!(
            "👁️  ledger-watch armed (baseline #{}): {} output/input path(s) over {} dir watch(es) — \
             warm no-ops will skip the ledger stat-walk when clean",
            id,
            inner.state.ledger_paths.len(),
            inner.ledger_dirs.len(),
        );
        id
    }

    /// Mint a fresh baseline id over the existing ledger path universe. A
    /// pruned rebuild keeps the same action graph and therefore the same tracked
    /// concrete ledger paths; when the existing watch is still healthy and no
    /// new external dirty event was observed, rebuilding the parent-dir watch
    /// set is wasted work.
    ///
    /// Returns 0 if the existing watch cannot safely authorize a fast baseline.
    pub fn refresh_existing_ledger_baseline(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.inotify.is_none()
            || inner.state.ledger_health != WatchHealth::Healthy
            || inner.state.ledger_paths.is_empty()
            || inner.state.ledger_dirty
        {
            return 0;
        }

        let id = inner.next_ledger_baseline_id;
        inner.next_ledger_baseline_id += 1;
        inner.state.ledger_baseline_at = Some(Instant::now());
        inner.state.ledger_baseline_id = id;
        eprintln!(
            "👁️  ledger-watch re-baselined (baseline #{}): {} output/input path(s) over existing {} dir watch(es)",
            id,
            inner.state.ledger_paths.len(),
            inner.ledger_dirs.len(),
        );
        id
    }

    /// Decide whether the ledger watch can authorize SKIPPING the
    /// `ledger.verify()` output stat-walk for a no-op whose snapshot was analyzed
    /// at `analyzed_at`.
    ///
    /// Returns `Ok(())` only when the ledger watch is HEALTHY, has a baseline,
    /// the baseline id matches `expected_baseline_id` (the GLOBAL watch is
    /// currently baselined over THIS snapshot's outputs — Codex 019f0368), and
    /// NO external change to a tracked output/input was observed since the
    /// baseline (`!ledger_dirty`). Returns `Err(reason)` otherwise (→ the caller
    /// runs the full `ledger.verify()`). NEVER returns `Ok` on any doubt.
    ///
    /// `expected_baseline_id == 0` (a snapshot that never installed a baseline,
    /// or one whose baseline was overwritten/invalidated) never matches.
    pub fn ledger_clean(&self, expected_baseline_id: u64) -> Result<(), String> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match inner.state.ledger_health {
            WatchHealth::Healthy => {}
            WatchHealth::Overflowed => return Err("ledger watch overflowed".into()),
            WatchHealth::Broken => return Err("ledger watch broken".into()),
            WatchHealth::Unsupported => {
                return Err("ledger watch unsupported / no baseline installed".into())
            }
        }
        if inner.state.ledger_baseline_at.is_none() || inner.state.ledger_baseline_id == 0 {
            return Err("ledger watch has no baseline".into());
        }
        if expected_baseline_id == 0 || inner.state.ledger_baseline_id != expected_baseline_id {
            return Err(format!(
                "ledger watch is baselined over a different snapshot (have #{}, want #{})",
                inner.state.ledger_baseline_id, expected_baseline_id
            ));
        }
        if inner.state.ledger_dirty {
            return Err("an external output/input change was observed".into());
        }
        Ok(())
    }
}

/// Fold one inotify event into the dirty state. Returns `false` (and sets the
/// health to Overflowed/Broken) on an event that taints the dirty set —
/// the caller must then full-validate until re-arm.
fn self_fold_event(
    inner: &mut WatcherInner,
    mask: EventMask,
    name: Option<String>,
    wd: WatchDescriptor,
) -> bool {
    // Kernel queue overflowed: events were dropped, BOTH the analysis dirty set
    // AND the ledger dirty set are INCOMPLETE (a dropped event could have been an
    // external output change). Poison both — every no-op now full-validates +
    // full-`ledger.verify()`s until re-arm/re-baseline.
    if mask.contains(EventMask::Q_OVERFLOW) {
        eprintln!("👁️  watcher: IN_Q_OVERFLOW — dirty set incomplete, marking Overflowed");
        inner.state.health = WatchHealth::Overflowed;
        inner.state.ledger_health = WatchHealth::Overflowed;
        return false;
    }
    // IGNORED: the watch was auto-removed (the watched dir was deleted). Drop the
    // bookkeeping; the DELETE_SELF/MOVE_SELF handling below dirties its subtree.
    // If the vanished dir was a LEDGER parent dir, its tracked children lost
    // coverage → poison the ledger watch (Codex 019f0368: DELETE_SELF/IGNORED on
    // a ledger parent is a coverage loss).
    if mask.contains(EventMask::IGNORED) {
        if let Some(dir) = inner.wd_to_dir.remove(&wd) {
            inner.dir_to_wd.remove(&dir);
            inner.analysis_dirs.remove(&dir);
            if inner.ledger_dirs.contains(&dir) {
                inner.state.ledger_health = WatchHealth::Broken;
            }
        }
        return true;
    }

    // Resolve the watched directory this event came from.
    let dir = match inner.wd_to_dir.get(&wd) {
        Some(d) => d.clone(),
        None => {
            // Unknown watch descriptor: we cannot place this event. Conservatively
            // broken (Codex: unknown wd → full-validate). Poison the ledger watch
            // too — the event might have been an external output change we can no
            // longer attribute.
            eprintln!("👁️  watcher: event on unknown watch descriptor — marking Broken");
            inner.state.health = WatchHealth::Broken;
            inner.state.ledger_health = WatchHealth::Broken;
            return false;
        }
    };

    let is_dir = mask.contains(EventMask::ISDIR);

    // ── Phase 4: is this a LEDGER-universe event? ───────────────────────────
    // `dir` is a ledger parent dir iff we added a ledger watch on it. A ledger
    // event marks `ledger_dirty` (unless it is the daemon's own in-build write,
    // suppressed while `build_active`). This is ADDITIVE to the analysis folding
    // below: a dir can be both an analysis dir and a ledger parent (e.g. a source
    // input's dir), in which case both sides update. Under `rush-out` (skipped by
    // the analysis roots) only the ledger side fires.
    let is_ledger_dir = inner.ledger_dirs.contains(&dir);

    // Events about the watched directory NODE itself (no child name).
    if mask.contains(EventMask::DELETE_SELF) || mask.contains(EventMask::MOVE_SELF) {
        // A ledger parent dir vanished/moved → tracked children lost coverage →
        // poison the ledger watch (NOT suppressed by build_active: the daemon
        // never deletes its own output parent dirs mid-build; a vanished output
        // dir is always a real loss). Codex 019f0368.
        if is_ledger_dir {
            inner.state.ledger_health = WatchHealth::Broken;
        }
        // The watched dir vanished/moved: its whole subtree is dirty and our
        // recursive coverage of it is gone. Conservatively dirty the subtree.
        inner.state.dirty_subtrees.insert(dir.clone());
        inner.state.dirty_paths.insert(dir.clone());
        if let Some(parent) = dir.parent() {
            inner.state.dirty_entry_dirs.insert(parent.to_path_buf());
        }
        if let Some(w) = inner.dir_to_wd.remove(&dir) {
            inner.wd_to_dir.remove(&w);
        }
        inner.analysis_dirs.remove(&dir);
        inner.ledger_dirs.remove(&dir);
        return true;
    }

    // Child events: compose the full child path = dir / name.
    let child = name.as_ref().map(|n| dir.join(n));

    // ── Phase 4: ledger-universe child event ────────────────────────────────
    // If `dir` is a ledger parent dir and the full child path is a tracked
    // ledger path, this event touches an output/input the freshness ledger
    // records. Mark `ledger_dirty` UNLESS it is the daemon's own in-build write
    // (suppressed while `build_active` — the no-op path never writes, so a
    // ledger event observed while NOT build_active is necessarily EXTERNAL:
    // a user `rm`/`touch`/rewrite of an output/input). Exact-path membership is
    // sufficient and sound for concrete file outputs/inputs (Codex 019f0368 Q3:
    // no glob/parent-entry-set logic is needed for the ledger's concrete paths).
    if is_ledger_dir {
        if let Some(c) = &child {
            if inner.state.ledger_paths.contains(c) {
                let suppress_as_own_write = if inner.state.build_active {
                    match &inner.state.own_write_paths {
                        None => true,
                        Some(paths) => paths.contains(c),
                    }
                } else {
                    false
                };
                if !suppress_as_own_write {
                    inner.state.ledger_dirty = true;
                }
            }
        }
    }

    // Shared ignore predicate (Codex 019f02fa): an event about rush's/Bazel's
    // OWN output or VCS entry (rush-out, .cache, compile_commands.json,
    // bazel-*, .git, …) is never an analysis-input change. Ignoring it matches
    // how package discovery / globs scope the recorded DirListing digest, so the
    // daemon's own output churn during a build does not spuriously dirty a
    // DirListing(root) dep and force a fallback. This CANNOT mask a real source
    // change: a source file/dir never has one of these reserved names.
    if let Some(n) = name.as_deref() {
        if is_ignored_entry_name(n) {
            return true;
        }
    }

    let create_like = mask.contains(EventMask::CREATE) || mask.contains(EventMask::MOVED_TO);
    let delete_like = mask.contains(EventMask::DELETE) || mask.contains(EventMask::MOVED_FROM);
    let modify_like = mask.contains(EventMask::MODIFY)
        || mask.contains(EventMask::CLOSE_WRITE)
        || mask.contains(EventMask::ATTRIB);

    if create_like || delete_like {
        // The child-NAME SET of `dir` changed → DirListing(dir) and any negative
        // FileExistence whose parent is `dir` is dirty.
        inner.state.dirty_entry_dirs.insert(dir.clone());
        if let Some(c) = &child {
            // The child path itself is dirty (covers a positive/negative
            // existence dep, content dep, metadata dep at that exact path).
            inner.state.dirty_paths.insert(c.clone());
            if is_dir {
                // A directory appeared/vanished: its whole subtree is dirty
                // (descendants we may have a negative dep on, and — for CREATE —
                // the recursive-watch race: files can appear under it before we
                // add a watch). Mark the subtree, and (best-effort) extend
                // coverage to the new dir so future edits under it are seen.
                inner.state.dirty_subtrees.insert(c.clone());
                if create_like && c.exists() {
                    if let Err(e) = WorkspaceWatcher::add_watch_recursive(inner, c) {
                        // Could not extend coverage to the new subtree: we'd miss
                        // future edits there. Conservatively Broken.
                        eprintln!(
                            "👁️  watcher: failed to extend watch to new dir {} ({e}); marking Broken",
                            c.display()
                        );
                        inner.state.health = WatchHealth::Broken;
                        // This taint aborts the drain loop; a still-Healthy ledger
                        // watch would then go blind to undrained events. Poison it
                        // too so the next no-op full-`ledger.verify()`s.
                        inner.state.ledger_health = WatchHealth::Broken;
                        return false;
                    }
                }
            }
        }
        return true;
    }

    if modify_like {
        // Content/metadata of a specific child changed.
        if let Some(c) = &child {
            inner.state.dirty_paths.insert(c.clone());
        } else {
            // A modify with no name shouldn't happen for a dir-watch (those are
            // file-watch events); be conservative and dirty the dir node.
            inner.state.dirty_paths.insert(dir.clone());
        }
        return true;
    }

    // Any other event class we did not explicitly model: be conservative.
    // (We only requested the mask above, so this should not fire, but if the
    // kernel delivers something unexpected we refuse to trust the dirty set.)
    eprintln!(
        "👁️  watcher: unmodeled event mask {:?} on {} — marking Broken",
        mask,
        dir.display()
    );
    inner.state.health = WatchHealth::Broken;
    inner.state.ledger_health = WatchHealth::Broken;
    false
}

/// Is this dependency dirty against the watch state? Returns `Some(reason)` if
/// dirty (the stat-walk cannot be skipped), `None` if provably clean.
///
/// Matching rules (Codex 019f02fa):
/// * a file-ish dep at `P` is dirty if `P` is in `dirty_paths`, or `P`'s parent
///   is a `dirty_entry_dir` (its name-set changed — covers `P` being
///   created/deleted, which is exactly what a negative/positive existence dep
///   cares about), or any `dirty_subtree` is an ancestor of `P`.
/// * a `DirListing(D)` dep is dirty if `D` is a `dirty_entry_dir`, or `D` is in
///   `dirty_paths`, or `D` is under a `dirty_subtree`.
/// * non-fs deps (Env/Opt/ConfigFingerprint/ProcessProbe/kernel state) are NOT covered by the
///   filesystem watcher — they are validated separately by the caller (the env/
///   config are folded into the AnalysisKey + command-semantics match; a
///   ProcessProbe is conservatively treated as "always re-validate" by returning
///   dirty so the manifest walk re-runs it).
fn dep_dirty(state: &WatchState, dep: &Dependency) -> Option<String> {
    let path_dirty = |p: &Path| -> Option<String> {
        if state.dirty_paths.contains(p) {
            return Some(format!("watched path dirty: {}", p.display()));
        }
        if let Some(parent) = p.parent() {
            if state.dirty_entry_dirs.contains(parent) {
                return Some(format!("parent dir entry-set changed: {}", p.display()));
            }
        }
        if is_under_any(&state.dirty_subtrees, p) {
            return Some(format!("under dirty subtree: {}", p.display()));
        }
        None
    };
    let dir_dirty = |d: &Path| -> Option<String> {
        if state.dirty_entry_dirs.contains(d) {
            return Some(format!("dir entry-set changed: {}", d.display()));
        }
        if state.dirty_paths.contains(d) {
            return Some(format!("watched dir path dirty: {}", d.display()));
        }
        if is_under_any(&state.dirty_subtrees, d) {
            return Some(format!("dir under dirty subtree: {}", d.display()));
        }
        None
    };
    match dep {
        Dependency::FileContent { path, .. }
        | Dependency::FileExistence { path, .. }
        | Dependency::Symlink { path, .. }
        | Dependency::FileMetadata { path, .. } => path_dirty(path),
        Dependency::DirListing { path, .. } => dir_dirty(path),
        // Non-filesystem deps: the watcher does not cover these. Env/Opt/Config
        // changes are caught by the AnalysisKey / command-semantics match BEFORE
        // we ever consult the watcher (a changed --define/env yields a different
        // config_fingerprint → different AnalysisKey → cache miss → full
        // analysis). A ProcessProbe must be re-run to be sure, so we force the
        // manifest walk for it (return dirty) — a probe in the manifest is rare
        // and correctness dominates.
        Dependency::Env { .. } | Dependency::Opt { .. } | Dependency::ConfigFingerprint { .. } => {
            None
        }
        Dependency::ProcessProbe { program, .. } => Some(format!(
            "process-probe dep requires re-validation: {}",
            program.display()
        )),
        Dependency::LinuxProcFilesystems { .. } => {
            Some("Linux filesystem registry requires re-validation".to_string())
        }
        Dependency::LinuxProcVmOvercommitMemory { .. } => {
            Some("Linux overcommit policy requires re-validation".to_string())
        }
        Dependency::LinuxCgroupV2CpuMax { .. } => {
            Some("Linux cgroup-v2 CPU quota requires re-validation".to_string())
        }
    }
}

/// Directory whose inotify watch is required to make a no-event verdict sound.
/// Non-filesystem dependencies have no required directory; their freshness is
/// covered by the AnalysisKey/semantics gate or forced manifest validation.
fn dependency_watch_dir(dep: &Dependency) -> Option<&Path> {
    match dep {
        Dependency::FileContent { path, .. }
        | Dependency::FileExistence { path, .. }
        | Dependency::FileMetadata { path, .. }
        | Dependency::Symlink { path, .. } => path.parent(),
        Dependency::DirListing { path, .. } => Some(path),
        Dependency::Env { .. }
        | Dependency::Opt { .. }
        | Dependency::ConfigFingerprint { .. }
        | Dependency::ProcessProbe { .. }
        | Dependency::LinuxProcFilesystems { .. }
        | Dependency::LinuxProcVmOvercommitMemory { .. }
        | Dependency::LinuxCgroupV2CpuMax { .. } => None,
    }
}

fn ensure_dependency_watch_coverage(inner: &WatcherInner, dep: &Dependency) -> Result<(), String> {
    let Some(dir) = dependency_watch_dir(dep) else {
        return Ok(());
    };
    if inner.analysis_dirs.contains(dir) {
        return Ok(());
    }
    Err(format!(
        "manifest dependency directory is not watched: {}",
        dir.display()
    ))
}

fn ensure_index_watch_coverage(
    inner: &WatcherInner,
    index: &ManifestWatchIndex,
) -> Result<(), String> {
    let first_unwatched = index
        .entry_sensitive_dirs
        .iter()
        .filter(|dir| !inner.analysis_dirs.contains(*dir))
        .min();
    match first_unwatched {
        Some(dir) => Err(format!(
            "manifest dependency directory is not watched: {}",
            dir.display()
        )),
        None => Ok(()),
    }
}

/// Return the exact dirty file path for deps that are safe to cover with the
/// action ledger, or an error explaining why the manifest must be fully
/// validated instead.
fn dep_exact_dirty_path<'a>(
    state: &WatchState,
    dep: &'a Dependency,
) -> Result<Option<&'a Path>, String> {
    let fileish = |p: &'a Path| -> Result<Option<&'a Path>, String> {
        if let Some(parent) = p.parent() {
            if state.dirty_entry_dirs.contains(parent) {
                return Err(format!("parent dir entry-set changed: {}", p.display()));
            }
        }
        if is_under_any(&state.dirty_subtrees, p) {
            return Err(format!("under dirty subtree: {}", p.display()));
        }
        if state.dirty_paths.contains(p) {
            return Ok(Some(p));
        }
        Ok(None)
    };

    match dep {
        Dependency::FileContent { path, .. }
        | Dependency::FileExistence { path, .. }
        | Dependency::FileMetadata { path, .. } => fileish(path),
        Dependency::Symlink { path, .. } => {
            if dep_dirty(state, dep).is_some() {
                Err(format!("symlink dep changed: {}", path.display()))
            } else {
                Ok(None)
            }
        }
        Dependency::DirListing { .. } => {
            if let Some(reason) = dep_dirty(state, dep) {
                Err(reason)
            } else {
                Ok(None)
            }
        }
        Dependency::Env { .. } | Dependency::Opt { .. } | Dependency::ConfigFingerprint { .. } => {
            Ok(None)
        }
        Dependency::ProcessProbe { program, .. } => Err(format!(
            "process-probe dep requires re-validation: {}",
            program.display()
        )),
        Dependency::LinuxProcFilesystems { .. } => {
            Err("Linux filesystem registry requires re-validation".to_string())
        }
        Dependency::LinuxProcVmOvercommitMemory { .. } => {
            Err("Linux overcommit policy requires re-validation".to_string())
        }
        Dependency::LinuxCgroupV2CpuMax { .. } => {
            Err("Linux cgroup-v2 CPU quota requires re-validation".to_string())
        }
    }
}

/// Is `p` at or under any directory in `set`?
fn is_under_any(set: &HashSet<PathBuf>, p: &Path) -> bool {
    if set.is_empty() {
        return false;
    }
    for d in set {
        if p == d || p.starts_with(d) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rush_incremental::FileKind;

    fn argv(items: &[&str]) -> Vec<Bytes> {
        items.iter().map(|s| Bytes::from(s.as_bytes().to_vec())).collect()
    }
    fn no_env(_: &[u8]) -> Option<Vec<u8>> {
        None
    }

    #[test]
    fn watchfs_mode_parsing() {
        assert_eq!(WatchFsMode::parse(&argv(&["build", "//..."]), no_env), WatchFsMode::Auto);
        assert_eq!(
            WatchFsMode::parse(&argv(&["build", "--watchfs=off", "//..."]), no_env),
            WatchFsMode::Off
        );
        assert_eq!(
            WatchFsMode::parse(&argv(&["build", "--watchfs", "require", "//..."]), no_env),
            WatchFsMode::Require
        );
        assert_eq!(
            WatchFsMode::parse(&argv(&["build", "--watchfs=auto"]), no_env),
            WatchFsMode::Auto
        );
        // env fallback
        assert_eq!(
            WatchFsMode::parse(&argv(&["build"]), |k| if k == b"RUSH_WATCHFS" {
                Some(b"off".to_vec())
            } else {
                None
            }),
            WatchFsMode::Off
        );
    }

    fn state_with(
        paths: &[&str],
        entry_dirs: &[&str],
        subtrees: &[&str],
        health: WatchHealth,
    ) -> WatchState {
        WatchState {
            epoch: 1,
            health,
            armed_at: Some(Instant::now()),
            dirty_paths: paths.iter().map(PathBuf::from).collect(),
            dirty_entry_dirs: entry_dirs.iter().map(PathBuf::from).collect(),
            dirty_subtrees: subtrees.iter().map(PathBuf::from).collect(),
            ledger_health: WatchHealth::Unsupported,
            ledger_paths: HashSet::new(),
            ledger_dirty: false,
            ledger_baseline_at: None,
            ledger_baseline_id: 0,
            build_active: false,
            own_write_paths: None,
        }
    }

    #[test]
    fn file_content_dirty_on_exact_path() {
        let st = state_with(&["/ws/a.cc"], &[], &[], WatchHealth::Healthy);
        let dep = Dependency::FileContent {
            path: PathBuf::from("/ws/a.cc"),
            digest: [0u8; 32],
        };
        assert!(dep_dirty(&st, &dep).is_some());
        let clean = Dependency::FileContent {
            path: PathBuf::from("/ws/b.cc"),
            digest: [0u8; 32],
        };
        assert!(dep_dirty(&st, &clean).is_none());
    }

    #[test]
    fn negative_existence_dirty_when_parent_entry_set_changes() {
        // A negative FileExistence on /ws/pkg/new.cc must be dirty when a file is
        // created in /ws/pkg (parent entry-set changed).
        let st = state_with(&[], &["/ws/pkg"], &[], WatchHealth::Healthy);
        let dep = Dependency::FileExistence {
            path: PathBuf::from("/ws/pkg/new.cc"),
            exists: false,
            kind: None,
        };
        assert!(dep_dirty(&st, &dep).is_some());
    }

    #[test]
    fn dirlisting_dirty_on_entry_dir() {
        let st = state_with(&[], &["/ws/pkg"], &[], WatchHealth::Healthy);
        let dep = Dependency::DirListing {
            path: PathBuf::from("/ws/pkg"),
            entries_digest: [0u8; 32],
        };
        assert!(dep_dirty(&st, &dep).is_some());
        // A DirListing on a different dir is clean.
        let other = Dependency::DirListing {
            path: PathBuf::from("/ws/other"),
            entries_digest: [0u8; 32],
        };
        assert!(dep_dirty(&st, &other).is_none());
    }

    #[test]
    fn subtree_dirties_everything_under_it() {
        let st = state_with(&[], &[], &["/ws/newdir"], WatchHealth::Healthy);
        let dep = Dependency::FileExistence {
            path: PathBuf::from("/ws/newdir/deep/file.h"),
            exists: false,
            kind: None,
        };
        assert!(dep_dirty(&st, &dep).is_some());
        let dirdep = Dependency::DirListing {
            path: PathBuf::from("/ws/newdir/deep"),
            entries_digest: [0u8; 32],
        };
        assert!(dep_dirty(&st, &dirdep).is_some());
    }

    #[test]
    fn process_probe_always_revalidates() {
        let st = state_with(&[], &[], &[], WatchHealth::Healthy);
        let dep = Dependency::ProcessProbe {
            program: PathBuf::from("/usr/bin/pkg-config"),
            args: vec![],
            env_keys: vec![],
            cwd: PathBuf::from("/ws"),
            status: 0,
            stdout_digest: [0u8; 32],
            stderr_digest: [0u8; 32],
            compare_stderr: true,
        };
        assert!(dep_dirty(&st, &dep).is_some());
    }

    #[test]
    fn linux_proc_filesystems_always_revalidates() {
        let st = state_with(&[], &[], &[], WatchHealth::Healthy);
        let dep = Dependency::LinuxProcFilesystems {
            bytes: 1,
            digest: [0u8; 32],
        };
        assert!(dep_dirty(&st, &dep).is_some());
        let index = ManifestWatchIndex::from_manifest(&manifest(vec![dep]));
        assert!(index.has_linux_proc_filesystems);
    }

    // ── Real-inotify end-to-end tests (exercise arm + drain + clean_for) ──
    //
    // These drive an actual inotify fd over a tempdir. They assert the
    // correctness contract: a quiet workspace is CLEAN (0-path no-op), and a
    // real edit/create/delete dirties the matching dependency so clean_for
    // returns Err (→ the caller full-validates, → the change rebuilds).

    fn content_dep(p: &Path) -> Dependency {
        Dependency::FileContent { path: p.to_path_buf(), digest: [0u8; 32] }
    }
    fn dirlisting_dep(p: &Path) -> Dependency {
        Dependency::DirListing { path: p.to_path_buf(), entries_digest: [0u8; 32] }
    }
    fn neg_existence_dep(p: &Path) -> Dependency {
        Dependency::FileExistence { path: p.to_path_buf(), exists: false, kind: None }
    }
    fn manifest(deps: Vec<Dependency>) -> rush_incremental::DependencyManifest {
        rush_incremental::DependencyManifest::from_deps(deps)
    }
    // Drain a few times with a small settle so async inotify delivery lands.
    fn settle_drain(w: &WorkspaceWatcher) {
        for _ in 0..5 {
            w.drain();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        w.drain();
    }

    #[test]
    fn e2e_quiet_workspace_is_clean_zero_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let a = root.join("a.cc");
        std::fs::write(&a, b"int main(){}").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        assert_eq!(w.health(), WatchHealth::Healthy);
        let analyzed_at = Instant::now();
        // No edits → clean.
        settle_drain(&w);
        let m = manifest(vec![content_dep(&a), dirlisting_dep(&root)]);
        assert!(
            w.clean_for(&m, analyzed_at).is_ok(),
            "a quiet workspace must authorize a 0-path no-op"
        );
    }

    #[test]
    fn e2e_unwatched_manifest_dependency_forces_full_validation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        // `.cache` is deliberately excluded from recursive analysis watches.
        // A Bzlmod/Bazel repository can nevertheless enter the manifest from
        // there, so a quiet dirty set alone must not authorize reuse.
        let module = root.join(".cache/rush/external/rules_cc/MODULE.bazel");
        std::fs::create_dir_all(module.parent().unwrap()).unwrap();
        std::fs::write(&module, b"module(name = \"rules_cc\")\n").unwrap();

        let watcher = WorkspaceWatcher::unarmed();
        watcher.arm(&[root]);
        assert_eq!(watcher.health(), WatchHealth::Healthy);
        let analyzed_at = Instant::now();
        settle_drain(&watcher);

        let manifest = manifest(vec![content_dep(&module)]);
        let reason = watcher.clean_for(&manifest, analyzed_at).unwrap_err();
        assert!(
            reason.contains("not watched"),
            "unexpected fallback: {reason}"
        );

        let index = ManifestWatchIndex::from_manifest(&manifest);
        let reason = watcher
            .admit_manifest_indexed_with_covered_dirty_paths(&index, analyzed_at, |_| false)
            .unwrap_err();
        assert!(
            reason.contains("not watched"),
            "unexpected fallback: {reason}"
        );

        let reason = watcher
            .manifest_dirty_paths_covered_by(&manifest, analyzed_at, |_| false)
            .unwrap_err();
        assert!(
            reason.contains("not watched"),
            "unexpected fallback: {reason}"
        );

        // A ledger watch installed after analysis is not analysis-continuous:
        // sharing the inotify descriptor must not turn this into a zero-stat
        // admission proof.
        watcher.begin_build();
        assert_ne!(
            watcher.install_ledger_watch(std::slice::from_ref(&module)),
            0
        );
        watcher.end_build();
        let reason = watcher.clean_for(&manifest, analyzed_at).unwrap_err();
        assert!(
            reason.contains("not watched"),
            "late ledger watch incorrectly covered analysis: {reason}"
        );
    }

    #[test]
    fn e2e_edit_cc_dirties_content_dep() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let a = root.join("a.cc");
        std::fs::write(&a, b"v1").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        let analyzed_at = Instant::now();
        // Edit the file AFTER arm + analyze.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&a, b"v2-different-length").unwrap();
        settle_drain(&w);
        let m = manifest(vec![content_dep(&a)]);
        assert!(
            w.clean_for(&m, analyzed_at).is_err(),
            "an edited .cc must dirty its FileContent dep (force full validate)"
        );
    }

    #[test]
    fn e2e_exact_dirty_manifest_path_can_be_covered_by_ledger() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let a = root.join("a.cc");
        std::fs::write(&a, b"v1").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        let analyzed_at = Instant::now();

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&a, b"v2-different-length").unwrap();
        settle_drain(&w);
        let m = manifest(vec![content_dep(&a)]);
        let covered = w
            .manifest_dirty_paths_covered_by(&m, analyzed_at, |p| p == a.as_path())
            .expect("exact content dirty covered by the ledger should be admissible");
        assert_eq!(covered, vec![a.clone()]);
        let index = ManifestWatchIndex::from_manifest(&m);
        assert_eq!(
            w.admit_manifest_indexed_with_covered_dirty_paths(
                &index,
                analyzed_at,
                |p| p == a.as_path()
            )
            .unwrap(),
            ManifestWatchAdmission::CoveredDirty(vec![a])
        );
    }

    #[test]
    fn e2e_exact_dirty_manifest_path_must_be_ledger_covered() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let a = root.join("a.cc");
        std::fs::write(&a, b"v1").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        let analyzed_at = Instant::now();

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&a, b"v2-different-length").unwrap();
        settle_drain(&w);
        let m = manifest(vec![content_dep(&a)]);
        assert!(
            w.manifest_dirty_paths_covered_by(&m, analyzed_at, |_| false)
                .is_err(),
            "an exact dirty dep not tracked by the ledger must full-validate"
        );
        let index = ManifestWatchIndex::from_manifest(&m);
        assert!(
            w.admit_manifest_indexed_with_covered_dirty_paths(&index, analyzed_at, |_| false)
                .is_err(),
            "indexed admission must also reject uncovered dirty deps"
        );
    }

    #[test]
    fn e2e_create_in_pkg_dirties_dirlisting_and_negative_existence() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let pkg = root.join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        let analyzed_at = Instant::now();
        // A glob over pkg (DirListing) + a negative existence on pkg/new.cc.
        let newf = pkg.join("new.cc");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&newf, b"x").unwrap();
        settle_drain(&w);
        // DirListing(pkg) must be dirty.
        let m_dir = manifest(vec![dirlisting_dep(&pkg)]);
        assert!(w.clean_for(&m_dir, analyzed_at).is_err(), "create must dirty DirListing");
        assert!(
            w.manifest_dirty_paths_covered_by(&m_dir, analyzed_at, |_| true)
                .is_err(),
            "directory-entry changes can alter analysis shape and must full-validate"
        );
        let idx_dir = ManifestWatchIndex::from_manifest(&m_dir);
        assert!(
            w.admit_manifest_indexed_with_covered_dirty_paths(&idx_dir, analyzed_at, |_| true)
                .is_err(),
            "indexed admission must reject directory-entry changes"
        );
        // Negative existence on the now-created file must be dirty.
        let m_neg = manifest(vec![neg_existence_dep(&newf)]);
        assert!(w.clean_for(&m_neg, analyzed_at).is_err(), "create must dirty negative existence");
        assert!(
            w.manifest_dirty_paths_covered_by(&m_neg, analyzed_at, |_| true)
                .is_err(),
            "created/deleted paths dirty the parent entry set and must full-validate"
        );
        let idx_neg = ManifestWatchIndex::from_manifest(&m_neg);
        assert!(
            w.admit_manifest_indexed_with_covered_dirty_paths(&idx_neg, analyzed_at, |_| true)
                .is_err(),
            "indexed admission must reject create/delete entry-set changes"
        );
    }

    #[test]
    fn e2e_unrelated_edit_keeps_dep_clean() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let a = root.join("a.cc");
        let b = root.join("b.cc");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        let analyzed_at = Instant::now();
        // Edit b; a's content dep must stay clean (precision, not just safety).
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&b, b"b-changed").unwrap();
        settle_drain(&w);
        let m = manifest(vec![content_dep(&a)]);
        // a is clean...
        assert!(w.clean_for(&m, analyzed_at).is_ok(), "an unrelated edit must NOT dirty a's dep");
        // ...but b is dirty.
        let mb = manifest(vec![content_dep(&b)]);
        assert!(w.clean_for(&mb, analyzed_at).is_err());
    }

    #[test]
    fn e2e_output_churn_does_not_dirty_root_dirlisting() {
        // The daemon's own outputs (rush-out/, .cache/, compile_commands.json)
        // churning inside the watched workspace root must NOT dirty a
        // DirListing(root) dep — else every warm no-op falls back. (Codex
        // 019f02fa shared-ignore-predicate fix.)
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::write(root.join("BUILD"), b"# pkg").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        let analyzed_at = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Simulate output churn: create rush-out/, .cache/, compile_commands.json
        // and write files under them.
        std::fs::create_dir_all(root.join("rush-out").join("objs")).unwrap();
        std::fs::write(root.join("rush-out").join("objs").join("a.o"), b"obj").unwrap();
        std::fs::create_dir_all(root.join(".cache").join("rush")).unwrap();
        std::fs::write(root.join(".cache").join("rush").join("x"), b"c").unwrap();
        std::fs::write(root.join("compile_commands.json"), b"[]").unwrap();
        settle_drain(&w);
        // A DirListing(root) dep must stay CLEAN despite the output churn.
        let m = manifest(vec![dirlisting_dep(&root)]);
        assert!(
            w.clean_for(&m, analyzed_at).is_ok(),
            "output churn must NOT dirty DirListing(root) — got {:?}",
            w.clean_for(&m, analyzed_at)
        );
        // ...but a REAL new source file in the root must still dirty it.
        std::fs::write(root.join("new_src.cc"), b"int x;").unwrap();
        settle_drain(&w);
        assert!(
            w.clean_for(&m, analyzed_at).is_err(),
            "a real new source file MUST dirty DirListing(root)"
        );
    }

    #[test]
    fn e2e_continuity_gate_rejects_pre_arm_snapshot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let a = root.join("a.cc");
        std::fs::write(&a, b"a").unwrap();
        // A snapshot "analyzed" BEFORE we arm: no continuity.
        let analyzed_before = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        settle_drain(&w);
        let m = manifest(vec![content_dep(&a)]);
        assert!(
            w.clean_for(&m, analyzed_before).is_err(),
            "a snapshot analyzed before the watch armed has no continuity → full validate"
        );
    }

    #[test]
    fn e2e_unarmed_watcher_never_authorizes_skip() {
        let w = WorkspaceWatcher::unarmed();
        assert_eq!(w.health(), WatchHealth::Unsupported);
        let m = manifest(vec![content_dep(Path::new("/ws/a.cc"))]);
        assert!(
            w.clean_for(&m, Instant::now()).is_err(),
            "an unarmed/unsupported watcher must always full-validate"
        );
    }

    #[test]
    fn e2e_overflow_forces_fallback() {
        // Simulate a queue overflow by folding a synthetic Q_OVERFLOW event:
        // health flips to Overflowed and clean_for refuses. (We test the fold
        // logic directly since forcing a real kernel overflow is flaky.)
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        {
            let mut inner = w.inner.lock().unwrap();
            inner.state.health = WatchHealth::Overflowed;
        }
        let a = root.join("a.cc");
        std::fs::write(&a, b"a").unwrap();
        let m = manifest(vec![content_dep(&a)]);
        assert!(
            w.clean_for(&m, Instant::now()).is_err(),
            "an overflowed watcher must fall back to full validate"
        );
    }

    #[test]
    fn metadata_dep_uses_path_rules() {
        let st = state_with(&["/ws/tool"], &[], &[], WatchHealth::Healthy);
        let dep = Dependency::FileMetadata {
            path: PathBuf::from("/ws/tool"),
            present: true,
            len: 10,
            mode: 0o755,
            mtime_ns: 0,
            ctime_ns: 0,
            dev: 0,
            ino: 0,
        };
        assert!(dep_dirty(&st, &dep).is_some());
        let _ = FileKind::File; // keep the import used across cfgs
    }

    // ── Phase 4: ledger (OUTPUT-universe) watch tests ───────────────────────
    //
    // These drive a real inotify fd over a tempdir "rush-out" to assert the
    // own-writes-vs-external distinction (the crux): a build's own writes inside
    // the begin/end window are SUPPRESSED; an external rm/modify between builds
    // is CAUGHT (ledger_clean -> Err -> caller full-`verify()`s -> rebuild).

    #[test]
    fn e2e_ledger_quiet_is_clean_zero_output_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let out = root.join("rush-out");
        std::fs::create_dir_all(&out).unwrap();
        let o = out.join("lib.o");
        std::fs::write(&o, b"obj").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]); // analysis roots skip rush-out
        // Install the ledger watch over the output universe.
        let bid = w.install_ledger_watch(&[o.clone()]);
        settle_drain(&w);
        // No external change → ledger watch authorizes skipping verify().
        assert!(
            w.ledger_clean(bid).is_ok(),
            "a quiet output universe must authorize a 0-output-path no-op"
        );
    }

    #[test]
    fn e2e_ledger_external_rm_is_caught() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let out = root.join("rush-out");
        std::fs::create_dir_all(&out).unwrap();
        let o = out.join("lib.o");
        std::fs::write(&o, b"obj").unwrap();
        let bin = out.join("app");
        std::fs::write(&bin, b"ELF").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        let bid = w.install_ledger_watch(&[o.clone(), bin.clone()]);
        settle_drain(&w);
        assert!(w.ledger_clean(bid).is_ok(), "clean before the rm");
        // External rm of a .o (no build_active) → CAUGHT.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::remove_file(&o).unwrap();
        settle_drain(&w);
        assert!(
            w.ledger_clean(bid).is_err(),
            "an external rm of a tracked output MUST dirty the ledger watch"
        );
    }

    #[test]
    fn e2e_ledger_external_modify_is_caught() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let out = root.join("rush-out");
        std::fs::create_dir_all(&out).unwrap();
        let bin = out.join("app");
        std::fs::write(&bin, b"v1").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        let bid = w.install_ledger_watch(&[bin.clone()]);
        settle_drain(&w);
        assert!(w.ledger_clean(bid).is_ok());
        // External modify of the final binary → CAUGHT.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&bin, b"v2-tampered").unwrap();
        settle_drain(&w);
        assert!(
            w.ledger_clean(bid).is_err(),
            "an external modify of a tracked output MUST dirty the ledger watch"
        );
    }

    #[test]
    fn e2e_ledger_own_writes_suppressed_external_after_window_caught() {
        // The crux: the daemon's OWN output writes (begin_build..end_build) must
        // NOT dirty the ledger; an EXTERNAL write after end_build MUST.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let out = root.join("rush-out");
        std::fs::create_dir_all(&out).unwrap();
        let o = out.join("lib.o");
        std::fs::write(&o, b"v1").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        let _ = w.install_ledger_watch(&[o.clone()]);
        settle_drain(&w);

        // Simulate a build: own-write while build_active → suppressed.
        w.begin_build();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&o, b"v2-rebuilt-by-daemon").unwrap(); // daemon's own write
        settle_drain(&w); // drained WHILE build_active
        // Re-baseline (install resets dirty) + close window + final drain, exactly
        // as the orchestrator's capture block does.
        let bid = w.install_ledger_watch(&[o.clone()]);
        w.drain();
        w.end_build();
        let _ = w.drain();
        assert!(
            w.ledger_clean(bid).is_ok(),
            "the daemon's own in-build write must NOT dirty the ledger watch"
        );

        // Now an EXTERNAL write after the window → CAUGHT.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&o, b"v3-external-tamper").unwrap();
        settle_drain(&w);
        assert!(
            w.ledger_clean(bid).is_err(),
            "an external write after the build window MUST dirty the ledger watch"
        );
    }

    #[test]
    fn e2e_ledger_pruned_build_rebaseline_suppresses_active_output() {
        // A pruned rebuild only owns the affected slice's declared outputs.
        // Rewriting one of those paths while build_active must not poison the
        // ledger, so the caller can mint a cheap baseline over the unchanged
        // ledger watch universe.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let out = root.join("rush-out");
        std::fs::create_dir_all(&out).unwrap();
        let active = out.join("active.o");
        let inactive = out.join("inactive.o");
        std::fs::write(&active, b"a1").unwrap();
        std::fs::write(&inactive, b"i1").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        let bid = w.install_ledger_watch(&[active.clone(), inactive.clone()]);
        settle_drain(&w);
        assert!(w.ledger_clean(bid).is_ok());

        w.begin_pruned_build(&[active.clone()]);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&active, b"a2-rebuilt-by-daemon").unwrap();
        settle_drain(&w);
        let new_bid = w.refresh_existing_ledger_baseline();
        w.drain();
        w.end_build();
        let _ = w.drain();
        assert_ne!(
            new_bid, 0,
            "active-slice own writes must allow a cheap pruned rebaseline"
        );
        assert!(
            w.ledger_clean(new_bid).is_ok(),
            "the rebaselined ledger must stay clean after an active output write"
        );
    }

    #[test]
    fn e2e_ledger_pruned_build_inactive_output_blocks_rebaseline() {
        // An inactive tracked output touched during a pruned build is NOT a
        // daemon-owned write for this slice. The event must stay dirty and block
        // the cheap existing-watch rebaseline.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let out = root.join("rush-out");
        std::fs::create_dir_all(&out).unwrap();
        let active = out.join("active.o");
        let inactive = out.join("inactive.o");
        std::fs::write(&active, b"a1").unwrap();
        std::fs::write(&inactive, b"i1").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        let bid = w.install_ledger_watch(&[active.clone(), inactive.clone()]);
        settle_drain(&w);
        assert!(w.ledger_clean(bid).is_ok());

        w.begin_pruned_build(&[active.clone()]);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&active, b"a2-rebuilt-by-daemon").unwrap();
        std::fs::write(&inactive, b"i2-external-tamper").unwrap();
        settle_drain(&w);
        assert_eq!(
            w.refresh_existing_ledger_baseline(),
            0,
            "inactive tracked output movement must block cheap pruned rebaseline"
        );
        w.end_build();
        let _ = w.drain();
        assert!(
            w.ledger_clean(bid).is_err(),
            "inactive tracked output movement must force full ledger.verify()"
        );
    }

    #[test]
    fn e2e_ledger_external_change_during_window_after_basline_is_caught() {
        // Race guard: an external change that lands AFTER end_build (i.e. after
        // the suppression window closed) is caught by the final/next drain, even
        // if it lands "during" what a naive whole-build window would have covered.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let out = root.join("rush-out");
        std::fs::create_dir_all(&out).unwrap();
        let o = out.join("lib.o");
        std::fs::write(&o, b"v1").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        w.begin_build();
        let bid = w.install_ledger_watch(&[o.clone()]);
        w.drain();
        w.end_build(); // window closed
        // External rm lands now (post-window) → must be caught.
        std::fs::remove_file(&o).unwrap();
        settle_drain(&w);
        assert!(
            w.ledger_clean(bid).is_err(),
            "an external rm after the suppression window closed MUST be caught"
        );
    }

    #[test]
    fn e2e_ledger_overflow_forces_full_verify() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let out = root.join("rush-out");
        std::fs::create_dir_all(&out).unwrap();
        let o = out.join("lib.o");
        std::fs::write(&o, b"obj").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        let bid = w.install_ledger_watch(&[o.clone()]);
        settle_drain(&w);
        assert!(w.ledger_clean(bid).is_ok());
        // Simulate a queue overflow: poisons the ledger watch → full verify().
        {
            let mut inner = w.inner.lock().unwrap();
            inner.state.ledger_health = WatchHealth::Overflowed;
        }
        assert!(
            w.ledger_clean(bid).is_err(),
            "an overflowed ledger watch must fall back to full ledger.verify()"
        );
    }

    #[test]
    fn e2e_ledger_unsupported_until_baseline() {
        // A freshly-armed watcher (no build yet) has NO ledger baseline → every
        // no-op must full-`ledger.verify()` until a build installs one.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        assert!(
            w.ledger_clean(1).is_err(),
            "no ledger baseline yet → must full-verify"
        );
    }

    #[test]
    fn e2e_ledger_unrelated_sibling_does_not_dirty() {
        // A non-tracked sibling in a tracked parent dir must NOT dirty the ledger
        // (exact-child-path membership, not whole-dir).
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let out = root.join("rush-out");
        std::fs::create_dir_all(&out).unwrap();
        let o = out.join("lib.o");
        std::fs::write(&o, b"obj").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        let bid = w.install_ledger_watch(&[o.clone()]); // only lib.o is tracked
        settle_drain(&w);
        // Write an UNTRACKED sibling in the same dir.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(out.join("scratch.tmp"), b"junk").unwrap();
        settle_drain(&w);
        assert!(
            w.ledger_clean(bid).is_ok(),
            "an untracked sibling write must NOT dirty the ledger watch"
        );
    }

    #[test]
    fn e2e_ledger_baseline_id_mismatch_forces_full_verify() {
        // The global ledger watch is baselined over ONE snapshot's outputs at a
        // time. A snapshot holding a STALE baseline id (e.g. snapshot A after a
        // build of snapshot B overwrote the global watch) must NOT be authorized
        // to skip — it would be consulting a watch over B's outputs, not A's
        // (Codex 019f0368). Only the exact current baseline id matches.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let out = root.join("rush-out");
        std::fs::create_dir_all(&out).unwrap();
        let a = out.join("a.o");
        let b = out.join("b.o");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let w = WorkspaceWatcher::unarmed();
        w.arm(&[root.clone()]);
        // "Snapshot A" installs a baseline over {a.o}.
        let bid_a = w.install_ledger_watch(&[a.clone()]);
        assert!(w.ledger_clean(bid_a).is_ok(), "A is clean for its own baseline");
        // "Snapshot B" builds and OVERWRITES the global watch with {b.o}.
        let bid_b = w.install_ledger_watch(&[b.clone()]);
        assert_ne!(bid_a, bid_b, "each install mints a fresh baseline id");
        // A no-op for snapshot A (still holding bid_a) must NOT skip — the watch
        // now covers B's outputs, not A's.
        assert!(
            w.ledger_clean(bid_a).is_err(),
            "a snapshot with a stale baseline id MUST full-verify"
        );
        // B (the current baseline) may still skip.
        assert!(w.ledger_clean(bid_b).is_ok(), "B is clean for the current baseline");
        // id 0 (never installed) never matches.
        assert!(w.ledger_clean(0).is_err(), "baseline id 0 never authorizes a skip");
    }
}
