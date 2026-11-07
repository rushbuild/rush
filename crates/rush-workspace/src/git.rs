//! Git repository cloning and management.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;

/// Fetch Git repository.
pub async fn fetch_git_repository(
    cache_dir: &Path,
    name: &str,
    remote: &str,
    commit: &str,
    _shallow_since: Option<&str>,
    build_file: Option<&str>,
) -> Result<PathBuf> {
    let repo_dir = cache_dir.join(name);

    // Check if already cloned
    if repo_dir.exists() && repo_dir.join(".git").exists() {
        println!("📦 Using cached git repository: {}", name);
        return Ok(repo_dir);
    }

    println!("📥 Cloning {} from {}", name, remote);

    // Clone in blocking context
    let remote = remote.to_string();
    let commit = commit.to_string();
    let repo_dir_clone = repo_dir.clone();

    tokio::task::spawn_blocking(move || clone_git_repository(&repo_dir_clone, &remote, &commit))
        .await??;

    // Copy BUILD file if provided
    if let Some(build_file_path) = build_file {
        let src = PathBuf::from(build_file_path);
        let dst = repo_dir.join("BUILD");
        if src.exists() {
            fs::copy(&src, &dst)
                .await
                .with_context(|| format!("Failed to copy BUILD file from {}", build_file_path))?;
        }
    }

    Ok(repo_dir)
}

/// Clone Git repository (blocking).
fn clone_git_repository(dest: &Path, remote: &str, commit_or_tag: &str) -> Result<()> {
    use git2::{build::RepoBuilder, FetchOptions};

    // Create parent directory
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Configure fetch options
    let mut fetch_opts = FetchOptions::new();
    fetch_opts.depth(1); // Shallow clone

    let mut builder = RepoBuilder::new();
    builder.fetch_options(fetch_opts);

    // Clone repository
    let repo = builder
        .clone(remote, dest)
        .with_context(|| format!("Failed to clone repository from {}", remote))?;

    // Checkout specific commit/tag
    checkout_ref(&repo, commit_or_tag)?;

    Ok(())
}

/// Checkout specific commit or tag.
fn checkout_ref(repo: &git2::Repository, commit_or_tag: &str) -> Result<()> {
    // Try to find as commit first
    if let Ok(oid) = git2::Oid::from_str(commit_or_tag) {
        if let Ok(commit) = repo.find_commit(oid) {
            let tree = commit.tree()?;
            repo.checkout_tree(tree.as_object(), None)?;
            repo.set_head_detached(commit.id())?;
            return Ok(());
        }
    }

    // Try as tag
    if let Ok(tag_ref) = repo.find_reference(&format!("refs/tags/{}", commit_or_tag)) {
        let commit = tag_ref.peel_to_commit()?;
        let tree = commit.tree()?;
        repo.checkout_tree(tree.as_object(), None)?;
        repo.set_head_detached(commit.id())?;
        return Ok(());
    }

    // Try as branch
    if let Ok(branch_ref) = repo.find_reference(&format!("refs/heads/{}", commit_or_tag)) {
        let commit = branch_ref.peel_to_commit()?;
        let tree = commit.tree()?;
        repo.checkout_tree(tree.as_object(), None)?;
        repo.set_head_detached(commit.id())?;
        return Ok(());
    }

    anyhow::bail!("Could not find commit, tag, or branch: {}", commit_or_tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    #[ignore] // Requires internet connection
    async fn test_clone_small_repo() {
        let temp_dir = TempDir::new().unwrap();

        let result = fetch_git_repository(
            temp_dir.path(),
            "test_repo",
            "https://github.com/octocat/Hello-World.git",
            "master",
            None,
            None,
        )
        .await;

        if result.is_ok() {
            let repo_path = result.unwrap();
            assert!(repo_path.exists());
            assert!(repo_path.join(".git").exists());
        }
    }
}
