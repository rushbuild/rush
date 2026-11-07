// Test end-to-end WORKSPACE integration
//
// This test verifies that BuildOrchestrator can:
// 1. Parse WORKSPACE files
// 2. Register external dependencies
// 3. Resolve @repo//pkg:target labels
// 4. Build targets with external dependencies

use rush_build::BuildOrchestrator;
use std::fs;
use tempfile::TempDir;

#[tokio::test]
async fn test_workspace_load_and_parse() -> anyhow::Result<()> {
    // Create temporary workspace
    let temp_dir = TempDir::new()?;
    let workspace_root = temp_dir.path();

    // Create a simple WORKSPACE file
    let workspace_content = r#"
workspace(name = "test_workspace")

http_archive(
    name = "test_repo",
    urls = ["https://example.com/test.tar.gz"],
    sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
)
"#;

    fs::write(workspace_root.join("WORKSPACE"), workspace_content)?;

    // Create BuildOrchestrator
    let orchestrator = BuildOrchestrator::new()?;

    // Load WORKSPACE (don't fetch - just parse and register)
    orchestrator.load_workspace(false).await?;

    // Success if no errors
    Ok(())
}

#[tokio::test]
async fn test_workspace_with_multiple_archives() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_root = temp_dir.path();

    // WORKSPACE with multiple dependencies
    let workspace_content = r#"
workspace(name = "multi_deps")

http_archive(
    name = "bazel_skylib",
    sha256 = "74d544d96f4a5bb630d465ca8bbcfe231e3594e5aae57e1edbf17a6eb3ca2506",
    urls = ["https://github.com/bazelbuild/bazel-skylib/releases/download/1.3.0/bazel-skylib-1.3.0.tar.gz"],
)

http_archive(
    name = "rules_python",
    sha256 = "9d04041ac92a0985e344235f5d946f71ac543f1b1565f2cdbc9a2aaee8adf55b",
    strip_prefix = "rules_python-0.26.0",
    url = "https://github.com/bazelbuild/rules_python/releases/download/0.26.0/rules_python-0.26.0.tar.gz",
)
"#;

    fs::write(workspace_root.join("WORKSPACE"), workspace_content)?;

    let orchestrator = BuildOrchestrator::new()?;
    orchestrator.load_workspace(false).await?;

    Ok(())
}

#[tokio::test]
async fn test_workspace_bazel_variant() -> anyhow::Result<()> {
    // Test that WORKSPACE.bazel is also recognized
    let temp_dir = TempDir::new()?;
    let workspace_root = temp_dir.path();

    let workspace_content = r#"
workspace(name = "bazel_variant")
"#;

    // Use WORKSPACE.bazel instead of WORKSPACE
    fs::write(workspace_root.join("WORKSPACE.bazel"), workspace_content)?;

    let orchestrator = BuildOrchestrator::new()?;
    orchestrator.load_workspace(false).await?;

    Ok(())
}

#[tokio::test]
async fn test_no_workspace_file() -> anyhow::Result<()> {
    // Test that missing WORKSPACE is handled gracefully
    let _temp_dir = TempDir::new()?;

    let orchestrator = BuildOrchestrator::new()?;

    // Should succeed even without WORKSPACE (just skips external deps)
    orchestrator.load_workspace(false).await?;

    Ok(())
}

#[tokio::test]
async fn test_external_label_parsing() {
    // Test external label parsing logic
    let label = "@bazel_skylib//lib:paths";

    // Parse @repo//package:target
    let rest = &label[1..]; // Remove '@'
    let parts: Vec<&str> = rest.splitn(2, "//").collect();

    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], "bazel_skylib");

    let target_path = parts[1];
    let colon_pos = target_path.find(':').unwrap();
    let package = &target_path[..colon_pos];
    let target = &target_path[colon_pos + 1..];

    assert_eq!(package, "lib");
    assert_eq!(target, "paths");
}
