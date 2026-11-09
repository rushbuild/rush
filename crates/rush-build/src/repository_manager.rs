// Rush Build - Repository Manager
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use crate::cache_paths;
use crate::dependency_graph::{DependencyGraph, Label};
use crate::external_deps::SystemPackageMapper;
use crate::BuildOrchestrator;
use anyhow::{anyhow, Context, Result};
use dashmap::DashMap;
use rush_parse::HttpArchive;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Cursor, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tracing::info;

// This is an immutable, same-user source-cache receipt, not a claim that the
// extracted tree is rehashed on each use. Changing a declaration or BUILD
// overlay requires a fresh cache; unknown existing contents are never replaced.
const HTTP_ARCHIVE_RECEIPT: &str = ".rush-http-archive-source-v1.json";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpArchiveReceipt {
    version: u32,
    definition_sha256: String,
    archive_sha256: String,
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn safe_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(anyhow!(
            "http_archive requires a relative path without parent traversal: {}",
            path.display()
        ));
    }
    Ok(())
}

// Resolve the overlay before downloading, and fingerprint the exact bytes later
// written. A missing overlay is a source-definition error, never a warning.
fn http_archive_build_overlay(workspace: &Path, archive: &HttpArchive) -> Result<Option<Vec<u8>>> {
    if archive.build_file.is_some() && archive.build_file_content.is_some() {
        return Err(anyhow!(
            "http_archive {} specifies both build_file and build_file_content",
            archive.name
        ));
    }
    if let Some(content) = &archive.build_file_content {
        return Ok(Some(content.as_bytes().to_vec()));
    }
    let Some(label) = &archive.build_file else {
        return Ok(None);
    };
    let parsed = Label::parse(label, None, Some(""))?;
    if parsed.workspace.is_some() {
        return Err(anyhow!("http_archive {} build_file {} requires external repository resolution; use the repository-rule loader", archive.name, label));
    }
    let relative = Path::new(&parsed.package).join(&parsed.target);
    safe_relative_path(&relative)?;
    let source = workspace.join(relative);
    std::fs::read(&source).map(Some).with_context(|| {
        format!(
            "http_archive {} read declared build_file {} at {}",
            archive.name,
            label,
            source.display()
        )
    })
}

fn http_archive_definition(archive: &HttpArchive, overlay: Option<&[u8]>) -> Result<String> {
    if let Some(expected) = &archive.sha256 {
        if !valid_sha256(expected) {
            return Err(anyhow!(
                "http_archive {} has an invalid SHA256",
                archive.name
            ));
        }
    }
    // Deterministic, unambiguous serialization, including owner mappings and
    // declaration order of mirror URLs. This deliberately differs from a
    // delimiter-concatenated cache key.
    let mappings: BTreeMap<_, _> = archive.repo_mapping.iter().collect();
    let definition = serde_json::json!({
        "version": 1,
        "name": archive.name,
        "urls": archive.urls,
        "sha256": archive.sha256.as_ref().map(|value| value.to_ascii_lowercase()),
        "strip_prefix": archive.strip_prefix,
        "build_file": archive.build_file,
        "build_file_content": archive.build_file_content,
        "build_overlay_sha256": overlay.map(sha256),
        "repo_mapping": mappings,
    });
    Ok(sha256(&serde_json::to_vec(&definition)?))
}

fn http_archive_cache_is_reusable(
    repository: &Path,
    definition: &str,
    expected_sha: Option<&str>,
) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(repository) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let admitted = (|| -> Option<()> {
        if !metadata.file_type().is_dir() {
            return None;
        }
        let marker = repository.join(HTTP_ARCHIVE_RECEIPT);
        if !std::fs::symlink_metadata(&marker)
            .ok()?
            .file_type()
            .is_file()
        {
            return None;
        }
        let receipt: HttpArchiveReceipt =
            serde_json::from_slice(&std::fs::read(marker).ok()?).ok()?;
        if receipt.version != 1
            || receipt.definition_sha256 != definition
            || !valid_sha256(&receipt.archive_sha256)
        {
            return None;
        }
        if let Some(expected) = expected_sha {
            if !receipt.archive_sha256.eq_ignore_ascii_case(expected) {
                return None;
            }
        }
        Some(())
    })()
    .is_some();
    if !admitted {
        return Err(anyhow!("http_archive cache {} exists without a matching verified source/definition receipt; existing contents were preserved. Inspect and move this cache explicitly, or use a fresh workspace cache", repository.display()));
    }
    Ok(true)
}

fn archive_destination(repository: &Path, path: &Path) -> Result<PathBuf> {
    safe_relative_path(path)?;
    let relative: PathBuf = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    if relative.starts_with(HTTP_ARCHIVE_RECEIPT) {
        return Err(anyhow!(
            "http_archive entry uses reserved source receipt path"
        ));
    }
    let destination = repository.join(relative);
    // Never follow an archive-provided symlink while extracting a later entry
    // or writing the BUILD overlay/receipt. Otherwise an entry could manufacture
    // a valid-looking receipt even when extraction failed.
    let mut ancestor = Some(destination.as_path());
    while let Some(path) = ancestor {
        if path == repository {
            break;
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(anyhow!(
                    "http_archive extraction would follow a symlink at {}",
                    path.display()
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        ancestor = path.parent();
    }
    Ok(destination)
}

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

        safe_relative_path(Path::new(&archive.name))?;
        if let Some(prefix) = archive
            .strip_prefix
            .as_deref()
            .filter(|prefix| !prefix.is_empty())
        {
            safe_relative_path(Path::new(prefix))?;
        }
        let overlay = http_archive_build_overlay(workspace_root, archive)?;
        let definition = http_archive_definition(archive, overlay.as_deref())?;
        if http_archive_cache_is_reusable(&repo_dir, &definition, archive.sha256.as_deref())? {
            return Ok(repo_dir);
        }

        if archive.urls.is_empty() {
            return Err(anyhow!(
                "No URLs provided for http_archive {}",
                archive.name
            ));
        }

        info!("🌐 Fetching @{} from {}", archive.name, archive.urls[0]);

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

        let actual_sha = sha256(&content);
        if let Some(expected_sha) = &archive.sha256 {
            if !actual_sha.eq_ignore_ascii_case(expected_sha) {
                return Err(anyhow!(
                    "SHA256 mismatch for {}: expected {}, got {}",
                    archive.name,
                    expected_sha,
                    actual_sha
                ));
            }
        }

        // Own only a new destination. A racing or unknown existing directory
        // is never truncated/replaced. Failed extraction leaves no valid receipt.
        if let Some(parent) = repo_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir(&repo_dir).with_context(|| {
            format!(
                "http_archive create new source destination {} without replacing existing contents",
                repo_dir.display()
            )
        })?;

        let cursor = Cursor::new(content);
        let mut prefix_seen = archive.strip_prefix.as_deref().is_none_or(str::is_empty);
        if archive.urls[0].ends_with(".zip") {
            let mut zip = zip::ZipArchive::new(cursor)?;

            for i in 0..zip.len() {
                let mut file = zip.by_index(i)?;
                let path = file
                    .enclosed_name()
                    .ok_or_else(|| anyhow!("http_archive ZIP entry escapes its source directory"))?
                    .to_path_buf();

                // Handle strip_prefix
                let path = if let Some(prefix) = &archive.strip_prefix {
                    if let Ok(stripped) = path.strip_prefix(prefix) {
                        prefix_seen = true;
                        stripped.to_path_buf()
                    } else {
                        continue;
                    }
                } else {
                    path
                };

                if path.as_os_str().is_empty() {
                    continue;
                }
                let out_path = archive_destination(&repo_dir, &path)?;

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
                        prefix_seen = true;
                        stripped.to_path_buf()
                    } else {
                        continue;
                    }
                } else {
                    path
                };

                if path.as_os_str().is_empty() {
                    continue;
                }
                let out_path = archive_destination(&repo_dir, &path)?;

                // Ensure parent directory exists
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                if entry.header().entry_type().is_hard_link() {
                    let link = entry
                        .link_name()?
                        .ok_or_else(|| anyhow!("http_archive hard link has no target"))?;
                    let source = if let Some(prefix) = &archive.strip_prefix {
                        link.strip_prefix(prefix).with_context(|| {
                            format!(
                                "http_archive hard link target {} is outside strip_prefix",
                                link.display()
                            )
                        })?
                    } else {
                        link.as_ref()
                    };
                    let source = archive_destination(&repo_dir, source)?;
                    if !std::fs::symlink_metadata(&source)?.file_type().is_file() {
                        return Err(anyhow!(
                            "http_archive hard link target is not an extracted regular file"
                        ));
                    }
                    std::fs::hard_link(source, &out_path)?;
                } else {
                    entry.unpack(&out_path)?;
                }
            }
        }

        if !prefix_seen {
            return Err(anyhow!("http_archive {} strip_prefix was not found in the archive; no source receipt was published", archive.name));
        }
        if let Some(overlay) = overlay {
            // Bazel prefers BUILD.bazel over BUILD. An upstream BUILD.bazel
            // must not silently shadow the explicitly declared replacement.
            let build = archive_destination(&repo_dir, Path::new("BUILD.bazel"))?;
            // An upstream BUILD can share its inode with another archived file.
            // Replace the directory entry rather than truncating that inode and
            // silently modifying an unrelated source through a hard link.
            let mut replacement = tempfile::NamedTempFile::new_in(&repo_dir)?;
            replacement.write_all(&overlay)?;
            replacement.as_file().sync_all()?;
            replacement
                .persist(&build)
                .map_err(|error| error.error)
                .with_context(|| {
                    format!("replace declared BUILD overlay at {}", build.display())
                })?;
        }

        let receipt = HttpArchiveReceipt {
            version: 1,
            definition_sha256: definition,
            archive_sha256: actual_sha,
        };
        let mut marker = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(repo_dir.join(HTTP_ARCHIVE_RECEIPT))
            .context("http_archive create source receipt without following archive entries")?;
        marker.write_all(&serde_json::to_vec(&receipt)?)?;
        marker.sync_all()?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn workspace() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        // Never write a test fixture into an operator-selected shared cache.
        assert!(cache_paths::workspace_external_dir(root.path()).starts_with(root.path()));
        root
    }

    fn manager() -> RepositoryManager {
        RepositoryManager::new(Arc::new(SystemPackageMapper::new()))
    }

    fn definition(url: String, payload: &[u8]) -> HttpArchive {
        HttpArchive {
            name: "source_fixture".into(),
            urls: vec![url],
            sha256: Some(sha256(payload)),
            strip_prefix: Some("upstream/source".into()),
            build_file: None,
            build_file_content: Some(
                "cc_library(name = \"headers\", hdrs = [\"include/float8.h\"])\n".into(),
            ),
            repo_mapping: HashMap::new(),
        }
    }

    fn tar_payload(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(gzip);
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(content.len() as u64);
            header.set_cksum();
            archive.append_data(&mut header, path, *content).unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap()
    }

    async fn serve_once(payload: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 1024];
                let read = stream.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                assert!(request.len() < 16 * 1024);
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(&payload).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        (format!("http://{address}/source.tar.gz"), task)
    }

    #[tokio::test]
    async fn http_archive_unmarked_adapter_is_refused_and_preserved() {
        let root = workspace();
        let mut archive = definition("http://127.0.0.1:1/unreachable.tar.gz".into(), b"unused");
        archive.name = "ml_dtypes".into();
        // TensorFlow 2.17's pinned XLA third_party/py/ml_dtypes/workspace.bzl
        // declaration. This test refuses before network; the next test proves
        // a real checksum-verified HTTP materialization with local bytes.
        archive.urls = vec!["https://github.com/jax-ml/ml_dtypes/archive/24084d9ed2c3d45bf83b7a9bff833aa185bf9172.tar.gz".into()];
        archive.sha256 =
            Some("c916a3e6b3d9bdcb476f506fdbbecb6d5e9f21f82f221dfcb42b320b4e85e55a".into());
        archive.strip_prefix =
            Some("ml_dtypes-24084d9ed2c3d45bf83b7a9bff833aa185bf9172/ml_dtypes".into());
        let repository = cache_paths::workspace_external_dir(root.path()).join(&archive.name);
        std::fs::create_dir_all(repository.join("include")).unwrap();
        let old_build = b"# obsolete local adapter, never verified upstream source\n";
        std::fs::write(repository.join("BUILD"), old_build).unwrap();
        std::fs::write(repository.join("include/sentinel"), b"preserve me").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("sentinel", repository.join("include/mirror")).unwrap();

        // This is a real registered http_archive declaration, not a local_repo
        // fixture. Refusal precedes any upstream HTTP request.
        let manager = manager();
        manager.register_http_archive(archive.clone());
        let error = manager
            .fetch_http_archive(root.path(), &archive)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("without a matching verified source/definition receipt"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(repository.join("BUILD")).unwrap(), old_build);
        assert_eq!(
            std::fs::read(repository.join("include/sentinel")).unwrap(),
            b"preserve me"
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(repository.join("include/mirror")).unwrap(),
            Path::new("sentinel")
        );
        assert!(!repository.join(HTTP_ARCHIVE_RECEIPT).exists());
    }

    #[tokio::test]
    async fn http_archive_verified_source_reuses_only_matching_declaration_and_overlay() {
        let root = workspace();
        let payload = tar_payload(&[
            (
                "upstream/source/include/float8.h",
                b"#define SOURCE_VALUE 42\n" as &[u8],
            ),
            (
                "upstream/source/BUILD.bazel",
                b"# upstream rules must be replaced\n",
            ),
        ]);
        let (url, server) = serve_once(payload.clone()).await;
        let mut archive = definition(url, &payload);
        archive.build_file_content = None;
        archive.build_file = Some("//:overlay.BUILD".into());
        let original_overlay = b"cc_library(name = \"headers\", hdrs = [\"include/float8.h\"])\n";
        std::fs::write(root.path().join("overlay.BUILD"), original_overlay).unwrap();
        let manager = manager();
        let repository = manager
            .fetch_http_archive(root.path(), &archive)
            .await
            .unwrap();
        server.await.unwrap(); // Listener is gone: a second fetch must use its receipt.
        assert_eq!(
            std::fs::read(repository.join("include/float8.h")).unwrap(),
            b"#define SOURCE_VALUE 42\n"
        );
        assert_eq!(
            std::fs::read(repository.join("BUILD.bazel")).unwrap(),
            original_overlay
        );
        let marker = std::fs::read(repository.join(HTTP_ARCHIVE_RECEIPT)).unwrap();
        let receipt: HttpArchiveReceipt = serde_json::from_slice(&marker).unwrap();
        assert_eq!(receipt.archive_sha256, sha256(&payload));
        assert_eq!(
            manager
                .fetch_http_archive(root.path(), &archive)
                .await
                .unwrap(),
            repository
        );

        std::fs::write(
            root.path().join("overlay.BUILD"),
            b"# changed local declaration\n",
        )
        .unwrap();
        let error = manager
            .fetch_http_archive(root.path(), &archive)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("existing contents were preserved"));
        std::fs::write(root.path().join("overlay.BUILD"), original_overlay).unwrap();
        for changed in 0..4 {
            let mut other = archive.clone();
            match changed {
                0 => {
                    other
                        .repo_mapping
                        .insert("dep".into(), "different_owner".into());
                }
                1 => other.sha256 = Some("0".repeat(64)),
                2 => other.strip_prefix = Some("upstream".into()),
                _ => other
                    .urls
                    .push("http://127.0.0.1:1/another-mirror.tar.gz".into()),
            }
            assert!(manager
                .fetch_http_archive(root.path(), &other)
                .await
                .unwrap_err()
                .to_string()
                .contains("existing contents were preserved"));
        }
        assert_eq!(
            std::fs::read(repository.join(HTTP_ARCHIVE_RECEIPT)).unwrap(),
            marker
        );
        assert_eq!(
            std::fs::read(repository.join("BUILD.bazel")).unwrap(),
            original_overlay
        );
    }

    #[tokio::test]
    async fn http_archive_checksum_failure_does_not_publish_source() {
        let root = workspace();
        let payload = tar_payload(&[("upstream/source/file", b"actual bytes")]);
        let (url, server) = serve_once(payload.clone()).await;
        let mut archive = definition(url, &payload);
        archive.sha256 = Some("0".repeat(64));
        let error = manager()
            .fetch_http_archive(root.path(), &archive)
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(error.to_string().contains("SHA256 mismatch"));
        assert!(!cache_paths::workspace_external_dir(root.path())
            .join(&archive.name)
            .exists());
    }

    #[tokio::test]
    async fn http_archive_failed_extraction_cannot_supply_its_own_receipt() {
        let root = workspace();
        let payload = tar_payload(&[(
            "upstream/source/.rush-http-archive-source-v1.json",
            b"forged source admission",
        )]);
        let (url, server) = serve_once(payload.clone()).await;
        let archive = definition(url, &payload);
        let manager = manager();
        let error = manager
            .fetch_http_archive(root.path(), &archive)
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(error.to_string().contains("reserved source receipt"));
        let repository = cache_paths::workspace_external_dir(root.path()).join(&archive.name);
        assert!(repository.is_dir());
        assert!(!repository.join(HTTP_ARCHIVE_RECEIPT).exists());
        assert!(manager
            .fetch_http_archive(root.path(), &archive)
            .await
            .unwrap_err()
            .to_string()
            .contains("existing contents were preserved"));
    }

    #[cfg(unix)]
    #[test]
    fn http_archive_symlink_cache_or_receipt_never_admits_source() {
        let root = workspace();
        let actual = root.path().join("actual");
        std::fs::create_dir(&actual).unwrap();
        let expected = "definition";
        let receipt = HttpArchiveReceipt {
            version: 1,
            definition_sha256: expected.into(),
            archive_sha256: "a".repeat(64),
        };
        std::fs::write(
            actual.join(HTTP_ARCHIVE_RECEIPT),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        let linked = root.path().join("linked");
        std::os::unix::fs::symlink(&actual, &linked).unwrap();
        assert!(http_archive_cache_is_reusable(&linked, expected, None).is_err());
        let repository = root.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        std::os::unix::fs::symlink(
            actual.join(HTTP_ARCHIVE_RECEIPT),
            repository.join(HTTP_ARCHIVE_RECEIPT),
        )
        .unwrap();
        assert!(http_archive_cache_is_reusable(&repository, expected, None).is_err());
        assert!(http_archive_cache_is_reusable(&actual, expected, None).unwrap());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn http_archive_overlay_detaches_upstream_hardlinked_build_file() {
        use std::os::unix::fs::MetadataExt;
        let root = workspace();
        let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gzip);
        let original = b"# upstream content shared with a source file\n";
        let mut regular = tar::Header::new_gnu();
        regular.set_mode(0o644);
        regular.set_size(original.len() as u64);
        regular.set_cksum();
        tar.append_data(&mut regular, "upstream/source/BUILD.bazel", &original[..])
            .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_mode(0o644);
        link.set_entry_type(tar::EntryType::Link);
        link.set_size(0);
        link.set_link_name("upstream/source/BUILD.bazel").unwrap();
        link.set_cksum();
        tar.append_data(&mut link, "upstream/source/include/upstream.h", &[][..])
            .unwrap();
        let payload = tar.into_inner().unwrap().finish().unwrap();
        let (url, server) = serve_once(payload.clone()).await;
        let archive = definition(url, &payload);
        let repository = manager()
            .fetch_http_archive(root.path(), &archive)
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(
            std::fs::read(repository.join("include/upstream.h")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("BUILD.bazel")).unwrap(),
            archive.build_file_content.unwrap()
        );
        assert_ne!(
            std::fs::metadata(repository.join("BUILD.bazel"))
                .unwrap()
                .ino(),
            std::fs::metadata(repository.join("include/upstream.h"))
                .unwrap()
                .ino()
        );
    }
}
