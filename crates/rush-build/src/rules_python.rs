// Rush Build - Python Rules
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Python build rules (py_library, py_binary, py_test).

use anyhow::Result;
use rush_action::{Action, Artifact, Command};
use rush_parse::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Python library (collection of .py files).
pub struct PyLibrary {
    /// Library name.
    pub name: String,
    /// Python source files (.py).
    pub srcs: Vec<String>,
    /// Python dependencies (other py_library targets).
    pub deps: Vec<String>,
    /// Data files to include.
    pub data: Vec<String>,
    /// Python imports directory.
    pub imports: Option<String>,
}

impl PyLibrary {
    /// Parse from BUILD file attributes.
    pub fn from_rule(rule: &rush_parse::Rule) -> Result<Self> {
        let name = rule.name.clone();

        let srcs = extract_string_list(&rule.attributes, "srcs").unwrap_or_default();
        let deps = extract_string_list(&rule.attributes, "deps").unwrap_or_default();
        let data = extract_string_list(&rule.attributes, "data").unwrap_or_default();
        let imports = extract_string(&rule.attributes, "imports");

        Ok(PyLibrary {
            name,
            srcs,
            deps,
            data,
            imports,
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

/// Python binary (executable script).
pub struct PyBinary {
    /// Binary name.
    pub name: String,
    /// Main Python file.
    pub main: String,
    /// Python source files (.py).
    pub srcs: Vec<String>,
    /// Python dependencies (py_library targets).
    pub deps: Vec<String>,
    /// Data files to include.
    pub data: Vec<String>,
    /// Python interpreter (default: python3).
    pub python_version: String,
}

impl PyBinary {
    /// Parse from BUILD file attributes.
    pub fn from_rule(rule: &rush_parse::Rule) -> Result<Self> {
        let name = rule.name.clone();

        let main = extract_string(&rule.attributes, "main")
            .ok_or_else(|| anyhow::anyhow!("py_binary requires 'main' attribute"))?;

        let srcs = extract_string_list(&rule.attributes, "srcs").unwrap_or_default();
        let deps = extract_string_list(&rule.attributes, "deps").unwrap_or_default();
        let data = extract_string_list(&rule.attributes, "data").unwrap_or_default();

        let python_version = extract_string(&rule.attributes, "python_version")
            .unwrap_or_else(|| "python3".to_string());

        Ok(PyBinary {
            name,
            main,
            srcs,
            deps,
            data,
            python_version,
        })
    }

    /// Create a wrapper script action.
    pub fn create_action(
        &self,
        workspace_root: &Path,
        package: &str,
        output_dir: &Path,
    ) -> Result<Action> {
        let main_path = workspace_root.join(package).join(&self.main);
        let output_file = output_dir.join(&self.name);

        // Create a wrapper shell script
        let wrapper_content = format!(
            "#!/bin/bash\nexec {} \"$0.runfiles/{}/{}\" \"$@\"\n",
            self.python_version, package, self.main
        );

        let mut inputs = vec![Artifact::Source(main_path.clone())];
        for src in &self.srcs {
            inputs.push(Artifact::Source(workspace_root.join(package).join(src)));
        }

        // Use a simple echo to create the wrapper (in real implementation, use proper file writing)
        let args = vec![
            "-c".to_string(),
            format!(
                "echo '{}' > {} && chmod +x {}",
                wrapper_content.replace('\n', "\\n"),
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
            mnemonic: "PyBinary".to_string(),
        })
    }
}

/// Python test.
pub struct PyTest {
    /// Test name.
    pub name: String,
    /// Main test file.
    pub main: String,
    /// Python source files (.py).
    pub srcs: Vec<String>,
    /// Python dependencies (py_library targets).
    pub deps: Vec<String>,
    /// Data files to include.
    pub data: Vec<String>,
    /// Python interpreter (default: python3).
    pub python_version: String,
}

impl PyTest {
    /// Parse from BUILD file attributes.
    pub fn from_rule(rule: &rush_parse::Rule) -> Result<Self> {
        let name = rule.name.clone();

        let main = extract_string(&rule.attributes, "main")
            .ok_or_else(|| anyhow::anyhow!("py_test requires 'main' attribute"))?;

        let srcs = extract_string_list(&rule.attributes, "srcs").unwrap_or_default();
        let deps = extract_string_list(&rule.attributes, "deps").unwrap_or_default();
        let data = extract_string_list(&rule.attributes, "data").unwrap_or_default();

        let python_version = extract_string(&rule.attributes, "python_version")
            .unwrap_or_else(|| "python3".to_string());

        Ok(PyTest {
            name,
            main,
            srcs,
            deps,
            data,
            python_version,
        })
    }

    /// Create a test runner action.
    pub fn create_action(
        &self,
        workspace_root: &Path,
        package: &str,
        output_dir: &Path,
    ) -> Result<Action> {
        let main_path = workspace_root.join(package).join(&self.main);
        let output_file = output_dir.join(format!("{}_test", self.name));

        // Create a test wrapper
        let wrapper_content = format!(
            "#!/bin/bash\nexec {} -m pytest \"{}\" \"$@\"\n",
            self.python_version,
            main_path.display()
        );

        let mut inputs = vec![Artifact::Source(main_path.clone())];
        for src in &self.srcs {
            inputs.push(Artifact::Source(workspace_root.join(package).join(src)));
        }

        let args = vec![
            "-c".to_string(),
            format!(
                "echo '{}' > {} && chmod +x {}",
                wrapper_content.replace('\n', "\\n"),
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
            mnemonic: "PyTest".to_string(),
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

fn extract_string(attrs: &HashMap<String, Value>, key: &str) -> Option<String> {
    match attrs.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}
