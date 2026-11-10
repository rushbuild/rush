//! The [`DependencyManifest`] — the ordered, deduped, deterministic record of
//! everything one analysis observed, plus its validation and fingerprint logic.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rush_command_core::CommandSemantics;

use crate::dependency::{content_digest, dir_listing_digest, Dependency, FileKind, Staleness};
use crate::probe::{run_probe, stat_snapshot, MetaSnapshot};
use crate::tracked_io::{classify, lookup_option};

/// Domain tag for the manifest fingerprint so it can never collide with an
/// unrelated blake3 hash (or with a per-dependency content/dir digest).
// V2 distinguishes descriptor-qualified content reuse from the legacy stat-only
// contract. Restart resident daemons on upgrade. Proof payload/wall-clock times
// are serialized for validation, but are not semantic build inputs.
const MANIFEST_DOMAIN: &[u8] = b"rush.incremental.v2/manifest-fingerprint/2\n";

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
/// two analyses that made the same semantic observations produce an equal
/// manifest and fingerprint. Descriptor-read proof payload is serialized but
/// excluded from semantic equality/identity; missing proof wins conservatively.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DependencyManifest {
    #[serde(deserialize_with = "deserialize_manifest_deps")]
    deps: Vec<Dependency>,
}

impl PartialEq for DependencyManifest {
    fn eq(&self, other: &Self) -> bool {
        self.deps.len() == other.deps.len()
            && self
                .deps
                .iter()
                .zip(&other.deps)
                .all(|(a, b)| match (a, b) {
                    (
                        Dependency::FileContent {
                            path: a,
                            digest: ad,
                            ..
                        },
                        Dependency::FileContent {
                            path: b,
                            digest: bd,
                            ..
                        },
                    ) => a == b && ad == bd,
                    _ => a == b,
                })
    }
}
impl Eq for DependencyManifest {}

fn deserialize_manifest_deps<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Dependency>, D::Error> {
    let deps = <Vec<Dependency> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(canonicalize_content_observations(deps))
}

// Coalesce only identical content values within side-effect-free spans. Any
// missing/invalid/unqualified proof wins conservatively; never create a binding
// by combining an ordinary content read with a separate metadata observation.
fn canonicalize_content_observations(deps: Vec<Dependency>) -> Vec<Dependency> {
    fn span(deps: Vec<Dependency>) -> Vec<Dependency> {
        let mut content = BTreeMap::<(PathBuf, [u8; 32]), Option<crate::FileContentBinding>>::new();
        for dep in &deps {
            if let Dependency::FileContent {
                path,
                digest,
                binding,
            } = dep
            {
                let binding = binding
                    .as_ref()
                    .filter(|binding| {
                        binding.matches_content(path, digest) && binding.historically_qualified()
                    })
                    .cloned();
                content
                    .entry((path.clone(), *digest))
                    .and_modify(|existing| {
                        *existing = match (existing.as_ref(), binding.as_ref()) {
                            (Some(a), Some(b)) if a.same_bound_value(b) => Some(
                                if (a.read_start_ns, a.read_end_ns)
                                    <= (b.read_start_ns, b.read_end_ns)
                                {
                                    a.clone()
                                } else {
                                    b.clone()
                                },
                            ),
                            _ => None,
                        };
                    })
                    .or_insert(binding);
            }
        }
        deps.into_iter()
            .filter_map(|dep| match dep {
                Dependency::FileContent { path, digest, .. } => content
                    .remove(&(path.clone(), digest))
                    .map(|binding| Dependency::FileContent {
                        path,
                        digest,
                        binding,
                    }),
                other => Some(other),
            })
            .collect()
    }
    let mut result = Vec::new();
    let mut pending = Vec::new();
    for dep in deps {
        if matches!(dep, Dependency::ProcessProbe { .. }) {
            result.extend(span(std::mem::take(&mut pending)));
            result.push(dep);
        } else {
            pending.push(dep);
        }
    }
    result.extend(span(pending));
    result
}

impl DependencyManifest {
    /// Canonicalize native borrowed observations without retaining every
    /// read-time proof variant in scratch storage. The raw ledger is untouched.
    /// Native construction sorts globally, placing every FileContent before
    /// ProcessProbe; wire deserialization still uses its separate span reducer.
    pub(crate) fn from_recorded_refs<'a>(
        recorded: impl IntoIterator<Item = &'a Dependency>,
    ) -> Self {
        // Keep the minimum full-Ord representative, including first-seen ties:
        // equivalent Path spellings can serialize differently even though Ord
        // treats them as equal. This matches the original stable owned sort.
        let mut content = BTreeMap::<
            (&'a Path, [u8; 32]),
            (&'a Dependency, Option<&'a crate::FileContentBinding>),
        >::new();
        let mut other = BTreeSet::new();
        for dep in recorded {
            if let Dependency::FileContent {
                path,
                digest,
                binding,
            } = dep
            {
                let binding = binding.as_ref().filter(|binding| {
                    binding.matches_content(path, digest) && binding.historically_qualified()
                });
                content
                    .entry((path.as_path(), *digest))
                    .and_modify(|(representative, existing)| {
                        if dep < *representative {
                            *representative = dep;
                        }
                        *existing = match (*existing, binding) {
                            (Some(a), Some(b)) if a.same_bound_value(b) => Some(
                                if (a.read_start_ns, a.read_end_ns)
                                    <= (b.read_start_ns, b.read_end_ns)
                                {
                                    a
                                } else {
                                    b
                                },
                            ),
                            _ => None,
                        };
                    })
                    .or_insert((dep, binding));
            } else {
                other.insert(dep);
            }
        }
        let mut reduced: Vec<_> = content
            .into_values()
            .map(|(representative, binding)| {
                let Dependency::FileContent { path, digest, .. } = representative else {
                    unreachable!()
                };
                Dependency::FileContent {
                    path: path.clone(),
                    digest: *digest,
                    binding: binding.cloned(),
                }
            })
            .collect();
        reduced.extend(other.into_iter().cloned());
        // The existing owned normalizer remains the final authority. Its input
        // is now bounded by distinct values/facts, not distinct proof clocks.
        Self::from_recorded(reduced)
    }

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
        // Preserve the existing native canonical sort before coalescing proof
        // variants, so intervening raw probes cannot change wire multiplicity.
        let mut recorded = canonicalize_content_observations(recorded);
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
    /// [`Staleness::Stale`] or [`Staleness::Fresh`] if all still match.
    ///
    /// Filesystem dependencies are re-checked against the real filesystem;
    /// environment and option dependencies are re-checked against the **passed**
    /// [`CommandSemantics`] (never `std::env`), so a daemon validates a manifest
    /// against the *new* command's semantics — which is exactly how a changed
    /// `--define` or env var invalidates a retained graph.
    /// Validate and return both the verdict and the admitted manifest.
    ///
    /// Within a side-effect-free span, stale full metadata takes precedence
    /// over content reads. Process-probe boundaries retain their ordering.
    /// On rejection this returns `Stale` with the original
    /// manifest as `refreshed` (reuse is rejected; nothing to refresh). When all
    /// dependencies still hold, the original exact observations are returned;
    /// re-reading them would only reproduce values already compared by `recheck`.
    pub fn validate(&self, sem: &CommandSemantics) -> Validated {
        if let Some((dep, reason)) = first_stale(&self.deps, sem) {
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

/// Find a stale dependency while preserving process-probe ordering.
///
/// Side-effect-free spans are checked concurrently. A process probe runs only
/// after every preceding observation was proven fresh, matching the original
/// early-return behavior for externally visible probe execution.
fn first_stale<'a>(
    deps: &'a [Dependency],
    sem: &CommandSemantics,
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
        // Metadata rejection takes diagnostic precedence within this
        // side-effect-free span. In particular, a known kind/identity change
        // must not trigger a content read of a FIFO before rejecting reuse.
        // Never move this preflight across the preceding process probe.
        let metadata_shortcuts = match preflight_metadata(&deps[start..end], sem) {
            Ok(shortcuts) => shortcuts,
            Err(stale) => return Some(stale),
        };
        for (offset, reason) in recheck_observations(&deps[start..end], sem, &metadata_shortcuts)
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
/// Check each full metadata observation once before this span's content reads.
/// A mismatch already rejects the manifest: returning that actual metadata
/// dependency avoids an unnecessary, potentially blocking content re-read.
/// Matching unique companions retain the existing content early cutoff.
fn preflight_metadata<'a>(
    deps: &'a [Dependency],
    sem: &CommandSemantics,
) -> Result<BTreeSet<&'a Path>, (&'a Dependency, String)> {
    preflight_metadata_at(deps, sem, SystemTime::now())
}

fn preflight_metadata_at<'a>(
    deps: &'a [Dependency],
    sem: &CommandSemantics,
    now: SystemTime,
) -> Result<BTreeSet<&'a Path>, (&'a Dependency, String)> {
    let mut metadata = BTreeMap::<&Path, (&Dependency, usize)>::new();
    let mut contents = BTreeMap::<&Path, Vec<&Dependency>>::new();
    for dep in deps {
        if let Dependency::FileContent { path, .. } = dep {
            contents.entry(path).or_default().push(dep);
        }
        if let Dependency::FileMetadata { path, .. } = dep {
            if let Some(reason) = recheck(dep, sem) {
                return Err((dep, reason));
            }
            metadata
                .entry(path.as_path())
                .and_modify(|(_, count)| *count += 1)
                .or_insert((dep, 1));
        }
    }
    Ok(metadata
        .into_iter()
        .filter_map(|(path, (dep, count))| {
            let content = contents.get(path)?;
            let [Dependency::FileContent {
                path: content_path,
                digest,
                binding: Some(binding),
            }] = content.as_slice()
            else {
                return None;
            };
            (count == 1
                && content_shortcut_metadata_eligible(dep, now)
                && binding.matches_content(content_path, digest)
                && binding.matches_metadata(dep)
                && binding.qualifies_at(now))
            .then_some(path)
        })
        .collect())
}

fn content_shortcut_metadata_eligible(dep: &Dependency, now: SystemTime) -> bool {
    #[cfg(unix)]
    {
        let Dependency::FileMetadata {
            present: true,
            mode,
            mtime_ns,
            ctime_ns,
            ..
        } = dep
        else {
            return false;
        };
        let Ok(now) = now.duration_since(UNIX_EPOCH) else {
            return false;
        };
        let Ok(now) = i128::try_from(now.as_nanos()) else {
            return false;
        };
        // Symlink-node metadata cannot witness its followed target's content.
        // Unknown, future and recent/coarse timestamps require actual hashing.
        mode & libc::S_IFMT as u32 == libc::S_IFREG as u32
            && *mtime_ns > 0
            && *ctime_ns > 0
            && now - mtime_ns >= 2_000_000_000
            && now - ctime_ns >= 2_000_000_000
    }
    #[cfg(not(unix))]
    {
        let _ = (dep, now);
        false
    }
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
    if matches!(dep, Dependency::FileMetadata { .. }) {
        // This exact observation was checked by the span's preflight. Reuse
        // the same outcome that admitted the content shortcut, not a second
        // observation that could reopen the unsafe content fallback path.
        return None;
    }
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
        Dependency::FileContent { path, digest, .. } => {
            // Re-hash current contents. The manifest-level caller may skip this
            // read only when a unique companion FileMetadata dependency matches
            // every recorded field exactly. Without that witness, correctness is
            // the content digest and unreadable bytes are stale.
            match validation_content_digest(path) {
                Ok(current) => {
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

#[cfg(unix)]
fn open_content_for_validation(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    // Follow final symlinks intentionally: FileContent records the bytes read
    // through them. A FIFO target must not wait for a writer before fstat.
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(unix)]
fn validation_content_digest_with_open(
    path: &Path,
    open: impl FnOnce(&Path) -> std::io::Result<fs::File>,
) -> std::io::Result<[u8; 32]> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let signature = |md: &fs::Metadata| {
        (
            md.dev(),
            md.ino(),
            md.mode(),
            md.len(),
            md.mtime(),
            md.mtime_nsec(),
            md.ctime(),
            md.ctime_nsec(),
        )
    };
    let mut file = open(path)?;
    let before = file.metadata()?;
    if !before.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "content validation requires a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if !after.is_file() || signature(&before) != signature(&after) {
        return Err(std::io::Error::other(
            "file changed during content validation",
        ));
    }
    Ok(content_digest(&bytes))
}

fn validation_content_digest(path: &Path) -> std::io::Result<[u8; 32]> {
    #[cfg(unix)]
    {
        validation_content_digest_with_open(path, open_content_for_validation)
    }
    #[cfg(not(unix))]
    {
        fs::read(path).map(|bytes| content_digest(&bytes))
    }
}

/// Encode one dependency into the fingerprint hasher, length-prefixed and
/// tagged per variant so no two variants (or values) can collide.
fn encode_dep(hasher: &mut blake3::Hasher, dep: &Dependency) {
    match dep {
        Dependency::FileContent { path, digest, .. } => {
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

#[cfg(test)]
mod metadata_preflight_tests {
    use super::*;
    use crate::{RealTrackedIo, TrackedIo};
    use bytes::Bytes;
    use rush_protocol::command::v1::RunRequest;

    fn semantics() -> CommandSemantics {
        CommandSemantics::from_run_request(&RunRequest {
            argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"build")],
            protocol_major: rush_protocol::PROTOCOL_MAJOR,
            protocol_minor: rush_protocol::PROTOCOL_MINOR,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn borrowed_recordings_preserve_exact_canonical_wire_and_fingerprint() {
        let path = PathBuf::from("/snapshot/header.h");
        let digest = content_digest(b"header");
        let proof = crate::FileContentBinding {
            version: 1,
            path: path.clone(),
            digest,
            len: 6,
            mode: 0o100644,
            dev: 1,
            ino: 2,
            mtime_ns: 1,
            ctime_ns: 1,
            read_start_ns: 3_000_000_001,
            read_end_ns: 3_000_000_002,
        };
        let mut later = proof.clone();
        later.read_start_ns += 10;
        later.read_end_ns += 10;
        let mut other_inode = proof.clone();
        other_inode.ino += 1;
        let mut invalid = proof.clone();
        invalid.version = 99;
        let mut fresh = proof.clone();
        fresh.read_start_ns = 1;
        fresh.read_end_ns = 2;
        let mut wrong_value = proof.clone();
        wrong_value.digest = [9; 32];
        let content = |binding| Dependency::FileContent {
            path: path.clone(),
            digest,
            binding,
        };
        let probe = Dependency::ProcessProbe {
            program: PathBuf::from("/bin/tool"),
            args: vec![vec![0xff, b'a']],
            env_keys: vec![b"PATH".to_vec()],
            cwd: PathBuf::from("/snapshot"),
            status: 0,
            stdout_digest: [3; 32],
            stderr_digest: [4; 32],
            compare_stderr: true,
        };
        let mut other_probe = probe.clone();
        if let Dependency::ProcessProbe {
            status,
            compare_stderr,
            ..
        } = &mut other_probe
        {
            *status = 1;
            *compare_stderr = false;
        }
        let common = vec![
            Dependency::FileContent {
                path: path.clone(),
                digest: [8; 32],
                binding: None,
            },
            Dependency::FileExistence {
                path: path.clone(),
                exists: true,
                kind: Some(FileKind::File),
            },
            Dependency::FileExistence {
                path: path.clone(),
                exists: false,
                kind: None,
            },
            Dependency::Symlink {
                path: path.clone(),
                target: Some(PathBuf::from("other")),
            },
            Dependency::Symlink {
                path: path.clone(),
                target: None,
            },
            Dependency::DirListing {
                path: path.clone(),
                entries_digest: [1; 32],
            },
            Dependency::Env {
                name: vec![0xff],
                value: Some(vec![0xfe]),
            },
            Dependency::Env {
                name: vec![0xff],
                value: None,
            },
            Dependency::Opt {
                key: "mode".into(),
                value: Some("a".into()),
            },
            Dependency::Opt {
                key: "mode".into(),
                value: Some("b".into()),
            },
            Dependency::ConfigFingerprint { digest: [2; 32] },
            Dependency::FileMetadata {
                path: path.clone(),
                present: true,
                len: 6,
                mode: 0o100644,
                mtime_ns: 1,
                ctime_ns: 1,
                dev: 1,
                ino: 2,
            },
            Dependency::FileMetadata {
                path: path.clone(),
                present: true,
                len: 7,
                mode: 0o100644,
                mtime_ns: 2,
                ctime_ns: 2,
                dev: 1,
                ino: 3,
            },
            probe,
            other_probe,
            Dependency::LinuxProcFilesystems {
                bytes: 9,
                digest: [5; 32],
            },
            Dependency::LinuxProcVmOvercommitMemory { value: 0 },
            Dependency::LinuxProcVmOvercommitMemory { value: 1 },
            Dependency::LinuxCgroupV2CpuMax {
                values: vec![(9, [6; 32])],
            },
            // Path ordering normalizes these spellings; their wire bytes differ.
            Dependency::FileExistence {
                path: PathBuf::from("/snapshot/a//b"),
                exists: false,
                kind: None,
            },
            Dependency::FileExistence {
                path: PathBuf::from("/snapshot/a/b"),
                exists: false,
                kind: None,
            },
        ];
        for bindings in [
            vec![None],
            vec![Some(proof.clone())],
            vec![Some(proof.clone()), Some(later)],
            vec![Some(proof.clone()), None],
            vec![Some(proof.clone()), Some(other_inode)],
            vec![Some(proof.clone()), Some(invalid)],
            vec![Some(proof.clone()), Some(fresh)],
            vec![Some(proof), Some(wrong_value)],
        ] {
            let mut values = common.clone();
            values.extend(bindings.into_iter().map(content));
            for rotation in 0..values.len() {
                values.rotate_left(1);
                let mut raw = Vec::new();
                for (index, dep) in values.iter().enumerate() {
                    raw.extend(std::iter::repeat(dep.clone()).take(1 + (index + rotation) % 7));
                }
                if rotation % 2 == 0 {
                    raw.reverse();
                }
                let expected = DependencyManifest::from_recorded(raw.clone());
                let actual = DependencyManifest::from_recorded_refs(raw.iter());
                assert_eq!(
                    serde_json::to_vec(&actual).unwrap(),
                    serde_json::to_vec(&expected).unwrap(),
                    "proof payload and probe multiplicity must match the owned path"
                );
                assert_eq!(actual.fingerprint(), expected.fingerprint());
            }
        }
        let empty = DependencyManifest::from_recorded_refs(std::iter::empty());
        assert!(empty.is_empty());
    }

    #[test]
    fn borrowed_native_proof_reduction_preserves_full_wire_path_representatives() {
        let digest = content_digest(b"same");
        let proof = crate::FileContentBinding {
            version: 1,
            path: PathBuf::from("/snapshot/header.h"),
            digest,
            len: 4,
            mode: 0o100644,
            dev: 1,
            ino: 2,
            mtime_ns: 1,
            ctime_ns: 1,
            read_start_ns: 3_000_000_001,
            read_end_ns: 3_000_000_002,
        };
        let mut late = proof.clone();
        late.read_start_ns += 10;
        late.read_end_ns += 10;
        let mut other_inode = proof.clone();
        other_inode.ino += 1;
        let mut invalid = proof.clone();
        invalid.version = 99;
        let mut fresh = proof.clone();
        fresh.read_start_ns = 1;
        let mut wrong_value = proof.clone();
        wrong_value.digest = [7; 32];
        let variants = [
            None,
            Some(proof.clone()),
            Some(late),
            Some(other_inode),
            Some(invalid),
            Some(fresh),
            Some(wrong_value),
        ];
        for a in &variants {
            for b in &variants {
                let mut same_repr = proof.clone();
                same_repr.path = PathBuf::from("/snapshot//header.h");
                let mut raw = vec![
                    Dependency::FileContent {
                        path: PathBuf::from("/snapshot/header.h"),
                        digest,
                        binding: a.clone(),
                    },
                    Dependency::ProcessProbe {
                        program: PathBuf::from("/tool"),
                        args: vec![],
                        env_keys: vec![],
                        cwd: PathBuf::from("/snapshot"),
                        status: 0,
                        stdout_digest: [1; 32],
                        stderr_digest: [0; 32],
                        compare_stderr: false,
                    },
                    Dependency::FileContent {
                        path: PathBuf::from("/snapshot//header.h"),
                        digest,
                        binding: b.clone(),
                    },
                    Dependency::FileContent {
                        path: PathBuf::from("/snapshot/./header.h"),
                        digest,
                        binding: Some(same_repr),
                    },
                    Dependency::FileContent {
                        path: PathBuf::from("/snapshot/header.h"),
                        digest: [8; 32],
                        binding: None,
                    },
                ];
                for _ in 0..raw.len() {
                    raw.rotate_left(1);
                    for values in [raw.clone(), raw.iter().rev().cloned().collect()] {
                        let expected = DependencyManifest::from_recorded(values.clone());
                        let actual = DependencyManifest::from_recorded_refs(values.iter());
                        assert_eq!(
                            serde_json::to_vec(&actual).unwrap(),
                            serde_json::to_vec(&expected).unwrap(),
                            "raw path spelling and complete binding, not just semantic Eq"
                        );
                        assert_eq!(actual.fingerprint(), expected.fingerprint());
                    }
                }
            }
        }
    }

    #[test]
    fn borrowed_manifest_keeps_raw_marks_and_conflicting_reads() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("header.h");
        fs::write(&file, b"first").unwrap();
        let io = RealTrackedIo::new(semantics(), dir.path());
        io.read_file(&file).unwrap();
        let prefix = io.captured_since(0);
        let mark = io.mark();
        for _ in 0..4096 {
            io.record_deps(&prefix);
        }
        let raw = io.captured_since(mark);
        let captured_manifest = io.captured_manifest_since(mark);
        assert_eq!(
            serde_json::to_vec(&captured_manifest).unwrap(),
            serde_json::to_vec(&DependencyManifest::from_deps(raw.clone())).unwrap()
        );
        assert_eq!(io.captured_since(mark), raw);
        let first = io.manifest();
        assert_eq!(first.len(), prefix.len());
        assert_eq!(io.mark(), mark + raw.len());
        assert_eq!(io.captured_since(mark), raw);
        assert_eq!(first.validate_staleness(&semantics()), Staleness::Fresh);
        fs::write(&file, b"second").unwrap();
        io.read_file(&file).unwrap();
        let both = io.manifest();
        assert_eq!(both.len(), prefix.len() + 1);
        assert_eq!(
            both.deps()
                .iter()
                .filter(|dep| matches!(dep, Dependency::FileContent { .. }))
                .count(),
            2,
            "contradictory values and their kind observation must survive"
        );
        assert!(matches!(
            both.validate_staleness(&semantics()),
            Staleness::Stale { .. }
        ));
        assert_eq!(io.captured_since(0)[..mark], prefix);
    }

    #[test]
    fn metadata_preflight_retains_unique_companion_requirement() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tool");
        fs::write(&file, b"contents").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2100));
        let sem = semantics();
        let io = RealTrackedIo::new(sem.clone(), dir.path());
        io.read_canonical_regular_file(&file).unwrap().unwrap();
        let manifest = io.manifest();
        let metadata = manifest
            .deps()
            .iter()
            .find(|dep| matches!(dep, Dependency::FileMetadata { .. }))
            .unwrap()
            .clone();
        let content = manifest
            .deps()
            .iter()
            .find(|dep| matches!(dep, Dependency::FileContent { .. }))
            .unwrap()
            .clone();
        let later = SystemTime::now() + std::time::Duration::from_secs(10);
        let unique = vec![content.clone(), metadata.clone()];
        assert!(preflight_metadata_at(&unique, &sem, later)
            .unwrap()
            .contains(file.as_path()));
        let duplicate = vec![content, metadata.clone(), metadata];
        assert!(
            preflight_metadata_at(&duplicate, &sem, later)
                .unwrap()
                .is_empty(),
            "duplicate metadata must not gain a new content shortcut"
        );
    }

    #[test]
    #[cfg(unix)]
    fn metadata_preflight_fifo_rejects_before_content_and_preserves_probe_boundaries() {
        use std::os::unix::ffi::OsStrExt;
        const CHILD_ROOT: &str = "RUSH_MANIFEST_METADATA_FIFO_CHILD";
        let Some(root) = std::env::var_os(CHILD_ROOT) else {
            let temp = tempfile::tempdir().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "manifest::metadata_preflight_tests::metadata_preflight_fifo_rejects_before_content_and_preserves_probe_boundaries", "--nocapture"])
                .env(CHILD_ROOT, temp.path()).stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped()).spawn().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if child.try_wait().unwrap().is_some() {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        output.status.success(),
                        "manifest FIFO child failed: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!(
                        "manifest FIFO child blocked: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            return;
        };
        let root = PathBuf::from(root);
        let file = root.join("tool");
        fs::write(&file, b"original").unwrap();
        let sem = semantics();
        let file_io = RealTrackedIo::new(sem.clone(), &root);
        file_io.metadata_full(&file);
        file_io.read_file(&file).unwrap();
        let recorded_file_deps = file_io.manifest().deps().to_vec();
        let content_io = RealTrackedIo::new(sem.clone(), &root);
        content_io.read_file(&file).unwrap();
        let content_only_fifo = content_io.manifest();
        let link = root.join("followed-fifo");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let link_io = RealTrackedIo::new(sem.clone(), &root);
        link_io.read_file(&link).unwrap();
        let followed_fifo = link_io.manifest();
        let counter = root.join("counter");
        let probe_io = RealTrackedIo::new(sem.clone(), &root);
        let arguments = vec![
            b"-c".to_vec(),
            b"printf x >> \"$1\"; printf stable".to_vec(),
            b"probe".to_vec(),
            counter.as_os_str().as_bytes().to_vec(),
        ];
        assert_eq!(
            probe_io
                .probe_process(Path::new("/bin/sh"), &arguments, &[], &root)
                .unwrap()
                .status,
            0
        );
        let probe = probe_io
            .manifest()
            .deps()
            .iter()
            .find(|dep| matches!(dep, Dependency::ProcessProbe { .. }))
            .unwrap()
            .clone();
        assert_eq!(fs::read(&counter).unwrap(), b"x");
        fs::remove_file(&file).unwrap();
        let cpath = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        for manifest in [&content_only_fifo, &followed_fifo] {
            assert!(
                matches!(
                    manifest.validate_staleness(&sem),
                    Staleness::Stale {
                        dep: Dependency::FileContent { .. },
                        ..
                    }
                ),
                "content hashing must refuse a known FIFO or followed FIFO without blocking"
            );
        }
        let race = root.join("content-open-race");
        fs::write(&race, b"regular").unwrap();
        let error = validation_content_digest_with_open(&race, |path| {
            fs::remove_file(path)?;
            let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            if unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            open_content_for_validation(path)
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);

        // Use only authentic observations, explicitly preserving their order to
        // exercise both process-probe boundaries (including persisted order).
        let later_probe = DependencyManifest {
            deps: recorded_file_deps
                .iter()
                .cloned()
                .chain(std::iter::once(probe.clone()))
                .collect(),
        };
        let rejected = later_probe.validate(&sem);
        assert!(
            matches!(&rejected.staleness, Staleness::Stale { dep: Dependency::FileMetadata { path, .. }, .. } if path == &file)
        );
        assert_eq!(rejected.refreshed, later_probe);
        assert_eq!(
            fs::read(&counter).unwrap(),
            b"x",
            "metadata refusal must precede the later process probe"
        );

        let earlier_probe = DependencyManifest {
            deps: std::iter::once(probe).chain(recorded_file_deps).collect(),
        };
        let rejected = earlier_probe.validate(&sem);
        assert!(
            matches!(&rejected.staleness, Staleness::Stale { dep: Dependency::FileMetadata { path, .. }, .. } if path == &file)
        );
        assert_eq!(rejected.refreshed, earlier_probe);
        assert_eq!(
            fs::read(&counter).unwrap(),
            b"xx",
            "later metadata must not suppress an earlier process probe"
        );

        let plain = root.join("content-only");
        fs::write(&plain, b"one").unwrap();
        let plain_io = RealTrackedIo::new(sem.clone(), &root);
        plain_io.read_file(&plain).unwrap();
        let plain_manifest = plain_io.manifest();
        assert_eq!(plain_manifest.validate_staleness(&sem), Staleness::Fresh);
        fs::write(&plain, b"two").unwrap();
        assert!(
            matches!(
                plain_manifest.validate_staleness(&sem),
                Staleness::Stale {
                    dep: Dependency::FileContent { .. },
                    ..
                }
            ),
            "content-only dependencies must retain their actual hashing decision"
        );
    }
    #[test]
    #[cfg(unix)]
    fn metadata_shortcuts_do_not_hide_symlink_target_edits_or_missing_content() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let other = dir.path().join("same-bytes-target");
        let link = dir.path().join("link");
        fs::write(&target, b"old").unwrap();
        fs::write(&other, b"old").unwrap();
        symlink(&target, &link).unwrap();
        let sem = semantics();
        let io = RealTrackedIo::new(sem.clone(), dir.path());
        io.read_file(&link).unwrap();
        io.metadata_full(&link);
        let manifest = io.manifest();
        assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);
        let future = SystemTime::now() + std::time::Duration::from_secs(10);
        assert!(
            !preflight_metadata_at(manifest.deps(), &sem, future)
                .unwrap()
                .contains(link.as_path()),
            "even old symlink-node metadata cannot discharge followed content"
        );
        fs::write(&target, b"new").unwrap();
        let rejected = manifest.validate(&sem);
        assert!(matches!(
            rejected.staleness,
            Staleness::Stale {
                dep: Dependency::FileContent { .. },
                ..
            }
        ));
        assert_eq!(rejected.refreshed, manifest);
        fs::write(&target, b"old").unwrap();
        let content_only_io = RealTrackedIo::new(sem.clone(), dir.path());
        content_only_io.read_file(&link).unwrap();
        let content_only = content_only_io.manifest();
        fs::remove_file(&link).unwrap();
        symlink(&other, &link).unwrap();
        assert_eq!(
            content_only.validate_staleness(&sem),
            Staleness::Fresh,
            "equal bytes on a different stable followed inode remain content-equivalent"
        );
        let absent = dir.path().join("disappearing");
        fs::write(&absent, b"old").unwrap();
        let missing_io = RealTrackedIo::new(sem.clone(), dir.path());
        missing_io.read_file(&absent).unwrap();
        fs::remove_file(&absent).unwrap();
        missing_io.metadata_full(&absent);
        let missing = missing_io.manifest();
        assert!(!preflight_metadata_at(missing.deps(), &sem, future)
            .unwrap()
            .contains(absent.as_path()));
        assert!(
            matches!(
                missing.validate_staleness(&sem),
                Staleness::Stale {
                    dep: Dependency::FileContent { .. },
                    ..
                }
            ),
            "matching absent metadata must not skip the unreadable old content"
        );
    }

    #[test]
    #[cfg(unix)]
    fn metadata_shortcut_clock_guard_requires_known_aged_regular_observations() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, b"old").unwrap();
        let sem = semantics();
        let io = RealTrackedIo::new(sem.clone(), dir.path());
        io.read_file(&file).unwrap();
        io.metadata_full(&file);
        let manifest = io.manifest();
        let metadata = manifest
            .deps()
            .iter()
            .find(|dep| matches!(dep, Dependency::FileMetadata { .. }))
            .unwrap();
        let Dependency::FileMetadata {
            mtime_ns, ctime_ns, ..
        } = metadata
        else {
            unreachable!()
        };
        let recent = UNIX_EPOCH
            + std::time::Duration::from_nanos(u64::try_from((*mtime_ns).max(*ctime_ns)).unwrap());
        let aged = recent + std::time::Duration::from_secs(10);
        assert!(!content_shortcut_metadata_eligible(metadata, recent));
        assert!(!content_shortcut_metadata_eligible(
            metadata,
            recent - std::time::Duration::from_secs(1)
        ));
        assert!(!content_shortcut_metadata_eligible(metadata, UNIX_EPOCH));
        assert!(content_shortcut_metadata_eligible(metadata, aged));
        let mut unknown = metadata.clone();
        if let Dependency::FileMetadata { ctime_ns, .. } = &mut unknown {
            *ctime_ns = 0;
        }
        assert!(!content_shortcut_metadata_eligible(&unknown, aged));
        // Both observations below are authentic: content was read before the
        // write, while the companion metadata was recorded after it. Since the
        // new metadata is recent, validation must hash and expose the conflict.
        let conflicting_io = RealTrackedIo::new(sem.clone(), dir.path());
        conflicting_io.read_file(&file).unwrap();
        fs::write(&file, b"new").unwrap();
        conflicting_io.metadata_full(&file);
        let conflicting = conflicting_io.manifest();
        assert!(!preflight_metadata(conflicting.deps(), &sem)
            .unwrap()
            .contains(file.as_path()));
        assert!(matches!(
            conflicting.validate_staleness(&sem),
            Staleness::Stale {
                dep: Dependency::FileContent { .. },
                ..
            }
        ));
        let aged_clock = SystemTime::now() + std::time::Duration::from_secs(10);
        assert!(
            !preflight_metadata_at(conflicting.deps(), &sem, aged_clock)
                .unwrap()
                .contains(file.as_path()),
            "old content and later metadata cannot acquire a fabricated association when they age"
        );
        std::thread::sleep(std::time::Duration::from_millis(2100));
        assert!(matches!(
            conflicting.validate_staleness(&sem),
            Staleness::Stale {
                dep: Dependency::FileContent { .. },
                ..
            }
        ));
    }

    #[test]
    #[cfg(unix)]
    fn descriptor_bindings_preserve_semantic_identity_and_conservative_wire_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tool");
        fs::write(&file, b"stable tool").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2100));
        let sem = semantics();
        let first_io = RealTrackedIo::new(sem.clone(), dir.path());
        first_io
            .read_canonical_regular_file(&file)
            .unwrap()
            .unwrap();
        let first = first_io.manifest();
        let first_content = first
            .deps()
            .iter()
            .find(|dep| matches!(dep, Dependency::FileContent { .. }))
            .unwrap()
            .clone();
        let Dependency::FileContent {
            binding: Some(first_binding),
            ..
        } = &first_content
        else {
            panic!("aged descriptor read must retain its actual proof");
        };
        assert!(first_binding.qualifies_at(SystemTime::now()));
        std::thread::sleep(std::time::Duration::from_millis(10));
        let second_io = RealTrackedIo::new(sem.clone(), dir.path());
        second_io
            .read_canonical_regular_file(&file)
            .unwrap()
            .unwrap();
        let second = second_io.manifest();
        let second_content = second
            .deps()
            .iter()
            .find(|dep| matches!(dep, Dependency::FileContent { .. }))
            .unwrap()
            .clone();
        assert_eq!(
            first, second,
            "read-clock provenance is not a semantic input"
        );
        assert_eq!(first.fingerprint(), second.fingerprint());
        assert_ne!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&second).unwrap(),
            "actual read times are still serialized as proof payload"
        );
        let repeated = DependencyManifest::from_deps(
            first_io
                .captured_since(0)
                .into_iter()
                .chain(second_io.captured_since(0))
                .collect(),
        );
        assert_eq!(repeated, first);
        assert_eq!(repeated.fingerprint(), first.fingerprint());
        assert_eq!(
            repeated
                .deps()
                .iter()
                .filter(|dep| matches!(dep, Dependency::FileContent { .. }))
                .count(),
            1
        );
        let interleaved_io = RealTrackedIo::new(sem.clone(), dir.path());
        interleaved_io
            .read_canonical_regular_file(&file)
            .unwrap()
            .unwrap();
        interleaved_io
            .probe_process(
                Path::new("/bin/sh"),
                &[b"-c".to_vec(), b"printf stable".to_vec()],
                &[],
                dir.path(),
            )
            .unwrap();
        interleaved_io
            .read_canonical_regular_file(&file)
            .unwrap()
            .unwrap();
        let interleaved = interleaved_io.manifest();
        let interleaved_roundtrip: DependencyManifest =
            serde_json::from_value(serde_json::to_value(&interleaved).unwrap()).unwrap();
        assert_eq!(interleaved, interleaved_roundtrip);
        assert_eq!(
            interleaved.fingerprint(),
            interleaved_roundtrip.fingerprint()
        );
        assert_eq!(
            interleaved
                .deps()
                .iter()
                .filter(|dep| matches!(dep, Dependency::FileContent { .. }))
                .count(),
            1,
            "a raw intervening probe must not make proof times semantic multiplicity"
        );
        let mut wire_deps = first.deps().to_vec();
        wire_deps.push(second_content);
        let duplicate_wire: DependencyManifest =
            serde_json::from_value(serde_json::json!({"deps": wire_deps})).unwrap();
        assert_eq!(duplicate_wire, first);
        assert_eq!(duplicate_wire.fingerprint(), first.fingerprint());
        let ordinary_io = RealTrackedIo::new(sem.clone(), dir.path());
        ordinary_io.read_file(&file).unwrap();
        let ordinary_content = ordinary_io
            .manifest()
            .deps()
            .iter()
            .find(|dep| matches!(dep, Dependency::FileContent { .. }))
            .unwrap()
            .clone();
        let mut mixed_deps = first.deps().to_vec();
        mixed_deps.push(ordinary_content);
        let mixed: DependencyManifest =
            serde_json::from_value(serde_json::json!({"deps": mixed_deps})).unwrap();
        assert_eq!(mixed, first);
        assert_eq!(mixed.fingerprint(), first.fingerprint());
        assert!(mixed
            .deps()
            .iter()
            .any(|dep| matches!(dep, Dependency::FileContent { binding: None, .. })));
        assert!(
            !preflight_metadata(mixed.deps(), &sem)
                .unwrap()
                .contains(file.as_path()),
            "mixed bound/unbound observations must never invent a binding"
        );
        let validated = first.validate(&sem);
        assert_eq!(validated.staleness, Staleness::Fresh);
        assert_eq!(
            serde_json::to_value(&validated.refreshed).unwrap(),
            serde_json::to_value(&first).unwrap(),
            "validation must preserve exact proof payload, not only semantic equality"
        );
        let mut old = blake3::Hasher::new();
        old.update(b"rush.incremental.v1/manifest-fingerprint/1\n");
        old.update(&(first.deps().len() as u64).to_le_bytes());
        for dep in first.deps() {
            encode_dep(&mut old, dep);
        }
        assert_ne!(
            first.fingerprint(),
            *old.finalize().as_bytes(),
            "v2 must not reuse the old stat-only namespace"
        );
        let mut legacy_json = serde_json::to_value(&first).unwrap();
        for dep in legacy_json["deps"].as_array_mut().unwrap() {
            if let Some(content) = dep.get_mut("FileContent") {
                content.as_object_mut().unwrap().remove("binding");
            }
        }
        let legacy: DependencyManifest = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(legacy, first);
        assert_eq!(legacy.fingerprint(), first.fingerprint());
        assert!(!preflight_metadata(legacy.deps(), &sem)
            .unwrap()
            .contains(file.as_path()));
        assert_eq!(legacy.validate_staleness(&sem), Staleness::Fresh);
        let content_index = first
            .deps()
            .iter()
            .position(|dep| matches!(dep, Dependency::FileContent { .. }))
            .unwrap();
        for key in ["version", "digest", "path", "read_end_ns"] {
            let mut json = serde_json::to_value(&first).unwrap();
            let binding = &mut json["deps"][content_index]["FileContent"]["binding"];
            match key {
                "version" => binding["version"] = serde_json::json!(99),
                "digest" => binding["digest"] = serde_json::to_value([0u8; 32]).unwrap(),
                "path" => binding["path"] = serde_json::json!(dir.path().join("other")),
                "read_end_ns" => binding["read_end_ns"] = serde_json::json!(0),
                _ => unreachable!(),
            }
            let invalid: DependencyManifest = serde_json::from_value(json).unwrap();
            assert!(
                !preflight_metadata(invalid.deps(), &sem)
                    .unwrap()
                    .contains(file.as_path()),
                "{key}"
            );
            assert_eq!(
                invalid.validate_staleness(&sem),
                Staleness::Fresh,
                "invalid provenance must hash valid content, not invent a value"
            );
        }
        let mut malformed = serde_json::to_value(&first).unwrap();
        malformed["deps"][content_index]["FileContent"]["binding"]
            .as_object_mut()
            .unwrap()
            .remove("read_start_ns");
        assert!(serde_json::from_value::<DependencyManifest>(malformed).is_err());
        let mut unknown = serde_json::to_value(&first).unwrap();
        unknown["deps"][content_index]["FileContent"]["binding"]["unknown"] =
            serde_json::json!(true);
        assert!(serde_json::from_value::<DependencyManifest>(unknown).is_err());
        let before_read_end = UNIX_EPOCH
            + std::time::Duration::from_nanos(
                u64::try_from(first_binding.read_end_ns - 1).unwrap(),
            );
        assert!(
            !preflight_metadata_at(first.deps(), &sem, before_read_end)
                .unwrap()
                .contains(file.as_path()),
            "a clock behind the actual historical read end cannot grant reuse"
        );
    }

    #[test]
    #[cfg(unix)]
    fn descriptor_fresh_read_proof_never_ages_into_a_shortcut() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fresh");
        fs::write(&file, b"fresh bytes").unwrap();
        let sem = semantics();
        let io = RealTrackedIo::new(sem.clone(), dir.path());
        io.read_canonical_regular_file(&file).unwrap().unwrap();
        let raw = io.captured_since(0);
        let binding = raw
            .iter()
            .find_map(|dep| match dep {
                Dependency::FileContent {
                    binding: Some(binding),
                    ..
                } => Some(binding),
                _ => None,
            })
            .unwrap();
        assert!(!binding.historically_qualified());
        assert!(!binding.qualifies_at(SystemTime::now() + std::time::Duration::from_secs(20)));
        let canonical = io.manifest();
        assert!(canonical
            .deps()
            .iter()
            .any(|dep| matches!(dep, Dependency::FileContent { binding: None, .. })));
        assert!(!preflight_metadata_at(
            canonical.deps(),
            &sem,
            SystemTime::now() + std::time::Duration::from_secs(20)
        )
        .unwrap()
        .contains(file.as_path()));
        let later_stat_io = RealTrackedIo::new(sem.clone(), dir.path());
        later_stat_io.read_file(&file).unwrap();
        fs::write(&file, b"other bytes").unwrap();
        later_stat_io.metadata_full(&file);
        let contradictory = later_stat_io.manifest();
        assert!(!preflight_metadata_at(
            contradictory.deps(),
            &sem,
            SystemTime::now() + std::time::Duration::from_secs(20)
        )
        .unwrap()
        .contains(file.as_path()));
        std::thread::sleep(std::time::Duration::from_millis(2100));
        assert!(matches!(
            contradictory.validate_staleness(&sem),
            Staleness::Stale {
                dep: Dependency::FileContent { .. },
                ..
            }
        ));
    }
}
