//! Local repository handling.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Resolve local repository path.
pub async fn resolve_local_repository(
    _workspace_root: &Path,
    _name: &str,
    path: &Path,
) -> Result<PathBuf> {
    // Local repositories are just path references
    // Validate the path exists
    if !path.exists() {
        anyhow::bail!("Local repository path does not exist: {}", path.display());
    }

    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_resolve_existing_path() {
        let temp_dir = TempDir::new().unwrap();
        let result = resolve_local_repository(temp_dir.path(), "test", temp_dir.path()).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_resolve_non_existing_path() {
        let temp_dir = TempDir::new().unwrap();
        let non_existent = temp_dir.path().join("does_not_exist");

        let result = resolve_local_repository(temp_dir.path(), "test", &non_existent).await;

        assert!(result.is_err());
    }
}
