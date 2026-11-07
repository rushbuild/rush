// Rush workspace cache path helpers
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use std::path::{Path, PathBuf};

/// Get the workspace cache root (defaults to `<workspace>/.cache/rush`)
pub fn workspace_cache_root(workspace_root: &Path) -> PathBuf {
    if let Ok(dir) = std::env::var("RUSH_WORKSPACE_CACHE_ROOT") {
        PathBuf::from(dir)
    } else {
        workspace_root.join(".cache").join("rush")
    }
}

/// Get the workspace external repository directory (cache_root/external)
pub fn workspace_external_dir(workspace_root: &Path) -> PathBuf {
    workspace_cache_root(workspace_root).join("external")
}

/// Directory used to track which external repos were fetched via Bazel
pub fn workspace_bazel_fetch_marker_dir(workspace_root: &Path) -> PathBuf {
    workspace_external_dir(workspace_root).join(".rush_bazel_fetched")
}

/// Phase 5.C.4 commit 134 (Batch Q): canonical path
/// to the workspace-vendored external repo directory
/// for `<repo_name>`. This is the **vendored** copy
/// the user sees in `<workspace>/external/<repo>/`,
/// distinct from rush's own
/// `<cache_root>/external/<repo>` (see
/// `workspace_external_dir`). The pre-existing
/// SKIP_FETCH_REPOS allowlist in
/// `bzl_loader::try_fetch_known_repo` checks the
/// vendored copy; commit 134's
/// `RUSH_FETCH_SKIP_IF_MATERIALIZED=1` extension
/// generalizes the same check to any repo with a
/// BUILD file present at this path.
pub fn workspace_vendored_external_repo_dir(
    workspace_root: &Path,
    repo_name: &str,
) -> PathBuf {
    workspace_root.join("external").join(repo_name)
}

/// Get the workspace output root (defaults to `<workspace>/rush-out`)
pub fn workspace_output_root(workspace_root: &Path) -> PathBuf {
    if let Ok(dir) = std::env::var("RUSH_WORKSPACE_OUTPUT_ROOT") {
        PathBuf::from(dir)
    } else {
        workspace_root.join("rush-out")
    }
}

/// Get the workspace bin symlink target (defaults to `<workspace>/rush-bin`)
pub fn workspace_bin_symlink(workspace_root: &Path) -> PathBuf {
    if let Ok(path) = std::env::var("RUSH_WORKSPACE_BIN_LINK") {
        PathBuf::from(path)
    } else {
        workspace_root.join("rush-bin")
    }
}
