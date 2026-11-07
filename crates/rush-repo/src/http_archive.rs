// Rush Repo - HTTP Archive Support
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! HTTP archive fetching and extraction.
//!
//! Downloads and caches external dependencies from HTTP(S) URLs.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// HTTP archive rule for external dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpArchive {
    /// Name of the repository.
    pub name: String,

    /// Download URL.
    pub url: String,

    /// Expected SHA256 hash (for verification).
    pub sha256: Option<String>,

    /// Strip prefix from archive paths.
    pub strip_prefix: Option<String>,

    /// Type of archive (auto-detected if None).
    pub type_hint: Option<ArchiveType>,

    /// BUILD file content to write.
    pub build_file_content: Option<String>,

    /// Path to BUILD file to copy.
    pub build_file: Option<PathBuf>,
}

/// Archive type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveType {
    /// tar.gz or .tgz
    TarGz,
    /// .tar.xz
    TarXz,
    /// .tar.bz2
    TarBz2,
    /// .zip
    Zip,
    /// .tar
    Tar,
}

impl ArchiveType {
    /// Detect archive type from URL.
    pub fn from_url(url: &str) -> Option<Self> {
        if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
            Some(ArchiveType::TarGz)
        } else if url.ends_with(".tar.xz") {
            Some(ArchiveType::TarXz)
        } else if url.ends_with(".tar.bz2") || url.ends_with(".tbz2") {
            Some(ArchiveType::TarBz2)
        } else if url.ends_with(".zip") {
            Some(ArchiveType::Zip)
        } else if url.ends_with(".tar") {
            Some(ArchiveType::Tar)
        } else {
            None
        }
    }

    /// Get file extension.
    pub fn extension(&self) -> &'static str {
        match self {
            ArchiveType::TarGz => "tar.gz",
            ArchiveType::TarXz => "tar.xz",
            ArchiveType::TarBz2 => "tar.bz2",
            ArchiveType::Zip => "zip",
            ArchiveType::Tar => "tar",
        }
    }
}

/// HTTP archive manager.
pub struct HttpArchiveManager {
    /// Cache directory for downloaded archives.
    cache_dir: PathBuf,

    /// External repositories directory.
    external_dir: PathBuf,
}

impl HttpArchiveManager {
    /// Create a new HTTP archive manager.
    pub fn new(workspace_root: &Path) -> Result<Self> {
        let cache_dir = workspace_root.join(".rush-cache").join("archives");
        let external_dir = workspace_root.join("external");

        fs::create_dir_all(&cache_dir)?;
        fs::create_dir_all(&external_dir)?;

        Ok(HttpArchiveManager {
            cache_dir,
            external_dir,
        })
    }

    /// Fetch and extract an HTTP archive.
    pub async fn fetch(&self, archive: &HttpArchive) -> Result<PathBuf> {
        // Check if already extracted and valid
        let repo_dir = self.external_dir.join(&archive.name);
        if repo_dir.exists() {
            // Validate by checking for BUILD or BUILD.bazel file
            let has_build = repo_dir.join("BUILD").exists()
                || repo_dir.join("BUILD.bazel").exists()
                || repo_dir.join("WORKSPACE").exists();

            if has_build {
                println!("      ✓ {}: {}", archive.name, repo_dir.display());
                return Ok(repo_dir);
            } else {
                // Incomplete fetch - clean up and retry
                println!("  Removing incomplete repository: {}", archive.name);
                fs::remove_dir_all(&repo_dir)?;
            }
        }

        println!("📦 Fetching {} from {}", archive.name, archive.url);

        // Determine archive type
        let archive_type = archive
            .type_hint
            .or_else(|| ArchiveType::from_url(&archive.url))
            .ok_or_else(|| anyhow::anyhow!("Cannot determine archive type for {}", archive.url))?;

        // Download archive
        let archive_path = self
            .download(&archive.url, &archive.name, archive_type)
            .await?;

        // Verify SHA256 if provided
        if let Some(expected_sha256) = &archive.sha256 {
            self.verify_sha256(&archive_path, expected_sha256)?;
        }

        // Extract archive
        let extract_dir = self.extract(&archive_path, archive_type)?;

        // Handle strip_prefix
        let final_dir = if let Some(prefix) = &archive.strip_prefix {
            let prefixed_dir = extract_dir.join(prefix);
            if !prefixed_dir.exists() {
                anyhow::bail!("Strip prefix {} not found in archive", prefix);
            }

            // Ensure repo_dir doesn't exist before moving
            if repo_dir.exists() {
                fs::remove_dir_all(&repo_dir)?;
            }

            // Move contents to repo_dir
            fs::create_dir_all(&repo_dir)?;
            Self::move_dir_contents(&prefixed_dir, &repo_dir)?;

            // Clean up extraction directory
            let _ = fs::remove_dir_all(&extract_dir);

            repo_dir.clone()
        } else {
            // Ensure repo_dir doesn't exist before renaming
            if repo_dir.exists() {
                fs::remove_dir_all(&repo_dir)?;
            }

            // Rename extraction directory to repo name
            fs::rename(&extract_dir, &repo_dir)?;
            repo_dir.clone()
        };

        // Add BUILD file if specified
        if let Some(content) = &archive.build_file_content {
            let build_path = final_dir.join("BUILD");
            fs::write(build_path, content)?;
        } else if let Some(build_file) = &archive.build_file {
            let build_path = final_dir.join("BUILD");
            fs::copy(build_file, build_path)?;
        }

        println!("✓ Extracted {} to {}", archive.name, final_dir.display());

        Ok(final_dir)
    }

    /// Download archive from URL.
    async fn download(&self, url: &str, name: &str, archive_type: ArchiveType) -> Result<PathBuf> {
        let filename = format!("{}.{}", name, archive_type.extension());
        let archive_path = self.cache_dir.join(&filename);

        // Check cache
        if archive_path.exists() {
            println!("  Using cached archive: {}", archive_path.display());
            return Ok(archive_path);
        }

        // Check distdir(s) if provided via env RUSH_DISTDIR (colon-separated)
        if let Ok(dist) = std::env::var("RUSH_DISTDIR") {
            for dir in dist.split(':') {
                let candidate = std::path::Path::new(dir).join(&filename);
                if candidate.exists() {
                    println!("  Using distdir archive: {}", candidate.display());
                    std::fs::copy(&candidate, &archive_path)?;
                    return Ok(archive_path);
                }
            }
        }

        println!("  Downloading...");

        // Download with reqwest
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()?;

        let response = client.get(url).send().await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to download {}: {}", url, response.status());
        }

        let bytes = response.bytes().await?;

        // Write to cache
        fs::write(&archive_path, bytes)?;

        println!("  Downloaded {} bytes", fs::metadata(&archive_path)?.len());

        Ok(archive_path)
    }

    /// Download a single file into the cache directory with the provided filename.
    pub async fn download_file(&self, url: &str, filename: &str) -> Result<PathBuf> {
        let out_path = self.cache_dir.join(filename);

        // Check cache
        if out_path.exists() {
            println!("  Using cached file: {}", out_path.display());
            return Ok(out_path);
        }

        // Check distdir(s) (RUSH_DISTDIR) for offline artifact
        if let Ok(dist) = std::env::var("RUSH_DISTDIR") {
            for dir in dist.split(':') {
                let candidate = std::path::Path::new(dir).join(filename);
                if candidate.exists() {
                    println!("  Using distdir file: {}", candidate.display());
                    std::fs::copy(&candidate, &out_path)?;
                    return Ok(out_path);
                }
            }
        }

        println!("  Downloading {}", url);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()?;
        let response = client.get(url).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("Failed to download {}: {}", url, response.status());
        }
        let bytes = response.bytes().await?;
        std::fs::write(&out_path, bytes)?;
        Ok(out_path)
    }

    /// Verify SHA256 hash.
    fn verify_sha256(&self, path: &Path, expected: &str) -> Result<()> {
        use sha2::{Digest, Sha256};

        println!("  Verifying SHA256 checksum...");

        let data = fs::read(path)?;
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let result = hasher.finalize();
        let actual = hex::encode(result);

        if actual != expected {
            anyhow::bail!(
                "SHA256 mismatch!\n  Expected: {}\n  Got:      {}",
                expected,
                actual
            );
        }

        println!("  ✓ SHA256 verified");
        Ok(())
    }

    /// Extract archive.
    fn extract(&self, archive_path: &Path, archive_type: ArchiveType) -> Result<PathBuf> {
        let extract_dir = self.cache_dir.join(format!(
            "extract_{}",
            archive_path.file_stem().unwrap().to_string_lossy()
        ));

        // Remove existing extraction directory
        if extract_dir.exists() {
            fs::remove_dir_all(&extract_dir)?;
        }

        fs::create_dir_all(&extract_dir)?;

        println!("  Extracting...");

        match archive_type {
            ArchiveType::TarGz => {
                Command::new("tar")
                    .args([
                        "xzf",
                        archive_path.to_str().unwrap(),
                        "-C",
                        extract_dir.to_str().unwrap(),
                    ])
                    .status()?;
            }
            ArchiveType::TarXz => {
                Command::new("tar")
                    .args([
                        "xJf",
                        archive_path.to_str().unwrap(),
                        "-C",
                        extract_dir.to_str().unwrap(),
                    ])
                    .status()?;
            }
            ArchiveType::TarBz2 => {
                Command::new("tar")
                    .args([
                        "xjf",
                        archive_path.to_str().unwrap(),
                        "-C",
                        extract_dir.to_str().unwrap(),
                    ])
                    .status()?;
            }
            ArchiveType::Tar => {
                Command::new("tar")
                    .args([
                        "xf",
                        archive_path.to_str().unwrap(),
                        "-C",
                        extract_dir.to_str().unwrap(),
                    ])
                    .status()?;
            }
            ArchiveType::Zip => {
                Command::new("unzip")
                    .args([
                        "-q",
                        archive_path.to_str().unwrap(),
                        "-d",
                        extract_dir.to_str().unwrap(),
                    ])
                    .status()?;
            }
        }

        Ok(extract_dir)
    }

    /// Move directory contents.
    fn move_dir_contents(src: &Path, dst: &Path) -> Result<()> {
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());

            if src_path.is_dir() {
                fs::create_dir_all(&dst_path)?;
                Self::move_dir_contents(&src_path, &dst_path)?;
            } else {
                fs::copy(&src_path, &dst_path)?;
            }
        }
        Ok(())
    }

    /// Get external directory.
    pub fn external_dir(&self) -> &Path {
        &self.external_dir
    }

    /// Clear cache.
    pub fn clear_cache(&self) -> Result<usize> {
        let mut removed = 0;

        for entry in fs::read_dir(&self.cache_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
                removed += 1;
            } else if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
                removed += 1;
            }
        }

        Ok(removed)
    }

    /// List cached archives.
    pub fn list_cached(&self) -> Result<Vec<String>> {
        let mut archives = Vec::new();

        for entry in fs::read_dir(&self.cache_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                archives.push(entry.file_name().to_string_lossy().to_string());
            }
        }

        Ok(archives)
    }

    /// List fetched repositories.
    pub fn list_repos(&self) -> Result<Vec<String>> {
        let mut repos = Vec::new();

        for entry in fs::read_dir(&self.external_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                repos.push(entry.file_name().to_string_lossy().to_string());
            }
        }

        Ok(repos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_archive_type_detection() {
        assert_eq!(
            ArchiveType::from_url("https://example.com/file.tar.gz"),
            Some(ArchiveType::TarGz)
        );

        assert_eq!(
            ArchiveType::from_url("https://example.com/file.zip"),
            Some(ArchiveType::Zip)
        );

        assert_eq!(
            ArchiveType::from_url("https://example.com/file.tar.xz"),
            Some(ArchiveType::TarXz)
        );
    }

    #[test]
    fn test_archive_extension() {
        assert_eq!(ArchiveType::TarGz.extension(), "tar.gz");
        assert_eq!(ArchiveType::Zip.extension(), "zip");
        assert_eq!(ArchiveType::TarXz.extension(), "tar.xz");
    }
}
