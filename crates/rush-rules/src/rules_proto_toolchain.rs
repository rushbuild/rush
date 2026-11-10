// Rush Build - native proto language-toolchain declarations
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Retain native proto language-toolchain declarations without inventing a
//! configured ProtoLangToolchainInfo or a replacement compiler.
//!
//! Attribute/default authority: Bazel 6.5.0
//! src/main/starlark/builtins_bzl/common/proto/proto_lang_toolchain.bzl.

use crate::{context::BuildContext, Artifact, BuildRule, RuleInstance, Value};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;

/// The implicit compiler is a configuration field, not a globally resolved
/// executable. Resolving it requires the configured proto fragment and exec
/// transition; this declaration intentionally does neither.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProtoCompilerSelection {
    ConfigurationField,
    /// Preserve a label found in a captured native declaration. This does not
    /// admit its compiler attribute or supply the missing exec transition.
    ExplicitLabel(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtoLangToolchainDeclaration {
    pub name: String,
    pub command_line: String,
    pub output_files: String,
    pub plugin_format_flag: String,
    pub plugin: Option<String>,
    pub runtime: Option<String>,
    pub blacklisted_protos: Vec<String>,
    pub toolchain_type: Option<String>,
    pub progress_message: String,
    pub mnemonic: String,
    pub compiler: ProtoCompilerSelection,
}

fn string(attrs: &HashMap<String, Value>, name: &str, default: Option<&str>) -> Result<String> {
    match attrs.get(name) {
        Some(Value::String(value)) => Ok(value.clone()),
        None => default.map(str::to_owned).with_context(|| format!("proto_lang_toolchain requires {name}")),
        _ => bail!("proto_lang_toolchain attribute {name} must be a string"),
    }
}

fn label(attrs: &HashMap<String, Value>, name: &str) -> Result<Option<String>> {
    match attrs.get(name) {
        None | Some(Value::None) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() && !value.chars().any(|c| c.is_whitespace() || c.is_control()) => Ok(Some(value.clone())),
        _ => bail!("proto_lang_toolchain attribute {name} must be a nonempty label or None"),
    }
}

impl ProtoLangToolchainDeclaration {
    pub fn parse(attrs: &HashMap<String, Value>) -> Result<Self> {
        let output_files = string(attrs, "output_files", Some("legacy"))?;
        if !matches!(output_files.as_str(), "single" | "multiple" | "legacy") {
            bail!("proto_lang_toolchain output_files must be single, multiple, or legacy");
        }
        let blacklisted_protos = match attrs.get("blacklisted_protos") {
            None => Vec::new(),
            Some(Value::List(values)) => values.iter().map(|value| match value {
                Value::String(value) if !value.is_empty() && !value.chars().any(|c| c.is_whitespace() || c.is_control()) => Ok(value.clone()),
                _ => bail!("proto_lang_toolchain blacklisted_protos must contain labels"),
            }).collect::<Result<Vec<_>>>()?,
            _ => bail!("proto_lang_toolchain blacklisted_protos must be a label list"),
        };
        let explicit_compiler = label(attrs, "proto_compiler")?;
        let internal_compiler = label(attrs, "_proto_compiler")?;
        if explicit_compiler.is_some() && internal_compiler.is_some() {
            bail!("proto_lang_toolchain has competing compiler declarations");
        }
        let compiler = explicit_compiler.or(internal_compiler)
            .map(ProtoCompilerSelection::ExplicitLabel).unwrap_or(ProtoCompilerSelection::ConfigurationField);
        Ok(Self {
            name: string(attrs, "name", None)?,
            command_line: string(attrs, "command_line", None)?,
            output_files,
            plugin_format_flag: string(attrs, "plugin_format_flag", Some(""))?,
            plugin: label(attrs, "plugin")?,
            runtime: label(attrs, "runtime")?,
            blacklisted_protos,
            toolchain_type: label(attrs, "toolchain_type")?,
            progress_message: string(attrs, "progress_message", Some("Generating proto_library %{label}"))?,
            mnemonic: string(attrs, "mnemonic", Some("GenProto"))?,
            compiler,
        })
    }

    pub fn dependencies(&self) -> Vec<String> {
        self.exec_dependencies().into_iter().chain(self.runtime.iter().cloned()).chain(self.toolchain_type.iter().cloned())
            .chain(self.blacklisted_protos.iter().cloned()).collect()
    }

    pub fn exec_dependencies(&self) -> Vec<String> {
        let mut result: Vec<_> = self.plugin.iter().cloned().collect();
        if let ProtoCompilerSelection::ExplicitLabel(label) = &self.compiler { result.push(label.clone()); }
        result
    }
}

pub const UNSUPPORTED_PROTO_TOOLCHAIN: &str = "Rush cannot configure proto_lang_toolchain: ProtoLangToolchainInfo, configured proto compiler/plugin exec transitions, runtime and blacklisted-proto semantics are not implemented; no system or bootstrap protoc substitution is permitted for this consumer";

pub struct ProtoLangToolchainRule;
impl BuildRule for ProtoLangToolchainRule {
    fn name(&self) -> &str { "proto_lang_toolchain" }
    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        Ok(Box::new(ProtoLangToolchainInstance { declaration: ProtoLangToolchainDeclaration::parse(attrs)? }))
    }
}

pub struct ProtoLangToolchainInstance {
    pub declaration: ProtoLangToolchainDeclaration,
}

#[async_trait]
impl RuleInstance for ProtoLangToolchainInstance {
    fn target_name(&self) -> &str { &self.declaration.name }
    fn rule_type(&self) -> &str { "proto_lang_toolchain" }
    fn dependencies(&self) -> Vec<String> { self.declaration.dependencies() }
    fn exec_only_dependencies(&self) -> Vec<String> { self.declaration.exec_dependencies() }
    fn configuration_error(&self) -> Option<&str> { Some(UNSUPPORTED_PROTO_TOOLCHAIN) }
    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> { bail!("{UNSUPPORTED_PROTO_TOOLCHAIN}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn attrs() -> HashMap<String, Value> {
        HashMap::from([
            ("name".into(),Value::String("language".into())),
            ("command_line".into(),Value::String("--custom_out=$(OUT)".into())),
            ("plugin".into(),Value::String("@tools//:plugin".into())),
            ("runtime".into(),Value::String(":runtime".into())),
            ("blacklisted_protos".into(),Value::List(vec![Value::String("@proto//:well_known".into())])),
            ("toolchain_type".into(),Value::String("//:type".into())),
        ])
    }
    #[test]
    fn proto_toolchain_preserves_typed_declaration_and_dependency_roles() {
        let raw=attrs();
        let declaration=ProtoLangToolchainDeclaration::parse(&raw).unwrap();
        assert_eq!(declaration.command_line,"--custom_out=$(OUT)");
        assert_eq!(declaration.output_files,"legacy");
        assert_eq!(declaration.compiler,ProtoCompilerSelection::ConfigurationField);
        let instance=ProtoLangToolchainRule.parse_attributes(&raw).unwrap();
        assert_eq!(instance.dependencies(),["@tools//:plugin",":runtime","//:type","@proto//:well_known"]);
        assert_eq!(instance.exec_only_dependencies(),["@tools//:plugin"]);
        assert_eq!(instance.configuration_error(),Some(UNSUPPORTED_PROTO_TOOLCHAIN));
        assert_eq!(declaration.progress_message,"Generating proto_library %{label}");
        assert_eq!(declaration.mnemonic,"GenProto");
        let mut explicit=raw.clone();explicit.insert("_proto_compiler".into(),Value::String("@compiler//:protoc".into()));
        let explicit=ProtoLangToolchainDeclaration::parse(&explicit).unwrap();
        assert_eq!(explicit.compiler,ProtoCompilerSelection::ExplicitLabel("@compiler//:protoc".into()));
        assert_eq!(explicit.exec_dependencies(),["@tools//:plugin","@compiler//:protoc"]);
        assert!(explicit.dependencies().contains(&"@compiler//:protoc".to_string()));
    }
    #[test]
    fn proto_toolchain_refuses_malformed_attributes_without_synthesizing_tools() {
        for (name,value) in [
            ("command_line",Value::Bool(false)),("output_files",Value::String("unknown".into())),
            ("plugin",Value::String("bad label".into())),("runtime",Value::Bool(false)),
            ("blacklisted_protos",Value::List(vec![Value::Int(1)])),
            ("proto_compiler",Value::Bool(false)),
            ("_proto_compiler",Value::Int(0)),
        ] {
            let mut raw=attrs();raw.insert(name.into(),value);
            assert!(ProtoLangToolchainDeclaration::parse(&raw).is_err(),"{name}");
        }
        let mut raw=attrs();raw.remove("command_line");
        assert!(ProtoLangToolchainDeclaration::parse(&raw).is_err());
    }
}
