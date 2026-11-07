use anyhow::Result;
use async_trait::async_trait;
use rush_rules::{
    context::BuildContext, extract_string, extract_string_list, providers::ProviderSet, Artifact,
    ArtifactType, BuildRule, RuleInstance, Value,
};
use std::collections::HashMap;
use std::path::PathBuf;

/// td_library() — MLIR TableGen source library (@llvm-project//mlir:tblgen.bzl).
///
/// Groups .td files and exposes their include roots to gentbl_cc_library
/// consumers. TF, @llvm-project//mlir, @local_xla and @stablehlo all model
/// their TableGen dependency DAG with td_library targets
/// (`tensorflow_ops_td_files`, `OpBaseTdFiles`, `hlo_ops_td_files`, …).
/// rush previously stubbed the macro as a no-op in its synthesized
/// tblgen.bzl, so every such target vanished from the graph: gentbl rules
/// warned "depends on X which doesn't exist" and lost the transitive
/// `-I` roots Bazel would have supplied — mlir-tblgen then failed with
/// "Could not find include file 'mhlo/IR/hlo_base.td'" whenever a .td
/// include had to resolve through a dep's `includes` attr rather than the
/// consumer's own package.
///
/// Example (third_party/xla/xla/mlir_hlo/BUILD):
/// ```python
/// td_library(
///     name = "hlo_ops_td_files",
///     srcs = glob(["mhlo/IR/*.td"]),
///     includes = ["."],
///     deps = ["@llvm-project//mlir:OpBaseTdFiles", ...],
/// )
/// ```
///
/// Semantics (per upstream tblgen.bzl, confirmed via Codex review
/// 019ebd35): each `includes` entry resolves package-relative against the
/// DEFINING rule's own repo root — `"."` is the package dir itself,
/// `"/foo"` is repo-root/foo — and transitive include roots accumulate
/// through `deps` in declared order. No implicit repo root is added (the
/// repo root on tblgen's -I comes from the gentbl rule itself, not from
/// td_library deps).
pub struct TdLibraryRule;

#[async_trait]
impl BuildRule for TdLibraryRule {
    fn name(&self) -> &str {
        "td_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs").unwrap_or_default();
        let includes = extract_string_list(attrs, "includes").unwrap_or_default();
        let deps = extract_string_list(attrs, "deps").unwrap_or_default();

        Ok(Box::new(TdLibraryInstance {
            name,
            srcs,
            includes,
            deps,
        }))
    }

    fn documentation(&self) -> &str {
        "MLIR TableGen source library: .td files + include roots for gentbl consumers"
    }
}

struct TdLibraryInstance {
    name: String,
    srcs: Vec<String>,
    includes: Vec<String>,
    deps: Vec<String>,
}

#[async_trait]
impl RuleInstance for TdLibraryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        // deps are td_library labels; srcs are mostly plain .td filenames
        // (which flow as artifacts from build(), not graph edges) but may
        // also be labels (filegroups / cross-package files).
        let mut all = self.deps.clone();
        all.extend(
            self.srcs
                .iter()
                .filter(|s| s.starts_with(':') || s.starts_with("//") || s.starts_with('@'))
                .cloned(),
        );
        all
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        // No actions — a td_library is source-only. Expose the .td files
        // as artifacts so they flow to consumers as dep artifacts.
        let mut artifacts = Vec::new();
        for src in &self.srcs {
            if src.starts_with(':') || src.starts_with("//") || src.starts_with('@') {
                if let Some(dep_artifacts) = context.direct_artifacts(src) {
                    artifacts.extend(dep_artifacts.iter().cloned());
                }
                continue;
            }
            artifacts.push(Artifact {
                path: context.source_path(src),
                artifact_type: ArtifactType::Generated,
            });
        }
        Ok(artifacts)
    }

    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> ProviderSet {
        use rush_action::ids::ArtifactKind;
        use rush_rules::providers::{CcInfo, DirectCcInfo};
        use std::path::Path;

        // Register the .td srcs as source artifacts so the files exist in
        // the artifact graph (no producer — they're repo sources).
        for src in &self.srcs {
            if src.starts_with(':') || src.starts_with("//") || src.starts_with('@') {
                continue;
            }
            ctx.register_source_artifact(Path::new(src), ArtifactKind::Source);
        }

        // STERILE CcInfo: include_dirs only — no headers, defines, or link
        // inputs (a td_library is not a C++ library; gentbl_cc_library
        // consumes dep CcInfo.compilation.include_dirs as tblgen -I roots).
        // resolve_package_include anchors each entry against THIS rule's
        // package + workspace (repo-aware), which is exactly Bazel's
        // td_library `includes` semantics; transitive roots accumulate
        // through deps via from_direct_and_deps.
        let direct_include_dirs: Vec<_> = self
            .includes
            .iter()
            .map(|inc| ctx.resolve_package_include(Path::new(inc)))
            .collect();
        let direct = DirectCcInfo {
            include_dirs: direct_include_dirs,
            ..DirectCcInfo::default()
        };
        let cc_info = CcInfo::from_direct_and_deps(
            direct,
            rush_rules::providers::extract_cc_infos(dep_providers).into_iter(),
        );
        ProviderSet::empty().with_cc_info(cc_info)
    }

    fn providers(&self) -> ProviderSet {
        // Legacy provider path: the .td files (CcInfo include roots come
        // from analyze(), which is the provider source PASS-B threads
        // through to consumers).
        let files: Vec<PathBuf> = self
            .srcs
            .iter()
            .filter(|s| !s.starts_with(':') && !s.starts_with("//") && !s.starts_with('@'))
            .map(PathBuf::from)
            .collect();
        ProviderSet::empty().with_files(rush_rules::providers::FilesInfo::new(files))
    }

    fn rule_type(&self) -> &str {
        "td_library"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_td_library() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "name".to_string(),
            Value::String("hlo_ops_td_files".to_string()),
        );
        attrs.insert(
            "srcs".to_string(),
            Value::List(vec![Value::String("mhlo/IR/hlo_ops.td".to_string())]),
        );
        attrs.insert(
            "includes".to_string(),
            Value::List(vec![Value::String(".".to_string())]),
        );
        attrs.insert(
            "deps".to_string(),
            Value::List(vec![Value::String(
                "@llvm-project//mlir:OpBaseTdFiles".to_string(),
            )]),
        );

        let rule = TdLibraryRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "hlo_ops_td_files");
        let deps = instance.dependencies();
        assert_eq!(deps.len(), 1);
        assert!(deps.contains(&"@llvm-project//mlir:OpBaseTdFiles".to_string()));
    }
}
