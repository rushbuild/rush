use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rush_rules::{
    context::BuildContext, extract_string, extract_string_list, providers::ProviderSet, Artifact,
    BuildRule, RuleInstance, Value,
};
use std::collections::HashMap;

/// tf_gen_op_wrappers_cc rule
///
/// Generates C++ wrapper code for TensorFlow ops by running the
/// `cc_op_gen_main` codegen binary against each registered op
/// library. The Bazel macro at `tensorflow.bzl:tf_gen_op_wrapper_cc`
/// expands to a per-op cc_binary that links the codegen tool plus
/// `<pkg>:<op>_op_lib` (which carries the `REGISTER_OP(...)` macros),
/// then a genrule that runs the binary to write
/// `<gendir>/<pkg>/ops/<op>.{h,cc,_internal.h,_internal.cc}`.
///
/// Rush doesn't yet build that codegen binary or run it. See
/// UNIMPLEMENTED.md for the removal plan.
pub struct TfGenOpWrappersCcRule;

#[async_trait]
impl BuildRule for TfGenOpWrappersCcRule {
    fn name(&self) -> &str {
        "tf_gen_op_wrappers_cc"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs").unwrap_or_default();
        let deps = extract_string_list(attrs, "deps").unwrap_or_default();
        let op_lib_names = extract_string_list(attrs, "op_lib_names").unwrap_or_default();
        let pkg = extract_string(attrs, "pkg").ok();

        Ok(Box::new(TfGenOpWrappersCcInstance {
            name,
            srcs,
            deps,
            op_lib_names,
            pkg,
        }))
    }

    fn documentation(&self) -> &str {
        "Generates C++ wrapper code for TensorFlow ops via cc_op_gen_main."
    }
}

struct TfGenOpWrappersCcInstance {
    name: String,
    #[allow(dead_code)]
    srcs: Vec<String>,
    #[allow(dead_code)]
    deps: Vec<String>,
    op_lib_names: Vec<String>,
    pkg: Option<String>,
}

#[async_trait]
impl RuleInstance for TfGenOpWrappersCcInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        self.deps.clone()
    }

    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
        // Hard-fail with the specific work item. Returning an empty
        // Vec used to "succeed" silently and the consumer's compile
        // would later fail with a cryptic
        // `tensorflow/cc/ops/<X>.h: No such file or directory` —
        // the exact silent-stub anti-pattern docs/feedback_no_fake_stubs.md
        // forbids. Surface the gap at the real source: this rule.
        Err(anyhow!(
            "tf_gen_op_wrappers_cc `{}` (pkg={}, op_lib_names={:?}): rush does not \
             yet build the cc_op_gen_main codegen binary nor invoke it to produce \
             the per-op wrapper headers. Bazel macro: tensorflow/tensorflow.bzl \
             tf_gen_op_wrapper_cc — expands to a tf_cc_binary linking \
             //tensorflow/cc:cc_op_gen_main + <pkg>:<op>_op_lib, then a genrule \
             that runs the binary to write <gendir>/<pkg>/ops/<op>.{{h,cc,_internal.{{h,cc}}}}. \
             Implementing this requires building most of TF core first (cc_op_gen_main \
             links against it). See UNIMPLEMENTED.md \"Op wrapper codegen\".",
            self.name,
            self.pkg.as_deref().unwrap_or("(unset)"),
            self.op_lib_names
        ))
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "tf_gen_op_wrappers_cc"
    }
}
