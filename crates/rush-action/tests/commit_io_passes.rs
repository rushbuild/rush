// Rush Action - measured I/O cost of committing an output
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! How many times does committing a cached output read its bytes?
//!
//! The 2026-08-22 external review observed that the commit path claimed
//! to be single-pass and was not: it digested the file, copied it into
//! the CAS, then re-hashed it to build the fingerprint. This test pins
//! the answer by MEASURING `/proc/self/io` `rchar` rather than by
//! reading the code, and it compares the shipped path against an
//! emulation of the old one so the saving is a number, not a claim.

use rush_action::artifact_value::{
    content_digest_of_file, fingerprint_path, record_output, BlobSink,
    ContentDigest,
};
use std::path::Path;

fn rchar() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("rchar: ")?.trim().parse().ok())
        })
        .unwrap_or(0)
}

/// A sink that writes the blob out, like the real CAS does.
struct CopyingSink {
    root: std::path::PathBuf,
}

impl CopyingSink {
    fn blob(&self, digest: &ContentDigest) -> std::path::PathBuf {
        self.root.join(digest.to_hex())
    }
}

impl BlobSink for CopyingSink {
    fn store(
        &self,
        path: &Path,
        digest: &ContentDigest,
        _size: u64,
    ) -> anyhow::Result<()> {
        let dest = self.blob(digest);
        if !dest.is_file() {
            std::fs::copy(path, &dest)?;
        }
        Ok(())
    }

    fn digest_and_store(
        &self,
        path: &Path,
        _size: u64,
    ) -> anyhow::Result<ContentDigest> {
        // One pass: hash while writing the blob.
        use std::io::{Read, Write};
        let tmp = self.root.join("incoming.tmp");
        let mut src = std::fs::File::open(path)?;
        let mut out = std::fs::File::create(&tmp)?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        drop(out);
        let digest = ContentDigest(*hasher.finalize().as_bytes());
        std::fs::rename(&tmp, self.blob(&digest))?;
        Ok(digest)
    }
}

/// What the commit path used to do: digest, store (copying), then
/// re-derive the fingerprint by re-reading the file.
fn legacy_commit(path: &Path, sink: &CopyingSink) -> anyhow::Result<()> {
    let digest = content_digest_of_file(path)?;
    let size = std::fs::metadata(path)?.len();
    sink.store(path, &digest, size)?;
    let _fp = fingerprint_path(path)?;
    Ok(())
}

#[test]
fn committing_an_output_reads_it_once() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = dir.path().join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    let sink = CopyingSink { root: blobs };

    // 64 outputs x 1 MiB. Big enough that per-file overhead is noise.
    let payload: Vec<u8> = (0..(1024 * 1024)).map(|i| (i % 251) as u8).collect();
    let mut files = Vec::new();
    for i in 0..64 {
        let p = dir.path().join(format!("out-{i}.o"));
        let mut bytes = payload.clone();
        bytes[0] = i as u8; // distinct content per file
        std::fs::write(&p, &bytes).unwrap();
        files.push(p);
    }
    let total: u64 = files.len() as u64 * payload.len() as u64;

    let t0 = rchar();
    for f in &files {
        legacy_commit(f, &sink).unwrap();
    }
    let legacy_bytes = rchar() - t0;

    // Fresh blob dir so the shipped path does the same real work.
    let blobs2 = dir.path().join("blobs2");
    std::fs::create_dir_all(&blobs2).unwrap();
    let sink2 = CopyingSink { root: blobs2 };
    let t1 = rchar();
    for f in &files {
        record_output(f, Some(&sink2)).unwrap();
    }
    let current_bytes = rchar() - t1;

    let legacy_passes = legacy_bytes as f64 / total as f64;
    let current_passes = current_bytes as f64 / total as f64;
    println!(
        "commit I/O over {} MiB: legacy {:.2} passes ({} B), \
         current {:.2} passes ({} B)",
        total / (1024 * 1024),
        legacy_passes,
        legacy_bytes,
        current_passes,
        current_bytes
    );

    // The shipped path must be at least a full pass cheaper, and must
    // be within noise of a single pass.
    assert!(
        current_passes < legacy_passes - 0.8,
        "expected the commit path to save ~1 full read pass: \
         legacy {legacy_passes:.2} vs current {current_passes:.2}"
    );
    assert!(
        current_passes < 1.35,
        "commit should read each output about once, got {current_passes:.2}"
    );
}
