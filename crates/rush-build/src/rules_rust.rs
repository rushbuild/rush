// Rush Build - Rust Rules
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Rust build rules (rust_library, rust_binary, rust_test).

use anyhow::Result;
use rush_action::{Action, Artifact, Command};
use rush_parse::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Build a Rust library (.rlib).
pub struct RustLibrary {
    /// Library name.
    pub name: String,
    /// Source files (.rs).
    pub srcs: Vec<String>,
    /// Dependencies (other rust_library targets).
    pub deps: Vec<String>,
    /// Crate type (lib, rlib, dylib, etc.).
    pub crate_type: String,
    /// Edition (2015, 2018, 2021).
    pub edition: String,
    /// Rustc flags.
    pub rustc_flags: Vec<String>,
}

impl RustLibrary {
    /// Parse from BUILD file attributes.
    pub fn from_rule(rule: &rush_parse::Rule) -> Result<Self> {
        let name = rule.name.clone();

        let srcs = extract_string_list(&rule.attributes, "srcs")?;
        let deps = extract_string_list(&rule.attributes, "deps").unwrap_or_default();

        let crate_type =
            extract_string(&rule.attributes, "crate_type").unwrap_or_else(|| "rlib".to_string());

        let edition =
            extract_string(&rule.attributes, "edition").unwrap_or_else(|| "2021".to_string());

        let rustc_flags = extract_string_list(&rule.attributes, "rustc_flags").unwrap_or_default();

        Ok(RustLibrary {
            name,
            srcs,
            deps,
            crate_type,
            edition,
            rustc_flags,
        })
    }

    /// Create a rustc compile action.
    pub fn create_action(
        &self,
        workspace_root: &Path,
        package: &str,
        output_dir: &Path,
    ) -> Result<Action> {
        // Main source file (lib.rs or first source)
        let main_src = self
            .srcs
            .first()
            .ok_or_else(|| anyhow::anyhow!("No source files for rust_library: {}", self.name))?;

        let src_path = workspace_root.join(package).join(main_src);
        let output_file = output_dir.join(format!("lib{}.rlib", self.name));

        let mut args = vec![
            "--crate-name".to_string(),
            self.name.clone(),
            "--crate-type".to_string(),
            self.crate_type.clone(),
            "--edition".to_string(),
            self.edition.clone(),
            src_path.display().to_string(),
            "--out-dir".to_string(),
            output_dir.display().to_string(),
        ];

        // Add custom flags
        args.extend_from_slice(&self.rustc_flags);

        // Add dependency paths
        for dep in &self.deps {
            args.push("--extern".to_string());
            args.push(format!(
                "{}={}",
                dep,
                output_dir.join(format!("lib{}.rlib", dep)).display()
            ));
        }

        let inputs = vec![Artifact::Source(src_path.clone())];

        let command = Command {
            executable: PathBuf::from("rustc"),
            args,
            working_dir: None,
            execution_info: rush_action::ExecutionInfo::default(),
        };

        let env = HashMap::new();
        let action_id = Action::compute_id(&inputs, &command, &env)?;

        Ok(Action {
            id: action_id,
            inputs,
            outputs: vec![Artifact::Source(output_file.clone())],
            command,
            env,
            mnemonic: "Rustc".to_string(),
        })
    }
}

/// Build a Rust binary executable.
pub struct RustBinary {
    /// Binary name.
    pub name: String,
    /// Source files (.rs).
    pub srcs: Vec<String>,
    /// Dependencies (rust_library targets).
    pub deps: Vec<String>,
    /// Edition (2015, 2018, 2021).
    pub edition: String,
    /// Rustc flags.
    pub rustc_flags: Vec<String>,
}

impl RustBinary {
    /// Parse from BUILD file attributes.
    pub fn from_rule(rule: &rush_parse::Rule) -> Result<Self> {
        let name = rule.name.clone();

        let srcs = extract_string_list(&rule.attributes, "srcs")?;
        let deps = extract_string_list(&rule.attributes, "deps").unwrap_or_default();

        let edition =
            extract_string(&rule.attributes, "edition").unwrap_or_else(|| "2021".to_string());

        let rustc_flags = extract_string_list(&rule.attributes, "rustc_flags").unwrap_or_default();

        Ok(RustBinary {
            name,
            srcs,
            deps,
            edition,
            rustc_flags,
        })
    }

    /// Create a rustc compile action.
    pub fn create_action(
        &self,
        workspace_root: &Path,
        package: &str,
        output_dir: &Path,
    ) -> Result<Action> {
        // Main source file (main.rs or first source)
        let main_src = self
            .srcs
            .first()
            .ok_or_else(|| anyhow::anyhow!("No source files for rust_binary: {}", self.name))?;

        let src_path = workspace_root.join(package).join(main_src);
        let output_file = output_dir.join(&self.name);

        let mut args = vec![
            "--crate-name".to_string(),
            self.name.clone(),
            "--crate-type".to_string(),
            "bin".to_string(),
            "--edition".to_string(),
            self.edition.clone(),
            src_path.display().to_string(),
            "-o".to_string(),
            output_file.display().to_string(),
        ];

        // Add custom flags
        args.extend_from_slice(&self.rustc_flags);

        // Add dependency paths
        for dep in &self.deps {
            args.push("--extern".to_string());
            args.push(format!(
                "{}={}",
                dep,
                output_dir.join(format!("lib{}.rlib", dep)).display()
            ));
        }

        let inputs = vec![Artifact::Source(src_path.clone())];

        let command = Command {
            executable: PathBuf::from("rustc"),
            args,
            working_dir: None,
            execution_info: rush_action::ExecutionInfo::default(),
        };

        let env = HashMap::new();
        let action_id = Action::compute_id(&inputs, &command, &env)?;

        Ok(Action {
            id: action_id,
            inputs,
            outputs: vec![Artifact::Source(output_file.clone())],
            command,
            env,
            mnemonic: "RustcBinary".to_string(),
        })
    }
}

/// Build a Rust test binary.
pub struct RustTest {
    /// Test name.
    pub name: String,
    /// Source files (.rs).
    pub srcs: Vec<String>,
    /// Dependencies (rust_library targets).
    pub deps: Vec<String>,
    /// Edition (2015, 2018, 2021).
    pub edition: String,
    /// Rustc flags.
    pub rustc_flags: Vec<String>,
}

impl RustTest {
    /// Parse from BUILD file attributes.
    pub fn from_rule(rule: &rush_parse::Rule) -> Result<Self> {
        let name = rule.name.clone();

        let srcs = extract_string_list(&rule.attributes, "srcs")?;
        let deps = extract_string_list(&rule.attributes, "deps").unwrap_or_default();

        let edition =
            extract_string(&rule.attributes, "edition").unwrap_or_else(|| "2021".to_string());

        let rustc_flags = extract_string_list(&rule.attributes, "rustc_flags").unwrap_or_default();

        Ok(RustTest {
            name,
            srcs,
            deps,
            edition,
            rustc_flags,
        })
    }

    /// Create a rustc test compile action.
    pub fn create_action(
        &self,
        workspace_root: &Path,
        package: &str,
        output_dir: &Path,
    ) -> Result<Action> {
        let main_src = self
            .srcs
            .first()
            .ok_or_else(|| anyhow::anyhow!("No source files for rust_test: {}", self.name))?;

        let src_path = workspace_root.join(package).join(main_src);
        let output_file = output_dir.join(&self.name);

        let mut args = vec![
            "--crate-name".to_string(),
            self.name.clone(),
            "--edition".to_string(),
            self.edition.clone(),
            "--test".to_string(),
            src_path.display().to_string(),
            "-o".to_string(),
            output_file.display().to_string(),
        ];

        // Add custom flags
        args.extend_from_slice(&self.rustc_flags);

        // Add dependency paths
        for dep in &self.deps {
            args.push("--extern".to_string());
            args.push(format!(
                "{}={}",
                dep,
                output_dir.join(format!("lib{}.rlib", dep)).display()
            ));
        }

        let inputs = vec![Artifact::Source(src_path.clone())];

        let command = Command {
            executable: PathBuf::from("rustc"),
            args,
            working_dir: None,
            execution_info: rush_action::ExecutionInfo::default(),
        };

        let env = HashMap::new();
        let action_id = Action::compute_id(&inputs, &command, &env)?;

        Ok(Action {
            id: action_id,
            inputs,
            outputs: vec![Artifact::Source(output_file.clone())],
            command,
            env,
            mnemonic: "RustcTest".to_string(),
        })
    }
}

// Helper functions

fn extract_string_list(attrs: &HashMap<String, Value>, key: &str) -> Result<Vec<String>> {
    match attrs.get(key) {
        Some(Value::List(list)) => Ok(list
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect()),
        _ => anyhow::bail!("Missing or invalid '{}' attribute", key),
    }
}

fn extract_string(attrs: &HashMap<String, Value>, key: &str) -> Option<String> {
    match attrs.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}
