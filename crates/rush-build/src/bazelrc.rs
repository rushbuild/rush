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

/// Bounded UTF-8 form of Bazel 6.5 util/strings.cc Tokenize: quoting and
/// backslash escapes preserve argument boundaries, # comments apply outside
/// quotes. Unterminated quotes/escapes are explicitly unsupported.
fn tokenize_rc_line(line: &str) -> Result<Vec<String>> {
    let mut output = Vec::new(); let mut token = String::new();
    let mut quote = None; let mut chars = line.chars();
    while let Some(character) = chars.next() {
        if character == '\\' {
            token.push(chars.next().context("unterminated rc escape")?); continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter { quote = None; } else { token.push(character); }
        } else if character == '#' { break; }
        else if character == '\'' || character == '"' { quote = Some(character); }
        else if matches!(character, ' ' | '\t' | '\r' | '\n') {
            if !token.is_empty() { output.push(std::mem::take(&mut token)); }
        } else { token.push(character); }
    }
    if quote.is_some() { anyhow::bail!("unterminated rc quote"); }
    if !token.is_empty() { output.push(token); }
    Ok(output)
}

impl BazelRc {
    /// Parse a .bazelrc file
    pub fn parse(path: &Path) -> Result<Self> {
        let mut rc = BazelRc {
            common_flags: Vec::new(),
            build_flags: Vec::new(),
            test_flags: Vec::new(),
            run_flags: Vec::new(),
            configs: HashMap::new(),
            startup_flags: Vec::new(),
        };

        let workspace = path.parent().unwrap_or_else(|| Path::new("."));
        rc.parse_file(path, workspace, &mut Vec::new())?;

        Ok(rc)
    }

    fn parse_file(&mut self, path: &Path, workspace: &Path, stack: &mut Vec<std::path::PathBuf>) -> Result<()> {
        let canonical = std::fs::canonicalize(path)
            .with_context(|| format!("resolve rc import {}", path.display()))?;
        if stack.contains(&canonical) { anyhow::bail!("rc import cycle at {}", path.display()); }
        if stack.len() >= 64 { anyhow::bail!("rc import depth exceeds supported bound"); }
        let content = std::fs::read_to_string(&canonical)
            .with_context(|| format!("read rc import {}", canonical.display()))?;
        stack.push(canonical);
        let content = content.replace("\\\r\n", "").replace("\\\n", "");
        for line in content.lines() {
            let tokens = tokenize_rc_line(line)?;
            if tokens.is_empty() { continue; }
            if matches!(tokens[0].as_str(), "import" | "try-import") {
                if tokens.len() != 2 { anyhow::bail!("rc import requires exactly one path"); }
                let raw = tokens[1].replace("%workspace%", workspace.to_str()
                    .context("rc workspace path is not UTF-8")?);
                if raw.contains('%') { anyhow::bail!("unsupported rc import substitution {raw}"); }
                let imported = std::path::PathBuf::from(raw);
                let imported = if imported.is_absolute() { imported } else { workspace.join(imported) };
                if tokens[0] == "try-import" && !imported.try_exists()? { continue; }
                self.parse_file(&imported, workspace, stack)?;
            } else { self.parse_tokens(tokens)?; }
        }
        stack.pop();
        Ok(())
    }

    /// Parse a single .bazelrc line
    fn parse_line(&mut self, line: &str) -> Result<()> {
        self.parse_tokens(tokenize_rc_line(line)?)
    }

    fn parse_tokens(&mut self, parts: Vec<String>) -> Result<()> {
        if parts.is_empty() { return Ok(()); }
        let command = parts[0].as_str();
        let flags: Vec<String> = parts[1..].to_vec();

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
    fn rc_quotes_comments_escapes_and_continuations_match_bazel_65() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".bazelrc");
        std::fs::write(&path, concat!(
            r#"build --define SPACE='a b' --define ESC=a\ b --define HASH='a#b' # comment"#,
            "\n", "build --define CONT=ab", "\\", "\n", "cd\n",
        )).unwrap();
        let parsed = BazelRc::parse(&path).unwrap();
        assert_eq!(parsed.build_flags, ["--define","SPACE=a b","--define","ESC=a b","--define","HASH=a#b","--define","CONT=abcd"]);
        assert!(tokenize_rc_line("build --define='unterminated").is_err());
    }

    #[test]
    fn rc_imports_use_workspace_relative_paths_and_refuse_cycles_or_required_missing() {
        let temp=tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("rc")).unwrap();
        let path=temp.path().join(".bazelrc");
        std::fs::write(&path,"import %workspace%/rc/first.rc\ntry-import absent.rc\n").unwrap();
        std::fs::write(temp.path().join("rc/first.rc"),"build --define KEY=first\nimport choice.rc\n").unwrap();
        std::fs::write(temp.path().join("choice.rc"),"build --define KEY=workspace\n").unwrap();
        std::fs::write(temp.path().join("rc/choice.rc"),"build --define KEY=relative\n").unwrap();
        assert_eq!(BazelRc::parse(&path).unwrap().build_flags,
            ["--define","KEY=first","--define","KEY=workspace"]);
        std::fs::write(temp.path().join("choice.rc"),"import %workspace%/.bazelrc\n").unwrap();
        assert!(BazelRc::parse(&path).unwrap_err().to_string().contains("cycle"));
        std::fs::write(&path,"import missing.rc\n").unwrap();
        assert!(BazelRc::parse(&path).is_err());
    }

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
