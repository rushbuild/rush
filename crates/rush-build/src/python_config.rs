//! Materialize a typed `@local_config_python//:python_headers` repository.
//!
//! TensorFlow's hermetic Python repository contains real headers, but Rush's
//! historical `local_config_python` placeholder exposed an empty `cc_library`.
//! This module asks the selected interpreter for its configured include tree,
//! records every source observation through tracked IO, and copies the headers
//! into Rush's own external-repository cache. Generated BUILD paths are entirely
//! relative, so action keys never embed the source machine's Python prefix.

use anyhow::{bail, Context, Result};
use rush_incremental::{tracked, FileKind};
use std::collections::BTreeMap;
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const PYTHON_CONFIG_QUERY: &[u8] = br#"import os, sys, sysconfig
include = sysconfig.get_path("include")
if not include:
    raise SystemExit("sysconfig returned no include directory")
sys.stdout.buffer.write(os.fsencode(include) + b"\0" + f"{sys.version_info[0]}.{sys.version_info[1]}".encode("ascii"))
"#;

static MATERIALIZATION_ID: AtomicU64 = AtomicU64::new(0);

/// Refresh Rush's configured-Python repository from `RUSH_PY3`, or from the
/// TensorFlow/rules_python hermetic repository already fetched under the Bazel
/// output base. When no interpreter has been configured or fetched, an honest
/// target-less repository is installed so unrelated builds remain usable.
pub(crate) fn materialize_local_config_python(
    workspace_root: &Path,
    repo_path: &Path,
) -> Result<()> {
    let selected = select_python_interpreter(workspace_root, repo_path)?;
    let files = match selected {
        Some(interpreter) => configured_repository_files(workspace_root, &interpreter)?,
        None => unavailable_repository_files(),
    };

    if repository_matches(repo_path, &files)? {
        return Ok(());
    }
    replace_repository(repo_path, &files)
}

fn select_python_interpreter(workspace_root: &Path, repo_path: &Path) -> Result<Option<PathBuf>> {
    if let Some(raw) = tracked::getenv(b"RUSH_PY3") {
        if raw.is_empty() {
            bail!("RUSH_PY3 is set but empty");
        }
        let configured = path_from_bytes(raw.as_ref())?;
        let configured = if configured.is_absolute() {
            configured
        } else {
            workspace_root.join(configured)
        };
        return canonical_executable(&configured)
            .with_context(|| {
                format!(
                    "RUSH_PY3 points to an unusable interpreter {}",
                    configured.display()
                )
            })
            .map(Some);
    }

    let cache_root = repo_path
        .parent()
        .and_then(Path::parent)
        .context("local_config_python repository has no Rush cache-root parent")?;
    let bazel_external = cache_root.join("bazel_output_base").join("external");
    if tracked::metadata_kind(&bazel_external) != Some(FileKind::Dir) {
        return Ok(None);
    }

    let desired_version = hermetic_python_version(&bazel_external)?;
    let mut candidates = Vec::new();
    for name in tracked::read_dir_names(&bazel_external)
        .with_context(|| format!("failed to scan {}", bazel_external.display()))?
    {
        let Some(name_text) = name.to_str() else {
            continue;
        };
        let priority = if is_legacy_python_platform_repo(name_text) {
            0
        } else if name_text.starts_with("rules_python++python+python_") {
            match desired_version.as_deref() {
                Some(version) if rules_python_repo_matches_version(name_text, version) => 1,
                Some(_) => 3,
                None => 2,
            }
        } else {
            continue;
        };
        let candidate = bazel_external.join(&name).join("bin").join("python3");
        if matches!(
            tracked::metadata_kind(&candidate),
            Some(FileKind::File | FileKind::Symlink)
        ) {
            candidates.push((priority, candidate));
        }
    }
    candidates.sort();

    for (_, candidate) in candidates {
        if let Ok(canonical) = canonical_executable(&candidate) {
            return Ok(Some(canonical));
        }
    }
    Ok(None)
}

fn is_legacy_python_platform_repo(name: &str) -> bool {
    name.starts_with("python_") && name != "python_version_repo"
}

fn rules_python_repo_matches_version(repo_name: &str, version: &str) -> bool {
    let underscored = version.replace('.', "_");
    repo_name.contains(&format!("python_{underscored}_"))
        || repo_name.contains(&format!("python_{version}_"))
}

fn hermetic_python_version(bazel_external: &Path) -> Result<Option<String>> {
    let version_file = bazel_external
        .join("python_version_repo")
        .join("py_version.bzl");
    if tracked::metadata_kind(&version_file) != Some(FileKind::File) {
        return Ok(None);
    }
    let content = tracked::read_to_string(&version_file)
        .with_context(|| format!("failed to read {}", version_file.display()))?;
    for line in content.lines() {
        if !line.contains("HERMETIC_PYTHON_VERSION") {
            continue;
        }
        if let Some(value) = quoted_value(line) {
            return Ok(Some(value.to_string()));
        }
    }
    Ok(None)
}

fn quoted_value(line: &str) -> Option<&str> {
    let quote = line.find(|character| character == '\'' || character == '"')?;
    let delimiter = line.as_bytes()[quote];
    let rest = &line[quote + 1..];
    let end = rest.as_bytes().iter().position(|byte| *byte == delimiter)?;
    Some(&rest[..end])
}

fn canonical_executable(path: &Path) -> Result<PathBuf> {
    let canonical = tracked::canonicalize(path)
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    if tracked::metadata_kind(&canonical) != Some(FileKind::File) {
        bail!("{} is not a regular file", canonical.display());
    }
    // Interpreter selection depends on executable metadata as well as its
    // subprocess result; in particular, chmod -x must invalidate a daemon graph.
    if !tracked::current_required().metadata_full(&canonical) {
        bail!("cannot stat {}", canonical.display());
    }
    Ok(canonical)
}

fn configured_repository_files(
    workspace_root: &Path,
    interpreter: &Path,
) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let query = tracked::probe_process(
        interpreter,
        &[b"-c".to_vec(), PYTHON_CONFIG_QUERY.to_vec()],
        &[],
        workspace_root,
    )
    .with_context(|| {
        format!(
            "failed to query Python interpreter {}",
            interpreter.display()
        )
    })?;
    if query.status != 0 {
        bail!(
            "Python interpreter {} failed while querying headers (status {}): {}",
            interpreter.display(),
            query.status,
            String::from_utf8_lossy(&query.stderr).trim()
        );
    }
    let separator = query
        .stdout
        .iter()
        .position(|byte| *byte == 0)
        .with_context(|| {
            format!(
                "Python interpreter {} returned a malformed header query",
                interpreter.display()
            )
        })?;
    let include_raw = &query.stdout[..separator];
    let version_raw = &query.stdout[separator + 1..];
    if include_raw.is_empty() || version_raw.is_empty() || version_raw.contains(&0) {
        bail!(
            "Python interpreter {} returned an incomplete header query",
            interpreter.display()
        );
    }
    let version = std::str::from_utf8(version_raw).with_context(|| {
        format!(
            "Python interpreter {} returned a non-ASCII version",
            interpreter.display()
        )
    })?;
    if !valid_major_minor(version) {
        bail!(
            "Python interpreter {} returned invalid major.minor version {:?}",
            interpreter.display(),
            version
        );
    }

    let include_path = path_from_bytes(include_raw)?;
    let include_path = if include_path.is_absolute() {
        include_path
    } else {
        workspace_root.join(include_path)
    };
    let include_path = tracked::canonicalize(&include_path).with_context(|| {
        format!(
            "cannot resolve Python include dir {}",
            include_path.display()
        )
    })?;
    if tracked::metadata_kind(&include_path) != Some(FileKind::Dir) {
        bail!(
            "Python include path {} is not a directory",
            include_path.display()
        );
    }

    let mut headers = BTreeMap::new();
    collect_headers(&include_path, Path::new(""), &mut headers)?;
    if !headers.contains_key(Path::new("Python.h")) {
        bail!(
            "Python include path {} does not contain Python.h",
            include_path.display()
        );
    }

    let include_name = format!("python{version}");
    let mut files = BTreeMap::new();
    files.insert(
        PathBuf::from("WORKSPACE"),
        b"workspace(name = \"local_config_python\")\n".to_vec(),
    );
    files.insert(
        PathBuf::from("BUILD"),
        configured_build_file(&include_name).into_bytes(),
    );
    for (relative, bytes) in headers {
        files.insert(
            Path::new("include").join(&include_name).join(relative),
            bytes,
        );
    }
    Ok(files)
}

fn valid_major_minor(value: &str) -> bool {
    let Some((major, minor)) = value.split_once('.') else {
        return false;
    };
    !major.is_empty()
        && !minor.is_empty()
        && !minor.contains('.')
        && major.bytes().all(|byte| byte.is_ascii_digit())
        && minor.bytes().all(|byte| byte.is_ascii_digit())
}

fn configured_build_file(include_name: &str) -> String {
    format!(
        "# Generated by Rush from the configured Python interpreter.\n\
         package(default_visibility = [\"//visibility:public\"])\n\n\
         # Compatibility labels retained from Rush's former placeholder.\n\
         # Python rule execution resolves RUSH_PY3 independently; configured\n\
         # NumPy header discovery is outside this repository's Python.h scope.\n\
         filegroup(\n\
         \x20   name = \"python_runtime\",\n\
         \x20   srcs = [],\n\
         )\n\n\
         toolchain(\n\
         \x20   name = \"py_toolchain\",\n\
         \x20   toolchain_type = \"//python:toolchain_type\",\n\
         \x20   toolchain = \":python_runtime\",\n\
         )\n\n\
         cc_library(\n\
         \x20   name = \"python_headers\",\n\
         \x20   hdrs = glob([\"include/**/*.h\"]),\n\
         \x20   includes = [\"include\", \"include/{include_name}\"],\n\
         )\n\n\
         cc_library(\n\
         \x20   name = \"numpy_headers\",\n\
         \x20   hdrs = [],\n\
         \x20   includes = [],\n\
         )\n\n\
         alias(\n\
         \x20   name = \"headers\",\n\
         \x20   actual = \":python_headers\",\n\
         )\n"
    )
}

fn unavailable_repository_files() -> BTreeMap<PathBuf, Vec<u8>> {
    BTreeMap::from([
        (
            PathBuf::from("WORKSPACE"),
            b"workspace(name = \"local_config_python\")\n".to_vec(),
        ),
        (
            PathBuf::from("BUILD"),
            b"# Rush found no configured or hermetic Python interpreter.\n\
              # Set RUSH_PY3 or fetch TensorFlow's hermetic Python repository.\n\
              package(default_visibility = [\"//visibility:public\"])\n"
                .to_vec(),
        ),
    ])
}

fn collect_headers(
    source_root: &Path,
    relative_dir: &Path,
    out: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<()> {
    let directory = source_root.join(relative_dir);
    let mut names = tracked::read_dir_names(&directory)
        .with_context(|| format!("failed to list Python headers in {}", directory.display()))?;
    names.sort();
    for name in names {
        let relative = relative_dir.join(&name);
        let path = source_root.join(&relative);
        match tracked::metadata_kind(&path) {
            Some(FileKind::Dir) => collect_headers(source_root, &relative, out)?,
            Some(FileKind::File) if path.extension() == Some(OsStr::new("h")) => {
                out.insert(
                    relative,
                    tracked::read_file(&path)
                        .with_context(|| {
                            format!("failed to read Python header {}", path.display())
                        })?
                        .to_vec(),
                );
            }
            Some(FileKind::Symlink) if path.extension() == Some(OsStr::new("h")) => {
                let resolved = tracked::canonicalize(&path).with_context(|| {
                    format!("failed to resolve Python header {}", path.display())
                })?;
                if tracked::metadata_kind(&resolved) != Some(FileKind::File) {
                    bail!("Python header symlink {} is not a file", path.display());
                }
                out.insert(
                    relative,
                    tracked::read_file(&resolved)
                        .with_context(|| {
                            format!("failed to read Python header {}", resolved.display())
                        })?
                        .to_vec(),
                );
            }
            Some(FileKind::Symlink) => {
                let resolved = tracked::canonicalize(&path)
                    .with_context(|| format!("failed to resolve {}", path.display()))?;
                if tracked::metadata_kind(&resolved) == Some(FileKind::Dir) {
                    bail!(
                        "Python include tree contains unsupported directory symlink {}",
                        path.display()
                    );
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn repository_matches(repo_path: &Path, expected: &BTreeMap<PathBuf, Vec<u8>>) -> Result<bool> {
    if !repo_path.is_dir() {
        return Ok(false);
    }
    let mut actual_paths = Vec::new();
    for entry in walkdir::WalkDir::new(repo_path).follow_links(false) {
        let entry = entry.with_context(|| format!("failed to inspect {}", repo_path.display()))?;
        if entry.path() == repo_path {
            continue;
        }
        if entry.file_type().is_dir() {
            continue;
        }
        if !entry.file_type().is_file() {
            return Ok(false);
        }
        actual_paths.push(
            entry
                .path()
                .strip_prefix(repo_path)
                .expect("walked path must remain below repository")
                .to_path_buf(),
        );
    }
    actual_paths.sort();
    let expected_paths: Vec<_> = expected.keys().cloned().collect();
    if actual_paths != expected_paths {
        return Ok(false);
    }
    for (relative, bytes) in expected {
        if fs::read(repo_path.join(relative))? != *bytes {
            return Ok(false);
        }
    }
    Ok(true)
}

fn replace_repository(repo_path: &Path, files: &BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
    let parent = repo_path
        .parent()
        .context("local_config_python repository has no parent")?;
    fs::create_dir_all(parent)?;
    let id = MATERIALIZATION_ID.fetch_add(1, Ordering::Relaxed);
    let repo_name = repo_path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("local_config_python");
    let stage = parent.join(format!(
        ".{repo_name}.rush-stage-{}-{id}",
        std::process::id()
    ));
    let backup = parent.join(format!(
        ".{repo_name}.rush-backup-{}-{id}",
        std::process::id()
    ));
    remove_path_if_present(&stage)?;
    remove_path_if_present(&backup)?;
    fs::create_dir(&stage)?;

    let populate_result = (|| -> Result<()> {
        for (relative, bytes) in files {
            let destination = stage.join(relative);
            if let Some(directory) = destination.parent() {
                fs::create_dir_all(directory)?;
            }
            fs::write(&destination, bytes)
                .with_context(|| format!("failed to write {}", destination.display()))?;
        }
        Ok(())
    })();
    if let Err(error) = populate_result {
        let _ = remove_path_if_present(&stage);
        return Err(error);
    }

    let had_previous = fs::symlink_metadata(repo_path).is_ok();
    if had_previous {
        fs::rename(repo_path, &backup).with_context(|| {
            format!(
                "failed to stage existing Python repository {}",
                repo_path.display()
            )
        })?;
    }
    if let Err(error) = fs::rename(&stage, repo_path) {
        if had_previous {
            let _ = fs::rename(&backup, repo_path);
        }
        let _ = remove_path_if_present(&stage);
        return Err(error).with_context(|| {
            format!(
                "failed to install configured Python repository {}",
                repo_path.display()
            )
        });
    }
    if had_previous {
        remove_path_if_present(&backup)?;
    }
    Ok(())
}

fn remove_path_if_present(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(String::from_utf8(bytes.to_vec())?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rush_action::ids::{ArtifactRegistry, ConfigId, TargetId};
    use rush_command_core::CommandSemantics;
    use rush_incremental::{Dependency, RealTrackedIo, TrackedIo};
    use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
    use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use rush_rules::analysis_context::{AnalysisContext, StubIncludeResolver};
    use rush_rules::context::{BuildContext, Toolchain};
    use rush_rules::registry::RuleRegistry;
    use rush_rules::{BuildRule, Value};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn semantics(workspace_root: &Path, rush_py3: Option<&Path>) -> CommandSemantics {
        let environment = rush_py3
            .map(|path| EnvironmentVariable {
                name: Bytes::from_static(b"RUSH_PY3"),
                #[cfg(unix)]
                value: Bytes::copy_from_slice(path.as_os_str().as_bytes()),
                #[cfg(not(unix))]
                value: Bytes::from(path.display().to_string()),
            })
            .into_iter()
            .collect();
        let request = RunRequest {
            auth_cookie: Bytes::new(),
            argv: vec![
                Bytes::from_static(b"rush"),
                Bytes::from_static(b"build"),
                Bytes::from_static(b"//:extension.so"),
            ],
            working_directory: Bytes::from(workspace_root.display().to_string()),
            environment,
            block_for_lock: true,
            client_build_id: "python-config-test".to_string(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            workspace_config: Vec::new(),
        };
        CommandSemantics::from_run_request(&request).expect("test command semantics")
    }

    #[cfg(unix)]
    fn fake_python(path: &Path, include: &Path, version: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!(
                "#!/bin/sh\nprintf '%s\\000%s' '{}' '{}'\n",
                include.display(),
                version
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn materialize_with(
        workspace_root: &Path,
        repo_path: &Path,
        semantics: CommandSemantics,
    ) -> Arc<RealTrackedIo> {
        let io = Arc::new(RealTrackedIo::new(semantics, workspace_root));
        let shared: tracked::SharedTrackedIo = io.clone();
        tracked::scope(shared, || {
            materialize_local_config_python(workspace_root, repo_path).unwrap()
        });
        io
    }

    #[cfg(unix)]
    #[test]
    fn discovers_refreshes_and_honors_rush_py3_without_host_paths() {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let cache_root = workspace_root.join(".cache/rush");
        let bazel_external = cache_root.join("bazel_output_base/external");
        let repo_path = cache_root.join("external/local_config_python");
        fs::create_dir_all(&workspace_root).unwrap();

        let include_a = bazel_external.join("python_x86_64-unknown-linux-gnu/include/python3.10");
        fs::create_dir_all(include_a.join("cpython")).unwrap();
        fs::write(include_a.join("Python.h"), b"/* A */\n").unwrap();
        fs::write(include_a.join("cpython/object.h"), b"/* object A */\n").unwrap();
        let python_a = bazel_external.join("python_x86_64-unknown-linux-gnu/bin/python3");
        fake_python(&python_a, &include_a, "3.10");
        fs::create_dir_all(bazel_external.join("python_version_repo")).unwrap();
        fs::write(
            bazel_external.join("python_version_repo/py_version.bzl"),
            "HERMETIC_PYTHON_VERSION = \"3.10\"\n",
        )
        .unwrap();

        let auto_semantics = semantics(&workspace_root, None);
        let io = materialize_with(&workspace_root, &repo_path, auto_semantics.clone());
        let python_h = repo_path.join("include/python3.10/Python.h");
        assert_eq!(fs::read(&python_h).unwrap(), b"/* A */\n");
        assert!(repo_path
            .join("include/python3.10/cpython/object.h")
            .is_file());
        let build = fs::read_to_string(repo_path.join("BUILD")).unwrap();
        assert!(build.contains("cc_library("));
        assert!(build.contains("includes = [\"include\", \"include/python3.10\"]"));
        assert!(!build.contains(&include_a.display().to_string()));
        assert!(!build.contains(&python_a.display().to_string()));

        let manifest = io.manifest();
        assert!(manifest.deps().iter().any(|dep| matches!(
            dep,
            Dependency::Env { name, value: None } if name == b"RUSH_PY3"
        )));
        assert!(manifest.deps().iter().any(
            |dep| matches!(dep, Dependency::ProcessProbe { program, .. } if program == &python_a)
        ));
        assert!(manifest.deps().iter().any(|dep| matches!(
            dep,
            Dependency::FileContent { path, .. } if path == &include_a.join("Python.h")
        )));
        assert!(manifest.deps().iter().any(|dep| matches!(
            dep,
            Dependency::DirListing { path, .. } if path == &include_a
        )));

        let inode_before = fs::metadata(&python_h).unwrap().ino();
        materialize_with(&workspace_root, &repo_path, auto_semantics);
        assert_eq!(fs::metadata(&python_h).unwrap().ino(), inode_before);

        fs::write(include_a.join("Python.h"), b"/* A refreshed */\n").unwrap();
        materialize_with(
            &workspace_root,
            &repo_path,
            semantics(&workspace_root, None),
        );
        assert_eq!(fs::read(&python_h).unwrap(), b"/* A refreshed */\n");

        let include_b = temp.path().join("configured/include");
        fs::create_dir_all(&include_b).unwrap();
        fs::write(include_b.join("Python.h"), b"/* B */\n").unwrap();
        fs::write(include_b.join("only_b.h"), b"/* only B */\n").unwrap();
        let python_b = temp.path().join("configured/bin/python3");
        fake_python(&python_b, &include_b, "3.12");
        let override_io = materialize_with(
            &workspace_root,
            &repo_path,
            semantics(&workspace_root, Some(&python_b)),
        );
        assert!(!repo_path.join("include/python3.10/Python.h").exists());
        assert_eq!(
            fs::read(repo_path.join("include/python3.12/Python.h")).unwrap(),
            b"/* B */\n"
        );
        assert!(repo_path.join("include/python3.12/only_b.h").is_file());
        assert!(override_io.manifest().deps().iter().any(|dep| matches!(
            dep,
            Dependency::Env { name, value: Some(value) }
                if name == b"RUSH_PY3" && value == python_b.as_os_str().as_bytes()
        )));
    }

    #[cfg(unix)]
    #[test]
    fn unusable_override_fails_and_unconfigured_state_removes_stale_target() {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let repo_path = workspace_root.join(".cache/rush/external/local_config_python");
        fs::create_dir_all(repo_path.join("include/python3.9")).unwrap();
        let stale_build = b"cc_library(name = \"python_headers\", hdrs = [])\n";
        fs::write(repo_path.join("BUILD"), stale_build).unwrap();
        fs::write(repo_path.join("include/python3.9/Python.h"), b"stale\n").unwrap();

        let missing = temp.path().join("missing/python3");
        let io = Arc::new(RealTrackedIo::new(
            semantics(&workspace_root, Some(&missing)),
            &workspace_root,
        ));
        let shared: tracked::SharedTrackedIo = io;
        let error = tracked::scope(shared, || {
            materialize_local_config_python(&workspace_root, &repo_path)
        })
        .expect_err("an explicit unusable RUSH_PY3 must not fall back");
        assert!(error.to_string().contains("RUSH_PY3"));
        assert_eq!(fs::read(repo_path.join("BUILD")).unwrap(), stale_build);

        materialize_with(
            &workspace_root,
            &repo_path,
            semantics(&workspace_root, None),
        );
        let build = fs::read_to_string(repo_path.join("BUILD")).unwrap();
        assert!(build.contains("found no configured or hermetic Python"));
        assert!(!build.contains("python_headers"));
        assert!(!repo_path.join("include/python3.9/Python.h").exists());
        let parsed = tracked::test_scope(&repo_path, || {
            rush_parse::parse_build_file(&repo_path.join("BUILD")).unwrap()
        });
        assert!(parsed
            .rules
            .iter()
            .all(|rule| rule.name != "python_headers"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_entry_materializes_and_registers_python_before_best_effort_eval() {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let bazel_external = workspace_root.join(".cache/rush/bazel_output_base/external");
        let include = bazel_external.join("python_x86_64-unknown-linux-gnu/include/python3.10");
        fs::create_dir_all(&include).unwrap();
        fs::write(include.join("Python.h"), b"/* configured early */\n").unwrap();
        let interpreter = bazel_external.join("python_x86_64-unknown-linux-gnu/bin/python3");
        fake_python(&interpreter, &include, "3.10");

        // An evaluation failure is deliberately best-effort in load_workspace.
        // Python configuration and registration must happen before that early
        // return, while a directory with no WORKSPACE remains a true no-op.
        fs::write(
            workspace_root.join("WORKSPACE"),
            "this is deliberately invalid Starlark\n",
        )
        .unwrap();
        let orchestrator = crate::BuildOrchestrator::with_workspace(workspace_root.clone());
        let io = Arc::new(RealTrackedIo::new(
            semantics(&workspace_root, None),
            &workspace_root,
        ));
        *orchestrator
            .tracked_io
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(io);
        orchestrator.load_workspace(false).await.unwrap();

        let repo_path = workspace_root.join(".cache/rush/external/local_config_python");
        assert_eq!(
            fs::read(repo_path.join("include/python3.10/Python.h")).unwrap(),
            b"/* configured early */\n"
        );
        assert_eq!(
            orchestrator
                .external_repo_paths
                .lock()
                .await
                .get("local_config_python"),
            Some(&repo_path)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn generated_cc_info_reaches_pybind_extension_scheduler_and_legacy() {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let output_dir = temp.path().join("out");
        let cache_root = workspace_root.join(".cache/rush");
        let repo_path = cache_root.join("external/local_config_python");
        let include = temp.path().join("hermetic/include");
        fs::create_dir_all(&workspace_root).unwrap();
        fs::create_dir_all(include.join("cpython")).unwrap();
        fs::write(include.join("Python.h"), b"#pragma once\n").unwrap();
        fs::write(include.join("cpython/object.h"), b"#pragma once\n").unwrap();
        let interpreter = temp.path().join("hermetic/bin/python3");
        fake_python(&interpreter, &include, "3.10");
        materialize_with(
            &workspace_root,
            &repo_path,
            semantics(&workspace_root, Some(&interpreter)),
        );

        let parsed = tracked::test_scope(&repo_path, || {
            rush_parse::parse_build_file(&repo_path.join("BUILD")).unwrap()
        });
        let python_rule = parsed
            .rules
            .iter()
            .find(|rule| rule.name == "python_headers")
            .expect("generated python_headers target");
        for required_label in [
            "python_headers",
            "headers",
            "numpy_headers",
            "python_runtime",
            "py_toolchain",
        ] {
            assert!(
                parsed.rules.iter().any(|rule| rule.name == required_label),
                "generated repository dropped compatibility label {required_label}"
            );
        }
        assert_eq!(python_rule.rule_type, "cc_library");
        let python_attrs: HashMap<_, _> = python_rule
            .attributes
            .iter()
            .map(|(name, value)| {
                Ok((
                    name.clone(),
                    crate::rule_execution::parse_value_to_rush_value(value)?,
                ))
            })
            .collect::<Result<_>>()
            .unwrap();
        let python_instance = rush_rules::rules_cc::CcLibraryRule
            .parse_attributes(&python_attrs)
            .unwrap();

        let resolver = StubIncludeResolver::new(repo_path.clone());
        let toolchain = Toolchain::default();
        let mut registry = ArtifactRegistry::default();
        let python_providers = {
            let mut context = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                "",
                Some("local_config_python"),
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &repo_path,
                &[],
            );
            python_instance.analyze(&mut context, &[])
        };
        let python_cc = python_providers.get_cc_info().expect("typed Python CcInfo");
        let copied_python_h = repo_path.join("include/python3.10/Python.h");
        assert!(python_cc
            .compilation
            .headers
            .iter()
            .any(|aid| registry.artifact_path(aid) == copied_python_h));
        assert!(python_cc.compilation.include_dirs.iter().any(|path_id| {
            registry.paths().path(path_id) == repo_path.join("include/python3.10")
        }));

        // TensorFlow's real chain is:
        //   @pybind11//:pybind11
        //     -> @org_tensorflow//third_party/python_runtime:headers (alias)
        //     -> @local_config_python//:python_headers.
        // Exercise the same alias rule and the orchestrator's provider-forwarding
        // operation before analyzing the synthetic pybind library.
        let alias_attrs = HashMap::from([
            ("name".to_string(), Value::String("headers".to_string())),
            (
                "actual".to_string(),
                Value::String("@local_config_python//:python_headers".to_string()),
            ),
        ]);
        let alias_instance = tensorflow_rules::AliasRule
            .parse_attributes(&alias_attrs)
            .unwrap();
        assert_eq!(
            alias_instance.dependencies(),
            vec!["@local_config_python//:python_headers"]
        );
        let mut python_runtime_headers = alias_instance.providers();
        python_runtime_headers.extend_missing_from(&python_providers);
        assert!(Arc::ptr_eq(
            python_runtime_headers.get_cc_info().unwrap(),
            python_providers.get_cc_info().unwrap()
        ));

        let pybind_header = workspace_root.join("third_party/pybind/include/pybind11/pybind11.h");
        fs::create_dir_all(pybind_header.parent().unwrap()).unwrap();
        fs::write(&pybind_header, b"#pragma once\n").unwrap();
        let pybind_attrs = HashMap::from([
            ("name".to_string(), Value::String("pybind11".to_string())),
            (
                "hdrs".to_string(),
                Value::List(vec![Value::String(
                    "third_party/pybind/include/pybind11/pybind11.h".to_string(),
                )]),
            ),
            (
                "includes".to_string(),
                Value::List(vec![Value::String(
                    "third_party/pybind/include".to_string(),
                )]),
            ),
            (
                "deps".to_string(),
                Value::List(vec![Value::String(
                    "@org_tensorflow//third_party/python_runtime:headers".to_string(),
                )]),
            ),
        ]);
        let pybind_instance = rush_rules::rules_cc::CcLibraryRule
            .parse_attributes(&pybind_attrs)
            .unwrap();
        let pybind_providers = {
            let resolver = StubIncludeResolver::new(workspace_root.clone());
            let mut context = AnalysisContext::new(
                TargetId(2),
                ConfigId::DEFAULT,
                "",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            );
            pybind_instance.analyze(&mut context, &[Arc::new(python_runtime_headers)])
        };

        let source = workspace_root.join("extension.cc");
        fs::write(
            &source,
            "#include <Python.h>\n#include <pybind11/pybind11.h>\nint f() { return 0; }\n",
        )
        .unwrap();
        let tf_attrs = HashMap::from([
            (
                "name".to_string(),
                Value::String("extension.so".to_string()),
            ),
            (
                "srcs".to_string(),
                Value::List(vec![Value::String("extension.cc".to_string())]),
            ),
            (
                "deps".to_string(),
                Value::List(vec![Value::String("//third_party:pybind11".to_string())]),
            ),
            (
                "linkopts".to_string(),
                Value::List(vec![Value::String("-shared".to_string())]),
            ),
        ]);
        let mut rules = RuleRegistry::new();
        tensorflow_rules::register_tensorflow_rules(&mut rules);
        let tf_instance = rules
            .get("tf_cc_binary")
            .unwrap()
            .parse_attributes(&tf_attrs)
            .unwrap();
        let labeled = vec![(
            "//third_party:pybind11".to_string(),
            Arc::new(pybind_providers.clone_forwarded()),
        )];
        let target = TargetId(3);
        {
            let resolver = StubIncludeResolver::new(workspace_root.clone());
            let mut context = AnalysisContext::new(
                target,
                ConfigId::DEFAULT,
                "",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            )
            .with_labeled_dep_providers(&labeled);
            tf_instance.analyze(&mut context, &[labeled[0].1.clone()]);
        }

        let compile_id = registry
            .actions_of(target)
            .iter()
            .copied()
            .find(|id| {
                registry
                    .strings()
                    .str(registry.declared_action(*id).mnemonic)
                    == "CppCompile"
            })
            .expect("extension compile action");
        let action = registry.declared_action(compile_id);
        let input_paths: Vec<_> = action
            .inputs
            .iter()
            .map(|aid| registry.artifact_path(*aid).to_path_buf())
            .collect();
        for required in [&source, &pybind_header, &copied_python_h] {
            assert!(
                input_paths.contains(required),
                "missing input {}",
                required.display()
            );
        }
        let scheduler = rush_action::command_spec::NormalizedCommandSpec::from_registry_action(
            &registry, compile_id,
        )
        .unwrap();
        for required in [
            repo_path.join("include"),
            repo_path.join("include/python3.10"),
            workspace_root.join("third_party/pybind/include"),
        ] {
            assert!(
                scheduler
                    .argv
                    .contains(&format!("-I{}", required.display())),
                "missing include root {} in {:?}",
                required.display(),
                scheduler.argv
            );
        }

        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let build_context = BuildContext::new(workspace_root, output_dir)
            .with_package(PathBuf::new())
            .with_toolchain(toolchain)
            .with_dependencies(
                HashMap::new(),
                vec![("//third_party:pybind11".to_string(), pybind_providers)],
            )
            .with_artifact_registry(registry);
        let legacy = tf_instance
            .build_legacy_command_specs(&build_context)
            .await
            .into_iter()
            .map(|(_, spec)| spec)
            .find(|spec| spec.mnemonic == "CppCompile")
            .expect("legacy extension compile action");
        assert_eq!(legacy, scheduler);
    }
}
