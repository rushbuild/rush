// Rush Build - Shell Rules
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Shell build rules (sh_library, sh_binary, sh_test).

use anyhow::Result;
use rush_action::{Action, Artifact, Command};
use rush_parse::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Shell library (collection of shell scripts and data files).
pub struct ShLibrary {
    /// Library name.
    pub name: String,
    /// Shell script sources.
    pub srcs: Vec<String>,
    /// Dependencies (other sh_library targets).
    pub deps: Vec<String>,
    /// Data files to include.
    pub data: Vec<String>,
}

impl ShLibrary {
    /// Parse from BUILD file attributes.
    pub fn from_rule(rule: &rush_parse::Rule) -> Result<Self> {
        let name = rule.name.clone();

        let srcs = extract_string_list(&rule.attributes, "srcs").unwrap_or_default();
        let deps = extract_string_list(&rule.attributes, "deps").unwrap_or_default();
        let data = extract_string_list(&rule.attributes, "data").unwrap_or_default();

        Ok(ShLibrary {
            name,
            srcs,
            deps,
            data,
        })
    }

    /// Get all source paths.
    pub fn get_sources(&self, workspace_root: &Path, package: &str) -> Vec<PathBuf> {
        self.srcs
            .iter()
            .map(|src| workspace_root.join(package).join(src))
            .collect()
    }
}

/// Shell binary (executable shell script).
pub struct ShBinary {
    /// Binary name.
    pub name: String,
    /// Main shell script file.
    pub srcs: Vec<String>,
    /// Dependencies (sh_library targets).
    pub deps: Vec<String>,
    /// Data files to include.
    pub data: Vec<String>,
    /// Command-line arguments to pass to the script.
    pub args: Vec<String>,
    /// Environment variables.
    pub env: HashMap<String, String>,
}

impl ShBinary {
    /// Parse from BUILD file attributes.
    pub fn from_rule(rule: &rush_parse::Rule) -> Result<Self> {
        let name = rule.name.clone();

        let srcs = extract_string_list(&rule.attributes, "srcs")
            .unwrap_or_else(|_| vec![format!("{}.sh", name)]);

        let deps = extract_string_list(&rule.attributes, "deps").unwrap_or_default();
        let data = extract_string_list(&rule.attributes, "data").unwrap_or_default();
        let args = extract_string_list(&rule.attributes, "args").unwrap_or_default();
        // TODO: Add env support when Value::Dict is added
        let env = HashMap::new();

        Ok(ShBinary {
            name,
            srcs,
            deps,
            data,
            args,
            env,
        })
    }

    /// Create a wrapper script action.
    pub fn create_action(
        &self,
        workspace_root: &Path,
        package: &str,
        output_dir: &Path,
    ) -> Result<Action> {
        // The main script is typically the first (or only) source file
        let main_script = self
            .srcs
            .first()
            .ok_or_else(|| anyhow::anyhow!("sh_binary requires at least one source file"))?;

        let main_path = workspace_root.join(package).join(main_script);
        if !main_path.exists() {
            anyhow::bail!("Shell script not found: {}", main_path.display());
        }

        let output_file = output_dir.join(&self.name);

        // For sh_binary, we simply copy the script and make it executable
        // This is the simplest correct implementation matching Bazel's behavior
        let mut inputs = vec![Artifact::Source(main_path.clone())];
        for src in &self.srcs {
            let src_path = workspace_root.join(package).join(src);
            if src_path != main_path {
                inputs.push(Artifact::Source(src_path));
            }
        }
        for data_file in &self.data {
            inputs.push(Artifact::Source(
                workspace_root.join(package).join(data_file),
            ));
        }

        // Use cp and chmod to copy and make executable
        let args = vec![
            "-c".to_string(),
            format!(
                "cp {} {} && chmod +x {}",
                main_path.display(),
                output_file.display(),
                output_file.display()
            ),
        ];

        let command = Command {
            executable: PathBuf::from("/bin/sh"),
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
            mnemonic: "ShBinary".to_string(),
        })
    }
}

/// Shell test.
pub struct ShTest {
    /// Test name.
    pub name: String,
    /// Test shell script file.
    pub srcs: Vec<String>,
    /// Dependencies (sh_library targets).
    pub deps: Vec<String>,
    /// Data files to include.
    pub data: Vec<String>,
    /// Command-line arguments to pass to the test.
    pub args: Vec<String>,
    /// Environment variables.
    pub env: HashMap<String, String>,
}

impl ShTest {
    /// Parse from BUILD file attributes.
    pub fn from_rule(rule: &rush_parse::Rule) -> Result<Self> {
        let name = rule.name.clone();

        let srcs = extract_string_list(&rule.attributes, "srcs")
            .unwrap_or_else(|_| vec![format!("{}_test.sh", name)]);

        let deps = extract_string_list(&rule.attributes, "deps").unwrap_or_default();
        let data = extract_string_list(&rule.attributes, "data").unwrap_or_default();
        let args = extract_string_list(&rule.attributes, "args").unwrap_or_default();
        // TODO: Add env support when Value::Dict is added
        let env = HashMap::new();

        Ok(ShTest {
            name,
            srcs,
            deps,
            data,
            args,
            env,
        })
    }

    /// Create a test runner action.
    pub fn create_action(
        &self,
        workspace_root: &Path,
        package: &str,
        output_dir: &Path,
    ) -> Result<Action> {
        let main_script = self
            .srcs
            .first()
            .ok_or_else(|| anyhow::anyhow!("sh_test requires at least one source file"))?;

        let main_path = workspace_root.join(package).join(main_script);
        let output_file = output_dir.join(format!("{}_test", self.name));

        // Test wrapper sets up environment and runs the test script
        let mut env_setup = String::new();
        for (key, value) in &self.env {
            env_setup.push_str(&format!("export {}=\"{}\"\n", key, value));
        }

        let args_str = self.args.join(" ");
        let wrapper_content = format!(
            "#!/bin/bash\nset -e\n{env}exec \"{}\" {args} \"$@\"\n",
            main_path.display(),
            env = env_setup,
            args = args_str
        );

        let mut inputs = vec![Artifact::Source(main_path.clone())];
        for src in &self.srcs {
            let src_path = workspace_root.join(package).join(src);
            if src_path != main_path {
                inputs.push(Artifact::Source(src_path));
            }
        }
        for data_file in &self.data {
            inputs.push(Artifact::Source(
                workspace_root.join(package).join(data_file),
            ));
        }

        let args = vec![
            "-c".to_string(),
            format!(
                "echo '{}' > {} && chmod +x {}",
                wrapper_content.replace('\n', "\\n").replace('\'', "'\\''"),
                output_file.display(),
                output_file.display()
            ),
        ];

        let command = Command {
            executable: PathBuf::from("bash"),
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
            mnemonic: "ShTest".to_string(),
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
        Some(_) => anyhow::bail!("Attribute '{}' must be a list", key),
        None => anyhow::bail!("Missing attribute '{}'", key),
    }
}
