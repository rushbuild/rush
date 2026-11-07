//! The [`DependencyManifest`] — the ordered, deduped, deterministic record of
//! everything one analysis observed, plus its validation and fingerprint logic.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rush_command_core::CommandSemantics;

use crate::dependency::{content_digest, dir_listing_digest, Dependency, FileKind, Staleness};
use crate::probe::{run_probe, stat_snapshot, MetaSnapshot};
use crate::tracked_io::{classify, lookup_option};

/// Domain tag for the manifest fingerprint so it can never collide with an
/// unrelated blake3 hash (or with a per-dependency content/dir digest).
const MANIFEST_DOMAIN: &[u8] = b"rush.incremental.v1/manifest-fingerprint/1\n";

/// The outcome of [`DependencyManifest::validate`].
///
/// Carries both the staleness verdict and the admitted manifest. Every current
/// dependency variant stores only identity that `validate` compares exactly:
/// content and listing digests, complete metadata snapshots, or semantic probe
/// results. A fresh validation therefore returns the original manifest without
/// repeating filesystem reads that can only reconstruct identical values. On
/// `Stale`, `refreshed` is also the original manifest because reuse is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validated {
    pub staleness: Staleness,
    pub refreshed: DependencyManifest,
}

/// The manifest of everything one analysis observed — the daemon's
/// reuse-admission key.
///
/// Holds an ordered, deduped `Vec<Dependency>` in a canonical (sorted) order, so
/// two analyses that made the *same set* of observations in a *different order*
/// produce an identical manifest (and identical [`fingerprint`](Self::fingerprint)).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DependencyManifest {
    deps: Vec<Dependency>,
}

impl DependencyManifest {
    /// Build a canonical manifest from a raw recorded list: dedup identical
    /// observations and sort into a deterministic order.
    ///
    /// Sorting makes the manifest independent of read order; dedup collapses the
    /// repeated identical reads analysis naturally makes (the same header probed
    /// from many rules, say). Note that two *different* observations of the same
    /// path (e.g. a content read and an existence probe) are preserved — they are
    /// distinct dependencies and both must hold for reuse.
    pub(crate) fn from_recorded(mut recorded: Vec<Dependency>) -> Self {
        recorded.sort();
        recorded.dedup();
        Self { deps: recorded }
    }

    /// Construct directly from an already-canonical dep list (e.g. when
    /// deserializing a persisted manifest). Re-canonicalizes to be safe.
    pub fn from_deps(deps: Vec<Dependency>) -> Self {
        Self::from_recorded(deps)
    }

    /// The recorded dependencies, in canonical order.
    pub fn deps(&self) -> &[Dependency] {
        &self.deps
    }

    /// Number of recorded dependencies.
    pub fn len(&self) -> usize {
        self.deps.len()
    }

    /// Whether the manifest recorded no dependencies.
    pub fn is_empty(&self) -> bool {
        self.deps.is_empty()
    }

    /// Re-check every recorded dependency against the current world, returning the
    /// FIRST [`Staleness::Stale`] or [`Staleness::Fresh`] if all still match.
    ///
    /// Filesystem dependencies are re-checked against the real filesystem;
    /// environment and option dependencies are re-checked against the **passed**
    /// [`CommandSemantics`] (never `std::env`), so a daemon validates a manifest
    /// against the *new* command's semantics — which is exactly how a changed
    /// `--define` or env var invalidates a retained graph.
    /// Validate and return both the verdict and the admitted manifest.
    ///
    /// On the first stale dependency this returns `Stale` with the original
    /// manifest as `refreshed` (reuse is rejected; nothing to refresh). When all
    /// dependencies still hold, the original exact observations are returned;
    /// re-reading them would only reproduce values already compared by `recheck`.
    pub fn validate(&self, sem: &CommandSemantics) -> Validated {
        let metadata_shortcuts = fresh_metadata_shortcuts(&self.deps, sem);
        if let Some((dep, reason)) = first_stale(&self.deps, sem, &metadata_shortcuts) {
            return Validated {
                staleness: Staleness::Stale {
                    dep: dep.clone(),
                    reason,
                },
                refreshed: self.clone(),
            };
        }
        Validated {
            staleness: Staleness::Fresh,
            refreshed: self.clone(),
        }
    }

    /// Validate, discarding the refreshed manifest. Convenience for callers (and
    /// tests) that only need the verdict.
    pub fn validate_staleness(&self, sem: &CommandSemantics) -> Staleness {
        self.validate(sem).staleness
    }

    /// A cheap, stable equality key over the canonical dependency list.
    ///
    /// Domain-separated, length-prefixed blake3 — two manifests have the same
    /// fingerprint iff they recorded the same set of observations. Stable across
    /// process restarts (explicit encoding, not `Hash`/`Debug`), so it can key a
    /// persisted analysis cache.
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(MANIFEST_DOMAIN);
        hasher.update(&(self.deps.len() as u64).to_le_bytes());
        for dep in &self.deps {
            encode_dep(&mut hasher, dep);
        }
        *hasher.finalize().as_bytes()
    }
}

/// Find the first stale dependency while preserving process-probe ordering.
///
/// Side-effect-free spans are checked concurrently. A process probe runs only
/// after every preceding observation was proven fresh, matching the original
/// early-return behavior for externally visible probe execution.
fn first_stale<'a>(
    deps: &'a [Dependency],
    sem: &CommandSemantics,
    metadata_shortcuts: &BTreeSet<&Path>,
) -> Option<(&'a Dependency, String)> {
    let mut start = 0usize;
    while start < deps.len() {
        if matches!(deps[start], Dependency::ProcessProbe { .. }) {
            if let Some(reason) = recheck(&deps[start], sem) {
                return Some((&deps[start], reason));
            }
            start += 1;
            continue;
        }
        let end = deps[start..]
            .iter()
            .position(|dep| matches!(dep, Dependency::ProcessProbe { .. }))
            .map_or(deps.len(), |offset| start + offset);
        for (offset, reason) in
            recheck_observations(&deps[start..end], sem, metadata_shortcuts)
            .into_iter()
            .enumerate()
        {
            if let Some(reason) = reason {
                return Some((&deps[start + offset], reason));
            }
        }
        start = end;
    }
    None
}

/// Find content dependencies whose unique companion metadata is unchanged.
///
/// A metadata mismatch merely withholds the shortcut: normal content hashing
/// still decides the earlier content dependency, and the metadata dependency
/// retains its existing stale decision later in canonical order.
fn fresh_metadata_shortcuts<'a>(
    deps: &'a [Dependency],
    sem: &CommandSemantics,
) -> BTreeSet<&'a Path> {
    let mut metadata = BTreeMap::<&Path, (&Dependency, usize)>::new();
    for dep in deps {
        if let Dependency::FileMetadata { path, .. } = dep {
            metadata
                .entry(path.as_path())
                .and_modify(|(_, count)| *count += 1)
                .or_insert((dep, 1));
        }
    }
    metadata
        .into_iter()
        .filter_map(|(path, (dep, count))| {
            (count == 1 && recheck(dep, sem).is_none()).then_some(path)
        })
        .collect()
}

/// Recheck one side-effect-free manifest span with bounded parallelism.
fn recheck_observations(
    deps: &[Dependency],
    sem: &CommandSemantics,
    metadata_shortcuts: &BTreeSet<&Path>,
) -> Vec<Option<String>> {
    const MAX_WORKERS: usize = 8;
    debug_assert!(!deps
        .iter()
        .any(|dep| matches!(dep, Dependency::ProcessProbe { .. })));
    if deps.len() < 16 {
        return deps
            .iter()
            .map(|dep| recheck_observation(dep, sem, metadata_shortcuts))
            .collect();
    }

    let workers = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .min(MAX_WORKERS)
        .min(deps.len());
    if workers == 1 {
        return deps
            .iter()
            .map(|dep| recheck_observation(dep, sem, metadata_shortcuts))
            .collect();
    }
    let chunk_size = deps.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let checks = deps
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|dep| recheck_observation(dep, sem, metadata_shortcuts))
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        checks
            .into_iter()
            .flat_map(|check| check.join().expect("manifest recheck worker panicked"))
            .collect()
    })
}

fn recheck_observation(
    dep: &Dependency,
    sem: &CommandSemantics,
    metadata_shortcuts: &BTreeSet<&Path>,
) -> Option<String> {
    if let Dependency::FileContent { path, .. } = dep {
        if metadata_shortcuts.contains(path.as_path()) {
            return None;
        }
    }
    recheck(dep, sem)
}

/// Re-check one dependency against the current world.
///
/// Returns `Some(reason)` if it no longer matches (stale), `None` if it still
/// holds (fresh). Env / Opt are checked against `sem`; everything else against
/// the real filesystem.
fn recheck(dep: &Dependency, sem: &CommandSemantics) -> Option<String> {
    match dep {
        Dependency::FileContent { path, digest } => {
            // Re-hash current contents. The manifest-level caller may skip this
            // read only when a unique companion FileMetadata dependency matches
            // every recorded field exactly. Without that witness, correctness is
            // the content digest and unreadable bytes are stale.
            match fs::read(path) {
                Ok(bytes) => {
                    let current = content_digest(&bytes);
                    if &current != digest {
                        Some(format!("file content changed: {}", path.display()))
                    } else {
                        None
                    }
                }
                Err(e) => Some(format!(
                    "recorded file no longer readable: {} ({e})",
                    path.display()
                )),
            }
        }
        Dependency::FileExistence {
            path,
            exists,
            kind,
        } => {
            // Re-stat without following the link node (matches the recording side,
            // which used symlink_metadata). Compare both presence and kind.
            match fs::symlink_metadata(path) {
                Ok(meta) => {
                    if !*exists {
                        return Some(format!(
                            "path recorded absent now exists: {}",
                            path.display()
                        ));
                    }
                    let current_kind = classify(&meta);
                    if current_kind != *kind {
                        Some(format!(
                            "path kind changed ({:?} -> {:?}): {}",
                            kind,
                            current_kind,
                            path.display()
                        ))
                    } else {
                        None
                    }
                }
                Err(_) => {
                    if *exists {
                        Some(format!(
                            "recorded path no longer exists: {}",
                            path.display()
                        ))
                    } else {
                        // recorded absent, still absent → fresh
                        None
                    }
                }
            }
        }
        Dependency::Symlink { path, target } => {
            let current = fs::read_link(path).ok();
            if &current != target {
                Some(format!("symlink target changed: {}", path.display()))
            } else {
                None
            }
        }
        Dependency::DirListing {
            path,
            entries_digest,
        } => match fs::read_dir(path) {
            Ok(rd) => {
                let mut names: Vec<Vec<u8>> = Vec::new();
                for entry in rd {
                    match entry {
                        Ok(e) => names.push(os_string_bytes(&e.file_name())),
                        Err(e) => {
                            return Some(format!(
                                "directory listing failed mid-read: {} ({e})",
                                path.display()
                            ))
                        }
                    }
                }
                let current = dir_listing_digest(names);
                if &current != entries_digest {
                    Some(format!("directory listing changed: {}", path.display()))
                } else {
                    None
                }
            }
            Err(e) => Some(format!(
                "recorded directory no longer listable: {} ({e})",
                path.display()
            )),
        },
        Dependency::Env { name, value } => {
            let current = sem.getenv(name).map(|v| v.to_vec());
            if &current != value {
                Some(format!(
                    "environment variable changed: {}",
                    String::from_utf8_lossy(name)
                ))
            } else {
                None
            }
        }
        Dependency::Opt { key, value } => {
            let current = lookup_option(sem, key);
            if &current != value {
                Some(format!("build option changed: {key}"))
            } else {
                None
            }
        }
        Dependency::ConfigFingerprint { digest } => {
            let current = sem.config_fingerprint();
            if current != digest {
                Some("per-command configuration changed".to_string())
            } else {
                None
            }
        }
        Dependency::FileMetadata {
            path,
            present,
            len,
            mode,
            mtime_ns,
            ctime_ns,
            dev,
            ino,
        } => {
            let recorded = MetaSnapshot {
                present: *present,
                len: *len,
                mode: *mode,
                mtime_ns: *mtime_ns,
                ctime_ns: *ctime_ns,
                dev: *dev,
                ino: *ino,
            };
            let current = stat_snapshot(path);
            if current != recorded {
                Some(format!("file metadata changed: {}", path.display()))
            } else {
                None
            }
        }
        Dependency::ProcessProbe {
            program,
            args,
            env_keys,
            cwd,
            status,
            stdout_digest,
            stderr_digest,
            compare_stderr,
        } => {
            match run_probe(program, args, env_keys, cwd, sem) {
                Ok(out) => {
                    if out.status != *status {
                        Some(format!(
                            "process probe exit status changed ({status} -> {}): {}",
                            out.status,
                            program.display()
                        ))
                    } else if &out.stdout_digest != stdout_digest {
                        Some(format!("process probe stdout changed: {}", program.display()))
                    } else if *compare_stderr && &out.stderr_digest != stderr_digest {
                        Some(format!("process probe stderr changed: {}", program.display()))
                    } else {
                        None
                    }
                }
                Err(e) => Some(format!(
                    "process probe no longer runnable: {} ({e})",
                    program.display()
                )),
            }
        }
        Dependency::LinuxProcFilesystems { bytes, digest } => {
            match read_linux_proc_filesystems() {
                Ok(current) => {
                    let current_bytes = u64::try_from(current.len()).ok();
                    let current_digest = *blake3::hash(&current).as_bytes();
                    if current_bytes != Some(*bytes) || &current_digest != digest {
                        Some("Linux filesystem registry changed".to_string())
                    } else {
                        None
                    }
                }
                Err(e) => Some(format!("Linux filesystem registry is not readable ({e})")),
            }
        }
        Dependency::LinuxProcVmOvercommitMemory { value } => {
            match read_linux_proc_vm_overcommit_memory() {
                Ok(current) if current == *value => None,
                Ok(_) => Some("Linux virtual-memory overcommit policy changed".to_string()),
                Err(e) => Some(format!(
                    "Linux virtual-memory overcommit policy is not readable ({e})"
                )),
            }
        }
        Dependency::LinuxCgroupV2CpuMax { values } => {
            match read_linux_cgroup_v2_cpu_max_values() {
                Ok(current) if &current == values => None,
                Ok(_) => Some("Linux cgroup-v2 CPU quota hierarchy changed".to_string()),
                Err(e) => Some(format!("Linux cgroup-v2 CPU quota is not readable ({e})")),
            }
        }
    }
}

/// Encode one dependency into the fingerprint hasher, length-prefixed and
/// tagged per variant so no two variants (or values) can collide.
fn encode_dep(hasher: &mut blake3::Hasher, dep: &Dependency) {
    match dep {
        Dependency::FileContent { path, digest } => {
            hasher.update(&[0u8]);
            update_lp(hasher, path_bytes(path).as_slice());
            hasher.update(digest);
        }
        Dependency::FileExistence {
            path,
            exists,
            kind,
        } => {
            hasher.update(&[1u8]);
            update_lp(hasher, path_bytes(path).as_slice());
            hasher.update(&[*exists as u8]);
            hasher.update(&[kind.map(FileKind::tag).unwrap_or(0)]);
        }
        Dependency::Symlink { path, target } => {
            hasher.update(&[2u8]);
            update_lp(hasher, path_bytes(path).as_slice());
            match target {
                Some(t) => {
                    hasher.update(&[1u8]);
                    update_lp(hasher, path_bytes(t).as_slice());
                }
                None => {
                    hasher.update(&[0u8]);
                }
            }
        }
        Dependency::DirListing {
            path,
            entries_digest,
        } => {
            hasher.update(&[3u8]);
            update_lp(hasher, path_bytes(path).as_slice());
            hasher.update(entries_digest);
        }
        Dependency::Env { name, value } => {
            hasher.update(&[4u8]);
            update_lp(hasher, name);
            match value {
                Some(v) => {
                    hasher.update(&[1u8]);
                    update_lp(hasher, v);
                }
                None => {
                    hasher.update(&[0u8]);
                }
            }
        }
        Dependency::Opt { key, value } => {
            hasher.update(&[5u8]);
            update_lp(hasher, key.as_bytes());
            match value {
                Some(v) => {
                    hasher.update(&[1u8]);
                    update_lp(hasher, v.as_bytes());
                }
                None => {
                    hasher.update(&[0u8]);
                }
            }
        }
        Dependency::ConfigFingerprint { digest } => {
            hasher.update(&[6u8]);
            hasher.update(digest);
        }
        Dependency::FileMetadata {
            path,
            present,
            len,
            mode,
            mtime_ns,
            ctime_ns,
            dev,
            ino,
        } => {
            hasher.update(&[7u8]);
            update_lp(hasher, path_bytes(path).as_slice());
            hasher.update(&[*present as u8]);
            hasher.update(&len.to_le_bytes());
            hasher.update(&mode.to_le_bytes());
            hasher.update(&mtime_ns.to_le_bytes());
            hasher.update(&ctime_ns.to_le_bytes());
            hasher.update(&dev.to_le_bytes());
            hasher.update(&ino.to_le_bytes());
        }
        Dependency::ProcessProbe {
            program,
            args,
            env_keys,
            cwd,
            status,
            stdout_digest,
            stderr_digest,
            compare_stderr,
        } => {
            hasher.update(&[8u8]);
            update_lp(hasher, path_bytes(program).as_slice());
            hasher.update(&(args.len() as u64).to_le_bytes());
            for a in args {
                update_lp(hasher, a);
            }
            hasher.update(&(env_keys.len() as u64).to_le_bytes());
            for k in env_keys {
                update_lp(hasher, k);
            }
            update_lp(hasher, path_bytes(cwd).as_slice());
            hasher.update(&status.to_le_bytes());
            hasher.update(stdout_digest);
            hasher.update(stderr_digest);
            hasher.update(&[*compare_stderr as u8]);
        }
        Dependency::LinuxProcFilesystems { bytes, digest } => {
            hasher.update(&[9u8]);
            hasher.update(&bytes.to_le_bytes());
            hasher.update(digest);
        }
        Dependency::LinuxProcVmOvercommitMemory { value } => {
            hasher.update(&[10u8]);
            hasher.update(&[*value]);
        }
        Dependency::LinuxCgroupV2CpuMax { values } => {
            hasher.update(&[11u8]);
            hasher.update(&(values.len() as u64).to_le_bytes());
            for (bytes, digest) in values {
                hasher.update(&bytes.to_le_bytes());
                hasher.update(digest);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn read_linux_proc_filesystems() -> std::io::Result<Vec<u8>> {
    fs::read("/proc/filesystems")
}

#[cfg(target_os = "linux")]
fn read_linux_proc_vm_overcommit_memory() -> std::io::Result<u8> {
    let bytes = fs::read("/proc/sys/vm/overcommit_memory")?;
    match bytes.as_slice() {
        [value @ b'0'..=b'2', b'\n'] => Ok(*value),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected Linux overcommit policy value",
        )),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn read_linux_cgroup_v2_cpu_max_values(
) -> std::io::Result<Vec<(u64, [u8; 32])>> {
    let membership = fs::read("/proc/self/cgroup")?;
    let mut unified_path = None;
    for line in membership
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let mut fields = line.splitn(3, |byte| *byte == b':');
        let hierarchy = fields.next();
        let controllers = fields.next();
        let path = fields.next();
        if hierarchy != Some(b"0".as_slice()) || controllers != Some(b"".as_slice()) {
            continue;
        }
        let path = path.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "malformed Linux cgroup-v2 membership",
            )
        })?;
        if unified_path.replace(path).is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "multiple Linux cgroup-v2 memberships",
            ));
        }
    }
    let relative = unified_path.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Linux cgroup-v2 membership is unavailable",
        )
    })?;
    let suffix = relative.strip_prefix(b"/").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Linux cgroup-v2 membership is not absolute",
        )
    })?;
    let root = PathBuf::from("/sys/fs/cgroup");
    let mut path = root.clone();
    if !suffix.is_empty() {
        for component in suffix.split(|byte| *byte == b'/') {
            if component.is_empty()
                || component == b"."
                || component == b".."
                || component.contains(&0)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid Linux cgroup-v2 membership path",
                ));
            }
            path.push(OsStr::from_bytes(component));
        }
    }
    let mut values = BTreeSet::new();
    while path != root {
        let current = fs::read(path.join("cpu.max"))?;
        let bytes = u64::try_from(current.len()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Linux cgroup-v2 CPU quota is too large",
            )
        })?;
        values.insert((bytes, *blake3::hash(&current).as_bytes()));
        if !path.pop() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Linux cgroup-v2 membership escaped its mount",
            ));
        }
    }
    if values.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Linux cgroup-v2 CPU quota hierarchy is empty",
        ));
    }
    Ok(values.into_iter().collect())
}

#[cfg(not(target_os = "linux"))]
fn read_linux_proc_filesystems() -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Linux procfs is unavailable",
    ))
}

#[cfg(not(target_os = "linux"))]
fn read_linux_proc_vm_overcommit_memory() -> std::io::Result<u8> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Linux procfs is unavailable",
    ))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_linux_cgroup_v2_cpu_max_values(
) -> std::io::Result<Vec<(u64, [u8; 32])>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Linux cgroup-v2 is unavailable",
    ))
}

fn update_lp(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Lossless bytes for a path (raw on Unix, lossy UTF-8 elsewhere).
fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().into_owned().into_bytes()
    }
}

/// Lossless bytes for an `OsString` (raw on Unix, lossy elsewhere). Kept in sync
/// with the recording-side helper so the dir-listing digest matches.
fn os_string_bytes(s: &std::ffi::OsString) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        s.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        s.to_string_lossy().into_owned().into_bytes()
    }
}
