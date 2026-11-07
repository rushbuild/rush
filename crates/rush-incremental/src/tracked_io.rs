//! The [`TrackedIo`] trait and its production [`RealTrackedIo`] implementation.
//!
//! Analysis reads the world through a `TrackedIo`; each accessor records what it
//! observed (as a [`Dependency`]) before returning it, so the resulting
//! [`DependencyManifest`] is a complete record of the analysis's inputs.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use bytes::Bytes;

use rush_command_core::CommandSemantics;

use crate::dependency::{content_digest, dir_listing_digest, Dependency, FileKind};
use crate::manifest::{DependencyManifest, Validated};
use crate::probe::{run_probe, stat_snapshot};

/// The interface analysis MUST read the world through.
///
/// Every method that observes the filesystem, the environment, or a build option
/// *records* a [`Dependency`] for that observation before returning it. The
/// accumulated record is exposed via [`TrackedIo::manifest`]. Implementors use
/// interior mutability so the accessors can take `&self` (analysis holds a shared
/// reference and reads concurrently).
pub trait TrackedIo {
    /// Read a file's full contents. Records [`Dependency::FileContent`].
    fn read_file(&self, path: &Path) -> std::io::Result<Bytes>;

    /// Read a file's contents as UTF-8. Records [`Dependency::FileContent`]
    /// (keyed on the raw bytes, identically to [`Self::read_file`]).
    fn read_to_string(&self, path: &Path) -> std::io::Result<String>;

    /// Probe path existence. Records [`Dependency::FileExistence`] (with the
    /// observed kind when the path exists).
    fn exists(&self, path: &Path) -> bool;

    /// Probe path existence + type. Records [`Dependency::FileExistence`] carrying
    /// the kind. Returns `None` when the path is absent or is a kind we don't
    /// distinguish.
    fn metadata_kind(&self, path: &Path) -> Option<FileKind>;

    /// Probe a path's full stat metadata (len, unix mode, mtime/ctime, dev/ino).
    /// Records [`Dependency::FileMetadata`]. Returns `false` when the path is
    /// absent (a negative metadata probe is still recorded). Use this when a
    /// decision depends on the executable bit or other metadata that
    /// [`Self::metadata_kind`] does not capture (e.g. resolving `protoc`/`gcc`).
    fn metadata_full(&self, path: &Path) -> bool;

    /// Read a symlink's target. Records [`Dependency::Symlink`]. Returns `None`
    /// when the path is not a symlink / cannot be read.
    fn read_link(&self, path: &Path) -> Option<PathBuf>;

    /// List a directory's entry names (not joined with the dir path). Records
    /// [`Dependency::DirListing`] over the sorted name set.
    fn read_dir_names(&self, path: &Path) -> std::io::Result<Vec<OsString>>;

    /// Read an environment variable through the per-command semantics (NEVER
    /// `std::env`). Records [`Dependency::Env`].
    fn getenv(&self, name: &[u8]) -> Option<Bytes>;

    /// Read a build option through the per-command semantics. Records
    /// [`Dependency::Opt`].
    fn get_option(&self, key: &str) -> Option<String>;

    /// Consume the *whole* per-command configuration fingerprint. Records
    /// [`Dependency::ConfigFingerprint`] so that any change to the configuration
    /// (env / flags / defines / output base) invalidates conservatively. This is
    /// the only sanctioned way for analysis to depend on the configuration as a
    /// whole — there is no accessor returning the raw [`CommandSemantics`].
    fn config_fingerprint(&self) -> [u8; 32];

    /// Run a subprocess probe (pkg-config / nm / which / bazel / …), record its
    /// result as a [`Dependency::ProcessProbe`], and return the captured outcome.
    ///
    /// The child runs in `cwd` with a cleared environment into which only the
    /// named `env_keys` (resolved from the command semantics) are injected, so the
    /// probe depends on exactly the declared env. `args`/env values are raw bytes.
    /// Returns the normalized exit status, captured stdout/stderr, and their
    /// blake3 digests, or an I/O error if the program could not be spawned.
    fn probe_process(
        &self,
        program: &Path,
        args: &[Vec<u8>],
        env_keys: &[Vec<u8>],
        cwd: &Path,
    ) -> std::io::Result<ProcessProbeResult>;

    /// Run a subprocess probe whose semantic result is exit status + stdout.
    /// Stderr is still returned for diagnostics, but is deliberately excluded
    /// from freshness because tools such as Bazel print lifecycle/UI noise there.
    fn probe_process_stdout(
        &self,
        program: &Path,
        args: &[Vec<u8>],
        env_keys: &[Vec<u8>],
        cwd: &Path,
    ) -> std::io::Result<ProcessProbeResult>;

    /// Snapshot the dependencies recorded so far into a manifest.
    fn manifest(&self) -> DependencyManifest;

    /// Return the current length of the append-only recorded-dependency list.
    ///
    /// This is the "mark" for [`Self::captured_since`]: capture it *before* a
    /// sub-computation (e.g. loading one `.bzl`), then read back exactly the
    /// dependencies that computation recorded with `captured_since(mark)`. Used by
    /// the daemon `.bzl`/BUILD cache to store each cache entry's full transitive
    /// dependency set so a later cross-command cache HIT can replay it.
    fn mark(&self) -> usize;

    /// Clone the dependencies appended since `mark` (i.e. `recorded[mark..]`).
    ///
    /// `mark` is a value previously returned by [`Self::mark`]. If `mark` is past
    /// the current end (should not happen for a monotonically-growing append-only
    /// list, but defended against truncation/poison recovery) an empty vec is
    /// returned. The returned deps are NOT canonicalized — they are the raw
    /// recorded observations, suitable to store and later [`Self::record_deps`].
    fn captured_since(&self, mark: usize) -> Vec<Dependency>;

    /// Append a set of pre-existing dependencies to the recorded list (the REPLAY
    /// primitive). Used on a cache HIT to re-inject the dependencies that were
    /// recorded when the cached value was first produced, so the current command's
    /// manifest is complete as if the load had happened. Duplicates are harmless:
    /// [`DependencyManifest::from_recorded`] sorts + dedups at snapshot time.
    fn record_deps(&self, deps: &[Dependency]);

    /// Validate a cached value's dependency manifest against this command.
    ///
    /// Cache owners call this before replaying dependencies and serving a value
    /// produced by another command. The operation deliberately does not expose
    /// the raw [`CommandSemantics`] through the analysis interface.
    fn validate_manifest(&self, manifest: &DependencyManifest) -> Validated;

    /// Return a successful canonicalization cached for this analysis command.
    ///
    /// Implementations without a command-local cache may return `None`.
    #[doc(hidden)]
    fn canonicalize_cache_get(&self, _path: &Path) -> Option<PathBuf> {
        None
    }

    /// Cache a successful canonicalization for this analysis command.
    ///
    /// Implementations without a command-local cache may ignore this call.
    #[doc(hidden)]
    fn canonicalize_cache_put(&self, _path: PathBuf, _resolved: PathBuf) {}
}

/// The outcome of [`TrackedIo::probe_process`], also the data compared on
/// validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessProbeResult {
    /// Normalized exit status: the exit code, or `-(signal)` on unix when killed
    /// by a signal, or `-1` when neither is available.
    pub status: i32,
    /// Captured stdout bytes. The dependency manifest retains only the digest.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes. The dependency manifest retains only the digest.
    pub stderr: Vec<u8>,
    pub stdout_digest: [u8; 32],
    pub stderr_digest: [u8; 32],
}

/// Production [`TrackedIo`]: real filesystem for file ops, [`CommandSemantics`]
/// for env / options.
///
/// Recording is via an internal `Mutex<Vec<Dependency>>`, so the accessors take
/// `&self`. The manifest is deduped + sorted at snapshot time (see
/// [`DependencyManifest::from_recorded`]); recording itself just appends.
pub struct RealTrackedIo {
    semantics: CommandSemantics,
    /// Absolute workspace root. Every recorded path is normalized against this so
    /// validation (which may run from a different process cwd) re-reads the same
    /// file. Itself normalized to absolute + lexically-canonical at construction.
    root: PathBuf,
    recorded: Mutex<Vec<Dependency>>,
    /// Successful canonicalizations observed during this analysis command.
    canonicalized: Mutex<HashMap<PathBuf, PathBuf>>,
    /// Path existence and kind observations made during this command.
    existence_observations: Mutex<HashMap<PathBuf, (bool, Option<FileKind>)>>,
}

impl RealTrackedIo {
    /// Create a tracker backed by the given per-command semantics, rooted at
    /// `root` (the workspace/exec root). Relative paths passed to the accessors
    /// are joined to `root` and lexically canonicalized before recording, so the
    /// resulting manifest is independent of the daemon process's cwd.
    ///
    /// `root` is itself made absolute (joined to the current dir if relative) and
    /// lexically canonicalized.
    pub fn new(semantics: CommandSemantics, root: impl AsRef<Path>) -> Self {
        let root = normalize_against(&std::env::current_dir().unwrap_or_default(), root.as_ref());
        Self {
            semantics,
            root,
            recorded: Mutex::new(Vec::new()),
            canonicalized: Mutex::new(HashMap::new()),
            existence_observations: Mutex::new(HashMap::new()),
        }
    }

    /// Validate a cached analysis manifest against this command's semantics.
    ///
    /// Cache owners use this before admitting a value produced by an earlier
    /// command. Keeping the operation on `RealTrackedIo` preserves the rule that
    /// analysis cannot obtain the raw [`CommandSemantics`], while still checking
    /// environment and option dependencies against the daemon client's command.
    pub fn validate_manifest(&self, manifest: &DependencyManifest) -> Validated {
        manifest.validate(&self.semantics)
    }

    /// Normalize a caller-supplied path to an absolute, lexically-canonical path
    /// rooted at this tracker's workspace root. Relative paths are joined to the
    /// root; absolute paths are canonicalized as-is. Symlinks are NOT resolved
    /// (lexical only) — symlink identity matters for the dependency.
    fn norm(&self, path: &Path) -> PathBuf {
        normalize_against(&self.root, path)
    }

    fn record(&self, dep: Dependency) {
        // Lock poisoning here would mean a panic occurred while another thread
        // held the lock mid-record; recover the inner Vec and continue rather
        // than cascade the panic — a partially-recorded manifest is still safe
        // (it can only over-record, never silently drop a real dependency that
        // was successfully appended).
        let mut guard = match self.recorded.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push(dep);
    }

    fn observe_kind(&self, np: &Path) -> (bool, Option<FileKind>) {
        {
            let observations = match self.existence_observations.lock() {
                Ok(observations) => observations,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(observation) = observations.get(np) {
                return *observation;
            }
        }

        let observation = match fs::symlink_metadata(np) {
            Ok(meta) => (true, classify(&meta)),
            Err(_) => (false, None),
        };
        self.record(Dependency::FileExistence {
            path: np.to_path_buf(),
            exists: observation.0,
            kind: observation.1,
        });

        let mut observations = match self.existence_observations.lock() {
            Ok(observations) => observations,
            Err(poisoned) => poisoned.into_inner(),
        };
        observations.insert(np.to_path_buf(), observation);
        observation
    }

    /// Record the kind (existence) of an already-normalized path. Used alongside a
    /// content read so a file -> symlink swap to identical bytes is still caught.
    fn record_kind(&self, np: &Path) {
        self.observe_kind(np);
    }
}

/// Normalize `path` to an absolute, lexically-canonical path.
///
/// If `path` is relative it is joined onto `base`. The result is then collapsed
/// lexically: `.` components are dropped and `..` pops the previous *normal*
/// component (but never past the root). Crucially this does NOT touch the
/// filesystem — symlinks are left intact, since a symlink's identity is itself a
/// tracked dependency and resolving it away would hide a swap.
pub(crate) fn normalize_against(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop only a real (Normal) component; never ascend past the root
                // and never collapse a leading `..` that has no parent to pop.
                let popped = matches!(
                    out.components().next_back(),
                    Some(Component::Normal(_))
                );
                if popped {
                    out.pop();
                } else {
                    out.push(comp.as_os_str());
                }
            }
            Component::Normal(c) => out.push(c),
        }
    }
    out
}

/// Classify a path's kind from already-fetched metadata.
///
/// `symlink_metadata` is used by callers that need to detect the symlink itself;
/// `metadata` (follows links) is used by existence/type probes. We map only the
/// three kinds we model; everything else is `None`.
pub(crate) fn classify(meta: &fs::Metadata) -> Option<FileKind> {
    let ft = meta.file_type();
    if ft.is_symlink() {
        Some(FileKind::Symlink)
    } else if ft.is_dir() {
        Some(FileKind::Dir)
    } else if ft.is_file() {
        Some(FileKind::File)
    } else {
        None
    }
}

impl TrackedIo for RealTrackedIo {
    fn canonicalize_cache_get(&self, path: &Path) -> Option<PathBuf> {
        let cache = match self.canonicalized.lock() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        };
        cache.get(path).cloned()
    }

    fn canonicalize_cache_put(&self, path: PathBuf, resolved: PathBuf) {
        let mut cache = match self.canonicalized.lock() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        };
        cache.insert(path, resolved);
    }

    fn read_file(&self, path: &Path) -> std::io::Result<Bytes> {
        let np = self.norm(path);
        match fs::read(&np) {
            Ok(bytes) => {
                self.record(Dependency::FileContent {
                    path: np.clone(),
                    digest: content_digest(&bytes),
                });
                // ALSO record the path's kind so a regular-file -> symlink swap to
                // identical content is caught (the content digest alone misses it).
                self.record_kind(&np);
                Ok(Bytes::from(bytes))
            }
            Err(e) => {
                // Negative probe: record that the path was observed unreadable as
                // an absent existence dep, so a later-created file flips to Stale.
                self.record(Dependency::FileExistence {
                    path: np,
                    exists: false,
                    kind: None,
                });
                Err(e)
            }
        }
    }

    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        let np = self.norm(path);
        match fs::read_to_string(&np) {
            Ok(s) => {
                // Digest the raw bytes so it matches read_file for the same file and
                // so validation (which re-hashes raw bytes) agrees.
                self.record(Dependency::FileContent {
                    path: np.clone(),
                    digest: content_digest(s.as_bytes()),
                });
                self.record_kind(&np);
                Ok(s)
            }
            Err(e) => {
                self.record(Dependency::FileExistence {
                    path: np,
                    exists: false,
                    kind: None,
                });
                Err(e)
            }
        }
    }

    fn exists(&self, path: &Path) -> bool {
        // Use symlink_metadata so a dangling symlink counts as existing (the link
        // node is present even if its target is not), matching how Bazel treats a
        // path node. The recorded kind follows the link node's own type.
        let np = self.norm(path);
        self.observe_kind(&np).0
    }

    fn metadata_kind(&self, path: &Path) -> Option<FileKind> {
        let np = self.norm(path);
        self.observe_kind(&np).1
    }

    fn metadata_full(&self, path: &Path) -> bool {
        let np = self.norm(path);
        let s = stat_snapshot(&np);
        self.record(Dependency::FileMetadata {
            path: np,
            present: s.present,
            len: s.len,
            mode: s.mode,
            mtime_ns: s.mtime_ns,
            ctime_ns: s.ctime_ns,
            dev: s.dev,
            ino: s.ino,
        });
        s.present
    }

    fn read_link(&self, path: &Path) -> Option<PathBuf> {
        let np = self.norm(path);
        let target = fs::read_link(&np).ok();
        self.record(Dependency::Symlink {
            path: np,
            target: target.clone(),
        });
        target
    }

    fn read_dir_names(&self, path: &Path) -> std::io::Result<Vec<OsString>> {
        let np = self.norm(path);
        let rd = match fs::read_dir(&np) {
            Ok(rd) => rd,
            Err(e) => {
                // Negative probe: a missing/unlistable dir is recorded absent so a
                // dir that later appears (or becomes listable) flips to Stale.
                self.record(Dependency::FileExistence {
                    path: np,
                    exists: false,
                    kind: None,
                });
                return Err(e);
            }
        };
        let mut names: Vec<OsString> = Vec::new();
        for entry in rd {
            let entry = entry?;
            names.push(entry.file_name());
        }
        let digest = dir_listing_digest(names.iter().map(os_string_bytes).collect());
        self.record(Dependency::DirListing {
            path: np,
            entries_digest: digest,
        });
        Ok(names)
    }

    fn getenv(&self, name: &[u8]) -> Option<Bytes> {
        let value = self.semantics.getenv(name).map(|v| Bytes::from(v.to_vec()));
        self.record(Dependency::Env {
            name: name.to_vec(),
            value: value.as_ref().map(|b| b.to_vec()),
        });
        value
    }

    fn get_option(&self, key: &str) -> Option<String> {
        let value = lookup_option(&self.semantics, key);
        self.record(Dependency::Opt {
            key: key.to_string(),
            value: value.clone(),
        });
        value
    }

    fn config_fingerprint(&self) -> [u8; 32] {
        let digest = *self.semantics.config_fingerprint();
        self.record(Dependency::ConfigFingerprint { digest });
        digest
    }

    fn probe_process(
        &self,
        program: &Path,
        args: &[Vec<u8>],
        env_keys: &[Vec<u8>],
        cwd: &Path,
    ) -> std::io::Result<ProcessProbeResult> {
        let cwd = self.norm(cwd);
        let outcome = run_probe(program, args, env_keys, &cwd, &self.semantics)?;
        self.record(Dependency::ProcessProbe {
            program: program.to_path_buf(),
            args: args.to_vec(),
            env_keys: env_keys.to_vec(),
            cwd,
            status: outcome.status,
            stdout_digest: outcome.stdout_digest,
            stderr_digest: outcome.stderr_digest,
            compare_stderr: true,
        });
        Ok(ProcessProbeResult {
            status: outcome.status,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            stdout_digest: outcome.stdout_digest,
            stderr_digest: outcome.stderr_digest,
        })
    }


    fn probe_process_stdout(
        &self,
        program: &Path,
        args: &[Vec<u8>],
        env_keys: &[Vec<u8>],
        cwd: &Path,
    ) -> std::io::Result<ProcessProbeResult> {
        let cwd = self.norm(cwd);
        let outcome = run_probe(program, args, env_keys, &cwd, &self.semantics)?;
        self.record(Dependency::ProcessProbe {
            program: program.to_path_buf(),
            args: args.to_vec(),
            env_keys: env_keys.to_vec(),
            cwd,
            status: outcome.status,
            stdout_digest: outcome.stdout_digest,
            stderr_digest: [0; 32],
            compare_stderr: false,
        });
        Ok(ProcessProbeResult {
            status: outcome.status,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            stdout_digest: outcome.stdout_digest,
            stderr_digest: outcome.stderr_digest,
        })
    }

    fn manifest(&self) -> DependencyManifest {
        let guard = match self.recorded.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        DependencyManifest::from_recorded(guard.clone())
    }

    fn mark(&self) -> usize {
        let guard = match self.recorded.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.len()
    }

    fn captured_since(&self, mark: usize) -> Vec<Dependency> {
        let guard = match self.recorded.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if mark >= guard.len() {
            Vec::new()
        } else {
            guard[mark..].to_vec()
        }
    }

    fn record_deps(&self, deps: &[Dependency]) {
        if deps.is_empty() {
            return;
        }
        let mut guard = match self.recorded.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.extend_from_slice(deps);
    }

    fn validate_manifest(&self, manifest: &DependencyManifest) -> Validated {
        RealTrackedIo::validate_manifest(self, manifest)
    }
}

/// Resolve a build option from the command semantics.
///
/// rush's option surface is currently the parsed `--define`s. We resolve a
/// `get_option(key)` against those defines (key match). This keeps the option
/// source authoritative (the per-command semantics) rather than `std::env`, and
/// the same resolution is reused by validation so a define change invalidates.
pub(crate) fn lookup_option(sem: &CommandSemantics, key: &str) -> Option<String> {
    sem.defines()
        .iter()
        .find(|d| d.key == key)
        .map(|d| d.value.clone())
}

/// Lossless bytes of an `OsString` for digesting.
///
/// On Unix this is the raw bytes (paths are not guaranteed UTF-8). On other
/// platforms we fall back to the lossy UTF-8 bytes; rush's target platform is
/// Unix, and the digest only needs to be stable + collision-resistant within a
/// single platform's runtime, which both forms satisfy.
fn os_string_bytes(s: &OsString) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        s.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        s.to_string_lossy().into_owned().into_bytes()
    }
}
