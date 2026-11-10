// Rush Build - configured FeatureFlagInfo metadata bridge
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Resolve consumed string FeatureFlagInfo predicates through genuine Bazel
//! analysis. No rule implementations, compiler names, or defaults are invented.
//! This is a semantic subprocess dependency: validation reruns the exact probes.
//! It does not claim source-object identity or universal toolchain equivalence.

use anyhow::{bail, Context, Result};
use rush_command_core::CommandSemantics;
use rush_incremental::tracked;
use rush_starlark::config_defines::{self, FeatureFlagPredicate};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(crate) struct FeatureFlagCommandState {
    pub semantics: CommandSemantics,
    pub analysis: Option<ConfiguredFeatureFlagAnalysis>,
}

#[derive(Clone, Debug)]
pub(crate) struct NativeCompilerSelection {
    pub cc: PathBuf,
    pub cxx: PathBuf,
    pub additional_cxx: Vec<PathBuf>,
}

impl NativeCompilerSelection {
    /// Base drivers come from the actual execution Toolchain, not a second
    /// guessed compiler resolver. Include every source-specific override.
    pub(crate) fn from_toolchain(
        toolchain: &rush_rules::context::Toolchain,
        semantics: &CommandSemantics,
    ) -> Result<Self> {
        for (name, enabled_only) in [
            ("RUSH_GPU_CXX", false),
            ("RUSH_CUDA_CXX", false),
            ("RUSH_ENABLE_ROCM", true),
            ("RUSH_ENABLE_CUDA", true),
        ] {
            let enabled = |value: &[u8]| {
                if enabled_only {
                    value == b"1"
                } else {
                    !value.is_empty()
                }
            };
            let actual = std::env::var_os(name).map(|value| bytes(value.as_os_str()));
            if semantics.getenv(name.as_bytes()).is_some_and(enabled)
                || actual.as_deref().is_some_and(enabled)
            {
                bail!("configured flag compiler agreement supports CPU driver selection only; captured or resident {name} selects a GPU route");
            }
        }
        let actual_path = std::env::var_os("PATH").map(|value| bytes(value.as_os_str()));
        if semantics.getenv(b"PATH") != actual_path.as_deref() {
            bail!("resident execution PATH differs from this client's immutable request; restart the daemon or use --daemon=off");
        }
        if actual_path.as_ref().is_some_and(|path| {
            std::env::split_paths(&os_string(path)).any(|entry| !entry.is_absolute())
        }) {
            bail!("configured compiler agreement requires absolute PATH entries; relative execution lookup depends on the resident working directory");
        }
        let overrides = rush_rules::rules_cc::NativeCxxOverrides::from_lookup(|name| {
            semantics.getenv(name.as_bytes()).map(os_string)
        });
        let client_base = rush_rules::context::Toolchain::from_environment_lookup(|name| {
            semantics.getenv(name.as_bytes()).map(os_string)
        });
        let actual_overrides = rush_rules::rules_cc::execution_native_cxx_overrides();
        if client_base.cc != toolchain.cc
            || client_base.cxx != toolchain.cxx
            || overrides != actual_overrides
        {
            bail!("resident execution compiler selection differs from this client's immutable request; restart the daemon or use --daemon=off");
        }
        if std::iter::once(&toolchain.cc)
            .chain(std::iter::once(&toolchain.cxx))
            .chain(actual_overrides.configured_drivers())
            .any(|driver| !driver.is_absolute() && driver.components().count() > 1)
        {
            bail!("configured compiler agreement requires absolute driver paths or bare names on absolute PATH entries; relative execution lookup depends on the resident working directory");
        }
        Ok(Self {
            cc: toolchain.cc.clone(),
            cxx: toolchain.cxx.clone(),
            additional_cxx: overrides.configured_drivers().cloned().collect(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct ConfiguredFeatureFlagAnalysis {
    workspace: PathBuf,
    executable: PathBuf,
    startup: Vec<Vec<u8>>,
    flags: Vec<Vec<u8>>,
    environment_keys: Vec<Vec<u8>>,
    environment: BTreeMap<Vec<u8>, Vec<u8>>,
    fingerprint: [u8; 32],
    native: NativeCompilerSelection,
}

impl ConfiguredFeatureFlagAnalysis {
    pub(crate) fn new(
        workspace: &Path,
        bazel_binary: &Path,
        output_base: &Path,
        semantics: &CommandSemantics,
        native: NativeCompilerSelection,
    ) -> Result<Self> {
        if !cfg!(unix) {
            bail!("configured FeatureFlagInfo metadata currently requires Unix byte-preserving command IO");
        }
        let tokens = rush_command_core::build_arguments::from_command(semantics)
            .map_err(anyhow::Error::msg)?;
        let mut flags: Vec<_> =
            rush_command_core::native_options::NativeConfiguration::from_command_semantics(
                semantics,
            )
            .map_err(anyhow::Error::msg)?
            .defines
            .into_iter()
            .map(|(key, value)| format!("--define={key}={value}"))
            .collect();
        flags.extend(project_build_arguments(&tokens)?);
        let fingerprint = tracked::config_fingerprint();
        if &fingerprint != semantics.config_fingerprint() {
            bail!("configured metadata command scope does not match its immutable request");
        }
        let environment: BTreeMap<_, _> = tracked::command_environment()?.into_iter().collect();
        let executable = resolve_tool(bazel_binary, workspace, &environment)?;
        let mut startup: Vec<Vec<u8>> = project_startup_arguments(
            &rush_command_core::build_arguments::startup_from_command(semantics)
                .map_err(anyhow::Error::msg)?,
        )?
        .into_iter()
        .map(String::into_bytes)
        .collect();
        startup.extend([
            b"--ignore_all_rc_files".to_vec(),
            bytes(OsStr::new(&format!(
                "--output_base={}",
                output_base
                    .to_str()
                    .context("configured metadata output base is not UTF-8")?
            ))),
            b"--max_idle_secs=30".to_vec(),
        ]);
        let context = Self {
            workspace: workspace.to_path_buf(),
            executable,
            startup,
            flags: flags.into_iter().map(String::into_bytes).collect(),
            environment_keys: environment.keys().cloned().collect(),
            environment,
            fingerprint,
            native,
        };
        let version = context.probe(&context.executable, &[b"--version".to_vec()])?;
        if version.trim_ascii() != b"bazel 6.5.0" {
            bail!("configured FeatureFlagInfo metadata currently requires Bazel 6.5.0");
        }
        Ok(context)
    }

    /// Run only when a genuine native condition with flag_values is consumed.
    /// Install nothing until two fresh configured rounds and compiler agreement
    /// succeed, then recheck command generation and the native declaration.
    pub(crate) fn validate_registered_condition(&self, scope: &str, label: &str) -> Result<()> {
        if tracked::config_fingerprint() != self.fingerprint {
            bail!("configured metadata command changed before validation");
        }
        let generation = config_defines::feature_flag_generation(scope);
        let expected = config_defines::feature_flag_predicate(scope, label)
            .context("configured flag condition has no native declaration")?;
        if expected.flag_values.is_empty() {
            return Ok(());
        }
        let first = self.round(&expected)?;
        let second = self.round(&expected)?;
        if first != second {
            bail!("configured FeatureFlagInfo metadata changed between fresh analysis rounds");
        }
        for compiler in &first.toolchains {
            self.validate_compiler_agreement(compiler)?;
        }
        config_defines::install_verified_feature_flag_condition(
            scope,
            generation,
            expected,
            first.values,
        )
        .map_err(anyhow::Error::msg)
    }

    fn round(&self, expected: &FeatureFlagPredicate) -> Result<SemanticRound> {
        // deps() selects the actual configured/transitioned flag prerequisites.
        // A stand-alone root query would not prove the condition's own edges.
        let expression = format!("deps({})", query_label(&expected.label)?);
        let proto = self.cquery(
            &expression,
            &[
                "--output=jsonproto",
                "--proto:output_rule_attrs=all",
                "--proto:definition_stack=true",
            ],
        )?;
        let declaration = ConfiguredDeclaration::parse(&proto, expected)?;
        let mut observed = ProviderObservations::empty();
        for checksum in declaration.domains.keys() {
            let expression = format!("config({expression}, '{checksum}')");
            let formatter = PROVIDER_EXPRESSION.replacen(
                "{\"label\":",
                &format!("{{\"configuration\":\"{checksum}\",\"label\":"),
                1,
            );
            let providers = self.cquery(
                &expression,
                &["--output=starlark", &format!("--starlark:expr={formatter}")],
            )?;
            let domain = declaration.validate_provider_domain(checksum, &providers)?;
            observed.merge(domain)?;
        }
        if !observed.labels.contains(&expected.label) {
            bail!("configured provider query omitted the condition target");
        }
        let mut values = BTreeMap::new();
        let mut producers = BTreeSet::new();
        for declared in expected.flag_values.keys() {
            let actual = declaration.resolve_alias(declared)?;
            if !declaration
                .metadata
                .keys()
                .any(|(label, _)| label == &actual)
            {
                bail!(
                    "configured flag producer is absent from authoritative rule metadata: {actual}"
                );
            }
            if !producers.insert(actual.clone()) {
                bail!("condition references one producer through duplicate flag aliases");
            }
            let value = observed.features.get(&actual).with_context(|| {
                format!("flag {declared} lacks a configured string FeatureFlagInfo")
            })?;
            values.insert(declared.clone(), (actual, value.clone()));
        }
        Ok(SemanticRound {
            metadata: declaration.metadata,
            raw_occurrences: declaration.raw_occurrences,
            domains: declaration.domains,
            values,
            toolchains: observed.toolchains.into_iter().collect(),
        })
    }

    fn cquery(&self, expression: &str, output: &[&str]) -> Result<Vec<u8>> {
        // Genuine configured analysis may materialize repositories. Invalidate
        // earlier path-kind/canonicalization memos before that producer runs.
        tracked::refresh_filesystem_observations();
        let mut args = self.startup.clone();
        args.push(b"cquery".to_vec());
        args.extend(self.flags.iter().cloned());
        args.extend([
            b"--noshow_progress".to_vec(),
            b"--color=no".to_vec(),
            b"--curses=no".to_vec(),
        ]);
        args.extend(output.iter().map(|arg| arg.as_bytes().to_vec()));
        args.push(expression.as_bytes().to_vec());
        self.probe(&self.executable, &args)
    }

    fn probe(&self, program: &Path, arguments: &[Vec<u8>]) -> Result<Vec<u8>> {
        let result =
            tracked::probe_process(program, arguments, &self.environment_keys, &self.workspace)?;
        // Bazel 6.5 can exit zero after a Starlark formatter error. Never let a
        // missing output record become proof, even when the exit status is zero.
        if result.status != 0
            || result
                .stderr
                .split(|byte| *byte == b'\n')
                .any(|line| line.starts_with(b"ERROR:"))
        {
            bail!(
                "configured metadata probe failed (status {}): {}",
                result.status,
                String::from_utf8_lossy(&result.stderr)
            );
        }
        Ok(result.stdout)
    }

    fn validate_compiler_agreement(&self, compiler: &CompilerMetadata) -> Result<()> {
        if compiler.compiler.is_empty()
            || compiler.cpu.is_empty()
            || compiler.toolchain_id.is_empty()
        {
            bail!("configured CcToolchainInfo lacks compiler/cpu/toolchain identity");
        }
        if !Path::new(&compiler.executable).is_absolute() {
            bail!(
                "relative configured compiler executable needs an explicit execution-root mapping"
            );
        }
        let configured = resolve_tool(
            Path::new(&compiler.executable),
            &self.workspace,
            &self.environment,
        )?;
        let mut drivers = vec![
            (self.native.cc.clone(), "c"),
            (self.native.cxx.clone(), "c++"),
        ];
        drivers.extend(
            self.native
                .additional_cxx
                .iter()
                .cloned()
                .map(|driver| (driver, "c++")),
        );
        for (driver, language) in drivers {
            let driver = resolve_tool(&driver, &self.workspace, &self.environment)?;
            for arguments in [
                vec!["-dumpversion"],
                vec!["-dumpmachine"],
                vec!["-dM", "-E", "-x", language, "/dev/null"],
            ] {
                let arguments: Vec<_> = arguments
                    .iter()
                    .map(|arg| arg.as_bytes().to_vec())
                    .collect();
                let expected = self.probe(&configured, &arguments)?;
                let actual = self.probe(&driver, &arguments)?;
                if expected.is_empty() || actual != expected {
                    bail!(
                        "native driver {} disagrees with configured CcToolchainInfo {} in {:?}",
                        driver.display(),
                        configured.display(),
                        arguments
                    );
                }
            }
        }
        Ok(())
    }
}

// This expression always emits a JSON record, including source files whose
// providers() is None. Errors/empty/missing records cannot silently become false.
const PROVIDER_EXPRESSION: &str = r#"json.encode({"label":str(target.label),"feature":{"type":type(providers(target)["FeatureFlagInfo"].value),"value":providers(target)["FeatureFlagInfo"].value} if "FeatureFlagInfo" in (providers(target) or {}) else None,"toolchain":{"compiler":providers(target)["CcToolchainInfo"].compiler,"executable":providers(target)["CcToolchainInfo"].compiler_executable,"cpu":providers(target)["CcToolchainInfo"].cpu,"toolchain_id":providers(target)["CcToolchainInfo"].toolchain_id} if "CcToolchainInfo" in (providers(target) or {}) else None})"#;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct CompilerMetadata {
    compiler: String,
    executable: String,
    cpu: String,
    toolchain_id: String,
}
#[derive(Debug, Eq, PartialEq)]
struct SemanticRound {
    metadata: BTreeMap<(String, String), Value>,
    raw_occurrences: BTreeMap<(String, String), usize>,
    domains: BTreeMap<String, BTreeMap<String, usize>>,
    values: BTreeMap<String, (String, String)>,
    toolchains: Vec<CompilerMetadata>,
}
struct ConfiguredDeclaration {
    metadata: BTreeMap<(String, String), Value>,
    raw_occurrences: BTreeMap<(String, String), usize>,
    aliases: BTreeMap<String, String>,
    domains: BTreeMap<String, BTreeMap<String, usize>>,
}
impl ConfiguredDeclaration {
    fn parse(bytes: &[u8], expected: &FeatureFlagPredicate) -> Result<Self> {
        let value: Value =
            serde_json::from_slice(bytes).context("invalid configured query JSON proto")?;
        let results = value
            .get("results")
            .and_then(Value::as_array)
            .context("configured query JSON proto has no results")?;
        if results.is_empty() {
            bail!("configured declaration query returned no targets");
        }
        let mut configurations = BTreeMap::new();
        for configuration in value
            .get("configurations")
            .and_then(Value::as_array)
            .context("configured query JSON proto has no configuration table")?
        {
            let id = configuration
                .get("id")
                .and_then(Value::as_u64)
                .filter(|id| *id != 0)
                .context("invalid configured table identity")?;
            let checksum = string(configuration, "checksum")?;
            if configurations.insert(id, checksum.to_owned()).is_some() {
                bail!("duplicate configured table identity");
            }
        }
        let mut metadata = BTreeMap::new();
        let mut aliases = BTreeMap::new();
        let mut domains: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        let mut raw_occurrences = BTreeMap::new();
        let mut complete_results = BTreeMap::new();
        let mut saw_condition = false;
        for result in results {
            let target = result
                .get("target")
                .context("configured result has no target")?;
            let kind = string(target, "type")?;
            let field = match kind {
                "RULE" => "rule",
                "SOURCE_FILE" => "sourceFile",
                "GENERATED_FILE" => "generatedFile",
                "PACKAGE_GROUP" => "packageGroup",
                "ENVIRONMENT_GROUP" => "environmentGroup",
                _ => bail!("unsupported configured target kind {kind}"),
            };
            let target_value = target
                .get(field)
                .context("configured target lacks typed payload")?;
            let label = canonical(string(target_value, "name")?)?;
            let checksum = result
                .get("configuration")
                .and_then(|c| c.get("checksum"))
                .and_then(Value::as_str)
                .filter(|v| {
                    *v == "null" || (v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
                })
                .context("configured target has no complete configuration identity")?;
            if checksum == "null" {
                if result
                    .get("configurationId")
                    .is_some_and(|id| id.as_u64() != Some(0))
                {
                    bail!("unconfigured result carries a configured identity");
                }
            } else {
                let id = result
                    .get("configurationId")
                    .and_then(Value::as_u64)
                    .context("configured result has no numeric configuration identity")?;
                if configurations.get(&id).map(String::as_str) != Some(checksum) {
                    bail!("configured result disagrees with its configuration table");
                }
            }
            let identity = (label.clone(), checksum.to_owned());
            *raw_occurrences.entry(identity.clone()).or_default() += 1;
            if let Some(previous) = complete_results.get(&identity) {
                // Bazel 6.5 configured-proto can repeat a complete result while
                // config(...,checksum)'s provider projection emits one node.
                // Preserve every raw occurrence and compare the ENTIRE result
                // (including configurationId), not just label or rule class.
                // Provider-bearing repeats are refused at the domain boundary.
                if previous != result {
                    bail!("differing configured results claim one identity: {label}");
                }
                continue;
            }
            complete_results.insert(identity, result.clone());
            *domains
                .entry(checksum.to_owned())
                .or_default()
                .entry(label.clone())
                .or_default() += 1;
            if kind != "RULE" {
                continue;
            }
            let rule = target_value;
            let attributes = attributes(rule)?;
            if label == expected.label {
                if string(rule, "ruleClass")? != "config_setting" {
                    bail!("configured condition is not a genuine config_setting");
                }
                let actual = FeatureFlagPredicate {
                    label: label.clone(),
                    define_values: dictionary(
                        &attributes,
                        "define_values",
                        "STRING_DICT",
                        "stringDictValue",
                        false,
                    )?,
                    native_values: dictionary(
                        &attributes,
                        "values",
                        "STRING_DICT",
                        "stringDictValue",
                        false,
                    )?,
                    flag_values: dictionary(
                        &attributes,
                        "flag_values",
                        "LABEL_KEYED_STRING_DICT",
                        "labelKeyedStringDictValue",
                        true,
                    )?,
                    constraint_values: labels(&attributes, "constraint_values")?,
                };
                if actual != *expected {
                    bail!("Bazel condition predicate disagrees with native declaration: {label}");
                }
                saw_condition = true;
            }
            if string(rule, "ruleClass")? == "alias" {
                let attr = attributes
                    .get("actual")
                    .context("configured alias lacks actual")?;
                if string(attr, "type")? != "LABEL" {
                    bail!("configured alias actual is not a resolved label");
                }
                let actual = canonical(string(attr, "stringValue")?)?;
                if aliases
                    .insert(label.clone(), actual.clone())
                    .is_some_and(|old| old != actual)
                {
                    bail!("alias resolves differently in multiple configurations: {label}");
                }
            }
            if metadata
                .insert((label.clone(), checksum.to_string()), rule.clone())
                .is_some()
            {
                bail!("duplicate configured rule identity: {label}");
            }
        }
        if !saw_condition {
            bail!("configured declaration omitted the requested condition");
        }
        let mut result = Self {
            metadata,
            aliases,
            raw_occurrences,
            domains: BTreeMap::new(),
        };
        for (configuration, labels) in domains {
            let mut canonical = BTreeMap::new();
            for (label, count) in labels {
                *canonical.entry(result.resolve_alias(&label)?).or_default() += count;
            }
            result.domains.insert(configuration, canonical);
        }
        Ok(result)
    }
    fn validate_provider_domain(
        &self,
        configuration: &str,
        bytes: &[u8],
    ) -> Result<ProviderObservations> {
        let observed = ProviderObservations::parse_in_configuration(bytes, configuration)?;
        for ((label, checksum), count) in &self.raw_occurrences {
            if checksum == configuration
                && *count > 1
                && observed
                    .relevant_labels
                    .contains(&self.resolve_alias(label)?)
            {
                bail!("duplicate configured result for provider-bearing target remains ambiguous: {label}");
            }
        }
        if self.domains.get(configuration) != Some(&observed.counts) {
            bail!("configured provider output does not cover its exact declaration/configuration domain");
        }
        Ok(observed)
    }

    fn resolve_alias(&self, label: &str) -> Result<String> {
        let mut current = label.to_string();
        let mut visited = BTreeSet::new();
        while let Some(actual) = self.aliases.get(&current) {
            if !visited.insert(current.clone()) {
                bail!("configured flag alias cycle");
            }
            current = actual.clone();
        }
        Ok(current)
    }
}
struct ProviderObservations {
    relevant_labels: BTreeSet<String>,
    counts: BTreeMap<String, usize>,
    labels: BTreeSet<String>,
    features: BTreeMap<String, String>,
    toolchains: BTreeSet<CompilerMetadata>,
}
impl ProviderObservations {
    fn empty() -> Self {
        Self {
            relevant_labels: BTreeSet::new(),
            counts: BTreeMap::new(),
            labels: BTreeSet::new(),
            features: BTreeMap::new(),
            toolchains: BTreeSet::new(),
        }
    }
    fn merge(&mut self, other: Self) -> Result<()> {
        self.labels.extend(other.labels);
        self.relevant_labels.extend(other.relevant_labels);
        self.toolchains.extend(other.toolchains);
        for (label, value) in other.features {
            if self
                .features
                .insert(label.clone(), value.clone())
                .is_some_and(|old| old != value)
            {
                bail!("feature flag has conflicting values across actual configurations: {label}");
            }
        }
        Ok(())
    }
    fn parse(bytes: &[u8]) -> Result<Self> {
        Self::parse_inner(bytes, None)
    }
    fn parse_in_configuration(bytes: &[u8], configuration: &str) -> Result<Self> {
        Self::parse_inner(bytes, Some(configuration))
    }
    fn parse_inner(bytes: &[u8], configuration: Option<&str>) -> Result<Self> {
        let mut result = Self::empty();
        let mut records = 0;
        for record in serde_json::Deserializer::from_slice(bytes).into_iter::<Value>() {
            let record = record.context("invalid configured provider JSON")?;
            if let Some(expected) = configuration {
                if string(&record, "configuration")? != expected {
                    bail!("provider record has wrong configuration association");
                }
            }
            let label = canonical(string(&record, "label")?)?;
            *result.counts.entry(label.clone()).or_default() += 1;
            result.labels.insert(label.clone());
            records += 1;
            let feature = record
                .get("feature")
                .context("provider record lacks feature field")?;
            if !feature.is_null() {
                result.relevant_labels.insert(label.clone());
                if string(feature, "type")? != "string" {
                    bail!("FeatureFlagInfo value is not string");
                }
                let value = string(feature, "value")?.to_string();
                if result
                    .features
                    .insert(label.clone(), value.clone())
                    .is_some_and(|old| old != value)
                {
                    bail!("feature flag has conflicting configured values: {label}");
                }
            }
            let toolchain = record
                .get("toolchain")
                .context("provider record lacks toolchain field")?;
            if !toolchain.is_null() {
                result.relevant_labels.insert(label.clone());
                result.toolchains.insert(CompilerMetadata {
                    compiler: string(toolchain, "compiler")?.into(),
                    executable: string(toolchain, "executable")?.into(),
                    cpu: string(toolchain, "cpu")?.into(),
                    toolchain_id: string(toolchain, "toolchain_id")?.into(),
                });
            }
        }
        if records == 0 {
            bail!("configured provider query returned no JSON records");
        }
        Ok(result)
    }
}
fn string<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("configured metadata lacks string {name}"))
}
fn canonical(label: &str) -> Result<String> {
    if !label.starts_with("//") && !label.starts_with('@') {
        bail!("configured metadata label is not absolute");
    }
    Ok(crate::dependency_graph::Label::parse(label, None, None)?.to_string())
}
fn query_label(label: &str) -> Result<String> {
    let label = canonical(label)?;
    if label
        .chars()
        .any(|c| c.is_whitespace() || "'\"()[],".contains(c))
    {
        bail!("label cannot be represented in bounded configured query");
    }
    Ok(label)
}
fn attributes(rule: &Value) -> Result<BTreeMap<String, Value>> {
    let mut result = BTreeMap::new();
    for attr in rule
        .get("attribute")
        .and_then(Value::as_array)
        .context("configured rule lacks attributes")?
    {
        let name = string(attr, "name")?.to_string();
        if result.insert(name, attr.clone()).is_some() {
            bail!("duplicate configured attribute");
        }
    }
    Ok(result)
}
fn dictionary(
    attrs: &BTreeMap<String, Value>,
    name: &str,
    kind: &str,
    field: &str,
    label_keys: bool,
) -> Result<BTreeMap<String, String>> {
    let attr = attrs
        .get(name)
        .with_context(|| format!("configured condition lacks {name}"))?;
    if string(attr, "type")? != kind {
        bail!("configured condition {name} has wrong type");
    }
    let mut result = BTreeMap::new();
    if let Some(entries) = attr.get(field) {
        for entry in entries
            .as_array()
            .context("configured dictionary is not an array")?
        {
            let key = string(entry, "key")?;
            let key = if label_keys {
                canonical(key)?
            } else {
                key.to_string()
            };
            if result
                .insert(key, string(entry, "value")?.to_string())
                .is_some()
            {
                bail!("duplicate configured dictionary key");
            }
        }
    }
    Ok(result)
}
fn labels(attrs: &BTreeMap<String, Value>, name: &str) -> Result<BTreeSet<String>> {
    let attr = attrs
        .get(name)
        .context("configured condition lacks constraint_values")?;
    if string(attr, "type")? != "LABEL_LIST" {
        bail!("configured constraints have wrong type");
    }
    attr.get("stringListValue")
        .map(|v| {
            v.as_array()
                .context("configured constraints are not array")?
                .iter()
                .map(|v| canonical(v.as_str().context("configured constraint is not label")?))
                .collect()
        })
        .unwrap_or_else(|| Ok(BTreeSet::new()))
}
fn resolve_tool(
    program: &Path,
    workspace: &Path,
    environment: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<PathBuf> {
    let path = if program.is_absolute() {
        program.to_path_buf()
    } else if program.components().count() > 1 {
        workspace.join(program)
    } else {
        let search = environment
            .get(b"PATH".as_slice())
            .context("configured tool resolution requires captured PATH")?;
        std::env::split_paths(&os_string(search))
            .map(|root| root.join(program))
            .find(|candidate| tracked::metadata_full(candidate))
            .context("configured metadata tool not found on captured PATH")?
    };
    let path = tracked::canonicalize(&path)?;
    if !tracked::metadata_full(&path) {
        bail!("configured metadata tool is unavailable");
    }
    tracked::read_file(&path).context("read configured tool executable identity")?;
    Ok(path)
}
#[cfg(unix)]
fn os_string(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    OsStr::from_bytes(bytes).to_owned()
}
#[cfg(not(unix))]
fn os_string(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}
#[cfg(unix)]
fn bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}
#[cfg(not(unix))]
fn bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

/// Bounded Bazel 6.5 startup profile. Unlike build-option booleans, native
/// startup nullary flags reject '=true/false'. Repeated JVM arguments append
/// in order and each separated operand is one argument, even with whitespace.
/// See startup_options.cc RegisterNullaryStartupFlag, ProcessArg and ProcessArgs
/// in https://github.com/bazelbuild/bazel/tree/6.5.0/src/main/cpp.
fn project_startup_arguments(tokens: &[String]) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if matches!(
            token.as_str(),
            "--windows_enable_symlinks" | "--nowindows_enable_symlinks"
        ) {
            result.push(token.clone());
        } else if token == "--host_jvm_args" {
            index += 1;
            let value = tokens
                .get(index)
                .filter(|value| !value.is_empty())
                .context("configured metadata startup --host_jvm_args requires a value")?;
            result.push(format!("--host_jvm_args={value}"));
        } else if token.starts_with("--host_jvm_args=") {
            result.push(token.clone());
        } else {
            // In particular, user output-base, rc loading, startup protocol and
            // executable overrides cannot displace this analysis-owned context.
            bail!("configured metadata does not support rc startup option {token:?}; supported startup options are --[no]windows_enable_symlinks and --host_jvm_args");
        }
        index += 1;
    }
    Ok(result)
}

/// Remove only known Rush transport/UI controls and target arguments. Unknown
/// separated flags are ambiguous; joined options are passed to genuine Bazel,
/// except controls which could replace this protocol or enable another rc pass.
fn project_build_arguments(tokens: &[String]) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if token == "--" {
            break;
        }
        if !token.starts_with('-') {
            index += 1;
            continue;
        }
        let (name, joined) = token
            .split_once('=')
            .map(|(a, b)| (a, Some(b)))
            .unwrap_or((token, None));
        if name == "--config" {
            bail!("configured metadata received an unexpanded config trigger");
        }
        if matches!(
            name,
            "--output"
                | "--starlark:expr"
                | "--starlark:file"
                | "--universe_scope"
                | "--query_file"
        ) || name.starts_with("--proto:")
            || name == "--bazelrc"
            || matches!(
                name,
                "--system_rc"
                    | "--nosystem_rc"
                    | "--workspace_rc"
                    | "--noworkspace_rc"
                    | "--home_rc"
                    | "--nohome_rc"
                    | "--ignore_all_rc_files"
            )
            || matches!(
                name,
                "--output_base" | "--output_user_root" | "--host_jvm_args"
            )
        {
            bail!("unsupported configured metadata command/startup override {name}");
        }
        if matches!(name, "--daemon" | "--watchfs") {
            if joined.is_none() {
                index += 1;
                tokens
                    .get(index)
                    .context("Rush routing flag requires a value")?;
            }
            index += 1;
            continue;
        }
        if matches!(name, "--verbose" | "-v" | "--announce_rc") {
            index += 1;
            continue;
        }
        let valued = matches!(
            name,
            "-j" | "--jobs"
                | "-c"
                | "--compilation_mode"
                | "--compilation-mode"
                | "--define"
                | "--cpu"
                | "--apple_platform_type"
                | "--crosstool_top"
                | "--copt"
                | "--features"
                | "--strip"
                | "--repo_env"
                | "--action_env"
                | "--repository_cache"
                | "--override_repository"
                | "--platforms"
                | "--extra_toolchains"
        );
        let boolean = matches!(
            name,
            "--stamp"
                | "--nostamp"
                | "--collect_code_coverage"
                | "--nocollect_code_coverage"
                | "--enable_platform_specific_config"
                | "--noenable_platform_specific_config"
                | "--experimental_repo_remote_exec"
                | "--experimental_cc_shared_library"
                | "--incompatible_enforce_config_setting_visibility"
                | "--noincompatible_enforce_config_setting_visibility"
                | "--noincompatible_remove_legacy_whole_archive"
                | "--experimental_guard_against_concurrent_changes"
        );
        let canonical_name = match name {
            "--compilation-mode" | "-c" => "--compilation_mode",
            "-j" => "--jobs",
            other => other,
        };
        if valued {
            let value = if let Some(value) = joined {
                value
            } else {
                index += 1;
                tokens
                    .get(index)
                    .context("configured option requires a value")?
            };
            result.push(format!("{canonical_name}={value}"));
        } else if joined.is_some() || boolean {
            if !name.starts_with("--") {
                bail!("unsupported short configured option {name}");
            }
            result.push(token.clone());
        } else {
            bail!("ambiguous/unsupported configured build option {token}");
        }
        index += 1;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_startup_projection_keeps_order_boundaries_and_refuses_overrides() {
        let tokens = [
            "--windows_enable_symlinks",
            "--host_jvm_args",
            "-Dkey=two words",
            "--host_jvm_args=-Xmx1g",
            "--nowindows_enable_symlinks",
            "--host_jvm_args=",
        ]
        .map(str::to_owned);
        assert_eq!(
            project_startup_arguments(&tokens).unwrap(),
            [
                "--windows_enable_symlinks",
                "--host_jvm_args=-Dkey=two words",
                "--host_jvm_args=-Xmx1g",
                "--nowindows_enable_symlinks",
                "--host_jvm_args=",
            ]
        );
        for invalid in [
            vec!["--host_jvm_args"],
            vec!["--host_jvm_args", ""],
            vec!["--windows_enable_symlinks=true"],
            vec!["--nowindows_enable_symlinks=false"],
            vec!["--output_base=/shared"],
            vec!["--output_base", "/shared"],
            vec!["--output_user_root=/shared"],
            vec!["--ignore_all_rc_files"],
            vec!["--bazelrc=/shared"],
            vec!["--invocation_policy={} "],
            vec!["--server_javabase=/other"],
            vec!["--batch"],
            vec!["--future_startup"],
            vec!["--"],
        ] {
            assert!(
                project_startup_arguments(
                    &invalid.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>()
                )
                .is_err(),
                "{invalid:?}"
            );
        }
        // The unary operand remains data, even when option-shaped: it cannot
        // override our own output-base argument at Bazel's startup boundary.
        assert_eq!(
            project_startup_arguments(&["--host_jvm_args".into(), "--output_base=/data".into()])
                .unwrap(),
            ["--host_jvm_args=--output_base=/data"]
        );
    }
    use serde_json::json;

    fn predicate() -> FeatureFlagPredicate {
        FeatureFlagPredicate {
            label: "//cfg:enabled".into(),
            define_values: BTreeMap::from([("MODE".into(), "a".into())]),
            native_values: BTreeMap::from([("cpu".into(), "k8".into())]),
            constraint_values: BTreeSet::from(["@platforms//os:linux".into()]),
            flag_values: BTreeMap::from([("//cfg:alias".into(), "a".into())]),
        }
    }
    fn metadata() -> Value {
        let attr = |name: &str, kind: &str, field: &str, value: Value| json!({"name":name,"type":kind,field:value});
        let result = |name: &str, class: &str, attrs: Vec<Value>| {
            json!({
            "target":{"type":"RULE","rule":{"name":name,"ruleClass":class,"attribute":attrs}},
            "configuration":{"checksum":"a".repeat(64)},"configurationId":1})
        };
        let mut metadata = json!({"configurations":[{"id":1,"checksum":"a".repeat(64),"mnemonic":"k8-fastbuild","platformName":"k8"}],"results":[result("//cfg:enabled","config_setting",vec![
            attr("define_values","STRING_DICT","stringDictValue",json!([{"key":"MODE","value":"a"}])),
            attr("values","STRING_DICT","stringDictValue",json!([{"key":"cpu","value":"k8"}])),
            attr("flag_values","LABEL_KEYED_STRING_DICT","labelKeyedStringDictValue",json!([{"key":"//cfg:alias","value":"a"}])),
            attr("constraint_values","LABEL_LIST","stringListValue",json!(["@platforms//os:linux"])),
        ]),result("//cfg:alias","alias",vec![attr("actual","LABEL","stringValue",json!("//cfg:mode"))])]});
        metadata["results"].as_array_mut().unwrap().push(result(
            "//cfg:mode",
            "config_feature_flag",
            vec![],
        ));
        metadata
    }
    #[test]
    fn configured_flag_metadata_preserves_actual_predicates_and_alias_identity() {
        let parsed =
            ConfiguredDeclaration::parse(&serde_json::to_vec(&metadata()).unwrap(), &predicate())
                .unwrap();
        assert_eq!(parsed.resolve_alias("//cfg:alias").unwrap(), "//cfg:mode");
        assert_eq!(parsed.metadata.len(), 3);
    }
    #[test]
    fn configured_flag_metadata_refuses_mismatched_missing_and_duplicate_attributes() {
        let mut wrong = metadata();
        wrong["results"][0]["target"]["rule"]["attribute"][2]["labelKeyedStringDictValue"][0]
            ["value"] = json!("b");
        let mut duplicate = metadata();
        let attr = duplicate["results"][0]["target"]["rule"]["attribute"][0].clone();
        duplicate["results"][0]["target"]["rule"]["attribute"]
            .as_array_mut()
            .unwrap()
            .push(attr);
        let mut wrong_class = metadata();
        wrong_class["results"][0]["target"]["rule"]["ruleClass"] = json!("filegroup");
        for invalid in [wrong, duplicate, wrong_class, json!({"results":[]})] {
            assert!(ConfiguredDeclaration::parse(
                &serde_json::to_vec(&invalid).unwrap(),
                &predicate()
            )
            .is_err());
        }
    }

    #[test]
    fn configured_flag_proto_repeats_preserve_raw_occurrences_and_exact_provider_coverage() {
        let mut proto = metadata();
        let duplicate = proto["results"][0].clone();
        proto["results"].as_array_mut().unwrap().push(duplicate);
        let declaration =
            ConfiguredDeclaration::parse(&serde_json::to_vec(&proto).unwrap(), &predicate())
                .unwrap();
        let checksum = "a".repeat(64);
        assert_eq!(
            declaration.raw_occurrences[&("//cfg:enabled".into(), checksum.clone())],
            2
        );
        assert_eq!(declaration.domains[&checksum]["//cfg:enabled"], 1);
        assert_eq!(declaration.metadata.len(), 3);
        let row = |label: &str| json!({"label":label,"configuration":checksum,"feature":null,"toolchain":null});
        let projected = [row("//cfg:enabled"), row("//cfg:mode"), row("//cfg:mode")];
        let bytes = projected
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        declaration
            .validate_provider_domain(&checksum, bytes.as_bytes())
            .unwrap();
        // A repeated JSON result does not authorize a second provider node.
        assert!(declaration
            .validate_provider_domain(
                &checksum,
                format!("{bytes}\n{}", row("//cfg:enabled")).as_bytes()
            )
            .is_err());
        // No missing alias/producer node can be hidden by the formatter profile.
        assert!(declaration
            .validate_provider_domain(
                &checksum,
                projected[..2]
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .as_bytes()
            )
            .is_err());
    }

    #[test]
    fn configured_flag_proto_repeats_refuse_differing_rows_and_provider_ambiguity() {
        for changed in ["rule", "configurationId"] {
            let mut proto = metadata();
            let mut duplicate = proto["results"][0].clone();
            if changed == "rule" {
                duplicate["target"]["rule"]["location"] = json!("different.BUILD:1:1");
            } else {
                duplicate["configurationId"] = json!(99);
            }
            proto["results"].as_array_mut().unwrap().push(duplicate);
            assert!(ConfiguredDeclaration::parse(
                &serde_json::to_vec(&proto).unwrap(),
                &predicate()
            )
            .is_err());
        }
        let mut proto = metadata();
        let duplicate = proto["results"][2].clone();
        proto["results"].as_array_mut().unwrap().push(duplicate);
        let declaration =
            ConfiguredDeclaration::parse(&serde_json::to_vec(&proto).unwrap(), &predicate())
                .unwrap();
        let checksum = "a".repeat(64);
        let condition = json!({"label":"//cfg:enabled","configuration":checksum,"feature":null,"toolchain":null});
        let producer = json!({"label":"//cfg:mode","configuration":checksum,"feature":{"type":"string","value":"a"},"toolchain":null});
        let rows = [
            condition.to_string(),
            producer.to_string(),
            producer.to_string(),
        ]
        .join("\n");
        let error = declaration
            .validate_provider_domain(&checksum, rows.as_bytes())
            .err()
            .unwrap();
        assert!(error.to_string().contains("provider-bearing"));
        // A genuinely distinct configuration stays a distinct identity/domain.
        let mut proto = metadata();
        let mut other = proto["results"][2].clone();
        other["configuration"]["checksum"] = json!("b".repeat(64));
        other["configurationId"] = json!(2);
        proto["configurations"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":2,"checksum":"b".repeat(64)}));
        proto["results"].as_array_mut().unwrap().push(other);
        let declaration =
            ConfiguredDeclaration::parse(&serde_json::to_vec(&proto).unwrap(), &predicate())
                .unwrap();
        assert_eq!(declaration.domains.len(), 2);
        assert_eq!(
            declaration.raw_occurrences[&("//cfg:mode".into(), "a".repeat(64))],
            1
        );
        assert_eq!(
            declaration.raw_occurrences[&("//cfg:mode".into(), "b".repeat(64))],
            1
        );
    }

    #[test]
    fn configured_flag_providers_require_typed_complete_nonconflicting_json() {
        let row =
            json!({"label":"@//cfg:mode","feature":{"type":"string","value":"a"},"toolchain":null});
        let valid = serde_json::to_vec(&row).unwrap();
        assert_eq!(
            ProviderObservations::parse(&valid).unwrap().features["//cfg:mode"],
            "a"
        );
        let mut conflict = row.clone();
        conflict["feature"]["value"] = json!("b");
        let mut conflicting = valid.clone();
        conflicting.extend(serde_json::to_vec(&conflict).unwrap());
        let mut wrong_type = row.clone();
        wrong_type["feature"] = json!({"type":"bool","value":true});
        for invalid in [
            Vec::new(),
            b"formatter error".to_vec(),
            conflicting,
            serde_json::to_vec(&wrong_type).unwrap(),
            b"{\"label\":\"//cfg:mode\"}".to_vec(),
        ] {
            assert!(ProviderObservations::parse(&invalid).is_err());
        }
    }
    #[test]
    fn configured_flag_projection_preserves_full_order_and_refuses_ambiguity() {
        let tokens = [
            "--define",
            "KEY=base",
            "//pkg:app",
            "--copt=-DVALUE=a b",
            "--daemon=auto",
            "-j",
            "8",
            "--compilation-mode",
            "opt",
            "--define=KEY=last",
            "--announce_rc",
        ]
        .map(str::to_owned);
        assert_eq!(
            project_build_arguments(&tokens).unwrap(),
            [
                "--define=KEY=base",
                "--copt=-DVALUE=a b",
                "--jobs=8",
                "--compilation_mode=opt",
                "--define=KEY=last"
            ]
        );
        for invalid in [
            vec!["--config=unexpanded"],
            vec!["--output=json"],
            vec!["--bazelrc=other"],
            vec!["--output_base=other"],
            vec!["--unknown", "value"],
            vec!["-j"],
        ] {
            assert!(project_build_arguments(
                &invalid.into_iter().map(str::to_owned).collect::<Vec<_>>()
            )
            .is_err());
        }
    }
    #[cfg(unix)]
    #[test]
    fn configured_flag_compiler_agreement_accepts_transparent_wrapper_and_refuses_mismatch() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let configured = temp.path().join("configured-driver");
        let wrapper = temp.path().join("native-wrapper");
        let wrong = temp.path().join("wrong-driver");
        std::fs::write(&configured,"#!/bin/sh\ncase \"$1\" in\n--version|-dumpversion) printf 'driver version 1\\n';;\n-dumpmachine) printf 'target-triple\\n';;\n-dM) printf '#define CONTRACT 1\\n';;\n*) exit 9;;\nesac\n").unwrap();
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nexec '{}' \"$@\"\n", configured.display()),
        )
        .unwrap();
        std::fs::write(&wrong, "#!/bin/sh\nprintf 'different driver\\n'\n").unwrap();
        for path in [&configured, &wrapper, &wrong] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        tracked::test_scope(temp.path(), || {
            let mut analysis = ConfiguredFeatureFlagAnalysis {
                workspace: temp.path().to_owned(),
                executable: configured.clone(),
                startup: vec![],
                flags: vec![],
                environment_keys: vec![],
                environment: BTreeMap::new(),
                fingerprint: tracked::config_fingerprint(),
                native: NativeCompilerSelection {
                    cc: wrapper.clone(),
                    cxx: wrapper.clone(),
                    additional_cxx: vec![],
                },
            };
            let metadata = CompilerMetadata {
                compiler: "observed-family".into(),
                executable: configured.to_str().unwrap().into(),
                cpu: "observed-cpu".into(),
                toolchain_id: "observed-toolchain".into(),
            };
            analysis.validate_compiler_agreement(&metadata).unwrap();
            analysis.native.additional_cxx.push(wrong);
            assert!(analysis.validate_compiler_agreement(&metadata).is_err());
        });
    }
    #[cfg(unix)]
    #[test]
    fn configured_flag_probe_refuses_zero_exit_formatter_errors() {
        let temp = tempfile::tempdir().unwrap();
        tracked::test_scope(temp.path(), || {
            let analysis = ConfiguredFeatureFlagAnalysis {
                workspace: temp.path().to_owned(),
                executable: PathBuf::from("/bin/sh"),
                startup: vec![],
                flags: vec![],
                environment_keys: vec![],
                environment: BTreeMap::new(),
                fingerprint: tracked::config_fingerprint(),
                native: NativeCompilerSelection {
                    cc: PathBuf::new(),
                    cxx: PathBuf::new(),
                    additional_cxx: vec![],
                },
            };
            assert!(analysis
                .probe(
                    Path::new("/bin/sh"),
                    &[
                        b"-c".to_vec(),
                        b"printf 'ERROR: formatter failure\\n' >&2; exit 0".to_vec()
                    ]
                )
                .is_err());
        });
    }
    #[test]
    fn configured_flag_provider_domains_refuse_missing_extra_and_foreign_configuration() {
        let declaration =
            ConfiguredDeclaration::parse(&serde_json::to_vec(&metadata()).unwrap(), &predicate())
                .unwrap();
        let checksum = "a".repeat(64);
        let row = |label: &str| {
            json!({"label":label,"configuration":checksum,"feature":null,"toolchain":null})
                .to_string()
        };
        let complete = [row("//cfg:enabled"), row("//cfg:mode"), row("//cfg:mode")].join("\n");
        declaration
            .validate_provider_domain(&checksum, complete.as_bytes())
            .unwrap();
        assert!(declaration
            .validate_provider_domain(&checksum, row("//cfg:enabled").as_bytes())
            .is_err());
        assert!(declaration
            .validate_provider_domain(
                &checksum,
                format!("{complete}\n{}", row("//extra:foreign")).as_bytes()
            )
            .is_err());
        assert!(declaration
            .validate_provider_domain(&"b".repeat(64), complete.as_bytes())
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn configured_flag_compiler_selection_refuses_resident_override_mismatch() {
        use bytes::Bytes;
        use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
        const CHILD: &str = "RUSH_TEST_FEATURE_FLAG_DRIVER_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","configured_flag_metadata::tests::configured_flag_compiler_selection_refuses_resident_override_mismatch","--nocapture"])
                .env(CHILD,"1").env("RUSH_TF_CRITICAL_CXX","/actual/critical")
                .env_remove("RUSH_CC").env_remove("RUSH_CXX").env_remove("RUSH_GPU_CXX")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let semantics = |critical: &str| {
            CommandSemantics::from_run_request(&RunRequest {
                argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"build")],
                environment: {
                    let mut environment = vec![EnvironmentVariable {
                        name: Bytes::from_static(b"RUSH_TF_CRITICAL_CXX"),
                        value: Bytes::copy_from_slice(critical.as_bytes()),
                    }];
                    if let Some(path) = std::env::var_os("PATH") {
                        environment.push(EnvironmentVariable {
                            name: Bytes::from_static(b"PATH"),
                            value: Bytes::from(bytes(path.as_os_str())),
                        });
                    }
                    environment
                },
                protocol_major: rush_protocol::PROTOCOL_MAJOR,
                protocol_minor: rush_protocol::PROTOCOL_MINOR,
                ..Default::default()
            })
            .unwrap()
        };
        let actual = rush_rules::context::Toolchain::default();
        let error =
            NativeCompilerSelection::from_toolchain(&actual, &semantics("/client/critical"))
                .unwrap_err();
        assert!(error.to_string().contains("restart the daemon"));
        let matched =
            NativeCompilerSelection::from_toolchain(&actual, &semantics("/actual/critical"))
                .unwrap();
        assert!(matched
            .additional_cxx
            .contains(&PathBuf::from("/actual/critical")));
    }

    #[cfg(unix)]
    #[test]
    fn configured_flag_compiler_selection_refuses_same_bare_names_with_different_path() {
        use bytes::Bytes;
        use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "RUSH_TEST_FEATURE_FLAG_PATH_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let temp = tempfile::tempdir().unwrap();
            let host = temp.path().join("host");
            let client = temp.path().join("client");
            std::fs::create_dir(&host).unwrap();
            std::fs::create_dir(&client).unwrap();
            for (root, identity) in [(&host, "host-driver"), (&client, "client-driver")] {
                for name in ["gcc", "g++"] {
                    let path = root.join(name);
                    std::fs::write(&path, format!("#!/bin/sh\nprintf '{identity}\\n'\n")).unwrap();
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
                }
            }
            assert_ne!(
                std::fs::read(host.join("gcc")).unwrap(),
                std::fs::read(client.join("gcc")).unwrap()
            );
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","configured_flag_metadata::tests::configured_flag_compiler_selection_refuses_same_bare_names_with_different_path","--nocapture"])
                .env(CHILD,"1").env("PATH",&host).env("RUSH_TEST_FEATURE_FLAG_CLIENT_PATH",&client)
                .env_remove("RUSH_CC").env_remove("RUSH_CXX").env_remove("RUSH_GPU_CXX")
                .env_remove("RUSH_TF_CRITICAL_CXX").output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let client = std::env::var_os("RUSH_TEST_FEATURE_FLAG_CLIENT_PATH").unwrap();
        let semantics = CommandSemantics::from_run_request(&RunRequest {
            argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"build")],
            environment: vec![EnvironmentVariable {
                name: Bytes::from_static(b"PATH"),
                value: Bytes::from(bytes(client.as_os_str())),
            }],
            protocol_major: rush_protocol::PROTOCOL_MAJOR,
            protocol_minor: rush_protocol::PROTOCOL_MINOR,
            ..Default::default()
        })
        .unwrap();
        let actual = rush_rules::context::Toolchain::default();
        let requested = rush_rules::context::Toolchain::from_environment_lookup(|_| None);
        assert_eq!(actual.cc, PathBuf::from("gcc"));
        assert_eq!(actual.cxx, PathBuf::from("g++"));
        assert_eq!(requested.cc, actual.cc);
        assert_eq!(requested.cxx, actual.cxx);
        let error = NativeCompilerSelection::from_toolchain(&actual, &semantics).unwrap_err();
        assert!(
            error.to_string().contains("execution PATH differs"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn configured_flag_compiler_selection_refuses_host_only_gpu_routing() {
        use bytes::Bytes;
        use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
        const CHILD: &str = "RUSH_TEST_FEATURE_FLAG_GPU_GUARD_CHILD";
        if std::env::var_os(CHILD).is_none() {
            for (name, value) in [
                ("RUSH_GPU_CXX", "/actual/gpu-driver"),
                ("RUSH_CUDA_CXX", "/actual/cuda-driver"),
                ("RUSH_ENABLE_CUDA", "1"),
                ("RUSH_ENABLE_ROCM", "1"),
            ] {
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command.args(["--exact","configured_flag_metadata::tests::configured_flag_compiler_selection_refuses_host_only_gpu_routing","--nocapture"])
                    .env(CHILD,"1").env_remove("RUSH_GPU_CXX").env_remove("RUSH_CUDA_CXX")
                    .env_remove("RUSH_ENABLE_CUDA").env_remove("RUSH_ENABLE_ROCM").env(name,value);
                let output = command.output().unwrap();
                assert!(
                    output.status.success(),
                    "{name}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let environment = std::env::var_os("PATH")
            .map(|path| {
                vec![EnvironmentVariable {
                    name: Bytes::from_static(b"PATH"),
                    value: Bytes::from(bytes(path.as_os_str())),
                }]
            })
            .unwrap_or_default();
        let semantics = CommandSemantics::from_run_request(&RunRequest {
            argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"build")],
            environment,
            protocol_major: rush_protocol::PROTOCOL_MAJOR,
            protocol_minor: rush_protocol::PROTOCOL_MINOR,
            ..Default::default()
        })
        .unwrap();
        let error = NativeCompilerSelection::from_toolchain(
            &rush_rules::context::Toolchain::default(),
            &semantics,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("CPU driver selection only"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn configured_flag_probe_stderr_admission_is_revalidated_with_unchanged_stdout() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let tool = temp.path().join("probe");
        std::fs::write(&tool, "#!/bin/sh\nprintf 'same stdout\\n'\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        tracked::test_scope(temp.path(), || {
            let analysis = ConfiguredFeatureFlagAnalysis {
                workspace: temp.path().to_owned(),
                executable: tool.clone(),
                startup: vec![],
                flags: vec![],
                environment_keys: vec![],
                environment: BTreeMap::new(),
                fingerprint: tracked::config_fingerprint(),
                native: NativeCompilerSelection {
                    cc: PathBuf::new(),
                    cxx: PathBuf::new(),
                    additional_cxx: vec![],
                },
            };
            assert_eq!(analysis.probe(&tool, &[]).unwrap(), b"same stdout\n");
            let io = tracked::current().unwrap();
            let manifest = io.manifest();
            assert!(manifest.deps().iter().any(|dependency| matches!(
                dependency,
                rush_incremental::Dependency::ProcessProbe {
                    compare_stderr: true,
                    ..
                }
            )));
            std::fs::write(
                &tool,
                "#!/bin/sh\nprintf 'same stdout\\n'\nprintf 'ERROR: now unsafe\\n' >&2\n",
            )
            .unwrap();
            assert!(analysis.probe(&tool, &[]).is_err());
            assert!(matches!(
                io.validate_manifest(&manifest).staleness,
                rush_incremental::Staleness::Stale { .. }
            ));
        });
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "requires genuine pinned Bazel 6.5; set RUSH_TEST_BAZEL6_5"]
    fn configured_flag_bazel_65_runtime_alias_domain_change_and_request_reset() {
        use bytes::Bytes;
        use rush_incremental::{RealTrackedIo, TrackedIo};
        use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
        use std::os::unix::ffi::OsStrExt;
        use std::sync::Arc;
        let binary = PathBuf::from(
            std::env::var_os("RUSH_TEST_BAZEL6_5").expect("set pinned Bazel test binary"),
        );
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join("home")).unwrap();
        std::fs::write(
            root.join("WORKSPACE"),
            "workspace(name=\"configured_flag_runtime_test\")\n",
        )
        .unwrap();
        let implementation="def _impl(ctx):\n    return [config_common.FeatureFlagInfo(value=ctx.var.get(\"MODE\", \"a\"))]\nfeature = rule(implementation=_impl)\n";
        std::fs::write(root.join("flag.bzl"), implementation).unwrap();
        std::fs::write(root.join("BUILD"),r#"load(":flag.bzl", "feature")
feature(name="mode")
alias(name="mode_alias",actual=":mode")
config_setting(name="condition",flag_values={":mode_alias":"a"})
config_feature_flag(name="bounded",allowed_values=["a","b"],default_value="a")
config_setting(name="invalid_domain",flag_values={":bounded":"z"})
config_setting(name="compiler_probe",flag_values={"@bazel_tools//tools/cpp:compiler":"unmatched-test-value"})
"#).unwrap();
        let mut env: Vec<_> = std::env::vars_os()
            .map(|(name, value)| {
                (
                    name.as_os_str().as_bytes().to_vec(),
                    value.as_os_str().as_bytes().to_vec(),
                )
            })
            .collect();
        env.retain(|(name, _)| {
            name != b"HOME"
                && name != rush_command_core::native_options::EFFECTIVE_NATIVE_FLAGS_ENV.as_bytes()
                && name
                    != rush_command_core::build_arguments::EFFECTIVE_BUILD_ARGUMENTS_ENV.as_bytes()
        });
        env.push((
            b"HOME".to_vec(),
            root.join("home").as_os_str().as_bytes().to_vec(),
        ));
        let output_base = root.join("output-base");
        let make_semantics = |mode: &str| {
            let mut environment: Vec<_> = env
                .iter()
                .map(|(name, value)| EnvironmentVariable {
                    name: Bytes::copy_from_slice(name),
                    value: Bytes::copy_from_slice(value),
                })
                .collect();
            let tokens = vec!["--jobs=8".to_owned(), format!("--define=MODE={mode}")];
            environment.push(EnvironmentVariable {
                name: Bytes::from(
                    rush_command_core::build_arguments::EFFECTIVE_BUILD_ARGUMENTS_ENV,
                ),
                value: Bytes::from(rush_command_core::build_arguments::encode_with_startup(
                    &tokens,
                    &[
                        "--windows_enable_symlinks".into(),
                        "--host_jvm_args".into(),
                        "-Drush.fixture=two words".into(),
                        "--host_jvm_args=-Drush.fixture=last".into(),
                    ],
                )),
            });
            CommandSemantics::from_run_request(&RunRequest {
                argv: vec![
                    Bytes::from_static(b"rush"),
                    Bytes::from_static(b"build"),
                    Bytes::from_static(b"//:condition"),
                ],
                environment,
                working_directory: Bytes::copy_from_slice(root.as_os_str().as_bytes()),
                protocol_major: rush_protocol::PROTOCOL_MAJOR,
                protocol_minor: rush_protocol::PROTOCOL_MINOR,
                ..Default::default()
            })
            .unwrap()
        };
        let run = |mode: &str, expected: bool, compiler: bool| {
            let semantics = make_semantics(mode);
            let native = NativeCompilerSelection::from_toolchain(
                &rush_rules::context::Toolchain::default(),
                &semantics,
            )
            .unwrap();
            let io = Arc::new(RealTrackedIo::new(semantics.clone(), root));
            tracked::scope(io.clone(), || {
                config_defines::with_configuration_scope("genuine-feature-flag-runtime", || {
                    config_defines::begin_command(&[]);
                    config_defines::consider_config_setting_predicates(
                        None,
                        "",
                        "condition",
                        &Default::default(),
                        &Default::default(),
                        &[],
                        &std::collections::HashMap::from([("//:mode_alias".into(), "a".into())]),
                    );
                    let context = ConfiguredFeatureFlagAnalysis::new(
                        root,
                        &binary,
                        &output_base,
                        &semantics,
                        native,
                    )
                    .unwrap();
                    context
                        .validate_registered_condition(
                            "genuine-feature-flag-runtime",
                            "//:condition",
                        )
                        .unwrap();
                    assert_eq!(
                        config_defines::is_active_config_setting("//:condition"),
                        expected
                    );
                    // No-op repeated validation still traverses genuine configured
                    // domains and cannot borrow values from a previous command.
                    context
                        .validate_registered_condition(
                            "genuine-feature-flag-runtime",
                            "//:condition",
                        )
                        .unwrap();
                    config_defines::consider_config_setting_predicates(
                        None,
                        "",
                        "invalid_domain",
                        &Default::default(),
                        &Default::default(),
                        &[],
                        &std::collections::HashMap::from([("//:bounded".into(), "z".into())]),
                    );
                    let error = context
                        .validate_registered_condition(
                            "genuine-feature-flag-runtime",
                            "//:invalid_domain",
                        )
                        .unwrap_err();
                    assert!(error.to_string().contains("not a valid value"), "{error:#}");
                    if compiler {
                        config_defines::consider_config_setting_predicates(
                            None,
                            "",
                            "compiler_probe",
                            &Default::default(),
                            &Default::default(),
                            &[],
                            &std::collections::HashMap::from([(
                                "@bazel_tools//tools/cpp:compiler".into(),
                                "unmatched-test-value".into(),
                            )]),
                        );
                        context
                            .validate_registered_condition(
                                "genuine-feature-flag-runtime",
                                "//:compiler_probe",
                            )
                            .unwrap();
                        assert!(!config_defines::is_active_config_setting(
                            "//:compiler_probe"
                        ));
                    }
                })
            });
            assert!(io.manifest().deps().iter().any(|dep| matches!(
                dep,
                rush_incremental::Dependency::ProcessProbe {
                    compare_stderr: true,
                    ..
                }
            )));
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run("a", true, true);
            run("b", false, false);
            run("a", true, false);
            std::fs::write(root.join("flag.bzl"),"def _impl(ctx):\n    return [config_common.FeatureFlagInfo(value=\"b\")]\nfeature = rule(implementation=_impl)\n# implementation changed independently of flags\n").unwrap();
            run("a", false, false);
            std::fs::write(root.join("flag.bzl"), implementation).unwrap();
            run("a", true, false);
        }));
        let mut shutdown = std::process::Command::new(&binary);
        shutdown
            .arg("--windows_enable_symlinks")
            .arg("--host_jvm_args=-Drush.fixture=two words")
            .arg("--host_jvm_args=-Drush.fixture=last")
            .arg("--ignore_all_rc_files")
            .arg(format!("--output_base={}", output_base.display()))
            .arg("--max_idle_secs=30")
            .arg("shutdown")
            .current_dir(root)
            .env_clear();
        for (name, value) in &env {
            shutdown.env(os_string(name), os_string(value));
        }
        let shutdown = shutdown.output().unwrap();
        assert!(
            shutdown.status.success(),
            "own Bazel server shutdown failed: {}",
            String::from_utf8_lossy(&shutdown.stderr)
        );
        if let Err(payload) = outcome {
            std::panic::resume_unwind(payload);
        }
    }
}
