// Rush Build - Workspace Rules
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Workspace-level repository management rules.
//!
//! Provides rules for defining and managing external repositories at the
//! workspace level, similar to Bazel's WORKSPACE file. Supports various
//! repository types including HTTP archives, Git repos, and local paths.
//!
//! NOTE: This module is currently unused. ExternalDepManager has been replaced
//! by SystemPackageMapper for external dependency resolution.

#![allow(dead_code, unused_variables, unused_mut, clippy::all)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

// NOTE: ExternalDepManager removed, replaced by SystemPackageMapper
// use crate::external_deps::{DepSource, ExternalDep, ExternalDepManager};

/// Workspace rule manager.
pub struct WorkspaceRules {
    /// Workspace root directory.
    workspace_root: PathBuf,

    /// Registered repository rules.
    rules: Arc<RwLock<HashMap<String, Arc<RepositoryRule>>>>,

    /// External dependency manager.
    // NOTE: Replaced by SystemPackageMapper
    // dep_manager: Arc<RwLock<ExternalDepManager>>,

    /// Resolved repositories.
    resolved: Arc<RwLock<HashMap<String, ResolvedRepository>>>,
}

/// Repository rule definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryRule {
    /// Repository name.
    pub name: String,

    /// Rule type.
    pub rule_type: RepositoryRuleType,

    /// Build file content (if not in repository).
    pub build_file_content: Option<String>,

    /// Build file path (if external).
    pub build_file: Option<PathBuf>,

    /// Workspace file content.
    pub workspace_file_content: Option<String>,

    /// Patch files to apply.
    pub patches: Vec<PathBuf>,

    /// Strip prefix for archives.
    pub strip_prefix: Option<String>,

    /// Environment variables.
    pub env: HashMap<String, String>,
}

/// Type of repository rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RepositoryRuleType {
    /// HTTP archive rule.
    HttpArchive {
        /// URL to download.
        url: String,
        /// SHA256 checksum.
        sha256: Option<String>,
    },

    /// Git repository rule.
    GitRepository {
        /// Remote URL.
        remote: String,
        /// Commit SHA.
        commit: Option<String>,
        /// Tag.
        tag: Option<String>,
        /// Branch.
        branch: Option<String>,
    },

    /// Local repository rule.
    LocalRepository {
        /// Path to repository.
        path: PathBuf,
    },

    /// New HTTP archive rule.
    NewHttpArchive {
        /// URL to download.
        url: String,
        /// SHA256 checksum.
        sha256: Option<String>,
    },

    /// New git repository rule.
    NewGitRepository {
        /// Remote URL.
        remote: String,
        /// Commit SHA.
        commit: Option<String>,
        /// Tag.
        tag: Option<String>,
    },

    /// New local repository rule.
    NewLocalRepository {
        /// Path to repository.
        path: PathBuf,
    },

    /// Maven repository.
    MavenRepository {
        /// Repository URL.
        url: String,
    },

    /// Custom repository rule.
    Custom {
        /// Implementation name.
        implementation: String,
        /// Custom attributes.
        attributes: HashMap<String, String>,
    },
}

/// Resolved repository.
#[derive(Debug, Clone)]
pub struct ResolvedRepository {
    /// Repository name.
    pub name: String,

    /// Local path where repository is available.
    pub path: PathBuf,

    /// Source rule.
    pub rule: Arc<RepositoryRule>,
}

impl WorkspaceRules {
    /// Create new workspace rules manager.
    pub fn new<P: AsRef<Path>>(workspace_root: P) -> Result<Self> {
        let workspace_root = workspace_root.as_ref().to_path_buf();

        let external_dir = workspace_root.join("external");
        let dep_manager = ExternalDepManager::new(&external_dir)?;

        Ok(WorkspaceRules {
            workspace_root,
            rules: Arc::new(RwLock::new(HashMap::new())),
            dep_manager: Arc::new(RwLock::new(dep_manager)),
            resolved: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Register a repository rule.
    pub async fn register_rule(&self, rule: RepositoryRule) -> Result<()> {
        let name = rule.name.clone();
        let mut rules = self.rules.write().await;
        rules.insert(name, Arc::new(rule));
        Ok(())
    }

    /// Resolve a repository rule.
    pub async fn resolve_repository(&self, name: &str) -> Result<ResolvedRepository> {
        // Check if already resolved
        {
            let resolved = self.resolved.read().await;
            if let Some(repo) = resolved.get(name) {
                return Ok(repo.clone());
            }
        }

        // Get rule
        let rule = {
            let rules = self.rules.read().await;
            rules
                .get(name)
                .context(format!("Repository rule not found: {}", name))?
                .clone()
        };

        // Resolve based on type
        let path = self.resolve_rule_type(&rule).await?;

        // Apply patches if any
        if !rule.patches.is_empty() {
            self.apply_patches(&path, &rule.patches).await?;
        }

        // Write BUILD file if provided
        if let Some(ref build_content) = rule.build_file_content {
            tokio::fs::write(path.join("BUILD"), build_content).await?;
        } else if let Some(ref build_file) = rule.build_file {
            tokio::fs::copy(build_file, path.join("BUILD")).await?;
        }

        // Write WORKSPACE file if provided
        if let Some(ref workspace_content) = rule.workspace_file_content {
            tokio::fs::write(path.join("WORKSPACE"), workspace_content).await?;
        }

        let resolved_repo = ResolvedRepository {
            name: name.to_string(),
            path: path.clone(),
            rule,
        };

        // Cache resolution
        {
            let mut resolved = self.resolved.write().await;
            resolved.insert(name.to_string(), resolved_repo.clone());
        }

        Ok(resolved_repo)
    }

    /// Resolve rule type to local path.
    async fn resolve_rule_type(&self, rule: &RepositoryRule) -> Result<PathBuf> {
        match &rule.rule_type {
            RepositoryRuleType::HttpArchive { url, sha256 }
            | RepositoryRuleType::NewHttpArchive { url, sha256 } => {
                // Convert to external dep
                let dep = ExternalDep {
                    name: rule.name.clone(),
                    source: DepSource::Http { url: url.clone() },
                    version: None,
                    sha256: sha256.clone(),
                    strip_prefix: rule.strip_prefix.clone(),
                    build_file: rule.build_file_content.clone(),
                    patches: rule.patches.clone(),
                };

                let mut dep_manager = self.dep_manager.write().await;
                dep_manager.register(dep);
                let resolved = dep_manager.fetch(&rule.name).await?;
                Ok(resolved.path)
            }

            RepositoryRuleType::GitRepository {
                remote,
                commit,
                tag,
                branch,
            } => {
                let dep = ExternalDep {
                    name: rule.name.clone(),
                    source: DepSource::Git {
                        url: remote.clone(),
                        commit: commit.clone(),
                        tag: tag.clone(),
                        branch: branch.clone().or_else(|| Some("main".to_string())),
                    },
                    version: None,
                    sha256: None,
                    strip_prefix: None,
                    build_file: rule.build_file_content.clone(),
                    patches: rule.patches.clone(),
                };

                let mut dep_manager = self.dep_manager.write().await;
                dep_manager.register(dep);
                let resolved = dep_manager.fetch(&rule.name).await?;
                Ok(resolved.path)
            }

            RepositoryRuleType::NewGitRepository {
                remote,
                commit,
                tag,
            } => {
                let dep = ExternalDep {
                    name: rule.name.clone(),
                    source: DepSource::Git {
                        url: remote.clone(),
                        commit: commit.clone(),
                        tag: tag.clone(),
                        branch: Some("main".to_string()),
                    },
                    version: None,
                    sha256: None,
                    strip_prefix: None,
                    build_file: rule.build_file_content.clone(),
                    patches: rule.patches.clone(),
                };

                let mut dep_manager = self.dep_manager.write().await;
                dep_manager.register(dep);
                let resolved = dep_manager.fetch(&rule.name).await?;
                Ok(resolved.path)
            }

            RepositoryRuleType::LocalRepository { path }
            | RepositoryRuleType::NewLocalRepository { path } => {
                // Local path - just return it
                Ok(path.clone())
            }

            RepositoryRuleType::MavenRepository { url } => {
                // Create directory for Maven repository index
                let maven_dir = self.workspace_root.join("external").join(&rule.name);
                tokio::fs::create_dir_all(&maven_dir).await?;

                // Store repository URL in metadata
                let metadata = format!("repository_url: {}\n", url);
                tokio::fs::write(maven_dir.join("METADATA"), metadata).await?;

                Ok(maven_dir)
            }

            RepositoryRuleType::Custom {
                implementation,
                attributes: _,
            } => {
                // Custom repository rules - not implemented
                anyhow::bail!(
                    "Custom repository rules not yet supported: {}",
                    implementation
                );
            }
        }
    }

    /// Apply patches to repository.
    async fn apply_patches(&self, repo_path: &Path, patches: &[PathBuf]) -> Result<()> {
        for patch in patches {
            let patch_path = if patch.is_absolute() {
                patch.clone()
            } else {
                self.workspace_root.join(patch)
            };

            tokio::process::Command::new("patch")
                .arg("-p1")
                .arg("-i")
                .arg(&patch_path)
                .current_dir(repo_path)
                .status()
                .await
                .context("Failed to apply patch")?;
        }

        Ok(())
    }

    /// Resolve all repository rules.
    pub async fn resolve_all(&self) -> Result<Vec<ResolvedRepository>> {
        let rule_names: Vec<String> = {
            let rules = self.rules.read().await;
            rules.keys().cloned().collect()
        };

        let mut resolved_repos = Vec::new();

        for name in rule_names {
            let repo = self.resolve_repository(&name).await?;
            resolved_repos.push(repo);
        }

        Ok(resolved_repos)
    }

    /// Load WORKSPACE file.
    pub async fn load_workspace_file<P: AsRef<Path>>(&self, workspace_file: P) -> Result<()> {
        let _content = tokio::fs::read_to_string(workspace_file.as_ref())
            .await
            .context("Failed to read WORKSPACE file")?;

        // Parse WORKSPACE file
        // For now, this is a placeholder - real implementation would parse the DSL
        // and extract repository rules

        Ok(())
    }

    /// List all repository rules.
    pub async fn list_rules(&self) -> Vec<String> {
        let rules = self.rules.read().await;
        rules.keys().cloned().collect()
    }

    /// Get repository path.
    pub async fn get_repository_path(&self, name: &str) -> Option<PathBuf> {
        let resolved = self.resolved.read().await;
        resolved.get(name).map(|r| r.path.clone())
    }
}

/// Builder for HTTP archive rules.
pub struct HttpArchiveBuilder {
    name: String,
    url: String,
    sha256: Option<String>,
    strip_prefix: Option<String>,
    build_file_content: Option<String>,
    patches: Vec<PathBuf>,
}

impl HttpArchiveBuilder {
    /// Create new HTTP archive builder.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        HttpArchiveBuilder {
            name: name.into(),
            url: url.into(),
            sha256: None,
            strip_prefix: None,
            build_file_content: None,
            patches: Vec::new(),
        }
    }

    /// Set SHA256 checksum.
    pub fn sha256(mut self, sha256: impl Into<String>) -> Self {
        self.sha256 = Some(sha256.into());
        self
    }

    /// Set strip prefix.
    pub fn strip_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.strip_prefix = Some(prefix.into());
        self
    }

    /// Set BUILD file content.
    pub fn build_file_content(mut self, content: impl Into<String>) -> Self {
        self.build_file_content = Some(content.into());
        self
    }

    /// Add patch file.
    pub fn patch(mut self, patch: PathBuf) -> Self {
        self.patches.push(patch);
        self
    }

    /// Build repository rule.
    pub fn build(self) -> RepositoryRule {
        RepositoryRule {
            name: self.name,
            rule_type: RepositoryRuleType::HttpArchive {
                url: self.url,
                sha256: self.sha256,
            },
            build_file_content: self.build_file_content,
            build_file: None,
            workspace_file_content: None,
            patches: self.patches,
            strip_prefix: self.strip_prefix,
            env: HashMap::new(),
        }
    }
}

/// Builder for git repository rules.
pub struct GitRepositoryBuilder {
    name: String,
    remote: String,
    commit: Option<String>,
    tag: Option<String>,
    branch: Option<String>,
    build_file_content: Option<String>,
    patches: Vec<PathBuf>,
}

impl GitRepositoryBuilder {
    /// Create new git repository builder.
    pub fn new(name: impl Into<String>, remote: impl Into<String>) -> Self {
        GitRepositoryBuilder {
            name: name.into(),
            remote: remote.into(),
            commit: None,
            tag: None,
            branch: None,
            build_file_content: None,
            patches: Vec::new(),
        }
    }

    /// Set commit.
    pub fn commit(mut self, commit: impl Into<String>) -> Self {
        self.commit = Some(commit.into());
        self
    }

    /// Set tag.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Set branch.
    pub fn branch(mut self, branch: impl Into<String>) -> Self {
        self.branch = Some(branch.into());
        self
    }

    /// Set BUILD file content.
    pub fn build_file_content(mut self, content: impl Into<String>) -> Self {
        self.build_file_content = Some(content.into());
        self
    }

    /// Build repository rule.
    pub fn build(self) -> RepositoryRule {
        RepositoryRule {
            name: self.name,
            rule_type: RepositoryRuleType::GitRepository {
                remote: self.remote,
                commit: self.commit,
                tag: self.tag,
                branch: self.branch,
            },
            build_file_content: self.build_file_content,
            build_file: None,
            workspace_file_content: None,
            patches: self.patches,
            strip_prefix: None,
            env: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_archive_builder() {
        let rule = HttpArchiveBuilder::new("test", "https://example.com/test.tar.gz")
            .sha256("abc123")
            .strip_prefix("test-1.0")
            .build_file_content("# BUILD file")
            .build();

        assert_eq!(rule.name, "test");
        assert!(matches!(
            rule.rule_type,
            RepositoryRuleType::HttpArchive { .. }
        ));
        assert_eq!(rule.strip_prefix, Some("test-1.0".to_string()));
        assert!(rule.build_file_content.is_some());
    }

    #[test]
    fn test_git_repository_builder() {
        let rule = GitRepositoryBuilder::new("test", "https://github.com/test/repo.git")
            .tag("v1.0.0")
            .build_file_content("# BUILD")
            .build();

        assert_eq!(rule.name, "test");
        assert!(matches!(
            rule.rule_type,
            RepositoryRuleType::GitRepository { .. }
        ));

        if let RepositoryRuleType::GitRepository { ref tag, .. } = rule.rule_type {
            assert_eq!(tag, &Some("v1.0.0".to_string()));
        }
    }

    #[tokio::test]
    async fn test_workspace_rules_creation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = WorkspaceRules::new(temp_dir.path()).unwrap();

        let rules = workspace.list_rules().await;
        assert_eq!(rules.len(), 0);
    }

    #[tokio::test]
    async fn test_register_rule() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = WorkspaceRules::new(temp_dir.path()).unwrap();

        let rule = HttpArchiveBuilder::new("test", "https://example.com/test.tar.gz").build();

        workspace.register_rule(rule).await.unwrap();

        let rules = workspace.list_rules().await;
        assert_eq!(rules.len(), 1);
        assert!(rules.contains(&"test".to_string()));
    }
}
