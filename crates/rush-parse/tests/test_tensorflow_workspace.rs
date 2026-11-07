// Test parsing TensorFlow's WORKSPACE file

use rush_parse::parse_workspace_file;
use std::path::{Path, PathBuf};

#[test]
fn test_parse_tensorflow_workspace() {
    // Use TF_PATH env var, or default to ../tensorflow relative to repo root
    let tf_path = std::env::var("TF_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            // Assume we're running from rush repo root during tests
            PathBuf::from("../tensorflow")
        });

    let tf_workspace = tf_path.join("WORKSPACE");

    if !tf_workspace.exists() {
        // Skip test if TensorFlow repo not available
        println!(
            "Skipping test: TensorFlow WORKSPACE not found at {}",
            tf_workspace.display()
        );
        println!("Set TF_PATH environment variable or clone TensorFlow to ../tensorflow");
        return;
    }

    let ws = parse_workspace_file(&tf_workspace).expect("Failed to parse TensorFlow WORKSPACE");

    // TensorFlow should have workspace name
    assert_eq!(ws.name, Some("org_tensorflow".to_string()));

    // Should have multiple http_archive declarations
    assert!(
        !ws.http_archives.is_empty(),
        "Expected http_archive declarations"
    );

    // Should have load statements
    assert!(!ws.load_statements.is_empty(), "Expected load statements");

    // Check for specific known archives
    let has_bazel_skylib = ws.http_archives.iter().any(|a| a.name == "bazel_skylib");
    assert!(has_bazel_skylib, "Expected bazel_skylib archive");

    println!("✅ Successfully parsed TensorFlow WORKSPACE:");
    println!("   - {} http_archive entries", ws.http_archives.len());
    println!("   - {} git_repository entries", ws.git_repositories.len());
    println!("   - {} load statements", ws.load_statements.len());
}
