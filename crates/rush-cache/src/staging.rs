//! Private action-result staging for whole-capture certification.
//!
//! An external analyzer may produce thousands of individually valid action
//! results before Rush knows whether the complete capture is reusable. Writing
//! those records into the normal action cache as they arrive exposes a partial
//! capture. This module keeps them in a caller-owned private directory, checks
//! the exact expected action-ID set, seals its content digests durably, and
//! only then permits publication through [`crate::ActionCache`]. CAS blobs are
//! deliberately outside this transaction: content-addressed orphans are safe.

use crate::{cas::CasStore, ActionCache};
use anyhow::{Context, Result};
use rush_action::ActionId;
use rush_exec::ActionResult;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SEAL_SCHEMA: &str = "org.rushbuild.action-cache-stage";
const SEAL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SealedEntry {
    action_id: ActionId,
    digest: [u8; 32],
}

#[derive(Debug, Serialize, Deserialize)]
struct StageSeal {
    schema: String,
    version: u32,
    entries: Vec<SealedEntry>,
}

/// Journal-safe identity of one certified action-result stage.
///
/// The caller must commit this receipt in its own capture transaction before
/// relying on a stage after restart. The digest is deliberately not recovered
/// from the mutable stage itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionCacheStageReceipt {
    seal_digest: [u8; 32],
    entry_count: u64,
}

impl ActionCacheStageReceipt {
    /// Digest of the exact durable stage seal.
    pub fn seal_digest(&self) -> [u8; 32] {
        self.seal_digest
    }

    /// Number of action results certified by the seal.
    pub fn entry_count(&self) -> u64 {
        self.entry_count
    }
}

/// A private, not-yet-reusable set of captured action results.
pub struct StagedActionCache {
    directory: PathBuf,
    writer: Mutex<()>,
}

impl StagedActionCache {
    /// Create a new private staging directory at an exact caller-owned path.
    /// Existing paths are refused so stale or foreign records cannot be mixed
    /// into the current capture.
    pub fn create(directory: &Path) -> Result<Self> {
        let parent = directory
            .parent()
            .context("action-cache stage has no parent directory")?;
        if !parent.is_dir() {
            anyhow::bail!("action-cache stage parent is not a directory");
        }
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(directory)
            .context("create private action-cache stage")?;
        set_private_directory(directory)?;
        sync_directory(parent)?;
        Ok(Self {
            directory: directory.to_path_buf(),
            writer: Mutex::new(()),
        })
    }

    /// The private directory owned by this stage.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Durably stage one result without exposing it through normal lookup.
    /// Repeating an identical record is idempotent; a different record for the
    /// same action ID fails closed.
    pub fn put(&self, action_id: ActionId, result: &ActionResult) -> Result<()> {
        if result.action_id != action_id {
            anyhow::bail!("staged action result does not match its action ID");
        }
        let _writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("action-cache stage writer lock is poisoned"))?;
        if self.seal_path().exists() {
            anyhow::bail!("sealed action-cache stage cannot accept more results");
        }
        let bytes = bincode::serialize(result).context("serialize staged action result")?;
        let destination = self.entry_path(&action_id);
        match fs::read(&destination) {
            Ok(existing) if existing == bytes => return Ok(()),
            Ok(_) => anyhow::bail!("conflicting staged result for one action ID"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("read existing staged action result"),
        }
        atomic_write_private(&destination, &bytes)?;
        Ok(())
    }

    /// Verify that the stage contains exactly one valid result for every
    /// expected action ID and publish a durable content seal. No normal cache
    /// entry becomes visible at this boundary.
    pub fn certify(self, expected: &[ActionId]) -> Result<CertifiedActionCacheStage> {
        let _writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("action-cache stage writer lock is poisoned"))?;
        if self.seal_path().exists() {
            anyhow::bail!("action-cache stage is already sealed");
        }
        let expected_set = expected.iter().cloned().collect::<HashSet<_>>();
        if expected_set.len() != expected.len() {
            anyhow::bail!("expected action-ID set contains duplicates");
        }

        let mut entries = Vec::new();
        let mut observed = HashSet::new();
        for item in fs::read_dir(&self.directory).context("read action-cache stage")? {
            let item = item.context("read action-cache stage entry")?;
            if !item
                .file_type()
                .context("inspect action-cache stage entry")?
                .is_file()
                || item.path().extension().and_then(|value| value.to_str()) != Some("cache")
            {
                anyhow::bail!("action-cache stage contains an unexpected entry");
            }
            let bytes = fs::read(item.path()).context("read staged action result")?;
            let result: ActionResult =
                bincode::deserialize(&bytes).context("decode staged action result")?;
            if item.file_name() != entry_file_name(&result.action_id) {
                anyhow::bail!("staged action-result filename does not match its ID");
            }
            if !observed.insert(result.action_id.clone()) {
                anyhow::bail!("action-cache stage contains a duplicate action ID");
            }
            entries.push(SealedEntry {
                action_id: result.action_id,
                digest: *blake3::hash(&bytes).as_bytes(),
            });
        }
        if observed != expected_set {
            anyhow::bail!("action-cache stage does not match the expected action-ID set");
        }
        entries.sort_by_key(|entry| entry.action_id.to_string());
        let seal = StageSeal {
            schema: SEAL_SCHEMA.to_string(),
            version: SEAL_VERSION,
            entries,
        };
        let seal_bytes = bincode::serialize(&seal)?;
        let seal_digest = *blake3::hash(&seal_bytes).as_bytes();
        atomic_write_private(&self.seal_path(), &seal_bytes)?;
        drop(_writer);
        Ok(CertifiedActionCacheStage {
            directory: self.directory,
            receipt: ActionCacheStageReceipt {
                seal_digest,
                entry_count: expected.len() as u64,
            },
        })
    }

    fn seal_path(&self) -> PathBuf {
        self.directory.join("sealed")
    }

    fn entry_path(&self, action_id: &ActionId) -> PathBuf {
        self.directory.join(entry_file_name(action_id))
    }
}

/// A complete, digest-sealed action-result set eligible for publication.
#[derive(Debug)]
pub struct CertifiedActionCacheStage {
    directory: PathBuf,
    receipt: ActionCacheStageReceipt,
}

impl CertifiedActionCacheStage {
    /// Reopen a stage whose receipt was anchored by the caller's durable
    /// capture journal. The stage remains untrusted until publication checks
    /// the seal and every result against this receipt.
    pub fn open(directory: &Path, receipt: ActionCacheStageReceipt) -> Result<Self> {
        let metadata = fs::symlink_metadata(directory)
            .context("inspect certified action-cache stage directory")?;
        if !metadata.file_type().is_dir() {
            anyhow::bail!("certified action-cache stage is not a directory");
        }
        let stage = Self {
            directory: directory.to_path_buf(),
            receipt,
        };
        let seal = stage.load_seal()?;
        if seal.entries.len() as u64 != stage.receipt.entry_count {
            anyhow::bail!("action-cache stage receipt has the wrong entry count");
        }
        Ok(stage)
    }

    /// The private directory containing the certified records and seal.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Receipt that a capture journal must anchor for restart-safe reopening.
    pub fn receipt(&self) -> &ActionCacheStageReceipt {
        &self.receipt
    }

    /// Durably publish every sealed record through the normal action cache.
    /// A failed or interrupted publication leaves the stage intact for
    /// diagnosis; retrying is idempotent.
    pub async fn publish_into(&self, cache: &ActionCache) -> Result<usize> {
        self.publish_impl(cache, None).await
    }

    /// Durably publish every sealed record after a whole-stage CAS gate.
    ///
    /// Every result and every recursively referenced blob is checked before
    /// the first normal cache write. A concurrent later removal is still a
    /// conservative cache miss because materialization verifies CAS content.
    pub async fn publish_into_with_cas(
        &self,
        cache: &ActionCache,
        cas: &CasStore,
    ) -> Result<usize> {
        self.publish_impl(cache, Some(cas)).await
    }

    async fn publish_impl(&self, cache: &ActionCache, cas: Option<&CasStore>) -> Result<usize> {
        if !cache.supports_durable_publication() {
            anyhow::bail!("certified action results require a persistent cache backend");
        }
        let seal = self.load_seal()?;
        if seal.entries.len() as u64 != self.receipt.entry_count {
            anyhow::bail!("action-cache stage receipt has the wrong entry count");
        }
        let results = self.validate_all_results(&seal)?;
        if cas.is_some_and(|cas| {
            results
                .iter()
                .any(|result| !cas.records_available(&result.output_records))
        }) {
            anyhow::bail!("certified action results reference unavailable CAS content");
        }
        for (entry, result) in seal.entries.iter().zip(results) {
            cache.put_durable(entry.action_id.clone(), result).await?;
        }
        Ok(seal.entries.len())
    }

    fn validate_all_results(&self, seal: &StageSeal) -> Result<Vec<ActionResult>> {
        let sealed_ids = seal
            .entries
            .iter()
            .map(|entry| entry.action_id.clone())
            .collect::<HashSet<_>>();
        if sealed_ids.len() != seal.entries.len() {
            anyhow::bail!("action-cache stage seal contains duplicate IDs");
        }
        let sealed_names = seal
            .entries
            .iter()
            .map(|entry| (entry_file_name(&entry.action_id), entry.action_id.clone()))
            .collect::<HashMap<_, _>>();
        let mut directory_ids = HashSet::new();
        for item in fs::read_dir(&self.directory).context("read certified action-cache stage")? {
            let item = item.context("read certified action-cache stage entry")?;
            if item.file_name() == "sealed" {
                if !item
                    .file_type()
                    .context("inspect action-cache stage seal")?
                    .is_file()
                {
                    anyhow::bail!("action-cache stage seal is not a regular file");
                }
                continue;
            }
            if !item
                .file_type()
                .context("inspect certified action-cache stage entry")?
                .is_file()
                || item.path().extension().and_then(|value| value.to_str()) != Some("cache")
            {
                anyhow::bail!("certified action-cache stage contains an unexpected entry");
            }
            let Some(action_id) = sealed_names.get(&item.file_name()) else {
                anyhow::bail!("certified action-cache stage contains an unsealed result");
            };
            directory_ids.insert(action_id.clone());
        }
        if directory_ids != sealed_ids {
            anyhow::bail!("certified action-cache stage is missing a sealed result");
        }

        let mut results = Vec::with_capacity(seal.entries.len());
        for entry in &seal.entries {
            let path = self.directory.join(entry_file_name(&entry.action_id));
            let bytes = fs::read(path).context("read sealed action result")?;
            if *blake3::hash(&bytes).as_bytes() != entry.digest {
                anyhow::bail!("sealed action result content digest changed");
            }
            let result: ActionResult =
                bincode::deserialize(&bytes).context("decode sealed action result")?;
            if result.action_id != entry.action_id {
                anyhow::bail!("sealed action result no longer matches its ID");
            }
            results.push(result);
        }
        Ok(results)
    }

    fn load_seal(&self) -> Result<StageSeal> {
        let bytes = fs::read(self.directory.join("sealed"))
            .context("read certified action-cache stage seal")?;
        if *blake3::hash(&bytes).as_bytes() != self.receipt.seal_digest {
            anyhow::bail!("action-cache stage seal content digest changed");
        }
        let seal: StageSeal =
            bincode::deserialize(&bytes).context("decode action-cache stage seal")?;
        if seal.schema != SEAL_SCHEMA || seal.version != SEAL_VERSION {
            anyhow::bail!("action-cache stage seal has an unsupported schema");
        }
        Ok(seal)
    }
}

fn entry_file_name(action_id: &ActionId) -> std::ffi::OsString {
    format!("{action_id}.cache").into()
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("staged action-cache path has no parent")?;
    let temporary = parent.join(format!(
        ".stage-{}-{}.tmp",
        std::process::id(),
        unique_suffix()
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .context("create staged action-cache temporary file")?;
        file.write_all(bytes)
            .context("write staged action-cache temporary file")?;
        file.sync_all()
            .context("sync staged action-cache temporary file")?;
        fs::rename(&temporary, path).context("publish staged action-cache file")?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn set_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .context("protect action-cache stage directory")?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .context("open action-cache stage directory")?
        .sync_all()
        .context("sync action-cache stage directory")
}

fn unique_suffix() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{time}-{sequence}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CacheConfig;
    use rush_action::Artifact;
    use std::time::Duration;

    fn action_id(seed: &[u8]) -> ActionId {
        ActionId::new(blake3::hash(seed))
    }

    fn result(action_id: ActionId, exit_code: i32) -> ActionResult {
        ActionResult {
            action_id,
            exit_code,
            stdout: Vec::new(),
            stderr: Vec::new(),
            outputs: Vec::new(),
            output_records: Vec::new(),
            duration: Duration::ZERO,
            cached: false,
        }
    }

    fn disk_cache(root: &Path) -> ActionCache {
        ActionCache::with_disk(CacheConfig {
            cache_root: root.to_path_buf(),
            max_size: 0,
            ttl_seconds: 0,
        })
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn results_remain_invisible_until_certified_publication() {
        let temp = tempfile::tempdir().unwrap();
        let stages = temp.path().join("stages");
        fs::create_dir(&stages).unwrap();
        let id = action_id(b"compile");
        let stage = StagedActionCache::create(&stages.join("capture")).unwrap();
        stage.put(id.clone(), &result(id.clone(), 0)).unwrap();
        let cache_root = temp.path().join("cache");
        let cache = disk_cache(&cache_root);

        assert!(cache.get(&id).is_none());
        let certified = stage.certify(std::slice::from_ref(&id)).unwrap();
        assert!(cache.get(&id).is_none());
        assert_eq!(certified.publish_into(&cache).await.unwrap(), 1);

        let reopened = disk_cache(&cache_root);
        assert_eq!(reopened.get(&id).unwrap().exit_code, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn journal_receipt_reopens_a_certified_stage_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let stages = temp.path().join("stages");
        fs::create_dir(&stages).unwrap();
        let id = action_id(b"restart");
        let stage = StagedActionCache::create(&stages.join("capture")).unwrap();
        stage.put(id.clone(), &result(id.clone(), 0)).unwrap();
        let certified = stage.certify(std::slice::from_ref(&id)).unwrap();
        let receipt = certified.receipt().clone();
        assert_eq!(receipt.entry_count(), 1);
        drop(certified);

        let reopened = CertifiedActionCacheStage::open(&stages.join("capture"), receipt).unwrap();
        let cache_root = temp.path().join("cache");
        assert_eq!(
            reopened
                .publish_into(&disk_cache(&cache_root))
                .await
                .unwrap(),
            1
        );
        assert_eq!(disk_cache(&cache_root).get(&id).unwrap().exit_code, 0);
    }

    #[test]
    fn reopening_requires_the_journal_anchored_seal_digest() {
        let temp = tempfile::tempdir().unwrap();
        let stages = temp.path().join("stages");
        fs::create_dir(&stages).unwrap();
        let id = action_id(b"wrong-receipt");
        let stage = StagedActionCache::create(&stages.join("capture")).unwrap();
        stage.put(id.clone(), &result(id.clone(), 0)).unwrap();
        let certified = stage.certify(std::slice::from_ref(&id)).unwrap();
        let mut receipt = certified.receipt().clone();
        receipt.seal_digest[0] ^= 1;
        drop(certified);

        let error = CertifiedActionCacheStage::open(&stages.join("capture"), receipt).unwrap_err();
        assert!(error.to_string().contains("seal content digest changed"));
    }

    #[test]
    fn certification_requires_the_exact_expected_set() {
        let temp = tempfile::tempdir().unwrap();
        let stages = temp.path().join("stages");
        fs::create_dir(&stages).unwrap();
        let first = action_id(b"first");
        let missing = action_id(b"missing");
        let stage = StagedActionCache::create(&stages.join("capture")).unwrap();
        stage.put(first.clone(), &result(first.clone(), 0)).unwrap();

        let error = stage.certify(&[first, missing]).unwrap_err();
        assert!(error.to_string().contains("expected action-ID set"));
    }

    #[test]
    fn one_action_id_cannot_stage_conflicting_results() {
        let temp = tempfile::tempdir().unwrap();
        let stages = temp.path().join("stages");
        fs::create_dir(&stages).unwrap();
        let id = action_id(b"same-action");
        let stage = StagedActionCache::create(&stages.join("capture")).unwrap();
        stage.put(id.clone(), &result(id.clone(), 0)).unwrap();
        stage.put(id.clone(), &result(id.clone(), 0)).unwrap();

        let error = stage.put(id.clone(), &result(id, 1)).unwrap_err();
        assert!(error.to_string().contains("conflicting"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn changed_sealed_bytes_block_all_publication() {
        let temp = tempfile::tempdir().unwrap();
        let stages = temp.path().join("stages");
        fs::create_dir(&stages).unwrap();
        let first = action_id(b"first-valid");
        let changed = action_id(b"second-tampered");
        let stage = StagedActionCache::create(&stages.join("capture")).unwrap();
        stage.put(first.clone(), &result(first.clone(), 0)).unwrap();
        stage
            .put(changed.clone(), &result(changed.clone(), 0))
            .unwrap();
        let certified = stage.certify(&[first.clone(), changed.clone()]).unwrap();
        fs::write(
            certified.directory().join(entry_file_name(&changed)),
            b"changed",
        )
        .unwrap();
        let cache = disk_cache(&temp.path().join("cache"));

        let error = certified.publish_into(&cache).await.unwrap_err();
        assert!(error.to_string().contains("digest changed"));
        assert!(cache.get(&first).is_none());
        assert!(cache.get(&changed).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_cas_blob_blocks_all_publication() {
        let temp = tempfile::tempdir().unwrap();
        let cas_root = temp.path().join("cas-root");
        let cas = CasStore::new(&cas_root).unwrap();
        let first_path = temp.path().join("first.o");
        let second_path = temp.path().join("second.o");
        fs::write(&first_path, b"first captured output").unwrap();
        fs::write(&second_path, b"second captured output").unwrap();
        let first_record = cas.record_output_durable(&first_path).unwrap();
        let second_record = cas.record_output_durable(&second_path).unwrap();
        let missing_digest = second_record.content_digest.unwrap();
        let first = action_id(b"first-cas-result");
        let second = action_id(b"second-cas-result");
        let stages = temp.path().join("stages");
        fs::create_dir(&stages).unwrap();
        let stage = StagedActionCache::create(&stages.join("capture")).unwrap();
        for (id, path, record) in [
            (first.clone(), first_path, first_record),
            (second.clone(), second_path, second_record),
        ] {
            let mut value = result(id.clone(), 0);
            value.outputs = vec![Artifact::Source(path)];
            value.output_records = vec![record];
            stage.put(id, &value).unwrap();
        }
        let certified = stage.certify(&[first.clone(), second.clone()]).unwrap();
        fs::remove_file(
            cas_root
                .join("cas")
                .join(&missing_digest.to_hex()[..2])
                .join(missing_digest.to_hex()),
        )
        .unwrap();
        let cache = disk_cache(&temp.path().join("cache"));

        let error = certified
            .publish_into_with_cas(&cache, &cas)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unavailable CAS content"));
        assert!(cache.get(&first).is_none());
        assert!(cache.get(&second).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsealed_extra_file_blocks_all_publication() {
        let temp = tempfile::tempdir().unwrap();
        let stages = temp.path().join("stages");
        fs::create_dir(&stages).unwrap();
        let id = action_id(b"sealed");
        let stage = StagedActionCache::create(&stages.join("capture")).unwrap();
        stage.put(id.clone(), &result(id.clone(), 0)).unwrap();
        let certified = stage.certify(std::slice::from_ref(&id)).unwrap();
        fs::write(certified.directory().join("unexpected.cache"), b"foreign").unwrap();
        let cache = disk_cache(&temp.path().join("cache"));

        let error = certified.publish_into(&cache).await.unwrap_err();
        assert!(error.to_string().contains("unsealed result"));
        assert!(cache.get(&id).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn memory_cache_cannot_claim_durable_publication() {
        let temp = tempfile::tempdir().unwrap();
        let stages = temp.path().join("stages");
        fs::create_dir(&stages).unwrap();
        let id = action_id(b"memory-only");
        let stage = StagedActionCache::create(&stages.join("capture")).unwrap();
        stage.put(id.clone(), &result(id.clone(), 0)).unwrap();
        let certified = stage.certify(std::slice::from_ref(&id)).unwrap();

        let error = certified
            .publish_into(&ActionCache::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("persistent cache backend"));
    }

    #[cfg(unix)]
    #[test]
    fn stage_state_is_private_and_never_adjacent_to_outputs() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let stages = temp.path().join("stages");
        let output = temp.path().join("output");
        fs::create_dir(&stages).unwrap();
        fs::create_dir(&output).unwrap();
        let id = action_id(b"private");
        let stage = StagedActionCache::create(&stages.join("capture")).unwrap();
        stage.put(id.clone(), &result(id.clone(), 0)).unwrap();
        let entry = stage.entry_path(&id);

        assert_eq!(
            fs::metadata(stage.directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(entry).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(output).unwrap().count(), 0);
    }
}
