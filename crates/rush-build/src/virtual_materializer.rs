// Rush Build - CAS materializer for virtual artifacts
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 4: on-demand materialization of virtual CAS-backed artifacts.
//!
//! [`rush_action::virtual_artifacts`] holds the process-global registry
//! of outputs whose bytes live only in the CAS; this module turns those
//! records back into files when something genuinely needs to read them:
//! a cache-MISSing action's inputs (JIT), the dependency artifacts of a
//! rule that reads files outside the action manager (pre-build), and
//! the outputs of the targets the user asked for (top-level).
//!
//! Shared restore primitives ([`materialize_record`]) are also used by
//! the action manager's full CAS restore path so the two never drift.

use anyhow::{anyhow, Result};
use rush_action::artifact_value::{
    ArtifactValueKind, OutputRecord, TreeEntry,
};
use rush_action::virtual_artifacts::{self, MaterializePurpose};
use rush_cache::cas::CasStore;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Producer mnemonics whose outputs may stay CAS-backed on a cache hit.
/// Every consumer of these outputs either declares them as action
/// inputs (JIT covers execution) or is caught by the pre-build /
/// top-level materialization sweeps. Grow this list only after auditing
/// the consumers of a mnemonic's outputs.
pub fn virtual_mnemonic(mnemonic: &str) -> bool {
    matches!(
        mnemonic,
        "MojoPrecompile"
            | "MojoCompile"
            | "CppCompile"
            | "CppArchive"
            | "CcArchive"
            | "CppLink"
            | "CcLink"
    )
}

/// Rule kinds audited to read dependency outputs ONLY through actions
/// with declared inputs (or not at all): their dep artifacts may stay
/// virtual. Everything else gets its dep artifacts materialized before
/// `build()` runs — conservative for rules like genrule / py_* / uv
/// that read files directly.
pub fn rule_consumes_only_via_actions(rule_type: &str) -> bool {
    matches!(
        rule_type,
        "mojo_library"
            | "mojo_binary"
            | "mojo_test"
            | "cc_library"
            | "cc_binary"
            | "cc_test"
            | "filegroup"
            | "alias"
            | "source_file"
    )
}

/// Process-global CAS handle for materialization, sharing the action
/// cache root. `None` when the CAS cannot be opened — virtual hits are
/// then refused at the action manager (its own handle also fails), so
/// lookups here only fire for records that were never registered.
fn cas() -> Option<&'static CasStore> {
    static CAS: OnceLock<Option<CasStore>> = OnceLock::new();
    CAS.get_or_init(|| CasStore::new(&rush_cache::roots::cas_store_root()).ok())
        .as_ref()
}

fn set_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(p)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(p, perms)?;
    Ok(())
}

fn restore_tree(cas: &CasStore, dir: &Path, entries: &[TreeEntry]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    for e in entries {
        let p = dir.join(&e.name);
        match e.kind {
            ArtifactValueKind::RegularFile => {
                let digest = e
                    .content_digest
                    .ok_or_else(|| anyhow!("tree entry missing digest"))?;
                cas.materialize(&digest, &p)?;
                if e.executable {
                    set_executable(&p)?;
                }
            }
            ArtifactValueKind::Symlink => {
                let target = e
                    .symlink_target
                    .as_ref()
                    .ok_or_else(|| anyhow!("tree entry missing target"))?;
                let _ = std::fs::remove_file(&p);
                std::os::unix::fs::symlink(OsStr::from_bytes(target), &p)?;
            }
            ArtifactValueKind::Tree => restore_tree(cas, &p, &e.children)?,
        }
    }
    Ok(())
}

/// Materialize one committed output record from the CAS. Used both for full
/// cache-hit restores and for on-demand virtual materialization. Value
/// verification stays in the action record; no output-adjacent state is
/// published.
pub fn materialize_record(cas: &CasStore, record: &OutputRecord) -> Result<()> {
    match record.kind {
        ArtifactValueKind::RegularFile => {
            let digest = record
                .content_digest
                .ok_or_else(|| anyhow!("record missing digest"))?;
            cas.materialize(&digest, &record.path)?;
            if record.executable {
                set_executable(&record.path)?;
            }
        }
        ArtifactValueKind::Symlink => {
            let target = record
                .symlink_target
                .as_ref()
                .ok_or_else(|| anyhow!("record missing target"))?;
            if let Some(parent) = record.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let _ = std::fs::remove_file(&record.path);
            std::os::unix::fs::symlink(OsStr::from_bytes(target), &record.path)?;
        }
        ArtifactValueKind::Tree => {
            restore_tree(cas, &record.path, &record.tree)?;
        }
    }
    Ok(())
}

/// Whether every blob a set of records references is PRESENT in the
/// CAS (presence only — content is digest-verified at materialization).
/// The virtual-hit gate calls this so a hit is only taken when a later
/// consumer can actually get the bytes back.
pub fn records_available(cas: &CasStore, records: &[OutputRecord]) -> bool {
    cas.records_available(records)
}

const THIN_ARCHIVE_MAGIC: &[u8; 8] = b"!<thin>\n";

fn is_thin_archive(path: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map(|_| &magic == THIN_ARCHIVE_MAGIC)
        .unwrap_or(false)
}

/// Per-destination materialization lock.
///
/// Two concurrent consumers of the SAME virtual input (an archive
/// shared by dozens of links, say) must not restore it at once: the
/// second writer would race the first through the CAS temp file, and —
/// worse for thin archives — a consumer that arrives after the archive
/// is published but before its members are could execute against a
/// half-populated closure. One lock per path makes materialization of
/// a path (and of the structure it requires) atomic to observers.
fn path_lock(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|p| p.into_inner());
    Arc::clone(
        guard
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

/// Ensure the file behind `path` exists on disk, materializing it (and
/// anything it structurally requires) from the CAS when it is a
/// registered virtual artifact. Returns the bytes written (0 when the
/// path and everything it requires were already present, or the path
/// was never virtual).
///
/// Structural closure rules:
/// * A symlink (present or dangling) is chased one level per hop so a
///   staged link to a virtual target materializes the target.
/// * A THIN archive references member files by path, so its members are
///   materialized too — INCLUDING when the archive itself was already
///   on disk. Archive-present / members-virtual is the steady state this
///   phase creates (the two come from separate actions with independent
///   hit gates), and a thin archive without its members is useless to a
///   linker.
///
/// A registered record whose blobs are gone is a hard error: the
/// producing action already virtual-hit, so re-execution is no longer
/// schedulable this build. `CasStore::materialize` quarantines corrupt
/// blobs, so the NEXT build misses and re-executes (self-healing).
pub fn ensure_path_materialized(
    path: &Path,
    purpose: MaterializePurpose,
) -> Result<u64> {
    if !virtual_artifacts::enabled() {
        return Ok(0);
    }
    let mut bytes = 0u64;
    // Chase symlinks manually: a DANGLING staged link must still lead
    // us to its (virtual) target. Bounded to avoid link cycles.
    let mut current = path.to_path_buf();
    for _ in 0..16 {
        let is_link = std::fs::symlink_metadata(&current)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if !is_link {
            break;
        }
        let target = std::fs::read_link(&current)?;
        current = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .map(|p| p.join(&target))
                .unwrap_or(target)
        };
    }

    let lock = path_lock(&current);
    let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());

    if !current.exists() {
        let Some(record) = virtual_artifacts::lookup(&current) else {
            // Not a virtual artifact: absent for some other reason — the
            // consumer surfaces its own (pre-existing) error if it cares.
            return Ok(bytes);
        };
        let cas = cas().ok_or_else(|| {
            anyhow!(
                "virtual artifact {} is registered but the CAS is unavailable",
                current.display()
            )
        })?;
        materialize_record(cas, &record)?;
        virtual_artifacts::note_materialized(purpose, &current, record.size);
        bytes += record.size;
    }

    // A thin archive's bytes only NAME its members; a reader needs the
    // member files too, whether we just restored the archive or found
    // it already on disk.
    if is_thin_archive(&current) {
        for member in rush_action::thin_archive_member_paths(&current)? {
            bytes += ensure_path_materialized(&member, purpose)?;
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_action::artifact_value::record_output;

    struct CasSink<'a>(&'a CasStore);
    impl rush_action::artifact_value::BlobSink for CasSink<'_> {
        fn store(
            &self,
            path: &Path,
            digest: &rush_action::artifact_value::ContentDigest,
            _size: u64,
        ) -> Result<()> {
            self.0.store_file(path, digest)
        }
    }

    #[test]
    fn materialize_record_roundtrips_file_without_adjacent_state() {
        let dir = tempfile::tempdir().unwrap();
        let cas = CasStore::new(dir.path()).unwrap();
        let out = dir.path().join("pkg/lib.o");
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::fs::write(&out, b"object bytes").unwrap();
        let record = record_output(&out, Some(&CasSink(&cas))).unwrap();
        assert!(records_available(&cas, std::slice::from_ref(&record)));

        std::fs::remove_file(&out).unwrap();
        materialize_record(&cas, &record).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"object bytes");
        assert!(
            !dir.path().join("pkg/lib.o.rushid").exists(),
            "CAS restoration must not publish output-adjacent Rush state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialize_record_roundtrips_non_utf8_symlink_target() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().unwrap();
        let cas = CasStore::new(dir.path()).unwrap();
        let output = dir.path().join("pkg/link");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        let target = b"target-\xff";
        std::os::unix::fs::symlink(OsStr::from_bytes(target), &output).unwrap();
        let record = record_output(&output, Some(&CasSink(&cas))).unwrap();
        assert!(records_available(&cas, std::slice::from_ref(&record)));

        std::fs::remove_file(&output).unwrap();
        materialize_record(&cas, &record).unwrap();
        let restored = std::fs::read_link(&output).unwrap();
        assert_eq!(restored.as_os_str().as_bytes(), target);
    }

    #[cfg(unix)]
    #[test]
    fn materialize_tree_roundtrips_non_utf8_symlink_target() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().unwrap();
        let cas = CasStore::new(dir.path()).unwrap();
        let output = dir.path().join("tree");
        std::fs::create_dir(&output).unwrap();
        let link = output.join("link");
        let target = b"tree-target-\xff";
        std::os::unix::fs::symlink(OsStr::from_bytes(target), &link).unwrap();
        let record = record_output(&output, Some(&CasSink(&cas))).unwrap();
        assert!(records_available(&cas, std::slice::from_ref(&record)));

        std::fs::remove_dir_all(&output).unwrap();
        materialize_record(&cas, &record).unwrap();
        let restored = std::fs::read_link(output.join("link")).unwrap();
        assert_eq!(restored.as_os_str().as_bytes(), target);
    }

    #[test]
    fn records_available_detects_missing_blob() {
        let dir = tempfile::tempdir().unwrap();
        let cas = CasStore::new(dir.path()).unwrap();
        let out = dir.path().join("a.bin");
        std::fs::write(&out, b"data").unwrap();
        // No sink: the record exists but its blob was never stored.
        let record = record_output(&out, None).unwrap();
        assert!(!records_available(&cas, std::slice::from_ref(&record)));
        assert!(!records_available(&cas, &[]));
    }

    #[test]
    fn thin_archive_magic_detection() {
        let dir = tempfile::tempdir().unwrap();
        let thin = dir.path().join("t.a");
        std::fs::write(&thin, b"!<thin>\nrest").unwrap();
        assert!(is_thin_archive(&thin));
        let fat = dir.path().join("f.a");
        std::fs::write(&fat, b"!<arch>\nrest").unwrap();
        assert!(!is_thin_archive(&fat));
        assert!(!is_thin_archive(&dir.path().join("missing.a")));
    }
}
