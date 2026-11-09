// Rush Build - tracked WORKSPACE repository-rule materialization
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Admission for a genuine Bazel WORKSPACE repository-rule result. The parsed
//! http_archive tuple is insufficient: the rule may consume external overlays,
//! patches, repository environment, and imported rc files. A proof is written
//! only after executing the real rule with this exact command context. It binds
//! opaque Bazel semantics to that invocation; marker fields alone are not proof.

use anyhow::{bail, Context, Result};
use rush_incremental::tracked;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const PROOF_SUFFIX: &str = ".rush-workspace-inputs-v4.json";

#[derive(Debug)]
pub struct WorkspaceRepositoryFetchContext {
    startup_arguments: Vec<OsString>,
    environment: Vec<(Vec<u8>, Vec<u8>)>,
    fingerprint: String,
    executable: PathBuf,
    consistency_inputs: Vec<(Vec<u8>, Option<SourceStat>)>,
    pending_definition: std::sync::Mutex<Option<DefinitionSnapshot>>,
    required_module: Option<String>,
    pending_authority: std::sync::Mutex<SourceAuthority>,
}

impl WorkspaceRepositoryFetchContext {
    /// Apply before command-specific startup arguments such as --output_base.
    pub fn apply(&self, command: &mut Command) -> Result<()> {
        command.args(&self.startup_arguments).env_clear();
        for (name, value) in &self.environment {
            command.env(os_string(name)?, os_string(value)?);
        }
        Ok(())
    }
    /// Restrict this invocation to an exact .bzl source consumer. Graph-target
    /// fetches do not set this purpose and cannot reuse closure-only authority.
    pub fn for_required_module(mut self, repository: &str, label: &str) -> Result<Self> {
        self.required_module = Some(canonical_required_module_label(repository, label)?);
        Ok(self)
    }

    /// A successful declaration query already executed every WORKSPACE load.
    /// Admit only the requested canonical label and the exact file identity
    /// actually consumed in that stable, authoritative loadfiles closure.
    pub fn required_module_source_is_materialized(
        &self, workspace: &Path, output_base: &Path, repository: &str,
    ) -> Result<bool> {
        let Some(required) = &self.required_module else { return Ok(false); };
        let definition = self.pending_definition.lock()
            .map_err(|_| anyhow::anyhow!("repository definition lock poisoned"))?;
        let Some(definition) = definition.as_ref() else { return Ok(false); };
        if !required_module_matches(definition, workspace, output_base, repository, required)? {
            return Ok(false);
        }
        *self.pending_authority.lock()
            .map_err(|_| anyhow::anyhow!("repository authority lock poisoned"))? = SourceAuthority::LoadedWorkspaceClosure;
        Ok(true)
    }

    /// Native local_repository roots (including installed tool sources) are
    /// already materialized when Bazel's definition query resolves their source
    /// closure. Admit only the authoritative declared path, never a name-based
    /// stub or an unrelated existing directory.
    pub fn native_local_source_is_materialized(
        &self,
        output_base: &Path,
        repository: &str,
    ) -> Result<bool> {
        let definition = self
            .pending_definition
            .lock()
            .map_err(|_| anyhow::anyhow!("repository definition lock poisoned"))?;
        let Some(DefinitionSnapshot {
            profile: RepositoryProfile::NativeLocal { canonical_root },
            ..
        }) = definition.as_ref()
        else {
            return Ok(false);
        };
        let (marker, _) = repository_paths(output_base, repository)?;
        if !std::fs::symlink_metadata(&marker).is_ok_and(|metadata| metadata.file_type().is_file())
        {
            return Ok(false);
        }
        let content = std::fs::read_to_string(marker)?;
        if !content.lines().next().is_some_and(is_digest) {
            return Ok(false);
        }
        let source = output_base.join("external").join(repository);
        match std::fs::canonicalize(source) {
            Ok(actual) if path_identity(&actual)? == *canonical_root => {
                *self.pending_authority.lock()
                    .map_err(|_| anyhow::anyhow!("repository authority lock poisoned"))? = SourceAuthority::NativeLocal;
                Ok(true)
            }
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InvocationProof {
    version: u32,
    context_sha256: String,
    marker_sha256: String,
    authority: SourceAuthority,
    definition: DefinitionSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum SourceAuthority {
    FetchedRepository,
    NativeLocal,
    /// Only exact modules in this proof's authoritative loaded closure are
    /// admitted; this authority cannot skip a graph-target materialization.
    LoadedWorkspaceClosure,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SourceStat {
    len: u64,
    #[cfg(unix)]
    unix: (u64, u64, u32, i64, i64, i64, i64),
}

fn source_stat(path: &Path) -> Result<SourceStat> {
    #[cfg(not(unix))]
    {
        let _ = path;
        bail!("WORKSPACE repository refresh consistency requires Unix file identity and change-time metadata; this platform is not supported");
    }
    #[cfg(unix)]
    let metadata = std::fs::metadata(path)?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    #[cfg(unix)]
    {
        Ok(SourceStat {
            len: metadata.len(),
            unix: (
                metadata.dev(),
                metadata.ino(),
                metadata.mode(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DefinitionInput {
    label: String,
    canonical_path: Vec<u8>,
    sha256: String,
    stat: SourceStat,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DefinitionSnapshot {
    declaration_sha256: String,
    inputs: Vec<DefinitionInput>,
    /// Actual .bzl consumers reported by loadfiles(), excluding buildfiles()'s
    /// synthetic package BUILD labels. Each owner, path and file value is exact.
    loaded_modules: Vec<DefinitionInput>,
    profile: RepositoryProfile,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum RepositoryProfile {
    RepositoryRule,
    NativeLocal { canonical_root: Vec<u8> },
}

impl DefinitionSnapshot {
    // Metadata protects the refresh bracket. Warm early cutoff uses values.
    fn same_values(&self, other: &Self) -> bool {
        self.profile == other.profile
            && self.declaration_sha256 == other.declaration_sha256
            && definition_input_values_equal(&self.inputs, &other.inputs)
            && definition_input_values_equal(&self.loaded_modules, &other.loaded_modules)
    }
}

fn definition_input_values_equal(left: &[DefinitionInput], right: &[DefinitionInput]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(left, right)| {
        left.label == right.label
            && left.canonical_path == right.canonical_path
            && left.sha256 == right.sha256
    })
}

fn canonical_required_module_label(repository: &str, label: &str) -> Result<String> {
    if repository.is_empty() || matches!(repository, "." | "..")
        || repository.bytes().any(|byte| !byte.is_ascii_alphanumeric() && !b"._-+~".contains(&byte))
    {
        bail!("required module repository is not a valid path component");
    }
    let canonical = label.strip_prefix("@@").map(|rest| format!("@{rest}"))
        .unwrap_or_else(|| label.to_owned());
    let prefix = format!("@{repository}//");
    let relative = canonical.strip_prefix(&prefix)
        .context("required module label is outside its declared repository")?;
    let (package, target) = relative.split_once(':')
        .context("required module needs an exact package:target label")?;
    if target.is_empty() || package.contains(':') || target.contains(':')
        || canonical.bytes().any(|byte| matches!(byte, 0 | b'\n' | b'\r'))
        || Path::new(package).components().chain(Path::new(target).components())
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("required module is not a package-relative source label");
    }
    Ok(canonical)
}

fn required_module_matches(
    definition: &DefinitionSnapshot, workspace: &Path, output_base: &Path,
    repository: &str, label: &str,
) -> Result<bool> {
    let required = canonical_required_module_label(repository, label)?;
    let Some(loaded) = definition.loaded_modules.iter().find(|input|
        canonical_required_module_label(repository, &input.label).is_ok_and(|actual| actual == required))
    else { return Ok(false); };
    // Membership is label/owner-sensitive; identical bytes in another repo do
    // not authorize this module. The canonical path and stat must also match.
    let current = match definition_inputs(workspace, output_base, &[required]) {
        Ok(mut inputs) => inputs.pop().context("required module input omitted")?,
        Err(error) if error.chain().filter_map(|cause| cause.downcast_ref::<std::io::Error>())
            .any(|error| error.kind() == std::io::ErrorKind::NotFound) => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(loaded.canonical_path == current.canonical_path && loaded.stat == current.stat
        && loaded.sha256 == current.sha256
        && definition.inputs.iter().any(|input| input == loaded))
}

fn loaded_module_labels(output: &str) -> Result<Vec<String>> {
    let mut labels = output.lines().filter(|line| !line.is_empty())
        .map(str::to_owned).collect::<Vec<_>>();
    if labels.iter().any(|label| !label.contains(":") || !(label.starts_with("//") || label.starts_with('@'))) {
        bail!("Bazel loadfiles query returned an unsupported module label");
    }
    labels.sort();
    labels.dedup();
    Ok(labels)
}

fn query_loaded_module_labels(
    context: &WorkspaceRepositoryFetchContext,
    workspace: &Path,
    output_base: &Path,
    repository: &str,
) -> Result<Vec<String>> {
    loaded_module_labels(&query_definition_output(context, workspace, output_base,
        repository, "label", format!("loadfiles(//external:{repository})"))?)
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn is_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn os_string(bytes: &[u8]) -> Result<OsString> {
    if bytes.contains(&0) {
        bail!("NUL in repository command environment");
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        Ok(OsString::from(String::from_utf8(bytes.to_vec())?))
    }
}

fn path_identity(path: &Path) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(path.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        Ok(path
            .to_str()
            .context("repository input path is not UTF-8")?
            .as_bytes()
            .to_vec())
    }
}

fn optional_input(
    path: &Path,
    inputs: &mut BTreeMap<Vec<u8>, Option<String>>,
) -> Result<Option<String>> {
    if !tracked::exists(path) {
        inputs.insert(path_identity(path)?, None);
        return Ok(None);
    }
    let canonical = tracked::canonicalize(path)?;
    let content = tracked::read_file(&canonical)?;
    inputs.insert(path_identity(path)?, Some(digest(&content)));
    Ok(Some(String::from_utf8(content.to_vec()).with_context(
        || format!("repository configuration {} is not UTF-8", path.display()),
    )?))
}

fn import_path(workspace: &Path, value: &str) -> Result<PathBuf> {
    // Bazel rc import paths are one token, optionally quoted. Refuse forms we
    // cannot bind rather than silently leave an imported configuration untracked.
    let value = value.trim();
    let value = if value.starts_with('"') || value.starts_with('\'') {
        let quote = value.as_bytes()[0] as char;
        if !value.ends_with(quote) || value.len() < 2 {
            bail!("malformed Bazel rc import path");
        }
        &value[1..value.len() - 1]
    } else {
        if value.split_whitespace().count() != 1 {
            bail!("unsupported Bazel rc import path form: {value}");
        }
        value
    };
    if value.is_empty() || value.contains('\\') || value.contains('\0') {
        bail!("unsupported Bazel rc import path form");
    }
    if let Some(relative) = value.strip_prefix("%workspace%/") {
        return Ok(workspace.join(relative));
    }
    if value.contains('%') {
        bail!("unsupported Bazel rc import substitution: {value}");
    }
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    })
}

fn track_rc(
    workspace: &Path,
    path: &Path,
    required: bool,
    inputs: &mut BTreeMap<Vec<u8>, Option<String>>,
    active: &mut HashSet<PathBuf>,
    depth: usize,
) -> Result<bool> {
    if depth > 64 {
        bail!("Bazel rc import depth exceeded");
    }
    let Some(contents) = optional_input(path, inputs)? else {
        if required {
            bail!("required Bazel rc import {} is missing", path.display());
        }
        return Ok(false);
    };
    let canonical = tracked::canonicalize(path)?;
    if !active.insert(canonical.clone()) {
        bail!("cyclic Bazel rc import at {}", path.display());
    }
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((directive, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if matches!(directive, "import" | "try-import") {
            let imported = import_path(workspace, rest)?;
            track_rc(
                workspace,
                &imported,
                directive == "import",
                inputs,
                active,
                depth + 1,
            )?;
        }
    }
    active.remove(&canonical);
    Ok(true)
}

/// Bind the actual executable, captured client environment, command config and
/// standard/imported Bazel rc inputs. Explicit rc startup flags preserve the
/// standard system -> workspace -> home order while making the set observable;
/// unsupported import syntax fails closed. This WORKSPACE path is independent
/// of Rush's native build-flag interpretation.
pub fn prepare_workspace_repository_fetch(
    workspace: &Path,
    bazel_binary: &Path,
) -> Result<WorkspaceRepositoryFetchContext> {
    let configuration = tracked::config_fingerprint();
    let environment = tracked::command_environment()?;
    let executable = tracked::canonicalize(bazel_binary)?;
    if !tracked::metadata_full(&executable) {
        bail!("Bazel executable is unavailable");
    }
    let executable_sha = digest(&tracked::read_file(&executable)?);
    let home = environment
        .iter()
        .find(|(key, _)| key == b"HOME")
        .map(|(_, value)| os_string(value).map(PathBuf::from))
        .transpose()?;
    let mut candidates = vec![
        PathBuf::from("/etc/bazel.bazelrc"),
        workspace.join(".bazelrc"),
    ];
    if let Some(home) = home {
        candidates.push(home.join(".bazelrc"));
    }
    let mut inputs = BTreeMap::new();
    let mut startup_arguments = vec![
        "--nosystem_rc".into(),
        "--noworkspace_rc".into(),
        "--nohome_rc".into(),
    ];
    for path in candidates {
        if track_rc(workspace, &path, false, &mut inputs, &mut HashSet::new(), 0)? {
            let mut argument = OsString::from("--bazelrc=");
            argument.push(path.as_os_str());
            startup_arguments.push(argument);
        }
    }
    for name in [
        "WORKSPACE",
        "WORKSPACE.bazel",
        "MODULE.bazel",
        ".bazelversion",
        ".bazeliskrc",
    ] {
        optional_input(&workspace.join(name), &mut inputs)?;
    }
    // Serialize paths as byte vectors (JSON object keys cannot represent them).
    let mut consistency_inputs =
        vec![(path_identity(&executable)?, Some(source_stat(&executable)?))];
    for name in inputs.keys() {
        let path = PathBuf::from(os_string(name)?);
        let state = match source_stat(&path) {
            Ok(state) => Some(state),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        consistency_inputs.push((name.clone(), state));
    }
    let entries: Vec<_> = inputs.into_iter().collect();
    let fingerprint = digest(&serde_json::to_vec(&(
        1u32,
        configuration,
        path_identity(&executable)?,
        executable_sha,
        &environment,
        entries,
    ))?);
    Ok(WorkspaceRepositoryFetchContext {
        startup_arguments,
        environment,
        fingerprint,
        executable,
        consistency_inputs,
        pending_definition: std::sync::Mutex::new(None),
        required_module: None,
        pending_authority: std::sync::Mutex::new(SourceAuthority::FetchedRepository),
    })
}

fn repository_paths(output_base: &Path, repository: &str) -> Result<(PathBuf, PathBuf)> {
    if repository.is_empty()
        || !repository
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-+~".contains(&byte))
    {
        bail!("unsupported WORKSPACE repository name: {repository}");
    }
    let marker = output_base
        .join("external")
        .join(format!("@{repository}.marker"));
    let proof = output_base
        .join("external")
        .join(format!("@{repository}{PROOF_SUFFIX}"));
    Ok((marker, proof))
}

fn unescape(value: &str) -> Result<Option<String>> {
    if value == "\\0" {
        return Ok(None);
    }
    let mut result = String::new();
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            result.push(character);
            continue;
        }
        result.push(match chars.next() {
            Some('\\') => '\\',
            Some('n') => '\n',
            Some('s') => ' ',
            _ => bail!("unsupported Bazel repository-marker escape"),
        });
    }
    Ok(Some(result))
}

fn source_path(workspace: &Path, output_base: &Path, label: &str) -> Result<PathBuf> {
    let (root, rest) = if let Some(rest) = label.strip_prefix("//") {
        (workspace.to_path_buf(), rest)
    } else if let Some(rest) = label.strip_prefix("@//") {
        (workspace.to_path_buf(), rest)
    } else if let Some(rest) = label.strip_prefix('@') {
        let (owner, rest) = rest
            .strip_prefix('@')
            .unwrap_or(rest)
            .split_once("//")
            .context("unsupported repository-marker FILE label")?;
        repository_paths(output_base, owner)?;
        (output_base.join("external").join(owner), rest)
    } else {
        bail!("unsupported repository-marker FILE label: {label}");
    };
    if root == workspace && rest == "external:WORKSPACE" {
        let preferred = workspace.join("WORKSPACE.bazel");
        return Ok(if tracked::exists(&preferred) {
            preferred
        } else {
            workspace.join("WORKSPACE")
        });
    }
    let (package, target) = rest
        .split_once(':')
        .context("repository-marker FILE label requires an explicit target")?;
    let relative = Path::new(package).join(target);
    if target.is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("invalid repository-marker FILE source path: {label}");
    }
    Ok(root.join(relative))
}

fn query_definition_output(
    context: &WorkspaceRepositoryFetchContext,
    workspace: &Path,
    output_base: &Path,
    repository: &str,
    format: &str,
    expression: String,
) -> Result<String> {
    // Even a definition query can materialize external repositories. Earlier
    // path probes must remain evidence without hiding the resulting paths.
    tracked::refresh_filesystem_observations();
    let mut command = Command::new(&context.executable);
    context.apply(&mut command)?;
    command
        .arg("--output_base")
        .arg(output_base)
        .arg("query")
        .arg("--noshow_progress")
        .arg(format!("--output={format}"));
    if format == "streamed_jsonproto" {
        command
            .arg("--proto:definition_stack=true")
            .arg("--proto:output_rule_attrs=all");
    }
    command.arg(expression).current_dir(workspace);
    let output = command
        .output()
        .context("query actual WORKSPACE repository declaration")?;
    if !output.status.success() {
        bail!(
            "Bazel WORKSPACE repository declaration query for @{} failed with status {:?}: {}",
            repository,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("Bazel repository declaration query is not UTF-8")
}

fn query_definition(
    context: &WorkspaceRepositoryFetchContext,
    workspace: &Path,
    output_base: &Path,
    repository: &str,
    closure: bool,
) -> Result<String> {
    query_definition_output(
        context,
        workspace,
        output_base,
        repository,
        if closure { "label" } else { "build" },
        if closure {
            format!("buildfiles(//external:{repository})")
        } else {
            format!("//external:{repository}")
        },
    )
}

fn definition_labels(output: &str) -> Result<Vec<String>> {
    let mut labels: Vec<_> = output
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    labels.sort();
    labels.dedup();
    if !labels.iter().any(|label| label == "//external:WORKSPACE") {
        bail!("Bazel buildfiles query omitted the defining WORKSPACE input");
    }
    Ok(labels)
}

fn definition_inputs(
    workspace: &Path,
    output_base: &Path,
    labels: &[String],
) -> Result<Vec<DefinitionInput>> {
    labels
        .iter()
        .map(|label| {
            let path = source_path(workspace, output_base, label)?;
            let canonical = tracked::canonicalize(&path)?;
            let before = source_stat(&canonical)?;
            let bytes = tracked::read_file(&canonical)?;
            let after = source_stat(&canonical)?;
            if before != after {
                bail!("WORKSPACE definition input changed while reading {}", label);
            }
            Ok(DefinitionInput {
                label: label.clone(),
                canonical_path: path_identity(&canonical)?,
                sha256: digest(&bytes),
                stat: after,
            })
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryTarget {
    #[serde(rename = "type")]
    kind: String,
    rule: QueryRule,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueryRule {
    name: String,
    rule_class: String,
    #[serde(default)]
    location: Option<String>,
    attribute: Vec<serde_json::Value>,
    #[serde(default)]
    rule_input: Vec<String>,
    #[serde(default)]
    rule_output: Vec<String>,
    #[serde(default)]
    default_setting: Vec<String>,
    #[serde(default)]
    instantiation_stack: Vec<String>,
    #[serde(default)]
    definition_stack: Vec<String>,
}

fn repository_profile(
    metadata: &str,
    workspace: &Path,
    repository: &str,
) -> Result<RepositoryProfile> {
    // Pinned Bazel 6.5 ProtoOutputFormatter176–185/424 always injects this
    // attribute for Starlark rule definitions and never for native rules. Rule
    // class names alone are not a discriminator: Starlark may export a rule as
    // local_repository. Query with all attrs and definition stacks explicitly.
    // https://github.com/bazelbuild/bazel/blob/6.5.0/src/main/java/com/google/devtools/build/lib/query2/query/output/ProtoOutputFormatter.java
    let mut records = serde_json::Deserializer::from_str(metadata).into_iter::<QueryTarget>();
    let target = records
        .next()
        .context("repository metadata query returned no target")??;
    if records.next().is_some() {
        bail!("repository metadata query returned multiple targets");
    }
    if target.kind != "RULE"
        || target.rule.name != format!("//external:{repository}")
        || target.rule.rule_class.is_empty()
    {
        bail!("repository metadata query returned the wrong rule identity");
    }
    let rule = target.rule;
    let _ = (
        &rule.location,
        &rule.rule_input,
        &rule.rule_output,
        &rule.default_setting,
        &rule.instantiation_stack,
    );
    let mut attributes = BTreeMap::new();
    for value in &rule.attribute {
        let object = value
            .as_object()
            .context("repository metadata attribute is not an object")?;
        let name = object
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty())
            .context("repository metadata attribute has no name")?;
        object
            .get("type")
            .and_then(serde_json::Value::as_str)
            .filter(|kind| !kind.is_empty())
            .context("repository metadata attribute has no type")?;
        if attributes.insert(name, object).is_some() {
            bail!("duplicate repository metadata attribute {name}");
        }
    }
    let string = |name: &str| -> Result<Option<&str>> {
        let Some(attribute) = attributes.get(name) else {
            return Ok(None);
        };
        if attribute.get("type").and_then(serde_json::Value::as_str) != Some("STRING") {
            bail!("repository metadata attribute {name} is not STRING");
        }
        if attribute.keys().any(|key| {
            !matches!(
                key.as_str(),
                "name" | "type" | "stringValue" | "explicitlySpecified" | "nodep"
            )
        }) {
            bail!("unsupported shape for repository STRING attribute {name}");
        }
        Ok(Some(
            attribute
                .get("stringValue")
                .and_then(serde_json::Value::as_str)
                .context("repository metadata STRING has no value")?,
        ))
    };
    if string("name")? != Some(repository) {
        bail!("repository metadata name attribute mismatch");
    }
    if let Some(hash) = string("$rule_implementation_hash")? {
        if !is_digest(hash) {
            bail!("invalid Starlark repository implementation digest");
        }
        return Ok(RepositoryProfile::RepositoryRule);
    }
    if !rule.definition_stack.is_empty() {
        bail!("Starlark definition stack has no implementation digest");
    }
    if rule.rule_class != "local_repository" {
        return Ok(RepositoryProfile::RepositoryRule);
    }
    let path = PathBuf::from(string("path")?.context("native local_repository has no path")?);
    let path = if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    };
    let canonical = tracked::canonicalize(&path)?;
    Ok(RepositoryProfile::NativeLocal {
        canonical_root: path_identity(&canonical)?,
    })
}

fn capture_definition(
    context: &WorkspaceRepositoryFetchContext,
    workspace: &Path,
    output_base: &Path,
    repository: &str,
) -> Result<DefinitionSnapshot> {
    let labels = definition_labels(&query_definition(
        context,
        workspace,
        output_base,
        repository,
        true,
    )?)?;
    let module_labels = query_loaded_module_labels(context, workspace, output_base, repository)?;
    let before = definition_inputs(workspace, output_base, &labels)?;
    let modules_before = definition_inputs(workspace, output_base, &module_labels)?;
    let declaration = query_definition(context, workspace, output_base, repository, false)?;
    let metadata = query_definition_output(
        context,
        workspace,
        output_base,
        repository,
        "streamed_jsonproto",
        format!("//external:{repository}"),
    )?;
    let after_labels = definition_labels(&query_definition(
        context,
        workspace,
        output_base,
        repository,
        true,
    )?)?;
    let after_module_labels = query_loaded_module_labels(context, workspace, output_base, repository)?;
    let after = definition_inputs(workspace, output_base, &after_labels)?;
    let modules_after = definition_inputs(workspace, output_base, &after_module_labels)?;
    if labels != after_labels || before != after
        || module_labels != after_module_labels || modules_before != modules_after
    {
        bail!("WORKSPACE definition closure changed during repository declaration query");
    }
    Ok(DefinitionSnapshot {
        declaration_sha256: digest(&serde_json::to_vec(&(&declaration, &metadata))?),
        inputs: after,
        loaded_modules: modules_after,
        profile: repository_profile(&metadata, workspace, repository)?,
    })
}

fn verify_marker(
    workspace: &Path,
    output_base: &Path,
    repository: &str,
    context: &WorkspaceRepositoryFetchContext,
    profile: &RepositoryProfile,
) -> Result<(String, bool)> {
    let (marker, _) = repository_paths(output_base, repository)?;
    // Materialization sidecars are cache-admission artifacts, not source
    // inputs. Probe them without memoizing an absent/stale state that the
    // authentic fetch is about to replace in this same analysis scope. Only an
    // accepted final marker/proof is recorded below; FILE/ENV source inputs are
    // always tracked during verification.
    if !std::fs::symlink_metadata(&marker)?.file_type().is_file() {
        bail!(
            "Bazel repository marker is not a regular file at {}",
            marker.display()
        );
    }
    let content = std::fs::read_to_string(&marker)?;
    let mut lines = content.lines();
    if !lines.next().is_some_and(is_digest) {
        bail!("unsupported Bazel repository marker header");
    }
    let mut keys = HashSet::new();
    let mut semantics = false;
    let mut arch = false;
    let mut current = true;
    for line in lines {
        let (key, value) = line
            .split_once(' ')
            .context("malformed Bazel repository marker line")?;
        let key = unescape(key)?.context("null repository marker key")?;
        let value = unescape(value)?;
        if !keys.insert(key.clone()) {
            bail!("duplicate Bazel repository marker key");
        }
        if key == "STARLARK_SEMANTICS" {
            value
                .as_deref()
                .context("missing Bazel semantics marker")?
                .parse::<i32>()
                .context("unsupported Bazel semantics marker")?;
            semantics = true;
        } else if key == "ARCH:" {
            if value.as_deref() != Some(std::env::consts::ARCH) {
                bail!("Bazel repository marker architecture does not match this executor");
            }
            arch = true;
        } else if let Some(label) = key.strip_prefix("FILE:") {
            let source = source_path(workspace, output_base, label)?;
            let expected = value.as_deref();
            if let Some(expected) = expected {
                if !is_digest(expected) {
                    bail!("unsupported Bazel repository FILE digest");
                }
                if !tracked::exists(&source) {
                    current = false;
                    continue;
                }
                let canonical = tracked::canonicalize(&source)?;
                current &= digest(&tracked::read_file(&canonical)?) == expected;
            } else {
                current &= !tracked::exists(&source);
            }
        } else if let Some(name) = key.strip_prefix("ENV:") {
            if name.is_empty() || name.contains('=') || name.contains('\0') {
                bail!("invalid Bazel repository ENV input");
            }
            let _ = tracked::getenv(name.as_bytes());
            // --repo_env in rc files can override client environment. Its value
            // is bound by context plus the post-materialization marker digest, not guessed
            // from the process environment or compared to the client value.
            let _ = &context.environment;
        } else {
            bail!("unsupported Bazel repository marker input: {key}");
        }
    }
    if matches!(profile, RepositoryProfile::RepositoryRule) && (!semantics || !arch) {
        bail!("incomplete Bazel repository-rule marker semantics/architecture");
    }
    if let RepositoryProfile::NativeLocal { canonical_root } = profile {
        let actual = tracked::canonicalize(&output_base.join("external").join(repository))?;
        if path_identity(&actual)? != *canonical_root {
            bail!("native local_repository materialization does not match its authoritative declared path");
        }
    }
    Ok((digest(content.as_bytes()), current))
}

/// A false result requires authentic repository-rule refresh. No incomplete
/// sidecar, warmup hint, parsed http_archive tuple, or opaque semantics value
/// can stand in for the post-materialization invocation proof.
pub fn workspace_repository_inputs_are_current(
    workspace: &Path,
    output_base: &Path,
    repository: &str,
    context: &WorkspaceRepositoryFetchContext,
) -> Result<bool> {
    inputs_are_current_with_definition_loader(workspace, output_base, repository, context, || {
        capture_definition(context, workspace, output_base, repository)
    })
}

fn inputs_are_current_with_definition_loader(
    workspace: &Path,
    output_base: &Path,
    repository: &str,
    context: &WorkspaceRepositoryFetchContext,
    load_definition: impl FnOnce() -> Result<DefinitionSnapshot>,
) -> Result<bool> {
    let current = cached_inputs_are_current(workspace, output_base, repository, context)?;
    if current {
        return Ok(true);
    }
    let definition = load_definition()?;
    *context
        .pending_definition
        .lock()
        .map_err(|_| anyhow::anyhow!("repository definition lock poisoned"))? = Some(definition);
    Ok(false)
}

fn cached_inputs_are_current(
    workspace: &Path,
    output_base: &Path,
    repository: &str,
    context: &WorkspaceRepositoryFetchContext,
) -> Result<bool> {
    let (marker_path, proof) = repository_paths(output_base, repository)?;
    let materialized = output_base.join("external").join(repository);
    match std::fs::symlink_metadata(&materialized) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
        Ok(_) => {
            let marked = std::fs::symlink_metadata(&marker_path)
                .is_ok_and(|metadata| metadata.file_type().is_file())
                && std::fs::read_to_string(&marker_path)
                    .is_ok_and(|content| content.lines().next().is_some_and(is_digest));
            if !marked {
                bail!("repository {} exists without a Bazel source marker; unknown contents were preserved; inspect/move it explicitly or select a fresh cache", materialized.display());
            }
            if !std::fs::metadata(&materialized)?.is_dir() {
                bail!("Bazel materialized source root is not a directory");
            }
        }
    }
    let metadata = match std::fs::symlink_metadata(&proof) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() {
        bail!("repository invocation proof is not a regular file");
    }
    let raw = std::fs::read(&proof)?;
    let proof_value: InvocationProof = match serde_json::from_slice(&raw) {
        Ok(proof) => proof,
        Err(_) => return Ok(false),
    };
    if proof_value.version != 4 || proof_value.context_sha256 != context.fingerprint {
        return Ok(false);
    }
    match &proof_value.authority {
        SourceAuthority::FetchedRepository => {},
        SourceAuthority::NativeLocal => {
            if !matches!(&proof_value.definition.profile, RepositoryProfile::NativeLocal { .. }) {
                bail!("native-local source authority has no native-local declaration");
            }
        }
        SourceAuthority::LoadedWorkspaceClosure => {
            let Some(required) = &context.required_module else { return Ok(false); };
            if !required_module_matches(&proof_value.definition, workspace, output_base, repository, required)? {
                return Ok(false);
            }
        }
    }
    let labels: Vec<_> = proof_value
        .definition
        .inputs
        .iter()
        .map(|input| input.label.clone())
        .collect();
    let observed_inputs = match definition_inputs(workspace, output_base, &labels) {
        Ok(inputs) => inputs,
        Err(error)
            if error
                .chain()
                .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                .any(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let observed = DefinitionSnapshot {
        declaration_sha256: proof_value.definition.declaration_sha256.clone(),
        inputs: observed_inputs,
        loaded_modules: definition_inputs(workspace, output_base,
            &proof_value.definition.loaded_modules.iter().map(|input| input.label.clone()).collect::<Vec<_>>())?,
        profile: proof_value.definition.profile.clone(),
    };
    if !proof_value.definition.same_values(&observed) {
        return Ok(false);
    }
    let (marker, inputs_current) = verify_marker(
        workspace,
        output_base,
        repository,
        context,
        &proof_value.definition.profile,
    )?;
    if !inputs_current || marker != proof_value.marker_sha256 {
        return Ok(false);
    }
    let (marker_path, _) = repository_paths(output_base, repository)?;
    if digest(&tracked::read_file(&marker_path)?) != marker
        || tracked::read_file(&proof)?.as_ref() != raw.as_slice()
    {
        bail!("repository admission artifacts changed while validating source");
    }
    Ok(true)
}

fn verify_definition_bracket(
    before: &DefinitionSnapshot,
    after: &DefinitionSnapshot,
) -> Result<()> {
    if before == after {
        return Ok(());
    }
    let before_labels: std::collections::BTreeSet<_> = before
        .inputs
        .iter()
        .map(|input| input.label.as_str())
        .collect();
    let after_labels: std::collections::BTreeSet<_> = after
        .inputs
        .iter()
        .map(|input| input.label.as_str())
        .collect();
    let mut differences = Vec::new();
    if before.loaded_modules != after.loaded_modules {
        differences.push("actual loaded-module labels, file values or identities changed".to_string());
    }
    if before.declaration_sha256 != after.declaration_sha256 {
        differences.push(format!(
            "declaration output {} -> {}",
            before.declaration_sha256, after.declaration_sha256
        ));
    }
    if before_labels != after_labels {
        differences.push(format!(
            "load closure added {:?}, removed {:?}",
            after_labels
                .difference(&before_labels)
                .take(8)
                .collect::<Vec<_>>(),
            before_labels
                .difference(&after_labels)
                .take(8)
                .collect::<Vec<_>>()
        ));
    }
    for left in &before.inputs {
        let Some(right) = after.inputs.iter().find(|input| input.label == left.label) else {
            continue;
        };
        let mut aspects = Vec::new();
        if left.canonical_path != right.canonical_path {
            aspects.push("resolved path".to_string());
        }
        if left.sha256 != right.sha256 {
            aspects.push(format!("content {} -> {}", left.sha256, right.sha256));
        }
        if left.stat != right.stat {
            aspects.push(format!(
                "file identity/stat {:?} -> {:?}",
                left.stat, right.stat
            ));
        }
        if !aspects.is_empty() {
            differences.push(format!("{}: {}", left.label, aspects.join(", ")));
        }
        if differences.len() >= 10 {
            break;
        }
    }
    bail!("WORKSPACE repository definition changed during materialization; source was not admitted: {}", differences.join("; "));
}

/// Call only after successful genuine Bazel materialization, a verified native
/// local root, or an exact source module already consumed by the successful
/// WORKSPACE definition query under `context`. All three retain the same full
/// definition/stat/context/marker bracket. Closure-only authority cannot admit
/// graph-target fetches; existing markers never create authority on their own.
pub fn record_workspace_repository_fetch(
    workspace: &Path,
    output_base: &Path,
    repository: &str,
    context: &WorkspaceRepositoryFetchContext,
) -> Result<()> {
    let before = context
        .pending_definition
        .lock()
        .map_err(|_| anyhow::anyhow!("repository definition lock poisoned"))?
        .clone()
        .context("repository refresh was not preceded by a verified current declaration")?;
    let after = capture_definition(context, workspace, output_base, repository)?;
    verify_definition_bracket(&before, &after)?;
    let current_context = prepare_workspace_repository_fetch(workspace, &context.executable)?;
    if context.fingerprint != current_context.fingerprint
        || context.consistency_inputs != current_context.consistency_inputs
    {
        bail!("repository command configuration/tool inputs changed during materialization; source was not admitted");
    }
    publish_workspace_fetch(workspace, output_base, repository, context, after)
}

fn publish_workspace_fetch(
    workspace: &Path,
    output_base: &Path,
    repository: &str,
    context: &WorkspaceRepositoryFetchContext,
    definition: DefinitionSnapshot,
) -> Result<()> {
    if !std::fs::metadata(output_base.join("external").join(repository))?.is_dir() {
        bail!("Bazel materialized source root is not a directory");
    }
    let (marker_sha256, inputs_current) = verify_marker(
        workspace,
        output_base,
        repository,
        context,
        &definition.profile,
    )?;
    if !inputs_current {
        bail!("Bazel repository inputs changed during materialization; source was not admitted");
    }
    let (_, proof) = repository_paths(output_base, repository)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&proof) {
        if !metadata.file_type().is_file() {
            bail!(
                "refusing to replace unknown repository invocation proof at {}",
                proof.display()
            );
        }
    }
    let payload = serde_json::to_vec(&InvocationProof {
        version: 4,
        context_sha256: context.fingerprint.clone(),
        marker_sha256: marker_sha256.clone(),
        authority: context.pending_authority.lock()
            .map_err(|_| anyhow::anyhow!("repository authority lock poisoned"))?.clone(),
        definition,
    })?;
    // This sidecar lives beside Bazel's own marker, outside archive contents.
    // Unique file and rename prevent a partially written proof from admission.
    let parent = proof.parent().context("repository proof has no parent")?;
    let unique = parent.join(format!(
        ".rush-repo-proof-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    use std::io::Write;
    let mut temporary = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&unique)?;
    temporary.write_all(&payload)?;
    temporary.sync_all()?;
    std::fs::rename(&unique, &proof)?;
    // Record final proof bytes, not its previous absent/stale state.
    let _ = tracked::read_file(&proof)?;
    let (marker, _) = repository_paths(output_base, repository)?;
    if digest(&tracked::read_file(&marker)?) != marker_sha256 {
        bail!("repository marker changed while publishing source proof");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_incremental::{Staleness, TrackedIo};

    fn context() -> WorkspaceRepositoryFetchContext {
        WorkspaceRepositoryFetchContext {
            startup_arguments: vec!["--nosystem_rc".into()],
            environment: vec![(b"CLIENT_ONLY".to_vec(), b"captured".to_vec())],
            fingerprint: "fixture-context".into(),
            executable: PathBuf::from("unused-fixture-tool"),
            consistency_inputs: Vec::new(),
            pending_definition: std::sync::Mutex::new(None),
            required_module: None,
            pending_authority: std::sync::Mutex::new(SourceAuthority::FetchedRepository),
        }
    }

    fn fixture_definition() -> DefinitionSnapshot {
        DefinitionSnapshot {
            declaration_sha256: digest(b"unit fixture declaration, no Bazel execution claim"),
            inputs: Vec::new(),
            loaded_modules: Vec::new(),
            profile: RepositoryProfile::RepositoryRule,
        }
    }

    fn workspace_repository_inputs_are_current(
        workspace: &Path,
        output: &Path,
        repo: &str,
        context: &WorkspaceRepositoryFetchContext,
    ) -> Result<bool> {
        inputs_are_current_with_definition_loader(workspace, output, repo, context, || {
            Ok(fixture_definition())
        })
    }

    fn record_workspace_repository_fetch(
        workspace: &Path,
        output: &Path,
        repo: &str,
        context: &WorkspaceRepositoryFetchContext,
    ) -> Result<()> {
        publish_workspace_fetch(workspace, output, repo, context, fixture_definition())
    }

    fn marker(workspace: &Path, output: &Path, suffix: &str) -> PathBuf {
        let patch = workspace.join("patches/fix.patch");
        std::fs::create_dir_all(patch.parent().unwrap()).unwrap();
        std::fs::write(&patch, b"genuine patch bytes\n").unwrap();
        let external = output.join("external");
        std::fs::create_dir_all(external.join("source")).unwrap();
        let marker = external.join("@source.marker");
        // Shape and escaping of the pinned Bazel 6.5 WORKSPACE marker; this is
        // a unit fixture, not a claim that the test executed a repository rule.
        std::fs::write(&marker, format!("{}\nSTARLARK_SEMANTICS -1438583352\nARCH: {}\nFILE://patches:fix.patch {}\nENV:TF_SYSTEM_LIBS \\0\n{suffix}", "a".repeat(64), std::env::consts::ARCH, digest(b"genuine patch bytes\n"))).unwrap();
        marker
    }

    #[test]
    fn required_module_authority_requires_actual_owner_label_and_file_identity() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        marker(temp.path(), &output, "");
        let source = output.join("external/source");
        let other = output.join("external/other");
        std::fs::create_dir_all(&other).unwrap();
        for root in [&source, &other] {
            std::fs::create_dir_all(root.join("defs/subdir")).unwrap();
            std::fs::write(root.join("defs/subdir/child.bzl"), b"VALUE = 42\n").unwrap();
        }
        let label = "@source//defs:subdir/child.bzl";
        let context = context().for_required_module("source", label).unwrap();
        let definition = tracked::test_scope(temp.path(), || {
            let own = definition_inputs(temp.path(), &output, &[label.into()]).unwrap();
            let other = definition_inputs(temp.path(), &output, &["@other//defs:subdir/child.bzl".into()]).unwrap();
            let mut definition = fixture_definition();
            definition.inputs = own.clone();
            *context.pending_definition.lock().unwrap() = Some(definition.clone());
            assert!(!context.required_module_source_is_materialized(temp.path(), &output, "source").unwrap(), "a file on disk is not a loadfiles consumer");
            definition.inputs.extend(other.clone());
            definition.loaded_modules = other;
            *context.pending_definition.lock().unwrap() = Some(definition.clone());
            assert!(!context.required_module_source_is_materialized(temp.path(), &output, "source").unwrap(), "identical bytes owned by another repository do not qualify");
            definition.loaded_modules = own;
            *context.pending_definition.lock().unwrap() = Some(definition.clone());
            assert!(context.required_module_source_is_materialized(temp.path(), &output, "source").unwrap());
            assert_eq!(*context.pending_authority.lock().unwrap(), SourceAuthority::LoadedWorkspaceClosure);
            publish_workspace_fetch(temp.path(), &output, "source", &context, definition.clone()).unwrap();
            assert!(cached_inputs_are_current(temp.path(), &output, "source", &context).unwrap());
            assert!(!cached_inputs_are_current(temp.path(), &output, "source", &self::context()).unwrap(), "graph purpose must run genuine fetch");
            definition
        });
        // Refresh must not accept a replaced inode merely because bytes match.
        let replacement = source.join("replacement.bzl");
        std::fs::write(&replacement, b"VALUE = 42\n").unwrap();
        std::fs::rename(replacement, source.join("defs/subdir/child.bzl")).unwrap();
        tracked::test_scope(temp.path(), || {
            assert!(!required_module_matches(&definition, temp.path(), &output, "source", label).unwrap());
            assert!(!cached_inputs_are_current(temp.path(), &output, "source", &context).unwrap());
        });
        std::fs::write(source.join("defs/subdir/child.bzl"), b"VALUE = 43\n").unwrap();
        tracked::test_scope(temp.path(), || {
            assert!(!required_module_matches(&definition, temp.path(), &output, "source", label).unwrap());
        });
        assert!(self::context().for_required_module("source", "@other//defs:subdir/child.bzl").is_err());
        assert!(self::context().for_required_module("source", "@source//defs/subdir/child.bzl").is_err());
    }

    #[test]
    fn actual_loaded_closure_changes_and_failed_queries_never_publish_authority() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        marker(temp.path(), &output, "");
        let context = context().for_required_module("source", "@source//:defs.bzl").unwrap();
        tracked::test_scope(temp.path(), || {
            let error = inputs_are_current_with_definition_loader(temp.path(), &output, "source", &context,
                || bail!("actual module query failed")).unwrap_err();
            assert!(error.to_string().contains("actual module query failed"));
            assert!(context.pending_definition.lock().unwrap().is_none());
            assert!(!context.required_module_source_is_materialized(temp.path(), &output, "source").unwrap());
            let (_, proof) = repository_paths(&output, "source").unwrap();
            assert!(!proof.exists());
            let mut before = fixture_definition();
            std::fs::write(temp.path().join("defs.bzl"), b"VALUE = 42\n").unwrap();
            before.inputs = definition_inputs(temp.path(), &output, &["//:defs.bzl".into()]).unwrap();
            before.loaded_modules = before.inputs.clone();
            let mut no_module_load = before.clone();
            no_module_load.loaded_modules.clear();
            assert!(!before.same_values(&no_module_load));
            assert!(verify_definition_bracket(&before, &no_module_load).is_err());
        });
    }

    #[test]
    fn repository_inputs_require_post_sync_context_proof_and_track_patch_content() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        marker(temp.path(), &output, "");
        let context = context();
        tracked::test_scope(temp.path(), || {
            assert!(!workspace_repository_inputs_are_current(
                temp.path(),
                &output,
                "source",
                &context
            )
            .unwrap());
        });
        tracked::test_scope(temp.path(), || {
            record_workspace_repository_fetch(temp.path(), &output, "source", &context).unwrap();
        });
        tracked::test_scope(temp.path(), || {
            assert!(workspace_repository_inputs_are_current(
                temp.path(),
                &output,
                "source",
                &context
            )
            .unwrap());
            let io = tracked::current_required();
            let manifest = io.manifest();
            assert!(matches!(
                io.validate_manifest(&manifest).staleness,
                Staleness::Fresh
            ));
            std::fs::write(
                temp.path().join("patches/fix.patch"),
                b"edited patch bytes\n",
            )
            .unwrap();
            assert!(matches!(
                io.validate_manifest(&manifest).staleness,
                Staleness::Stale { .. }
            ));
        });
        tracked::test_scope(temp.path(), || {
            assert!(!workspace_repository_inputs_are_current(
                temp.path(),
                &output,
                "source",
                &context
            )
            .unwrap());
            assert!(
                record_workspace_repository_fetch(temp.path(), &output, "source", &context)
                    .is_err()
            );
        });
    }

    #[test]
    fn repository_inputs_bind_external_overlay_and_fail_on_missing_or_changed_source() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        let marker_path = marker(temp.path(), &output, "");
        let overlay = output.join("external/owner/config/overlay.BUILD");
        std::fs::create_dir_all(overlay.parent().unwrap()).unwrap();
        std::fs::write(&overlay, b"real external overlay\n").unwrap();
        let mut content = std::fs::read_to_string(&marker_path).unwrap();
        content.push_str(&format!(
            "FILE:@owner//config:overlay.BUILD {}\n",
            digest(b"real external overlay\n")
        ));
        std::fs::write(marker_path, content).unwrap();
        tracked::test_scope(temp.path(), || {
            record_workspace_repository_fetch(temp.path(), &output, "source", &context()).unwrap();
        });
        std::fs::remove_file(&overlay).unwrap();
        tracked::test_scope(temp.path(), || {
            assert!(!workspace_repository_inputs_are_current(
                temp.path(),
                &output,
                "source",
                &context()
            )
            .unwrap());
        });
    }

    #[test]
    fn repository_inputs_reject_unknown_architecture_semantics_and_marker_forms() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        let marker_path = marker(temp.path(), &output, "");
        let original = std::fs::read_to_string(&marker_path).unwrap();
        for invalid in [
            original.replace(&format!("ARCH: {}", std::env::consts::ARCH), "ARCH: alien"),
            original.replace(
                "STARLARK_SEMANTICS -1438583352",
                "STARLARK_SEMANTICS opaque",
            ),
            format!("{original}DIRECTORY:unmodeled value\n"),
            format!("{original}FILE://patches:fix.patch {}\n", "a".repeat(64)),
            format!("{original}FILE://patches:../outside {}\n", "a".repeat(64)),
        ] {
            std::fs::write(&marker_path, invalid).unwrap();
            tracked::test_scope(temp.path(), || {
                assert!(record_workspace_repository_fetch(
                    temp.path(),
                    &output,
                    "source",
                    &context()
                )
                .is_err());
            });
            assert!(!repository_paths(&output, "source").unwrap().1.exists());
        }
    }

    #[test]
    fn repository_inputs_refuse_changed_invocation_even_with_matching_marker() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        marker(temp.path(), &output, "");
        tracked::test_scope(temp.path(), || {
            record_workspace_repository_fetch(temp.path(), &output, "source", &context()).unwrap();
        });
        let mut changed = context();
        changed
            .fingerprint
            .push_str("-different-tool-or-config-or-env");
        tracked::test_scope(temp.path(), || {
            assert!(!workspace_repository_inputs_are_current(
                temp.path(),
                &output,
                "source",
                &changed
            )
            .unwrap());
        });
    }

    #[test]
    fn repository_context_tracks_tool_and_rc_import_edits_and_optional_creation() {
        let temp = tempfile::tempdir().unwrap();
        let tool = temp.path().join("bazel");
        std::fs::write(&tool, b"pinned tool fixture bytes\n").unwrap();
        std::fs::write(
            temp.path().join(".bazelrc"),
            "try-import %workspace%/repo.bazelrc\n",
        )
        .unwrap();
        let prepare = || {
            tracked::test_scope(temp.path(), || {
                prepare_workspace_repository_fetch(temp.path(), &tool)
                    .unwrap()
                    .fingerprint
            })
        };
        let missing = prepare();
        std::fs::write(
            temp.path().join("repo.bazelrc"),
            "common --repo_env=MODE=first\n",
        )
        .unwrap();
        let first = prepare();
        assert_ne!(missing, first);
        std::fs::write(
            temp.path().join("repo.bazelrc"),
            "common --repo_env=MODE=second\n",
        )
        .unwrap();
        let second = prepare();
        assert_ne!(first, second);
        std::fs::write(&tool, b"new pinned tool fixture bytes\n").unwrap();
        assert_ne!(second, prepare());
        std::fs::write(
            temp.path().join("repo.bazelrc"),
            "import %workspace%/.bazelrc\n",
        )
        .unwrap();
        tracked::test_scope(temp.path(), || {
            assert!(prepare_workspace_repository_fetch(temp.path(), &tool)
                .unwrap_err()
                .to_string()
                .contains("cyclic"));
        });
    }

    #[test]
    fn repository_context_applies_exact_environment_and_explicit_startup_arguments() {
        let context = context();
        let mut command = Command::new("unused-fixture-tool");
        command.env("PRIOR_COMMAND", "must disappear");
        context.apply(&mut command).unwrap();
        let entries: Vec<_> = command
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(OsString::from)))
            .collect();
        assert_eq!(
            entries,
            vec![(
                OsString::from("CLIENT_ONLY"),
                Some(OsString::from("captured"))
            )]
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("--nosystem_rc")]
        );
    }
    #[test]
    fn repository_inputs_refresh_deleted_source_and_preserve_unknown_materialization() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        marker(temp.path(), &output, "");
        tracked::test_scope(temp.path(), || {
            record_workspace_repository_fetch(temp.path(), &output, "source", &context()).unwrap();
        });
        std::fs::remove_dir(output.join("external/source")).unwrap();
        tracked::test_scope(temp.path(), || {
            assert!(!workspace_repository_inputs_are_current(
                temp.path(),
                &output,
                "source",
                &context()
            )
            .unwrap());
        });
        let source = output.join("external/unknown");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("sentinel"), "preserve me").unwrap();
        tracked::test_scope(temp.path(), || {
            let error = workspace_repository_inputs_are_current(
                temp.path(),
                &output,
                "unknown",
                &context(),
            )
            .unwrap_err();
            assert!(error
                .to_string()
                .contains("unknown contents were preserved"));
        });
        assert_eq!(
            std::fs::read_to_string(source.join("sentinel")).unwrap(),
            "preserve me"
        );
    }
    #[test]
    fn repository_definition_closure_invalidates_changed_patch_selection_and_detects_aba() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        marker(temp.path(), &output, "");
        let source = temp.path().join("defs.bzl");
        std::fs::write(&source, "SELECTED = 'first.patch'\n").unwrap();
        let snapshot = |declaration: &[u8]| {
            tracked::test_scope(temp.path(), || DefinitionSnapshot {
                declaration_sha256: digest(declaration),
                inputs: definition_inputs(temp.path(), &output, &["//:defs.bzl".into()]).unwrap(),
                loaded_modules: definition_inputs(temp.path(), &output, &["//:defs.bzl".into()]).unwrap(),
                profile: RepositoryProfile::RepositoryRule,
            })
        };
        let before = snapshot(b"first.patch declaration");
        tracked::test_scope(temp.path(), || {
            publish_workspace_fetch(temp.path(), &output, "source", &context(), before.clone())
                .unwrap();
        });
        std::fs::write(&source, "SELECTED = 'second.patch'\n").unwrap();
        let changed = snapshot(b"second.patch declaration");
        let context = context();
        tracked::test_scope(temp.path(), || {
            assert!(!inputs_are_current_with_definition_loader(
                temp.path(),
                &output,
                "source",
                &context,
                || Ok(changed.clone())
            )
            .unwrap());
        });
        assert_eq!(
            context.pending_definition.lock().unwrap().as_ref(),
            Some(&changed)
        );
        assert!(verify_definition_bracket(&before, &changed).is_err());
        // Replace by a fresh inode with identical original bytes: value-based
        // warm cutoff remains legitimate, but refresh consistency refuses ABA.
        let replacement = temp.path().join("replacement.bzl");
        std::fs::write(&replacement, "SELECTED = 'first.patch'\n").unwrap();
        std::fs::rename(replacement, &source).unwrap();
        let restored = snapshot(b"first.patch declaration");
        assert!(before.same_values(&restored));
        assert_ne!(before.inputs[0].stat, restored.inputs[0].stat);
        assert!(verify_definition_bracket(&before, &restored).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn native_local_repository_header_marker_requires_authoritative_matching_source_path() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        let marker_path = marker(temp.path(), &output, "");
        let actual = temp.path().join("installed-source");
        std::fs::create_dir(&actual).unwrap();
        std::fs::remove_dir(output.join("external/source")).unwrap();
        std::os::unix::fs::symlink(&actual, output.join("external/source")).unwrap();
        std::fs::write(marker_path, format!("{}\n", "a".repeat(64))).unwrap();
        tracked::test_scope(temp.path(), || {
            assert!(publish_workspace_fetch(
                temp.path(),
                &output,
                "source",
                &context(),
                fixture_definition()
            )
            .is_err());
        });
        let mut definition = fixture_definition();
        tracked::test_scope(temp.path(), || {
            let metadata = serde_json::json!({"type":"RULE", "rule":{"name":"//external:source", "ruleClass":"local_repository", "attribute":[{"name":"name","type":"STRING","stringValue":"source"},{"name":"path","type":"STRING","stringValue":actual.to_str().unwrap()}]}}).to_string();
            definition.profile = repository_profile(&metadata, temp.path(), "source").unwrap();
        });
        let context = context();
        *context.pending_definition.lock().unwrap() = Some(definition.clone());
        assert!(context
            .native_local_source_is_materialized(&output, "source")
            .unwrap());
        tracked::test_scope(temp.path(), || {
            publish_workspace_fetch(temp.path(), &output, "source", &context, definition.clone())
                .unwrap();
            assert!(cached_inputs_are_current(temp.path(), &output, "source", &context).unwrap());
        });
        let other = temp.path().join("different-source");
        std::fs::create_dir(&other).unwrap();
        definition.profile = RepositoryProfile::NativeLocal {
            canonical_root: path_identity(&other).unwrap(),
        };
        *context.pending_definition.lock().unwrap() = Some(definition.clone());
        assert!(!context
            .native_local_source_is_materialized(&output, "source")
            .unwrap());
        tracked::test_scope(temp.path(), || {
            assert!(
                publish_workspace_fetch(temp.path(), &output, "source", &context, definition)
                    .is_err()
            );
        });
    }
    #[test]
    fn starlark_rule_named_local_repository_is_not_a_native_path_profile() {
        let temp = tempfile::tempdir().unwrap();
        let metadata = serde_json::json!({"type":"RULE", "rule":{"name":"//external:source", "ruleClass":"local_repository", "definitionStack":["//:rules.bzl:1:1: <toplevel>"], "attribute":[{"name":"name","type":"STRING","stringValue":"source"},{"name":"path","type":"STRING","stringValue":"must-not-be-canonicalized"},{"name":"$rule_implementation_hash","type":"STRING","stringValue":"a".repeat(64)}]}});
        tracked::test_scope(temp.path(), || {
            assert_eq!(
                repository_profile(&metadata.to_string(), temp.path(), "source").unwrap(),
                RepositoryProfile::RepositoryRule
            );
            let mut duplicate = metadata.clone();
            let name = duplicate["rule"]["attribute"][0].clone();
            duplicate["rule"]["attribute"]
                .as_array_mut()
                .unwrap()
                .push(name);
            assert!(repository_profile(&duplicate.to_string(), temp.path(), "source").is_err());
            assert!(repository_profile(
                &format!("{}\n{}", metadata, metadata),
                temp.path(),
                "source"
            )
            .is_err());
            let mut wrong = metadata.clone();
            wrong["rule"]["name"] = "//external:other".into();
            assert!(repository_profile(&wrong.to_string(), temp.path(), "source").is_err());
        });
    }
}
