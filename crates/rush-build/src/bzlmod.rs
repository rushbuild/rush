// Rush Build - Bzlmod (MODULE.bazel) resolution
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Experimental native Bzlmod (MODULE.bazel) parsing and repository resolution.
//!
//! Modern Bazel projects (protobuf v35+, gRPC, …) declare their external
//! dependencies via `MODULE.bazel` + `bazel_dep()` (Bzlmod), not the WORKSPACE
//! `http_archive()` world that TF 2.15 uses. rush historically treated
//! MODULE.bazel as only a workspace-ROOT MARKER (`find_workspace_root`) and
//! ignored `module()`/`bazel_dep()`, so a `@rules_cc//…` label from a Bzlmod
//! project never resolved.
//!
//! This module evaluates MODULE.bazel through the SAME Starlark evaluator the
//! WORKSPACE loader uses (see [`crate::workspace_loader::WorkspaceLoader`]),
//! collects the `bazel_dep()` calls, and produces on-disk repo roots in the shape
//! consumed by rush's external-repository map. Root/transitive MODULE content,
//! vendored-candidate selection (including symlinks), and `RUSH_BZLMOD_*`
//! configuration are observed through the command-scoped tracked-I/O layer. A
//! successful resolution carries a deterministic semantic fingerprint.
//! Unsupported MODULE semantics are recorded by the parser and rejected before
//! a partial resolution can be returned.
//!
//! The native resolver remains a prerequisite, not a production path: it is not
//! installed in the MODULE-only BuildOrchestrator or daemon lifecycle. MODULE-only
//! workspaces retain the Bazel-backed resolver. Network-registry freshness and
//! native resolver lifecycle persistence are not provided; production wiring
//! must supply both.
//!
//! ## Scope safety (load-bearing)
//!
//! TF 2.15's tree CONTAINS MODULE.bazel files (e.g. `tensorflow/MODULE.bazel`),
//! yet builds via WORKSPACE. The native resolver is deliberately not called from
//! constructor-time repository setup: there is no command-scoped tracked-I/O
//! context there, and continuing after a resolution error could publish a stale,
//! partial repository map. Production WORKSPACE behavior is therefore unchanged.
//!
//! ## Current native-resolver scope
//!
//! Parser + a VENDORED registry: each `bazel_dep(name=N, version=V)` resolves to
//! `$RUSH_BZLMOD_REGISTRY/N/V/` (default `<workspace>/bzlmod_modules/N/V/`), an
//! already-unpacked repo with its BUILD/`.bzl` files. Missing versioned modules
//! can be fetched transitively from a BCR primary source URL; source archives and
//! ordered registry patches are SHA256-verified before atomic publication.
//! Mirrors, explicit archive types, overlays, MVS, module-extension evaluation,
//! and scoped canonical repo mappings remain unsupported. Canonical names are
//! simplified to the apparent name (register `@rules_cc`, not
//! `@@rules_cc~0.0.9`).

use crate::external_deps::SystemPackageMapper;
use crate::repository_manager::RepositoryManager;
use anyhow::{Context, Result};
use rush_parse::HttpArchive;
use rush_starlark::{Evaluator, Lexer, Parser, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Default Bazel Central Registry base URL (override with RUSH_BZLMOD_REGISTRY_URL).
const DEFAULT_BCR_URL: &str = "https://bcr.bazel.build";

const BCR_SOURCE_MARKER: &str = ".rush-bcr-source.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BcrPatch {
    name: String,
    integrity: String,
}

fn deserialize_bcr_patches<'de, D>(deserializer: D) -> std::result::Result<Vec<BcrPatch>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct PatchVisitor;

    impl<'de> serde::de::Visitor<'de> for PatchVisitor {
        type Value = Vec<BcrPatch>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map of BCR patch names to SRI digests")
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut patches = Vec::with_capacity(map.size_hint().unwrap_or(0));
            while let Some((name, integrity)) = map.next_entry::<String, String>()? {
                patches.push(BcrPatch { name, integrity });
            }
            Ok(patches)
        }
    }

    deserializer.deserialize_map(PatchVisitor)
}

fn default_bcr_source_type() -> String {
    "archive".to_string()
}

fn deserialize_present_optional_string<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BcrArchiveSource {
    #[serde(default = "default_bcr_source_type", rename = "type")]
    _source_type: String,
    url: String,
    #[serde(default, rename = "mirror_urls")]
    _mirror_urls: Vec<String>,
    integrity: String,
    #[serde(default)]
    strip_prefix: Option<String>,
    #[serde(default, deserialize_with = "deserialize_bcr_patches")]
    patches: Vec<BcrPatch>,
    #[serde(default)]
    patch_strip: usize,
    /// BCR overlays are a separate materialization semantic. Parse the field so
    /// a newer registry entry cannot be mistaken for a complete source tree.
    #[serde(default)]
    overlay: HashMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_present_optional_string")]
    _archive_type: Option<Option<String>>,
    /// BCR-only documentation metadata does not affect materialization.
    #[serde(default, rename = "docs_url")]
    _docs_url: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BcrSourceMarker<'a> {
    module: &'a str,
    version: &'a str,
    source_integrity: &'a str,
    strip_prefix: Option<&'a str>,
    patch_strip: usize,
    patches: &'a [BcrPatch],
}

/// One `bazel_dep()` declaration from a MODULE.bazel file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BazelDep {
    /// Module name (e.g. `rules_cc`).
    pub name: String,
    /// Requested version (e.g. `0.0.9`); empty if unspecified.
    pub version: String,
    /// Apparent repo name the importing module sees (`repo_name=` override,
    /// else the module name). This is the key `@<repo_name>//…` resolves under.
    pub repo_name: String,
}

/// A MODULE directive or argument whose effect this resolver cannot represent.
///
/// The parser retains these records instead of silently discarding calls. The
/// resolver rejects any parsed module containing one, so a future production
/// caller cannot obtain a partial repository graph by accident.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnsupportedModuleSemantic {
    /// MODULE.bazel in which the unsupported construct occurred.
    pub module_file: PathBuf,
    /// Directive name (`use_extension`, `bazel_dep`, ...).
    pub directive: String,
    /// Stable, human-readable reason.
    pub detail: String,
}

/// A parsed MODULE.bazel: the module's own self-declaration + its bazel_deps.
#[derive(Debug, Clone, Default)]
pub struct ParsedModule {
    /// `module(name = …)` (None if no module() call, e.g. a root MODULE.bazel
    /// that only declares deps).
    pub module_name: Option<String>,
    /// `module(version = ...)`, retained for coordinate validation and the
    /// semantic resolution fingerprint.
    pub module_version: Option<String>,
    /// The repo name this module is known by to ITSELF — `module(repo_name=…)`,
    /// defaulting to the module name. `@<self_repo_name>//…` inside the module's
    /// own BUILD/.bzl files resolves to its own repo (e.g. protobuf declares
    /// repo_name="com_google_protobuf" and self-refers as @com_google_protobuf).
    pub self_repo_name: Option<String>,
    /// `bazel_dep()` declarations.
    pub deps: Vec<BazelDep>,
    /// Constructs whose semantics are not represented by the native resolver.
    /// [`resolve`] rejects a module whenever this list is non-empty.
    pub unsupported_semantics: Vec<UnsupportedModuleSemantic>,
}

/// The output a Bzlmod resolver must produce to plug into the existing
/// external-repo machinery (the "registration triad" substrate).
///
/// Persistence boundary: this prerequisite type is serde-serializable and
/// round-trippable, and carries a separately deterministic semantic fingerprint.
/// The tracked command manifest independently proves local-input freshness. It
/// is deliberately not stored as an always-empty field in `AnalysisSnapshot` or
/// the cold certificate. Production wiring must invoke [`resolve`] under command
/// scope and only then store the real resolution and fingerprint alongside the
/// manifest it produced. Serialized map-entry order is not a stable wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BzlmodResolution {
    /// Canonical/apparent repo name -> on-disk repo root. Merged into the
    /// `create_builtin_repos` map (feeds `external_repo_paths` + the bzl_loader).
    pub repos: HashMap<String, PathBuf>,
    /// Per-repo apparent->canonical mapping; empty while `use_repo` is unsupported.
    pub repo_mappings: HashMap<String, HashMap<String, String>>,
    /// Domain-separated fingerprint of the supported, normalized module graph,
    /// tracked Bzlmod environment, selected repository roots, and repo mappings.
    /// This is stable across declaration/HashMap iteration order and is suitable
    /// for persistence or production cache scoping.
    pub semantic_fingerprint: [u8; 32],
}

/// The MODULE.bazel directives we register so the Starlark evaluator collects
/// them as rule calls instead of erroring on an undefined function. We currently
/// act only on `module` and the supported subset of `bazel_dep`; every remaining
/// directive is recorded and rejected rather than silently ignored.
const MODULE_DIRECTIVES: &[&str] = &[
    "module",
    "bazel_dep",
    "use_extension",
    "use_repo_rule",
    "use_repo",
    "register_toolchains",
    "register_execution_platforms",
    "archive_override",
    "local_path_override",
    "single_version_override",
    "multiple_version_override",
    "git_override",
    "bazel_compatibility",
    "include",
];

/// Parse a MODULE.bazel file into its `bazel_dep()` declarations by evaluating
/// it through the shared Starlark evaluator (same mechanism as the WORKSPACE
/// loader) and collecting the rule calls.
pub fn parse_module_file(module_file: &Path) -> Result<ParsedModule> {
    let content = rush_incremental::tracked::read_to_string(module_file)
        .with_context(|| format!("bzlmod: read MODULE.bazel `{}`", module_file.display()))?;

    let lexer = Lexer::new(&content);
    let mut parser = Parser::new(lexer);
    let program = parser
        .parse()
        .with_context(|| format!("bzlmod: parse MODULE.bazel `{}`", module_file.display()))?;

    let mut evaluator = Evaluator::new();
    for directive in MODULE_DIRECTIVES {
        evaluator.register_rule_function((*directive).to_string());
    }
    evaluator
        .eval_program(&program)
        .with_context(|| format!("bzlmod: evaluate MODULE.bazel `{}`", module_file.display()))?;

    let string_kwarg = |kwargs: &HashMap<String, Value>, key: &str| -> Option<String> {
        match kwargs.get(key) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        }
    };

    let mut deps = Vec::new();
    let mut module_name = None;
    let mut module_version = None;
    let mut module_repo_name = None;
    let mut unsupported_semantics = Vec::new();
    let mut saw_module = false;
    for rule_call in evaluator.get_rule_calls() {
        let mut record = |detail: String| {
            unsupported_semantics.push(UnsupportedModuleSemantic {
                module_file: module_file.to_path_buf(),
                directive: rule_call.rule_type.clone(),
                detail,
            });
        };
        match rule_call.rule_type.as_str() {
            // The module's own self-declaration: `module(name=…, repo_name=…)`.
            // Inside the module, `@<repo_name>//…` (default <name>) self-refers.
            "module" => {
                if saw_module {
                    record("multiple module() declarations are unsupported".to_string());
                }
                saw_module = true;
                if !rule_call.args.is_empty() {
                    record("positional arguments are unsupported".to_string());
                }
                let supported: BTreeSet<&str> = ["name", "version", "repo_name"].into();
                let mut extra: Vec<&str> = rule_call
                    .kwargs
                    .keys()
                    .map(String::as_str)
                    .filter(|key| !supported.contains(key))
                    .collect();
                extra.sort_unstable();
                for key in extra {
                    record(format!("keyword `{key}` semantics are unsupported"));
                }
                module_name = string_kwarg(&rule_call.kwargs, "name");
                module_version = string_kwarg(&rule_call.kwargs, "version");
                module_repo_name = string_kwarg(&rule_call.kwargs, "repo_name")
                    .filter(|repo_name| !repo_name.is_empty());
                for key in ["name", "version", "repo_name"] {
                    if rule_call.kwargs.contains_key(key)
                        && string_kwarg(&rule_call.kwargs, key).is_none()
                    {
                        record(format!("keyword `{key}` must be a string"));
                    }
                }
                if module_name.is_none() {
                    record("module() requires a string `name`".to_string());
                }
            }
            "bazel_dep" => {
                if !rule_call.args.is_empty() {
                    record("positional arguments are unsupported".to_string());
                }
                let supported: BTreeSet<&str> = ["name", "version", "repo_name"].into();
                let mut extra: Vec<&str> = rule_call
                    .kwargs
                    .keys()
                    .map(String::as_str)
                    .filter(|key| !supported.contains(key))
                    .collect();
                extra.sort_unstable();
                for key in extra {
                    record(format!("keyword `{key}` semantics are unsupported"));
                }
                let Some(name) = string_kwarg(&rule_call.kwargs, "name") else {
                    record("bazel_dep() requires a string `name`".to_string());
                    continue;
                };
                for key in ["version", "repo_name"] {
                    if rule_call.kwargs.contains_key(key)
                        && string_kwarg(&rule_call.kwargs, key).is_none()
                    {
                        record(format!("keyword `{key}` must be a string"));
                    }
                }
                let version = string_kwarg(&rule_call.kwargs, "version").unwrap_or_default();
                let repo_name = string_kwarg(&rule_call.kwargs, "repo_name")
                    .filter(|repo_name| !repo_name.is_empty())
                    .unwrap_or_else(|| name.clone());
                deps.push(BazelDep {
                    name,
                    version,
                    repo_name,
                });
            }
            _ => record("directive semantics are unsupported".to_string()),
        }
    }
    unsupported_semantics.sort();
    unsupported_semantics.dedup();
    let self_repo_name = module_repo_name.or_else(|| module_name.clone());
    Ok(ParsedModule {
        module_name,
        module_version,
        self_repo_name,
        deps,
        unsupported_semantics,
    })
}

fn reject_unsupported_semantics(parsed: &ParsedModule) -> Result<()> {
    if parsed.unsupported_semantics.is_empty() {
        return Ok(());
    }
    let details = parsed
        .unsupported_semantics
        .iter()
        .map(|issue| {
            format!(
                "{}: {}: {}",
                issue.module_file.display(),
                issue.directive,
                issue.detail
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    anyhow::bail!("bzlmod: unsupported MODULE semantics (native wiring refused): {details}")
}

/// Locate a vendored module's repo root under the registry: prefer the
/// versioned dir `<registry>/<name>/<version>/`, fall back to a flat
/// `<registry>/<name>/`. Returns None if neither exists.
fn find_vendored(registry: &Path, name: &str, version: &str) -> Option<PathBuf> {
    let tracked_dir =
        |candidate: PathBuf| match rush_incremental::tracked::metadata_kind(&candidate) {
            Some(rush_incremental::FileKind::Dir) => Some(candidate),
            Some(rush_incremental::FileKind::Symlink) => {
                let resolved = rush_incremental::tracked::canonicalize(&candidate).ok()?;
                (rush_incremental::tracked::metadata_kind(&resolved)
                    == Some(rush_incremental::FileKind::Dir))
                .then_some(resolved)
            }
            _ => None,
        };

    let versioned = registry.join(name).join(version);
    if let Some(versioned) = tracked_dir(versioned) {
        return Some(versioned);
    }
    let flat = registry.join(name);
    tracked_dir(flat)
}

fn validate_registry_segment(value: &str, description: &str) -> Result<()> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.contains('/')
        || value.contains('\\')
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        anyhow::bail!("bzlmod: invalid {description} registry segment `{value}`");
    }
    Ok(())
}

/// Bazel module names are lower-case identifiers with `._-` separators. Keeping
/// this stricter than a generic URL segment prevents both local-registry path
/// traversal and accidental requests for names the BCR cannot contain.
fn validate_module_name(name: &str) -> Result<()> {
    validate_registry_segment(name, "module name")?;
    let bytes = name.as_bytes();
    let valid = bytes.len() <= 128
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        });
    if !valid {
        anyhow::bail!("bzlmod: invalid Bazel module name `{name}`");
    }
    Ok(())
}

/// Repository aliases accepted by the native subset must work with Rush's
/// Bazel 6.1 compatibility control as well as newer Bazel releases. Bazel 6.1's
/// `RepositoryName.validateUserProvidedRepoName` uses `[a-zA-Z][-.\\w]*$`:
/// an ASCII letter first, followed by ASCII letters/digits, `_`, `-`, or `.`.
/// Newer Bazel permits a leading digit, but accepting the intersection keeps a
/// graph produced by this resolver consumable by the 6.1 control.
fn validate_user_repo_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    let valid = bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if !valid {
        anyhow::bail!(
            "bzlmod: invalid user-provided repository name `{name}` for the Bazel 6.1 compatibility subset"
        );
    }
    Ok(())
}

/// Validate one MODULE's apparent-repository namespace before resolving any of
/// its dependencies. Bazel assigns the module's own repo name and each visible
/// `bazel_dep` name in one namespace and rejects a second claim, even when two
/// declarations request the same module/version.
fn validate_module_repo_names(parsed: &ParsedModule, module_file: &Path) -> Result<()> {
    let mut claims: BTreeMap<String, String> = BTreeMap::new();
    let mut claim = |repo_name: &str, description: String| -> Result<()> {
        validate_user_repo_name(repo_name).with_context(|| {
            format!(
                "bzlmod: invalid user-provided repository name `{repo_name}` ({description}) in {}",
                module_file.display()
            )
        })?;
        if let Some(previous) = claims.insert(repo_name.to_string(), description.clone()) {
            anyhow::bail!(
                "bzlmod: apparent repository name `{repo_name}` is claimed both {previous} and {description} in {}; Bazel requires each MODULE repository name to be unique",
                module_file.display()
            );
        }
        Ok(())
    };

    if let Some(self_repo_name) = parsed.self_repo_name.as_deref() {
        claim(self_repo_name, "as the module's own repository".to_string())?;
    }
    for dep in &parsed.deps {
        claim(
            &dep.repo_name,
            format!("by bazel_dep(name = \"{}\")", dep.name),
        )?;
    }
    Ok(())
}

fn valid_version_identifier(
    identifier: &str,
    allow_hyphen: bool,
    numeric_must_fit_u64: bool,
) -> bool {
    !identifier.is_empty()
        && identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || (allow_hyphen && byte == b'-'))
        && (!numeric_must_fit_u64
            || !identifier.bytes().all(|byte| byte.is_ascii_digit())
            || identifier.parse::<u64>().is_ok())
}

fn valid_version_identifier_sequence(
    value: &str,
    allow_hyphen: bool,
    numeric_must_fit_u64: bool,
) -> bool {
    value
        .split('.')
        .all(|identifier| valid_version_identifier(identifier, allow_hyphen, numeric_must_fit_u64))
}

/// Bazel's relaxed SemVer is `RELEASE[-PRERELEASE][+BUILD]`: release
/// identifiers are ASCII alphanumeric, prerelease/build identifiers may also
/// contain hyphens, and every dot-separated identifier is non-empty. Bazel
/// rejects numeric release/prerelease identifiers that do not fit in u64.
fn normalize_module_version(version: &str) -> Result<&str> {
    validate_registry_segment(version, "module version")?;

    let (without_build, build) = match version.split_once('+') {
        Some((without_build, build)) if !build.contains('+') => (without_build, Some(build)),
        Some(_) => ("", None),
        None => (version, None),
    };
    let (release, prerelease) = match without_build.split_once('-') {
        Some((release, prerelease)) => (release, Some(prerelease)),
        None => (without_build, None),
    };

    let valid = valid_version_identifier_sequence(release, false, true)
        && prerelease.is_none_or(|value| valid_version_identifier_sequence(value, true, true))
        && build.is_none_or(|value| valid_version_identifier_sequence(value, true, false));
    if !valid {
        anyhow::bail!("bzlmod: invalid Bazel module version `{version}`");
    }
    // Bazel accepts +BUILD as input but deliberately strips it before comparing
    // versions or constructing registry requests.
    Ok(without_build)
}

/// Decode standard-alphabet base64 (A-Za-z0-9+/, `=` padding) into bytes.
/// Self-contained so Bzlmod needs no base64 crate dependency; used only to turn
/// a BCR `integrity` SRI into the hex sha256 `fetch_http_archive` expects.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s = s.trim();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = val(c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Convert a Bazel BCR `integrity` field (`"sha256-<base64-of-32-bytes>"`) into a
/// lowercase hex sha256 string (the form `RepositoryManager::fetch_http_archive`
/// verifies against). Returns None for non-sha256 or malformed integrity.
fn sri_to_hex(integrity: &str) -> Option<String> {
    let b64 = integrity.trim().strip_prefix("sha256-")?;
    let bytes = base64_decode(b64)?;
    if bytes.len() != 32 {
        return None;
    }
    Some(hex::encode(bytes))
}

fn verify_sha256_sri(content: &[u8], integrity: &str, description: &str) -> Result<()> {
    use sha2::{Digest, Sha256};

    let expected = sri_to_hex(integrity).with_context(|| {
        format!("bzlmod: unsupported integrity `{integrity}` for {description}")
    })?;
    let actual = hex::encode(Sha256::digest(content));
    if actual != expected {
        anyhow::bail!(
            "bzlmod: SHA256 mismatch for {description}: expected {expected}, got {actual}"
        );
    }
    Ok(())
}

fn bcr_registry_url(registry_url: &str, segments: &[&str]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(registry_url)
        .with_context(|| format!("bzlmod: invalid registry URL `{registry_url}`"))?;
    {
        let mut path = url.path_segments_mut().map_err(|_| {
            anyhow::anyhow!("bzlmod: registry URL `{registry_url}` cannot contain path segments")
        })?;
        path.pop_if_empty();
        path.extend(segments.iter().copied());
    }
    Ok(url)
}

fn patch_name_segments(name: &str) -> Result<Vec<&str>> {
    let path = Path::new(name);
    if path.is_absolute() || name.is_empty() {
        anyhow::bail!("bzlmod: unsafe BCR patch name `{name}`");
    }

    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(segment) => {
                let segment = segment.to_str().with_context(|| {
                    format!("bzlmod: BCR patch name `{name}` is not valid UTF-8")
                })?;
                if segment.is_empty() {
                    anyhow::bail!("bzlmod: unsafe BCR patch name `{name}`");
                }
                segments.push(segment);
            }
            _ => anyhow::bail!("bzlmod: unsafe BCR patch name `{name}`"),
        }
    }
    if segments.is_empty() {
        anyhow::bail!("bzlmod: unsafe BCR patch name `{name}`");
    }
    Ok(segments)
}

async fn download_bcr_patches(
    client: &reqwest::Client,
    registry_url: &str,
    name: &str,
    version: &str,
    patches: &[BcrPatch],
) -> Result<Vec<Vec<u8>>> {
    let mut contents = Vec::with_capacity(patches.len());
    for patch in patches {
        let patch_segments = patch_name_segments(&patch.name)?;
        let mut url_segments = vec!["modules", name, version, "patches"];
        url_segments.extend(patch_segments);
        let url = bcr_registry_url(registry_url, &url_segments)?;
        let bytes = client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("bzlmod: download BCR patch `{}` from {url}", patch.name))?
            .error_for_status()
            .with_context(|| format!("bzlmod: download BCR patch `{}` from {url}", patch.name))?
            .bytes()
            .await
            .with_context(|| format!("bzlmod: read BCR patch `{}` from {url}", patch.name))?;
        verify_sha256_sri(
            &bytes,
            &patch.integrity,
            &format!("BCR patch {} for {}@{}", patch.name, name, version),
        )?;
        contents.push(bytes.to_vec());
    }
    Ok(contents)
}

fn stripped_patch_path(
    header_path: &str,
    strip: usize,
    patch_name: &str,
) -> Result<Option<PathBuf>> {
    if header_path == "/dev/null" {
        return Ok(None);
    }
    if header_path.starts_with('"') || Path::new(header_path).is_absolute() {
        anyhow::bail!(
            "bzlmod: BCR patch `{patch_name}` contains unsupported or unsafe path `{header_path}`"
        );
    }

    let components: Vec<&OsStr> = Path::new(header_path)
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => Ok(value),
            _ => Err(anyhow::anyhow!(
                "bzlmod: BCR patch `{patch_name}` contains unsafe path `{header_path}`"
            )),
        })
        .collect::<Result<_>>()?;
    if components.len() <= strip {
        anyhow::bail!(
            "bzlmod: BCR patch `{patch_name}` path `{header_path}` has only {} component(s), less than or equal to patch_strip={strip}",
            components.len()
        );
    }

    let mut relative = PathBuf::new();
    for component in &components[strip..] {
        relative.push(component);
    }
    Ok(Some(relative))
}

fn validate_patch_paths(
    repository: &Path,
    patch_name: &str,
    patch: &[u8],
    strip: usize,
) -> Result<()> {
    let text = std::str::from_utf8(patch)
        .with_context(|| format!("bzlmod: BCR patch `{patch_name}` is not UTF-8 text"))?;
    let mut target_count = 0usize;

    for line in text.lines() {
        let Some(header) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
        else {
            continue;
        };
        let header_path = header.split('\t').next().unwrap_or(header);
        let Some(relative) = stripped_patch_path(header_path, strip, patch_name)? else {
            continue;
        };
        target_count += 1;

        // A normal relative path cannot escape unless an archive supplied a
        // symlink in one of its ancestors. Reject that case before invoking the
        // host patch tool rather than trusting platform-specific traversal
        // behavior.
        let mut cursor = repository.to_path_buf();
        for component in relative.components() {
            cursor.push(component.as_os_str());
            match std::fs::symlink_metadata(&cursor) {
                Ok(metadata) if metadata.file_type().is_symlink() => anyhow::bail!(
                    "bzlmod: BCR patch `{patch_name}` target `{}` traverses symlink `{}`",
                    relative.display(),
                    cursor.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "bzlmod: inspect BCR patch `{patch_name}` target `{}`",
                            cursor.display()
                        )
                    })
                }
            }
        }
    }

    if target_count == 0 {
        anyhow::bail!("bzlmod: BCR patch `{patch_name}` contains no unified-diff paths");
    }
    Ok(())
}

async fn apply_bcr_patches(
    repository: &Path,
    patches: &[BcrPatch],
    patch_contents: &[Vec<u8>],
    strip: usize,
) -> Result<()> {
    if patches.len() != patch_contents.len() {
        anyhow::bail!("bzlmod: internal patch metadata/content count mismatch");
    }

    for (patch, content) in patches.iter().zip(patch_contents) {
        validate_patch_paths(repository, &patch.name, content, strip)?;
        let patch_file = tempfile::Builder::new()
            .prefix("rush-bcr-patch-")
            .tempfile()
            .with_context(|| format!("bzlmod: create temporary file for `{}`", patch.name))?;
        std::fs::write(patch_file.path(), content)
            .with_context(|| format!("bzlmod: write temporary BCR patch `{}`", patch.name))?;

        let output = tokio::process::Command::new("patch")
            .arg(format!("-p{strip}"))
            .arg("--fuzz=0")
            .arg("--ignore-whitespace")
            .arg("-f")
            .arg("-i")
            .arg(patch_file.path())
            .current_dir(repository)
            .output()
            .await
            .with_context(|| format!("bzlmod: execute host patch tool for `{}`", patch.name))?;
        if !output.status.success() {
            anyhow::bail!(
                "bzlmod: applying BCR patch `{}` failed with status {:?}: stdout: {}; stderr: {}",
                patch.name,
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(())
}

fn source_marker_bytes(name: &str, version: &str, source: &BcrArchiveSource) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(&BcrSourceMarker {
        module: name,
        version,
        source_integrity: &source.integrity,
        strip_prefix: source.strip_prefix.as_deref(),
        patch_strip: source.patch_strip,
        patches: &source.patches,
    })
    .context("bzlmod: serialize BCR source marker")
}

fn safe_cache_component(value: &str) -> String {
    value
        .chars()
        .take(64)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn bcr_cache_name(name: &str, version: &str, marker: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let digest = hex::encode(Sha256::digest(marker));
    format!(
        "bzlmod-{}-{}-{}",
        safe_cache_component(name),
        safe_cache_component(version),
        &digest[..16]
    )
}

fn marker_matches(repository: &Path, expected: &[u8]) -> bool {
    let path = repository.join(BCR_SOURCE_MARKER);
    let Ok(metadata) = std::fs::symlink_metadata(&path) else {
        return false;
    };
    if !metadata.file_type().is_file() {
        return false;
    }
    std::fs::read(path)
        .map(|actual| actual == expected)
        .unwrap_or(false)
}

fn write_source_marker(repository: &Path, marker: &[u8]) -> Result<()> {
    let path = repository.join(BCR_SOURCE_MARKER);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| {
            format!(
                "bzlmod: create new BCR source marker without following an archive entry at {}",
                path.display()
            )
        })?;
    std::io::Write::write_all(&mut file, marker)
        .with_context(|| format!("bzlmod: write BCR source marker at {}", path.display()))
}

/// Decide whether an existing verified-metadata-keyed repository can be reused.
///
/// This is an explicit immutable-cache trust boundary: the marker fingerprints
/// the verified source archive and ordered patch metadata, but reuse does not
/// re-hash the complete extracted tree on every analysis. A missing or changed
/// marker therefore fails closed; callers must treat a matching published cache
/// directory as immutable, like the existing RepositoryManager archive cache.
fn cached_repository_is_reusable(repository: &Path, expected_marker: &[u8]) -> Result<bool> {
    if marker_matches(repository, expected_marker) {
        return Ok(true);
    }
    if repository.exists() {
        anyhow::bail!(
            "bzlmod: cached BCR repository {} exists without the expected source marker",
            repository.display()
        );
    }
    Ok(false)
}

/// Fresh, atomically-created parent for one BCR materialization attempt.
///
/// RepositoryManager treats any existing destination directory as a cache hit,
/// so it must never receive the path of a stale staging directory. `tempdir_in`
/// creates a random parent with mkdir/create-new semantics; RepositoryManager
/// writes a previously-nonexistent child beneath it. A crash can strand an old
/// parent, but a later attempt gets a different random parent and cannot reuse
/// the stranded child. TempDir removes the parent and partial child normally.
struct BcrStaging {
    _parent: tempfile::TempDir,
    archive_name: String,
    repository: PathBuf,
}

fn create_bcr_staging(external_dir: &Path, cache_name: &str) -> Result<BcrStaging> {
    std::fs::create_dir_all(external_dir).with_context(|| {
        format!(
            "bzlmod: create external repository directory {}",
            external_dir.display()
        )
    })?;
    let prefix = format!(".{cache_name}-staging-");
    let parent = tempfile::Builder::new()
        .prefix(&prefix)
        .tempdir_in(external_dir)
        .context("bzlmod: create unique BCR staging parent")?;
    let parent_name = parent
        .path()
        .file_name()
        .and_then(OsStr::to_str)
        .context("bzlmod: staging parent name is not valid UTF-8")?;
    let relative_repository = Path::new(parent_name).join("repository");
    let archive_name = relative_repository
        .to_str()
        .context("bzlmod: staging repository name is not valid UTF-8")?
        .to_string();
    let repository = external_dir.join(&relative_repository);
    debug_assert!(!repository.exists());
    Ok(BcrStaging {
        _parent: parent,
        archive_name,
        repository,
    })
}

fn parse_bcr_archive_source(
    name: &str,
    version: &str,
    source_json: &str,
) -> Result<BcrArchiveSource> {
    let raw: serde_json::Value = serde_json::from_str(source_json)
        .with_context(|| format!("bzlmod: parse source.json for {name}@{version}"))?;
    match raw.get("type") {
        None => {}
        Some(serde_json::Value::String(source_type)) if source_type == "archive" => {}
        Some(serde_json::Value::String(source_type)) => anyhow::bail!(
            "bzlmod: module {name}@{version} uses source type `{source_type}`, which remains UNFIXED; only the default `archive` type is supported"
        ),
        Some(source_type) => anyhow::bail!(
            "bzlmod: module {name}@{version} has invalid source type {source_type}; only the default `archive` type is supported"
        ),
    }
    if let Some(archive_type) = raw.get("archive_type") {
        anyhow::bail!(
            "bzlmod: module {name}@{version} specifies archive_type={archive_type}, which remains UNFIXED"
        );
    }

    let source: BcrArchiveSource = serde_json::from_str(source_json)
        .with_context(|| format!("bzlmod: parse archive source.json for {name}@{version}"))?;
    if !source.overlay.is_empty() {
        anyhow::bail!(
            "bzlmod: module {name}@{version} declares BCR overlays, which remain UNFIXED"
        );
    }
    Ok(source)
}

/// Fetch a module from a Bazel Central Registry: read
/// `<registry_url>/modules/<name>/<version>/source.json`, synthesize an
/// `HttpArchive` from its `url` + `integrity` (+ `strip_prefix`), download and
/// integrity-check the registry's ordered patches, and materialize the complete
/// source tree under a verified-metadata-keyed path. A staging repository is
/// renamed into place only after every patch succeeds, so an interrupted or
/// rejected patch cannot be admitted as a resolved module.
async fn bcr_fetch_async(
    registry_url: &str,
    name: &str,
    version: &str,
    workspace_root: &Path,
) -> Result<PathBuf> {
    validate_module_name(name)?;
    let version = normalize_module_version(version)?;
    let client = reqwest::Client::new();
    let source_url = bcr_registry_url(registry_url, &["modules", name, version, "source.json"])?;
    let source_json = client
        .get(source_url.clone())
        .send()
        .await
        .with_context(|| format!("bzlmod: download source.json for {name}@{version}"))?
        .error_for_status()
        .with_context(|| format!("bzlmod: download source.json for {name}@{version}"))?
        .text()
        .await
        .with_context(|| format!("bzlmod: read source.json for {name}@{version}"))?;
    let source = parse_bcr_archive_source(name, version, &source_json)?;
    let sha256 = sri_to_hex(&source.integrity)
        .with_context(|| format!("bzlmod: unsupported integrity `{}`", source.integrity))?;
    let marker = source_marker_bytes(name, version, &source)?;
    let cache_name = bcr_cache_name(name, version, &marker);
    let external_dir = crate::cache_paths::workspace_external_dir(workspace_root);
    let final_repository = external_dir.join(&cache_name);
    if cached_repository_is_reusable(&final_repository, &marker)? {
        return Ok(final_repository);
    }
    let patch_contents =
        download_bcr_patches(&client, registry_url, name, version, &source.patches).await?;
    let staging = create_bcr_staging(&external_dir, &cache_name)?;

    let archive = HttpArchive {
        name: staging.archive_name.clone(),
        urls: vec![source.url.clone()],
        sha256: Some(sha256),
        strip_prefix: source.strip_prefix.clone(),
        build_file: None,
        build_file_content: None,
        repo_mapping: HashMap::new(),
    };
    let repo_mgr = RepositoryManager::new(Arc::new(SystemPackageMapper::new()));
    let materialized = repo_mgr
        .fetch_http_archive(workspace_root, &archive)
        .await
        .with_context(|| format!("bzlmod: fetch BCR source {name}@{version}"))?;
    if materialized != staging.repository {
        anyhow::bail!(
            "bzlmod: RepositoryManager materialized {} outside reserved staging child {}",
            materialized.display(),
            staging.repository.display()
        );
    }

    let finish = async {
        apply_bcr_patches(
            &materialized,
            &source.patches,
            &patch_contents,
            source.patch_strip,
        )
        .await?;
        write_source_marker(&materialized, &marker)
            .with_context(|| format!("bzlmod: seal BCR source marker for {name}@{version}"))?;
        std::fs::rename(&materialized, &final_repository).with_context(|| {
            format!(
                "bzlmod: publish BCR repository {} as {}",
                materialized.display(),
                final_repository.display()
            )
        })?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(error) = finish {
        if marker_matches(&final_repository, &marker) {
            return Ok(final_repository);
        }
        return Err(error);
    }

    Ok(final_repository)
}

/// Synchronous wrapper around [`bcr_fetch_async`] for the (sync) resolver.
/// Network fetch requires a Tokio multi-thread runtime because this bridge uses
/// `block_in_place`. Missing/current-thread runtimes are explicit errors rather
/// than silently degrading to an incomplete graph; callers requesting hermetic
/// vendored-only resolution must bypass this function via RUSH_BZLMOD_NO_FETCH.
fn bcr_fetch(
    registry_url: &str,
    name: &str,
    version: &str,
    workspace_root: &Path,
) -> Result<PathBuf> {
    let handle = tokio::runtime::Handle::try_current().context(
        "bzlmod: BCR fetch requires a Tokio multi-thread runtime; set RUSH_BZLMOD_NO_FETCH=1 for vendored-only resolution",
    )?;
    if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
        anyhow::bail!(
            "bzlmod: BCR fetch cannot run on a Tokio current-thread runtime; set RUSH_BZLMOD_NO_FETCH=1 for vendored-only resolution"
        );
    }
    tokio::task::block_in_place(|| {
        handle.block_on(bcr_fetch_async(registry_url, name, version, workspace_root))
    })
}

fn fingerprint_field(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn fingerprint_path(hasher: &mut blake3::Hasher, path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        fingerprint_field(hasher, path.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    {
        fingerprint_field(hasher, path.to_string_lossy().as_bytes());
    }
}

fn fingerprint_optional_string(hasher: &mut blake3::Hasher, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            fingerprint_field(hasher, value.as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn normalized_version_for_fingerprint(version: &str) -> Result<&str> {
    if version.is_empty() {
        Ok("")
    } else {
        normalize_module_version(version)
    }
}

/// Hash the normalized resolver inputs and output with an explicit stable
/// encoding. MODULE declaration order and HashMap iteration order do not affect
/// this value; comments and formatting are intentionally not resolution
/// semantics. File-content freshness remains represented by the tracked manifest.
fn resolution_semantic_fingerprint(
    registry: &Path,
    registry_url: &str,
    no_fetch: bool,
    parsed_modules: &[(PathBuf, ParsedModule)],
    repos: &HashMap<String, PathBuf>,
    repo_mappings: &HashMap<String, HashMap<String, String>>,
) -> Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rush.bzlmod.semantic-resolution.v1\n");

    fingerprint_path(&mut hasher, registry);
    fingerprint_field(&mut hasher, registry_url.as_bytes());
    hasher.update(&[u8::from(no_fetch)]);

    let mut modules: Vec<_> = parsed_modules.iter().collect();
    modules.sort_by(|(path_a, _), (path_b, _)| path_a.as_os_str().cmp(path_b.as_os_str()));
    hasher.update(&(modules.len() as u64).to_le_bytes());
    for (path, module) in modules {
        fingerprint_path(&mut hasher, path);
        fingerprint_optional_string(&mut hasher, module.module_name.as_deref());
        let module_version = module
            .module_version
            .as_deref()
            .map(normalized_version_for_fingerprint)
            .transpose()?;
        fingerprint_optional_string(&mut hasher, module_version);
        fingerprint_optional_string(&mut hasher, module.self_repo_name.as_deref());

        let mut deps: Vec<_> = module
            .deps
            .iter()
            .map(|dep| {
                Ok::<_, anyhow::Error>((
                    dep.name.as_str(),
                    normalized_version_for_fingerprint(&dep.version)?,
                    dep.repo_name.as_str(),
                ))
            })
            .collect::<Result<_>>()?;
        deps.sort_unstable();
        hasher.update(&(deps.len() as u64).to_le_bytes());
        for (name, version, repo_name) in deps {
            fingerprint_field(&mut hasher, name.as_bytes());
            fingerprint_field(&mut hasher, version.as_bytes());
            fingerprint_field(&mut hasher, repo_name.as_bytes());
        }
    }

    let repos: BTreeMap<_, _> = repos.iter().collect();
    hasher.update(&(repos.len() as u64).to_le_bytes());
    for (name, path) in repos {
        fingerprint_field(&mut hasher, name.as_bytes());
        fingerprint_path(&mut hasher, path);
    }

    let mappings: BTreeMap<_, _> = repo_mappings.iter().collect();
    hasher.update(&(mappings.len() as u64).to_le_bytes());
    for (owner, mapping) in mappings {
        fingerprint_field(&mut hasher, owner.as_bytes());
        let mapping: BTreeMap<_, _> = mapping.iter().collect();
        hasher.update(&(mapping.len() as u64).to_le_bytes());
        for (apparent, canonical) in mapping {
            fingerprint_field(&mut hasher, apparent.as_bytes());
            fingerprint_field(&mut hasher, canonical.as_bytes());
        }
    }

    Ok(*hasher.finalize().as_bytes())
}

fn register_repo(repos: &mut HashMap<String, PathBuf>, name: &str, path: &Path) -> Result<()> {
    if let Some(existing) = repos.get(name) {
        if existing != path {
            anyhow::bail!(
                "bzlmod: apparent repository `{name}` maps to both {} and {}; scoped repository mappings remain unsupported",
                existing.display(),
                path.display()
            );
        }
        return Ok(());
    }
    repos.insert(name.to_string(), path.to_path_buf());
    Ok(())
}

#[derive(Debug, Clone)]
struct SelectedModule {
    version: String,
    repo_name: String,
    declared_from: PathBuf,
}

/// Resolve a workspace's MODULE.bazel into a [`BzlmodResolution`] over the full
/// TRANSITIVE bazel_dep closure: each resolved module's own MODULE.bazel is
/// parsed for further bazel_deps (BFS).
///
/// This prerequisite resolver is deliberately strict. MVS, scoped repository
/// mappings, module extensions/overrides, and other unimplemented semantics are
/// errors; missing modules and transitive parse failures are errors too. A caller
/// therefore receives either one complete graph within the documented subset or
/// no graph at all. The function must run inside `tracked::scope`: root and
/// transitive MODULE content, candidate presence/type, and every
/// `RUSH_BZLMOD_*` environment observation enter that command's manifest.
///
/// The native result is not yet wired into production MODULE-only analysis; the
/// default MODULE-only path remains the Bazel-backed resolver.
pub fn resolve(workspace_root: &Path) -> Result<BzlmodResolution> {
    let module_file = workspace_root.join("MODULE.bazel");
    let registry = rush_incremental::tracked::getenv_str("RUSH_BZLMOD_REGISTRY")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("bzlmod_modules"));
    let registry = if registry.is_absolute() {
        registry
    } else {
        workspace_root.join(registry)
    };
    let registry_url = rush_incremental::tracked::getenv_str("RUSH_BZLMOD_REGISTRY_URL")
        .unwrap_or_else(|| DEFAULT_BCR_URL.to_string());
    // RUSH_BZLMOD_NO_FETCH forces vendored-only (offline / hermetic) resolution.
    // Presence, not the textual value, is the established behavior.
    let no_fetch = rush_incremental::tracked::getenv(b"RUSH_BZLMOD_NO_FETCH").is_some();

    let mut repos = HashMap::new();
    let root_module = parse_module_file(&module_file)?;
    reject_unsupported_semantics(&root_module)?;
    validate_module_repo_names(&root_module, &module_file)?;
    if let Some(name) = root_module.module_name.as_deref() {
        validate_module_name(name)
            .with_context(|| format!("bzlmod: validate root module name `{name}`"))?;
    }
    if let Some(version) = root_module.module_version.as_deref() {
        if !version.is_empty() {
            normalize_module_version(version)
                .with_context(|| format!("bzlmod: validate root module version `{version}`"))?;
        }
    }

    let mut parsed_modules = vec![(module_file.clone(), root_module.clone())];
    let mut selected: BTreeMap<String, SelectedModule> = BTreeMap::new();
    let mut queue: VecDeque<(BazelDep, PathBuf, bool)> = root_module
        .deps
        .into_iter()
        .map(|dep| (dep, module_file.clone(), true))
        .collect();
    let mut considered = 0usize;

    while let Some((dep, declared_from, root_import)) = queue.pop_front() {
        considered += 1;
        validate_module_name(&dep.name)
            .with_context(|| format!("bzlmod: validate bazel_dep module name `{}`", dep.name))?;
        let normalized_version = if dep.version.is_empty() {
            dep.version.as_str()
        } else {
            normalize_module_version(&dep.version).with_context(|| {
                format!(
                    "bzlmod: validate version `{}` for module `{}`",
                    dep.version, dep.name
                )
            })?
        };
        if !root_import && dep.repo_name != dep.name {
            anyhow::bail!(
                "bzlmod: transitive bazel_dep `{}` in {} uses repo_name=`{}`; scoped repository mappings remain unsupported",
                dep.name,
                declared_from.display(),
                dep.repo_name
            );
        }
        if let Some(existing) = selected.get(&dep.name) {
            if existing.version != normalized_version {
                anyhow::bail!(
                    "bzlmod: module `{}` is requested at both `{}` (from {}) and `{}` (from {}); MVS remains unsupported",
                    dep.name,
                    existing.version,
                    existing.declared_from.display(),
                    normalized_version,
                    declared_from.display()
                );
            }
            if existing.repo_name != dep.repo_name {
                anyhow::bail!(
                    "bzlmod: module `{}` is imported as both `{}` and `{}`; scoped repository mappings remain unsupported",
                    dep.name,
                    existing.repo_name,
                    dep.repo_name
                );
            }
            continue;
        }
        selected.insert(
            dep.name.clone(),
            SelectedModule {
                version: normalized_version.to_string(),
                repo_name: dep.repo_name.clone(),
                declared_from: declared_from.clone(),
            },
        );
        // Prefer a vendored copy; otherwise fetch from the Bazel Central
        // Registry. A versionless bazel_dep can't be BCR-fetched (no version to
        // request), and RUSH_BZLMOD_NO_FETCH forces vendored-only.
        let path = match find_vendored(&registry, &dep.name, normalized_version) {
            Some(p) => p,
            None if !no_fetch && !dep.version.is_empty() => {
                match bcr_fetch(&registry_url, &dep.name, normalized_version, workspace_root) {
                    Ok(p) => p,
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "bzlmod: materialize bazel_dep(name=\"{}\", version=\"{}\")",
                                dep.name, dep.version
                            )
                        })
                    }
                }
            }
            None => {
                anyhow::bail!(
                    "bzlmod: no module for bazel_dep(name=\"{}\", version=\"{}\"): neither tracked vendored candidate exists under {}, and fetch is disabled or the dependency is unversioned",
                    dep.name,
                    dep.version,
                    registry.display()
                );
            }
        };
        // Register under the apparent repo name the IMPORTER uses (no canonical
        // @@name~version yet). The existing exact-match label resolver then finds
        // `@<repo_name>//…` with zero downstream changes.
        register_repo(&mut repos, &dep.repo_name, &path)?;

        // Recurse: parse this module's own MODULE.bazel for transitive deps AND
        // its SELF repo_name. A module's BUILD/.bzl self-refer as
        // `@<own_repo_name>//…` (e.g. protobuf declares repo_name=
        // "com_google_protobuf" and loads @com_google_protobuf//bazel:...), so we
        // must also register the module under its own repo_name or those loads
        // 404 against a stub.
        let dep_module = path.join("MODULE.bazel");
        match rush_incremental::tracked::metadata_kind(&dep_module) {
            Some(rush_incremental::FileKind::File) => {}
            Some(kind) => anyhow::bail!(
                "bzlmod: transitive MODULE path {} is not a regular file (observed {:?})",
                dep_module.display(),
                kind
            ),
            None => anyhow::bail!(
                "bzlmod: resolved module {}@{} at {} has no MODULE.bazel; refusing an incomplete transitive graph",
                dep.name,
                normalized_version,
                path.display()
            ),
        }
        let parsed = parse_module_file(&dep_module).with_context(|| {
            format!(
                "bzlmod: parse transitive module {}@{} from {}",
                dep.name,
                normalized_version,
                dep_module.display()
            )
        })?;
        reject_unsupported_semantics(&parsed)?;
        validate_module_repo_names(&parsed, &dep_module)?;
        let declared_name = parsed.module_name.as_deref().with_context(|| {
            format!(
                "bzlmod: transitive {} must declare module(name = \"{}\")",
                dep_module.display(),
                dep.name
            )
        })?;
        validate_module_name(declared_name)?;
        if declared_name != dep.name {
            anyhow::bail!(
                "bzlmod: selected {}@{} but {} declares module name `{}`",
                dep.name,
                normalized_version,
                dep_module.display(),
                declared_name
            );
        }
        if let Some(declared_version) = parsed.module_version.as_deref() {
            if !declared_version.is_empty()
                && normalize_module_version(declared_version)? != normalized_version
            {
                anyhow::bail!(
                    "bzlmod: selected {}@{} but {} declares version `{}`",
                    dep.name,
                    normalized_version,
                    dep_module.display(),
                    declared_version
                );
            }
        }
        if let Some(self_name) = &parsed.self_repo_name {
            if self_name != &dep.repo_name {
                register_repo(&mut repos, self_name, &path)?;
            }
        }
        for transitive in &parsed.deps {
            queue.push_back((transitive.clone(), dep_module.clone(), false));
        }
        parsed_modules.push((dep_module, parsed));
    }

    let repo_mappings = HashMap::new();
    let semantic_fingerprint = resolution_semantic_fingerprint(
        &registry,
        &registry_url,
        no_fetch,
        &parsed_modules,
        &repos,
        &repo_mappings,
    )?;

    eprintln!(
        "📦 bzlmod: resolved {} repo name(s) (transitive closure: {} dependency declarations considered) from {} (registry {}, semantic fingerprint {})",
        repos.len(),
        considered,
        module_file.display(),
        registry.display(),
        hex::encode(semantic_fingerprint)
    );

    Ok(BzlmodResolution {
        repos,
        repo_mappings,
        semantic_fingerprint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_incremental::{Dependency, Staleness, TrackedIo as _};
    use std::io::Write;

    fn test_semantics(root: &Path, env: &[(&str, &str)]) -> rush_command_core::CommandSemantics {
        use bytes::Bytes;
        use rush_command_core::CommandSemantics;
        use rush_protocol::command::v1::EnvironmentVariable;
        use rush_protocol::command::v1::RunRequest;
        use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};

        let req = RunRequest {
            auth_cookie: Bytes::new(),
            argv: vec![
                Bytes::from_static(b"rush"),
                Bytes::from_static(b"build"),
                Bytes::from_static(b"//foo:bar"),
            ],
            working_directory: Bytes::from(root.display().to_string()),
            environment: env
                .iter()
                .map(|(name, value)| EnvironmentVariable {
                    name: Bytes::copy_from_slice(name.as_bytes()),
                    value: Bytes::copy_from_slice(value.as_bytes()),
                })
                .collect(),
            block_for_lock: true,
            client_build_id: "bzlmod-test".to_string(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            workspace_config: Vec::new(),
        };
        CommandSemantics::from_run_request(&req).expect("build semantics")
    }

    /// Run a resolver operation in one real command scope and retain the IO so
    /// tests can inspect/validate the exact manifest that future daemon wiring
    /// will persist.
    fn with_tracked_scope_env<R>(
        root: &Path,
        env: &[(&str, &str)],
        f: impl FnOnce() -> R,
    ) -> (
        R,
        Arc<rush_incremental::RealTrackedIo>,
        rush_command_core::CommandSemantics,
    ) {
        let semantics = test_semantics(root, env);
        let io = Arc::new(rush_incremental::RealTrackedIo::new(
            semantics.clone(),
            root,
        ));
        let shared: rush_incremental::tracked::SharedTrackedIo = io.clone();
        let result = rush_incremental::tracked::scope(shared, f);
        (result, io, semantics)
    }

    fn with_tracked_scope<R>(root: &Path, f: impl FnOnce() -> R) -> R {
        with_tracked_scope_env(root, &[], f).0
    }

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    fn base64_encode(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let first = chunk[0];
            let second = chunk.get(1).copied().unwrap_or(0);
            let third = chunk.get(2).copied().unwrap_or(0);
            encoded.push(TABLE[(first >> 2) as usize] as char);
            encoded.push(TABLE[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
            if chunk.len() > 1 {
                encoded.push(TABLE[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
            } else {
                encoded.push('=');
            }
            if chunk.len() > 2 {
                encoded.push(TABLE[(third & 0x3f) as usize] as char);
            } else {
                encoded.push('=');
            }
        }
        encoded
    }

    fn sha256_sri(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};

        format!("sha256-{}", base64_encode(&Sha256::digest(bytes)))
    }

    #[test]
    fn parses_bazel_dep() {
        let dir = std::env::temp_dir().join(format!("bzlmod_test_parse_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mf = write(
            &dir,
            "MODULE.bazel",
            "module(name = \"t\", version = \"0.1\")\n\
             bazel_dep(name = \"rules_cc\", version = \"0.0.9\")\n\
             bazel_dep(name = \"platforms\", version = \"0.0.8\", repo_name = \"my_platforms\")\n",
        );
        let parsed = with_tracked_scope(&dir, || parse_module_file(&mf)).unwrap();
        assert_eq!(parsed.module_name.as_deref(), Some("t"));
        let deps = parsed.deps;
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].name, "rules_cc");
        assert_eq!(deps[0].version, "0.0.9");
        assert_eq!(deps[0].repo_name, "rules_cc");
        assert_eq!(deps[1].name, "platforms");
        assert_eq!(deps[1].repo_name, "my_platforms");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_against_vendored_registry() {
        let dir = std::env::temp_dir().join(format!("bzlmod_test_resolve_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write(
            &dir,
            "MODULE.bazel",
            "module(name = \"t\", version = \"0\")\nbazel_dep(name = \"rules_cc\", version = \"0.0.9+local\")\n",
        );
        // vendored module dir: <ws>/bzlmod_modules/rules_cc/0.0.9/
        write(
            &dir,
            "bzlmod_modules/rules_cc/0.0.9/MODULE.bazel",
            "module(name = \"rules_cc\", version = \"0.0.9\")\n",
        );
        write(&dir, "bzlmod_modules/rules_cc/0.0.9/BUILD", "# rules_cc\n");
        let res = with_tracked_scope(&dir, || resolve(&dir)).unwrap();
        assert_eq!(res.repos.len(), 1);
        assert!(res
            .repos
            .get("rules_cc")
            .unwrap()
            .ends_with("rules_cc/0.0.9"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_unversioned_module_fails_closed() {
        let dir = std::env::temp_dir().join(format!("bzlmod_test_missing_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write(
            &dir,
            "MODULE.bazel",
            "bazel_dep(name = \"nonexistent_mod\")\n",
        );
        let error = with_tracked_scope(&dir, || resolve(&dir))
            .unwrap_err()
            .to_string();
        assert!(error.contains("no module for bazel_dep"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transitive_diamond_resolves_each_once() {
        let dir = std::env::temp_dir().join(format!("bzlmod_test_diamond_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // root -> a, b ; a -> d ; b -> d  (diamond): d must register exactly once.
        write(
            &dir,
            "MODULE.bazel",
            "bazel_dep(name = \"a\", version = \"1\")\nbazel_dep(name = \"b\", version = \"1\")\n",
        );
        write(
            &dir,
            "bzlmod_modules/a/1/MODULE.bazel",
            "module(name = \"a\", version = \"1\")\nbazel_dep(name = \"d\", version = \"1\")\n",
        );
        write(&dir, "bzlmod_modules/a/1/BUILD", "# a\n");
        write(
            &dir,
            "bzlmod_modules/b/1/MODULE.bazel",
            "module(name = \"b\", version = \"1\")\nbazel_dep(name = \"d\", version = \"1\")\n",
        );
        write(&dir, "bzlmod_modules/b/1/BUILD", "# b\n");
        write(
            &dir,
            "bzlmod_modules/d/1/MODULE.bazel",
            "module(name = \"d\", version = \"1\")\n",
        );
        write(&dir, "bzlmod_modules/d/1/BUILD", "# d (leaf)\n");
        let res = with_tracked_scope(&dir, || resolve(&dir)).unwrap();
        assert!(res.repos.contains_key("a"));
        assert!(res.repos.contains_key("b"));
        assert!(res.repos.contains_key("d"));
        assert_eq!(res.repos.len(), 3, "diamond must register a,b,d once each");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tracked_manifest_captures_root_transitive_modules_candidates_and_env() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let root_module = write(
            root,
            "MODULE.bazel",
            "module(name = \"root\", version = \"1\")\nbazel_dep(name = \"a\", version = \"1\")\n",
        );
        let transitive_module = write(
            root,
            "registry/a/1/MODULE.bazel",
            "module(name = \"a\", version = \"1\")\nbazel_dep(name = \"leaf\", version = \"1\")\n",
        );
        let leaf_module = write(
            root,
            "registry/leaf/1/MODULE.bazel",
            "module(name = \"leaf\", version = \"1\")\n",
        );

        let env = [
            ("RUSH_BZLMOD_REGISTRY", "registry"),
            ("RUSH_BZLMOD_REGISTRY_URL", "https://registry.invalid"),
            ("RUSH_BZLMOD_NO_FETCH", "1"),
        ];
        let (resolution, io, semantics) = with_tracked_scope_env(root, &env, || resolve(root));
        let resolution = resolution.unwrap();
        assert_eq!(resolution.repos.len(), 2);

        let manifest = io.manifest();
        for module in [&root_module, &transitive_module, &leaf_module] {
            assert!(
                manifest.deps().iter().any(|dep| {
                    matches!(dep, Dependency::FileContent { path, .. } if path == module)
                }),
                "missing tracked MODULE content for {}",
                module.display()
            );
        }
        for candidate in [root.join("registry/a/1"), root.join("registry/leaf/1")] {
            assert!(manifest.deps().iter().any(|dep| {
                matches!(dep, Dependency::FileExistence { path, exists: true, kind: Some(rush_incremental::FileKind::Dir) } if path == &candidate)
            }), "missing tracked candidate probe for {}", candidate.display());
        }
        for name in [
            b"RUSH_BZLMOD_REGISTRY".as_slice(),
            b"RUSH_BZLMOD_REGISTRY_URL".as_slice(),
            b"RUSH_BZLMOD_NO_FETCH".as_slice(),
        ] {
            assert!(manifest.deps().iter().any(|dep| {
                matches!(dep, Dependency::Env { name: recorded, .. } if recorded.as_slice() == name)
            }), "missing tracked environment dependency {}", String::from_utf8_lossy(name));
        }

        std::fs::write(
            &transitive_module,
            b"module(name = \"a\", version = \"1\")\n# semantic source edit\n",
        )
        .unwrap();
        assert!(matches!(
            manifest.validate_staleness(&semantics),
            Staleness::Stale { .. }
        ));
    }

    #[test]
    fn appearing_preferred_vendored_candidate_invalidates_manifest_and_fingerprint() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        write(
            root,
            "MODULE.bazel",
            "module(name = \"root\")\nbazel_dep(name = \"rules_cc\", version = \"1\")\n",
        );
        let flat = write(
            root,
            "registry/rules_cc/MODULE.bazel",
            "module(name = \"rules_cc\", version = \"1\")\n",
        )
        .parent()
        .unwrap()
        .to_path_buf();
        let env = [
            ("RUSH_BZLMOD_REGISTRY", "registry"),
            ("RUSH_BZLMOD_NO_FETCH", "1"),
        ];

        let (first, io, semantics) = with_tracked_scope_env(root, &env, || resolve(root));
        let first = first.unwrap();
        assert_eq!(first.repos.get("rules_cc"), Some(&flat));
        let first_manifest = io.manifest();
        let preferred = root.join("registry/rules_cc/1");
        assert!(first_manifest.deps().iter().any(|dep| {
            matches!(dep, Dependency::FileExistence { path, exists: false, .. } if path == &preferred)
        }));

        write(
            root,
            "registry/rules_cc/1/MODULE.bazel",
            "module(name = \"rules_cc\", version = \"1\")\n",
        );
        assert!(matches!(
            first_manifest.validate_staleness(&semantics),
            Staleness::Stale { .. }
        ));

        let second = with_tracked_scope_env(root, &env, || resolve(root))
            .0
            .unwrap();
        assert_eq!(second.repos.get("rules_cc"), Some(&preferred));
        assert_ne!(first.semantic_fingerprint, second.semantic_fingerprint);
    }

    #[cfg(unix)]
    #[test]
    fn vendored_candidate_symlink_target_is_tracked() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        write(
            root,
            "MODULE.bazel",
            "module(name = \"root\")\nbazel_dep(name = \"rules_cc\", version = \"1\")\n",
        );
        let first_target = write(
            root,
            "targets/first/MODULE.bazel",
            "module(name = \"rules_cc\", version = \"1\")\n",
        )
        .parent()
        .unwrap()
        .to_path_buf();
        let second_target = write(
            root,
            "targets/second/MODULE.bazel",
            "module(name = \"rules_cc\", version = \"1\")\n",
        )
        .parent()
        .unwrap()
        .to_path_buf();
        let candidate = root.join("registry/rules_cc/1");
        std::fs::create_dir_all(candidate.parent().unwrap()).unwrap();
        symlink(&first_target, &candidate).unwrap();
        let env = [
            ("RUSH_BZLMOD_REGISTRY", "registry"),
            ("RUSH_BZLMOD_NO_FETCH", "1"),
        ];

        let (first, io, semantics) = with_tracked_scope_env(root, &env, || resolve(root));
        let first = first.unwrap();
        assert_eq!(first.repos.get("rules_cc"), Some(&first_target));
        let manifest = io.manifest();
        assert!(manifest.deps().iter().any(|dep| {
            matches!(dep, Dependency::Symlink { path, target: Some(_) } if path == &candidate)
        }));

        std::fs::remove_file(&candidate).unwrap();
        symlink(&second_target, &candidate).unwrap();
        assert!(matches!(
            manifest.validate_staleness(&semantics),
            Staleness::Stale { .. }
        ));
        let second = with_tracked_scope_env(root, &env, || resolve(root))
            .0
            .unwrap();
        assert_eq!(second.repos.get("rules_cc"), Some(&second_target));
        assert_ne!(first.semantic_fingerprint, second.semantic_fingerprint);
    }

    #[test]
    fn tracked_registry_env_change_invalidates_and_changes_resolution_fingerprint() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        write(
            root,
            "MODULE.bazel",
            "module(name = \"root\")\nbazel_dep(name = \"rules_cc\", version = \"1\")\n",
        );
        for registry in ["registry-a", "registry-b"] {
            write(
                root,
                &format!("{registry}/rules_cc/1/MODULE.bazel"),
                "module(name = \"rules_cc\", version = \"1\")\n",
            );
        }

        let env_a = [
            ("RUSH_BZLMOD_REGISTRY", "registry-a"),
            ("RUSH_BZLMOD_NO_FETCH", "1"),
        ];
        let env_b = [
            ("RUSH_BZLMOD_REGISTRY", "registry-b"),
            ("RUSH_BZLMOD_NO_FETCH", "1"),
        ];
        let (first, io, _) = with_tracked_scope_env(root, &env_a, || resolve(root));
        let first = first.unwrap();
        let second_semantics = test_semantics(root, &env_b);
        assert!(matches!(
            io.manifest().validate_staleness(&second_semantics),
            Staleness::Stale { .. }
        ));

        let second = with_tracked_scope_env(root, &env_b, || resolve(root))
            .0
            .unwrap();
        assert_ne!(first.repos, second.repos);
        assert_ne!(first.semantic_fingerprint, second.semantic_fingerprint);
    }

    #[test]
    fn semantic_fingerprint_is_order_independent_and_resolution_round_trips() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        for module in ["a", "b"] {
            write(
                root,
                &format!("registry/{module}/1/MODULE.bazel"),
                &format!("module(name = \"{module}\", version = \"1\")\n"),
            );
        }
        let env = [
            ("RUSH_BZLMOD_REGISTRY", "registry"),
            ("RUSH_BZLMOD_NO_FETCH", "1"),
        ];
        write(
            root,
            "MODULE.bazel",
            "module(name = \"root\")\nbazel_dep(name = \"a\", version = \"1\")\nbazel_dep(name = \"b\", version = \"1\")\n",
        );
        let first = with_tracked_scope_env(root, &env, || resolve(root))
            .0
            .unwrap();

        write(
            root,
            "MODULE.bazel",
            "# reordered, same module semantics\nmodule(name = \"root\")\nbazel_dep(name = \"b\", version = \"1\")\nbazel_dep(name = \"a\", version = \"1\")\n",
        );
        let second = with_tracked_scope_env(root, &env, || resolve(root))
            .0
            .unwrap();
        assert_eq!(first.repos, second.repos);
        assert_eq!(first.semantic_fingerprint, second.semantic_fingerprint);

        let encoded = serde_json::to_vec(&second).unwrap();
        let restored: BzlmodResolution = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(restored.repos, second.repos);
        assert_eq!(restored.repo_mappings, second.repo_mappings);
        assert_eq!(restored.semantic_fingerprint, second.semantic_fingerprint);
    }

    #[test]
    fn unsupported_module_semantics_are_recorded_and_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let module = write(
            root,
            "MODULE.bazel",
            "module(name = \"root\", compatibility_level = 1)\n\
             bazel_dep(name = \"rules_cc\", version = \"1\", dev_dependency = True)\n\
             use_extension(\"//:extensions.bzl\", \"deps\")\n",
        );
        let parsed = with_tracked_scope(root, || parse_module_file(&module)).unwrap();
        let details: Vec<_> = parsed
            .unsupported_semantics
            .iter()
            .map(|issue| (issue.directive.as_str(), issue.detail.as_str()))
            .collect();
        assert!(details.iter().any(|(directive, detail)| {
            *directive == "module" && detail.contains("compatibility_level")
        }));
        assert!(details.iter().any(|(directive, detail)| {
            *directive == "bazel_dep" && detail.contains("dev_dependency")
        }));
        assert!(details
            .iter()
            .any(|(directive, _)| *directive == "use_extension"));

        let error = with_tracked_scope(root, || resolve(root))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported MODULE semantics"));
        assert!(error.contains("native wiring refused"));
    }

    #[test]
    fn conflicting_versions_fail_closed_instead_of_first_wins() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        write(
            root,
            "MODULE.bazel",
            "module(name = \"root\")\n\
             bazel_dep(name = \"rules_cc\", version = \"1\", repo_name = \"rules_cc_v1\")\n\
             bazel_dep(name = \"rules_cc\", version = \"2\", repo_name = \"rules_cc_v2\")\n",
        );
        write(
            root,
            "registry/rules_cc/1/MODULE.bazel",
            "module(name = \"rules_cc\", version = \"1\")\n",
        );
        let env = [
            ("RUSH_BZLMOD_REGISTRY", "registry"),
            ("RUSH_BZLMOD_NO_FETCH", "1"),
        ];
        let error = with_tracked_scope_env(root, &env, || resolve(root))
            .0
            .unwrap_err()
            .to_string();
        assert!(error.contains("MVS remains unsupported"));
    }

    #[tokio::test]
    async fn bcr_patches_are_verified_and_applied_in_registry_order() {
        let repository = tempfile::tempdir().unwrap();
        write(repository.path(), "config.txt", "old\n");

        let first = b"diff --git a/config.txt b/config.txt\n\
--- a/config.txt\n\
+++ b/config.txt\n\
@@ -1 +1 @@\n\
-old\n\
+middle\n";
        let second = b"diff --git a/config.txt b/config.txt\n\
--- a/config.txt\n\
+++ b/config.txt\n\
@@ -1 +1 @@\n\
-middle\n\
+new\n";
        let source_json = format!(
            r#"{{
                "url": "https://example.invalid/archive.tar.gz",
                "integrity": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                "patch_strip": 1,
                "patches": {{
                    "z-first.patch": "{}",
                    "a-second.patch": "{}"
                }}
            }}"#,
            sha256_sri(first),
            sha256_sri(second)
        );
        let source = parse_bcr_archive_source("example", "1.0", &source_json).unwrap();
        assert_eq!(
            source
                .patches
                .iter()
                .map(|patch| patch.name.as_str())
                .collect::<Vec<_>>(),
            ["z-first.patch", "a-second.patch"]
        );
        verify_sha256_sri(first, &source.patches[0].integrity, "first patch").unwrap();
        verify_sha256_sri(second, &source.patches[1].integrity, "second patch").unwrap();

        apply_bcr_patches(
            repository.path(),
            &source.patches,
            &[first.to_vec(), second.to_vec()],
            source.patch_strip,
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(repository.path().join("config.txt")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn source_metadata_fields_fail_closed_when_materialization_is_unsupported() {
        parse_bcr_archive_source(
            "example",
            "1.0",
            r#"{
                "type": "archive",
                "url": "https://example.invalid/source.tar.gz",
                "mirror_urls": ["https://mirror.invalid/source.tar.gz"],
                "integrity": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                "docs_url": "https://docs.invalid/archive.zip"
            }"#,
        )
        .unwrap();

        let non_archive = parse_bcr_archive_source(
            "example",
            "1.0",
            r#"{
                "type": "git_repository",
                "remote": "https://example.invalid/repository.git",
                "commit": "0123456789abcdef"
            }"#,
        )
        .unwrap_err()
        .to_string();
        assert!(non_archive.contains("source type `git_repository`"));

        for archive_type in [r#""zip""#, "null"] {
            let json = format!(
                r#"{{
                    "url": "https://example.invalid/source",
                    "integrity": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                    "archive_type": {archive_type}
                }}"#
            );
            assert!(parse_bcr_archive_source("example", "1.0", &json)
                .unwrap_err()
                .to_string()
                .contains("archive_type="));
        }

        let unknown = r#"{
            "url": "https://example.invalid/source.tar.gz",
            "integrity": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "future_materialization_field": true
        }"#;
        assert!(parse_bcr_archive_source("example", "1.0", unknown).is_err());

        let overlay = r#"{
            "url": "https://example.invalid/source.tar.gz",
            "integrity": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "overlay": {"BUILD.bazel": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}
        }"#;
        assert!(parse_bcr_archive_source("example", "1.0", overlay)
            .unwrap_err()
            .to_string()
            .contains("overlays"));
    }

    #[tokio::test]
    async fn bcr_patch_context_must_match_without_fuzz() {
        let repository = tempfile::tempdir().unwrap();
        write(
            repository.path(),
            "config.txt",
            "different-before\nold\ncontext-after\n",
        );
        let patch = b"--- a/config.txt\n\
+++ b/config.txt\n\
@@ -1,3 +1,3 @@\n\
 context-before\n\
-old\n\
+new\n\
 context-after\n";
        let metadata = [BcrPatch {
            name: "no-fuzz.patch".to_string(),
            integrity: sha256_sri(patch),
        }];

        let error = apply_bcr_patches(repository.path(), &metadata, &[patch.to_vec()], 1)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("applying BCR patch `no-fuzz.patch` failed"));
        assert_eq!(
            std::fs::read_to_string(repository.path().join("config.txt")).unwrap(),
            "different-before\nold\ncontext-after\n"
        );
    }

    #[test]
    fn bcr_patch_integrity_and_paths_fail_closed() {
        let expected = sha256_sri(b"expected");
        let integrity_error = verify_sha256_sri(b"different", &expected, "test patch")
            .unwrap_err()
            .to_string();
        assert!(integrity_error.contains("SHA256 mismatch"));

        let repository = tempfile::tempdir().unwrap();
        let traversal = b"--- a/../outside\n+++ b/../outside\n@@ -0,0 +1 @@\n+bad\n";
        let path_error = validate_patch_paths(repository.path(), "escape.patch", traversal, 1)
            .unwrap_err()
            .to_string();
        assert!(path_error.contains("unsafe path"));
    }

    #[test]
    fn cached_bcr_repository_requires_exact_marker() {
        let parent = tempfile::tempdir().unwrap();
        let repository = parent.path().join("published");
        let expected = b"verified source and patch metadata";

        assert!(!cached_repository_is_reusable(&repository, expected).unwrap());

        std::fs::create_dir(&repository).unwrap();
        let missing = cached_repository_is_reusable(&repository, expected)
            .unwrap_err()
            .to_string();
        assert!(missing.contains("without the expected source marker"));

        std::fs::write(repository.join(BCR_SOURCE_MARKER), b"wrong metadata").unwrap();
        let wrong = cached_repository_is_reusable(&repository, expected)
            .unwrap_err()
            .to_string();
        assert!(wrong.contains("without the expected source marker"));

        std::fs::write(repository.join(BCR_SOURCE_MARKER), expected).unwrap();
        assert!(cached_repository_is_reusable(&repository, expected).unwrap());
    }

    #[test]
    fn bcr_source_marker_is_create_new_and_does_not_follow_symlinks() {
        let parent = tempfile::tempdir().unwrap();
        let repository = parent.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        let marker_path = repository.join(BCR_SOURCE_MARKER);

        write_source_marker(&repository, b"expected metadata").unwrap();
        assert!(marker_matches(&repository, b"expected metadata"));
        assert!(write_source_marker(&repository, b"replacement").is_err());

        #[cfg(unix)]
        {
            std::fs::remove_file(&marker_path).unwrap();
            let outside = parent.path().join("outside");
            std::fs::write(&outside, b"outside sentinel").unwrap();
            std::os::unix::fs::symlink(&outside, &marker_path).unwrap();

            assert!(write_source_marker(&repository, b"replacement").is_err());
            assert_eq!(std::fs::read(&outside).unwrap(), b"outside sentinel");
            assert!(!marker_matches(&repository, b"outside sentinel"));
        }
    }

    #[tokio::test]
    async fn fresh_staging_child_cannot_reuse_a_stale_repository() {
        let workspace = tempfile::tempdir().unwrap();
        let external_dir = crate::cache_paths::workspace_external_dir(workspace.path());
        std::fs::create_dir_all(&external_dir).unwrap();
        let cache_name = "bzlmod-grpc-1.74.1-test";

        // Model a directory stranded by an older/crashed attempt. If this path
        // reached RepositoryManager it would be accepted without validation.
        let stale_repository = external_dir
            .join(format!(".{cache_name}-staging-stale"))
            .join("repository");
        std::fs::create_dir_all(&stale_repository).unwrap();
        write(&stale_repository, "STALE", "must not be accepted\n");

        let staging = create_bcr_staging(&external_dir, cache_name).unwrap();
        assert_ne!(staging.repository, stale_repository);
        assert!(!staging.repository.exists());
        assert_eq!(external_dir.join(&staging.archive_name), staging.repository);

        let archive = HttpArchive {
            name: staging.archive_name.clone(),
            urls: Vec::new(),
            sha256: None,
            strip_prefix: None,
            build_file: None,
            build_file_content: None,
            repo_mapping: HashMap::new(),
        };
        let manager = RepositoryManager::new(Arc::new(SystemPackageMapper::new()));
        let error = manager
            .fetch_http_archive(workspace.path(), &archive)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("No URLs provided"));
        assert!(stale_repository.join("STALE").is_file());
    }

    #[test]
    fn user_repo_name_validation_matches_bazel_6_1_compatibility_subset() {
        // Differential control: Bazel 6.1's
        // RepositoryName.validateUserProvidedRepoName accepts these spellings.
        for valid in ["repo", "Repo", "r9", "com_google.foo-bar", "a_"] {
            validate_user_repo_name(valid).unwrap();
        }

        // Bazel 6.1 rejects each of these. A leading digit is accepted by newer
        // Bazel, but Rush intentionally keeps the 6.1-compatible intersection.
        for invalid in [
            "",
            "_repo",
            "-repo",
            ".repo",
            "9repo",
            "bad/name",
            "bad\\name",
            "repo+alias",
            "repo~alias",
            "repo alias",
            "répo",
        ] {
            assert!(
                validate_user_repo_name(invalid).is_err(),
                "repository name `{invalid}` must be rejected"
            );
        }
    }

    #[test]
    fn explicit_empty_repo_names_default_to_module_names() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let module_file = write(
            root,
            "MODULE.bazel",
            "module(name = \"root\", repo_name = \"\")\n\
             bazel_dep(name = \"rules_cc\", version = \"1\", repo_name = \"\")\n",
        );
        write(
            root,
            "registry/rules_cc/1/MODULE.bazel",
            "module(name = \"rules_cc\", version = \"1\", repo_name = \"\")\n",
        );

        let parsed = with_tracked_scope(root, || parse_module_file(&module_file)).unwrap();
        assert_eq!(parsed.self_repo_name.as_deref(), Some("root"));
        assert_eq!(parsed.deps[0].repo_name, "rules_cc");

        let env = [
            ("RUSH_BZLMOD_REGISTRY", "registry"),
            ("RUSH_BZLMOD_NO_FETCH", "1"),
        ];
        let resolution = with_tracked_scope_env(root, &env, || resolve(root))
            .0
            .unwrap();
        assert!(resolution.repos.contains_key("rules_cc"));
        assert!(!resolution.repos.contains_key(""));
    }

    #[test]
    fn duplicate_apparent_repo_names_fail_closed_per_module() {
        for (case, module_body) in [
            (
                "same_dep",
                "module(name = \"root\")\n\
                 bazel_dep(name = \"foo\", version = \"1\")\n\
                 bazel_dep(name = \"foo\", version = \"1\")\n",
            ),
            (
                "alias_collision",
                "module(name = \"root\")\n\
                 bazel_dep(name = \"foo\", version = \"1\", repo_name = \"shared\")\n\
                 bazel_dep(name = \"bar\", version = \"1\", repo_name = \"shared\")\n",
            ),
        ] {
            let workspace = tempfile::tempdir().unwrap();
            write(workspace.path(), "MODULE.bazel", module_body);
            let error = with_tracked_scope(workspace.path(), || resolve(workspace.path()))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("apparent repository name")
                    && error.contains("claimed both")
                    && error.contains("unique"),
                "{case}: unexpected error: {error}"
            );
        }
    }

    #[test]
    fn own_repo_name_collision_and_invalid_aliases_fail_closed() {
        for (case, module_body, expected) in [
            (
                "own_collision",
                "module(name = \"root\", repo_name = \"shared\")\n\
                 bazel_dep(name = \"foo\", version = \"1\", repo_name = \"shared\")\n",
                "claimed both",
            ),
            (
                "invalid_module_alias",
                "module(name = \"root\", repo_name = \"_root\")\n",
                "invalid user-provided repository name",
            ),
            (
                "invalid_dep_alias",
                "module(name = \"root\")\n\
                 bazel_dep(name = \"foo\", version = \"1\", repo_name = \"bad/name\")\n",
                "invalid user-provided repository name",
            ),
        ] {
            let workspace = tempfile::tempdir().unwrap();
            write(workspace.path(), "MODULE.bazel", module_body);
            let error = with_tracked_scope(workspace.path(), || resolve(workspace.path()))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(expected),
                "{case}: unexpected error: {error}"
            );
        }
    }

    #[test]
    fn registry_segments_reject_path_like_module_coordinates() {
        for invalid in [
            "",
            ".",
            "..",
            "../grpc",
            "grpc/child",
            "grpc\\child",
            "Grpc",
            "_grpc",
            "grpc.",
            "grpc+core",
        ] {
            assert!(
                validate_module_name(invalid).is_err(),
                "module name `{invalid}` must be rejected"
            );
        }
        for valid in ["grpc", "c-ares", "rules_cc", "abseil-cpp", "boringssl"] {
            validate_module_name(valid).unwrap();
        }

        for invalid in [
            "",
            ".",
            "..",
            "../1.0",
            "1.0/child",
            "1.0\\child",
            "1.0 rc1",
            ".1",
            "1.",
            "1..0",
            "1-",
            "1+",
            "1++meta",
            "1_0",
            "1:0",
            "18446744073709551616",
        ] {
            assert!(
                normalize_module_version(invalid).is_err(),
                "module version `{invalid}` must be rejected"
            );
        }
        for valid in [
            "1.74.1",
            "20250512.1",
            "0.20241024.0",
            "abc.2",
            "1.0.0-rc-1+meta.foo-bar",
            "1+18446744073709551616",
        ] {
            normalize_module_version(valid).unwrap();
        }
        assert_eq!(normalize_module_version("1.0+metadata").unwrap(), "1.0");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bcr_fetch_rejects_current_thread_runtime_without_panicking() {
        let workspace = tempfile::tempdir().unwrap();
        let error = bcr_fetch(
            "https://example.invalid",
            "not-fetched",
            "1.0",
            workspace.path(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("current-thread runtime"));
        assert!(error.contains("RUSH_BZLMOD_NO_FETCH=1"));
    }

    #[test]
    fn bcr_fetch_rejects_missing_runtime() {
        let workspace = tempfile::tempdir().unwrap();
        let error = bcr_fetch(
            "https://example.invalid",
            "not-fetched",
            "1.0",
            workspace.path(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("requires a Tokio multi-thread runtime"));
        assert!(error.contains("RUSH_BZLMOD_NO_FETCH=1"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires BCR and GitHub network access"]
    async fn real_grpc_bcr_patch_is_materialized_once() {
        let workspace = tempfile::tempdir().unwrap();
        let first = bcr_fetch_async(DEFAULT_BCR_URL, "grpc", "1.74.1", workspace.path())
            .await
            .unwrap();
        let cython = std::fs::read_to_string(first.join("bazel/cython_library.bzl")).unwrap();
        assert!(cython.contains("@rules_python//python/cc:current_py_cc_headers"));
        assert!(!cython.contains("@local_config_python//:python_headers"));
        let marker = std::fs::read_to_string(first.join(BCR_SOURCE_MARKER)).unwrap();
        assert!(marker.contains("\"patch_strip\": 1"));
        assert!(marker.contains("\"name\": \"adopt_bzlmod.patch\""));

        let second = bcr_fetch_async(DEFAULT_BCR_URL, "grpc", "1.74.1", workspace.path())
            .await
            .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn base64_and_sri_decode() {
        assert_eq!(base64_decode("TWFu").unwrap(), b"Man");
        assert_eq!(base64_decode("TWE=").unwrap(), b"Ma");
        assert_eq!(base64_decode("TQ==").unwrap(), b"M");
        // 32 zero bytes -> 43 'A' + '=' padding; sri_to_hex -> 64 hex zeros.
        let zeros_b64 = format!("{}=", "A".repeat(43));
        assert_eq!(
            sri_to_hex(&format!("sha256-{}", zeros_b64)).unwrap(),
            "0".repeat(64)
        );
        // Real BCR integrity (rules_cc 0.0.9) decodes to a 32-byte (64-hex) digest.
        let h = sri_to_hex("sha256-IDeHW5pEVtzkp50RKorohbvEqtlo5lh9ym5k86CQDN8=").unwrap();
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        // Non-sha256 / malformed integrity is rejected.
        assert!(sri_to_hex("sha512-AAAA").is_none());
        assert!(sri_to_hex("sha256-@@@").is_none());
    }
}
