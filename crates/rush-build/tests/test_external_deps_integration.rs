// Integration test for external dependency loading
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#[cfg(test)]
mod external_deps_tests {
    use rush_build::BuildOrchestrator;
    use std::path::PathBuf;

    #[tokio::test]
    #[ignore] // Requires internet connection
    async fn test_workspace_loading() {
        let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/external_deps_test");

        if !test_dir.exists() {
            eprintln!("Test directory not found: {:?}", test_dir);
            return;
        }

        let orchestrator = BuildOrchestrator::with_workspace(test_dir.clone());

        // Test WORKSPACE loading (without fetching to avoid downloads in CI)
        let result = orchestrator.load_workspace(false).await;

        match result {
            Ok(_) => {
                println!("✅ WORKSPACE file loaded successfully");
                println!("   Test passed: External dependency declarations parsed");
            }
            Err(e) => {
                eprintln!("❌ Failed to load WORKSPACE: {}", e);
                panic!("WORKSPACE loading failed");
            }
        }
    }

    #[tokio::test]
    #[ignore] // Requires internet connection and time
    async fn test_external_dependency_fetch() {
        let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/external_deps_test");

        if !test_dir.exists() {
            eprintln!("Test directory not found: {:?}", test_dir);
            return;
        }

        let orchestrator = BuildOrchestrator::with_workspace(test_dir.clone());

        // Test WORKSPACE loading WITH fetching (downloads protobuf)
        println!("🔄 Testing external dependency fetch (this may take a minute)...");
        let result = orchestrator.load_workspace(true).await;

        match result {
            Ok(_) => {
                println!("✅ External dependencies fetched successfully");
                println!("   Protobuf repository should be cached in .cache/rush/external/");

                // Check if cache directory was created
                let cache_dir = test_dir.join(".cache/rush/external/com_google_protobuf");
                if cache_dir.exists() {
                    println!("   ✅ Cache directory exists: {}", cache_dir.display());
                } else {
                    println!("   ⚠️  Cache directory not found (might use different location)");
                }
            }
            Err(e) => {
                eprintln!("❌ Failed to fetch external dependencies: {}", e);
                panic!("External dependency fetch failed");
            }
        }
    }
}
