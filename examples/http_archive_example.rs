// Rush - HTTP Archive Example
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Example demonstrating HTTP archive repository rule usage.

use rush_repo::HttpArchive;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    // Example: Download zlib from official mirror
    let zlib = HttpArchive::new(
        "zlib".to_string(),
        vec![
            "https://github.com/madler/zlib/releases/download/v1.3/zlib-1.3.tar.gz".to_string(),
            "https://zlib.net/zlib-1.3.tar.gz".to_string(),
        ],
        "ff0ba4c292013dbc27530b3a81e1f9a813cd39de01ca5e0f8bf355702efa593e".to_string(),
        Some("zlib-1.3".to_string()),
    );

    // Download and extract to cache directory
    let cache_dir = std::env::temp_dir().join("rush-cache-example");
    let extracted_path = zlib.download(&cache_dir).await?;

    println!("Successfully downloaded and extracted zlib to:");
    println!("  {}", extracted_path.display());

    // List some files from the extracted archive
    if let Ok(entries) = std::fs::read_dir(&extracted_path) {
        println!("\nExtracted files:");
        for entry in entries.take(10).flatten() {
            println!("  - {}", entry.file_name().to_string_lossy());
        }
    }

    Ok(())
}
