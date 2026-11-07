// Rush Build - Genrule Implementation
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Generic build rule for running arbitrary commands.
//!
//! Genrule is a fundamental Bazel rule that allows running arbitrary shell
//! commands to generate files. It's commonly used for code generation,
//! file transformation, and other custom build steps.

use anyhow::Result;
use rush_action::{Action, Artifact, Command};
use rush_parse::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Generic build rule that runs arbitrary commands.
pub struct GenRule {
    /// Rule name.
    pub name: String,
    /// Input source files.
    pub srcs: Vec<String>,
    /// Executable tools needed by the command.
    pub tools: Vec<String>,
    /// Output files to generate.
    pub outs: Vec<String>,
    /// Command to run (bash/sh).
    pub cmd: Option<String>,
    /// Command for bash specifically.
    pub cmd_bash: Option<String>,
    /// Command for PowerShell (Windows).
    pub cmd_ps: Option<String>,
    /// Execution requirements (e.g., local-only).
    pub local: bool,
    /// Environment variables.
    pub env: HashMap<String, String>,
}

impl GenRule {
    /// Parse from BUILD file attributes.
    pub fn from_rule(rule: &rush_parse::Rule) -> Result<Self> {
        let name = rule.name.clone();

        let srcs = extract_string_list(&rule.attributes, "srcs").unwrap_or_default();
        let tools = extract_string_list(&rule.attributes, "tools").unwrap_or_default();
        let outs = extract_string_list(&rule.attributes, "outs")?; // Required

        // Get command - try cmd, cmd_bash, cmd_ps in that order
        let cmd = extract_string(&rule.attributes, "cmd").ok();
        let cmd_bash = extract_string(&rule.attributes, "cmd_bash").ok();
        let cmd_ps = extract_string(&rule.attributes, "cmd_ps").ok();

        if cmd.is_none() && cmd_bash.is_none() && cmd_ps.is_none() {
            anyhow::bail!(
                "genrule '{}' must have at least one of: cmd, cmd_bash, cmd_ps",
                name
            );
        }

        let local = extract_bool(&rule.attributes, "local").unwrap_or(false);

        // TODO: Add env support when Value::Dict is properly supported
        let env = HashMap::new();

        Ok(GenRule {
            name,
            srcs,
            tools,
            outs,
            cmd,
            cmd_bash,
            cmd_ps,
            local,
            env,
        })
    }

    /// Create a genrule action.
    pub fn create_action(
        &self,
        workspace_root: &Path,
        package: &str,
        output_dir: &Path,
    ) -> Result<Action> {
        // Determine which command to use
        // On Linux/macOS, prefer cmd_bash > cmd, ignore cmd_ps
        let command_str = if let Some(bash_cmd) = &self.cmd_bash {
            bash_cmd.clone()
        } else if let Some(cmd) = &self.cmd {
            cmd.clone()
        } else {
            anyhow::bail!(
                "genrule '{}' has no compatible command (need cmd or cmd_bash on Unix)",
                self.name
            );
        };

        // Collect input artifacts
        let mut inputs = Vec::new();
        for src in &self.srcs {
            let src_path = workspace_root.join(package).join(src);
            inputs.push(Artifact::Source(src_path));
        }

        // Collect tool artifacts
        for tool in &self.tools {
            let tool_path = workspace_root.join(package).join(tool);
            inputs.push(Artifact::Source(tool_path));
        }

        // Prepare output artifacts
        let mut outputs = Vec::new();
        let mut output_paths = Vec::new();
        for out in &self.outs {
            let output_path = output_dir.join(out);
            output_paths.push(output_path.clone());
            outputs.push(Artifact::Source(output_path));
        }

        // Perform variable substitutions
        let substituted_cmd =
            self.substitute_variables(&command_str, workspace_root, package, &output_paths)?;

        // Create bash command to run the genrule
        // We need to:
        // 1. Create output directories
        // 2. Run the user's command
        let output_dirs: Vec<PathBuf> = output_paths
            .iter()
            .filter_map(|p| p.parent().map(|d| d.to_path_buf()))
            .collect();

        let mut setup_cmds = Vec::new();

        // Create output directories
        for dir in output_dirs {
            setup_cmds.push(format!("mkdir -p {}", shell_escape(dir.to_str().unwrap())));
        }

        // Run the actual command
        setup_cmds.push(substituted_cmd);

        let full_command = setup_cmds.join(" && ");

        let args = vec!["-c".to_string(), full_command];

        let mut exec_info = rush_action::ExecutionInfo::default();
        if self.local {
            // local = true means run without sandbox
            exec_info.requires_sandbox = false;
        }

        let command = Command {
            executable: PathBuf::from("/bin/bash"),
            args,
            working_dir: None,
            execution_info: exec_info,
        };

        let env = self.env.clone();
        let action_id = Action::compute_id(&inputs, &command, &env)?;

        Ok(Action {
            id: action_id,
            inputs,
            outputs,
            command,
            env,
            mnemonic: "GenRule".to_string(),
        })
    }

    /// Substitute Bazel/Make-style variables in command string.
    fn substitute_variables(
        &self,
        cmd: &str,
        workspace_root: &Path,
        package: &str,
        output_paths: &[PathBuf],
    ) -> Result<String> {
        let mut result = cmd.to_string();

        // $@ - first output file (or all outputs separated by space)
        if !output_paths.is_empty() {
            let first_out = output_paths[0].to_str().unwrap();
            result = result.replace("$@", first_out);
        }

        // $(@D) - directory of first output
        if !output_paths.is_empty() {
            if let Some(parent) = output_paths[0].parent() {
                let out_dir = parent.to_str().unwrap();
                result = result.replace("$(@D)", out_dir);
            }
        }

        // $(SRCS) - all source files
        let srcs_str: Vec<String> = self
            .srcs
            .iter()
            .map(|s| {
                workspace_root
                    .join(package)
                    .join(s)
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        result = result.replace("$(SRCS)", &srcs_str.join(" "));

        // $(OUTS) - all output files
        let outs_str: Vec<String> = output_paths
            .iter()
            .map(|p| p.to_str().unwrap().to_string())
            .collect();
        result = result.replace("$(OUTS)", &outs_str.join(" "));

        // Future work: Implement $(location <label>) substitution
        // This requires dependency resolution to find actual file paths

        Ok(result)
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

fn extract_string(attrs: &HashMap<String, Value>, key: &str) -> Result<String> {
    match attrs.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => anyhow::bail!("Attribute '{}' must be a string", key),
        None => anyhow::bail!("Missing attribute '{}'", key),
    }
}

fn extract_bool(attrs: &HashMap<String, Value>, key: &str) -> Result<bool> {
    match attrs.get(key) {
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => anyhow::bail!("Attribute '{}' must be a boolean", key),
        None => anyhow::bail!("Missing attribute '{}'", key),
    }
}

/// Escape string for shell command.
fn shell_escape(s: &str) -> String {
    if s.contains(' ') || s.contains('\'') || s.contains('"') {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_parse::Rule;

    #[test]
    fn test_genrule_parse() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "srcs".to_string(),
            Value::List(vec![Value::String("input.txt".to_string())]),
        );
        attrs.insert(
            "outs".to_string(),
            Value::List(vec![Value::String("output.txt".to_string())]),
        );
        attrs.insert(
            "cmd".to_string(),
            Value::String("cat $(SRCS) > $@".to_string()),
        );

        let rule = Rule {
            name: "test_genrule".to_string(),
            rule_type: "genrule".to_string(),
            attributes: attrs,
        };

        let genrule = GenRule::from_rule(&rule).unwrap();
        assert_eq!(genrule.name, "test_genrule");
        assert_eq!(genrule.srcs, vec!["input.txt"]);
        assert_eq!(genrule.outs, vec!["output.txt"]);
        assert_eq!(genrule.cmd, Some("cat $(SRCS) > $@".to_string()));
    }

    #[test]
    fn test_variable_substitution() {
        let genrule = GenRule {
            name: "test".to_string(),
            srcs: vec!["src1.txt".to_string(), "src2.txt".to_string()],
            tools: vec![],
            outs: vec!["out.txt".to_string()],
            cmd: Some("echo $(SRCS) > $@".to_string()),
            cmd_bash: None,
            cmd_ps: None,
            local: false,
            env: HashMap::new(),
        };

        let workspace = PathBuf::from("/workspace");
        let package = "pkg";
        let output_paths = vec![PathBuf::from("/out/pkg/out.txt")];

        let result = genrule
            .substitute_variables("echo $(SRCS) > $@", &workspace, package, &output_paths)
            .unwrap();

        assert!(result.contains("/workspace/pkg/src1.txt /workspace/pkg/src2.txt"));
        assert!(result.contains("/out/pkg/out.txt"));
    }
}
