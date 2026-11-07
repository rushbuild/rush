use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rush_rules::{
    context::BuildContext, extract_string, Artifact, ArtifactType, BuildRule, RuleInstance, Value,
};
use std::collections::HashMap;

pub struct ExpandTemplateRule;

impl BuildRule for ExpandTemplateRule {
    fn name(&self) -> &str {
        "expand_template"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let template = extract_string(attrs, "template")?;
        let out = extract_string(attrs, "out")?;
        let substitutions = match attrs.get("substitutions") {
            Some(Value::Dict(values)) => values
                .iter()
                .filter_map(|(key, value)| match value {
                    Value::String(value) => Some((key.clone(), value.clone())),
                    _ => None,
                })
                .collect(),
            Some(Value::List(entries)) => entries
                .iter()
                .filter_map(|entry| match entry {
                    Value::List(pair) => match pair.as_slice() {
                        [Value::String(key), Value::String(value)] => {
                            Some((key.clone(), value.clone()))
                        }
                        _ => None,
                    },
                    _ => None,
                })
                .collect(),
            _ => HashMap::new(),
        };
        let is_executable = matches!(attrs.get("is_executable"), Some(Value::Bool(true)));

        Ok(Box::new(ExpandTemplateInstance {
            name,
            template,
            out,
            substitutions,
            is_executable,
        }))
    }

    fn documentation(&self) -> &str {
        "Renders a template with deterministic string substitutions."
    }
}

struct ExpandTemplateInstance {
    name: String,
    template: String,
    out: String,
    substitutions: HashMap<String, String>,
    is_executable: bool,
}

#[async_trait]
impl RuleInstance for ExpandTemplateInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        if self.template.starts_with(':')
            || self.template.starts_with("//")
            || self.template.starts_with('@')
        {
            vec![self.template.clone()]
        } else {
            Vec::new()
        }
    }

    fn declared_outputs(&self) -> Vec<String> {
        vec![self.out.clone()]
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let template_path = if self.template.starts_with(':')
            || self.template.starts_with("//")
            || self.template.starts_with('@')
        {
            context
                .get_dependency_files(&self.template)
                .and_then(|files| files.into_iter().next())
                .ok_or_else(|| {
                    anyhow!(
                        "expand_template {}: template label {} produced no file",
                        self.name,
                        self.template
                    )
                })?
        } else {
            context.source_path(&self.template)
        };

        let mut rendered = std::fs::read_to_string(&template_path).map_err(|error| {
            anyhow!(
                "expand_template {}: cannot read {}: {}",
                self.name,
                template_path.display(),
                error
            )
        })?;

        let mut substitutions: Vec<_> = self.substitutions.iter().collect();
        substitutions.sort_by(|(left, _), (right, _)| {
            right
                .len()
                .cmp(&left.len())
                .then_with(|| left.cmp(right))
        });
        for (from, to) in substitutions {
            rendered = rendered.replace(from, to);
        }

        let package = context
            .package_path
            .as_ref()
            .map(|path| path.as_path())
            .unwrap_or_else(|| std::path::Path::new(""));
        let output = context
            .output_dir
            .join("genfiles")
            .join(package)
            .join(&self.out);
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&output, rendered)?;

        #[cfg(unix)]
        if self.is_executable {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&output)?.permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&output, permissions)?;
        }

        Ok(vec![Artifact {
            path: output,
            artifact_type: if self.is_executable {
                ArtifactType::Executable
            } else {
                ArtifactType::Generated
            },
        }])
    }

    fn providers(&self) -> rush_rules::providers::ProviderSet {
        rush_rules::providers::ProviderSet::empty()
    }

    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        _dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> rush_rules::providers::ProviderSet {
        use rush_action::ids::ArtifactKind;
        use rush_rules::providers::{DefaultInfo, FilesInfo, ProviderSet};
        use std::path::Path;

        let relative = Path::new("genfiles")
            .join(ctx.package())
            .join(&self.out);
        let output = ctx.register_generated_artifact(&relative, ArtifactKind::Generated, None);
        let _ = ctx.declare_incomplete_action(
            "ExpandTemplate",
            Vec::new(),
            vec![output],
            "template rendering executes through legacy BuildContext",
        );
        let output_path = ctx.predicted_output_path(&relative);

        ProviderSet::empty()
            .with_files(FilesInfo::new(vec![output_path]))
            .with_default_info(DefaultInfo {
                files: vec![output],
            })
    }

    fn rule_type(&self) -> &str {
        "expand_template"
    }
}
