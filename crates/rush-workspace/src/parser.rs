//! WORKSPACE file parser.
//!
//! Parses a simplified WORKSPACE format compatible with Bazel's syntax.

use crate::{ExternalRepository, Workspace};
use anyhow::{Context, Result};
use std::path::PathBuf;

/// Parse WORKSPACE file content.
pub fn parse_workspace(content: &str) -> Result<Workspace> {
    let mut workspace = Workspace::new();
    let mut lines = content.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();

        // Skip comments and empty lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Parse workspace name
        if trimmed.starts_with("workspace(") {
            if let Some(name) = parse_workspace_name(trimmed) {
                workspace.name = Some(name);
            }
            continue;
        }

        // Parse http_archive
        if trimmed.starts_with("http_archive(") {
            let repo = parse_http_archive(&mut lines, trimmed)?;
            workspace.add_repository(repo);
            continue;
        }

        // Parse git_repository
        if trimmed.starts_with("git_repository(") {
            let repo = parse_git_repository(&mut lines, trimmed)?;
            workspace.add_repository(repo);
            continue;
        }

        // Parse local_repository
        if trimmed.starts_with("local_repository(") {
            let repo = parse_local_repository(&mut lines, trimmed)?;
            workspace.add_repository(repo);
            continue;
        }
    }

    Ok(workspace)
}

/// Parse workspace name.
fn parse_workspace_name(line: &str) -> Option<String> {
    // workspace(name = "rush")
    extract_quoted_value(line, "name")
}

/// Parse http_archive rule.
fn parse_http_archive<'a, I>(lines: &mut I, first_line: &str) -> Result<ExternalRepository>
where
    I: Iterator<Item = &'a str>,
{
    let mut name = None;
    let mut url = None;
    let mut sha256 = None;
    let mut strip_prefix = None;
    let mut build_file = None;

    // Check if single-line definition
    if first_line.contains(')') {
        let content = first_line;
        name = extract_quoted_value(content, "name");
        url = extract_quoted_value(content, "url");
        sha256 = extract_quoted_value(content, "sha256");
        strip_prefix = extract_quoted_value(content, "strip_prefix");
        build_file = extract_quoted_value(content, "build_file");
    } else {
        // Multi-line definition
        for line in lines {
            let trimmed = line.trim();

            if trimmed.contains("name =") {
                name = extract_quoted_value(trimmed, "name");
            } else if trimmed.contains("url =") || trimmed.contains("urls =") {
                url = extract_quoted_value(trimmed, "url")
                    .or_else(|| extract_list_first(trimmed, "urls"));
            } else if trimmed.contains("sha256 =") {
                sha256 = extract_quoted_value(trimmed, "sha256");
            } else if trimmed.contains("strip_prefix =") {
                strip_prefix = extract_quoted_value(trimmed, "strip_prefix");
            } else if trimmed.contains("build_file =") {
                build_file = extract_quoted_value(trimmed, "build_file");
            }

            if trimmed.ends_with(')') {
                break;
            }
        }
    }

    let name = name.context("http_archive missing 'name' attribute")?;
    let url = url.context("http_archive missing 'url' attribute")?;

    Ok(ExternalRepository::Http {
        name,
        url,
        sha256,
        strip_prefix,
        build_file,
    })
}

/// Parse git_repository rule.
fn parse_git_repository<'a, I>(lines: &mut I, first_line: &str) -> Result<ExternalRepository>
where
    I: Iterator<Item = &'a str>,
{
    let mut name = None;
    let mut remote = None;
    let mut commit = None;
    let mut shallow_since = None;
    let mut build_file = None;

    if first_line.contains(')') {
        let content = first_line;
        name = extract_quoted_value(content, "name");
        remote = extract_quoted_value(content, "remote");
        commit = extract_quoted_value(content, "commit")
            .or_else(|| extract_quoted_value(content, "tag"));
        shallow_since = extract_quoted_value(content, "shallow_since");
        build_file = extract_quoted_value(content, "build_file");
    } else {
        for line in lines {
            let trimmed = line.trim();

            if trimmed.contains("name =") {
                name = extract_quoted_value(trimmed, "name");
            } else if trimmed.contains("remote =") {
                remote = extract_quoted_value(trimmed, "remote");
            } else if trimmed.contains("commit =") {
                commit = extract_quoted_value(trimmed, "commit");
            } else if trimmed.contains("tag =") {
                commit = extract_quoted_value(trimmed, "tag");
            } else if trimmed.contains("shallow_since =") {
                shallow_since = extract_quoted_value(trimmed, "shallow_since");
            } else if trimmed.contains("build_file =") {
                build_file = extract_quoted_value(trimmed, "build_file");
            }

            if trimmed.ends_with(')') {
                break;
            }
        }
    }

    let name = name.context("git_repository missing 'name' attribute")?;
    let remote = remote.context("git_repository missing 'remote' attribute")?;
    let commit = commit.context("git_repository missing 'commit' or 'tag' attribute")?;

    Ok(ExternalRepository::Git {
        name,
        remote,
        commit,
        shallow_since,
        build_file,
    })
}

/// Parse local_repository rule.
fn parse_local_repository<'a, I>(lines: &mut I, first_line: &str) -> Result<ExternalRepository>
where
    I: Iterator<Item = &'a str>,
{
    let mut name = None;
    let mut path = None;

    if first_line.contains(')') {
        let content = first_line;
        name = extract_quoted_value(content, "name");
        path = extract_quoted_value(content, "path");
    } else {
        for line in lines {
            let trimmed = line.trim();

            if trimmed.contains("name =") {
                name = extract_quoted_value(trimmed, "name");
            } else if trimmed.contains("path =") {
                path = extract_quoted_value(trimmed, "path");
            }

            if trimmed.ends_with(')') {
                break;
            }
        }
    }

    let name = name.context("local_repository missing 'name' attribute")?;
    let path = path.context("local_repository missing 'path' attribute")?;

    Ok(ExternalRepository::Local {
        name,
        path: PathBuf::from(path),
    })
}

/// Extract quoted string value from attribute.
fn extract_quoted_value(line: &str, attr: &str) -> Option<String> {
    let pattern = format!("{} =", attr);
    if let Some(start) = line.find(&pattern) {
        let after_eq = &line[start + pattern.len()..];
        let trimmed = after_eq.trim();

        // Find first quote
        if let Some(q_start) = trimmed.find('"') {
            // Find matching closing quote
            if let Some(q_end) = trimmed[q_start + 1..].find('"') {
                return Some(trimmed[q_start + 1..q_start + 1 + q_end].to_string());
            }
        }
    }
    None
}

/// Extract first item from list attribute.
fn extract_list_first(line: &str, attr: &str) -> Option<String> {
    let pattern = format!("{} =", attr);
    if let Some(start) = line.find(&pattern) {
        let after_eq = &line[start + pattern.len()..];
        let trimmed = after_eq.trim();

        // Find opening bracket
        if let Some(bracket_start) = trimmed.find('[') {
            let after_bracket = &trimmed[bracket_start + 1..];

            // Find first quoted string in list
            if let Some(q_start) = after_bracket.find('"') {
                if let Some(q_end) = after_bracket[q_start + 1..].find('"') {
                    return Some(after_bracket[q_start + 1..q_start + 1 + q_end].to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_workspace_name() {
        let content = r#"workspace(name = "my_project")"#;
        let ws = parse_workspace(content).unwrap();
        assert_eq!(ws.name, Some("my_project".to_string()));
    }

    #[test]
    fn test_parse_http_archive_single_line() {
        let content = r#"http_archive(name = "test", url = "https://example.com/test.tar.gz")"#;
        let ws = parse_workspace(content).unwrap();

        assert_eq!(ws.repositories.len(), 1);
        match &ws.repositories[0] {
            ExternalRepository::Http { name, url, .. } => {
                assert_eq!(name, "test");
                assert_eq!(url, "https://example.com/test.tar.gz");
            }
            _ => panic!("Expected HTTP repository"),
        }
    }

    #[test]
    fn test_parse_http_archive_multi_line() {
        let content = r#"
http_archive(
    name = "protobuf",
    url = "https://github.com/protocolbuffers/protobuf/archive/v3.21.0.tar.gz",
    sha256 = "abc123",
    strip_prefix = "protobuf-3.21.0",
)
"#;
        let ws = parse_workspace(content).unwrap();

        assert_eq!(ws.repositories.len(), 1);
        match &ws.repositories[0] {
            ExternalRepository::Http {
                name,
                url,
                sha256,
                strip_prefix,
                ..
            } => {
                assert_eq!(name, "protobuf");
                assert!(url.contains("protobuf"));
                assert_eq!(sha256.as_ref().unwrap(), "abc123");
                assert_eq!(strip_prefix.as_ref().unwrap(), "protobuf-3.21.0");
            }
            _ => panic!("Expected HTTP repository"),
        }
    }

    #[test]
    fn test_parse_git_repository() {
        let content = r#"
git_repository(
    name = "llvm-project",
    remote = "https://github.com/llvm/llvm-project.git",
    commit = "abc123def",
)
"#;
        let ws = parse_workspace(content).unwrap();

        assert_eq!(ws.repositories.len(), 1);
        match &ws.repositories[0] {
            ExternalRepository::Git {
                name,
                remote,
                commit,
                ..
            } => {
                assert_eq!(name, "llvm-project");
                assert!(remote.contains("llvm"));
                assert_eq!(commit, "abc123def");
            }
            _ => panic!("Expected Git repository"),
        }
    }

    #[test]
    fn test_parse_local_repository() {
        let content = r#"
local_repository(
    name = "local_lib",
    path = "../local_lib",
)
"#;
        let ws = parse_workspace(content).unwrap();

        assert_eq!(ws.repositories.len(), 1);
        match &ws.repositories[0] {
            ExternalRepository::Local { name, path } => {
                assert_eq!(name, "local_lib");
                assert_eq!(path, &PathBuf::from("../local_lib"));
            }
            _ => panic!("Expected Local repository"),
        }
    }

    #[test]
    fn test_parse_multiple_repositories() {
        let content = r#"
workspace(name = "test_workspace")

http_archive(
    name = "repo1",
    url = "https://example.com/repo1.tar.gz",
)

git_repository(
    name = "repo2",
    remote = "https://github.com/example/repo2.git",
    commit = "main",
)

local_repository(
    name = "repo3",
    path = "./third_party/repo3",
)
"#;
        let ws = parse_workspace(content).unwrap();

        assert_eq!(ws.name, Some("test_workspace".to_string()));
        assert_eq!(ws.repositories.len(), 3);
    }

    #[test]
    fn test_parse_with_comments() {
        let content = r#"
# This is a comment
workspace(name = "test")

# Another comment
http_archive(
    # Inline comment
    name = "test",
    url = "https://example.com/test.tar.gz",  # Trailing comment
)
"#;
        let ws = parse_workspace(content).unwrap();
        assert_eq!(ws.repositories.len(), 1);
    }
}
