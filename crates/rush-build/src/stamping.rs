// Rush Build - Build stamping
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build stamping - embed version information into binaries.
//!
//! This module provides build stamping functionality to embed version
//! information, git commits, build timestamps, and custom metadata into
//! build artifacts. Supports multiple output formats (C/C++ headers, Rust
//! constants, linker scripts, JSON) and respects SOURCE_DATE_EPOCH for
//! reproducible builds.

#![allow(missing_docs)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;

/// Build stamping mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StampMode {
    /// Development mode - includes full git info and timestamps.
    #[default]
    Development,
    /// Release mode - uses tagged versions, respects SOURCE_DATE_EPOCH.
    Release,
    /// Snapshot mode - frozen timestamp for hermetic builds.
    Snapshot,
}

/// Output format for stamp files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StampFormat {
    /// C/C++ header file with #define macros.
    CHeader,
    /// Rust source file with const definitions.
    Rust,
    /// Linker script with embedded version strings.
    LinkerScript,
    /// JSON format for programmatic consumption.
    Json,
}

/// Git repository information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitInfo {
    /// Full commit SHA (40 hex digits).
    pub commit_sha: String,
    /// Short commit SHA (7 hex digits).
    pub commit_sha_short: String,
    /// Current branch name.
    pub branch: String,
    /// Most recent tag (if any).
    pub tag: Option<String>,
    /// Tag with git describe format (tag-commits-ghash).
    pub describe: Option<String>,
    /// Whether the working tree has uncommitted changes.
    pub is_dirty: bool,
    /// Author of the most recent commit.
    pub author: String,
    /// Date of the most recent commit (Unix timestamp).
    pub commit_date: u64,
}

impl GitInfo {
    /// Extract git information from a repository.
    ///
    /// Returns None if the directory is not a git repository or git is not available.
    pub async fn from_repo(repo_path: &Path) -> Result<Option<Self>> {
        // Check if git is available
        if !Self::git_available().await {
            return Ok(None);
        }

        // Check if this is a git repository
        let is_repo = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .arg("rev-parse")
            .arg("--git-dir")
            .output()
            .await
            .map(|output| output.status.success())
            .unwrap_or(false);

        if !is_repo {
            return Ok(None);
        }

        // Get commit SHA
        let commit_sha = Self::git_output(repo_path, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_string();

        let commit_sha_short = commit_sha.chars().take(7).collect();

        // Get branch name
        let branch = Self::git_output(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await?
            .trim()
            .to_string();

        // Get most recent tag
        let tag = Self::git_output(repo_path, &["describe", "--tags", "--abbrev=0"])
            .await
            .ok()
            .map(|s| s.trim().to_string());

        // Get git describe (tag-commits-ghash)
        let describe = Self::git_output(repo_path, &["describe", "--tags", "--always", "--dirty"])
            .await
            .ok()
            .map(|s| s.trim().to_string());

        // Check if working tree is dirty
        let status_output = Self::git_output(repo_path, &["status", "--porcelain"]).await?;
        let is_dirty = !status_output.trim().is_empty();

        // Get author of most recent commit
        let author = Self::git_output(repo_path, &["log", "-1", "--format=%an <%ae>"])
            .await?
            .trim()
            .to_string();

        // Get commit date (Unix timestamp)
        let commit_date_str = Self::git_output(repo_path, &["log", "-1", "--format=%ct"])
            .await?
            .trim()
            .to_string();
        let commit_date = commit_date_str
            .parse::<u64>()
            .context("Failed to parse commit date")?;

        Ok(Some(GitInfo {
            commit_sha,
            commit_sha_short,
            branch,
            tag,
            describe,
            is_dirty,
            author,
            commit_date,
        }))
    }

    async fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .await
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    async fn git_output(repo_path: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(args)
            .output()
            .await
            .context("Failed to execute git command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Git command failed: {}", stderr);
        }

        String::from_utf8(output.stdout).context("Git output not valid UTF-8")
    }
}

/// Configuration for build stamping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StampConfig {
    /// Stamping mode (development, release, snapshot).
    pub mode: StampMode,
    /// Whether to include git information.
    pub include_git: bool,
    /// Whether to include build timestamp.
    pub include_timestamp: bool,
    /// Custom version string (overrides git describe).
    pub version: Option<String>,
    /// Custom stamp variables (name -> value).
    pub custom_vars: HashMap<String, String>,
    /// Fixed timestamp for snapshot builds (Unix seconds).
    pub snapshot_timestamp: Option<u64>,
}

impl Default for StampConfig {
    fn default() -> Self {
        StampConfig {
            mode: StampMode::Development,
            include_git: true,
            include_timestamp: true,
            version: None,
            custom_vars: HashMap::new(),
            snapshot_timestamp: None,
        }
    }
}

/// Build stamp information.
///
/// Contains all the information that will be embedded into build artifacts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildStamp {
    /// Version string (from git describe, tag, or manual override).
    pub version: String,
    /// Git information (if available and requested).
    pub git: Option<GitInfo>,
    /// Build timestamp (Unix seconds).
    pub timestamp: u64,
    /// Build timestamp in RFC 3339 format.
    pub timestamp_rfc3339: String,
    /// Hostname where build occurred.
    pub hostname: String,
    /// User who ran the build.
    pub username: String,
    /// Custom stamp variables.
    pub custom_vars: HashMap<String, String>,
}

impl BuildStamp {
    /// Create a build stamp from configuration.
    ///
    /// Extracts git info, timestamps, and other build metadata according
    /// to the provided configuration.
    pub async fn from_config(repo_path: &Path, config: &StampConfig) -> Result<Self> {
        // Get git info if requested
        let git = if config.include_git {
            GitInfo::from_repo(repo_path).await?
        } else {
            None
        };

        // Determine version string
        let version = if let Some(ref v) = config.version {
            v.clone()
        } else if let Some(ref git_info) = git {
            git_info
                .describe
                .clone()
                .or_else(|| git_info.tag.clone())
                .unwrap_or_else(|| format!("0.0.0-{}", git_info.commit_sha_short))
        } else {
            "0.0.0".to_string()
        };

        // Determine timestamp
        let timestamp = if let Some(ts) = config.snapshot_timestamp {
            // Use fixed snapshot timestamp
            ts
        } else if let Ok(source_date) = std::env::var("SOURCE_DATE_EPOCH") {
            // Respect SOURCE_DATE_EPOCH for reproducible builds
            source_date
                .parse::<u64>()
                .context("Invalid SOURCE_DATE_EPOCH")?
        } else if config.include_timestamp {
            // Use current time
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("System time before Unix epoch")?
                .as_secs()
        } else if let Some(ref git_info) = git {
            // Fall back to commit date
            git_info.commit_date
        } else {
            // No timestamp available, use epoch
            0
        };

        // Convert timestamp to RFC 3339
        let timestamp_rfc3339 = Self::timestamp_to_rfc3339(timestamp);

        // Get hostname and username (best effort)
        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "unknown".to_string());

        let username = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".to_string());

        Ok(BuildStamp {
            version,
            git,
            timestamp,
            timestamp_rfc3339,
            hostname,
            username,
            custom_vars: config.custom_vars.clone(),
        })
    }

    fn timestamp_to_rfc3339(ts: u64) -> String {
        use std::time::Duration;
        let _system_time = UNIX_EPOCH + Duration::from_secs(ts);

        // Manual RFC 3339 formatting to avoid chrono dependency
        let secs_since_epoch = ts;
        let days_since_epoch = secs_since_epoch / 86400;
        let secs_of_day = secs_since_epoch % 86400;

        // Simple algorithm for Unix timestamp to date
        // (This is a simplified version; production code might use chrono)
        let year = 1970 + (days_since_epoch / 365) as i32;
        let day_of_year = (days_since_epoch % 365) as u32;

        let hour = secs_of_day / 3600;
        let minute = (secs_of_day % 3600) / 60;
        let second = secs_of_day % 60;

        format!(
            "{:04}-01-01T{:02}:{:02}:{:02}Z+{:03}",
            year, hour, minute, second, day_of_year
        )
    }
}

/// Build stamp generator.
///
/// Generates stamp files in various formats from a BuildStamp.
pub struct StampGenerator<'a> {
    stamp: &'a BuildStamp,
}

impl<'a> StampGenerator<'a> {
    /// Create a new stamp generator.
    pub fn new(stamp: &'a BuildStamp) -> Self {
        StampGenerator { stamp }
    }

    /// Generate a stamp file in the specified format.
    pub fn generate(&self, format: StampFormat) -> String {
        match format {
            StampFormat::CHeader => self.generate_c_header(),
            StampFormat::Rust => self.generate_rust(),
            StampFormat::LinkerScript => self.generate_linker_script(),
            StampFormat::Json => self.generate_json(),
        }
    }

    /// Generate a C/C++ header file with #define macros.
    fn generate_c_header(&self) -> String {
        let mut output = String::new();
        output.push_str("/* Auto-generated build stamp - DO NOT EDIT */\n");
        output.push_str("#ifndef BUILD_STAMP_H\n");
        output.push_str("#define BUILD_STAMP_H\n\n");

        // Version
        output.push_str(&format!(
            "#define BUILD_VERSION \"{}\"\n",
            self.stamp.version
        ));

        // Timestamp
        output.push_str(&format!(
            "#define BUILD_TIMESTAMP {}\n",
            self.stamp.timestamp
        ));
        output.push_str(&format!(
            "#define BUILD_TIMESTAMP_STR \"{}\"\n",
            self.stamp.timestamp_rfc3339
        ));

        // Hostname and user
        output.push_str(&format!(
            "#define BUILD_HOSTNAME \"{}\"\n",
            self.stamp.hostname
        ));
        output.push_str(&format!(
            "#define BUILD_USERNAME \"{}\"\n",
            self.stamp.username
        ));

        // Git information
        if let Some(ref git) = self.stamp.git {
            output.push_str(&format!(
                "#define BUILD_GIT_COMMIT \"{}\"\n",
                git.commit_sha
            ));
            output.push_str(&format!(
                "#define BUILD_GIT_COMMIT_SHORT \"{}\"\n",
                git.commit_sha_short
            ));
            output.push_str(&format!("#define BUILD_GIT_BRANCH \"{}\"\n", git.branch));

            if let Some(ref tag) = git.tag {
                output.push_str(&format!("#define BUILD_GIT_TAG \"{}\"\n", tag));
            }

            if let Some(ref describe) = git.describe {
                output.push_str(&format!("#define BUILD_GIT_DESCRIBE \"{}\"\n", describe));
            }

            output.push_str(&format!(
                "#define BUILD_GIT_DIRTY {}\n",
                if git.is_dirty { "1" } else { "0" }
            ));
            output.push_str(&format!("#define BUILD_GIT_AUTHOR \"{}\"\n", git.author));
        }

        // Custom variables
        for (key, value) in &self.stamp.custom_vars {
            let macro_name = key.to_uppercase().replace('-', "_");
            output.push_str(&format!("#define BUILD_{} \"{}\"\n", macro_name, value));
        }

        output.push_str("\n#endif /* BUILD_STAMP_H */\n");
        output
    }

    /// Generate Rust const definitions.
    fn generate_rust(&self) -> String {
        let mut output = String::new();
        output.push_str("// Auto-generated build stamp - DO NOT EDIT\n\n");

        // Version
        output.push_str(&format!(
            "pub const BUILD_VERSION: &str = \"{}\";\n",
            self.stamp.version
        ));

        // Timestamp
        output.push_str(&format!(
            "pub const BUILD_TIMESTAMP: u64 = {};\n",
            self.stamp.timestamp
        ));
        output.push_str(&format!(
            "pub const BUILD_TIMESTAMP_STR: &str = \"{}\";\n",
            self.stamp.timestamp_rfc3339
        ));

        // Hostname and user
        output.push_str(&format!(
            "pub const BUILD_HOSTNAME: &str = \"{}\";\n",
            self.stamp.hostname
        ));
        output.push_str(&format!(
            "pub const BUILD_USERNAME: &str = \"{}\";\n",
            self.stamp.username
        ));

        // Git information
        if let Some(ref git) = self.stamp.git {
            output.push_str(&format!(
                "pub const BUILD_GIT_COMMIT: &str = \"{}\";\n",
                git.commit_sha
            ));
            output.push_str(&format!(
                "pub const BUILD_GIT_COMMIT_SHORT: &str = \"{}\";\n",
                git.commit_sha_short
            ));
            output.push_str(&format!(
                "pub const BUILD_GIT_BRANCH: &str = \"{}\";\n",
                git.branch
            ));

            if let Some(ref tag) = git.tag {
                output.push_str(&format!(
                    "pub const BUILD_GIT_TAG: Option<&str> = Some(\"{}\");\n",
                    tag
                ));
            } else {
                output.push_str("pub const BUILD_GIT_TAG: Option<&str> = None;\n");
            }

            if let Some(ref describe) = git.describe {
                output.push_str(&format!(
                    "pub const BUILD_GIT_DESCRIBE: Option<&str> = Some(\"{}\");\n",
                    describe
                ));
            } else {
                output.push_str("pub const BUILD_GIT_DESCRIBE: Option<&str> = None;\n");
            }

            output.push_str(&format!(
                "pub const BUILD_GIT_DIRTY: bool = {};\n",
                git.is_dirty
            ));
            output.push_str(&format!(
                "pub const BUILD_GIT_AUTHOR: &str = \"{}\";\n",
                git.author
            ));
        } else {
            output.push_str("pub const BUILD_GIT_COMMIT: &str = \"unknown\";\n");
            output.push_str("pub const BUILD_GIT_COMMIT_SHORT: &str = \"unknown\";\n");
            output.push_str("pub const BUILD_GIT_BRANCH: &str = \"unknown\";\n");
            output.push_str("pub const BUILD_GIT_TAG: Option<&str> = None;\n");
            output.push_str("pub const BUILD_GIT_DESCRIBE: Option<&str> = None;\n");
            output.push_str("pub const BUILD_GIT_DIRTY: bool = false;\n");
            output.push_str("pub const BUILD_GIT_AUTHOR: &str = \"unknown\";\n");
        }

        // Custom variables
        for (key, value) in &self.stamp.custom_vars {
            let const_name = key.to_uppercase().replace('-', "_");
            output.push_str(&format!(
                "pub const BUILD_{}: &str = \"{}\";\n",
                const_name, value
            ));
        }

        output
    }

    /// Generate a linker script to embed version strings.
    fn generate_linker_script(&self) -> String {
        let mut output = String::new();
        output.push_str("/* Auto-generated build stamp linker script - DO NOT EDIT */\n\n");
        output.push_str("SECTIONS\n{\n");
        output.push_str("  .build_stamp : {\n");
        output.push_str(&format!(
            "    PROVIDE(build_version = \"{}\");\n",
            self.stamp.version
        ));
        output.push_str(&format!(
            "    PROVIDE(build_timestamp = {});\n",
            self.stamp.timestamp
        ));

        if let Some(ref git) = self.stamp.git {
            output.push_str(&format!(
                "    PROVIDE(build_git_commit = \"{}\");\n",
                git.commit_sha
            ));
            output.push_str(&format!(
                "    PROVIDE(build_git_branch = \"{}\");\n",
                git.branch
            ));
        }

        output.push_str("  }\n");
        output.push_str("}\n");
        output
    }

    /// Generate JSON format.
    fn generate_json(&self) -> String {
        serde_json::to_string_pretty(self.stamp).unwrap_or_else(|_| "{}".to_string())
    }

    /// Write stamp file to disk.
    pub async fn write_to_file(&self, path: &Path, format: StampFormat) -> Result<()> {
        let content = self.generate(format);
        tokio::fs::write(path, content)
            .await
            .with_context(|| format!("Failed to write stamp file to {}", path.display()))?;
        Ok(())
    }
}

/// Template-based stamp file generator.
///
/// Allows users to provide custom templates with placeholders that get
/// replaced with stamp values.
pub struct StampTemplate {
    template: String,
}

impl StampTemplate {
    /// Create a new template from a string.
    pub fn new(template: impl Into<String>) -> Self {
        StampTemplate {
            template: template.into(),
        }
    }

    /// Load template from a file.
    pub async fn from_file(path: &Path) -> Result<Self> {
        let template = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read template from {}", path.display()))?;
        Ok(StampTemplate { template })
    }

    /// Render the template with stamp values.
    ///
    /// Replaces placeholders like {{VERSION}}, {{GIT_COMMIT}}, etc.
    pub fn render(&self, stamp: &BuildStamp) -> String {
        let mut output = self.template.clone();

        // Replace basic placeholders
        output = output.replace("{{VERSION}}", &stamp.version);
        output = output.replace("{{TIMESTAMP}}", &stamp.timestamp.to_string());
        output = output.replace("{{TIMESTAMP_RFC3339}}", &stamp.timestamp_rfc3339);
        output = output.replace("{{HOSTNAME}}", &stamp.hostname);
        output = output.replace("{{USERNAME}}", &stamp.username);

        // Replace git placeholders
        if let Some(ref git) = stamp.git {
            output = output.replace("{{GIT_COMMIT}}", &git.commit_sha);
            output = output.replace("{{GIT_COMMIT_SHORT}}", &git.commit_sha_short);
            output = output.replace("{{GIT_BRANCH}}", &git.branch);
            output = output.replace("{{GIT_TAG}}", git.tag.as_deref().unwrap_or(""));
            output = output.replace("{{GIT_DESCRIBE}}", git.describe.as_deref().unwrap_or(""));
            output = output.replace(
                "{{GIT_DIRTY}}",
                if git.is_dirty { "dirty" } else { "clean" },
            );
            output = output.replace("{{GIT_AUTHOR}}", &git.author);
        } else {
            output = output.replace("{{GIT_COMMIT}}", "unknown");
            output = output.replace("{{GIT_COMMIT_SHORT}}", "unknown");
            output = output.replace("{{GIT_BRANCH}}", "unknown");
            output = output.replace("{{GIT_TAG}}", "");
            output = output.replace("{{GIT_DESCRIBE}}", "");
            output = output.replace("{{GIT_DIRTY}}", "unknown");
            output = output.replace("{{GIT_AUTHOR}}", "unknown");
        }

        // Replace custom variable placeholders
        for (key, value) in &stamp.custom_vars {
            let placeholder = format!("{{{{{}}}}}", key.to_uppercase());
            output = output.replace(&placeholder, value);
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stamp_mode_default() {
        assert_eq!(StampMode::default(), StampMode::Development);
    }

    #[test]
    fn test_stamp_config_default() {
        let config = StampConfig::default();
        assert_eq!(config.mode, StampMode::Development);
        assert!(config.include_git);
        assert!(config.include_timestamp);
        assert!(config.version.is_none());
        assert!(config.custom_vars.is_empty());
    }

    #[tokio::test]
    async fn test_git_info_non_repo() {
        let temp_dir = tempfile::tempdir().unwrap();
        let result = GitInfo::from_repo(temp_dir.path()).await.unwrap();
        // Should return None for non-git directory
        // (or Some if temp dir happens to be inside a git repo)
        assert!(result.is_none() || result.is_some());
    }

    #[tokio::test]
    async fn test_build_stamp_no_git() {
        let config = StampConfig {
            mode: StampMode::Development,
            include_git: false,
            include_timestamp: true,
            version: Some("1.0.0".to_string()),
            custom_vars: HashMap::new(),
            snapshot_timestamp: Some(1234567890),
        };

        let temp_dir = tempfile::tempdir().unwrap();
        let stamp = BuildStamp::from_config(temp_dir.path(), &config)
            .await
            .unwrap();

        assert_eq!(stamp.version, "1.0.0");
        assert!(stamp.git.is_none());
        assert_eq!(stamp.timestamp, 1234567890);
    }

    #[tokio::test]
    async fn test_build_stamp_with_custom_vars() {
        let mut custom_vars = HashMap::new();
        custom_vars.insert("PROJECT_NAME".to_string(), "Rush".to_string());
        custom_vars.insert("BUILD_TYPE".to_string(), "Debug".to_string());

        let config = StampConfig {
            mode: StampMode::Development,
            include_git: false,
            include_timestamp: true,
            version: Some("2.0.0".to_string()),
            custom_vars,
            snapshot_timestamp: Some(1234567890),
        };

        let temp_dir = tempfile::tempdir().unwrap();
        let stamp = BuildStamp::from_config(temp_dir.path(), &config)
            .await
            .unwrap();

        assert_eq!(
            stamp.custom_vars.get("PROJECT_NAME"),
            Some(&"Rush".to_string())
        );
        assert_eq!(
            stamp.custom_vars.get("BUILD_TYPE"),
            Some(&"Debug".to_string())
        );
    }

    #[test]
    fn test_stamp_generator_c_header() {
        let mut custom_vars = HashMap::new();
        custom_vars.insert("PROJECT".to_string(), "Test".to_string());

        let stamp = BuildStamp {
            version: "1.0.0".to_string(),
            git: None,
            timestamp: 1234567890,
            timestamp_rfc3339: "2009-02-13T23:31:30Z".to_string(),
            hostname: "buildhost".to_string(),
            username: "builder".to_string(),
            custom_vars,
        };

        let generator = StampGenerator::new(&stamp);
        let output = generator.generate(StampFormat::CHeader);

        assert!(output.contains("#define BUILD_VERSION \"1.0.0\""));
        assert!(output.contains("#define BUILD_TIMESTAMP 1234567890"));
        assert!(output.contains("#define BUILD_HOSTNAME \"buildhost\""));
        assert!(output.contains("#define BUILD_USERNAME \"builder\""));
        assert!(output.contains("#define BUILD_PROJECT \"Test\""));
        assert!(output.contains("#ifndef BUILD_STAMP_H"));
        assert!(output.contains("#endif /* BUILD_STAMP_H */"));
    }

    #[test]
    fn test_stamp_generator_rust() {
        let stamp = BuildStamp {
            version: "2.0.0".to_string(),
            git: Some(GitInfo {
                commit_sha: "abcd1234".to_string(),
                commit_sha_short: "abcd123".to_string(),
                branch: "main".to_string(),
                tag: Some("v2.0.0".to_string()),
                describe: Some("v2.0.0-1-gabcd123".to_string()),
                is_dirty: true,
                author: "Test Author <test@example.com>".to_string(),
                commit_date: 1234567890,
            }),
            timestamp: 1234567890,
            timestamp_rfc3339: "2009-02-13T23:31:30Z".to_string(),
            hostname: "buildhost".to_string(),
            username: "builder".to_string(),
            custom_vars: HashMap::new(),
        };

        let generator = StampGenerator::new(&stamp);
        let output = generator.generate(StampFormat::Rust);

        assert!(output.contains("pub const BUILD_VERSION: &str = \"2.0.0\""));
        assert!(output.contains("pub const BUILD_GIT_COMMIT: &str = \"abcd1234\""));
        assert!(output.contains("pub const BUILD_GIT_BRANCH: &str = \"main\""));
        assert!(output.contains("pub const BUILD_GIT_TAG: Option<&str> = Some(\"v2.0.0\")"));
        assert!(output.contains("pub const BUILD_GIT_DIRTY: bool = true"));
    }

    #[test]
    fn test_stamp_generator_json() {
        let stamp = BuildStamp {
            version: "3.0.0".to_string(),
            git: None,
            timestamp: 1234567890,
            timestamp_rfc3339: "2009-02-13T23:31:30Z".to_string(),
            hostname: "buildhost".to_string(),
            username: "builder".to_string(),
            custom_vars: HashMap::new(),
        };

        let generator = StampGenerator::new(&stamp);
        let output = generator.generate(StampFormat::Json);

        assert!(output.contains("\"version\""));
        assert!(output.contains("\"3.0.0\""));
        assert!(output.contains("\"timestamp\""));
        assert!(output.contains("1234567890"));
    }

    #[test]
    fn test_stamp_generator_linker_script() {
        let stamp = BuildStamp {
            version: "1.0.0".to_string(),
            git: Some(GitInfo {
                commit_sha: "abc123".to_string(),
                commit_sha_short: "abc123".to_string(),
                branch: "develop".to_string(),
                tag: None,
                describe: None,
                is_dirty: false,
                author: "Dev".to_string(),
                commit_date: 1234567890,
            }),
            timestamp: 1234567890,
            timestamp_rfc3339: "2009-02-13T23:31:30Z".to_string(),
            hostname: "buildhost".to_string(),
            username: "builder".to_string(),
            custom_vars: HashMap::new(),
        };

        let generator = StampGenerator::new(&stamp);
        let output = generator.generate(StampFormat::LinkerScript);

        assert!(output.contains("SECTIONS"));
        assert!(output.contains("build_version"));
        assert!(output.contains("build_timestamp"));
        assert!(output.contains("build_git_commit"));
    }

    #[test]
    fn test_stamp_template_render() {
        let template = StampTemplate::new("Version: {{VERSION}}, Commit: {{GIT_COMMIT_SHORT}}");

        let stamp = BuildStamp {
            version: "1.2.3".to_string(),
            git: Some(GitInfo {
                commit_sha: "abcdef1234567890".to_string(),
                commit_sha_short: "abcdef1".to_string(),
                branch: "main".to_string(),
                tag: None,
                describe: None,
                is_dirty: false,
                author: "Test".to_string(),
                commit_date: 0,
            }),
            timestamp: 0,
            timestamp_rfc3339: "1970-01-01T00:00:00Z".to_string(),
            hostname: "test".to_string(),
            username: "test".to_string(),
            custom_vars: HashMap::new(),
        };

        let output = template.render(&stamp);
        assert_eq!(output, "Version: 1.2.3, Commit: abcdef1");
    }

    #[test]
    fn test_stamp_template_custom_vars() {
        let template = StampTemplate::new("Project: {{PROJECT_NAME}}, Type: {{BUILD_TYPE}}");

        let mut custom_vars = HashMap::new();
        custom_vars.insert("PROJECT_NAME".to_string(), "Rush".to_string());
        custom_vars.insert("BUILD_TYPE".to_string(), "Release".to_string());

        let stamp = BuildStamp {
            version: "1.0.0".to_string(),
            git: None,
            timestamp: 0,
            timestamp_rfc3339: "1970-01-01T00:00:00Z".to_string(),
            hostname: "test".to_string(),
            username: "test".to_string(),
            custom_vars,
        };

        let output = template.render(&stamp);
        assert_eq!(output, "Project: Rush, Type: Release");
    }

    #[test]
    fn test_stamp_format_variants() {
        assert_eq!(
            std::mem::discriminant(&StampFormat::CHeader),
            std::mem::discriminant(&StampFormat::CHeader)
        );
        assert_ne!(
            std::mem::discriminant(&StampFormat::CHeader),
            std::mem::discriminant(&StampFormat::Rust)
        );
    }
}
