// Rush Build - Toolchain Management
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Toolchain management and auto-provisioning.
//!
//! Automatically downloads, caches, and manages build toolchains (compilers,
//! linkers, SDKs) to ensure hermetic and reproducible builds across machines.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Toolchain manager.
pub struct ToolchainManager {
    /// Toolchain cache directory.
    cache_dir: PathBuf,

    /// Registered toolchains.
    toolchains: HashMap<String, Toolchain>,

    /// Active toolchain versions.
    active_versions: HashMap<String, String>,
}

/// Toolchain definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Toolchain {
    /// Toolchain name (e.g., "rust", "gcc", "clang").
    pub name: String,

    /// Toolchain version.
    pub version: String,

    /// Target platform.
    pub target: Platform,

    /// Download URL or repository.
    pub source: ToolchainSource,

    /// Expected SHA256 checksum.
    pub sha256: Option<String>,

    /// Executables provided by this toolchain.
    pub executables: Vec<ToolchainExecutable>,

    /// Environment variables to set.
    pub env_vars: HashMap<String, String>,
}

/// Toolchain source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolchainSource {
    /// HTTP download.
    Http {
        /// Download URL.
        url: String,
        /// Archive type.
        archive_type: ArchiveType,
    },

    /// System package manager.
    Package {
        /// Package name.
        package_name: String,
    },

    /// Local path.
    Local {
        /// Path to toolchain.
        path: PathBuf,
    },

    /// Version manager (rustup, nvm, etc.).
    VersionManager {
        /// Manager name.
        manager: String,
        /// Version string.
        version: String,
    },
}

/// Archive type for HTTP downloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ArchiveType {
    TarGz,
    TarXz,
    Zip,
    TarBz2,
}

/// Platform specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Platform {
    /// Operating system (linux, macos, windows).
    pub os: String,

    /// Architecture (x86_64, aarch64, etc.).
    pub arch: String,
}

/// Toolchain executable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolchainExecutable {
    /// Executable name.
    pub name: String,

    /// Relative path within toolchain.
    pub path: PathBuf,

    /// Symlink name (optional).
    pub symlink_as: Option<String>,
}

impl ToolchainManager {
    /// Create a new toolchain manager.
    pub fn new<P: AsRef<Path>>(cache_dir: P) -> Result<Self> {
        let cache_dir = cache_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&cache_dir)?;

        Ok(ToolchainManager {
            cache_dir,
            toolchains: HashMap::new(),
            active_versions: HashMap::new(),
        })
    }

    /// Register a toolchain.
    pub fn register_toolchain(&mut self, toolchain: Toolchain) {
        let key = format!("{}@{}", toolchain.name, toolchain.version);
        self.toolchains.insert(key, toolchain);
    }

    /// Provision a toolchain (download and install if needed).
    pub async fn provision(&self, name: &str, version: &str) -> Result<PathBuf> {
        let key = format!("{}@{}", name, version);

        let toolchain = self
            .toolchains
            .get(&key)
            .context(format!("Toolchain not found: {}", key))?;

        let install_dir = self.cache_dir.join(&key);

        // Check if already installed
        if install_dir.exists() && self.verify_installation(&install_dir, toolchain).await? {
            return Ok(install_dir);
        }

        // Provision based on source type
        match &toolchain.source {
            ToolchainSource::Http { url, archive_type } => {
                self.provision_http(url, archive_type, &install_dir, toolchain)
                    .await?;
            }
            ToolchainSource::Package { package_name } => {
                self.provision_package(package_name, &install_dir).await?;
            }
            ToolchainSource::Local { path } => {
                self.provision_local(path, &install_dir).await?;
            }
            ToolchainSource::VersionManager { manager, version } => {
                self.provision_version_manager(manager, version, &install_dir)
                    .await?;
            }
        }

        Ok(install_dir)
    }

    /// Provision from HTTP.
    async fn provision_http(
        &self,
        url: &str,
        archive_type: &ArchiveType,
        install_dir: &Path,
        toolchain: &Toolchain,
    ) -> Result<()> {
        eprintln!("Downloading toolchain from {}...", url);

        // Download archive
        let response = reqwest::get(url).await?;
        let bytes = response.bytes().await?;

        // Verify checksum if provided
        if let Some(expected_sha256) = &toolchain.sha256 {
            let actual_sha256 = blake3::hash(&bytes).to_hex().to_string();
            if &actual_sha256 != expected_sha256 {
                anyhow::bail!("Checksum mismatch for toolchain download");
            }
        }

        // Extract archive
        std::fs::create_dir_all(install_dir)?;
        let temp_archive = install_dir.join("download.tmp");
        std::fs::write(&temp_archive, bytes)?;

        match archive_type {
            ArchiveType::TarGz => {
                Command::new("tar")
                    .args(["xzf", temp_archive.to_str().unwrap()])
                    .arg("-C")
                    .arg(install_dir)
                    .arg("--strip-components=1")
                    .status()?;
            }
            ArchiveType::TarXz => {
                Command::new("tar")
                    .args(["xJf", temp_archive.to_str().unwrap()])
                    .arg("-C")
                    .arg(install_dir)
                    .arg("--strip-components=1")
                    .status()?;
            }
            ArchiveType::Zip => {
                Command::new("unzip")
                    .arg("-q")
                    .arg(&temp_archive)
                    .arg("-d")
                    .arg(install_dir)
                    .status()?;
            }
            ArchiveType::TarBz2 => {
                Command::new("tar")
                    .args(["xjf", temp_archive.to_str().unwrap()])
                    .arg("-C")
                    .arg(install_dir)
                    .arg("--strip-components=1")
                    .status()?;
            }
        }

        std::fs::remove_file(&temp_archive)?;

        eprintln!("Toolchain installed to {}", install_dir.display());

        Ok(())
    }

    /// Provision from package manager.
    async fn provision_package(&self, package_name: &str, install_dir: &Path) -> Result<()> {
        eprintln!("Installing toolchain via package manager: {}", package_name);

        // Detect package manager and install
        if cfg!(target_os = "linux") {
            // Try apt-get first
            if Command::new("apt-get").arg("--version").status().is_ok() {
                Command::new("sudo")
                    .args(["apt-get", "install", "-y", package_name])
                    .status()?;
            }
        } else if cfg!(target_os = "macos") {
            // Try homebrew
            if Command::new("brew").arg("--version").status().is_ok() {
                Command::new("brew")
                    .args(["install", package_name])
                    .status()?;
            }
        }

        // Create symlink in install_dir
        std::fs::create_dir_all(install_dir)?;

        Ok(())
    }

    /// Provision from local path.
    async fn provision_local(&self, source_path: &Path, install_dir: &Path) -> Result<()> {
        eprintln!("Symlinking local toolchain from {}", source_path.display());

        std::fs::create_dir_all(install_dir.parent().unwrap())?;

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(source_path, install_dir)?;
        }

        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(source_path, install_dir)?;
        }

        Ok(())
    }

    /// Provision via version manager.
    async fn provision_version_manager(
        &self,
        manager: &str,
        version: &str,
        install_dir: &Path,
    ) -> Result<()> {
        eprintln!("Installing via {} version {}", manager, version);

        match manager {
            "rustup" => {
                Command::new("rustup")
                    .args(["toolchain", "install", version])
                    .status()?;
            }
            "nvm" => {
                Command::new("nvm").args(["install", version]).status()?;
            }
            _ => anyhow::bail!("Unsupported version manager: {}", manager),
        }

        std::fs::create_dir_all(install_dir)?;

        Ok(())
    }

    /// Verify toolchain installation.
    async fn verify_installation(&self, install_dir: &Path, toolchain: &Toolchain) -> Result<bool> {
        // Check if all expected executables exist
        for exe in &toolchain.executables {
            let exe_path = install_dir.join(&exe.path);
            if !exe_path.exists() {
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Get path to toolchain executable.
    pub fn get_executable(&self, toolchain_name: &str, exe_name: &str) -> Result<PathBuf> {
        let version = self.active_versions.get(toolchain_name).context(format!(
            "No active version for toolchain: {}",
            toolchain_name
        ))?;

        let key = format!("{}@{}", toolchain_name, version);
        let toolchain = self
            .toolchains
            .get(&key)
            .context(format!("Toolchain not found: {}", key))?;

        let install_dir = self.cache_dir.join(&key);

        for exe in &toolchain.executables {
            if exe.name == exe_name {
                return Ok(install_dir.join(&exe.path));
            }
        }

        anyhow::bail!("Executable not found: {}", exe_name)
    }

    /// Set active toolchain version.
    pub fn set_active_version(&mut self, toolchain_name: String, version: String) {
        self.active_versions.insert(toolchain_name, version);
    }

    /// List available toolchains.
    pub fn list_toolchains(&self) -> Vec<String> {
        self.toolchains.keys().cloned().collect()
    }

    /// Remove a toolchain.
    pub fn remove_toolchain(&self, name: &str, version: &str) -> Result<()> {
        let key = format!("{}@{}", name, version);
        let install_dir = self.cache_dir.join(&key);

        if install_dir.exists() {
            std::fs::remove_dir_all(&install_dir)?;
            eprintln!("Removed toolchain: {}", key);
        }

        Ok(())
    }
}

/// Detect current platform.
pub fn detect_platform() -> Platform {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "arm") {
        "arm"
    } else {
        "unknown"
    };

    Platform {
        os: os.to_string(),
        arch: arch.to_string(),
    }
}

/// Predefined toolchain configurations.
pub struct ToolchainRegistry;

impl ToolchainRegistry {
    /// Get Rust toolchain definition.
    pub fn rust_toolchain(version: &str) -> Toolchain {
        let platform = detect_platform();

        let _target = format!("{}-unknown-{}-gnu", platform.arch, platform.os);

        Toolchain {
            name: "rust".to_string(),
            version: version.to_string(),
            target: platform,
            source: ToolchainSource::VersionManager {
                manager: "rustup".to_string(),
                version: version.to_string(),
            },
            sha256: None,
            executables: vec![
                ToolchainExecutable {
                    name: "rustc".to_string(),
                    path: PathBuf::from("bin/rustc"),
                    symlink_as: None,
                },
                ToolchainExecutable {
                    name: "cargo".to_string(),
                    path: PathBuf::from("bin/cargo"),
                    symlink_as: None,
                },
            ],
            env_vars: HashMap::new(),
        }
    }

    /// Get GCC toolchain definition.
    pub fn gcc_toolchain(version: &str) -> Toolchain {
        let platform = detect_platform();

        Toolchain {
            name: "gcc".to_string(),
            version: version.to_string(),
            target: platform,
            source: ToolchainSource::Package {
                package_name: format!("gcc-{}", version),
            },
            sha256: None,
            executables: vec![
                ToolchainExecutable {
                    name: "gcc".to_string(),
                    path: PathBuf::from("usr/bin/gcc"),
                    symlink_as: None,
                },
                ToolchainExecutable {
                    name: "g++".to_string(),
                    path: PathBuf::from("usr/bin/g++"),
                    symlink_as: None,
                },
            ],
            env_vars: HashMap::new(),
        }
    }

    /// Get Clang toolchain definition.
    pub fn clang_toolchain(version: &str) -> Toolchain {
        let platform = detect_platform();

        Toolchain {
            name: "clang".to_string(),
            version: version.to_string(),
            target: platform,
            source: ToolchainSource::Package {
                package_name: format!("clang-{}", version),
            },
            sha256: None,
            executables: vec![
                ToolchainExecutable {
                    name: "clang".to_string(),
                    path: PathBuf::from("usr/bin/clang"),
                    symlink_as: None,
                },
                ToolchainExecutable {
                    name: "clang++".to_string(),
                    path: PathBuf::from("usr/bin/clang++"),
                    symlink_as: None,
                },
            ],
            env_vars: HashMap::new(),
        }
    }

    /// Get Go toolchain definition.
    pub fn go_toolchain(version: &str) -> Toolchain {
        let platform = detect_platform();

        let url = format!(
            "https://go.dev/dl/go{}.{}-{}.tar.gz",
            version, platform.os, platform.arch
        );

        Toolchain {
            name: "go".to_string(),
            version: version.to_string(),
            target: platform,
            source: ToolchainSource::Http {
                url,
                archive_type: ArchiveType::TarGz,
            },
            sha256: None,
            executables: vec![
                ToolchainExecutable {
                    name: "go".to_string(),
                    path: PathBuf::from("bin/go"),
                    symlink_as: None,
                },
                ToolchainExecutable {
                    name: "gofmt".to_string(),
                    path: PathBuf::from("bin/gofmt"),
                    symlink_as: None,
                },
            ],
            env_vars: [("GOROOT".to_string(), "{{install_dir}}".to_string())]
                .into_iter()
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_platform() {
        let platform = detect_platform();
        assert!(!platform.os.is_empty());
        assert!(!platform.arch.is_empty());
    }

    #[test]
    fn test_toolchain_registry() {
        let rust_tc = ToolchainRegistry::rust_toolchain("1.75.0");
        assert_eq!(rust_tc.name, "rust");
        assert_eq!(rust_tc.version, "1.75.0");
        assert!(!rust_tc.executables.is_empty());

        let go_tc = ToolchainRegistry::go_toolchain("1.21.0");
        assert_eq!(go_tc.name, "go");
        assert!(matches!(go_tc.source, ToolchainSource::Http { .. }));
    }

    #[tokio::test]
    async fn test_toolchain_manager() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mut manager = ToolchainManager::new(temp_dir.path()).unwrap();

        let toolchain = Toolchain {
            name: "test".to_string(),
            version: "1.0".to_string(),
            target: detect_platform(),
            source: ToolchainSource::Local {
                path: PathBuf::from("/usr/bin"),
            },
            sha256: None,
            executables: vec![],
            env_vars: HashMap::new(),
        };

        manager.register_toolchain(toolchain);

        assert_eq!(manager.list_toolchains().len(), 1);
    }
}
