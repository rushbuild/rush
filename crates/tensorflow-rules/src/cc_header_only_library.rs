// TensorFlow cc_header_only_library rule
//
// This is a header-only cc_library variant used in TensorFlow.
// It only provides header files (no compilation) and tracks dependencies.
// For Rush's parsing phase, we just need to track the target and its dependencies.

use anyhow::Result;
use async_trait::async_trait;
use rush_rules::{extract_string, extract_string_list, Artifact, BuildRule, RuleInstance, Value};
use std::collections::HashMap;

pub struct CcHeaderOnlyLibraryRule;

impl BuildRule for CcHeaderOnlyLibraryRule {
    fn name(&self) -> &str {
        "cc_header_only_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let deps = extract_string_list(attrs, "deps").unwrap_or_default();
        let extra_deps = extract_string_list(attrs, "extra_deps").unwrap_or_default();

        // Combine deps and extra_deps
        let mut all_deps = deps;
        all_deps.extend(extra_deps);

        Ok(Box::new(CcHeaderOnlyLibraryInstance {
            name,
            deps: all_deps,
        }))
    }
}

struct CcHeaderOnlyLibraryInstance {
    name: String,
    deps: Vec<String>,
}

#[async_trait]
impl RuleInstance for CcHeaderOnlyLibraryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        self.deps.clone()
    }

    async fn build(&self, _context: &rush_rules::context::BuildContext) -> Result<Vec<Artifact>> {
        // Header-only library implementation:
        // This rule type only provides header files and forwards dependencies.
        // It does not produce compiled object files or libraries.
        // No Command::new execution required.
        Ok(Vec::new()) // HACK: Metadata-only rule, returns empty artifacts
    }

    fn analyze(
        &self,
        _ctx: &mut rush_rules::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> rush_rules::providers::ProviderSet {
        use rush_rules::providers::{CcInfo, DirectCcInfo};
        // cc_header_only_library re-exports the compilation+linking context of
        // its wrapped deps (Bazel does the same: it forwards the deps' CcInfo).
        // It contributes no headers/includes of its own, so the direct part is
        // empty and the whole value is the transitive merge of dep CcInfo. This
        // is what makes //tensorflow/core:framework_headers_lib propagate the
        // transitive include_dirs of @eigen_archive / @ml_dtypes / @com_google_absl
        // / … to consumers like //tensorflow/core/profiler:tfprof_options. Prior
        // to this, providers() returned ProviderSet::empty(), dropping ALL
        // transitive includes and forcing per-repo include band-aids.
        let direct = DirectCcInfo::default();
        let cc_info = CcInfo::from_direct_and_deps(
            direct,
            rush_rules::providers::extract_cc_infos(dep_providers).into_iter(),
        );
        rush_rules::providers::ProviderSet::empty().with_cc_info(cc_info)
    }

    fn providers(&self) -> rush_rules::providers::ProviderSet {
        rush_rules::providers::ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "cc_header_only_library"
    }
}
