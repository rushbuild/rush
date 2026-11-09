// SPDX-License-Identifier: MIT
//! Adapt declared native file actions to portable REAPI requests.

use crate::reapi::{ActionSpec, Outcome, ReapiClient};
use anyhow::{ensure, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A worker/toolchain-pinned executor for native build actions.
pub struct BuildExecutor {
    root: PathBuf,
    endpoint: String,
    instance: String,
    platform: BTreeMap<String, String>,
    client: tokio::sync::OnceCell<ReapiClient>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_toolchain_and_relocate_only_known_path_arguments() {
        let root = tempfile::tempdir().unwrap();
        assert!(BuildExecutor::new(
            root.path().into(),
            "http://localhost".into(),
            String::new(),
            BTreeMap::new()
        )
        .is_err());
        assert!(BuildExecutor::new(
            root.path().into(),
            "http://localhost".into(),
            String::new(),
            BTreeMap::from([("toolchain".into(), String::new())])
        )
        .is_err());
        let executor = BuildExecutor::new(
            root.path().into(),
            "http://localhost".into(),
            String::new(),
            BTreeMap::from([("toolchain".into(), "pinned".into())]),
        )
        .unwrap();
        let root = root.path().display().to_string();
        assert_eq!(
            executor.argument(&format!("-I{root}/include")).unwrap(),
            "-Iinclude"
        );
        assert_eq!(
            executor.argument(&format!("{root}/src/a.cc")).unwrap(),
            "src/a.cc"
        );
        assert!(executor.argument(&format!("echo {root}/input")).is_err());
        assert!(executor.path(Path::new("../outside")).is_err());
    }
}

impl BuildExecutor {
    /// Check protocol capabilities instead of probing a legacy HTTP health URL.
    pub async fn health_check(&self) -> Result<bool> {
        ReapiClient::connect(self.endpoint.clone(), self.instance.clone()).await?;
        Ok(true)
    }

    /// Bind this executor to one workspace and explicit worker properties.
    pub fn new(
        root: PathBuf,
        endpoint: String,
        instance: String,
        platform: BTreeMap<String, String>,
    ) -> Result<Self> {
        ensure!(
            platform
                .get("toolchain")
                .is_some_and(|value| !value.is_empty()),
            "remote builds require a pinned toolchain platform property"
        );
        Ok(Self {
            root: root.canonicalize()?,
            endpoint,
            instance,
            platform,
            client: tokio::sync::OnceCell::new(),
        })
    }

    fn path(&self, path: &Path) -> Result<PathBuf> {
        let path = if path.is_absolute() {
            path.strip_prefix(&self.root)
                .context("remote input/output lies outside the workspace")?
        } else {
            path
        };
        ensure!(
            !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "non-portable artifact path"
        );
        Ok(path.to_path_buf())
    }

    fn argument(&self, arg: &str) -> Result<String> {
        let root = self.root.to_str().context("workspace path must be UTF-8")?;
        for prefix in ["", "-I", "-L", "-iquote", "-isystem", "--sysroot="] {
            let Some(value) = arg.strip_prefix(prefix).and_then(|s| s.strip_prefix(root)) else {
                continue;
            };
            if value.is_empty() {
                return Ok(format!("{prefix}."));
            }
            if let Some(value) = value.strip_prefix('/') {
                return Ok(format!("{prefix}{value}"));
            }
        }
        ensure!(!arg.contains(root), "workspace path embedded in an unsupported argument; use a portable remote action manifest");
        Ok(arg.to_string())
    }

    /// Execute with declared inputs and outputs. No local fallback is allowed.
    pub async fn execute(
        &self,
        executable: &Path,
        args: &[String],
        environment: impl IntoIterator<Item = (String, String)>,
        cwd: Option<&Path>,
        inputs: &[PathBuf],
        outputs: &[PathBuf],
        timeout: Duration,
    ) -> Result<Outcome> {
        if let Some(cwd) = cwd {
            let cwd = if cwd.is_absolute() {
                cwd.to_path_buf()
            } else {
                self.root.join(cwd)
            };
            ensure!(
                cwd.canonicalize()? == self.root,
                "remote native actions currently require workspace-root working directories"
            );
        }
        if executable.is_absolute() && !executable.starts_with(&self.root) {
            ensure!(executable.starts_with("/usr/bin") || executable.starts_with("/bin"),
                "remote tool must be declared in the workspace or supplied by the pinned worker toolchain");
        }
        let mut arguments =
            vec![self.argument(executable.to_str().context("non-UTF-8 executable")?)?];
        for arg in args {
            arguments.push(self.argument(arg)?);
        }
        let mut environment: BTreeMap<String, String> = environment.into_iter().collect();
        ensure!(
            environment
                .values()
                .all(|v| !v.contains(self.root.to_str().unwrap())),
            "workspace paths in native action environment require an explicit portable manifest"
        );
        environment
            .entry("PATH".to_string())
            .or_insert_with(|| "/usr/bin:/bin".to_string());
        let mut portable_inputs = Vec::new();
        for input in inputs {
            portable_inputs.push(self.path(input)?);
            let local = if input.is_absolute() {
                input.clone()
            } else {
                self.root.join(input)
            };
            // Thin archives refer to separate object files; transmit those too.
            if local.is_file() {
                use std::io::Read;
                let mut magic = [0; 8];
                let n = std::fs::File::open(&local)?.read(&mut magic)?;
                if n == 8 && &magic == b"!<thin>\n" {
                    for member in rush_action::thin_archive_member_paths(&local)? {
                        portable_inputs.push(self.path(&member.canonicalize()?)?);
                    }
                }
            }
        }
        portable_inputs.sort();
        portable_inputs.dedup();
        let spec = ActionSpec {
            arguments,
            environment,
            inputs: portable_inputs,
            outputs: outputs
                .iter()
                .map(|p| self.path(p))
                .collect::<Result<_>>()?,
            platform: self.platform.clone(),
            timeout_seconds: u32::try_from(timeout.as_secs())?,
            no_cache: false,
        };
        let client = self
            .client
            .get_or_try_init(|| ReapiClient::connect(self.endpoint.clone(), self.instance.clone()))
            .await?;
        client
            .execute(&self.root, &spec, timeout + Duration::from_secs(120))
            .await
    }
}
