// Rush Rules - Hermetic Python runtime discovery
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Locate the hermetic Python interpreter for a rules_python runtime label.
//!
//! Bazel resolves `@python_<ver>//:py3_runtime` through the rules_python
//! toolchain; rush resolves the same interpreter from the materialized
//! platform repository (`rules_python++python+python_<ver>_<triple>`)
//! in the output bases it already consults for other toolchains.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

fn runtime_version(runtime_label: &str) -> String {
    runtime_label
        .trim_start_matches('@')
        .split("//")
        .next()
        .and_then(|repo| repo.strip_prefix("python_"))
        .unwrap_or(runtime_label)
        .to_string()
}

fn runtime_external_dirs(workspace_root: &Path) -> (PathBuf, Vec<PathBuf>) {
    let cache_root = std::env::var("RUSH_WORKSPACE_CACHE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root.join(".cache").join("rush"));
    let mut dirs = vec![cache_root.join("bazel_output_base").join("external")];
    if let Some(mojo) = std::env::var_os("RUSH_MOJO") {
        // <ob>/external/rules_mojo.../bin/mojo -> <ob>/external
        let mut path = PathBuf::from(mojo);
        for _ in 0..3 {
            path.pop();
        }
        dirs.push(path);
    }
    (cache_root, dirs)
}

fn find_exact_python(external_dirs: &[PathBuf], version: &str) -> Option<PathBuf> {
    for external in external_dirs {
        if let Ok(entries) = std::fs::read_dir(external) {
            let mut candidates: Vec<PathBuf> = entries
                .flatten()
                .filter(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    name.starts_with("rules_python++python+python_")
                        && name.contains(&format!("python_{}_", version))
                })
                .map(|entry| entry.path().join("bin").join("python3"))
                .filter(|path| path.is_file())
                .collect();
            candidates.sort();
            if let Some(path) = candidates.into_iter().next() {
                return Some(path);
            }
        }
    }
    None
}

fn python_platform_repository_for(version: &str, os: &str, arch: &str) -> Option<String> {
    let platform = match (os, arch) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        _ => return None,
    };
    Some(format!(
        "rules_python++python+python_{version}_{platform}"
    ))
}

fn materialize_exact_python(
    workspace_root: &Path,
    cache_root: &Path,
    version: &str,
) -> Result<bool> {
    if !workspace_root.join("MODULE.bazel").is_file()
        && !workspace_root.join("MODULE").is_file()
    {
        return Ok(false);
    }
    let repository = python_platform_repository_for(
        version,
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
    .ok_or_else(|| {
        anyhow!(
            "rules_python platform repository is unknown for {}-{}",
            std::env::consts::ARCH,
            std::env::consts::OS
        )
    })?;
    let bazel = match std::env::var_os("RUSH_BAZEL_PATH") {
        Some(path) if path.is_empty() => {
            return Err(anyhow!("RUSH_BAZEL_PATH is set but empty"));
        }
        Some(path) => PathBuf::from(path),
        None => PathBuf::from("bazel"),
    };
    fetch_repository_with_bazel(workspace_root, cache_root, &repository, &bazel)?;
    Ok(true)
}

fn fetch_repository_with_bazel(
    workspace_root: &Path,
    cache_root: &Path,
    repository: &str,
    bazel: &Path,
) -> Result<()> {
    let output_base = cache_root.join("bazel_output_base");
    let output = Command::new(bazel)
        .current_dir(workspace_root)
        .arg(format!("--output_base={}", output_base.display()))
        .arg("fetch")
        .arg(format!("--repo=@@{repository}"))
        .output()
        .with_context(|| {
            format!(
                "failed to start {} while materializing @@{}",
                bazel.display(),
                repository
            )
        })?;
    if !output.status.success() {
        return Err(anyhow!(
            "{} could not materialize @@{} (status {}): {}",
            bazel.display(),
            repository,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Find the hermetic python3 for a runtime label such as
/// `@python_3_10//:py3_runtime` (any label whose repo is `python_<ver>`),
/// or for a bare version string like `3_10`.
///
/// Resolution order:
/// 1. `RUSH_PY3` — explicit override, must exist.
/// 2. `<cache_root>/bazel_output_base/external/rules_python++python+python_<ver>_*/bin/python3`
///    where cache_root is `RUSH_WORKSPACE_CACHE_ROOT` or
///    `<workspace>/.cache/rush`.
/// 3. The output base `RUSH_MOJO` points into, when set — sibling
///    toolchains materialize in the same external tree.
/// 4. For Bzlmod workspaces, fetch the exact host-platform repository into
///    Rush's Bazel output base and repeat the lookup.
pub fn find_python3(workspace_root: &Path, runtime_label: &str) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("RUSH_PY3").map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        return Err(anyhow!(
            "RUSH_PY3 points to missing interpreter {}",
            path.display()
        ));
    }
    let version = runtime_version(runtime_label);

    let (cache_root, external_dirs) = runtime_external_dirs(workspace_root);
    if let Some(path) = find_exact_python(&external_dirs, &version) {
        return Ok(path);
    }
    let fetch_error = match materialize_exact_python(workspace_root, &cache_root, &version) {
        Ok(true) => {
            if let Some(path) = find_exact_python(&external_dirs, &version) {
                return Ok(path);
            }
            Some("Bazel reported success but the interpreter is absent".to_string())
        }
        Ok(false) => None,
        Err(error) => Some(format!("{error:#}")),
    };
    let fetch_detail = fetch_error
        .map(|error| format!("; automatic materialization failed: {error}"))
        .unwrap_or_default();
    Err(anyhow!(
        "no hermetic python_{} interpreter found (searched {} external dirs){}; \
         set RUSH_PY3",
        version,
        external_dirs.len(),
        fetch_detail
    ))
}

/// Like [`find_python3`], but when the requested version is not
/// materialized, fall back to the HIGHEST materialized rules_python
/// interpreter (deterministic; the workspace fetched it, so it is a real
/// toolchain of this build). This stdlib-only path does not trigger a fetch;
/// an explicit `RUSH_PY3` remains strict and never falls through.
pub fn find_python3_lenient(
    workspace_root: &Path,
    version_underscored: &str,
) -> Result<PathBuf> {
    if std::env::var_os("RUSH_PY3").is_some() {
        return find_python3(workspace_root, version_underscored);
    }
    let version = runtime_version(version_underscored);
    let (_, external_dirs) = runtime_external_dirs(workspace_root);
    if let Some(path) = find_exact_python(&external_dirs, &version) {
        return Ok(path);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for external in &external_dirs {
        if let Ok(entries) = std::fs::read_dir(external) {
            candidates.extend(
                entries
                    .flatten()
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .starts_with("rules_python++python+python_")
                    })
                    .map(|e| e.path().join("bin").join("python3"))
                    .filter(|p| p.is_file()),
            );
        }
    }
    candidates.sort();
    candidates.pop().ok_or_else(|| {
        anyhow!(
            "no hermetic rules_python interpreter materialized at all; \
             set RUSH_PY3 (requested version {})",
            version_underscored
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn canonical_platform_repository_tracks_python_abi() {
        assert_eq!(
            python_platform_repository_for("3_13", "linux", "x86_64"),
            Some(
                "rules_python++python+python_3_13_x86_64-unknown-linux-gnu"
                    .to_string()
            )
        );
        assert_eq!(
            python_platform_repository_for("3_12", "macos", "aarch64"),
            Some(
                "rules_python++python+python_3_12_aarch64-apple-darwin"
                    .to_string()
            )
        );
        assert_eq!(
            python_platform_repository_for("3_13", "windows", "x86_64"),
            None
        );
    }

    #[cfg(all(unix, target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn fetches_exact_runtime_into_configured_output_base() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let cache = temp.path().join("cache");
        let fake_bazel = temp.path().join("bazel");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            &fake_bazel,
            r#"#!/bin/sh
set -eu
output_base=${1#--output_base=}
test "$2" = "fetch"
test "$3" = "--repo=@@rules_python++python+python_3_13_x86_64-unknown-linux-gnu"
repo=${3#--repo=@@}
mkdir -p "$output_base/external/$repo/bin"
touch "$output_base/external/$repo/bin/python3"
printf '%s\n' "$@" > "$output_base/invocation.txt"
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_bazel).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_bazel, permissions).unwrap();

        let repository = python_platform_repository_for("3_13", "linux", "x86_64")
            .unwrap();
        fetch_repository_with_bazel(
            &workspace,
            &cache,
            &repository,
            &fake_bazel,
        )
        .unwrap();

        let external = cache.join("bazel_output_base").join("external");
        assert!(find_exact_python(&[external], "3_13").is_some());
        let invocation = std::fs::read_to_string(
            cache.join("bazel_output_base").join("invocation.txt"),
        )
        .unwrap();
        assert!(invocation.contains("--output_base="));
        assert!(invocation.contains("--repo=@@rules_python++python+python_3_13"));
    }
}
