//! The recorded-dependency types and the digest helpers used to compute them.

use std::path::PathBuf;

/// The on-disk kind of a path, as observed by an existence / type probe.
///
/// A regular file, a directory, or a symlink. We only distinguish the three
/// categories analysis actually branches on; anything else (sockets, fifos) is
/// reported as `None` by [`crate::TrackedIo::metadata_kind`] and recorded as a
/// kindless existence.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
}

impl FileKind {
    /// A stable single-byte tag for fingerprinting / encoding.
    pub(crate) fn tag(self) -> u8 {
        match self {
            FileKind::File => 1,
            FileKind::Dir => 2,
            FileKind::Symlink => 3,
        }
    }
}

/// One dependency observed during analysis.
///
/// Each variant captures the *aspect* of the world that mattered for the
/// observation — not always the full state. The validation semantics
/// (in [`crate::DependencyManifest::validate`]) re-check exactly that aspect:
///
/// * [`Dependency::FileContent`] — the bytes mattered (read_file / read_to_string).
///   Stores the blake3 of the contents.
/// * [`Dependency::FileExistence`] — only presence + type mattered (exists /
///   metadata). A negative probe is `exists == false`, `kind == None`.
/// * [`Dependency::Symlink`] — the link *target* mattered (read_link).
/// * [`Dependency::DirListing`] — the *set of entry names* mattered (read_dir,
///   i.e. `glob()` and package-boundary scans). Stores the blake3 of the sorted,
///   length-prefixed entry-name list so adding/removing any entry invalidates.
/// * [`Dependency::Env`] — an environment variable's value mattered (getenv).
///   `value == None` means "observed absent".
/// * [`Dependency::Opt`] — a build option / `--define` mattered (get_option).
/// * [`Dependency::ConfigFingerprint`] — analysis consumed the *whole*
///   per-command configuration (env + flags + defines + output base) via
///   [`crate::TrackedIo::config_fingerprint`]. Any config change invalidates
///   conservatively.
/// * [`Dependency::FileMetadata`] — the path's stat metadata mattered (len, unix
///   mode, mtime/ctime, dev/ino). Catches a `chmod +x` on a tool that the content
///   digest alone would miss.
/// * [`Dependency::ProcessProbe`] — analysis ran a subprocess (pkg-config / nm /
///   which / bazel) and branched on its result. Stores the program/args/env-keys/
///   cwd it ran under plus the observed status + stdout/stderr digests;
///   validation re-runs it and compares.
/// * [`Dependency::LinuxProcFilesystems`] — analysis consumed the complete
///   Linux filesystem-type registry. Validation re-reads that exact kernel
///   interface and compares its byte count and raw BLAKE3 digest.
/// * [`Dependency::LinuxProcVmOvercommitMemory`] — analysis consumed Linux's
///   numeric virtual-memory overcommit policy. Validation re-reads and parses
///   that exact kernel interface.
/// * [`Dependency::LinuxCgroupV2CpuMax`] — analysis consumed the complete CPU
///   quota hierarchy for its cgroup-v2 membership. Validation re-resolves the
///   current process's membership and compares the distinct controller values
///   without retaining ephemeral cgroup pathnames.
///
/// `Ord` is derived so manifests can be sorted into a canonical order before
/// fingerprinting; the derived lexicographic order over the fields is stable.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Dependency {
    /// read_file / read_to_string: CONTENT matters. blake3 of the bytes.
    FileContent { path: PathBuf, digest: [u8; 32] },
    /// exists() / metadata(): presence + type matter (NOT content).
    FileExistence {
        path: PathBuf,
        exists: bool,
        kind: Option<FileKind>,
    },
    /// read_link(): the symlink target matters.
    Symlink {
        path: PathBuf,
        target: Option<PathBuf>,
    },
    /// read_dir(): the SET of entry names matters. blake3 of sorted entry names.
    DirListing {
        path: PathBuf,
        entries_digest: [u8; 32],
    },
    /// getenv(): the value matters. `value == None` means observed absent.
    Env {
        name: Vec<u8>,
        value: Option<Vec<u8>>,
    },
    /// get_option(): a build option / `--define` etc.
    Opt { key: String, value: Option<String> },
    /// config_fingerprint(): the whole per-command configuration was consumed.
    /// Any change to env / flags / defines / output base flips this.
    ConfigFingerprint { digest: [u8; 32] },
    /// metadata_full(): the path's stat metadata mattered. On non-unix the
    /// unix-only fields (`mode`, `dev`, `ino`, `ctime_ns`) are recorded as 0.
    /// `present == false` records an observed-absent path (negative metadata
    /// probe); all numeric fields are 0 in that case.
    FileMetadata {
        path: PathBuf,
        present: bool,
        len: u64,
        mode: u32,
        mtime_ns: i128,
        ctime_ns: i128,
        dev: u64,
        ino: u64,
    },
    /// probe_process(): a subprocess was run and analysis branched on its result.
    /// Validation re-runs the same program/args/cwd (with the relevant env keys
    /// resolved from the *validating* command's semantics) and compares the exit
    /// status + stdout digest. `compare_stderr` selects whether stderr is also
    /// semantic; stdout-only probes use a zero stderr digest so lifecycle/UI
    /// noise cannot perturb manifest identity.
    ProcessProbe {
        program: PathBuf,
        args: Vec<Vec<u8>>,
        env_keys: Vec<Vec<u8>>,
        cwd: PathBuf,
        status: i32,
        stdout_digest: [u8; 32],
        stderr_digest: [u8; 32],
        #[serde(default = "default_compare_process_stderr")]
        compare_stderr: bool,
    },
    /// Complete value returned by Linux `/proc/filesystems`.
    ///
    /// The path is deliberately not configurable: this is a narrow typed
    /// kernel-state witness, not permission to treat arbitrary procfs files as
    /// ordinary stable inputs. The digest is raw BLAKE3 so a process-capture
    /// read digest can be used directly after its complete-value proof closes.
    LinuxProcFilesystems { bytes: u64, digest: [u8; 32] },
    /// Numeric value returned by Linux `/proc/sys/vm/overcommit_memory`.
    LinuxProcVmOvercommitMemory { value: u8 },
    /// Distinct complete `cpu.max` values from the process's cgroup-v2 ancestry.
    ///
    /// The cgroup pathname is deliberately not persisted because transient
    /// service and container names are placement details. Validation resolves
    /// `/proc/self/cgroup` again and compares the canonical value set. Duplicate
    /// unlimited levels do not perturb identity, while any distinct ancestor
    /// quota remains part of the dependency.
    LinuxCgroupV2CpuMax { values: Vec<(u64, [u8; 32])> },
}

fn default_compare_process_stderr() -> bool {
    true
}

/// The result of re-checking a [`crate::DependencyManifest`] against the current
/// world.
///
/// [`Staleness::Stale`] names the *first* dependency that no longer matches so the
/// daemon can both reject reuse and report a human-readable reason for the miss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staleness {
    Fresh,
    Stale { dep: Dependency, reason: String },
}

// ---------------------------------------------------------------------------
// Digest helpers — shared by the recording side (RealTrackedIo) and the
// validation side (DependencyManifest::validate) so a re-hash on validation
// produces a byte-identical digest to the one recorded.
// ---------------------------------------------------------------------------

/// Domain tag for file-content digests. Distinct per digest kind so a content
/// digest can never be confused with a dir-listing digest of the same bytes.
const CONTENT_DOMAIN: &[u8] = b"rush.incremental.v1/file-content/1\n";
/// Domain tag for directory-listing digests.
const DIRLISTING_DOMAIN: &[u8] = b"rush.incremental.v1/dir-listing/1\n";
/// Domain tag for process-probe stream (stdout/stderr) digests.
const PROCESS_STREAM_DOMAIN: &[u8] = b"rush.incremental.v1/process-stream/1\n";

/// blake3 of file contents, domain-separated.
pub fn content_digest(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(CONTENT_DOMAIN);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

/// blake3 of a captured process stream (stdout or stderr), domain-separated.
pub(crate) fn process_stream_digest(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(PROCESS_STREAM_DOMAIN);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

/// blake3 of a directory's entry-name set.
///
/// The names are sorted and each is length-prefixed (LE u64) before hashing, so
/// a name-set change can never collide with a different name-set (e.g. `["ab"]`
/// vs `["a", "b"]`). The caller must pass the *raw* entry names (no path joins);
/// names are hashed as their lossless byte representation.
pub fn dir_listing_digest(mut names: Vec<Vec<u8>>) -> [u8; 32] {
    names.sort();
    names.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(DIRLISTING_DOMAIN);
    hasher.update(&(names.len() as u64).to_le_bytes());
    for name in &names {
        hasher.update(&(name.len() as u64).to_le_bytes());
        hasher.update(name);
    }
    *hasher.finalize().as_bytes()
}
