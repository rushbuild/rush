// Rush Parse - WORKSPACE file parser
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! WORKSPACE file parser for external repository declarations.
//!
//! Parses WORKSPACE files to extract external dependency metadata like
//! http_archive, git_repository, and other repository rules.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Represents a parsed WORKSPACE file.
#[derive(Debug, Clone)]
pub struct WorkspaceFile {
    /// The path to the WORKSPACE file.
    pub path: PathBuf,

    /// The workspace name (from workspace(name = "..."))
    pub name: Option<String>,

    /// External repositories declared via http_archive
    pub http_archives: Vec<HttpArchive>,

    /// External repositories declared via git_repository
    pub git_repositories: Vec<GitRepository>,

    /// External repositories declared via local_repository
    pub local_repositories: Vec<LocalRepository>,

    /// Load statements for importing macros
    pub load_statements: Vec<LoadStatement>,
}

/// Represents an http_archive declaration.
#[derive(Debug, Clone)]
pub struct HttpArchive {
    /// Repository name
    pub name: String,

    /// Download URLs (list for mirrors)
    pub urls: Vec<String>,

    /// SHA256 checksum for verification
    pub sha256: Option<String>,

    /// Strip prefix when extracting archive
    pub strip_prefix: Option<String>,

    /// Build file to use instead of repo's BUILD file
    pub build_file: Option<String>,

    /// Build file content to inject
    pub build_file_content: Option<String>,

    /// Repository mapping (repo_name -> mapped_name)
    pub repo_mapping: std::collections::HashMap<String, String>,
}

/// Represents a git_repository declaration.
#[derive(Debug, Clone)]
pub struct GitRepository {
    /// Repository name
    pub name: String,

    /// Git remote URL
    pub remote: String,

    /// Git commit SHA, tag, or branch
    pub commit: Option<String>,

    /// Git tag
    pub tag: Option<String>,

    /// Git branch
    pub branch: Option<String>,

    /// Repository mapping (repo_name -> mapped_name)
    pub repo_mapping: std::collections::HashMap<String, String>,
}

/// Represents a local_repository declaration.
#[derive(Debug, Clone)]
pub struct LocalRepository {
    /// Repository name
    pub name: String,

    /// Path to local directory
    pub path: String,

    /// Repository mapping (repo_name -> mapped_name)
    pub repo_mapping: std::collections::HashMap<String, String>,
}

/// Represents a load() statement.
#[derive(Debug, Clone)]
pub struct LoadStatement {
    /// Path to .bzl file (e.g., "@bazel_tools//tools/build_defs/repo:http.bzl")
    pub path: String,

    /// Symbols to import (e.g., ["http_archive", "git_repository"])
    pub symbols: Vec<String>,
}

/// Parse a WORKSPACE file.
pub fn parse_workspace_file(path: &Path) -> Result<WorkspaceFile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read WORKSPACE file: {}", path.display()))?;

    parse_workspace_content(path, &content)
}

/// Parse WORKSPACE file content.
fn parse_workspace_content(path: &Path, content: &str) -> Result<WorkspaceFile> {
    let mut workspace = WorkspaceFile {
        path: path.to_path_buf(),
        name: None,
        http_archives: Vec::new(),
        git_repositories: Vec::new(),
        local_repositories: Vec::new(),
        load_statements: Vec::new(),
    };

    // Extract workspace name
    workspace.name = extract_workspace_name(content);

    // Extract load statements
    workspace.load_statements = extract_load_statements(content);

    // Extract http_archive declarations
    workspace.http_archives = extract_http_archives(content)?;

    // Extract git_repository declarations
    workspace.git_repositories = extract_git_repositories(content)?;

    // Extract local_repository declarations
    workspace.local_repositories = extract_local_repositories(content)?;

    Ok(workspace)
}

/// Extract workspace name from workspace(name = "...")
fn extract_workspace_name(content: &str) -> Option<String> {
    // Simple regex-based extraction for now
    let re = regex::Regex::new(r#"workspace\s*\(\s*name\s*=\s*"([^"]+)""#).ok()?;
    re.captures(content)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str().to_string())
}

/// Extract load() statements
fn extract_load_statements(content: &str) -> Vec<LoadStatement> {
    let mut statements = Vec::new();

    // Match load("path", "symbol1", "symbol2", ...)
    let re = regex::Regex::new(r#"load\s*\(\s*"([^"]+)"\s*,\s*([^)]+)\)"#).unwrap();

    // Extract symbols (quoted strings) - compile regex once before loop
    let symbol_re = regex::Regex::new(r#""([^"]+)""#).unwrap();

    for cap in re.captures_iter(content) {
        let path = cap.get(1).map(|m| m.as_str().to_string()).unwrap();
        let symbols_str = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let symbols: Vec<String> = symbol_re
            .captures_iter(symbols_str)
            .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
            .collect();

        statements.push(LoadStatement { path, symbols });
    }

    statements
}

/// Extract http_archive declarations
fn extract_http_archives(content: &str) -> Result<Vec<HttpArchive>> {
    let mut archives = Vec::new();

    // Find all http_archive( ... ) blocks
    let re = regex::Regex::new(r"http_archive\s*\(([^)]*(?:\([^)]*\)[^)]*)*)\)").unwrap();

    for cap in re.captures_iter(content) {
        let block = cap.get(1).map(|m| m.as_str()).unwrap_or("");

        let archive = HttpArchive {
            name: extract_attribute(block, "name").unwrap_or_default(),
            urls: extract_list_attribute(block, "urls").unwrap_or_else(|| {
                // Try singular "url" if "urls" not found
                extract_attribute(block, "url")
                    .map(|u| vec![u])
                    .unwrap_or_default()
            }),
            sha256: extract_attribute(block, "sha256"),
            strip_prefix: extract_attribute(block, "strip_prefix"),
            build_file: extract_attribute(block, "build_file"),
            build_file_content: extract_attribute(block, "build_file_content"),
            repo_mapping: extract_dict_attribute(block, "repo_mapping").unwrap_or_default(),
        };

        archives.push(archive);
    }

    Ok(archives)
}

/// Extract git_repository declarations
fn extract_git_repositories(content: &str) -> Result<Vec<GitRepository>> {
    let mut repos = Vec::new();

    // Find all git_repository( ... ) blocks
    let re = regex::Regex::new(r"git_repository\s*\(([^)]*)\)").unwrap();

    for cap in re.captures_iter(content) {
        let block = cap.get(1).map(|m| m.as_str()).unwrap_or("");

        let repo = GitRepository {
            name: extract_attribute(block, "name").unwrap_or_default(),
            remote: extract_attribute(block, "remote").unwrap_or_default(),
            commit: extract_attribute(block, "commit"),
            tag: extract_attribute(block, "tag"),
            branch: extract_attribute(block, "branch"),
            repo_mapping: extract_dict_attribute(block, "repo_mapping").unwrap_or_default(),
        };

        repos.push(repo);
    }

    Ok(repos)
}

/// Extract local_repository declarations
fn extract_local_repositories(content: &str) -> Result<Vec<LocalRepository>> {
    let mut repos = Vec::new();

    // Find all local_repository( ... ) blocks
    let re = regex::Regex::new(r"local_repository\s*\(([^)]*)\)").unwrap();

    for cap in re.captures_iter(content) {
        let block = cap.get(1).map(|m| m.as_str()).unwrap_or("");

        let repo = LocalRepository {
            name: extract_attribute(block, "name").unwrap_or_default(),
            path: extract_attribute(block, "path").unwrap_or_default(),
            repo_mapping: extract_dict_attribute(block, "repo_mapping").unwrap_or_default(),
        };

        repos.push(repo);
    }

    Ok(repos)
}

/// Extract a string attribute from a block (e.g., name = "foo")
fn extract_attribute(block: &str, attr_name: &str) -> Option<String> {
    let pattern = format!(r#"{}\s*=\s*"([^"]*)""#, regex::escape(attr_name));
    let re = regex::Regex::new(&pattern).ok()?;

    re.captures(block)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str().to_string())
}

/// Extract a list attribute from a block (e.g., urls = ["a", "b"])
fn extract_list_attribute(block: &str, attr_name: &str) -> Option<Vec<String>> {
    let pattern = format!(
        r#"{}\s*=\s*\[((?:[^]]*(?:\[[^\]]*\][^]]*)*)*)\]"#,
        regex::escape(attr_name)
    );
    let re = regex::Regex::new(&pattern).ok()?;

    let list_content = re
        .captures(block)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str())?;

    // Extract quoted strings from list
    let item_re = regex::Regex::new(r#""([^"]*)""#).ok()?;
    let items: Vec<String> = item_re
        .captures_iter(list_content)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect();

    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

/// Extract a dictionary attribute from a block (e.g., repo_mapping = {"@foo": "@bar"})
fn extract_dict_attribute(
    block: &str,
    attr_name: &str,
) -> Option<std::collections::HashMap<String, String>> {
    let pattern = format!(
        r#"{}\s*=\s*\{{((?:[^}}]*(?:\{{[^}}]*\}}[^}}]*)*)*)\}}"#,
        regex::escape(attr_name)
    );
    let re = regex::Regex::new(&pattern).ok()?;

    let dict_content = re
        .captures(block)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str())?;

    // Extract key-value pairs: "key": "value"
    let item_re = regex::Regex::new(r#""([^"]+)"\s*:\s*"([^"]+)""#).ok()?;
    let mut map = std::collections::HashMap::new();

    for cap in item_re.captures_iter(dict_content) {
        if let (Some(k), Some(v)) = (cap.get(1), cap.get(2)) {
            map.insert(k.as_str().to_string(), v.as_str().to_string());
        }
    }

    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_workspace() {
        let content = r#"
workspace(name = "hello_cpp")
"#;
        let ws = parse_workspace_content(Path::new("WORKSPACE"), content).unwrap();
        assert_eq!(ws.name, Some("hello_cpp".to_string()));
    }

    #[test]
    fn test_parse_http_archive() {
        let content = r#"
http_archive(
    name = "bazel_skylib",
    sha256 = "abc123",
    urls = [
        "https://example.com/skylib.tar.gz",
        "https://mirror.example.com/skylib.tar.gz",
    ],
    strip_prefix = "bazel-skylib-1.0.0",
)
"#;
        let ws = parse_workspace_content(Path::new("WORKSPACE"), content).unwrap();
        assert_eq!(ws.http_archives.len(), 1);

        let archive = &ws.http_archives[0];
        assert_eq!(archive.name, "bazel_skylib");
        assert_eq!(archive.sha256, Some("abc123".to_string()));
        assert_eq!(archive.urls.len(), 2);
        assert_eq!(archive.strip_prefix, Some("bazel-skylib-1.0.0".to_string()));
    }

    #[test]
    fn test_parse_git_repository() {
        let content = r#"
git_repository(
    name = "my_repo",
    remote = "https://github.com/example/repo.git",
    commit = "abc123def456",
)
"#;
        let ws = parse_workspace_content(Path::new("WORKSPACE"), content).unwrap();
        assert_eq!(ws.git_repositories.len(), 1);

        let repo = &ws.git_repositories[0];
        assert_eq!(repo.name, "my_repo");
        assert_eq!(repo.remote, "https://github.com/example/repo.git");
        assert_eq!(repo.commit, Some("abc123def456".to_string()));
    }

    #[test]
    fn test_parse_load_statement() {
        let content = r#"
load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive", "git_repository")
"#;
        let ws = parse_workspace_content(Path::new("WORKSPACE"), content).unwrap();
        assert_eq!(ws.load_statements.len(), 1);

        let load = &ws.load_statements[0];
        assert_eq!(load.path, "@bazel_tools//tools/build_defs/repo:http.bzl");
        assert_eq!(load.symbols.len(), 2);
        assert!(load.symbols.contains(&"http_archive".to_string()));
        assert!(load.symbols.contains(&"git_repository".to_string()));
    }

    #[test]
    fn test_parse_tensorflow_workspace() {
        let content = r#"
workspace(name = "org_tensorflow")

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")

http_archive(
    name = "bazel_skylib",
    sha256 = "74d544d96f4a5bb630d465ca8bbcfe231e3594e5aae57e1edbf17a6eb3ca2506",
    urls = [
        "https://storage.googleapis.com/mirror.tensorflow.org/github.com/bazelbuild/bazel-skylib/releases/download/1.3.0/bazel-skylib-1.3.0.tar.gz",
        "https://github.com/bazelbuild/bazel-skylib/releases/download/1.3.0/bazel-skylib-1.3.0.tar.gz",
    ],
)

http_archive(
    name = "rules_python",
    sha256 = "9d04041ac92a0985e344235f5d946f71ac543f1b1565f2cdbc9a2aaee8adf55b",
    strip_prefix = "rules_python-0.26.0",
    url = "https://github.com/bazelbuild/rules_python/releases/download/0.26.0/rules_python-0.26.0.tar.gz",
)
"#;
        let ws = parse_workspace_content(Path::new("WORKSPACE"), content).unwrap();

        assert_eq!(ws.name, Some("org_tensorflow".to_string()));
        assert_eq!(ws.load_statements.len(), 1);
        assert_eq!(ws.http_archives.len(), 2);

        // Check bazel_skylib
        let skylib = ws
            .http_archives
            .iter()
            .find(|a| a.name == "bazel_skylib")
            .unwrap();
        assert_eq!(skylib.urls.len(), 2);
        assert!(skylib.sha256.is_some());

        // Check rules_python
        let rules_python = ws
            .http_archives
            .iter()
            .find(|a| a.name == "rules_python")
            .unwrap();
        assert_eq!(rules_python.urls.len(), 1); // singular "url" field
        assert_eq!(
            rules_python.strip_prefix,
            Some("rules_python-0.26.0".to_string())
        );
    }
}
