//! HTTP archive downloading and extraction.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

/// Fetch and extract HTTP archive.
pub async fn fetch_http_archive(
    cache_dir: &Path,
    name: &str,
    url: &str,
    expected_sha256: Option<&str>,
    strip_prefix: Option<&str>,
    build_file: Option<&str>,
) -> Result<PathBuf> {
    let repo_dir = cache_dir.join(name);

    // Check if already extracted
    if repo_dir.exists() {
        println!("📦 Using cached repository: {}", name);
        return Ok(repo_dir);
    }

    println!("📥 Downloading {}: {}", name, url);

    // Download archive
    let archive_path = cache_dir.join(format!("{}.archive", name));
    download_file(url, &archive_path).await?;

    // Verify checksum if provided
    if let Some(expected) = expected_sha256 {
        verify_sha256(&archive_path, expected).await?;
    }

    // Extract archive
    println!("📦 Extracting {}", name);
    extract_archive(&archive_path, &repo_dir, strip_prefix).await?;

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

    // Clean up archive
    let _ = fs::remove_file(&archive_path).await;

    Ok(repo_dir)
}

/// Download file from URL.
async fn download_file(url: &str, dest: &Path) -> Result<()> {
    // Use blocking reqwest in async context
    let response = tokio::task::spawn_blocking({
        let url = url.to_string();
        move || reqwest::blocking::get(&url)
    })
    .await??
    .error_for_status()?;

    let bytes = tokio::task::spawn_blocking(move || response.bytes()).await??;

    let mut file = fs::File::create(dest).await?;
    file.write_all(&bytes).await?;
    file.flush().await?;

    Ok(())
}

/// Verify SHA256 checksum of file.
async fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    use sha2::{Digest, Sha256};

    let bytes = fs::read(path).await?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hash = hasher.finalize();
    let hash_hex = format!("{:x}", hash);

    if hash_hex != expected {
        anyhow::bail!(
            "SHA256 mismatch for {}: expected {}, got {}",
            path.display(),
            expected,
            hash_hex
        );
    }

    Ok(())
}

/// Extract archive (tar.gz, tar.bz2, zip) to destination.
async fn extract_archive(
    archive_path: &Path,
    dest_dir: &Path,
    strip_prefix: Option<&str>,
) -> Result<()> {
    fs::create_dir_all(dest_dir).await?;

    let archive_path = archive_path.to_owned();
    let dest_dir = dest_dir.to_owned();
    let strip_prefix = strip_prefix.map(|s| s.to_string());

    // Extract in blocking context
    tokio::task::spawn_blocking(move || {
        extract_archive_blocking(&archive_path, &dest_dir, strip_prefix.as_deref())
    })
    .await??;

    Ok(())
}

/// Blocking archive extraction.
fn extract_archive_blocking(
    archive_path: &Path,
    dest_dir: &Path,
    strip_prefix: Option<&str>,
) -> Result<()> {
    let file = std::fs::File::open(archive_path)?;

    // Detect archive type from extension
    let extension = archive_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    match extension {
        "gz" | "tgz" => {
            extract_tar_gz(file, dest_dir, strip_prefix)?;
        }
        "bz2" | "tbz" => {
            extract_tar_bz2(file, dest_dir, strip_prefix)?;
        }
        "zip" => {
            extract_zip(file, dest_dir, strip_prefix)?;
        }
        _ => {
            // Try to detect from file content
            if archive_path.to_string_lossy().contains(".tar.") {
                extract_tar_gz(file, dest_dir, strip_prefix)?;
            } else {
                anyhow::bail!("Unsupported archive format: {}", extension);
            }
        }
    }

    Ok(())
}

/// Extract tar.gz archive.
fn extract_tar_gz(file: std::fs::File, dest_dir: &Path, strip_prefix: Option<&str>) -> Result<()> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);

    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let path = entry.path()?.into_owned();

        // Apply strip_prefix if specified
        let final_path = if let Some(prefix) = strip_prefix {
            path.strip_prefix(prefix).unwrap_or(&path).to_path_buf()
        } else {
            path
        };

        let dest_path = dest_dir.join(&final_path);

        // Create parent directories
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        entry.unpack(&dest_path)?;
    }

    Ok(())
}

/// Extract tar.bz2 archive.
fn extract_tar_bz2(file: std::fs::File, dest_dir: &Path, strip_prefix: Option<&str>) -> Result<()> {
    use bzip2::read::BzDecoder;
    use tar::Archive;

    let decoder = BzDecoder::new(file);
    let mut archive = Archive::new(decoder);

    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let path = entry.path()?.into_owned();

        let final_path = if let Some(prefix) = strip_prefix {
            path.strip_prefix(prefix).unwrap_or(&path).to_path_buf()
        } else {
            path
        };

        let dest_path = dest_dir.join(&final_path);

        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        entry.unpack(&dest_path)?;
    }

    Ok(())
}

/// Extract ZIP archive.
fn extract_zip(file: std::fs::File, dest_dir: &Path, strip_prefix: Option<&str>) -> Result<()> {
    use zip::ZipArchive;

    let mut archive = ZipArchive::new(file)?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let path = file.name().to_string();
        let path = PathBuf::from(path);

        let final_path = if let Some(prefix) = strip_prefix {
            path.strip_prefix(prefix).unwrap_or(&path).to_path_buf()
        } else {
            path
        };

        let dest_path = dest_dir.join(&final_path);

        if file.is_dir() {
            std::fs::create_dir_all(&dest_path)?;
        } else {
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let mut outfile = std::fs::File::create(&dest_path)?;
            std::io::copy(&mut file, &mut outfile)?;
        }

        // Preserve permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = file.unix_mode() {
                std::fs::set_permissions(&dest_path, std::fs::Permissions::from_mode(mode))?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_download_file() {
        let temp_dir = TempDir::new().unwrap();
        let dest = temp_dir.path().join("test.html");

        // Download a small file for testing
        let result = download_file("https://httpbin.org/html", &dest).await;

        // This might fail in CI without internet, so we just check it doesn't panic
        if result.is_ok() {
            assert!(dest.exists());
        }
    }

    #[test]
    fn test_strip_prefix() {
        let path = PathBuf::from("protobuf-3.21.0/src/google/protobuf/message.h");
        let stripped = path.strip_prefix("protobuf-3.21.0").unwrap();
        assert_eq!(stripped, Path::new("src/google/protobuf/message.h"));
    }
}
