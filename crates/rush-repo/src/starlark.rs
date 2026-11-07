// Rush Repo - Starlark loader
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Basic Starlark (.bzl) file loader.
//!
//! Provides functionality to load and evaluate Starlark (.bzl) files, which
//! are used to define custom repository rules and macros in Bazel-style
//! build systems.
//!
//! # Phase 1 Implementation
//!
//! This initial implementation uses regex-based extraction to parse common
//! Starlark patterns, particularly focusing on Envoy's `external_http_archive()`
//! and similar repository rule calls. Full Starlark interpreter integration
//! is planned for a future release.

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::workspace::{RepositoryRule, Value};

// Lazy-compiled regexes for performance
static RULE_PATTERN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?m)^[ \t]*([a-z_][a-z0-9_]*)\s*\("#).unwrap());

static ATTR_PATTERN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"^([a-z_][a-z0-9_]*)\s*=\s*"#).unwrap());

/// Loader for Starlark (.bzl) files.
///
/// Handles loading, parsing, and evaluating Starlark files that define
/// repository rules, macros, and other build-related functions.
///
/// # Implementation Strategy
///
/// For Phase 1, this loader uses regex-based extraction similar to the
/// WORKSPACE parser. It focuses on extracting repository rule calls from
/// .bzl files, particularly patterns used in Envoy's repositories.bzl.
pub struct StarlarkLoader {
    /// Root directory of the workspace
    workspace_root: PathBuf,

    /// Loaded file contents keyed by normalized path
    loaded_files: HashMap<String, String>,

    /// Extracted repository rules from loaded files
    repository_rules: Vec<RepositoryRule>,
}

impl StarlarkLoader {
    /// Create a new Starlark loader with the given workspace root.
    ///
    /// # Arguments
    ///
    /// * `workspace_root` - Path to the workspace root directory
    pub fn new<P: AsRef<Path>>(workspace_root: P) -> Self {
        Self {
            workspace_root: workspace_root.as_ref().to_path_buf(),
            loaded_files: HashMap::new(),
            repository_rules: Vec::new(),
        }
    }

    /// Load and parse a Starlark (.bzl) file.
    ///
    /// This method reads the file, extracts repository rule calls, and stores
    /// them for later retrieval.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the .bzl file to load (relative to workspace root)
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success, or an error if loading/parsing fails.
    pub fn load_file<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let path_ref = path.as_ref();
        let full_path = if path_ref.is_absolute() {
            path_ref.to_path_buf()
        } else {
            self.workspace_root.join(path_ref)
        };

        tracing::debug!("Loading Starlark file: {}", full_path.display());

        let content = std::fs::read_to_string(&full_path)
            .with_context(|| format!("Failed to read .bzl file: {}", full_path.display()))?;

        // Store the file content
        let normalized_path = path_ref.to_string_lossy().to_string();
        self.loaded_files
            .insert(normalized_path.clone(), content.clone());

        // Extract repository rules from the file
        let rules = self.extract_repository_rules(&content)?;

        tracing::info!(
            "Extracted {} repository rules from {}",
            rules.len(),
            path_ref.display()
        );

        self.repository_rules.extend(rules);

        Ok(())
    }

    /// Evaluate a load() statement and load the referenced .bzl file.
    ///
    /// This method resolves the load path and recursively loads the referenced
    /// .bzl file.
    ///
    /// # Arguments
    ///
    /// * `load_stmt` - The path from a load() statement (e.g., "@bazel_tools//tools/build_defs/repo:http.bzl")
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success, or an error if the file cannot be loaded.
    pub fn evaluate_load(&mut self, load_stmt: &str) -> Result<()> {
        // For now, we just track that we've seen this load statement
        // Full resolution of external repositories would require more infrastructure
        tracing::debug!("Processing load statement: {}", load_stmt);

        // If it's a local file (starts with // or :), try to load it
        if load_stmt.starts_with("//") || load_stmt.starts_with(":") {
            let local_path = self.resolve_local_path(load_stmt)?;
            if local_path.exists() {
                self.load_file(&local_path)?;
            } else {
                tracing::warn!("Load path does not exist: {}", local_path.display());
            }
        }

        Ok(())
    }

    /// Get a repository rule by name.
    ///
    /// # Arguments
    ///
    /// * `name` - Name of the repository to retrieve
    ///
    /// # Returns
    ///
    /// Returns the repository rule if found, or None.
    pub fn get_symbol(&self, name: &str) -> Option<&RepositoryRule> {
        self.repository_rules.iter().find(|r| r.name == name)
    }

    /// Get all extracted repository rules.
    pub fn get_repository_rules(&self) -> &[RepositoryRule] {
        &self.repository_rules
    }

    /// Extract repository rules from .bzl file content.
    ///
    /// This method looks for common patterns like:
    /// - `http_archive(...)`
    /// - `git_repository(...)`
    /// - `external_http_archive(...)`
    /// - Custom macros that wrap repository rules
    fn extract_repository_rules(&self, content: &str) -> Result<Vec<RepositoryRule>> {
        let mut rules = Vec::new();

        // Remove comments first
        let content = self.remove_comments(content);

        // Match repository rule calls
        // Matches: rule_name(...)
        for cap in RULE_PATTERN_RE.captures_iter(&content) {
            let rule_type_str = cap.get(1).unwrap().as_str();
            let match_start = cap.get(0).unwrap().start();

            // Skip common non-repository functions
            if rule_type_str == "def"
                || rule_type_str == "load"
                || rule_type_str == "return"
                || rule_type_str == "print"
                || rule_type_str == "if"
                || rule_type_str == "for"
            {
                continue;
            }

            // Extract the full rule body
            if let Some(rule_body) = self.extract_parenthesized_content(&content, match_start) {
                // Parse attributes from the body
                if let Ok(mut attributes) = self.parse_rule_attributes(&rule_body) {
                    // Extract the name from attributes (remove to avoid clone)
                    if let Some(Value::String(name)) = attributes.remove("name") {
                        rules.push(RepositoryRule {
                            rule_type: rule_type_str.to_string(),
                            name,
                            attributes,
                        });
                    }
                }
            }
        }

        Ok(rules)
    }

    /// Remove Python-style comments from content.
    fn remove_comments(&self, content: &str) -> String {
        let mut result = String::new();
        let mut in_string = false;
        let mut string_char = '\0';
        let mut escape = false;

        for ch in content.chars() {
            if escape {
                result.push(ch);
                escape = false;
                continue;
            }

            if ch == '\\' && in_string {
                escape = true;
                result.push(ch);
                continue;
            }

            if (ch == '"' || ch == '\'') && !in_string {
                in_string = true;
                string_char = ch;
                result.push(ch);
            } else if ch == string_char && in_string {
                in_string = false;
                result.push(ch);
            } else if ch == '#' && !in_string {
                // Skip until end of line
                for next_ch in content.chars().skip(result.len()) {
                    if next_ch == '\n' {
                        result.push('\n');
                        break;
                    }
                }
            } else {
                result.push(ch);
            }
        }

        result
    }

    /// Extract content within parentheses starting from a given position.
    fn extract_parenthesized_content(&self, content: &str, start_pos: usize) -> Option<String> {
        let remaining = &content[start_pos..];
        let mut depth = 0;
        let mut body = String::new();
        let mut started = false;

        for ch in remaining.chars() {
            if ch == '(' {
                depth += 1;
                started = true;
            } else if ch == ')' {
                depth -= 1;
                if depth == 0 && started {
                    body.push(ch);
                    break;
                }
            }
            if started {
                body.push(ch);
            }
        }

        if depth == 0 && !body.is_empty() {
            Some(body)
        } else {
            None
        }
    }

    /// Parse rule attributes from a rule body.
    ///
    /// This uses the same parsing logic as the WORKSPACE parser.
    fn parse_rule_attributes(&self, body: &str) -> Result<HashMap<String, Value>> {
        let mut attributes = HashMap::new();

        // Normalize whitespace
        let normalized = body.replace(['\n', '\r'], " ");
        let mut pos = 0;
        let chars: Vec<char> = normalized.chars().collect();

        while pos < chars.len() {
            // Skip whitespace
            while pos < chars.len() && chars[pos].is_whitespace() {
                pos += 1;
            }

            if pos >= chars.len() {
                break;
            }

            // Match attribute name
            let remaining: String = chars[pos..].iter().collect();

            if let Some(cap) = ATTR_PATTERN_RE.captures(&remaining) {
                let key = cap.get(1).unwrap().as_str().to_string();
                pos += cap.get(0).unwrap().end();

                // Extract the value
                let (value, new_pos) = self.extract_attribute_value(&chars, pos)?;
                pos = new_pos;

                if let Ok(parsed_value) = self.parse_value(&value) {
                    attributes.insert(key, parsed_value);
                }

                // Skip trailing comma and whitespace
                while pos < chars.len() && (chars[pos] == ',' || chars[pos].is_whitespace()) {
                    pos += 1;
                }
            } else {
                pos += 1;
            }
        }

        Ok(attributes)
    }

    /// Extract an attribute value from the character stream.
    fn extract_attribute_value(&self, chars: &[char], start_pos: usize) -> Result<(String, usize)> {
        let mut pos = start_pos;
        let mut value = String::new();

        // Skip leading whitespace
        while pos < chars.len() && chars[pos].is_whitespace() {
            pos += 1;
        }

        if pos >= chars.len() {
            return Ok((value, pos));
        }

        if chars[pos] == '"' {
            // Quoted string
            value.push(chars[pos]);
            pos += 1;

            while pos < chars.len() {
                if chars[pos] == '\\' && pos + 1 < chars.len() {
                    value.push(chars[pos]);
                    pos += 1;
                    value.push(chars[pos]);
                    pos += 1;
                } else if chars[pos] == '"' {
                    value.push(chars[pos]);
                    pos += 1;
                    break;
                } else {
                    value.push(chars[pos]);
                    pos += 1;
                }
            }
        } else if chars[pos] == '[' {
            // List
            let mut depth = 0;
            while pos < chars.len() {
                if chars[pos] == '[' {
                    depth += 1;
                } else if chars[pos] == ']' {
                    depth -= 1;
                }
                value.push(chars[pos]);
                pos += 1;
                if depth == 0 {
                    break;
                }
            }
        } else {
            // Unquoted value (boolean or identifier)
            while pos < chars.len()
                && !chars[pos].is_whitespace()
                && chars[pos] != ','
                && chars[pos] != ')'
            {
                value.push(chars[pos]);
                pos += 1;
            }
        }

        Ok((value, pos))
    }

    /// Parse a value (string, list, boolean).
    fn parse_value(&self, value_str: &str) -> Result<Value> {
        let trimmed = value_str.trim();

        // Boolean
        if trimmed == "True" || trimmed == "true" {
            return Ok(Value::Bool(true));
        } else if trimmed == "False" || trimmed == "false" {
            return Ok(Value::Bool(false));
        }

        // List
        if trimmed.starts_with('[') {
            return self.parse_list(trimmed);
        }

        // String (quoted)
        if trimmed.starts_with('"') {
            return self.parse_string(trimmed);
        }

        // Unquoted string
        Ok(Value::String(trimmed.to_string()))
    }

    /// Parse a list value.
    fn parse_list(&self, list_str: &str) -> Result<Value> {
        let trimmed = list_str.trim();

        if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
            anyhow::bail!("Invalid list format: {}", list_str);
        }

        let content = &trimmed[1..trimmed.len() - 1];
        let mut items = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        let mut escape = false;

        for ch in content.chars() {
            if escape {
                current.push(ch);
                escape = false;
                continue;
            }

            match ch {
                '\\' => escape = true,
                '"' => {
                    in_quotes = !in_quotes;
                    current.push(ch);
                }
                ',' if !in_quotes => {
                    let item = current.trim();
                    if !item.is_empty() {
                        if let Ok(value) = self.parse_value(item) {
                            items.push(value);
                        }
                    }
                    current.clear();
                }
                _ => current.push(ch),
            }
        }

        // Last item
        let item = current.trim();
        if !item.is_empty() {
            if let Ok(value) = self.parse_value(item) {
                items.push(value);
            }
        }

        Ok(Value::List(items))
    }

    /// Parse a quoted string value.
    fn parse_string(&self, str_value: &str) -> Result<Value> {
        let trimmed = str_value.trim();

        if (trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        {
            let content = &trimmed[1..trimmed.len() - 1];
            let unescaped = content
                .replace(r#"\""#, "\"")
                .replace(r"\'", "'")
                .replace(r"\\", "\\");
            return Ok(Value::String(unescaped));
        }

        Ok(Value::String(trimmed.to_string()))
    }

    /// Resolve a local load path to a filesystem path.
    fn resolve_local_path(&self, load_path: &str) -> Result<PathBuf> {
        // Handle paths like "//bazel:repositories.bzl" or ":file.bzl"
        let path = if let Some(stripped) = load_path.strip_prefix("//") {
            // Absolute path from workspace root
            let relative = stripped.replace(':', "/");
            self.workspace_root.join(relative)
        } else if let Some(relative) = load_path.strip_prefix(':') {
            // Relative to current package (assume workspace root for now)
            self.workspace_root.join(relative)
        } else {
            // Already a relative or absolute path
            PathBuf::from(load_path)
        };

        Ok(path)
    }
}

impl Default for StarlarkLoader {
    fn default() -> Self {
        Self::new(".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_extract_simple_repository_rule() {
        let content = r#"
def load_dependencies():
    http_archive(
        name = "zlib",
        urls = ["https://github.com/madler/zlib/archive/v1.2.13.tar.gz"],
        sha256 = "b3a24de97a8fdbc835b9833169501030b8977031bcb54b3b3ac13740f846ab30",
    )
"#;

        let loader = StarlarkLoader::new(".");
        let rules = loader.extract_repository_rules(content).unwrap();

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_type, "http_archive");
        assert_eq!(rules[0].name, "zlib");
        assert!(rules[0].attributes.contains_key("urls"));
        assert!(rules[0].attributes.contains_key("sha256"));
    }

    #[test]
    fn test_extract_external_http_archive() {
        let content = r#"
def envoy_dependencies():
    external_http_archive(
        name = "boringssl",
    )

    external_http_archive(
        name = "com_github_grpc_grpc",
        patches = ["@envoy//bazel:grpc.patch"],
        patch_args = ["-p1"],
    )
"#;

        let loader = StarlarkLoader::new(".");
        let rules = loader.extract_repository_rules(content).unwrap();

        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].rule_type, "external_http_archive");
        assert_eq!(rules[0].name, "boringssl");

        assert_eq!(rules[1].rule_type, "external_http_archive");
        assert_eq!(rules[1].name, "com_github_grpc_grpc");
        assert!(rules[1].attributes.contains_key("patches"));
    }

    #[test]
    fn test_load_file() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let bzl_file = temp_dir.path().join("test.bzl");

        std::fs::write(
            &bzl_file,
            r#"
# Test .bzl file
http_archive(
    name = "test_dep",
    urls = ["https://example.com/dep.tar.gz"],
)
"#,
        )?;

        let mut loader = StarlarkLoader::new(temp_dir.path());
        loader.load_file("test.bzl")?;

        let rules = loader.get_repository_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "test_dep");

        Ok(())
    }

    #[test]
    fn test_get_symbol() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let bzl_file = temp_dir.path().join("deps.bzl");

        std::fs::write(
            &bzl_file,
            r#"
git_repository(
    name = "my_repo",
    remote = "https://github.com/example/repo.git",
    commit = "abc123",
)
"#,
        )?;

        let mut loader = StarlarkLoader::new(temp_dir.path());
        loader.load_file("deps.bzl")?;

        let repo = loader.get_symbol("my_repo");
        assert!(repo.is_some());
        assert_eq!(repo.unwrap().rule_type, "git_repository");

        let missing = loader.get_symbol("nonexistent");
        assert!(missing.is_none());

        Ok(())
    }

    #[test]
    fn test_comment_removal() {
        let loader = StarlarkLoader::new(".");
        let content = r#"
# This is a comment
http_archive(name = "test")  # inline comment
load("file.bzl", "symbol")  # another comment
"#;

        let cleaned = loader.remove_comments(content);
        assert!(!cleaned.contains("# This is a comment"));
        assert!(!cleaned.contains("# inline comment"));
        assert!(cleaned.contains("http_archive"));
    }

    #[test]
    fn test_multiple_rules_in_file() {
        let content = r#"
def setup_dependencies():
    http_archive(
        name = "dep1",
        urls = ["https://example.com/dep1.tar.gz"],
    )

    git_repository(
        name = "dep2",
        remote = "https://github.com/example/dep2.git",
    )

    external_http_archive(
        name = "dep3",
    )
"#;

        let loader = StarlarkLoader::new(".");
        let rules = loader.extract_repository_rules(content).unwrap();

        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].name, "dep1");
        assert_eq!(rules[1].name, "dep2");
        assert_eq!(rules[2].name, "dep3");
    }

    #[test]
    fn test_resolve_local_path() -> Result<()> {
        let loader = StarlarkLoader::new("/workspace");

        let path1 = loader.resolve_local_path("//bazel:repositories.bzl")?;
        assert_eq!(path1, PathBuf::from("/workspace/bazel/repositories.bzl"));

        let path2 = loader.resolve_local_path(":deps.bzl")?;
        assert_eq!(path2, PathBuf::from("/workspace/deps.bzl"));

        Ok(())
    }
}
