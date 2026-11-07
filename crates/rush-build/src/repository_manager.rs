// Rush Build - Repository Manager
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use crate::cache_paths;
use crate::dependency_graph::{DependencyGraph, Label};
use crate::external_deps::SystemPackageMapper;
use crate::BuildOrchestrator;
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use rush_parse::HttpArchive;
use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::info;

/// Manages external repositories and their resolution.
pub struct RepositoryManager {
    /// System package mapper for resolving system dependencies.
    system_mapper: Arc<SystemPackageMapper>,

    /// Registered HTTP archives from WORKSPACE files.
    http_archives: DashMap<String, HttpArchive>,
}

impl RepositoryManager {
    /// Create a new repository manager.
    pub fn new(system_mapper: Arc<SystemPackageMapper>) -> Self {
        Self {
            system_mapper,
            http_archives: DashMap::new(),
        }
    }

    /// Register an HTTP archive definition.
    pub fn register_http_archive(&self, archive: HttpArchive) {
        self.http_archives.insert(archive.name.clone(), archive);
    }

    /// Get a registered HTTP archive by name.
    pub fn get_http_archive(&self, name: &str) -> Option<HttpArchive> {
        self.http_archives.get(name).map(|a| a.clone())
    }

    /// Fetch an HTTP archive repository.
    pub async fn fetch_http_archive(
        &self,
        workspace_root: &Path,
        archive: &HttpArchive,
    ) -> Result<PathBuf> {
        let external_dir = cache_paths::workspace_external_dir(workspace_root);
        let repo_dir = external_dir.join(&archive.name);

        // Check if already exists
        if repo_dir.exists() {
            // TODO: Check if valid (e.g. marker file)
            return Ok(repo_dir);
        }

        if archive.urls.is_empty() {
            return Err(anyhow!(
                "No URLs provided for http_archive {}",
                archive.name
            ));
        }

        info!("🌐 Fetching @{} from {}", archive.name, archive.urls[0]);

        // Create temp dir for download
        let temp_dir = tempfile::tempdir()?;
        let download_path = temp_dir.path().join("archive");

        // Download
        let client = reqwest::Client::new();
        let mut response = client.get(&archive.urls[0]).send().await?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to download {}: {}",
                archive.urls[0],
                response.status()
            ));
        }

        let mut content = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            content.extend_from_slice(&chunk);
        }

        // Verify SHA256
        if let Some(expected_sha) = &archive.sha256 {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&content);
            let result = hasher.finalize();
            let actual_sha = hex::encode(result);

            if &actual_sha != expected_sha {
                return Err(anyhow!(
                    "SHA256 mismatch for {}: expected {}, got {}",
                    archive.name,
                    expected_sha,
                    actual_sha
                ));
            }
        }

        // Extract
        std::fs::create_dir_all(&repo_dir)?;

        let cursor = Cursor::new(content);
        if archive.urls[0].ends_with(".zip") {
            let mut zip = zip::ZipArchive::new(cursor)?;

            for i in 0..zip.len() {
                let mut file = zip.by_index(i)?;
                let path = file.mangled_name();

                // Handle strip_prefix
                let path = if let Some(prefix) = &archive.strip_prefix {
                    if let Ok(stripped) = path.strip_prefix(prefix) {
                        stripped.to_path_buf()
                    } else {
                        continue;
                    }
                } else {
                    path
                };

                let out_path = repo_dir.join(path);

                if file.is_dir() {
                    std::fs::create_dir_all(&out_path)?;
                } else {
                    if let Some(parent) = out_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let mut outfile = File::create(&out_path)?;
                    std::io::copy(&mut file, &mut outfile)?;
                }
            }
        } else {
            // Assume tar.gz
            let tar = flate2::read::GzDecoder::new(cursor);
            let mut archive_tar = tar::Archive::new(tar);

            for entry in archive_tar.entries()? {
                let mut entry = entry?;
                let path = entry.path()?.to_path_buf();

                // Handle strip_prefix
                let path = if let Some(prefix) = &archive.strip_prefix {
                    if let Ok(stripped) = path.strip_prefix(prefix) {
                        stripped.to_path_buf()
                    } else {
                        continue;
                    }
                } else {
                    path
                };

                let out_path = repo_dir.join(path);

                // Ensure parent directory exists
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                entry.unpack(&out_path)?;
            }
        }

        // Handle build_file_content (inline BUILD file)
        if let Some(content) = &archive.build_file_content {
            std::fs::write(repo_dir.join("BUILD"), content)?;
        }

        // Handle build_file (path to local BUILD file)
        if let Some(build_file_path) = &archive.build_file {
            // The build_file path is relative to the main workspace root
            // We need to resolve it and copy it to the repo root as BUILD

            // Note: This assumes build_file_path is relative to workspace_root.
            // If it starts with "//", we should strip it.
            let clean_path = if build_file_path.starts_with("//") {
                build_file_path.strip_prefix("//").unwrap()
            } else if build_file_path.starts_with(":") {
                build_file_path.strip_prefix(":").unwrap()
            } else {
                build_file_path
            };

            // Handle label format (package:target) -> path
            let path_str = clean_path.replace(':', "/");
            let source_path = workspace_root.join(path_str);

            if source_path.exists() {
                std::fs::copy(&source_path, repo_dir.join("BUILD"))?;
            } else {
                // Try appending /BUILD if it's a package path
                let source_path_build = source_path.join("BUILD");
                if source_path_build.exists() {
                    std::fs::copy(&source_path_build, repo_dir.join("BUILD"))?;
                } else {
                    // Just warn for now, don't fail the fetch
                    info!("⚠️  Could not find build_file: {}", build_file_path);
                }
            }
        }

        Ok(repo_dir)
    }

    /// Resolve a missing dependency by synthesizing stub targets or registering external repos.
    ///
    /// Returns `true` if the dependency was successfully resolved/synthesized.
    pub async fn resolve_missing_dependency(
        &self,
        orchestrator: &BuildOrchestrator,
        graph: &mut DependencyGraph,
        missing_label: &Label,
    ) -> Result<bool> {
        let missing_label_str = missing_label.to_string();

        // Check for known system stubs.
        //
        // NOTE: `@ml_dtypes` used to live on this list but was removed
        // once `external_deps.rs` started writing a proper stub `BUILD`
        // for it (with `cc_library(... includes = [<clone_root>])` so
        // CcInfo propagates `-I<clone_root>` to downstream compiles
        // via normal provider merging). A bare `register_system_stub`
        // here would short-circuit that BUILD parse and drop the
        // include_dirs, leaving consumers like
        // `@local_tsl//tsl/platform:types` unable to find
        // `ml_dtypes/include/float8.h`.
        if missing_label_str.starts_with("@com_google_benchmark")
            || missing_label_str.starts_with("@llvm-project")
            || missing_label_str.starts_with("@llvm_terminfo")
            || missing_label_str.starts_with("@llvm_zlib")
            || missing_label_str.starts_with("@llvm_openmp")
            || missing_label_str.starts_with("@pypi")
            || missing_label_str.starts_with("@flatbuffers")
            || missing_label_str.starts_with("@local_config_rocm")
            // XNNPACK / mobile-inference stack. Not needed for a
            // non-GPU, non-mobile libtensorflow_cc.so build; stubbing
            // these avoids expensive bazel-subprocess fetches that
            // (a) spawn ~500 MB java daemons per repo and (b)
            // sometimes report success without producing the expected
            // `external/<repo>/` directory, aborting the rush build.
            || missing_label_str.starts_with("@XNNPACK")
            || missing_label_str.starts_with("@arm_neon_")
            || missing_label_str.starts_with("@clog")
            // NOTE: @cpuinfo is NOT stubbed — TF's libtensorflow_cc.so links
            // the TFLite kernels (cpu_backend_context) which need real
            // include/cpuinfo.h + cpuinfo_* symbols. Materialized (real source
            // + libcpuinfo.a, bundling clog) via external_deps.rs +
            // scripts/bootstrap_cpuinfo.sh.
            // NOTE: @onednn is NOT stubbed — TensorFlow's Eigen contraction
            // kernel directly includes dnnl.h and must consume the real
            // @onednn//:mkl_dnn provider.
            || missing_label_str.starts_with("@FP")
            // @FXdiv stays stubbed: only pthreadpool's libpthreadpool.a build
            // consumes fxdiv.h (handled by scripts/bootstrap_pthreadpool.sh out
            // of band); no TF consumer includes it directly, so it never reaches
            // a compile on the critical path.
            || missing_label_str.starts_with("@FXdiv")
            // NOTE: @pthreadpool is NOT stubbed — TF's libtensorflow_cc.so links
            // the TFLite kernels (cpu_backend_context) which need real
            // pthreadpool.h + pthreadpool_* symbols. Materialized (real source +
            // libpthreadpool.a) via external_deps.rs + scripts/
            // bootstrap_pthreadpool.sh; falls through to the generic BUILD-parse
            // path so the adapter cc_library (with includes=[<clone>/include]) is
            // loaded instead of an empty stub with a bare -I<repo_root>.
            // NOTE: @ruy is NOT stubbed — TF's libtensorflow_cc.so links the
            // TFLite kernels (cpu_backend_*) which need real ruy/context.h +
            // ruy:: symbols. It is materialized (real source + libruy.a) via
            // external_deps.rs + scripts/bootstrap_ruy.sh.
            || missing_label_str.starts_with("@upb")
            // Low-level / config repos that TF's BUILD mentions but
            // whose actual headers aren't on the critical-path of
            // libtensorflow_cc.so.
            || missing_label_str.starts_with("@nccl_archive")
            || missing_label_str.starts_with("@nvtx_archive")
            || missing_label_str.starts_with("@mkl_dnn_acl_compatible")
            || missing_label_str.starts_with("@local_config_git")
            || missing_label_str.starts_with("@local_execution_config_platform")
        {
            eprintln!("    ⚙️  Synthesizing stub target for {}", missing_label);
            orchestrator.register_system_stub(graph, missing_label)?;
            return Ok(true);
        }

        // Check if this is a registered http_archive
        if let Some(repo_name) = &missing_label.workspace {
            if let Some(archive) = self.get_http_archive(repo_name) {
                eprintln!("    🌐 Fetching http_archive @{}...", repo_name);

                // Fetch the archive
                match self
                    .fetch_http_archive(&orchestrator.workspace_root, &archive)
                    .await
                {
                    Ok(repo_path) => {
                        eprintln!("    ✅ Fetched @{} to {}", repo_name, repo_path.display());

                        // Register the repo path in the orchestrator
                        orchestrator
                            .external_repo_paths
                            .lock()
                            .await
                            .insert(repo_name.clone(), repo_path.clone());

                        // Also update bzl_loader so it can find .bzl files in this repo
                        if let Ok(mut loader) = orchestrator.bzl_loader.lock() {
                            loader.add_external_repo(repo_name.clone(), repo_path);
                            // Register repo mapping if present
                            if !archive.repo_mapping.is_empty() {
                                loader.set_command_repo_mapping_for(
                                    repo_name.clone(),
                                    archive.repo_mapping.clone(),
                                );
                            }
                        }

                        // Now that the repo is fetched, we can try to find the BUILD file
                        // and parse the target. But for now, we just return true to indicate
                        // we did something. The caller should retry parsing.
                        return Ok(true);
                    }
                    Err(e) => {
                        eprintln!("    ❌ Failed to fetch @{}: {}", repo_name, e);
                        // Don't return error, let the build fail naturally later
                    }
                }
            }
        }

        // Special-case system-provided repositories
        if let Some(repo) = missing_label.workspace.as_deref() {
            if repo == "com_google_absl" && missing_label.package == "absl/strings" {
                eprintln!(
                    "    ⚙️  Synthesizing stub targets for @com_google_absl//{}",
                    missing_label.package
                );
                orchestrator.register_absl_strings_stub(graph)?;
                return Ok(true);
            }

            if repo == "com_google_absl" {
                eprintln!(
                    "    ⚙️  Synthesizing stub target for @com_google_absl//{}",
                    missing_label.package
                );
                orchestrator.register_absl_stub(graph, missing_label)?;
                return Ok(true);
            }

            // @ml_dtypes is handled via its real stub BUILD (written by
            // external_deps.rs) which carries the include_dirs CcInfo
            // consumers need; fall through so the generic BUILD-parsing
            // recovery picks it up instead of creating a bare
            // SystemCcLibrary.

            if repo == "six_archive" {
                eprintln!(
                    "    ⚙️  Synthesizing stub target for @six_archive//{}",
                    missing_label.package
                );
                orchestrator.register_system_stub(graph, missing_label)?;
                return Ok(true);
            }

            if repo == "local_tsl" && missing_label.package == "external" {
                eprintln!("    ⚙️  Synthesizing stub targets for @local_tsl//external");
                orchestrator.register_local_tsl_external_stubs(graph)?;
                return Ok(true);
            }
        }

        Ok(false)
    }
}
