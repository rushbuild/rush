// Rush Repo - Git repository handling
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Git repository cloning and management.
//!
//! Implements the `git_repository` repository rule, which clones Git repositories
//! at specific commits, tags, or branches. Provides hermetic builds by pinning
//! to exact commits.

use anyhow::{anyhow, Context, Result};
use git2::{build::RepoBuilder, Cred, FetchOptions, RemoteCallbacks, Repository};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Git repository rule.
///
/// Clones a Git repository at a specific commit, tag, or branch.
/// Similar to Bazel's `git_repository` rule.
///
/// # Example
///
/// ```no_run
/// use rush_repo::git::GitRepository;
///
/// # async fn example() -> anyhow::Result<()> {
/// let repo = GitRepository::new(
///     "my-repo".to_string(),
///     "https://github.com/example/repo.git".to_string(),
/// )
/// .with_commit("abc123def456".to_string());
///
/// let path = repo.fetch("/tmp/rush-cache").await?;
/// println!("Cloned to: {}", path.display());
/// # Ok(())
/// # }
/// ```
pub struct GitRepository {
    /// Name of the repository
    pub name: String,

    /// Git remote URL
    pub remote: String,

    /// Commit SHA to checkout (most hermetic)
    pub commit: Option<String>,

    /// Tag to checkout
    pub tag: Option<String>,

    /// Branch to checkout (least hermetic)
    pub branch: Option<String>,

    /// Shallow clone depth (None for full clone)
    pub shallow_since: Option<String>,
}

impl GitRepository {
    /// Create a new Git repository rule.
    ///
    /// # Arguments
    ///
    /// * `name` - Name of the repository
    /// * `remote` - Git remote URL
    pub fn new(name: String, remote: String) -> Self {
        Self {
            name,
            remote,
            commit: None,
            tag: None,
            branch: None,
            shallow_since: None,
        }
    }

    /// Pin to a specific commit SHA.
    pub fn with_commit(mut self, commit: String) -> Self {
        self.commit = Some(commit);
        self
    }

    /// Checkout a specific tag.
    pub fn with_tag(mut self, tag: String) -> Self {
        self.tag = Some(tag);
        self
    }

    /// Checkout a specific branch.
    pub fn with_branch(mut self, branch: String) -> Self {
        self.branch = Some(branch);
        self
    }

    /// Enable shallow cloning since a specific date.
    pub fn with_shallow_since(mut self, date: String) -> Self {
        self.shallow_since = Some(date);
        self
    }

    /// Clone the repository to the cache directory.
    ///
    /// Clones the Git repository to `cache_dir/repos/<name>`, checking out the
    /// specified commit, tag, or branch. If the repository already exists in the
    /// cache, returns the cached path without re-cloning.
    ///
    /// # Arguments
    ///
    /// * `cache_dir` - Base directory for caching repositories
    ///
    /// # Returns
    ///
    /// Path to the cloned repository directory.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No reference (commit/tag/branch) is specified
    /// - Network or authentication failures occur
    /// - The specified reference doesn't exist
    /// - Filesystem operations fail
    pub async fn fetch<P: AsRef<Path>>(&self, cache_dir: P) -> Result<PathBuf> {
        self.validate()?;

        let cache_dir = cache_dir.as_ref();
        let repos_dir = cache_dir.join("repos");
        let repo_dir = repos_dir.join(&self.name);

        // Check if already cached
        if repo_dir.exists() {
            info!(
                repository = %self.name,
                path = %repo_dir.display(),
                "Repository already cached"
            );
            return Ok(repo_dir);
        }

        info!(
            repository = %self.name,
            remote = %self.remote,
            "Cloning Git repository"
        );

        // Create repos directory
        fs::create_dir_all(&repos_dir).with_context(|| {
            format!("Failed to create repos directory: {}", repos_dir.display())
        })?;

        // Clone to temporary directory first
        let temp_dir =
            tempfile::tempdir_in(&repos_dir).context("Failed to create temporary directory")?;

        self.clone_repository(temp_dir.path())?;
        self.checkout_ref(temp_dir.path())?;

        // Remove .git directory for hermetic builds
        let git_dir = temp_dir.path().join(".git");
        if git_dir.exists() {
            debug!(path = %git_dir.display(), "Removing .git directory");
            fs::remove_dir_all(&git_dir).with_context(|| {
                format!("Failed to remove .git directory: {}", git_dir.display())
            })?;
        }

        // Move to final location
        fs::rename(temp_dir.path(), &repo_dir).with_context(|| {
            format!(
                "Failed to move {} to {}",
                temp_dir.path().display(),
                repo_dir.display()
            )
        })?;

        info!(
            repository = %self.name,
            path = %repo_dir.display(),
            "Repository cloned successfully"
        );

        Ok(repo_dir)
    }

    /// Validate repository configuration.
    ///
    /// Ensures that at least one reference (commit, tag, or branch) is specified.
    /// Warns if a branch is used, as it's not hermetic.
    fn validate(&self) -> Result<()> {
        if self.commit.is_none() && self.tag.is_none() && self.branch.is_none() {
            return Err(anyhow!(
                "Repository '{}': must specify at least one of commit, tag, or branch",
                self.name
            ));
        }

        // Warn if using branch (not hermetic)
        if self.branch.is_some() && self.commit.is_none() && self.tag.is_none() {
            warn!(
                repository = %self.name,
                branch = ?self.branch,
                "Using branch reference is not hermetic; consider using commit or tag"
            );
        }

        Ok(())
    }

    /// Clone the repository using git2.
    ///
    /// Performs the actual git clone operation with appropriate callbacks for
    /// authentication and progress reporting.
    fn clone_repository(&self, target_dir: &Path) -> Result<()> {
        let mut callbacks = RemoteCallbacks::new();

        // Set up authentication callback
        // TODO: Add support for SSH keys and credential helpers
        callbacks.credentials(|_url, username_from_url, _allowed_types| {
            debug!(
                url = _url,
                username = ?username_from_url,
                "Git credential callback invoked"
            );

            // For now, only support public repositories
            // Future: Check for SSH keys, credential helpers, etc.
            Cred::default()
        });

        // Set up progress callback for logging
        callbacks.transfer_progress(|progress| {
            if progress.received_objects() == progress.total_objects() {
                debug!(
                    received = progress.received_objects(),
                    total = progress.total_objects(),
                    bytes = progress.received_bytes(),
                    "Clone progress: receiving objects complete"
                );
            }
            true
        });

        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);

        // Configure shallow clone if requested
        if self.shallow_since.is_some() {
            fetch_options.depth(1);
            debug!("Configuring shallow clone with depth 1");
        }

        let mut builder = RepoBuilder::new();
        builder.fetch_options(fetch_options);

        // Set branch if specified
        if let Some(branch) = &self.branch {
            builder.branch(branch);
            debug!(branch = %branch, "Cloning specific branch");
        }

        debug!(
            remote = %self.remote,
            target = %target_dir.display(),
            "Starting git clone"
        );

        builder
            .clone(&self.remote, target_dir)
            .with_context(|| format!("Failed to clone repository from {}", self.remote))?;

        debug!("Git clone completed");
        Ok(())
    }

    /// Checkout the specified reference.
    ///
    /// After cloning, checks out the specific commit, tag, or branch as requested.
    /// Priority order: commit > tag > branch.
    fn checkout_ref(&self, repo_path: &Path) -> Result<()> {
        let repo = Repository::open(repo_path)
            .with_context(|| format!("Failed to open repository at {}", repo_path.display()))?;

        // Determine which ref to checkout (commit takes precedence)
        let ref_name = if let Some(commit) = &self.commit {
            debug!(commit = %commit, "Checking out specific commit");
            commit.clone()
        } else if let Some(tag) = &self.tag {
            debug!(tag = %tag, "Checking out tag");
            format!("refs/tags/{}", tag)
        } else if let Some(branch) = &self.branch {
            debug!(branch = %branch, "Checking out branch");
            format!("refs/heads/{}", branch)
        } else {
            // This should be caught by validate(), but be defensive
            return Err(anyhow!("No reference specified"));
        };

        // Resolve the reference to a commit
        let (object, reference) = repo
            .revparse_ext(&ref_name)
            .with_context(|| format!("Failed to resolve reference: {}", ref_name))?;

        // Checkout the commit
        repo.checkout_tree(&object, None)
            .with_context(|| format!("Failed to checkout reference: {}", ref_name))?;

        // Update HEAD
        match reference {
            Some(gref) => {
                debug!(reference = %gref.name().unwrap_or("unnamed"), "Setting HEAD to reference");
                repo.set_head(gref.name().unwrap_or(&ref_name))
            }
            None => {
                debug!(oid = %object.id(), "Setting HEAD to detached commit");
                repo.set_head_detached(object.id())
            }
        }
        .with_context(|| "Failed to update HEAD")?;

        // Verify checkout
        let head = repo.head().context("Failed to get HEAD")?;
        let commit = head
            .peel_to_commit()
            .context("Failed to peel HEAD to commit")?;

        info!(
            commit = %commit.id(),
            message = commit.summary().unwrap_or(""),
            "Checked out commit"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_repository_construction() {
        let repo = GitRepository::new(
            "test-repo".to_string(),
            "https://github.com/example/repo.git".to_string(),
        );

        assert_eq!(repo.name, "test-repo");
        assert_eq!(repo.remote, "https://github.com/example/repo.git");
        assert!(repo.commit.is_none());
        assert!(repo.tag.is_none());
        assert!(repo.branch.is_none());
    }

    #[test]
    fn test_builder_pattern() {
        let repo = GitRepository::new(
            "test-repo".to_string(),
            "https://github.com/example/repo.git".to_string(),
        )
        .with_commit("abc123".to_string())
        .with_shallow_since("2024-01-01".to_string());

        assert_eq!(repo.commit, Some("abc123".to_string()));
        assert_eq!(repo.shallow_since, Some("2024-01-01".to_string()));
    }

    #[test]
    fn test_validate_no_ref() {
        let repo = GitRepository::new(
            "test-repo".to_string(),
            "https://github.com/example/repo.git".to_string(),
        );

        let result = repo.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must specify at least one"));
    }

    #[test]
    fn test_validate_with_commit() {
        let repo = GitRepository::new(
            "test-repo".to_string(),
            "https://github.com/example/repo.git".to_string(),
        )
        .with_commit("abc123".to_string());

        assert!(repo.validate().is_ok());
    }

    #[test]
    fn test_validate_with_tag() {
        let repo = GitRepository::new(
            "test-repo".to_string(),
            "https://github.com/example/repo.git".to_string(),
        )
        .with_tag("v1.0.0".to_string());

        assert!(repo.validate().is_ok());
    }

    #[test]
    fn test_validate_with_branch() {
        let repo = GitRepository::new(
            "test-repo".to_string(),
            "https://github.com/example/repo.git".to_string(),
        )
        .with_branch("main".to_string());

        assert!(repo.validate().is_ok());
    }

    // Integration tests - marked with #[ignore] as they require network access
    #[tokio::test]
    #[ignore]
    async fn test_clone_public_repo_by_tag() {
        let temp_dir = tempfile::tempdir().unwrap();

        // Clone a small, stable public repository (git2 test repo)
        let repo = GitRepository::new(
            "libgit2-test".to_string(),
            "https://github.com/libgit2/TestGitRepository.git".to_string(),
        )
        .with_tag("v0.0.1".to_string());

        let result = repo.fetch(temp_dir.path()).await;
        assert!(result.is_ok());

        let repo_path = result.unwrap();
        assert!(repo_path.exists());
        assert!(repo_path.join("README").exists());

        // Verify .git directory was removed
        assert!(!repo_path.join(".git").exists());
    }

    #[tokio::test]
    #[ignore]
    async fn test_clone_public_repo_by_commit() {
        let temp_dir = tempfile::tempdir().unwrap();

        // Clone a specific commit
        let repo = GitRepository::new(
            "libgit2-test".to_string(),
            "https://github.com/libgit2/TestGitRepository.git".to_string(),
        )
        .with_commit("7b4384978d2493e851f9cca7858815fac9b10980".to_string());

        let result = repo.fetch(temp_dir.path()).await;
        assert!(result.is_ok());

        let repo_path = result.unwrap();
        assert!(repo_path.exists());
    }

    #[tokio::test]
    #[ignore]
    async fn test_clone_invalid_url() {
        let temp_dir = tempfile::tempdir().unwrap();

        let repo = GitRepository::new(
            "invalid-repo".to_string(),
            "https://github.com/this-repo-does-not-exist-12345.git".to_string(),
        )
        .with_branch("main".to_string());

        let result = repo.fetch(temp_dir.path()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn test_clone_invalid_reference() {
        let temp_dir = tempfile::tempdir().unwrap();

        let repo = GitRepository::new(
            "libgit2-test".to_string(),
            "https://github.com/libgit2/TestGitRepository.git".to_string(),
        )
        .with_tag("this-tag-does-not-exist".to_string());

        let result = repo.fetch(temp_dir.path()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn test_caching_behavior() {
        let temp_dir = tempfile::tempdir().unwrap();

        let repo = GitRepository::new(
            "libgit2-test".to_string(),
            "https://github.com/libgit2/TestGitRepository.git".to_string(),
        )
        .with_tag("v0.0.1".to_string());

        // First fetch
        let result1 = repo.fetch(temp_dir.path()).await;
        assert!(result1.is_ok());

        // Second fetch should use cache
        let result2 = repo.fetch(temp_dir.path()).await;
        assert!(result2.is_ok());
        assert_eq!(result1.unwrap(), result2.unwrap());
    }

    #[tokio::test]
    #[ignore]
    async fn test_shallow_clone() {
        let temp_dir = tempfile::tempdir().unwrap();

        let repo = GitRepository::new(
            "libgit2-test".to_string(),
            "https://github.com/libgit2/TestGitRepository.git".to_string(),
        )
        .with_branch("master".to_string())
        .with_shallow_since("2020-01-01".to_string());

        let result = repo.fetch(temp_dir.path()).await;
        assert!(result.is_ok());

        let repo_path = result.unwrap();
        assert!(repo_path.exists());
    }
}
