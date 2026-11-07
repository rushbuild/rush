// Rush Action - External action-graph manifest
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Versioned records for importing a complete Kbuild action graph.
//!
//! The JSONL format keeps commands opaque. A compound shell recipe is an
//! ordinary program invocation whose arguments include `-c` and the complete
//! script. Rush never parses or rewrites that script. Multiple recipe shells
//! remain separate command entries and execute sequentially as one action.
//!
//! Supported versions execute trusted commands without a per-action sandbox
//! over a clean, prepared output tree established by the wrapper/exporter. The
//! executor freezes the complete output-tree snapshot taken under its lock.
//! Those prepared entries are ambient read-only inputs available to every
//! action and need not be repeated in each action. `preexisting_obj_inputs`
//! names only producerless object paths explicitly used as graph inputs; it is
//! not a whitelist of the prepared snapshot.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::path::PathBuf;

/// Schema identifier accepted by the first Kbuild importer.
pub const KBUILD_ACTION_MANIFEST_SCHEMA: &str = "org.kernel.kbuild.actions";

/// Version 1 schema with an inline environment on every action.
pub const KBUILD_ACTION_MANIFEST_VERSION: u32 = 1;

/// Schema version that interns repeated action environments in the header.
pub const KBUILD_ACTION_MANIFEST_VERSION_2: u32 = 2;

/// Latest schema version emitted by new exporters.
pub const KBUILD_ACTION_MANIFEST_LATEST_VERSION: u32 = KBUILD_ACTION_MANIFEST_VERSION_2;

/// JSONL record discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestRecordKind {
    /// Declares schema and filesystem roots.
    Header,
    /// Declares one executable action.
    Action,
    /// Reports a construct the exporter could not represent.
    Unsupported,
    /// Terminates the stream and states the action count.
    Final,
}

/// First record in a version 1 manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionManifestHeader {
    /// Must be [`ManifestRecordKind::Header`].
    pub record: ManifestRecordKind,
    /// Stable schema identifier.
    pub schema: String,
    /// Schema version.
    pub version: u32,
    /// Source, output, and external tool roots.
    pub roots: ManifestRoots,
    /// Producerless output-tree paths that actions may reference.
    ///
    /// This is not the complete prepared output-tree snapshot. Rush freezes
    /// every entry present when it acquires the output-tree lock and makes
    /// those entries ambient read-only inputs to every action.
    pub preexisting_obj_inputs: Vec<ManifestTypedPath>,
}

/// One exact environment interned by a version 2 header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestNamedEnvironment {
    /// Manifest-local identifier referenced by action records.
    pub id: String,
    /// Exact explicit environment or inherited-environment overlay.
    #[serde(deserialize_with = "deserialize_unique_btree_map")]
    pub env: BTreeMap<String, String>,
}

/// First record in a version 2 manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionManifestHeaderV2 {
    /// Must be [`ManifestRecordKind::Header`].
    pub record: ManifestRecordKind,
    /// Stable schema identifier.
    pub schema: String,
    /// Must be [`KBUILD_ACTION_MANIFEST_VERSION_2`].
    pub version: u32,
    /// Source, output, and external tool roots.
    pub roots: ManifestRoots,
    /// Producerless output-tree paths that actions may reference.
    pub preexisting_obj_inputs: Vec<ManifestTypedPath>,
    /// Environments referenced by version 2 action records.
    pub environments: Vec<ManifestNamedEnvironment>,
}

/// Filesystem roots used by every tagged path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestRoots {
    /// Absolute source-tree root, addressed as `src`.
    pub src: PathBuf,
    /// Absolute output-tree root, addressed as `obj`.
    pub obj: PathBuf,
    /// Named absolute roots containing external tools and tool inputs.
    #[serde(deserialize_with = "deserialize_unique_btree_map")]
    pub tools: BTreeMap<String, PathBuf>,
}

/// A path relative to one declared root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestPath {
    /// `src`, `obj`, or a key from [`ManifestRoots::tools`].
    pub root: String,
    /// Relative path beneath the selected root.
    pub path: PathBuf,
}

/// One exact process invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestCommand {
    /// Executable, interpreter, or shell.
    pub program: ManifestPath,
    /// Arguments passed after argv[0], in their exact order.
    pub argv: Vec<String>,
}

/// How a command receives environment variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestEnvPolicy {
    /// Clear the process environment, then install `env`.
    ExplicitOnly,
    /// Inherit the invoking process environment, then overlay `env`.
    InheritHost,
}

/// Resource class used by Rush permit accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestResourceClass {
    /// Compiler or assembler action.
    Compile,
    /// Linux Kbuild compiler or assembler action.
    KernelCompile,
    /// Final or relocatable link action.
    Link,
    /// Linux Kbuild final or relocatable link action.
    KernelLink,
    /// Archive creation action.
    Archive,
    /// Source or metadata generation action.
    Codegen,
    /// Linux Kbuild source or metadata generation action.
    KernelCodegen,
    /// Lightweight action without a narrower class.
    Other,
}

/// Expected filesystem shape of a declared output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestOutputKind {
    /// Regular file or symbolic link.
    File,
    /// Directory produced by the action.
    Directory,
}

/// A rooted path paired with its required filesystem shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestTypedPath {
    /// Tagged path beneath a declared root.
    pub path: ManifestPath,
    /// Required filesystem shape.
    pub kind: ManifestOutputKind,
}

/// One declared action output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestOutput {
    /// Output path. The importer requires the `obj` root.
    pub path: ManifestPath,
    /// Required shape after every command succeeds.
    pub kind: ManifestOutputKind,
}

/// Reserved depfile descriptor for a later schema implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestDepfile {
    /// Format identifier, such as `gcc` or `kbuild_fixdep`.
    pub format: String,
    /// Depfile path beneath a declared root.
    pub path: ManifestPath,
}

/// One executable action record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionManifestAction {
    /// Must be [`ManifestRecordKind::Action`].
    pub record: ManifestRecordKind,
    /// Unique logical action identifier.
    pub id: String,
    /// Human-readable action family.
    pub mnemonic: String,
    /// Exact process invocations, executed sequentially.
    pub commands: Vec<ManifestCommand>,
    /// Working directory shared by every command.
    pub cwd: ManifestPath,
    /// Declared environment or inherited-environment overlay.
    #[serde(deserialize_with = "deserialize_unique_btree_map")]
    pub env: BTreeMap<String, String>,
    /// Environment inheritance contract.
    pub env_policy: ManifestEnvPolicy,
    /// Inputs whose values affect the action.
    pub normal_inputs: Vec<ManifestPath>,
    /// Inputs that add scheduling edges without value semantics.
    pub order_only_inputs: Vec<ManifestPath>,
    /// Filesystem objects the action must produce.
    pub outputs: Vec<ManifestOutput>,
    /// Permit-accounting class.
    pub resource: ManifestResourceClass,
    /// Reserved dynamic-dependency contract. Version 1 requires `null`.
    pub depfile: Option<ManifestDepfile>,
    /// Reserved Kconfig-value dependencies. Version 1 requires an empty list.
    pub config_deps: Vec<String>,
    /// Reserved value-based downstream suppression. Version 1 requires false.
    pub restat: bool,
    /// Reserved unconditional-execution behavior. Version 1 requires false.
    pub always_run: bool,
    /// Reserved action-cache behavior. Version 1 requires false.
    pub cacheable: bool,
}

/// One executable action record in a version 2 manifest.
///
/// Version 2 replaces the repeated inline `env` map with `env_ref`. All other
/// action semantics are identical to version 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionManifestActionV2 {
    /// Must be [`ManifestRecordKind::Action`].
    pub record: ManifestRecordKind,
    /// Unique logical action identifier.
    pub id: String,
    /// Human-readable action family.
    pub mnemonic: String,
    /// Exact process invocations, executed sequentially.
    pub commands: Vec<ManifestCommand>,
    /// Working directory shared by every command.
    pub cwd: ManifestPath,
    /// Header environment identifier.
    pub env_ref: String,
    /// Environment inheritance contract.
    pub env_policy: ManifestEnvPolicy,
    /// Inputs whose values affect the action.
    pub normal_inputs: Vec<ManifestPath>,
    /// Inputs that add scheduling edges without value semantics.
    pub order_only_inputs: Vec<ManifestPath>,
    /// Filesystem objects the action must produce.
    pub outputs: Vec<ManifestOutput>,
    /// Permit-accounting class.
    pub resource: ManifestResourceClass,
    /// Reserved dynamic-dependency contract. Version 2 requires `null`.
    pub depfile: Option<ManifestDepfile>,
    /// Reserved Kconfig-value dependencies. Version 2 requires an empty list.
    pub config_deps: Vec<String>,
    /// Reserved value-based downstream suppression. Version 2 requires false.
    pub restat: bool,
    /// Reserved unconditional-execution behavior. Version 2 requires false.
    pub always_run: bool,
    /// Reserved action-cache behavior. Version 2 requires false.
    pub cacheable: bool,
}

/// Exporter-generated record that makes incompleteness explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionManifestUnsupported {
    /// Must be [`ManifestRecordKind::Unsupported`].
    pub record: ManifestRecordKind,
    /// Related action identifier, when one was available.
    pub action_id: Option<String>,
    /// Stable unsupported-feature name.
    pub feature: String,
    /// Human-readable exporter explanation.
    pub reason: String,
}

/// Last record in every complete manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionManifestFinal {
    /// Must be [`ManifestRecordKind::Final`].
    pub record: ManifestRecordKind,
    /// Number of preceding action records.
    pub action_count: u64,
}

fn deserialize_unique_btree_map<'de, D, K, V>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    K: Deserialize<'de> + Ord,
    V: Deserialize<'de>,
{
    struct UniqueMapVisitor<K, V>(PhantomData<(K, V)>);

    impl<'de, K, V> serde::de::Visitor<'de> for UniqueMapVisitor<K, V>
    where
        K: Deserialize<'de> + Ord,
        V: Deserialize<'de>,
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map with unique keys")
        }

        fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = access.next_entry()? {
                if values.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate map key"));
                }
            }
            Ok(values)
        }
    }

    deserializer.deserialize_map(UniqueMapVisitor(PhantomData))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_record_round_trips_without_losing_shell_text() {
        let action = ActionManifestAction {
            record: ManifestRecordKind::Action,
            id: "init/main.o".to_string(),
            mnemonic: "CCompile".to_string(),
            commands: vec![ManifestCommand {
                program: ManifestPath {
                    root: "shell".to_string(),
                    path: PathBuf::from("sh"),
                },
                argv: vec![
                    "-c".to_string(),
                    "set -e; cc -c '$src' -o '$obj' && echo done".to_string(),
                ],
            }],
            cwd: ManifestPath {
                root: "obj".to_string(),
                path: PathBuf::from("."),
            },
            env: BTreeMap::new(),
            env_policy: ManifestEnvPolicy::InheritHost,
            normal_inputs: vec![ManifestPath {
                root: "src".to_string(),
                path: PathBuf::from("init/main.c"),
            }],
            order_only_inputs: Vec::new(),
            outputs: vec![ManifestOutput {
                path: ManifestPath {
                    root: "obj".to_string(),
                    path: PathBuf::from("init/main.o"),
                },
                kind: ManifestOutputKind::File,
            }],
            resource: ManifestResourceClass::Compile,
            depfile: None,
            config_deps: Vec::new(),
            restat: false,
            always_run: false,
            cacheable: false,
        };

        let encoded = serde_json::to_string(&action).unwrap();
        let decoded: ActionManifestAction = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, action);
    }

    #[test]
    fn kernel_resource_classes_have_stable_wire_names() {
        for (class, wire_name) in [
            (ManifestResourceClass::KernelCompile, "\"kernel_compile\""),
            (ManifestResourceClass::KernelLink, "\"kernel_link\""),
            (ManifestResourceClass::KernelCodegen, "\"kernel_codegen\""),
        ] {
            let encoded = serde_json::to_string(&class).unwrap();
            assert_eq!(encoded, wire_name);
            assert_eq!(
                serde_json::from_str::<ManifestResourceClass>(&encoded).unwrap(),
                class
            );
        }
    }

    #[test]
    fn action_record_rejects_unknown_fields() {
        let json = r#"{
            "record":"action",
            "id":"a",
            "mnemonic":"Other",
            "commands":[],
            "cwd":{"root":"obj","path":"."},
            "env":{},
            "env_policy":"explicit_only",
            "normal_inputs":[],
            "order_only_inputs":[],
            "outputs":[],
            "resource":"other",
            "depfile":null,
            "config_deps":[],
            "restat":false,
            "always_run":false,
            "cacheable":false,
            "unexpected":true
        }"#;
        let error = serde_json::from_str::<ActionManifestAction>(json).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn header_requires_the_tools_map() {
        let json = r#"{
            "record":"header",
            "schema":"org.kernel.kbuild.actions",
            "version":1,
            "roots":{"src":"/src","obj":"/obj"},
            "preexisting_obj_inputs":[]
        }"#;
        let error = serde_json::from_str::<ActionManifestHeader>(json).unwrap_err();
        assert!(error.to_string().contains("tools"));
    }

    #[test]
    fn action_record_rejects_duplicate_environment_keys() {
        let json = r#"{
            "record":"action",
            "id":"a",
            "mnemonic":"Other",
            "commands":[],
            "cwd":{"root":"obj","path":"."},
            "env":{"PATH":"first","PATH":"second"},
            "env_policy":"explicit_only",
            "normal_inputs":[],
            "order_only_inputs":[],
            "outputs":[],
            "resource":"other",
            "depfile":null,
            "config_deps":[],
            "restat":false,
            "always_run":false,
            "cacheable":false
        }"#;
        let error = serde_json::from_str::<ActionManifestAction>(json).unwrap_err();
        assert!(error.to_string().contains("duplicate map key"));
    }

    #[test]
    fn version_two_header_rejects_duplicate_named_environment_keys() {
        let json = r#"{
            "record":"header",
            "schema":"org.kernel.kbuild.actions",
            "version":2,
            "roots":{"src":"/src","obj":"/obj","tools":{}},
            "preexisting_obj_inputs":[],
            "environments":[{"id":"env-a","env":{"CC":"first","CC":"second"}}]
        }"#;
        let error = serde_json::from_str::<ActionManifestHeaderV2>(json).unwrap_err();
        assert!(error.to_string().contains("duplicate map key"));
    }

    #[test]
    fn version_two_action_rejects_inline_environment() {
        let json = r#"{
            "record":"action",
            "id":"a",
            "mnemonic":"Other",
            "commands":[],
            "cwd":{"root":"obj","path":"."},
            "env_ref":"env-a",
            "env":{},
            "env_policy":"inherit_host",
            "normal_inputs":[],
            "order_only_inputs":[],
            "outputs":[],
            "resource":"other",
            "depfile":null,
            "config_deps":[],
            "restat":false,
            "always_run":false,
            "cacheable":false
        }"#;
        let error = serde_json::from_str::<ActionManifestActionV2>(json).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }
}
