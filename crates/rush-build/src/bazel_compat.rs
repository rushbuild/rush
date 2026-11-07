// Rush Build - Bazel Compatibility Layer
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Bazel compatibility layer for migration support.
//!
//! Provides compatibility with Bazel BUILD files and command-line interface,
//! enabling incremental migration from Bazel to Rush without rewriting all
//! build files at once.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parse the boolean values accepted by Bazel's command-line option parser.
pub fn parse_bazel_bool(value: &str) -> std::result::Result<bool, String> {
    match value {
        "1" => Ok(true),
        "0" => Ok(false),
        value
            if ["true", "yes", "t", "y"]
                .iter()
                .any(|candidate| value.eq_ignore_ascii_case(candidate)) =>
        {
            Ok(true)
        }
        value
            if ["false", "no", "f", "n"]
                .iter()
                .any(|candidate| value.eq_ignore_ascii_case(candidate)) =>
        {
            Ok(false)
        }
        _ => Err(format!("'{value}' is not a boolean")),
    }
}

/// Bazel compatibility mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatibilityMode {
    /// Strict Bazel compatibility.
    Strict,

    /// Lenient mode with Rush extensions.
    Lenient,

    /// Rush mode with Bazel fallback.
    Hybrid,
}

/// Bazel-to-Rush rule mapper.
pub struct BazelRuleMapper {
    /// Rule mapping table.
    mappings: HashMap<String, RuleMapping>,

    /// Compatibility mode.
    _mode: CompatibilityMode,
}

/// Rule mapping from Bazel to Rush.
#[derive(Debug, Clone)]
struct RuleMapping {
    /// Bazel rule name.
    bazel_name: String,

    /// Rush rule name.
    rush_name: String,

    /// Attribute mappings.
    attribute_mappings: HashMap<String, String>,

    /// Required transformations.
    _transformations: Vec<AttributeTransformation>,
}

/// Attribute transformation.
#[derive(Debug, Clone)]
enum AttributeTransformation {
    /// Rename attribute.
    _Rename { from: String, to: String },

    /// Transform value.
    _Transform {
        attribute: String,
        transform_fn: fn(&str) -> String,
    },

    /// Split into multiple attributes.
    _Split {
        from: String,
        to: Vec<String>,
        split_fn: fn(&str) -> Vec<String>,
    },
}

impl BazelRuleMapper {
    /// Create a new Bazel rule mapper.
    pub fn new(mode: CompatibilityMode) -> Self {
        let mut mapper = BazelRuleMapper {
            mappings: HashMap::new(),
            _mode: mode,
        };

        mapper.register_default_mappings();
        mapper
    }

    /// Register default Bazel-to-Rush rule mappings.
    fn register_default_mappings(&mut self) {
        // C++ rules
        self.register_mapping(RuleMapping {
            bazel_name: "cc_library".to_string(),
            rush_name: "cpp_library".to_string(),
            attribute_mappings: [
                ("name".to_string(), "name".to_string()),
                ("srcs".to_string(), "srcs".to_string()),
                ("hdrs".to_string(), "hdrs".to_string()),
                ("deps".to_string(), "deps".to_string()),
                ("copts".to_string(), "copts".to_string()),
                ("linkopts".to_string(), "linkopts".to_string()),
            ]
            .into_iter()
            .collect(),
            _transformations: vec![],
        });

        self.register_mapping(RuleMapping {
            bazel_name: "cc_binary".to_string(),
            rush_name: "cpp_binary".to_string(),
            attribute_mappings: [
                ("name".to_string(), "name".to_string()),
                ("srcs".to_string(), "srcs".to_string()),
                ("deps".to_string(), "deps".to_string()),
            ]
            .into_iter()
            .collect(),
            _transformations: vec![],
        });

        self.register_mapping(RuleMapping {
            bazel_name: "cc_test".to_string(),
            rush_name: "cpp_test".to_string(),
            attribute_mappings: [
                ("name".to_string(), "name".to_string()),
                ("srcs".to_string(), "srcs".to_string()),
                ("deps".to_string(), "deps".to_string()),
            ]
            .into_iter()
            .collect(),
            _transformations: vec![],
        });

        // Python rules
        self.register_mapping(RuleMapping {
            bazel_name: "py_library".to_string(),
            rush_name: "python_library".to_string(),
            attribute_mappings: [
                ("name".to_string(), "name".to_string()),
                ("srcs".to_string(), "srcs".to_string()),
                ("deps".to_string(), "deps".to_string()),
            ]
            .into_iter()
            .collect(),
            _transformations: vec![],
        });

        self.register_mapping(RuleMapping {
            bazel_name: "py_binary".to_string(),
            rush_name: "python_binary".to_string(),
            attribute_mappings: [
                ("name".to_string(), "name".to_string()),
                ("srcs".to_string(), "srcs".to_string()),
                ("main".to_string(), "main".to_string()),
                ("deps".to_string(), "deps".to_string()),
            ]
            .into_iter()
            .collect(),
            _transformations: vec![],
        });

        // Go rules (if using rules_go)
        self.register_mapping(RuleMapping {
            bazel_name: "go_library".to_string(),
            rush_name: "go_library".to_string(),
            attribute_mappings: [
                ("name".to_string(), "name".to_string()),
                ("srcs".to_string(), "srcs".to_string()),
                ("deps".to_string(), "deps".to_string()),
                ("importpath".to_string(), "import_path".to_string()),
            ]
            .into_iter()
            .collect(),
            _transformations: vec![],
        });

        // Rust rules (if using rules_rust)
        self.register_mapping(RuleMapping {
            bazel_name: "rust_library".to_string(),
            rush_name: "rust_library".to_string(),
            attribute_mappings: [
                ("name".to_string(), "name".to_string()),
                ("srcs".to_string(), "srcs".to_string()),
                ("deps".to_string(), "deps".to_string()),
                ("crate_name".to_string(), "crate_name".to_string()),
            ]
            .into_iter()
            .collect(),
            _transformations: vec![],
        });

        // Generic rules
        self.register_mapping(RuleMapping {
            bazel_name: "genrule".to_string(),
            rush_name: "genrule".to_string(),
            attribute_mappings: [
                ("name".to_string(), "name".to_string()),
                ("srcs".to_string(), "srcs".to_string()),
                ("outs".to_string(), "outs".to_string()),
                ("cmd".to_string(), "cmd".to_string()),
            ]
            .into_iter()
            .collect(),
            _transformations: vec![],
        });

        self.register_mapping(RuleMapping {
            bazel_name: "filegroup".to_string(),
            rush_name: "filegroup".to_string(),
            attribute_mappings: [
                ("name".to_string(), "name".to_string()),
                ("srcs".to_string(), "srcs".to_string()),
            ]
            .into_iter()
            .collect(),
            _transformations: vec![],
        });
    }

    /// Register a rule mapping.
    fn register_mapping(&mut self, mapping: RuleMapping) {
        self.mappings.insert(mapping.bazel_name.clone(), mapping);
    }

    /// Map Bazel rule to Rush rule.
    pub fn map_rule(&self, bazel_rule: &str) -> Option<&str> {
        self.mappings.get(bazel_rule).map(|m| m.rush_name.as_str())
    }

    /// Map Bazel attribute to Rush attribute.
    pub fn map_attribute(&self, bazel_rule: &str, bazel_attr: &str) -> Option<&str> {
        self.mappings
            .get(bazel_rule)
            .and_then(|m| m.attribute_mappings.get(bazel_attr))
            .map(|s| s.as_str())
    }
}

/// Bazel command-line compatibility.
pub struct BazelCli {
    /// Command mappings.
    command_mappings: HashMap<String, String>,
}

impl BazelCli {
    /// Create a new Bazel CLI compatibility layer.
    pub fn new() -> Self {
        let mut cli = BazelCli {
            command_mappings: HashMap::new(),
        };

        cli.register_commands();
        cli
    }

    /// Register Bazel command mappings.
    fn register_commands(&mut self) {
        // Build commands
        self.command_mappings
            .insert("build".to_string(), "build".to_string());
        self.command_mappings
            .insert("test".to_string(), "test".to_string());
        self.command_mappings
            .insert("run".to_string(), "run".to_string());

        // Query commands
        self.command_mappings
            .insert("query".to_string(), "query".to_string());
        self.command_mappings
            .insert("cquery".to_string(), "query".to_string()); // Configured query

        // Info commands
        self.command_mappings
            .insert("info".to_string(), "info".to_string());
        self.command_mappings
            .insert("version".to_string(), "version".to_string());

        // Clean commands
        self.command_mappings
            .insert("clean".to_string(), "clean".to_string());
    }

    /// Map Bazel command to Rush command.
    pub fn map_command(&self, bazel_cmd: &str) -> Option<&str> {
        self.command_mappings.get(bazel_cmd).map(|s| s.as_str())
    }

    /// Translate Bazel flags to Rush flags.
    pub fn translate_flags(&self, bazel_flags: &[String]) -> Vec<String> {
        let mut rush_flags = Vec::new();

        for flag in bazel_flags {
            if flag.starts_with("--config=") {
                // Map --config=<name> to equivalent
                rush_flags.push(flag.clone());
            } else if flag.starts_with("--compilation_mode=") {
                // Map --compilation_mode to --mode
                let mode = flag.strip_prefix("--compilation_mode=").unwrap();
                rush_flags.push(format!("--mode={}", mode));
            } else if flag == "--keep_going" || flag == "-k" {
                rush_flags.push("--keep-going".to_string());
            } else if flag.starts_with("--jobs=") || flag.starts_with("-j") {
                // Map --jobs=N to --jobs=N (compatible)
                rush_flags.push(flag.clone());
            } else if flag.starts_with("--output_filter=") {
                // Map to Rush equivalent
                let filter = flag.strip_prefix("--output_filter=").unwrap();
                rush_flags.push(format!("--output-filter={}", filter));
            } else {
                // Pass through unknown flags in lenient mode
                rush_flags.push(flag.clone());
            }
        }

        rush_flags
    }
}

impl Default for BazelCli {
    fn default() -> Self {
        Self::new()
    }
}

/// Bazel label compatibility.
pub struct BazelLabel {
    /// Workspace name.
    pub workspace: Option<String>,

    /// Package path.
    pub package: String,

    /// Target name.
    pub target: String,
}

impl BazelLabel {
    /// Parse a Bazel label.
    pub fn parse(label: &str) -> Result<Self> {
        if !label.starts_with("//") && !label.starts_with('@') {
            anyhow::bail!("Invalid Bazel label: {}", label);
        }

        let (workspace, rest) = if let Some(stripped) = label.strip_prefix("@//") {
            (None, stripped)
        } else if let Some(stripped) = label.strip_prefix("@@//") {
            // Bzlmod canonical root-module label: same as the main workspace.
            (None, stripped)
        } else if let Some(stripped) = label.strip_prefix('@') {
            // '@@name//...' (canonical) parses like '@name//...': strip the
            // extra '@' so the workspace is the canonical repository name.
            let stripped = stripped.strip_prefix('@').unwrap_or(stripped);
            let parts: Vec<_> = stripped.splitn(2, "//").collect();
            if parts.len() == 2 {
                // Standard format: @repo//package:target
                (Some(parts[0].to_string()), parts[1])
            } else {
                // Repository-only format: @repo (treat as @repo//:repo)
                // Use ":repo" as rest so package is empty (root of repo)
                let repo_name = parts[0];
                let rest_with_colon = format!(":{}", repo_name);
                (
                    Some(repo_name.to_string()),
                    Box::leak(rest_with_colon.into_boxed_str()) as &str,
                )
            }
        } else if let Some(stripped) = label.strip_prefix("//") {
            (None, stripped)
        } else {
            anyhow::bail!("Invalid label: {}", label);
        };

        let parts: Vec<_> = rest.splitn(2, ':').collect();

        let (package, target) = match parts.len() {
            1 => {
                // Implicit target name (same as package name)
                let pkg = parts[0].to_string();
                let tgt = pkg.split('/').next_back().unwrap_or(&pkg).to_string();
                (pkg, tgt)
            }
            2 => (parts[0].to_string(), parts[1].to_string()),
            _ => anyhow::bail!("Invalid label format: {}", label),
        };

        Ok(BazelLabel {
            workspace,
            package,
            target,
        })
    }

    /// Convert to Rush label format.
    pub fn to_rush_label(&self) -> String {
        match &self.workspace {
            Some(ws) => format!("@{}//{}:{}", ws, self.package, self.target),
            None => format!("//{}:{}", self.package, self.target),
        }
    }
}

/// WORKSPACE file parser (simplified).
pub struct WorkspaceParser {
    /// External dependencies.
    pub externals: HashMap<String, ExternalDependency>,
}

/// External dependency.
#[derive(Debug, Clone)]
pub struct ExternalDependency {
    /// Dependency name.
    pub name: String,

    /// Dependency type.
    pub dep_type: ExternalDepType,

    /// URL or path.
    pub location: String,

    /// Version or commit hash.
    pub version: Option<String>,
}

/// External dependency type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalDepType {
    /// HTTP archive.
    HttpArchive,

    /// Git repository.
    GitRepository,

    /// Local path.
    LocalPath,

    /// Maven artifact.
    Maven,
}

impl WorkspaceParser {
    /// Create a new WORKSPACE parser.
    pub fn new() -> Self {
        WorkspaceParser {
            externals: HashMap::new(),
        }
    }

    /// Parse WORKSPACE file.
    pub fn parse<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let content =
            std::fs::read_to_string(path.as_ref()).context("Failed to read WORKSPACE file")?;

        // Simplified parsing - in practice would use Starlark parser
        for line in content.lines() {
            if line.trim().starts_with("http_archive(") {
                // Parse http_archive rule
                // This is a simplified version
            }

            if line.trim().starts_with("git_repository(") {
                // Parse git_repository rule
            }
        }

        Ok(())
    }
}

impl Default for WorkspaceParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Migration assistant.
pub struct MigrationAssistant {
    /// Source directory (Bazel workspace).
    source_dir: PathBuf,

    /// Issues found during migration.
    issues: Vec<MigrationIssue>,
}

/// Migration issue.
#[derive(Debug, Clone)]
pub struct MigrationIssue {
    /// File where issue was found.
    pub file: PathBuf,

    /// Line number.
    pub line: usize,

    /// Issue description.
    pub description: String,

    /// Suggested fix.
    pub suggestion: Option<String>,
}

impl MigrationAssistant {
    /// Create a new migration assistant.
    pub fn new<P: AsRef<Path>>(source_dir: P) -> Self {
        MigrationAssistant {
            source_dir: source_dir.as_ref().to_path_buf(),
            issues: Vec::new(),
        }
    }

    /// Analyze Bazel workspace for migration.
    pub fn analyze(&mut self) -> Result<()> {
        // Find all BUILD files
        use walkdir::WalkDir;

        for entry in WalkDir::new(&self.source_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();

            if path.file_name() == Some("BUILD".as_ref())
                || path.file_name() == Some("BUILD.bazel".as_ref())
            {
                self.analyze_build_file(path)?;
            }
        }

        Ok(())
    }

    /// Analyze a BUILD file.
    fn analyze_build_file(&mut self, path: &Path) -> Result<()> {
        let content = std::fs::read_to_string(path)?;

        for (line_num, line) in content.lines().enumerate() {
            // Check for Bazel-specific features not yet supported
            if line.contains("aspect") {
                self.issues.push(MigrationIssue {
                    file: path.to_path_buf(),
                    line: line_num + 1,
                    description: "Aspects may require manual migration".to_string(),
                    suggestion: Some("Review aspect usage and convert to Rush aspects".to_string()),
                });
            }

            if line.contains("repository_rule") {
                self.issues.push(MigrationIssue {
                    file: path.to_path_buf(),
                    line: line_num + 1,
                    description: "Repository rules need manual conversion".to_string(),
                    suggestion: Some(
                        "Convert repository_rule to Rush-compatible format".to_string(),
                    ),
                });
            }

            if line.contains("select(") {
                self.issues.push(MigrationIssue {
                    file: path.to_path_buf(),
                    line: line_num + 1,
                    description: "Configurable attributes (select()) may need adjustment"
                        .to_string(),
                    suggestion: Some("Verify select() conditions work with Rush".to_string()),
                });
            }
        }

        Ok(())
    }

    /// Get migration issues.
    pub fn issues(&self) -> &[MigrationIssue] {
        &self.issues
    }

    /// Generate migration report.
    pub fn generate_report(&self) -> String {
        let mut report = String::from("Bazel to Rush Migration Report\n");
        report.push_str("===============================\n\n");

        if self.issues.is_empty() {
            report.push_str("No migration issues found. Workspace appears compatible.\n");
        } else {
            report.push_str(&format!(
                "Found {} potential issues:\n\n",
                self.issues.len()
            ));

            for issue in &self.issues {
                report.push_str(&format!(
                    "{}:{} - {}\n",
                    issue.file.display(),
                    issue.line,
                    issue.description
                ));

                if let Some(ref suggestion) = issue.suggestion {
                    report.push_str(&format!("  Suggestion: {}\n", suggestion));
                }

                report.push('\n');
            }
        }

        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rule_mapping() {
        let mapper = BazelRuleMapper::new(CompatibilityMode::Strict);

        assert_eq!(mapper.map_rule("cc_library"), Some("cpp_library"));
        assert_eq!(mapper.map_rule("cc_binary"), Some("cpp_binary"));
        assert_eq!(mapper.map_rule("py_library"), Some("python_library"));
    }

    #[test]
    fn test_label_parsing() {
        let label = BazelLabel::parse("//foo/bar:baz").unwrap();
        assert_eq!(label.workspace, None);
        assert_eq!(label.package, "foo/bar");
        assert_eq!(label.target, "baz");

        let external = BazelLabel::parse("@myrepo//foo:bar").unwrap();
        assert_eq!(external.workspace, Some("myrepo".to_string()));
        assert_eq!(external.package, "foo");
        assert_eq!(external.target, "bar");

        let repository_root = BazelLabel::parse("@myrepo").unwrap();
        assert_eq!(repository_root.workspace, Some("myrepo".to_string()));
        assert_eq!(repository_root.package, "");
        assert_eq!(repository_root.target, "myrepo");
        assert_eq!(repository_root.to_rush_label(), "@myrepo//:myrepo");
    }

    #[test]
    fn test_command_mapping() {
        let cli = BazelCli::new();

        assert_eq!(cli.map_command("build"), Some("build"));
        assert_eq!(cli.map_command("test"), Some("test"));
        assert_eq!(cli.map_command("query"), Some("query"));
    }

    #[test]
    fn test_flag_translation() {
        let cli = BazelCli::new();

        let bazel_flags = vec![
            "--compilation_mode=opt".to_string(),
            "--keep_going".to_string(),
            "--jobs=8".to_string(),
        ];

        let rush_flags = cli.translate_flags(&bazel_flags);

        assert!(rush_flags.contains(&"--mode=opt".to_string()));
        assert!(rush_flags.contains(&"--keep-going".to_string()));
        assert!(rush_flags.contains(&"--jobs=8".to_string()));
    }
}
