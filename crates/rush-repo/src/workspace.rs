// Rush Repo - WORKSPACE parser
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! WORKSPACE file parser.
//!
//! Parses WORKSPACE files to extract repository rules and dependencies.
//! This module handles the loading and interpretation of WORKSPACE files,
//! which define external dependencies for a Rush project.
//!
//! # Example
//!
//! ```rust
//! use rush_repo::workspace::parse_workspace_content;
//!
//! # fn main() -> anyhow::Result<()> {
//! let content = r#"
//! workspace(name = "my_project")
//!
//! load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
//!
//! http_archive(
//!     name = "zlib",
//!     urls = ["https://github.com/madler/zlib/archive/v1.2.13.tar.gz"],
//!     sha256 = "b3a24de97a8fdbc835b9833169501030b8977031bcb54b3b3ac13740f846ab30",
//! )
//! "#;
//!
//! let workspace = parse_workspace_content(content)?;
//! assert_eq!(workspace.name, Some("my_project".to_string()));
//! assert_eq!(workspace.loads.len(), 1);
//! assert_eq!(workspace.repos.len(), 1);
//! assert_eq!(workspace.repos[0].name, "zlib");
//! # Ok(())
//! # }
//! ```

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;

// Lazy-compiled regexes for performance (compile once, use many times)
static WORKSPACE_NAME_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?m)workspace\s*\(\s*name\s*=\s*"([^"]+)""#).unwrap());

static LOAD_PATTERN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?m)load\s*\(\s*"([^"]+)""#).unwrap());

static SYMBOL_PATTERN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#""([^"]+)""#).unwrap());

static RULE_PATTERN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?m)^([a-z_]+)\s*\("#).unwrap());

static ATTR_PATTERN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"^([a-z_][a-z0-9_]*)\s*=\s*"#).unwrap());

/// Represents a parsed WORKSPACE file.
#[derive(Debug, Clone)]
pub struct WorkspaceFile {
    /// The workspace name, if specified in workspace(name = "...")
    pub name: Option<String>,

    /// Load statements from the WORKSPACE file
    pub loads: Vec<LoadStatement>,

    /// Repository rules defined in the WORKSPACE file
    pub repos: Vec<RepositoryRule>,
}

/// Represents a load statement from a WORKSPACE file.
///
/// Example: `load("@rules_rust//rust:defs.bzl", "rust_library", "rust_binary")`
#[derive(Debug, Clone, PartialEq)]
pub struct LoadStatement {
    /// Path to the .bzl file being loaded
    pub path: String,

    /// Symbols being loaded from the file
    pub symbols: Vec<String>,
}

/// Represents a value in a repository rule attribute.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A string value
    String(String),

    /// A list of values
    List(Vec<Value>),

    /// A boolean value
    Bool(bool),

    /// A dictionary/map value
    Dict(HashMap<String, Value>),
}

/// Represents a repository rule from a WORKSPACE file.
///
/// Example: `http_archive(name = "zlib", urls = [...], sha256 = "...")`
#[derive(Debug, Clone)]
pub struct RepositoryRule {
    /// The type of repository rule (e.g., "http_archive", "git_repository")
    pub rule_type: String,

    /// The name of the repository
    pub name: String,

    /// All attributes of the rule
    pub attributes: HashMap<String, Value>,
}

/// Parse a WORKSPACE file at the given path.
///
/// # Arguments
///
/// * `path` - Path to the WORKSPACE or WORKSPACE.bazel file
///
/// # Returns
///
/// Returns a parsed `WorkspaceFile` structure containing the workspace name,
/// load statements, and repository rules.
///
/// # Errors
///
/// Returns an error if the file cannot be read or parsed.
pub fn parse_workspace(path: &Path) -> Result<WorkspaceFile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read WORKSPACE file: {}", path.display()))?;

    parse_workspace_content(&content)
}

/// Parse WORKSPACE file content from a string.
///
/// This function parses the content of a WORKSPACE file and extracts:
/// - The workspace name declaration
/// - Load statements
/// - Repository rule calls
///
/// # Arguments
///
/// * `content` - The WORKSPACE file content as a string
///
/// # Returns
///
/// Returns a parsed `WorkspaceFile` structure.
///
/// # Errors
///
/// Returns an error if the content cannot be parsed.
pub fn parse_workspace_content(content: &str) -> Result<WorkspaceFile> {
    let mut workspace_file = WorkspaceFile {
        name: None,
        loads: Vec::new(),
        repos: Vec::new(),
    };

    // Remove comments
    let content = remove_comments(content);

    // Parse workspace name
    workspace_file.name = parse_workspace_name(&content)?;

    // Parse load statements
    workspace_file.loads = parse_load_statements(&content)?;

    // Parse repository rules
    workspace_file.repos = parse_repository_rules(&content)?;

    Ok(workspace_file)
}

/// Remove Python-style comments from the content.
fn remove_comments(content: &str) -> String {
    let mut result = String::new();
    for line in content.lines() {
        if let Some(pos) = line.find('#') {
            result.push_str(&line[..pos]);
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }
    result
}

/// Parse the workspace name declaration.
///
/// Example: `workspace(name = "my_workspace")`
fn parse_workspace_name(content: &str) -> Result<Option<String>> {
    if let Some(cap) = WORKSPACE_NAME_RE.captures(content) {
        Ok(Some(cap.get(1).unwrap().as_str().to_string()))
    } else {
        Ok(None)
    }
}

/// Parse load statements from WORKSPACE content.
///
/// Example: `load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")`
fn parse_load_statements(content: &str) -> Result<Vec<LoadStatement>> {
    let mut loads = Vec::new();

    for cap in LOAD_PATTERN_RE.captures_iter(content) {
        let path = cap.get(1).unwrap().as_str().to_string();
        let match_start = cap.get(0).unwrap().start();

        // Extract the full load statement to get symbols
        if let Some(load_body) = extract_parenthesized_content(content, match_start) {
            let symbols = parse_load_symbols(&load_body)?;
            loads.push(LoadStatement { path, symbols });
        }
    }

    Ok(loads)
}

/// Parse symbols from a load statement body.
fn parse_load_symbols(body: &str) -> Result<Vec<String>> {
    let mut symbols = Vec::new();

    // Find all quoted strings after the first one (which is the path)
    let mut first = true;

    for cap in SYMBOL_PATTERN_RE.captures_iter(body) {
        if first {
            // Skip the first match (it's the path)
            first = false;
            continue;
        }
        symbols.push(cap.get(1).unwrap().as_str().to_string());
    }

    Ok(symbols)
}

/// Parse repository rules from WORKSPACE content.
///
/// Examples:
/// - `http_archive(name = "zlib", urls = [...], sha256 = "...")`
/// - `git_repository(name = "repo", remote = "...", commit = "...")`
fn parse_repository_rules(content: &str) -> Result<Vec<RepositoryRule>> {
    let mut repos = Vec::new();

    // Match repository rule calls - common patterns include:
    // http_archive, git_repository, new_git_repository, http_file, etc.
    // This pattern matches the rule name at the start of a line, followed by an opening paren
    for cap in RULE_PATTERN_RE.captures_iter(content) {
        let rule_type_str = cap.get(1).unwrap().as_str();
        let match_start = cap.get(0).unwrap().start();

        // Skip 'workspace' and 'load' - they're not repository rules
        if rule_type_str == "workspace" || rule_type_str == "load" {
            continue;
        }

        // Extract the full rule body
        if let Some(rule_body) = extract_parenthesized_content(content, match_start) {
            let mut attributes = parse_rule_attributes(&rule_body)?;

            // Extract the name from attributes (remove to avoid clone)
            if let Some(Value::String(name)) = attributes.remove("name") {
                repos.push(RepositoryRule {
                    rule_type: rule_type_str.to_string(),
                    name,
                    attributes,
                });
            }
        }
    }

    Ok(repos)
}

/// Extract content within parentheses starting from a given position.
///
/// This function handles nested parentheses correctly.
fn extract_parenthesized_content(content: &str, start_pos: usize) -> Option<String> {
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
/// Handles:
/// - Simple strings: `name = "value"`
/// - Lists: `urls = ["http://...", "https://..."]`
/// - Booleans: `strip_prefix = True`
/// - Multi-line strings: `patch_cmds = ["cmd1", "cmd2"]`
fn parse_rule_attributes(body: &str) -> Result<HashMap<String, Value>> {
    let mut attributes = HashMap::new();

    // Normalize whitespace while preserving structure
    let normalized = body.replace(['\n', '\r'], " ");

    // Match attribute assignments more carefully
    // We need to find key = value where value can be:
    // - A quoted string (possibly containing commas)
    // - A list [...] (possibly spanning multiple lines)
    // - A boolean (True/False)

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

        // Try to match an attribute name (including numbers)
        let remaining: String = chars[pos..].iter().collect();

        if let Some(cap) = ATTR_PATTERN_RE.captures(&remaining) {
            let key = cap.get(1).unwrap().as_str().to_string();
            pos += cap.get(0).unwrap().end();

            // Now extract the value
            let (value, new_pos) = extract_attribute_value(&chars, pos)?;
            pos = new_pos;

            if let Ok(parsed_value) = parse_value(&value) {
                attributes.insert(key, parsed_value);
            }

            // Skip trailing comma and whitespace
            while pos < chars.len() && (chars[pos] == ',' || chars[pos].is_whitespace()) {
                pos += 1;
            }
        } else {
            // Skip this character if we can't match an attribute
            pos += 1;
        }
    }

    Ok(attributes)
}

/// Extract an attribute value from the character stream.
/// Returns the value string and the new position.
fn extract_attribute_value(chars: &[char], start_pos: usize) -> Result<(String, usize)> {
    let mut pos = start_pos;
    let mut value = String::new();

    // Skip leading whitespace
    while pos < chars.len() && chars[pos].is_whitespace() {
        pos += 1;
    }

    if pos >= chars.len() {
        return Ok((value, pos));
    }

    // Check what kind of value we have
    if chars[pos] == '"' {
        // Quoted string
        value.push(chars[pos]);
        pos += 1;

        while pos < chars.len() {
            if chars[pos] == '\\' && pos + 1 < chars.len() {
                // Escape sequence
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
fn parse_value(value_str: &str) -> Result<Value> {
    let trimmed = value_str.trim();

    // Boolean
    if trimmed == "True" || trimmed == "true" {
        return Ok(Value::Bool(true));
    } else if trimmed == "False" || trimmed == "false" {
        return Ok(Value::Bool(false));
    }

    // List
    if trimmed.starts_with('[') {
        return parse_list(trimmed);
    }

    // String (quoted)
    if trimmed.starts_with('"') {
        return parse_string(trimmed);
    }

    // Unquoted string (fallback)
    Ok(Value::String(trimmed.to_string()))
}

/// Parse a list value.
///
/// Example: `["item1", "item2", "item3"]`
fn parse_list(list_str: &str) -> Result<Value> {
    let trimmed = list_str.trim();

    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        anyhow::bail!("Invalid list format: {}", list_str);
    }

    let content = &trimmed[1..trimmed.len() - 1];
    let mut items = Vec::new();

    // Simple split on commas, handling quoted strings
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
                    if let Ok(value) = parse_value(item) {
                        items.push(value);
                    }
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    // Don't forget the last item
    let item = current.trim();
    if !item.is_empty() {
        if let Ok(value) = parse_value(item) {
            items.push(value);
        }
    }

    Ok(Value::List(items))
}

/// Parse a quoted string value.
///
/// Handles both single and double quotes, and basic escape sequences.
fn parse_string(str_value: &str) -> Result<Value> {
    let trimmed = str_value.trim();

    // Multi-line string (triple quotes)
    if trimmed.starts_with(r#"""""#) || trimmed.starts_with("'''") {
        let quote = if trimmed.starts_with(r#"""""#) {
            r#"""""#
        } else {
            "'''"
        };

        if trimmed.ends_with(quote) && trimmed.len() >= 2 * quote.len() {
            let content = &trimmed[quote.len()..trimmed.len() - quote.len()];
            return Ok(Value::String(content.to_string()));
        }
    }

    // Single-line string
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        let content = &trimmed[1..trimmed.len() - 1];
        // Handle basic escape sequences
        let unescaped = content
            .replace(r#"\""#, "\"")
            .replace(r"\'", "'")
            .replace(r"\\", "\\");
        return Ok(Value::String(unescaped));
    }

    Ok(Value::String(trimmed.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_workspace_name() {
        let content = r#"
workspace(name = "my_project")

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
"#;

        let result = parse_workspace_name(content).unwrap();
        assert_eq!(result, Some("my_project".to_string()));
    }

    #[test]
    fn test_parse_load_statements() {
        let content = r#"
load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
load("@rules_rust//rust:defs.bzl", "rust_library", "rust_binary")
"#;

        let loads = parse_load_statements(content).unwrap();
        assert_eq!(loads.len(), 2);

        assert_eq!(
            loads[0].path,
            "@bazel_tools//tools/build_defs/repo:http.bzl"
        );
        assert_eq!(loads[0].symbols, vec!["http_archive"]);

        assert_eq!(loads[1].path, "@rules_rust//rust:defs.bzl");
        assert_eq!(loads[1].symbols, vec!["rust_library", "rust_binary"]);
    }

    #[test]
    fn test_parse_simple_http_archive() {
        let content = r#"
http_archive(
    name = "zlib",
    urls = ["https://github.com/madler/zlib/archive/v1.2.13.tar.gz"],
    sha256 = "b3a24de97a8fdbc835b9833169501030b8977031bcb54b3b3ac13740f846ab30",
    strip_prefix = "zlib-1.2.13",
)
"#;

        let repos = parse_repository_rules(content).unwrap();
        assert_eq!(repos.len(), 1);

        let repo = &repos[0];
        assert_eq!(repo.rule_type, "http_archive");
        assert_eq!(repo.name, "zlib");

        // Check attributes
        assert!(matches!(
            repo.attributes.get("sha256"),
            Some(Value::String(_))
        ));

        if let Some(Value::List(urls)) = repo.attributes.get("urls") {
            assert_eq!(urls.len(), 1);
            if let Value::String(url) = &urls[0] {
                assert!(url.contains("zlib"));
            }
        } else {
            panic!("Expected urls to be a list");
        }
    }

    #[test]
    fn test_parse_multiple_urls() {
        let content = r#"
http_archive(
    name = "protobuf",
    urls = [
        "https://github.com/protocolbuffers/protobuf/archive/v21.7.tar.gz",
        "https://mirror.example.com/protobuf-21.7.tar.gz",
    ],
    sha256 = "abcdef1234567890",
)
"#;

        let repos = parse_repository_rules(content).unwrap();
        assert_eq!(repos.len(), 1);

        let repo = &repos[0];
        if let Some(Value::List(urls)) = repo.attributes.get("urls") {
            assert_eq!(urls.len(), 2);
        } else {
            panic!("Expected urls to be a list");
        }
    }

    #[test]
    fn test_parse_complete_workspace() {
        let content = r#"
workspace(name = "rush_example")

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")

# External dependency: zlib
http_archive(
    name = "zlib",
    urls = ["https://github.com/madler/zlib/archive/v1.2.13.tar.gz"],
    sha256 = "b3a24de97a8fdbc835b9833169501030b8977031bcb54b3b3ac13740f846ab30",
    strip_prefix = "zlib-1.2.13",
)

# Another dependency
http_archive(
    name = "googletest",
    urls = ["https://github.com/google/googletest/archive/release-1.12.1.tar.gz"],
    sha256 = "81964fe578e9bd7c94dfdb09c8e4d6e6759e19967e397dbea48d1c10e45d0df2",
)
"#;

        let workspace = parse_workspace_content(content).unwrap();

        assert_eq!(workspace.name, Some("rush_example".to_string()));
        assert_eq!(workspace.loads.len(), 1);
        assert_eq!(workspace.repos.len(), 2);

        assert_eq!(workspace.repos[0].name, "zlib");
        assert_eq!(workspace.repos[1].name, "googletest");
    }

    #[test]
    fn test_remove_comments() {
        let content = r#"
# This is a comment
workspace(name = "test")  # inline comment
load("file.bzl", "symbol")  # another comment
"#;

        let cleaned = remove_comments(content);
        assert!(!cleaned.contains("# This is a comment"));
        assert!(!cleaned.contains("# inline comment"));
        assert!(cleaned.contains("workspace(name = \"test\")"));
    }

    #[test]
    fn test_parse_patches_list() {
        let content = r#"
http_archive(
    name = "mylib",
    urls = ["https://example.com/lib.tar.gz"],
    patches = [
        "//patches:fix1.patch",
        "//patches:fix2.patch",
    ],
)
"#;

        let repos = parse_repository_rules(content).unwrap();
        assert_eq!(repos.len(), 1);

        if let Some(Value::List(patches)) = repos[0].attributes.get("patches") {
            assert_eq!(patches.len(), 2);
        } else {
            panic!("Expected patches to be a list");
        }
    }
}
