//! External workspace and dependency management for Rush build system.
//!
//! This module provides support for:
//! - WORKSPACE file parsing
//! - External repository downloading (HTTP, Git, local)
//! - Dependency caching and verification
//! - Repository resolution and management

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;

pub mod download;
pub mod git;
pub mod http;
pub mod local;
pub mod parser;
pub mod resolver;

/// Represents an external repository dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExternalRepository {
    /// HTTP/HTTPS archive download.
    Http {
        /// Repository name (e.g., "protobuf").
        name: String,
        /// Download URL.
        url: String,
        /// Expected SHA256 hash for verification.
        sha256: Option<String>,
        /// Strip prefix from extracted archive.
        strip_prefix: Option<String>,
        /// BUILD file to use (if not present in archive).
        build_file: Option<String>,
    },
    /// Git repository clone.
    Git {
        /// Repository name.
        name: String,
        /// Git remote URL.
        remote: String,
        /// Commit SHA, tag, or branch.
        commit: String,
        /// Shallow clone depth (None = full clone).
        shallow_since: Option<String>,
        /// BUILD file to use.
        build_file: Option<String>,
    },
    /// Local directory reference.
    Local {
        /// Repository name.
        name: String,
        /// Path to local directory.
        path: PathBuf,
    },
}

impl ExternalRepository {
    /// Get the repository name.
    pub fn name(&self) -> &str {
        match self {
            ExternalRepository::Http { name, .. } => name,
            ExternalRepository::Git { name, .. } => name,
            ExternalRepository::Local { name, .. } => name,
        }
    }

    /// Get a unique identifier for caching.
    pub fn cache_key(&self) -> String {
        match self {
            ExternalRepository::Http { url, sha256, .. } => {
                if let Some(hash) = sha256 {
                    format!("http_{}", hash)
                } else {
                    format!("http_{}", blake3::hash(url.as_bytes()))
                }
            }
            ExternalRepository::Git { remote, commit, .. } => {
                format!("git_{}_{}", blake3::hash(remote.as_bytes()), commit)
            }
            ExternalRepository::Local { path, .. } => {
                format!("local_{}", blake3::hash(path.to_string_lossy().as_bytes()))
            }
        }
    }
}

/// WORKSPACE file representation.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// Workspace name.
    pub name: Option<String>,
    /// External repositories.
    pub repositories: Vec<ExternalRepository>,
}

impl Workspace {
    /// Create empty workspace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse WORKSPACE file from path.
    pub async fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref())
            .await
            .context("Failed to read WORKSPACE file")?;

        parser::parse_workspace(&content)
    }

    /// Add an external repository.
    pub fn add_repository(&mut self, repo: ExternalRepository) {
        self.repositories.push(repo);
    }

    /// Find repository by name.
    pub fn find_repository(&self, name: &str) -> Option<&ExternalRepository> {
        self.repositories.iter().find(|r| r.name() == name)
    }
}

/// External repository manager.
pub struct RepositoryManager {
    /// Workspace root directory.
    workspace_root: PathBuf,
    /// External repository cache directory.
    cache_dir: PathBuf,
    /// Loaded workspace.
    workspace: Workspace,
    /// Resolved repository paths.
    resolved: HashMap<String, PathBuf>,
}

impl RepositoryManager {
    /// Create new repository manager.
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        let workspace_root = workspace_root.into();
        let cache_dir = workspace_root.join(".rush-cache").join("external");

        Self {
            workspace_root,
            cache_dir,
            workspace: Workspace::new(),
            resolved: HashMap::new(),
        }
    }

    /// Load WORKSPACE file.
    pub async fn load_workspace(&mut self) -> Result<()> {
        let workspace_file = self.workspace_root.join("WORKSPACE");

        if workspace_file.exists() {
            self.workspace = Workspace::from_file(&workspace_file).await?;
        }

        Ok(())
    }

    /// Ensure cache directory exists.
    async fn ensure_cache_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.cache_dir).await?;
        Ok(())
    }

    /// Fetch and cache an external repository.
    pub async fn fetch_repository(&mut self, name: &str) -> Result<PathBuf> {
        // Check if already resolved
        if let Some(path) = self.resolved.get(name) {
            return Ok(path.clone());
        }

        // Find repository definition
        let repo = self
            .workspace
            .find_repository(name)
            .ok_or_else(|| anyhow::anyhow!("Repository '{}' not found in WORKSPACE", name))?
            .clone();

        self.ensure_cache_dir().await?;

        // Fetch based on type
        let repo_path = match &repo {
            ExternalRepository::Http {
                url,
                sha256,
                strip_prefix,
                build_file,
                ..
            } => {
                http::fetch_http_archive(
                    &self.cache_dir,
                    name,
                    url,
                    sha256.as_deref(),
                    strip_prefix.as_deref(),
                    build_file.as_deref(),
                )
                .await?
            }
            ExternalRepository::Git {
                remote,
                commit,
                shallow_since,
                build_file,
                ..
            } => {
                git::fetch_git_repository(
                    &self.cache_dir,
                    name,
                    remote,
                    commit,
                    shallow_since.as_deref(),
                    build_file.as_deref(),
                )
                .await?
            }
            ExternalRepository::Local { path, .. } => {
                // Local repositories don't need fetching
                self.workspace_root.join(path)
            }
        };

        self.resolved.insert(name.to_string(), repo_path.clone());
        Ok(repo_path)
    }

    /// Resolve repository path (fetch if needed).
    pub async fn resolve(&mut self, name: &str) -> Result<PathBuf> {
        self.fetch_repository(name).await
    }

    /// Get all repository names.
    pub fn repository_names(&self) -> Vec<&str> {
        self.workspace
            .repositories
            .iter()
            .map(|r| r.name())
            .collect()
    }

    /// Clean cache directory.
    pub async fn clean_cache(&self) -> Result<()> {
        if self.cache_dir.exists() {
            fs::remove_dir_all(&self.cache_dir).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repository_name() {
        let repo = ExternalRepository::Http {
            name: "test".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: None,
            strip_prefix: None,
            build_file: None,
        };

        assert_eq!(repo.name(), "test");
    }

    #[test]
    fn test_cache_key_uniqueness() {
        let repo1 = ExternalRepository::Http {
            name: "test".to_string(),
            url: "https://example.com/v1.tar.gz".to_string(),
            sha256: None,
            strip_prefix: None,
            build_file: None,
        };

        let repo2 = ExternalRepository::Http {
            name: "test".to_string(),
            url: "https://example.com/v2.tar.gz".to_string(),
            sha256: None,
            strip_prefix: None,
            build_file: None,
        };

        assert_ne!(repo1.cache_key(), repo2.cache_key());
    }

    #[tokio::test]
    async fn test_workspace_creation() {
        let ws = Workspace::new();
        assert!(ws.repositories.is_empty());
    }

    #[tokio::test]
    async fn test_add_repository() {
        let mut ws = Workspace::new();

        let repo = ExternalRepository::Local {
            name: "local_test".to_string(),
            path: PathBuf::from("/tmp/test"),
        };

        ws.add_repository(repo);
        assert_eq!(ws.repositories.len(), 1);
        assert!(ws.find_repository("local_test").is_some());
    }
}
