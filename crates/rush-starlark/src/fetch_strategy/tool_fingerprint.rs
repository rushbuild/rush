// Rush Build - guarded analyzer content fingerprints
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Reuse only a stable tool's content fingerprints, never source admission,
//! configuration, process results or repository authority. The caller still
//! resolves its requested tool through tracked canonicalization on every call.

use anyhow::{bail, Context, Result};
use lru::LruCache;
use rush_incremental::{tracked, Dependency, DependencyManifest, FileKind, Staleness};
use sha2::{Digest, Sha256};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ENTRIES: usize = 32;
const MAX_CAPTURED_DEPS: usize = 8;
const MAX_ENTRY_BYTES: usize = 16 * 1024;
const CLOCK_GUARD_NS: i128 = 2_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Signature {
    len: u64,
    mode: u32,
    dev: u64,
    ino: u64,
    mtime_ns: i128,
    ctime_ns: i128,
}

impl Signature {
    /// A memo requires a fresh, canonical regular-file identity. Unknown
    /// metadata/platforms merely disable the memo; tracked reads still run.
    fn read(path: &Path) -> Option<Self> {
        tracked::fresh_canonical_regular_file_metadata(path)
            .ok()
            .flatten()
            .map(Self::from)
    }

    fn old_enough(&self, now: SystemTime) -> bool {
        let Ok(now) = now.duration_since(UNIX_EPOCH) else {
            return false;
        };
        let Ok(now) = i128::try_from(now.as_nanos()) else {
            return false;
        };
        // Fresh writes and backward/future clocks cannot authorize a memo hit.
        now - self.mtime_ns >= CLOCK_GUARD_NS && now - self.ctime_ns >= CLOCK_GUARD_NS
    }

    fn matches_metadata(&self, dep: &Dependency, tool: &Path) -> bool {
        matches!(dep, Dependency::FileMetadata {
            path, present: true, len, mode, mtime_ns, ctime_ns, dev, ino,
        } if path == tool && (*len, *mode, *mtime_ns, *ctime_ns, *dev, *ino)
            == (self.len, self.mode, self.mtime_ns, self.ctime_ns, self.dev, self.ino))
    }
}

impl From<rush_incremental::CanonicalRegularFileMetadata> for Signature {
    fn from(md: rush_incremental::CanonicalRegularFileMetadata) -> Self {
        Self {
            len: md.len,
            mode: md.mode,
            dev: md.dev,
            ino: md.ino,
            mtime_ns: md.mtime_ns,
            ctime_ns: md.ctime_ns,
        }
    }
}

struct Entry {
    signature: Signature,
    sha256: String,
    // Retain every raw observation, including identical duplicates; the
    // canonical manifest is only for validation, not a replacement record.
    deps: Vec<Dependency>,
    manifest: DependencyManifest,
}

struct ToolFingerprintMemo {
    entries: Mutex<LruCache<PathBuf, Arc<Entry>>>,
    #[cfg(test)]
    reads: std::sync::atomic::AtomicUsize,
}

impl ToolFingerprintMemo {
    fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(
                NonZeroUsize::new(capacity.clamp(1, MAX_ENTRIES)).unwrap(),
            )),
            #[cfg(test)]
            reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn evict(&self, tool: &Path) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.pop(tool);
        }
    }

    fn fingerprint(&self, tool: &Path) -> Result<String> {
        self.fingerprint_with(tool, SystemTime::now, tracked::read_canonical_regular_file)
    }

    // The injected reader/clock allow deterministic real-filesystem race and
    // coarse-clock tests; production always uses tracked read + the live clock.
    fn fingerprint_with(
        &self,
        tool: &Path,
        clock: impl FnMut() -> SystemTime,
        read: impl FnOnce(&Path) -> std::io::Result<Option<rush_incremental::CanonicalRegularFileRead>>,
    ) -> Result<String> {
        self.fingerprint_with_validator(tool, clock, read, |io, manifest| {
            io.validate_manifest(manifest)
        })
    }

    fn fingerprint_with_validator(
        &self,
        tool: &Path,
        mut clock: impl FnMut() -> SystemTime,
        read: impl FnOnce(&Path) -> std::io::Result<Option<rush_incremental::CanonicalRegularFileRead>>,
        validate: impl FnOnce(
            &dyn rush_incremental::TrackedIo,
            &DependencyManifest,
        ) -> rush_incremental::Validated,
    ) -> Result<String> {
        if !tool.is_absolute() {
            bail!("tool content fingerprint requires an absolute canonical path");
        }
        let io = tracked::current_required();
        let before = Signature::read(tool);
        if before.is_none() && !io.metadata_full(tool) {
            self.evict(tool);
            bail!("analyzer executable is unavailable: {}", tool.display());
        }
        let cached = self
            .entries
            .lock()
            .ok()
            .and_then(|mut entries| entries.get(tool).cloned());
        if let (Some(before), Some(entry)) = (&before, cached) {
            if &entry.signature == before && before.old_enough(clock()) {
                // This generic validator checks ALL captured tool observations.
                // Its existing unique-metadata shortcut avoids the full content
                // re-read only when the strict companion stat still matches.
                let validated = validate(io.as_ref(), &entry.manifest);
                let after = Signature::read(tool);
                if after.as_ref() != Some(before) {
                    self.evict(tool);
                    bail!(
                        "analyzer executable changed during fingerprint validation: {}",
                        tool.display()
                    );
                }
                if matches!(validated.staleness, Staleness::Fresh) && before.old_enough(clock()) {
                    io.record_deps(&entry.deps);
                    return Ok(entry.sha256.clone());
                }
            }
        }
        self.evict(tool);
        // A fill that starts inside a recent write/coarse timestamp tick must
        // remain ineligible even if hashing runs past the age guard boundary.
        let fill_started_old = before
            .as_ref()
            .is_some_and(|signature| signature.old_enough(clock()));
        let mark = io.mark();
        #[cfg(test)]
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (contents, bound) = match read(tool) {
            Ok(Some(read)) => (read.contents, Some(Signature::from(read.metadata))),
            Ok(None) => (io.read_file(tool)?, None),
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
                (io.read_file(tool)?, None)
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read analyzer executable {}", tool.display()))
            }
        };
        let deps = io.captured_since(mark);
        let bytes = contents.as_ref();
        let sha256 = hex::encode(Sha256::digest(bytes));
        let after = Signature::read(tool);
        if before.is_some()
            && (after != before
                || bound
                    .as_ref()
                    .is_some_and(|bound| Some(bound) != before.as_ref()))
        {
            bail!(
                "analyzer executable changed while reading its fingerprint: {}",
                tool.display()
            );
        }
        if let Some(signature) = after.filter(|after| {
            fill_started_old && before.as_ref() == Some(after) && after.old_enough(clock())
        }) {
            if bound.as_ref() == Some(&signature)
                && admissible_capture(tool, &signature, bytes, &deps, clock())
            {
                let manifest = DependencyManifest::from_deps(deps.clone());
                let entry = Arc::new(Entry {
                    signature,
                    sha256: sha256.clone(),
                    deps,
                    manifest,
                });
                // Poisoned/contended bookkeeping never grants authority. Do not
                // hold the cache lock across reads, validation or any process.
                if let Ok(mut entries) = self.entries.lock() {
                    entries.put(tool.to_path_buf(), entry);
                }
            }
        }
        Ok(sha256)
    }
}

fn admissible_capture(
    tool: &Path,
    signature: &Signature,
    bytes: &[u8],
    deps: &[Dependency],
    now: SystemTime,
) -> bool {
    if deps.len() > MAX_CAPTURED_DEPS {
        return false;
    }
    let mut size = std::mem::size_of::<Entry>() + tool.as_os_str().as_encoded_bytes().len() + 128;
    let mut content = 0;
    let mut metadata = 0;
    let digest = rush_incremental::content_digest(bytes);
    for dep in deps {
        let path = match dep {
            Dependency::FileContent {
                path,
                digest: recorded,
                binding: Some(binding),
            } if path == tool
                && *recorded == digest
                && binding.matches_content(tool, &digest)
                && binding.qualifies_at(now)
                && (
                    binding.len,
                    binding.mode,
                    binding.dev,
                    binding.ino,
                    binding.mtime_ns,
                    binding.ctime_ns,
                ) == (
                    signature.len,
                    signature.mode,
                    signature.dev,
                    signature.ino,
                    signature.mtime_ns,
                    signature.ctime_ns,
                ) =>
            {
                content += 1;
                path
            }
            Dependency::FileMetadata { path, .. } if signature.matches_metadata(dep, tool) => {
                metadata += 1;
                path
            }
            Dependency::FileExistence {
                path,
                exists: true,
                kind: Some(FileKind::File),
            } if path == tool => path,
            // Concurrent unrelated captures, absent/kind-changing observations
            // and unsupported dependency types remain live but are not memoized.
            _ => return false,
        };
        let binding_path = match dep {
            Dependency::FileContent {
                binding: Some(binding),
                ..
            } => binding.path.as_os_str().as_encoded_bytes().len(),
            _ => 0,
        };
        size += 2
            * (std::mem::size_of::<Dependency>()
                + path.as_os_str().as_encoded_bytes().len()
                + binding_path);
        if size > MAX_ENTRY_BYTES {
            return false;
        }
    }
    // Exactly one coherent metadata/content pair is necessary for the generic
    // manifest's unique-metadata content shortcut. Never synthesize either.
    content == 1 && metadata == 1
}

/// Preserve the existing lowercase hexadecimal SHA256 and all tracked BLAKE3
/// content/kind/stat observations while avoiding repeated stable-tool reads.
/// The selected/requested tool's tracked canonicalization remains the caller's
/// responsibility; every memo use independently guards that canonical path.
pub(super) fn tracked_tool_sha256(canonical_tool: &Path) -> Result<String> {
    static MEMO: OnceLock<ToolFingerprintMemo> = OnceLock::new();
    MEMO.get_or_init(|| ToolFingerprintMemo::new(MAX_ENTRIES))
        .fingerprint(canonical_tool)
}

#[cfg(test)]
mod tests {
    #![cfg(unix)]
    use super::*;
    use rush_incremental::{RealTrackedIo, TrackedIo};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn later() -> SystemTime {
        SystemTime::now() + Duration::from_secs(10)
    }
    fn count(memo: &ToolFingerprintMemo) -> usize {
        memo.reads.load(Ordering::Relaxed)
    }
    fn observe(
        memo: &ToolFingerprintMemo,
        path: &Path,
        now: SystemTime,
    ) -> (String, DependencyManifest) {
        tracked::test_scope(path.parent().unwrap(), || {
            let path = tracked::canonicalize(path).unwrap();
            let result = memo
                .fingerprint_with(&path, || now, tracked::read_canonical_regular_file)
                .unwrap();
            (result, tracked::current_required().manifest())
        })
    }

    #[test]
    fn tool_fingerprint_memo_reuses_sha256_and_replays_exact_current_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let tool = temp.path().join("analyzer");
        std::fs::write(&tool, b"exact analyzer bytes").unwrap();
        std::thread::sleep(Duration::from_millis(2100));
        let memo = ToolFingerprintMemo::new(MAX_ENTRIES);
        let first = observe(&memo, &tool, later());
        assert_eq!(
            first.0,
            hex::encode(Sha256::digest(b"exact analyzer bytes"))
        );
        let second = observe(&memo, &tool, later());
        assert_eq!(
            first, second,
            "a fresh tracker must receive every original observation"
        );
        assert_eq!(
            count(&memo),
            1,
            "the stable hit must not invoke the content reader"
        );
        assert!(second.1.deps().iter().any(|dep| matches!(dep,
            Dependency::FileContent { path, digest, .. } if path == &tool && *digest == rush_incremental::content_digest(b"exact analyzer bytes"))));
        assert!(second
            .1
            .deps()
            .iter()
            .any(|dep| matches!(dep, Dependency::FileMetadata { path, .. } if path == &tool)));
    }

    #[test]
    fn tool_fingerprint_memo_invalidates_edits_restored_mtime_replacement_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let tool = temp.path().join("analyzer");
        std::fs::write(&tool, b"original").unwrap();
        std::thread::sleep(Duration::from_millis(2100));
        let memo = ToolFingerprintMemo::new(MAX_ENTRIES);
        let original = observe(&memo, &tool, later());
        let old_mtime = std::fs::metadata(&tool).unwrap().modified().unwrap();
        std::fs::write(&tool, b"modified").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&tool)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_mtime))
            .unwrap();
        let edited = observe(&memo, &tool, later());
        assert_ne!(edited.0, original.0);
        assert_eq!(
            count(&memo),
            2,
            "ctime must defeat restored mtime and identical length"
        );
        let old_inode = tracked::test_scope(temp.path(), || Signature::read(&tool).unwrap()).ino;
        let replacement = temp.path().join("replacement");
        std::fs::write(&replacement, b"modified").unwrap();
        std::fs::rename(replacement, &tool).unwrap();
        assert_ne!(
            tracked::test_scope(temp.path(), || Signature::read(&tool).unwrap()).ino,
            old_inode
        );
        let replaced = observe(&memo, &tool, later());
        assert_eq!(
            replaced.0, edited.0,
            "same bytes retain the SHA256 format/value"
        );
        assert_eq!(
            count(&memo),
            3,
            "equal bytes on a new inode still require a read"
        );
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        let chmod = observe(&memo, &tool, later());
        assert_eq!(chmod.0, replaced.0);
        assert_eq!(count(&memo), 4, "mode changes must not reuse old metadata");
        assert_ne!(chmod.1, replaced.1);
    }

    #[test]
    fn tool_fingerprint_memo_preserves_missing_and_symlink_observations() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("one");
        let b = temp.path().join("two");
        let alias = temp.path().join("selected");
        std::fs::write(&a, b"one").unwrap();
        std::fs::write(&b, b"two").unwrap();
        symlink(&a, &alias).unwrap();
        std::thread::sleep(Duration::from_millis(2100));
        let memo = ToolFingerprintMemo::new(MAX_ENTRIES);
        let first = observe(&memo, &alias, later());
        std::fs::remove_file(&alias).unwrap();
        symlink(&b, &alias).unwrap();
        let second = observe(&memo, &alias, later());
        assert_ne!(first.0, second.0);
        assert_eq!(count(&memo), 2);
        tracked::test_scope(temp.path(), || {
            assert!(matches!(
                tracked::current_required()
                    .validate_manifest(&first.1)
                    .staleness,
                Staleness::Stale { .. }
            ));
        });
        std::fs::remove_file(&b).unwrap();
        tracked::test_scope(temp.path(), || {
            assert!(memo
                .fingerprint_with(&b, later, tracked::read_canonical_regular_file)
                .is_err());
            assert!(tracked::current_required()
                .manifest()
                .deps()
                .iter()
                .any(|dep| matches!(dep,
                Dependency::FileMetadata { path, present: false, .. } if path == &b)));
            // A producer may create the missing path; keep the earlier negative
            // dependency, resume live observations, and acquire current bytes.
            std::fs::write(&b, b"new").unwrap();
            tracked::refresh_filesystem_observations();
            let restored = memo
                .fingerprint_with(&b, later, tracked::read_canonical_regular_file)
                .unwrap();
            assert_eq!(restored, hex::encode(Sha256::digest(b"new")));
            assert!(tracked::current_required()
                .manifest()
                .deps()
                .iter()
                .any(|dep| matches!(dep,
                Dependency::FileMetadata { path, present: false, .. } if path == &b)));
        });
        assert_eq!(count(&memo), 3);
    }

    #[test]
    fn tool_fingerprint_memo_refuses_changed_read_and_foreign_capture() {
        let temp = tempfile::tempdir().unwrap();
        let tool = temp.path().join("analyzer");
        std::fs::write(&tool, b"first").unwrap();
        let memo = ToolFingerprintMemo::new(MAX_ENTRIES);
        tracked::test_scope(temp.path(), || {
            let error = memo
                .fingerprint_with(&tool, later, |path| {
                    let bytes = tracked::read_canonical_regular_file(path)?;
                    std::fs::write(path, b"other")?;
                    Ok(bytes)
                })
                .unwrap_err();
            assert!(
                error.to_string().contains("changed while reading"),
                "{error:#}"
            );
            assert!(tracked::current_required().manifest().deps().iter().any(|dep| matches!(dep,
                Dependency::FileContent { digest, .. } if *digest == rush_incremental::content_digest(b"first"))));
        });
        assert_eq!(memo.entries.lock().unwrap().len(), 0);
        tracked::test_scope(temp.path(), || {
            memo.fingerprint_with(&tool, later, |path| {
                let bytes = tracked::read_canonical_regular_file(path)?;
                let _ = tracked::getenv(b"FOREIGN_CONCURRENT_DEPENDENCY");
                Ok(bytes)
            })
            .unwrap();
            assert!(tracked::current_required()
                .manifest()
                .deps()
                .iter()
                .any(|dep| matches!(dep,
                Dependency::Env { name, .. } if name == b"FOREIGN_CONCURRENT_DEPENDENCY")));
        });
        assert_eq!(
            memo.entries.lock().unwrap().len(),
            0,
            "unrelated observations stay live but cannot enter a tool memo"
        );
        observe(&memo, &tool, later());
        assert_eq!(count(&memo), 3);
    }

    #[test]
    fn tool_fingerprint_memo_refuses_bound_bytes_from_transient_parent_directory() {
        let temp = tempfile::tempdir().unwrap();
        let active = temp.path().join("active");
        let other = temp.path().join("other");
        let saved = temp.path().join("saved");
        std::fs::create_dir(&active).unwrap();
        std::fs::create_dir(&other).unwrap();
        let tool = active.join("analyzer");
        std::fs::write(&tool, b"first").unwrap();
        std::fs::write(other.join("analyzer"), b"other").unwrap();
        let memo = ToolFingerprintMemo::new(MAX_ENTRIES);
        tracked::test_scope(temp.path(), || {
            let before = Signature::read(&tool).unwrap();
            let error = memo
                .fingerprint_with(&tool, later, |path| {
                    std::fs::rename(&active, &saved)?;
                    std::fs::rename(&other, &active)?;
                    let captured = tracked::read_canonical_regular_file(path);
                    std::fs::rename(&active, &other)?;
                    std::fs::rename(&saved, &active)?;
                    assert_eq!(
                        captured
                            .as_ref()
                            .unwrap()
                            .as_ref()
                            .unwrap()
                            .contents
                            .as_ref(),
                        b"other"
                    );
                    captured
                })
                .unwrap_err();
            assert!(
                error.to_string().contains("changed while reading"),
                "{error:#}"
            );
            assert_eq!(
                Signature::read(&tool).unwrap(),
                before,
                "the original file's pre/post stat is identical"
            );
        });
        assert_eq!(memo.entries.lock().unwrap().len(), 0);
        let correct = observe(&memo, &tool, later());
        assert_eq!(correct.0, hex::encode(Sha256::digest(b"first")));
        assert_eq!(count(&memo), 2);
    }

    #[test]
    fn tool_fingerprint_memo_enforces_clock_bounds_and_lru_limits() {
        let temp = tempfile::tempdir().unwrap();
        let tool = temp.path().join("analyzer");
        std::fs::write(&tool, b"tool").unwrap();
        let memo = ToolFingerprintMemo::new(2);
        let signature = tracked::test_scope(temp.path(), || Signature::read(&tool).unwrap());
        let recent = UNIX_EPOCH
            + Duration::from_nanos(
                u64::try_from(signature.ctime_ns.max(signature.mtime_ns)).unwrap(),
            );
        observe(&memo, &tool, recent);
        observe(&memo, &tool, recent);
        assert_eq!(count(&memo), 2);
        assert_eq!(
            memo.entries.lock().unwrap().len(),
            0,
            "fresh/coarse-clock files cannot seed a memo"
        );
        observe(&memo, &tool, later());
        assert_eq!(count(&memo), 3);
        observe(&memo, &tool, UNIX_EPOCH);
        assert_eq!(
            count(&memo),
            4,
            "backward clock must refuse an otherwise matching hit"
        );
        assert_eq!(memo.entries.lock().unwrap().len(), 0);
        assert!(!signature.old_enough(UNIX_EPOCH - Duration::from_secs(1)));
        for number in 0..3 {
            std::fs::write(temp.path().join(format!("analyzer-{number}")), [number]).unwrap();
        }
        std::thread::sleep(Duration::from_millis(2100));
        for number in 0..3 {
            let file = temp.path().join(format!("analyzer-{number}"));
            observe(&memo, &file, later());
            assert!(memo.entries.lock().unwrap().len() <= 2);
        }
        assert_eq!(count(&memo), 7);
        observe(&memo, &temp.path().join("analyzer-0"), later());
        assert_eq!(
            count(&memo),
            8,
            "the least-recently-used entry must have been evicted"
        );
    }

    #[test]
    fn tool_fingerprint_memo_fresh_start_cannot_age_into_admission_during_read() {
        let temp = tempfile::tempdir().unwrap();
        let tool = temp.path().join("analyzer");
        std::fs::write(&tool, b"fresh tool").unwrap();
        let signature = tracked::test_scope(temp.path(), || Signature::read(&tool).unwrap());
        let recent = UNIX_EPOCH
            + Duration::from_nanos(
                u64::try_from(signature.ctime_ns.max(signature.mtime_ns)).unwrap(),
            );
        let advanced = recent + Duration::from_secs(10);
        let clock = std::cell::Cell::new(recent);
        let memo = ToolFingerprintMemo::new(MAX_ENTRIES);
        tracked::test_scope(temp.path(), || {
            let result = memo
                .fingerprint_with(
                    &tool,
                    || clock.get(),
                    |path| {
                        let result = tracked::read_canonical_regular_file(path);
                        clock.set(advanced);
                        result
                    },
                )
                .unwrap();
            assert_eq!(result, hex::encode(Sha256::digest(b"fresh tool")));
        });
        assert_eq!(
            clock.get(),
            advanced,
            "the end of the read is outside the coarse-clock guard"
        );
        assert_eq!(
            memo.entries.lock().unwrap().len(),
            0,
            "fresh-at-start fills must never seed a memo"
        );
        assert_eq!(count(&memo), 1);
        std::thread::sleep(Duration::from_millis(2100));
        observe(&memo, &tool, advanced);
        observe(&memo, &tool, advanced);
        assert_eq!(
            count(&memo),
            2,
            "only a separate, already-aged fill may be reused"
        );
    }

    #[test]
    fn tool_fingerprint_memo_poison_and_unsupported_reads_fall_back_to_reads() {
        let temp = tempfile::tempdir().unwrap();
        let tool = temp.path().join("analyzer");
        std::fs::write(&tool, b"tool").unwrap();
        let memo = Arc::new(ToolFingerprintMemo::new(MAX_ENTRIES));
        tracked::test_scope(temp.path(), || {
            memo.fingerprint_with(&tool, later, |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "fixture backend lacks bound read",
                ))
            })
            .unwrap();
        });
        assert_eq!(
            memo.entries.lock().unwrap().len(),
            0,
            "untracked bytes cannot seed a memo"
        );
        let poisoning = memo.clone();
        assert!(std::thread::spawn(move || {
            let _guard = poisoning.entries.lock().unwrap();
            panic!("controlled poisoned-cache fixture");
        })
        .join()
        .is_err());
        observe(&memo, &tool, later());
        observe(&memo, &tool, later());
        assert_eq!(
            count(&memo),
            3,
            "poisoned cache must retain fresh-read behavior"
        );
    }

    #[test]
    fn tool_fingerprint_memo_keeps_current_env_rc_and_negative_inputs() {
        use bytes::Bytes;
        use rush_command_core::CommandSemantics;
        use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
        let temp = tempfile::tempdir().unwrap();
        let tool = temp.path().join("analyzer");
        let rc = temp.path().join(".bazelrc");
        let optional = temp.path().join("optional-input");
        std::fs::write(&tool, b"unchanged tool").unwrap();
        std::fs::write(&rc, b"build --define=VALUE=first\n").unwrap();
        std::thread::sleep(Duration::from_millis(2100));
        let memo = ToolFingerprintMemo::new(MAX_ENTRIES);
        let run = |value: &str| {
            let semantics = CommandSemantics::from_run_request(&RunRequest {
                argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"build")],
                working_directory: Bytes::from(temp.path().display().to_string()),
                environment: vec![EnvironmentVariable {
                    name: Bytes::from_static(b"CLIENT_VALUE"),
                    value: Bytes::copy_from_slice(value.as_bytes()),
                }],
                protocol_major: rush_protocol::PROTOCOL_MAJOR,
                protocol_minor: rush_protocol::PROTOCOL_MINOR,
                ..Default::default()
            })
            .unwrap();
            let io = Arc::new(RealTrackedIo::new(semantics, temp.path()));
            let result = tracked::scope(io.clone(), || {
                let _ = tracked::command_environment().unwrap();
                let _ = tracked::read_file(&rc).unwrap();
                let _ = tracked::exists(&optional);
                memo.fingerprint_with(&tool, later, tracked::read_canonical_regular_file)
                    .unwrap()
            });
            (result, io)
        };
        let first = run("first");
        std::fs::write(&rc, b"build --define=VALUE=other\n").unwrap();
        std::fs::write(&optional, b"newly materialized").unwrap();
        let second = run("other");
        assert_eq!(first.0, second.0);
        assert_eq!(
            count(&memo),
            1,
            "only the tool content is reused, not command/RC inputs"
        );
        assert_ne!(first.1.manifest(), second.1.manifest());
        assert!(matches!(
            second.1.validate_manifest(&first.1.manifest()).staleness,
            Staleness::Stale { .. }
        ));
        assert!(first.1.manifest().deps().iter().any(|dep| matches!(dep,
            Dependency::FileExistence { path, exists: false, .. } if path == &optional)));
    }

    #[test]
    fn tool_fingerprint_memo_capture_bounds_and_digest_coherence_are_strict() {
        let temp = tempfile::tempdir().unwrap();
        let tool = temp.path().join("analyzer");
        std::fs::write(&tool, b"tool").unwrap();
        std::thread::sleep(Duration::from_millis(2100));
        let signature = tracked::test_scope(temp.path(), || Signature::read(&tool).unwrap());
        let captured = tracked::test_scope(temp.path(), || {
            let io = tracked::current_required();
            let mark = io.mark();
            io.read_canonical_regular_file(&tool).unwrap().unwrap();
            io.captured_since(mark)
        });
        assert!(admissible_capture(
            &tool,
            &signature,
            b"tool",
            &captured,
            later()
        ));
        assert!(!admissible_capture(
            &tool,
            &signature,
            b"fake",
            &captured,
            later()
        ));
        let mut duplicate = captured.clone();
        duplicate.extend(captured.clone());
        assert!(!admissible_capture(
            &tool,
            &signature,
            b"tool",
            &duplicate,
            later()
        ));
        let oversized = vec![captured[0].clone(); MAX_CAPTURED_DEPS + 1];
        assert!(!admissible_capture(
            &tool,
            &signature,
            b"tool",
            &oversized,
            later()
        ));
        let huge = PathBuf::from(format!("/{}", "x".repeat(MAX_ENTRY_BYTES)));
        let huge_deps: Vec<_> = captured
            .iter()
            .cloned()
            .map(|mut dep| {
                match &mut dep {
                    Dependency::FileContent { path, binding, .. } => {
                        *path = huge.clone();
                        if let Some(binding) = binding {
                            binding.path = huge.clone();
                        }
                    }
                    Dependency::FileMetadata { path, .. }
                    | Dependency::FileExistence { path, .. } => *path = huge.clone(),
                    _ => unreachable!(),
                }
                dep
            })
            .collect();
        assert!(!admissible_capture(
            &huge,
            &signature,
            b"tool",
            &huge_deps,
            later()
        ));
    }
    #[test]
    fn tool_fingerprint_memo_known_fifo_and_precheck_race_refuse_without_fallback() {
        use std::os::unix::ffi::OsStrExt;
        const CHILD_ROOT: &str = "RUSH_TOOL_FINGERPRINT_FIFO_CHILD";
        let Some(root) = std::env::var_os(CHILD_ROOT) else {
            let temp = tempfile::tempdir().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "fetch_strategy::tool_fingerprint::tests::tool_fingerprint_memo_known_fifo_and_precheck_race_refuse_without_fallback", "--nocapture"])
                .env(CHILD_ROOT, temp.path()).stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped()).spawn().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if child.try_wait().unwrap().is_some() {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        output.status.success(),
                        "FIFO memo child failed: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!(
                        "FIFO memo child blocked: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            return;
        };
        let root = PathBuf::from(root);
        let fifo = |path: &Path| {
            let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            if unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) } == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        };
        let memo = ToolFingerprintMemo::new(MAX_ENTRIES);
        let known = root.join("known");
        fifo(&known).unwrap();
        tracked::test_scope(&root, || {
            let error = memo
                .fingerprint_with(&known, later, tracked::read_canonical_regular_file)
                .unwrap_err();
            assert!(
                error.chain().any(|error| error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::InvalidInput)),
                "{error:#}"
            );
            assert!(!tracked::current_required()
                .manifest()
                .deps()
                .iter()
                .any(|dep| matches!(dep, Dependency::FileContent { .. })));
        });
        let race = root.join("race");
        std::fs::write(&race, b"regular analyzer").unwrap();
        tracked::test_scope(&root, || {
            let error = memo
                .fingerprint_with(&race, later, |path| {
                    // The helper has already observed the regular signature, but
                    // the capable reader has not started its own precheck yet.
                    std::fs::remove_file(path)?;
                    fifo(path)?;
                    tracked::read_canonical_regular_file(path)
                })
                .unwrap_err();
            assert!(
                error.chain().any(|error| error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::InvalidInput)),
                "{error:#}"
            );
            assert!(!tracked::current_required()
                .manifest()
                .deps()
                .iter()
                .any(|dep| matches!(dep, Dependency::FileContent { .. })));
        });
        assert_eq!(memo.entries.lock().unwrap().len(), 0);
        assert_eq!(count(&memo), 2);
    }
    #[test]
    fn tool_fingerprint_memo_warm_validation_fifo_race_refuses_without_content_read() {
        use std::os::unix::ffi::OsStrExt;
        const CHILD_ROOT: &str = "RUSH_TOOL_FINGERPRINT_WARM_FIFO_CHILD";
        let Some(root) = std::env::var_os(CHILD_ROOT) else {
            let temp = tempfile::tempdir().unwrap();
            std::fs::write(temp.path().join("analyzer"), b"warm analyzer").unwrap();
            std::thread::sleep(Duration::from_millis(2100));
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "fetch_strategy::tool_fingerprint::tests::tool_fingerprint_memo_warm_validation_fifo_race_refuses_without_content_read", "--nocapture"])
                .env(CHILD_ROOT, temp.path()).stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped()).spawn().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if child.try_wait().unwrap().is_some() {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        output.status.success(),
                        "warm FIFO child failed: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!(
                        "warm FIFO child blocked: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            return;
        };
        let root = PathBuf::from(root);
        let tool = root.join("analyzer");
        assert_eq!(std::fs::read(&tool).unwrap(), b"warm analyzer");
        let memo = ToolFingerprintMemo::new(MAX_ENTRIES);
        observe(&memo, &tool, later());
        assert_eq!(count(&memo), 1);
        tracked::test_scope(&root, || {
            let error = memo.fingerprint_with_validator(&tool, later,
                |_| panic!("warm stale metadata must not invoke the fill reader"),
                |io, manifest| {
                    // The helper's fresh SignatureA check has finished. Swap
                    // before calling the ACTUAL generic manifest validator.
                    std::fs::remove_file(&tool).unwrap();
                    let cpath = std::ffi::CString::new(tool.as_os_str().as_bytes()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
                    let validated = io.validate_manifest(manifest);
                    assert!(matches!(&validated.staleness, Staleness::Stale { dep: Dependency::FileMetadata { path, .. }, .. } if path == &tool));
                    assert_eq!(&validated.refreshed, manifest, "refusal must preserve the original observations");
                    assert_eq!(serde_json::to_value(&validated.refreshed).unwrap(), serde_json::to_value(manifest).unwrap(),
                        "semantic equality must not hide discarded proof payload");
                    validated
                }).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("changed during fingerprint validation"),
                "{error:#}"
            );
        });
        assert_eq!(memo.entries.lock().unwrap().len(), 0);
        assert_eq!(
            count(&memo),
            1,
            "no fallback fill or ordinary file read is allowed"
        );
    }
}
