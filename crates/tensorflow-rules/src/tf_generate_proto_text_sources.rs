//! Require TensorFlow's declared proto-text generator and source macro.

use anyhow::{bail, Result};
use rush_rules::{BuildRule, RuleInstance, Value};
use std::collections::HashMap;

/// Diagnose direct native calls instead of manufacturing replacement sources.
pub struct TfGenerateProtoTextSourcesRule;

impl BuildRule for TfGenerateProtoTextSourcesRule {
    fn name(&self) -> &str {
        "tf_generate_proto_text_sources"
    }

    fn parse_attributes(&self, _attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        bail!(
            "load tf_generate_proto_text_sources from TensorFlow's tensorflow.bzl; \
            its genrule must build and run the declared gen_proto_text_functions tool"
        )
    }

    fn documentation(&self) -> &str {
        "Load TensorFlow's macro to declare the proto-text generator, headers and C++ library."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_proto_text_rule_refuses_replacement_sources() {
        let error = TfGenerateProtoTextSourcesRule
            .parse_attributes(&HashMap::new())
            .err()
            .expect("a missing source macro must not manufacture proto-text outputs");
        assert!(error.to_string().contains("gen_proto_text_functions"));
    }
}
