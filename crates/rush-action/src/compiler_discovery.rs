// Rush Action - Pending compiler dependency discovery
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Requests and byte parsing for a future native compiler-discovery boundary.
//!
//! Nothing here executes a compiler, certifies a read, authorizes a cache hit,
//! or changes an existing action/registry serialization. A depfile lists positive
//! filenames; it does not prove negative searches, SDK completeness, producer
//! ordering, or the values actually read. Those require later certification.
//! Compilers can also change filename spelling in their output (for example,
//! Clang can replace header-name backslashes with slashes). This parser retains
//! emitted spelling; a future consumer must not assume it identifies a read.

use crate::command_spec::NormalizedCommandSpec;
use anyhow::{anyhow, bail, Result};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Maximum admitted wire size, bounding parser allocation independently of IO.
pub const MAX_DEPFILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 64 * 1024;
const MAX_DEPENDENCIES: usize = 262_144;
const MARKER_PREFIX: &str = "rush_cpp_discovery_v1_";

/// Requested compiler transport family, not proof of the executable's identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompilerFamily {
    /// GCC Make-style dependency output.
    Gcc,
    /// Clang Make-style dependency output.
    Clang,
}

/// Requested private depfile destination and exact single-target marker.
///
/// The caller's path/token are only a request and a cross-output mixup guard.
/// They are NOT a filesystem ownership capability or read-time authority.
/// A future manager must create/hold the destination and certify its contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepfileRequest {
    path: PathBuf,
    marker: String,
}

impl DepfileRequest {
    /// Describe a destination without reading or creating anything. The token
    /// must be chosen by the later owning manager for its particular request.
    pub fn new(path: PathBuf, request_token: [u8; 32]) -> Result<Self> {
        require_absolute_path(&path, "depfile destination")?;
        if path.file_name().is_none() {
            bail!("depfile destination requires a filename");
        }
        Ok(Self {
            path,
            marker: format!(
                "{MARKER_PREFIX}{}",
                blake3::Hash::from(request_token).to_hex()
            ),
        })
    }

    /// Requested destination, whose physical ownership remains unproved.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// ASCII target supplied exactly to `-MT`, with no Make metacharacters.
    pub fn target_marker(&self) -> &str {
        &self.marker
    }

    /// Owned transport arguments to be installed only at a later finalized
    /// manager boundary. `-MD` retains SDK inputs; `-MMD` would omit them.
    /// This method neither mutates nor validates another command's arguments.
    pub fn dependency_arguments(&self) -> Vec<OsString> {
        vec![
            OsString::from("-MD"),
            OsString::from("-MF"),
            self.path.as_os_str().to_os_string(),
            OsString::from("-MT"),
            OsString::from(&self.marker),
        ]
    }

    /// Parse one positive dependency list and check this request's marker.
    /// A match is not certification of freshness, ownership or consumed values.
    pub fn parse(&self, bytes: &[u8]) -> Result<DepFileEntry> {
        let entry = parse_depfile_bytes(bytes)?;
        if entry.target.as_os_str() != std::ffi::OsStr::new(&self.marker) {
            bail!("compiler depfile target does not match the requested marker");
        }
        Ok(entry)
    }
}

/// Pending compile request. It deliberately has no ActionId/certification API.
///
/// The normalized command is a SNAPSHOT, not a finalized execution command.
/// Later native command rewrites must run before a manager binds discovery to
/// what actually executes. This type cannot authorize those rewrites or reuse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingCompileDiscovery {
    family: CompilerFamily,
    command_snapshot: NormalizedCommandSpec,
    source: PathBuf,
    object_output: PathBuf,
    depfile: DepfileRequest,
}

impl PendingCompileDiscovery {
    /// Check request shape only; do not inspect paths or mint execution proof.
    pub fn new(
        family: CompilerFamily,
        command_snapshot: NormalizedCommandSpec,
        source: PathBuf,
        object_output: PathBuf,
        depfile: DepfileRequest,
    ) -> Result<Self> {
        if !matches!(
            command_snapshot.mnemonic.as_str(),
            "CCompile" | "CppCompile"
        ) {
            bail!("compiler discovery requires a C/C++ compile request");
        }
        let cwd = command_snapshot.cwd.as_deref().ok_or_else(|| {
            anyhow!("pending compiler discovery requires an explicit working directory")
        })?;
        require_absolute_path(cwd, "compile working directory")?;
        require_absolute_path(&source, "compile source")?;
        require_absolute_path(&object_output, "compile object output")?;
        if source.file_name().is_none() || object_output.file_name().is_none() {
            bail!("compiler discovery source/object require filenames");
        }
        if source == object_output || depfile.path == source || depfile.path == object_output {
            bail!("compiler discovery source/object/depfile destinations must be distinct");
        }
        if !command_snapshot.declared_inputs.contains(&source)
            || !command_snapshot.declared_outputs.contains(&object_output)
        {
            bail!("pending compiler discovery source/object must be declared in the snapshot");
        }
        if command_snapshot.declared_inputs.contains(&depfile.path) {
            bail!("requested depfile cannot also be a declared compile input");
        }
        Ok(Self {
            family,
            command_snapshot,
            source,
            object_output,
            depfile,
        })
    }

    /// Requested family; executable/version qualification remains pending.
    pub fn family(&self) -> CompilerFamily {
        self.family
    }

    /// Unfinalized normalized snapshot. No later mutation is covered by it.
    pub fn command_snapshot(&self) -> &NormalizedCommandSpec {
        &self.command_snapshot
    }

    /// Exact requested source spelling, before any filesystem resolution.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Requested object destination; this type does not own its inode.
    pub fn object_output(&self) -> &Path {
        &self.object_output
    }

    /// Requested dependency transport, not a capability to publish its result.
    pub fn depfile_request(&self) -> &DepfileRequest {
        &self.depfile
    }
}

fn require_absolute_path(path: &Path, description: &str) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        || path.as_os_str().as_encoded_bytes().contains(&0)
    {
        bail!("{description} requires an absolute path without parent traversal or NUL");
    }
    Ok(())
}

/// One parsed compiler rule. Every dependency is retained in emitted order,
/// including duplicates, extensionless paths and non-header source files.
/// Parsing this structure does not certify the listed inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepFileEntry {
    /// Decoded single rule target (the pending request uses an ASCII marker).
    pub target: PathBuf,
    /// All decoded positive dependency filenames; no extension filtering.
    pub dependencies: Vec<PathBuf>,
}

/// Parse the admitted GCC/Clang single-rule byte grammar, not a Make program.
///
/// Admit space-delimited words, compiler line wrapping between words, escaped
/// spaces/tabs and `#`, doubled `$`, literal backslashes outside those escape
/// contexts, and interior RHS colons. Reject extra rules/targets, variables,
/// comments, recipes, order-only/assignment syntax, controls and malformed
/// continuations. On Unix filenames preserve their raw bytes. Other platforms
/// refuse invalid UTF-8 rather than substitute replacement characters.
///
/// Compiler Make output is not an injective encoding for arbitrary filenames
/// (for example a trailing backslash beside a separator). This parser's byte
/// interpretation is never a substitute for later read-time input matching.
pub fn parse_depfile_bytes(bytes: &[u8]) -> Result<DepFileEntry> {
    if bytes.len() > MAX_DEPFILE_BYTES {
        bail!("compiler depfile exceeds the admitted byte limit");
    }
    if bytes.contains(&0) {
        bail!("compiler depfile contains NUL");
    }
    let mut parser = DepfileParser { bytes, index: 0 };
    parser.spaces();
    let target = parser.word(true)?;
    parser.spaces();
    if target.is_empty() || parser.bytes.get(parser.index) != Some(&b':') {
        bail!("Invalid depfile: missing colon or multiple target words");
    }
    parser.index += 1;
    let mut dependencies = Vec::new();
    loop {
        parser.spaces_and_continuations()?;
        if parser.index == bytes.len() {
            break;
        }
        if matches!(bytes[parser.index], b'\r' | b'\n') {
            parser.newline()?;
            while parser.index < bytes.len() {
                parser.spaces();
                if parser.index == bytes.len() {
                    break;
                }
                if !matches!(bytes[parser.index], b'\r' | b'\n') {
                    bail!("compiler depfile contains another rule or trailing data");
                }
                parser.newline()?;
            }
            break;
        }
        if dependencies.len() == MAX_DEPENDENCIES {
            bail!("compiler depfile exceeds the admitted dependency count");
        }
        let word = parser.word(false)?;
        if word.is_empty() {
            bail!("compiler depfile contains an empty dependency token");
        }
        dependencies.push(path_from_bytes(word)?);
    }
    if dependencies.is_empty() {
        bail!("Invalid depfile: no dependencies found");
    }
    Ok(DepFileEntry {
        target: path_from_bytes(target)?,
        dependencies,
    })
}

struct DepfileParser<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl DepfileParser<'_> {
    fn spaces(&mut self) {
        while self.bytes.get(self.index) == Some(&b' ') {
            self.index += 1;
        }
    }

    fn newline(&mut self) -> Result<()> {
        if self.bytes.get(self.index) == Some(&b'\r') {
            self.index += 1;
        }
        if self.bytes.get(self.index) != Some(&b'\n') {
            bail!("compiler depfile contains a bare carriage return");
        }
        self.index += 1;
        Ok(())
    }

    fn spaces_and_continuations(&mut self) -> Result<()> {
        loop {
            self.spaces();
            if self.bytes.get(self.index) != Some(&b'\\')
                || !matches!(self.bytes.get(self.index + 1), Some(b'\r' | b'\n'))
            {
                return Ok(());
            }
            self.index += 1;
            self.newline()?;
            if self.bytes.get(self.index) != Some(&b' ') {
                bail!("compiler depfile continuation requires indented next word");
            }
            self.spaces();
            if self
                .bytes
                .get(self.index)
                .is_none_or(|byte| matches!(byte, b'\r' | b'\n'))
            {
                bail!("compiler depfile continuation has no next filename");
            }
        }
    }

    fn word(&mut self, target: bool) -> Result<Vec<u8>> {
        let mut word = Vec::new();
        while let Some(&byte) = self.bytes.get(self.index) {
            if matches!(byte, b' ' | b'\r' | b'\n') || (target && byte == b':') {
                break;
            }
            if byte == b'\\' {
                let start = self.index;
                while self.bytes.get(self.index) == Some(&b'\\') {
                    self.index += 1;
                }
                let count = self.index - start;
                let next = *self
                    .bytes
                    .get(self.index)
                    .ok_or_else(|| anyhow!("compiler depfile ends in a backslash"))?;
                match next {
                    b' ' | b'\t' => {
                        if count % 2 == 0 {
                            bail!("compiler depfile has ambiguous backslashes before whitespace");
                        }
                        word.extend(std::iter::repeat(b'\\').take(count / 2));
                        word.push(next);
                        self.index += 1;
                    }
                    b'#' => {
                        // GCC/Clang add exactly one slash before #, without
                        // doubling pre-existing literal backslashes there.
                        word.extend(std::iter::repeat(b'\\').take(count - 1));
                        word.push(b'#');
                        self.index += 1;
                    }
                    b'\r' | b'\n' => {
                        bail!("compiler depfile continuation splits a filename");
                    }
                    b':' if target => {
                        bail!("compiler depfile target has an ambiguous escaped colon");
                    }
                    _ => word.extend(std::iter::repeat(b'\\').take(count)),
                }
            } else if byte == b'$' {
                if self.bytes.get(self.index + 1) != Some(&b'$') {
                    bail!("compiler depfile contains unescaped variable syntax");
                }
                word.push(b'$');
                self.index += 2;
            } else {
                if byte < 32 || byte == 127 || matches!(byte, b'#' | b'|' | b';' | b'=') {
                    bail!("compiler depfile contains unsupported control or Make syntax");
                }
                if byte == b':'
                    && (word.is_empty()
                        || self
                            .bytes
                            .get(self.index + 1)
                            .is_none_or(|next| next.is_ascii_whitespace()))
                {
                    bail!("compiler depfile contains unsupported extra rule syntax");
                }
                word.push(byte);
                self.index += 1;
            }
            if word.len() > MAX_PATH_BYTES {
                bail!("compiler depfile filename exceeds the admitted byte limit");
            }
        }
        Ok(word)
    }
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf> {
    Ok(PathBuf::from(String::from_utf8(bytes).map_err(|_| {
        anyhow!("compiler depfile has non-UTF8 filename bytes unsupported on this platform")
    })?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_depfile_preserves_all_filenames_and_compiler_escapes() {
        let input = br"marker: source.cc api\ name.h hash\#name.h dollar$$name.h colon:name.h win\path\foo.h two\\back.h slash\ back.h slash\\\ space.h slash\\#hash.h extensionless generated.def source.cc";
        let parsed = parse_depfile_bytes(input).unwrap();
        assert_eq!(parsed.target, Path::new("marker"));
        assert_eq!(
            parsed.dependencies,
            [
                "source.cc",
                "api name.h",
                "hash#name.h",
                "dollar$name.h",
                "colon:name.h",
                r"win\path\foo.h",
                r"two\\back.h",
                "slash back.h",
                r"slash\ space.h",
                r"slash\#hash.h",
                "extensionless",
                "generated.def",
                "source.cc",
            ]
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>()
        );
        assert_eq!(
            parse_depfile_bytes(b"marker: literal$$(name).h")
                .unwrap()
                .dependencies,
            vec![PathBuf::from("literal$(name).h")],
            "quoted dollars are literal filename data, never expanded"
        );
        assert_eq!(
            parse_depfile_bytes(b"marker: tab\\\tname.h")
                .unwrap()
                .dependencies,
            vec![PathBuf::from("tab\tname.h")]
        );
    }

    #[test]
    fn compiler_depfile_continuations_are_only_between_filename_words() {
        let parsed =
            parse_depfile_bytes(b"marker: source.c \\\n one.h \\\r\n two.h\r\n\n").unwrap();
        assert_eq!(
            parsed.dependencies,
            vec![
                PathBuf::from("source.c"),
                PathBuf::from("one.h"),
                PathBuf::from("two.h")
            ]
        );
        for bytes in [
            b"marker: split\\\n filename.h\n".as_slice(),
            b"marker: source.c \\\n ",
            b"marker: source.c \\\n",
            b"marker: source.c \\\n\n",
            b"marker: source.c\r",
        ] {
            assert!(
                parse_depfile_bytes(bytes).is_err(),
                "malformed input {bytes:?}"
            );
        }
    }

    #[test]
    fn compiler_depfile_real_transport_profiles_preserve_emitted_spelling() {
        // Reduced GCC 16 and Clang 21 -MD/-MF/-MT output. The absolute case
        // directory is replaced with /case; all escaping/wrapping is retained.
        let gcc = b"marker: \\\n /case/source.c \\\n /usr/include/stdc-predef.h \\\n /case/back\\\\\\ space.h\n";
        assert_eq!(
            parse_depfile_bytes(gcc).unwrap().dependencies,
            vec![
                PathBuf::from("/case/source.c"),
                PathBuf::from("/usr/include/stdc-predef.h"),
                PathBuf::from(r"/case/back\ space.h"),
            ]
        );
        let clang = b"marker: \\\n  /case/source.c \\\n  /case/back/\\ space.h\n";
        assert_eq!(
            parse_depfile_bytes(clang).unwrap().dependencies,
            vec![
                PathBuf::from("/case/source.c"),
                PathBuf::from("/case/back/ space.h"),
            ],
            "the parser cannot recover a read path from compiler-normalized spelling"
        );
        // Successful compilers can emit forms outside this admitted grammar:
        // GCC's trailing-backslash filename is continuation-ambiguous; Clang's
        // raw filename tab and both tools' Make punctuation are refused intact.
        for bytes in [
            b"marker: \\\n /case/source.c \\\n /case/trailing\\\n".as_slice(),
            b"marker: \\\n  /case/source.c \\\n  /case/tab\tname.h\n",
            b"marker: /case/bar|name.h\n",
            b"marker: /case/semi;name.h\n",
        ] {
            assert!(parse_depfile_bytes(bytes).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn compiler_depfile_keeps_non_utf8_filename_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let parsed = parse_depfile_bytes(b"marker: source.c raw-\xff.h utf8-\xc3\xa9.h\n").unwrap();
        assert_eq!(parsed.dependencies[1].as_os_str().as_bytes(), b"raw-\xff.h");
        assert_eq!(
            parsed.dependencies[2].as_os_str().as_bytes(),
            b"utf8-\xc3\xa9.h"
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn compiler_depfile_refuses_unrepresentable_filename_bytes() {
        assert!(parse_depfile_bytes(b"marker: raw-\xff.h\n").is_err());
    }

    #[test]
    fn compiler_depfile_rejects_make_programs_and_malformed_rules() {
        for bytes in [
            b"".as_slice(),
            b"no-colon source.c",
            b": source.c",
            b"one two: source.c",
            b"marker:",
            b"marker: source.c\nother: other.c\n",
            b"marker:: source.c",
            b"marker: source.c | other.h",
            b"marker: source.c; echo run",
            b"marker: source.c x=y",
            b"marker: $(variable).h",
            b"marker: ${variable}.h",
            b"marker: $x.h",
            b"marker: raw#comment.h",
            b"marker: x\0.h",
            b"marker: raw\tname.h",
            b"marker: path\\\\ next.h",
            b"marker: source.c\\",
            b"marker: other: source.c",
            b"target\\:name: source.c",
            b"marker: source.c\n\tcommand\n",
        ] {
            assert!(
                parse_depfile_bytes(bytes).is_err(),
                "must refuse input {bytes:?}"
            );
        }
    }

    #[test]
    fn compiler_depfile_limits_are_enforced_before_unbounded_allocation() {
        let bytes = vec![b' '; MAX_DEPFILE_BYTES + 1];
        assert!(parse_depfile_bytes(&bytes)
            .unwrap_err()
            .to_string()
            .contains("byte limit"));
    }

    #[test]
    fn compiler_depfile_request_checks_exact_marker_without_minting_authority() {
        let root = tempfile::tempdir().unwrap();
        let first = DepfileRequest::new(root.path().join("private one.d"), [1; 32]).unwrap();
        let second = DepfileRequest::new(root.path().join("private two.d"), [2; 32]).unwrap();
        let bytes = format!("{}: source.c extensionless\n", first.target_marker());
        assert_eq!(first.parse(bytes.as_bytes()).unwrap().dependencies.len(), 2);
        assert!(second.parse(bytes.as_bytes()).is_err());
        for alias in [
            format!("{}/", first.target_marker()),
            format!("{}/.", first.target_marker()),
        ] {
            assert!(
                first
                    .parse(format!("{alias}: source.c\n").as_bytes())
                    .is_err(),
                "decoded target spelling must match the marker exactly"
            );
        }
        assert!(
            !first.path().exists(),
            "constructing/parsing a request performs no filesystem publication"
        );
        assert_eq!(
            first.dependency_arguments(),
            vec![
                OsString::from("-MD"),
                OsString::from("-MF"),
                first.path().as_os_str().to_os_string(),
                OsString::from("-MT"),
                OsString::from(first.target_marker())
            ]
        );
        assert!(DepfileRequest::new(PathBuf::from("relative.d"), [1; 32]).is_err());
        assert!(DepfileRequest::new(root.path().join("../escape.d"), [1; 32]).is_err());
    }

    fn snapshot(root: &Path) -> NormalizedCommandSpec {
        NormalizedCommandSpec {
            mnemonic: "CppCompile".into(),
            executable: root.join("compiler"),
            cwd: Some(root.to_path_buf()),
            argv: vec![
                "-c".into(),
                root.join("source.cc").display().to_string(),
                "-o".into(),
                root.join("object.o").display().to_string(),
            ],
            env: Vec::new(),
            declared_inputs: vec![root.join("source.cc")],
            declared_outputs: vec![root.join("object.o")],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: crate::ids::ActionEnvPolicy::ExplicitOnly,
        }
    }

    #[test]
    fn compiler_depfile_pending_descriptor_requires_consistent_request_shape() {
        let root = tempfile::tempdir().unwrap();
        let command = snapshot(root.path());
        let depfile = DepfileRequest::new(root.path().join("owned.d"), [3; 32]).unwrap();
        let pending = PendingCompileDiscovery::new(
            CompilerFamily::Clang,
            command.clone(),
            root.path().join("source.cc"),
            root.path().join("object.o"),
            depfile.clone(),
        )
        .unwrap();
        assert_eq!(pending.family(), CompilerFamily::Clang);
        assert_eq!(pending.command_snapshot(), &command);
        assert_eq!(pending.depfile_request(), &depfile);
        let mut no_cwd = command.clone();
        no_cwd.cwd = None;
        assert!(PendingCompileDiscovery::new(
            CompilerFamily::Gcc,
            no_cwd,
            pending.source().into(),
            pending.object_output().into(),
            depfile.clone()
        )
        .is_err());
        assert!(PendingCompileDiscovery::new(
            CompilerFamily::Gcc,
            command,
            pending.source().into(),
            pending.source().into(),
            depfile
        )
        .is_err());
    }
}
