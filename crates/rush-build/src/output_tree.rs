// Output tree management
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Output tree organization for build artifacts.
//!
//! Manages the `rush-out/` directory structure, keeping build outputs
//! separate from source files. Mirrors Bazel's `bazel-out/` approach.

use crate::cache_paths;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Configuration for output organization
#[derive(Debug, Clone)]
pub enum BuildConfig {
    /// Fast build (default)
    Fastbuild,

    /// Optimized build
    Opt,

    /// Debug build
    Debug,
}

impl BuildConfig {
    /// Get the configuration directory name
    pub fn dir_name(&self) -> &str {
        match self {
            BuildConfig::Fastbuild => "fastbuild",
            BuildConfig::Opt => "opt",
            BuildConfig::Debug => "dbg",
        }
    }

    /// Get compiler flags for this build configuration
    pub fn compiler_flags(&self) -> Vec<String> {
        match self {
            BuildConfig::Fastbuild => {
                // Fast build: minimal optimization, fast compilation
                vec![
                    "-O0".to_string(),
                    "-g".to_string(),
                    "-std=c++17".to_string(),
                ]
            }
            BuildConfig::Opt => {
                // Optimized build: match Bazel's default opt level.
                vec![
                    "-O2".to_string(),
                    "-DNDEBUG".to_string(),
                    "-ffunction-sections".to_string(),
                    "-fdata-sections".to_string(),
                    "-std=c++17".to_string(),
                ]
            }
            BuildConfig::Debug => {
                // Debug build: full debug info, no optimization
                vec![
                    "-O0".to_string(),
                    "-g3".to_string(),
                    "-ggdb".to_string(),
                    "-fno-omit-frame-pointer".to_string(),
                    "-std=c++17".to_string(),
                ]
            }
        }
    }

    /// Parse from string (for CLI)
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "fastbuild" => Some(BuildConfig::Fastbuild),
            "opt" => Some(BuildConfig::Opt),
            "dbg" | "debug" => Some(BuildConfig::Debug),
            _ => None,
        }
    }
}

/// Output tree manager
pub struct OutputTree {
    /// Workspace root
    workspace_root: PathBuf,

    /// Output root (rush-out or override)
    output_root: PathBuf,

    /// Build configuration
    config: BuildConfig,
}

impl OutputTree {
    /// Create a new output tree manager
    pub fn new(workspace_root: PathBuf, config: BuildConfig) -> Self {
        let output_root = cache_paths::workspace_output_root(&workspace_root);
        Self {
            workspace_root,
            output_root,
            config,
        }
    }

    /// Get the build configuration
    pub fn config(&self) -> &BuildConfig {
        &self.config
    }

    /// Get the output tree root directory
    /// Returns: workspace_root/rush-out/
    pub fn output_root(&self) -> PathBuf {
        self.output_root.clone()
    }

    /// Get the configuration directory
    /// Returns: workspace_root/rush-out/k8-fastbuild/
    pub fn config_dir(&self) -> PathBuf {
        self.output_root()
            .join(format!("k8-{}", self.config.dir_name()))
    }

    /// Get the bin directory for a package
    /// Returns: workspace_root/rush-out/k8-fastbuild/bin/package/
    pub fn bin_dir(&self, package: &str) -> PathBuf {
        self.config_dir().join("bin").join(package)
    }

    /// Get the obj directory for a package
    /// Returns: workspace_root/rush-out/k8-fastbuild/obj/package/
    pub fn obj_dir(&self, package: &str) -> PathBuf {
        self.config_dir().join("obj").join(package)
    }

    /// Get the genfiles directory for a package
    /// Returns: workspace_root/rush-out/k8-fastbuild/genfiles/package/
    pub fn genfiles_dir(&self, package: &str) -> PathBuf {
        self.config_dir().join("genfiles").join(package)
    }

    /// Get the lib directory for a package
    /// Returns: workspace_root/rush-out/k8-fastbuild/lib/package/
    pub fn lib_dir(&self, package: &str) -> PathBuf {
        self.config_dir().join("lib").join(package)
    }

    /// Get the per-target virtual-includes directory for Bazel's
    /// `include_prefix` / `strip_include_prefix` header remapping.
    ///
    /// Mirrors Bazel, which materializes a `_virtual_includes/<target>/`
    /// tree under the bin dir and adds that root to the quote-include
    /// search path. The headers are laid out under this root at their
    /// remapped include path (e.g. `third_party/farmhash_gpu/src/foo.h`),
    /// so a consumer that does `#include "third_party/farmhash_gpu/src/foo.h"`
    /// resolves against `-I<this dir>`.
    ///
    /// Returns: workspace_root/rush-out/k8-fastbuild/_virtual_includes/package/target/
    pub fn virtual_includes_dir(&self, package: &str, target: &str) -> PathBuf {
        self.config_dir()
            .join("_virtual_includes")
            .join(package)
            .join(target)
    }

    /// Get the path for an object file
    /// Returns: rush-out/k8-fastbuild/obj/package/filename.o
    pub fn obj_path(&self, package: &str, filename: &str) -> PathBuf {
        self.obj_dir(package).join(filename)
    }

    /// Get the path for an executable
    /// Returns: rush-out/k8-fastbuild/bin/package/executable
    pub fn bin_path(&self, package: &str, name: &str) -> PathBuf {
        self.bin_dir(package).join(name)
    }

    /// Get the path for a library archive
    /// Returns: rush-out/k8-fastbuild/lib/package/libname.a
    pub fn lib_path(&self, package: &str, name: &str) -> PathBuf {
        self.lib_dir(package).join(name)
    }

    /// Ensure output directories exist for a package
    pub fn ensure_dirs(&self, package: &str) -> Result<()> {
        std::fs::create_dir_all(self.bin_dir(package))?;
        std::fs::create_dir_all(self.obj_dir(package))?;
        std::fs::create_dir_all(self.lib_dir(package))?;
        Ok(())
    }

    /// Get relative path from workspace root
    pub fn relative_path(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.workspace_root)
            .unwrap_or(path)
            .to_path_buf()
    }

    /// Create a symlink to the output for easy access
    /// Creates: workspace_root/rush-bin -> rush-out/k8-fastbuild/bin/
    pub fn create_convenience_symlinks(&self) -> Result<()> {
        let bin_link = cache_paths::workspace_bin_symlink(&self.workspace_root);
        let bin_target = self.config_dir().join("bin");

        // Remove existing symlink if present
        if bin_link.exists() || bin_link.symlink_metadata().is_ok() {
            std::fs::remove_file(&bin_link).ok();
        }

        // Create symlink
        #[cfg(unix)]
        std::os::unix::fs::symlink(&bin_target, &bin_link)?;

        Ok(())
    }
}

impl Default for OutputTree {
    fn default() -> Self {
        Self::new(PathBuf::from("."), BuildConfig::Fastbuild)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_output_paths() {
        let tree = OutputTree::new(PathBuf::from("/workspace"), BuildConfig::Fastbuild);

        assert_eq!(tree.output_root(), PathBuf::from("/workspace/rush-out"));

        assert_eq!(
            tree.config_dir(),
            PathBuf::from("/workspace/rush-out/k8-fastbuild")
        );

        assert_eq!(
            tree.bin_dir("examples/hello"),
            PathBuf::from("/workspace/rush-out/k8-fastbuild/bin/examples/hello")
        );

        assert_eq!(
            tree.obj_path("examples/hello", "main.o"),
            PathBuf::from("/workspace/rush-out/k8-fastbuild/obj/examples/hello/main.o")
        );

        assert_eq!(
            tree.bin_path("examples/hello", "hello"),
            PathBuf::from("/workspace/rush-out/k8-fastbuild/bin/examples/hello/hello")
        );
    }

    #[test]
    fn test_build_configs() {
        assert_eq!(BuildConfig::Fastbuild.dir_name(), "fastbuild");
        assert_eq!(BuildConfig::Opt.dir_name(), "opt");
        assert_eq!(BuildConfig::Debug.dir_name(), "dbg");
    }
}
