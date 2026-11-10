//! Cold-daemon no-op certificate — the disk-backed first-no-op fast path.
//!
//! ## Why this exists
//!
//! rush's persistent daemon (`rushd`) collapses the WARM no-op to ~0.10s by
//! retaining the fully-analyzed [`AnalysisSnapshot`](crate::analysis_snapshot)
//! in memory and, on a reuse hit, deciding the build is a true no-op when:
//!
//!   1. `snapshot.manifest.validate(&semantics) == Fresh` — no analysis input
//!      (`.bzl` / `BUILD` / glob dir-listing / config fingerprint) moved, AND
//!   2. `snapshot.retained_execution_state.ledger.verify() == Ok` — no file in
//!      the fixed action graph (inputs / outputs / sources) moved.
//!
//! When both hold the whole PASS-B wavefront is skipped and the command returns
//! `Ok` **without ever touching the graph's `Arc<dyn RuleInstance>` nodes**.
//!
//! But the [`SnapshotCache`](crate::analysis_snapshot::SnapshotCache) is purely
//! in-memory: a COLD / restarted daemon has nothing to reuse, so its FIRST
//! no-op of an unchanged workspace re-runs full analysis (`.bzl` eval + `BUILD`
//! parse + PASS-A) from scratch — ~177s for a big target — before it can prove
//! the build was a no-op.
//!
//! ## The certificate
//!
//! The KEY OBSERVATION is that the two-part no-op decision above needs ONLY the
//! `DependencyManifest` and the `FreshnessLedger` — NEITHER requires the
//! un-serializable `Arc<dyn RuleInstance>` graph. Both are plain data. So after
//! a fully-successful cold build we persist a *certificate* — the manifest + the
//! ledger + the [`AnalysisKey`] + a compat/schema stamp — to disk. On a cold
//! daemon's first command we load the certificate for the command's key and run
//! the SAME two-part decision against the live filesystem. If it passes, the
//! workspace is unchanged and fully built: we report a no-op and return,
//! skipping `.bzl` eval, `BUILD` parse, PASS-A, and the wavefront entirely.
//!
//! The graph is reconstructed (full analysis) ONLY when something actually
//! changed — which already requires full analysis today.
//!
//! ## Correctness (Codex gpt-5.5 xhigh review, thread 019f01fb)
//!
//! For the EXACT same [`AnalysisKey`], "persisted manifest validates `Fresh`
//! against the new semantics AND persisted ledger verifies `Ok`" is SUFFICIENT
//! to declare a cold no-op without re-running analysis — it is precisely the
//! warm path's decision, sourced from disk instead of RAM. Guards:
//!
//!   * The [`AnalysisKey::daemon_compat_fingerprint`] (now folding the running
//!     rush binary's content digest) gates the record: a different / rebuilt
//!     rush binary produces a different key hash AND a different stored compat
//!     fingerprint, so it never trusts a stale certificate.
//!   * The schema version + stored full key are compared on read; the filename
//!     hash is only an index.
//!   * The certificate is written ATOMICALLY (unique temp file + rename) and
//!     ONLY after a fully-successful build with a non-empty ledger.
//!   * Corrupt / partial / wrong-schema / wrong-key / wrong-compat / decode-fail
//!     / empty-ledger records are treated as a cache MISS, never an error — the
//!     command falls through to full analysis.
//!   * Exact key matching only: no subset / superset target-pattern reuse.
//!
//! A stale certificate is impossible to act on unsoundly: even a (hypothetical)
//! collision still has to pass `manifest.validate()` + `ledger.verify()` against
//! the real filesystem before it serves a no-op.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use rush_incremental::{DependencyManifest, FileMeta};

use crate::analysis_snapshot::AnalysisKey;
use crate::freshness_ledger::FreshnessLedger;

/// Bump when the on-disk record layout changes incompatibly. An older-schema
/// record is a cache MISS (never an error).
// Version 2 adds the explicit FileContent descriptor-binding Option field.
const SCHEMA_VERSION: u32 = 2;

/// Subdirectory under the workspace cache root holding cold-no-op certificates.
const CACHE_SUBDIR: &str = "daemon-noop-cache";

/// One unique path's recorded metadata, serializable form of a ledger entry.
#[derive(serde::Serialize, serde::Deserialize)]
struct LedgerEntry {
    path: PathBuf,
    meta: FileMeta,
}

/// The persisted cold-no-op certificate for one [`AnalysisKey`].
///
/// Self-describing: it stores the full key (compared on read; the filename hash
/// is only an index) plus the schema/compat stamp. The manifest + ledger are
/// the two-part no-op proof.
#[derive(serde::Serialize, serde::Deserialize)]
struct Certificate {
    schema_version: u32,
    /// The stable hash of `key` at write time — re-checked on read so a record
    /// that landed in the wrong file (should never happen) is rejected.
    key_hash: [u8; 32],
    /// The full analysis key. Compared field-for-field against the cold
    /// command's key before the manifest/ledger are trusted.
    key: AnalysisKey,
    /// The analysis-input admission proof.
    manifest: DependencyManifest,
    /// The action-graph file-state table.
    ledger_paths: Vec<LedgerEntry>,
    /// Number of declared actions the ledger was built from (diagnostics only).
    ledger_action_count: usize,
    /// Wall-clock write time (diagnostics only).
    created_unix: u64,
}

/// The directory holding cold-no-op certificates for this workspace.
fn cache_dir(workspace_root: &Path) -> PathBuf {
    crate::cache_paths::workspace_cache_root(workspace_root).join(CACHE_SUBDIR)
}

/// The certificate path for `key`: `<cache>/daemon-noop-cache/<aa>/<full-hash>.bin`,
/// sharded by the first hex byte to keep any one directory small.
fn certificate_path(workspace_root: &Path, key: &AnalysisKey) -> PathBuf {
    let hash = key.stable_hash();
    let hex = hex_lower(&hash);
    let shard = &hex[..2];
    cache_dir(workspace_root)
        .join(shard)
        .join(format!("{hex}.bin"))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// Persist the cold-no-op certificate for `key` from the just-built manifest +
/// ledger. Called ONLY after a fully-successful build with a non-empty ledger.
///
/// Atomic: serializes to a unique temp file in the destination directory, fsyncs
/// it, then renames into place. A failure here is logged and swallowed — a
/// missing/partial certificate is just a future cache miss, never a build error.
pub fn persist(
    workspace_root: &Path,
    key: &AnalysisKey,
    manifest: &DependencyManifest,
    ledger: &FreshnessLedger,
) {
    // Never persist a useless certificate: an empty ledger always verifies as a
    // MISS (see FreshnessLedger::verify), so it could never serve a cold no-op.
    if ledger.is_empty() {
        return;
    }
    let dest = certificate_path(workspace_root, key);
    let Some(dir) = dest.parent() else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("⚠️  cold-no-op cache: create_dir_all failed: {e}");
        return;
    }

    let ledger_paths: Vec<LedgerEntry> = ledger
        .paths()
        .iter()
        .map(|(path, meta)| LedgerEntry {
            path: path.clone(),
            meta: *meta,
        })
        .collect();
    let cert = Certificate {
        schema_version: SCHEMA_VERSION,
        key_hash: key.stable_hash(),
        key: key.clone(),
        manifest: manifest.clone(),
        ledger_paths,
        ledger_action_count: ledger.action_count(),
        created_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };

    let bytes = match bincode::serialize(&cert) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("⚠️  cold-no-op cache: serialize failed: {e}");
            return;
        }
    };

    // Unique temp file in the SAME directory (so rename is atomic on the same
    // filesystem), write, fsync, rename.
    let tmp = dir.join(format!(
        ".tmp-{}-{}.bin",
        std::process::id(),
        cert.created_unix
    ));
    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        eprintln!("⚠️  cold-no-op cache: temp write failed: {e}");
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        eprintln!("⚠️  cold-no-op cache: rename failed: {e}");
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    // Best-effort directory fsync for durability of the rename.
    if let Ok(dh) = std::fs::File::open(dir) {
        let _ = dh.sync_all();
    }
    eprintln!(
        "💾 cold-no-op certificate written ({} manifest deps, {} ledger paths)",
        manifest.len(),
        cert.ledger_paths.len()
    );
}

/// A loaded certificate's manifest + reconstructed ledger, ready for the
/// two-part no-op decision. The graph is intentionally absent — a hit serves a
/// no-op without it; a miss falls through to full analysis.
pub struct LoadedCertificate {
    pub manifest: DependencyManifest,
    pub ledger: FreshnessLedger,
}

/// Load and authenticate the cold-no-op certificate for `key`.
///
/// Returns `Some` only when the file exists, decodes, has the current schema,
/// and its stored key matches `key` EXACTLY (which already includes the
/// daemon-compat fingerprint folding the running binary's identity). Any other
/// outcome — missing, corrupt, wrong schema, key mismatch, empty ledger — is a
/// cache MISS (`None`), never an error: the caller falls through to full
/// analysis. The returned manifest/ledger must STILL be validated against the
/// live filesystem by the caller before a no-op is served.
pub fn load(workspace_root: &Path, key: &AnalysisKey) -> Option<LoadedCertificate> {
    let path = certificate_path(workspace_root, key);
    let bytes = std::fs::read(&path).ok()?;
    // Reject older layouts before decoding the changed dependency payload.
    // Fixed-width little-endian encoding matches bincode::serialize below.
    let version = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
    if version != SCHEMA_VERSION {
        return None;
    }
    let cert: Certificate = match bincode::deserialize(&bytes) {
        Ok(c) => c,
        Err(_) => {
            // Corrupt / wrong-version layout: drop it so it stops costing reads.
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    if cert.schema_version != SCHEMA_VERSION {
        return None;
    }
    // Exact key match (the filename hash is only an index, not authority). The
    // key carries the daemon-compat fingerprint, so a different/rebuilt binary
    // is rejected here.
    if &cert.key != key || cert.key_hash != key.stable_hash() {
        return None;
    }
    let mut paths: HashMap<PathBuf, FileMeta> = HashMap::with_capacity(cert.ledger_paths.len());
    for e in cert.ledger_paths {
        paths.insert(e.path, e.meta);
    }
    let ledger = FreshnessLedger::from_parts(paths, cert.ledger_action_count);
    if ledger.is_empty() {
        return None;
    }
    Some(LoadedCertificate {
        manifest: cert.manifest,
        ledger,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis_snapshot::daemon_compat_fingerprint;
    use std::io::Write as _;

    /// Build an `AnalysisKey` directly (persist/load need only the key, not
    /// `CommandSemantics`), so the test does not depend on the private
    /// `CommandSemantics::from_parts` constructor.
    fn key_for(ws: &Path, patterns: &[&str]) -> AnalysisKey {
        AnalysisKey {
            workspace_realpath: ws.to_path_buf(),
            output_base: ws.as_os_str().as_encoded_bytes().to_vec(),
            command_kind: Some("build".to_string()),
            ordered_target_patterns: patterns.iter().map(|s| s.to_string()).collect(),
            config_fingerprint: [7u8; 32],
            daemon_compat_fingerprint: daemon_compat_fingerprint(),
        }
    }

    #[test]
    fn roundtrip_persist_load_then_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();

        // A real file the ledger tracks.
        let tracked = ws.join("out.o");
        {
            let mut f = std::fs::File::create(&tracked).unwrap();
            f.write_all(b"hello").unwrap();
        }
        let mut paths = HashMap::new();
        paths.insert(tracked.clone(), rush_incremental::stat_meta(&tracked));
        let ledger = FreshnessLedger::from_parts(paths, 1);
        let manifest = DependencyManifest::from_deps(vec![rush_incremental::Dependency::FileContent {
            path: tracked.clone(),
            digest: rush_incremental::content_digest_of_regular_file(&tracked, 1 << 20).unwrap(),
            binding: None,
        }]);
        let key = key_for(ws, &["//:foo"]);

        persist(ws, &key, &manifest, &ledger);

        let loaded = load(ws, &key).expect("certificate must load");
        assert_eq!(loaded.manifest, manifest);
        // The reconstructed ledger verifies clean against the unchanged file.
        assert!(loaded.ledger.verify().is_ok());

        // Touch the file → reconstructed ledger now MISSES (proves the loaded
        // ledger is a faithful, equally-strong replay).
        std::thread::sleep(std::time::Duration::from_millis(20));
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&tracked)
                .unwrap();
            f.write_all(b"HELLO").unwrap();
        }
        let loaded2 = load(ws, &key).expect("certificate still loads");
        assert!(
            loaded2.ledger.verify().is_err(),
            "a touched tracked file must invalidate the reconstructed ledger"
        );
    }

    #[test]
    fn legacy_binary_dependency_layout_is_a_miss_before_payload_decode() {
        #[derive(serde::Serialize)]
        enum LegacyDependency {
            FileContent { path: PathBuf, digest: [u8; 32] },
        }
        #[derive(serde::Serialize)]
        struct LegacyManifest {
            deps: Vec<LegacyDependency>,
        }
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let tracked = ws.join("out.o");
        std::fs::write(&tracked, b"hello").unwrap();
        let key = key_for(ws, &["//:foo"]);
        let legacy = bincode::serialize(&(
            1u32,
            key.stable_hash(),
            &key,
            LegacyManifest {
                deps: vec![LegacyDependency::FileContent {
                    path: tracked.clone(),
                    digest: rush_incremental::content_digest_of_regular_file(&tracked, 1 << 20)
                        .unwrap(),
                }],
            },
            vec![LedgerEntry {
                path: tracked.clone(),
                meta: rush_incremental::stat_meta(&tracked),
            }],
            1usize,
            0u64,
        ))
        .unwrap();
        let path = certificate_path(ws, &key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &legacy).unwrap();
        assert!(load(ws, &key).is_none());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            legacy,
            "recognized legacy layouts must not be decoded as corruption or removed"
        );
    }

    #[test]
    fn different_key_is_a_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();

        let tracked = ws.join("a.o");
        std::fs::write(&tracked, b"x").unwrap();
        let mut paths = HashMap::new();
        paths.insert(tracked.clone(), rush_incremental::stat_meta(&tracked));
        let ledger = FreshnessLedger::from_parts(paths, 1);
        let manifest = DependencyManifest::from_deps(Vec::new());

        let key_a = key_for(ws, &["//:a"]);
        persist(ws, &key_a, &manifest, &ledger);

        // A DIFFERENT target pattern => different key => MISS.
        let key_b = key_for(ws, &["//:b"]);
        assert!(load(ws, &key_b).is_none());
        // The original still hits.
        assert!(load(ws, &key_a).is_some());
    }

    #[test]
    fn empty_ledger_is_never_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let key = key_for(ws, &["//:empty"]);
        let ledger = FreshnessLedger::from_parts(HashMap::new(), 0);
        persist(
            ws,
            &key,
            &DependencyManifest::from_deps(Vec::new()),
            &ledger,
        );
        assert!(load(ws, &key).is_none(), "empty-ledger cert must not exist");
    }
}
