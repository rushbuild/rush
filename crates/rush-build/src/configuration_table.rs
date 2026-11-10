// Rush Build - Configured identity foundation
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Stable configuration identity for future configured graph integration.
//!
//! This module does not select a toolchain, apply a transition or configure a
//! build. Callers must supply every admitted semantic option (including its
//! resolved defaults) and actual selected-input digests. Existing native graph,
//! action-key and public Label behavior remain unchanged.

use anyhow::{bail, Context, Result};
use rush_action::ids::ConfigId;
use std::collections::HashMap;

const CONFIGURATION_DOMAIN: &[u8] = b"rush.configured-configuration.v1\0";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ConfigurationRole {
    Target,
    Exec,
}

/// A value identity, never a dense ConfigId or an execution-authority token.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConfigurationDigest([u8; 32]);

impl ConfigurationDigest {
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// One ordered semantic option. Unknown option names are errors, not omissions.
/// Compiler arguments retain their exact bytes; this is not an argv parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SemanticNativeOption {
    tag: u8,
    value: Vec<u8>,
}

impl SemanticNativeOption {
    pub(crate) fn new(name: &str, value: &[u8]) -> Result<Self> {
        // These numeric tags and their meanings are part of encoding v1.
        // Recording host/language options here does not implement them in the
        // native executor. That requires later configured-toolchain wiring.
        let tag = match name {
            "cpu" => 0,
            "compilation_mode" => 1,
            "apple_platform_type" => 2,
            "crosstool_top" => 3,
            "stamp" => 4,
            "copt" => 5,
            "define" => 6,
            "collect_code_coverage" => 7,
            "features" => 8,
            "strip" => 9,
            "cxxopt" => 10,
            "conlyopt" => 11,
            "host_copt" => 12,
            "host_cxxopt" => 13,
            "host_conlyopt" => 14,
            _ => bail!("unsupported configured identity option {name:?}"),
        };
        if value.contains(&0) {
            bail!("configured identity option {name:?} contains NUL");
        }
        if !matches!(tag, 5 | 10 | 11 | 12 | 13 | 14) {
            let text = std::str::from_utf8(value)
                .with_context(|| format!("configured identity option {name:?} is not UTF-8"))?;
            if name == "cpu" && text.is_empty() {
                bail!("configured identity requires an explicit CPU value");
            }
            rush_command_core::native_options::canonical_native_predicate(name, text)
                .map_err(anyhow::Error::msg)?;
        }
        Ok(Self { tag, value: value.to_vec() })
    }
}

/// An already selected platform or toolchain and its complete semantic digest.
/// The producer owns digest completeness; this type performs no host discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectedInputIdentity {
    identity: Vec<u8>,
    semantic_digest: [u8; 32],
}

impl SelectedInputIdentity {
    pub(crate) fn new(identity: &[u8], semantic_digest: [u8; 32]) -> Result<Self> {
        if identity.is_empty() || identity.contains(&0) {
            bail!("selected configuration input needs a nonempty identity without NUL");
        }
        // Reserve zero for unavailable identity, rather than admit a placeholder.
        if semantic_digest == [0; 32] {
            bail!("selected configuration input has no semantic digest");
        }
        Ok(Self { identity: identity.to_vec(), semantic_digest })
    }

    fn encode(&self, out: &mut Vec<u8>) {
        encode_bytes(out, &self.identity);
        out.extend_from_slice(&self.semantic_digest);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigurationValue {
    role: ConfigurationRole,
    options: Vec<SemanticNativeOption>,
    target_platform: SelectedInputIdentity,
    execution_platform: SelectedInputIdentity,
    toolchain: SelectedInputIdentity,
}

impl ConfigurationValue {
    pub(crate) fn new(
        role: ConfigurationRole,
        options: Vec<SemanticNativeOption>,
        target_platform: SelectedInputIdentity,
        execution_platform: SelectedInputIdentity,
        toolchain: SelectedInputIdentity,
    ) -> Self {
        Self { role, options, target_platform, execution_platform, toolchain }
    }

    /// V1: domain, role byte, option count, ordered (tag, bytes), then target
    /// platform, execution platform and toolchain (identity bytes, 32-byte digest).
    /// Counts and byte lengths are fixed-width little-endian u64 values.
    pub(crate) fn digest(&self) -> ConfigurationDigest {
        ConfigurationDigest(*blake3::hash(&self.encoding()).as_bytes())
    }

    fn encoding(&self) -> Vec<u8> {
        let mut out = CONFIGURATION_DOMAIN.to_vec();
        out.push(match self.role { ConfigurationRole::Target => 0, ConfigurationRole::Exec => 1 });
        out.extend_from_slice(&(self.options.len() as u64).to_le_bytes());
        for option in &self.options {
            out.push(option.tag);
            encode_bytes(&mut out, &option.value);
        }
        self.target_platform.encode(&mut out);
        self.execution_platform.encode(&mut out);
        self.toolchain.encode(&mut out);
        out
    }
}

pub(super) fn encode_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Local handles for stable values. DEFAULT(0) stays the legacy unconfigured
/// sentinel; this table neither installs configurations nor owns global state.
#[derive(Default)]
pub(crate) struct ConfigurationTable {
    values: Vec<ConfigurationValue>,
    by_digest: HashMap<ConfigurationDigest, ConfigId>,
}

impl ConfigurationTable {
    pub(crate) fn intern(&mut self, value: ConfigurationValue) -> Result<ConfigId> {
        let digest = value.digest();
        if let Some(id) = self.by_digest.get(&digest).copied() {
            if self.get(id) != Some(&value) {
                bail!("configured identity digest collision");
            }
            return Ok(id);
        }
        let id = next_id(self.values.len())?;
        self.values.push(value);
        self.by_digest.insert(digest, id);
        Ok(id)
    }

    pub(crate) fn get(&self, id: ConfigId) -> Option<&ConfigurationValue> {
        self.values.get(usize::try_from(id.0.checked_sub(1)?).ok()?)
    }
}

fn next_id(length: usize) -> Result<ConfigId> {
    Ok(ConfigId(u32::try_from(length).ok().and_then(|n| n.checked_add(1))
        .context("configured identity table exhausted ConfigId handles")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(role: ConfigurationRole) -> ConfigurationValue {
        ConfigurationValue::new(role,
            vec![SemanticNativeOption::new("compilation_mode", b"opt").unwrap()],
            SelectedInputIdentity::new(b"target-platform", [1; 32]).unwrap(),
            SelectedInputIdentity::new(b"exec-platform", [2; 32]).unwrap(),
            SelectedInputIdentity::new(b"toolchain", [3; 32]).unwrap())
    }

    #[test]
    fn configured_identity_role_order_and_selected_inputs_are_semantic() {
        let base = value(ConfigurationRole::Target);
        assert_ne!(base.digest(), value(ConfigurationRole::Exec).digest());
        let mut first = base.clone();
        first.options.extend([SemanticNativeOption::new("copt", b"-O2").unwrap(),
            SemanticNativeOption::new("copt", b"-O3").unwrap()]);
        let mut second = first.clone();
        second.options.swap(1, 2);
        assert_ne!(first.digest(), second.digest());
        for field in 0..3 {
            let mut changed = base.clone();
            let selected = match field { 0 => &mut changed.target_platform,
                1 => &mut changed.execution_platform, _ => &mut changed.toolchain };
            selected.semantic_digest[0] ^= 1;
            assert_ne!(base.digest(), changed.digest());
            let mut renamed = base.clone();
            let selected = match field { 0 => &mut renamed.target_platform,
                1 => &mut renamed.execution_platform, _ => &mut renamed.toolchain };
            selected.identity.push(b'x');
            assert_ne!(base.digest(), renamed.digest());
        }
        let mut swapped = base.clone();
        std::mem::swap(&mut swapped.target_platform, &mut swapped.execution_platform);
        assert_ne!(base.digest(), swapped.digest());
        let mut language = base.clone();
        language.options.push(SemanticNativeOption::new("copt", b"-O2").unwrap());
        let mut other_language = base.clone();
        other_language.options.push(SemanticNativeOption::new("cxxopt", b"-O2").unwrap());
        assert_ne!(language.digest(), other_language.digest());
        let mut raw = base.clone();
        raw.options.push(SemanticNativeOption::new("host_cxxopt", &[b'-', 0xff]).unwrap());
        assert_ne!(raw.digest(), base.digest());
    }

    #[test]
    fn configured_identity_encoding_is_versioned_and_unambiguous() {
        let value = value(ConfigurationRole::Target);
        let bytes = value.encoding();
        let mut expected = b"rush.configured-configuration.v1\0".to_vec();
        expected.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 3, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend_from_slice(b"opt");
        for (length, identity, digest) in [(15u64, b"target-platform".as_slice(), [1; 32]),
            (13, b"exec-platform".as_slice(), [2; 32]), (9, b"toolchain".as_slice(), [3; 32])] {
            expected.extend_from_slice(&length.to_le_bytes());
            expected.extend_from_slice(identity);
            expected.extend_from_slice(&digest);
        }
        assert_eq!(bytes, expected);
        let mut left = value.clone();
        left.options = vec![SemanticNativeOption::new("copt", b"ab").unwrap(),
            SemanticNativeOption::new("copt", b"c").unwrap()];
        let mut right = value;
        right.options = vec![SemanticNativeOption::new("copt", b"a").unwrap(),
            SemanticNativeOption::new("copt", b"bc").unwrap()];
        assert_ne!(left.encoding(), right.encoding());
        assert_ne!(left.digest(), right.digest());
    }

    #[test]
    fn configured_identity_rejects_unknown_or_unavailable_inputs() {
        for (name, bytes) in [("custom", b"value".as_slice()), ("cpu", b""),
            ("compilation_mode", b"invented"), ("stamp", b"maybe"),
            ("define", b"missing-assignment"), ("copt", b"bad\0argument")] {
            assert!(SemanticNativeOption::new(name, bytes).is_err(), "{name}");
        }
        assert!(SelectedInputIdentity::new(b"", [1; 32]).is_err());
        assert!(SelectedInputIdentity::new(b"toolchain", [0; 32]).is_err());
        assert!(SelectedInputIdentity::new(b"bad\0identity", [1; 32]).is_err());
    }

    #[test]
    fn configured_identity_table_handles_do_not_enter_stable_identity() {
        let target = value(ConfigurationRole::Target);
        let exec = value(ConfigurationRole::Exec);
        let mut first = ConfigurationTable::default();
        let target_id = first.intern(target.clone()).unwrap();
        let exec_id = first.intern(exec.clone()).unwrap();
        assert_eq!(first.intern(target.clone()).unwrap(), target_id);
        assert_ne!(target_id, ConfigId::DEFAULT);
        let mut second = ConfigurationTable::default();
        let other_exec = second.intern(exec).unwrap();
        let other_target = second.intern(target).unwrap();
        assert_ne!(target_id, other_target);
        assert_eq!(first.get(target_id).unwrap().digest(), second.get(other_target).unwrap().digest());
        assert_eq!(first.get(exec_id).unwrap().digest(), second.get(other_exec).unwrap().digest());
        assert!(first.get(ConfigId::DEFAULT).is_none());
        assert!(first.get(ConfigId(u32::MAX)).is_none());
        assert!(next_id(u32::MAX as usize).is_err());
    }
}
