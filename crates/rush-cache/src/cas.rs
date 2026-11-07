// Rush Cache - Content-addressed store
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Content-addressed blob store: `ContentDigest -> immutable bytes`.
//!
//! Layout: `<root>/cas/<first-2-hex>/<hex64>`. Blobs are published
//! atomically (temp + rename) and BEFORE any ActionResult that refers to
//! them, so a crash can orphan blobs (harmless) but can never leave a
//! valid ActionResult pointing at unpublished data. Any read that fails
//! digest verification is a miss, never an incorrect hit.

use anyhow::{anyhow, Context, Result};
use rush_action::artifact_value::{
    record_output_with_required_sink, ArtifactValueKind, BlobSink, ContentDigest, OutputRecord,
    TreeEntry,
};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// File-per-object CAS v1. Simplest correct implementation first; packed
/// small-object segments and reflink materialization are later phases.
pub struct CasStore {
    root: PathBuf,
}

impl CasStore {
    /// Open (creating if needed) a CAS under `root/cas`.
    pub fn new(root: &Path) -> Result<Self> {
        let root = root.join("cas");
        create_dir_all_durable(&root)
            .with_context(|| format!("create CAS root {}", root.display()))?;
        Ok(CasStore { root })
    }

    fn blob_path(&self, digest: &ContentDigest) -> PathBuf {
        let hex = digest.to_hex();
        self.root.join(&hex[..2]).join(hex)
    }

    /// Record an output while requiring every regular file to be installed in
    /// this CAS and made durable before the record is returned.
    ///
    /// Files are read once into private scratch files, hashed during that
    /// read, synced, atomically renamed under the resulting digest, and then
    /// followed by directory syncs. Existing destinations are replaced with
    /// the bytes just verified rather than trusted by pathname. Directory
    /// outputs apply the same rule recursively; symlinks need no blob.
    pub fn record_output_durable(&self, path: &Path) -> Result<OutputRecord> {
        record_output_with_required_sink(path, &DurableCasSink { store: self })
    }

    /// Whether a blob exists (presence only; content is verified on read).
    pub fn has(&self, digest: &ContentDigest) -> bool {
        self.blob_path(digest).is_file()
    }

    /// Whether every blob referenced by a nonempty output set is present.
    ///
    /// This is a presence gate only. Materialization verifies each blob's
    /// digest before serving it, so corruption remains a cache miss.
    pub fn records_available(&self, records: &[OutputRecord]) -> bool {
        fn tree_available(cas: &CasStore, entries: &[TreeEntry]) -> bool {
            entries.iter().all(|entry| match entry.kind {
                ArtifactValueKind::RegularFile => entry
                    .content_digest
                    .map(|digest| cas.has(&digest))
                    .unwrap_or(false),
                ArtifactValueKind::Symlink => entry.symlink_target.is_some(),
                ArtifactValueKind::Tree => tree_available(cas, &entry.children),
            })
        }

        fn record_available(cas: &CasStore, record: &OutputRecord) -> bool {
            match record.kind {
                ArtifactValueKind::RegularFile => record
                    .content_digest
                    .map(|digest| cas.has(&digest))
                    .unwrap_or(false),
                ArtifactValueKind::Symlink => record.symlink_target.is_some(),
                ArtifactValueKind::Tree => tree_available(cas, &record.tree),
            }
        }

        !records.is_empty() && records.iter().all(|record| record_available(self, record))
    }

    /// Store a file's bytes under `digest`. Idempotent; atomic publish.
    pub fn store_file(&self, src: &Path, digest: &ContentDigest) -> Result<()> {
        let dest = self.blob_path(digest);
        if dest.is_file() {
            return Ok(());
        }
        let parent = dest.parent().expect("blob path has parent");
        std::fs::create_dir_all(parent)?;
        // Unique per CALL, not per (pid, digest): two threads in one
        // process storing the same digest would otherwise share a
        // scratch path, and the second File::create truncates the
        // first's partially written file. The single-pass path already
        // learned this; the public helper kept the race.
        let tmp = {
            static SEQ: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            parent.join(format!(
                ".tmp-{}-{}-{}",
                std::process::id(),
                n,
                digest.to_hex()
            ))
        };
        std::fs::copy(src, &tmp)
            .with_context(|| format!("CAS store copy {}", src.display()))?;
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("CAS publish {}", dest.display()))?;
        Ok(())
    }

    /// Materialize a blob to `dest`, VERIFYING the digest while copying.
    /// A missing, truncated or corrupt blob is an error the caller must
    /// turn into a cache miss — never a served hit. Plain copy through a
    /// temp file: hardlinks are forbidden (a later in-place write could
    /// corrupt the CAS inode).
    pub fn materialize(&self, digest: &ContentDigest, dest: &Path) -> Result<()> {
        let src = self.blob_path(digest);
        let mut file = std::fs::File::open(&src)
            .with_context(|| format!("CAS blob missing: {}", src.display()))?;
        // Refresh the blob's mtime so eviction sees it as recently used.
        // Best-effort: an old mtime only makes the blob a better eviction
        // candidate, never a wrong build.
        let _ = file.set_times(
            std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()),
        );
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // The scratch name must be unique per CALL, not derived from
        // `dest`. `with_extension` REPLACES the extension, so `foo.a`
        // and `foo.so` (or two threads restoring one shared input)
        // would collide on a single temp: the second `File::create`
        // truncates the first writer's file mid-stream, and the digest
        // check below hashes the SOURCE stream, so a mangled temp would
        // still verify and be published.
        let tmp = {
            static SEQ: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut name = dest.as_os_str().to_os_string();
            name.push(format!(".cas-restore-{}-{}", std::process::id(), n));
            PathBuf::from(name)
        };
        let mut out = std::fs::File::create(&tmp)?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            out.write_all(&buffer[..n])?;
        }
        drop(out);
        if hasher.finalize().as_bytes() != &digest.0 {
            let _ = std::fs::remove_file(&tmp);
            // Quarantine the corrupt blob so the next store repopulates it.
            let _ = std::fs::remove_file(&src);
            return Err(anyhow!(
                "CAS blob corrupt (digest mismatch): {}",
                src.display()
            ));
        }
        std::fs::rename(&tmp, dest)?;
        Ok(())
    }
}

impl CasStore {
    /// Read `src` exactly ONCE: hash it and write the bytes to a scratch
    /// file in the same pass, then publish the scratch file under the
    /// digest it turned out to have.
    ///
    /// The digest is not known until the read finishes, so the scratch
    /// name cannot be derived from it up front; it is unique per call
    /// for the same reason `materialize` needs one. Publishing is a
    /// rename onto the blob path, which is idempotent — if a concurrent
    /// writer got there first, both wrote identical content by
    /// construction.
    fn digest_and_store_single_pass(&self, src: &Path) -> Result<ContentDigest> {
        let staging = self.root.join("incoming");
        std::fs::create_dir_all(&staging)?;
        let tmp = {
            static SEQ: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            staging.join(format!(".in-{}-{}", std::process::id(), n))
        };

        let mut file = std::fs::File::open(src)
            .with_context(|| format!("CAS tee open {}", src.display()))?;
        let mut out = std::fs::File::create(&tmp)?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0u8; 65536];
        let copy = (|| -> Result<()> {
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
                out.write_all(&buffer[..n])?;
            }
            Ok(())
        })();
        drop(out);
        if let Err(e) = copy {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let digest = ContentDigest(*hasher.finalize().as_bytes());

        let dest = self.blob_path(&digest);
        if dest.is_file() {
            let _ = std::fs::remove_file(&tmp);
            return Ok(digest);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&tmp, &dest).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            anyhow!("CAS publish {}: {}", dest.display(), e)
        })?;
        Ok(digest)
    }

    fn digest_and_store_durable_single_pass(
        &self,
        src: &Path,
        expected_size: u64,
    ) -> Result<ContentDigest> {
        let staging = self.root.join("incoming");
        create_dir_all_durable(&staging).context("create durable CAS staging directory")?;
        let tmp = staging.join(format!(".in-durable-{}", unique_durable_suffix()));

        let publication = (|| -> Result<ContentDigest> {
            let mut input = File::open(src)
                .with_context(|| format!("durable CAS tee open {}", src.display()))?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut output = options
                .open(&tmp)
                .context("create durable CAS scratch file")?;
            let mut hasher = blake3::Hasher::new();
            let mut buffer = [0u8; 65536];
            let mut observed_size = 0u64;
            loop {
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                observed_size = observed_size
                    .checked_add(count as u64)
                    .context("durable CAS byte count overflow")?;
                hasher.update(&buffer[..count]);
                output.write_all(&buffer[..count])?;
            }
            if observed_size != expected_size {
                anyhow::bail!("durable CAS source size changed during storage");
            }
            output.sync_all().context("sync durable CAS scratch file")?;
            drop(output);

            let digest = ContentDigest(*hasher.finalize().as_bytes());
            let destination = self.blob_path(&digest);
            let shard = destination.parent().expect("blob path has parent");
            create_dir_all_durable(shard).context("create durable CAS shard")?;
            std::fs::rename(&tmp, &destination)
                .with_context(|| format!("publish durable CAS blob {}", destination.display()))?;
            sync_directory(shard).context("sync durable CAS shard")?;
            sync_directory(&staging).context("sync durable CAS staging directory")?;
            Ok(digest)
        })();

        if publication.is_err() && std::fs::remove_file(&tmp).is_ok() {
            let _ = sync_directory(&staging);
        }
        publication
    }
}

struct DurableCasSink<'a> {
    store: &'a CasStore,
}

impl BlobSink for DurableCasSink<'_> {
    fn store(&self, path: &Path, digest: &ContentDigest, size: u64) -> Result<()> {
        let observed = self
            .store
            .digest_and_store_durable_single_pass(path, size)?;
        if &observed != digest {
            anyhow::bail!("durable CAS source digest changed before storage")
        }
        Ok(())
    }

    fn digest_and_store(&self, path: &Path, size: u64) -> Result<ContentDigest> {
        self.store
            .digest_and_store_durable_single_pass(path, size)
    }
}

fn unique_durable_suffix() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}-{sequence}", std::process::id())
}

fn create_dir_all_durable(path: &Path) -> Result<()> {
    let absolute;
    let path = if path.is_absolute() {
        path
    } else {
        absolute = std::env::current_dir()
            .context("resolve current directory for relative CAS path")?
            .join(path);
        &absolute
    };
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        match std::fs::metadata(current) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => anyhow::bail!(
                "CAS directory path is not a directory: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
                current = current
                    .parent()
                    .context("CAS directory path has no existing ancestor")?;
            }
            Err(error) => return Err(error).context("inspect CAS directory path"),
        }
    }

    for directory in missing.into_iter().rev() {
        match std::fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::metadata(&directory)
                    .context("inspect concurrently created CAS directory")?;
                if !metadata.is_dir() {
                    anyhow::bail!(
                        "concurrently created CAS path is not a directory: {}",
                        directory.display()
                    );
                }
            }
            Err(error) => return Err(error).context("create CAS directory"),
        }
        let parent = directory
            .parent()
            .context("created CAS directory has no parent")?;
        sync_directory(parent).context("sync CAS directory parent")?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open directory {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("sync directory {}", path.display()))
}

impl BlobSink for CasStore {
    fn store(&self, path: &Path, digest: &ContentDigest, _size: u64) -> Result<()> {
        self.store_file(path, digest)
    }

    fn digest_and_store(&self, path: &Path, _size: u64) -> Result<ContentDigest> {
        self.digest_and_store_single_pass(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(bytes: &[u8]) -> ContentDigest {
        ContentDigest(*blake3::hash(bytes).as_bytes())
    }

    fn backdate(path: &Path, secs: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(t)).unwrap();
    }

    /// gc must evict oldest-first down to the budget, leave newer blobs
    /// alone, report honest byte counts, and clear crash-stale scratch
    /// files. An evicted blob must read back as the error materialize
    /// already defines - the miss path - never as anything else.
    #[test]
    fn gc_evicts_lru_to_budget_and_removes_stale_tmp() {
        let dir = std::env::temp_dir().join(format!("rush-cas-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cas = CasStore::new(&dir).unwrap();

        let mk = |name: &str, body: &[u8], age: u64| -> ContentDigest {
            let src = dir.join(name);
            std::fs::write(&src, body).unwrap();
            let d = digest_of(body);
            cas.store_file(&src, &d).unwrap();
            backdate(&cas.blob_path(&d), age);
            d
        };
        let old = mk("a", &[b'a'; 4096], 300);
        let mid = mk("b", &[b'b'; 4096], 200);
        let new = mk("c", &[b'c'; 4096], 100);

        // A stale scratch file from a crashed writer, two days old.
        let shard = cas.blob_path(&old);
        let tmp = shard.parent().unwrap().join(".tmp-999-0-dead");
        std::fs::write(&tmp, b"partial").unwrap();
        backdate(&tmp, 2 * 86_400);
        let incoming = dir.join("cas/incoming");
        std::fs::create_dir(&incoming).unwrap();
        let incoming_tmp = incoming.join(".in-999-0");
        std::fs::write(&incoming_tmp, b"partial").unwrap();
        backdate(&incoming_tmp, 2 * 86_400);

        // 12,288 bytes stored against a 10,000-byte budget; eviction runs
        // to 90% of budget = 9,000, and the two newest blobs are 8,192,
        // so exactly the oldest must go.
        let stats = cas.gc(10_000).unwrap();
        assert_eq!(stats.evicted, 1, "one blob over budget");
        assert_eq!(stats.stale_tmp, 2, "stale scratch files removed");
        assert!(stats.bytes_after <= 10_000);
        assert!(!cas.has(&old), "oldest blob must be evicted");
        assert!(cas.has(&mid) && cas.has(&new), "newer blobs must survive");
        assert!(!tmp.exists());
        assert!(!incoming_tmp.exists());

        // The evicted blob is a MISS, expressed as materialize's error.
        let out = dir.join("restore");
        assert!(cas.materialize(&old, &out).is_err());

        // Under budget: gc is a no-op and says so.
        let stats2 = cas.gc(10_000).unwrap();
        assert_eq!(stats2.evicted, 0);
        assert_eq!(stats2.bytes_before, stats2.bytes_after);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// materialize refreshes a blob's mtime, so a blob that is USED stops
    /// being the eviction victim - the property that makes LRU mean LRU.
    #[test]
    fn materialize_refreshes_mtime_for_lru() {
        let dir = std::env::temp_dir().join(format!("rush-cas-touch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cas = CasStore::new(&dir).unwrap();
        let src = dir.join("s");
        std::fs::write(&src, b"payload").unwrap();
        let d = digest_of(b"payload");
        cas.store_file(&src, &d).unwrap();
        backdate(&cas.blob_path(&d), 500);
        let before = std::fs::metadata(cas.blob_path(&d)).unwrap().modified().unwrap();
        cas.materialize(&d, &dir.join("out")).unwrap();
        let after = std::fs::metadata(cas.blob_path(&d)).unwrap().modified().unwrap();
        assert!(after > before, "materialize must refresh the blob mtime");
        std::fs::remove_dir_all(&dir).ok();
    }
    use rush_action::artifact_value::content_digest_of_file;

    #[test]
    fn store_and_materialize_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"cached bytes").unwrap();
        let digest = content_digest_of_file(&src).unwrap();
        let cas = CasStore::new(dir.path()).unwrap();
        cas.store_file(&src, &digest).unwrap();
        assert!(cas.has(&digest));
        let dest = dir.path().join("restored/out.bin");
        cas.materialize(&digest, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"cached bytes");
    }

    #[test]
    fn corrupt_blob_is_a_miss_and_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"good bytes").unwrap();
        let digest = content_digest_of_file(&src).unwrap();
        let cas = CasStore::new(dir.path()).unwrap();
        cas.store_file(&src, &digest).unwrap();
        // Corrupt the published blob.
        let blob = dir
            .path()
            .join("cas")
            .join(&digest.to_hex()[..2])
            .join(digest.to_hex());
        std::fs::write(&blob, b"evil bytes").unwrap();
        let dest = dir.path().join("out.bin");
        assert!(cas.materialize(&digest, &dest).is_err());
        assert!(!dest.exists());
        assert!(!cas.has(&digest), "corrupt blob must be quarantined");
    }

    /// Many threads inserting the SAME digest concurrently, with a
    /// reader materializing throughout. Requested by the 2026-08-22
    /// follow-up review: the published bytes must always verify, and no
    /// scratch-file error may surface as a failure.
    #[test]
    fn concurrent_same_digest_inserts_publish_verified_bytes() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("payload.bin");
        let payload: Vec<u8> = (0..(2 * 1024 * 1024)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &payload).unwrap();
        let digest = content_digest_of_file(&src).unwrap();
        let cas = Arc::new(CasStore::new(dir.path()).unwrap());

        let mut handles = Vec::new();
        for _ in 0..16 {
            let cas = Arc::clone(&cas);
            let src = src.clone();
            let digest = digest;
            handles.push(std::thread::spawn(move || {
                cas.store_file(&src, &digest).expect("store must not fail");
            }));
        }
        // A reader racing publication: it may legitimately miss before
        // the first publish, but must never observe corrupt bytes.
        for i in 0..8 {
            let cas = Arc::clone(&cas);
            let dest = dir.path().join(format!("out-{i}.bin"));
            let digest = digest;
            handles.push(std::thread::spawn(move || {
                if cas.has(&digest) {
                    if let Ok(()) = cas.materialize(&digest, &dest) {
                        assert_eq!(
                            content_digest_of_file(&dest).unwrap(),
                            digest,
                            "a materialized blob must verify"
                        );
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(cas.has(&digest));
        let out = dir.path().join("final.bin");
        cas.materialize(&digest, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), payload);
        // No scratch files may be left behind.
        let leftovers: Vec<_> = walkdir::WalkDir::new(dir.path().join("cas"))
            .into_iter()
            .flatten()
            .filter(|e| {
                e.file_name().to_string_lossy().starts_with(".tmp-")
            })
            .collect();
        assert!(leftovers.is_empty(), "scratch files leaked: {leftovers:?}");
    }

    #[test]
    fn truncated_blob_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, vec![7u8; 100_000]).unwrap();
        let digest = content_digest_of_file(&src).unwrap();
        let cas = CasStore::new(dir.path()).unwrap();
        cas.store_file(&src, &digest).unwrap();
        let blob = dir
            .path()
            .join("cas")
            .join(&digest.to_hex()[..2])
            .join(digest.to_hex());
        let bytes = std::fs::read(&blob).unwrap();
        std::fs::write(&blob, &bytes[..50_000]).unwrap();
        assert!(cas
            .materialize(&digest, &dir.path().join("out.bin"))
            .is_err());
    }

    #[test]
    fn durable_record_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("captured.o");
        std::fs::write(&source, b"durable captured bytes").unwrap();

        let record = CasStore::new(dir.path())
            .unwrap()
            .record_output_durable(&source)
            .unwrap();
        let digest = record.content_digest.expect("regular output has a digest");

        let reopened = CasStore::new(dir.path()).unwrap();
        let restored = dir.path().join("restored.o");
        reopened.materialize(&digest, &restored).unwrap();
        assert_eq!(std::fs::read(restored).unwrap(), b"durable captured bytes");
    }

    #[test]
    fn durable_record_replaces_an_untrusted_existing_blob() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("captured.o");
        std::fs::write(&source, b"verified captured bytes").unwrap();
        let digest = content_digest_of_file(&source).unwrap();
        let cas = CasStore::new(dir.path()).unwrap();
        let destination = cas.blob_path(&digest);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::write(&destination, b"corrupt preexisting bytes").unwrap();

        let record = cas.record_output_durable(&source).unwrap();

        assert_eq!(record.content_digest, Some(digest));
        let restored = dir.path().join("restored.o");
        cas.materialize(&digest, &restored).unwrap();
        assert_eq!(std::fs::read(restored).unwrap(), b"verified captured bytes");
    }

    #[test]
    fn durable_tree_record_requires_every_child_blob() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("captured-tree");
        std::fs::create_dir_all(tree.join("nested")).unwrap();
        std::fs::write(tree.join("root.o"), b"root bytes").unwrap();
        std::fs::write(tree.join("nested/child.o"), b"child bytes").unwrap();
        let cas = CasStore::new(dir.path()).unwrap();

        let record = cas.record_output_durable(&tree).unwrap();
        let mut digests = Vec::new();
        fn collect(
            entries: &[rush_action::artifact_value::TreeEntry],
            out: &mut Vec<ContentDigest>,
        ) {
            for entry in entries {
                if let Some(digest) = entry.content_digest {
                    out.push(digest);
                }
                collect(&entry.children, out);
            }
        }
        collect(&record.tree, &mut digests);

        assert_eq!(digests.len(), 2);
        assert!(digests.iter().all(|digest| cas.has(digest)));
        assert!(cas.records_available(std::slice::from_ref(&record)));
        std::fs::remove_file(cas.blob_path(&digests[0])).unwrap();
        assert!(!cas.records_available(std::slice::from_ref(&record)));
    }

    #[test]
    fn durable_record_fails_without_leaking_a_scratch_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("captured.o");
        std::fs::write(&source, b"blocked shard bytes").unwrap();
        let digest = content_digest_of_file(&source).unwrap();
        let cas = CasStore::new(dir.path()).unwrap();
        std::fs::write(cas.root.join(&digest.to_hex()[..2]), b"not a directory").unwrap();

        assert!(cas.record_output_durable(&source).is_err());
        let incoming = cas.root.join("incoming");
        let leftovers = std::fs::read_dir(incoming).unwrap().count();
        assert_eq!(
            leftovers, 0,
            "failed durable stores must clean scratch files"
        );
        assert!(!cas.has(&digest));
    }

    #[test]
    fn concurrent_durable_records_converge_without_scratch_files() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("captured.o");
        std::fs::write(&source, vec![23u8; 2 * 1024 * 1024]).unwrap();
        let digest = content_digest_of_file(&source).unwrap();
        let cas = Arc::new(CasStore::new(dir.path()).unwrap());
        let mut threads = Vec::new();
        for _ in 0..16 {
            let cas = Arc::clone(&cas);
            let source = source.clone();
            threads.push(std::thread::spawn(move || {
                cas.record_output_durable(&source).unwrap()
            }));
        }
        for thread in threads {
            assert_eq!(thread.join().unwrap().content_digest, Some(digest));
        }

        let restored = dir.path().join("restored.o");
        cas.materialize(&digest, &restored).unwrap();
        assert_eq!(content_digest_of_file(&restored).unwrap(), digest);
        assert_eq!(
            std::fs::read_dir(cas.root.join("incoming"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn durable_blob_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("captured.o");
        std::fs::write(&source, b"private bytes").unwrap();
        let cas = CasStore::new(dir.path()).unwrap();
        let record = cas.record_output_durable(&source).unwrap();
        let digest = record.content_digest.unwrap();

        let mode = std::fs::metadata(cas.blob_path(&digest))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

/// What one CAS gc pass did, for the caller's log line.
#[derive(Debug, Default)]
pub struct CasGcStats {
    /// Bytes on disk before the pass.
    pub bytes_before: u64,
    /// Bytes on disk after the pass.
    pub bytes_after: u64,
    /// Blobs deleted.
    pub evicted: usize,
    /// Stale scratch files removed.
    pub stale_tmp: usize,
}

impl CasStore {
    /// Evict least-recently-used blobs until the store fits `max_bytes`.
    ///
    /// This store had NO eviction: a TensorFlow certification sweep wrote
    /// 415 GB of blobs in under an hour and ran the host out of disk -
    /// the sweep's own guard aborted it at 98% full. Eviction is safe by
    /// construction here: materialize() treats a missing blob as an error
    /// the caller converts to a cache miss, never a wrong build, and
    /// materialize refreshes mtime so hot blobs stay. Evicts to 90% of
    /// the budget so back-to-back builds do not thrash the boundary.
    /// Scratch files older than a day are crash leftovers and are removed
    /// regardless of budget.
    pub fn gc(&self, max_bytes: u64) -> Result<CasGcStats> {
        let mut stats = CasGcStats::default();
        let now = std::time::SystemTime::now();
        let mut blobs: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
        let shards = match std::fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(_) => return Ok(stats),
        };
        for shard in shards.flatten() {
            let sp = shard.path();
            if !sp.is_dir() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&sp) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(md) = entry.metadata() else { continue };
                if !md.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(".tmp-")
                    || name.starts_with(".cas-")
                    || name.starts_with(".in-")
                {
                    let stale = md
                        .modified()
                        .ok()
                        .and_then(|m| now.duration_since(m).ok())
                        .is_some_and(|age| age.as_secs() > 86_400);
                    if stale && std::fs::remove_file(&path).is_ok() {
                        stats.stale_tmp += 1;
                    }
                    continue;
                }
                let mtime = md.modified().unwrap_or(std::time::UNIX_EPOCH);
                stats.bytes_before += md.len();
                blobs.push((path, md.len(), mtime));
            }
        }
        stats.bytes_after = stats.bytes_before;
        if stats.bytes_before <= max_bytes {
            return Ok(stats);
        }
        let target = max_bytes / 10 * 9;
        blobs.sort_by_key(|(_, _, mtime)| *mtime);
        for (path, len, _) in blobs {
            if stats.bytes_after <= target {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                stats.bytes_after -= len;
                stats.evicted += 1;
            }
        }
        Ok(stats)
    }
}
