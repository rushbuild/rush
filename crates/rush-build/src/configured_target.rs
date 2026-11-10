// Rush Build - Configured target identity
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Internal configured-target identity; public Label APIs remain unconfigured.
//!
//! Graph storage can distinguish these identities; production loading and
//! execution still use only the explicit legacy target view.
//! The caller must resolve repository mappings before supplying a canonical
//! Label. No dense ConfigId, discovery order or display formatting enters the
//! stable key.

use crate::configuration_table::{encode_bytes, ConfigurationDigest};
use crate::dependency_graph::Label;
use std::fmt;

const CONFIGURED_TARGET_DOMAIN: &[u8] = b"rush.configured-target.v1\0";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConfiguredTargetKey {
    label: Label,
    configuration: ConfigurationDigest,
}

impl ConfiguredTargetKey {
    pub(crate) fn new(label: Label, configuration: ConfigurationDigest) -> Self {
        Self { label, configuration }
    }

    pub(crate) fn label(&self) -> &Label {
        &self.label
    }

    pub(crate) fn configuration(&self) -> ConfigurationDigest {
        self.configuration
    }

    /// V1: domain, optional-repository tag/bytes, package bytes, target bytes,
    /// then the 32-byte semantic configuration digest. Byte lengths are u64 LE.
    pub(crate) fn stable_digest(&self) -> [u8; 32] {
        let mut bytes = CONFIGURED_TARGET_DOMAIN.to_vec();
        match &self.label.workspace {
            Some(repository) => {
                bytes.push(1);
                encode_bytes(&mut bytes, repository.as_bytes());
            }
            None => bytes.push(0),
        }
        encode_bytes(&mut bytes, self.label.package.as_bytes());
        encode_bytes(&mut bytes, self.label.target.as_bytes());
        bytes.extend_from_slice(self.configuration.as_bytes());
        *blake3::hash(&bytes).as_bytes()
    }
}

/// One graph identity. Legacy native callers have not supplied a complete
/// semantic configuration and must never impersonate a configured target.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum NodeKey {
    LegacyTarget(Label),
    Configured(ConfiguredTargetKey),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum NodeScope {
    LegacyTarget,
    Configured(ConfigurationDigest),
}

impl NodeKey {
    pub(crate) fn label(&self) -> &Label {
        match self { Self::LegacyTarget(label) => label, Self::Configured(key) => key.label() }
    }

    pub(crate) fn scope(&self) -> NodeScope {
        match self {
            Self::LegacyTarget(_) => NodeScope::LegacyTarget,
            Self::Configured(key) => NodeScope::Configured(key.configuration()),
        }
    }

    /// Metadata references inherit the owner's exact configuration. A build
    /// dependency transition requires an explicit replacement key instead.
    pub(crate) fn with_label(&self, label: Label) -> Self {
        match self {
            Self::LegacyTarget(_) => Self::LegacyTarget(label),
            Self::Configured(key) => Self::Configured(ConfiguredTargetKey::new(label, key.configuration())),
        }
    }

    pub(crate) fn legacy_label(&self) -> Option<&Label> {
        match self { Self::LegacyTarget(label) => Some(label), Self::Configured(_) => None }
    }
}

impl fmt::Display for NodeKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LegacyTarget(label) => fmt::Display::fmt(label, formatter),
            Self::Configured(key) => write!(formatter, "{} [configuration {}]", key.label(),
                blake3::Hash::from(*key.configuration().as_bytes()).to_hex()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration_table::{ConfigurationRole, ConfigurationValue, SelectedInputIdentity};

    fn configuration(role: ConfigurationRole) -> ConfigurationDigest {
        let selected = SelectedInputIdentity::new(b"selected", [1; 32]).unwrap();
        ConfigurationValue::new(role, vec![], selected.clone(), selected.clone(), selected).digest()
    }

    #[test]
    fn configured_target_identity_keeps_role_and_repository_without_changing_labels() {
        let label = Label::parse("//pkg:target", None, None).unwrap();
        let target = ConfiguredTargetKey::new(label.clone(), configuration(ConfigurationRole::Target));
        let exec = ConfiguredTargetKey::new(label.clone(), configuration(ConfigurationRole::Exec));
        assert_eq!(target.label(), &label);
        assert_ne!(target, exec);
        assert_ne!(target.stable_digest(), exec.stable_digest());
        let external = ConfiguredTargetKey::new(Label::parse("@@owner//pkg:target", None, None).unwrap(),
            configuration(ConfigurationRole::Target));
        assert_ne!(target.stable_digest(), external.stable_digest());
        let explicit_main = ConfiguredTargetKey::new(Label::parse("@@//pkg:target", None, None).unwrap(),
            configuration(ConfigurationRole::Target));
        assert_eq!(target, explicit_main);
        assert_eq!(target.stable_digest(), explicit_main.stable_digest());
        let boundary = ConfiguredTargetKey::new(Label::parse("//pk:gtarget", None, None).unwrap(),
            configuration(ConfigurationRole::Target));
        assert_ne!(target.stable_digest(), boundary.stable_digest());
    }
}
