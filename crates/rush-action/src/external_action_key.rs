//! Canonical cache keys for actions produced by an external analyzer.
//!
//! An external graph supplies logical action identifiers for dependency
//! edges and diagnostics. Those identifiers are never cache authority. Rush
//! rebuilds an [`ActionId`] from the exact commands, explicit environment,
//! current input values, declared outputs, and execution properties.

use crate::ActionId;
use std::collections::{BTreeMap, BTreeSet};

/// Version of the byte encoding hashed by [`ExternalActionKey::compute_id`].
pub const EXTERNAL_ACTION_KEY_VERSION: u32 = 9;

const KEY_DOMAIN: &[u8] = b"rush.external-action-key.v9\n";

/// Linux signal state that an external action must inherit across exec.
///
/// Caught dispositions and pending signals are deliberately absent: a
/// certified external action may contain neither at its invocation boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalSignalState {
    /// Exact blocked-signal mask.
    pub blocked: u64,
    /// Exact set of signals whose disposition is ignored.
    pub ignored: u64,
}

impl ExternalSignalState {
    /// Reject state that Linux cannot install.
    pub fn validate(self) -> anyhow::Result<()> {
        // Signal numbers are part of the stable Linux ABI. Avoid making this
        // key-only crate depend on libc merely for SIGKILL and SIGSTOP.
        const SIGKILL_MASK: u64 = 1_u64 << (9 - 1);
        const SIGSTOP_MASK: u64 = 1_u64 << (19 - 1);
        if (self.blocked | self.ignored) & (SIGKILL_MASK | SIGSTOP_MASK) != 0 {
            anyhow::bail!("external signal state contains an unmaskable signal");
        }
        Ok(())
    }
}

/// Linux credential state that an external action must inherit across exec.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCredentialState {
    /// Real user identity.
    pub real_user: u32,
    /// Effective user identity.
    pub effective_user: u32,
    /// Saved-set user identity.
    pub saved_user: u32,
    /// Filesystem user identity used for permission checks.
    pub filesystem_user: u32,
    /// Real group identity.
    pub real_group: u32,
    /// Effective group identity.
    pub effective_group: u32,
    /// Saved-set group identity.
    pub saved_group: u32,
    /// Filesystem group identity used for permission checks.
    pub filesystem_group: u32,
    /// Canonical nondecreasing supplementary group identities.
    pub supplementary_groups: Vec<u32>,
    /// Inheritable Linux capability set.
    pub inheritable_capabilities: u64,
    /// Permitted Linux capability set.
    pub permitted_capabilities: u64,
    /// Effective Linux capability set.
    pub effective_capabilities: u64,
    /// Bounding Linux capability set.
    pub bounding_capabilities: u64,
    /// Ambient Linux capability set.
    pub ambient_capabilities: u64,
    /// Whether the Linux no-new-privileges bit is set.
    pub no_new_privileges: bool,
}

impl ExternalCredentialState {
    /// Reject identities or group encodings that Linux cannot inherit.
    pub fn validate(&self) -> anyhow::Result<()> {
        if [
            self.real_user,
            self.effective_user,
            self.saved_user,
            self.filesystem_user,
            self.real_group,
            self.effective_group,
            self.saved_group,
            self.filesystem_group,
        ]
        .contains(&u32::MAX)
            || self.supplementary_groups.len() > 65_536
            || self
                .supplementary_groups
                .iter()
                .any(|identity| *identity == u32::MAX)
            || self
                .supplementary_groups
                .windows(2)
                .any(|pair| pair[0] > pair[1])
        {
            anyhow::bail!("external credential state contains an invalid identity");
        }
        Ok(())
    }
}

/// A stable path name independent of an absolute machine-local root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ExternalPathKey {
    /// Logical root name, such as `src`, `obj`, or `toolchain`.
    pub root: String,
    /// Lossless platform path bytes relative to `root`.
    pub path: Vec<u8>,
}

/// One exact command in a possibly multi-command action.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExternalCommandKey {
    /// Executable's stable rooted path.
    pub executable: ExternalPathKey,
    /// Digest of the executable bytes observed for this action.
    pub executable_digest: [u8; 32],
    /// Whether the executable had any execute bit set.
    pub executable_bit: bool,
    /// Exact argv[0] presented to the executable.
    pub argv0: ExternalArgumentKey,
    /// Arguments after argv[0], in exact order and canonical path form.
    pub argv: Vec<ExternalArgumentKey>,
    /// Complete explicit environment for this command.
    pub environment: BTreeMap<Vec<u8>, ExternalArgumentKey>,
    /// Stable rooted working directory for this command.
    pub working_directory: ExternalPathKey,
}

/// One canonical command argument.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ExternalArgumentKey {
    /// Lossless non-path argument bytes.
    Literal(Vec<u8>),
    /// A path argument rooted independently from its machine location.
    Path(ExternalPathKey),
    /// One argv element made from adjacent literal and rooted-path fragments.
    Joined(Vec<ExternalArgumentPart>),
}

/// One fragment of a joined external command argument.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ExternalArgumentPart {
    /// Literal bytes copied without separators.
    Literal(Vec<u8>),
    /// Rooted path rendered at execution time.
    Path(ExternalPathKey),
}

/// Current value of one declared external-action input.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum ExternalInputValue {
    /// Regular file bytes and whether the executable bit was set.
    File {
        /// Content digest.
        digest: [u8; 32],
        /// Executable-bit identity.
        executable: bool,
    },
    /// Symbolic-link target bytes. The target is not normalized.
    Symlink {
        /// Lossless target bytes returned by `readlink`.
        target: Vec<u8>,
    },
    /// Canonical digest of a tree's shape and entry values.
    Tree {
        /// Digest of the recursively encoded tree.
        digest: [u8; 32],
    },
    /// An observed missing path.
    Missing,
    /// Canonical digest of sorted directory-member names.
    DirectoryListing {
        /// Digest of sorted, length-prefixed member names.
        digest: [u8; 32],
    },
    /// An existing directory and its permission mode.
    Directory {
        /// Permission and special bits that affect directory access.
        mode: u32,
    },
    /// A named semantic value, such as `CONFIG_PREEMPT=y`.
    Virtual {
        /// Stable virtual-input name.
        name: Vec<u8>,
        /// Exact semantic value bytes.
        value: Vec<u8>,
    },
}

/// A stable path paired with the value Rush observed at key calculation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ExternalInputKey {
    /// Stable rooted path or namespace location.
    pub path: ExternalPathKey,
    /// Current value of the input.
    pub value: ExternalInputValue,
}

/// Shape and destination of one declared output.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum ExternalOutputKind {
    /// A regular file.
    File,
    /// A symbolic link.
    Symlink,
    /// A directory tree.
    Tree,
}

/// One declared output included in cache identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ExternalOutputKey {
    /// Stable rooted output path.
    pub path: ExternalPathKey,
    /// Required filesystem shape.
    pub kind: ExternalOutputKind,
    /// Exact permission and special bits required after replay.
    pub mode: u32,
}

/// Complete canonical key material for one external action.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExternalActionKey {
    /// Schema that defined the action record.
    pub schema: String,
    /// Exact command sequence. Sequence order is semantic.
    pub commands: Vec<ExternalCommandKey>,
    /// Stable rooted working directory.
    pub working_directory: ExternalPathKey,
    /// Exact signal state installed before the first command exec.
    pub signal_state: ExternalSignalState,
    /// Exact Linux file-creation mask installed before the first command exec.
    pub file_creation_mask: u32,
    /// Exact Linux credential state required at the first command exec.
    pub credentials: ExternalCredentialState,
    /// Current values of normal, tool, negative, directory, and virtual inputs.
    pub inputs: Vec<ExternalInputKey>,
    /// Declared output names and shapes.
    pub outputs: Vec<ExternalOutputKey>,
    /// Platform and executor properties that can affect output bytes.
    pub execution_properties: BTreeMap<String, String>,
}

impl ExternalActionKey {
    /// Recompute the content-addressed identifier for an external action.
    ///
    /// Input and output declarations are sets and are sorted before encoding.
    /// Exact command order and argv order are preserved. Duplicate input or
    /// output paths fail closed instead of depending on exporter ordering.
    pub fn compute_id(&self) -> anyhow::Result<ActionId> {
        if self.commands.is_empty() {
            anyhow::bail!("external action key requires at least one command");
        }
        self.signal_state.validate()?;
        if self.file_creation_mask & !0o777 != 0 {
            anyhow::bail!("external action key has an invalid file-creation mask");
        }
        self.credentials.validate()?;

        let mut inputs = self.inputs.clone();
        inputs.sort();
        ensure_unique_paths(inputs.iter().map(|input| &input.path), "input")?;

        let mut outputs = self.outputs.clone();
        outputs.sort();
        ensure_unique_paths(outputs.iter().map(|output| &output.path), "output")?;

        let mut hasher = blake3::Hasher::new();
        hasher.update(KEY_DOMAIN);
        put_u32(&mut hasher, EXTERNAL_ACTION_KEY_VERSION);
        put_bytes(&mut hasher, self.schema.as_bytes());
        put_path(&mut hasher, &self.working_directory);

        put_u64(&mut hasher, self.commands.len() as u64);
        for command in &self.commands {
            put_path(&mut hasher, &command.executable);
            put_bytes(&mut hasher, &command.executable_digest);
            hasher.update(&[u8::from(command.executable_bit)]);
            put_argument(&mut hasher, &command.argv0);
            put_u64(&mut hasher, command.argv.len() as u64);
            for arg in &command.argv {
                put_argument(&mut hasher, arg);
            }
            put_u64(&mut hasher, command.environment.len() as u64);
            for (name, value) in &command.environment {
                put_bytes(&mut hasher, name);
                put_argument(&mut hasher, value);
            }
            put_path(&mut hasher, &command.working_directory);
        }

        hasher.update(&self.signal_state.blocked.to_le_bytes());
        hasher.update(&self.signal_state.ignored.to_le_bytes());
        hasher.update(&self.file_creation_mask.to_le_bytes());
        for identity in [
            self.credentials.real_user,
            self.credentials.effective_user,
            self.credentials.saved_user,
            self.credentials.filesystem_user,
            self.credentials.real_group,
            self.credentials.effective_group,
            self.credentials.saved_group,
            self.credentials.filesystem_group,
        ] {
            put_u32(&mut hasher, identity);
        }
        put_u64(
            &mut hasher,
            self.credentials.supplementary_groups.len() as u64,
        );
        for identity in &self.credentials.supplementary_groups {
            put_u32(&mut hasher, *identity);
        }
        for capabilities in [
            self.credentials.inheritable_capabilities,
            self.credentials.permitted_capabilities,
            self.credentials.effective_capabilities,
            self.credentials.bounding_capabilities,
            self.credentials.ambient_capabilities,
        ] {
            put_u64(&mut hasher, capabilities);
        }
        hasher.update(&[u8::from(self.credentials.no_new_privileges)]);

        put_u64(&mut hasher, inputs.len() as u64);
        for input in &inputs {
            put_path(&mut hasher, &input.path);
            put_input_value(&mut hasher, &input.value);
        }

        put_u64(&mut hasher, outputs.len() as u64);
        for output in &outputs {
            put_path(&mut hasher, &output.path);
            let tag = match output.kind {
                ExternalOutputKind::File => 1,
                ExternalOutputKind::Symlink => 2,
                ExternalOutputKind::Tree => 3,
            };
            hasher.update(&[tag]);
            hasher.update(&output.mode.to_le_bytes());
        }

        put_u64(&mut hasher, self.execution_properties.len() as u64);
        for (name, value) in &self.execution_properties {
            put_bytes(&mut hasher, name.as_bytes());
            put_bytes(&mut hasher, value.as_bytes());
        }

        Ok(ActionId::new(hasher.finalize()))
    }
}

fn ensure_unique_paths<'a>(
    paths: impl Iterator<Item = &'a ExternalPathKey>,
    kind: &str,
) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for path in paths {
        if !seen.insert(path) {
            anyhow::bail!(
                "external action key has duplicate {kind} path {}:{}",
                path.root,
                String::from_utf8_lossy(&path.path)
            );
        }
    }
    Ok(())
}

fn put_input_value(hasher: &mut blake3::Hasher, value: &ExternalInputValue) {
    match value {
        ExternalInputValue::File { digest, executable } => {
            hasher.update(&[1, u8::from(*executable)]);
            put_bytes(hasher, digest);
        }
        ExternalInputValue::Symlink { target } => {
            hasher.update(&[2]);
            put_bytes(hasher, target);
        }
        ExternalInputValue::Tree { digest } => {
            hasher.update(&[3]);
            put_bytes(hasher, digest);
        }
        ExternalInputValue::Missing => {
            hasher.update(&[4]);
        }
        ExternalInputValue::DirectoryListing { digest } => {
            hasher.update(&[5]);
            put_bytes(hasher, digest);
        }
        ExternalInputValue::Virtual { name, value } => {
            hasher.update(&[6]);
            put_bytes(hasher, name);
            put_bytes(hasher, value);
        }
        ExternalInputValue::Directory { mode } => {
            hasher.update(&[7]);
            hasher.update(&mode.to_le_bytes());
        }
    }
}

fn put_argument(hasher: &mut blake3::Hasher, argument: &ExternalArgumentKey) {
    match argument {
        ExternalArgumentKey::Literal(value) => {
            hasher.update(&[1]);
            put_bytes(hasher, value);
        }
        ExternalArgumentKey::Path(path) => {
            hasher.update(&[2]);
            put_path(hasher, path);
        }
        ExternalArgumentKey::Joined(parts) => {
            hasher.update(&[3]);
            put_u64(hasher, parts.len() as u64);
            for part in parts {
                match part {
                    ExternalArgumentPart::Literal(value) => {
                        hasher.update(&[1]);
                        put_bytes(hasher, value);
                    }
                    ExternalArgumentPart::Path(path) => {
                        hasher.update(&[2]);
                        put_path(hasher, path);
                    }
                }
            }
        }
    }
}

fn put_path(hasher: &mut blake3::Hasher, path: &ExternalPathKey) {
    put_bytes(hasher, path.root.as_bytes());
    put_bytes(hasher, &path.path);
}

fn put_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    put_u64(hasher, bytes.len() as u64);
    hasher.update(bytes);
}

fn put_u32(hasher: &mut blake3::Hasher, value: u32) {
    hasher.update(&value.to_le_bytes());
}

fn put_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(root: &str, value: &str) -> ExternalPathKey {
        ExternalPathKey {
            root: root.to_string(),
            path: value.as_bytes().to_vec(),
        }
    }

    fn key() -> ExternalActionKey {
        ExternalActionKey {
            schema: "org.rushbuild.external-action.v1".to_string(),
            commands: vec![ExternalCommandKey {
                executable: path("toolchain", "bin/cc"),
                executable_digest: [3; 32],
                executable_bit: true,
                argv0: ExternalArgumentKey::Literal(b"cc".to_vec()),
                argv: vec![
                    ExternalArgumentKey::Literal(b"-c".to_vec()),
                    ExternalArgumentKey::Path(path("src", "a.c")),
                ],
                environment: BTreeMap::from([(
                    b"LANG".to_vec(),
                    ExternalArgumentKey::Literal(b"C".to_vec()),
                )]),
                working_directory: path("obj", "."),
            }],
            working_directory: path("obj", "."),
            signal_state: ExternalSignalState::default(),
            file_creation_mask: 0o022,
            credentials: ExternalCredentialState::default(),
            inputs: vec![
                ExternalInputKey {
                    path: path("src", "a.c"),
                    value: ExternalInputValue::File {
                        digest: [1; 32],
                        executable: false,
                    },
                },
                ExternalInputKey {
                    path: path("config", "CONFIG_PREEMPT"),
                    value: ExternalInputValue::Virtual {
                        name: b"CONFIG_PREEMPT".to_vec(),
                        value: b"y".to_vec(),
                    },
                },
            ],
            outputs: vec![ExternalOutputKey {
                path: path("obj", "a.o"),
                kind: ExternalOutputKind::File,
                mode: 0o644,
            }],
            execution_properties: BTreeMap::from([(
                "target_arch".to_string(),
                "x86_64".to_string(),
            )]),
        }
    }

    #[test]
    fn canonical_key_is_deterministic_and_set_order_independent() {
        let first = key();
        let mut second = first.clone();
        second.inputs.reverse();
        assert_eq!(first.compute_id().unwrap(), second.compute_id().unwrap());
    }

    #[test]
    fn every_semantic_class_changes_the_key() {
        let baseline = key().compute_id().unwrap();

        let mut command = key();
        command.commands[0]
            .argv
            .push(ExternalArgumentKey::Literal(b"-g".to_vec()));
        assert_ne!(baseline, command.compute_id().unwrap());

        let mut argv0 = key();
        argv0.commands[0].argv0 = ExternalArgumentKey::Literal(b"gcc".to_vec());
        assert_ne!(baseline, argv0.compute_id().unwrap());

        let mut executable_mode = key();
        executable_mode.commands[0].executable_bit = false;
        assert_ne!(baseline, executable_mode.compute_id().unwrap());

        let mut environment = key();
        environment.commands[0].environment.insert(
            b"LANG".to_vec(),
            ExternalArgumentKey::Literal(b"C.UTF-8".to_vec()),
        );
        assert_ne!(baseline, environment.compute_id().unwrap());

        let mut working_directory = key();
        working_directory.commands[0].working_directory = path("obj", "subdir");
        assert_ne!(baseline, working_directory.compute_id().unwrap());

        let mut signal_state = key();
        signal_state.signal_state.blocked = 1_u64 << (10 - 1);
        assert_ne!(baseline, signal_state.compute_id().unwrap());
        signal_state = key();
        signal_state.signal_state.ignored = 1_u64 << (12 - 1);
        assert_ne!(baseline, signal_state.compute_id().unwrap());

        let mut file_creation_mask = key();
        file_creation_mask.file_creation_mask = 0o077;
        assert_ne!(baseline, file_creation_mask.compute_id().unwrap());

        let mut credentials = key();
        credentials.credentials.effective_user = 1000;
        assert_ne!(baseline, credentials.compute_id().unwrap());
        credentials = key();
        credentials.credentials.supplementary_groups = vec![10, 20];
        assert_ne!(baseline, credentials.compute_id().unwrap());
        credentials = key();
        credentials.credentials.effective_capabilities = 1;
        assert_ne!(baseline, credentials.compute_id().unwrap());
        credentials = key();
        credentials.credentials.no_new_privileges = true;
        assert_ne!(baseline, credentials.compute_id().unwrap());

        let mut joined_environment = key();
        joined_environment.commands[0].environment.insert(
            b"OUTPUT".to_vec(),
            ExternalArgumentKey::Joined(vec![
                ExternalArgumentPart::Literal(b"prefix=".to_vec()),
                ExternalArgumentPart::Path(path("obj", "generated")),
            ]),
        );
        assert_ne!(baseline, joined_environment.compute_id().unwrap());

        let mut input = key();
        input.inputs[0].value = ExternalInputValue::File {
            digest: [9; 32],
            executable: false,
        };
        assert_ne!(baseline, input.compute_id().unwrap());

        let mut directory = key();
        directory.inputs[0].value = ExternalInputValue::Directory { mode: 0o755 };
        let directory_id = directory.compute_id().unwrap();
        directory.inputs[0].value = ExternalInputValue::Directory { mode: 0o700 };
        assert_ne!(directory_id, directory.compute_id().unwrap());

        let mut config = key();
        config.inputs[1].value = ExternalInputValue::Virtual {
            name: b"CONFIG_PREEMPT".to_vec(),
            value: b"n".to_vec(),
        };
        assert_ne!(baseline, config.compute_id().unwrap());

        let mut output = key();
        output.outputs[0].kind = ExternalOutputKind::Tree;
        assert_ne!(baseline, output.compute_id().unwrap());

        let mut output_mode = key();
        output_mode.outputs[0].mode = 0o600;
        assert_ne!(baseline, output_mode.compute_id().unwrap());

        let mut platform = key();
        platform
            .execution_properties
            .insert("target_arch".to_string(), "arm64".to_string());
        assert_ne!(baseline, platform.compute_id().unwrap());
    }

    #[test]
    fn command_sequence_is_order_sensitive() {
        let mut first = key();
        first.commands.push(ExternalCommandKey {
            executable: path("toolchain", "bin/objcopy"),
            executable_digest: [4; 32],
            executable_bit: true,
            argv0: ExternalArgumentKey::Literal(b"objcopy".to_vec()),
            argv: vec![ExternalArgumentKey::Path(path("obj", "a.o"))],
            environment: BTreeMap::new(),
            working_directory: path("obj", "tools"),
        });
        let mut second = first.clone();
        second.commands.reverse();
        assert_ne!(first.compute_id().unwrap(), second.compute_id().unwrap());
    }

    #[test]
    fn duplicate_paths_fail_closed() {
        let mut duplicate = key();
        duplicate.inputs.push(duplicate.inputs[0].clone());
        let error = duplicate.compute_id().unwrap_err().to_string();
        assert!(error.contains("duplicate input path"), "{error}");
    }

    #[test]
    fn unmaskable_signal_state_fails_closed() {
        let mut unmaskable = key();
        unmaskable.signal_state.ignored = 1_u64 << (9 - 1);
        let error = unmaskable.compute_id().unwrap_err().to_string();
        assert!(error.contains("unmaskable signal"), "{error}");
    }

    #[test]
    fn invalid_file_creation_mask_fails_closed() {
        let mut invalid = key();
        invalid.file_creation_mask = 0o1000;
        let error = invalid.compute_id().unwrap_err().to_string();
        assert!(error.contains("invalid file-creation mask"), "{error}");
    }

    #[test]
    fn invalid_credential_state_fails_closed() {
        let mut duplicate = key();
        duplicate.credentials.supplementary_groups = vec![10, 10, 20];
        let duplicate_id = duplicate.compute_id().unwrap();
        let mut unique = key();
        unique.credentials.supplementary_groups = vec![10, 20];
        assert_ne!(duplicate_id, unique.compute_id().unwrap());

        let mut invalid = key();
        invalid.credentials.supplementary_groups = vec![20, 10];
        assert!(invalid.compute_id().is_err());

        invalid = key();
        invalid.credentials.real_user = u32::MAX;
        assert!(invalid.compute_id().is_err());
    }
}
