// Rush Build - Go Build Rules
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Go build rules for compiling Go code.
//!
//! Provides rules for building Go libraries, binaries, and tests with
//! support for Go modules, build tags, and cross-compilation.

use anyhow::{Context, Result};
use rush_action::{Action, Artifact, Command};
use std::collections::HashMap;
use std::path::PathBuf;

/// Go build mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoBuildMode {
    /// Default build mode.
    Default,

    /// Build as C archive.
    CArchive,

    /// Build as C shared library.
    CShared,

    /// Build as shared library for plugins.
    Plugin,
}

impl GoBuildMode {
    /// Get flag for build mode.
    pub fn flag(&self) -> Option<String> {
        match self {
            GoBuildMode::Default => None,
            GoBuildMode::CArchive => Some("-buildmode=c-archive".to_string()),
            GoBuildMode::CShared => Some("-buildmode=c-shared".to_string()),
            GoBuildMode::Plugin => Some("-buildmode=plugin".to_string()),
        }
    }
}

/// Go library rule attributes.
#[derive(Debug, Clone)]
pub struct GoLibraryAttrs {
    /// Library name (import path).
    pub name: String,

    /// Source files.
    pub srcs: Vec<PathBuf>,

    /// Dependencies (import paths).
    pub deps: Vec<String>,

    /// Build tags.
    pub tags: Vec<String>,

    /// Additional go build flags.
    pub goflags: Vec<String>,

    /// CGo enabled.
    pub cgo: bool,

    /// Embed files/directories.
    pub embed: Vec<PathBuf>,
}

impl GoLibraryAttrs {
    /// Create a new Go library.
    pub fn new(name: String) -> Self {
        GoLibraryAttrs {
            name,
            srcs: Vec::new(),
            deps: Vec::new(),
            tags: Vec::new(),
            goflags: Vec::new(),
            cgo: false,
            embed: Vec::new(),
        }
    }
}

/// Go binary rule attributes.
#[derive(Debug, Clone)]
pub struct GoBinaryAttrs {
    /// Binary name.
    pub name: String,

    /// Source files.
    pub srcs: Vec<PathBuf>,

    /// Dependencies.
    pub deps: Vec<String>,

    /// Build tags.
    pub tags: Vec<String>,

    /// Additional go build flags.
    pub goflags: Vec<String>,

    /// GOOS target.
    pub goos: Option<String>,

    /// GOARCH target.
    pub goarch: Option<String>,

    /// Build mode.
    pub buildmode: GoBuildMode,

    /// Enable race detector.
    pub race: bool,

    /// CGo enabled.
    pub cgo: bool,

    /// Embed files/directories.
    pub embed: Vec<PathBuf>,
}

impl GoBinaryAttrs {
    /// Create a new Go binary.
    pub fn new(name: String) -> Self {
        GoBinaryAttrs {
            name,
            srcs: Vec::new(),
            deps: Vec::new(),
            tags: Vec::new(),
            goflags: Vec::new(),
            goos: None,
            goarch: None,
            buildmode: GoBuildMode::Default,
            race: false,
            cgo: false,
            embed: Vec::new(),
        }
    }
}

/// Go test rule attributes.
#[derive(Debug, Clone)]
pub struct GoTestAttrs {
    /// Test name.
    pub name: String,

    /// Source files (including *_test.go).
    pub srcs: Vec<PathBuf>,

    /// Dependencies.
    pub deps: Vec<String>,

    /// Build tags.
    pub tags: Vec<String>,

    /// Test flags.
    pub testflags: Vec<String>,

    /// Enable race detector.
    pub race: bool,

    /// Run benchmarks.
    pub bench: bool,

    /// Coverage output.
    pub coverage: bool,
}

impl GoTestAttrs {
    /// Create a new Go test.
    pub fn new(name: String) -> Self {
        GoTestAttrs {
            name,
            srcs: Vec::new(),
            deps: Vec::new(),
            tags: Vec::new(),
            testflags: Vec::new(),
            race: false,
            bench: false,
            coverage: false,
        }
    }
}

/// Go build rules implementation.
pub struct GoRules {
    /// Go executable path.
    go_executable: String,

    /// Output directory.
    output_dir: PathBuf,

    /// GOPATH (if using legacy mode).
    gopath: Option<PathBuf>,

    /// Enable Go modules.
    use_modules: bool,
}

impl GoRules {
    /// Create a new Go rules instance.
    pub fn new(output_dir: PathBuf) -> Self {
        GoRules {
            go_executable: "go".to_string(),
            output_dir,
            gopath: None,
            use_modules: true,
        }
    }

    /// Set Go executable path.
    pub fn with_go_executable(mut self, path: String) -> Self {
        self.go_executable = path;
        self
    }

    /// Set GOPATH.
    pub fn with_gopath(mut self, gopath: PathBuf) -> Self {
        self.gopath = Some(gopath);
        self
    }

    /// Disable Go modules.
    pub fn without_modules(mut self) -> Self {
        self.use_modules = false;
        self
    }

    /// Build a Go library.
    pub fn build_library(&self, attrs: &GoLibraryAttrs) -> Result<Vec<Action>> {
        // Go libraries don't produce artifacts directly in Go modules mode
        // They're built as part of dependent binaries/tests

        // For now, we just create a validation action to ensure it compiles
        let mut args = vec!["build".to_string()];

        // Add build tags
        if !attrs.tags.is_empty() {
            args.push("-tags".to_string());
            args.push(attrs.tags.join(","));
        }

        // Add custom flags
        args.extend(attrs.goflags.iter().cloned());

        // Add import path or package
        args.push(".".to_string());

        let env = self.build_env(attrs.cgo);

        let command = Command {
            executable: PathBuf::from(&self.go_executable),
            args,
            working_dir: None,
            execution_info: rush_action::ExecutionInfo::default(),
        };

        let inputs: Vec<_> = attrs
            .srcs
            .iter()
            .map(|p| Artifact::Source(p.clone()))
            .collect();

        let action_id = Action::compute_id(&inputs, &command, &env)?;

        let action = Action {
            id: action_id,
            inputs,
            outputs: Vec::new(), // No output artifact for libraries
            command,
            env,
            mnemonic: "GoLibrary".to_string(),
        };

        Ok(vec![action])
    }

    /// Build a Go binary.
    pub fn build_binary(&self, attrs: &GoBinaryAttrs) -> Result<Vec<Action>> {
        let mut args = vec!["build".to_string()];

        // Add build mode
        if let Some(mode_flag) = attrs.buildmode.flag() {
            args.push(mode_flag);
        }

        // Add build tags
        if !attrs.tags.is_empty() {
            args.push("-tags".to_string());
            args.push(attrs.tags.join(","));
        }

        // Add race detector
        if attrs.race {
            args.push("-race".to_string());
        }

        // Add custom flags
        args.extend(attrs.goflags.iter().cloned());

        // Output path
        let output_path = if cfg!(target_os = "windows") && attrs.goos.as_deref() != Some("linux") {
            self.output_dir.join(format!("{}.exe", attrs.name))
        } else {
            self.output_dir.join(&attrs.name)
        };

        args.push("-o".to_string());
        args.push(output_path.display().to_string());

        // Package to build
        args.push(".".to_string());

        let mut env = self.build_env(attrs.cgo);

        // Set cross-compilation targets
        if let Some(goos) = &attrs.goos {
            env.insert("GOOS".to_string(), goos.clone());
        }
        if let Some(goarch) = &attrs.goarch {
            env.insert("GOARCH".to_string(), goarch.clone());
        }

        let command = Command {
            executable: PathBuf::from(&self.go_executable),
            args,
            working_dir: None,
            execution_info: rush_action::ExecutionInfo::default(),
        };

        let inputs: Vec<_> = attrs
            .srcs
            .iter()
            .map(|p| Artifact::Source(p.clone()))
            .collect();

        let action_id = Action::compute_id(&inputs, &command, &env)?;

        let action = Action {
            id: action_id,
            inputs,
            outputs: vec![Artifact::Source(output_path)],
            command,
            env,
            mnemonic: "GoBinary".to_string(),
        };

        Ok(vec![action])
    }

    /// Build and run Go tests.
    pub fn build_test(&self, attrs: &GoTestAttrs) -> Result<Vec<Action>> {
        let mut args = vec!["test".to_string()];

        // Add build tags
        if !attrs.tags.is_empty() {
            args.push("-tags".to_string());
            args.push(attrs.tags.join(","));
        }

        // Add race detector
        if attrs.race {
            args.push("-race".to_string());
        }

        // Add coverage
        if attrs.coverage {
            let coverage_file = self.output_dir.join(format!("{}_coverage.out", attrs.name));
            args.push("-coverprofile".to_string());
            args.push(coverage_file.display().to_string());
        }

        // Add benchmarks
        if attrs.bench {
            args.push("-bench".to_string());
            args.push(".".to_string());
        }

        // Add verbose
        args.push("-v".to_string());

        // Add custom test flags
        args.extend(attrs.testflags.iter().cloned());

        // Package to test
        args.push(".".to_string());

        let env = self.build_env(false);

        let command = Command {
            executable: PathBuf::from(&self.go_executable),
            args,
            working_dir: None,
            execution_info: rush_action::ExecutionInfo::default(),
        };

        let inputs: Vec<_> = attrs
            .srcs
            .iter()
            .map(|p| Artifact::Source(p.clone()))
            .collect();

        let action_id = Action::compute_id(&inputs, &command, &env)?;

        let action = Action {
            id: action_id,
            inputs,
            outputs: Vec::new(),
            command,
            env,
            mnemonic: "GoTest".to_string(),
        };

        Ok(vec![action])
    }

    /// Build environment variables.
    fn build_env(&self, cgo: bool) -> HashMap<String, String> {
        let mut env = HashMap::new();

        // Set GOPATH if specified
        if let Some(gopath) = &self.gopath {
            env.insert("GOPATH".to_string(), gopath.display().to_string());
        }

        // Set GO111MODULE based on use_modules setting
        if self.use_modules {
            env.insert("GO111MODULE".to_string(), "on".to_string());
        } else {
            env.insert("GO111MODULE".to_string(), "off".to_string());
        }

        // Set CGO_ENABLED
        env.insert(
            "CGO_ENABLED".to_string(),
            if cgo { "1" } else { "0" }.to_string(),
        );

        env
    }

    /// Detect Go version.
    pub fn detect_go_version(&self) -> Result<String> {
        let output = std::process::Command::new(&self.go_executable)
            .arg("version")
            .output()
            .context("Failed to run go version")?;

        if !output.status.success() {
            anyhow::bail!("go version failed");
        }

        let version = String::from_utf8(output.stdout)?;
        Ok(version.trim().to_string())
    }

    /// Initialize Go module.
    pub fn init_module(&self, module_path: &str) -> Result<()> {
        let output = std::process::Command::new(&self.go_executable)
            .args(["mod", "init", module_path])
            .output()
            .context("Failed to initialize Go module")?;

        if !output.status.success() {
            anyhow::bail!("go mod init failed");
        }

        Ok(())
    }

    /// Download dependencies.
    pub fn download_deps(&self) -> Result<()> {
        let output = std::process::Command::new(&self.go_executable)
            .args(["mod", "download"])
            .output()
            .context("Failed to download Go dependencies")?;

        if !output.status.success() {
            anyhow::bail!("go mod download failed");
        }

        Ok(())
    }

    /// Tidy dependencies.
    pub fn tidy_deps(&self) -> Result<()> {
        let output = std::process::Command::new(&self.go_executable)
            .args(["mod", "tidy"])
            .output()
            .context("Failed to tidy Go dependencies")?;

        if !output.status.success() {
            anyhow::bail!("go mod tidy failed");
        }

        Ok(())
    }

    /// Vendor dependencies.
    pub fn vendor_deps(&self) -> Result<()> {
        let output = std::process::Command::new(&self.go_executable)
            .args(["mod", "vendor"])
            .output()
            .context("Failed to vendor Go dependencies")?;

        if !output.status.success() {
            anyhow::bail!("go mod vendor failed");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_go_build_mode_flags() {
        assert_eq!(GoBuildMode::Default.flag(), None);
        assert_eq!(
            GoBuildMode::CArchive.flag(),
            Some("-buildmode=c-archive".to_string())
        );
        assert_eq!(
            GoBuildMode::Plugin.flag(),
            Some("-buildmode=plugin".to_string())
        );
    }

    #[test]
    fn test_library_attrs() {
        let mut attrs = GoLibraryAttrs::new("mylib".to_string());
        attrs.srcs.push(PathBuf::from("lib.go"));
        attrs.tags.push("integration".to_string());

        assert_eq!(attrs.name, "mylib");
        assert_eq!(attrs.srcs.len(), 1);
        assert_eq!(attrs.tags.len(), 1);
    }

    #[test]
    fn test_binary_attrs() {
        let mut attrs = GoBinaryAttrs::new("myapp".to_string());
        attrs.srcs.push(PathBuf::from("main.go"));
        attrs.goos = Some("linux".to_string());
        attrs.goarch = Some("amd64".to_string());

        assert_eq!(attrs.name, "myapp");
        assert_eq!(attrs.goos, Some("linux".to_string()));
        assert_eq!(attrs.goarch, Some("amd64".to_string()));
    }

    #[test]
    fn test_test_attrs() {
        let mut attrs = GoTestAttrs::new("mytest".to_string());
        attrs.srcs.push(PathBuf::from("lib_test.go"));
        attrs.race = true;
        attrs.coverage = true;

        assert_eq!(attrs.name, "mytest");
        assert!(attrs.race);
        assert!(attrs.coverage);
    }

    #[test]
    fn test_build_env() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let rules = GoRules::new(temp_dir.path().to_path_buf());

        let env = rules.build_env(true);
        assert_eq!(env.get("CGO_ENABLED"), Some(&"1".to_string()));
        assert_eq!(env.get("GO111MODULE"), Some(&"on".to_string()));
    }
}
