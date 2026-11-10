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
    native: Option<NativeCompilerSelection>,
    deferred_toolchain: Option<rush_rules::context::Toolchain>,
    semantics: Option<CommandSemantics>,
}

impl ConfiguredFeatureFlagAnalysis {
    pub(crate) fn new(
        workspace: &Path,
        bazel_binary: &Path,
        output_base: &Path,
        semantics: &CommandSemantics,
        native: NativeCompilerSelection,
    ) -> Result<Self> {
        Self::new_inner(workspace, bazel_binary, output_base, semantics, Some(native), None)
    }

    pub(crate) fn new_with_toolchain(
        workspace: &Path, bazel_binary: &Path, output_base: &Path,
        semantics: &CommandSemantics, toolchain: rush_rules::context::Toolchain,
    ) -> Result<Self> {
        Self::new_inner(workspace, bazel_binary, output_base, semantics, None, Some(toolchain))
    }

    fn new_inner(
        workspace: &Path, bazel_binary: &Path, output_base: &Path,
        semantics: &CommandSemantics, native: Option<NativeCompilerSelection>,
        deferred_toolchain: Option<rush_rules::context::Toolchain>,
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
            deferred_toolchain,
            semantics: Some(semantics.clone()),
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
        // Raw boolean settings are independent of a C/C++ driver. Preserve
        // the existing strict CPU compiler contract whenever a string feature
        // participates, including mixed string/bool conditions.
        if !first.values.is_empty() {
            let mut checked = self.clone();
            if checked.native.is_none() {
                checked.native = Some(NativeCompilerSelection::from_toolchain(
                    checked.deferred_toolchain.as_ref().context("string feature proof lacks native toolchain")?,
                    checked.semantics.as_ref().context("string feature proof lacks immutable command")?,
                )?);
            }
            for compiler in &first.toolchains { checked.validate_compiler_agreement(compiler)?; }
        }
        config_defines::install_verified_configured_flag_condition(
            scope,
            generation,
            expected,
            first.values,
            first.bool_values,
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
        let mut bool_values = BTreeMap::new();
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
            if let Some(value) = observed.features.get(&actual) {
                // Native ConfigSetting checks FeatureFlagInfo before raw build
                // settings, even if this producer also has a bool default.
                values.insert(declared.clone(), (actual, value.clone()));
            } else {
                if declared != &actual {
                    bail!("Bazel 6.5 bool flag_values aliases are not supported: {declared}");
                }
                config_defines::parse_bazel_bool_predicate(&expected.flag_values[declared]).map_err(anyhow::Error::msg)?;
                bool_values.insert(declared.clone(), configured_bool_value(&declaration, &observed, &expected.label, &actual)?);
            }
        }
        Ok(SemanticRound {
            metadata: declaration.metadata,
            raw_occurrences: declaration.raw_occurrences,
            domains: declaration.domains,
            values,
            bool_values,
            options: observed.options,
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
        let native = self.native.as_ref().context("compiler agreement lacks native compiler authority")?;
        let mut drivers = vec![
            (native.cc.clone(), "c"),
            (native.cxx.clone(), "c++"),
        ];
        drivers.extend(
            native
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

// The provider constructor is an input from the native API's defining module,
// never a match on the exported name alone. target.files is queried even when
// empty: metadata-only treatment must not discard a producer's real outputs.
const OPTIONS_PROVIDER_EXPRESSION: &str = r#"json.encode({"label":str(target.label),"configuration":"CHECKSUM","provider_keys":sorted([str(k) for k in (providers(target) or {})]),"files":[str(f.path) for f in target.files.to_list()],"info":{"key":PROVIDER_KEY,"type":type(providers(target)[PROVIDER_KEY].value),"value":providers(target)[PROVIDER_KEY].value} if PROVIDER_KEY in (providers(target) or {}) else None,"raw_options":[{"label":str(k),"type":type(v),"value":v} for k,v in (build_options(target) or {}).items() if not str(k).startswith("//command_line_option:")]})"#;

#[derive(Debug, PartialEq, Eq)]
struct OptionsHeaderRound {
    metadata: BTreeMap<(String, String), Value>,
    raw_occurrences: BTreeMap<(String, String), usize>,
    domains: BTreeMap<String, BTreeMap<String, usize>>,
    providers: BTreeMap<(String, String), Vec<Value>>,
    settings: BTreeMap<String, rush_rules::providers::ConfiguredBuildSettingInfo>,
    consumer_configuration: String,
    consumer_configuration_id: u64,
    template_label: String,
}

impl ConfiguredFeatureFlagAnalysis {
    /// Observe genuine configured BuildSettingInfo values for a native options
    /// consumer. This uses the existing immutable query capsule and records all
    /// probes; it does not attach providers, cache results or complete a writer.
    /// Only SOURCE templates and empty-file metadata setting producers qualify.
    pub(crate) fn resolve_options_header_inputs(
        &self,
        request: &rush_rules::providers::OptionsHeaderRequest,
    ) -> Result<std::sync::Arc<rush_rules::providers::VerifiedOptionsHeaderInputs>> {
        validate_options_request(request)?;
        if tracked::config_fingerprint() != self.fingerprint {
            bail!("options header metadata scope differs from its captured native command");
        }
        let source_path = tracked::canonicalize(&request.template_path)?;
        let first_source = tracked::read_canonical_regular_file(&source_path)?
            .context("options header requires descriptor-bound SOURCE template reads")?;
        let first = self.options_header_round(request)?;
        let second = self.options_header_round(request)?;
        let second_path = tracked::canonicalize(&request.template_path)?;
        let second_source = tracked::read_canonical_regular_file(&second_path)?
            .context("options header requires descriptor-bound SOURCE template reads")?;
        if first != second
            || source_path != second_path
            || first_source.metadata != second_source.metadata
            || first_source.contents != second_source.contents
            || tracked::config_fingerprint() != self.fingerprint
        {
            bail!(
                "options header configured inputs or SOURCE template changed between fresh rounds"
            );
        }
        Ok(std::sync::Arc::new(
            rush_rules::providers::VerifiedOptionsHeaderInputs {
                request: request.clone(),
                command_fingerprint: self.fingerprint,
                main_workspace_root: tracked::canonicalize(&self.workspace)?,
                consumer_configuration: first.consumer_configuration,
                consumer_configuration_id: first.consumer_configuration_id,
                settings: first.settings,
                template: rush_rules::providers::OptionsHeaderSource {
                    label: first.template_label,
                    path: source_path,
                    digest: *blake3::hash(&first_source.contents).as_bytes(),
                    metadata: first_source.metadata,
                },
            },
        ))
    }

    fn options_header_round(
        &self,
        request: &rush_rules::providers::OptionsHeaderRequest,
    ) -> Result<OptionsHeaderRound> {
        let expression = format!("deps({})", query_label(&request.consumer_label)?);
        let bytes = self.cquery(
            &expression,
            &[
                "--output=jsonproto",
                "--proto:output_rule_attrs=all",
                "--proto:definition_stack=true",
            ],
        )?;
        let declaration = ConfiguredDeclaration::parse_consumer(
            &bytes,
            &request.consumer_label,
            |rule, attrs| validate_options_consumer(rule, attrs, request),
        )?;
        let mut observations = BTreeMap::new();
        for (checksum, expected) in &declaration.domains {
            // Unconfigured SOURCE nodes remain in the complete JSONproto
            // domain. They have no setting provider and are checked separately.
            if checksum == "null" {
                continue;
            }
            let formatter = OPTIONS_PROVIDER_EXPRESSION
                .replace("CHECKSUM", checksum)
                .replace(
                    "PROVIDER_KEY",
                    &serde_json::to_string(&request.provider_key)?,
                );
            let output = self.cquery(
                &format!("config({expression}, '{checksum}')"),
                &["--output=starlark", &format!("--starlark:expr={formatter}")],
            )?;
            let rows = options_provider_rows(&output, checksum, expected)?;
            for (label, rows) in rows {
                observations.insert((label, checksum.clone()), rows);
            }
        }
        finish_options_round(request, declaration, &bytes, observations)
    }
}

fn validate_options_request(request: &rush_rules::providers::OptionsHeaderRequest) -> Result<()> {
    for label in std::iter::once(&request.consumer_label)
        .chain(std::iter::once(&request.template_label))
        .chain(std::iter::once(&request.output_label))
        .chain(request.settings.keys())
    {
        if canonical(label)? != *label {
            bail!("options header request label is not canonical: {label}");
        }
        query_label(label)?;
    }
    if !request.template_path.is_absolute() {
        bail!("options header template artifact is not absolute");
    }
    let (module, export) = request
        .provider_key
        .rsplit_once('%')
        .context("options header lacks a complete provider constructor identity")?;
    if export != "BuildSettingInfo" || query_label(module)? != module {
        bail!("options header provider constructor identity is not canonical");
    }
    if request.expected_implementation_hash.len() != 64
        || !request
            .expected_implementation_hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        bail!("options header lacks a qualified expected consumer implementation hash");
    }
    // Rule attributes currently carry an unordered dictionary. Refuse any
    // noncommuting substitution order rather than guess which value wins.
    let mut identifiers = BTreeSet::new();
    for identifier in request.settings.values() {
        if identifier.is_empty()
            || !identifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            bail!("options header supports nonempty literal macro identifiers only");
        }
        if identifiers.iter().any(|other: &&str| {
            other.starts_with(identifier.as_str()) || identifier.starts_with(*other)
        }) {
            bail!("options header has duplicate or overlapping substitutions without ordered native attributes");
        }
        identifiers.insert(identifier.as_str());
    }
    Ok(())
}

fn validate_options_consumer(
    rule: &Value,
    attrs: &BTreeMap<String, Value>,
    request: &rush_rules::providers::OptionsHeaderRequest,
) -> Result<()> {
    if string(rule, "ruleClass")? != "tf_gen_options_header" {
        bail!("configured options consumer is not tf_gen_options_header");
    }
    if dictionary(
        attrs,
        "build_settings",
        "LABEL_KEYED_STRING_DICT",
        "labelKeyedStringDictValue",
        true,
    )? != request.settings
    {
        bail!("configured options build_settings differs from the native declaration");
    }
    for (name, kind, expected) in [
        ("template", "LABEL", &request.template_label),
        ("output_header", "OUTPUT", &request.output_label),
    ] {
        let attr = attrs
            .get(name)
            .with_context(|| format!("configured options lacks {name}"))?;
        if string(attr, "type")? != kind || canonical(string(attr, "stringValue")?)? != *expected {
            bail!("configured options {name} differs from native declaration");
        }
    }
    let implementation_attr = attrs
        .get("$rule_implementation_hash")
        .context("configured options lacks genuine implementation metadata")?;
    if string(implementation_attr, "type")? != "STRING" {
        bail!("configured options implementation metadata has the wrong type");
    }
    let implementation = string(implementation_attr, "stringValue")?;
    if implementation.len() != 64 || !implementation.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("configured options implementation metadata is malformed");
    }
    if implementation != request.expected_implementation_hash {
        bail!("configured options consumer implementation differs from the qualified renderer");
    }
    Ok(())
}

fn options_provider_rows(
    bytes: &[u8],
    configuration: &str,
    expected: &BTreeMap<String, usize>,
) -> Result<BTreeMap<String, Vec<Value>>> {
    let mut rows: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in serde_json::Deserializer::from_slice(bytes).into_iter::<Value>() {
        let row = row.context("invalid options provider JSON")?;
        if string(&row, "configuration")? != configuration {
            bail!("options provider has a foreign configuration association");
        }
        let label = canonical(string(&row, "label")?)?;
        let files = row
            .get("files")
            .and_then(Value::as_array)
            .context("options provider lacks complete DefaultInfo.files")?;
        if files.iter().any(|v| v.as_str().is_none()) {
            bail!("options provider file path is not a string");
        }
        let keys = row
            .get("provider_keys")
            .and_then(Value::as_array)
            .context("options provider lacks constructor keys")?;
        let mut unique = BTreeSet::new();
        for key in keys {
            if !unique.insert(
                key.as_str()
                    .context("provider key is not a string")?
                    .to_owned(),
            ) {
                bail!("duplicate options provider constructor key");
            }
        }
        if row.get("info").is_none() {
            bail!("options provider lacks an explicit info result");
        }
        rows.entry(label).or_default().push(row);
    }
    let actual = rows
        .iter()
        .map(|(label, values)| (label.clone(), values.len()))
        .collect::<BTreeMap<_, _>>();
    if &actual != expected {
        bail!("options provider records do not cover the exact alias/configuration domain");
    }
    // Alias+actual projection can legitimately repeat one consumed target.
    // Preserve every row/count and require full payload equality, not label-only
    // deduplication or equality of just the boolean value.
    for values in rows.values() {
        if values.iter().any(|v| v != &values[0]) {
            bail!("options alias observations disagree on the complete provider payload");
        }
    }
    Ok(rows)
}

fn finish_options_round(
    request: &rush_rules::providers::OptionsHeaderRequest,
    declaration: ConfiguredDeclaration,
    bytes: &[u8],
    observations: BTreeMap<(String, String), Vec<Value>>,
) -> Result<OptionsHeaderRound> {
    let raw: Value = serde_json::from_slice(bytes)?;
    let consumer_keys = declaration
        .metadata
        .keys()
        .filter(|(label, _)| label == &request.consumer_label)
        .collect::<Vec<_>>();
    if consumer_keys.len() != 1 || consumer_keys[0].1 == "null" {
        bail!("options consumer must have one unambiguous configured identity");
    }
    let checksum = consumer_keys[0].1.clone();
    let ids = raw["configurations"]
        .as_array()
        .context("missing configuration table")?
        .iter()
        .filter(|row| row.get("checksum").and_then(Value::as_str) == Some(checksum.as_str()))
        .map(|row| {
            row.get("id")
                .and_then(Value::as_u64)
                .filter(|v| *v != 0)
                .context("invalid options configuration ID")
        })
        .collect::<Result<Vec<_>>>()?;
    if ids.len() != 1 {
        bail!("options configuration checksum has ambiguous numeric identity");
    }
    let configuration_id = ids[0];
    let template_label = declaration.resolve_alias(&request.template_label)?;
    let mut templates = 0;
    for row in raw["results"]
        .as_array()
        .context("missing configured results")?
    {
        let target = &row["target"];
        match target.get("type").and_then(Value::as_str) {
            Some("SOURCE_FILE")
                if canonical(string(&target["sourceFile"], "name")?)? == template_label =>
            {
                if string(&row["configuration"], "checksum")? != "null" {
                    bail!("SOURCE template unexpectedly has a configured identity");
                }
                templates += 1;
            }
            Some("GENERATED_FILE")
                if canonical(string(&target["generatedFile"], "name")?)? == template_label =>
            {
                bail!("generated options templates require exact native producer/ArtifactId routing; SOURCE templates only");
            }
            _ => (),
        }
    }
    if templates != 1 {
        bail!("options template is not exactly one authoritative SOURCE_FILE");
    }
    let mut producers = BTreeSet::new();
    let mut settings = BTreeMap::new();
    for requested in request.settings.keys() {
        let producer = declaration.resolve_alias(requested)?;
        if !producers.insert(producer.clone()) {
            bail!("options settings reference one producer through multiple aliases");
        }
        let identities = declaration
            .metadata
            .keys()
            .filter(|(label, _)| label == &producer)
            .collect::<Vec<_>>();
        if identities.len() != 1 || identities[0].1 != checksum {
            bail!("options setting producer is missing or has an ambiguous/foreign configuration: {producer}");
        }
        for label in [requested, &producer] {
            if declaration
                .raw_occurrences
                .get(&(label.clone(), checksum.clone()))
                .is_some_and(|n| *n != 1)
            {
                bail!("options setting has repeated raw configured producer identities");
            }
        }
        let rows = observations
            .get(&(producer.clone(), checksum.clone()))
            .context("options setting has no configured provider observations")?;
        let row = &rows[0];
        if !row["files"].as_array().context("missing files")?.is_empty() {
            bail!("options setting DefaultInfo.files is nonempty; metadata-only native execution would drop producer outputs");
        }
        let info = row
            .get("info")
            .filter(|v| !v.is_null())
            .context("options setting lacks the genuine required BuildSettingInfo provider")?;
        if string(info, "key")? != request.provider_key
            || !row["provider_keys"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_str() == Some(request.provider_key.as_str()))
        {
            bail!("options setting has a foreign BuildSettingInfo constructor");
        }
        if string(info, "type")? != "bool" {
            bail!("options BuildSettingInfo.value must be bool");
        }
        let value = info
            .get("value")
            .and_then(Value::as_bool)
            .context("options provider bool is malformed")?;
        settings.insert(
            requested.clone(),
            rush_rules::providers::ConfiguredBuildSettingInfo {
                requested_label: requested.clone(),
                producer_label: producer,
                consumed_label: string(row, "label")?.to_owned(),
                provider_key: request.provider_key.clone(),
                value,
                configuration_checksum: checksum.clone(),
                configuration_id,
                observed_occurrences: rows.len(),
            },
        );
    }
    Ok(OptionsHeaderRound {
        metadata: declaration.metadata,
        raw_occurrences: declaration.raw_occurrences,
        domains: declaration.domains,
        providers: observations,
        settings,
        consumer_configuration: checksum,
        consumer_configuration_id: configuration_id,
        template_label,
    })
}
// This expression always emits a JSON record, including source files whose
// providers() is None. Errors/empty/missing records cannot silently become false.
const PROVIDER_EXPRESSION: &str = r#"json.encode({"label":str(target.label),"options":[{"label":str(k),"type":type(v),"value":v} for k,v in (build_options(target) or {}).items() if not str(k).startswith("//command_line_option:")],"feature":{"type":type(providers(target)["FeatureFlagInfo"].value),"value":providers(target)["FeatureFlagInfo"].value} if "FeatureFlagInfo" in (providers(target) or {}) else None,"toolchain":{"compiler":providers(target)["CcToolchainInfo"].compiler,"executable":providers(target)["CcToolchainInfo"].compiler_executable,"cpu":providers(target)["CcToolchainInfo"].cpu,"toolchain_id":providers(target)["CcToolchainInfo"].toolchain_id} if "CcToolchainInfo" in (providers(target) or {}) else None})"#;

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
    bool_values: BTreeMap<String, config_defines::VerifiedBoolBuildSetting>,
    options: BTreeMap<(String, String), BTreeMap<String, RawSettingOption>>,
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
        Self::parse_consumer(bytes, &expected.label, |rule, attrs| {
            if string(rule, "ruleClass")? != "config_setting" {
                bail!("configured condition is not a genuine config_setting");
            }
            let actual = FeatureFlagPredicate {
                label: expected.label.clone(),
                define_values: dictionary(
                    attrs,
                    "define_values",
                    "STRING_DICT",
                    "stringDictValue",
                    false,
                )?,
                native_values: dictionary(
                    attrs,
                    "values",
                    "STRING_DICT",
                    "stringDictValue",
                    false,
                )?,
                flag_values: dictionary(
                    attrs,
                    "flag_values",
                    "LABEL_KEYED_STRING_DICT",
                    "labelKeyedStringDictValue",
                    true,
                )?,
                constraint_values: labels(attrs, "constraint_values")?,
            };
            if actual != *expected {
                bail!(
                    "Bazel condition predicate disagrees with native declaration: {}",
                    expected.label
                );
            }
            Ok(())
        })
    }

    fn parse_consumer(
        bytes: &[u8],
        consumer: &str,
        mut validate: impl FnMut(&Value, &BTreeMap<String, Value>) -> Result<()>,
    ) -> Result<Self> {
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
            if label == consumer {
                validate(rule, &attributes)?;
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
        let mut observed = ProviderObservations::parse_in_configuration(bytes, configuration)?;
        for ((label, checksum), rule) in &self.metadata {
            if checksum == configuration && boolean_default(rule)?.is_some() {
                observed.relevant_labels.insert(self.resolve_alias(label)?);
            }
        }
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
/// The native condition query is the build-setting-kind authority. A BOOLEAN
/// attribute alone is deliberately insufficient: ordinary rules may declare it.
fn boolean_default(rule: &Value) -> Result<Option<bool>> {
    let attrs = attributes(rule)?;
    let Some(attr) = attrs.get("build_setting_default") else { return Ok(None); };
    if string(attr, "type")? != "BOOLEAN" { return Ok(None); }
    let value = attr.get("booleanValue").and_then(Value::as_bool)
        .context("BOOLEAN default lacks typed boolean value")?;
    if attr.get("intValue").is_some_and(|v| v.as_i64() != Some(i64::from(value)))
        || attr.get("stringValue").is_some_and(|v| v.as_str() != Some(if value { "true" } else { "false" })) {
        bail!("BOOLEAN default has contradictory typed encodings");
    }
    Ok(Some(value))
}

fn configured_bool_value(
    declaration: &ConfiguredDeclaration, observed: &ProviderObservations,
    condition: &str, producer: &str,
) -> Result<config_defines::VerifiedBoolBuildSetting> {
    let mut result = None;
    for ((label, checksum), rule) in &declaration.metadata {
        if label != producer { continue; }
        // An actual configured condition and its producer must share this
        // configuration, not merely occur somewhere in the query result.
        if !declaration.metadata.contains_key(&(condition.to_owned(), checksum.clone())) {
            bail!("bool producer is outside the condition configuration");
        }
        let default = boolean_default(rule)?.context("flag has no string FeatureFlagInfo or typed BOOLEAN build-setting default")?;
        let options = observed.options.get(&(producer.to_owned(), checksum.clone()))
            .context("bool producer lacks complete typed build_options receipt")?;
        let condition_options = observed.options.get(&(condition.to_owned(), checksum.clone()))
            .context("bool condition lacks complete typed build_options receipt")?;
        if options != condition_options { bail!("condition and bool producer options disagree within configuration"); }
        let value = if let Some(option) = options.get(producer) {
            if option.kind != "bool" { bail!("configured bool option has wrong Starlark type"); }
            option.value.as_bool().context("configured bool option lacks boolean value")?
        } else { default };
        let candidate = config_defines::VerifiedBoolBuildSetting { producer: producer.to_owned(), default, value };
        if result.as_ref().is_some_and(|old| old != &candidate) {
            bail!("bool setting has conflicting defaults/values across actual configurations");
        }
        result = Some(candidate);
    }
    result.context("bool producer is absent from authoritative configured rules")
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RawSettingOption { kind: String, value: Value }

struct ProviderObservations {
    options: BTreeMap<(String, String), BTreeMap<String, RawSettingOption>>,
    relevant_labels: BTreeSet<String>,
    counts: BTreeMap<String, usize>,
    labels: BTreeSet<String>,
    features: BTreeMap<String, String>,
    toolchains: BTreeSet<CompilerMetadata>,
}
impl ProviderObservations {
    fn empty() -> Self {
        Self {
            options: BTreeMap::new(),
            relevant_labels: BTreeSet::new(),
            counts: BTreeMap::new(),
            labels: BTreeSet::new(),
            features: BTreeMap::new(),
            toolchains: BTreeSet::new(),
        }
    }
    fn merge(&mut self, other: Self) -> Result<()> {
        for (key, value) in other.options {
            if self.options.insert(key, value.clone()).is_some_and(|old| old != value) {
                bail!("conflicting configured raw build-setting options");
            }
        }
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
            if let Some(options) = record.get("options") {
                let options = options.as_array().context("raw build-setting options are not a complete list")?;
                let mut values = BTreeMap::new();
                for option in options {
                    let name = canonical(string(option, "label")?)?;
                    let value = RawSettingOption { kind: string(option, "type")?.to_owned(), value: option.get("value").context("raw option lacks value")?.clone() };
                    if values.insert(name, value).is_some() { bail!("duplicate raw build-setting option"); }
                }
                let key = (label.clone(), configuration.unwrap_or_default().to_owned());
                if result.options.insert(key, values.clone()).is_some_and(|old| old != values) {
                    bail!("conflicting raw build-setting options within configuration");
                }
            }
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
        if joined.is_none() && (name.starts_with("--//") || name.starts_with("--@")
            || name.starts_with("--no//") || name.starts_with("--no@")) {
            let raw = name.strip_prefix("--no").filter(|v| v.starts_with("//") || v.starts_with('@'))
                .unwrap_or_else(|| &name[2..]);
            query_label(raw)?;
            result.push(token.clone());
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

    fn options_request() -> rush_rules::providers::OptionsHeaderRequest {
        rush_rules::providers::OptionsHeaderRequest {
            consumer_label: "//:header".into(),
            template_label: "//:options.tpl".into(),
            output_label: "//:options.h".into(),
            template_path: "/source/options.tpl".into(),
            provider_key: "@bazel_skylib//rules:common_settings.bzl%BuildSettingInfo".into(),
            expected_implementation_hash: "b".repeat(64),
            settings: BTreeMap::from([("//:flag_alias".into(), "VALUE".into())]),
        }
    }

    // Minimal public-label form of actual Bazel6.5 oracle02 JSONproto and
    // alias+actual provider projection. Extra raw observations are retained.
    fn options_fixture(value: bool) -> (Value, Vec<Value>) {
        let cfg = "a".repeat(64);
        let rule = |name: &str, kind: &str, attrs: Vec<Value>| {
            serde_json::json!({
                "target":{"type":"RULE","rule":{"name":name,"ruleClass":kind,"attribute":attrs}},
                "configuration":{"checksum":cfg},"configurationId":1})
        };
        let source = serde_json::json!({"target":{"type":"SOURCE_FILE","sourceFile":{"name":"//:options.tpl"}},
                "configuration":{"checksum":"null"},"configurationId":0});
        let consumer = rule(
            "//:header",
            "tf_gen_options_header",
            vec![
                serde_json::json!({"name":"build_settings","type":"LABEL_KEYED_STRING_DICT","labelKeyedStringDictValue":[{"key":"//:flag_alias","value":"VALUE"}]}),
                serde_json::json!({"name":"template","type":"LABEL","stringValue":"//:options.tpl"}),
                serde_json::json!({"name":"output_header","type":"OUTPUT","stringValue":"//:options.h"}),
                serde_json::json!({"name":"$rule_implementation_hash","type":"STRING","stringValue":"b".repeat(64)}),
            ],
        );
        let alias = rule(
            "//:flag_alias",
            "alias",
            vec![serde_json::json!({"name":"actual","type":"LABEL","stringValue":"//:flag"})],
        );
        let producer = rule("//:flag", "bool_flag", vec![]);
        let metadata = serde_json::json!({"configurations":[{"id":1,"checksum":cfg}],
                "results":[consumer,alias,producer,source]});
        let ordinary = serde_json::json!({"label":"@//:header","configuration":cfg,
                "provider_keys":["FileProvider"],"files":["bazel-out/config/bin/options.h"],"info":null,"raw_options":[]});
        let provider = serde_json::json!({"label":"@//:flag","configuration":cfg,
                "provider_keys":["@bazel_skylib//rules:common_settings.bzl%BuildSettingInfo","FileProvider"],"files":[],
                "info":{"key":"@bazel_skylib//rules:common_settings.bzl%BuildSettingInfo","type":"bool","value":value},
                "raw_options":[{"label":"//:flag","type":"bool","value":!value}]});
        (metadata, vec![ordinary, provider.clone(), provider])
    }

    fn options_fixture_round(
        request: &rush_rules::providers::OptionsHeaderRequest,
        metadata: &Value,
        rows: &[Value],
    ) -> Result<OptionsHeaderRound> {
        let bytes = serde_json::to_vec(metadata)?;
        let declaration =
            ConfiguredDeclaration::parse_consumer(&bytes, &request.consumer_label, |rule, attrs| {
                validate_options_consumer(rule, attrs, request)
            })?;
        let cfg = "a".repeat(64);
        let data = rows
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let parsed = options_provider_rows(data.as_bytes(), &cfg, &declaration.domains[&cfg])?;
        let observations = parsed
            .into_iter()
            .map(|(label, values)| ((label, cfg.clone()), values))
            .collect();
        finish_options_round(request, declaration, &bytes, observations)
    }

    #[test]
    fn options_header_metadata_preserves_genuine_provider_alias_and_raw_value_distinction() {
        for value in [false, true, false] {
            let request = options_request();
            let (metadata, rows) = options_fixture(value);
            let round = options_fixture_round(&request, &metadata, &rows).unwrap();
            let setting = &round.settings["//:flag_alias"];
            assert_eq!(setting.producer_label, "//:flag");
            assert_eq!(setting.consumed_label, "@//:flag");
            assert_eq!(setting.provider_key, request.provider_key);
            assert_eq!(setting.value, value);
            assert_eq!(setting.observed_occurrences, 2);
            assert_eq!(setting.configuration_id, 1);
            assert_eq!(
                round.raw_occurrences[&("//:flag_alias".into(), "a".repeat(64))],
                1
            );
            assert_eq!(
                round.providers[&("//:flag".into(), "a".repeat(64))].len(),
                2
            );
            assert_eq!(
                round.providers[&("//:flag".into(), "a".repeat(64))][0]["raw_options"][0]["value"],
                !value
            );
        }
    }

    #[test]
    fn options_header_metadata_refuses_wrong_constructor_type_outputs_and_domain() {
        let request = options_request();
        let (metadata, rows) = options_fixture(true);
        let mut wrong = rows.clone();
        for row in &mut wrong[1..] {
            row["info"]["key"] = "//:foreign.bzl%BuildSettingInfo".into();
        }
        assert!(options_fixture_round(&request, &metadata, &wrong)
            .unwrap_err()
            .to_string()
            .contains("foreign"));
        let mut wrong = rows.clone();
        for row in &mut wrong[1..] {
            row["info"]["type"] = "string".into();
            row["info"]["value"] = "true".into();
        }
        assert!(options_fixture_round(&request, &metadata, &wrong)
            .unwrap_err()
            .to_string()
            .contains("must be bool"));
        let mut wrong = rows.clone();
        for row in &mut wrong[1..] {
            row["files"] = serde_json::json!(["dropped-output"]);
        }
        assert!(options_fixture_round(&request, &metadata, &wrong)
            .unwrap_err()
            .to_string()
            .contains("drop producer outputs"));
        let mut wrong = rows.clone();
        wrong.pop();
        assert!(options_fixture_round(&request, &metadata, &wrong)
            .unwrap_err()
            .to_string()
            .contains("exact alias/configuration domain"));
        let mut wrong = rows.clone();
        wrong[2]["info"]["value"] = false.into();
        assert!(options_fixture_round(&request, &metadata, &wrong)
            .unwrap_err()
            .to_string()
            .contains("complete provider payload"));
        let mut wrong = rows.clone();
        wrong[1]["configuration"] = "c".repeat(64).into();
        assert!(options_fixture_round(&request, &metadata, &wrong)
            .unwrap_err()
            .to_string()
            .contains("foreign configuration"));
        let mut wrong = metadata.clone();
        wrong["results"][0]["configurationId"] = 7.into();
        assert!(options_fixture_round(&request, &wrong, &rows).is_err());
        let mut wrong = metadata.clone();
        let producer = wrong["results"][2].clone();
        wrong["results"].as_array_mut().unwrap().push(producer);
        assert!(options_fixture_round(&request, &wrong, &rows)
            .unwrap_err()
            .to_string()
            .contains("repeated raw"));
    }

    #[test]
    fn options_header_metadata_refuses_changed_consumer_and_generated_or_missing_template() {
        let request = options_request();
        validate_options_request(&request).unwrap();
        let (metadata, rows) = options_fixture(true);
        let mut wrong = metadata.clone();
        wrong["results"][0]["target"]["rule"]["attribute"][0]["labelKeyedStringDictValue"][0]
            ["value"] = "OTHER".into();
        assert!(options_fixture_round(&request, &wrong, &rows)
            .unwrap_err()
            .to_string()
            .contains("native declaration"));
        let mut wrong = metadata.clone();
        wrong["results"][0]["target"]["rule"]["attribute"][3]["stringValue"] =
            "c".repeat(64).into();
        assert!(options_fixture_round(&request, &wrong, &rows)
            .unwrap_err()
            .to_string()
            .contains("qualified renderer"));
        let mut unqualified = request.clone();
        unqualified.expected_implementation_hash.clear();
        assert!(validate_options_request(&unqualified)
            .unwrap_err()
            .to_string()
            .contains("qualified expected"));
        let mut wrong = metadata.clone();
        wrong["results"][3]["target"] = serde_json::json!({"type":"GENERATED_FILE","generatedFile":{"name":"//:options.tpl","generatingRule":"//:writer"}});
        assert!(options_fixture_round(&request, &wrong, &rows)
            .unwrap_err()
            .to_string()
            .contains("SOURCE templates only"));
        let mut wrong = metadata.clone();
        wrong["results"].as_array_mut().unwrap().pop();
        assert!(options_fixture_round(&request, &wrong, &rows)
            .unwrap_err()
            .to_string()
            .contains("SOURCE_FILE"));
        let mut wrong = request.clone();
        wrong
            .settings
            .insert("//:other".into(), "VALUE_SUFFIX".into());
        assert!(validate_options_request(&wrong)
            .unwrap_err()
            .to_string()
            .contains("overlapping"));
    }
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
    fn bool_metadata(default: bool) -> (Value, FeatureFlagPredicate) {
        let mut value = metadata();
        value["results"].as_array_mut().unwrap().remove(1);
        value["results"][0]["target"]["rule"]["attribute"][2]["labelKeyedStringDictValue"] = json!([{"key":"//cfg:mode","value":"True"}]);
        value["results"][1]["target"]["rule"]["ruleClass"] = json!("custom_bool");
        value["results"][1]["target"]["rule"]["attribute"] = json!([{
            "name":"build_setting_default","type":"BOOLEAN", "booleanValue":default,
            "intValue":i64::from(default),"stringValue":if default { "true" } else { "false" }
        }]);
        let mut predicate = predicate();
        predicate.flag_values = BTreeMap::from([("//cfg:mode".into(), "True".into())]);
        (value, predicate)
    }

    fn bool_observed(options: Value) -> ProviderObservations {
        let rows = ["//cfg:enabled", "//cfg:mode"].map(|label| json!({
            "label":label,"configuration":"a".repeat(64),"feature":null,"toolchain":null,"options":options
        }).to_string()).join("\n");
        ProviderObservations::parse_in_configuration(rows.as_bytes(), &"a".repeat(64)).unwrap()
    }

    #[test]
    fn configured_bool_metadata_uses_typed_options_or_authoritative_default() {
        for default in [false, true] {
            let (raw, predicate) = bool_metadata(default);
            let declaration = ConfiguredDeclaration::parse(&serde_json::to_vec(&raw).unwrap(), &predicate).unwrap();
            let observed = bool_observed(json!([]));
            let proof = configured_bool_value(&declaration, &observed, "//cfg:enabled", "//cfg:mode").unwrap();
            assert_eq!((proof.default, proof.value), (default, default));
            for value in [false, true] {
                let observed = bool_observed(json!([{"label":"//cfg:mode","type":"bool","value":value}]));
                let proof = configured_bool_value(&declaration, &observed, "//cfg:enabled", "//cfg:mode").unwrap();
                assert_eq!((proof.default, proof.value), (default, value));
            }
        }
    }

    #[test]
    fn configured_bool_metadata_refuses_incomplete_conflicting_and_malformed_values() {
        let (raw, predicate) = bool_metadata(false);
        let declaration = ConfiguredDeclaration::parse(&serde_json::to_vec(&raw).unwrap(), &predicate).unwrap();
        let checksum = "a".repeat(64);
        let mut missing = bool_observed(json!([]));
        missing.options.remove(&("//cfg:mode".into(), checksum.clone()));
        let mut conflict = bool_observed(json!([]));
        conflict.options.get_mut(&("//cfg:enabled".into(), checksum.clone())).unwrap().insert("//cfg:mode".into(), RawSettingOption { kind:"bool".into(), value:json!(true) });
        for observed in [missing, conflict,
            bool_observed(json!([{"label":"//cfg:mode","type":"string","value":"true"}])),
            bool_observed(json!([{"label":"//cfg:mode","type":"bool","value":"true"}]))] {
            assert!(configured_bool_value(&declaration, &observed, "//cfg:enabled", "//cfg:mode").is_err());
        }
        for (field, wrong) in [("booleanValue", json!("false")), ("intValue", json!(1)), ("stringValue", json!("true"))] {
            let mut bad = raw.clone();
            bad["results"][1]["target"]["rule"]["attribute"][0][field] = wrong;
            let bad = ConfiguredDeclaration::parse(&serde_json::to_vec(&bad).unwrap(), &predicate).unwrap();
            assert!(configured_bool_value(&bad, &bool_observed(json!([])), "//cfg:enabled", "//cfg:mode").is_err());
        }
        let mut transitioned = ConfiguredDeclaration::parse(&serde_json::to_vec(&raw).unwrap(), &predicate).unwrap();
        let other = "b".repeat(64);
        let rule = transitioned.metadata[&("//cfg:mode".into(), checksum.clone())].clone();
        transitioned.metadata.insert(("//cfg:mode".into(),other.clone()),rule);
        let mut observed = bool_observed(json!([]));
        observed.options.insert(("//cfg:mode".into(),other.clone()),BTreeMap::new());
        assert!(configured_bool_value(&transitioned,&observed,"//cfg:enabled","//cfg:mode").is_err());
        transitioned.metadata.insert(("//cfg:enabled".into(),other.clone()),
            transitioned.metadata[&("//cfg:enabled".into(),checksum.clone())].clone());
        let different = BTreeMap::from([("//cfg:mode".into(),RawSettingOption {kind:"bool".into(),value:json!(true)})]);
        observed.options.insert(("//cfg:mode".into(),other.clone()),different.clone());
        observed.options.insert(("//cfg:enabled".into(),other),different);
        assert!(configured_bool_value(&transitioned,&observed,"//cfg:enabled","//cfg:mode").is_err());
        let duplicate = json!({"label":"//cfg:mode","configuration":checksum,"feature":null,"toolchain":null,
            "options":[{"label":"//cfg:mode","type":"bool","value":false},{"label":"//cfg:mode","type":"bool","value":false}]});
        assert!(ProviderObservations::parse_in_configuration(duplicate.to_string().as_bytes(), &checksum).is_err());
    }

    #[test]
    fn configured_bool_projection_preserves_unary_options_and_refuses_malformed_labels() {
        let tokens = ["--//:enabled", "--no//:enabled", "--@repo//cfg:enabled", "--no@repo//cfg:enabled", "--//:enabled=false"].map(str::to_owned);
        assert_eq!(project_build_arguments(&tokens).unwrap(), tokens);
        for invalid in ["--//:bad label", "--@repo//:bad'quote", "--no//:bad,label"] {
            assert!(project_build_arguments(&[invalid.into()]).is_err());
        }
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
                native: Some(NativeCompilerSelection {
                    cc: wrapper.clone(),
                    cxx: wrapper.clone(),
                    additional_cxx: vec![],
                }),
                deferred_toolchain: None,
                semantics: None,
            };
            let metadata = CompilerMetadata {
                compiler: "observed-family".into(),
                executable: configured.to_str().unwrap().into(),
                cpu: "observed-cpu".into(),
                toolchain_id: "observed-toolchain".into(),
            };
            analysis.validate_compiler_agreement(&metadata).unwrap();
            analysis.native.as_mut().unwrap().additional_cxx.push(wrong);
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
                native: Some(NativeCompilerSelection {
                    cc: PathBuf::new(),
                    cxx: PathBuf::new(),
                    additional_cxx: vec![],
                }),
                deferred_toolchain: None,
                semantics: None,
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
                native: Some(NativeCompilerSelection {
                    cc: PathBuf::new(),
                    cxx: PathBuf::new(),
                    additional_cxx: vec![],
                }),
                deferred_toolchain: None,
                semantics: None,
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
    #[ignore = "requires genuine Bazel6.5 and full Skylib1.3 archive; set RUSH_TEST_BAZEL6_5 and RUSH_TEST_SKYLIB_ARCHIVE"]
    fn options_header_genuine_bazel65_values_alias_reset_and_context_binding() {
        use bytes::Bytes;
        use rush_incremental::{RealTrackedIo, TrackedIo};
        use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
        use std::os::unix::ffi::OsStrExt;
        use std::sync::Arc;
        let binary = PathBuf::from(
            std::env::var_os("RUSH_TEST_BAZEL6_5").expect("set genuine pinned Bazel6.5"),
        );
        let archive = PathBuf::from(
            std::env::var_os("RUSH_TEST_SKYLIB_ARCHIVE")
                .expect("set complete authenticated Skylib1.3 archive"),
        )
        .canonicalize()
        .unwrap();
        let escaped = archive
            .as_os_str()
            .as_bytes()
            .iter()
            .map(|&b| {
                if b.is_ascii_alphanumeric() || b"/._-".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect::<String>();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("home")).unwrap();
        std::fs::write(root.join("WORKSPACE"),format!(r###"workspace(name="options_provider_fixture")
load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
http_archive(name="bazel_skylib",urls=["file://{escaped}"],type="tar.gz",sha256="74d544d96f4a5bb630d465ca8bbcfe231e3594e5aae57e1edbf17a6eb3ca2506")
"###)).unwrap();
        // Small real rule fixture implementing the same public TF2.17 template
        // API. The unchanged upstream implementation is separately exercised
        // by the private byte oracle; no replacement BuildSettingInfo is used.
        let qualified_header_source = r###"load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
def _header(ctx):
    substitutions = {}
    for target, identifier in ctx.attr.build_settings.items():
        substitutions["#define_option " + identifier] = "// %s\n#define TF_OPTION_%s() %s" % (target.label, identifier, {True:"true",False:"false"}[target[BuildSettingInfo].value])
    ctx.actions.expand_template(template=ctx.file.template, output=ctx.outputs.output_header, substitutions=substitutions)
    return [DefaultInfo(files=depset([ctx.outputs.output_header]))]
tf_gen_options_header = rule(implementation=_header,attrs={"template":attr.label(allow_single_file=True),"output_header":attr.output(),"build_settings":attr.label_keyed_string_dict(providers=[BuildSettingInfo])})
def _inverted(ctx):
    return [BuildSettingInfo(value=not ctx.build_setting_value)]
inverted_bool = rule(implementation=_inverted,build_setting=config.bool(flag=True))
def _output(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".kept")
    ctx.actions.write(out,"required output")
    return [BuildSettingInfo(value=True),DefaultInfo(files=depset([out]))]
output_bool = rule(implementation=_output,build_setting=config.bool(flag=True))
"###;
        std::fs::write(root.join("header.bzl"), qualified_header_source).unwrap();
        std::fs::write(root.join("foreign.bzl"),"BuildSettingInfo=provider(fields=[\"value\"])\ndef _impl(ctx):\n    return [BuildSettingInfo(value=False)]\nforeign_bool=rule(implementation=_impl,build_setting=config.bool(flag=True))\n").unwrap();
        let template = b"  #define_option VALUE\r\nunknown:#define_option OTHER\nend";
        std::fs::write(root.join("options.tpl"), template).unwrap();
        let mut build = String::from(
            r###"load("@bazel_skylib//rules:common_settings.bzl","bool_flag","string_flag")
load(":header.bzl","tf_gen_options_header","inverted_bool","output_bool")
load(":foreign.bzl","foreign_bool")
bool_flag(name="flag",build_setting_default=False)
string_flag(name="text",build_setting_default="wrong-type")
inverted_bool(name="inverted",build_setting_default=False)
output_bool(name="output_flag",build_setting_default=False)
foreign_bool(name="foreign",build_setting_default=False)
alias(name="flag_alias",actual=":flag")
"###,
        );
        for (name, setting) in [
            ("header", "flag"),
            ("header_alias", "flag_alias"),
            ("header_inverted", "inverted"),
            ("header_string", "text"),
            ("header_foreign", "foreign"),
            ("header_output", "output_flag"),
        ] {
            build.push_str(&format!("tf_gen_options_header(name=\"{name}\",template=\"options.tpl\",output_header=\"{name}.h\",build_settings={{\":{setting}\":\"VALUE\"}})\n"));
        }
        std::fs::write(root.join("BUILD"), build).unwrap();
        let startup = vec![
            "--host_jvm_args=-Xmx512m".to_owned(),
            "--host_jvm_args=-XX:ActiveProcessorCount=8".to_owned(),
        ];
        let environment = vec![
            (
                b"HOME".to_vec(),
                root.join("home").as_os_str().as_bytes().to_vec(),
            ),
            (b"PATH".to_vec(), b"/usr/bin:/bin".to_vec()),
            (b"LANG".to_vec(), b"C.UTF-8".to_vec()),
            (b"CUDA_VISIBLE_DEVICES".to_vec(), Vec::new()),
            (b"HIP_VISIBLE_DEVICES".to_vec(), Vec::new()),
            (b"ROCR_VISIBLE_DEVICES".to_vec(), Vec::new()),
        ];
        let output_base = root.join("output-base");
        let make_command = |flags: &[&str]| {
            let mut tokens = vec!["--jobs=8".to_owned()];
            tokens.extend(flags.iter().map(|s| s.to_string()));
            let mut env = environment
                .iter()
                .map(|(name, value)| EnvironmentVariable {
                    name: Bytes::copy_from_slice(name),
                    value: Bytes::copy_from_slice(value),
                })
                .collect::<Vec<_>>();
            env.push(EnvironmentVariable {
                name: Bytes::from(
                    rush_command_core::build_arguments::EFFECTIVE_BUILD_ARGUMENTS_ENV,
                ),
                value: Bytes::from(rush_command_core::build_arguments::encode_with_startup(
                    &tokens, &startup,
                )),
            });
            CommandSemantics::from_run_request(&RunRequest {
                argv: vec![
                    Bytes::from_static(b"rush"),
                    Bytes::from_static(b"build"),
                    Bytes::from_static(b"//:header"),
                ],
                environment: env,
                working_directory: Bytes::copy_from_slice(root.as_os_str().as_bytes()),
                protocol_major: rush_protocol::PROTOCOL_MAJOR,
                protocol_minor: rush_protocol::PROTOCOL_MINOR,
                ..Default::default()
            })
            .unwrap()
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Test-only qualification: source bytes are the fixed, reviewed fixture
            // above. Production callers must supply their independently source-bound
            // implementation identity, never trust a same-query observed hash.
            let qualification_command = make_command(&[]);
            let qualification_io =
                Arc::new(RealTrackedIo::new(qualification_command.clone(), &root));
            let expected_implementation_hash =
                tracked::scope(qualification_io, || -> Result<String> {
                    anyhow::ensure!(
                        tracked::read_file(&root.join("header.bzl"))?.as_ref()
                            == qualified_header_source.as_bytes()
                    );
                    let analysis = ConfiguredFeatureFlagAnalysis::new_with_toolchain(
                        &root,
                        &binary,
                        &output_base,
                        &qualification_command,
                        rush_rules::context::Toolchain::default(),
                    )?;
                    let bytes = analysis.cquery(
                        "deps(//:header)",
                        &[
                            "--output=jsonproto",
                            "--proto:output_rule_attrs=all",
                            "--proto:definition_stack=true",
                        ],
                    )?;
                    let mut expected = None;
                    ConfiguredDeclaration::parse_consumer(&bytes, "//:header", |rule, attrs| {
                        anyhow::ensure!(string(rule, "ruleClass")? == "tf_gen_options_header");
                        expected = Some(
                            string(
                                attrs
                                    .get("$rule_implementation_hash")
                                    .context("missing fixture implementation")?,
                                "stringValue",
                            )?
                            .to_owned(),
                        );
                        Ok(())
                    })?;
                    anyhow::ensure!(
                        tracked::read_file(&root.join("header.bzl"))?.as_ref()
                            == qualified_header_source.as_bytes()
                    );
                    expected.context("no qualified fixture implementation")
                })
                .unwrap();
            let request_for =
                |name: &str, setting: &str| rush_rules::providers::OptionsHeaderRequest {
                    consumer_label: format!("//:{name}"),
                    template_label: "//:options.tpl".into(),
                    output_label: format!("//:{name}.h"),
                    settings: BTreeMap::from([(format!("//:{setting}"), "VALUE".into())]),
                    template_path: root.join("options.tpl"),
                    provider_key: "@bazel_skylib//rules:common_settings.bzl%BuildSettingInfo"
                        .into(),
                    expected_implementation_hash: expected_implementation_hash.clone(),
                };
            let resolve = |name: &str, setting: &str, flags: &[&str]| {
                let command = make_command(flags);
                let io = Arc::new(RealTrackedIo::new(command.clone(), &root));
                let value = tracked::scope(io.clone(), || {
                    let analysis = ConfiguredFeatureFlagAnalysis::new_with_toolchain(
                        &root,
                        &binary,
                        &output_base,
                        &command,
                        rush_rules::context::Toolchain::default(),
                    )?;
                    analysis.resolve_options_header_inputs(&request_for(name, setting))
                });
                // All genuine query/tool/source observations stay in the manifest;
                // the structured record alone is not a reusable certificate.
                assert!(io
                    .manifest()
                    .deps()
                    .iter()
                    .any(|d| matches!(d, rush_incremental::Dependency::ProcessProbe { .. })));
                (command, value)
            };
            let (default_command, default) = resolve("header", "flag", &[]);
            let default = default.unwrap();
            assert!(!default.settings["//:flag"].value);
            let (override_command, overridden) = resolve("header", "flag", &["--//:flag=true"]);
            let overridden = overridden.unwrap();
            assert!(overridden.settings["//:flag"].value);
            let (_, reset) = resolve("header", "flag", &[]);
            assert!(!reset.unwrap().settings["//:flag"].value);
            let (_, alias) = resolve("header_alias", "flag_alias", &[]);
            let alias = alias.unwrap();
            assert_eq!(alias.settings["//:flag_alias"].producer_label, "//:flag");
            assert_eq!(alias.settings["//:flag_alias"].consumed_label, "@//:flag");
            let (_, inverted) = resolve("header_inverted", "inverted", &[]);
            assert!(inverted.unwrap().settings["//:inverted"].value);
            let (_, inverted) = resolve("header_inverted", "inverted", &["--//:inverted=true"]);
            assert!(!inverted.unwrap().settings["//:inverted"].value);
            for (name, setting) in [
                ("header_string", "text"),
                ("header_foreign", "foreign"),
                ("header_output", "output_flag"),
            ] {
                assert!(
                    resolve(name, setting, &[]).1.is_err(),
                    "unsupported actual provider/output shape must refuse"
                );
            }
            let make_context =
                || rush_rules::context::BuildContext::new(root.clone(), root.join("native-output"));
            assert!(make_context()
                .with_options_header_inputs(default.clone())
                .is_err());
            assert!(make_context()
                .with_options_header_command(override_command)
                .with_options_header_inputs(default.clone())
                .is_err());
            let context = make_context()
                .with_options_header_command(default_command.clone())
                .with_options_header_inputs(default.clone())
                .unwrap();
            assert_eq!(context.read_options_header_template().unwrap(), template);
            let other_root = root.join("other-main");
            std::fs::create_dir(&other_root).unwrap();
            let mut changed_root = make_context()
                .with_options_header_command(default_command.clone())
                .with_options_header_inputs(default.clone())
                .unwrap();
            changed_root.main_workspace_root = other_root;
            assert!(changed_root.options_header_inputs().is_err());
            assert!(changed_root.read_options_header_template().is_err());
            let rendered =
                tensorflow_rules::tf_gen_options_header::render_options_header(template, &default)
                    .unwrap();
            assert_eq!(
                rendered,
                b"  // @//:flag\n#define TF_OPTION_VALUE() false\r\nunknown:#define_option OTHER\nend"
            );
            assert!(context
                .with_options_header_command(default_command.clone())
                .options_header_inputs()
                .is_err());
            std::fs::write(root.join("options.tpl"), b"changed:#define_option VALUE").unwrap();
            let context = make_context()
                .with_options_header_command(default_command)
                .with_options_header_inputs(default.clone())
                .unwrap();
            assert!(context.read_options_header_template().is_err());
            let (_, changed) = resolve("header", "flag", &[]);
            assert_ne!(changed.unwrap().template.digest, default.template.digest);
            // Genuine BuildSettingInfo inversion above is supported. Changing
            // the consumer renderer itself under the same declaration is not.
            let changed_renderer = qualified_header_source.replace(
                "target[BuildSettingInfo].value]",
                "not target[BuildSettingInfo].value]",
            );
            assert_ne!(changed_renderer, qualified_header_source);
            std::fs::write(root.join("header.bzl"), changed_renderer).unwrap();
            let error = resolve("header", "flag", &[]).1.unwrap_err();
            assert!(
                error.to_string().contains("qualified renderer"),
                "{error:#}"
            );
        }));
        let mut shutdown = std::process::Command::new(&binary);
        shutdown
            .args(&startup)
            .arg("--ignore_all_rc_files")
            .arg(format!("--output_base={}", output_base.display()))
            .arg("--max_idle_secs=30")
            .arg("shutdown")
            .current_dir(&root)
            .env_clear();
        for (key, value) in &environment {
            shutdown.env(os_string(key), os_string(value));
        }
        let stopped = shutdown.output().unwrap();
        assert!(
            stopped.status.success(),
            "own Bazel shutdown failed: {}",
            String::from_utf8_lossy(&stopped.stderr)
        );
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
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
