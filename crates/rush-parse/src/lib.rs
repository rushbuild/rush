// Rush Parse - BUILD file parser
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! BUILD file parser for Starlark subset.

#![warn(missing_docs)]

pub mod envoy_compat;
mod starlark_eval;

/// Bridge to full rush-starlark parser (experimental)
pub mod starlark_bridge;

/// WORKSPACE file parser
pub mod workspace;

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use envoy_compat::translate_envoy_macro;
pub use starlark_bridge::{
    parse_build_file_lazy, parse_build_file_with_bzl_loader, parse_build_file_with_loader,
    parse_build_file_with_starlark,
};
pub use starlark_eval::{MacroDefinition, StarlarkEvaluator};
pub use workspace::{
    parse_workspace_file, GitRepository, HttpArchive, LoadStatement, LocalRepository, WorkspaceFile,
};

/// Represents a parsed BUILD file.
#[derive(Debug)]
pub struct BuildFile {
    /// The path to the BUILD file.
    pub path: PathBuf,
    /// The list of rules defined in the BUILD file.
    pub rules: Vec<Rule>,
}

/// Represents a single rule within a BUILD file.
#[derive(Debug, Clone)]
pub struct Rule {
    /// The name of the rule.
    pub name: String,
    /// The type of the rule (e.g., "rust_library").
    pub rule_type: String,
    /// The attributes of the rule.
    pub attributes: HashMap<String, Value>,
}

pub(crate) fn validate_unique_rule_names(rules: &[Rule], build_path: &Path) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for rule in rules {
        if !names.insert(rule.name.as_str()) {
            anyhow::bail!(
                "duplicate target name '{}' in BUILD file {}",
                rule.name,
                build_path.display()
            );
        }
    }
    Ok(())
}

/// Represents the different types of values for a rule attribute.
#[derive(Debug)]
pub enum Value {
    /// A string value.
    String(String),
    /// An integer value retained for configurable-expression evaluation.
    Int(i64),
    /// A list of values.
    List(Vec<Value>),
    /// A dictionary retained for configurable-expression evaluation.
    Dict(HashMap<String, Value>),
    /// A boolean value.
    Bool(bool),
    /// The Starlark `None` value retained for configurable-expression
    /// evaluation.
    None,
    /// A select expression mapping conditions to values.
    Select(HashMap<String, Value>),
    /// A compact operation containing a configurable operand. This is resolved
    /// only when the target enters the configured build closure.
    Configurable {
        /// The Starlark operation to apply after resolving both operands.
        op: rush_starlark::ConfigurableOp,
        /// Left operand.
        left: Box<Value>,
        /// Right operand.
        right: Box<Value>,
    },
}

/// Parses a BUILD file at the given path.
///
/// # Errors
///
/// Returns error if file cannot be parsed.
pub fn parse_build_file(path: &Path) -> Result<Arc<BuildFile>> {
    // Find workspace root (look for WORKSPACE file)
    let workspace_root = find_workspace_root(path)?;

    // Create evaluator
    let evaluator = StarlarkEvaluator::new(workspace_root);

    // Parse the BUILD file
    evaluator.eval_build_file(path)
}

/// Find workspace root by looking for WORKSPACE or MODULE.bazel file
fn find_workspace_root(start_path: &Path) -> Result<PathBuf> {
    let mut current = start_path.parent().unwrap_or(start_path);

    loop {
        if current.join("WORKSPACE").exists()
            || current.join("WORKSPACE.bazel").exists()
            || current.join("MODULE.bazel").exists()
        {
            return Ok(current.to_path_buf());
        }

        match current.parent() {
            Some(parent) => current = parent,
            None => anyhow::bail!("Could not find WORKSPACE or MODULE.bazel file"),
        }
    }
}
