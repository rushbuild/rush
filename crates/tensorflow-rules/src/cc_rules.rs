// Standard cc_library and cc_binary rules (wrappers around TensorFlow rules)
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use anyhow::Result;
use async_trait::async_trait;
use rush_rules::{BuildRule, RuleInstance, Value};
use std::collections::HashMap;

use crate::{TfCcBinaryRule, TfCcLibraryRule};

/// Standard cc_library rule (delegates to tf_cc_library)
pub struct CcLibraryRule;

#[async_trait]
impl BuildRule for CcLibraryRule {
    fn name(&self) -> &str {
        "cc_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        // Delegate to TfCcLibraryRule
        TfCcLibraryRule.parse_attributes(attrs)
    }

    fn documentation(&self) -> &str {
        "Standard C++ library rule"
    }
}

/// Standard cc_binary rule (delegates to tf_cc_binary)
pub struct CcBinaryRule;

#[async_trait]
impl BuildRule for CcBinaryRule {
    fn name(&self) -> &str {
        "cc_binary"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        // Delegate to TfCcBinaryRule
        TfCcBinaryRule.parse_attributes(attrs)
    }

    fn documentation(&self) -> &str {
        "Standard C++ binary rule"
    }
}
