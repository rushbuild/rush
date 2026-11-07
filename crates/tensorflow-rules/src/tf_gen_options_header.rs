// TensorFlow tf_gen_options_header(): renders a `.tpl` options header by
// replacing each `#define_option X` placeholder with a concrete
// `#define TF_OPTION_X() <bool>` macro. The `<bool>` comes from the rule's
// `build_settings` attribute: each setting maps a Bazel flag label to a
// macro name, and the macro is set to `true` iff the flag is on for this
// build.
//
// Rush doesn't track Bazel build-setting flags, so we conservatively treat
// every option as off (`false`). That matches the default for every
// setting TF exposes this way — nothing flips one on by default upstream.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rush_rules::{
    context::BuildContext, extract_string, extract_string_opt, Artifact, ArtifactType, BuildRule,
    RuleInstance, Value,
};
use std::collections::{HashMap, HashSet};

pub struct TfGenOptionsHeaderRule;

impl BuildRule for TfGenOptionsHeaderRule {
    fn name(&self) -> &str {
        "tf_gen_options_header"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let template = extract_string(attrs, "template")?;
        let output_header = extract_string(attrs, "output_header")?;
        // build_settings is a dict<string, string> in BUILD files. We only
        // need the values (the TF_OPTION_<name>s that should resolve to
        // `true`). We treat anything listed here as `false` for now (see
        // file header for rationale), so just keep the set of option
        // names we know about to emit a deterministic output.
        let enabled_options: HashSet<String> = match attrs.get("build_settings") {
            Some(Value::Dict(m)) => m
                .iter()
                .filter_map(|(_, v)| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => HashSet::new(),
        };
        // Allow an optional `package` attribute for tests; not normally set.
        let _ = extract_string_opt(attrs, "visibility");
        Ok(Box::new(TfGenOptionsHeaderInstance {
            name,
            template,
            output_header,
            enabled_options,
        }))
    }

    fn documentation(&self) -> &str {
        "Renders a .tpl options header into genfiles, resolving #define_option placeholders."
    }
}

struct TfGenOptionsHeaderInstance {
    name: String,
    template: String,
    output_header: String,
    #[allow(dead_code)]
    enabled_options: HashSet<String>,
}

#[async_trait]
impl RuleInstance for TfGenOptionsHeaderInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        Vec::new()
    }

    fn declared_outputs(&self) -> Vec<String> {
        vec![self.output_header.clone()]
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let package = context
            .package_path
            .as_ref()
            .ok_or_else(|| anyhow!("tf_gen_options_header needs package_path context"))?;
        let template_path = context.workspace_root.join(package).join(&self.template);
        let content = std::fs::read_to_string(&template_path).map_err(|e| {
            anyhow!(
                "tf_gen_options_header {}: can't read template {}: {}",
                self.name,
                template_path.display(),
                e
            )
        })?;

        let rendered: String = content
            .lines()
            .map(|line| {
                let trimmed = line.trim_start();
                if let Some(rest) = trimmed.strip_prefix("#define_option ") {
                    let opt = rest.trim();
                    // Rush always renders as `false` — see file-level comment.
                    format!("#define TF_OPTION_{}() false\n", opt)
                } else {
                    format!("{}\n", line)
                }
            })
            .collect();

        // Output path: genfiles/<package>/<output_header>
        let genfiles = context.output_dir.join("genfiles");
        let out_dir = genfiles.join(package);
        std::fs::create_dir_all(&out_dir)?;
        let out_path = out_dir.join(&self.output_header);
        std::fs::write(&out_path, rendered)?;

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
        use std::path::Path;

        let rel = Path::new("genfiles")
            .join(ctx.package())
            .join(&self.output_header);
        let output = ctx.register_generated_artifact(&rel, ArtifactKind::Header, None);
        let _ = ctx.declare_incomplete_action(
            "GenOptionsHeader",
            Vec::new(),
            vec![output],
            "tf_gen_options_header still executes through legacy BuildContext",
        );

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

    #[test]
    fn reports_label_addressable_output_header() {
        let attrs = HashMap::from([
            ("name".to_string(), Value::String("options".to_string())),
            (
                "template".to_string(),
                Value::String("options.h.tpl".to_string()),
            ),
            (
                "output_header".to_string(),
                Value::String("generated/options.h".to_string()),
            ),
        ]);
        let instance = TfGenOptionsHeaderRule.parse_attributes(&attrs).unwrap();

        assert_eq!(
            instance.declared_outputs(),
            vec!["generated/options.h"]
        );
    }
}
