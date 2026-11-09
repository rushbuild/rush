// External dependency resolution - map to system packages
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! External dependency resolution for @repo//path:target style dependencies.
//!
//! This module maps external Bazel repositories to system-installed packages,
//! avoiding the need to build external dependencies from source.

use crate::cache_paths;
use anyhow::Result;
use rush_rules::rules_system::detect_system_protobuf;
use std::path::{Path, PathBuf};

const PYBIND11_BOOTSTRAP_SHA256: &str =
    "832e2f309c57da9c1e6d4542dedd34b24e4192ecb4d62f6f4866a737454c9970";
const PYBIND11_HEADER_WITNESS: &str = "include/pybind11/pybind11.h";

fn verified_pybind11_clone(cache_root: &Path) -> Option<PathBuf> {
    let clone = cache_root.join("pybind11");
    let stamp = std::fs::read_to_string(clone.join(".rush-bootstrap-sha256")).ok()?;
    if stamp.trim() != PYBIND11_BOOTSTRAP_SHA256 || !clone.join(PYBIND11_HEADER_WITNESS).is_file() {
        return None;
    }
    Some(clone)
}

fn materialize_tf217_rules_java_facade(external_cache: &Path) -> Result<PathBuf> {
    let repo = external_cache.join("rules_java");
    std::fs::create_dir_all(repo.join("java"))?;
    std::fs::write(
        repo.join("BUILD"),
        "package(default_visibility = [\"//visibility:public\"])\n",
    )?;
    std::fs::write(repo.join("java/BUILD"), "exports_files([\"defs.bzl\"])\n")?;
    std::fs::write(
        repo.join("java/defs.bzl"),
        "# Synthesized by rush for the TF-2.17 C++ climb.\n\
         # Java build targets forward to native so Rush retains fail-loud graph nodes.\n\
         # Toolchain/configuration declarations remain load-only in this facade.\n\
         def java_test(**kwargs):\n    native.java_test(**kwargs)\n\
         def java_library(**kwargs):\n    native.java_library(**kwargs)\n\
         def java_binary(**kwargs):\n    native.java_binary(**kwargs)\n\
         def java_import(**kwargs):\n    native.java_import(**kwargs)\n\
         def java_plugin(**kwargs):\n    native.java_plugin(**kwargs)\n\
         def _java_toolchain_load_only(**kwargs):\n    pass\n\
         java_package_configuration = _java_toolchain_load_only\n\
         java_runtime = _java_toolchain_load_only\n\
         java_toolchain = _java_toolchain_load_only\n\
         http_jar = _java_toolchain_load_only\n\
         JavaInfo = provider(fields = [])\n\
         JavaPluginInfo = provider(fields = [])\n\
         java_common = struct(JavaInfo = JavaInfo, JavaPluginInfo = JavaPluginInfo)\n",
    )?;
    Ok(repo)
}

fn pybind11_declared_headers(clone: &Path) -> Result<Vec<PathBuf>> {
    let mut headers = Vec::new();
    for relative_dir in ["include/pybind11", "include/pybind11/detail"] {
        let dir = clone.join(relative_dir);
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("h") {
                continue;
            }
            let relative = path.strip_prefix(clone)?.to_path_buf();
            if relative_dir == "include/pybind11"
                && matches!(
                    relative.file_name().and_then(|n| n.to_str()),
                    Some("common.h" | "eigen.h")
                )
            {
                continue;
            }
            headers.push(relative);
        }
    }
    headers.sort();
    if !headers
        .iter()
        .any(|header| header == Path::new(PYBIND11_HEADER_WITNESS))
    {
        anyhow::bail!(
            "verified pybind11 clone {} does not contain its declared root header",
            clone.display()
        );
    }
    Ok(headers)
}

fn publish_staged_directory(staged: &Path, live: &Path) -> Result<()> {
    let live_metadata = match std::fs::symlink_metadata(live) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::rename(staged, live)?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if !live_metadata.is_dir() {
        anyhow::bail!(
            "refusing to replace non-directory repository {}",
            live.display()
        );
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        nix::fcntl::renameat2(
            None,
            staged,
            None,
            live,
            nix::fcntl::RenameFlags::RENAME_EXCHANGE,
        )?;
        return Ok(());
    }

    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        let parent = live
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", live.display()))?;
        let backup = tempfile::Builder::new()
            .prefix(".pybind11-previous-")
            .tempdir_in(parent)?;
        let backup_path = backup.path().to_path_buf();
        std::fs::remove_dir(&backup_path)?;
        std::fs::rename(live, &backup_path)?;
        if let Err(error) = std::fs::rename(staged, live) {
            let rollback = std::fs::rename(&backup_path, live);
            return match rollback {
                Ok(()) => Err(error.into()),
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "failed to publish {}: {}; rollback also failed: {}",
                    live.display(),
                    error,
                    rollback_error
                )),
            };
        }
        std::fs::rename(&backup_path, staged)?;
        Ok(())
    }
}

fn materialize_pybind11_repository(repo: &Path, clone: Option<&Path>) -> Result<()> {
    let parent = repo
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", repo.display()))?;
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".pybind11-stage-")
        .tempdir_in(parent)?;
    let staged_build = staging.path().join("BUILD");

    let build = match clone {
        Some(clone) => {
            let headers = pybind11_declared_headers(clone)?;
            for relative in &headers {
                let destination = staging.path().join(relative);
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(clone.join(relative), destination)?;
            }

            let rendered_headers = headers
                .iter()
                .map(|header| {
                    format!(
                        "        \"{}\",\n",
                        header.to_string_lossy().replace('\\', "/")
                    )
                })
                .collect::<String>();
            format!(
                "# Materialized pybind11 v2.10.4 from TensorFlow's pinned archive.\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                     name = \"pybind11\",\n\
                     hdrs = [\n\
                 {rendered_headers}\
                     ],\n\
                     copts = [\n\
                         \"-fexceptions\",\n\
                         \"-Wno-undefined-inline\",\n\
                         \"-Wno-pragma-once-outside-header\",\n\
                     ],\n\
                     includes = [\"include\"],\n\
                     strip_include_prefix = \"include\",\n\
                     visibility = [\"//visibility:public\"],\n\
                     deps = [\"@org_tensorflow//third_party/python_runtime:headers\"],\n\
                 )\n"
            )
        }
        None => "# @pybind11 requires TensorFlow's pinned pybind11 v2.10.4 source.\n\
             # Run scripts/bootstrap_light_repos.sh into RUSH_GLOBAL_CLONE_CACHE.\n\
             package(default_visibility = [\"//visibility:public\"])\n"
            .to_string(),
    };
    std::fs::write(&staged_build, build)?;

    std::fs::create_dir_all(staging.path().join("pybind11"))?;
    std::fs::write(
        staging.path().join("pybind11/BUILD"),
        "# Stub BUILD for pybind11/pybind11 - Rush handles Python bindings differently\n\
         package(default_visibility = [\"//visibility:public\"])\n\n\
         cc_library(\n\
             name = \"pybind11\",\n\
             srcs = [],\n\
             hdrs = [],\n\
             includes = [\"/usr/include/pybind11\", \"/usr/include/python3.11\"],\n\
             visibility = [\"//visibility:public\"],\n\
         )\n\n\
         cc_library(\n\
             name = \"pybind11_embed\",\n\
             srcs = [],\n\
             hdrs = [],\n\
             includes = [\"/usr/include/pybind11\", \"/usr/include/python3.11\"],\n\
             visibility = [\"//visibility:public\"],\n\
         )\n",
    )?;
    std::fs::write(
        staging.path().join("build_defs.bzl"),
        r#"# Stub build_defs.bzl for pybind11

def pybind_extension(**kwargs):
    pass

def pybind_library(**kwargs):
    native.cc_library(**kwargs)
"#,
    )?;

    publish_staged_directory(staging.path(), repo)?;
    Ok(())
}

/// System package mapping for common external repositories
pub struct SystemPackageMapper;

/// The protobuf headers rush is about to put on the include path, and the
/// `protoc` that will generate sources against them, must agree.
///
/// Returns a description of the mismatch when they do not. Compares the
/// `GOOGLE_PROTOBUF_VERSION` macro in `google/protobuf/stubs/common.h`
/// (encoded major*1_000_000 + minor*1_000 + patch) against
/// `protoc --version`, on the major.minor pair — patch differences are
/// compatible, a minor difference is not.
///
/// Returns `None` when either side cannot be determined: an unknown
/// version is not evidence of a mismatch, and refusing to build on a
/// missing `protoc` would break workspaces that never compile a proto.
/// The C++ compiler rush will use, matching `rules_cc`'s resolution
/// (`RUSH_CXX`, else `g++` from PATH).
fn resolved_cxx() -> String {
    std::env::var("RUSH_CXX")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "g++".to_string())
}

fn default_global_clone_cache_root_from(
    xdg_cache_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(root) = xdg_cache_home.filter(|root| !root.is_empty()) {
        return PathBuf::from(root).join("rush/external");
    }
    if let Some(root) = home.filter(|root| !root.is_empty()) {
        return PathBuf::from(root).join(".cache/rush/external");
    }
    PathBuf::from(".cache/rush/external")
}

fn default_global_clone_cache_root() -> PathBuf {
    default_global_clone_cache_root_from(
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("HOME"),
    )
}

/// Guard the PINNED Abseil archives against the compiler rush will use.
///
/// `scripts/bootstrap_absl.sh` records the toolchain it built with in
/// `<libdir>/.rush-toolchain` — and nothing ever read it. Archives built by
/// a different compiler therefore linked silently and failed hundreds of
/// archives deep on a template instantiation whose MANGLING differs between
/// compilers. Observed for real: `absl::Cord::Cord(std::string&&)`, where
/// g++ 11.4 references `...Li0EEEOT_` while the archive defined the longer
/// `Tn<enable_if<...>::type>` encoding — the same symbol to a human, two
/// different symbols to the linker.
///
/// Returns a message when the archives must not be linked. `None` when
/// they match, or when none are built (the caller already degrades to an
/// honest "run the bootstrap" link failure in that case).
fn absl_toolchain_skew(absl_dir: &std::path::Path, cxx: &str) -> Option<String> {
    let libdir = absl_dir.join(".rush-absl/lib");
    let has_archives = std::fs::read_dir(&libdir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .any(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with("libabsl_") && n.ends_with(".a"))
                .unwrap_or(false)
        });
    if !has_archives {
        return None;
    }

    let stamp_path = libdir.join(".rush-toolchain");
    let Ok(stamp) = std::fs::read_to_string(&stamp_path) else {
        return Some(format!(
            "pinned Abseil archives in {} were built by an unrecorded \
             toolchain (no {}). Their C++ ABI cannot be checked against the \
             compiler rush will use, and a mismatch fails as an undefined \
             reference to an absl template hundreds of archives into the \
             link. Re-run scripts/bootstrap_absl.sh. Refusing to link \
             archives of unknown provenance.",
            libdir.display(),
            stamp_path.display()
        ));
    };
    let built_with = stamp
        .lines()
        .find_map(|l| l.strip_prefix("cxx_version="))
        .map(|v| v.trim().to_string())?;

    let out = std::process::Command::new(cxx).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let current = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();

    if current == built_with {
        return None;
    }
    Some(format!(
        "Abseil ABI skew: the pinned archives in {} were built with \
         \"{}\" but rush compiles with \"{}\". C++ mangling of absl's \
         templates differs between them, so the link fails with an \
         undefined reference to a symbol that IS in the archive (observed: \
         absl::Cord::Cord(std::string&&)). Re-run scripts/bootstrap_absl.sh \
         with this compiler. Refusing to link ABI-mismatched archives.",
        libdir.display(),
        built_with,
        current
    ))
}

fn protobuf_version_skew(
    pinned_include: &str,
    pinned_active: bool,
    pb_config: &rush_rules::rules_system::SystemProtobufConfig,
    protoc_bin: &std::path::Path,
) -> Option<String> {
    let include_roots: Vec<PathBuf> = if pinned_active {
        vec![PathBuf::from(pinned_include)]
    } else if pb_config.found {
        pb_config.include_dirs.clone()
    } else {
        vec![PathBuf::from("/usr/include")]
    };

    let header_version = include_roots.iter().find_map(|root| {
        let h = root.join("google/protobuf/stubs/common.h");
        let text = std::fs::read_to_string(&h).ok()?;
        let line = text
            .lines()
            .find(|l| l.contains("#define GOOGLE_PROTOBUF_VERSION "))?;
        let n: u64 = line.split_whitespace().last()?.parse().ok()?;
        Some((n / 1_000_000, (n / 1_000) % 1_000, h))
    })?;

    // `protoc_bin` is the compiler rush will ACTUALLY run, passed in by the
    // caller rather than re-derived here: probing a different binary than the
    // one that generates the sources makes this gate both miss a real skew
    // and reject a correctly-pinned build.
    let out = std::process::Command::new(protoc_bin).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let ver = text.split_whitespace().last()?;
    let mut parts = ver.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);

    let (h_major, h_minor, h_path) = header_version;
    if (major, minor) == (h_major, h_minor) {
        return None;
    }
    Some(format!(
        "protobuf version skew: the protoc rush runs ({}) is {}.{} but the \
         protobuf headers rush would compile against are {}.{} ({}). Every generated \
         .pb.cc carries a version guard, so this fails later as \"This file \
         was generated by a newer version of protoc\" on an unrelated-looking \
         target. Fix by materializing the declared @com_google_protobuf (see \
         docs/ci/tf-corpus-environment.md) or by putting a matching protoc \
         first on PATH. Refusing to substitute the host's copy for a declared \
         dependency.",
        protoc_bin.display(), major, minor, h_major, h_minor, h_path.display()
    ))
}

/// Pick the protobuf include directory for a given clone-cache root: the
/// pinned 3.21.9 headers when that cache holds the static archive, the
/// system headers otherwise. Returns the directory and whether it is the
/// pinned one.
///
/// Split out of the resolver so the precedence is testable without
/// depending on which protobuf the host happens to have installed. The
/// test that asserted "/usr/include" outright passed only on a machine
/// with no pinned cache, which is the wrong thing for a unit test to
/// measure.
pub(crate) fn protobuf_include_dir_for_cache(cache_root: &std::path::Path) -> (PathBuf, bool) {
    let pinned_dir = cache_root.join("com_google_protobuf");
    if pinned_dir.join("libprotobuf.a").exists() {
        let inc = std::fs::read_to_string(pinned_dir.join("INCLUDE_ROOT"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| pinned_dir.join("src"));
        (inc, true)
    } else {
        (PathBuf::from("/usr/include"), false)
    }
}

impl SystemPackageMapper {
    /// Create a new SystemPackageMapper.
    pub fn new() -> Self {
        Self
    }

    /// Return true if the repository is expected to be satisfied by a local/system shim.
    pub fn is_system_repo(repo: &str) -> bool {
        matches!(
            repo,
            "bazel_tools"
                | "com_github_grpc_grpc"
                | "com_google_fuzztest"
                | "com_google_googletest"
                | "com_google_protobuf"
                | "com_googlesource_code_re2"
                | "curl"
                | "double_conversion"
                | "ducc"
                | "eigen_archive"
                | "farmhash_archive"
                | "fft2d"
                | "gemmlowp"
                | "gif"
                | "highwayhash"
                | "icu"
                | "jsoncpp_git"
                | "libjpeg_turbo"
                | "local_config_nccl"
                | "local_tsl"
                | "local_xla"
                | "nsync"
                | "org_brotli"
                | "org_sqlite"
                | "png"
                | "pypi_opt_einsum"
                | "pybind11"
                | "rules_python"
                | "snappy"
                | "sobol_data"
                | "tf_runtime"
                | "zlib"
                | "absl_py"
                | "python"
                | "pypi"
                | "com_google_absl"
                | "com_google_googleapis"
                | "tf_runtime"
        )
    }

    /// Map an external repository target to system include paths
    ///
    /// Returns include directories that should be added to compilation
    pub fn resolve_external_target(
        repo: &str,
        target: &str,
        workspace_root: &Path,
    ) -> Result<Vec<PathBuf>> {
        let mut include_dirs = Vec::new();

        match (repo, target) {
            // Protocol Buffers (includes Abseil as dependency)
            ("com_google_protobuf", "protobuf_headers") | ("com_google_protobuf", "protobuf") => {
                // #32: prefer TF's PINNED protobuf 3.21.9 headers when the static
                // archive is materialized — compiling TF code against the system
                // 4.25 headers while linking the 3.21.9 .a is the version skew
                // that breaks op-gen TextFormat. Fall back to /usr/include only
                // when the pinned protobuf is absent.
                let cache_root = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| default_global_clone_cache_root());
                let (inc, pinned) = protobuf_include_dir_for_cache(&cache_root);
                if pinned {
                    eprintln!(
                        "[SYSMAP] Mapped @{}//{} -> {:?} (pinned protobuf 3.21.9)",
                        repo, target, inc
                    );
                } else {
                    eprintln!(
                        "[SYSMAP] Mapped @{}//{} -> /usr/include (system protobuf)",
                        repo, target
                    );
                }
                include_dirs.push(inc);
            }

            // Eigen linear algebra library
            ("eigen_archive", "eigen3") | ("eigen_archive", "eigen") => {
                // Prefer ml_dtypes' vendored Eigen if available (has get_integer_by_size)
                let ml_dtypes_eigen = workspace_root.join("external/ml_dtypes_eigen");
                if ml_dtypes_eigen.exists() {
                    include_dirs.push(ml_dtypes_eigen);
                    eprintln!(
                        "[SYSMAP] Mapped @{}//{} -> external/ml_dtypes_eigen (vendored)",
                        repo, target
                    );
                } else {
                    // Fallback to system Eigen3 headers
                    include_dirs.push(PathBuf::from("/usr/include/eigen3"));
                    include_dirs.push(PathBuf::from("/usr/include"));
                    eprintln!(
                        "[SYSMAP] Mapped @{}//{} -> /usr/include/eigen3 (system)",
                        repo, target
                    );
                }
            }

            // Google Test
            ("com_google_googletest", _) => {
                include_dirs.push(PathBuf::from("/usr/include"));

                eprintln!(
                    "[SYSMAP] Mapped @{}//{} -> /usr/include (gtest)",
                    repo, target
                );
            }

            // double-conversion (floating point conversion helpers)
            ("double_conversion", _) => {
                include_dirs.push(PathBuf::from("/usr/include/double-conversion"));
                include_dirs.push(PathBuf::from("/usr/include"));

                eprintln!(
                    "[SYSMAP] Mapped @{}//{} -> /usr/include/double-conversion",
                    repo, target
                );
            }

            // Snappy compression (used by TensorFlow for TFRecord, etc.)
            ("snappy", _) => {
                include_dirs.push(PathBuf::from("/usr/include"));

                eprintln!(
                    "[SYSMAP] Mapped @{}//{} -> /usr/include (snappy)",
                    repo, target
                );
            }

            // GIF image library
            ("gif", _) => {
                include_dirs.push(PathBuf::from("/usr/include"));

                eprintln!(
                    "[SYSMAP] Mapped @{}//{} -> /usr/include (gif)",
                    repo, target
                );
            }

            // libjpeg-turbo (system JPEG implementation)
            ("libjpeg_turbo", _) => {
                include_dirs.push(PathBuf::from("/usr/include"));

                eprintln!(
                    "[SYSMAP] Mapped @{}//{} -> /usr/include (libjpeg-turbo)",
                    repo, target
                );
            }

            // jsoncpp (JSON parser)
            ("jsoncpp_git", _) => {
                include_dirs.push(PathBuf::from("/usr/include/jsoncpp"));
                include_dirs.push(PathBuf::from("/usr/include"));

                eprintln!(
                    "[SYSMAP] Mapped @{}//{} -> /usr/include/jsoncpp (jsoncpp)",
                    repo, target
                );
            }

            // Local TensorFlow third_party code
            ("absl_py", _) => {
                let absl_path = workspace_root.join("tensorflow/third_party/absl");
                if absl_path.exists() {
                    include_dirs.push(absl_path.clone());
                    eprintln!(
                        "[SYSMAP] Mapped @{}//{} -> {:?} (workspace absl)",
                        repo, target, absl_path
                    );
                }
            }
            ("com_google_googleapis", _) => {
                let googleapis_path = workspace_root.join("tensorflow/third_party/googleapis");
                if googleapis_path.exists() {
                    include_dirs.push(googleapis_path.clone());
                    eprintln!(
                        "[SYSMAP] Mapped @{}//{} -> {:?} (workspace googleapis)",
                        repo, target, googleapis_path
                    );
                }
            }
            ("com_github_grpc_grpc", _) => {
                let grpc_path = workspace_root.join("tensorflow/third_party/grpc");
                if grpc_path.exists() {
                    include_dirs.push(grpc_path.clone());
                    eprintln!(
                        "[SYSMAP] Mapped @{}//{} -> {:?} (workspace grpc)",
                        repo, target, grpc_path
                    );
                }
            }
            ("python", _) | ("pypi", _) => {
                let py_path = workspace_root.join("tensorflow/third_party/py");
                if py_path.exists() {
                    include_dirs.push(py_path.clone());
                    eprintln!(
                        "[SYSMAP] Mapped @{}//{} -> {:?} (workspace py)",
                        repo, target, py_path
                    );
                }
                let python_runtime = workspace_root.join("tensorflow/third_party/python_runtime");
                if python_runtime.exists() {
                    include_dirs.push(python_runtime.clone());
                    eprintln!(
                        "[SYSMAP] Mapped @{}//{} -> {:?} (workspace python_runtime)",
                        repo, target, python_runtime
                    );
                }
            }

            // Unknown external dependency - return error to trigger on-demand fetch
            _ => {
                eprintln!(
                    "[SYSMAP] ⚠️  Unknown external repository: @{}//{}",
                    repo, target
                );
                eprintln!("[SYSMAP]    Will attempt on-demand fetch");
                anyhow::bail!(
                    "Unknown external repository @{}//{} - not found in system packages",
                    repo,
                    target
                );
            }
        }

        Ok(include_dirs)
    }

    /// Check if system packages are installed
    pub fn check_system_packages() -> Vec<String> {
        let mut missing = Vec::new();

        // Check for protobuf
        if !PathBuf::from("/usr/include/google/protobuf/message.h").exists() {
            missing.push("protobuf-dev (or protobuf-devel)".to_string());
        }

        // Check for Eigen
        if !PathBuf::from("/usr/include/eigen3/Eigen/Core").exists() {
            missing.push("libeigen3-dev (or eigen3-devel)".to_string());
        }

        // Check for Abseil (comes with modern protobuf)
        if !PathBuf::from("/usr/include/absl/base/attributes.h").exists() {
            eprintln!("[SYSMAP] Note: Abseil headers not found, but may be included with protobuf");
        }

        missing
    }

    /// Get repository root path mappings for known external repositories
    ///
    /// This maps repository names to their actual filesystem locations.
    /// For system-package repos, we create stub repositories in the Rush cache.
    pub fn get_known_repository_paths(
        workspace_root: &Path,
    ) -> Result<std::collections::HashMap<String, PathBuf>> {
        let mut repo_paths = std::collections::HashMap::new();

        let external_cache = cache_paths::workspace_external_dir(workspace_root);

        // TF 2.17 (WORKSPACE-mode) tensorflow.bzl loads @rules_java//java:defs.bzl (java_test) at
        // the top level — new vs 2.15. Java is irrelevant to rush's C++ corpus, and materializing
        // real rules_java 5.3.5 would drag Java internals (java_common/JavaInfo native APIs) rush
        // doesn't model. Synthesize an @rules_java facade so the load succeeds. The five Java
        // build-target rules forward to native.java_* and therefore become fail-loud graph nodes;
        // JavaInfo and JavaPluginInfo remain load-time provider symbols. Gated by
        // RUSH_ENABLE_TF217_WORKSPACE_FACADES so the certified TF-2.15 corpus build is untouched.
        // (Codex 019eebc6.)
        if std::env::var("RUSH_ENABLE_TF217_WORKSPACE_FACADES").as_deref() == Ok("1") {
            let rj = materialize_tf217_rules_java_facade(&external_cache)?;
            repo_paths.insert("rules_java".to_string(), rj);
        }

        // @rules_cc//cc:defs.bzl is NOT TF-version-specific. Any BUILD file
        // may load cc_library/cc_binary/cc_test from it — TensorFlow's own
        // XLA/stablehlo packages do, and so does BoringSSL's upstream BUILD,
        // which TF 2.15 pins. The real defs.bzl pulls in
        // @bazel_tools//tools/cpp:cc_flags_supplier.bzl (the embedded Bazel
        // cc-toolchain machinery rush does not materialize), so rush used to
        // fail the load, stub the target, and continue — and the build then
        // died hundreds of archives later on undefined EVP_* symbols with
        // nothing naming BoringSSL. rush registers the C++ rules as builtins
        // anyway, so synthesize a facade that re-exports them via native.*
        // (deferred to call time, so a missing native.X never breaks a load).
        //
        // Only when nothing has already provided a real @rules_cc.
        if !repo_paths.contains_key("rules_cc") {
            let rc = external_cache.join("rules_cc");
            std::fs::create_dir_all(rc.join("cc"))?;
            std::fs::write(
                rc.join("BUILD"),
                "package(default_visibility = [\"//visibility:public\"])\n",
            )?;
            std::fs::write(rc.join("cc/BUILD"), "exports_files([\"defs.bzl\"])\n")?;
            std::fs::write(
                rc.join("cc/defs.bzl"),
                "# Synthesized by rush — @rules_cc//cc:defs.bzl facade re-exporting rush's native\n\
                 # C++ rules, bypassing the real defs.bzl's @bazel_tools//tools/cpp machinery.\n\
                 def cc_library(**kwargs):\n    native.cc_library(**kwargs)\n\
                 def cc_binary(**kwargs):\n    native.cc_binary(**kwargs)\n\
                 def cc_test(**kwargs):\n    native.cc_test(**kwargs)\n\
                 def cc_import(**kwargs):\n    native.cc_import(**kwargs)\n\
                 def objc_library(**kwargs):\n    native.objc_library(**kwargs)\n",
            )?;
            repo_paths.insert("rules_cc".to_string(), rc);
        }

        // Same story for @rules_proto//proto:defs.bzl. Riegeli — which
        // TensorFlow pins and tsl/platform reaches through record_reader —
        // loads proto_library from it. Without a facade that BUILD failed to
        // parse, rush stubbed @riegeli//riegeli/records:record_reader and
        // continued, and the build died with 52 undefined riegeli vtables.
        // rush registers proto_library and ProtoInfo as builtins already.
        if std::env::var("RUSH_FACADE_DEBUG").as_deref() == Ok("1") {
            eprintln!(
                "[facade] rules_proto already present={} path={:?}; rules_cc present={}",
                repo_paths.contains_key("rules_proto"),
                repo_paths.get("rules_proto"),
                repo_paths.contains_key("rules_cc"),
            );
        }
        if !repo_paths.contains_key("rules_proto") {
            let rp = external_cache.join("rules_proto");
            std::fs::create_dir_all(rp.join("proto"))?;
            std::fs::write(
                rp.join("BUILD"),
                "package(default_visibility = [\"//visibility:public\"])\n",
            )?;
            std::fs::write(rp.join("proto/BUILD"), "exports_files([\"defs.bzl\"])\n")?;
            std::fs::write(
                rp.join("proto/defs.bzl"),
                "# Synthesized by rush — @rules_proto//proto:defs.bzl facade re-exporting\n\
                 # rush's native proto rules, bypassing the real defs.bzl's toolchain machinery.\n\
                 def proto_library(**kwargs):\n    native.proto_library(**kwargs)\n\
                 def proto_lang_toolchain(**kwargs):\n    pass\n\
                 proto_common = struct()\n",
            )?;
            repo_paths.insert("rules_proto".to_string(), rp);
        }


        // Host-global clone cache: large external clones (eigen_archive,
        // llvm-project, ducc, …) are shared across
        // workspaces and live outside the workspace's `.cache/rush`.
        // `RUSH_GLOBAL_CLONE_CACHE` overrides the historical default of
        // `$XDG_CACHE_HOME/rush/external`. Stubs below use
        // `resolve_clone_path(name, witness)` to probe the workspace-local
        // slot first (so an explicit workspace-local clone wins), then fall
        // back to the global cache — returning whichever path has the
        // witness file.
        let global_clone_root = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| default_global_clone_cache_root());
        // AS (Pro K→H lane): also probe the workspace-external
        // directory `<workspace>/external/<name>` for vendored
        // clones. Several repos (@nsync and @eigen_archive) are laid down by the TF
        // checkout or a sibling tool into `tensorflow/external/`
        // rather than `.cache/rush/external/`. Without this probe,
        // the .cache generator emits an empty stub (e.g.
        // `includes = []` for @nsync) which then shadows the
        // workspace-external BUILD because register_cached_repo
        // checks .cache first. Probing workspace_external lets the
        // generator discover the vendored clone and emit a non-
        // empty stub with the correct include path.
        let workspace_external_root = workspace_root.join("external");
        let resolve_clone_path = |name: &str, witness: &str| -> Option<PathBuf> {
            // The local slot is the SAME directory the synthesis below
            // writes into. Symlinks the synthesis plants (ducc's google/,
            // fft2d's fftsg.c, linenoise's sources) made the repo satisfy
            // its own probe on the NEXT run, so the "clone" resolved to
            // the repo itself and the re-synthesis wrote self-referential
            // symlinks (src -> src), emptying the enumerated targets and
            // breaking the include farm. A directory only counts as a
            // local clone when the witness CANONICALIZES inside it - true
            // for a genuinely dropped-in clone, false for planted links,
            // and canonicalize itself fails on a self-loop.
            let local = external_cache.join(name);
            let local_witness = local.join(witness);
            if local_witness.exists() {
                let genuine = std::fs::canonicalize(&local_witness)
                    .ok()
                    .zip(std::fs::canonicalize(&local).ok())
                    .is_some_and(|(w, l)| w.starts_with(&l));
                if genuine {
                    return Some(local);
                }
            }
            let workspace = workspace_external_root.join(name);
            if workspace.join(witness).exists() {
                return Some(workspace);
            }
            let global = global_clone_root.join(name);
            if global.join(witness).exists() {
                return Some(global);
            }
            None
        };

        // Create stub repository for protobuf
        // TensorFlow loads @com_google_protobuf//:protobuf.bzl which doesn't exist in /usr/include
        // We create a minimal stub that provides empty symbols
        let protobuf_repo = external_cache.join("com_google_protobuf");
        std::fs::create_dir_all(&protobuf_repo)?;
        std::fs::write(
            protobuf_repo.join("protobuf.bzl"),
            "# Stub protobuf.bzl - Rush uses system protobuf\n\
             def cc_proto_library(**kwargs):\n    pass\n\n\
             def proto_gen(**kwargs):\n    pass\n\n\
             def py_proto_library(**kwargs):\n    pass\n\n\
             def internal_copied_filegroup(**kwargs):\n    pass\n\n\
             def internal_gen_well_known_protos_java(**kwargs):\n    pass\n\n\
             def internal_protobuf_py_tests(**kwargs):\n    pass\n",
        )?;

        // FIX #190: Create BUILD file for protobuf with system library support
        // Detect system protobuf installation for distribution-agnostic configuration
        let pb_config = detect_system_protobuf();

        // #32: prefer TF's PINNED protobuf 3.21.9 when materialized. TF 2.15
        // hard-requires protobuf 3.21.x; the system shared lib here is 4.25.x
        // (libprotobuf.so.32), whose stricter TextFormat parser rejects the
        // raw-newline string default of PrintV2's `end: string = '\n'` attr and
        // aborts the op-wrapper codegen tools at static REGISTER_OP. When
        // scripts/bootstrap_protobuf.sh has produced the static PIC archives,
        // link those (and use the pinned src as the include root) instead of the
        // system protobuf. Gated on the .a existing — HONEST: absent → fall back
        // to the prior system-protobuf behavior. Same materialize-real-lib
        // pattern as abseil/farmhash/nsync/ruy.
        let pinned_pb_dir = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_global_clone_cache_root())
            .join("com_google_protobuf");
        let pinned_pb_lib = pinned_pb_dir.join("libprotobuf.a");
        let pinned_pb_active = pinned_pb_lib.exists();
        // The pinned include root (written by the bootstrap as INCLUDE_ROOT, the
        // protobuf-3.21.9 `<src>/src` dir holding google/protobuf/*.h + the
        // well-known-type .proto). Fall back to <dir>/src if the marker is absent.
        let pinned_pb_include = if pinned_pb_active {
            std::fs::read_to_string(pinned_pb_dir.join("INCLUDE_ROOT"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| pinned_pb_dir.join("src").display().to_string())
        } else {
            String::new()
        };

        // A protobuf whose headers disagree with the protoc that will
        // generate the sources is a guaranteed, and famously confusing,
        // compile failure: every generated .pb.cc opens with a version
        // guard, so the error arrives as "This file was generated by a
        // newer version of protoc" on some unrelated-looking target long
        // after the real mistake. Check it here, where both versions are
        // known, and refuse rather than let a host copy stand in for a
        // declared dependency. Observed for real: a host with protobuf
        // 3.12 headers and protoc 3.21.9 on PATH produced 17 such
        // failures across a TensorFlow corpus run.
        let resolved_protoc = rush_rules::rules_system::protobuf_protoc_path();
        if let Some(skew) = protobuf_version_skew(
            &pinned_pb_include,
            pinned_pb_active,
            &pb_config,
            &resolved_protoc,
        ) {
            return Err(anyhow::anyhow!("{}", skew));
        }

        // Same contract for the pinned Abseil archives: a declared
        // dependency built by a foreign toolchain must not be linked.
        let absl_dir = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_global_clone_cache_root())
            .join("com_google_absl");
        if let Some(skew) = absl_toolchain_skew(&absl_dir, &resolved_cxx()) {
            return Err(anyhow::anyhow!("{}", skew));
        }

        // Format include directories and link flags for BUILD file
        let include_dirs_str = if pinned_pb_active {
            format!("\"{}\"", pinned_pb_include)
        } else if pb_config.found {
            pb_config
                .include_dirs
                .iter()
                .map(|p| format!("\"{}\"", p.display()))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            "\"/usr/include\"".to_string()
        };

        // The LITE runtime. protobuf's own BUILD exposes :protobuf_lite, and
        // consumers ask for it directly (riegeli's messages target does).
        // rush exposed no such label, so it resolved to nothing, was stubbed
        // empty, and the build carried on. The bootstrap already builds
        // libprotobuf-lite.a; fall back to the full archive, which is a
        // superset, when only that is present.
        let pinned_pb_lite = pinned_pb_dir.join("libprotobuf-lite.a");
        let lite_link_flags_str = if pinned_pb_active {
            let archive = if pinned_pb_lite.is_file() {
                pinned_pb_lite.clone()
            } else {
                pinned_pb_lib.clone()
            };
            format!("\"{}\", \"-lz\", \"-pthread\"", archive.display())
        } else if pb_config.found {
            pb_config
                .link_flags
                .iter()
                .map(|f| format!("\"{}\"", f.replace("-lprotobuf", "-lprotobuf-lite")))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            "\"-lprotobuf-lite\"".to_string()
        };

        let link_flags_str = if pinned_pb_active {
            // Absolute path to the pinned static archive + zlib + pthread (the
            // libs protobuf 3.21.9 needs). protobuf 3.21 is pre-abseil, so no
            // abseil link dep here.
            format!(
                "\"{}\", \"-lz\", \"-pthread\"",
                pinned_pb_lib.display()
            )
        } else if pb_config.found {
            pb_config
                .link_flags
                .iter()
                .map(|f| format!("\"{}\"", f))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            "\"-lprotobuf\"".to_string()
        };

        // protoc_lib link flags: pinned libprotoc.a + libprotobuf.a when active,
        // else the system `-lprotoc` plus the system protobuf link flags.
        let protoc_link_flags_str = if pinned_pb_active {
            format!(
                "\"{}\", \"{}\", \"-lz\", \"-pthread\"",
                pinned_pb_dir.join("libprotoc.a").display(),
                pinned_pb_lib.display()
            )
        } else {
            format!("\"-lprotoc\", {}", link_flags_str)
        };

        // Generate BUILD file with cc_library for system protobuf
        // Uses "includes" attribute to specify system include paths
        // and "linkopts" for link flags - both are standard Bazel attributes
        let protobuf_build = format!(
            r#"# System protobuf BUILD - Rush uses pkg-config detected system protobuf
# Auto-generated by Rush build system
# Include dirs detected: [{include_dirs}]
# Link flags detected: [{link_flags}]

# Main protobuf library - provides system headers and link flags
cc_library(
    name = "protobuf",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    linkopts = [{link_flags}],
    visibility = ["//visibility:public"],
)

# Protobuf LITE runtime - a real target upstream, asked for by name
cc_library(
    name = "protobuf_lite",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    linkopts = [{lite_link_flags}],
    visibility = ["//visibility:public"],
)

# Protobuf headers-only (alias to main library)
alias(
    name = "protobuf_headers",
    actual = ":protobuf",
    visibility = ["//visibility:public"],
)

# Protobuf compiler library
cc_library(
    name = "protoc_lib",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    linkopts = [{protoc_link_flags}],
    visibility = ["//visibility:public"],
)

# Python protobuf (stub - Rush handles Python differently)
py_library(
    name = "protobuf_python",
    srcs = [],
    visibility = ["//visibility:public"],
)

# Python well-known types genproto stub
filegroup(
    name = "well_known_types_py_pb2_genproto",
    srcs = [],
    visibility = ["//visibility:public"],
)

# Well-known proto types - these are pre-compiled in system protobuf
# They provide system headers from the protobuf include directory
cc_library(
    name = "any_proto",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "timestamp_proto",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "duration_proto",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "empty_proto",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "struct_proto",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "wrappers_proto",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "field_mask_proto",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "api_proto",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "type_proto",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "source_context_proto",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "compiler_plugin_proto",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "descriptor_proto",
    srcs = [],
    hdrs = [],
    includes = [{include_dirs}],
    visibility = ["//visibility:public"],
)

# CC versions of well-known types
alias(
    name = "cc_wkt_protos",
    actual = ":protobuf",
    visibility = ["//visibility:public"],
)

alias(
    name = "cc_wkt_protos_genproto",
    actual = ":protobuf",
    visibility = ["//visibility:public"],
)

# Aliases for well-known types with _cc, _cc_impl, and _genproto suffixes
# TensorFlow requests these variants
alias(name = "any_proto_cc", actual = ":any_proto")
alias(name = "any_proto_cc_impl", actual = ":any_proto")
alias(name = "any_proto_genproto", actual = ":any_proto")
alias(name = "timestamp_proto_cc", actual = ":timestamp_proto")
alias(name = "timestamp_proto_cc_impl", actual = ":timestamp_proto")
alias(name = "timestamp_proto_genproto", actual = ":timestamp_proto")
alias(name = "duration_proto_cc", actual = ":duration_proto")
alias(name = "duration_proto_cc_impl", actual = ":duration_proto")
alias(name = "duration_proto_genproto", actual = ":duration_proto")
alias(name = "empty_proto_cc", actual = ":empty_proto")
alias(name = "empty_proto_cc_impl", actual = ":empty_proto")
alias(name = "empty_proto_genproto", actual = ":empty_proto")
alias(name = "struct_proto_cc", actual = ":struct_proto")
alias(name = "struct_proto_cc_impl", actual = ":struct_proto")
alias(name = "struct_proto_genproto", actual = ":struct_proto")
alias(name = "wrappers_proto_cc", actual = ":wrappers_proto")
alias(name = "wrappers_proto_cc_impl", actual = ":wrappers_proto")
alias(name = "wrappers_proto_genproto", actual = ":wrappers_proto")
alias(name = "field_mask_proto_cc", actual = ":field_mask_proto")
alias(name = "field_mask_proto_cc_impl", actual = ":field_mask_proto")
alias(name = "field_mask_proto_genproto", actual = ":field_mask_proto")
alias(name = "api_proto_cc", actual = ":api_proto")
alias(name = "api_proto_cc_impl", actual = ":api_proto")
alias(name = "api_proto_genproto", actual = ":api_proto")
alias(name = "type_proto_cc", actual = ":type_proto")
alias(name = "type_proto_cc_impl", actual = ":type_proto")
alias(name = "type_proto_genproto", actual = ":type_proto")
alias(name = "source_context_proto_cc", actual = ":source_context_proto")
alias(name = "source_context_proto_cc_impl", actual = ":source_context_proto")
alias(name = "source_context_proto_genproto", actual = ":source_context_proto")
alias(name = "compiler_plugin_proto_cc", actual = ":compiler_plugin_proto")
alias(name = "compiler_plugin_proto_cc_impl", actual = ":compiler_plugin_proto")
alias(name = "compiler_plugin_proto_genproto", actual = ":compiler_plugin_proto")
alias(name = "descriptor_proto_cc", actual = ":descriptor_proto")
alias(name = "descriptor_proto_cc_impl", actual = ":descriptor_proto")
alias(name = "descriptor_proto_genproto", actual = ":descriptor_proto")
"#,
            include_dirs = include_dirs_str,
            link_flags = link_flags_str,
            lite_link_flags = lite_link_flags_str,
            protoc_link_flags = protoc_link_flags_str,
        );

        std::fs::write(protobuf_repo.join("BUILD"), protobuf_build)?;

        repo_paths.insert("com_google_protobuf".to_string(), protobuf_repo);

        // TensorFlow vendored repositories (in workspace)
        let local_tsl = workspace_root.join("third_party/xla/third_party/tsl");
        if local_tsl.exists() {
            repo_paths.insert("local_tsl".to_string(), local_tsl);
        }

        let local_xla = workspace_root.join("third_party/xla");
        if local_xla.exists() {
            repo_paths.insert("local_xla".to_string(), local_xla);
        }

        // FIX #205: FuzzTest stub - testing framework dependency
        let fuzztest_repo = external_cache.join("com_google_fuzztest");
        std::fs::create_dir_all(&fuzztest_repo)?;
        std::fs::write(
            fuzztest_repo.join("BUILD"),
            "# Stub FuzzTest library - Rush uses system fuzztest or skips tests\n\
             package(\n\
                 default_visibility = [\"//visibility:public\"],\n\
             )\n",
        )?;
        std::fs::write(
            fuzztest_repo
                .join("fuzztest")
                .with_extension("")
                .join("BUILD"),
            {
                std::fs::create_dir_all(fuzztest_repo.join("fuzztest"))?;
                "# Stub FuzzTest framework\n\
                 cc_library(\n\
                     name = \"fuzztest\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\
                 cc_library(\n\
                     name = \"fuzztest_gtest_main\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n"
            },
        )?;

        repo_paths.insert("com_google_fuzztest".to_string(), fuzztest_repo);

        // local_config_cuda adapter. CPU builds retain the historical disabled
        // stub. RUSH_ENABLE_CUDA=1 materializes a system-toolkit repository so
        // TensorFlow's CUDA selects, headers, and DSO dependencies become real.
        let cuda_enabled = std::env::var("RUSH_ENABLE_CUDA").as_deref() == Ok("1");
        let cuda_path = std::env::var("RUSH_CUDA_PATH")
            .or_else(|_| std::env::var("CUDA_HOME"))
            .or_else(|_| std::env::var("CUDA_PATH"))
            .unwrap_or_else(|_| "/usr/local/cuda".to_string());
        let cudnn_path = std::env::var("RUSH_CUDNN_PATH")
            .or_else(|_| std::env::var("CUDNN_INSTALL_PATH"))
            .unwrap_or_else(|_| cuda_path.clone());
        let cuda_version =
            std::env::var("RUSH_CUDA_VERSION").unwrap_or_else(|_| "12.2".to_string());
        let cudnn_version =
            std::env::var("RUSH_CUDNN_VERSION").unwrap_or_else(|_| "8".to_string());
        let cuda_arch =
            std::env::var("RUSH_CUDA_ARCH").unwrap_or_else(|_| "sm_80".to_string());
        let cuda_major_version = cuda_version
            .split('.')
            .next()
            .filter(|version| !version.is_empty())
            .unwrap_or("12")
            .to_string();
        let cuda_library_roots = [
            std::path::PathBuf::from(&cuda_path).join("targets/x86_64-linux/lib"),
            std::path::PathBuf::from(&cuda_path).join("lib64"),
            std::path::PathBuf::from(&cuda_path).join("extras/CUPTI/lib64"),
        ];
        let cuda_component_version =
            |env_name: &str, library_name: &str, fallback: &str| -> String {
                if let Ok(version) = std::env::var(env_name) {
                    return version;
                }

                let soname_prefix = format!("lib{library_name}.so.");
                for root in &cuda_library_roots {
                    let unversioned_library = root.join(format!("lib{library_name}.so"));
                    let Ok(target) = std::fs::read_link(unversioned_library) else {
                        continue;
                    };
                    let Some(target_name) = target.file_name().and_then(|name| name.to_str())
                    else {
                        continue;
                    };
                    let Some(version) = target_name
                        .strip_prefix(&soname_prefix)
                        .and_then(|suffix| suffix.split('.').next())
                        .filter(|version| {
                            !version.is_empty()
                                && version.chars().all(|character| character.is_ascii_digit())
                        })
                    else {
                        continue;
                    };
                    return version.to_string();
                }

                fallback.to_string()
            };
        let cudart_version =
            cuda_component_version("RUSH_CUDART_VERSION", "cudart", &cuda_major_version);
        let cupti_version =
            cuda_component_version("RUSH_CUPTI_VERSION", "cupti", &cuda_major_version);
        let cublas_version =
            cuda_component_version("RUSH_CUBLAS_VERSION", "cublas", &cuda_major_version);
        let cusolver_version =
            cuda_component_version("RUSH_CUSOLVER_VERSION", "cusolver", "11");
        let curand_version = cuda_component_version("RUSH_CURAND_VERSION", "curand", "10");
        let cufft_version = cuda_component_version("RUSH_CUFFT_VERSION", "cufft", "11");
        let cusparse_version =
            cuda_component_version("RUSH_CUSPARSE_VERSION", "cusparse", &cuda_major_version);
        let cuda_compute_capabilities = {
            let capabilities = cuda_arch
                .split(|character| character == ',' || character == ';')
                .filter_map(|architecture| {
                    let architecture = architecture
                        .trim()
                        .strip_prefix("sm_")
                        .or_else(|| architecture.trim().strip_prefix("compute_"))
                        .unwrap_or_else(|| architecture.trim());
                    let capability = architecture.replace('.', "");
                    if !capability.is_empty()
                        && capability
                            .chars()
                            .all(|character| character.is_ascii_digit())
                    {
                        Some(capability)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            if capabilities.is_empty() {
                "80".to_string()
            } else {
                capabilities.join(", ")
            }
        };
        let local_config_cuda_repo = external_cache.join("local_config_cuda");
        std::fs::create_dir_all(local_config_cuda_repo.join("cuda"))?;

        // Materialize the CUDA-disabled `cuda_config.h`.
        // TF's dso_loader.cc does `#include "third_party/gpus/cuda/cuda_config.h"`
        // which would normally be produced by the user running
        // `configure.py`. We ship a zero-CUDA version inside the stub
        // at `tf_config_root/third_party/gpus/cuda/cuda_config.h`, and
        // shared_include_dirs_for_workspace surfaces
        // `tf_config_root` on -I for `local_config_cuda` so the include
        // resolves.
        let cuda_config_root = local_config_cuda_repo
            .join("tf_config_root/third_party/gpus/cuda");
        std::fs::create_dir_all(&cuda_config_root)?;
        if cuda_enabled {
            // Bazel's cuda_header_library creates virtual include trees for
            // headers referenced as third_party/gpus/{cuda,cudnn}/.... Rush
            // propagates the repository's tf_config_root directly, so mirror
            // those virtual trees with symlinks into the installed toolkits.
            #[cfg(unix)]
            {
                let virtual_header_roots = [
                    (
                        cuda_config_root.join("include"),
                        std::path::PathBuf::from(&cuda_path).join("include"),
                    ),
                    (
                        cuda_config_root.join("extras/CUPTI/include"),
                        std::path::PathBuf::from(&cuda_path).join("extras/CUPTI/include"),
                    ),
                    (
                        cuda_config_root.join("nvml/include"),
                        std::path::PathBuf::from(&cuda_path).join("include"),
                    ),
                    (
                        local_config_cuda_repo
                            .join("tf_config_root/third_party/gpus/cudnn"),
                        std::path::PathBuf::from(&cudnn_path).join("include"),
                    ),
                ];

                for (link, target) in virtual_header_roots {
                    if let Ok(metadata) = std::fs::symlink_metadata(&link) {
                        if metadata.file_type().is_symlink() || metadata.is_file() {
                            std::fs::remove_file(&link)?;
                        } else {
                            std::fs::remove_dir_all(&link)?;
                        }
                    }
                    if let Some(parent) = link.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::os::unix::fs::symlink(target, link)?;
                }
            }
        }
        let cuda_config = if cuda_enabled {
            format!(
                "// Auto-generated rush CUDA configuration\n\
                 #ifndef CUDA_CUDA_CONFIG_H_\n\
                 #define CUDA_CUDA_CONFIG_H_\n\
                 #define TF_CUDA_VERSION \"{cuda_version}\"\n\
                 #define TF_CUDART_VERSION \"{cudart_version}\"\n\
                 #define TF_CUPTI_VERSION \"{cupti_version}\"\n\
                 #define TF_CUBLAS_VERSION \"{cublas_version}\"\n\
                 #define TF_CUSOLVER_VERSION \"{cusolver_version}\"\n\
                 #define TF_CURAND_VERSION \"{curand_version}\"\n\
                 #define TF_CUFFT_VERSION \"{cufft_version}\"\n\
                 #define TF_CUSPARSE_VERSION \"{cusparse_version}\"\n\
                 #define TF_CUDNN_VERSION \"{cudnn_version}\"\n\
                 #define TF_CUDA_TOOLKIT_PATH \"{cuda_path}\"\n\
                 #define TF_CUDA_COMPUTE_CAPABILITIES {cuda_compute_capabilities}\n\
                 #endif  // CUDA_CUDA_CONFIG_H_\n"
            )
        } else {
            "// Auto-generated rush stub for TF's cuda_config.h\n\
             // (user hasn't enabled CUDA; build assumes CUDA disabled).\n\
             #ifndef CUDA_CUDA_CONFIG_H_\n\
             #define CUDA_CUDA_CONFIG_H_\n\
             #define TF_CUDA_VERSION \"\"\n\
             #define TF_CUDART_VERSION \"\"\n\
             #define TF_CUPTI_VERSION \"\"\n\
             #define TF_CUBLAS_VERSION \"\"\n\
             #define TF_CUSOLVER_VERSION \"\"\n\
             #define TF_CURAND_VERSION \"\"\n\
             #define TF_CUFFT_VERSION \"\"\n\
             #define TF_CUSPARSE_VERSION \"\"\n\
             #define TF_CUDNN_VERSION \"\"\n\
             #define TF_CUDA_TOOLKIT_PATH \"\"\n\
             #define TF_CUDA_COMPUTE_CAPABILITIES 0\n\
             #endif  // CUDA_CUDA_CONFIG_H_\n"
                .to_string()
        };
        std::fs::write(cuda_config_root.join("cuda_config.h"), cuda_config)?;
        let cuda_root_build = if cuda_enabled {
            "# Rush CUDA configuration repository\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             config_setting(\n\
                 name = \"enable_cuda\",\n\
                 define_values = {\"CUDA_ENABLED\": \"1\"},\n\
             )\n\
             config_setting(\n\
                 name = \"is_cuda_enabled\",\n\
                 define_values = {\"CUDA_ENABLED\": \"1\"},\n\
             )\n\
             config_setting(\n\
                 name = \"is_cuda_compiler_clang\",\n\
                 define_values = {\"CUDA_COMPILER\": \"clang\"},\n\
             )\n\
             config_setting(\n\
                 name = \"is_cuda_compiler_nvcc\",\n\
                 define_values = {\"CUDA_COMPILER\": \"nvcc\"},\n\
             )\n"
        } else {
            "# Stub BUILD file for local_config_cuda (CUDA disabled)\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             config_setting(\n\
                 name = \"is_cuda_enabled\",\n\
                 define_values = {\"CUDA_ENABLED\": \"1\"},\n\
             )\n\n\
             config_setting(\n\
                 name = \"is_cuda_compiler_clang\",\n\
                 define_values = {\"CUDA_COMPILER\": \"clang\"},\n\
             )\n\n\
             config_setting(\n\
                 name = \"is_cuda_compiler_nvcc\",\n\
                 define_values = {\"CUDA_COMPILER\": \"nvcc\"},\n\
             )\n"
        };
        std::fs::write(local_config_cuda_repo.join("BUILD"), cuda_root_build)?;
        // `cuda_headers` is a cc_library (not a filegroup) so its
        // CcInfo.include_dirs can carry the tf_config_root path that
        // holds our stubbed cuda_config.h. Downstream cc_library compiles
        // (e.g. tsl/platform/default:dso_loader) pick it up through the
        // orchestrator's transitive CcInfo propagation.
        let cuda_cfg_root_str = local_config_cuda_repo
            .join("tf_config_root")
            .display()
            .to_string();
        let cuda_build = if cuda_enabled {
            r#"# Rush system CUDA package
package(default_visibility = ["//visibility:public"])

config_setting(name = "using_clang", define_values = {"CUDA_COMPILER": "clang"})
config_setting(name = "using_nvcc", define_values = {"CUDA_COMPILER": "nvcc"})

cc_library(
    name = "cuda_headers",
    includes = ["@CUDA_INCLUDE@", "@CUDA_CONFIG_ROOT@"],
)
cc_library(name = "cublas_headers", includes = ["@CUDA_INCLUDE@"], deps = [":cuda_headers"])
cc_library(name = "cusolver_headers", includes = ["@CUDA_INCLUDE@"], deps = [":cuda_headers"])
cc_library(name = "cufft_headers", includes = ["@CUDA_INCLUDE@"], deps = [":cuda_headers"])
cc_library(name = "cusparse_headers", includes = ["@CUDA_INCLUDE@"], deps = [":cuda_headers"])
cc_library(name = "curand_headers", includes = ["@CUDA_INCLUDE@"], deps = [":cuda_headers"])
cc_library(name = "cudnn_header", includes = ["@CUDNN_INCLUDE@"], deps = [":cuda_headers"])
cc_library(name = "cupti_headers", includes = ["@CUDA_CUPTI_INCLUDE@"], deps = [":cuda_headers"])
cc_library(name = "nvml_headers", includes = ["@CUDA_NVML_INCLUDE@"], deps = [":cuda_headers"])

cc_library(name = "cudart_static", linkopts = ["-L@CUDA_LIB@", "-Wl,-rpath,@CUDA_LIB@", "-lcudart", "-ldl", "-lpthread", "-lrt"])
cc_library(name = "cudart", linkopts = ["-L@CUDA_LIB@", "-Wl,-rpath,@CUDA_LIB@", "-lcudart"])
cc_library(name = "cuda_driver", linkopts = ["-L@CUDA_STUB_LIB@", "-lcuda"])
cc_library(name = "cublas", linkopts = ["-L@CUDA_LIB@", "-Wl,-rpath,@CUDA_LIB@", "-lcublas"])
cc_library(name = "cublasLt", linkopts = ["-L@CUDA_LIB@", "-Wl,-rpath,@CUDA_LIB@", "-lcublasLt"])
cc_library(name = "cusolver", linkopts = ["-L@CUDA_LIB@", "-Wl,-rpath,@CUDA_LIB@", "-lcusolver", "-lgomp"])
cc_library(name = "cudnn", linkopts = ["-L@CUDNN_LIB@", "-Wl,-rpath,@CUDNN_LIB@", "-lcudnn"])
cc_library(name = "cufft", linkopts = ["-L@CUDA_LIB@", "-Wl,-rpath,@CUDA_LIB@", "-lcufft"])
cc_library(name = "curand", linkopts = ["-L@CUDA_LIB@", "-Wl,-rpath,@CUDA_LIB@", "-lcurand"])
cc_library(name = "cusparse", linkopts = ["-L@CUDA_LIB@", "-Wl,-rpath,@CUDA_LIB@", "-lcusparse", "-lgomp"])
cc_library(name = "cupti_dsos", linkopts = ["-L@CUDA_CUPTI_LIB@", "-Wl,-rpath,@CUDA_CUPTI_LIB@", "-lcupti"])

cc_library(
    name = "cuda",
    deps = [
        ":cublas",
        ":cublasLt",
        ":cuda_headers",
        ":cudart",
        ":cudnn",
        ":cufft",
        ":curand",
    ],
)
alias(name = "cub_headers", actual = ":cuda_headers")
filegroup(name = "libdevice_root", srcs = ["@CUDA_LIBDEVICE@"])
"#
            .replace("@CUDA_INCLUDE@", &format!("{cuda_path}/include"))
            .replace("@CUDA_CONFIG_ROOT@", &cuda_cfg_root_str)
            .replace("@CUDNN_INCLUDE@", &format!("{cudnn_path}/include"))
            .replace(
                "@CUDA_CUPTI_INCLUDE@",
                &format!("{cuda_path}/extras/CUPTI/include"),
            )
            .replace("@CUDA_NVML_INCLUDE@", &format!("{cuda_path}/include"))
            .replace("@CUDA_LIB@", &format!("{cuda_path}/lib64"))
            .replace(
                "@CUDA_STUB_LIB@",
                &format!("{cuda_path}/lib64/stubs"),
            )
            .replace("@CUDNN_LIB@", &format!("{cudnn_path}/lib"))
            .replace(
                "@CUDA_CUPTI_LIB@",
                &format!("{cuda_path}/extras/CUPTI/lib64"),
            )
            .replace(
                "@CUDA_LIBDEVICE@",
                &format!("{cuda_path}/nvvm/libdevice/libdevice.10.bc"),
            )
        } else {
            format!(
                "# Stub BUILD for local_config_cuda/cuda\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 config_setting(name = \"using_clang\", define_values = {{\"CUDA_COMPILER\": \"clang\"}})\n\
                 config_setting(name = \"using_nvcc\", define_values = {{\"CUDA_COMPILER\": \"nvcc\"}})\n\
                 cc_library(\n\
                     name = \"cuda_headers\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     includes = [\"{cfg_root}\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\
                 cc_library(name = \"cudart_static\", srcs = [], hdrs = [])\n\
                 cc_library(name = \"cudart\", srcs = [], hdrs = [])\n\
                 cc_library(name = \"cuda_driver\", srcs = [], hdrs = [])\n",
                cfg_root = cuda_cfg_root_str
            )
        };
        std::fs::write(local_config_cuda_repo.join("cuda/BUILD"), cuda_build)?;
        // Use a raw string literal (r#"..."#) so function-body indentation
        // survives intact. Rust's `\n\` line-continuation strips leading
        // whitespace on the next line, turning `def f():\n\    pass` into
        // `def f():\npass` — invalid Starlark (Expected Indent, got Return).
        // TF's tf_gpu_kernel_library / tf_cuda_library route GPU sources through
        // cuda_library. Lower it to native cc_library for either GPU backend.
        let rocm_enabled = std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1");
        let cuda_library_def = if rocm_enabled || cuda_enabled {
            "def cuda_library(**kwargs):\n    native.cc_library(**kwargs)\n"
        } else {
            "def cuda_library(**kwargs):\n    pass\n"
        };
        let cuda_build_defs = if cuda_enabled {
            r#"# build_defs.bzl for Rush CUDA builds

def if_cuda(if_true, if_false = []):
    return if_true

def if_cuda_is_configured(x, no_cuda = []):
    return x

def if_cuda_exec(if_true, if_false = []):
    return if_true

def if_cuda_clang(if_true, if_false = []):
    return if_true

def if_cuda_clang_opt(if_true, if_false = []):
    return if_true

def if_cuda_toolkit_path(if_true, if_false = []):
    return if_true

def cuda_compiler(if_cuda_clang, if_nvcc, neither = []):
    return if_cuda_clang

def cuda_library(**kwargs):
    native.cc_library(**kwargs)

def cuda_header_library(name, hdrs = [], deps = [], **kwargs):
    native.cc_library(name = name, hdrs = hdrs, deps = deps, **kwargs)

def cuda_cc_test(**kwargs):
    native.cc_test(**kwargs)

def cuda_default_copts():
    return ["-DGOOGLE_CUDA=1"]

def cuda_gpu_architectures():
    return ["@CUDA_ARCH@"]

def enable_cuda_flag(name, **kwargs):
    native.config_setting(name = name, define_values = {"CUDA_ENABLED": "1"})

def if_cuda_newer_than(wanted_ver, if_true, if_false = []):
    return if_true
"#
            .replace("@CUDA_ARCH@", &cuda_arch)
        } else {
            format!(
            "{prefix}{cuda_library}{suffix}",
            prefix = r#"# build_defs.bzl for local_config_cuda (CUDA disabled; ROCm gated)

def if_cuda(if_true, if_false = []):
    return select({
        "@local_config_cuda//:is_cuda_enabled": if_true,
        "//conditions:default": if_false,
    })

def if_cuda_is_configured(x, no_cuda = []):
    # CUDA is disabled in this build — always return the no_cuda branch.
    return no_cuda

def if_cuda_exec(if_true, if_false = []):
    # CUDA-at-exec-time branch, TF's tensorflow.bzl uses this to add
    # `-DGOOGLE_CUDA=1` to copts. Since CUDA is disabled, always
    # return the if_false branch.
    return if_false

def if_cuda_clang(if_true, if_false = []):
    return select({
        "@local_config_cuda//:is_cuda_compiler_clang": if_true,
        "//conditions:default": if_false,
    })

def if_cuda_clang_opt(if_true, if_false = []):
    return if_false

"#,
            cuda_library = cuda_library_def,
            suffix = r#"
def cuda_default_copts():
    return []

def cuda_gpu_architectures():
    return []

def cuda_header_library(**kwargs):
    native.cc_library(**kwargs)

def enable_cuda_flag(**kwargs):
    pass

def if_cuda_newer_than(wanted_ver, if_true, if_false = []):
    # CUDA is disabled (ROCm path) — it is never "newer than" any wanted version,
    # so always take the if_false branch (matches the TF/XLA build_defs.bzl.tpl signature).
    return if_false
"#,
            )
        };
        std::fs::write(
            local_config_cuda_repo.join("cuda/build_defs.bzl"),
            cuda_build_defs,
        )?;
        repo_paths.insert("local_config_cuda".to_string(), local_config_cuda_repo);

        // FIX: local_config_nccl stub (NCCL disabled builds)
        let local_config_nccl_repo = external_cache.join("local_config_nccl");
        std::fs::create_dir_all(&local_config_nccl_repo)?;
        let nccl_cfg_dir = local_config_nccl_repo
            .join("tf_config_root/third_party/nccl");
        std::fs::create_dir_all(&nccl_cfg_dir)?;
        std::fs::write(
            nccl_cfg_dir.join("nccl_config.h"),
            "// Rush stub for nccl_config.h (NCCL disabled)\n\
             #ifndef NCCL_NCCL_CONFIG_H_\n\
             #define NCCL_NCCL_CONFIG_H_\n\
             #define TF_NCCL_VERSION \"\"\n\
             #endif\n",
        )?;
        let nccl_cfg_root_str = local_config_nccl_repo
            .join("tf_config_root")
            .display()
            .to_string();
        std::fs::write(
            local_config_nccl_repo.join("BUILD"),
            format!(
                "# Stub BUILD for local_config_nccl (NCCL disabled)\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                     name = \"nccl_config\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     includes = [\"{cfg}\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n",
                cfg = nccl_cfg_root_str
            ),
        )?;
        repo_paths.insert("local_config_nccl".to_string(), local_config_nccl_repo);

        // FIX: local_config_tensorrt stub (TensorRT disabled builds)
        let local_config_tensorrt_repo = external_cache.join("local_config_tensorrt");
        std::fs::create_dir_all(&local_config_tensorrt_repo)?;
        // Materialize a TensorRT-disabled config header at
        // tf_config_root/third_party/tensorrt/tensorrt_config.h for dso_loader.cc.
        let tensorrt_cfg_dir = local_config_tensorrt_repo
            .join("tf_config_root/third_party/tensorrt");
        std::fs::create_dir_all(&tensorrt_cfg_dir)?;
        std::fs::write(
            tensorrt_cfg_dir.join("tensorrt_config.h"),
            "// Rush stub for tensorrt_config.h (TensorRT disabled)\n\
             #ifndef TENSORRT_TENSORRT_CONFIG_H_\n\
             #define TENSORRT_TENSORRT_CONFIG_H_\n\
             #define TF_TENSORRT_VERSION \"\"\n\
             #endif\n",
        )?;
        let tensorrt_cfg_root_str = local_config_tensorrt_repo
            .join("tf_config_root")
            .display()
            .to_string();
        std::fs::write(
            local_config_tensorrt_repo.join("BUILD"),
            format!(
                "# Stub BUILD for local_config_tensorrt (TensorRT disabled)\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 config_setting(\n\
                     name = \"is_tensorrt_enabled\",\n\
                     define_values = {{\"TENSORRT_ENABLED\": \"1\"}},\n\
                 )\n\
                 cc_library(\n\
                     name = \"tensorrt_headers\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     includes = [\"{cfg}\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\
                 cc_library(name = \"tensorrt_lib\", srcs = [], hdrs = [])\n",
                cfg = tensorrt_cfg_root_str
            ),
        )?;
        std::fs::write(
            local_config_tensorrt_repo.join("build_defs.bzl"),
            r#"# Stub build_defs.bzl for local_config_tensorrt

def if_tensorrt(if_true, if_false = []):
    return select({
        "@local_config_tensorrt//:is_tensorrt_enabled": if_true,
        "//conditions:default": if_false,
    })

def if_tensorrt_exec(if_true, if_false = []):
    # TensorRT-at-exec-time branch, TF's tensorflow.bzl uses this
    # to add `-DGOOGLE_TENSORRT=1` to copts. TensorRT is disabled,
    # always return the if_false branch.
    return if_false
"#,
        )?;
        repo_paths.insert(
            "local_config_tensorrt".to_string(),
            local_config_tensorrt_repo,
        );

        // local_config_rocm — STUB by default (ROCm disabled), or a REAL ROCm config
        // facade under RUSH_ENABLE_ROCM=1 (prune's AMD W7900 / gfx1100 + ROCm 6.4.3 at
        // /opt/rocm). rush calls hipcc directly (the RUSH_GPU_CXX toolchain swap), so no
        // Bazel cc_toolchain/crosstool is modeled — this repo just supplies the if_rocm/
        // rocm_copts/version/arch values + the hip headers/link target. Gated so CPU/463
        // and the RUSH_ENABLE_BZLMOD protobuf path are unchanged.
        let rocm_enabled = std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1");
        let local_config_rocm_repo = external_cache.join("local_config_rocm");
        std::fs::create_dir_all(local_config_rocm_repo.join("rocm"))?;
        // Materialize rocm_config.h at rocm_config_root/rocm/rocm_config.h
        // for dso_loader.cc's `#include "rocm/rocm_config.h"`.
        let rocm_cfg_dir = local_config_rocm_repo.join("rocm_config_root/rocm");
        std::fs::create_dir_all(&rocm_cfg_dir)?;
        let rocm_config_h = if rocm_enabled {
            // TF_ROCM_VERSION / TF_MIOPEN_VERSION / TF_HIPBLASLT are used in #if
            // preprocessor expressions (e.g. dso_loader.cc: `#if TF_ROCM_VERSION >=
            // 40500`), so they MUST be integer tokens, not string literals — matching
            // TF's rocm_config.h.tpl (`#define TF_ROCM_VERSION %{rocm_version_number}`).
            // Only the toolkit path / arch list are strings.
            "// Rush ROCm config (RUSH_ENABLE_ROCM=1) — W7900/gfx1100, ROCm 6.4.3\n\
             #ifndef ROCM_ROCM_CONFIG_H_\n\
             #define ROCM_ROCM_CONFIG_H_\n\
             #define TF_ROCM_VERSION 60403\n\
             #define TF_ROCM_TOOLKIT_PATH \"/opt/rocm\"\n\
             #define TF_MIOPEN_VERSION 0\n\
             #define TF_HIPBLASLT 0\n\
             #define TF_ROCM_AMDGPU_TARGETS \"gfx1100\"\n\
             #endif\n"
        } else {
            "// Rush stub for rocm_config.h (ROCm disabled)\n\
             #ifndef ROCM_ROCM_CONFIG_H_\n\
             #define ROCM_ROCM_CONFIG_H_\n\
             #define TF_ROCM_VERSION \"\"\n\
             #define TF_ROCM_TOOLKIT_PATH \"\"\n\
             #define TF_MIOPEN_VERSION \"\"\n\
             #endif\n"
        };
        std::fs::write(rocm_cfg_dir.join("rocm_config.h"), rocm_config_h)?;
        // TF's GPU headers reach the ROCm toolkit via a `rocm/include/...` prefix
        // (gpu_device_functions.h: `#include "rocm/include/hip/hip_complex.h"`, stream_executor
        // rocm_driver / gpu_types.h: `#include "rocm/include/hip/hip_runtime.h"`) — Bazel's
        // @local_config_rocm symlinks the toolkit headers there. We mirror /opt/rocm/include under
        // rocm_config_root/rocm/include (which is already on -I before /opt/rocm/include), but
        // OVERLAY exactly one header — hip/hip_runtime.h — to fix the ROCm-6 ABI skew, while every
        // other header stays a symlink to the real toolkit (so kernels/codegen don't regress).
        //
        // The skew: TF 2.15's rocm/ backend uses APIs ROCm 6 versioned away — notably
        // hipDeviceProp_t::gcnArch (the int ISA), dropped from hipDeviceProp_tR0600 (ROCm 6 keeps
        // only the string gcnArchName). ROCm still SHIPS + EXPORTS the legacy hipDeviceProp_tR0000
        // (with gcnArch) and hipGetDevicePropertiesR0000 (verified via nm on libamdhip64). The
        // overlay includes the REAL HIP runtime then remaps hipDeviceProp_t -> the legacy R0000
        // struct AND hipGetDeviceProperties -> the matching R0000 fill function (remapped together,
        // so struct+writer stay ABI-consistent). This is FAITHFUL — it uses ROCm's real exported
        // legacy ABI, not a fake field, and patches no TF source. A header overlay (not a blunt
        // -include) preserves the includer's macro env (e.g. __HIP_DISABLE_CPP_FUNCTIONS__ that
        // rocm_driver_wrapper.h sets). The internal `hip/...` includes of the real runtime resolve
        // to /opt/rocm/include as usual.
        let rocm_overlay_header = "#pragma once\n\
             // rush ROCm-6 legacy-ABI compat overlay — see external_deps.rs\n\
             #include \"rocm/rocm_config.h\"\n\
             #include \"/opt/rocm/include/hip/hip_runtime.h\"\n\
             #if defined(TENSORFLOW_USE_ROCM) && TENSORFLOW_USE_ROCM && (TF_ROCM_VERSION >= 60000)\n\
             #include \"/opt/rocm/include/hip/hip_deprecated.h\"\n\
             #undef hipDeviceProp_t\n\
             #define hipDeviceProp_t hipDeviceProp_tR0000\n\
             #undef hipGetDeviceProperties\n\
             #define hipGetDeviceProperties hipGetDevicePropertiesR0000\n\
             #endif\n";
        // rocm_compat_root kept for the rocm_headers/lib `includes` (best-effort "hip/..." style).
        let rocm_compat_root = local_config_rocm_repo.join("rocm_compat");
        let rocm_compat_root_str = rocm_compat_root.display().to_string();
        if rocm_enabled {
            // TF 2.17+ is ROCm-6-NATIVE: its rocm_driver.cc reads hipDeviceProp_t::gcnArchName
            // (the R0600 struct), so the legacy-ABI R0000 remap overlay below would actively BREAK
            // it (R0000 has no gcnArchName). Only TF 2.15 (which reads the dropped int gcnArch)
            // needs the overlay. RUSH_ENABLE_TF217_WORKSPACE_FACADES is set for every 2.17 build →
            // use it to skip the remap and mirror the REAL hip_runtime.h. (Codex 019eebc6.)
            let tf217 = std::env::var("RUSH_ENABLE_TF217_WORKSPACE_FACADES").is_ok();
            // Build the rocm/include mirror once (sentinel-gated; bump the sentinel name if the
            // overlay content changes). rocm_inc = rocm_config_root/rocm/include.
            let rocm_inc = rocm_cfg_dir.join("include");
            let sentinel = rocm_inc.join(if tf217 { ".rush_mirror_v4_native" } else { ".rush_mirror_v2_overlay" });
            if !sentinel.exists() {
                let _ = std::fs::remove_dir_all(&rocm_inc);
                let _ = std::fs::remove_file(&rocm_inc); // drop any prior blanket symlink
                std::fs::create_dir_all(rocm_inc.join("hip"))?;
                let real_inc = std::path::Path::new("/opt/rocm/include");
                if let Ok(entries) = std::fs::read_dir(real_inc) {
                    for e in entries.flatten() {
                        if e.file_name() == *std::ffi::OsStr::new("hip") {
                            continue; // real dir built below (overlay + symlinks)
                        }
                        let _ = std::os::unix::fs::symlink(e.path(), rocm_inc.join(e.file_name()));
                    }
                }
                // ROCm 6 relocated each component's umbrella header into a per-component subdir
                // (/opt/rocm/include/rocblas/rocblas.h, hipblas/hipblas.h, …), but TF 2.17 still
                // includes them TOP-level as `rocm/include/<comp>.h` (tensorflow/core/util/
                // gpu_solvers.h + the rocm StreamExecutor: rocblas/rocsolver/hipsolver/hipsparse/
                // hipfft/hipblaslt/…). Add a top-level compat symlink rocm/include/<comp>.h ->
                // <comp>/<comp>.h for every relocated component (general, no hardcoded list). (Codex 019eebc6.)
                if let Ok(entries) = std::fs::read_dir(real_inc) {
                    for e in entries.flatten() {
                        if e.path().is_dir() {
                            let comp = e.file_name();
                            let header = e.path().join(&comp).with_extension("h");
                            if header.is_file() {
                                // FORWARDING header, NOT a symlink to the deep file: rocblas.h does
                                // `#include "internal/rocblas-auxiliary.h"` (relative to its OWN dir).
                                // A top-level symlink would make gcc resolve internal/ against
                                // rocm/include/ (no internal/ there) → not found. A forwarding header
                                // at rocm/include/<comp>.h that does `#include "<comp>/<comp>.h"` keeps
                                // the real header processed in its real dir (the <comp>/ subdir is
                                // already mirrored), so its relative internal/ includes resolve. (Codex 019eebc6.)
                                if let Some(c) = comp.to_str() {
                                    let mut top = comp.to_os_string();
                                    top.push(".h");
                                    let _ = std::fs::write(
                                        rocm_inc.join(top),
                                        format!("#pragma once\n#include \"{c}/{c}.h\"\n"),
                                    );
                                }
                            }
                        }
                    }
                }
                // 2.15: overlay hip_runtime.h with the R0000 remap. 2.17: mirror the real header.
                if !tf217 {
                    std::fs::write(rocm_inc.join("hip").join("hip_runtime.h"), rocm_overlay_header)?;
                }
                if let Ok(entries) = std::fs::read_dir(real_inc.join("hip")) {
                    for e in entries.flatten() {
                        if !tf217 && e.file_name() == *std::ffi::OsStr::new("hip_runtime.h") {
                            continue; // overlaid above (2.15 only)
                        }
                        let _ = std::os::unix::fs::symlink(
                            e.path(),
                            rocm_inc.join("hip").join(e.file_name()),
                        );
                    }
                }
                let _ = std::fs::write(&sentinel, "1\n");
            }
            // Best-effort overlay under the "hip/..." prefix root too (harmless redundancy).
            // 2.17-native skips it (no remap).
            if !tf217 {
                for sub in ["hip", "rocm/include/hip"] {
                    let dir = rocm_compat_root.join(sub);
                    std::fs::create_dir_all(&dir)?;
                    std::fs::write(dir.join("hip_runtime.h"), rocm_overlay_header)?;
                }
            }
        }
        let rocm_cfg_root_str = local_config_rocm_repo
            .join("rocm_config_root")
            .display()
            .to_string();
        std::fs::write(
            local_config_rocm_repo.join("BUILD"),
            "# Stub BUILD for local_config_rocm (ROCm disabled)\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             config_setting(\n\
                 name = \"is_rocm_enabled\",\n\
                 define_values = {\"ROCM_ENABLED\": \"1\"},\n\
             )\n\
             filegroup(name = \"rocm_headers\", srcs = [])\n",
        )?;
        let rocm_rocm_build = if rocm_enabled {
            // The @local_config_rocm//rocm: math/util lib targets the XLA StreamExecutor ROCm
            // backend depends on (exact names grepped from xla/stream_executor/**/BUILD). Each
            // maps to a system .so under /opt/rocm/lib. Sonames depend on the installed ROCm
            // 6.4.3) — bare -l names resolve via the dev .so symlinks (NO-STUBS: no guessing).
            // -Wl,-rpath so the produced GPU test binary finds the .so at runtime.
            let rocm_libs: &[(&str, &[&str])] = &[
                ("rocblas", &["-lrocblas"]),
                ("miopen", &["-lMIOpen"]),
                ("hipfft", &["-lhipfft"]),
                ("hiprand", &["-lhiprand"]),
                ("hipsparse", &["-lhipsparse"]),
                ("hipsolver", &["-lhipsolver"]),
                ("rocsolver", &["-lrocsolver"]),
                ("hipblaslt", &["-lhipblaslt", "-lhipblas"]),
                ("roctracer", &["-lroctracer64", "-lroctx64"]),
            ];
            let mut libs_build = String::new();
            for (name, lflags) in rocm_libs {
                let mut linkopts = String::from("\"-L/opt/rocm/lib\"");
                for lf in *lflags {
                    linkopts.push_str(&format!(", \"{}\"", lf));
                }
                linkopts.push_str(", \"-Wl,-rpath,/opt/rocm/lib\"");
                libs_build.push_str(&format!(
                    "cc_library(\n    name = \"{name}\",\n    srcs = [],\n    hdrs = [],\n    \
                     includes = [\"{compat}\", \"{cfg}\", \"/opt/rocm/include\"],\n    linkopts = [{linkopts}],\n    \
                     visibility = [\"//visibility:public\"],\n)\n",
                    name = name, compat = rocm_compat_root_str, cfg = rocm_cfg_root_str, linkopts = linkopts
                ));
            }
            format!(
                "# Rush ROCm config BUILD (RUSH_ENABLE_ROCM=1)\n\
                 package(default_visibility = [\"//visibility:public\"])\n\
                 cc_library(\n\
                     name = \"rocm_headers\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     includes = [\"{compat}\", \"{cfg}\", \"/opt/rocm/include\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\
                 cc_library(\n\
                     name = \"rocm\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     linkopts = [\"-L/opt/rocm/lib\", \"-lamdhip64\", \"-Wl,-rpath,/opt/rocm/lib\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\
                 cc_library(\n\
                     name = \"hip\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     linkopts = [\"-L/opt/rocm/lib\", \"-lamdhip64\", \"-Wl,-rpath,/opt/rocm/lib\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\
                 {libs}",
                compat = rocm_compat_root_str, cfg = rocm_cfg_root_str, libs = libs_build
            )
        } else {
            format!(
                "# Stub BUILD for local_config_rocm/rocm\n\
                 package(default_visibility = [\"//visibility:public\"])\n\
                 cc_library(\n\
                     name = \"rocm_headers\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     includes = [\"{cfg}\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n",
                cfg = rocm_cfg_root_str
            )
        };
        std::fs::write(local_config_rocm_repo.join("rocm/BUILD"), rocm_rocm_build)?;
        let rocm_build_defs = if rocm_enabled {
            // Real ROCm: if_rocm/if_rocm_is_configured take the TRUE branch, gfx1100,
            // and copts carry -DTENSORFLOW_USE_ROCM=1 + --offload-arch=gfx1100. rocm_library
            // lowers to native cc_library so the target is actually built (hipcc compiles
            // the .cu.cc via the RUSH_GPU_CXX swap).
            r#"# Rush ROCm build_defs (RUSH_ENABLE_ROCM=1) — W7900/gfx1100, ROCm 6.4.3

def if_rocm(if_true, if_false = []):
    return if_true

def if_cuda_or_rocm(if_true, if_false = []):
    return if_true

def if_gpu_is_configured(if_true, if_false = []):
    return if_true

def rocm_default_copts():
    return ["-DTENSORFLOW_USE_ROCM=1", "--offload-arch=gfx1100"]

def if_rocm_is_configured(x):
    return x

def rocm_library(**kwargs):
    native.cc_library(**kwargs)

def rocm_gpu_architectures():
    return ["gfx1100"]

def rocm_copts(opts = []):
    return ["-DTENSORFLOW_USE_ROCM=1", "--offload-arch=gfx1100"] + opts

def if_rocm_hipblaslt(if_true, if_false = []):
    return if_false

def if_hipblaslt(if_true, if_false = []):
    return if_false

def if_rocm_hipsparselt(if_true, if_false = []):
    return if_false
"#
        } else {
            r#"# Stub build_defs.bzl for local_config_rocm

def if_rocm(if_true, if_false = []):
    return select({
        "@local_config_rocm//:is_rocm_enabled": if_true,
        "//conditions:default": if_false,
    })

def if_cuda_or_rocm(if_true, if_false = []):
    return if_false

def if_gpu_is_configured(if_true, if_false = []):
    return if_false

def rocm_default_copts():
    return []

def if_rocm_is_configured(x):
    return []

def rocm_library(**kwargs):
    pass

def rocm_gpu_architectures():
    # ROCm is disabled — no architectures.
    return []

def rocm_copts():
    return []

def if_rocm_hipblaslt(if_true, if_false = []):
    # ROCm hipBLASLt is disabled in this build.
    return if_false

def if_hipblaslt(if_true, if_false = []):
    return if_false

def if_rocm_hipsparselt(if_true, if_false = []):
    return if_false
"#
        };
        std::fs::write(
            local_config_rocm_repo.join("rocm/build_defs.bzl"),
            rocm_build_defs,
        )?;
        repo_paths.insert("local_config_rocm".to_string(), local_config_rocm_repo);

        // FIX: com_google_googleapis stub
        let googleapis_repo = external_cache.join("com_google_googleapis");
        std::fs::create_dir_all(&googleapis_repo)?;
        std::fs::write(
            googleapis_repo.join("BUILD"),
            "# Stub BUILD for com_google_googleapis\n\
             package(default_visibility = [\"//visibility:public\"])\n",
        )?;
        std::fs::write(
            googleapis_repo.join("repository_rules.bzl"),
            r#"# Stub repository_rules.bzl for com_google_googleapis

def switched_rules_by_language(
        name,
        cc = False,
        csharp = False,
        gapic = False,
        go = False,
        grpc = False,
        java = False,
        nodejs = False,
        php = False,
        python = False,
        ruby = False,
        rules_override = {}):
    native.filegroup(
        name = name,
        srcs = [],
        visibility = ["//visibility:public"],
    )

def config_googleapis():
    """Stub for config_googleapis - no-op."""
    pass
"#,
        )?;
        repo_paths.insert("com_google_googleapis".to_string(), googleapis_repo);

        // FIX: png stub - libpng image library
        let png_repo = external_cache.join("png");
        std::fs::create_dir_all(png_repo.join("png"))?;
        // TF references @png//:png at the ROOT package, and TF's png.BUILD
        // also declares a genrule producing pnglibconf.h (named
        // snappy_stubs_public_h upstream - a copy-paste in TF's own file,
        // mirrored verbatim so the declared label set matches). This
        // adapter links the system libpng, so the system's pnglibconf.h IS
        // the configuration matching what gets linked; stage it and
        // publish through a plain-cp genrule.
        let png_genrule = if std::fs::copy(
            "/usr/include/pnglibconf.h",
            png_repo.join("pnglibconf.h.rush-src"),
        )
        .is_ok()
        {
            "\ngenrule(\n\
                 name = \"snappy_stubs_public_h\",\n\
                 srcs = [\"pnglibconf.h.rush-src\"],\n\
                 outs = [\"pnglibconf.h\"],\n\
                 cmd = \"cp $< $@\",\n\
             )\n"
        } else {
            ""
        };
        std::fs::write(
            png_repo.join("BUILD"),
            format!(
                "# Stub BUILD for png - Rush uses system libpng\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                     name = \"png\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     includes = [\"/usr/include\"],\n\
                     linkopts = [\"-lpng\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n{png_genrule}"
            ),
        )?;
        std::fs::write(
            png_repo.join("png/BUILD"),
            "# Stub BUILD for png/png - Rush uses system libpng\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"png\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 includes = [\"/usr/include\"],\n\
                 linkopts = [\"-lpng\"],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n",
        )?;
        repo_paths.insert("png".to_string(), png_repo);

        // FIX: libjpeg_turbo stub - JPEG image library
        let jpeg_repo = external_cache.join("libjpeg_turbo");
        std::fs::create_dir_all(&jpeg_repo)?;
        std::fs::write(
            jpeg_repo.join("BUILD"),
            "# Stub BUILD for libjpeg_turbo - Rush uses system libjpeg\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"jpeg\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 includes = [\"/usr/include\"],\n\
                 linkopts = [\"-ljpeg\"],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n\n\
             alias(name = \"libjpeg_turbo\", actual = \":jpeg\")\n",
        )?;
        repo_paths.insert("libjpeg_turbo".to_string(), jpeg_repo);

        // FIX: gif stub - GIF image library (system libgif, libgif-dev).
        // TF's `@local_tsl//tsl/platform:gif` deps bare `@gif`, whose canonical
        // target is the root `@gif//:gif`. Keep the `gif/` compatibility package
        // for explicit historical labels that still use the nested spelling.
        let gif_repo = external_cache.join("gif");
        std::fs::create_dir_all(gif_repo.join("gif"))?;
        let gif_build = "# Stub BUILD for gif - Rush uses system libgif\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"gif\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 includes = [\"/usr/include\"],\n\
                 linkopts = [\"-lgif\"],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n";
        std::fs::write(gif_repo.join("BUILD"), gif_build)?;
        std::fs::write(gif_repo.join("gif/BUILD"), gif_build)?;
        repo_paths.insert("gif".to_string(), gif_repo);

        // Prefer TensorFlow's pinned Snappy build when materialized. Ubuntu's
        // newer system Snappy removed RawCompressFromIOVec, which TF 2.15 still
        // calls from tsl/platform/default/port.cc. The pinned cache is produced
        // by scripts/bootstrap_snappy.sh with PIC enabled; retain the system
        // library as an honest fallback for workspaces that do not need the old
        // API.
        let snappy_repo = external_cache.join("snappy");
        std::fs::create_dir_all(snappy_repo.join("snappy"))?;
        let pinned_snappy = resolve_clone_path("snappy", "build/libsnappy.a").filter(|root| {
            root.join("snappy.h").is_file()
                && root.join("build/snappy-stubs-public.h").is_file()
        });
        let (snappy_source, snappy_includes, snappy_linkopts) = match pinned_snappy {
            Some(root) => (
                "TF-pinned cache",
                format!("\"{}\", \"{}\"", root.join("build").display(), root.display()),
                format!("\"{}\"", root.join("build/libsnappy.a").display()),
            ),
            None => (
                "system library",
                "\"/usr/include\"".to_string(),
                "\"-lsnappy\"".to_string(),
            ),
        };
        // TF's snappy.BUILD also declares two generated headers. Stage the
        // real content next to the BUILD at synthesis time - the pinned
        // clone's cmake-generated headers when present, the system's stubs
        // header otherwise - and let plain-cp genrules publish them, so the
        // labels the audit checks are REAL targets producing real bytes.
        let mut snappy_genrules = String::new();
        {
            let staged_config = snappy_repo.join("config.h.rush-src");
            let staged_stubs = snappy_repo.join("snappy-stubs-public.h.rush-src");
            let clone_build = resolve_clone_path("snappy", "build/config.h")
                .map(|root| root.join("build"));
            let config_ok = match &clone_build {
                Some(b) => std::fs::copy(b.join("config.h"), &staged_config).is_ok(),
                None => false,
            };
            let stubs_ok = match &clone_build {
                Some(b) => {
                    std::fs::copy(b.join("snappy-stubs-public.h"), &staged_stubs).is_ok()
                }
                None => std::fs::copy(
                    "/usr/include/snappy-stubs-public.h",
                    &staged_stubs,
                )
                .is_ok(),
            };
            if config_ok {
                snappy_genrules.push_str(
                    "\ngenrule(\n\
                         name = \"config_h\",\n\
                         srcs = [\"config.h.rush-src\"],\n\
                         outs = [\"config.h\"],\n\
                         cmd = \"cp $< $@\",\n\
                     )\n",
                );
            }
            if stubs_ok {
                snappy_genrules.push_str(
                    "\ngenrule(\n\
                         name = \"snappy_stubs_public_h\",\n\
                         srcs = [\"snappy-stubs-public.h.rush-src\"],\n\
                         outs = [\"snappy-stubs-public.h\"],\n\
                         cmd = \"cp $< $@\",\n\
                     )\n",
                );
            }
        }
        let snappy_build = format!(
            "# Stub BUILD for snappy - Rush uses {snappy_source}\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"snappy\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 includes = [{snappy_includes}],\n\
                 linkopts = [{snappy_linkopts}],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n{snappy_genrules}"
        );
        std::fs::write(snappy_repo.join("BUILD"), &snappy_build)?;
        std::fs::write(snappy_repo.join("snappy/BUILD"), snappy_build)?;
        repo_paths.insert("snappy".to_string(), snappy_repo);

        // #39: jsoncpp — TF references `@jsoncpp_git//:jsoncpp` for the JSON
        // parser (Json::Value, used by the profiler/XPlane JSON exporters). The
        // sysmap entry above only injects the `/usr/include/jsoncpp` headers, so
        // the symbols compiled fine but were never linked — libtensorflow_cc.so
        // had 37 undefined `Json::Value` symbols (and framework.so the same).
        // Debian's `libjsoncpp-dev` (1.9.6, ABI-compatible with TF's pinned
        // 1.9.5) ships `/usr/lib/.../libjsoncpp.{a,so}`, so materialize a stub
        // repo whose `cc_library` carries `-ljsoncpp` — mirrors the png/jpeg/
        // gif/snappy system-lib stubs. The cc_library lives at the repo ROOT
        // because TF imports it as `//:jsoncpp`.
        let jsoncpp_repo = external_cache.join("jsoncpp_git");
        std::fs::create_dir_all(&jsoncpp_repo)?;
        std::fs::write(
            jsoncpp_repo.join("BUILD"),
            "# Stub BUILD for jsoncpp_git - Rush uses system libjsoncpp\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"jsoncpp\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 includes = [\"/usr/include/jsoncpp\", \"/usr/include\"],\n\
                 linkopts = [\"-ljsoncpp\"],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n",
        )?;
        repo_paths.insert("jsoncpp_git".to_string(), jsoncpp_repo);

        // #39: brotli — Riegeli requires the shared-dictionary API, which older
        // distro libbrotli packages do not expose. Prefer a source-matched PIC
        // bootstrap in RUSH_GLOBAL_CLONE_CACHE/org_brotli when present; retain
        // the distro libraries as an honest fallback for older TF/Riegeli trees
        // that only use the stable decoder/encoder API.
        let brotli_repo = external_cache.join("org_brotli");
        std::fs::create_dir_all(&brotli_repo)?;
        let pinned_brotli_dir = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_global_clone_cache_root())
            .join("org_brotli");
        let pinned_brotli_libs = [
            pinned_brotli_dir.join("libbrotlicommon.a"),
            pinned_brotli_dir.join("libbrotlidec.a"),
            pinned_brotli_dir.join("libbrotlienc.a"),
        ];
        let pinned_brotli_active =
            pinned_brotli_libs.iter().all(|path| path.is_file());
        let pinned_brotli_include = std::fs::read_to_string(
            pinned_brotli_dir.join("INCLUDE_ROOT"),
        )
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| {
            Path::new(value)
                .join("brotli/shared_dictionary.h")
                .is_file()
        });
        let (brotli_include, brotli_common, brotli_dec, brotli_enc) =
            if pinned_brotli_active && pinned_brotli_include.is_some() {
                (
                    pinned_brotli_include.unwrap(),
                    pinned_brotli_libs[0].display().to_string(),
                    pinned_brotli_libs[1].display().to_string(),
                    pinned_brotli_libs[2].display().to_string(),
                )
            } else {
                (
                    "/usr/include".to_string(),
                    "-lbrotlicommon".to_string(),
                    "-lbrotlidec".to_string(),
                    "-lbrotlienc".to_string(),
                )
            };
        std::fs::write(
            brotli_repo.join("BUILD"),
            format!(
                "# Stub BUILD for @org_brotli - Rush uses a compatible local Brotli\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                     name = \"brotlicommon\",\n\
                     includes = [\"{brotli_include}\"],\n\
                     linkopts = [\"{brotli_common}\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\n\
                 cc_library(\n\
                     name = \"brotlidec\",\n\
                     includes = [\"{brotli_include}\"],\n\
                     linkopts = [\"{brotli_dec}\"],\n\
                     deps = [\":brotlicommon\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\n\
                 cc_library(\n\
                     name = \"brotlienc\",\n\
                     includes = [\"{brotli_include}\"],\n\
                     linkopts = [\"{brotli_enc}\"],\n\
                     deps = [\":brotlicommon\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n"
            ),
        )?;
        repo_paths.insert("org_brotli".to_string(), brotli_repo);

        // TF's `@local_tsl//tsl/platform/cloud:*` targets dep bare `@curl`, whose
        // canonical target is `@curl//:curl`. Retain the nested package for
        // explicit compatibility labels. Debian `libcurl4-openssl-dev` supplies
        // libcurl and the multiarch include directory used by both spellings.
        let curl_repo = external_cache.join("curl_sys");
        std::fs::create_dir_all(curl_repo.join("curl"))?;
        let curl_build = "# Stub BUILD for curl - Rush uses system libcurl (libcurl4-openssl-dev)\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"curl\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 includes = [\"/usr/include/x86_64-linux-gnu\", \"/usr/include\"],\n\
                 linkopts = [\"-lcurl\"],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n";
        std::fs::write(curl_repo.join("BUILD"), curl_build)?;
        std::fs::write(curl_repo.join("curl/BUILD"), curl_build)?;
        repo_paths.insert("curl".to_string(), curl_repo);

        // #39: ICU — TF references `@icu//:common` for the Unicode ops
        // (UnicodeString, ByteSink, ErrorCode, CharacterIterator — used by the
        // tensorflow/core/kernels unicode/normalize ops). third_party/icu/
        // BUILD.system models `common -> :icuuc` with `icuuc` carrying
        // `linkopts = ["-licuuc"]`; with no real @icu repo rush left those
        // symbols compiled-but-unlinked (27 undefined `icu_78::*` in
        // libtensorflow_cc.so). prune's Debian `libicu-dev` is 78.x — the SAME
        // major as TF's vendored `icu_78::` versioned namespace — so the system
        // `libicuuc.so.78` resolves them. Materialize a stub @icu repo mirroring
        // BUILD.system (the jsoncpp/png/jpeg system-lib stub pattern); carry
        // `-licuuc` on `common` directly too so the link flag does not depend on
        // transitive linkopt propagation. ICU headers live under
        // /usr/include/unicode, already on the default include path.
        let icu_repo = external_cache.join("icu");
        std::fs::create_dir_all(&icu_repo)?;
        std::fs::write(
            icu_repo.join("BUILD"),
            "# Stub BUILD for @icu - Rush uses system libicuuc (libicu-dev)\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"headers\",\n\
                 includes = [\"/usr/include\"],\n\
             )\n\n\
             cc_library(\n\
                 name = \"icuuc\",\n\
                 includes = [\"/usr/include\"],\n\
                 linkopts = [\"-licuuc\"],\n\
             )\n\n\
             cc_library(\n\
                 name = \"common\",\n\
                 deps = [\":icuuc\"],\n\
                 linkopts = [\"-licuuc\"],\n\
             )\n",
        )?;
        repo_paths.insert("icu".to_string(), icu_repo);

        // FIX: llvm-project stub — LLVM/MLIR at TF's pinned SHA.
        //
        // Not a Debian package at the version TF expects. Clone once into
        // $XDG_CACHE_HOME/rush/external/llvm-project/ (SHA pinned in
        // third_party/llvm/workspace.bzl). When the clone is present, we
        // register a single catch-all cc_library that exposes BOTH the
        // mlir/include and llvm/include dirs on -I, so
        // `#include "mlir/Dialect/..."` and `#include "llvm/..."` both
        // resolve. The ~hundreds of fine-grained labels TF references
        // (@llvm-project//mlir:IR, :Support, :Pass, …) all become
        // unknown-target missing deps that rush's lazy-loader stubs as
        // empty SystemCcLibrary; downstream compiles pick up the headers
        // via the transitive -I path, so header-only consumers build
        // fine. Targets that actually LINK against MLIR will surface as
        // link-time unresolved symbols — that's out of scope for this
        // stub; real LLVM/MLIR compilation would need real BUILD rules.
        let llvm_repo = external_cache.join("llvm-project");
        // Prefer the TF-pinned LLVM clone's CMake build tree — its
        // mlir-tblgen outputs are from the exact sources TF's MLIR code
        // is written against (MLIR API versions match). Debian
        // `libmlir-<N>-dev` can substitute but trips API skew at
        // TF-owned dialect targets.
        //
        // Witness the clone via the generated MLIR TableGen output so
        // both workspace-local and host-global clone caches are picked
        // up consistently.
        let llvm_clone = resolve_clone_path(
            "llvm-project",
            "build/tools/mlir/include/mlir/IR/BuiltinLocationAttributes.h.inc",
        );
        let clone_build = llvm_clone
            .as_ref()
            .map(|p| p.join("build"))
            .unwrap_or_else(|| llvm_repo.join("build"));
        let clone_has_generated = llvm_clone.is_some();
        let sys_llvm_inc = ["/usr/lib/llvm-19/include", "/usr/lib/llvm-18/include",
                            "/usr/lib/llvm-17/include"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.join("mlir/IR/BuiltinLocationAttributes.h.inc").exists());
        let (inc_dirs, repo_path): (Vec<String>, PathBuf) = if clone_has_generated {
            // `llvm_clone` is guaranteed `Some` when `clone_has_generated`
            // is true — probe the resolved clone root (which may be
            // workspace-local or the host-global cache).
            let clone_root = llvm_clone.as_ref().unwrap();
            (
                vec![
                    clone_build.join("tools/mlir/include").display().to_string(),
                    clone_build.join("include").display().to_string(),
                    clone_root.join("mlir/include").display().to_string(),
                    clone_root.join("llvm/include").display().to_string(),
                ],
                llvm_repo.clone(),
            )
        } else if let Some(sys_inc) = sys_llvm_inc {
            std::fs::create_dir_all(&llvm_repo)?;
            (vec![sys_inc.display().to_string()], llvm_repo.clone())
        } else {
            // No clone-build and no system — leave @llvm-project unregistered.
            (Vec::new(), llvm_repo.clone())
        };
        if !inc_dirs.is_empty() {
            std::fs::create_dir_all(&repo_path)?;
            let includes_list = inc_dirs
                .iter()
                .map(|p| format!("\"{}\"", p))
                .collect::<Vec<_>>()
                .join(", ");
            std::fs::write(
                repo_path.join("BUILD"),
                format!(
                    "# Stub BUILD for llvm-project — Rush uses system libmlir-*-dev\n\
                     # headers when available, otherwise a cloned source tree.\n\
                     package(default_visibility = [\"//visibility:public\"])\n\n\
                     cc_library(\n\
                         name = \"llvm_mlir_headers\",\n\
                         srcs = [],\n\
                         hdrs = [],\n\
                         includes = [{includes}],\n\
                         visibility = [\"//visibility:public\"],\n\
                     )\n",
                    includes = includes_list,
                ),
            )?;
            // Stub `mlir/tblgen.bzl` with no-op codegen macros. TF's
            // tf2xla / mlir packages `load(@llvm-project//mlir:tblgen.bzl,
            // "gentbl_cc_library", ...)` but rush's gentbl_cc_library
            // is registered as a rule in MACRO_PRIORITY_OVERRIDE — the
            // actual .bzl content is read only for the `load()`
            // existence check. Providing empty defs with matching names
            // keeps rush's parser happy without changing behavior.
            let mlir_dir = repo_path.join("mlir");
            std::fs::create_dir_all(&mlir_dir)?;
            std::fs::write(
                mlir_dir.join("tblgen.bzl"),
                "# Stub mlir/tblgen.bzl — rush's native gentbl_cc_library\n\
                 # and td_library (MACRO_PRIORITY_OVERRIDE) handle these\n\
                 # calls directly; the defs below exist only so `load()`\n\
                 # resolves the symbols. The override intercepts every\n\
                 # call site, so the no-op bodies never run.\n\
                 def gentbl_cc_library(**kwargs):\n    pass\n\n\
                 def gentbl_filegroup(**kwargs):\n    pass\n\n\
                 def td_library(**kwargs):\n    pass\n",
            )?;
            repo_paths.insert("llvm-project".to_string(), repo_path);
        }

        // FIX: ducc stub — Distinctly Useful Code Collection (TF FFT
        // backend). TF's fft_impl.cc does `#include
        // "ducc/google/threading.h"`. ducc's Bazel rule uses
        // `include_prefix = "ducc"` which rush doesn't re-implement, so
        // we rely on a sibling symlink-farm dir
        // `external/ducc_include_root/ducc -> external/ducc/` and put
        // that root on -I via the stub's `includes = [...]`.
        // Bootstrap clone (one-time):
        //   git clone https://gitlab.mpcdf.mpg.de/mtr/ducc.git
        //     $XDG_CACHE_HOME/rush/external/ducc
        //   cd $XDG_CACHE_HOME/rush/external/ducc && git checkout <TF-pinned
        //     DUCC_COMMIT from third_party/ducc/workspace.bzl>
        //   cp /data/tensorflow/third_party/ducc/{threading.h,threading.cc,
        //     ducc0_custom_lowlevel_threading.h} google/
        //   mkdir -p $XDG_CACHE_HOME/rush/external/ducc_include_root && \
        //     ln -sf $XDG_CACHE_HOME/rush/external/ducc \
        //            $XDG_CACHE_HOME/rush/external/ducc_include_root/ducc
        let ducc_repo = external_cache.join("ducc");
        if let Some(ducc_clone) = resolve_clone_path("ducc", "google/threading.h") {
            std::fs::create_dir_all(&ducc_repo)?;
            // The include-root symlink farm exposes `ducc/...` as an -I prefix
            // (ducc's Bazel rule uses include_prefix = "ducc"). rush CREATES it
            // here — `<root>/ducc -> <ducc_clone>` — so consumers that
            // `#include "ducc/google/threading.h"` resolve via -I<root>.
            // Previously this symlink was a manual one-time bootstrap step and
            // when absent (empty ducc_include_root) //tensorflow/core/kernels:fft_impl
            // failed "ducc/google/threading.h: No such file or directory".
            // Co-locate the farm next to the clone (writable, deterministic),
            // but FALL BACK to the workspace's own external dir when that
            // location cannot be written.
            //
            // A read-only or FULL clone cache used to be reported as a
            // warning and then built through: the -I still pointed at the
            // farm that was never created, so the failure surfaced twenty
            // minutes later as dozens of `'ducc/google/threading.h' file not
            // found` compile errors with nothing connecting them to the
            // cause. Observed for real on a host whose cache filesystem was
            // 100% full: 64 of 98 corpus build failures, all one ENOSPC.
            // A farm that cannot be built is a hard error, not a warning.
            let farm_candidates = {
                let mut v = Vec::new();
                if let Some(parent) = ducc_clone.parent() {
                    v.push(parent.join("ducc_include_root"));
                }
                v.push(external_cache.join("ducc_include_root"));
                v
            };
            let mut staged: Option<PathBuf> = None;
            let mut last_err: Option<String> = None;
            for dir in &farm_candidates {
                if std::fs::create_dir_all(dir).is_err() {
                    last_err = Some(format!("cannot create {}", dir.display()));
                    continue;
                }
                let link = dir.join("ducc");
                // Re-point idempotently: remove a stale symlink/dir first.
                if link.symlink_metadata().is_ok() {
                    let _ = std::fs::remove_file(&link);
                }
                #[cfg(unix)]
                {
                    match std::os::unix::fs::symlink(&ducc_clone, &link) {
                        Ok(()) => {
                            staged = Some(dir.clone());
                            break;
                        }
                        Err(e) => {
                            last_err = Some(format!(
                                "{} -> {}: {}",
                                link.display(),
                                ducc_clone.display(),
                                e
                            ));
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    staged = Some(dir.clone());
                    break;
                }
            }
            let ducc_include_root_dir = match staged {
                Some(dir) => dir,
                None => {
                    return Err(anyhow::anyhow!(
                        "@ducc: could not create the include-prefix farm in any \
                         candidate location ({}). Last error: {}. Every consumer \
                         of `#include \"ducc/...\"` would fail to compile with a \
                         misleading 'file not found'; refusing to build instead.",
                        farm_candidates
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                        last_err.unwrap_or_else(|| "unknown".to_string())
                    ));
                }
            };
            let ducc_include_root = ducc_include_root_dir.display().to_string();
            let ducc_src = ducc_clone.join("src").display().to_string();
            // #39 Phase R: link the REAL libducc.a (ducc0::detail_threading::*
            // from src/ducc0/infra/threading.cc, built by
            // scripts/bootstrap_ducc_fft2d.sh) so the FFT kernel's threading
            // symbols are DEFINED, not just header-visible. HONEST-when-present.
            // The bootstrap deposits the archive NEXT TO THE CLONE
            // (${CACHE}/ducc/libducc.a in the global clone cache), so probe
            // the resolved clone first; the per-workspace stub dir stays as
            // fallback for hand-placed archives. Probing only ducc_repo
            // missed the bootstrap output entirely — the linkopts stayed
            // empty and every fft_impl consumer (eager/common_runtime
            // tests) died at link with execStatic undefined.
            let ducc_lib = [ducc_clone.join("libducc.a"), ducc_repo.join("libducc.a")]
                .into_iter()
                .find(|p| p.is_file());
            // WHOLE-ARCHIVE libducc.a: it carries ducc0::detail_threading::* (referenced,
            // so pulled normally) AND the google/fft.cc template instantiations
            // (ducc0::google::c2c<double> etc.). The latter are UNreferenced at the
            // shared-lib link (undefined symbols are deferred in a .so link), so a plain
            // archive link leaves them out -> libtensorflow_cc.so fails dlopen RTLD_NOW on
            // ducc0::google::c2c<double>. Whole-archive forces every member in, matching the
            // libflatbuffers_util.a handling (build_orchestrator_ext.rs). (Codex 019eebc6.)
            let ducc_linkopts = match &ducc_lib {
                Some(p) => format!(
                    "\"-Wl,--whole-archive\", \"{}\", \"-Wl,--no-whole-archive\", \"-pthread\"",
                    p.display()
                ),
                None => String::new(),
            };
            // TF's ducc.BUILD exports filegroup(mobile_srcs_no_runtime) =
            // DUCC_SOURCES + DUCC_HEADERS. Symlink the clone's src/ and
            // google/ trees into the repo and enumerate the real files, so
            // the filegroup names bytes that exist. Enumerated at synthesis
            // rather than glob()ed so the result does not depend on glob
            // semantics across symlinked directories.
            let ducc_filegroup = {
                let mut srcs: Vec<String> = Vec::new();
                for top in ["src", "google"] {
                    let link = ducc_repo.join(top);
                    if link.symlink_metadata().is_ok() {
                        let _ = std::fs::remove_file(&link);
                    }
                    #[cfg(unix)]
                    let _ = std::os::unix::fs::symlink(ducc_clone.join(top), &link);
                    let mut stack = vec![ducc_clone.join(top)];
                    while let Some(dir) = stack.pop() {
                        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                        for e in rd.flatten() {
                            let path = e.path();
                            if path.is_dir() {
                                stack.push(path);
                            } else if path
                                .extension()
                                .and_then(|x| x.to_str())
                                .is_some_and(|x| matches!(x, "cc" | "h" | "hpp"))
                            {
                                if let Ok(rel) = path.strip_prefix(&ducc_clone) {
                                    srcs.push(format!(
                                        "\"{}\"",
                                        rel.display()
                                    ));
                                }
                            }
                        }
                    }
                }
                srcs.sort();
                if srcs.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\nfilegroup(\n    name = \"mobile_srcs_no_runtime\",\n    srcs = [{}],\n)\n",
                        srcs.join(", ")
                    )
                }
            };
            std::fs::write(
                ducc_repo.join("BUILD"),
                format!(
                    "# Stub BUILD for @ducc — cc_library carrying the\n\
                     # symlink-farm root that exposes `ducc/...` as an -I prefix\n\
                     # plus the clone's `src/` dir so internal `ducc0/...`\n\
                     # includes from fft.h into its own infra headers resolve.\n\
                     package(default_visibility = [\"//visibility:public\"])\n\n\
                     cc_library(\n\
                         name = \"fft\",\n\
                         srcs = [],\n\
                         hdrs = [],\n\
                         includes = [\"{root}\", \"{src}\"],\n\
                         linkopts = [{linkopts}],\n\
                         visibility = [\"//visibility:public\"],\n\
                     )\n\n\
                     cc_library(\n\
                         name = \"fft_wrapper\",\n\
                         srcs = [],\n\
                         hdrs = [],\n\
                         includes = [\"{root}\", \"{src}\"],\n\
                         linkopts = [{linkopts}],\n\
                         visibility = [\"//visibility:public\"],\n\
                     )\n{filegroup}",
                    root = ducc_include_root,
                    src = ducc_src,
                    linkopts = ducc_linkopts,
                    filegroup = ducc_filegroup,
                ),
            )?;
            repo_paths.insert("ducc".to_string(), ducc_repo);
        }

        // FIX: nsync stub - Google nsync library.
        //
        // nsync isn't a Debian package; if the source is cloned into the
        // external cache (public/nsync_cv.h present), point the stub at its
        // `public/` dir so `#include "nsync_cv.h"` resolves. Otherwise leave
        // the empty header-only stub (targets that don't actually need nsync
        // headers still link).
        // AS (Pro K→H lane): only write the .cache stub when we
        // actually find a clone with the witness file. The prior
        // behavior wrote an empty stub
        // (`includes = []`) unconditionally, which then SHADOWED
        // any workspace-external BUILD that did declare correct
        // includes (because register_cached_repo prefers .cache
        // over workspace_external).
        let nsync_clone = resolve_clone_path("nsync", "public/nsync_cv.h");
        if let Some(p) = &nsync_clone {
            let nsync_repo = external_cache.join("nsync");
            std::fs::create_dir_all(&nsync_repo)?;
            let nsync_public = p.join("public").display().to_string();
            // nsync_cpp carries the nsync C API in `namespace nsync`
            // (`nsync::nsync_mu_lock`, …) — TF/tsl link against those. rush
            // can't compile nsync's .c-as-C++ build directly, so
            // scripts/bootstrap_nsync_cpp.sh materializes libnsync_cpp.a from
            // the real source. When that archive exists, expose it (+ -pthread,
            // nsync's linkopt) as the nsync_cpp stub's `linkopts` so the
            // cc_library propagates it as a link fragment to consumers. HONEST:
            // emit nothing when the archive is absent (NO faked flag — the link
            // then fails cleanly on nsync:: symbols, prompting the bootstrap).
            // The bootstrap writes libnsync_cpp.a into the GLOBAL clone cache
            // (RUSH_GLOBAL_CLONE_CACHE, default $XDG_CACHE_HOME/rush/external),
            // alongside the llvm/eigen clones — not necessarily this workspace
            // .cache `external_cache`. Check the global cache first, then the
            // local repo dir.
            let nsync_global = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| default_global_clone_cache_root());
            let nsync_lib = [
                nsync_global.join("nsync").join("libnsync_cpp.a"),
                nsync_repo.join("libnsync_cpp.a"),
            ]
            .into_iter()
            .find(|p| p.is_file());
            let nsync_cpp_linkopts = match &nsync_lib {
                Some(lib) => format!("\"{}\", \"-pthread\"", lib.display()),
                None => String::new(),
            };
            std::fs::write(
                nsync_repo.join("BUILD"),
                format!(
                    "# Stub BUILD for nsync — Rush uses cloned-source nsync.\n\
                     # Vendored public headers at {public}.\n\
                     package(default_visibility = [\"//visibility:public\"])\n\n\
                     cc_library(\n\
                         name = \"nsync_headers\",\n\
                         srcs = [],\n\
                         hdrs = [],\n\
                         includes = [\"{public}\"],\n\
                         visibility = [\"//visibility:public\"],\n\
                     )\n\n\
                     cc_library(\n\
                         name = \"nsync_cpp\",\n\
                         srcs = [],\n\
                         hdrs = [],\n\
                         includes = [\"{public}\"],\n\
                         linkopts = [{linkopts}],\n\
                         visibility = [\"//visibility:public\"],\n\
                     )\n",
                    public = nsync_public,
                    linkopts = nsync_cpp_linkopts,
                ),
            )?;
            repo_paths.insert("nsync".to_string(), nsync_repo);
        }
        // When no clone is found, do NOT write a shadow stub.
        // register_cached_repo's workspace_external fall-through
        // will discover a vendored layout at
        // `<workspace>/external/nsync/` if one exists (with the
        // correct package-relative `includes = ["public"]`).

        // @ruy — Google's matmul library. TF 2.15's libtensorflow_cc.so links
        // the TFLite runtime kernels (//tensorflow/lite/kernels:cpu_backend_*),
        // which #include "ruy/context.h" and dep @ruy//ruy:*. ruy was formerly
        // stubbed (repository_manager.rs, "mobile stack — not needed"), which is
        // wrong: cpu_backend_context.cc failed `ruy/context.h: No such file`.
        // scripts/bootstrap_ruy.sh materializes the genuine pinned ruy source
        // (headers at <root>/ruy/*.h) and compiles a real libruy.a with the
        // per-file AVX/AVX2-FMA/AVX512 copts transcribed from ruy's build_defs.
        // Here we synthesize the @ruy//ruy:* and @ruy//ruy/profiler:* targets as
        // cc_library adapters whose `includes = [<root>]` resolves the headers
        // and whose linkopts seed libruy.a (emitted ONLY when the archive really
        // exists — HONEST: absent → link fails cleanly on ruy:: symbols,
        // prompting the bootstrap). dedup_archive_link_flags collapses the
        // repeated libruy.a across the several adapter targets at link time.
        if let Some(ruy_clone) = resolve_clone_path("ruy", "ruy/context.h") {
            let ruy_repo = external_cache.join("ruy");
            std::fs::create_dir_all(ruy_repo.join("ruy").join("profiler"))?;
            let ruy_root = ruy_clone.display().to_string();
            let ruy_global = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| default_global_clone_cache_root());
            let ruy_lib = [
                ruy_global.join("ruy").join("libruy.a"),
                ruy_clone.join("libruy.a"),
            ]
            .into_iter()
            .find(|p| p.is_file());
            let ruy_linkopts = match &ruy_lib {
                Some(lib) => format!("\"{}\", \"-pthread\"", lib.display()),
                None => String::new(),
            };
            // Adapter cc_library generator: includes = [<root>] (header
            // resolution) + linkopts = libruy.a (link). All ruy:* targets share
            // the single libruy.a; dedup collapses it on the link line.
            let mk = |name: &str| {
                format!(
                    "cc_library(\n\
                     \x20   name = \"{name}\",\n\
                     \x20   srcs = [],\n\
                     \x20   hdrs = [],\n\
                     \x20   includes = [\"{root}\"],\n\
                     \x20   linkopts = [{linkopts}],\n\
                     \x20   visibility = [\"//visibility:public\"],\n\
                     )\n",
                    name = name,
                    root = ruy_root,
                    linkopts = ruy_linkopts,
                )
            };
            // Package @ruy//ruy : the targets TF references (grep @ruy// in TF).
            let mut ruy_build = String::from(
                "# Stub BUILD for @ruy — Rush uses cloned ruy source + libruy.a.\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n",
            );
            for t in [
                "ruy", "context", "denormal", "matrix", "mul_params", "path",
                "reference_mul", "thread_pool",
            ] {
                ruy_build.push_str(&mk(t));
                ruy_build.push('\n');
            }
            std::fs::write(ruy_repo.join("ruy").join("BUILD"), ruy_build)?;
            // Package @ruy//ruy/profiler : profiler + instrumentation.
            let mut prof_build = String::from(
                "# Stub BUILD for @ruy//ruy/profiler.\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n",
            );
            prof_build.push_str(&mk("profiler"));
            prof_build.push('\n');
            prof_build.push_str(&mk("instrumentation"));
            std::fs::write(
                ruy_repo.join("ruy").join("profiler").join("BUILD"),
                prof_build,
            )?;
            repo_paths.insert("ruy".to_string(), ruy_repo);
        }

        // @cpuinfo — pytorch/cpuinfo CPU feature detection. Pulled into the
        // same TFLite-kernel closure as @ruy: cpu_backend_context.cc does
        // `#ifdef TFLITE_HAVE_CPUINFO #include "include/cpuinfo.h"` and calls
        // cpuinfo_* at runtime; the BUILD dep is
        // @cpuinfo//:cpuinfo_with_unstripped_include_path. Formerly stubbed
        // (mobile stack) → "include/cpuinfo.h: No such file". scripts/
        // bootstrap_cpuinfo.sh materializes the pinned source (header resolves
        // via -I<root> as `include/cpuinfo.h`) and builds a real libcpuinfo.a
        // (x86_64-linux source set + bundled clog). Same adapter shape as @ruy.
        if let Some(cpuinfo_clone) = resolve_clone_path("cpuinfo", "include/cpuinfo.h") {
            let cpuinfo_repo = external_cache.join("cpuinfo");
            std::fs::create_dir_all(&cpuinfo_repo)?;
            let cpuinfo_root = cpuinfo_clone.display().to_string();
            // `cpuinfo_with_unstripped_include_path` keeps the `include/`
            // prefix (consumers write `include/cpuinfo.h`); the plain
            // `cpuinfo` target strips it (consumers write `cpuinfo.h`).
            let cpuinfo_inc = cpuinfo_clone.join("include").display().to_string();
            let cpuinfo_global = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| default_global_clone_cache_root());
            let cpuinfo_lib = [
                cpuinfo_global.join("cpuinfo").join("libcpuinfo.a"),
                cpuinfo_clone.join("libcpuinfo.a"),
            ]
            .into_iter()
            .find(|p| p.is_file());
            let cpuinfo_linkopts = match &cpuinfo_lib {
                Some(lib) => format!("\"{}\", \"-pthread\"", lib.display()),
                None => String::new(),
            };
            let mk_cpuinfo = |name: &str, inc: &str| {
                format!(
                    "cc_library(\n\
                     \x20   name = \"{name}\",\n\
                     \x20   srcs = [],\n\
                     \x20   hdrs = [],\n\
                     \x20   includes = [\"{inc}\"],\n\
                     \x20   linkopts = [{linkopts}],\n\
                     \x20   visibility = [\"//visibility:public\"],\n\
                     )\n",
                    name = name,
                    inc = inc,
                    linkopts = cpuinfo_linkopts,
                )
            };
            let mut cpuinfo_build = String::from(
                "# Stub BUILD for @cpuinfo — Rush uses cloned source + libcpuinfo.a.\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n",
            );
            cpuinfo_build.push_str(&mk_cpuinfo(
                "cpuinfo_with_unstripped_include_path",
                &cpuinfo_root,
            ));
            cpuinfo_build.push('\n');
            cpuinfo_build.push_str(&mk_cpuinfo("cpuinfo", &cpuinfo_inc));
            cpuinfo_build.push('\n');
            cpuinfo_build.push_str(&mk_cpuinfo("cpuinfo_impl", &cpuinfo_inc));
            std::fs::write(cpuinfo_repo.join("BUILD"), cpuinfo_build)?;
            repo_paths.insert("cpuinfo".to_string(), cpuinfo_repo);
        }

        // @pthreadpool — Maratyszcza/pthreadpool, the thread pool TFLite kernels
        // run on. //tensorflow/lite/kernels:cpu_backend_context.h does
        //   #include "pthreadpool.h"   // from @pthreadpool
        // and the kernels call pthreadpool_* at runtime. TF's BUILD lists the
        // bare `@pthreadpool` dep, which rush canonicalizes to
        // @pthreadpool//pthreadpool:pthreadpool — so the adapter BUILD lives in
        // the `pthreadpool` subpackage (same shape as @ruy//ruy:*). Formerly
        // stubbed empty → "pthreadpool.h: No such file or directory". The public
        // header lives at `include/pthreadpool.h`; pthreadpool exposes it via
        // `includes=["include"]` + `strip_include_prefix="include"` so consumers
        // write the bare `pthreadpool.h` — we reproduce that with
        // `includes=[<clone>/include]`. The real symbols come from
        // libpthreadpool.a (scripts/bootstrap_pthreadpool.sh builds the genuine
        // pinned source — NO STUBS); linkopts references it when present and
        // degrades to a clean link-time `pthreadpool_*` undefined-symbol error
        // (prompting the bootstrap) when absent — same HONEST contract as @ruy /
        // @cpuinfo.
        if let Some(pthreadpool_clone) =
            resolve_clone_path("pthreadpool", "include/pthreadpool.h")
        {
            let pthreadpool_repo = external_cache.join("pthreadpool");
            let pthreadpool_pkg = pthreadpool_repo.join("pthreadpool");
            std::fs::create_dir_all(&pthreadpool_pkg)?;
            let pthreadpool_inc = pthreadpool_clone.join("include").display().to_string();
            let pthreadpool_global = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| default_global_clone_cache_root());
            let pthreadpool_lib = [
                pthreadpool_global.join("pthreadpool").join("libpthreadpool.a"),
                pthreadpool_clone.join("libpthreadpool.a"),
            ]
            .into_iter()
            .find(|p| p.is_file());
            let pthreadpool_linkopts = match &pthreadpool_lib {
                Some(lib) => format!("\"{}\", \"-pthread\"", lib.display()),
                None => String::new(),
            };
            let pthreadpool_build = format!(
                "# Stub BUILD for @pthreadpool — Rush uses cloned source + libpthreadpool.a.\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                 \x20   name = \"pthreadpool\",\n\
                 \x20   srcs = [],\n\
                 \x20   hdrs = [],\n\
                 \x20   includes = [\"{inc}\"],\n\
                 \x20   defines = [\"PTHREADPOOL_NO_DEPRECATED_API\"],\n\
                 \x20   linkopts = [{linkopts}],\n\
                 \x20   visibility = [\"//visibility:public\"],\n\
                 )\n",
                inc = pthreadpool_inc,
                linkopts = pthreadpool_linkopts,
            );
            std::fs::write(pthreadpool_pkg.join("BUILD"), pthreadpool_build)?;
            repo_paths.insert("pthreadpool".to_string(), pthreadpool_repo);
        }

        // FIX: eigen_archive stub - Eigen linear algebra library.
        //
        // Prefer a TF-pinned Eigen source tree cloned into the external cache
        // (`$XDG_CACHE_HOME/rush/external/eigen_archive/Eigen/Core` present). The
        // system /usr/include/eigen3 (Debian's libeigen3-dev 3.4.0) is too old
        // for @ml_dtypes's float8.h — it's missing
        // `Eigen::numext::get_integer_by_size` which the header requires. If
        // the clone is present we point the stub at it; otherwise fall back
        // to the system headers (preserves existing behavior for targets that
        // don't pull ml_dtypes).
        let eigen_repo = external_cache.join("eigen_archive");
        std::fs::create_dir_all(&eigen_repo)?;
        let eigen_clone = resolve_clone_path("eigen_archive", "Eigen/Core");
        let eigen_has_source_clone = eigen_clone.is_some();
        let eigen_includes: String = match &eigen_clone {
            Some(p) => format!("\"{}\"", p.display()),
            None => "\"/usr/include/eigen3\"".to_string(),
        };
        std::fs::write(
            eigen_repo.join("BUILD"),
            format!(
                "# Stub BUILD for eigen_archive — Rush uses {source} Eigen.\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                     name = \"eigen3\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     includes = [{inc}],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\n\
                 cc_library(\n\
                     name = \"eigen3_internal\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     includes = [{inc}],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\n\
                 alias(name = \"eigen\", actual = \":eigen3\")\n",
                source = if eigen_has_source_clone { "cloned-source" } else { "system" },
                inc = eigen_includes,
            ),
        )?;
        // Mirror the Eigen header roots into the synthesized repo dir so the
        // ALWAYS-added repo-root -I (`.cache/rush/external/eigen_archive`)
        // resolves `Eigen/*` and `unsupported/Eigen/CXX11/Tensor` even for
        // targets whose dep chain (e.g. //tensorflow/core:framework_headers_lib
        // -> //third_party/eigen3 -> @eigen_archive//:eigen3_internal) doesn't
        // propagate the eigen3 cc_library's includes=[clone] transitively. The
        // eigen3 target's include still carries the clone for direct deps; this
        // symlink farm is a belt-and-suspenders so the bare repo-root -I (which
        // rush emits for every registered repo) also finds Eigen. No argv change
        // — it only makes an already-present -I resolve more headers, so
        // PASS-A/PASS-B stay identical. //tensorflow/core/profiler:tfprof_options
        // failed "unsupported/Eigen/CXX11/Tensor: No such file" without this.
        #[cfg(unix)]
        if let Some(eigen_clone_path) = &eigen_clone {
          // SELF-REFERENCE GUARD: resolve_clone_path probes the synthesized
          // repo (external_cache/<name>) FIRST, so once this mirror exists the
          // next run resolves the clone to the repo itself — symlinking
          // <repo>/Eigen -> <repo>/Eigen would create a cycle ("Too many levels
          // of symbolic links"). Only mirror when the clone is a DISTINCT real
          // source dir; when clone == repo the prior good symlinks already
          // point at the real source and are left untouched.
          if eigen_clone_path.as_path() != eigen_repo.as_path() {
            for sub in ["Eigen", "unsupported"] {
                let target = eigen_clone_path.join(sub);
                if target.exists() {
                    let link = eigen_repo.join(sub);
                    if link.symlink_metadata().is_ok() {
                        let _ = std::fs::remove_file(&link);
                    }
                    if let Err(e) = std::os::unix::fs::symlink(&target, &link) {
                        eprintln!(
                            "    ⚠️  @eigen_archive: failed to mirror {} -> {}: {}",
                            link.display(),
                            target.display(),
                            e
                        );
                    }
                }
            }
          }
        }
        repo_paths.insert("eigen_archive".to_string(), eigen_repo);

        // FIX: double_conversion stub - Double-precision conversion library.
        // Rush uses the system library (libdouble-conversion-dev). The
        // include root must be /usr/include, NOT /usr/include/double-conversion:
        // consumers write `#include "double-conversion/double-conversion.h"`,
        // so the `double-conversion/` prefix has to resolve under the -I dir.
        // The deeper path left that prefixed include unresolvable, which broke
        // every target transiting @local_tsl//tsl/platform:numbers. This mirrors
        // the working snappy stub (includes = ["/usr/include"]).
        let double_conversion_repo = external_cache.join("double_conversion");
        std::fs::create_dir_all(&double_conversion_repo)?;
        std::fs::write(
            double_conversion_repo.join("BUILD"),
            "# Stub BUILD for double_conversion - Rush uses system double-conversion\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"double-conversion\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 includes = [\"/usr/include\"],\n\
                 linkopts = [\"-ldouble-conversion\"],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n",
        )?;
        repo_paths.insert("double_conversion".to_string(), double_conversion_repo);

        // FIX: highwayhash - Google HighwayHash library. When a TF-pinned clone
        // is present (highwayhash/sip_hash.h), expose the clone root on -I so
        // headers resolve, AND link the real libhighwayhash.a (built by
        // scripts/bootstrap_highwayhash.sh via highwayhash's OWN Makefile, which
        // does the per-SIMD-target highwayhash_target.cc compile correctly) so
        // the dispatch symbols — HighwayHash<N>::operator(), HighwayHashCat<N>,
        // InstructionSets::Supported — resolve at the .so link. HONEST: linkopts
        // empty when the archive is absent (link fails cleanly on those symbols,
        // prompting the bootstrap) — same real-`.a` pattern as farmhash/ruy. The
        // header-only stub used to leave those 6 symbols undefined in the .so.
        let highwayhash_repo = external_cache.join("highwayhash");
        let highwayhash_clone =
            resolve_clone_path("highwayhash", "highwayhash/sip_hash.h");
        let highwayhash_inc_line = match &highwayhash_clone {
            Some(p) => format!("includes = [\"{}\"]", p.display()),
            None => "includes = []".to_string(),
        };
        let hh_global = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| default_global_clone_cache_root());
        let hh_lib = [
            hh_global.join("highwayhash").join("libhighwayhash.a"),
            highwayhash_repo.join("libhighwayhash.a"),
        ]
        .into_iter()
        .find(|p| p.is_file());
        let highwayhash_links = match &hh_lib {
            Some(lib) => format!("    linkopts = [\"{}\"],\n", lib.display()),
            None => String::new(),
        };
        std::fs::create_dir_all(&highwayhash_repo)?;
        // Every label TensorFlow's third_party/highwayhash/highwayhash.BUILD
        // declares. Upstream highwayhash ships no BUILD of its own (it is a
        // Makefile project), so TF supplies one and its consumers use the
        // fine-grained labels: riegeli's chunk encoder asks for :hh_types,
        // :instruction_sets and :highwayhash_dynamic. Exposing only
        // :highwayhash and :sip_hash left those unresolvable, and rush
        // stubbed them empty and carried on.
        //
        // rush links ONE archive built from the whole source tree by
        // scripts/bootstrap_highwayhash.sh (highwayhash's own Makefile does
        // the per-SIMD-target compile correctly), so every label maps to the
        // same includes and the same archive - the same shape as the
        // canonical all_pinned_static group used for Abseil.
        const HIGHWAYHASH_TARGETS: &[&str] = &[
            "arch_specific", "compiler_specific", "cpu_aarch64", "cpu_ppc",
            "endianess", "haswell", "hh_avx2", "hh_neon", "hh_portable",
            "hh_sse41", "hh_types", "hh_vsx", "highwayhash",
            "highwayhash_dynamic", "iaca", "instruction_sets", "k8", "load3",
            "os_specific", "scalar", "sip_hash", "vector128", "vector256",
        ];
        let mut highwayhash_build = String::from(
            "# BUILD for highwayhash: the labels TF's highwayhash.BUILD declares,\n             # each backed by the archive rush builds from the pinned source.\n             package(default_visibility = [\"//visibility:public\"])\n\n",
        );
        for target in HIGHWAYHASH_TARGETS {
            highwayhash_build.push_str(&format!(
                "cc_library(\n    name = \"{name}\",\n    {incs},\n{links}                     visibility = [\"//visibility:public\"],\n)\n\n",
                name = target,
                incs = highwayhash_inc_line,
                links = highwayhash_links
            ));
        }
        std::fs::write(highwayhash_repo.join("BUILD"), highwayhash_build)?;
        repo_paths.insert("highwayhash".to_string(), highwayhash_repo);

        // FIX: farmhash_archive stub - Google FarmHash library.
        // TF compiles src/farmhash.cc with -DNAMESPACE_FOR_HASH_FUNCTIONS=util,
        // exposing `util::Fingerprint64`. scripts/bootstrap_farmhash.sh
        // materializes libfarmhash.a from the real source; seed the stub's
        // linkopts with it when present (global clone cache first). HONEST:
        // empty when absent (link fails cleanly on util::Fingerprint64).
        let farmhash_repo = external_cache.join("farmhash_archive");
        std::fs::create_dir_all(&farmhash_repo)?;
        let farmhash_global = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| default_global_clone_cache_root());
        let farmhash_lib = [
            farmhash_global.join("farmhash_archive").join("libfarmhash.a"),
            farmhash_repo.join("libfarmhash.a"),
        ]
        .into_iter()
        .find(|p| p.is_file());
        let farmhash_linkopts = match &farmhash_lib {
            Some(lib) => format!("\"{}\"", lib.display()),
            None => String::new(),
        };
        let farmhash_includes = farmhash_lib
            .as_ref()
            .and_then(|lib| lib.parent())
            .map(|root| root.join("src"))
            .filter(|include| include.join("farmhash.h").is_file())
            .map(|include| format!("\"{}\"", include.display()))
            .unwrap_or_default();
        std::fs::write(
            farmhash_repo.join("BUILD"),
            format!(
                "# Stub BUILD for farmhash_archive\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                     name = \"farmhash\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     includes = [{includes}],\n\
                     linkopts = [{linkopts}],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n",
                includes = farmhash_includes,
                linkopts = farmhash_linkopts,
            ),
        )?;
        repo_paths.insert("farmhash_archive".to_string(), farmhash_repo);

        // farmhash_gpu_archive — the GPU build of FarmHash. In TF's WORKSPACE
        // this is a SEPARATE tf_http_archive from @farmhash_archive: it fetches
        // the same upstream FarmHash but applies third_party/farmhash/
        // farmhash_support_cuda.patch, which RENAMES src/farmhash.cc to
        // src/farmhash_gpu.h and adds __device__/__host__ qualifiers so the
        // body can be #included into a .cu.cc (e.g.
        // tensor_to_hash_bucket_op_gpu.cu.cc) and run on the GPU. Its
        // cc_library(name="farmhash_gpu") sets include_prefix=
        // "third_party/farmhash_gpu" with hdrs=["src/farmhash_gpu.h"], so
        // consumers include "third_party/farmhash_gpu/src/farmhash_gpu.h".
        //
        // Rush materializes this faithfully (no copy hack): take the real
        // FarmHash source already on disk (the @farmhash_archive clone), apply
        // the in-tree patch with patch(1) — exactly what tf_http_archive's
        // patch_file does — to produce src/farmhash_gpu.h, then emit a BUILD
        // whose cc_library carries hdrs + include_prefix so rush's
        // include_prefix virtual-include farm exposes the prefixed path on the
        // (hipcc) compile -I set. HONEST: if the source or the patch is absent,
        // or patch(1) fails, the header is not materialized and the consuming
        // .cu.cc fails cleanly on the missing include (no stub header content).
        let farmhash_gpu_repo = external_cache.join("farmhash_gpu_archive");
        let farmhash_gpu_hdr = farmhash_gpu_repo.join("src/farmhash_gpu.h");
        if !farmhash_gpu_hdr.is_file() {
            // Real FarmHash source: src/farmhash.cc (clone cache / workspace).
            let farmhash_src = resolve_clone_path("farmhash_src", "src/farmhash.cc")
                .or_else(|| resolve_clone_path("farmhash_archive", "src/farmhash.cc"))
                .map(|root| root.join("src/farmhash.cc"));
            // The CUDA/device patch, vendored in the TF tree (both the
            // top-level and the TSL-embedded copy are byte-identical).
            let farmhash_patch = [
                workspace_root.join("third_party/farmhash/farmhash_support_cuda.patch"),
                workspace_root.join(
                    "third_party/xla/third_party/tsl/third_party/farmhash/\
                     farmhash_support_cuda.patch",
                ),
            ]
            .into_iter()
            .find(|p| p.is_file());
            if let (Some(src), Some(patch)) = (farmhash_src, farmhash_patch) {
                let _ = std::fs::create_dir_all(farmhash_gpu_repo.join("src"));
                // Stage src/farmhash.cc; the patch renames it to
                // src/farmhash_gpu.h (rename from src/farmhash.cc).
                if std::fs::copy(&src, farmhash_gpu_repo.join("src/farmhash.cc")).is_ok() {
                    let status = std::process::Command::new("patch")
                        .arg("-p1")
                        .arg("--forward")
                        .arg("-i")
                        .arg(&patch)
                        .current_dir(&farmhash_gpu_repo)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    // The patch is a rename diff; on success farmhash.cc is
                    // gone and farmhash_gpu.h exists. Clean up a leftover .cc
                    // (and a stray .orig) so only the real header remains.
                    if matches!(status, Ok(s) if s.success()) {
                        let _ = std::fs::remove_file(farmhash_gpu_repo.join("src/farmhash.cc"));
                        let _ =
                            std::fs::remove_file(farmhash_gpu_repo.join("src/farmhash.cc.orig"));
                    }
                }
            }
        }
        // Emit the BUILD with the real include_prefix cc_library whenever the
        // header is present (just materialized or from a prior run).
        if farmhash_gpu_hdr.is_file() {
            std::fs::write(
                farmhash_gpu_repo.join("BUILD"),
                "# BUILD for farmhash_gpu_archive — GPU FarmHash header (CUDA/HIP).\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                 \x20   name = \"farmhash_gpu\",\n\
                 \x20   hdrs = [\"src/farmhash_gpu.h\"],\n\
                 \x20   include_prefix = \"third_party/farmhash_gpu\",\n\
                 \x20   visibility = [\"//visibility:public\"],\n\
                 )\n",
            )?;
            // Realize the Bazel `include_prefix` view. The cc_library sets
            // include_prefix="third_party/farmhash_gpu" so consumers include
            // "third_party/farmhash_gpu/src/farmhash_gpu.h". Bazel stages that
            // remapped path under bazel-out/_virtual_includes/<target>/ and
            // puts the farm root on -I; rush's include_prefix farm
            // (stage_virtual_includes / stage_include_prefix) does the same
            // for cc_library deps that flow through the typed analyze() /
            // resolve_external_dep paths. The TF .cu.cc GPU-kernel compile,
            // however, reaches @farmhash_gpu_archive only as a metadata-only
            // dep that contributes the raw repo root to -I (the cc_library is
            // not analyzed on that path), so the farm root never reaches the
            // hipcc -I set. Materialize the prefixed view INSIDE the repo
            // root — which IS already on -I — so the prefixed include
            // resolves. This is the same path remapping Bazel's include_prefix
            // produces (only the farm location differs); it exposes ONLY the
            // include_prefix-relative path, so no unrelated header is shadowed.
            let prefixed = farmhash_gpu_repo
                .join("third_party/farmhash_gpu/src/farmhash_gpu.h");
            if let Some(parent) = prefixed.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // This cache can be copied between workspaces. Recreate the view
            // every pass so a broken absolute symlink from the source
            // workspace cannot survive. A relative target keeps the generated
            // repository relocatable.
            let _ = std::fs::remove_file(&prefixed);
            #[cfg(unix)]
            let linked = std::os::unix::fs::symlink(
                "../../../src/farmhash_gpu.h",
                &prefixed,
            )
            .is_ok();
            #[cfg(not(unix))]
            let linked = false;
            if !linked {
                let _ = std::fs::copy(&farmhash_gpu_hdr, &prefixed);
            }
            repo_paths.insert("farmhash_gpu_archive".to_string(), farmhash_gpu_repo);
        }

        // FIX: fft2d — link the REAL libfft2d.a (rdft from fftsg.c, built by
        // scripts/bootstrap_ducc_fft2d.sh) so the real-FFT `rdft` is DEFINED.
        // HONEST-when-present. Both the root and the `fft2d/` subpackage (bare
        // `@fft2d` resolves to `@fft2d//fft2d:fft2d`) carry the same linkopts.
        let fft2d_repo = external_cache.join("fft2d");
        std::fs::create_dir_all(fft2d_repo.join("fft2d"))?;
        // Like @ducc: the bootstrap script writes libfft2d.a into the
        // global clone cache, so resolve across all clone roots (the
        // archive itself is the witness) instead of only the
        // per-workspace stub dir.
        let fft2d_lib = resolve_clone_path("fft2d", "libfft2d.a")
            .map(|root| root.join("libfft2d.a"))
            .or_else(|| {
                let local = fft2d_repo.join("libfft2d.a");
                local.is_file().then_some(local)
            });
        // WHOLE-ARCHIVE libfft2d.a: `rdft` (+ cdft/rdft2d/…) are undefined-and-deferred at the
        // shared-lib link, so a plain archive link leaves them out -> libtensorflow_cc.so fails
        // dlopen RTLD_NOW on `rdft`. Force every member in (same as @ducc / libflatbuffers_util.a).
        // (Codex 019eebc6.)
        let fft2d_linkopts = match &fft2d_lib {
            Some(p) => format!(
                "\"-Wl,--whole-archive\", \"{}\", \"-Wl,--no-whole-archive\"",
                p.display()
            ),
            None => String::new(),
        };
        // TF's fft2d.BUILD declares filegroup(fft2d_srcs) over the two C
        // sources mobile builds compile directly. Symlink them from the
        // clone so the filegroup is REAL; declare it only when both landed.
        let fft2d_srcs_group = match resolve_clone_path("fft2d", "fftsg.c") {
            Some(clone) => {
                let mut linked = 0;
                for f in ["fftsg.c", "fftsg2d.c"] {
                    let dest = fft2d_repo.join(f);
                    if dest.symlink_metadata().is_ok() {
                        let _ = std::fs::remove_file(&dest);
                    }
                    #[cfg(unix)]
                    if std::os::unix::fs::symlink(clone.join(f), &dest).is_ok() {
                        linked += 1;
                    }
                }
                if linked == 2 {
                    "\nfilegroup(\n\
                         name = \"fft2d_srcs\",\n\
                         srcs = [\"fftsg.c\", \"fftsg2d.c\"],\n\
                     )\n"
                } else {
                    ""
                }
            }
            None => "",
        };
        let fft2d_build = format!(
            "# Stub BUILD for fft2d — Rush links the real libfft2d.a (rdft).\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"fft2d\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 linkopts = [{linkopts}],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n{fft2d_srcs_group}",
            linkopts = fft2d_linkopts,
        );
        std::fs::write(fft2d_repo.join("BUILD"), &fft2d_build)?;
        // The fft2d/ subpackage supports explicit nested compatibility labels.
        // The filegroup's srcs are symlinked at the root, so the subpackage
        // carries only the library target.
        let fft2d_sub_build = format!(
            "# Stub BUILD for fft2d — Rush links the real libfft2d.a (rdft).\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"fft2d\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 linkopts = [{linkopts}],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n",
            linkopts = fft2d_linkopts,
        );
        std::fs::write(fft2d_repo.join("fft2d/BUILD"), fft2d_sub_build)?;
        repo_paths.insert("fft2d".to_string(), fft2d_repo);

        // FIX: gemmlowp stub - Google GEMM low-precision library.
        // When a TF-pinned clone of gemmlowp is present at this path
        // (public/gemmlowp.h exists), expose its root on -I so
        // `#include "public/gemmlowp.h"`, `#include "fixedpoint/fixedpoint.h"`
        // and friends resolve. Otherwise fall back to an empty header-only
        // stub (header-less consumers at the lazy-loading boundary stay happy).
        let gemmlowp_repo = external_cache.join("gemmlowp");
        let gemmlowp_clone = resolve_clone_path("gemmlowp", "public/gemmlowp.h");
        let gemmlowp_includes_line = match &gemmlowp_clone {
            Some(p) => format!("includes = [\"{}\"]", p.display()),
            None => "includes = []".to_string(),
        };
        std::fs::create_dir_all(gemmlowp_repo.join("gemmlowp"))?;
        // TF references gemmlowp via the ROOT package: `@gemmlowp` (the default
        // //:gemmlowp target), `@gemmlowp//:fixedpoint`, `@gemmlowp//:profiler`,
        // `@gemmlowp//:eight_bit_int_gemm` (see tensorflow/lite/kernels/internal
        // and others). The targets MUST live in the root BUILD with
        // includes=[clone] so `#include "fixedpoint/fixedpoint.h"` /
        // "public/gemmlowp.h" resolve — a previous version put them in a
        // `gemmlowp/` subpackage with an EMPTY root BUILD, so `@gemmlowp//:fixedpoint`
        // resolved to nothing and lite/kernels/internal:common failed
        // "fixedpoint/fixedpoint.h: No such file or directory".
        let gemmlowp_targets = ["gemmlowp", "fixedpoint", "profiler", "eight_bit_int_gemm"];
        let mut gemmlowp_build = String::from(
            "# Stub BUILD for gemmlowp — Rush uses the cloned gemmlowp source tree.\n\
             package(default_visibility = [\"//visibility:public\"])\n\n",
        );
        for t in gemmlowp_targets {
            gemmlowp_build.push_str(&format!(
                "cc_library(\n\
                 \x20   name = \"{name}\",\n\
                 \x20   srcs = [],\n\
                 \x20   hdrs = [],\n\
                 \x20   {incs},\n\
                 \x20   visibility = [\"//visibility:public\"],\n\
                 )\n\n",
                name = t,
                incs = gemmlowp_includes_line,
            ));
        }
        std::fs::write(gemmlowp_repo.join("BUILD"), gemmlowp_build)?;
        // Keep a gemmlowp/ subpackage mirror for any @gemmlowp//gemmlowp:* refs.
        std::fs::write(
            gemmlowp_repo.join("gemmlowp/BUILD"),
            format!(
                "# Subpackage mirror — same targets as the root package.\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                     name = \"gemmlowp\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     {incs},\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\n\
                 cc_library(\n\
                     name = \"fixedpoint\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     {incs},\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\n\
                 cc_library(\n\
                     name = \"profiler\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     {incs},\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n",
                incs = gemmlowp_includes_line
            ),
        )?;
        repo_paths.insert("gemmlowp".to_string(), gemmlowp_repo);

        // FIX: com_github_grpc_grpc stub - gRPC library
        let grpc_repo = external_cache.join("com_github_grpc_grpc");
        std::fs::create_dir_all(&grpc_repo)?;
        std::fs::write(
            grpc_repo.join("BUILD"),
            "# Stub BUILD for com_github_grpc_grpc - Rush uses system gRPC\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"grpc++\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 includes = [\"/usr/include\"],\n\
                 linkopts = [\"-lgrpc++\", \"-lgrpc\", \"-labsl_synchronization\"],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n\n\
             cc_library(\n\
                 name = \"grpc\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 includes = [\"/usr/include\"],\n\
                 linkopts = [\"-lgrpc\"],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n\n\
             cc_library(\n\
                 name = \"grpc_unsecure\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n",
        )?;
        std::fs::create_dir_all(grpc_repo.join("bazel"))?;
        std::fs::write(
            grpc_repo.join("bazel/generate_cc.bzl"),
            "# Stub generate_cc.bzl - Rush uses system gRPC; protos_all_cc builds\n\
             # don't need grpc service generation, so generate_cc is a no-op here.\n\
             def generate_cc(**kwargs):\n    pass\n",
        )?;
        // Stub cc_grpc_library.bzl (loaded by TF's core/data/service/BUILD).
        // gRPC C++ service codegen isn't needed for libtensorflow_cc.so
        // (server/client targets are out of scope). Make the macro a
        // no-op so the BUILD evaluation completes.
        std::fs::write(
            grpc_repo.join("bazel/cc_grpc_library.bzl"),
            "# Stub cc_grpc_library.bzl - no grpc service codegen in rush.\n\
             def cc_grpc_library(**kwargs):\n    pass\n",
        )?;
        repo_paths.insert("com_github_grpc_grpc".to_string(), grpc_repo);

        // @com_googlesource_code_re2 must use the TF-pinned source when it is
        // materialized. The distro RE2 ABI is coupled to the distro Abseil;
        // combining its headers/library with TF's pinned Abseil fails at
        // compile time (absl::string_view vs RE2::StringPiece) and would be an
        // ABI violation even if it happened to compile. bootstrap_re2.sh puts
        // the exact TF workspace pin in the global clone cache, whose real
        // BUILD.bazel compiles RE2 against the same CcInfo dependency graph as
        // the rest of TensorFlow.
        if let Some(re2_repo) =
            resolve_clone_path("com_googlesource_code_re2", "re2/re2.h")
        {
            repo_paths.insert("com_googlesource_code_re2".to_string(), re2_repo);
        } else {
            // Retain the historical system adapter for non-TensorFlow users
            // that intentionally rely on the host RE2. TensorFlow builds with
            // pinned Abseil should run scripts/bootstrap_re2.sh instead.
            let re2_repo = external_cache.join("com_googlesource_code_re2");
            std::fs::create_dir_all(&re2_repo)?;
            std::fs::write(
                re2_repo.join("BUILD"),
                "# Stub BUILD for re2 - run scripts/bootstrap_re2.sh for pinned source\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                     name = \"re2\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     includes = [\"/usr/include\"],\n\
                     linkopts = [\"-lre2\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n",
            )?;
            eprintln!(
                "⚠️  @com_googlesource_code_re2 uses the system adapter; \
                 run scripts/bootstrap_re2.sh --apply for TensorFlow's pinned RE2"
            );
            repo_paths.insert("com_googlesource_code_re2".to_string(), re2_repo);
        }

        // org_sqlite — REAL vendored sqlite3.c amalgamation (NO STUBS). TF's
        // //tensorflow/core/lib/db uses the sqlite3 C API; ~45 sqlite3_* symbols
        // were undefined. The PRIOR `-lsqlite3` here was a fake system-lib
        // substitution; this links the real libsqlite_amalgamation.a built by
        // scripts/bootstrap_sqlite.sh from the genuine TF-pinned sqlite3.c
        // (259 sqlite3_* defs) — same real-`.a` pattern as farmhash/ruy/highwayhash.
        // Bare `@org_sqlite` resolves to the root `:org_sqlite` target. The
        // nested package remains available for explicit compatibility labels.
        // HONEST: linkopts empty when the .a is absent (link fails cleanly,
        // prompting the bootstrap). -ldl/-lpthread per sqlite.BUILD.
        let sqlite_repo = external_cache.join("org_sqlite");
        let sqlite_global = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| default_global_clone_cache_root());
        let sqlite_dir = if sqlite_global.join("org_sqlite").join("sqlite3.h").is_file() {
            sqlite_global.join("org_sqlite")
        } else {
            sqlite_repo.clone()
        };
        let sqlite_lib = [
            sqlite_global.join("org_sqlite").join("libsqlite_amalgamation.a"),
            sqlite_repo.join("libsqlite_amalgamation.a"),
        ]
        .into_iter()
        .find(|p| p.is_file());
        let sqlite_links = match &sqlite_lib {
            Some(lib) => format!(
                "    linkopts = [\"{}\", \"-ldl\", \"-lpthread\"],\n",
                lib.display()
            ),
            None => String::new(),
        };
        let sqlite_inc = format!("includes = [\"{}\"]", sqlite_dir.display());
        std::fs::create_dir_all(&sqlite_repo)?;
        std::fs::write(
            sqlite_repo.join("BUILD"),
            format!(
                "# @org_sqlite — real vendored amalgamation (libsqlite_amalgamation.a)\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                     name = \"sqlite3\",\n\
                     {inc},\n\
                 {links}    visibility = [\"//visibility:public\"],\n\
                 )\n\n\
                 alias(name = \"sqlite\", actual = \":sqlite3\")\n\
                 alias(name = \"org_sqlite\", actual = \":sqlite3\")\n",
                inc = sqlite_inc,
                links = sqlite_links
            ),
        )?;
        // Preserve the explicit nested compatibility target over the same real
        // amalgamation archive.
        let sqlite_subpkg = sqlite_repo.join("org_sqlite");
        std::fs::create_dir_all(&sqlite_subpkg)?;
        std::fs::write(
            sqlite_subpkg.join("BUILD"),
            format!(
                "# @org_sqlite//org_sqlite:org_sqlite — real vendored amalgamation\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                     name = \"org_sqlite\",\n\
                     {inc},\n\
                 {links}    visibility = [\"//visibility:public\"],\n\
                 )\n",
                inc = sqlite_inc,
                links = sqlite_links
            ),
        )?;
        repo_paths.insert("org_sqlite".to_string(), sqlite_repo);

        // Prefer TensorFlow's pinned GoogleTest source. Minimal CPU hosts often
        // have neither gtest headers nor libraries installed, and a source-less
        // system adapter lets that absence surface much later in unrelated TSL
        // compiles. The real upstream BUILD.bazel exports the correct
        // googletest/include and googlemock/include roots through CcInfo.
        if let Some(googletest_repo) = resolve_clone_path(
            "com_google_googletest",
            "googletest/include/gtest/gtest.h",
        ) {
            repo_paths.insert("com_google_googletest".to_string(), googletest_repo);
        } else {
            let googletest_repo = external_cache.join("com_google_googletest");
            std::fs::create_dir_all(&googletest_repo)?;
            std::fs::write(
                googletest_repo.join("BUILD"),
                "# Stub BUILD for googletest - run scripts/bootstrap_googletest.sh for source\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                     name = \"gtest\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     includes = [\"/usr/include\"],\n\
                     linkopts = [\"-lgmock\", \"-lgtest\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\n\
                 cc_library(\n\
                     name = \"gtest_main\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     includes = [\"/usr/include\"],\n\
                     linkopts = [\"-lgtest_main\", \"-lgmock\", \"-lgtest\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n\n\
                 cc_library(\n\
                     name = \"gmock\",\n\
                     srcs = [],\n\
                     hdrs = [],\n\
                     includes = [\"/usr/include\"],\n\
                     linkopts = [\"-lgmock\"],\n\
                     visibility = [\"//visibility:public\"],\n\
                 )\n",
            )?;
            eprintln!(
                "⚠️  @com_google_googletest uses the system adapter; \
                 run scripts/bootstrap_googletest.sh --apply for pinned source"
            );
            repo_paths.insert("com_google_googletest".to_string(), googletest_repo);
        }

        // com_google_benchmark stub — Rush uses the system Google Benchmark
        // (Debian libbenchmark-dev: libbenchmark.so + libbenchmark_main.a),
        // exactly like the googletest stub above. tsl/platform:test_main —
        // linked by EVERY tf_cc_test — unconditionally calls
        // benchmark::Initialize/RunSpecifiedBenchmarks/PrintDefaultHelp, so its
        // `@com_google_benchmark//:benchmark` dep MUST resolve to a target that
        // carries the -lbenchmark linkopt. Without this synth the dep fell to
        // the missing-dep handler (target contains "benchmark") → empty
        // register_system_stub → no -lbenchmark → `undefined reference to
        // benchmark::*` at the harness link (it only stayed hidden while the
        // corpus relinked against warm/cached binaries). Mirrors the system-lib
        // arm in build_orchestrator_ext.rs (-lbenchmark{,_main} -lpthread).
        let benchmark_repo = external_cache.join("com_google_benchmark");
        std::fs::create_dir_all(&benchmark_repo)?;
        std::fs::write(
            benchmark_repo.join("BUILD"),
            "# Stub BUILD for google benchmark - Rush uses system libbenchmark\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"benchmark\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 includes = [\"/usr/include\"],\n\
                 linkopts = [\"-lbenchmark\", \"-lpthread\"],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n\n\
             cc_library(\n\
                 name = \"benchmark_main\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 includes = [\"/usr/include\"],\n\
                 linkopts = [\"-lbenchmark_main\", \"-lbenchmark\", \"-lpthread\"],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n",
        )?;
        repo_paths.insert("com_google_benchmark".to_string(), benchmark_repo);

        // @absl_py - REAL abseil-py Python source (NO STUBS).
        // py_strict_binary tools used by genrules (e.g. the MLIR
        // gen_quantized_function_library) `from absl import app/flags` and call
        // app.run()/flags.DEFINE_* — an empty stub py_library(srcs=[]) makes
        // `import absl` silently resolve to a code-less namespace package and the
        // tool crashes at runtime. The real source (which ships its own BUILD
        // files defining //absl:app, //absl/flags, ...) is materialized by
        // scripts/bootstrap_absl_py.sh (SHA256-verified abseil-py v1.0.0) into
        // RUSH_GLOBAL_CLONE_CACHE/absl_py. Register that real tree; if it isn't
        // present, log and skip rather than emit a fake stub (run the bootstrap).
        if let Some(absl_py_repo) = resolve_clone_path("absl_py", "absl/app.py") {
            repo_paths.insert("absl_py".to_string(), absl_py_repo);
        } else {
            eprintln!(
                "⚠️  @absl_py not materialized (no absl/app.py in workspace/global \
                 clone cache); run scripts/bootstrap_absl_py.sh. py_strict_binary \
                 tools that import absl (e.g. MLIR gen_quantized_function_library) \
                 will fail until then."
            );
        }

        // FIX: linenoise stub - Line editing library used by TensorFlow profiler
        // This is a small C library for command-line editing, similar to readline
        let linenoise_repo = external_cache.join("linenoise");
        std::fs::create_dir_all(linenoise_repo.join("linenoise"))?;
        // TF builds linenoise from source (a two-file C library; there is
        // no system package to adapt to). When the pinned clone is
        // materialized (scripts/bootstrap_light_repos.sh), symlink the two
        // files and declare the REAL root target TF's build_file declares.
        // Without the clone the root stays bare and the stub-label audit
        // keeps reporting the gap - a declared target whose sources do not
        // exist would be a stub wearing a real name.
        let linenoise_root_lib = match resolve_clone_path("linenoise", "linenoise.c") {
            Some(clone) => {
                let mut linked = 0;
                for f in ["linenoise.c", "linenoise.h"] {
                    let dest = linenoise_repo.join(f);
                    if dest.symlink_metadata().is_ok() {
                        let _ = std::fs::remove_file(&dest);
                    }
                    #[cfg(unix)]
                    if std::os::unix::fs::symlink(clone.join(f), &dest).is_ok() {
                        linked += 1;
                    }
                }
                if linked == 2 {
                    "\ncc_library(\n\
                         name = \"linenoise\",\n\
                         srcs = [\"linenoise.c\"],\n\
                         hdrs = [\"linenoise.h\"],\n\
                         visibility = [\"//visibility:public\"],\n\
                     )\n"
                } else {
                    ""
                }
            }
            None => "",
        };
        std::fs::write(
            linenoise_repo.join("BUILD"),
            format!(
                "# Stub BUILD for linenoise\n\
                 package(default_visibility = [\"//visibility:public\"])\n{linenoise_root_lib}"
            ),
        )?;
        std::fs::write(
            linenoise_repo.join("linenoise/BUILD"),
            "# Stub BUILD for linenoise/linenoise\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"linenoise\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n",
        )?;
        repo_paths.insert("linenoise".to_string(), linenoise_repo);

        // TensorFlow's root @pybind11 target is header-only. Materialize the
        // exact header set from the checksum-stamped v2.10.4 bootstrap clone
        // into the generated repository so both the action graph and the
        // compiler consume declared files under a stable repository path.
        let pybind11_repo = external_cache.join("pybind11");
        let pybind11_clone = verified_pybind11_clone(&global_clone_root);
        materialize_pybind11_repository(&pybind11_repo, pybind11_clone.as_deref())?;
        repo_paths.insert("pybind11".to_string(), pybind11_repo);

        // FIX: zlib stub - zlib compression library
        // TensorFlow uses @zlib//zlib:zlib format
        let zlib_repo = external_cache.join("zlib");
        std::fs::create_dir_all(zlib_repo.join("zlib"))?;
        std::fs::write(
            zlib_repo.join("BUILD"),
            "# Stub BUILD for zlib - Rush uses system zlib\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"zlib\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 includes = [\"/usr/include\"],\n\
                 linkopts = [\"-lz\"],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n",
        )?;
        std::fs::write(
            zlib_repo.join("zlib/BUILD"),
            "# Stub BUILD for zlib/zlib - Rush uses system zlib\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             cc_library(\n\
                 name = \"zlib\",\n\
                 srcs = [],\n\
                 hdrs = [],\n\
                 includes = [\"/usr/include\"],\n\
                 linkopts = [\"-lz\"],\n\
                 visibility = [\"//visibility:public\"],\n\
             )\n",
        )?;
        repo_paths.insert("zlib".to_string(), zlib_repo);

        // FIX: com_google_absl stub - Google Abseil C++ library.
        // Prefer a TF-pinned clone when present (absl/types/optional.h
        // exists) — Debian's libabsl20240722 renames the inline-versioned
        // `absl::optional` into `absl::debian7::optional` and the
        // mismatch breaks TF's covariant overrides on
        // //tensorflow/c/experimental/stream_executor:stream_executor.
        // Fall back to system /usr/include when no clone is present.
        let absl_repo = external_cache.join("com_google_absl");
        let absl_clone = resolve_clone_path("com_google_absl", "absl/types/optional.h");
        let absl_has_source_clone = absl_clone.is_some();
        let absl_inc = match &absl_clone {
            Some(p) => p.display().to_string(),
            None => "/usr/include".to_string(),
        };
        std::fs::create_dir_all(absl_repo.join("absl/strings"))?;
        std::fs::create_dir_all(absl_repo.join("absl/synchronization"))?;
        std::fs::create_dir_all(absl_repo.join("absl/types"))?;
        std::fs::create_dir_all(absl_repo.join("absl/base"))?;
        std::fs::create_dir_all(absl_repo.join("absl/container"))?;
        std::fs::create_dir_all(absl_repo.join("absl/hash"))?;
        std::fs::create_dir_all(absl_repo.join("absl/status"))?;
        std::fs::create_dir_all(absl_repo.join("absl/time"))?;
        std::fs::create_dir_all(absl_repo.join("absl/log"))?;
        std::fs::create_dir_all(absl_repo.join("absl/algorithm"))?;
        std::fs::create_dir_all(absl_repo.join("absl/memory"))?;
        std::fs::create_dir_all(absl_repo.join("absl/meta"))?;
        std::fs::create_dir_all(absl_repo.join("absl/numeric"))?;
        std::fs::create_dir_all(absl_repo.join("absl/functional"))?;
        std::fs::create_dir_all(absl_repo.join("absl/utility"))?;
        std::fs::create_dir_all(absl_repo.join("absl/cleanup"))?;
        std::fs::create_dir_all(absl_repo.join("absl/debugging"))?;
        std::fs::create_dir_all(absl_repo.join("absl/random"))?;
        std::fs::create_dir_all(absl_repo.join("absl/flags"))?;
        std::fs::create_dir_all(absl_repo.join("absl/crc"))?;

        std::fs::write(
            absl_repo.join("BUILD"),
            "# Stub BUILD for com_google_absl - Rush uses system Abseil\n\
             package(default_visibility = [\"//visibility:public\"])\n",
        )?;

        // When a TF-pinned Abseil source clone is present, rush COMPILES TF
        // against its headers (inline namespace lts_20230125). The system
        // libabsl (Debian lts_20240722) is a DIFFERENT inline namespace, so
        // linking `-labsl_*` leaves every absl::lts_20230125::* symbol
        // undefined. Build the pinned source into real static PIC archives
        // (scripts/bootstrap_absl.sh, cmake) and link THOSE instead.
        //
        // Abseil's components are heavily interdependent; rather than model
        // its DAG, expose ONE canonical target @com_google_absl//absl:
        // all_pinned_static that wraps every libabsl_*.a in a single
        // `-Wl,--start-group ... -Wl,--end-group` (archives stay lazy, so
        // over-approximation is cheap). Every per-component adapter then just
        // `deps` on it — so the group lands on the consumer link line exactly
        // once (NOT once per component, which dedup wouldn't collapse). NO
        // STUBS: when the archives aren't built yet the canonical target
        // carries no linkopts, so the link fails cleanly on absl symbols and
        // prompts the bootstrap — same HONEST contract as @ruy/@cpuinfo.
        // (Codex gpt-5.5 xhigh thread 019e83bc.)
        let absl_static_libs: Vec<std::path::PathBuf> = if let Some(ref clone) = absl_clone {
            let libdir = clone.join(".rush-absl/lib");
            let mut libs: Vec<std::path::PathBuf> = std::fs::read_dir(&libdir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("libabsl_") && n.ends_with(".a"))
                        .unwrap_or(false)
                })
                .collect();
            libs.sort();
            libs
        } else {
            Vec::new()
        };
        if absl_has_source_clone {
            // Canonical group target. Empty linkopts when unbuilt (degrade
            // honestly → bootstrap prompt); the real group when built.
            let absl_canonical_linkopts = if absl_static_libs.is_empty() {
                String::new()
            } else {
                let mut parts: Vec<String> = vec!["\"-Wl,--start-group\"".to_string()];
                parts.extend(
                    absl_static_libs
                        .iter()
                        .map(|p| format!("\"{}\"", p.display())),
                );
                parts.push("\"-Wl,--end-group\"".to_string());
                parts.push("\"-pthread\"".to_string());
                parts.push("\"-ldl\"".to_string());
                parts.push("\"-lrt\"".to_string());
                parts.join(", ")
            };
            std::fs::write(
                absl_repo.join("absl/BUILD"),
                format!(
                    "# Canonical pinned-Abseil static link group (lts_20230125).\n\
                     package(default_visibility = [\"//visibility:public\"])\n\n\
                     cc_library(\n\
                     \x20   name = \"all_pinned_static\",\n\
                     \x20   srcs = [],\n\
                     \x20   hdrs = [],\n\
                     \x20   includes = [\"{inc}\"],\n\
                     \x20   linkopts = [{opts}],\n\
                     \x20   visibility = [\"//visibility:public\"],\n\
                     )\n",
                    inc = absl_inc,
                    opts = absl_canonical_linkopts,
                ),
            )?;
        }

        // Generate stub BUILD files for each abseil subpackage
        let absl_packages: Vec<(&str, Vec<&str>)> = vec![
            ("strings", vec!["strings", "str_format", "cord", "string_view", "ascii"]),
            ("synchronization", vec!["synchronization", "mutex", "notification", "barrier"]),
            ("types", vec!["types", "optional", "span", "variant", "any", "bad_optional_access"]),
            ("base", vec!["base", "core_headers", "dynamic_annotations", "config", "raw_logging_internal", "log_severity", "throw_delegate", "endian"]),
            ("container", vec!["container", "flat_hash_map", "flat_hash_set", "node_hash_map", "node_hash_set", "inlined_vector", "fixed_array", "btree"]),
            ("hash", vec!["hash", "city", "hash_testing"]),
            ("status", vec!["status", "statusor"]),
            ("time", vec!["time", "civil_time", "time_zone"]),
            ("log", vec!["log", "die_if_null", "check", "absl_check", "absl_log"]),
            ("algorithm", vec!["algorithm", "container"]),
            ("memory", vec!["memory"]),
            ("meta", vec!["meta", "type_traits"]),
            ("numeric", vec!["numeric", "int128", "bits"]),
            ("functional", vec!["functional", "function_ref", "bind_front", "any_invocable"]),
            ("utility", vec!["utility"]),
            ("cleanup", vec!["cleanup"]),
            ("debugging", vec!["debugging", "leak_check", "stacktrace", "symbolize", "failure_signal_handler"]),
            ("random", vec!["random", "distributions", "random_internal_pool_urbg", "seed_gen_exception", "bit_gen_ref", "seed_sequences"]),
            ("flags", vec!["flags", "flag", "marshalling", "parse", "usage", "reflection", "commandlineflag"]),
            ("crc", vec!["crc32c"]),
        ];

        for (pkg, targets) in absl_packages {
            let mut build_content = format!(
                "# Stub BUILD for com_google_absl/absl/{} - Rush uses system Abseil\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n",
                pkg
            );
            for target in targets {
                if absl_has_source_clone {
                    // Pinned path: headers from the clone + link the canonical
                    // pinned-static group via deps (NOT system -labsl_*).
                    build_content.push_str(&format!(
                        "cc_library(\n\
                             name = \"{}\",\n\
                             srcs = [],\n\
                             hdrs = [],\n\
                             includes = [\"{inc}\"],\n\
                             deps = [\"@com_google_absl//absl:all_pinned_static\"],\n\
                             visibility = [\"//visibility:public\"],\n\
                         )\n\n",
                        target, inc = absl_inc
                    ));
                } else {
                    // No pinned clone: fall back to system Abseil headers + libs
                    // (self-consistent for a non-TF-pinned build).
                    build_content.push_str(&format!(
                        "cc_library(\n\
                             name = \"{}\",\n\
                             srcs = [],\n\
                             hdrs = [],\n\
                             includes = [\"{inc}\"],\n\
                             linkopts = [\"-labsl_{}\"],\n\
                             visibility = [\"//visibility:public\"],\n\
                         )\n\n",
                        target, target, inc = absl_inc
                    ));
                }
            }
            std::fs::write(absl_repo.join(format!("absl/{}/BUILD", pkg)), build_content)?;
        }

        repo_paths.insert("com_google_absl".to_string(), absl_repo);

        // tf_runtime stub — real TFRT headers are fetched into the global
        // clone cache at `<global>/tf_runtime/include/tfrt/**` from the
        // upstream runtime archive at the TF-pinned commit (see
        // workspace.bzl). Prefer that populated slot via
        // resolve_clone_path so consumer `#include "tfrt/X.h"` resolves;
        // fall back to the empty workspace-local slot when headers
        // haven't been fetched yet.
        let tf_runtime_repo =
            resolve_clone_path("tf_runtime", "include/tfrt/concurrency/async_value.h")
                .unwrap_or_else(|| external_cache.join("tf_runtime"));
        std::fs::create_dir_all(&tf_runtime_repo)?;
        // Hold off on rewriting BUILD if the include tree exists —
        // this keeps a human-edited BUILD during iteration and only
        // synthesizes the stub when the tree hasn't been populated
        // yet. Writing globs that resolve to zero files (when
        // include/ is empty) still yields a workable cc_library, so
        // regardless of whether upstream headers are present, the
        // BUILD is generated once per run.
        // Each tf_runtime cc_library propagates `-I<repo>/include` AND deps on
        // @llvm-project//llvm:Support — TFRT headers (e.g. tfrt/support/logging.h)
        // transitively `#include "llvm/Support/raw_ostream.h"`, so consumers like
        // @local_xla//xla/runtime:async_runtime need the pinned-LLVM include dirs
        // that the @llvm-project CcInfo exposes. Without the dep, only -I<include>
        // reached the compile and the llvm/* headers were not found.
        let tf_runtime_targets: &[(&str, &str)] = &[
            ("async_value", "include/tfrt/concurrency/*.h"),
            ("ref_count", "include/tfrt/concurrency/*.h"),
            ("hostcontext", "include/tfrt/host_context/*.h"),
            // Upstream TFRT exposes an alwayslink variant of hostcontext;
            // TF's next_pluggable_device example_plugin deps on it. Same
            // headers + libtfrt.a seed — without this entry the dep is
            // simply missing and the consumer compiles with NO tfrt or
            // llvm include dirs (then falls into any stray llvm headers
            // on the default search path).
            ("hostcontext_alwayslink", "include/tfrt/host_context/*.h"),
            ("support", "include/tfrt/support/*.h"),
            ("dtype", "include/tfrt/dtype/*.h"),
            // #39 Phase R corpus tail: the runtime_fallback / tfrt tests
            // (attr_util_test, tensor_util_test, …) depend on @tf_runtime//:tensor
            // and :core_runtime. Those object files are (tensor) or are now
            // (core_runtime) compiled into libtfrt.a by bootstrap_tf_runtime.sh,
            // so expose real header-views over the SAME archive instead of
            // letting rush stub the dep as an empty SystemCcLibrary (which dropped
            // the TFRT symbols and failed the link). tensor_alwayslink mirrors the
            // upstream alwayslink variant some kernels dep on.
            ("tensor", "include/tfrt/tensor/*.h"),
            ("tensor_alwayslink", "include/tfrt/tensor/*.h"),
            ("core_runtime", "include/tfrt/core_runtime/*.h"),
            ("tfrt_all_hdrs", "include/tfrt/**/*.h"),
        ];
        // #39 Phase R: link the REAL libtfrt.a (built by
        // scripts/bootstrap_tf_runtime.sh from the pinned TFRT core source) so
        // the TFRT symbols that block dlopen — tsl::AsyncValue::*,
        // tfrt::HostContext, CreateMultiThreadedWorkQueue, Aligned{Alloc,Free},
        // CreateMallocAllocator, RegisterDeviceType, internal::LogStream* — are
        // DEFINED, not just header-visible. HONEST: linkopts seed the archive
        // ONLY when it is really present (else the targets stay header-only and
        // the link fails cleanly, prompting the bootstrap). `-pthread` since the
        // TFRT work queues use std::thread. dedup collapses the single
        // libtfrt.a across the several adapter targets at link time.
        // libtfrt.a (bootstrap-built pinned-TFRT core) is exposed as a `srcs`
        // PREBUILT archive (not a linkopt). rush's cc_library now routes
        // `.a`/`.so` srcs through CcInfo.linker_inputs (see rules_cc.rs
        // prebuilt_lib_src), so the archive lands inside the consumer link's
        // global --start-group — where libtfrt.a's async_value.o back-reference
        // to the pinned-absl (lts_20230125) BlockingCounter resolves against
        // the absl archives the framework closure already places in that group.
        // A raw-linkopt libtfrt.a was emitted AFTER the closed group, leaving
        // those back-references undefined. HONEST: srcs is seeded only when the
        // archive really exists (else header-only, link fails cleanly →
        // bootstrap prompt). `@com_google_absl//absl:all_pinned_static` stays
        // on deps for the -I and as the absl source-of-truth.
        let tfrt_lib = tf_runtime_repo.join("libtfrt.a");
        let (tfrt_srcs, tfrt_linkopts) = if tfrt_lib.is_file() {
            ("\"libtfrt.a\"".to_string(), "\"-pthread\", \"-ldl\", \"-lrt\"".to_string())
        } else {
            (String::new(), String::new())
        };
        let mut tf_runtime_build = String::from(
            "# tf_runtime stub BUILD — headers live under include/tfrt/ when\n\
             # fetched (scripts/bootstrap_tf_runtime.sh). cc_library entries\n\
             # propagate -I<repo>/include via `includes` plus @llvm-project//llvm:Support\n\
             # so transitive `#include \"llvm/Support/*.h\"` from TFRT headers resolve.\n\
             package(default_visibility = [\"//visibility:public\"])\n\n",
        );
        for (name, hdr_glob) in tf_runtime_targets {
            tf_runtime_build.push_str(&format!(
                "cc_library(\n\
                 \x20   name = \"{name}\",\n\
                 \x20   srcs = [{srcs}],\n\
                 \x20   hdrs = glob([\"{hdr_glob}\", \"third_party/llvm_derived/include/**/*.h\", \"cpp_tests/include/**/*.h\"], allow_empty = True),\n\
                 \x20   includes = [\"include\", \"third_party/llvm_derived/include\", \"cpp_tests/include\"],\n\
                 \x20   linkopts = [{linkopts}],\n\
                 \x20   deps = [\"@llvm-project//llvm:Support\", \"@com_google_absl//absl:all_pinned_static\"],\n\
                 \x20   visibility = [\"//visibility:public\"],\n\
                 )\n\n",
                name = name,
                srcs = tfrt_srcs,
                hdr_glob = hdr_glob,
                linkopts = tfrt_linkopts,
            ));
        }
        // #39 corpus tail: td_library targets for the TFRT TableGen include
        // tree. The TF tfrt dialects (tensorflow/compiler/mlir/tfrt/ir:
        // tfrt_fallback_async etc.) gentbl over .td that `include
        // "tfrt/tfrt_op_base.td"` / "tfrt/core_runtime/opdefs/corert_base.td"
        // / "tfrt/compiler/opdefs/tfrt_op_interfaces.td", and depend on
        // @tf_runtime//:{OpBaseTdFiles,CoreRTTdFiles,compiler_td_files} to put
        // <repo>/include on the tblgen -I path. Without these targets rush
        // stubbed them empty → mlir-tblgen could not resolve the include →
        // "Unexpected token at top level" at tfrt_fallback_async.td. Mirror the
        // upstream (stripped) TFRT BUILD defs verbatim; `includes=["include"]`
        // is what supplies the -I root. The .td srcs are present from
        // bootstrap_tf_runtime.sh's include/ copy.
        tf_runtime_build.push_str(
            "td_library(\n\
             \x20   name = \"OpBaseTdFiles\",\n\
             \x20   srcs = [\n\
             \x20       \"include/tfrt/basic_kernels/opdefs/tfrt_base.td\",\n\
             \x20       \"include/tfrt/tensor/opdefs/host_tensor.td\",\n\
             \x20       \"include/tfrt/tensor/opdefs/tensor.td\",\n\
             \x20       \"include/tfrt/tensor/opdefs/tensor_shape_base.td\",\n\
             \x20       \"include/tfrt/tfrt_op_base.td\",\n\
             \x20   ],\n\
             \x20   includes = [\"include\"],\n\
             \x20   visibility = [\"//visibility:public\"],\n\
             )\n\n\
             td_library(\n\
             \x20   name = \"CoreRTTdFiles\",\n\
             \x20   srcs = [\n\
             \x20       \"include/tfrt/core_runtime/opdefs/corert_base.td\",\n\
             \x20       \"include/tfrt/core_runtime/opdefs/corert_traits.td\",\n\
             \x20   ],\n\
             \x20   includes = [\"include\"],\n\
             \x20   visibility = [\"//visibility:public\"],\n\
             )\n\n\
             td_library(\n\
             \x20   name = \"compiler_td_files\",\n\
             \x20   srcs = [\n\
             \x20       \"include/tfrt/compiler/opdefs/tfrt_op_interfaces.td\",\n\
             \x20       \"include/tfrt/compiler/opdefs/tfrt_traits.td\",\n\
             \x20   ],\n\
             \x20   includes = [\"include\"],\n\
             \x20   deps = [\"@llvm-project//mlir:OpBaseTdFiles\"],\n\
             \x20   visibility = [\"//visibility:public\"],\n\
             )\n\n",
        );
        std::fs::write(tf_runtime_repo.join("BUILD"), tf_runtime_build)?;
        repo_paths.insert("tf_runtime".to_string(), tf_runtime_repo);

        // Facade for @build_bazel_rules_android. TF's lite/* BUILDs
        // `load(@build_bazel_rules_android//android:rules.bzl,
        // "android_library", ...)` etc. Preserve each declaration as a real
        // target. Rush's Android rule instances carry a deferred configuration
        // error, so unrelated host C++ siblings remain loadable while a selected
        // or transitively reachable Android target fails before analysis.
        let android_rules_repo = external_cache.join("build_bazel_rules_android");
        std::fs::create_dir_all(android_rules_repo.join("android"))?;
        std::fs::write(
            android_rules_repo.join("BUILD"),
            "# Rush facade BUILD for build_bazel_rules_android\n\
             package(default_visibility = [\"//visibility:public\"])\n",
        )?;
        std::fs::write(
            android_rules_repo.join("android/BUILD"),
            "# Rush facade BUILD for build_bazel_rules_android/android\n\
             package(default_visibility = [\"//visibility:public\"])\n",
        )?;
        std::fs::write(
            android_rules_repo.join("android/rules.bzl"),
            "# Rush facade: retain Android declarations and reject them if configured.\n\
             def android_library(**kwargs):\n    native.android_library(**kwargs)\n\n\
             def android_binary(**kwargs):\n    native.android_binary(**kwargs)\n\n\
             def aar_import(**kwargs):\n    native.aar_import(**kwargs)\n",
        )?;
        repo_paths.insert(
            "build_bazel_rules_android".to_string(),
            android_rules_repo,
        );

        Ok(repo_paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn tf217_rules_java_facade_forwards_to_fail_loud_targets() {
        use std::sync::{Arc, Mutex};

        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("WORKSPACE"), "").unwrap();
        let repo = materialize_tf217_rules_java_facade(&workspace.path().join("external")).unwrap();
        let package = workspace.path().join("p");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("BUILD"),
            r#"
load("@rules_java//java:defs.bzl", "java_plugin")

java_plugin(name = "processor", deps = [])
cc_library(name = "host", srcs = [])
"#,
        )
        .unwrap();

        let mut loader = rush_starlark::BzlLoader::new(workspace.path().to_path_buf());
        loader.add_external_repo("rules_java".to_string(), repo);
        let build_file = rush_incremental::tracked::test_scope(workspace.path(), || {
            rush_parse::parse_build_file_with_bzl_loader(
                &package.join("BUILD"),
                Some(Arc::new(Mutex::new(loader))),
            )
        })
        .unwrap();
        assert_eq!(
            build_file
                .rules
                .iter()
                .map(|rule| rule.rule_type.as_str())
                .collect::<Vec<_>>(),
            vec!["java_plugin", "cc_library"]
        );

        let mut registry = rush_rules::registry::RuleRegistry::new();
        registry.register(Box::new(rush_rules::rules_cc::CcLibraryRule));
        for java_rule in crate::rules_java::UnsupportedJavaRule::all() {
            registry.register(Box::new(java_rule));
        }
        let instances = crate::rule_execution::build_file_to_instances(&build_file, &registry)
            .expect("loaded facade rules must instantiate");
        let host = instances
            .iter()
            .find(|instance| instance.target_name() == "host")
            .unwrap();
        assert!(host.configuration_error().is_none());

        let plugin = instances
            .iter()
            .find(|instance| instance.target_name() == "processor")
            .unwrap();
        let error = plugin
            .configuration_error()
            .expect("selected loaded Java target must fail loudly");
        assert!(error.contains("java_plugin"), "{error}");
        assert!(error.contains("JavaPluginInfo"), "{error}");

        let context = rush_rules::context::BuildContext::new(
            workspace.path().to_path_buf(),
            workspace.path().join("rush-out"),
        );
        let error = plugin.build(&context).await.unwrap_err().to_string();
        assert!(error.contains("cannot be configured"), "{error}");
    }

    #[test]
    fn pybind11_pin_matches_the_bootstrap_source() {
        let bootstrap = include_str!("../../../scripts/bootstrap_light_repos.sh");
        assert!(bootstrap.contains("https://github.com/pybind/pybind11/archive/v2.10.4.tar.gz"));
        assert!(bootstrap.contains(PYBIND11_BOOTSTRAP_SHA256));
        assert!(bootstrap.contains("pybind11-2.10.4"));
        assert!(bootstrap.contains(PYBIND11_HEADER_WITNESS));
    }

    #[test]
    fn pybind11_clone_requires_the_pinned_checksum_and_header() {
        let cache = tempfile::tempdir().unwrap();
        let clone = cache.path().join("pybind11");
        let witness = clone.join(PYBIND11_HEADER_WITNESS);
        std::fs::create_dir_all(witness.parent().unwrap()).unwrap();
        std::fs::write(&witness, "// pybind11").unwrap();

        assert!(verified_pybind11_clone(cache.path()).is_none());
        std::fs::write(clone.join(".rush-bootstrap-sha256"), "wrong\n").unwrap();
        assert!(verified_pybind11_clone(cache.path()).is_none());
        std::fs::write(
            clone.join(".rush-bootstrap-sha256"),
            format!("{PYBIND11_BOOTSTRAP_SHA256}\n"),
        )
        .unwrap();
        assert_eq!(verified_pybind11_clone(cache.path()), Some(clone.clone()));

        std::fs::remove_file(witness).unwrap();
        assert!(verified_pybind11_clone(cache.path()).is_none());
    }

    #[test]
    fn pybind11_repository_declares_and_materializes_real_headers() {
        use rush_parse::Value as ParsedValue;

        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("WORKSPACE"), "").unwrap();
        let cache = workspace.path().join("clone-cache");
        let clone = cache.join("pybind11");
        for header in [
            "include/pybind11/attr.h",
            "include/pybind11/common.h",
            "include/pybind11/eigen.h",
            PYBIND11_HEADER_WITNESS,
            "include/pybind11/detail/common.h",
            "include/pybind11/detail/type_caster_base.h",
        ] {
            let path = clone.join(header);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("// {header}\n")).unwrap();
        }
        std::fs::write(
            clone.join(".rush-bootstrap-sha256"),
            format!("{PYBIND11_BOOTSTRAP_SHA256}\n"),
        )
        .unwrap();

        let repo = workspace.path().join(".cache/rush/external/pybind11");
        let verified = verified_pybind11_clone(&cache).unwrap();
        materialize_pybind11_repository(&repo, Some(&verified)).unwrap();

        let build = std::fs::read_to_string(repo.join("BUILD")).unwrap();
        assert!(build.contains("name = \"pybind11\""));
        assert!(build.contains("includes = [\"include\"]"));
        assert!(build.contains("strip_include_prefix = \"include\""));
        assert!(build.contains("deps = [\"@org_tensorflow//third_party/python_runtime:headers\"]"));
        assert!(!build.contains("Stub"));
        assert!(repo.join(PYBIND11_HEADER_WITNESS).is_file());
        assert!(repo
            .join("include/pybind11/detail/type_caster_base.h")
            .is_file());
        assert!(!repo.join("include/pybind11/common.h").exists());
        assert!(!repo.join("include/pybind11/eigen.h").exists());

        let parsed = rush_parse::parse_build_file(&repo.join("BUILD")).unwrap();
        let rule = parsed
            .rules
            .iter()
            .find(|rule| rule.name == "pybind11")
            .unwrap();
        let hdrs = match rule.attributes.get("hdrs").unwrap() {
            ParsedValue::List(values) => values,
            value => panic!("expected hdrs list, got {value:?}"),
        };
        let rendered_hdrs: Vec<&str> = hdrs
            .iter()
            .map(|value| match value {
                ParsedValue::String(value) => value.as_str(),
                value => panic!("expected header string, got {value:?}"),
            })
            .collect();
        assert!(rendered_hdrs.contains(&PYBIND11_HEADER_WITNESS));
        assert!(rendered_hdrs.contains(&"include/pybind11/detail/common.h"));
        assert!(!rendered_hdrs.contains(&"include/pybind11/common.h"));
        assert!(!rendered_hdrs.contains(&"include/pybind11/eigen.h"));
        let deps = match rule.attributes.get("deps").unwrap() {
            ParsedValue::List(values) => values,
            value => panic!("expected deps list, got {value:?}"),
        };
        assert!(matches!(
            deps.as_slice(),
            [ParsedValue::String(dep)]
                if dep == "@org_tensorflow//third_party/python_runtime:headers"
        ));

        let original_build = build;
        std::fs::write(clone.join(PYBIND11_HEADER_WITNESS), "// refreshed\n").unwrap();
        materialize_pybind11_repository(&repo, Some(&verified)).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join(PYBIND11_HEADER_WITNESS)).unwrap(),
            "// refreshed\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("BUILD")).unwrap(),
            original_build
        );

        std::fs::remove_dir_all(clone.join("include/pybind11/detail")).unwrap();
        std::fs::write(clone.join(PYBIND11_HEADER_WITNESS), "// incomplete\n").unwrap();
        assert!(materialize_pybind11_repository(&repo, Some(&verified)).is_err());
        assert_eq!(
            std::fs::read_to_string(repo.join(PYBIND11_HEADER_WITNESS)).unwrap(),
            "// refreshed\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("BUILD")).unwrap(),
            original_build
        );
    }

    #[test]
    fn pybind11_repository_withholds_target_without_verified_source() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("WORKSPACE"), "").unwrap();
        let repo = workspace.path().join(".cache/rush/external/pybind11");
        let stale = repo.join(PYBIND11_HEADER_WITNESS);
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, "// stale\n").unwrap();

        materialize_pybind11_repository(&repo, None).unwrap();

        assert!(!repo.join("include").exists());
        let build = std::fs::read_to_string(repo.join("BUILD")).unwrap();
        assert!(!build.contains("cc_library("));
        let parsed = rush_parse::parse_build_file(&repo.join("BUILD")).unwrap();
        assert!(parsed.rules.is_empty());
    }

    #[test]
    fn test_protobuf_mapping() {
        // Assert the PRECEDENCE, against caches this test creates. The
        // previous version asserted "/usr/include" from whatever the host
        // had, so it passed on a machine with no pinned protobuf and failed
        // on one with a pinned protobuf even though rush was choosing
        // correctly in both cases.
        let tmp = tempfile::tempdir().unwrap();

        // No pinned archive: system headers.
        let (inc, pinned) = protobuf_include_dir_for_cache(tmp.path());
        assert_eq!(inc, PathBuf::from("/usr/include"));
        assert!(!pinned);

        // Pinned archive plus a recorded INCLUDE_ROOT: that root wins, so a
        // compile cannot pick up 4.25 headers while linking the 3.21.9 .a.
        let pinned_dir = tmp.path().join("com_google_protobuf");
        std::fs::create_dir_all(&pinned_dir).unwrap();
        std::fs::write(pinned_dir.join("libprotobuf.a"), b"").unwrap();
        std::fs::write(pinned_dir.join("INCLUDE_ROOT"), "/pinned/include\n").unwrap();
        let (inc, pinned) = protobuf_include_dir_for_cache(tmp.path());
        assert_eq!(inc, PathBuf::from("/pinned/include"));
        assert!(pinned);

        // Pinned archive with no INCLUDE_ROOT falls back to <dir>/src, never
        // to the system headers.
        std::fs::remove_file(pinned_dir.join("INCLUDE_ROOT")).unwrap();
        let (inc, pinned) = protobuf_include_dir_for_cache(tmp.path());
        assert_eq!(inc, pinned_dir.join("src"));
        assert!(pinned);

        // An empty INCLUDE_ROOT is treated as absent rather than as "".
        std::fs::write(pinned_dir.join("INCLUDE_ROOT"), "  \n").unwrap();
        let (inc, _) = protobuf_include_dir_for_cache(tmp.path());
        assert_eq!(inc, pinned_dir.join("src"));
    }

    #[test]
    fn test_eigen_mapping() {
        let workspace_root = PathBuf::from("/tmp");
        let dirs = SystemPackageMapper::resolve_external_target(
            "eigen_archive",
            "eigen3",
            &workspace_root,
        )
        .unwrap();

        assert!(!dirs.is_empty());
        assert_eq!(dirs[0], PathBuf::from("/usr/include/eigen3"));
    }

    #[test]
    fn ml_dtypes_requires_a_declared_source_repository() {
        let root = tempfile::tempdir().unwrap();
        let header = root.path().join("external/ml_dtypes/include/float8.h");
        std::fs::create_dir_all(header.parent().unwrap()).unwrap();
        std::fs::write(header, b"// source witness\n").unwrap();
        assert!(!SystemPackageMapper::is_system_repo("ml_dtypes"));
        assert!(SystemPackageMapper::resolve_external_target(
            "ml_dtypes", "float8", root.path(),
        ).is_err(), "source headers must use their declared repository and CcInfo");
    }

    /// The gate must judge the protoc rush will actually run, and must stay
    /// silent when it cannot determine a version rather than guessing.
    ///
    /// The protoc path is injected, so this test mutates no process-wide
    /// state and cannot race the rest of the suite.
    #[test]
    fn protobuf_version_skew_compares_the_protoc_rush_runs() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let inc = dir.path().join("include");
        std::fs::create_dir_all(inc.join("google/protobuf/stubs")).unwrap();
        std::fs::write(
            inc.join("google/protobuf/stubs/common.h"),
            "#define GOOGLE_PROTOBUF_VERSION 3021009\n",
        )
        .unwrap();
        let inc_s = inc.display().to_string();

        let fake = |name: &str, ver: &str| -> std::path::PathBuf {
            let p = dir.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\necho \"libprotoc {}\"\n", ver)).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };

        let pb_config = rush_rules::rules_system::SystemProtobufConfig {
            found: false,
            include_dirs: Vec::new(),
            link_flags: Vec::new(),
            protoc_path: None,
        };

        // Compiler matches the headers: the build proceeds.
        let matching = fake("protoc-match", "3.21.9");
        assert!(protobuf_version_skew(&inc_s, true, &pb_config, &matching).is_none());

        // Real skew: refuse up front, naming both versions.
        let skewed = fake("protoc-skew", "3.12.4");
        let msg = protobuf_version_skew(&inc_s, true, &pb_config, &skewed)
            .expect("3.12 protoc against 3.21 headers must be rejected");
        assert!(msg.contains("3.12") && msg.contains("3.21"), "{}", msg);

        // Version undeterminable: stay silent instead of guessing.
        let missing = dir.path().join("no-such-protoc");
        assert!(protobuf_version_skew(&inc_s, true, &pb_config, &missing).is_none());
    }
}

#[cfg(test)]
mod portable_cache_root_tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn default_global_cache_uses_standard_user_locations() {
        assert_eq!(
            default_global_clone_cache_root_from(
                Some(OsString::from("/var/cache/example")),
                Some(OsString::from("/users/example")),
            ),
            PathBuf::from("/var/cache/example/rush/external")
        );
        assert_eq!(
            default_global_clone_cache_root_from(
                None,
                Some(OsString::from("/users/example")),
            ),
            PathBuf::from("/users/example/.cache/rush/external")
        );
        assert_eq!(
            default_global_clone_cache_root_from(None, None),
            PathBuf::from(".cache/rush/external")
        );
    }
}
