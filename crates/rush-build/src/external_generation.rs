//! Restart-safe ownership of persisted external-analysis generations.
//!
//! Snapshot bytes, selection markers, capture journals, and locks live below
//! Rush-owned state, never beside files in an external build output tree. A
//! capture journal is published before the analyzer may mutate that tree and
//! is removed only after a complete snapshot generation becomes active or the
//! caller confirms rollback. A crash therefore leaves a durable reason to
//! reject replay rather than admitting a snapshot/tree mismatch.

use crate::external_analysis_snapshot::{
    load_serialized_if_fresh, ExternalAnalysisKey, ExternalAnalysisSnapshot,
    ExternalSnapshotAdmission,
};
use crate::external_capture_adoption::ExternalCaptureAdoption;
use anyhow::{Context, Result};
use rand::{rngs::OsRng, RngCore};
use rush_cache::cas::CasStore;
use rush_cache::staging::{ActionCacheStageReceipt, CertifiedActionCacheStage};
use rush_cache::ActionCache;
use rush_command_core::CommandSemantics;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

const ACTIVE_SCHEMA: &str = "org.rushbuild.external-generation-active";
const JOURNAL_SCHEMA: &str = "org.rushbuild.external-generation-journal";
const TOMBSTONE_SCHEMA: &str = "org.rushbuild.external-generation-inactive";
const OWNERSHIP_SCHEMA: &str = "org.rushbuild.external-output-ownership";
const MANAGED_JOURNAL_SCHEMA: &str = "org.rushbuild.external-managed-capture";
const STATE_VERSION: u32 = 1;
const MANAGED_JOURNAL_VERSION: u32 = 3;

#[derive(Debug, Serialize, Deserialize)]
struct ActiveGeneration {
    schema: String,
    version: u32,
    key_hash: [u8; 32],
    snapshot_digest: [u8; 32],
}

#[derive(Debug, Serialize, Deserialize)]
struct CaptureJournal {
    schema: String,
    version: u32,
    key_hash: [u8; 32],
    transaction: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct InvalidationTombstone {
    schema: String,
    version: u32,
    reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct OutputDirectoryIdentity {
    device: u64,
    inode: u64,
    mode: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManagedOutputOwnership {
    schema: String,
    version: u32,
    output_hash: [u8; 32],
    identity: OutputDirectoryIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum ManagedCapturePhase {
    Preparing,
    Prepared,
    Active,
    CommitReady {
        snapshot_digest: [u8; 32],
    },
    ActionResultsReady {
        snapshot_digest: [u8; 32],
        stage_receipt: ActionCacheStageReceipt,
    },
    ActionResultsPublished {
        snapshot_digest: [u8; 32],
        stage_receipt: ActionCacheStageReceipt,
    },
    Published {
        snapshot_digest: [u8; 32],
        stage_receipt: Option<ActionCacheStageReceipt>,
    },
    KeepingUncertified {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedCaptureJournal {
    schema: String,
    version: u32,
    output_hash: [u8; 32],
    key_hash: [u8; 32],
    capture_id: [u8; 16],
    transaction: String,
    backup_name: PathBuf,
    fresh_name: PathBuf,
    action_stage_name: PathBuf,
    original: OutputDirectoryIdentity,
    fresh: Option<OutputDirectoryIdentity>,
    phase: ManagedCapturePhase,
}

/// Result of repairing a process death in an exact-path managed capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedCaptureRecovery {
    /// No managed capture journal existed.
    NoPendingCapture,
    /// The pre-capture tree was restored.
    RolledBack,
    /// A fully prepared snapshot publication was completed.
    Published,
    /// A successful but uncertified analyzer tree was retained while replay
    /// stayed invalidated.
    KeptUncertified,
}

/// Controller-selected identity for one exact-path capture transaction.
///
/// The opaque context digest binds the active analysis key, logical output
/// path, fresh directory identity, and private journal transaction. An
/// analyzer adapter must bind these values into its observation stream and
/// return the same identity before the controller can publish a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCaptureTransactionIdentity {
    capture_id: [u8; 16],
    context_digest: [u8; 32],
}

impl ExternalCaptureTransactionIdentity {
    /// Construct an identity inside the controller and its capture adapters.
    pub(crate) fn new(capture_id: [u8; 16], context_digest: [u8; 32]) -> Self {
        Self {
            capture_id,
            context_digest,
        }
    }

    /// Random controller-selected transaction identifier.
    pub fn capture_id(self) -> [u8; 16] {
        self.capture_id
    }

    /// Digest binding the transaction to its key, path, and fresh directory.
    pub fn context_digest(self) -> [u8; 32] {
        self.context_digest
    }
}

/// Rush-owned state for the one active generation at an exact output path.
///
/// Different analysis keys for the same output path intentionally share the
/// lock, journal, and active marker. They cannot concurrently claim one
/// mutable tree. The directory name contains only a domain-separated hash of
/// the output path; private host paths do not appear in the state layout.
#[derive(Debug, Clone)]
pub struct ExternalGenerationStore {
    directory: PathBuf,
    expected_key: ExternalAnalysisKey,
    output_hash: [u8; 32],
}

impl ExternalGenerationStore {
    /// Select Rush-owned state below `state_root` for `expected_key`'s exact
    /// external output path.
    pub fn new(state_root: &Path, expected_key: ExternalAnalysisKey) -> Self {
        let output_hash = output_root_hash(&expected_key.output_root);
        Self {
            directory: state_root
                .join("external-analysis")
                .join(hex::encode(output_hash)),
            expected_key,
            output_hash,
        }
    }

    /// The opaque Rush-owned directory used by this exact output path.
    pub fn state_directory(&self) -> &Path {
        &self.directory
    }

    /// Exact logical output path authenticated by this generation store.
    pub(crate) fn output_root(&self) -> &Path {
        &self.expected_key.output_root
    }

    /// Full analysis key this store accepts for publication.
    pub(crate) fn expected_key(&self) -> &ExternalAnalysisKey {
        &self.expected_key
    }

    /// Load the atomically selected generation, rejecting any unresolved
    /// capture before snapshot bytes are considered.
    pub fn load_active_if_fresh(&self, semantics: &CommandSemantics) -> ExternalSnapshotAdmission {
        if !self.directory.exists() {
            return ExternalSnapshotAdmission::Missing(
                "external generation state does not exist".to_string(),
            );
        }
        let _lock = match StateLock::acquire(&self.lock_path(), LockMode::Shared) {
            Ok(lock) => lock,
            Err(error) => {
                return ExternalSnapshotAdmission::Invalid(format!(
                    "external generation state is busy or cannot be locked: {error}"
                ))
            }
        };
        if self.journal_path().exists() {
            return ExternalSnapshotAdmission::Invalid(
                "external capture transaction is unresolved".to_string(),
            );
        }
        if self.managed_journal_path().exists() {
            return ExternalSnapshotAdmission::Invalid(
                "managed external capture transaction is unresolved".to_string(),
            );
        }
        if self.ownership_path().exists() {
            if let Err(error) = self.validate_managed_ownership_locked() {
                return ExternalSnapshotAdmission::Invalid(format!(
                    "managed external output ownership changed: {error}"
                ));
            }
        }
        let marker_bytes = match fs::read(self.active_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return ExternalSnapshotAdmission::Missing(self.inactive_reason())
            }
            Err(error) => {
                return ExternalSnapshotAdmission::Invalid(format!(
                    "active external generation read failed: {error}"
                ))
            }
        };
        let marker: ActiveGeneration = match bincode::deserialize(&marker_bytes) {
            Ok(marker) => marker,
            Err(error) => {
                return ExternalSnapshotAdmission::Invalid(format!(
                    "active external generation decode failed: {error}"
                ))
            }
        };
        if marker.schema != ACTIVE_SCHEMA || marker.version != STATE_VERSION {
            return ExternalSnapshotAdmission::Invalid(
                "active external generation has an unsupported state schema".to_string(),
            );
        }
        if marker.key_hash != self.expected_key.stable_hash() {
            return ExternalSnapshotAdmission::Missing(
                "active external generation belongs to a different analysis key".to_string(),
            );
        }
        let snapshot_path = self.generation_path(marker.snapshot_digest);
        let snapshot_bytes = match fs::read(&snapshot_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return ExternalSnapshotAdmission::Invalid(format!(
                    "active external snapshot read failed: {error}"
                ))
            }
        };
        if *blake3::hash(&snapshot_bytes).as_bytes() != marker.snapshot_digest {
            return ExternalSnapshotAdmission::Invalid(
                "active external snapshot content digest changed".to_string(),
            );
        }
        load_serialized_if_fresh(&snapshot_bytes, &self.expected_key, semantics)
    }

    /// Acquire exclusive output-state ownership and publish a capture journal.
    ///
    /// If a prior process died with a journal present, its possibly mismatched
    /// active generation is invalidated before the new transaction begins.
    /// The caller must restore the prior output tree before calling
    /// [`ExternalCaptureTransaction::finish_rolled_back`].
    pub fn begin_capture(&self) -> Result<ExternalCaptureTransaction> {
        ensure_private_directory(&self.directory)?;
        ensure_private_directory(&self.generations_dir())?;
        let lock = StateLock::acquire(&self.lock_path(), LockMode::Exclusive)?;
        if self.managed_journal_path().exists() {
            anyhow::bail!("managed external capture requires recovery");
        }
        if self.ownership_path().exists() {
            anyhow::bail!("managed external output requires an exact-path managed capture");
        }
        if self.journal_path().exists() {
            self.invalidate_locked("interrupted external capture requires recapture")?;
        }
        let journal = CaptureJournal {
            schema: JOURNAL_SCHEMA.to_string(),
            version: STATE_VERSION,
            key_hash: self.expected_key.stable_hash(),
            transaction: format!("{}-{}", std::process::id(), unique_suffix()),
        };
        atomic_write(&self.journal_path(), &bincode::serialize(&journal)?)?;
        Ok(ExternalCaptureTransaction {
            store: self.clone(),
            _lock: lock,
            finished: false,
        })
    }

    /// Claim an empty or absent output directory for Rush-managed captures.
    ///
    /// A nonempty tree is never adopted: v1 cannot prove that it owns those
    /// bytes. The ownership receipt lives in private external state and binds
    /// the exact directory inode. Replacing the directory later therefore
    /// makes replay fail closed.
    pub fn claim_managed_output(&self) -> Result<()> {
        ensure_private_directory(&self.directory)?;
        ensure_private_directory(&self.generations_dir())?;
        let _lock = StateLock::acquire(&self.lock_path(), LockMode::Exclusive)?;
        if self.journal_path().exists() || self.managed_journal_path().exists() {
            anyhow::bail!("external capture requires recovery before ownership claim");
        }
        if self.ownership_path().exists() {
            return self.validate_managed_ownership_locked().map(|_| ());
        }
        if self.active_path().exists() {
            anyhow::bail!("an existing generation cannot be converted to managed ownership");
        }

        let output = &self.expected_key.output_root;
        let parent = output_parent(output)?;
        let mut created = false;
        let identity = match output_directory_identity(output)? {
            Some(identity) => {
                if fs::read_dir(output)?.next().transpose()?.is_some() {
                    anyhow::bail!("refusing to claim a nonempty external output directory");
                }
                identity
            }
            None => {
                create_output_directory(output, 0o755)?;
                sync_directory(parent)?;
                created = true;
                output_directory_identity(output)?
                    .context("new external output directory disappeared")?
            }
        };
        let result = self.write_managed_ownership(identity);
        if result.is_err() && created {
            let _ = fs::remove_dir(output);
            let _ = sync_directory(parent);
        }
        result
    }

    /// Move the currently owned tree aside and place a new empty directory at
    /// the same logical output path. The returned transaction keeps exclusive
    /// ownership until commit, rollback, or uncertified-success handling.
    pub fn begin_clean_managed_capture(&self) -> Result<ExternalManagedCapture> {
        ensure_private_directory(&self.directory)?;
        ensure_private_directory(&self.generations_dir())?;
        ensure_private_directory(&self.action_stages_dir())?;
        let lock = StateLock::acquire(&self.lock_path(), LockMode::Exclusive)?;
        if self.journal_path().exists() || self.managed_journal_path().exists() {
            anyhow::bail!("external capture requires recovery before recapture");
        }
        let original = self.validate_managed_ownership_locked()?;
        let output = &self.expected_key.output_root;
        let parent = output_parent(output)?;
        let transaction = format!("{}-{}", std::process::id(), unique_suffix());
        let mut capture_id = [0_u8; 16];
        OsRng
            .try_fill_bytes(&mut capture_id)
            .context("generate managed capture transaction identifier")?;
        let backup_name = sibling_name(output, "rush-capture-backup", &transaction)?;
        let fresh_name = sibling_name(output, "rush-capture-fresh", &transaction)?;
        let action_stage_name = PathBuf::from(format!("{transaction}.stage"));
        let backup = parent.join(&backup_name);
        let fresh_path = parent.join(&fresh_name);
        if backup.exists() || fresh_path.exists() {
            anyhow::bail!("managed capture sibling already exists");
        }

        let mut journal = ManagedCaptureJournal {
            schema: MANAGED_JOURNAL_SCHEMA.to_string(),
            version: MANAGED_JOURNAL_VERSION,
            output_hash: self.output_hash,
            key_hash: self.expected_key.stable_hash(),
            capture_id,
            transaction,
            backup_name,
            fresh_name,
            action_stage_name,
            original,
            fresh: None,
            phase: ManagedCapturePhase::Preparing,
        };
        self.write_managed_journal(&journal)?;
        create_output_directory(&fresh_path, original.mode)?;
        sync_directory(parent)?;
        let fresh = output_directory_identity(&fresh_path)?
            .context("fresh managed output directory disappeared")?;
        journal.fresh = Some(fresh);
        journal.phase = ManagedCapturePhase::Prepared;
        self.write_managed_journal(&journal)?;

        rename_noreplace(output, &backup).context("move managed output to capture backup")?;
        rename_noreplace(&fresh_path, output).context("publish fresh managed output path")?;
        sync_directory(parent)?;
        journal.phase = ManagedCapturePhase::Active;
        self.write_managed_journal(&journal)?;
        let identity = managed_capture_transaction_identity(&journal)?;
        Ok(ExternalManagedCapture {
            store: self.clone(),
            journal,
            identity,
            _lock: lock,
            finished: false,
        })
    }

    /// Repair a journal left by process death without guessing about unknown
    /// directory identities. Pre-publication work rolls back; a fully prepared
    /// commit finishes publication; uncertified success remains invalidated.
    pub fn recover_managed_capture(&self) -> Result<ManagedCaptureRecovery> {
        ensure_private_directory(&self.directory)?;
        let _lock = StateLock::acquire(&self.lock_path(), LockMode::Exclusive)?;
        if self.journal_path().exists() {
            anyhow::bail!("non-managed external capture requires recapture");
        }
        let Some(journal) = self.read_managed_journal()? else {
            return Ok(ManagedCaptureRecovery::NoPendingCapture);
        };
        match journal.phase.clone() {
            ManagedCapturePhase::Preparing => {
                self.recover_managed_preparation_locked(&journal)?;
                Ok(ManagedCaptureRecovery::RolledBack)
            }
            ManagedCapturePhase::Prepared | ManagedCapturePhase::Active => {
                self.rollback_managed_locked(&journal)?;
                Ok(ManagedCaptureRecovery::RolledBack)
            }
            ManagedCapturePhase::CommitReady { snapshot_digest } => {
                self.finish_managed_commit_locked(journal, snapshot_digest, None)?;
                Ok(ManagedCaptureRecovery::Published)
            }
            ManagedCapturePhase::ActionResultsReady { .. } => {
                anyhow::bail!("managed capture recovery requires action cache and CAS publication")
            }
            ManagedCapturePhase::ActionResultsPublished {
                snapshot_digest,
                stage_receipt,
            } => {
                self.finish_managed_commit_locked(journal, snapshot_digest, Some(stage_receipt))?;
                Ok(ManagedCaptureRecovery::Published)
            }
            ManagedCapturePhase::Published {
                snapshot_digest,
                stage_receipt,
            } => {
                self.finish_managed_commit_locked(journal, snapshot_digest, stage_receipt)?;
                Ok(ManagedCaptureRecovery::Published)
            }
            ManagedCapturePhase::KeepingUncertified { reason } => {
                self.finish_keep_uncertified_locked(&journal, &reason)?;
                Ok(ManagedCaptureRecovery::KeptUncertified)
            }
        }
    }

    /// Repair a managed capture whose certified action results may still need
    /// durable publication before its snapshot becomes replayable.
    pub async fn recover_managed_capture_with_cache(
        &self,
        cache: &ActionCache,
        cas: &CasStore,
    ) -> Result<ManagedCaptureRecovery> {
        ensure_private_directory(&self.directory)?;
        let _lock = StateLock::acquire(&self.lock_path(), LockMode::Exclusive)?;
        if self.journal_path().exists() {
            anyhow::bail!("non-managed external capture requires recapture");
        }
        let Some(mut journal) = self.read_managed_journal()? else {
            return Ok(ManagedCaptureRecovery::NoPendingCapture);
        };
        match journal.phase.clone() {
            ManagedCapturePhase::Preparing => {
                self.recover_managed_preparation_locked(&journal)?;
                Ok(ManagedCaptureRecovery::RolledBack)
            }
            ManagedCapturePhase::Prepared | ManagedCapturePhase::Active => {
                self.rollback_managed_locked(&journal)?;
                Ok(ManagedCaptureRecovery::RolledBack)
            }
            ManagedCapturePhase::CommitReady { snapshot_digest } => {
                self.finish_managed_commit_locked(journal, snapshot_digest, None)?;
                Ok(ManagedCaptureRecovery::Published)
            }
            ManagedCapturePhase::ActionResultsReady {
                snapshot_digest,
                stage_receipt,
            } => {
                self.validate_action_stage_receipt(
                    snapshot_digest,
                    journal.key_hash,
                    &stage_receipt,
                )?;
                let stage = CertifiedActionCacheStage::open(
                    &self.action_stage_path(&journal),
                    stage_receipt.clone(),
                )?;
                stage.publish_into_with_cas(cache, cas).await?;
                journal.phase = ManagedCapturePhase::ActionResultsPublished {
                    snapshot_digest,
                    stage_receipt: stage_receipt.clone(),
                };
                self.write_managed_journal(&journal)?;
                self.finish_managed_commit_locked(journal, snapshot_digest, Some(stage_receipt))?;
                Ok(ManagedCaptureRecovery::Published)
            }
            ManagedCapturePhase::ActionResultsPublished {
                snapshot_digest,
                stage_receipt,
            } => {
                self.finish_managed_commit_locked(journal, snapshot_digest, Some(stage_receipt))?;
                Ok(ManagedCaptureRecovery::Published)
            }
            ManagedCapturePhase::Published {
                snapshot_digest,
                stage_receipt,
            } => {
                self.finish_managed_commit_locked(journal, snapshot_digest, stage_receipt)?;
                Ok(ManagedCaptureRecovery::Published)
            }
            ManagedCapturePhase::KeepingUncertified { reason } => {
                self.finish_keep_uncertified_locked(&journal, &reason)?;
                Ok(ManagedCaptureRecovery::KeptUncertified)
            }
        }
    }

    fn validate_managed_ownership_locked(&self) -> Result<OutputDirectoryIdentity> {
        let bytes = fs::read(self.ownership_path()).context("read managed output ownership")?;
        let ownership: ManagedOutputOwnership =
            bincode::deserialize(&bytes).context("decode managed output ownership")?;
        if ownership.schema != OWNERSHIP_SCHEMA
            || ownership.version != STATE_VERSION
            || ownership.output_hash != self.output_hash
        {
            anyhow::bail!("managed output ownership has an unsupported identity");
        }
        let actual = output_directory_identity(&self.expected_key.output_root)?
            .context("managed output directory is missing")?;
        if actual != ownership.identity {
            anyhow::bail!("managed output directory identity no longer matches its receipt");
        }
        Ok(actual)
    }

    fn write_managed_ownership(&self, identity: OutputDirectoryIdentity) -> Result<()> {
        let ownership = ManagedOutputOwnership {
            schema: OWNERSHIP_SCHEMA.to_string(),
            version: STATE_VERSION,
            output_hash: self.output_hash,
            identity,
        };
        atomic_write(&self.ownership_path(), &bincode::serialize(&ownership)?)
    }

    fn read_managed_journal(&self) -> Result<Option<ManagedCaptureJournal>> {
        let bytes = match fs::read(self.managed_journal_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("read managed capture journal"),
        };
        let journal: ManagedCaptureJournal =
            bincode::deserialize(&bytes).context("decode managed capture journal")?;
        if journal.schema != MANAGED_JOURNAL_SCHEMA
            || journal.version != MANAGED_JOURNAL_VERSION
            || journal.output_hash != self.output_hash
            || !is_single_normal_component(&journal.backup_name)
            || !is_single_normal_component(&journal.fresh_name)
            || !is_single_normal_component(&journal.action_stage_name)
            || (journal.phase != ManagedCapturePhase::Preparing && journal.fresh.is_none())
        {
            anyhow::bail!("managed capture journal has an unsupported identity");
        }
        Ok(Some(journal))
    }

    fn write_managed_journal(&self, journal: &ManagedCaptureJournal) -> Result<()> {
        atomic_write(&self.managed_journal_path(), &bincode::serialize(journal)?)
    }

    fn store_snapshot_blob(&self, snapshot: &ExternalAnalysisSnapshot) -> Result<[u8; 32]> {
        if snapshot.key != self.expected_key {
            anyhow::bail!("captured snapshot key does not match its generation store");
        }
        snapshot.validate_structure()?;
        let snapshot_bytes = bincode::serialize(snapshot)?;
        let snapshot_digest = *blake3::hash(&snapshot_bytes).as_bytes();
        atomic_write(&self.generation_path(snapshot_digest), &snapshot_bytes)?;
        Ok(snapshot_digest)
    }

    fn validate_snapshot_blob(&self, snapshot_digest: [u8; 32], key_hash: [u8; 32]) -> Result<()> {
        self.load_snapshot_blob(snapshot_digest, key_hash)
            .map(|_| ())
    }

    fn load_snapshot_blob(
        &self,
        snapshot_digest: [u8; 32],
        key_hash: [u8; 32],
    ) -> Result<ExternalAnalysisSnapshot> {
        let bytes = fs::read(self.generation_path(snapshot_digest))
            .context("read prepared external snapshot")?;
        if *blake3::hash(&bytes).as_bytes() != snapshot_digest {
            anyhow::bail!("prepared external snapshot content digest changed");
        }
        let snapshot = crate::external_analysis_snapshot::decode_serialized_snapshot(&bytes)
            .context("decode prepared external snapshot")?;
        if snapshot.key.stable_hash() != key_hash
            || output_root_hash(&snapshot.key.output_root) != self.output_hash
        {
            anyhow::bail!("prepared external snapshot key changed");
        }
        snapshot.validate_structure()?;
        Ok(snapshot)
    }

    fn validate_action_stage_receipt(
        &self,
        snapshot_digest: [u8; 32],
        key_hash: [u8; 32],
        receipt: &ActionCacheStageReceipt,
    ) -> Result<()> {
        let snapshot = self.load_snapshot_blob(snapshot_digest, key_hash)?;
        if receipt.entry_count() != reusable_action_count(&snapshot) as u64 {
            anyhow::bail!("certified action stage does not cover every reusable action");
        }
        Ok(())
    }

    fn publish_active_digest(&self, snapshot_digest: [u8; 32]) -> Result<()> {
        self.publish_active_marker(self.expected_key.stable_hash(), snapshot_digest)
    }

    fn publish_active_marker(&self, key_hash: [u8; 32], snapshot_digest: [u8; 32]) -> Result<()> {
        let marker = ActiveGeneration {
            schema: ACTIVE_SCHEMA.to_string(),
            version: STATE_VERSION,
            key_hash,
            snapshot_digest,
        };
        atomic_write(&self.active_path(), &bincode::serialize(&marker)?)?;
        remove_if_present(&self.tombstone_path())
    }

    fn managed_paths(&self, journal: &ManagedCaptureJournal) -> Result<(PathBuf, PathBuf)> {
        let parent = output_parent(&self.expected_key.output_root)?;
        Ok((
            parent.join(&journal.backup_name),
            parent.join(&journal.fresh_name),
        ))
    }

    fn recover_managed_preparation_locked(&self, journal: &ManagedCaptureJournal) -> Result<()> {
        let output = &self.expected_key.output_root;
        let parent = output_parent(output)?;
        let (backup, fresh_path) = self.managed_paths(journal)?;
        if output_directory_identity(output)? != Some(journal.original) || backup.exists() {
            anyhow::bail!("managed capture preparation changed the owned output tree");
        }
        if fresh_path.exists() {
            if fs::read_dir(&fresh_path)?.next().transpose()?.is_some() {
                anyhow::bail!("refusing to remove a nonempty unverified fresh sibling");
            }
            fs::remove_dir(&fresh_path).context("remove empty managed fresh sibling")?;
            sync_directory(parent)?;
        }
        self.remove_private_action_stage_locked(journal, None, true)?;
        self.write_managed_ownership(journal.original)?;
        remove_if_present(&self.managed_journal_path())
    }

    fn rollback_managed_locked(&self, journal: &ManagedCaptureJournal) -> Result<()> {
        let output = &self.expected_key.output_root;
        let parent = output_parent(output)?;
        let (backup, fresh_path) = self.managed_paths(journal)?;
        let fresh = journal
            .fresh
            .context("managed capture journal lacks fresh directory identity")?;
        let output_identity = output_directory_identity(output)?;
        let backup_identity = output_directory_identity(&backup)?;
        let staged_identity = output_directory_identity(&fresh_path)?;

        if output_identity == Some(journal.original) && backup_identity.is_none() {
            if let Some(identity) = staged_identity {
                if identity != fresh {
                    anyhow::bail!("unknown directory occupies managed fresh sibling");
                }
                remove_owned_directory(&fresh_path, fresh)?;
            }
        } else if backup_identity == Some(journal.original) {
            match output_identity {
                Some(identity) if identity == fresh => {
                    remove_owned_directory(output, fresh)?;
                }
                None => {}
                Some(_) => anyhow::bail!("unknown directory occupies managed output path"),
            }
            if let Some(identity) = staged_identity {
                if identity != fresh {
                    anyhow::bail!("unknown directory occupies managed fresh sibling");
                }
                remove_owned_directory(&fresh_path, fresh)?;
            }
            rename_noreplace(&backup, output).context("restore managed output backup")?;
        } else {
            anyhow::bail!("managed capture backup identity is not recoverable");
        }

        sync_directory(parent)?;
        self.remove_private_action_stage_locked(journal, None, true)?;
        self.write_managed_ownership(journal.original)?;
        remove_if_present(&self.managed_journal_path())
    }

    fn finish_managed_commit_locked(
        &self,
        mut journal: ManagedCaptureJournal,
        snapshot_digest: [u8; 32],
        stage_receipt: Option<ActionCacheStageReceipt>,
    ) -> Result<()> {
        let output = &self.expected_key.output_root;
        let fresh = journal
            .fresh
            .context("managed capture journal lacks fresh directory identity")?;
        if output_directory_identity(output)? != Some(fresh) {
            anyhow::bail!("managed output changed before snapshot publication");
        }
        self.validate_snapshot_blob(snapshot_digest, journal.key_hash)?;
        if let Some(receipt) = &stage_receipt {
            self.validate_action_stage_receipt(snapshot_digest, journal.key_hash, receipt)?;
            let allow_missing = matches!(journal.phase, ManagedCapturePhase::Published { .. });
            self.validate_private_action_stage_locked(&journal, receipt, allow_missing)?;
        }
        self.publish_active_marker(journal.key_hash, snapshot_digest)?;
        self.write_managed_ownership(fresh)?;
        journal.phase = ManagedCapturePhase::Published {
            snapshot_digest,
            stage_receipt: stage_receipt.clone(),
        };
        self.write_managed_journal(&journal)?;

        let parent = output_parent(output)?;
        let (backup, fresh_path) = self.managed_paths(&journal)?;
        if fresh_path.exists() {
            anyhow::bail!("managed fresh sibling unexpectedly reappeared");
        }
        if let Some(identity) = output_directory_identity(&backup)? {
            if identity != journal.original {
                anyhow::bail!("managed capture backup identity changed");
            }
            remove_owned_directory(&backup, journal.original)?;
            sync_directory(parent)?;
        }
        self.remove_private_action_stage_locked(&journal, stage_receipt.as_ref(), true)?;
        remove_if_present(&self.managed_journal_path())
    }

    fn finish_keep_uncertified_locked(
        &self,
        journal: &ManagedCaptureJournal,
        reason: &str,
    ) -> Result<()> {
        let output = &self.expected_key.output_root;
        let fresh = journal
            .fresh
            .context("managed capture journal lacks fresh directory identity")?;
        if output_directory_identity(output)? != Some(fresh) {
            anyhow::bail!("managed output changed before invalidation");
        }
        self.invalidate_locked(reason)?;
        self.write_managed_ownership(fresh)?;
        let parent = output_parent(output)?;
        let (backup, fresh_path) = self.managed_paths(journal)?;
        if fresh_path.exists() {
            anyhow::bail!("managed fresh sibling unexpectedly reappeared");
        }
        if let Some(identity) = output_directory_identity(&backup)? {
            if identity != journal.original {
                anyhow::bail!("managed capture backup identity changed");
            }
            remove_owned_directory(&backup, journal.original)?;
            sync_directory(parent)?;
        }
        self.remove_private_action_stage_locked(journal, None, true)?;
        remove_if_present(&self.managed_journal_path())
    }

    fn invalidate_locked(&self, reason: &str) -> Result<()> {
        match fs::remove_file(self.active_path()) {
            Ok(()) => sync_directory(&self.directory)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove active external generation"),
        }
        let tombstone = InvalidationTombstone {
            schema: TOMBSTONE_SCHEMA.to_string(),
            version: STATE_VERSION,
            reason: reason.to_string(),
        };
        atomic_write(&self.tombstone_path(), &bincode::serialize(&tombstone)?)
    }

    fn inactive_reason(&self) -> String {
        let bytes = match fs::read(self.tombstone_path()) {
            Ok(bytes) => bytes,
            Err(_) => return "no active external generation exists".to_string(),
        };
        match bincode::deserialize::<InvalidationTombstone>(&bytes) {
            Ok(tombstone)
                if tombstone.schema == TOMBSTONE_SCHEMA && tombstone.version == STATE_VERSION =>
            {
                tombstone.reason
            }
            _ => "external generation is inactive".to_string(),
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.directory.join("output.lock")
    }

    fn journal_path(&self) -> PathBuf {
        self.directory.join("capture.pending")
    }

    fn managed_journal_path(&self) -> PathBuf {
        self.directory.join("managed-capture.pending")
    }

    fn ownership_path(&self) -> PathBuf {
        self.directory.join("managed-output")
    }

    fn active_path(&self) -> PathBuf {
        self.directory.join("active")
    }

    fn tombstone_path(&self) -> PathBuf {
        self.directory.join("inactive")
    }

    fn generations_dir(&self) -> PathBuf {
        self.directory.join("generations")
    }

    fn action_stages_dir(&self) -> PathBuf {
        self.directory.join("action-stages")
    }

    fn action_stage_path(&self, journal: &ManagedCaptureJournal) -> PathBuf {
        self.action_stages_dir().join(&journal.action_stage_name)
    }

    fn validate_private_action_stage_locked(
        &self,
        journal: &ManagedCaptureJournal,
        receipt: &ActionCacheStageReceipt,
        allow_missing: bool,
    ) -> Result<()> {
        let path = self.action_stage_path(journal);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                CertifiedActionCacheStage::open(&path, receipt.clone())?;
                Ok(())
            }
            Ok(_) => anyhow::bail!("managed action stage is not a directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_missing => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!("managed action stage is missing")
            }
            Err(error) => Err(error).context("inspect managed action stage"),
        }
    }

    fn remove_private_action_stage_locked(
        &self,
        journal: &ManagedCaptureJournal,
        receipt: Option<&ActionCacheStageReceipt>,
        allow_missing: bool,
    ) -> Result<()> {
        let path = self.action_stage_path(journal);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => anyhow::bail!("managed action stage is not a directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_missing => {
                return Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!("managed action stage is missing")
            }
            Err(error) => return Err(error).context("inspect managed action stage"),
        }
        if let Some(receipt) = receipt {
            CertifiedActionCacheStage::open(&path, receipt.clone())?;
        }
        fs::remove_dir_all(&path).context("remove managed action stage")?;
        sync_directory(&self.action_stages_dir())
    }

    fn generation_path(&self, digest: [u8; 32]) -> PathBuf {
        self.generations_dir()
            .join(format!("{}.snapshot", hex::encode(digest)))
    }
}

fn output_root_hash(output: &Path) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rush.external-output-state.v1\n");
    let bytes = output.as_os_str().as_encoded_bytes();
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

fn reusable_action_count(snapshot: &ExternalAnalysisSnapshot) -> usize {
    snapshot
        .graph
        .actions
        .iter()
        .filter(|action| action.cacheable && !action.always_run)
        .count()
}

/// Exclusive capture ownership for one external output path.
///
/// Dropping an unfinished transaction deliberately leaves its journal on disk.
/// A reader then rejects replay, and the next writer invalidates the old active
/// generation before starting a clean recapture.
pub struct ExternalCaptureTransaction {
    store: ExternalGenerationStore,
    _lock: StateLock,
    finished: bool,
}

impl ExternalCaptureTransaction {
    /// Publish a complete snapshot generation and make it the only active
    /// generation for this output path.
    pub fn commit(mut self, snapshot: &ExternalAnalysisSnapshot) -> Result<()> {
        let snapshot_digest = self.store.store_snapshot_blob(snapshot)?;
        self.store.publish_active_digest(snapshot_digest)?;
        remove_if_present(&self.store.journal_path())?;
        self.finished = true;
        Ok(())
    }

    /// Finish a failed capture after the caller has restored the exact prior
    /// output tree. The previously active snapshot remains selected.
    pub fn finish_rolled_back(mut self) -> Result<()> {
        remove_if_present(&self.store.journal_path())?;
        self.finished = true;
        Ok(())
    }

    /// Record that Make succeeded but capture certification did not. Any prior
    /// snapshot is invalidated because the visible output tree may now belong
    /// to an uncertified generation.
    pub fn invalidate(mut self, reason: &str) -> Result<()> {
        self.store.invalidate_locked(reason)?;
        remove_if_present(&self.store.journal_path())?;
        self.finished = true;
        Ok(())
    }
}

/// Exclusive transaction for one clean analyzer run at the exact output path.
///
/// Dropping it leaves the private journal intact. Recovery then rolls back
/// work that had not reached the commit-ready boundary, or completes a
/// snapshot publication that had crossed it.
pub struct ExternalManagedCapture {
    store: ExternalGenerationStore,
    journal: ManagedCaptureJournal,
    identity: ExternalCaptureTransactionIdentity,
    _lock: StateLock,
    finished: bool,
}

impl ExternalManagedCapture {
    /// Identity that the analyzer observation must bind and return.
    pub fn identity(&self) -> ExternalCaptureTransactionIdentity {
        self.identity
    }

    /// Exact private path where this capture must stage certified action
    /// results before committing a reusable snapshot.
    pub fn action_stage_directory(&self) -> PathBuf {
        self.store.action_stage_path(&self.journal)
    }

    fn verify_fresh_output(&self) -> Result<()> {
        let fresh = self
            .journal
            .fresh
            .context("managed capture journal lacks fresh directory identity")?;
        if output_directory_identity(&self.store.expected_key.output_root)? != Some(fresh) {
            anyhow::bail!("managed output directory identity changed during capture");
        }
        Ok(())
    }

    fn prepare_commit(&mut self, snapshot: &ExternalAnalysisSnapshot) -> Result<[u8; 32]> {
        self.verify_fresh_output()?;
        if reusable_action_count(snapshot) != 0 {
            anyhow::bail!("reusable external actions require certified action-result publication");
        }
        if self.action_stage_directory().exists() {
            anyhow::bail!("snapshot-only capture has an unexpected action stage");
        }
        let snapshot_digest = self.store.store_snapshot_blob(snapshot)?;
        self.journal.phase = ManagedCapturePhase::CommitReady { snapshot_digest };
        self.store.write_managed_journal(&self.journal)?;
        Ok(snapshot_digest)
    }

    fn prepare_replay_commit(&mut self, snapshot: &ExternalAnalysisSnapshot) -> Result<[u8; 32]> {
        self.verify_fresh_output()?;
        if self.action_stage_directory().exists() {
            anyhow::bail!("clean external replay has an unexpected action stage");
        }
        let snapshot_digest = self.store.store_snapshot_blob(snapshot)?;
        self.journal.phase = ManagedCapturePhase::CommitReady { snapshot_digest };
        self.store.write_managed_journal(&self.journal)?;
        Ok(snapshot_digest)
    }

    fn prepare_commit_with_action_results(
        &mut self,
        snapshot: &ExternalAnalysisSnapshot,
        adoption: ExternalCaptureAdoption,
    ) -> Result<([u8; 32], CertifiedActionCacheStage, ActionCacheStageReceipt)> {
        self.verify_fresh_output()?;
        let expected = reusable_action_count(snapshot);
        if expected == 0 || adoption.adopted_action_count() != expected {
            anyhow::bail!("certified action stage does not cover every reusable action");
        }
        let adoption_snapshot_digest = adoption.snapshot_digest();
        let stage = adoption.into_stage();
        if stage.directory() != self.action_stage_directory() {
            anyhow::bail!("certified action stage does not belong to this capture");
        }
        let receipt = stage.receipt().clone();
        if receipt.entry_count() != expected as u64 {
            anyhow::bail!("certified action stage receipt has the wrong action count");
        }
        let snapshot_digest = self.store.store_snapshot_blob(snapshot)?;
        if snapshot_digest != adoption_snapshot_digest {
            anyhow::bail!("certified action stage belongs to a different snapshot");
        }
        self.journal.phase = ManagedCapturePhase::ActionResultsReady {
            snapshot_digest,
            stage_receipt: receipt.clone(),
        };
        self.store.write_managed_journal(&self.journal)?;
        Ok((snapshot_digest, stage, receipt))
    }

    /// Publish a capture whose snapshot contains no reusable actions and leave
    /// its freshly built tree at the original logical output path.
    pub fn commit(mut self, snapshot: &ExternalAnalysisSnapshot) -> Result<()> {
        let snapshot_digest = self.prepare_commit(snapshot)?;
        self.store
            .finish_managed_commit_locked(self.journal.clone(), snapshot_digest, None)?;
        self.finished = true;
        Ok(())
    }

    /// Publish a clean replay of an already active snapshot.
    ///
    /// Reusable action records were certified when the generation was first
    /// adopted. Replaying a continuation in a fresh managed tree therefore
    /// republishes the same authenticated snapshot without staging a second
    /// copy of those records. The ordinary commit-ready journal boundary keeps
    /// output replacement restart-safe.
    pub fn commit_replay(mut self, snapshot: &ExternalAnalysisSnapshot) -> Result<()> {
        let snapshot_digest = self.prepare_replay_commit(snapshot)?;
        self.store
            .finish_managed_commit_locked(self.journal.clone(), snapshot_digest, None)?;
        self.finished = true;
        Ok(())
    }

    /// Publish a complete captured action set before making its snapshot
    /// replayable, preserving a restartable journal boundary around both.
    pub async fn commit_with_action_results(
        mut self,
        snapshot: &ExternalAnalysisSnapshot,
        adoption: ExternalCaptureAdoption,
        cache: &ActionCache,
        cas: &CasStore,
    ) -> Result<()> {
        let (snapshot_digest, stage, stage_receipt) =
            self.prepare_commit_with_action_results(snapshot, adoption)?;
        stage.publish_into_with_cas(cache, cas).await?;
        self.journal.phase = ManagedCapturePhase::ActionResultsPublished {
            snapshot_digest,
            stage_receipt: stage_receipt.clone(),
        };
        self.store.write_managed_journal(&self.journal)?;
        self.store.finish_managed_commit_locked(
            self.journal.clone(),
            snapshot_digest,
            Some(stage_receipt),
        )?;
        self.finished = true;
        Ok(())
    }

    /// Discard a failed analyzer tree and restore the exact pre-capture tree.
    pub fn rollback(mut self) -> Result<()> {
        self.store.rollback_managed_locked(&self.journal)?;
        self.finished = true;
        Ok(())
    }

    /// Keep a successful analyzer's output tree, but invalidate replay because
    /// completeness certification failed.
    pub fn keep_uncertified(mut self, reason: &str) -> Result<()> {
        self.verify_fresh_output()?;
        self.journal.phase = ManagedCapturePhase::KeepingUncertified {
            reason: reason.to_string(),
        };
        self.store.write_managed_journal(&self.journal)?;
        self.store
            .finish_keep_uncertified_locked(&self.journal, reason)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for ExternalManagedCapture {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // The journal is the recovery authority. Never remove it from Drop.
    }
}

impl Drop for ExternalCaptureTransaction {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // The durable journal is the fail-closed recovery signal. Never remove
        // it from Drop: unwinding and process death must have the same policy.
    }
}

#[derive(Clone, Copy)]
enum LockMode {
    Shared,
    Exclusive,
}

struct StateLock {
    _file: File,
}

#[cfg(unix)]
impl Drop for StateLock {
    #[allow(deprecated)]
    fn drop(&mut self) {
        use nix::fcntl::{flock, FlockArg};
        use std::os::fd::AsRawFd;

        let _ = flock(self._file.as_raw_fd(), FlockArg::Unlock);
    }
}

impl StateLock {
    #[cfg(unix)]
    #[allow(deprecated)]
    fn acquire(path: &Path, mode: LockMode) -> Result<Self> {
        use nix::fcntl::{flock, FlockArg};
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("open external state lock {}", path.display()))?;
        let argument = match mode {
            LockMode::Shared => FlockArg::LockSharedNonblock,
            LockMode::Exclusive => FlockArg::LockExclusiveNonblock,
        };
        flock(file.as_raw_fd(), argument)
            .with_context(|| format!("lock external state {}", path.display()))?;
        Ok(Self { _file: file })
    }

    #[cfg(not(unix))]
    fn acquire(_path: &Path, _mode: LockMode) -> Result<Self> {
        anyhow::bail!("external generation state requires Unix flock")
    }
}

fn output_parent(output: &Path) -> Result<&Path> {
    if !output.is_absolute() {
        anyhow::bail!("managed external output path must be absolute");
    }
    let parent = output
        .parent()
        .context("managed external output path has no parent")?;
    if output.file_name().is_none() || parent == output {
        anyhow::bail!("managed external output path cannot be a filesystem root");
    }
    let metadata = fs::metadata(parent).context("inspect managed output parent")?;
    if !metadata.file_type().is_dir() {
        anyhow::bail!("managed external output parent is not a directory");
    }
    if fs::canonicalize(parent).context("canonicalize managed output parent")? != parent {
        anyhow::bail!("managed external output parent must use its canonical path");
    }
    if output.exists()
        && fs::canonicalize(output).context("canonicalize managed output directory")? != output
    {
        anyhow::bail!("managed external output must use its canonical path");
    }
    Ok(parent)
}

fn sibling_name(output: &Path, role: &str, transaction: &str) -> Result<PathBuf> {
    let name = output
        .file_name()
        .context("managed external output path has no final component")?;
    let mut sibling = std::ffi::OsString::from(".");
    sibling.push(name);
    sibling.push(".");
    sibling.push(role);
    sibling.push("-");
    sibling.push(transaction);
    Ok(PathBuf::from(sibling))
}

fn is_single_normal_component(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

#[cfg(unix)]
fn output_directory_identity(path: &Path) -> Result<Option<OutputDirectoryIdentity>> {
    use std::os::unix::fs::MetadataExt;

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect managed output directory"),
    };
    if !metadata.file_type().is_dir() {
        anyhow::bail!("managed external output is not a directory");
    }
    Ok(Some(OutputDirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode() & 0o7777,
    }))
}

#[cfg(not(unix))]
fn output_directory_identity(_path: &Path) -> Result<Option<OutputDirectoryIdentity>> {
    anyhow::bail!("managed external output requires Unix directory identities")
}

fn create_output_directory(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        use std::os::unix::fs::PermissionsExt;

        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(path)
            .with_context(|| format!("create managed output directory {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("set managed output mode {}", path.display()))?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        anyhow::bail!("managed external output requires Unix directory creation")
    }
}

fn remove_owned_directory(path: &Path, expected: OutputDirectoryIdentity) -> Result<()> {
    if output_directory_identity(path)? != Some(expected) {
        anyhow::bail!("refusing to remove a managed directory whose identity changed");
    }
    fs::remove_dir_all(path).context("remove owned managed output directory")
}

#[cfg(target_os = "linux")]
fn rename_noreplace(source: &Path, destination: &Path) -> Result<()> {
    nix::fcntl::renameat2(
        None,
        source,
        None,
        destination,
        nix::fcntl::RenameFlags::RENAME_NOREPLACE,
    )
    .with_context(|| {
        format!(
            "rename managed output without replacement {} -> {}",
            source.display(),
            destination.display()
        )
    })
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(_source: &Path, _destination: &Path) -> Result<()> {
    anyhow::bail!("managed external output requires Linux no-replace rename")
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect {}", path.display()))?;
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("external state path has no parent directory")?;
    let temporary = parent.join(format!(
        ".external-state-{}-{}.tmp",
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
            .with_context(|| format!("create {}", temporary.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "publish external state {} -> {}",
                temporary.display(),
                path.display()
            )
        })?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_directory(
            path.parent()
                .context("external state path has no parent directory")?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open state directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("sync state directory {}", path.display()))
}

fn managed_capture_transaction_identity(
    journal: &ManagedCaptureJournal,
) -> Result<ExternalCaptureTransactionIdentity> {
    let fresh = journal
        .fresh
        .context("managed capture identity lacks a fresh directory")?;
    let transaction_bytes = journal.transaction.as_bytes();
    let transaction_length = u64::try_from(transaction_bytes.len())
        .context("managed capture transaction length does not fit u64")?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rush.external-managed-capture-context.v1\0");
    hasher.update(&journal.output_hash);
    hasher.update(&journal.key_hash);
    hasher.update(&journal.capture_id);
    hasher.update(&transaction_length.to_le_bytes());
    hasher.update(transaction_bytes);
    for identity in [journal.original, fresh] {
        hasher.update(&identity.device.to_le_bytes());
        hasher.update(&identity.inode.to_le_bytes());
        hasher.update(&identity.mode.to_le_bytes());
    }
    Ok(ExternalCaptureTransactionIdentity::new(
        journal.capture_id,
        *hasher.finalize().as_bytes(),
    ))
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_action_identity::{
        certify_external_action_capture_witnesses, ExternalActionCaptureWitness,
        ExternalExecutableWitness,
    };
    use crate::external_analysis_snapshot::{
        ExternalAction, ExternalActionGraph, ExternalAnalyzerIdentity, ExternalCommand,
        ExternalOutput, ExternalSnapshotDecision,
    };
    use crate::external_capture_adoption::stage_external_capture_results;
    use bytes::Bytes;
    use rush_action::action_manifest::ManifestResourceClass;
    use rush_action::external_action_key::{
        ExternalArgumentKey, ExternalInputValue, ExternalOutputKind, ExternalPathKey,
    };
    use rush_cache::{ActionCache, CacheConfig};
    use rush_incremental::DependencyManifest;
    use rush_protocol::command::v1::RunRequest;
    use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use std::collections::BTreeMap;

    fn snapshot(root: &Path, goal: &[u8]) -> ExternalAnalysisSnapshot {
        let source = root.join("src");
        let output = root.join("obj");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&output).unwrap();
        let analyzer = ExternalAnalyzerIdentity {
            kind: "test-analyzer".to_string(),
            executable_digest: [7; 32],
            argv: vec![b"analyzer".to_vec(), goal.to_vec()],
            working_directory: source.clone(),
            environment: BTreeMap::new(),
            build_options: BTreeMap::new(),
            platform: BTreeMap::new(),
        };
        let key = ExternalAnalysisKey::new(
            source.clone(),
            output.clone(),
            &analyzer,
            vec![goal.to_vec()],
        );
        ExternalAnalysisSnapshot::new(
            key,
            analyzer,
            DependencyManifest::from_deps(Vec::new()),
            ExternalActionGraph {
                roots: BTreeMap::from([("src".to_string(), source), ("obj".to_string(), output)]),
                kconfig: None,
                initial_inputs: Vec::new(),
                empty_output_inputs: Vec::new(),
                actions: Vec::new(),
                analysis_barriers: Vec::new(),
                requested_outputs: Vec::new(),
            },
        )
        .unwrap()
    }

    fn rooted(root: &str, path: &str) -> ExternalPathKey {
        ExternalPathKey {
            root: root.to_string(),
            path: path.as_bytes().to_vec(),
        }
    }

    fn reusable_snapshot(root: &Path) -> (ExternalAnalysisSnapshot, ExternalActionCaptureWitness) {
        let empty = snapshot(root, b"all");
        let tool = empty.key.workspace_root.join("tool");
        fs::write(&tool, b"captured compiler").unwrap();
        let action = ExternalAction {
            logical_id: "compile".to_string(),
            mnemonic: "KernelCompile".to_string(),
            commands: vec![ExternalCommand {
                executable: rooted("src", "tool"),
                argv0: ExternalArgumentKey::Path(rooted("src", "tool")),
                argv: vec![ExternalArgumentKey::Literal(b"captured.c".to_vec())],
                environment: None,
                working_directory: None,
            }],
            working_directory: rooted("obj", "."),
            environment: BTreeMap::new(),
            signal_state: Default::default(),
            file_creation_mask: 0o022,
            credentials: Default::default(),
            normal_inputs: Vec::new(),
            order_only_inputs: Vec::new(),
            outputs: vec![ExternalOutput {
                path: rooted("obj", "captured.o"),
                kind: ExternalOutputKind::File,
                mode: 0o644,
            }],
            dependencies: Vec::new(),
            config_deps: Vec::new(),
            depfile: None,
            restat: true,
            always_run: false,
            cacheable: true,
            local_only: true,
            sandbox: false,
            resource: ManifestResourceClass::KernelCompile,
            execution_properties: BTreeMap::new(),
        };
        let snapshot = ExternalAnalysisSnapshot::new(
            empty.key,
            empty.analyzer,
            empty.analysis_manifest,
            ExternalActionGraph {
                roots: empty.graph.roots,
                kconfig: None,
                initial_inputs: Vec::new(),
                empty_output_inputs: Vec::new(),
                actions: vec![action],
                analysis_barriers: Vec::new(),
                requested_outputs: vec![rooted("obj", "captured.o")],
            },
        )
        .unwrap();
        let witness = ExternalActionCaptureWitness {
            logical_id: "compile".to_string(),
            executables: vec![ExternalExecutableWitness {
                path: rooted("src", "tool"),
                value: ExternalInputValue::File {
                    digest: [3; 32],
                    executable: true,
                },
            }],
            inputs: Vec::new(),
        };
        (snapshot, witness)
    }

    fn disk_cache(root: &Path) -> ActionCache {
        ActionCache::with_disk(CacheConfig {
            cache_root: root.to_path_buf(),
            max_size: 0,
            ttl_seconds: 0,
        })
        .unwrap()
    }

    fn semantics(root: &Path) -> CommandSemantics {
        CommandSemantics::from_run_request(&RunRequest {
            auth_cookie: Bytes::new(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"linux")],
            working_directory: Bytes::copy_from_slice(root.as_os_str().as_encoded_bytes()),
            environment: Vec::new(),
            block_for_lock: true,
            client_build_id: "external-generation-test".to_string(),
            workspace_config: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn committed_generation_survives_a_new_store_instance() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        let state = temp.path().join("state");
        let store = ExternalGenerationStore::new(&state, snapshot.key.clone());
        store.begin_capture().unwrap().commit(&snapshot).unwrap();

        let reopened = ExternalGenerationStore::new(&state, snapshot.key.clone());
        match reopened.load_active_if_fresh(&semantics(temp.path())) {
            ExternalSnapshotAdmission::Hit(loaded) => assert_eq!(loaded, snapshot),
            admission => panic!("unexpected decision {}", admission.decision().as_str()),
        }
        assert!(!store
            .state_directory()
            .starts_with(&snapshot.key.output_root));
    }

    #[test]
    fn unfinished_capture_blocks_reads_and_forces_next_capture_to_recapture() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key.clone());
        store.begin_capture().unwrap().commit(&snapshot).unwrap();
        let interrupted = store.begin_capture().unwrap();
        drop(interrupted);

        let admission = store.load_active_if_fresh(&semantics(temp.path()));
        assert_eq!(
            admission.decision(),
            ExternalSnapshotDecision::SnapshotInvalid
        );
        assert!(admission.reason().unwrap().contains("unresolved"));

        store.begin_capture().unwrap().finish_rolled_back().unwrap();
        let admission = store.load_active_if_fresh(&semantics(temp.path()));
        assert_eq!(
            admission.decision(),
            ExternalSnapshotDecision::SnapshotMissing
        );
        assert!(admission.reason().unwrap().contains("interrupted"));
    }

    #[test]
    fn confirmed_output_rollback_preserves_the_prior_generation() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key.clone());
        store.begin_capture().unwrap().commit(&snapshot).unwrap();
        store.begin_capture().unwrap().finish_rolled_back().unwrap();

        assert!(matches!(
            store.load_active_if_fresh(&semantics(temp.path())),
            ExternalSnapshotAdmission::Hit(_)
        ));
    }

    #[test]
    fn uncertified_success_invalidates_the_prior_generation() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key.clone());
        store.begin_capture().unwrap().commit(&snapshot).unwrap();
        store
            .begin_capture()
            .unwrap()
            .invalidate("capture completeness check failed")
            .unwrap();

        let admission = store.load_active_if_fresh(&semantics(temp.path()));
        assert_eq!(
            admission.decision(),
            ExternalSnapshotDecision::SnapshotMissing
        );
        assert_eq!(
            admission.reason(),
            Some("capture completeness check failed")
        );
    }

    #[test]
    fn wrong_key_cannot_replace_an_active_generation() {
        let temp = tempfile::tempdir().unwrap();
        let original = snapshot(temp.path(), b"all");
        let other = snapshot(temp.path(), b"modules");
        let store = ExternalGenerationStore::new(&temp.path().join("state"), original.key.clone());
        let transaction = store.begin_capture().unwrap();
        assert!(transaction.commit(&other).is_err());

        let admission = store.load_active_if_fresh(&semantics(temp.path()));
        assert_eq!(
            admission.decision(),
            ExternalSnapshotDecision::SnapshotInvalid
        );
    }

    #[test]
    fn all_keys_for_one_output_path_share_state_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let first = snapshot(temp.path(), b"all");
        let second = snapshot(temp.path(), b"modules");
        let state = temp.path().join("state");
        let first_store = ExternalGenerationStore::new(&state, first.key);
        let second_store = ExternalGenerationStore::new(&state, second.key);
        assert_eq!(
            first_store.state_directory(),
            second_store.state_directory()
        );
    }

    #[test]
    fn managed_output_claim_refuses_preexisting_user_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        fs::write(snapshot.key.output_root.join("user-owned"), b"keep").unwrap();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key);

        let error = store.claim_managed_output().unwrap_err();
        assert!(error.to_string().contains("nonempty"));
        assert_eq!(
            fs::read(store.expected_key.output_root.join("user-owned")).unwrap(),
            b"keep"
        );
        assert!(!store.ownership_path().exists());
    }

    #[test]
    fn managed_output_claim_is_idempotent_but_replacement_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        let output = snapshot.key.output_root.clone();
        fs::remove_dir(&output).unwrap();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key);

        store.claim_managed_output().unwrap();
        store.claim_managed_output().unwrap();
        assert_eq!(fs::read_dir(&output).unwrap().count(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(store.ownership_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(store.state_directory())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        fs::remove_dir(&output).unwrap();
        fs::create_dir(&output).unwrap();

        let admission = store.load_active_if_fresh(&semantics(temp.path()));
        assert_eq!(
            admission.decision(),
            ExternalSnapshotDecision::SnapshotInvalid
        );
        assert!(admission.reason().unwrap().contains("ownership changed"));
    }

    #[cfg(unix)]
    #[test]
    fn managed_output_claim_refuses_alias_paths() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let mut snapshot = snapshot(temp.path(), b"all");
        let real_parent = temp.path().join("real-parent");
        fs::create_dir(&real_parent).unwrap();
        let alias_parent = temp.path().join("alias-parent");
        symlink(&real_parent, &alias_parent).unwrap();
        snapshot.key.output_root = alias_parent.join("obj");
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key);

        let error = store.claim_managed_output().unwrap_err();
        assert!(error.to_string().contains("canonical"));
        assert!(!real_parent.join("obj").exists());
    }

    #[test]
    fn managed_capture_rollback_restores_tree_and_active_generation() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        let output = snapshot.key.output_root.clone();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key.clone());
        store.claim_managed_output().unwrap();

        let first = store.begin_clean_managed_capture().unwrap();
        let first_identity = first.identity();
        assert_ne!(first_identity.capture_id(), [0; 16]);
        assert_ne!(first_identity.context_digest(), [0; 32]);
        fs::write(output.join("old"), b"old-tree").unwrap();
        first.commit(&snapshot).unwrap();
        let second = store.begin_clean_managed_capture().unwrap();
        let second_identity = second.identity();
        assert_ne!(second_identity.capture_id(), first_identity.capture_id());
        assert_ne!(
            second_identity.context_digest(),
            first_identity.context_digest()
        );
        assert!(!output.join("old").exists());
        fs::write(output.join("partial"), b"failed-capture").unwrap();
        second.rollback().unwrap();

        assert_eq!(fs::read(output.join("old")).unwrap(), b"old-tree");
        assert!(!output.join("partial").exists());
        assert!(matches!(
            store.load_active_if_fresh(&semantics(temp.path())),
            ExternalSnapshotAdmission::Hit(_)
        ));
        assert!(!store.managed_journal_path().exists());
    }

    #[test]
    fn dropped_managed_capture_blocks_replay_then_recovers_old_tree() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        let output = snapshot.key.output_root.clone();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key.clone());
        store.claim_managed_output().unwrap();
        let first = store.begin_clean_managed_capture().unwrap();
        fs::write(output.join("old"), b"certified").unwrap();
        first.commit(&snapshot).unwrap();

        let interrupted = store.begin_clean_managed_capture().unwrap();
        fs::write(output.join("partial"), b"uncertain").unwrap();
        drop(interrupted);
        assert_eq!(
            store
                .load_active_if_fresh(&semantics(temp.path()))
                .decision(),
            ExternalSnapshotDecision::SnapshotInvalid
        );
        assert_eq!(
            store.recover_managed_capture().unwrap(),
            ManagedCaptureRecovery::RolledBack
        );
        assert_eq!(fs::read(output.join("old")).unwrap(), b"certified");
        assert!(!output.join("partial").exists());
    }

    #[test]
    fn preparation_crash_removes_only_its_empty_sibling() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        let output = snapshot.key.output_root.clone();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key);
        store.claim_managed_output().unwrap();
        let original = output_directory_identity(&output).unwrap().unwrap();
        let transaction = "preparation-crash".to_string();
        let journal = ManagedCaptureJournal {
            schema: MANAGED_JOURNAL_SCHEMA.to_string(),
            version: MANAGED_JOURNAL_VERSION,
            output_hash: store.output_hash,
            key_hash: store.expected_key.stable_hash(),
            capture_id: [7; 16],
            transaction: transaction.clone(),
            backup_name: sibling_name(&output, "rush-capture-backup", &transaction).unwrap(),
            fresh_name: sibling_name(&output, "rush-capture-fresh", &transaction).unwrap(),
            action_stage_name: PathBuf::from(format!("{transaction}.stage")),
            original,
            fresh: None,
            phase: ManagedCapturePhase::Preparing,
        };
        store.write_managed_journal(&journal).unwrap();
        let fresh = output.parent().unwrap().join(&journal.fresh_name);
        fs::create_dir(&fresh).unwrap();

        assert_eq!(
            store.recover_managed_capture().unwrap(),
            ManagedCaptureRecovery::RolledBack
        );
        assert!(!fresh.exists());
        assert_eq!(output_directory_identity(&output).unwrap(), Some(original));
    }

    #[test]
    fn prior_managed_journal_version_fails_closed_without_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        let output = snapshot.key.output_root.clone();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key);
        store.claim_managed_output().unwrap();
        let original = output_directory_identity(&output).unwrap().unwrap();
        let transaction = "prior-version".to_string();
        let journal = ManagedCaptureJournal {
            schema: MANAGED_JOURNAL_SCHEMA.to_string(),
            version: MANAGED_JOURNAL_VERSION - 1,
            output_hash: store.output_hash,
            key_hash: store.expected_key.stable_hash(),
            capture_id: [7; 16],
            transaction: transaction.clone(),
            backup_name: sibling_name(&output, "rush-capture-backup", &transaction).unwrap(),
            fresh_name: sibling_name(&output, "rush-capture-fresh", &transaction).unwrap(),
            action_stage_name: PathBuf::from(format!("{transaction}.stage")),
            original,
            fresh: None,
            phase: ManagedCapturePhase::Preparing,
        };
        store.write_managed_journal(&journal).unwrap();

        let error = store.recover_managed_capture().unwrap_err();
        assert!(error.to_string().contains("unsupported identity"));
        assert!(store.managed_journal_path().exists());
        assert_eq!(output_directory_identity(&output).unwrap(), Some(original));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_rename_never_replaces_an_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("source-value"), b"source").unwrap();
        fs::write(destination.join("destination-value"), b"destination").unwrap();

        assert!(rename_noreplace(&source, &destination).is_err());
        assert_eq!(fs::read(source.join("source-value")).unwrap(), b"source");
        assert_eq!(
            fs::read(destination.join("destination-value")).unwrap(),
            b"destination"
        );
    }

    #[test]
    fn commit_ready_crash_completes_publication_on_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        let output = snapshot.key.output_root.clone();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key.clone());
        store.claim_managed_output().unwrap();

        let mut interrupted = store.begin_clean_managed_capture().unwrap();
        fs::write(output.join("new"), b"certified").unwrap();
        interrupted.prepare_commit(&snapshot).unwrap();
        drop(interrupted);

        assert_eq!(
            store.recover_managed_capture().unwrap(),
            ManagedCaptureRecovery::Published
        );
        assert_eq!(fs::read(output.join("new")).unwrap(), b"certified");
        assert!(matches!(
            store.load_active_if_fresh(&semantics(temp.path())),
            ExternalSnapshotAdmission::Hit(_)
        ));
        assert!(!store.managed_journal_path().exists());
    }

    #[test]
    fn snapshot_only_commit_rejects_reusable_actions() {
        let temp = tempfile::tempdir().unwrap();
        let (snapshot, _) = reusable_snapshot(temp.path());
        let output = snapshot.key.output_root.clone();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key.clone());
        store.claim_managed_output().unwrap();
        let capture = store.begin_clean_managed_capture().unwrap();
        fs::write(output.join("captured.o"), b"captured output").unwrap();

        let error = capture.commit(&snapshot).unwrap_err();
        assert!(error.to_string().contains("action-result publication"));
        assert_eq!(
            store
                .load_active_if_fresh(&semantics(temp.path()))
                .decision(),
            ExternalSnapshotDecision::SnapshotInvalid
        );
        assert!(store.managed_journal_path().exists());
        assert_eq!(
            store.recover_managed_capture().unwrap(),
            ManagedCaptureRecovery::RolledBack
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn action_results_publish_before_snapshot_admission() {
        let temp = tempfile::tempdir().unwrap();
        let (snapshot, witness) = reusable_snapshot(temp.path());
        let output = snapshot.key.output_root.clone();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key.clone());
        store.claim_managed_output().unwrap();
        let capture = store.begin_clean_managed_capture().unwrap();
        fs::write(output.join("captured.o"), b"captured output").unwrap();
        let cas_root = temp.path().join("cas");
        let cas = CasStore::new(&cas_root).unwrap();
        let adoption = stage_external_capture_results(
            &snapshot,
            std::slice::from_ref(&witness),
            &cas,
            &capture.action_stage_directory(),
        )
        .unwrap();
        let action_id = certify_external_action_capture_witnesses(
            &snapshot.graph,
            std::slice::from_ref(&witness),
        )
        .unwrap()["compile"]
            .clone();
        let cache = disk_cache(&temp.path().join("cache"));

        capture
            .commit_with_action_results(&snapshot, adoption, &cache, &cas)
            .await
            .unwrap();

        assert!(cache.get(&action_id).is_some());
        assert!(matches!(
            store.load_active_if_fresh(&semantics(temp.path())),
            ExternalSnapshotAdmission::Hit(_)
        ));
        assert!(!store.managed_journal_path().exists());
        assert!(fs::read_dir(store.action_stages_dir())
            .unwrap()
            .next()
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn action_results_cannot_authorize_a_different_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let (snapshot, witness) = reusable_snapshot(temp.path());
        let mut different_snapshot = snapshot.clone();
        different_snapshot.graph.actions[0].mnemonic = "KernelCodegen".to_string();
        different_snapshot.validate_structure().unwrap();
        let output = snapshot.key.output_root.clone();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key.clone());
        store.claim_managed_output().unwrap();
        let capture = store.begin_clean_managed_capture().unwrap();
        let action_stage = capture.action_stage_directory();
        fs::write(output.join("captured.o"), b"captured output").unwrap();
        let cas = CasStore::new(&temp.path().join("cas")).unwrap();
        let adoption = stage_external_capture_results(
            &snapshot,
            std::slice::from_ref(&witness),
            &cas,
            &capture.action_stage_directory(),
        )
        .unwrap();
        let cache = disk_cache(&temp.path().join("cache"));

        let error = capture
            .commit_with_action_results(&different_snapshot, adoption, &cache, &cas)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("different snapshot"));
        assert_eq!(
            store
                .load_active_if_fresh(&semantics(temp.path()))
                .decision(),
            ExternalSnapshotDecision::SnapshotInvalid
        );
        assert!(store.managed_journal_path().exists());
        assert!(action_stage.exists());
        assert_eq!(
            store.recover_managed_capture().unwrap(),
            ManagedCaptureRecovery::RolledBack
        );
        assert!(!action_stage.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn action_results_ready_crash_republishes_then_admits_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let (snapshot, witness) = reusable_snapshot(temp.path());
        let output = snapshot.key.output_root.clone();
        let state = temp.path().join("state");
        let store = ExternalGenerationStore::new(&state, snapshot.key.clone());
        store.claim_managed_output().unwrap();
        let mut capture = store.begin_clean_managed_capture().unwrap();
        fs::write(output.join("captured.o"), b"captured output").unwrap();
        let cas = CasStore::new(&temp.path().join("cas")).unwrap();
        let adoption = stage_external_capture_results(
            &snapshot,
            std::slice::from_ref(&witness),
            &cas,
            &capture.action_stage_directory(),
        )
        .unwrap();
        let action_id = certify_external_action_capture_witnesses(
            &snapshot.graph,
            std::slice::from_ref(&witness),
        )
        .unwrap()["compile"]
            .clone();
        let (_, stage, _) = capture
            .prepare_commit_with_action_results(&snapshot, adoption)
            .unwrap();
        drop(stage);
        drop(capture);
        let cache_root = temp.path().join("cache");
        let cache = disk_cache(&cache_root);

        assert!(store.recover_managed_capture().is_err());
        assert!(cache.get(&action_id).is_none());
        assert_eq!(
            store
                .recover_managed_capture_with_cache(&cache, &cas)
                .await
                .unwrap(),
            ManagedCaptureRecovery::Published
        );
        assert!(disk_cache(&cache_root).get(&action_id).is_some());
        assert!(matches!(
            store.load_active_if_fresh(&semantics(temp.path())),
            ExternalSnapshotAdmission::Hit(_)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_cas_blob_keeps_ready_snapshot_unpublished() {
        let temp = tempfile::tempdir().unwrap();
        let (snapshot, witness) = reusable_snapshot(temp.path());
        let output = snapshot.key.output_root.clone();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key.clone());
        store.claim_managed_output().unwrap();
        let mut capture = store.begin_clean_managed_capture().unwrap();
        let captured_output = output.join("captured.o");
        fs::write(&captured_output, b"captured output").unwrap();
        let cas_root = temp.path().join("cas");
        let cas = CasStore::new(&cas_root).unwrap();
        let adoption = stage_external_capture_results(
            &snapshot,
            std::slice::from_ref(&witness),
            &cas,
            &capture.action_stage_directory(),
        )
        .unwrap();
        let action_id = certify_external_action_capture_witnesses(
            &snapshot.graph,
            std::slice::from_ref(&witness),
        )
        .unwrap()["compile"]
            .clone();
        let (_, stage, _) = capture
            .prepare_commit_with_action_results(&snapshot, adoption)
            .unwrap();
        drop(stage);
        drop(capture);
        let record = cas.record_output_durable(&captured_output).unwrap();
        let digest = record.content_digest.unwrap();
        let digest_hex = digest.to_hex();
        fs::remove_file(
            cas_root
                .join("cas")
                .join(&digest_hex[..2])
                .join(&digest_hex),
        )
        .unwrap();
        let cache = disk_cache(&temp.path().join("cache"));

        let error = store
            .recover_managed_capture_with_cache(&cache, &cas)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unavailable CAS content"));
        assert!(cache.get(&action_id).is_none());
        assert_eq!(
            store
                .load_active_if_fresh(&semantics(temp.path()))
                .decision(),
            ExternalSnapshotDecision::SnapshotInvalid
        );
        assert!(store.managed_journal_path().exists());

        cas.record_output_durable(&captured_output).unwrap();
        assert_eq!(
            store
                .recover_managed_capture_with_cache(&cache, &cas)
                .await
                .unwrap(),
            ManagedCaptureRecovery::Published
        );
        assert!(cache.get(&action_id).is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn published_action_results_survive_snapshot_publication_crash() {
        let temp = tempfile::tempdir().unwrap();
        let (snapshot, witness) = reusable_snapshot(temp.path());
        let output = snapshot.key.output_root.clone();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key.clone());
        store.claim_managed_output().unwrap();
        let mut capture = store.begin_clean_managed_capture().unwrap();
        fs::write(output.join("captured.o"), b"captured output").unwrap();
        let cas = CasStore::new(&temp.path().join("cas")).unwrap();
        let adoption = stage_external_capture_results(
            &snapshot,
            std::slice::from_ref(&witness),
            &cas,
            &capture.action_stage_directory(),
        )
        .unwrap();
        let cache = disk_cache(&temp.path().join("cache"));
        let (snapshot_digest, stage, stage_receipt) = capture
            .prepare_commit_with_action_results(&snapshot, adoption)
            .unwrap();
        stage.publish_into_with_cas(&cache, &cas).await.unwrap();
        capture.journal.phase = ManagedCapturePhase::ActionResultsPublished {
            snapshot_digest,
            stage_receipt,
        };
        store.write_managed_journal(&capture.journal).unwrap();
        drop(capture);

        assert_eq!(
            store.recover_managed_capture().unwrap(),
            ManagedCaptureRecovery::Published
        );
        assert!(matches!(
            store.load_active_if_fresh(&semantics(temp.path())),
            ExternalSnapshotAdmission::Hit(_)
        ));
    }

    #[test]
    fn different_goal_can_finish_shared_output_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let first_snapshot = snapshot(temp.path(), b"all");
        let second_snapshot = snapshot(temp.path(), b"modules");
        let output = first_snapshot.key.output_root.clone();
        let state = temp.path().join("state");
        let first_store = ExternalGenerationStore::new(&state, first_snapshot.key.clone());
        first_store.claim_managed_output().unwrap();

        let mut interrupted = first_store.begin_clean_managed_capture().unwrap();
        fs::write(output.join("new"), b"certified-for-first-goal").unwrap();
        interrupted.prepare_commit(&first_snapshot).unwrap();
        drop(interrupted);

        let second_store = ExternalGenerationStore::new(&state, second_snapshot.key);
        assert_eq!(
            second_store.recover_managed_capture().unwrap(),
            ManagedCaptureRecovery::Published
        );
        assert_eq!(
            second_store
                .load_active_if_fresh(&semantics(temp.path()))
                .decision(),
            ExternalSnapshotDecision::SnapshotMissing
        );
        assert!(matches!(
            first_store.load_active_if_fresh(&semantics(temp.path())),
            ExternalSnapshotAdmission::Hit(_)
        ));
    }

    #[test]
    fn uncertified_reason_survives_process_death() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        let output = snapshot.key.output_root.clone();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key);
        store.claim_managed_output().unwrap();

        let mut interrupted = store.begin_clean_managed_capture().unwrap();
        fs::write(output.join("new"), b"uncertified").unwrap();
        interrupted.journal.phase = ManagedCapturePhase::KeepingUncertified {
            reason: "specific completeness failure".to_string(),
        };
        store.write_managed_journal(&interrupted.journal).unwrap();
        drop(interrupted);

        assert_eq!(
            store.recover_managed_capture().unwrap(),
            ManagedCaptureRecovery::KeptUncertified
        );
        let admission = store.load_active_if_fresh(&semantics(temp.path()));
        assert_eq!(admission.reason(), Some("specific completeness failure"));
        assert_eq!(fs::read(output.join("new")).unwrap(), b"uncertified");
    }

    #[test]
    fn uncertified_success_keeps_new_tree_but_invalidates_replay() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        let output = snapshot.key.output_root.clone();
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key.clone());
        store.claim_managed_output().unwrap();
        let first = store.begin_clean_managed_capture().unwrap();
        fs::write(output.join("old"), b"certified").unwrap();
        first.commit(&snapshot).unwrap();

        let uncertified = store.begin_clean_managed_capture().unwrap();
        fs::write(output.join("new"), b"make-success").unwrap();
        uncertified
            .keep_uncertified("capture completeness check failed")
            .unwrap();

        assert!(!output.join("old").exists());
        assert_eq!(fs::read(output.join("new")).unwrap(), b"make-success");
        let admission = store.load_active_if_fresh(&semantics(temp.path()));
        assert_eq!(
            admission.decision(),
            ExternalSnapshotDecision::SnapshotMissing
        );
        assert_eq!(
            admission.reason(),
            Some("capture completeness check failed")
        );
    }

    #[test]
    fn changed_generation_bytes_fail_before_snapshot_decode() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path(), b"all");
        let store = ExternalGenerationStore::new(&temp.path().join("state"), snapshot.key.clone());
        store.begin_capture().unwrap().commit(&snapshot).unwrap();
        let marker: ActiveGeneration =
            bincode::deserialize(&fs::read(store.active_path()).unwrap()).unwrap();
        fs::write(store.generation_path(marker.snapshot_digest), b"changed").unwrap();

        let admission = store.load_active_if_fresh(&semantics(temp.path()));
        assert_eq!(
            admission.decision(),
            ExternalSnapshotDecision::SnapshotInvalid
        );
        assert!(admission.reason().unwrap().contains("digest changed"));
    }
}
