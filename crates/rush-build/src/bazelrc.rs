// .bazelrc parser
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

/// Parsed .bazelrc configuration
#[derive(Debug, Clone)]
pub struct BazelRc {
    /// Common flags (apply to all commands)
    pub common_flags: Vec<String>,

    /// Build-specific flags
    pub build_flags: Vec<String>,

    /// Test-specific flags
    pub test_flags: Vec<String>,

    /// Run-specific flags
    pub run_flags: Vec<String>,

    /// Named configurations (`--config=name`), kept command-scoped so build
    /// never consumes options from `test:name` or `run:name`.
    pub configs: HashMap<String, BazelRcNamedConfig>,

    /// Startup flags (for JVM, ignored in Rush)
    pub startup_flags: Vec<String>,
}

/// Command-scoped definitions for one `.bazelrc` configuration name.
#[derive(Debug, Clone, Default)]
pub struct BazelRcNamedConfig {
    /// `common:name` options.
    pub common_flags: Vec<String>,

    /// `build:name` options.
    pub build_flags: Vec<String>,

    /// `test:name` options.
    pub test_flags: Vec<String>,

    /// `run:name` options.
    pub run_flags: Vec<String>,
}

impl BazelRc {
    /// Parse a .bazelrc file
    pub fn parse(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read .bazelrc: {}", path.display()))?;

        let mut rc = BazelRc {
            common_flags: Vec::new(),
            build_flags: Vec::new(),
            test_flags: Vec::new(),
            run_flags: Vec::new(),
            configs: HashMap::new(),
            startup_flags: Vec::new(),
        };

        for line in content.lines() {
            // Skip comments and empty lines
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            // Parse the line
            rc.parse_line(trimmed)?;
        }

        Ok(rc)
    }

    /// Parse a single .bazelrc line
    fn parse_line(&mut self, line: &str) -> Result<()> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            return Ok(());
        }

        let command = parts[0];
        let flags: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

        match command {
            "startup" => {
                // Startup flags are for JVM, we ignore them
                self.startup_flags.extend(flags);
            }
            "common" => {
                self.common_flags.extend(flags);
            }
            "build" => {
                self.build_flags.extend(flags);
            }
            "test" => {
                self.test_flags.extend(flags);
            }
            "run" => {
                self.run_flags.extend(flags);
            }
            cmd if cmd.starts_with("build:") => {
                // Config-specific build flags: build:release --opt
                let config_name = &cmd[6..]; // Remove "build:" prefix
                self.configs
                    .entry(config_name.to_string())
                    .or_default()
                    .build_flags
                    .extend(flags);
            }
            cmd if cmd.starts_with("test:") => {
                // Config-specific test flags
                let config_name = &cmd[5..];
                self.configs
                    .entry(config_name.to_string())
                    .or_default()
                    .test_flags
                    .extend(flags);
            }
            cmd if cmd.starts_with("run:") => {
                // Config-specific run flags
                let config_name = &cmd[4..];
                self.configs
                    .entry(config_name.to_string())
                    .or_default()
                    .run_flags
                    .extend(flags);
            }
            cmd if cmd.starts_with("common:") => {
                let config_name = &cmd[7..];
                self.configs
                    .entry(config_name.to_string())
                    .or_default()
                    .common_flags
                    .extend(flags);
            }
            "fetch" => {
                // Fetch flags, less common
                // For now, treat as build flags
                self.build_flags.extend(flags);
            }
            _ => {
                // Unknown command, skip or warn
            }
        }

        Ok(())
    }

    /// Get flags for a specific command with config expansion
    pub fn get_flags(&self, command: &str, configs: &[String]) -> Vec<String> {
        let mut flags = Vec::new();

        // Add common flags
        flags.extend(self.common_flags.clone());

        // Add command-specific flags
        match command {
            "build" => flags.extend(self.build_flags.clone()),
            "test" => {
                flags.extend(self.build_flags.clone());
                flags.extend(self.test_flags.clone());
            }
            "run" => {
                flags.extend(self.build_flags.clone());
                flags.extend(self.run_flags.clone());
            }
            _ => {}
        }

        // Add config-specific flags
        for config in configs {
            if let Some(config_flags) =
                self.named_config_flags(command, config)
            {
                flags.extend(config_flags);
            }
        }

        flags
    }

    /// Parse flags from a .bazelrc file and expand --config references
    pub fn get_expanded_flags(&self, command: &str, cli_flags: &[String]) -> Vec<String> {
        let mut configs = Vec::new();

        // Extract --config= flags from CLI
        for flag in cli_flags {
            if let Some(config_name) = flag.strip_prefix("--config=") {
                configs.push(config_name.to_string());
            }
        }

        self.get_flags(command, &configs)
    }

    /// Return one named config's options applicable to `command`, ordered by
    /// Bazel command specificity (`common`, then inherited `build`, then the
    /// more-specific command).
    pub fn named_config_flags(
        &self,
        command: &str,
        name: &str,
    ) -> Option<Vec<String>> {
        let config = self.configs.get(name)?;
        let mut flags = config.common_flags.clone();
        match command {
            "build" => flags.extend(config.build_flags.clone()),
            "test" => {
                flags.extend(config.build_flags.clone());
                flags.extend(config.test_flags.clone());
            }
            "run" => {
                flags.extend(config.build_flags.clone());
                flags.extend(config.run_flags.clone());
            }
            _ => {}
        }
        (!flags.is_empty()).then_some(flags)
    }
}

/// Parse compiler flags from .bazelrc
#[derive(Debug, Clone)]
pub struct CompilerFlags {
    /// C++ compiler flags (--cxxopt)
    pub cxx_opts: Vec<String>,

    /// C compiler flags (--copt)
    pub c_opts: Vec<String>,

    /// Linker flags (--linkopt)
    pub link_opts: Vec<String>,

    /// Compilation mode (--compilation_mode)
    pub compilation_mode: Option<String>,

    /// Action environment variables (--action_env)
    pub action_env: HashMap<String, String>,

    /// Platform (--config=linux, --config=macos, etc.)
    pub platform: Option<String>,
}

impl CompilerFlags {
    /// Extract compiler flags from .bazelrc flags
    pub fn from_bazelrc_flags(flags: &[String]) -> Self {
        let mut compiler_flags = CompilerFlags {
            cxx_opts: Vec::new(),
            c_opts: Vec::new(),
            link_opts: Vec::new(),
            compilation_mode: None,
            action_env: HashMap::new(),
            platform: None,
        };

        for flag in flags {
            if let Some(opt) = flag.strip_prefix("--cxxopt=") {
                compiler_flags.cxx_opts.push(opt.to_string());
            } else if let Some(opt) = flag.strip_prefix("--copt=") {
                compiler_flags.c_opts.push(opt.to_string());
            } else if let Some(opt) = flag.strip_prefix("--linkopt=") {
                compiler_flags.link_opts.push(opt.to_string());
            } else if let Some(mode) = flag.strip_prefix("--compilation_mode=") {
                compiler_flags.compilation_mode = Some(mode.to_string());
            } else if let Some(mode) = flag.strip_prefix("-c ") {
                // Short form: -c opt
                compiler_flags.compilation_mode = Some(mode.to_string());
            } else if let Some(env) = flag.strip_prefix("--action_env=") {
                if let Some((key, value)) = env.split_once('=') {
                    compiler_flags
                        .action_env
                        .insert(key.to_string(), value.to_string());
                } else {
                    // Just key, get from environment
                    if let Ok(value) = std::env::var(env) {
                        compiler_flags.action_env.insert(env.to_string(), value);
                    }
                }
            } else if let Some(config) = flag.strip_prefix("--config=") {
                // Platform detection
                if config == "linux" || config == "macos" || config == "windows" {
                    compiler_flags.platform = Some(config.to_string());
                }
            }
        }

        compiler_flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_bazelrc() {
        let content = r#"
# Comment
startup --host_jvm_args=-Xmx3g

common --noenable_bzlmod
build --color=yes
build --cxxopt=-std=c++20
build:release --compilation_mode=opt
build:release --strip=always
common:release --announce_rc
test:release --test_output=errors
run:release --run_under=env

test --test_verbose_timeout_warnings
        "#;

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();

        let rc = BazelRc::parse(file.path()).unwrap();

        assert_eq!(rc.common_flags, vec!["--noenable_bzlmod"]);
        assert!(rc.build_flags.contains(&"--color=yes".to_string()));
        assert!(rc.build_flags.contains(&"--cxxopt=-std=c++20".to_string()));

        let release = rc.configs.get("release").unwrap();
        assert!(release
            .build_flags
            .contains(&"--compilation_mode=opt".to_string()));
        assert!(release
            .build_flags
            .contains(&"--strip=always".to_string()));
        assert_eq!(
            rc.named_config_flags("build", "release").unwrap(),
            vec![
                "--announce_rc",
                "--compilation_mode=opt",
                "--strip=always",
            ]
        );
        assert_eq!(
            rc.named_config_flags("test", "release").unwrap(),
            vec![
                "--announce_rc",
                "--compilation_mode=opt",
                "--strip=always",
                "--test_output=errors",
            ]
        );
        assert_eq!(
            rc.named_config_flags("run", "release").unwrap(),
            vec![
                "--announce_rc",
                "--compilation_mode=opt",
                "--strip=always",
                "--run_under=env",
            ]
        );
    }

    #[test]
    fn test_compiler_flags_extraction() {
        let flags = vec![
            "--cxxopt=-std=c++20".to_string(),
            "--copt=-O3".to_string(),
            "--linkopt=-fuse-ld=lld".to_string(),
            "-c opt".to_string(),
            "--action_env=CC=clang".to_string(),
        ];

        let compiler_flags = CompilerFlags::from_bazelrc_flags(&flags);

        assert_eq!(compiler_flags.cxx_opts, vec!["-std=c++20"]);
        assert_eq!(compiler_flags.c_opts, vec!["-O3"]);
        assert_eq!(compiler_flags.link_opts, vec!["-fuse-ld=lld"]);
        assert_eq!(compiler_flags.compilation_mode, Some("opt".to_string()));
        assert_eq!(
            compiler_flags.action_env.get("CC"),
            Some(&"clang".to_string())
        );
    }
}
