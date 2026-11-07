// Rush Build - System Library Rules
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! System library rule implementations for linking against system-installed libraries.
//!
//! This module provides `system_cc_library` which uses pkg-config for distribution-agnostic
//! detection of system libraries like protobuf, grpc, etc.

use crate::context::BuildContext;
use crate::providers::ProviderSet;
use crate::{extract_string, extract_string_list, Artifact, ArtifactType, BuildRule, RuleInstance, Value};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

/// system_cc_library rule - represents a system-installed C++ library
///
/// Uses pkg-config to detect include paths and link flags in a distribution-agnostic way.
/// Falls back to well-known paths if pkg-config is not available.
///
/// Example:
/// ```python
/// system_cc_library(
///     name = "protobuf",
///     pkg_config_name = "protobuf",  # Name to pass to pkg-config
///     fallback_include_dirs = ["/usr/include"],
///     fallback_link_flags = ["-lprotobuf"],
/// )
/// ```
pub struct SystemCcLibraryRule;

#[async_trait]
impl BuildRule for SystemCcLibraryRule {
    fn name(&self) -> &str {
        "system_cc_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let pkg_config_name = extract_string(attrs, "pkg_config_name")
            .unwrap_or_else(|_| name.clone());
        let fallback_include_dirs = extract_string_list(attrs, "fallback_include_dirs")
            .unwrap_or_default();
        let fallback_link_flags = extract_string_list(attrs, "fallback_link_flags")
            .unwrap_or_default();
        let fallback_header_files = extract_string_list(attrs, "fallback_header_files")
            .unwrap_or_default();
        let deps = extract_string_list(attrs, "deps").unwrap_or_default();

        Ok(Box::new(SystemCcLibraryInstance {
            name,
            pkg_config_name,
            fallback_include_dirs,
            fallback_link_flags,
            fallback_header_files,
            deps,
        }))
    }

    fn documentation(&self) -> &str {
        "System C++ library - uses pkg-config for distribution-agnostic detection"
    }
}

struct SystemCcLibraryInstance {
    name: String,
    pkg_config_name: String,
    fallback_include_dirs: Vec<String>,
    fallback_link_flags: Vec<String>,
    fallback_header_files: Vec<String>,
    deps: Vec<String>,
}

impl SystemCcLibraryInstance {
    /// Query pkg-config for include directories
    fn get_pkg_config_cflags(&self) -> Option<Vec<String>> {
        let output = Command::new("pkg-config")
            .arg("--cflags")
            .arg(&self.pkg_config_name)
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let cflags = String::from_utf8_lossy(&output.stdout);
        Some(
            cflags
                .split_whitespace()
                .map(|s| s.to_string())
                .collect()
        )
    }

    /// Query pkg-config for link flags
    fn get_pkg_config_libs(&self) -> Option<Vec<String>> {
        let output = Command::new("pkg-config")
            .arg("--libs")
            .arg(&self.pkg_config_name)
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let libs = String::from_utf8_lossy(&output.stdout);
        Some(
            libs
                .split_whitespace()
                .map(|s| s.to_string())
                .collect()
        )
    }

    /// Extract include directories from cflags (paths starting with -I)
    fn extract_include_dirs(cflags: &[String]) -> Vec<PathBuf> {
        cflags
            .iter()
            .filter_map(|flag| {
                if flag.starts_with("-I") {
                    Some(PathBuf::from(&flag[2..]))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Get the actual include directories (pkg-config or fallback)
    fn get_include_dirs(&self) -> Vec<PathBuf> {
        if let Some(cflags) = self.get_pkg_config_cflags() {
            let dirs = Self::extract_include_dirs(&cflags);
            if !dirs.is_empty() {
                return dirs;
            }
        }

        // Fallback to configured paths
        self.fallback_include_dirs
            .iter()
            .map(PathBuf::from)
            .collect()
    }

    /// Get the actual link flags (pkg-config or fallback)
    fn get_link_flags(&self) -> Vec<String> {
        if let Some(libs) = self.get_pkg_config_libs() {
            if !libs.is_empty() {
                return libs;
            }
        }

        // Fallback to configured flags
        self.fallback_link_flags.clone()
    }
}

#[async_trait]
impl RuleInstance for SystemCcLibraryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        self.deps.clone()
    }

    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
        // System libraries provide "synthetic" header artifacts pointing to system paths
        // This allows dependent targets to find the headers
        let mut artifacts = Vec::new();
        let include_dirs = self.get_include_dirs();

        // If we have explicit header files, create artifacts for them
        for header_file in &self.fallback_header_files {
            let header_path = PathBuf::from(header_file);
            if header_path.exists() {
                artifacts.push(Artifact {
                    path: header_path,
                    artifact_type: ArtifactType::Header,
                });
            }
        }

        // Also add a representative header from each include dir
        // This helps dependent rules find include paths
        for include_dir in &include_dirs {
            if include_dir.exists() {
                // Create a "marker" artifact for the include directory
                // The path points to the directory itself, which rules can use to derive -I flags
                artifacts.push(Artifact {
                    path: include_dir.clone(),
                    artifact_type: ArtifactType::Header,
                });
            }
        }

        Ok(artifacts)
    }

    /// Phase 5.B.3.e: analyze() is the canonical entry point.
    /// system_cc_library exposes pkg-config-derived include dirs
    /// and link flags; no headers, no static libraries
    /// (system-installed). Include dirs are absolute paths from
    /// pkg-config so resolve_package_include passes them through
    /// unchanged.
    fn analyze(
        &self,
        ctx: &mut crate::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<crate::providers::ProviderSet>],
    ) -> ProviderSet {
        use crate::providers::{CcInfo, DirectCcInfo, LinkFragment};

        let direct_include_dirs: Vec<_> = self
            .get_include_dirs()
            .iter()
            .map(|p| ctx.resolve_package_include(p.as_path()))
            .collect();
        let direct_link_fragments: Vec<LinkFragment> = self
            .get_link_flags()
            .iter()
            .map(|f| LinkFragment::Flag(ctx.intern_string(f)))
            .collect();
        let direct = DirectCcInfo {
            include_dirs: direct_include_dirs,
            link_fragments: direct_link_fragments,
            ..DirectCcInfo::default()
        };
        let cc_info = CcInfo::from_direct_and_deps(direct, crate::providers::extract_cc_infos(dep_providers).into_iter());
        ProviderSet::empty().with_cc_info(cc_info)
    }

    fn rule_type(&self) -> &str {
        "system_cc_library"
    }
}

/// Detect system protobuf installation and return configuration
pub fn detect_system_protobuf() -> SystemProtobufConfig {
    // Try pkg-config first
    if let Ok(output) = Command::new("pkg-config")
        .arg("--exists")
        .arg("protobuf")
        .status()
    {
        if output.success() {
            // Get cflags
            let cflags = Command::new("pkg-config")
                .arg("--cflags")
                .arg("protobuf")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();

            // Get libs
            let libs = Command::new("pkg-config")
                .arg("--libs")
                .arg("protobuf")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();

            let include_dirs: Vec<PathBuf> = cflags
                .split_whitespace()
                .filter_map(|f| f.strip_prefix("-I").map(PathBuf::from))
                .collect();

            let link_flags: Vec<String> = libs
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();

            return SystemProtobufConfig {
                found: true,
                include_dirs: if include_dirs.is_empty() {
                    vec![PathBuf::from("/usr/include")]
                } else {
                    include_dirs
                },
                link_flags: if link_flags.is_empty() {
                    vec!["-lprotobuf".to_string()]
                } else {
                    link_flags
                },
                protoc_path: find_protoc(),
            };
        }
    }

    // Fallback: check common paths
    let common_include_paths = [
        "/usr/include",
        "/usr/local/include",
        "/opt/homebrew/include",  // macOS Homebrew ARM
        "/usr/local/opt/protobuf/include",  // macOS Homebrew Intel
    ];

    for path in &common_include_paths {
        let pb_header = PathBuf::from(path).join("google/protobuf/message.h");
        if pb_header.exists() {
            return SystemProtobufConfig {
                found: true,
                include_dirs: vec![PathBuf::from(path)],
                link_flags: vec!["-lprotobuf".to_string()],
                protoc_path: find_protoc(),
            };
        }
    }

    // Not found
    SystemProtobufConfig {
        found: false,
        include_dirs: Vec::new(),
        link_flags: Vec::new(),
        protoc_path: None,
    }
}

/// Find protoc binary
fn find_protoc() -> Option<PathBuf> {
    // An explicit compiler always wins. Accept a bare command name as well as
    // an existing path so callers can intentionally select a PATH-resolved
    // wrapper.
    for env_name in ["RUSH_PROTOC", "PROTOC"] {
        if let Ok(value) = std::env::var(env_name) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
    }

    // TensorFlow's pinned protobuf bootstrap installs protoc beside the
    // matching headers and static runtime archives. Prefer that compiler over
    // the host protoc so generated C++ and the linked protobuf runtime have
    // identical version semantics.
    let pinned = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/rush-cache/external"))
        .join("com_google_protobuf")
        .join("protoc");
    if pinned.is_file() {
        return Some(pinned);
    }

    let common_paths = [
        "/usr/bin/protoc",
        "/usr/local/bin/protoc",
        "/opt/homebrew/bin/protoc",
    ];

    for path in &common_paths {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }

    // Try PATH
    if let Ok(output) = Command::new("which").arg("protoc").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }

    None
}

/// Include directories for the protobuf RUNTIME headers that a generated
/// `.pb.cc` includes (`google/protobuf/*.h`).
///
/// This must agree with [`protobuf_protoc_path`]: the generated source and
/// the headers it is compiled against carry a version guard, so resolving
/// the compiler from the pinned bootstrap while hardcoding `/usr/include`
/// for the headers fails with "generated by a newer version of protoc" in
/// a file that names neither protobuf nor the target being built.
///
/// Prefers the pinned bootstrap's include root, then whatever the system
/// protobuf detection reports, and only then `/usr/include`.
pub fn protobuf_runtime_include_dirs() -> Vec<PathBuf> {
    let pinned_dir = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/rush-cache/external"))
        .join("com_google_protobuf");

    if pinned_dir.join("libprotobuf.a").is_file() {
        // The bootstrap records the include root it built against; fall back
        // to <dir>/src, which is where protobuf keeps its headers.
        let root = std::fs::read_to_string(pinned_dir.join("INCLUDE_ROOT"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| pinned_dir.join("src"));
        if root.join("google/protobuf/stubs/common.h").is_file() {
            return vec![root];
        }
    }

    let cfg = detect_system_protobuf();
    if cfg.found && !cfg.include_dirs.is_empty() {
        return cfg.include_dirs;
    }
    vec![PathBuf::from("/usr/include")]
}

/// Resolve the protobuf compiler used for generated workspace sources.
///
/// The fallback command preserves the prior diagnostic when no compiler can
/// be discovered: spawning `protoc` fails at the generation site with the
/// operating system error.
pub fn protobuf_protoc_path() -> PathBuf {
    find_protoc().unwrap_or_else(|| PathBuf::from("protoc"))
}

/// Configuration for system protobuf installation
#[derive(Debug)]
pub struct SystemProtobufConfig {
    pub found: bool,
    pub include_dirs: Vec<PathBuf>,
    pub link_flags: Vec<String>,
    pub protoc_path: Option<PathBuf>,
}

/// Link flags for the protobuf runtime, preferring TF's PINNED protobuf 3.21.9
/// static archive when materialized (scripts/bootstrap_protobuf.sh →
/// $RUSH_GLOBAL_CLONE_CACHE/com_google_protobuf/libprotobuf.a), else the system
/// `-lprotobuf`.
///
/// #32: TF 2.15 hard-requires protobuf 3.21.x. The system shared lib on the
/// build host is 4.25.x (libprotobuf.so.32), whose stricter TextFormat parser
/// aborts the op-wrapper codegen tools at static REGISTER_OP (PrintV2's
/// `end: string = '\n'` newline default). Linking the pinned static archive
/// fixes both the codegen tools and the final libtensorflow_cc.so. Gated on the
/// .a existing — HONEST: absent → system protobuf as before. protobuf 3.21 is
/// pre-abseil, so the only extra link deps are zlib + pthread.
pub fn protobuf_runtime_link_flags() -> Vec<String> {
    let pinned = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/rush-cache/external"))
        .join("com_google_protobuf")
        .join("libprotobuf.a");
    if pinned.exists() {
        vec![
            pinned.display().to_string(),
            "-lz".to_string(),
            "-pthread".to_string(),
        ]
    } else {
        vec!["-lprotobuf".to_string()]
    }
}

/// The protobuf header include dir, preferring TF's pinned protobuf 3.21.9
/// `<src>` when materialized (so generated `.pb.cc` is compiled against the same
/// 3.21.x headers as the runtime it links — ABI consistency), else
/// `/usr/include`. Returns the dir string. See protobuf_runtime_link_flags / #32.
pub fn protobuf_include_dir() -> String {
    let pinned_dir = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/rush-cache/external"))
        .join("com_google_protobuf");
    if pinned_dir.join("libprotobuf.a").exists() {
        if let Ok(s) = std::fs::read_to_string(pinned_dir.join("INCLUDE_ROOT")) {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
        return pinned_dir.join("src").display().to_string();
    }
    "/usr/include".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_system_protobuf() {
        let config = detect_system_protobuf();
        // On systems with protobuf installed, this should find it
        println!("Protobuf found: {}", config.found);
        println!("Include dirs: {:?}", config.include_dirs);
        println!("Link flags: {:?}", config.link_flags);
        println!("Protoc path: {:?}", config.protoc_path);
    }

    #[test]
    fn test_extract_include_dirs() {
        let cflags = vec![
            "-I/usr/include".to_string(),
            "-I/usr/local/include".to_string(),
            "-DFOO=1".to_string(),
        ];

        let dirs = SystemCcLibraryInstance::extract_include_dirs(&cflags);
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0], PathBuf::from("/usr/include"));
        assert_eq!(dirs[1], PathBuf::from("/usr/local/include"));
    }
}
