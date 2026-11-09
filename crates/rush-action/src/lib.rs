// Rush Action - Action abstraction
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Action abstraction for build units.
//!
//! Actions represent units of work (compile, link, test) with declared
//! inputs and outputs. They are content-addressed for caching.

#![warn(missing_docs)]

pub mod action_dag;
pub mod action_key;
pub mod action_manifest;
pub mod artifact_value;
pub mod canonical_action_key;
pub mod command_spec;
pub mod external_action_key;
pub mod ids;
pub mod scheduler;
pub mod virtual_artifacts;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

// Invalidate legacy action-cache entries produced before ctime-aware source
// memoization. Those entries may contain outputs keyed by stale source bytes.
const ACTION_ID_DOMAIN: &[u8] = b"rush.legacy-action-id.v2\0";

/// Content-addressed identifier for an action.
///
/// Actions are uniquely identified by the BLAKE3 hash of their inputs,
/// command, and environment. This enables content-based caching.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActionId(blake3::Hash);

impl ActionId {
    /// Create an ActionId from a BLAKE3 hash.
    pub fn new(hash: blake3::Hash) -> Self {
        ActionId(hash)
    }

    /// Get the underlying hash.
    pub fn hash(&self) -> &blake3::Hash {
        &self.0
    }
}

impl std::fmt::Display for ActionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.to_hex())
    }
}

/// An artifact (file) used or produced by an action.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Artifact {
    /// A source file from the repository.
    Source(PathBuf),

    /// A file derived from an action's output.
    Derived {
        /// The action that produces this artifact.
        action: ActionId,
        /// The output path relative to the action's output directory.
        output_path: PathBuf,
    },
}

/// Execution requirements and constraints for an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionInfo {
    /// Whether this action requires network access.
    pub requires_network: bool,

    /// Whether this action must run in a sandbox.
    pub requires_sandbox: bool,

    /// Maximum execution time allowed.
    pub timeout: Duration,
}

impl Default for ExecutionInfo {
    fn default() -> Self {
        ExecutionInfo {
            requires_network: false,
            requires_sandbox: true,
            timeout: Duration::from_secs(300), // 5 minutes
        }
    }
}

/// A command to execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// Path to the executable.
    pub executable: PathBuf,

    /// Command-line arguments.
    pub args: Vec<String>,

    /// Working directory for the spawned process; None = inherit the
    /// orchestrator's cwd (the workspace root). Mojo precompiles run from a
    /// staged source root for Bazel-sandbox parity — the compiler embeds
    /// the positional root verbatim, so paths must be relative to a cwd
    /// that mirrors Bazel's exec root.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,

    /// Execution requirements.
    pub execution_info: ExecutionInfo,
}

/// A build action.
///
/// Actions are the fundamental units of work in Rush. They declare inputs,
/// outputs, and a command to execute. Actions are content-addressed by their
/// inputs and command for efficient caching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    /// Unique identifier (content-based hash).
    pub id: ActionId,

    /// Input artifacts required by this action.
    pub inputs: Vec<Artifact>,

    /// Output artifacts produced by this action.
    pub outputs: Vec<Artifact>,

    /// The command to execute.
    pub command: Command,

    /// Environment variables for the command.
    pub env: HashMap<String, String>,

    /// Mnemonic for this action type (e.g., "CppCompile", "Link").
    pub mnemonic: String,
}

/// Enumerate a GNU thin archive's member paths. See
/// [`artifact_value::fingerprint_path`] for the correctness gate this
/// serves.
pub fn thin_archive_member_paths(archive_path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    Action::thin_archive_member_paths_impl(archive_path)
}

/// Result of an action execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    /// The action that was executed.
    pub action_id: ActionId,

    /// Exit code of the command.
    pub exit_code: i32,

    /// Standard output.
    pub stdout: Vec<u8>,

    /// Standard error.
    pub stderr: Vec<u8>,

    /// Output artifacts produced.
    pub outputs: Vec<Artifact>,

    /// Full output records committed with this result, in `outputs`
    /// order: each output's ArtifactFingerprint (the hit gate compares
    /// these against the live filesystem value, preserving config-skew
    /// protection), plus the shape and content digests a CAS restore needs to
    /// rematerialize deleted outputs. The field also versions the cached
    /// encoding: legacy entries fail to deserialize and become misses.
    pub output_records: Vec<artifact_value::OutputRecord>,

    /// Duration of execution.
    pub duration: Duration,

    /// Whether the result was served from cache.
    pub cached: bool,
}

impl Action {
    fn hash_source_input(hasher: &mut blake3::Hasher, path: &Path) -> anyhow::Result<()> {
        // An intentionally virtual value has no filesystem bytes to hash.
        // Its in-process registry entry came from an authenticated action
        // result whose CAS blobs were checked before registration. Never use
        // the entry while a filesystem object exists: direct replacement must
        // be observed rather than masked by stale acceleration state.
        match std::fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(record) = crate::virtual_artifacts::lookup(path) {
                    hasher.update(b"artifact-fingerprint\0");
                    hasher.update(&record.artifact_fingerprint.0);
                    return Ok(());
                }
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "Action::compute_id: inspect input `{}`: {}",
                    path.display(),
                    error
                ));
            }
        }

        // Plain files go through a process-wide digest memo. Without it,
        // every action re-hashed every declared input: the Mojo toolchain
        // declares its bin/ and lib/ (0.3 GB over 8 files) as inputs to
        // every action, so an incremental build with 889 dirty-slice
        // targets streamed 267 GB of identical bytes through blake3 and
        // saturated memory bandwidth - measured as clean-target
        // verification stretching from ~20 ms to ~1.3 s. The memo hashes
        // stable files once and revalidates mtime, ctime, device, inode,
        // size and mode on every use. Restoring mtime cannot hide a write.
        // Thin archives bypass the memo: their key must track member
        // CONTENT, and editing a member does not touch the archive file's
        // metadata.
        if let Some(digest) = Self::memoized_plain_file_digest(path)? {
            let metadata = std::fs::metadata(path).map_err(|error| {
                anyhow::anyhow!(
                    "Action::compute_id: stat input `{}`: {}",
                    path.display(),
                    error
                )
            })?;
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            let fingerprint = crate::artifact_value::fingerprint_known_plain_from_digest(
                &crate::artifact_value::ContentDigest(digest),
                executable,
            );
            hasher.update(b"artifact-fingerprint\0");
            hasher.update(&fingerprint.0);
            return Ok(());
        }
        let fingerprint = crate::artifact_value::fingerprint_path(path)?;
        hasher.update(b"artifact-fingerprint\0");
        hasher.update(&fingerprint.0);
        Ok(())
    }

    /// Digest of a plain (non-thin-archive) file, memoized process-wide.
    ///
    /// Returns None for a thin archive so the caller falls through to the
    /// member-tracking path. The signature check runs on every lookup: a
    /// stat costs microseconds against the milliseconds-to-seconds of a
    /// re-hash. Modification/change timestamps, size, inode, device and mode
    /// changes force a re-read, including writes that restore the old mtime.
    /// The signature is captured before reading and confirmed after, so a
    /// write racing the read cannot pin a stale digest to a valid
    /// signature - the entry is simply not stored and the next lookup
    /// re-reads.
    fn memoized_plain_file_digest(path: &Path) -> anyhow::Result<Option<[u8; 32]>> {
        use std::collections::HashMap;
        use std::os::unix::fs::MetadataExt;
        use std::sync::{Mutex, OnceLock};

        type Signature = (i64, i64, u64, u64, i64, i64, u64, u32);
        type Shard = Mutex<HashMap<PathBuf, (Signature, [u8; 32])>>;
        static SHARDS: OnceLock<Vec<Shard>> = OnceLock::new();
        const N_SHARDS: usize = 16;
        let shards = SHARDS.get_or_init(|| {
            (0..N_SHARDS).map(|_| Mutex::new(HashMap::new())).collect()
        });
        let shard = |p: &Path| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(p, &mut h);
            &shards[(std::hash::Hasher::finish(&h) as usize) % N_SHARDS]
        };
        let signature = |md: &std::fs::Metadata| (
            md.mtime(), md.mtime_nsec(), md.len(), md.ino(),
            md.ctime(), md.ctime_nsec(), md.dev(), md.mode(),
        );

        let md = std::fs::metadata(path).map_err(|e| {
            anyhow::anyhow!("Action::compute_id: stat input `{}`: {}", path.display(), e)
        })?;
        let sig = signature(&md);

        if let Ok(guard) = shard(path).lock() {
            if let Some((cached_sig, digest)) = guard.get(path) {
                if *cached_sig == sig {
                    return Ok(Some(*digest));
                }
            }
        }

        let mut file = std::fs::File::open(path).map_err(|e| {
            anyhow::anyhow!("Action::compute_id: open input `{}`: {}", path.display(), e)
        })?;
        let mut first = [0u8; 8];
        let mut first_len = 0usize;
        while first_len < first.len() {
            let n = file.read(&mut first[first_len..]).map_err(|e| {
                anyhow::anyhow!("Action::compute_id: read input `{}`: {}", path.display(), e)
            })?;
            if n == 0 {
                break;
            }
            first_len += n;
        }
        if first_len == first.len() && &first == b"!<thin>\n" {
            return Ok(None);
        }

        let mut content_hasher = blake3::Hasher::new();
        content_hasher.update(&first[..first_len]);
        let mut buffer = [0u8; 65536];
        loop {
            let n = file.read(&mut buffer).map_err(|e| {
                anyhow::anyhow!("Action::compute_id: read input `{}`: {}", path.display(), e)
            })?;
            if n == 0 {
                break;
            }
            content_hasher.update(&buffer[..n]);
        }
        let digest = *content_hasher.finalize().as_bytes();

        // Confirm the same signature after reading. Wait for both modification
        // and change timestamps to leave the existing two-second coarse-clock
        // guard window before storing; fresh writes must not populate a memo
        // entry that a second write in the same timestamp tick could reuse.
        let old_enough = md
            .modified()
            .ok()
            .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
            .is_some_and(|age| age >= std::time::Duration::from_secs(2));
        let ctime_ns = i128::from(md.ctime()) * 1_000_000_000 + i128::from(md.ctime_nsec());
        let now_ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i128).unwrap_or(0);
        let change_old_enough = now_ns - ctime_ns >= 2_000_000_000;
        if old_enough && change_old_enough {
            if let Ok(md_after) = std::fs::metadata(path) {
                if signature(&md_after) == sig {
                    if let Ok(mut guard) = shard(path).lock() {
                        guard.insert(path.to_path_buf(), (sig, digest));
                    }
                }
            }
        }
        Ok(Some(digest))
    }

    /// Enumerate a GNU thin archive's member paths (resolved relative to
    /// the archive's directory), skipping symbol and string tables. Shared
    /// by ActionId hashing and artifact fingerprinting.
    pub(crate) fn thin_archive_member_paths_impl(
        archive_path: &Path,
    ) -> anyhow::Result<Vec<PathBuf>> {
        let archive_bytes = std::fs::read(archive_path)?;
        let mut members = Vec::new();
        let mut pos = 8usize;
        let mut string_table: Vec<u8> = Vec::new();
        while pos + 60 <= archive_bytes.len() {
            let header = &archive_bytes[pos..pos + 60];
            if &header[58..60] != b"`\n" {
                anyhow::bail!(
                    "malformed thin archive `{}`",
                    archive_path.display()
                );
            }
            pos += 60;
            let name = Self::trim_ascii_spaces(&header[0..16]);
            let name = std::str::from_utf8(name)
                .map_err(|e| anyhow::anyhow!("non-utf8 member name: {}", e))?;
            let size_raw = Self::trim_ascii_spaces(&header[48..58]);
            let size = std::str::from_utf8(size_raw)
                .ok()
                .and_then(|s| s.trim().parse::<usize>().ok())
                .ok_or_else(|| anyhow::anyhow!("invalid member size"))?;
            match name {
                "/" | "/SYM64/" => {
                    pos = pos
                        .checked_add(size + (size & 1))
                        .ok_or_else(|| anyhow::anyhow!("offset overflow"))?;
                    if pos > archive_bytes.len() {
                        anyhow::bail!("truncated symbol table");
                    }
                }
                "//" => {
                    let end = pos
                        .checked_add(size)
                        .ok_or_else(|| anyhow::anyhow!("offset overflow"))?;
                    if end > archive_bytes.len() {
                        anyhow::bail!("truncated string table");
                    }
                    string_table = archive_bytes[pos..end].to_vec();
                    pos = end + (size & 1);
                }
                _ => {
                    let member =
                        Self::thin_archive_member_name(name, &string_table, archive_path)?;
                    members.push(if member.is_absolute() {
                        member
                    } else {
                        archive_path
                            .parent()
                            .unwrap_or_else(|| Path::new(""))
                            .join(member)
                    });
                }
            }
        }
        Ok(members)
    }

    fn trim_ascii_spaces(mut bytes: &[u8]) -> &[u8] {
        while bytes.last() == Some(&b' ') {
            bytes = &bytes[..bytes.len() - 1];
        }
        bytes
    }

    fn thin_archive_member_name(
        raw_name: &str,
        string_table: &[u8],
        archive_path: &Path,
    ) -> anyhow::Result<PathBuf> {
        // GNU ar may encode a long-name reference in the fixed-width field as
        // `/0<spaces>/`. Trimming spaces before the optional slash leaves the
        // padding intact, so the numeric offset is missed and `/0<spaces>` is
        // mistaken for an absolute filesystem path. Normalize both suffix
        // characters together before interpreting the reference.
        let raw_name = raw_name.trim_end_matches([' ', '/']);
        if let Some(offset) = raw_name
            .strip_prefix('/')
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse::<usize>().ok())
        {
            if offset >= string_table.len() {
                anyhow::bail!(
                    "Action::compute_id: thin archive `{}` references string-table offset {}",
                    archive_path.display(),
                    offset
                );
            }
            let rest = &string_table[offset..];
            let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
            let mut name = &rest[..end];
            while matches!(name.last(), Some(b'\0' | b'\n' | b'/')) {
                name = &name[..name.len() - 1];
            }
            let s = std::str::from_utf8(name).map_err(|e| {
                anyhow::anyhow!(
                    "Action::compute_id: thin archive `{}` has non-utf8 long member name: {}",
                    archive_path.display(),
                    e
                )
            })?;
            return Ok(PathBuf::from(s));
        }

        let name = raw_name;
        if name.is_empty() {
            anyhow::bail!(
                "Action::compute_id: thin archive `{}` has empty member name",
                archive_path.display()
            );
        }
        Ok(PathBuf::from(name))
    }

    /// Compute the content-based ActionId for this action.
    ///
    /// The ID is a versioned BLAKE3 hash of the serialized action data
    /// (inputs, command, environment). Identical actions share cache hits.
    pub fn compute_id(
        inputs: &[Artifact],
        command: &Command,
        env: &HashMap<String, String>,
    ) -> anyhow::Result<ActionId> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(ACTION_ID_DOMAIN);

        // Hash inputs
        for input in inputs {
            match input {
                Artifact::Source(path) => {
                    // For source files, hash the content. Wrap IO errors
                    // with the offending path so the failing site is
                    // visible — bare `?` here propagates `ErrorKind::NotFound`
                    // as "No such file or directory (os error 2)" with no
                    // context, which buries the cause behind every caller.
                    Self::hash_source_input(&mut hasher, path)?;
                }
                Artifact::Derived {
                    action: _,
                    output_path,
                } => {
                    // REFUSED. Hashing a derived INPUT by its producer's
                    // ActionId is provenance keying: a re-run that
                    // produces byte-identical output gets a new id and
                    // defeats content-based early cutoff. That is the
                    // exact bug the artifact-value program was built to
                    // remove (the 334 s -> 14.6 s result), and a census
                    // on 2026-08-22 confirmed no live input takes this
                    // arm. Fail loudly rather than let it come back
                    // quietly: an input is a path, and its value is the
                    // current filesystem value or verified virtual record.
                    return Err(anyhow::anyhow!(
                        "action input {} is an Artifact::Derived. Action \
                         inputs must be Artifact::Source so their VALUE \
                         (stamp or content) enters the action id; hashing \
                         a producer's identity reintroduces provenance \
                         keying and defeats early cutoff.",
                        output_path.display()
                    ));
                }
            }
        }

        // Hash command
        let cmd_serialized = bincode::serialize(command).unwrap();
        hasher.update(&cmd_serialized);

        // Hash environment (sorted for determinism)
        let mut env_vec: Vec<_> = env.iter().collect();
        env_vec.sort_by_key(|(k, _)| *k);
        for (k, v) in env_vec {
            hasher.update(k.as_bytes());
            hasher.update(v.as_bytes());
        }

        Ok(ActionId::new(hasher.finalize()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_id_invalidates_pre_ctime_cache_namespace() {
        let command = Command {
            executable: "compiler".into(),
            args: vec!["input.cc".into()],
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };
        let old = blake3::hash(&bincode::serialize(&command).unwrap());
        let current = Action::compute_id(&[], &command, &HashMap::new()).unwrap();
        assert_ne!(current.hash(), &old);
    }

    /// A mature memo entry must still observe same-size writes whose mtime
    /// was restored. Also cover fresh rewrites and thin-archive transitions.
    #[test]
    fn memoized_plain_file_digest_tracks_ctime_and_skips_thin() {
        let dir = std::env::temp_dir().join(format!(
            "rush-action-memo-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("input.mojo");
        let backdate = |path: &std::path::Path, secs: u64| {
            let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
            let h = std::fs::File::options().write(true).open(path).unwrap();
            h.set_times(std::fs::FileTimes::new().set_modified(t)).unwrap();
        };
        let mtime_of = |path: &std::path::Path| std::fs::metadata(path).unwrap().modified().unwrap();

        // Let both timestamp clocks age so this exercises a stored entry.
        std::fs::write(&f, b"fn main(): pass").unwrap();
        backdate(&f, 10);
        let aged_mtime = mtime_of(&f);
        std::thread::sleep(std::time::Duration::from_millis(2100));
        let d1 = Action::memoized_plain_file_digest(&f).unwrap().unwrap();
        assert_eq!(d1, *blake3::hash(b"fn main(): pass").as_bytes());

        // An editor/build tool can preserve mtime after replacing file bytes.
        // ctime must invalidate the old digest even when size/inode are equal.
        std::fs::write(&f, b"fn main(): quit").unwrap();
        let h = std::fs::File::options().write(true).open(&f).unwrap();
        h.set_times(std::fs::FileTimes::new().set_modified(aged_mtime)).unwrap();
        drop(h);
        let d2 = Action::memoized_plain_file_digest(&f).unwrap().unwrap();
        assert_ne!(d2, d1, "restored mtime must not conceal changed bytes");
        assert_eq!(d2, *blake3::hash(b"fn main(): quit").as_bytes());

        // A moved mtime is a changed signature: the new bytes are read.
        backdate(&f, 5);
        let d3 = Action::memoized_plain_file_digest(&f).unwrap().unwrap();
        assert_eq!(d3, *blake3::hash(b"fn main(): quit").as_bytes());

        // A changed size is a changed signature.
        std::fs::write(&f, b"fn main(): return").unwrap();
        backdate(&f, 5);
        let d4 = Action::memoized_plain_file_digest(&f).unwrap().unwrap();
        assert_eq!(d4, *blake3::hash(b"fn main(): return").as_bytes());

        // A freshly-written file is never stored: an immediate same-size
        // rewrite inside the mtime tick must still be re-read.
        std::fs::write(&f, b"fn main(): freshA").unwrap();
        let d5 = Action::memoized_plain_file_digest(&f).unwrap().unwrap();
        assert_eq!(d5, *blake3::hash(b"fn main(): freshA").as_bytes());
        std::fs::write(&f, b"fn main(): freshB").unwrap();
        let d6 = Action::memoized_plain_file_digest(&f).unwrap().unwrap();
        assert_eq!(
            d6,
            *blake3::hash(b"fn main(): freshB").as_bytes(),
            "a fresh file must never be served from the memo"
        );

        // Thin archives bypass the memo entirely.
        let thin = dir.join("lib.a");
        std::fs::write(&thin, b"!<thin>\n_rest_of_archive").unwrap();
        assert!(Action::memoized_plain_file_digest(&thin).unwrap().is_none());
        std::fs::write(&f, b"!<thin>\n_rest_of_archive").unwrap();
        backdate(&f, 10);
        assert!(Action::memoized_plain_file_digest(&f).unwrap().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_action_id_display() {
        let hash = blake3::hash(b"test");
        let id = ActionId::new(hash);
        let display = id.to_string();
        assert_eq!(display, hash.to_hex().to_string());
    }

    #[test]
    fn test_artifact_variants() {
        let source = Artifact::Source(PathBuf::from("/src/main.rs"));
        assert!(matches!(source, Artifact::Source(_)));

        let hash = blake3::hash(b"action");
        let derived = Artifact::Derived {
            action: ActionId::new(hash),
            output_path: PathBuf::from("output.o"),
        };
        assert!(matches!(derived, Artifact::Derived { .. }));
    }

    #[test]
    fn test_execution_info_defaults() {
        let info = ExecutionInfo::default();
        assert!(!info.requires_network);
        assert!(info.requires_sandbox);
        assert_eq!(info.timeout, Duration::from_secs(300));
    }

    #[test]
    fn test_command_creation() {
        let cmd = Command {
            executable: PathBuf::from("/usr/bin/gcc"),
            args: vec!["-c".to_string(), "main.c".to_string()],
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };
        assert_eq!(cmd.args.len(), 2);
    }

    #[test]
    fn test_action_compute_id_determinism() {
        // Real files with Source inputs. This test used to build
        // Artifact::Derived inputs purely to avoid touching the
        // filesystem, which quietly kept the provenance-keyed hash arm
        // exercised; that arm is now refused outright, so the test does
        // what a real action does.
        let dir = std::env::temp_dir()
            .join(format!("rush-detid-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let main_c = dir.join("main.c");
        let other_c = dir.join("other.c");
        std::fs::write(&main_c, b"int main(){return 0;}").unwrap();
        std::fs::write(&other_c, b"int other(){return 1;}").unwrap();

        let inputs = vec![Artifact::Source(main_c.clone())];
        let cmd = Command {
            executable: PathBuf::from("/usr/bin/gcc"),
            args: vec!["-c".to_string()],
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };
        let env = HashMap::new();

        let id1 = Action::compute_id(&inputs, &cmd, &env).unwrap();
        let id2 = Action::compute_id(&inputs, &cmd, &env).unwrap();
        assert_eq!(id1, id2, "Same inputs should produce same ID");

        let inputs2 = vec![Artifact::Source(other_c.clone())];
        let id3 = Action::compute_id(&inputs2, &cmd, &env).unwrap();
        assert_ne!(id1, id3, "Different inputs should produce different IDs");

        let cmd2 = Command {
            executable: PathBuf::from("/usr/bin/clang"),
            args: vec!["-c".to_string()],
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };
        let id4 = Action::compute_id(&inputs, &cmd2, &env).unwrap();
        assert_ne!(id1, id4, "Different command should produce different IDs");

        let mut env2 = HashMap::new();
        env2.insert("CC".to_string(), "gcc".to_string());
        let id5 = Action::compute_id(&inputs, &cmd, &env2).unwrap();
        assert_ne!(id1, id5, "Different env should produce different IDs");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A derived artifact as an action INPUT is refused, so the
    /// provenance-keyed hash arm cannot be revived by accident.
    #[test]
    fn derived_inputs_are_refused() {
        let inputs = vec![Artifact::Derived {
            action: ActionId::new(blake3::hash(b"parent")),
            output_path: PathBuf::from("generated.o"),
        }];
        let cmd = Command {
            executable: PathBuf::from("/usr/bin/ld"),
            args: Vec::new(),
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };
        let err = Action::compute_id(&inputs, &cmd, &HashMap::new())
            .expect_err("a Derived input must not be hashed by producer identity");
        assert!(
            err.to_string().contains("Artifact::Derived"),
            "error should name the offending variant, got: {err}"
        );
    }

    #[test]
    fn adjacent_stamp_cannot_mask_a_source_content_change() {
        let unique = format!(
            "rush-action-stamp-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).unwrap();

        let source = dir.join("input.o");
        std::fs::write(&source, b"object-v1").unwrap();
        let mut stamp_path = source.as_os_str().to_os_string();
        stamp_path.push(".rushid");
        let fp_v1 =
            crate::artifact_value::fingerprint_path(&source).unwrap();
        std::fs::write(&stamp_path, fp_v1.to_stamp()).unwrap();

        let inputs = vec![Artifact::Source(source.clone())];
        let cmd = Command {
            executable: PathBuf::from("/usr/bin/ld"),
            args: vec!["input.o".to_string()],
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };
        let env = HashMap::new();
        let id1 = Action::compute_id(&inputs, &cmd, &env).unwrap();

        // Adjacent state is never trusted. Changing the file while leaving a
        // once-valid stamp in place must still change the consumer key.
        std::fs::write(&source, b"object-v2-longer").unwrap();
        let id2 = Action::compute_id(&inputs, &cmd, &env).unwrap();
        assert_ne!(id1, id2, "adjacent state must not mask content changes");

        // Early cutoff: a producer re-run that leaves the output
        // byte-identical keeps the consumer key unchanged without relying on
        // adjacent metadata.
        std::fs::write(&source, b"object-v1").unwrap();
        let fp_again =
            crate::artifact_value::fingerprint_path(&source).unwrap();
        assert_eq!(fp_v1, fp_again);
        std::fs::write(&stamp_path, fp_again.to_stamp()).unwrap();
        let id_same = Action::compute_id(&inputs, &cmd, &env).unwrap();
        assert_eq!(id1, id_same, "byte-identical output must not re-key");

        // Changing only the adjacent stamp cannot change the input value.
        let fp_v2 = crate::artifact_value::ArtifactFingerprint([9; 32]);
        std::fs::write(&stamp_path, fp_v2.to_stamp()).unwrap();
        let id3 = Action::compute_id(&inputs, &cmd, &env).unwrap();
        assert_eq!(id1, id3, "adjacent state is not an action-key input");

        // Legacy provenance-format stamps are ignored too.
        std::fs::write(&stamp_path, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        std::fs::write(&source, b"object-v1").unwrap();
        let id_legacy = Action::compute_id(&inputs, &cmd, &env).unwrap();
        std::fs::write(&source, b"object-v2-longer").unwrap();
        let id_legacy2 = Action::compute_id(&inputs, &cmd, &env).unwrap();
        assert_ne!(
            id_legacy, id_legacy2,
            "legacy stamps must not mask content changes"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thin_archive_member_bytes_participate_in_action_id() {
        let unique = format!(
            "rush-action-thin-archive-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).unwrap();

        fn ar_header(name: &str, size: usize) -> Vec<u8> {
            let mut header = vec![b' '; 60];
            let fields = [
                (0usize, 16usize, name.to_string()),
                (16, 12, "0".to_string()),
                (28, 6, "0".to_string()),
                (34, 6, "0".to_string()),
                (40, 8, "644".to_string()),
                (48, 10, size.to_string()),
            ];
            for (start, width, value) in fields {
                let bytes = value.as_bytes();
                header[start..start + bytes.len().min(width)]
                    .copy_from_slice(&bytes[..bytes.len().min(width)]);
            }
            header[58] = b'`';
            header[59] = b'\n';
            header
        }

        let obj = dir.join("x.o");
        let archive = dir.join("libx.a");
        std::fs::write(&obj, b"object-v1").unwrap();
        let mut archive_bytes = b"!<thin>\n".to_vec();
        let string_table = b"x.o/\n";
        archive_bytes.extend(ar_header("//", string_table.len()));
        archive_bytes.extend(string_table);
        if string_table.len() & 1 == 1 {
            archive_bytes.push(b'\n');
        }
        archive_bytes.extend(ar_header("/0             /", 9));
        std::fs::write(&archive, &archive_bytes).unwrap();

        let cmd = Command {
            executable: PathBuf::from("/usr/bin/ld"),
            args: vec!["libx.a".to_string()],
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };
        let env = HashMap::new();
        let inputs = vec![Artifact::Source(archive.clone())];
        let id1 = Action::compute_id(&inputs, &cmd, &env).unwrap();

        std::fs::write(&obj, b"object-v2").unwrap();
        assert_eq!(std::fs::read(&archive).unwrap(), archive_bytes);
        let id2 = Action::compute_id(&inputs, &cmd, &env).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
        assert_ne!(
            id1, id2,
            "thin archive member bytes must affect dependent action IDs"
        );
    }
}
