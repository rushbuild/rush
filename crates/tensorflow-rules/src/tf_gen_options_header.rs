// TensorFlow options headers consume genuine configured BuildSettingInfo
// values. Literal substitutions preserve the template's bytes and EOF; raw
// build-setting defaults are not substitutes for provider implementation values.

use anyhow::{anyhow, ensure, Result};
use async_trait::async_trait;
use rush_rules::{
    context::{BuildContext, NativeCompileMode},
    extract_string, Artifact, ArtifactType, BuildRule, RuleInstance, Value,
};
use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path};

pub struct TfGenOptionsHeaderRule;

impl BuildRule for TfGenOptionsHeaderRule {
    fn name(&self) -> &str {
        "tf_gen_options_header"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        Ok(Box::new(TfGenOptionsHeaderInstance::parse(attrs)?))
    }

    fn documentation(&self) -> &str {
        "Expand an options template using genuine configured boolean BuildSettingInfo values."
    }
}

struct TfGenOptionsHeaderInstance {
    name: String,
    template: String,
    output_header: String,
    build_settings: BTreeMap<String, String>,
}

impl TfGenOptionsHeaderInstance {
    fn parse(attrs: &HashMap<String, Value>) -> Result<Self> {
        let name = extract_string(attrs, "name")?;
        let template = extract_string(attrs, "template")?;
        let output_header = extract_string(attrs, "output_header")?;
        ensure!(
            !output_header.is_empty()
                && Path::new(&output_header)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "options output_header must remain inside its package output directory"
        );
        let mut build_settings = BTreeMap::new();
        match attrs.get("build_settings") {
            None => (),
            Some(Value::Dict(values)) => {
                for (label, identifier) in values {
                    let Value::String(identifier) = identifier else {
                        return Err(anyhow!("options build_settings values must be strings"));
                    };
                    ensure!(!label.is_empty(), "options build_settings label is empty");
                    build_settings.insert(label.clone(), identifier.clone());
                }
            }
            Some(_) => {
                return Err(anyhow!(
                    "options build_settings must be a label-keyed string dictionary"
                ))
            }
        }
        Ok(Self {
            name,
            template,
            output_header,
            build_settings,
        })
    }

    fn check_request(
        &self,
        inputs: &rush_rules::providers::VerifiedOptionsHeaderInputs,
        package: &str,
        repository: Option<&str>,
    ) -> Result<()> {
        let prefix = repository
            .filter(|r| !r.is_empty())
            .map(|r| format!("@{r}//"))
            .unwrap_or_else(|| "//".to_owned());
        let consumer = format!("{prefix}{package}:{}", self.name);
        let output = format!("{prefix}{package}:{}", self.output_header);
        ensure!(
            inputs.request.consumer_label == consumer && inputs.request.output_label == output,
            "configured options record belongs to another consumer/output"
        );
        ensure!(
            rush_rules::providers::dep_label_matches_repository_exact(
                &inputs.request.template_label,
                &self.template,
                package,
                repository
            ),
            "configured options template label differs from this rule"
        );
        ensure!(
            inputs.request.settings.len() == self.build_settings.len(),
            "configured options setting count differs from this rule"
        );
        for (label, identifier) in &self.build_settings {
            let matches = inputs
                .request
                .settings
                .iter()
                .filter(|(canonical, value)| {
                    *value == identifier
                        && rush_rules::providers::dep_label_matches_repository_exact(
                            canonical, label, package, repository,
                        )
                })
                .count();
            ensure!(
                matches == 1,
                "configured options label/identifier differs from this rule: {label}"
            );
        }
        Ok(())
    }

    fn analysis_template(
        &self,
        ctx: &rush_rules::analysis_context::AnalysisContext,
    ) -> Result<rush_action::ids::ArtifactId> {
        let inputs = ctx
            .options_header_inputs()
            .ok_or_else(|| anyhow!("options header has no target-specific configured inputs"))?;
        self.check_request(inputs, ctx.package(), ctx.workspace())?;
        let matches = ctx
            .labeled_dep_providers()
            .iter()
            .filter(|(label, _)| label == &inputs.request.template_label)
            .map(|(_, provider)| provider)
            .collect::<Vec<_>>();
        ensure!(
            matches.len() == 1,
            "options template requires one exact labeled dependency provider"
        );
        let files = matches[0].get_default_info().ok_or_else(|| {
            anyhow!("options template dependency has no actual DefaultInfo artifacts")
        })?;
        ensure!(
            files.files.len() == 1,
            "options template requires one actual SOURCE artifact"
        );
        let id = files.files[0];
        let registry = ctx.registry();
        ensure!(
            (id.0 as usize) < registry.artifact_count(),
            "options template artifact is outside registry"
        );
        let artifact = registry.artifact(id);
        ensure!(artifact.is_source && artifact.owner_action.is_none()
            && artifact.owner_action_declared.is_none()
            && registry.artifact_path(id).as_os_str() == inputs.request.template_path.as_os_str()
            && registry.find_artifact_by_path(registry.artifact_path(id), ctx.config_id()) == Some(id),
            "options template must be the exact same-config SOURCE dependency; generated templates are unsupported");
        Ok(id)
    }

    fn checked_inputs<'a>(
        &self,
        context: &'a BuildContext,
    ) -> Result<&'a rush_rules::providers::VerifiedOptionsHeaderInputs> {
        let inputs = context.options_header_inputs()?;
        let package = context
            .package_path
            .as_deref()
            .and_then(Path::to_str)
            .ok_or_else(|| anyhow!("options header requires its exact UTF-8 package context"))?;
        self.check_request(inputs, package, context.repository.as_deref())?;
        let artifacts = context
            .dependency_artifacts
            .get(&inputs.request.template_label)
            .ok_or_else(|| {
                anyhow!("options template has no exact declared native dependency artifact")
            })?;
        ensure!(artifacts.direct().len() == 1
            && artifacts.direct()[0].artifact_type == ArtifactType::Source
            && artifacts.direct()[0].path == inputs.request.template_path,
            "options template requires the exact single SOURCE dependency artifact; generated templates are unsupported");
        Ok(inputs)
    }
}

/// Render the authentic TF substitutions without trimming lines, normalizing
/// newline bytes or adding a final newline. Input records come from genuine
/// configured analysis; this helper itself grants no provider or writer proof.
pub fn render_options_header(
    template: &[u8],
    inputs: &rush_rules::providers::VerifiedOptionsHeaderInputs,
) -> Result<Vec<u8>> {
    ensure!(
        inputs.settings.len() == inputs.request.settings.len(),
        "options header has incomplete configured settings"
    );
    let mut identifiers = std::collections::BTreeSet::new();
    for identifier in inputs.request.settings.values() {
        ensure!(
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "options header supports nonempty literal macro identifiers only"
        );
        ensure!(
            !identifiers
                .iter()
                .any(|other: &&str| other.starts_with(identifier.as_str())
                    || identifier.starts_with(*other)),
            "options header substitutions require unambiguous native ordering"
        );
        identifiers.insert(identifier.as_str());
    }
    let mut rendered = template.to_vec();
    for (requested, identifier) in &inputs.request.settings {
        let setting = inputs.settings.get(requested).ok_or_else(|| {
            anyhow!("options header is missing configured BuildSettingInfo for {requested}")
        })?;
        ensure!(
            setting.requested_label == *requested
                && setting.provider_key == inputs.request.provider_key
                && setting.configuration_checksum == inputs.consumer_configuration
                && setting.configuration_id == inputs.consumer_configuration_id
                && setting.observed_occurrences > 0,
            "options header setting has inconsistent constructor/configuration provenance"
        );
        let needle = format!("#define_option {identifier}").into_bytes();
        let replacement = format!(
            "// {}\n#define TF_OPTION_{}() {}",
            setting.consumed_label,
            identifier,
            if setting.value { "true" } else { "false" }
        );
        let mut next = Vec::new();
        let mut offset = 0;
        while let Some(relative) = rendered[offset..]
            .windows(needle.len())
            .position(|chunk| chunk == needle)
        {
            let start = offset + relative;
            next.extend_from_slice(&rendered[offset..start]);
            next.extend_from_slice(replacement.as_bytes());
            offset = start + needle.len();
        }
        next.extend_from_slice(&rendered[offset..]);
        rendered = next;
    }
    Ok(rendered)
}

#[async_trait]
impl RuleInstance for TfGenOptionsHeaderInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn options_header_declaration(
        &self,
    ) -> Option<rush_rules::providers::OptionsHeaderDeclaration> {
        Some(rush_rules::providers::OptionsHeaderDeclaration {
            template: self.template.clone(),
            output_header: self.output_header.clone(),
            build_settings: self.build_settings.clone(),
        })
    }

    fn validate_analysis_dependencies(
        &self,
        ctx: &rush_rules::analysis_context::AnalysisContext,
    ) -> Result<()> {
        if ctx.options_header_inputs().is_some() {
            self.analysis_template(ctx)?;
        }
        Ok(())
    }

    fn dependencies(&self) -> Vec<String> {
        // Settings are genuine configured-analysis inputs, not native runtime
        // producers. The first profile is explicitly uncached diagnostic only.
        vec![self.template.clone()]
    }

    fn undeclared_source_inputs(&self) -> Vec<String> {
        // The actual source_file dependency and typed action own this input;
        // label spellings are not filesystem paths.
        Vec::new()
    }

    fn declared_outputs(&self) -> Vec<String> {
        vec![self.output_header.clone()]
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let inputs = self.checked_inputs(context)?;
        let package = context.package_path.as_ref().unwrap();
        let out_path = context
            .output_dir
            .join("genfiles")
            .join(package)
            .join(&self.output_header);
        if context.native_compile_mode() == NativeCompileMode::UncachedDiagnosticV1 {
            context.execute_inline_options_header(&out_path).await?;
            return Ok(vec![Artifact {
                path: out_path,
                artifact_type: ArtifactType::Header,
            }]);
        }
        let content = context.read_options_header_template()?;
        let rendered = render_options_header(&content, inputs)?;
        let text = std::str::from_utf8(&rendered).map_err(|_| {
            anyhow!("options header inline writer currently requires a UTF-8 source template")
        })?;
        std::fs::create_dir_all(out_path.parent().unwrap())?;
        rush_rules::generated_io::write_generated_file(&out_path, text)?;
        Ok(vec![Artifact {
            path: out_path,
            artifact_type: ArtifactType::Header,
        }])
    }

    fn providers(&self) -> rush_rules::providers::ProviderSet {
        rush_rules::providers::ProviderSet::empty()
    }

    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> rush_rules::providers::ProviderSet {
        use rush_action::ids::ArtifactKind;
        use rush_rules::providers::{
            extract_cc_infos, CcInfo, DefaultInfo, DirectCcInfo, FilesInfo, ProviderSet,
        };
        let rel = Path::new("genfiles")
            .join(ctx.package())
            .join(&self.output_header);
        let output = ctx.register_generated_artifact(&rel, ArtifactKind::Header, None);
        if ctx.options_header_inputs().is_some() {
            match self.analysis_template(ctx) {
                Ok(template) => {
                    if let Err(error) =
                        ctx.declare_action("GenOptionsHeader", vec![template], vec![output])
                    {
                        ctx.fail_native_namespace_analysis(format!(
                            "options header declaration failed: {error}"
                        ));
                    }
                }
                Err(error) => {
                    ctx.fail_native_namespace_analysis(format!(
                        "options header input admission failed: {error:#}"
                    ));
                }
            }
        } else {
            let _ = ctx.declare_incomplete_action("GenOptionsHeader", Vec::new(), vec![output],
                "options header requires configured provider/source binding and owned inline completion");
        }
        let include_dir = ctx.predicted_output_path(Path::new("genfiles"));
        let direct = DirectCcInfo {
            headers: vec![output],
            include_dirs: vec![ctx.intern_path(&include_dir)],
            ..DirectCcInfo::default()
        };
        let cc_info =
            CcInfo::from_direct_and_deps(direct, extract_cc_infos(dep_providers).into_iter());
        let output_path = ctx.predicted_output_path(&rel);
        ProviderSet::empty()
            .with_cc_info(cc_info)
            .with_files(FilesInfo::new(vec![output_path]))
            .with_default_info(DefaultInfo {
                files: vec![output],
            })
    }

    fn rule_type(&self) -> &str {
        "tf_gen_options_header"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_rules::providers::{
        ConfiguredBuildSettingInfo, OptionsHeaderRequest, OptionsHeaderSource,
        VerifiedOptionsHeaderInputs,
    };

    fn inputs(value: bool) -> VerifiedOptionsHeaderInputs {
        let requested = "//cfg:alias".to_owned();
        let key = "@bazel_skylib//rules:common_settings.bzl%BuildSettingInfo".to_owned();
        VerifiedOptionsHeaderInputs {
            request: OptionsHeaderRequest {
                consumer_label: "//:header".into(),
                template_label: "//:options.tpl".into(),
                output_label: "//:options.h".into(),
                settings: BTreeMap::from([(requested.clone(), "VALUE".into())]),
                template_path: "/options.tpl".into(),
                provider_key: key.clone(),
                expected_implementation_hash: "b".repeat(64),
            },
            command_fingerprint: [1; 32],
            main_workspace_root: "/source".into(),
            consumer_configuration: "a".repeat(64),
            consumer_configuration_id: 1,
            settings: BTreeMap::from([(
                requested.clone(),
                ConfiguredBuildSettingInfo {
                    requested_label: requested,
                    producer_label: "//cfg:flag".into(),
                    consumed_label: "@//cfg:flag".into(),
                    provider_key: key,
                    value,
                    configuration_checksum: "a".repeat(64),
                    configuration_id: 1,
                    observed_occurrences: 2,
                },
            )]),
            template: OptionsHeaderSource {
                label: "//:options.tpl".into(),
                path: "/options.tpl".into(),
                digest: [0; 32],
                metadata: rush_rules::providers::OptionsHeaderSourceMetadata {
                    len: 0,
                    mode: 0o100644,
                    dev: 1,
                    ino: 1,
                    mtime_ns: 0,
                    ctime_ns: 0,
                },
            },
        }
    }

    #[test]
    fn reports_label_addressable_output_header() {
        let attrs = HashMap::from([
            ("name".into(), Value::String("options".into())),
            ("template".into(), Value::String("options.h.tpl".into())),
            (
                "output_header".into(),
                Value::String("generated/options.h".into()),
            ),
        ]);
        let instance = TfGenOptionsHeaderRule.parse_attributes(&attrs).unwrap();
        assert_eq!(instance.declared_outputs(), vec!["generated/options.h"]);
    }

    #[test]
    fn options_header_keeps_setting_labels_and_template_dependencies() {
        let attrs = HashMap::from([
            ("name".into(), Value::String("options".into())),
            ("template".into(), Value::String(":options.tpl".into())),
            ("output_header".into(), Value::String("options.h".into())),
            (
                "build_settings".into(),
                Value::Dict(HashMap::from([
                    ("//cfg:a".into(), Value::String("FIRST".into())),
                    ("@other//cfg:b".into(), Value::String("SECOND".into())),
                ])),
            ),
        ]);
        let instance = TfGenOptionsHeaderRule.parse_attributes(&attrs).unwrap();
        assert_eq!(instance.dependencies(), vec![":options.tpl"]);
        assert!(instance.undeclared_source_inputs().is_empty());
        let declaration = instance.options_header_declaration().unwrap();
        assert_eq!(declaration.template, ":options.tpl");
        assert_eq!(declaration.output_header, "options.h");
        assert_eq!(
            declaration.build_settings,
            BTreeMap::from([
                ("//cfg:a".into(), "FIRST".into()),
                ("@other//cfg:b".into(), "SECOND".into()),
            ])
        );
        let mut wrong = attrs.clone();
        wrong.insert(
            "build_settings".into(),
            Value::Dict(HashMap::from([("//cfg:a".into(), Value::Bool(true))])),
        );
        assert!(TfGenOptionsHeaderRule.parse_attributes(&wrong).is_err());
    }

    #[test]
    fn options_header_literal_bytes_match_genuine_bazel65_template_oracle() {
        // Public-label equivalent of the actual pinned6.5 oracle: repeated
        // placeholders, CRLF, indentation, unknown token and no final newline.
        let template = b"prefix\r\n  #define_option VALUE\r\n#define_option VALUE\nunknown:#define_option UNKNOWN\nend";
        for value in [false, true, false] {
            let info = inputs(value);
            let label = "@//cfg:flag";
            let bit = if value { "true" } else { "false" };
            let expected = format!("prefix\r\n  // {label}\n#define TF_OPTION_VALUE() {bit}\r\n// {label}\n#define TF_OPTION_VALUE() {bit}\nunknown:#define_option UNKNOWN\nend");
            assert_eq!(
                render_options_header(template, &info).unwrap(),
                expected.as_bytes()
            );
        }
        let mut wrong = inputs(true);
        wrong.settings.get_mut("//cfg:alias").unwrap().provider_key =
            "//:foreign.bzl%BuildSettingInfo".into();
        assert!(render_options_header(template, &wrong).is_err());
        wrong = inputs(true);
        wrong.settings.clear();
        assert!(render_options_header(template, &wrong).is_err());
    }
    #[test]
    fn options_header_analysis_uses_only_actual_labeled_source_artifact() {
        use rush_action::ids::{ArtifactKey, ArtifactKind, ArtifactRegistry, ConfigId, TargetId};
        use rush_rules::analysis_context::{AnalysisContext, StubIncludeResolver};
        use rush_rules::providers::{DefaultInfo, ProviderSet};
        use std::sync::Arc;
        for shape in [
            "source",
            "generated",
            "multiple",
            "wrong-label",
            "wrong-config",
            "missing",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let source = root.join("options.tpl");
            std::fs::write(&source, "#define_option VALUE\n").unwrap();
            let mut record = inputs(true);
            record.request.template_path = source.clone();
            record.template.path = source.clone();
            let attrs = HashMap::from([
                ("name".into(), Value::String("header".into())),
                ("template".into(), Value::String(":options.tpl".into())),
                ("output_header".into(), Value::String("options.h".into())),
                (
                    "build_settings".into(),
                    Value::Dict(HashMap::from([(
                        "//cfg:alias".into(),
                        Value::String("VALUE".into()),
                    )])),
                ),
            ]);
            let rule = rush_rules::with_visibility(
                TfGenOptionsHeaderRule.parse_attributes(&attrs).unwrap(),
                Some(vec!["//visibility:public".into()]),
            );
            assert_eq!(
                rule.options_header_declaration().unwrap(),
                rush_rules::providers::OptionsHeaderDeclaration {
                    template: ":options.tpl".into(),
                    output_header: "options.h".into(),
                    build_settings: BTreeMap::from([("//cfg:alias".into(), "VALUE".into())]),
                }
            );
            let mut registry = ArtifactRegistry::new();
            let config = ConfigId(7);
            let exec_path = registry.intern_path(&source);
            let id = registry.intern_artifact(
                ArtifactKey {
                    config: if shape == "wrong-config" {
                        ConfigId(8)
                    } else {
                        config
                    },
                    exec_path,
                },
                ArtifactKind::Source,
                shape != "generated",
                TargetId(3),
                None,
            );
            let provider = Arc::new(ProviderSet::empty().with_default_info(DefaultInfo {
                files: if shape == "multiple" {
                    vec![id, id]
                } else {
                    vec![id]
                },
            }));
            let labeled = if shape == "missing" {
                vec![]
            } else {
                vec![(
                    if shape == "wrong-label" {
                        "@other//:options.tpl".into()
                    } else {
                        "//:options.tpl".into()
                    },
                    provider,
                )]
            };
            let resolver = StubIncludeResolver::new(root.clone());
            let toolchain = rush_rules::context::Toolchain::default();
            let out = root.join("out");
            let mut ctx = AnalysisContext::new(
                TargetId(1),
                config,
                "",
                None,
                &out,
                &mut registry,
                &resolver,
                &toolchain,
                &root,
                &[],
            )
            .with_labeled_dep_providers(&labeled)
            .with_options_header_inputs(Some(&record));
            if shape == "source" {
                rule.validate_analysis_dependencies(&ctx).unwrap();
                let providers = rule.analyze(&mut ctx, &[]);
                assert!(ctx.native_namespace_analysis_error().is_none());
                let registry = ctx.registry();
                assert_eq!(
                    registry.artifact_count(),
                    2,
                    "analyze must not fabricate a second template input"
                );
                let declarations = registry.declared_actions().collect::<Vec<_>>();
                assert_eq!(declarations.len(), 1);
                assert_eq!(declarations[0].1.inputs.as_ref(), &[id]);
                assert_eq!(
                    declarations[0].1.outputs.as_ref(),
                    providers.get_default_info().unwrap().files.as_slice()
                );
                assert!(matches!(
                    declarations[0].1.status,
                    rush_action::ids::ActionDeclarationStatus::Complete
                ));
            } else {
                assert!(
                    rule.validate_analysis_dependencies(&ctx).is_err(),
                    "{shape}"
                );
                rule.analyze(&mut ctx, &[]);
                assert!(ctx.native_namespace_analysis_error().is_some(), "{shape}");
                assert_eq!(ctx.registry().artifact_count(), 2);
                assert_eq!(
                    ctx.registry().declared_action_count(),
                    0,
                    "invalid binding must not declare a ready producer"
                );
            }
        }
    }
}
