// Rush Action - Artifact values
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Artifact values: the identity of what downstream computation OBSERVES,
//! kept strictly separate from the identity of the computation that
//! produced it.
//!
//! ```text
//! ActionId            = identity of the computation
//! ContentDigest       = identity of stored bytes
//! ArtifactFingerprint = value observed by downstream computation
//! ```
//!
//! The retired `.rushid` producer stamp first carried the producer's
//! ActionId and later carried a versioned [`ArtifactFingerprint`]. Both
//! forms put Rush state in the user's output tree, where it could affect
//! other build tools. Production code now records fingerprints in cached
//! action results and verifies live filesystem values. The legacy encoding
//! remains only so reserved-state collision checks can reject old sidecars.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;

/// Domain separation for fingerprint hashing. Bump the version when the
/// fingerprint FORMULA changes; old stamps then read as invalid and
/// consumers fall back to content hashing.
const FP_DOMAIN: &[u8] = b"rush-artifact-fp-v1\0";

/// Retired value-stamp prefix, retained only to recognize old sidecars.
pub const STAMP_PREFIX: &str = "rushfp1:";

/// BLAKE3 of an artifact's stored bytes. Identity of content in a CAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentDigest(pub [u8; 32]);

impl ContentDigest {
    pub fn to_hex(&self) -> String {
        blake3::Hash::from(self.0).to_hex().to_string()
    }
}

/// The semantic value a downstream action observes: kind, content, and
/// the filesystem properties Bazel semantics let consumers see
/// (executable bit; symlink target; tree structure). Never includes
/// host metadata such as mtime, uid or inode numbers, and never the
/// producer's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactFingerprint(pub [u8; 32]);

impl ArtifactFingerprint {
    pub fn to_hex(&self) -> String {
        blake3::Hash::from(self.0).to_hex().to_string()
    }

    pub fn from_hex(s: &str) -> Result<Self> {
        let hash = blake3::Hash::from_hex(s)
            .map_err(|e| anyhow!("invalid fingerprint hex: {}", e))?;
        Ok(ArtifactFingerprint(*hash.as_bytes()))
    }

    /// The retired stamp-file representation: `rushfp1:<hex64>`.
    pub fn to_stamp(&self) -> String {
        format!("{}{}", STAMP_PREFIX, self.to_hex())
    }

    /// Parse a retired value stamp. Returns None for the older
    /// producer-ActionId format (bare hex) or any malformed encoding.
    pub fn parse_stamp(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let hex = text.trim().strip_prefix(STAMP_PREFIX)?;
        Self::from_hex(hex).ok()
    }
}

/// Observable artifact shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactValueKind {
    RegularFile,
    Symlink,
    Tree,
}

/// Compute the ContentDigest of a regular file by streaming its bytes.
pub fn content_digest_of_file(path: &Path) -> Result<ContentDigest> {
    let mut hasher = blake3::Hasher::new();
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("content digest: open {}", path.display()))?;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(ContentDigest(*hasher.finalize().as_bytes()))
}

/// Compute the ArtifactFingerprint for a filesystem path.
///
/// - Regular file: domain, kind, content digest, executable bit. GNU thin
///   archives additionally fold in each member's fingerprint: the archive
///   bytes hold only member PATHS, but a linker observes member contents,
///   so identical archive bytes with a changed member must change the
///   fingerprint (the correctness gate that forbade naive byte digests).
/// - Symlink: domain, kind, target path (the link itself is the value;
///   consumers that follow it declare the target separately).
/// - Tree: domain, kind, and a deterministic (name-sorted) recursion of
///   child names, kinds and fingerprints.
pub fn fingerprint_path(path: &Path) -> Result<ArtifactFingerprint> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("fingerprint: lstat {}", path.display()))?;
    let file_type = meta.file_type();
    let mut hasher = blake3::Hasher::new();
    hasher.update(FP_DOMAIN);

    if file_type.is_symlink() {
        hasher.update(b"symlink\0");
        let target = std::fs::read_link(path)?;
        hasher.update(target.as_os_str().as_encoded_bytes());
        return Ok(ArtifactFingerprint(*hasher.finalize().as_bytes()));
    }

    if file_type.is_dir() {
        hasher.update(b"tree\0");
        let mut entries: Vec<_> = std::fs::read_dir(path)?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            hasher.update(name.as_encoded_bytes());
            hasher.update(b"\0");
            let child = fingerprint_path(&entry.path())?;
            hasher.update(&child.0);
        }
        return Ok(ArtifactFingerprint(*hasher.finalize().as_bytes()));
    }

    reject_special_file(path, &file_type)?;
    let digest = content_digest_of_file(path)?;
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    fingerprint_regular_from_digest(path, &digest, executable)
}

/// The regular-file fingerprint, computed from a content digest the
/// caller ALREADY has.
///
/// Byte-identical to the regular-file branch of [`fingerprint_path`] —
/// enforced by `fingerprint_from_digest_matches_fingerprint_path` — but
/// it does not re-read the file. Committing an output used to hash it
/// twice (once for the digest, once for the fingerprint) plus a third
/// read to copy it into the CAS; this is the seam that removes the
/// second pass.
pub fn fingerprint_regular_from_digest(
    path: &Path,
    digest: &ContentDigest,
    executable: bool,
) -> Result<ArtifactFingerprint> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(FP_DOMAIN);
    hasher.update(b"file\0");
    hasher.update(&digest.0);
    #[cfg(unix)]
    hasher.update(&[executable as u8]);
    #[cfg(not(unix))]
    let _ = executable;

    // Thin-archive correctness gate: fold in member fingerprints. The
    // archive bytes hold only member PATHS, so identical bytes with a
    // changed member MUST produce a different value.
    if is_thin_archive(path) {
        for member in crate::thin_archive_member_paths(path)? {
            hasher.update(member.as_os_str().as_encoded_bytes());
            hasher.update(b"\0");
            match fingerprint_path(&member) {
                Ok(fp) => hasher.update(&fp.0),
                // A missing member still changes the value deterministically.
                Err(_) => hasher.update(b"missing-member\0"),
            };
        }
    }

    Ok(ArtifactFingerprint(*hasher.finalize().as_bytes()))
}

/// Reject outputs that are neither regular files, symlinks nor
/// directories.
///
/// Opening a FIFO for reading BLOCKS until a writer appears, so a
/// declared output that is a pipe (or a socket, or a device node) would
/// hang the commit rather than fail it. Sockets and device nodes have no
/// meaningful content value either. Fail closed instead.
fn reject_special_file(path: &Path, ft: &std::fs::FileType) -> Result<()> {
    if ft.is_file() {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "unsupported output kind at {} (not a regular file, symlink or \
         directory); refusing to fingerprint it",
        path.display()
    ))
}

fn is_thin_archive(path: &Path) -> bool {
    let mut magic = [0u8; 8];
    match std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)) {
        Ok(()) => &magic == b"!<thin>\n",
        Err(_) => false,
    }
}

/// One entry of a tree output's canonical structure, recorded so a CAS
/// restore can rebuild the directory without the original filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub kind: ArtifactValueKind,
    /// Regular files: the blob digest. Trees/symlinks: None.
    pub content_digest: Option<ContentDigest>,
    pub executable: bool,
    /// Lossless native target bytes for symbolic links.
    pub symlink_target: Option<Vec<u8>>,
    pub children: Vec<TreeEntry>,
}

/// Everything the cache records about one committed output: its semantic
/// value (fingerprint), and enough shape + content identity to
/// rematerialize it from a CAS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRecord {
    pub path: std::path::PathBuf,
    pub kind: ArtifactValueKind,
    pub artifact_fingerprint: ArtifactFingerprint,
    /// Regular files only.
    pub content_digest: Option<ContentDigest>,
    pub size: u64,
    pub executable: bool,
    /// Lossless native target bytes for symbolic links.
    pub symlink_target: Option<Vec<u8>>,
    /// Tree outputs: the canonical child structure.
    pub tree: Vec<TreeEntry>,
}

/// Sink offered to [`record_output`] so content flows into a CAS in the
/// SAME read pass that computes digests (never read an output twice).
pub trait BlobSink {
    /// Store the file at `path`, whose content digest is `digest`, into
    /// the CAS. Must be atomic (temp + rename) and idempotent. Failure is
    /// reported to the caller; [`record_output`] may fall back to a digest,
    /// while [`record_output_with_required_sink`] fails closed.
    fn store(&self, path: &Path, digest: &ContentDigest, size: u64) -> Result<()>;

    /// Read `path` ONCE: compute its content digest and store the bytes
    /// under it.
    ///
    /// The default implementation is the honest two-pass fallback
    /// (digest, then store). A sink that can tee — writing its temp file
    /// from the same read that feeds the hasher — overrides this. The
    /// commit path calls this instead of digesting and storing
    /// separately, so the number of full passes over a committed output
    /// is a property of the sink rather than of every call site.
    fn digest_and_store(&self, path: &Path, size: u64) -> Result<ContentDigest> {
        let digest = content_digest_of_file(path)?;
        self.store(path, &digest, size)?;
        Ok(digest)
    }
}

/// Compute an output's full record, optionally teeing regular-file
/// content into a CAS. Directory children are stored recursively.
pub fn record_output(
    path: &Path,
    sink: Option<&dyn BlobSink>,
) -> Result<OutputRecord> {
    record_output_inner(path, sink, false)
}

/// Compute an output's full record and require every regular-file byte stream
/// to be accepted by `sink`.
///
/// Unlike [`record_output`], this never falls back to a digest-only record when
/// storage fails. Capture adoption uses this boundary before it exposes an
/// action result: every content digest in the result must already name a blob
/// accepted by the caller's durable sink.
pub fn record_output_with_required_sink(
    path: &Path,
    sink: &dyn BlobSink,
) -> Result<OutputRecord> {
    record_output_inner(path, Some(sink), true)
}

fn record_output_inner(
    path: &Path,
    sink: Option<&dyn BlobSink>,
    require_sink: bool,
) -> Result<OutputRecord> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("record output: lstat {}", path.display()))?;
    let file_type = meta.file_type();

    if file_type.is_symlink() {
        let target = std::fs::read_link(path)?;
        return Ok(OutputRecord {
            path: path.to_path_buf(),
            kind: ArtifactValueKind::Symlink,
            artifact_fingerprint: fingerprint_path(path)?,
            content_digest: None,
            size: 0,
            executable: false,
            symlink_target: Some(symlink_target_bytes(&target)),
            tree: Vec::new(),
        });
    }

    if file_type.is_dir() {
        let tree = record_tree_children(path, sink, require_sink)?;
        return Ok(OutputRecord {
            path: path.to_path_buf(),
            kind: ArtifactValueKind::Tree,
            artifact_fingerprint: fingerprint_path(path)?,
            content_digest: None,
            size: 0,
            executable: false,
            symlink_target: None,
            tree,
        });
    }

    reject_special_file(path, &file_type)?;
    let size = meta.len();
    // ONE pass over the bytes when the sink can tee (the CAS can), two
    // otherwise — and none after this, because the fingerprint is
    // derived from the digest rather than re-read.
    let digest = digest_for_record(path, size, sink, require_sink)?;
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    Ok(OutputRecord {
        path: path.to_path_buf(),
        kind: ArtifactValueKind::RegularFile,
        artifact_fingerprint: fingerprint_regular_from_digest(
            path, &digest, executable,
        )?,
        content_digest: Some(digest),
        size,
        executable,
        symlink_target: None,
        tree: Vec::new(),
    })
}

fn record_tree_children(
    dir: &Path,
    sink: Option<&dyn BlobSink>,
    require_sink: bool,
) -> Result<Vec<TreeEntry>> {
    let mut entries: Vec<_> =
        std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    let mut out = Vec::new();
    for entry in entries {
        let p = entry.path();
        let meta = std::fs::symlink_metadata(&p)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if meta.file_type().is_symlink() {
            out.push(TreeEntry {
                name,
                kind: ArtifactValueKind::Symlink,
                content_digest: None,
                executable: false,
                symlink_target: Some(symlink_target_bytes(&std::fs::read_link(&p)?)),
                children: Vec::new(),
            });
        } else if meta.is_dir() {
            out.push(TreeEntry {
                name,
                kind: ArtifactValueKind::Tree,
                content_digest: None,
                executable: false,
                symlink_target: None,
                children: record_tree_children(&p, sink, require_sink)?,
            });
        } else {
            reject_special_file(&p, &meta.file_type())?;
            let digest = digest_for_record(&p, meta.len(), sink, require_sink)?;
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            out.push(TreeEntry {
                name,
                kind: ArtifactValueKind::RegularFile,
                content_digest: Some(digest),
                executable,
                symlink_target: None,
                children: Vec::new(),
            });
        }
    }
    Ok(out)
}

fn symlink_target_bytes(target: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;

        target.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        target.as_os_str().to_string_lossy().as_bytes().to_vec()
    }
}

fn digest_for_record(
    path: &Path,
    size: u64,
    sink: Option<&dyn BlobSink>,
    require_sink: bool,
) -> Result<ContentDigest> {
    match sink {
        Some(sink) if require_sink => sink.digest_and_store(path, size),
        Some(sink) => sink
            .digest_and_store(path, size)
            .or_else(|_| content_digest_of_file(path)),
        None => content_digest_of_file(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::File::create(&p).unwrap().write_all(bytes).unwrap();
        p
    }

    #[test]
    fn same_bytes_same_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a", b"hello");
        let b = write_file(dir.path(), "b", b"hello");
        assert_eq!(fingerprint_path(&a).unwrap(), fingerprint_path(&b).unwrap());
    }

    #[test]
    fn different_bytes_different_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a", b"hello");
        let b = write_file(dir.path(), "b", b"world");
        assert_ne!(fingerprint_path(&a).unwrap(), fingerprint_path(&b).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn executable_bit_changes_fingerprint() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a", b"#!/bin/sh\n");
        let before = fingerprint_path(&a).unwrap();
        let mut perms = std::fs::metadata(&a).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&a, perms).unwrap();
        assert_ne!(before, fingerprint_path(&a).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_target_is_the_value() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "t1", b"x");
        let l = dir.path().join("l");
        std::os::unix::fs::symlink("t1", &l).unwrap();
        let fp1 = fingerprint_path(&l).unwrap();
        std::fs::remove_file(&l).unwrap();
        std::os::unix::fs::symlink("t2", &l).unwrap();
        assert_ne!(fp1, fingerprint_path(&l).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn output_record_preserves_non_utf8_symlink_target() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        let target = b"target-\xff";
        std::os::unix::fs::symlink(OsStr::from_bytes(target), &link).unwrap();

        let record = record_output(&link, None).unwrap();
        assert_eq!(record.symlink_target.as_deref(), Some(target.as_slice()));
        let encoded = bincode::serialize(&record).unwrap();
        let decoded: OutputRecord = bincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn tree_fingerprint_deterministic_and_child_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("tree");
        std::fs::create_dir(&t).unwrap();
        write_file(&t, "b", b"2");
        write_file(&t, "a", b"1");
        let fp1 = fingerprint_path(&t).unwrap();
        let fp2 = fingerprint_path(&t).unwrap();
        assert_eq!(fp1, fp2);
        write_file(&t, "a", b"changed");
        assert_ne!(fp1, fingerprint_path(&t).unwrap());
    }

    #[test]
    fn legacy_stamp_bytes_are_rejected() {
        let legacy = blake3::hash(b"producer").to_hex().to_string();
        assert!(ArtifactFingerprint::parse_stamp(legacy.as_bytes()).is_none());
        let fp = ArtifactFingerprint([7u8; 32]);
        let parsed = ArtifactFingerprint::parse_stamp(fp.to_stamp().as_bytes());
        assert_eq!(parsed, Some(fp));
    }

    // ── artifact-equivalence battery ────────────────────────────────
    //
    // Requested by the 2026-08-22 external architecture review: the
    // cases where "same bytes" must NOT mean "same artifact", and the
    // cases where two different producers must safely converge.

    /// Build a GNU THIN archive naming one member. Thin archives store
    /// member PATHS, not member bytes, which is the whole point of the
    /// fold this battery exercises.
    fn write_thin_archive(
        dir: &Path,
        member: &str,
        member_len: usize,
    ) -> std::path::PathBuf {
        fn ar_header(name: &str, size: usize) -> [u8; 60] {
            let mut header = [b' '; 60];
            for (start, width, value) in [
                (0usize, 16usize, name.to_string()),
                (48, 10, size.to_string()),
            ] {
                let bytes = value.as_bytes();
                let n = bytes.len().min(width);
                header[start..start + n].copy_from_slice(&bytes[..n]);
            }
            header[58] = b'`';
            header[59] = b'\n';
            header
        }
        let archive = dir.join("libx.a");
        let mut bytes = b"!<thin>\n".to_vec();
        let table = format!("{}/\n", member);
        bytes.extend(ar_header("//", table.len()));
        bytes.extend(table.as_bytes());
        if table.len() & 1 == 1 {
            bytes.push(b'\n');
        }
        bytes.extend(ar_header("/0             /", member_len));
        std::fs::write(&archive, &bytes).unwrap();
        archive
    }

    #[test]
    fn thin_archive_value_changes_when_only_a_member_changes() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "x.o", b"object-v1");
        let archive = write_thin_archive(dir.path(), "x.o", 9);
        let bytes_before = std::fs::read(&archive).unwrap();
        let fp1 = fingerprint_path(&archive).unwrap();

        // Rewrite the MEMBER only. The archive's own bytes are untouched.
        write_file(dir.path(), "x.o", b"object-v2");
        assert_eq!(std::fs::read(&archive).unwrap(), bytes_before);
        let fp2 = fingerprint_path(&archive).unwrap();

        assert_ne!(
            fp1, fp2,
            "a thin archive whose member content changed must change value \
             even though its own bytes did not"
        );
        // And the naive byte digest must NOT change — proving the fold is
        // the only thing that catches it.
        assert_eq!(
            content_digest_of_file(&archive).unwrap(),
            content_digest_of_file(&archive).unwrap()
        );
    }

    #[test]
    fn thin_archive_missing_member_is_deterministic_and_distinct() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "x.o", b"object-v1");
        let archive = write_thin_archive(dir.path(), "x.o", 9);
        let present = fingerprint_path(&archive).unwrap();
        std::fs::remove_file(dir.path().join("x.o")).unwrap();
        let missing_a = fingerprint_path(&archive).unwrap();
        let missing_b = fingerprint_path(&archive).unwrap();
        assert_eq!(missing_a, missing_b, "missing member must be stable");
        assert_ne!(present, missing_a, "losing a member must change the value");
    }

    #[test]
    fn fingerprint_from_digest_matches_fingerprint_path() {
        let dir = tempfile::tempdir().unwrap();
        // Plain file.
        let plain = write_file(dir.path(), "plain.bin", b"hello");
        let d = content_digest_of_file(&plain).unwrap();
        assert_eq!(
            fingerprint_regular_from_digest(&plain, &d, false).unwrap(),
            fingerprint_path(&plain).unwrap()
        );
        // Executable file — the exec bit is part of the value.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let exe = write_file(dir.path(), "run.sh", b"#!/bin/sh\n");
            let mut perms = std::fs::metadata(&exe).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&exe, perms).unwrap();
            let d = content_digest_of_file(&exe).unwrap();
            assert_eq!(
                fingerprint_regular_from_digest(&exe, &d, true).unwrap(),
                fingerprint_path(&exe).unwrap()
            );
        }
        // Thin archive — the derived form must fold members identically.
        write_file(dir.path(), "x.o", b"object-v1");
        let archive = write_thin_archive(dir.path(), "x.o", 9);
        let d = content_digest_of_file(&archive).unwrap();
        assert_eq!(
            fingerprint_regular_from_digest(&archive, &d, false).unwrap(),
            fingerprint_path(&archive).unwrap()
        );
    }

    struct CountingSink {
        store_calls: std::sync::atomic::AtomicUsize,
        tee_calls: std::sync::atomic::AtomicUsize,
    }
    impl BlobSink for CountingSink {
        fn store(&self, _p: &Path, _d: &ContentDigest, _s: u64) -> Result<()> {
            self.store_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        fn digest_and_store(&self, path: &Path, _size: u64) -> Result<ContentDigest> {
            self.tee_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            content_digest_of_file(path)
        }
    }

    #[test]
    fn commit_uses_the_sinks_single_pass_entry_point() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let f = write_file(dir.path(), "out.o", b"bytes");
        let sink = CountingSink {
            store_calls: std::sync::atomic::AtomicUsize::new(0),
            tee_calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let record = record_output(&f, Some(&sink)).unwrap();
        assert_eq!(sink.tee_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            sink.store_calls.load(Ordering::Relaxed),
            0,
            "the commit path must not digest and store separately"
        );
        // The record is unchanged by the optimization.
        assert_eq!(record.artifact_fingerprint, fingerprint_path(&f).unwrap());
        assert_eq!(record.content_digest, Some(content_digest_of_file(&f).unwrap()));
    }

    struct FailingSink;

    impl BlobSink for FailingSink {
        fn store(&self, _path: &Path, _digest: &ContentDigest, _size: u64) -> Result<()> {
            anyhow::bail!("injected sink failure")
        }

        fn digest_and_store(&self, _path: &Path, _size: u64) -> Result<ContentDigest> {
            anyhow::bail!("injected sink failure")
        }
    }

    #[test]
    fn required_sink_rejects_digest_only_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "out.o", b"captured bytes");

        assert!(record_output(&file, Some(&FailingSink)).is_ok());
        assert!(record_output_with_required_sink(&file, &FailingSink).is_err());

        let tree = dir.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        write_file(&tree, "child.o", b"tree bytes");
        assert!(record_output_with_required_sink(&tree, &FailingSink).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn special_files_are_rejected_not_hung_on() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            return; // no mkfifo available; nothing to assert
        }
        // Both must FAIL rather than block forever opening the pipe.
        assert!(fingerprint_path(&fifo).is_err());
        assert!(record_output(&fifo, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn tree_child_mode_change_changes_value() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("tree");
        std::fs::create_dir(&t).unwrap();
        let child = write_file(&t, "tool", b"#!/bin/sh\n");
        let before = fingerprint_path(&t).unwrap();
        let mut perms = std::fs::metadata(&child).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&child, perms).unwrap();
        assert_ne!(
            before,
            fingerprint_path(&t).unwrap(),
            "a child becoming executable changes what a consumer observes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tree_symlink_target_change_changes_value() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("tree");
        std::fs::create_dir(&t).unwrap();
        write_file(&t, "a", b"1");
        write_file(&t, "b", b"2");
        let link = t.join("link");
        std::os::unix::fs::symlink("a", &link).unwrap();
        let before = fingerprint_path(&t).unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("b", &link).unwrap();
        assert_ne!(before, fingerprint_path(&t).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_value_is_the_target_path_not_its_content() {
        let dir = tempfile::tempdir().unwrap();
        let target = write_file(dir.path(), "t", b"v1");
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let before = fingerprint_path(&link).unwrap();
        std::fs::write(&target, b"v2-different-length").unwrap();
        assert_eq!(
            before,
            fingerprint_path(&link).unwrap(),
            "a symlink's value is where it points; the target is declared \
             separately by whoever consumes it"
        );
    }

    #[test]
    fn identical_bytes_from_different_producers_converge() {
        // The early-cutoff corollary: value, not provenance. Two files
        // written by "different producers" with identical content and
        // mode must be interchangeable to a consumer.
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a.o", b"same");
        let b = write_file(dir.path(), "b.o", b"same");
        assert_eq!(
            fingerprint_path(&a).unwrap(),
            fingerprint_path(&b).unwrap()
        );
    }
}
