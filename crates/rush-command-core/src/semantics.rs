//! Per-command semantics — the replacement for process-global state.
//!
//! # Why this exists (Codex P0 risk #1)
//!
//! Today the rush CLI writes parsed flags (`--define`, config options) into
//! `std::env` and the analysis phase reads `--define`/environment from those
//! **process globals**. In a one-shot CLI process that is merely ugly. In a
//! **persistent daemon** it is a correctness hole: a build invoked by client A
//! would mutate process-global state that a concurrent or subsequent build from
//! client B then reads — build-A contamination of build-B.
//!
//! [`CommandSemantics`] captures the *entire* per-invocation context that today
//! leaks through process globals, sourced **exclusively from the client's
//! [`RunRequest`]** and never from the daemon's own `std::env`. Analysis will
//! eventually read its environment through [`CommandSemantics::getenv`] instead of
//! `std::env::var`, and read its defines through [`CommandSemantics::defines`]
//! instead of the process-global `config_defines`.
//!
//! **Migration note (deferred — NOT done here):** rewiring the existing
//! `std::env`/`config_defines` read sites in the analysis code to call into this
//! struct is a *later* brick. This crate only *defines* the per-command context;
//! it does not yet replace any live read site, and is not wired into the CLI.

use std::collections::BTreeMap;

use bytes::Bytes;

use rush_protocol::command::v1::{RunRequest, WorkspaceConfigFile};
use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};

use crate::error::CommandError;

/// Domain separation tag mixed into the config fingerprint so a fingerprint from
/// this crate can never collide with an unrelated blake3 hash of the same bytes.
const FINGERPRINT_DOMAIN: &[u8] = b"rush.command.v1/config-fingerprint/1\n";

/// A parsed `--define KEY=VALUE` build define.
///
/// Defines are part of the analysis-affecting configuration, so they participate
/// in the [`config_fingerprint`](CommandSemantics::config_fingerprint).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BuildDefine {
    pub key: String,
    pub value: String,
}

/// The full, transport-independent context for a single command invocation.
///
/// Constructed once per `Run` from the client [`RunRequest`]; immutable
/// thereafter. Holds the client environment snapshot, the normalized argv, the
/// parsed build options/defines, the working directory and output base, and a
/// derived deterministic [`config_fingerprint`](Self::config_fingerprint) keyed
/// to the analysis-affecting subset of that state.
#[derive(Debug, Clone)]
pub struct CommandSemantics {
    /// Client environment snapshot, ordered by name. This is the *tracked* env —
    /// the per-command replacement for `std::env`. Read it via [`Self::getenv`].
    env: BTreeMap<Bytes, Bytes>,
    /// Normalized argv exactly as the client sent it (argv[0] is the program).
    argv: Vec<Bytes>,
    /// The build command verb, if recognizable (argv[1] as UTF-8 lossy), e.g.
    /// `"build"`, `"test"`, `"query"`. `None` if argv[1] is missing.
    command: Option<String>,
    /// Parsed `--define KEY=VALUE` flags, sorted by key for determinism.
    defines: Vec<BuildDefine>,
    /// All flag-shaped argv tokens (those starting with `-`), in order. Part of
    /// the analysis-affecting configuration.
    flags: Vec<Bytes>,
    /// The client's working directory (raw bytes; Unix paths are not UTF-8).
    working_directory: Bytes,
    /// The output base / workspace root this command resolves against. Derived
    /// from `--output_base=` if present, else the working directory.
    output_base: Bytes,
    /// The deterministic config fingerprint, computed once at construction.
    config_fingerprint: [u8; 32],
}

impl CommandSemantics {
    /// Build the per-command context from a client [`RunRequest`].
    ///
    /// Validates the protocol version and the minimal request shape, snapshots the
    /// environment into an ordered map, parses `--define`s and flags from argv, and
    /// computes the [`config_fingerprint`](Self::config_fingerprint).
    ///
    /// Nothing here reads `std::env` — the resulting context depends only on the
    /// request, which is the whole point.
    pub fn from_run_request(request: &RunRequest) -> Result<Self, CommandError> {
        if request.protocol_major != PROTOCOL_MAJOR {
            return Err(CommandError::ProtocolMismatch {
                client_major: request.protocol_major,
                client_minor: request.protocol_minor,
                daemon_major: PROTOCOL_MAJOR,
                daemon_minor: PROTOCOL_MINOR,
            });
        }
        if request.argv.is_empty() {
            return Err(CommandError::InvalidRequest("argv is empty".to_string()));
        }

        // Environment snapshot — last write wins on duplicate names, mirroring how
        // a real environment behaves. Ordered (BTreeMap) for deterministic hashing.
        let mut env: BTreeMap<Bytes, Bytes> = BTreeMap::new();
        for var in &request.environment {
            env.insert(var.name.clone(), var.value.clone());
        }

        let argv: Vec<Bytes> = request.argv.clone();
        let working_directory = request.working_directory.clone();

        // Parsing of flags / `--define`s and the fingerprint derivation is shared
        // with `from_process` so the wire-sourced and process-sourced capture
        // points stay in lockstep. The daemon must NOT read its own disk on a
        // client's behalf, so the workspace-config content (root `.bazelrc` +
        // `WORKSPACE`/`WORKSPACE.bazel` + `MODULE.bazel`) is carried explicitly
        // by the client in `request.workspace_config` and folded into the
        // fingerprint here via the SAME `from_parts` code path that `from_process`'s
        // `read_workspace_config_content` feeds. A daemon request carrying the
        // same config content therefore produces a fingerprint identical to the
        // in-process `from_process` for the same workspace.
        let workspace_config: Vec<(Vec<u8>, Vec<u8>)> = request
            .workspace_config
            .iter()
            .map(|f| (f.name.to_vec(), f.content.to_vec()))
            .collect();

        Ok(Self::from_parts(
            argv,
            env,
            working_directory,
            workspace_config,
        ))
    }

    /// Build the per-command context by snapshotting **this process's** own
    /// argv / environment / working directory.
    ///
    /// This is the single-shot *transitional* capture point used before the
    /// daemon exists: the live CLI process reads `std::env::args_os()`,
    /// `std::env::vars_os()` and the current directory exactly once, here, and
    /// hands the resulting immutable snapshot to analysis. It is the analysis
    /// path's ONE legitimate `std::env` read — analogous to the executor's
    /// allowlisted raw IO — the very capture point that the tracked-read rule
    /// exists to funnel everything else away from. Once the daemon lands, this
    /// is replaced by [`from_run_request`](Self::from_run_request) (the client's
    /// snapshot over the wire); the rest of analysis is unaffected because it
    /// only ever sees the resulting [`CommandSemantics`].
    ///
    /// Unlike [`from_run_request`], this never fails on a protocol check (there
    /// is no wire protocol involved) and tolerates a missing cwd (falls back to
    /// an empty working directory rather than aborting the build).
    pub fn from_process() -> Self {
        // argv[0] is the program name, exactly as `from_run_request` expects.
        let argv: Vec<Bytes> = std::env::args_os()
            .map(|a| Bytes::from(os_string_bytes(&a)))
            .collect();

        let mut env: BTreeMap<Bytes, Bytes> = BTreeMap::new();
        for (name, value) in std::env::vars_os() {
            env.insert(
                Bytes::from(os_string_bytes(&name)),
                Bytes::from(os_string_bytes(&value)),
            );
        }

        let cwd = std::env::current_dir().ok();
        let working_directory = cwd
            .as_ref()
            .map(|p| Bytes::from(os_path_bytes(p)))
            .unwrap_or_default();

        // Group 1 (persistent-daemon TrackedIo finishing slice): the root
        // `.bazelrc`, `WORKSPACE`/`WORKSPACE.bazel`, and `MODULE.bazel` are read
        // at CLI startup, BEFORE the per-command `tracked::scope` exists, so they
        // cannot be routed through `tracked::*`. Their *content* nonetheless
        // affects the build configuration and the analysis graph, so a reused
        // daemon graph must invalidate when they change. We fold their content
        // into the config fingerprint (recorded as a single
        // `ConfigFingerprint` dep): editing any file changes the fingerprint,
        // the dep goes Stale, and the graph is invalidated. This is the
        // single-shot capture point, so reading our own disk here is legitimate
        // (analogous to the argv/env/cwd snapshot above).
        let workspace_config = cwd
            .as_deref()
            .map(read_workspace_config_content)
            .unwrap_or_default();

        Self::from_parts(argv, env, working_directory, workspace_config)
    }

    /// Shared constructor for [`from_run_request`] and [`from_process`]: given an
    /// argv, an environment snapshot, and a working directory, parse the flags /
    /// defines and compute the fingerprint. Keeps both capture points in lockstep
    /// (identical parsing + fingerprinting) so a daemon command and a single-shot
    /// command with the same inputs produce the same semantics.
    fn from_parts(
        argv: Vec<Bytes>,
        env: BTreeMap<Bytes, Bytes>,
        working_directory: Bytes,
        workspace_config: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Self {
        let command = argv
            .get(1)
            .map(|b| String::from_utf8_lossy(b).into_owned());

        let mut flags: Vec<Bytes> = Vec::new();
        let mut defines: Vec<BuildDefine> = Vec::new();
        let mut idx = 1usize; // skip argv[0] (program name)
        while idx < argv.len() {
            let tok = &argv[idx];
            if tok.starts_with(b"-") {
                // Exclude NON-analysis-affecting routing/watcher flags from the
                // fingerprint flags: `--daemon` selects the transport and
                // `--watchfs` selects the daemon's freshness-check strategy.
                // Neither changes the analyzed graph, so folding them into the
                // config fingerprint would needlessly fork the reuse key (the
                // daemon already strips `--daemon`; clap injects a default
                // `--watchfs=auto` on EVERY build, which must not perturb the key
                // vs a cached snapshot or the in-process path). The token stays
                // in `argv` so the orchestrator can still read the watcher mode.
                let s = String::from_utf8_lossy(tok);
                let is_routing = s == "--daemon"
                    || s.starts_with("--daemon=")
                    || s == "--watchfs"
                    || s.starts_with("--watchfs=");
                if !is_routing {
                    flags.push(tok.clone());
                }
            }
            let tok_str = String::from_utf8_lossy(tok);
            if let Some(rest) = tok_str.strip_prefix("--define=") {
                if let Some(def) = parse_define(rest) {
                    defines.push(def);
                }
            } else if tok_str == "--define" {
                if let Some(next) = argv.get(idx + 1) {
                    let next_str = String::from_utf8_lossy(next);
                    if let Some(def) = parse_define(&next_str) {
                        defines.push(def);
                    }
                    idx += 1;
                }
            }
            idx += 1;
        }
        defines.sort();
        defines.dedup();

        let output_base = flags
            .iter()
            .find_map(|f| {
                let s = String::from_utf8_lossy(f);
                s.strip_prefix("--output_base=")
                    .map(|p| Bytes::from(p.as_bytes().to_vec()))
            })
            .unwrap_or_else(|| working_directory.clone());

        let config_fingerprint = compute_config_fingerprint(
            &env,
            &flags,
            &defines,
            &output_base,
            &workspace_config,
        );

        Self {
            env,
            argv,
            command,
            defines,
            flags,
            working_directory,
            output_base,
            config_fingerprint,
        }
    }

    /// Tracked, per-command environment read.
    ///
    /// **This is the replacement for `std::env::var`.** Analysis code that today
    /// reads the process-global environment should (in a later migration brick)
    /// read it through this accessor so that each command sees only the
    /// environment its own client sent — never the daemon's process environment
    /// and never another command's.
    pub fn getenv(&self, name: &[u8]) -> Option<&[u8]> {
        self.env.get(name).map(|v| v.as_ref())
    }

    /// The ordered client environment snapshot.
    pub fn env(&self) -> &BTreeMap<Bytes, Bytes> {
        &self.env
    }

    /// The normalized argv (argv[0] is the program name).
    pub fn argv(&self) -> &[Bytes] {
        &self.argv
    }

    /// The build verb (argv[1] as lossy UTF-8), if present.
    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    /// All flag-shaped argv tokens, in order.
    pub fn flags(&self) -> &[Bytes] {
        &self.flags
    }

    /// The parsed `--define`s (sorted, deduped).
    ///
    /// **This is the replacement for the process-global `config_defines`.**
    pub fn defines(&self) -> &[BuildDefine] {
        &self.defines
    }

    /// The client's working directory (raw bytes).
    pub fn working_directory(&self) -> &[u8] {
        &self.working_directory
    }

    /// The output base / workspace root for this command (raw bytes).
    pub fn output_base(&self) -> &[u8] {
        &self.output_base
    }

    /// Deterministic fingerprint of the analysis-affecting configuration.
    ///
    /// Two requests that differ only in analysis-irrelevant ways (e.g. a different
    /// `client_build_id`) yield the same fingerprint; two requests that differ in
    /// env, flags, defines, or output base yield different fingerprints. This will
    /// later key analysis-cache reuse, so it must be stable across process restarts
    /// — hence the explicit length-prefixed, domain-separated encoding below rather
    /// than relying on `Debug` formatting or `Hash` (whose ordering/representation
    /// is not a stability contract).
    pub fn config_fingerprint(&self) -> &[u8; 32] {
        &self.config_fingerprint
    }
}

/// Populate a [`RunRequest`]'s `workspace_config` from a workspace root.
///
/// This is the CLIENT-side capture point for the daemon path: the client (which
/// has the live filesystem) reads the root `.bazelrc` / `WORKSPACE` /
/// `WORKSPACE.bazel` / `MODULE.bazel` and stuffs their content into the request,
/// so the daemon can fold it into the config fingerprint WITHOUT reading its own
/// disk. It uses the exact same reader
/// [`from_process`](CommandSemantics::from_process) uses, so a daemon request
/// built this way fingerprints identically to the in-process path for the same
/// workspace.
///
/// `start` is any directory inside the workspace (typically the client's cwd);
/// the workspace root is discovered by walking up to the first marker, exactly
/// like the in-process path.
pub fn populate_workspace_config(request: &mut RunRequest, start: &std::path::Path) {
    request.workspace_config = read_workspace_config_content(start)
        .into_iter()
        .map(|(name, content)| WorkspaceConfigFile {
            name: Bytes::from(name),
            content: Bytes::from(content),
        })
        .collect();
}

/// Convenience extension: build a [`RunRequest`] carrying the workspace config.
pub trait RunRequestExt {
    /// Return `self` with `workspace_config` populated from the workspace
    /// containing `start` (the client's cwd). See [`populate_workspace_config`].
    fn with_workspace_config_from(self, start: &std::path::Path) -> Self;
}

impl RunRequestExt for RunRequest {
    fn with_workspace_config_from(mut self, start: &std::path::Path) -> Self {
        populate_workspace_config(&mut self, start);
        self
    }
}

/// Parse a `KEY=VALUE` define body. Returns `None` if there is no `=`.
fn parse_define(body: &str) -> Option<BuildDefine> {
    let (key, value) = body.split_once('=')?;
    Some(BuildDefine {
        key: key.to_string(),
        value: value.to_string(),
    })
}

/// Length-prefixed, domain-separated blake3 over the analysis-affecting state.
///
/// The encoding is unambiguous: every variable-length field is prefixed with its
/// byte length as a little-endian u64, so no concatenation can be reinterpreted
/// (e.g. env `{ "AB" -> "" }` cannot collide with `{ "A" -> "B" }`).
fn compute_config_fingerprint(
    env: &BTreeMap<Bytes, Bytes>,
    flags: &[Bytes],
    defines: &[BuildDefine],
    output_base: &[u8],
    workspace_config: &[(Vec<u8>, Vec<u8>)],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(FINGERPRINT_DOMAIN);

    // Environment (BTreeMap iterates in sorted key order -> deterministic).
    let analysis_env = env
        .iter()
        .filter(|(name, _)| !is_diagnostic_env(name))
        .collect::<Vec<_>>();
    hasher.update(b"env\n");
    hasher.update(&(analysis_env.len() as u64).to_le_bytes());
    for (name, value) in analysis_env {
        update_lp(&mut hasher, name);
        update_lp(&mut hasher, value);
    }

    // Flags (in argv order — order is significant for flags).
    hasher.update(b"flags\n");
    hasher.update(&(flags.len() as u64).to_le_bytes());
    for f in flags {
        update_lp(&mut hasher, f);
    }

    // Defines (already sorted+deduped → deterministic).
    hasher.update(b"defines\n");
    hasher.update(&(defines.len() as u64).to_le_bytes());
    for d in defines {
        update_lp(&mut hasher, d.key.as_bytes());
        update_lp(&mut hasher, d.value.as_bytes());
    }

    // Output base.
    hasher.update(b"output_base\n");
    update_lp(&mut hasher, output_base);

    // Workspace-config content (root `.bazelrc` + `WORKSPACE`/`WORKSPACE.bazel`
    // + `MODULE.bazel`).
    // Each entry is (relative-name, content); the list is ordered deterministically
    // by the producer (read_workspace_config_content), and each field is
    // length-prefixed so a changed file name or body changes the fingerprint.
    // Empty content (file absent, or the wire path) contributes a length-0 body so
    // that a present-but-empty file and an absent file stay distinguishable via the
    // name list and entry count.
    hasher.update(b"workspace_config\n");
    hasher.update(&(workspace_config.len() as u64).to_le_bytes());
    for (name, content) in workspace_config {
        update_lp(&mut hasher, name);
        update_lp(&mut hasher, content);
    }

    *hasher.finalize().as_bytes()
}

fn is_diagnostic_env(name: &[u8]) -> bool {
    matches!(
        name,
        b"RUSH_ACTION_TIMING"
            | b"RUSH_ACTION_TRACE"
            | b"RUSH_PROFILE_PHASES"
            | b"RUSH_REUSE_TIMING"
    )
}

/// Read the workspace-root `.bazelrc`, `WORKSPACE`/`WORKSPACE.bazel`, and
/// `MODULE.bazel` content for the config fingerprint (Group 1).
///
/// Walks up from `start` looking for a workspace root marker
/// (`WORKSPACE`, `WORKSPACE.bazel`, or `MODULE.bazel`); the first ancestor with a
/// marker is the workspace root. At that root we read `.bazelrc`, `WORKSPACE`,
/// `WORKSPACE.bazel`, and `MODULE.bazel` (whichever exist) and return their
/// (name, content) pairs in a fixed order. Returns an empty vec if no workspace
/// root is found.
///
/// This deliberately does NOT consume `import`/`try-import` lines in the
/// `.bazelrc` (a Bazel feature `BazelRc::parse` handles): the parser remains the
/// authority on build behavior; this function only contributes content to the
/// invalidation fingerprint, and the root files are the ones read at startup
/// before any per-command tracked scope.
fn read_workspace_config_content(start: &std::path::Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    const MARKERS: [&str; 3] = ["WORKSPACE", "WORKSPACE.bazel", "MODULE.bazel"];
    const CONFIG_FILES: [&str; 4] = [".bazelrc", "WORKSPACE", "WORKSPACE.bazel", "MODULE.bazel"];

    let mut root: Option<&std::path::Path> = None;
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if MARKERS.iter().any(|m| dir.join(m).exists()) {
            root = Some(dir);
            break;
        }
        cur = dir.parent();
    }

    let Some(root) = root else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for name in CONFIG_FILES {
        let path = root.join(name);
        if let Ok(content) = std::fs::read(&path) {
            out.push((name.as_bytes().to_vec(), content));
        }
    }
    out
}

/// Update the hasher with a length-prefixed byte field.
fn update_lp(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Lossless bytes of an `OsString` for the process-capture path. On Unix this is
/// the raw bytes (argv / env / paths are not guaranteed UTF-8); elsewhere we fall
/// back to lossy UTF-8. rush's target platform is Unix.
fn os_string_bytes(s: &std::ffi::OsString) -> Vec<u8> {
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

/// Lossless bytes of a `Path` (the cwd) for the process-capture path.
fn os_path_bytes(p: &std::path::Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        p.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        p.to_string_lossy().into_owned().into_bytes()
    }
}

#[cfg(test)]
mod workspace_config_tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A throwaway directory under the system temp dir, cleaned up on drop.
    struct TmpDir(std::path::PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let uniq = format!(
                "rush_wscfg_{}_{}_{}",
                tag,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            p.push(uniq);
            std::fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fp_with(workspace_config: &[(Vec<u8>, Vec<u8>)]) -> [u8; 32] {
        let env: BTreeMap<Bytes, Bytes> = BTreeMap::new();
        compute_config_fingerprint(&env, &[], &[], b"/ws", workspace_config)
    }

    #[test]
    fn workspace_config_content_changes_fingerprint() {
        // Same env/flags/defines/output_base, different `.bazelrc` content ⇒
        // different fingerprint. This is the non-vacuous proof that editing the
        // root config files (folded in via `read_workspace_config_content`)
        // flips the recorded `ConfigFingerprint` dependency Stale.
        let a = fp_with(&[(b".bazelrc".to_vec(), b"build --copt=-O2\n".to_vec())]);
        let b = fp_with(&[(b".bazelrc".to_vec(), b"build --copt=-O3\n".to_vec())]);
        assert_ne!(a, b, "changing .bazelrc content must change the fingerprint");

        // Absent config files (the wire path / no workspace root) differ from a
        // present-but-empty `.bazelrc`.
        let none = fp_with(&[]);
        let empty = fp_with(&[(b".bazelrc".to_vec(), Vec::new())]);
        assert_ne!(none, empty, "present-empty must differ from absent");
    }

    #[test]
    fn read_workspace_config_finds_root_and_reads_files() {
        // Marker + config files at the root; cwd is a nested subdir. The walk
        // must find the root and return `.bazelrc` + `WORKSPACE` content in order.
        let dir = TmpDir::new("root");
        let root = dir.path();
        std::fs::write(root.join("WORKSPACE"), b"workspace(name='x')\n").unwrap();
        std::fs::write(root.join(".bazelrc"), b"build --copt=-O2\n").unwrap();
        let nested = root.join("pkg/sub");
        std::fs::create_dir_all(&nested).unwrap();

        let got = read_workspace_config_content(&nested);
        // CONFIG_FILES order is .bazelrc, WORKSPACE, WORKSPACE.bazel,
        // MODULE.bazel.
        assert_eq!(got.len(), 2, "found .bazelrc + WORKSPACE");
        assert_eq!(got[0].0, b".bazelrc".to_vec());
        assert_eq!(got[0].1, b"build --copt=-O2\n".to_vec());
        assert_eq!(got[1].0, b"WORKSPACE".to_vec());
        assert_eq!(got[1].1, b"workspace(name='x')\n".to_vec());
    }

    #[test]
    fn module_bazel_content_changes_workspace_config_fingerprint() {
        let dir = TmpDir::new("module");
        let root = dir.path();
        std::fs::write(
            root.join("MODULE.bazel"),
            b"module(name = \"demo\")\nbazel_dep(name = \"rules_cc\", version = \"0.0.9\")\n",
        )
        .unwrap();

        let first = read_workspace_config_content(root);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, b"MODULE.bazel".to_vec());
        let first_fingerprint = fp_with(&first);

        std::fs::write(
            root.join("MODULE.bazel"),
            b"module(name = \"demo\")\nbazel_dep(name = \"rules_cc\", version = \"0.1.1\")\n",
        )
        .unwrap();
        let second = read_workspace_config_content(root);
        assert_ne!(first_fingerprint, fp_with(&second));
    }

    #[test]
    fn read_workspace_config_no_root_is_empty() {
        // A directory tree with no workspace marker yields no config content
        // (and never panics walking to the filesystem root).
        let dir = TmpDir::new("noroot");
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(read_workspace_config_content(&nested).is_empty());
    }

    /// Process-global cwd is shared mutable state; serialize the cwd-mutating
    /// tests so they don't race each other (or any other test that reads cwd).
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Build a `RunRequest` that mirrors EXACTLY what `from_process` snapshots
    /// (same argv, env, cwd), plus the client-supplied workspace_config. This is
    /// the daemon path's input: the client captured argv/env/cwd and the root
    /// config files and shipped them over the wire.
    fn run_request_mirroring_process(
        argv: &[Bytes],
        env: &BTreeMap<Bytes, Bytes>,
        cwd: &Bytes,
        workspace_root: &std::path::Path,
    ) -> RunRequest {
        let mut req = RunRequest {
            auth_cookie: Bytes::new(),
            argv: argv.to_vec(),
            working_directory: cwd.clone(),
            environment: env
                .iter()
                .map(|(name, value)| {
                    rush_protocol::command::v1::EnvironmentVariable {
                        name: name.clone(),
                        value: value.clone(),
                    }
                })
                .collect(),
            block_for_lock: false,
            client_build_id: String::new(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            workspace_config: Vec::new(),
        };
        // Client-side capture of the root config files (the legitimate read
        // point) — the exact reader the in-process path uses.
        populate_workspace_config(&mut req, workspace_root);
        req
    }

    /// THE key invariant: for the same workspace + same argv/env/cwd, the daemon
    /// path (`from_run_request` with the client-carried workspace_config) produces
    /// a config fingerprint IDENTICAL to the in-process path (`from_process`).
    /// And editing `.bazelrc` shifts BOTH fingerprints identically. This proves
    /// the daemon's reuse key covers workspace config exactly like the CLI.
    #[test]
    fn daemon_fingerprint_matches_in_process_for_same_workspace_config() {
        let _guard = CWD_LOCK.lock().unwrap();
        let saved_cwd = std::env::current_dir().ok();

        let dir = TmpDir::new("fp_eq");
        let root = dir.path();
        std::fs::write(root.join("WORKSPACE"), b"workspace(name='proj')\n").unwrap();
        std::fs::write(root.join(".bazelrc"), b"build --copt=-O2\n").unwrap();

        // Enter the workspace so `from_process` captures it as cwd and finds the
        // root markers there.
        std::env::set_current_dir(root).unwrap();

        // In-process path: real argv/env/cwd + on-disk config read here.
        let in_process = CommandSemantics::from_process();

        // Mirror the SAME argv/env/cwd that from_process just snapshotted, so the
        // ONLY thing under test is whether the wire-carried workspace_config folds
        // identically. (We re-read argv/env/cwd the same way from_process does.)
        let argv: Vec<Bytes> = std::env::args_os()
            .map(|a| Bytes::from(os_string_bytes(&a)))
            .collect();
        let mut env: BTreeMap<Bytes, Bytes> = BTreeMap::new();
        for (name, value) in std::env::vars_os() {
            env.insert(
                Bytes::from(os_string_bytes(&name)),
                Bytes::from(os_string_bytes(&value)),
            );
        }
        let cwd = Bytes::from(os_path_bytes(
            &std::env::current_dir().unwrap(),
        ));

        let req = run_request_mirroring_process(&argv, &env, &cwd, root);
        let daemon = CommandSemantics::from_run_request(&req).unwrap();

        assert_eq!(
            in_process.config_fingerprint(),
            daemon.config_fingerprint(),
            "daemon-path fingerprint must equal in-process fingerprint for the same workspace config"
        );

        // Now edit .bazelrc and re-derive BOTH paths. They must (a) each change
        // from their previous value and (b) remain equal to each other.
        std::fs::write(root.join(".bazelrc"), b"build --copt=-O3\n").unwrap();

        let in_process_2 = CommandSemantics::from_process();
        let req_2 = run_request_mirroring_process(&argv, &env, &cwd, root);
        let daemon_2 = CommandSemantics::from_run_request(&req_2).unwrap();

        assert_ne!(
            in_process.config_fingerprint(),
            in_process_2.config_fingerprint(),
            "editing .bazelrc must change the in-process fingerprint"
        );
        assert_ne!(
            daemon.config_fingerprint(),
            daemon_2.config_fingerprint(),
            "editing .bazelrc must change the daemon fingerprint"
        );
        assert_eq!(
            in_process_2.config_fingerprint(),
            daemon_2.config_fingerprint(),
            "after the edit the two paths must still agree"
        );

        // Restore cwd before releasing the lock so unrelated tests are unaffected.
        if let Some(c) = saved_cwd {
            let _ = std::env::set_current_dir(c);
        }
    }
}
