// Dependency graph for build orchestration
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use anyhow::{anyhow, Result};
use crate::configured_target::{ConfiguredTargetKey, NodeKey, NodeScope};
use crate::configuration_table::ConfigurationDigest;
use rush_rules::RuleInstance;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// Label identifying a build target
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Label {
    /// Workspace name (None for main workspace)
    pub workspace: Option<String>,

    /// Package path (e.g., "tensorflow/core")
    pub package: String,

    /// Target name
    pub target: String,
}

impl Label {
    /// Parse a label string
    ///
    /// Supports:
    /// - `//package:target` - absolute label
    /// - `:target` - relative label (requires current package/workspace context)
    /// - `target` - bare target name (equiv to `:target`, requires current package/workspace)
    /// - `//package` - implicit target (target = last package component)
    /// - `@workspace//package:target` - external workspace
    /// - `@workspace` - external repository root target (`@workspace//:workspace`)
    pub fn parse(
        label_str: &str,
        current_workspace: Option<&str>,
        current_package: Option<&str>,
    ) -> Result<Self> {
        // Handle relative label (:target)
        if let Some(stripped) = label_str.strip_prefix(':') {
            let target = stripped.to_string();
            let package = current_package
                .ok_or_else(|| {
                    anyhow!(
                        "Relative label '{}' used without current package context",
                        label_str
                    )
                })?
                .to_string();

            return Ok(Label {
                workspace: current_workspace.map(|ws| ws.to_string()),
                package,
                target,
            });
        }

        // Handle bare target name (treat as :target if in same package)
        // In Bazel, "foo" within a BUILD file is equivalent to ":foo"
        if !label_str.starts_with("//") && !label_str.starts_with('@') && current_package.is_some()
        {
            let target = label_str.to_string();
            let package = current_package.unwrap().to_string();

            return Ok(Label {
                workspace: current_workspace.map(|ws| ws.to_string()),
                package,
                target,
            });
        }

        // Handle external workspace (@workspace//package:target or @workspace)
        let (workspace, rest) = if let Some(stripped) = label_str.strip_prefix("@//") {
            // Bazel's explicit main-repository spelling. This must not inherit
            // an external repository context and must not create an empty
            // external repository named "".
            (None, stripped)
        } else if let Some(stripped) = label_str.strip_prefix("@@//") {
            // Bzlmod canonical root-module spelling: same as the main repo.
            (None, stripped)
        } else if let Some(stripped) = label_str.strip_prefix('@') {
            // '@@name//...' (canonical) parses like '@name//...'.
            let stripped = stripped.strip_prefix('@').unwrap_or(stripped);
            let parts: Vec<_> = stripped.splitn(2, "//").collect();
            if parts.len() == 2 {
                // Standard format: @repo//package:target
                (Some(parts[0].to_string()), parts[1])
            } else {
                // Repository-only format: @repo (treat as @repo//:repo)
                let repo_name = parts[0];
                if repo_name.is_empty() {
                    return Err(anyhow!("Invalid external repository label: {label_str}"));
                }
                return Ok(Label {
                    workspace: Some(repo_name.to_string()),
                    package: String::new(),
                    target: repo_name.to_string(),
                });
            }
        } else if let Some(stripped) = label_str.strip_prefix("//") {
            // Visibility/external sentinels and the one virtual default select
            // label remain main-repository scoped. Named `//conditions`
            // targets are ordinary repo-relative labels.
            let package = stripped.split(':').next().unwrap_or(stripped);
            let workspace = if matches!(package, "visibility" | "external")
                || stripped == "conditions:default"
            {
                None
            } else {
                current_workspace.map(|ws| ws.to_string())
            };
            (workspace, stripped)
        } else {
            return Err(anyhow!(
                "Invalid label format: {} (must start with // or :)",
                label_str
            ));
        };

        // Split package:target
        let parts: Vec<_> = rest.splitn(2, ':').collect();

        let (package, target) = match parts.len() {
            1 => {
                // Implicit target name (//foo/bar → //foo/bar:bar)
                let pkg = parts[0].to_string();
                let tgt = pkg.split('/').next_back().unwrap_or(&pkg).to_string();
                (pkg, tgt)
            }
            2 => (parts[0].to_string(), parts[1].to_string()),
            _ => return Err(anyhow!("Invalid label format: {}", label_str)),
        };

        Ok(Label {
            workspace,
            package,
            target,
        })
    }

    /// Get the package path
    pub fn package_path(&self) -> &str {
        &self.package
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.workspace {
            Some(ws) => write!(f, "@{}//{}:{}", ws, self.package, self.target),
            None => write!(f, "//{}:{}", self.package, self.target),
        }
    }
}

/// Borrowed label edges in the legacy target inspection view. Configured
/// incoming edges are outside this view; execution uses exact key APIs.
#[derive(Clone, Copy)]
pub struct LegacyNodeLabels<'a> {
    keys: &'a [NodeKey],
}

pub struct LegacyLabelIter<'a> {
    keys: std::slice::Iter<'a, NodeKey>,
}

impl<'a> Iterator for LegacyLabelIter<'a> {
    type Item = &'a Label;
    fn next(&mut self) -> Option<Self::Item> {
        self.keys.find_map(NodeKey::legacy_label)
    }
}

impl<'a> LegacyNodeLabels<'a> {
    pub fn iter(&self) -> LegacyLabelIter<'a> { LegacyLabelIter { keys: self.keys.iter() } }
    pub fn len(&self) -> usize { self.iter().count() }
    pub fn is_empty(&self) -> bool { self.first().is_none() }
    pub fn first(&self) -> Option<&'a Label> { self.iter().next() }
    pub fn contains(&self, label: &Label) -> bool { self.iter().any(|entry| entry == label) }
}

impl<'a> IntoIterator for &LegacyNodeLabels<'a> {
    type Item = &'a Label;
    type IntoIter = LegacyLabelIter<'a>;
    fn into_iter(self) -> Self::IntoIter { self.iter() }
}

impl fmt::Debug for LegacyNodeLabels<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_list().entries(self.iter()).finish()
    }
}

impl PartialEq<Vec<Label>> for LegacyNodeLabels<'_> {
    fn eq(&self, other: &Vec<Label>) -> bool { self.iter().eq(other.iter()) }
}

impl std::ops::Index<usize> for LegacyNodeLabels<'_> {
    type Output = Label;
    fn index(&self, index: usize) -> &Self::Output { self.iter().nth(index).expect("legacy edge index out of bounds") }
}

/// Read-only public Label API. References borrow the single keyed node store;
/// no projected graph, edge vectors, or RuleInstances are duplicated.
#[derive(Clone, Copy)]
pub struct LegacyTargetNodeView<'a> {
    pub label: &'a Label,
    pub rule_instance: &'a Arc<dyn RuleInstance>,
    pub dependencies: LegacyNodeLabels<'a>,
    pub dependents: LegacyNodeLabels<'a>,
    pub(crate) package_group_references: LegacyNodeLabels<'a>,
    pub(crate) configuration_references: LegacyNodeLabels<'a>,
    pub(crate) constraint_value_references: LegacyNodeLabels<'a>,
    pub(crate) constraint_setting_reference: Option<&'a Label>,
}

/// Controlled legacy mutation: replacement reparses all edges atomically.
/// Exposing a mutable Label projection would let changes bypass keyed edges.
pub struct LegacyTargetNodeMutView<'a> {
    graph: &'a mut DependencyGraph,
    label: Label,
}

impl LegacyTargetNodeMutView<'_> {
    pub fn view(&self) -> LegacyTargetNodeView<'_> { self.graph.get(&self.label).expect("borrowed legacy node exists") }
    pub fn replace_rule_instance(&mut self, rule: Box<dyn RuleInstance>) -> Result<()> {
        self.graph.add_node(self.label.clone(), rule)?;
        self.graph.rebuild_reverse_edges();
        Ok(())
    }
}

/// A single authoritative node. Every scheduling and metadata edge carries
/// full identity; normal production insertion still creates LegacyTarget only.
pub(crate) struct DependencyNode {
    /// The label identifying this target
    pub label: NodeKey,

    /// The rule instance. `Arc` (not `Box`) so the PASS-B parallel build
    /// wavefront can clone the handle into independent `'static` build futures
    /// (one per ready target) without borrowing the graph. All access is
    /// immutable (`RuleInstance` methods take `&self`), so sharing is sound.
    pub rule_instance: Arc<dyn RuleInstance>,

    /// Dependencies (labels of targets this depends on)
    pub dependencies: Vec<NodeKey>,

    /// Package-group labels referenced by this target's visibility, or by a
    /// package_group's `includes`. These are analysis metadata edges: they
    /// participate in lazy discovery and validation, but never provider flow
    /// or build ordering.
    pub(crate) package_group_references: Vec<NodeKey>,

    /// Configurable-condition labels referenced by this target's `select()`
    /// expressions. Like package groups, these are analysis metadata edges:
    /// they are loaded and visibility-checked without contributing providers
    /// or build ordering.
    pub(crate) configuration_references: Vec<NodeKey>,

    /// The subset of configuration metadata edges originating from
    /// `config_setting(constraint_values = ...)`. Their final alias-resolved
    /// targets must specifically be `constraint_value` declarations.
    pub(crate) constraint_value_references: Vec<NodeKey>,

    /// The `constraint_setting` metadata reference owned by a native
    /// `constraint_value`. It is loaded and provider/type-validated only when
    /// that value enters the configured metadata closure.
    pub(crate) constraint_setting_reference: Option<NodeKey>,

    /// Reverse dependencies (labels of targets that depend on this)
    pub dependents: Vec<NodeKey>,
}

/// Dependency graph for build orchestration
pub struct DependencyGraph {
    /// All nodes indexed by label
    nodes: HashMap<NodeKey, DependencyNode>,

    /// Apparent repository names that identify the main repository.
    /// Labels are canonicalized at graph ingress so every later phase sees
    /// one stable repository identity.
    main_repository_aliases: HashSet<String>,

    /// Workspace-scoped configuration registry identity captured by the
    /// orchestrator. Graph validation may run after thread-local scopes have
    /// been restored, so compatibility checks must use this explicit value.
    configuration_registry_scope: Option<String>,
    configured_registry_scopes: HashMap<ConfigurationDigest, String>,
    configured_visibility_options: HashMap<ConfigurationDigest, VisibilityOptions>,

    visibility_options: VisibilityOptions,
}

/// Bazel visibility compatibility switches that affect graph validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VisibilityOptions {
    enforce_config_setting_visibility: bool,
    config_setting_private_default_visibility: bool,
    package_group_has_public_syntax: bool,
    fix_package_group_reporoot_syntax: bool,
}

impl Default for VisibilityOptions {
    fn default() -> Self {
        Self {
            enforce_config_setting_visibility: true,
            config_setting_private_default_visibility: false,
            package_group_has_public_syntax: true,
            fix_package_group_reporoot_syntax: true,
        }
    }
}

impl VisibilityOptions {
    pub(crate) fn from_argv(argv: &[bytes::Bytes]) -> Result<Self> {
        let mut options = Self::default();
        options.apply_argv(argv)?;
        Ok(options)
    }

    pub(crate) fn from_command_semantics(
        semantics: &rush_command_core::CommandSemantics,
    ) -> Result<Self> {
        let mut options = Self::default();
        if let Some(raw_flags) =
            semantics.getenv(b"RUSH_EFFECTIVE_VISIBILITY_FLAGS")
        {
            let flags = std::str::from_utf8(raw_flags).map_err(|error| {
                anyhow!(
                    "RUSH_EFFECTIVE_VISIBILITY_FLAGS is not UTF-8: {}",
                    error
                )
            })?;
            for argument in flags.lines() {
                options.apply_argument(argument)?;
            }
            return Ok(options);
        }
        options.apply_argv(semantics.argv())?;
        Ok(options)
    }

    fn apply_argv(&mut self, argv: &[bytes::Bytes]) -> Result<()> {
        for raw in argv {
            let Ok(argument) = std::str::from_utf8(raw) else {
                continue;
            };
            if argument == "--" {
                break;
            }
            self.apply_argument(argument)?;
        }
        Ok(())
    }

    fn apply_argument(&mut self, argument: &str) -> Result<()> {
        Self::apply_bool_argument(
            argument,
            "incompatible_enforce_config_setting_visibility",
            &mut self.enforce_config_setting_visibility,
        )?;
        Self::apply_bool_argument(
            argument,
            "incompatible_config_setting_private_default_visibility",
            &mut self.config_setting_private_default_visibility,
        )?;
        Self::apply_bool_argument(
            argument,
            "incompatible_package_group_has_public_syntax",
            &mut self.package_group_has_public_syntax,
        )?;
        Self::apply_bool_argument(
            argument,
            "incompatible_fix_package_group_reporoot_syntax",
            &mut self.fix_package_group_reporoot_syntax,
        )?;
        Ok(())
    }

    fn apply_bool_argument(
        argument: &str,
        name: &str,
        value: &mut bool,
    ) -> Result<()> {
        if argument == format!("--{name}") {
            *value = true;
            return Ok(());
        }
        if argument == format!("--no{name}") {
            *value = false;
            return Ok(());
        }
        let prefix = format!("--{name}=");
        let Some(raw_value) = argument.strip_prefix(&prefix) else {
            return Ok(());
        };
        *value = crate::bazel_compat::parse_bazel_bool(raw_value).map_err(
            |_| {
                anyhow!(
                    "--{} expects a boolean value, got '{}'",
                    name,
                    raw_value
                )
            },
        )?;
        Ok(())
    }
}

impl DependencyGraph {
    /// Create a new dependency graph
    pub fn new() -> Self {
        Self::with_main_repository_aliases(HashSet::new())
    }

    /// Create a graph that canonicalizes the supplied main-repository names.
    pub fn with_main_repository_aliases(main_repository_aliases: HashSet<String>) -> Self {
        DependencyGraph {
            nodes: HashMap::new(),
            main_repository_aliases,
            configuration_registry_scope: None,
            configured_registry_scopes: HashMap::new(),
            configured_visibility_options: HashMap::new(),
            visibility_options: VisibilityOptions::default(),
        }
    }

    pub(crate) fn set_configuration_registry_scope(&mut self, scope: String) {
        // Installing legacy authority cannot leave a configured registry
        // accidentally pointing at the same mutable workspace namespace.
        self.configured_registry_scopes.retain(|_, configured| configured != &scope);
        self.configuration_registry_scope = Some(scope);
    }

    pub(crate) fn feature_flag_error_is_verified(&self, label: &Label, rule: &dyn RuleInstance) -> bool {
        self.feature_flag_key_error_is_verified(&NodeKey::LegacyTarget(label.clone()), rule)
    }

    fn registry_scope_for_key(&self, key: &NodeKey) -> Option<&str> {
        match key.scope() {
            NodeScope::LegacyTarget => self.configuration_registry_scope.as_deref(),
            NodeScope::Configured(digest) => self.configured_registry_scopes.get(&digest).map(String::as_str),
        }
    }

    /// The producer of a complete configuration must supply its actual scoped
    /// metadata authority. A configured key never inherits the legacy registry.
    pub(crate) fn set_configured_registry_scope(&mut self, configuration: ConfigurationDigest, scope: String) -> Result<()> {
        if scope.is_empty() { return Err(anyhow!("configured metadata registry scope is empty")); }
        if self.configuration_registry_scope.as_ref() == Some(&scope)
            || self.configured_registry_scopes.iter().any(|(digest, other)| *digest != configuration && other == &scope) {
            return Err(anyhow!("configured metadata registry authority must be distinct from legacy and other configurations"));
        }
        if self.configured_registry_scopes.get(&configuration).is_some_and(|old| old != &scope) {
            return Err(anyhow!("configured metadata registry scope cannot be replaced"));
        }
        self.configured_registry_scopes.insert(configuration, scope);
        Ok(())
    }

    /// Caller supplies the actual immutable configuration's visibility policy;
    /// neither a digest nor the legacy command can establish these values.
    pub(crate) fn set_configured_visibility_options(&mut self, configuration: ConfigurationDigest, options: VisibilityOptions) -> Result<()> {
        if self.configured_visibility_options.get(&configuration).is_some_and(|old| *old != options) {
            return Err(anyhow!("configured visibility authority cannot be replaced"));
        }
        self.configured_visibility_options.insert(configuration, options);
        Ok(())
    }

    fn visibility_options_for_key(&self, key: &NodeKey) -> Option<VisibilityOptions> {
        match key.scope() {
            NodeScope::LegacyTarget => Some(self.visibility_options),
            NodeScope::Configured(digest) => self.configured_visibility_options.get(&digest).copied(),
        }
    }

    fn require_visibility_options(&self, key: &NodeKey) -> Result<VisibilityOptions> {
        self.visibility_options_for_key(key).ok_or_else(|| anyhow!("configured metadata/visibility scope for {key} has no explicit authority"))
    }

    fn feature_flag_key_error_is_verified(&self, label: &NodeKey, rule: &dyn RuleInstance) -> bool {
        rule.configuration_error_needs_feature_flag_proof()
            && self.registry_scope_for_key(label).is_some_and(|scope|
                rush_starlark::config_defines::feature_flag_condition_is_verified(scope, &label.label().to_string()))
    }

    fn has_pycross_environment_alias_profile(&self, label: &NodeKey) -> bool {
        self.registry_scope_for_key(label).is_some_and(|scope| {
            rush_starlark::config_defines::pycross_environment_alias_profile_in_scope(
                scope, &label.label().to_string(),
            ).is_some()
        })
    }

    pub(crate) fn set_visibility_options(
        &mut self,
        options: VisibilityOptions,
    ) {
        self.visibility_options = options;
    }

    /// Canonicalize a label against this graph's immutable repository map.
    pub fn canonicalize_label(&self, mut label: Label) -> Label {
        if label
            .workspace
            .as_ref()
            .is_some_and(|workspace| self.main_repository_aliases.contains(workspace))
        {
            label.workspace = None;
        }
        label
    }

    fn canonicalize_key(&self, key: NodeKey) -> NodeKey {
        key.with_label(self.canonicalize_label(key.label().clone()))
    }

    fn package_spec_matches(
        &self,
        group: &Label,
        candidate: &Label,
        raw_specification: &str,
    ) -> Result<(bool, bool)> {
        self.package_spec_matches_with_options(group, candidate, raw_specification, self.visibility_options)
    }

    fn package_spec_matches_for_key(&self, group: &NodeKey, candidate: &NodeKey, specification: &str) -> Result<(bool, bool)> {
        self.package_spec_matches_with_options(group.label(), candidate.label(), specification, self.require_visibility_options(group)?)
    }

    fn package_spec_matches_with_options(
        &self, group: &Label, candidate: &Label, raw_specification: &str, options: VisibilityOptions,
    ) -> Result<(bool, bool)> {
        if options.fix_package_group_reporoot_syntax
            && !options.package_group_has_public_syntax
        {
            return Err(anyhow!(
                "Package group {} cannot use the repository-local //... \
                 meaning without \
                 --incompatible_package_group_has_public_syntax",
                group
            ));
        }
        let raw_specification = raw_specification.trim();
        if raw_specification.is_empty() {
            return Err(anyhow!(
                "Package group {} contains an empty package specification",
                group
            ));
        }

        let (negative, specification) = match raw_specification.strip_prefix('-') {
            Some(specification) => (true, specification),
            None => (false, raw_specification),
        };
        if specification.is_empty() {
            return Err(anyhow!(
                "Package group {} has invalid package specification '{}'",
                group,
                raw_specification
            ));
        }

        let matches = match specification {
            "public" | "private"
                if !options.package_group_has_public_syntax =>
            {
                return Err(anyhow!(
                    "Package group {} uses '{}' but public/private package \
                     specifications require \
                     --incompatible_package_group_has_public_syntax",
                    group,
                    specification
                ));
            }
            "public" if !negative => true,
            "private" if !negative => false,
            "public" | "private" => {
                return Err(anyhow!(
                    "Package group {} cannot negate package specification '{}'",
                    group,
                    specification
                ));
            }
            _ => {
                if specification.starts_with('@')
                    || !specification.starts_with("//")
                    || specification.contains(':')
                {
                    return Err(anyhow!(
                        "Package group {} has invalid repository-local package specification '{}'",
                        group,
                        raw_specification
                    ));
                }

                let package_pattern = &specification[2..];
                let (package, recursive) = if package_pattern == "..." {
                    ("", true)
                } else if let Some(package) = package_pattern.strip_suffix("/...") {
                    (package, true)
                } else {
                    (package_pattern, false)
                };
                if (!package.is_empty()
                    && (package.starts_with('/')
                        || package.ends_with('/')
                        || package.split('/').any(|part| {
                            part.is_empty() || matches!(part, "." | "..")
                        })))
                    || package.contains("...")
                {
                    return Err(anyhow!(
                        "Package group {} has invalid package specification '{}'",
                        group,
                        raw_specification
                    ));
                }

                if recursive
                    && package.is_empty()
                    && !options.fix_package_group_reporoot_syntax
                {
                    true
                } else if candidate.workspace != group.workspace {
                    false
                } else if recursive {
                    package.is_empty()
                        || candidate.package == package
                        || candidate
                            .package
                            .strip_prefix(package)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                } else {
                    candidate.package == package
                }
            }
        };
        Ok((negative, matches))
    }

    fn package_group_contains(
        &self,
        group: &NodeKey,
        candidate: &NodeKey,
        visibility_owner: &NodeKey,
        stack: &mut Vec<NodeKey>,
        cache: &mut HashMap<NodeKey, bool>,
    ) -> Result<bool> {
        let group = self.canonicalize_key(group.clone());
        if let Some(cycle_start) = stack.iter().position(|entry| entry == &group) {
            let mut cycle = stack[cycle_start..].to_vec();
            cycle.push(group.clone());
            return Err(anyhow!(
                "Package group include cycle while checking visibility of {}: {}",
                visibility_owner,
                cycle
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" -> ")
            ));
        }
        if let Some(matches) = cache.get(&group) {
            return Ok(*matches);
        }

        let Some(node) = self.nodes.get(&group) else {
            if let Some(includer) = stack.last() {
                return Err(anyhow!(
                    "Package group {} includes unknown package_group {} while checking visibility of {}",
                    includer,
                    group,
                    visibility_owner
                ));
            }
            return Err(anyhow!(
                "Target {} visibility references unknown package_group {}",
                visibility_owner,
                group
            ));
        };
        if let Some(configuration_error) = node.rule_instance.configuration_error() {
            return Err(anyhow!(
                "Package group {} cannot be configured while checking visibility of {}: {}",
                group,
                visibility_owner,
                configuration_error
            ));
        }
        if node.rule_instance.rule_type() != "package_group" {
            if let Some(includer) = stack.last() {
                return Err(anyhow!(
                    "Package group {} includes {}, but it is a {} target rather than a package_group",
                    includer,
                    group,
                    node.rule_instance.rule_type()
                ));
            }
            return Err(anyhow!(
                "Target {} visibility references {}, but it is a {} target rather than a package_group",
                visibility_owner,
                group,
                node.rule_instance.rule_type()
            ));
        }
        let packages = node.rule_instance.package_group_packages().ok_or_else(|| {
            anyhow!(
                "Target {} reports rule type package_group without package metadata",
                group
            )
        })?;

        let mut positive_match = false;
        let mut negative_match = false;
        for specification in packages {
            let (negative, matches) =
                self.package_spec_matches_for_key(&group, candidate, specification)?;
            if negative {
                negative_match |= matches;
            } else {
                positive_match |= matches;
            }
        }
        let own_match = positive_match && !negative_match;

        stack.push(group.clone());
        let mut included_match = false;
        for included_group in &node.package_group_references {
            if self.package_group_contains(
                included_group,
                candidate,
                visibility_owner,
                stack,
                cache,
            )? {
                included_match = true;
            }
        }
        stack.pop();

        let matches = own_match || included_match;
        cache.insert(group, matches);
        Ok(matches)
    }

    fn validate_visibility_edge(
        &self,
        dependent: &NodeKey,
        dependency: &DependencyNode,
    ) -> Result<()> {
        self.require_visibility_options(&dependency.label)?;
        let same_package = dependent.label().workspace == dependency.label.label().workspace
            && dependent.label().package == dependency.label.label().package;
        if same_package || dependency.rule_instance.rule_type() == "package_group" {
            return Ok(());
        }
        if self.uses_config_setting_public_default(dependency) {
            return Ok(());
        }
        let Some(visibility) = dependency.rule_instance.visibility() else {
            // Bazel 6.1's legacy implicit source-file export mode does not use
            // ordinary rule-target default visibility. Config-setting modes
            // were handled explicitly above using their value-origin marker.
            if dependency.rule_instance.rule_type() == "source_file" {
                return Ok(());
            }
            return Err(anyhow!(
                "Target {} cannot depend on {} because {} has private default visibility",
                dependent,
                dependency.label,
                dependency.label
            ));
        };

        let mut cache = HashMap::new();
        let mut granted = false;
        let mut private = false;
        for specification in visibility {
            let specification = specification.trim();
            let parsed = Label::parse(
                specification,
                dependency.label.label().workspace.as_deref(),
                Some(&dependency.label.label().package),
            )
            .map_err(|error| {
                anyhow!(
                    "Target {} has invalid visibility specification '{}': {}",
                    dependency.label,
                    specification,
                    error
                )
            })?;
            let parsed = dependency.label.with_label(self.canonicalize_label(parsed));
            if Self::is_public_visibility_label(parsed.label()) {
                granted = true;
                continue;
            }
            if Self::is_private_visibility_label(parsed.label()) {
                private = true;
                continue;
            }
            let same_repository = dependent.label().workspace == parsed.label().workspace;
            let package_match = dependent.label().package == parsed.label().package;
            let subpackage_match = parsed.label().package.is_empty()
                || package_match
                || dependent
                    .label()
                    .package
                    .strip_prefix(&parsed.label().package)
                    .is_some_and(|suffix| suffix.starts_with('/'));

            match parsed.label().target.as_str() {
                "__pkg__" if same_repository && package_match => granted = true,
                "__subpackages__" if same_repository && subpackage_match => granted = true,
                "__pkg__" | "__subpackages__" => {}
                _ => {
                    if self.package_group_contains(
                        &parsed,
                        dependent,
                        &dependency.label,
                        &mut Vec::new(),
                        &mut cache,
                    )? {
                        granted = true;
                    }
                }
            }
        }

        if granted {
            return Ok(());
        }

        if private {
            return Err(anyhow!(
                "Target {} cannot depend on {} because {} has private visibility",
                dependent,
                dependency.label,
                dependency.label
            ));
        }
        Err(anyhow!(
            "Target {} cannot depend on {} because {} is not visible from {} \
             under visibility {:?}",
            dependent,
            dependency.label,
            dependency.label,
            dependent,
            visibility
        ))
    }

    fn uses_config_setting_public_default(
        &self,
        target: &DependencyNode,
    ) -> bool {
        target.rule_instance.rule_type() == "config_setting"
            && self.visibility_options_for_key(&target.label).is_some_and(|options|
                options.enforce_config_setting_visibility && !options.config_setting_private_default_visibility)
            && !target.rule_instance.visibility_was_explicit()
    }

    fn is_public_visibility_label(label: &Label) -> bool {
        label.workspace.is_none()
            && label.package == "visibility"
            && label.target == "public"
    }

    fn is_private_visibility_label(label: &Label) -> bool {
        label.workspace.is_none()
            && label.package == "visibility"
            && label.target == "private"
    }

    fn alias_actual(node: &DependencyNode) -> Result<&NodeKey> {
        let [actual] = node.dependencies.as_slice() else {
            return Err(anyhow!(
                "Configuration alias {} must have exactly one actual \
                 target, found {}",
                node.label,
                node.dependencies.len()
            ));
        };
        if actual.scope() != node.label.scope() {
            return Err(anyhow!("configuration metadata alias {} crosses configuration scope to {}", node.label, actual));
        }
        Ok(actual)
    }

    fn validate_configuration_references(
        &self,
        owner: &NodeKey,
        node: &DependencyNode,
        inherited_pycross_environment_alias: bool,
    ) -> Result<()> {
        let visibility_options = self.require_visibility_options(owner)?;
        let check_owner_visibility = !node
            .rule_instance
            .suppresses_configuration_condition_visibility();
        for condition_label in &node.configuration_references {
            let mut current_label = condition_label;
            let mut first = true;
            let mut path = Vec::new();
            let pycross_environment_name =
                self.has_pycross_environment_alias_profile(condition_label);
            let mut pycross_environment_alias = inherited_pycross_environment_alias;
            loop {
                if path.contains(current_label) {
                    path.push(current_label.clone());
                    return Err(anyhow!(
                        "Target {} select() condition alias cycle: {}",
                        owner,
                        path.iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(" -> ")
                    ));
                }
                path.push(current_label.clone());
                let condition = self.nodes.get(current_label).ok_or_else(|| {
                    anyhow!(
                        "Target {} select() references {} which doesn't exist \
                         in the graph",
                        owner,
                        current_label
                    )
                })?;
                if first {
                    pycross_environment_alias |= pycross_environment_name
                        && condition.rule_instance.is_configuration_alias();
                }
                if let Some(configuration_error) =
                    condition.rule_instance.configuration_error()
                {
                    let preserved_pycross_flag_boundary = pycross_environment_alias
                        && (inherited_pycross_environment_alias || !first)
                        && condition.rule_instance.configuration_error_needs_feature_flag_proof();
                    let verified_feature_flag_boundary = self.feature_flag_key_error_is_verified(
                        current_label, condition.rule_instance.as_ref());
                    if !preserved_pycross_flag_boundary && !verified_feature_flag_boundary {
                        return Err(anyhow!(
                            "Target {} select() references unconfigurable \
                             condition {}: {}",
                            owner,
                            current_label,
                            configuration_error
                        ));
                    }
                }
                if condition.rule_instance.is_configuration_alias() {
                    if check_owner_visibility
                        && first
                        && visibility_options.enforce_config_setting_visibility
                        && visibility_options.config_setting_private_default_visibility
                    {
                        self.validate_visibility_edge(owner, condition)?;
                    }
                    let actual_label = Self::alias_actual(condition)?;
                    let actual = self.nodes.get(actual_label).ok_or_else(|| {
                        anyhow!(
                            "Configuration alias {} refers to {} which \
                             doesn't exist in the graph",
                            condition.label,
                            actual_label
                        )
                    })?;
                    if visibility_options.enforce_config_setting_visibility
                    {
                        self.validate_visibility_edge(&condition.label, actual)?;
                    }
                    current_label = actual_label;
                    first = false;
                    continue;
                }
                if !condition.rule_instance.is_configuration_condition() {
                    return Err(anyhow!(
                        "Target {} select() key {} is a {} target, not a \
                         config_setting or constraint_value",
                        owner,
                        current_label,
                        condition.rule_instance.rule_type()
                    ));
                }
                if check_owner_visibility
                    && visibility_options.enforce_config_setting_visibility
                    && (first
                        || !visibility_options.config_setting_private_default_visibility)
                {
                    self.validate_visibility_edge(owner, condition)?;
                }
                break;
            }
        }
        Ok(())
    }

    /// Configuration metadata reached through a generated rules_pycross
    /// environment alias may terminate in a flag_values-backed config_setting.
    /// Track that provenance across native configurable-alias chains so the
    /// deliberately unsupported flag_values diagnostic is suppressed only for
    /// this preserved compatibility path. All nodes/edges still load and pass
    /// their normal target-kind, cycle, visibility, and missing-target checks.
    fn pycross_environment_metadata_scope(
        &self,
        roots: &HashSet<NodeKey>,
    ) -> HashSet<NodeKey> {
        let mut queue = VecDeque::new();
        for label in self.configuration_metadata_scope(roots) {
            if self.has_pycross_environment_alias_profile(&label)
                && self.nodes.get(&label).is_some_and(|node| {
                node.rule_instance.is_configuration_alias()
            }) {
                queue.push_back(label);
            }
        }

        let mut scope = HashSet::new();
        while let Some(label) = queue.pop_front() {
            if !scope.insert(label.clone()) {
                continue;
            }
            let Some(node) = self.nodes.get(&label) else {
                continue;
            };
            queue.extend(node.configuration_references.iter().cloned());
            if node.rule_instance.is_configuration_alias() {
                queue.extend(node.dependencies.iter().cloned());
            }
        }
        scope
    }

    fn validate_constraint_value_references(
        &self,
        owner: &NodeKey,
        node: &DependencyNode,
    ) -> Result<()> {
        let mut references = node.constraint_value_references.clone();
        references.sort_by_key(ToString::to_string);
        let mut values_by_setting: HashMap<String, Vec<String>> = HashMap::new();
        for reference in &references {
            let mut current = reference;
            let mut edge_owner = owner.clone();
            let mut path = Vec::new();
            loop {
                if path.contains(current) {
                    path.push(current.clone());
                    return Err(anyhow!(
                        "Config setting {} constraint value alias cycle: {}",
                        owner,
                        path.iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(" -> ")
                    ));
                }
                path.push(current.clone());
                let target = self.nodes.get(current).ok_or_else(|| {
                    anyhow!(
                        "Config setting {} references constraint value {} \
                         which doesn't exist in the graph",
                        owner,
                        current
                    )
                })?;
                self.validate_visibility_edge(&edge_owner, target)?;
                if target.rule_instance.is_configuration_alias() {
                    edge_owner = target.label.clone();
                    current = Self::alias_actual(target)?;
                    continue;
                }
                if target.rule_instance.rule_type() != "constraint_value" {
                    return Err(anyhow!(
                        "Config setting {} constraint_values entry {} is a {} \
                         target, not a constraint_value",
                        owner,
                        current,
                        target.rule_instance.rule_type()
                    ));
                }
                let setting = self.validate_constraint_value_setting_reference(
                    current,
                    target,
                )?;
                values_by_setting
                    .entry(setting.to_string())
                    .or_default()
                    .push(current.to_string());
                break;
            }
        }
        let mut duplicates = values_by_setting
            .into_iter()
            .filter_map(|(setting, mut values)| {
                values.sort();
                values.dedup();
                (values.len() > 1).then_some(format!(
                    "{} => [{}]",
                    setting,
                    values.join(", ")
                ))
            })
            .collect::<Vec<_>>();
        duplicates.sort();
        if !duplicates.is_empty() {
            return Err(anyhow!(
                "Config setting {} has duplicate constraint values for the same constraint_setting: {}",
                owner,
                duplicates.join("; ")
            ));
        }
        Ok(())
    }

    fn validate_constraint_value_setting_reference(
        &self,
        owner: &NodeKey,
        value: &DependencyNode,
    ) -> Result<NodeKey> {
        if value.rule_instance.rule_type() != "constraint_value" {
            return Err(anyhow!(
                "Target {} is a {}, not a constraint_value",
                owner,
                value.rule_instance.rule_type()
            ));
        }
        let mut current = value.constraint_setting_reference.as_ref().ok_or_else(|| {
            anyhow!(
                "Constraint value {} has no constraint_setting metadata",
                owner
            )
        })?;
        let mut edge_owner = owner.clone();
        let mut path = Vec::new();
        loop {
            if path.contains(current) {
                path.push(current.clone());
                return Err(anyhow!(
                    "Constraint value {} constraint_setting alias cycle: {}",
                    owner,
                    path.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" -> ")
                ));
            }
            path.push(current.clone());
            let setting = self.nodes.get(current).ok_or_else(|| {
                anyhow!(
                    "Constraint value {} references constraint_setting {} which doesn't exist in the graph",
                    owner,
                    current
                )
            })?;
            self.validate_visibility_edge(&edge_owner, setting)?;
            if setting.rule_instance.is_configuration_alias() {
                edge_owner = setting.label.clone();
                current = Self::alias_actual(setting)?;
                continue;
            }
            if setting.rule_instance.rule_type() != "constraint_setting" {
                return Err(anyhow!(
                    "Constraint value {} constraint_setting {} is a {} target, not a constraint_setting",
                    owner,
                    current,
                    setting.rule_instance.rule_type()
                ));
            }
            return Ok(current.clone());
        }
    }

    fn validate_loaded_package_groups(&self, scope: &HashSet<NodeKey>) -> Result<()> {
        let configurations: HashSet<_> = scope.iter().map(NodeKey::scope).collect();
        for (label, node) in &self.nodes {
            if !configurations.contains(&label.scope()) { continue; }
            if node.rule_instance.rule_type() != "package_group" {
                continue;
            }
            let packages = node
                .rule_instance
                .package_group_packages()
                .ok_or_else(|| {
                    anyhow!(
                        "Target {} reports rule type package_group without \
                         package metadata",
                        label
                    )
                })?;
            for specification in packages {
                self.package_spec_matches_for_key(label, label, specification)?;
            }
        }
        Ok(())
    }

    /// Existing graph nodes reached through `select()` metadata, including
    /// native-alias actuals and select keys used to configure those aliases.
    ///
    /// This closure is deliberately separate from build reachability: select
    /// keys do not contribute providers or build order, but every loaded key
    /// must receive the same type and visibility validation regardless of
    /// whether its BUILD file was part of the initial parse or loaded lazily.
    fn configuration_metadata_scope(
        &self,
        critical_set: &HashSet<NodeKey>,
    ) -> HashSet<NodeKey> {
        let mut scope = critical_set.clone();
        let mut visited_conditions = HashSet::new();
        let mut queue = VecDeque::new();

        for owner in critical_set {
            let Some(node) = self.nodes.get(owner) else {
                continue;
            };
            queue.extend(node.configuration_references.iter().cloned());
            queue.extend(node.constraint_value_references.iter().cloned());
            if let Some(setting) = &node.constraint_setting_reference {
                queue.push_back(setting.clone());
            }
        }

        while let Some(condition_label) = queue.pop_front() {
            if !visited_conditions.insert(condition_label.clone()) {
                continue;
            }
            scope.insert(condition_label.clone());
            let Some(condition) = self.nodes.get(&condition_label) else {
                continue;
            };
            queue.extend(condition.configuration_references.iter().cloned());
            queue.extend(condition.constraint_value_references.iter().cloned());
            if let Some(setting) = &condition.constraint_setting_reference {
                queue.push_back(setting.clone());
            }
            if condition.rule_instance.is_configuration_alias() {
                if let Ok(actual) = Self::alias_actual(condition) {
                    queue.push_back(actual.clone());
                }
            }
        }
        scope
    }

    /// Bazel's built-in default select key is not a graph target. Every other
    /// condition, including targets in the main `//conditions` package, must
    /// be loaded and validated as configuration metadata.
    fn is_builtin_default_condition(condition: &Label) -> bool {
        condition.workspace.is_none()
            && condition.package == "conditions"
            && condition.target == "default"
    }

    /// Add a node to the graph
    pub fn add_node(&mut self, label: Label, rule_instance: Box<dyn RuleInstance>) -> Result<()> {
        self.add_key_node(NodeKey::LegacyTarget(label), rule_instance)
    }

    pub(crate) fn add_configured_node(&mut self, key: ConfiguredTargetKey, rule_instance: Box<dyn RuleInstance>) -> Result<()> {
        self.add_key_node(NodeKey::Configured(key), rule_instance)
    }

    fn add_key_node(&mut self, key: NodeKey, rule_instance: Box<dyn RuleInstance>) -> Result<()> {
        let key = self.canonicalize_key(key);
        let label = key.label().clone();
        // Parse dependencies from rule instance
        let dep_strings = rule_instance.dependencies();
        let current_package = label.package_path();
        let parent_workspace = label.workspace.clone();
        let dependencies: Result<Vec<Label>> = dep_strings
            .iter()
            .map(|dep_str| {
                let trimmed = dep_str.trim();
                let mut parsed =
                    Label::parse(trimmed, label.workspace.as_deref(), Some(current_package))?;

                // Relative labels (":target" or bare target names) should inherit the
                // workspace of the parent target. Bazel keeps repository context for
                // these references, and TensorFlow's protobuf aliases rely on this
                // behavior (Fix #216).
                let is_relative = trimmed.starts_with(':')
                    || (!trimmed.starts_with("//") && !trimmed.starts_with('@'));
                if is_relative && parsed.workspace.is_none() {
                    if let Some(ws) = &parent_workspace {
                        parsed.workspace = Some(ws.clone());
                    }
                }

                Ok(parsed)
            })
            .collect();

        let mut dependencies = dependencies?;

        // Apply workspace mapping for TSL/XLA packages
        // When a dependency has no workspace but starts with tsl/ or xla/,
        // map to @local_tsl or @local_xla
        for (dep, raw_dependency) in dependencies.iter_mut().zip(dep_strings.iter()) {
            let raw_dependency = raw_dependency.trim();
            // Preserve Bazel's two explicit main-repository spellings before
            // applying TensorFlow's convenience rewrite. Label::parse maps
            // both to workspace=None, which otherwise looks identical to a
            // plain //tsl/... label that should resolve through @local_tsl.
            if raw_dependency.starts_with("@//") || raw_dependency.starts_with("@@//") {
                dep.workspace = None;
                continue;
            }
            // An explicit spelling of the declared main repository wins over
            // TensorFlow's special external-repository rewrites. Without this
            // guard a legal workspace named `local_xla` could turn its own
            // `@local_xla//tsl/...` label into the unrelated @local_tsl repo.
            if dep
                .workspace
                .as_ref()
                .is_some_and(|workspace| self.main_repository_aliases.contains(workspace))
            {
                dep.workspace = None;
                continue;
            }
            if dep.workspace.is_none() {
                if dep.package.starts_with("tsl/") || dep.package == "tsl" {
                    dep.workspace = Some("local_tsl".to_string());
                } else if dep.package.starts_with("xla/") || dep.package == "xla" {
                    dep.workspace = Some("local_xla".to_string());
                }
            }
            // FIX: @local_xla//tsl:* should map to @local_tsl//tsl:*
            // This happens when TSL's .bzl files are loaded from XLA context
            // and use //tsl:target which gets expanded to @local_xla//tsl:target
            if dep.workspace.as_deref() == Some("local_xla")
                && (dep.package.starts_with("tsl/") || dep.package == "tsl")
            {
                dep.workspace = Some("local_tsl".to_string());
            }
        }
        let dependencies: Vec<Label> = dependencies
            .into_iter()
            .map(|dependency| self.canonicalize_label(dependency))
            .collect();

        let is_package_group = rule_instance.rule_type() == "package_group";
        let raw_group_references: Vec<&str> = if is_package_group {
            rule_instance
                .package_group_includes()
                .unwrap_or_default()
                .iter()
                .map(String::as_str)
                .collect()
        } else {
            rule_instance
                .visibility()
                .unwrap_or_default()
                .iter()
                .map(String::as_str)
                .collect()
        };
        let mut package_group_references = Vec::new();
        let mut visibility_sentinel = None;
        for raw_reference in raw_group_references {
            let raw_reference = raw_reference.trim();
            let parsed = Label::parse(
                raw_reference,
                label.workspace.as_deref(),
                Some(label.package_path()),
            )
            .map_err(|error| {
                if is_package_group {
                    anyhow!(
                        "Package group {} has invalid include '{}': {}",
                        label,
                        raw_reference,
                        error
                    )
                } else {
                    anyhow!(
                        "Target {} has invalid visibility specification '{}': {}",
                        label,
                        raw_reference,
                        error
                    )
                }
            })?;
            let parsed = self.canonicalize_label(parsed);
            if !is_package_group
                && (Self::is_public_visibility_label(&parsed)
                    || Self::is_private_visibility_label(&parsed))
            {
                visibility_sentinel = Some(parsed);
                continue;
            }
            if !is_package_group
                && matches!(parsed.target.as_str(), "__pkg__" | "__subpackages__")
            {
                continue;
            }
            if !package_group_references.contains(&parsed) {
                package_group_references.push(parsed);
            }
        }
        if let Some(sentinel) = visibility_sentinel {
            if rule_instance.visibility().map_or(0, <[_]>::len) != 1 {
                return Err(anyhow!(
                    "Target {} cannot combine {} with other visibility \
                     specifications",
                    label,
                    sentinel
                ));
            }
        }

        let mut configuration_references = Vec::new();
        for raw_reference in rule_instance.configuration_conditions() {
            let raw_reference = raw_reference.trim();
            if matches!(
                raw_reference,
                "//conditions:default"
                    | "@//conditions:default"
                    | "@@//conditions:default"
            ) {
                continue;
            }
            let parsed = Label::parse(
                raw_reference,
                label.workspace.as_deref(),
                Some(label.package_path()),
            )
            .map_err(|error| {
                anyhow!(
                    "Target {} has invalid select() condition '{}': {}",
                    label,
                    raw_reference,
                    error
                )
            })?;
            let parsed = self.canonicalize_label(parsed);
            if Self::is_builtin_default_condition(&parsed) {
                continue;
            }
            if !configuration_references.contains(&parsed) {
                configuration_references.push(parsed);
            }
        }

        let mut constraint_value_references = Vec::new();
        for raw_reference in rule_instance.configuration_constraint_values() {
            let raw_reference = raw_reference.trim();
            let parsed = Label::parse(
                raw_reference,
                label.workspace.as_deref(),
                Some(label.package_path()),
            )
            .map_err(|error| {
                anyhow!(
                    "Config setting {} has invalid constraint value '{}': {}",
                    label,
                    raw_reference,
                    error
                )
            })?;
            let parsed = self.canonicalize_label(parsed);
            if !constraint_value_references.contains(&parsed) {
                constraint_value_references.push(parsed);
            }
        }

        let constraint_setting_reference = rule_instance
            .configuration_constraint_setting()
            .map(str::trim)
            .map(|raw_reference| {
                Label::parse(
                    raw_reference,
                    label.workspace.as_deref(),
                    Some(label.package_path()),
                )
                .map(|parsed| self.canonicalize_label(parsed))
                .map_err(|error| {
                    anyhow!(
                        "Constraint value {} has invalid constraint_setting '{}': {}",
                        label,
                        raw_reference,
                        error
                    )
                })
            })
            .transpose()?;

        let node = DependencyNode {
            label: key.clone(),
            // Box -> Arc (From impl); callers keep passing Box::new(rule), no
            // churn at the ~42 add_node call sites.
            rule_instance: Arc::from(rule_instance),
            dependencies: dependencies.into_iter().map(|label| key.with_label(label)).collect(),
            package_group_references: package_group_references.into_iter().map(|label| key.with_label(label)).collect(),
            configuration_references: configuration_references.into_iter().map(|label| key.with_label(label)).collect(),
            constraint_value_references: constraint_value_references.into_iter().map(|label| key.with_label(label)).collect(),
            constraint_setting_reference: constraint_setting_reference.map(|label| key.with_label(label)),
            dependents: Vec::new(),
        };

        self.nodes.insert(key, node);
        Ok(())
    }

    /// A transition replaces only the configuration of already declared build
    /// dependencies. It cannot invent a new dependency label or metadata scope.
    /// This stores identity only; no production loader/executor calls it yet.
    pub(crate) fn set_configured_dependency_keys(&mut self, key: &NodeKey, dependencies: Vec<NodeKey>) -> Result<()> {
        let key = self.canonicalize_key(key.clone());
        let dependencies: Vec<_> = dependencies.into_iter().map(|key| self.canonicalize_key(key)).collect();
        if key.scope() == NodeScope::LegacyTarget {
            return Err(anyhow!("legacy target dependencies cannot activate configured transitions"));
        }
        if dependencies.iter().any(|dependency| dependency.scope() == NodeScope::LegacyTarget) {
            return Err(anyhow!("configured dependency transitions require complete configured keys; LegacyTarget has no semantic configuration"));
        }
        let node = self.nodes.get_mut(&key).ok_or_else(|| anyhow!("unknown configured dependency owner {key}"))?;
        if node.dependencies.len() != dependencies.len()
            || node.dependencies.iter().zip(&dependencies).any(|(old, new)| old.label() != new.label()) {
            return Err(anyhow!("configured transition must preserve declared dependency labels and order for {key}"));
        }
        node.dependencies = dependencies;
        self.rebuild_reverse_edges();
        Ok(())
    }

    /// Check if the graph already contains a node for the given label
    pub(crate) fn has_key(&self, label: &NodeKey) -> bool {
        self.nodes.contains_key(label)
    }

    /// The `rule_type()` of the node at `label`, if present. Used by
    /// output-label alias resolution to confirm a derived producer target is
    /// actually a `cc_shared_library` before aliasing its .so output to it.
    pub(crate) fn key_rule_type(&self, label: &NodeKey) -> Option<&str> {
        self.nodes.get(label).map(|n| n.rule_instance.rule_type())
    }

    /// Snapshot of every label currently in the graph. Used by the lazy-load
    /// loop to seed its "needed" set from the initial BUILD-parse state.
    pub(crate) fn all_keys(&self) -> Vec<NodeKey> {
        self.nodes.keys().cloned().collect()
    }

    /// Collect every (dependent, missing_dep) edge in the graph that rush
    /// wants the lazy loader to chase, skipping platform/GPU/test targets
    /// that `resolve()` would also ignore.
    ///
    /// Returns edges in no particular order. Used by the orchestrator's
    /// lazy-loading loop to batch-process missing deps per iteration instead
    /// of only handling the single edge that `resolve()` happens to report
    /// first (which caused the loop to hit its iteration cap on large
    /// targets like `//tensorflow/core:lib_internal_impl`).
    pub(crate) fn unresolved_key_deps(&self) -> Vec<(NodeKey, NodeKey)> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let all_nodes: HashSet<NodeKey> = self.nodes.keys().cloned().collect();
        let missing_conditions =
            self.missing_configuration_references_for_scope(&all_nodes);
        let condition_owners: HashMap<NodeKey, NodeKey> = missing_conditions
            .iter()
            .map(|(owner, condition)| (condition.clone(), owner.clone()))
            .collect();
        for (label, node) in &self.nodes {
            for dep in &node.dependencies {
                if self.nodes.contains_key(dep) {
                    continue;
                }
                if node.label.scope() == NodeScope::LegacyTarget
                    && !Self::dep_edge_is_mandatory(node)
                    && Self::is_skippable_missing_dep(dep)
                {
                    continue;
                }
                // Dedup by the missing-dep label (many dependents can
                // reference the same missing target; one lazy-load attempt
                // per missing target is enough).
                if !seen.insert(dep.clone()) {
                    continue;
                }
                let owner = condition_owners
                    .get(dep)
                    .cloned()
                    .unwrap_or_else(|| label.clone());
                out.push((owner, dep.clone()));
            }
        }
        for (owner, group) in self.missing_package_groups_for_scope(&all_nodes) {
            if seen.insert(group.clone()) {
                let owner = condition_owners
                    .get(&group)
                    .cloned()
                    .unwrap_or(owner);
                out.push((owner, group));
            }
        }
        for (owner, condition) in missing_conditions {
            if seen.insert(condition.clone()) {
                out.push((owner, condition));
            }
        }
        out
    }

    /// Add package-group metadata referenced by an analyzed target.
    ///
    /// Visibility groups and package-group includes are metadata edges, not
    /// build dependencies. Bazel loads and validates them even when the target
    /// is a direct root or is reached only by a same-package dependency. The
    /// same-package exception applies only to the final permission check.
    fn push_package_group_seeds(
        &self,
        target: &DependencyNode,
        seeds: &mut Vec<(NodeKey, NodeKey)>,
    ) {
        if self.uses_config_setting_public_default(target) {
            return;
        }
        for group in &target.package_group_references {
            seeds.push((target.label.clone(), group.clone()));
        }
    }

    fn package_group_seeds_for_scope(
        &self,
        critical_set: &HashSet<NodeKey>,
    ) -> Vec<(NodeKey, NodeKey)> {
        let mut seeds = Vec::new();
        for label in self.configuration_metadata_scope(critical_set) {
            let Some(node) = self.nodes.get(&label) else {
                continue;
            };
            self.push_package_group_seeds(node, &mut seeds);
        }
        seeds.sort_by(|(left_owner, left_group), (right_owner, right_group)| {
            left_owner
                .to_string()
                .cmp(&right_owner.to_string())
                .then_with(|| left_group.to_string().cmp(&right_group.to_string()))
        });
        seeds
    }

    fn missing_package_groups_for_scope(
        &self,
        critical_set: &HashSet<NodeKey>,
    ) -> Vec<(NodeKey, NodeKey)> {
        let mut queue: VecDeque<(NodeKey, NodeKey)> = self
            .package_group_seeds_for_scope(critical_set)
            .into();
        let mut visited = HashSet::new();
        let mut missing = Vec::new();
        while let Some((owner, group)) = queue.pop_front() {
            if !visited.insert(group.clone()) {
                continue;
            }
            let Some(group_node) = self.nodes.get(&group) else {
                missing.push((owner, group));
                continue;
            };
            if group_node.rule_instance.rule_type() != "package_group" {
                continue;
            }
            for included in &group_node.package_group_references {
                queue.push_back((group.clone(), included.clone()));
            }
        }
        missing
    }

    fn validate_package_group_metadata_for_scope(
        &self,
        critical_set: &HashSet<NodeKey>,
    ) -> Result<()> {
        for (owner, group) in self
            .package_group_seeds_for_scope(critical_set)
        {
            let metadata_root = self
                .nodes
                .get(&owner)
                .filter(|node| {
                    node.rule_instance.rule_type() == "package_group"
                })
                .map_or(&group, |_| &owner);
            self.package_group_contains(
                metadata_root,
                &owner,
                &owner,
                &mut Vec::new(),
                &mut HashMap::new(),
            )?;
        }
        Ok(())
    }

    fn missing_configuration_references_for_scope(
        &self,
        critical_set: &HashSet<NodeKey>,
    ) -> Vec<(NodeKey, NodeKey)> {
        let mut queue = VecDeque::new();
        for owner in critical_set {
            let Some(node) = self.nodes.get(owner) else {
                continue;
            };
            for condition in &node.configuration_references {
                queue.push_back((owner.clone(), condition.clone()));
            }
            for value in &node.constraint_value_references {
                queue.push_back((owner.clone(), value.clone()));
            }
            if let Some(setting) = &node.constraint_setting_reference {
                queue.push_back((owner.clone(), setting.clone()));
            }
        }

        let mut missing = Vec::new();
        let mut visited_conditions = HashSet::new();
        while let Some((owner, condition_label)) = queue.pop_front() {
            let Some(condition) = self.nodes.get(&condition_label) else {
                missing.push((owner, condition_label));
                continue;
            };
            if !visited_conditions.insert(condition_label.clone()) {
                continue;
            }
            for nested in &condition.configuration_references {
                queue.push_back((condition.label.clone(), nested.clone()));
            }
            for value in &condition.constraint_value_references {
                queue.push_back((condition.label.clone(), value.clone()));
            }
            if let Some(setting) = &condition.constraint_setting_reference {
                queue.push_back((condition.label.clone(), setting.clone()));
            }
            if condition.rule_instance.is_configuration_alias() {
                if let Ok(actual) = Self::alias_actual(condition) {
                    queue.push_back((condition.label.clone(), actual.clone()));
                }
            }
        }
        missing.sort_by(|
            (left_owner, left_condition),
            (right_owner, right_condition),
        | {
            left_condition
                .to_string()
                .cmp(&right_condition.to_string())
                .then_with(|| {
                    left_owner.to_string().cmp(&right_owner.to_string())
                })
        });
        missing.dedup_by(|left, right| left.1 == right.1);
        missing.sort_by(|
            (left_owner, left_condition),
            (right_owner, right_condition),
        | {
            left_owner
                .to_string()
                .cmp(&right_owner.to_string())
                .then_with(|| {
                    left_condition
                        .to_string()
                        .cmp(&right_condition.to_string())
                })
        });
        missing
    }

    /// An alias's `actual` edge is mandatory: Bazel resolves it
    /// unconditionally, so the platform/GPU/test name-skip heuristic must
    /// never suppress lazy-loading of an alias referent (Modular's
    /// `//:<gpu>_gpu` -> `@mojo_gpu_toolchains//:<gpu>_gpu`
    /// constraint_values were starved by the `_gpu` suffix skip). Covers
    /// label_flag too, which reports rule_type "alias".
    fn dep_edge_is_mandatory(node: &DependencyNode) -> bool {
        matches!(
            node.rule_instance.rule_type(),
            "alias" | "proto_alias" | "shared_library_output_alias"
        )
    }

    /// Skips that hold regardless of why the caller asks: a Windows `.dll`
    /// on Linux is absent for every purpose. Discovery uses this predicate
    /// alone, so the lazy loader chases everything else and lets the
    /// rule_type filters in the orchestrator classify it once its BUILD
    /// file is parsed.
    fn is_skippable_for_discovery(dep: &NodeKey) -> bool {
        #[cfg(not(target_os = "windows"))]
        if dep.label().target.ends_with(".dll")
            || dep.label().target.ends_with(".lib")
            || dep.label().target.contains("_windows")
        {
            return true;
        }
        #[cfg(not(target_os = "macos"))]
        if dep.label().target.ends_with(".dylib") || dep.label().target.contains("_macos") {
            return true;
        }
        #[cfg(not(target_os = "linux"))]
        if dep.label().target.contains("_linux") {
            return true;
        }
        // GPU-variant targets are skippable ONLY when GPU is NOT configured.
        // Under RUSH_ENABLE_ROCM=1 the GPU path is live, so these are REAL deps
        // that must be resolved — skipping them silently drops, e.g.,
        // @local_xla//xla/service/gpu/fusions/mlir/ir:xla_gpu (a core XLA MLIR
        // dialect library whose name merely ENDS in `_gpu`), so its gentbl
        // `.inc` never generates and a transitive consumer dies on a missing
        // generated header. (Builds fine when named explicitly — proving it's
        // this over-broad skip, not a target bug.)
        let gpu_enabled = std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1")
            || std::env::var("RUSH_ENABLE_GPU").as_deref() == Ok("1");
        if !gpu_enabled
            && (dep.label().target.ends_with("_gpu")
                || dep.label().target.ends_with("_gpu_lib")
                || dep.label().target.contains("_cuda")
                || dep.label().target.contains("_rocm"))
        {
            return true;
        }
        false
    }

    /// Tolerance predicate: decides whether an edge that is STILL missing
    /// after discovery is an error. It adds a name-based test clause on
    /// top of the discovery skips, because a genuine `cc_test` never needs
    /// to resolve for a consumer to link.
    ///
    /// That clause governs tolerance only. Applying it to discovery starves
    /// helper libraries whose names merely contain `_test_`:
    /// `//tensorflow/core/grappler/inputs:trivial_test_graph_input_yielder`
    /// is a `cc_library`, but `contains("_test_")` matched it, so
    /// `find_all_missing_keys` never emitted it, its BUILD file was
    /// never parsed, no node existed, and six grappler tests died on
    /// `undefined reference to TrivialTestGraphInputYielder`. A missing
    /// label has no rule_type yet — parsing its BUILD is what supplies one
    /// — so discovery must chase it and let 96d26bd1's rule_type filters
    /// (build_orchestrator_ext.rs:2606, :3347) reject real test rules.
    fn is_skippable_missing_dep(dep: &NodeKey) -> bool {
        if Self::is_skippable_for_discovery(dep) {
            return true;
        }
        if dep.label().target.ends_with("_test")
            || dep.label().target.ends_with("_tests")
            || dep.label().target.contains("_test_")
        {
            return true;
        }
        false
    }

    /// Same as `resolve()` but only reports a missing edge if the dep_owner
    /// is in `critical_set` — i.e. transitively reachable from the requested
    /// build target. Peripheral nodes (siblings in the root BUILD file that
    /// aren't on the critical path) have their unresolved edges ignored,
    /// so the lazy-loader's iteration budget is spent on nodes rush will
    /// actually build.
    ///
    /// Still populates `node.dependents` for every node. Still applies
    /// the platform/GPU/test skip rules.
    pub(crate) fn resolve_key_scope(&mut self, critical_set: &HashSet<NodeKey>) -> Result<()> {
        for key in critical_set {
            self.require_visibility_options(key)?;
            if key.scope() != NodeScope::LegacyTarget && !self.nodes.contains_key(key) {
                return Err(anyhow!("unknown configured scope root {key}"));
            }
        }
        self.validate_loaded_package_groups(critical_set)?;
        self.validate_package_group_metadata_for_scope(critical_set)?;
        self.rebuild_reverse_edges();

        // Validate only critical-path deps.
        for (label, node) in &self.nodes {
            if !critical_set.contains(label) {
                continue;
            }
            for dep in &node.dependencies {
                if let Some(dependency) = self.nodes.get(dep) {
                    self.validate_visibility_edge(label, dependency)?;
                    continue;
                }
                if node.label.scope() == NodeScope::LegacyTarget
                    && !Self::dep_edge_is_mandatory(node)
                    && Self::is_skippable_missing_dep(dep)
                {
                    continue;
                }
                return Err(anyhow!(
                    "Target {} depends on {} which doesn't exist in the graph",
                    label,
                    dep
                ));
            }
        }
        let mut configuration_scope: Vec<NodeKey> = self
            .configuration_metadata_scope(critical_set)
            .into_iter()
            .collect();
        let pycross_environment_scope =
            self.pycross_environment_metadata_scope(critical_set);
        configuration_scope.sort_by_key(ToString::to_string);
        for label in configuration_scope {
            let Some(node) = self.nodes.get(&label) else {
                continue;
            };
            self.validate_configuration_references(
                &label,
                node,
                pycross_environment_scope.contains(&label),
            )?;
            self.validate_constraint_value_references(&label, node)?;
            if node.rule_instance.rule_type() == "constraint_value" {
                self.validate_constraint_value_setting_reference(&label, node)?;
            }
        }
        Ok(())
    }

    /// Reject label-addressable package-genfiles declarations that map to the
    /// same generated-file path.
    ///
    /// Dependency resolution is intentionally scoped to the requested target,
    /// but these declarations are a loaded-package invariant: an unreachable
    /// sibling in an already-loaded BUILD file must not be allowed to claim the
    /// same path. Repository identity is part of Bazel's output namespace;
    /// main-repository aliases have already been canonicalized at graph ingress.
    /// Walk labels in lexical order so the diagnostic is stable even though the
    /// graph itself is stored in a HashMap.
    pub fn validate_declared_output_ownership(&self) -> Result<()> {
        self.validate_key_declared_output_ownership(&self.legacy_scope())
    }

    pub(crate) fn validate_key_declared_output_ownership(&self, scope: &HashSet<NodeKey>) -> Result<()> {
        let configurations: HashSet<_> = scope.iter().map(NodeKey::scope).collect();
        let mut nodes: Vec<_> = self.nodes.values().filter(|node| configurations.contains(&node.label.scope())).collect();
        nodes.sort_by_key(|node| node.label.to_string());
        let mut owners: HashMap<(NodeScope, Option<String>, String, PathBuf), NodeKey> = HashMap::new();
        for node in nodes {
            let label = node.label.label();
            for output in node.rule_instance.declared_outputs() {
                let normalized = Self::normalize_declared_output(label, &output)?;
                let key = (node.label.scope(), label.workspace.clone(), label.package.clone(), normalized.clone());
                if let Some(owner) = owners.get(&key) {
                    let output_label = Label { workspace: label.workspace.clone(), package: label.package.clone(),
                        target: normalized.to_string_lossy().into_owned() };
                    if owner == &node.label {
                        return Err(anyhow!("Target {} declares output {} more than once", node.label, output_label));
                    }
                    return Err(anyhow!("Declared output {} has multiple owners: {} and {}", output_label, owner, node.label));
                }
                owners.insert(key, node.label.clone());
            }
        }
        Ok(())
    }

    pub(crate) fn normalize_declared_output(label: &Label, output: &str) -> Result<PathBuf> {
        let mut normalized = PathBuf::new();
        for component in Path::new(output).components() {
            match component {
                Component::Normal(part) => normalized.push(part),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(anyhow!(
                        "Target {} declares output '{}' outside its package",
                        label,
                        output
                    ));
                }
            }
        }
        if normalized.as_os_str().is_empty() {
            return Err(anyhow!("Target {} declares an empty output path", label));
        }
        Ok(normalized)
    }

    /// Collect *all* missing critical-path dependencies in a single pass.
    ///
    /// Analogous to `resolve_key_scope`, but instead of aborting on
    /// the first missing edge it walks the full critical set and
    /// returns every `(dependent, missing_dep)` pair, skipping only
    /// platform/GPU labels per `is_skippable_for_discovery` — test-NAMED
    /// labels are chased, because the name does not tell us the rule kind
    /// and helper libraries live under such names. This
    /// lets the outer resolution loop fix an entire layer of missing
    /// deps per iteration instead of retrying O(N × graph_size) work
    /// for N missing edges — turns a library-scale target like
    /// `libtensorflow_cc.so` from quadratic to roughly linear in
    /// graph nodes × topological depth.
    ///
    /// Does not mutate the graph; callers should run the normal
    /// `resolve_key_scope` once the batch is empty to pick up any
    /// late reverse-dep edges.
    pub(crate) fn find_all_missing_keys(
        &self,
        critical_set: &HashSet<NodeKey>,
    ) -> Vec<(NodeKey, NodeKey)> {
        self.find_all_missing_keys_with_configuration(critical_set).0
    }

    /// Retain the configuration-metadata provenance already found by the
    /// scoped traversal. Compatibility library resolvers must not replace
    /// these declarations, including alias destinations, with library stubs.
    pub(crate) fn find_all_missing_keys_with_configuration(
        &self,
        critical_set: &HashSet<NodeKey>,
    ) -> (Vec<(NodeKey, NodeKey)>, HashMap<NodeKey, NodeKey>) {
        let mut missing = Vec::new();
        let missing_conditions =
            self.missing_configuration_references_for_scope(critical_set);
        let condition_owners: HashMap<NodeKey, NodeKey> = missing_conditions
            .iter()
            .map(|(owner, condition)| (condition.clone(), owner.clone()))
            .collect();
        for (label, node) in &self.nodes {
            if !critical_set.contains(label) {
                continue;
            }
            for dep in &node.dependencies {
                if self.nodes.contains_key(dep) {
                    continue;
                }
                if node.label.scope() == NodeScope::LegacyTarget
                    && !Self::dep_edge_is_mandatory(node)
                    && Self::is_skippable_for_discovery(dep)
                {
                    continue;
                }
                let owner = condition_owners
                    .get(dep)
                    .cloned()
                    .unwrap_or_else(|| label.clone());
                missing.push((owner, dep.clone()));
            }
        }
        let mut seen: HashSet<NodeKey> = missing
            .iter()
            .map(|(_, dependency)| dependency.clone())
            .collect();
        for (owner, group) in self.missing_package_groups_for_scope(critical_set) {
            if seen.insert(group.clone()) {
                let owner = condition_owners
                    .get(&group)
                    .cloned()
                    .unwrap_or(owner);
                missing.push((owner, group));
            }
        }
        for (owner, condition) in missing_conditions {
            if seen.insert(condition.clone()) {
                missing.push((owner, condition));
            }
        }
        (missing, condition_owners)
    }

    /// Resolve dependencies and build reverse dependency edges
    pub(crate) fn resolve_all_keys(&mut self) -> Result<()> {
        self.resolve_key_scope(&self.nodes.keys().cloned().collect())
    }

    /// Get all nodes reachable from a target (transitive dependencies)
    pub(crate) fn get_reachable_keys(&self, target: &NodeKey) -> HashSet<NodeKey> {
        let mut reachable = HashSet::new();
        let mut queue = VecDeque::new();
        // #39 graph-RSS: only the first few missing edges are ever printed, so
        // count the rest instead of retaining a (String,String) pair for EVERY
        // missing edge. On a partially-resolved TF graph (millions of
        // not-yet-in-graph dep edges during lazy expansion) the old unbounded
        // Vec allocated tens of GB of throwaway label strings — the discrete
        // ~58GB spike that OOM'd core:lib analyze on the 91GB host.
        let mut missing_count: usize = 0;
        let mut first_missing: Vec<(String, String)> = Vec::new();

        if self.nodes.contains_key(target) {
            queue.push_back(target.clone());
            reachable.insert(target.clone());
        }

        while let Some(label) = queue.pop_front() {
            if let Some(node) = self.nodes.get(&label) {
                for dep in &node.dependencies {
                    if self.nodes.contains_key(dep) {
                        if !reachable.contains(dep) {
                            reachable.insert(dep.clone());
                            queue.push_back(dep.clone());
                        }
                    } else {
                        // Dependency not found in graph - might be a resolution issue.
                        // Keep only the first 5 for display; count the rest (the
                        // old code retained ALL of them — see #39 note above).
                        missing_count += 1;
                        if first_missing.len() < 5 {
                            first_missing.push((label.to_string(), dep.to_string()));
                        }
                    }
                }
            }
        }

        // Report missing dependencies if any (only first 5 to avoid spam)
        if missing_count > 0 {
            eprintln!("  ⚠️  {} dependencies not found in graph:", missing_count);
            for (from, to) in &first_missing {
                eprintln!("     {} -> {}", from, to);
            }
            if missing_count > 5 {
                eprintln!("     ... and {} more", missing_count - 5);
            }
        }

        reachable
    }

    /// Topological sort scoped to a specific target - only includes reachable nodes
    ///
    /// Returns targets in dependency order (dependencies before dependents)
    pub(crate) fn topological_sort_keys_from(&self, target: &NodeKey) -> Result<Vec<NodeKey>> {
        if target.scope() != NodeScope::LegacyTarget && !self.nodes.contains_key(target) {
            return Err(anyhow!("unknown configured traversal root {target}"));
        }
        let reachable = self.get_reachable_keys(target);
        self.topological_sort_key_scope(&reachable)
    }

    /// Topological sort scoped to the union of nodes reachable from
    /// several roots. `rush build t1 t2 …` batches N requested labels
    /// into one invocation: analysis already chases every root, so the
    /// build order must cover the union too — not just the first label.
    pub(crate) fn topological_sort_keys_from_roots(&self, targets: &[NodeKey]) -> Result<Vec<NodeKey>> {
        let mut reachable = HashSet::new();
        for target in targets {
            if target.scope() != NodeScope::LegacyTarget && !self.nodes.contains_key(target) {
                return Err(anyhow!("unknown configured traversal root {target}"));
            }
            reachable.extend(self.get_reachable_keys(target));
        }
        self.topological_sort_key_scope(&reachable)
    }

    /// Topological sort scoped to a set of nodes
    fn topological_sort_key_scope(&self, scope: &HashSet<NodeKey>) -> Result<Vec<NodeKey>> {
        for key in scope {
            let Some(node) = self.nodes.get(key) else { continue; };
            if key.scope() != NodeScope::LegacyTarget {
                if let Some(missing) = node.dependencies.iter().find(|dep| !self.nodes.contains_key(*dep)) {
                    return Err(anyhow!("configured target {key} depends on missing exact key {missing}"));
                }
            }
        }
        let mut in_degree: HashMap<NodeKey, usize> = HashMap::new();

        // Only consider nodes in scope
        for label in scope {
            if let Some(node) = self.nodes.get(label) {
                let valid_dep_count = node
                    .dependencies
                    .iter()
                    .filter(|dep| scope.contains(*dep))
                    .count();
                in_degree.insert(label.clone(), valid_dep_count);
            }
        }

        // Queue nodes with no dependencies
        let mut queue: VecDeque<NodeKey> = in_degree
            .iter()
            .filter(|(_, &degree)| degree == 0)
            .map(|(label, _)| label.clone())
            .collect();

        let mut sorted = Vec::new();

        while let Some(label) = queue.pop_front() {
            sorted.push(label.clone());

            // Reduce in-degree for dependents that are in scope
            if let Some(node) = self.nodes.get(&label) {
                for dependent in &node.dependents {
                    if scope.contains(dependent) {
                        if let Some(degree) = in_degree.get_mut(dependent) {
                            *degree -= 1;
                            if *degree == 0 {
                                queue.push_back(dependent.clone());
                            }
                        }
                    }
                }
            }
        }

        // Recovery pass for stale reverse-edge bookkeeping.
        //
        // `in_degree` here is maintained by decrementing via
        // `node.dependents` as each label is dequeued — but if `resolve()`
        // / `resolve_key_scope()` didn't populate `dependents` to cover
        // every forward edge (NodeKey-equality / parse-context quirks can
        // drop reverse edges silently), some nodes end up stuck with
        // `in_degree > 0` even though every actual dep in scope has been
        // sorted. Rather than reporting a bogus cycle, recompute in-degree
        // from scratch against `sorted_set` and re-queue any node whose
        // unsorted-in-scope deps are genuinely zero. Fixed-point loop so
        // a chain of such nodes (A → B → C) unblocks in one recovery
        // pass.
        let mut recovered_any = true;
        while recovered_any {
            recovered_any = false;
            let sorted_snapshot: HashSet<NodeKey> = sorted.iter().cloned().collect();
            let stuck: Vec<NodeKey> = in_degree
                .iter()
                .filter(|(l, &d)| d > 0 && !sorted_snapshot.contains(l))
                .map(|(l, _)| l.clone())
                .collect();
            for label in stuck {
                let (actually_unresolved, dependents_to_decrement) = {
                    let Some(node) = self.nodes.get(&label) else {
                        continue;
                    };
                    let actually_unresolved = node
                        .dependencies
                        .iter()
                        .filter(|dep| {
                            scope.contains(*dep) && !sorted_snapshot.contains(*dep)
                        })
                        .count();
                    let dependents: Vec<NodeKey> = node
                        .dependents
                        .iter()
                        .filter(|d| scope.contains(*d))
                        .cloned()
                        .collect();
                    (actually_unresolved, dependents)
                };
                if actually_unresolved == 0 {
                    in_degree.insert(label.clone(), 0);
                    sorted.push(label.clone());
                    recovered_any = true;
                    for dependent in dependents_to_decrement {
                        if let Some(d) = in_degree.get_mut(&dependent) {
                            if *d > 0 {
                                *d -= 1;
                            }
                        }
                    }
                }
            }
        }

        // Check for cycles within scope
        if sorted.len() != scope.len() {
            // For each unsorted node, dump the full dep/dependents picture so
            // we can distinguish a genuine cycle from a reverse-edge bookkeeping
            // bug. "all deps in-scope are sorted but in_degree > 0" means the
            // decrement loop never fired — usually because the reverse edge
            // wasn't wired up in `resolve`.
            let sorted_set: HashSet<&NodeKey> = sorted.iter().collect();
            let mut detail: Vec<String> = Vec::new();
            for (label, &degree) in in_degree.iter().filter(|(_, &d)| d > 0).take(40) {
                let node = self.nodes.get(label);
                let deps: Vec<(String, bool, bool)> = node
                    .map(|n| {
                        n.dependencies
                            .iter()
                            .map(|dep| {
                                let in_scope = scope.contains(dep);
                                let in_sorted = sorted_set.contains(dep);
                                (dep.to_string(), in_scope, in_sorted)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let unresolved_sorted = deps
                    .iter()
                    .filter(|(_, in_scope, in_sorted)| *in_scope && !*in_sorted)
                    .map(|(d, _, _)| d.clone())
                    .collect::<Vec<_>>();
                let dependents_count = node.map(|n| n.dependents.len()).unwrap_or(0);
                detail.push(format!(
                    "{} (in_deg={} deps={} dependents_back={} unresolved_in_scope=[{}] all_deps=[{}])",
                    label,
                    degree,
                    deps.len(),
                    dependents_count,
                    unresolved_sorted.join(", "),
                    deps.iter()
                        .map(|(d, s, r)| format!("{}:{}{}", d, if *s { "S" } else { "-" }, if *r { "R" } else { "-" }))
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
            }
            return Err(anyhow!(
                "Circular dependency detected! Sorted {} nodes but scope has {} nodes.\nUnsorted:\n  {}",
                sorted.len(),
                scope.len(),
                detail.join("\n  ")
            ));
        }

        Ok(sorted)
    }

    /// Topological sort - returns build order
    ///
    /// Returns targets in dependency order (dependencies before dependents)
    pub(crate) fn topological_sort_keys(&self) -> Result<Vec<NodeKey>> {
        let mut in_degree: HashMap<NodeKey, usize> = HashMap::new();

        // Calculate in-degree for each node (count only dependencies that exist in the graph)
        // This fixes the case where validation skips certain missing deps but they still
        // count toward the in-degree, causing false "circular dependency" errors.
        for (label, node) in &self.nodes {
            let valid_dep_count = node
                .dependencies
                .iter()
                .filter(|dep| {
                    // Count only if the dependency exists in the graph
                    // OR if it's a platform-specific target we should skip
                    if self.nodes.contains_key(*dep) {
                        return true;
                    }
                    if label.scope() != NodeScope::LegacyTarget {
                        return true;
                    }
                    // Skip these - they're platform/GPU specific and were filtered in validation
                    #[cfg(not(target_os = "windows"))]
                    if dep.label().target.ends_with(".dll")
                        || dep.label().target.ends_with(".lib")
                        || dep.label().target.contains("_windows")
                    {
                        return false;
                    }
                    #[cfg(not(target_os = "macos"))]
                    if dep.label().target.ends_with(".dylib") || dep.label().target.contains("_macos") {
                        return false;
                    }
                    #[cfg(not(target_os = "linux"))]
                    if dep.label().target.contains("_linux") {
                        return false;
                    }
                    // GPU-variant MISSING deps are skippable (don't count toward
                    // in-degree) only when GPU is NOT configured — same gate as
                    // is_skippable_missing_dep / resolve() (0770b65). Under
                    // RUSH_ENABLE_ROCM=1 these are real deps; if one is still
                    // missing it's a genuine resolution gap, not a skip, and the
                    // sort should reflect it rather than silently dropping the
                    // in-degree edge.
                    let gpu_enabled = std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1")
                        || std::env::var("RUSH_ENABLE_GPU").as_deref() == Ok("1");
                    if !gpu_enabled
                        && (dep.label().target.ends_with("_gpu")
                            || dep.label().target.ends_with("_gpu_lib")
                            || dep.label().target.contains("_cuda")
                            || dep.label().target.contains("_rocm"))
                    {
                        return false;
                    }
                    // Skip test targets
                    if dep.label().target.ends_with("_test")
                        || dep.label().target.ends_with("_tests")
                        || dep.label().target.contains("_test_")
                    {
                        return false;
                    }
                    // This dependency doesn't exist but wasn't skipped in validation
                    // This shouldn't happen if validation passed
                    true
                })
                .count();
            in_degree.insert(label.clone(), valid_dep_count);
        }

        // Queue nodes with no dependencies
        let mut queue: VecDeque<NodeKey> = in_degree
            .iter()
            .filter(|(_, &degree)| degree == 0)
            .map(|(label, _)| label.clone())
            .collect();

        let mut sorted = Vec::new();

        while let Some(label) = queue.pop_front() {
            sorted.push(label.clone());

            // Reduce in-degree for dependents
            if let Some(node) = self.nodes.get(&label) {
                for dependent in &node.dependents {
                    let degree = in_degree.get_mut(dependent).unwrap();
                    *degree -= 1;
                    if *degree == 0 {
                        queue.push_back(dependent.clone());
                    }
                }
            }
        }

        // Check for cycles
        if sorted.len() != self.nodes.len() {
            let mut remaining: Vec<String> = in_degree
                .iter()
                .filter(|(_, &degree)| degree > 0)
                .take(10)
                .map(|(label, &degree)| format!("{} (remaining deps: {})", label, degree))
                .collect();
            if remaining.is_empty() {
                remaining.push("unknown nodes".to_string());
            }
            return Err(anyhow!(
                "Circular dependency detected! Sorted {} nodes but graph has {} nodes. Examples: {}",
                sorted.len(),
                self.nodes.len(),
                remaining.join(", ")
            ));
        }

        Ok(sorted)
    }

    fn legacy_scope(&self) -> HashSet<NodeKey> {
        self.nodes.keys().filter(|key| key.scope() == NodeScope::LegacyTarget).cloned().collect()
    }

    fn legacy_key(&self, label: &Label) -> NodeKey {
        NodeKey::LegacyTarget(self.canonicalize_label(label.clone()))
    }

    fn legacy_root_key(&self, label: &Label) -> Result<NodeKey> {
        let key = self.legacy_key(label);
        if !self.nodes.contains_key(&key) && self.nodes.keys().any(|candidate|
            candidate.scope() != NodeScope::LegacyTarget && candidate.label() == key.label()) {
            return Err(anyhow!("Label root {} has configured instances but no legacy target instance; use exact configured key traversal", key.label()));
        }
        Ok(key)
    }

    fn legacy_keys(&self, labels: &HashSet<Label>) -> HashSet<NodeKey> {
        labels.iter().map(|label| self.legacy_key(label)).collect()
    }

    fn project_legacy_keys(keys: impl IntoIterator<Item = NodeKey>) -> Result<Vec<Label>> {
        keys.into_iter().map(|key| match key {
            NodeKey::LegacyTarget(label) => Ok(label),
            NodeKey::Configured(_) => Err(anyhow!("legacy Label traversal cannot return configured execution identity {key}; use exact key traversal")),
        }).collect()
    }

    fn legacy_view(node: &DependencyNode) -> LegacyTargetNodeView<'_> {
        LegacyTargetNodeView {
            label: node.label.label(), rule_instance: &node.rule_instance,
            dependencies: LegacyNodeLabels { keys: &node.dependencies },
            dependents: LegacyNodeLabels { keys: &node.dependents },
            package_group_references: LegacyNodeLabels { keys: &node.package_group_references },
            configuration_references: LegacyNodeLabels { keys: &node.configuration_references },
            constraint_value_references: LegacyNodeLabels { keys: &node.constraint_value_references },
            constraint_setting_reference: node.constraint_setting_reference.as_ref().and_then(NodeKey::legacy_label),
        }
    }

    pub fn get(&self, label: &Label) -> Option<LegacyTargetNodeView<'_>> {
        self.nodes.get(&self.legacy_key(label)).map(Self::legacy_view)
    }

    pub fn get_mut(&mut self, label: &Label) -> Option<LegacyTargetNodeMutView<'_>> {
        let label = self.canonicalize_label(label.clone());
        self.nodes.contains_key(&NodeKey::LegacyTarget(label.clone()))
            .then_some(LegacyTargetNodeMutView { graph: self, label })
    }

    pub(crate) fn get_key(&self, key: &NodeKey) -> Option<&DependencyNode> {
        self.nodes.get(&self.canonicalize_key(key.clone()))
    }

    /// The explicit legacy target inspection view. Configured/exec entries
    /// never answer a Label lookup, census, or generated-output producer scan.
    pub fn iter_target_nodes(&self) -> impl Iterator<Item = (&Label, LegacyTargetNodeView<'_>)> + '_ {
        self.nodes.iter().filter_map(|(key, node)| key.legacy_label().map(|label| (label, Self::legacy_view(node))))
    }

    pub fn target_node_count(&self) -> usize { self.iter_target_nodes().count() }
    pub fn has_node(&self, label: &Label) -> bool { self.nodes.contains_key(&self.legacy_key(label)) }
    pub fn node_rule_type(&self, label: &Label) -> Option<&str> {
        self.nodes.get(&self.legacy_key(label)).map(|node| node.rule_instance.rule_type())
    }
    pub fn all_labels(&self) -> Vec<Label> { self.iter_target_nodes().map(|(label, _)| label.clone()).collect() }

    pub fn unresolved_deps(&self) -> Vec<(Label, Label)> {
        self.unresolved_key_deps().into_iter().filter_map(|(owner, dep)| {
            match (owner, dep) { (NodeKey::LegacyTarget(owner), NodeKey::LegacyTarget(dep)) => Some((owner, dep)), _ => None }
        }).collect()
    }

    pub fn resolve_within_scope(&mut self, scope: &HashSet<Label>) -> Result<()> {
        self.resolve_key_scope(&self.legacy_keys(scope))
    }
    pub fn resolve(&mut self) -> Result<()> { self.resolve_key_scope(&self.legacy_scope()) }

    pub fn find_all_missing_in_scope(&self, scope: &HashSet<Label>) -> Vec<(Label, Label)> {
        self.find_all_missing_in_scope_with_configuration(scope).0
    }
    pub(crate) fn find_all_missing_in_scope_with_configuration(&self, scope: &HashSet<Label>) -> (Vec<(Label, Label)>, HashMap<Label, Label>) {
        let (missing, provenance) = self.find_all_missing_keys_with_configuration(&self.legacy_keys(scope));
        let missing = missing.into_iter().map(|(owner, dep)| match (owner, dep) {
            (NodeKey::LegacyTarget(owner), NodeKey::LegacyTarget(dep)) => (owner, dep),
            _ => unreachable!("legacy metadata edges inherit only legacy identity"),
        }).collect();
        let provenance = provenance.into_iter().map(|(dep, owner)| match (dep, owner) {
            (NodeKey::LegacyTarget(dep), NodeKey::LegacyTarget(owner)) => (dep, owner),
            _ => unreachable!("legacy metadata edges inherit only legacy identity"),
        }).collect();
        (missing, provenance)
    }

    /// Label execution/traversal is a checked projection, never an arbitrary
    /// configured instance of the same label. Production still inserts legacy.
    pub fn get_reachable_nodes(&self, target: &Label) -> Result<HashSet<Label>> {
        Ok(Self::project_legacy_keys(self.get_reachable_keys(&self.legacy_root_key(target)?))?.into_iter().collect())
    }
    pub fn topological_sort_from(&self, target: &Label) -> Result<Vec<Label>> {
        Self::project_legacy_keys(self.topological_sort_keys_from(&self.legacy_root_key(target)?)?)
    }
    pub fn topological_sort_from_roots(&self, targets: &[Label]) -> Result<Vec<Label>> {
        let roots = targets.iter().map(|label| self.legacy_root_key(label)).collect::<Result<Vec<_>>>()?;
        Self::project_legacy_keys(self.topological_sort_keys_from_roots(&roots)?)
    }
    pub fn topological_sort(&self) -> Result<Vec<Label>> {
        // The all-graph execution API cannot silently discard configured
        // instances. Unlike target inspection, this is a checked full order.
        Self::project_legacy_keys(self.topological_sort_keys()?)
    }

    fn rebuild_reverse_edges(&mut self) {
        let mut reverse: HashMap<NodeKey, Vec<NodeKey>> = HashMap::new();
        for (owner, node) in &self.nodes {
            for dep in &node.dependencies { reverse.entry(dep.clone()).or_default().push(owner.clone()); }
        }
        for (key, node) in &mut self.nodes {
            node.dependents = reverse.remove(key).unwrap_or_default();
        }
    }

    /// Number of nodes in the graph (all exact identities).
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// #39 graph-RSS census: structural accounting of the dependency graph.
    /// The core:lib external-RSS trace localized the 54GB hog to the
    /// post-.bzl-load graph-construction phase (BUILD eval -> RuleInstance ->
    /// this graph -> lazy-dep resolution). This sizes the graph shell itself
    /// — nodes, edge Vecs, retained label strings — so the per-package ledger
    /// can tell whether the graph structure dominates or whether the weight is
    /// in the boxed RuleInstance attr payloads (which this does NOT walk; that
    /// gap is itself diagnostic — graph small + RSS huge => attrs are the hog).
    pub fn mem_census(&self) -> String {
        use std::collections::BTreeMap;
        use std::fmt::Write as _;

        let nodes = self.nodes.len();
        let mut dependency_edges = 0usize;
        let mut dependent_edges = 0usize;
        let mut dep_vec_cap = 0usize; // allocated slots, not just len
        let mut label_string_bytes = 0usize;
        let mut dep_label_string_bytes = 0usize;
        let mut max_fanout = 0usize;
        let mut max_fanout_label = String::new();
        let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();

        for (label, node) in &self.nodes {
            let nd = node.dependencies.len();
            let nr = node.dependents.len();
            dependency_edges += nd;
            dependent_edges += nr;
            dep_vec_cap += node.dependencies.capacity() + node.dependents.capacity();
            label_string_bytes += label.to_string().len();
            for d in &node.dependencies {
                dep_label_string_bytes += d.to_string().len();
            }
            for d in &node.dependents {
                dep_label_string_bytes += d.to_string().len();
            }
            if nd > max_fanout {
                max_fanout = nd;
                max_fanout_label = label.to_string();
            }
            *by_kind
                .entry(node.rule_instance.rule_type().to_string())
                .or_insert(0) += 1;
        }

        let mb = |b: usize| (b as f64) / (1024.0 * 1024.0);
        let mut out = String::new();
        let _ = writeln!(out, "DependencyGraph mem census:");
        let _ = writeln!(
            out,
            "  nodes={} (HashMap capacity={})",
            nodes,
            self.nodes.capacity()
        );
        let _ = writeln!(
            out,
            "  dependency_edges={} dependent_edges={} (NodeKey Vec capacity slots={} ~{:.1} MB @ size_of NodeKey)",
            dependency_edges,
            dependent_edges,
            dep_vec_cap,
            mb(dep_vec_cap * std::mem::size_of::<NodeKey>())
        );
        let _ = writeln!(
            out,
            "  label_string_bytes={} (~{:.1} MB) dep_label_string_bytes={} (~{:.1} MB)",
            label_string_bytes,
            mb(label_string_bytes),
            dep_label_string_bytes,
            mb(dep_label_string_bytes)
        );
        let _ = writeln!(out, "  max_fanout={} ({})", max_fanout, max_fanout_label);
        let mut kinds: Vec<(String, usize)> = by_kind.into_iter().collect();
        kinds.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let _ = write!(out, "  rule-kind histogram:");
        for (k, c) in kinds.iter().take(12) {
            let _ = write!(out, " {}={}", k, c);
        }
        out
    }

    /// Check if graph is empty
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

impl Default for DependencyGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use rush_rules::rules_cc::CcBinaryRule;
    use rush_rules::rules_config::UnconfigurableTargetInstance;
    use rush_rules::{
        context::BuildContext, providers::ProviderSet, Artifact, BuildRule,
        PyLibraryRule, Value,
    };
    use std::collections::HashMap;

    struct DummyInstance {
        deps: Vec<String>,
    }

    struct DummyOutputInstance {
        outputs: Vec<String>,
    }

    struct DummyVisibilityInstance {
        visibility: Vec<String>,
    }

    struct DummyPackageGroupInstance {
        name: String,
        packages: Vec<String>,
        includes: Vec<String>,
    }

    struct DummyConfigurationConditionInstance {
        name: String,
        rule_type: &'static str,
    }

    struct DummyAliasInstance {
        name: String,
        actual: String,
    }

    #[async_trait]
    impl RuleInstance for DummyInstance {
        fn target_name(&self) -> &str {
            "dummy"
        }

        fn dependencies(&self) -> Vec<String> {
            self.deps.clone()
        }

        async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
            Err(anyhow!("DummyInstance should not be built in tests"))
        }

        fn providers(&self) -> ProviderSet {
            ProviderSet::empty()
        }
    }

    #[async_trait]
    impl RuleInstance for DummyOutputInstance {
        fn target_name(&self) -> &str {
            "dummy_output"
        }

        fn dependencies(&self) -> Vec<String> {
            Vec::new()
        }

        fn declared_outputs(&self) -> Vec<String> {
            self.outputs.clone()
        }

        async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
            Err(anyhow!("DummyOutputInstance should not be built in tests"))
        }

        fn providers(&self) -> ProviderSet {
            ProviderSet::empty()
        }
    }

    #[async_trait]
    impl RuleInstance for DummyVisibilityInstance {
        fn target_name(&self) -> &str {
            "visible_dummy"
        }

        fn dependencies(&self) -> Vec<String> {
            Vec::new()
        }

        fn visibility(&self) -> Option<&[String]> {
            Some(&self.visibility)
        }

        async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
            Err(anyhow!("DummyVisibilityInstance should not be built in tests"))
        }

        fn providers(&self) -> ProviderSet {
            ProviderSet::empty()
        }
    }

    #[async_trait]
    impl RuleInstance for DummyPackageGroupInstance {
        fn target_name(&self) -> &str {
            &self.name
        }

        fn dependencies(&self) -> Vec<String> {
            Vec::new()
        }

        fn package_group_packages(&self) -> Option<&[String]> {
            Some(&self.packages)
        }

        fn package_group_includes(&self) -> Option<&[String]> {
            Some(&self.includes)
        }

        async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
            Err(anyhow!(
                "DummyPackageGroupInstance should not be built in tests"
            ))
        }

        fn providers(&self) -> ProviderSet {
            ProviderSet::empty()
        }

        fn rule_type(&self) -> &str {
            "package_group"
        }
    }

    #[async_trait]
    impl RuleInstance for DummyConfigurationConditionInstance {
        fn target_name(&self) -> &str {
            &self.name
        }

        fn dependencies(&self) -> Vec<String> {
            Vec::new()
        }

        async fn build(
            &self,
            _context: &BuildContext,
        ) -> Result<Vec<Artifact>> {
            Err(anyhow!(
                "configuration-condition dummy should not be built in tests"
            ))
        }

        fn providers(&self) -> ProviderSet {
            ProviderSet::empty()
        }

        fn rule_type(&self) -> &str {
            self.rule_type
        }

        fn is_configuration_condition(&self) -> bool {
            true
        }
    }

    #[async_trait]
    impl RuleInstance for DummyAliasInstance {
        fn target_name(&self) -> &str {
            &self.name
        }

        fn dependencies(&self) -> Vec<String> {
            vec![self.actual.clone()]
        }

        async fn build(
            &self,
            _context: &BuildContext,
        ) -> Result<Vec<Artifact>> {
            Err(anyhow!("configuration alias dummy should not be built"))
        }

        fn providers(&self) -> ProviderSet {
            ProviderSet::empty()
        }

        fn rule_type(&self) -> &str {
            "alias"
        }

        fn is_configuration_alias(&self) -> bool {
            true
        }
    }

    fn add_package_group(
        graph: &mut DependencyGraph,
        label: &str,
        packages: &[&str],
        includes: &[&str],
    ) {
        let label = Label::parse(label, None, None).unwrap();
        graph
            .add_node(
                label.clone(),
                Box::new(DummyPackageGroupInstance {
                    name: label.target.clone(),
                    packages: packages.iter().map(|value| value.to_string()).collect(),
                    includes: includes.iter().map(|value| value.to_string()).collect(),
                }),
            )
            .unwrap();
    }

    fn add_config_setting(
        graph: &mut DependencyGraph,
        label: &str,
        visibility: Option<Vec<String>>,
        visibility_was_explicit: bool,
    ) {
        let label = Label::parse(label, None, None).unwrap();
        let instance = rush_rules::with_rule_metadata(
            Box::new(DummyConfigurationConditionInstance {
                name: label.target.clone(),
                rule_type: "config_setting",
            }),
            visibility,
            visibility_was_explicit,
            Vec::new(),
        );
        graph.add_node(label, instance).unwrap();
    }

    fn add_constrained_config_setting(
        graph: &mut DependencyGraph,
        label: &str,
        constraints: &[&str],
    ) {
        let label = Label::parse(label, None, None).unwrap();
        let attrs = HashMap::from([
            (
                "name".to_string(),
                Value::String(label.target.clone()),
            ),
            (
                "constraint_values".to_string(),
                Value::List(
                    constraints
                        .iter()
                        .map(|value| Value::String((*value).to_string()))
                        .collect(),
                ),
            ),
        ]);
        let instance = rush_rules::rules_config::ConfigSettingRule
            .parse_attributes(&attrs)
            .unwrap();
        graph.add_node(label, instance).unwrap();
    }

    fn add_constraint_value(
        graph: &mut DependencyGraph,
        label: &str,
        visibility: Option<Vec<String>>,
    ) {
        let label = Label::parse(label, None, None).unwrap();
        let setting_label = Label {
            workspace: label.workspace.clone(),
            package: label.package.clone(),
            target: "__test_constraint_setting".to_string(),
        };
        if !graph.has_node(&setting_label) {
            let setting = rush_rules::rules_config::ConfigNounRule {
                rule_type: "constraint_setting",
            }
            .parse_attributes(&HashMap::from([(
                "name".to_string(),
                Value::String(setting_label.target.clone()),
            )]))
            .unwrap();
            graph.add_node(setting_label, setting).unwrap();
        }
        let constraint = rush_rules::rules_config::ConfigNounRule {
            rule_type: "constraint_value",
        }
        .parse_attributes(&HashMap::from([
            (
                "name".to_string(),
                Value::String(label.target.clone()),
            ),
            (
                "constraint_setting".to_string(),
                Value::String(":__test_constraint_setting".to_string()),
            ),
        ]))
        .unwrap();
        let instance = rush_rules::with_rule_metadata(
            constraint,
            visibility,
            true,
            Vec::new(),
        );
        graph.add_node(label, instance).unwrap();
    }

    fn add_select_consumer(
        graph: &mut DependencyGraph,
        label: &str,
        conditions: &[&str],
    ) {
        let label = Label::parse(label, None, None).unwrap();
        let instance = rush_rules::with_rule_metadata(
            Box::new(DummyInstance { deps: Vec::new() }),
            None,
            false,
            conditions.iter().map(|value| value.to_string()).collect(),
        );
        graph.add_node(label, instance).unwrap();
    }

    fn add_alias(
        graph: &mut DependencyGraph,
        label: &str,
        actual: &str,
        visibility: Option<Vec<String>>,
    ) {
        add_configurable_alias(graph, label, actual, visibility, &[]);
    }

    fn add_configurable_alias(
        graph: &mut DependencyGraph,
        label: &str,
        actual: &str,
        visibility: Option<Vec<String>>,
        conditions: &[&str],
    ) {
        let label = Label::parse(label, None, None).unwrap();
        let instance = rush_rules::with_rule_metadata(
            Box::new(DummyAliasInstance {
                name: label.target.clone(),
                actual: actual.to_string(),
            }),
            visibility,
            true,
            conditions.iter().map(|value| value.to_string()).collect(),
        );
        graph.add_node(label, instance).unwrap();
    }

    fn configured_fixture_key(label: &str, role: crate::configuration_table::ConfigurationRole) -> NodeKey {
        use crate::configuration_table::{ConfigurationValue, SelectedInputIdentity};
        let platform = SelectedInputIdentity::new(b"unit-test-platform", [1; 32]).unwrap();
        let execution = SelectedInputIdentity::new(b"unit-test-execution-platform", [2; 32]).unwrap();
        let toolchain = SelectedInputIdentity::new(b"unit-test-toolchain", [3; 32]).unwrap();
        NodeKey::Configured(ConfiguredTargetKey::new(Label::parse(label, None, None).unwrap(),
            ConfigurationValue::new(role, Vec::new(), platform, execution, toolchain).digest()))
    }

    fn add_configured_fixture(graph: &mut DependencyGraph, key: &NodeKey, rule: Box<dyn RuleInstance>) {
        let NodeKey::Configured(key) = key else { panic!("configured fixture needs a configured key"); };
        // Unit fixtures explicitly supply their visibility policy. The graph
        // does not infer this authority from a digest or the legacy command.
        graph.set_configured_visibility_options(key.configuration(), VisibilityOptions::default()).unwrap();
        graph.add_configured_node(key.clone(), rule).unwrap();
    }

    #[test]
    fn configured_graph_keeps_duplicate_labels_out_of_legacy_lookup() {
        use crate::configuration_table::ConfigurationRole::{Exec, Target};
        let mut graph = DependencyGraph::new();
        let exec = configured_fixture_key("//pkg:tool", Exec);
        let target = configured_fixture_key("//pkg:tool", Target);
        let label = exec.label().clone();
        let NodeKey::Configured(exec_key) = &exec else { unreachable!() };
        graph.add_configured_node(exec_key.clone(), Box::new(DummyInstance { deps: Vec::new() })).unwrap();
        assert!(graph.get(&label).is_none());
        assert!(!graph.has_node(&label));
        assert_eq!(graph.target_node_count(), 0);
        assert!(graph.all_labels().is_empty());
        assert!(graph.get_reachable_nodes(&label).unwrap_err().to_string().contains("no legacy target"));
        assert!(graph.topological_sort_from(&label).is_err());
        assert!(graph.resolve_key_scope(&HashSet::from([exec.clone()])).unwrap_err().to_string().contains("no explicit authority"));
        graph.set_configured_visibility_options(exec_key.configuration(), VisibilityOptions::default()).unwrap();
        add_configured_fixture(&mut graph, &target, Box::new(DummyInstance { deps: Vec::new() }));
        graph.add_node(label.clone(), Box::new(DummyInstance { deps: Vec::new() })).unwrap();
        assert_eq!(graph.len(), 3);
        assert_eq!(graph.target_node_count(), 1);
        assert_eq!(graph.all_labels(), vec![label.clone()]);
        let legacy = graph.get(&label).unwrap();
        assert!(!Arc::ptr_eq(legacy.rule_instance, &graph.get_key(&exec).unwrap().rule_instance));
        assert!(!Arc::ptr_eq(&graph.get_key(&target).unwrap().rule_instance, &graph.get_key(&exec).unwrap().rule_instance));
        assert_eq!(graph.topological_sort_from(&label).unwrap(), vec![label.clone()]);
        assert!(graph.set_configured_dependency_keys(&NodeKey::LegacyTarget(label), Vec::new()).is_err(),
            "legacy loader APIs cannot activate a configured transition");
    }

    #[test]
    fn configured_graph_edges_reachability_cycles_and_reverse_edges_use_full_keys() {
        use crate::configuration_table::ConfigurationRole::{Exec, Target};
        let mut graph = DependencyGraph::new();
        let target_a = configured_fixture_key("//pkg:a", Target);
        let exec_a = configured_fixture_key("//pkg:a", Exec);
        let exec_b = configured_fixture_key("//pkg:b", Exec);
        add_configured_fixture(&mut graph, &target_a, Box::new(DummyInstance { deps: vec![":b".into()] }));
        add_configured_fixture(&mut graph, &exec_b, Box::new(DummyInstance { deps: vec![":a".into()] }));
        add_configured_fixture(&mut graph, &exec_a, Box::new(DummyInstance { deps: Vec::new() }));
        graph.set_configured_dependency_keys(&target_a, vec![exec_b.clone()]).unwrap();
        graph.resolve_all_keys().unwrap();
        assert_eq!(graph.get_reachable_keys(&target_a), HashSet::from([target_a.clone(), exec_a.clone(), exec_b.clone()]));
        let order = graph.topological_sort_keys_from(&target_a).unwrap();
        assert_eq!(order, vec![exec_a.clone(), exec_b.clone(), target_a.clone()]);
        assert_eq!(graph.get_key(&exec_a).unwrap().dependents, vec![exec_b.clone()]);
        assert_eq!(graph.get_key(&exec_b).unwrap().dependents, vec![target_a.clone()]);
        let legacy_b = exec_b.label().clone();
        graph.add_node(legacy_b.clone(), Box::new(DummyInstance { deps: Vec::new() })).unwrap();
        let before_dependencies = graph.get_key(&target_a).unwrap().dependencies.clone();
        let before_dependents = graph.get_key(&exec_b).unwrap().dependents.clone();
        assert!(graph.set_configured_dependency_keys(&target_a,
            vec![NodeKey::LegacyTarget(legacy_b.clone())]).unwrap_err().to_string().contains("complete configured keys"));
        assert_eq!(graph.get_key(&target_a).unwrap().dependencies, before_dependencies);
        assert_eq!(graph.get_key(&exec_b).unwrap().dependents, before_dependents);
        assert!(graph.get(&legacy_b).unwrap().dependents.is_empty());
        assert_eq!(graph.topological_sort_keys_from(&target_a).unwrap(), order);
        assert_eq!(graph.get_reachable_keys(&target_a), HashSet::from([target_a.clone(), exec_a.clone(), exec_b.clone()]));
        assert_eq!(graph.topological_sort_from(&legacy_b).unwrap(), vec![legacy_b]);
        assert!(DependencyGraph::project_legacy_keys(order).is_err());
        assert!(graph.set_configured_dependency_keys(&target_a, vec![exec_a.clone()]).is_err(),
            "transition metadata cannot invent a new dependency label");
        add_configured_fixture(&mut graph, &exec_a, Box::new(DummyInstance { deps: vec![":b".into()] }));
        graph.resolve_all_keys().unwrap();
        assert!(graph.topological_sort_keys_from(&target_a).unwrap_err().to_string().contains("Circular dependency"));
        add_configured_fixture(&mut graph, &exec_a, Box::new(DummyInstance { deps: Vec::new() }));
        graph.resolve_all_keys().unwrap();
        assert_eq!(graph.get_key(&exec_b).unwrap().dependents, vec![target_a.clone()],
            "replacement must not retain an obsolete reverse edge from another exact key");
        assert_eq!(graph.topological_sort_keys_from(&target_a).unwrap().len(), 3);
    }

    #[test]
    fn configured_graph_missing_conditions_and_aliases_do_not_borrow_other_configurations() {
        use crate::configuration_table::ConfigurationRole::{Exec, Target};
        let mut graph = DependencyGraph::new();
        let target = configured_fixture_key("//app:consumer", Target);
        let exec = configured_fixture_key("//app:consumer", Exec);
        let target_condition = configured_fixture_key("//cfg:enabled", Target);
        let exec_condition = configured_fixture_key("//cfg:enabled", Exec);
        for owner in [&target, &exec] {
            add_configured_fixture(&mut graph, owner, rush_rules::with_rule_metadata(
                Box::new(DummyInstance { deps: Vec::new() }), None, true, vec!["//cfg:enabled".into()]));
        }
        let missing = graph.find_all_missing_keys(&HashSet::from([target.clone(), exec.clone()]));
        assert_eq!(missing.into_iter().map(|(_, key)| key).collect::<HashSet<_>>(),
            HashSet::from([target_condition.clone(), exec_condition.clone()]));
        add_configured_fixture(&mut graph, &exec_condition, rush_rules::with_rule_metadata(
            Box::new(DummyInstance { deps: Vec::new() }), Some(vec!["//visibility:public".into()]), true, Vec::new()));
        add_config_setting(&mut graph, "//cfg:enabled", Some(vec!["//visibility:public".into()]), true);
        assert_eq!(graph.find_all_missing_keys(&HashSet::from([target.clone()])), vec![(target.clone(), target_condition.clone())]);
        add_configured_fixture(&mut graph, &target_condition, rush_rules::with_rule_metadata(
            Box::new(DummyConfigurationConditionInstance { name: "enabled".into(), rule_type: "config_setting" }),
            Some(vec!["//visibility:public".into()]), true, Vec::new()));
        graph.resolve_key_scope(&HashSet::from([target.clone()])).unwrap();
        assert!(graph.resolve_key_scope(&HashSet::from([exec.clone()])).unwrap_err().to_string().contains("not a config_setting"));
        // Even explicit transition keys cannot turn metadata alias actuals
        // into an implicit lookup in the target condition's other config.
        let alias = configured_fixture_key("//cfg:alias", Target);
        add_configured_fixture(&mut graph, &alias, Box::new(DummyAliasInstance { name: "alias".into(), actual: ":enabled".into() }));
        graph.set_configured_dependency_keys(&alias, vec![exec_condition]).unwrap();
        let consumer = configured_fixture_key("//app:alias_consumer", Target);
        add_configured_fixture(&mut graph, &consumer, rush_rules::with_rule_metadata(
            Box::new(DummyInstance { deps: Vec::new() }), None, true, vec!["//cfg:alias".into()]));
        assert!(graph.resolve_key_scope(&HashSet::from([consumer])).unwrap_err().to_string().contains("crosses configuration scope"));
    }

    #[test]
    fn configured_graph_registry_authority_isolated_by_configuration_and_command() {
        use crate::configuration_table::ConfigurationRole::{Exec, Target};
        use rush_starlark::config_defines as registry;

        fn declare(scope: &str) {
            registry::with_configuration_scope(scope, || {
                registry::begin_command(&[]);
                registry::consider_config_setting_predicates(None, "cfg", "feature",
                    &HashMap::new(), &HashMap::new(), &[],
                    &HashMap::from([("//cfg:flag".into(), "a".into())]));
            });
        }
        fn publish(scope: &str, actual: &str) {
            let predicate = registry::feature_flag_predicate(scope, "//cfg:feature").unwrap();
            // Exercise the existing verified-metadata publication boundary;
            // this unit test does not emulate a provider or execute a config.
            registry::install_verified_feature_flag_condition(scope,
                registry::feature_flag_generation(scope), predicate,
                std::collections::BTreeMap::from([("//cfg:flag".into(), ("//cfg:flag".into(), actual.into()))])).unwrap();
        }
        fn condition() -> Box<dyn RuleInstance> {
            let rule = rush_rules::rules_config::ConfigSettingRule.parse_attributes(&HashMap::from([
                ("name".into(), Value::String("feature".into())),
                ("flag_values".into(), Value::Dict(HashMap::from([("//cfg:flag".into(), Value::String("a".into()))]))),
            ])).unwrap();
            rush_rules::with_rule_metadata(rule, Some(vec!["//visibility:public".into()]), true, Vec::new())
        }
        fn consumer() -> Box<dyn RuleInstance> {
            rush_rules::with_rule_metadata(Box::new(DummyInstance { deps: Vec::new() }), None, true,
                vec!["//cfg:feature".into()])
        }

        let temp = tempfile::tempdir().unwrap();
        rush_incremental::tracked::test_scope(temp.path(), || {
            let legacy_scope = format!("{}:legacy", temp.path().display());
            let target_scope = format!("{}:target", temp.path().display());
            let exec_scope = format!("{}:exec", temp.path().display());
            for scope in [&legacy_scope, &target_scope, &exec_scope] { declare(scope); }
            publish(&legacy_scope, "a");
            publish(&exec_scope, "b");
            assert!(registry::is_active_config_setting_in_scope(&legacy_scope, "//cfg:feature"));
            assert!(!registry::is_active_config_setting_in_scope(&exec_scope, "//cfg:feature"));

            let mut graph = DependencyGraph::new();
            graph.set_configuration_registry_scope(legacy_scope.clone());
            let owner_label = Label::parse("//app:consumer", None, None).unwrap();
            graph.add_node(owner_label.clone(), consumer()).unwrap();
            graph.add_node(Label::parse("//cfg:feature", None, None).unwrap(), condition()).unwrap();
            let target = configured_fixture_key("//app:consumer", Target);
            let exec = configured_fixture_key("//app:consumer", Exec);
            for role in [Target, Exec] {
                add_configured_fixture(&mut graph, &configured_fixture_key("//app:consumer", role), consumer());
                add_configured_fixture(&mut graph, &configured_fixture_key("//cfg:feature", role), condition());
            }
            let NodeScope::Configured(target_digest) = target.scope() else { unreachable!() };
            let NodeScope::Configured(exec_digest) = exec.scope() else { unreachable!() };
            graph.resolve_within_scope(&HashSet::from([owner_label.clone()])).unwrap();
            assert!(graph.resolve_key_scope(&HashSet::from([target.clone()])).unwrap_err().to_string().contains("unconfigurable"),
                "missing configured authority cannot consume the legacy proof");
            assert!(graph.set_configured_registry_scope(target_digest, legacy_scope.clone()).is_err());
            graph.set_configured_registry_scope(target_digest, target_scope.clone()).unwrap();
            assert!(graph.set_configured_registry_scope(exec_digest, target_scope.clone()).is_err(),
                "two configurations cannot alias one registry authority");
            graph.set_configured_registry_scope(exec_digest, exec_scope.clone()).unwrap();
            assert!(graph.set_configured_registry_scope(target_digest, format!("{target_scope}:replacement")).is_err());
            graph.resolve_key_scope(&HashSet::from([exec.clone()])).unwrap();
            assert!(graph.resolve_key_scope(&HashSet::from([target.clone()])).unwrap_err().to_string().contains("unconfigurable"),
                "exec proof for the same condition label cannot authorize target metadata");
            publish(&target_scope, "a");
            graph.resolve_key_scope(&HashSet::from([target.clone()])).unwrap();
            assert!(registry::is_active_config_setting_in_scope(&target_scope, "//cfg:feature"));
            registry::with_configuration_scope(&target_scope, || registry::begin_command(&[]));
            assert!(graph.resolve_key_scope(&HashSet::from([target])).unwrap_err().to_string().contains("unconfigurable"));
            graph.resolve_key_scope(&HashSet::from([exec])).unwrap();
            graph.resolve_within_scope(&HashSet::from([owner_label])).unwrap();
        });
    }

    #[test]
    fn configured_graph_missing_dependencies_are_configuration_qualified() {
        use crate::configuration_table::ConfigurationRole::{Exec, Target};
        let mut graph = DependencyGraph::new();
        let mut owners = Vec::new();
        let mut expected = HashSet::new();
        for role in [Target, Exec] {
            let owner = configured_fixture_key("//pkg:consumer", role);
            add_configured_fixture(&mut graph, &owner, Box::new(DummyInstance { deps: vec![":required_test".into()] }));
            expected.insert(configured_fixture_key("//pkg:required_test", role));
            owners.push(owner);
        }
        // A legacy declaration must not fill either configured missing edge.
        graph.add_node(Label::parse("//pkg:required_test", None, None).unwrap(),
            Box::new(DummyInstance { deps: Vec::new() })).unwrap();
        assert_eq!(graph.unresolved_key_deps().into_iter().map(|(_, dep)| dep).collect::<HashSet<_>>(), expected);
        for owner in owners {
            assert!(graph.resolve_key_scope(&HashSet::from([owner.clone()])).unwrap_err().to_string().contains("required_test"),
                "configured missing edges cannot inherit legacy test/platform skip heuristics");
            assert!(graph.topological_sort_keys_from(&owner).unwrap_err().to_string().contains("missing exact key"));
        }
    }

    #[test]
    fn configured_graph_visibility_and_output_ownership_stay_in_exact_scope() {
        use crate::configuration_table::ConfigurationRole::{Exec, Target};
        let mut graph = DependencyGraph::new();
        let mut roots = Vec::new();
        for role in [Target, Exec] {
            let consumer = configured_fixture_key("//clients:consumer", role);
            let secret = configured_fixture_key("//owner:secret", role);
            let group = configured_fixture_key("//groups:friends", role);
            add_configured_fixture(&mut graph, &consumer, Box::new(DummyInstance { deps: vec!["//owner:secret".into()] }));
            add_configured_fixture(&mut graph, &secret, Box::new(DummyVisibilityInstance { visibility: vec!["//groups:friends".into()] }));
            add_configured_fixture(&mut graph, &group, Box::new(DummyPackageGroupInstance {
                name: "friends".into(), packages: vec![if role == Target { "//clients/...".into() } else { "//nobody/...".into() }], includes: Vec::new() }));
            roots.push(graph.get_reachable_keys(&consumer));
            let output = configured_fixture_key("//generated:producer", role);
            add_configured_fixture(&mut graph, &output, Box::new(DummyOutputInstance { outputs: vec!["result.h".into()] }));
        }
        graph.resolve_key_scope(&roots[0]).unwrap();
        assert!(graph.resolve_key_scope(&roots[1]).unwrap_err().to_string().contains("not visible"));
        let both: HashSet<_> = graph.all_keys().into_iter().collect();
        graph.validate_key_declared_output_ownership(&both).unwrap();
        let target_collision = configured_fixture_key("//generated:second", Target);
        add_configured_fixture(&mut graph, &target_collision, Box::new(DummyOutputInstance { outputs: vec!["./result.h".into()] }));
        assert!(graph.validate_key_declared_output_ownership(&HashSet::from([target_collision])).unwrap_err().to_string().contains("multiple owners"));
        graph.validate_key_declared_output_ownership(&HashSet::from([configured_fixture_key("//generated:producer", Exec)])).unwrap();
        graph.validate_declared_output_ownership().unwrap();
    }

    #[test]
    fn test_label_parsing_absolute() {
        let label = Label::parse("//foo/bar:baz", None, None).unwrap();
        assert_eq!(label.workspace, None);
        assert_eq!(label.package, "foo/bar");
        assert_eq!(label.target, "baz");
        assert_eq!(label.to_string(), "//foo/bar:baz");
    }

    #[test]
    fn test_label_parsing_relative() {
        let label = Label::parse(":baz", None, Some("foo/bar")).unwrap();
        assert_eq!(label.workspace, None);
        assert_eq!(label.package, "foo/bar");
        assert_eq!(label.target, "baz");
        assert_eq!(label.to_string(), "//foo/bar:baz");
    }

    #[test]
    fn test_label_parsing_relative_with_workspace() {
        let label = Label::parse(":baz", Some("external_repo"), Some("foo/bar")).unwrap();
        assert_eq!(label.workspace, Some("external_repo".to_string()));
        assert_eq!(label.package, "foo/bar");
        assert_eq!(label.target, "baz");
        assert_eq!(label.to_string(), "@external_repo//foo/bar:baz");
    }

    #[test]
    fn test_label_parsing_implicit_target() {
        let label = Label::parse("//foo/bar", None, None).unwrap();
        assert_eq!(label.package, "foo/bar");
        assert_eq!(label.target, "bar");
        assert_eq!(label.to_string(), "//foo/bar:bar");
    }

    #[test]
    fn test_label_parsing_external() {
        let label = Label::parse("@myrepo//foo:bar", None, None).unwrap();
        assert_eq!(label.workspace, Some("myrepo".to_string()));
        assert_eq!(label.package, "foo");
        assert_eq!(label.target, "bar");
        assert_eq!(label.to_string(), "@myrepo//foo:bar");
    }

    #[test]
    fn test_label_parsing_bare_external_repository_uses_root_target() {
        let shorthand = Label::parse("@myrepo", None, None).unwrap();
        let explicit = Label::parse("@myrepo//:myrepo", None, None).unwrap();

        assert_eq!(shorthand, explicit);
        assert_eq!(shorthand.workspace, Some("myrepo".to_string()));
        assert_eq!(shorthand.package, "");
        assert_eq!(shorthand.target, "myrepo");
        assert_eq!(shorthand.to_string(), "@myrepo//:myrepo");

        let canonical = Label::parse("@@myrepo", None, None).unwrap();
        assert_eq!(canonical, explicit);
        assert!(Label::parse("@", None, None).is_err());
    }

    #[test]
    fn test_label_parsing_absolute_with_workspace_context() {
        let label = Label::parse("//foo:bar", Some("myrepo"), None).unwrap();
        assert_eq!(label.workspace, Some("myrepo".to_string()));
        assert_eq!(label.package, "foo");
        assert_eq!(label.target, "bar");
        assert_eq!(label.to_string(), "@myrepo//foo:bar");
    }

    #[test]
    fn test_label_relative_without_context() {
        let result = Label::parse(":baz", None, None);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("without current package context"));
    }

    #[test]
    fn test_dependency_graph_simple() {
        // This would need mock RuleInstance implementations to test properly
        // For now, just test graph creation
        let graph = DependencyGraph::new();
        assert_eq!(graph.len(), 0);
        assert!(graph.is_empty());
    }

    #[test]
    fn bare_external_repository_dependency_resolves_to_root_target() {
        let mut graph = DependencyGraph::new();
        let repository_root = Label::parse("@repo//:repo", None, None).unwrap();
        graph
            .add_node(
                repository_root.clone(),
                Box::new(DummyVisibilityInstance {
                    visibility: vec!["//visibility:public".to_string()],
                }),
            )
            .unwrap();

        let consumer = Label::parse("//pkg:consumer", None, None).unwrap();
        graph
            .add_node(
                consumer.clone(),
                Box::new(DummyInstance {
                    deps: vec!["@repo".to_string()],
                }),
            )
            .unwrap();
        graph.resolve().unwrap();

        assert_eq!(
            graph.get(&consumer).unwrap().dependencies,
            vec![repository_root]
        );
    }

    #[test]
    fn py_library_bare_data_target_orders_before_wrapper() {
        let mut graph = DependencyGraph::new();
        let extension = Label::parse("//pkg:binding.so", None, None).unwrap();
        let extension_attrs = HashMap::from([
            (
                "name".to_string(),
                Value::String("binding.so".to_string()),
            ),
            ("linkshared".to_string(), Value::Int(1)),
        ]);
        graph
            .add_node(
                extension.clone(),
                CcBinaryRule.parse_attributes(&extension_attrs).unwrap(),
            )
            .unwrap();

        let attrs = HashMap::from([
            ("name".to_string(), Value::String("binding".to_string())),
            (
                "data".to_string(),
                Value::List(vec![Value::String("binding.so".to_string())]),
            ),
        ]);
        let wrapper = Label::parse("//pkg:binding", None, None).unwrap();
        let instance = PyLibraryRule.parse_attributes(&attrs).unwrap();
        graph.add_node(wrapper.clone(), instance).unwrap();
        graph.resolve().unwrap();

        assert_eq!(
            graph.get(&wrapper).unwrap().dependencies,
            vec![extension.clone()]
        );
        let order = graph.topological_sort().unwrap();
        let extension_index = order.iter().position(|label| label == &extension).unwrap();
        let wrapper_index = order.iter().position(|label| label == &wrapper).unwrap();
        assert!(extension_index < wrapper_index);
    }

    #[test]
    fn declared_output_ownership_rejects_same_package_collision() {
        let mut graph = DependencyGraph::new();
        let first = Label::parse("//pkg:first", None, None).unwrap();
        let second = Label::parse("//pkg:second", None, None).unwrap();
        graph
            .add_node(
                first,
                Box::new(DummyOutputInstance {
                    outputs: vec!["nested/shared.txt".to_string()],
                }),
            )
            .unwrap();
        graph
            .add_node(
                second,
                Box::new(DummyOutputInstance {
                    outputs: vec!["nested/./shared.txt".to_string()],
                }),
            )
            .unwrap();

        let error = graph.validate_declared_output_ownership().unwrap_err();
        let message = error.to_string();
        assert!(message.contains("//pkg:nested/shared.txt"), "{message}");
        assert!(message.contains("//pkg:first"), "{message}");
        assert!(message.contains("//pkg:second"), "{message}");
    }

    #[test]
    fn declared_output_ownership_allows_distinct_physical_paths() {
        let mut graph = DependencyGraph::new();
        for label in ["//one:first", "//two:second", "@repo//three:third"] {
            graph
                .add_node(
                    Label::parse(label, None, None).unwrap(),
                    Box::new(DummyOutputInstance {
                        outputs: vec!["nested/shared.txt".to_string()],
                    }),
                )
                .unwrap();
        }
        graph
            .add_node(
                Label::parse("//one:unique", None, None).unwrap(),
                Box::new(DummyOutputInstance {
                    outputs: vec![
                        "nested/other.txt".to_string(),
                        "other/nested.txt".to_string(),
                    ],
                }),
            )
            .unwrap();

        graph.validate_declared_output_ownership().unwrap();
    }

    #[test]
    fn declared_output_ownership_allows_distinct_repositories() {
        let mut graph = DependencyGraph::new();
        for label in ["//pkg:main", "@repo//pkg:external"] {
            graph
                .add_node(
                    Label::parse(label, None, None).unwrap(),
                    Box::new(DummyOutputInstance {
                        outputs: vec!["shared.txt".to_string()],
                    }),
                )
                .unwrap();
        }

        graph.validate_declared_output_ownership().unwrap();
    }

    #[test]
    fn declared_output_ownership_canonicalizes_main_repository_alias() {
        let aliases = HashSet::from(["self".to_string()]);
        let mut graph = DependencyGraph::with_main_repository_aliases(aliases);
        for label in ["//pkg:main", "@self//pkg:alias"] {
            graph
                .add_node(
                    Label::parse(label, None, None).unwrap(),
                    Box::new(DummyOutputInstance {
                        outputs: vec!["shared.txt".to_string()],
                    }),
                )
                .unwrap();
        }

        let error = graph.validate_declared_output_ownership().unwrap_err();
        let message = error.to_string();
        assert!(message.contains("//pkg:shared.txt"), "{message}");
        assert!(message.contains("//pkg:main"), "{message}");
        assert!(message.contains("//pkg:alias"), "{message}");
    }

    #[test]
    fn main_repository_alias_is_one_graph_target() {
        let aliases = HashSet::from(["self".to_string()]);
        let mut graph = DependencyGraph::with_main_repository_aliases(aliases);
        for label in ["//pkg:same", "@self//pkg:same"] {
            graph
                .add_node(
                    Label::parse(label, None, None).unwrap(),
                    Box::new(DummyOutputInstance {
                        outputs: vec!["shared.txt".to_string()],
                    }),
                )
                .unwrap();
        }

        assert_eq!(graph.len(), 1);
        graph.validate_declared_output_ownership().unwrap();
        assert!(graph.has_node(&Label::parse("//pkg:same", None, None).unwrap()));
    }

    #[test]
    fn main_repository_alias_precedes_tensorflow_repository_remaps() {
        for (workspace_name, dependency) in [
            ("local_xla", "@local_xla//tsl/foo:dep"),
            ("local_tsl", "//tsl/foo:dep"),
        ] {
            let aliases = HashSet::from([workspace_name.to_string()]);
            let mut graph = DependencyGraph::with_main_repository_aliases(aliases);
            let owner = Label::parse("//pkg:owner", None, None).unwrap();
            graph
                .add_node(
                    owner.clone(),
                    Box::new(DummyInstance {
                        deps: vec![dependency.to_string()],
                    }),
                )
                .unwrap();

            let node = graph.get(&owner).unwrap();
            assert_eq!(
                node.dependencies,
                vec![Label::parse("//tsl/foo:dep", None, None).unwrap()]
            );
        }
    }

    #[test]
    fn explicit_main_repository_spellings_precede_tensorflow_remaps() {
        let mut graph = DependencyGraph::new();
        let owner = Label::parse("//pkg:owner", None, None).unwrap();
        graph
            .add_node(
                owner.clone(),
                Box::new(DummyInstance {
                    deps: vec![
                        "@//tsl/foo:explicit_main".to_string(),
                        "@@//xla/foo:explicit_main".to_string(),
                        "//tsl/foo:external".to_string(),
                        "//xla/foo:external".to_string(),
                    ],
                }),
            )
            .unwrap();

        assert_eq!(
            graph.get(&owner).unwrap().dependencies,
            vec![
                Label::parse("//tsl/foo:explicit_main", None, None).unwrap(),
                Label::parse("//xla/foo:explicit_main", None, None).unwrap(),
                Label::parse("@local_tsl//tsl/foo:external", None, None).unwrap(),
                Label::parse("@local_xla//xla/foo:external", None, None).unwrap(),
            ]
        );
    }

    #[test]
    fn absolute_labels_inherit_external_repository_context() {
        let mut graph = DependencyGraph::new();
        let owner = Label::parse("@repo//pkg:owner", None, None).unwrap();
        graph
            .add_node(
                owner.clone(),
                Box::new(DummyInstance {
                    deps: vec!["//q:missing".to_string()],
                }),
            )
            .unwrap();

        assert_eq!(
            graph.get(&owner).unwrap().dependencies,
            vec![Label::parse("@repo//q:missing", None, None).unwrap()]
        );
    }

    #[test]
    fn reserved_absolute_packages_use_main_repository_identity() {
        for package in ["visibility", "external"] {
            let raw = format!("//{package}:target");
            let parsed = Label::parse(&raw, Some("ext"), Some("pkg")).unwrap();
            assert_eq!(parsed.workspace, None, "{raw}");

            let explicit = format!("@ext//{package}:target");
            let parsed =
                Label::parse(&explicit, Some("other"), Some("pkg")).unwrap();
            assert_eq!(parsed.workspace.as_deref(), Some("ext"), "{explicit}");
        }

        let named_condition =
            Label::parse("//conditions:linux", Some("ext"), Some("pkg"))
                .unwrap();
        assert_eq!(named_condition.workspace.as_deref(), Some("ext"));

        let default_condition = Label::parse(
            "//conditions:default",
            Some("ext"),
            Some("pkg"),
        )
        .unwrap();
        assert_eq!(default_condition.workspace, None);
    }

    #[test]
    fn canonical_public_visibility_spellings_do_not_load_groups() {
        for visibility in [
            "//visibility:public",
            "@//visibility:public",
            "@@//visibility:public",
            "@self//visibility:public",
        ] {
            let aliases = HashSet::from(["self".to_string()]);
            let mut graph =
                DependencyGraph::with_main_repository_aliases(aliases);
            let target = Label::parse("//lib:target", None, None).unwrap();
            graph
                .add_node(
                    target.clone(),
                    Box::new(DummyVisibilityInstance {
                        visibility: vec![visibility.to_string()],
                    }),
                )
                .unwrap();
            graph
                .add_node(
                    Label::parse("//app:consumer", None, None).unwrap(),
                    Box::new(DummyInstance {
                        deps: vec![target.to_string()],
                    }),
                )
                .unwrap();

            assert!(graph
                .get(&target)
                .unwrap()
                .package_group_references
                .is_empty());
            graph.resolve().unwrap();
        }

        let mut external = DependencyGraph::new();
        let target = Label::parse("@ext//lib:target", None, None).unwrap();
        external
            .add_node(
                target.clone(),
                Box::new(DummyVisibilityInstance {
                    visibility: vec!["//visibility:public".to_string()],
                }),
            )
            .unwrap();
        external
            .add_node(
                Label::parse("@other//app:consumer", None, None).unwrap(),
                Box::new(DummyInstance {
                    deps: vec![target.to_string()],
                }),
            )
            .unwrap();
        external.resolve().unwrap();
    }

    #[test]
    fn canonical_private_visibility_spellings_remain_private() {
        for visibility in [
            "@//visibility:private",
            "@@//visibility:private",
            "@self//visibility:private",
        ] {
            let aliases = HashSet::from(["self".to_string()]);
            let mut graph =
                DependencyGraph::with_main_repository_aliases(aliases);
            let target = Label::parse("//lib:target", None, None).unwrap();
            graph
                .add_node(
                    target.clone(),
                    Box::new(DummyVisibilityInstance {
                        visibility: vec![visibility.to_string()],
                    }),
                )
                .unwrap();
            graph
                .add_node(
                    Label::parse("//app:consumer", None, None).unwrap(),
                    Box::new(DummyInstance {
                        deps: vec![target.to_string()],
                    }),
                )
                .unwrap();

            let error = graph.resolve().unwrap_err().to_string();
            assert!(error.contains("private visibility"), "{error}");
        }
    }

    #[test]
    fn bare_visibility_names_are_relative_package_groups() {
        for group_name in ["public", "private"] {
            let mut graph = DependencyGraph::new();
            add_package_group(
                &mut graph,
                &format!("//lib:{group_name}"),
                &["//app"],
                &[],
            );
            let target = Label::parse("//lib:target", None, None).unwrap();
            graph
                .add_node(
                    target.clone(),
                    Box::new(DummyVisibilityInstance {
                        visibility: vec![group_name.to_string()],
                    }),
                )
                .unwrap();
            graph
                .add_node(
                    Label::parse("//app:consumer", None, None).unwrap(),
                    Box::new(DummyInstance {
                        deps: vec![target.to_string()],
                    }),
                )
                .unwrap();

            assert_eq!(
                graph
                    .get(&target)
                    .unwrap()
                    .package_group_references,
                vec![Label::parse(
                    &format!("//lib:{group_name}"),
                    None,
                    None,
                )
                .unwrap()]
            );
            graph.resolve().unwrap();
        }
    }

    #[test]
    fn visibility_sentinels_cannot_be_combined_with_other_specs() {
        for sentinel in [
            "//visibility:public",
            "@//visibility:private",
            "@@//visibility:public",
            "@self//visibility:private",
        ] {
            let aliases = HashSet::from(["self".to_string()]);
            let mut graph =
                DependencyGraph::with_main_repository_aliases(aliases);
            let error = graph
                .add_node(
                    Label::parse("//lib:target", None, None).unwrap(),
                    Box::new(DummyVisibilityInstance {
                        visibility: vec![
                            sentinel.to_string(),
                            "//groups:friends".to_string(),
                        ],
                    }),
                )
                .unwrap_err()
                .to_string();
            assert!(error.contains("cannot combine"), "{error}");
        }
    }

    #[test]
    fn explicit_private_visibility_rejects_cross_package_dependency() {
        let mut graph = DependencyGraph::new();
        let secret = Label::parse("//lib:secret", None, None).unwrap();
        let app = Label::parse("//app:app", None, None).unwrap();
        graph
            .add_node(
                secret.clone(),
                Box::new(DummyVisibilityInstance {
                    visibility: vec!["//visibility:private".to_string()],
                }),
            )
            .unwrap();
        graph
            .add_node(
                app.clone(),
                Box::new(DummyInstance {
                    deps: vec![secret.to_string()],
                }),
            )
            .unwrap();

        let error = graph.resolve().unwrap_err().to_string();
        assert!(error.contains("//app:app"), "{error}");
        assert!(error.contains("//lib:secret"), "{error}");
        assert!(error.contains("private visibility"), "{error}");
    }

    #[test]
    fn explicit_private_visibility_allows_only_same_repository_package() {
        let private = "//visibility:private".to_string();

        let mut same_package = DependencyGraph::new();
        let secret = Label::parse("@repo//lib:secret", None, None).unwrap();
        same_package
            .add_node(
                secret.clone(),
                Box::new(DummyVisibilityInstance {
                    visibility: vec![private.clone()],
                }),
            )
            .unwrap();
        same_package
            .add_node(
                Label::parse("@repo//lib:friend", None, None).unwrap(),
                Box::new(DummyInstance {
                    deps: vec![secret.to_string()],
                }),
            )
            .unwrap();
        same_package.resolve().unwrap();

        let mut different_repository = DependencyGraph::new();
        different_repository
            .add_node(
                secret.clone(),
                Box::new(DummyVisibilityInstance {
                    visibility: vec![private],
                }),
            )
            .unwrap();
        different_repository
            .add_node(
                Label::parse("//lib:main", None, None).unwrap(),
                Box::new(DummyInstance {
                    deps: vec![secret.to_string()],
                }),
            )
            .unwrap();
        let error = different_repository.resolve().unwrap_err().to_string();
        assert!(error.contains("private visibility"), "{error}");
    }

    #[test]
    fn visibility_options_parse_bazel_boolean_spellings_in_order() {
        let argv = vec![
            bytes::Bytes::from_static(b"rush"),
            bytes::Bytes::from_static(
                b"--noincompatible_enforce_config_setting_visibility",
            ),
            bytes::Bytes::from_static(
                b"--incompatible_enforce_config_setting_visibility=true",
            ),
            bytes::Bytes::from_static(
                b"--incompatible_config_setting_private_default_visibility",
            ),
            bytes::Bytes::from_static(
                b"--incompatible_package_group_has_public_syntax=false",
            ),
            bytes::Bytes::from_static(
                b"--noincompatible_fix_package_group_reporoot_syntax",
            ),
        ];
        let options = VisibilityOptions::from_argv(&argv).unwrap();
        assert!(options.enforce_config_setting_visibility);
        assert!(options.config_setting_private_default_visibility);
        assert!(!options.package_group_has_public_syntax);
        assert!(!options.fix_package_group_reporoot_syntax);
    }

    #[test]
    fn visibility_options_ignore_arguments_after_terminator() {
        let argv = vec![
            bytes::Bytes::from_static(b"rush"),
            bytes::Bytes::from_static(
                b"--noincompatible_enforce_config_setting_visibility",
            ),
            bytes::Bytes::from_static(b"--"),
            bytes::Bytes::from_static(
                b"--incompatible_enforce_config_setting_visibility",
            ),
        ];
        let options = VisibilityOptions::from_argv(&argv).unwrap();
        assert!(!options.enforce_config_setting_visibility);
    }

    #[test]
    fn visibility_options_accept_only_bazel_boolean_values() {
        for raw_value in [
            "true", "TRUE", "1", "yes", "Yes", "t", "T", "y", "Y",
            "false", "FALSE", "0", "no", "No", "f", "F", "n", "N",
        ] {
            let argument = bytes::Bytes::from(format!(
                "--incompatible_enforce_config_setting_visibility={raw_value}"
            ));
            VisibilityOptions::from_argv(&[argument]).unwrap();
        }

        for raw_value in ["on", "off", "enabled", ""] {
            let argument = bytes::Bytes::from(format!(
                "--incompatible_enforce_config_setting_visibility={raw_value}"
            ));
            let error = VisibilityOptions::from_argv(&[argument])
                .unwrap_err()
                .to_string();
            assert!(error.contains("expects a boolean"), "{error}");
        }
    }

    #[test]
    fn daemon_command_semantics_apply_bazelrc_before_client_argv() {
        use rush_protocol::command::v1::{
            EnvironmentVariable, RunRequest,
        };

        let request = RunRequest {
            auth_cookie: bytes::Bytes::new(),
            argv: vec![
                bytes::Bytes::from_static(b"rush"),
                bytes::Bytes::from_static(b"build"),
                bytes::Bytes::from_static(
                    b"--incompatible_enforce_config_setting_visibility=false",
                ),
                bytes::Bytes::from_static(b"--config=strict"),
            ],
            working_directory: bytes::Bytes::from_static(b"/workspace"),
            environment: vec![EnvironmentVariable {
                name: bytes::Bytes::from_static(
                    b"RUSH_EFFECTIVE_VISIBILITY_FLAGS",
                ),
                value: bytes::Bytes::from_static(
                    b"--noincompatible_enforce_config_setting_visibility\n\
                      --incompatible_enforce_config_setting_visibility\n\
                      --noincompatible_package_group_has_public_syntax",
                ),
            }],
            block_for_lock: true,
            client_build_id: "test-client".to_string(),
            protocol_major: rush_protocol::PROTOCOL_MAJOR,
            protocol_minor: rush_protocol::PROTOCOL_MINOR,
            workspace_config: Vec::new(),
        };
        let semantics =
            rush_command_core::CommandSemantics::from_run_request(&request)
                .unwrap();
        let options =
            VisibilityOptions::from_command_semantics(&semantics).unwrap();
        assert!(options.enforce_config_setting_visibility);
        assert!(!options.package_group_has_public_syntax);
    }

    #[test]
    fn config_setting_select_edges_follow_bazel_6_visibility_modes() {
        let private = Some(vec!["//visibility:private".to_string()]);

        let mut default_mode = DependencyGraph::new();
        add_config_setting(
            &mut default_mode,
            "//conditions:explicit_private",
            private.clone(),
            true,
        );
        add_select_consumer(
            &mut default_mode,
            "//consumer:target",
            &["//conditions:explicit_private"],
        );
        let error = default_mode.resolve().unwrap_err().to_string();
        assert!(error.contains("private visibility"), "{error}");

        let mut enforcement_off = DependencyGraph::new();
        enforcement_off
            .visibility_options
            .enforce_config_setting_visibility = false;
        add_config_setting(
            &mut enforcement_off,
            "//conditions:explicit_private",
            private.clone(),
            true,
        );
        add_select_consumer(
            &mut enforcement_off,
            "//consumer:target",
            &["//conditions:explicit_private"],
        );
        enforcement_off.resolve().unwrap();

        let mut migration_default = DependencyGraph::new();
        add_config_setting(
            &mut migration_default,
            "//conditions:implicit",
            private.clone(),
            false,
        );
        add_select_consumer(
            &mut migration_default,
            "//consumer:target",
            &["//conditions:implicit"],
        );
        migration_default.resolve().unwrap();

        let mut strict_default = DependencyGraph::new();
        strict_default
            .visibility_options
            .config_setting_private_default_visibility = true;
        add_config_setting(
            &mut strict_default,
            "//conditions:implicit",
            private,
            false,
        );
        add_select_consumer(
            &mut strict_default,
            "//consumer:target",
            &["//conditions:implicit"],
        );
        let error = strict_default.resolve().unwrap_err().to_string();
        assert!(error.contains("private visibility"), "{error}");
    }

    #[test]
    fn select_constraint_visibility_obeys_the_enforcement_flag() {
        let private = Some(vec!["//visibility:private".to_string()]);

        let mut default_mode = DependencyGraph::new();
        add_constraint_value(
            &mut default_mode,
            "//conditions:constraint_private",
            private.clone(),
        );
        add_select_consumer(
            &mut default_mode,
            "//consumer:target",
            &["//conditions:constraint_private"],
        );
        let error = default_mode.resolve().unwrap_err().to_string();
        assert!(error.contains("private visibility"), "{error}");

        let mut enforcement_off = DependencyGraph::new();
        enforcement_off
            .visibility_options
            .enforce_config_setting_visibility = false;
        add_constraint_value(
            &mut enforcement_off,
            "//conditions:constraint_private",
            private,
        );
        add_select_consumer(
            &mut enforcement_off,
            "//consumer:target",
            &["//conditions:constraint_private"],
        );
        enforcement_off.resolve().unwrap();

        let mut alias_default_mode = DependencyGraph::new();
        add_constraint_value(
            &mut alias_default_mode,
            "//conditions:constraint_private",
            Some(vec!["//visibility:private".to_string()]),
        );
        add_alias(
            &mut alias_default_mode,
            "//conditions:public_alias",
            "//conditions:constraint_private",
            Some(vec!["//visibility:public".to_string()]),
        );
        add_select_consumer(
            &mut alias_default_mode,
            "//consumer:target",
            &["//conditions:public_alias"],
        );
        let error = alias_default_mode.resolve().unwrap_err().to_string();
        assert!(error.contains("private visibility"), "{error}");

        let mut alias_enforcement_off = DependencyGraph::new();
        alias_enforcement_off
            .visibility_options
            .enforce_config_setting_visibility = false;
        add_constraint_value(
            &mut alias_enforcement_off,
            "//conditions:constraint_private",
            Some(vec!["//visibility:private".to_string()]),
        );
        add_alias(
            &mut alias_enforcement_off,
            "//conditions:public_alias",
            "//conditions:constraint_private",
            Some(vec!["//visibility:public".to_string()]),
        );
        add_select_consumer(
            &mut alias_enforcement_off,
            "//consumer:target",
            &["//conditions:public_alias"],
        );
        alias_enforcement_off.resolve().unwrap();
    }

    #[test]
    fn select_conditions_are_lazy_metadata_edges_not_build_dependencies() {
        let mut graph = DependencyGraph::new();
        let consumer = Label::parse("//consumer:target", None, None).unwrap();
        let condition = Label::parse("//conditions:mode", None, None).unwrap();
        add_select_consumer(
            &mut graph,
            "//consumer:target",
            &["//conditions:mode"],
        );

        let node = graph.get(&consumer).unwrap();
        assert!(node.dependencies.is_empty());
        assert_eq!(node.configuration_references, vec![condition.clone()]);
        assert_eq!(graph.unresolved_deps(), vec![(consumer, condition)]);
    }

    #[test]
    fn select_condition_package_group_is_discovered_as_metadata() {
        let mut graph = DependencyGraph::new();
        add_config_setting(
            &mut graph,
            "//conditions:group_visible",
            Some(vec!["//groups:friends".to_string()]),
            true,
        );
        add_select_consumer(
            &mut graph,
            "//consumer:target",
            &["//conditions:group_visible"],
        );

        let consumer = Label::parse("//consumer:target", None, None).unwrap();
        let condition =
            Label::parse("//conditions:group_visible", None, None).unwrap();
        let group = Label::parse("//groups:friends", None, None).unwrap();
        let scope = HashSet::from([consumer, condition.clone()]);
        assert_eq!(
            graph.find_all_missing_in_scope(&scope),
            vec![(condition, group)]
        );

        add_package_group(&mut graph, "//groups:friends", &["//consumer"], &[]);
        graph.resolve().unwrap();
    }

    #[test]
    fn implicit_config_setting_does_not_load_ignored_package_default() {
        let mut graph = DependencyGraph::new();
        add_config_setting(
            &mut graph,
            "//conditions:implicit",
            Some(vec!["//groups:missing".to_string()]),
            false,
        );
        add_select_consumer(
            &mut graph,
            "//consumer:target",
            &["//conditions:implicit"],
        );

        let consumer = Label::parse("//consumer:target", None, None).unwrap();
        let scope = HashSet::from([consumer]);
        assert!(graph.find_all_missing_in_scope(&scope).is_empty());
        graph.resolve().unwrap();

        graph
            .visibility_options
            .config_setting_private_default_visibility = true;
        let condition =
            Label::parse("//conditions:implicit", None, None).unwrap();
        let group = Label::parse("//groups:missing", None, None).unwrap();
        assert_eq!(
            graph.find_all_missing_in_scope(&scope),
            vec![(condition, group)]
        );
    }

    #[test]
    fn select_aliases_follow_bazel_visibility_policies() {
        fn graph(
            alias_visibility: &str,
            condition_visibility: &str,
            condition_was_explicit: bool,
        ) -> (DependencyGraph, HashSet<Label>) {
            let mut graph = DependencyGraph::new();
            add_config_setting(
                &mut graph,
                "//conditions:mode",
                Some(vec![condition_visibility.to_string()]),
                condition_was_explicit,
            );
            add_alias(
                &mut graph,
                "//aliases:mode",
                "//conditions:mode",
                Some(vec![alias_visibility.to_string()]),
            );
            add_select_consumer(
                &mut graph,
                "//consumer:target",
                &["//aliases:mode"],
            );
            let scope = HashSet::from([
                Label::parse("//consumer:target", None, None).unwrap(),
            ]);
            (graph, scope)
        }

        let (mut migration, scope) = graph(
            "//visibility:private",
            "//visibility:private",
            false,
        );
        migration.resolve_within_scope(&scope).unwrap();

        let (mut strict_alias, scope) = graph(
            "//visibility:private",
            "//visibility:public",
            true,
        );
        strict_alias
            .visibility_options
            .config_setting_private_default_visibility = true;
        let error = strict_alias
            .resolve_within_scope(&scope)
            .unwrap_err()
            .to_string();
        assert!(error.contains("private visibility"), "{error}");

        // In the migration/default mode, select() visibility belongs to the
        // terminal condition. A private alias may therefore expose a public
        // config_setting as a key; the alias's own visibility is ignored.
        let (mut private_alias_public_condition, scope) = graph(
            "//visibility:private",
            "//visibility:public",
            true,
        );
        private_alias_public_condition
            .resolve_within_scope(&scope)
            .unwrap();

        let (mut explicit_private, scope) = graph(
            "//visibility:public",
            "//visibility:private",
            true,
        );
        let error = explicit_private
            .resolve_within_scope(&scope)
            .unwrap_err()
            .to_string();
        assert!(error.contains("private visibility"), "{error}");

        let (mut enforcement_off, scope) = graph(
            "//visibility:private",
            "//visibility:public",
            true,
        );
        enforcement_off
            .visibility_options
            .enforce_config_setting_visibility = false;
        enforcement_off.resolve_within_scope(&scope).unwrap();

        let (mut off_uses_ordinary_default, scope) = graph(
            "//visibility:public",
            "//visibility:private",
            false,
        );
        off_uses_ordinary_default
            .visibility_options
            .enforce_config_setting_visibility = false;
        off_uses_ordinary_default
            .resolve_within_scope(&scope)
            .unwrap();
    }

    #[test]
    fn missing_dependency_batch_retains_exact_configuration_alias_provenance() {
        let mut graph = DependencyGraph::new();
        add_select_consumer(&mut graph, "//consumer:target", &["//aliases:mode"]);
        add_alias(&mut graph, "//aliases:mode", "@generated//:condition", None);
        let consumer = Label::parse("//consumer:target", None, None).unwrap();
        let ordinary = Label::parse("@generated//:library", None, None).unwrap();
        graph.add_node(Label::parse("//consumer:ordinary", None, None).unwrap(), Box::new(DummyInstance {
            deps: vec![ordinary.to_string()],
        })).unwrap();
        let scope = HashSet::from([consumer, Label::parse("//consumer:ordinary", None, None).unwrap()]);
        let (missing, configuration) = graph.find_all_missing_in_scope_with_configuration(&scope);
        let condition = Label::parse("@generated//:condition", None, None).unwrap();
        assert_eq!(configuration, HashMap::from([(
            condition.clone(), Label::parse("//aliases:mode", None, None).unwrap(),
        )]));
        let missing_labels: HashSet<_> = missing.into_iter().map(|(_, label)| label).collect();
        assert_eq!(missing_labels, HashSet::from([condition, ordinary]));
    }

    #[test]
    fn select_alias_actual_is_lazy_metadata_and_must_be_a_condition() {
        let mut graph = DependencyGraph::new();
        add_alias(
            &mut graph,
            "//aliases:mode",
            "//conditions:mode",
            Some(vec!["//visibility:public".to_string()]),
        );
        add_select_consumer(
            &mut graph,
            "//consumer:target",
            &["//aliases:mode"],
        );
        let consumer = Label::parse("//consumer:target", None, None).unwrap();
        let alias = Label::parse("//aliases:mode", None, None).unwrap();
        let condition = Label::parse("//conditions:mode", None, None).unwrap();
        let scope = HashSet::from([consumer]);
        assert_eq!(
            graph.find_all_missing_in_scope(&scope),
            vec![(alias, condition.clone())]
        );

        graph
            .add_node(
                condition,
                rush_rules::with_rule_metadata(
                    Box::new(DummyInstance { deps: Vec::new() }),
                    Some(vec!["//visibility:public".to_string()]),
                    true,
                    Vec::new(),
                ),
            )
            .unwrap();
        for enforce in [true, false] {
            graph
                .visibility_options
                .enforce_config_setting_visibility = enforce;
            let error = graph
                .resolve_within_scope(&scope)
                .unwrap_err()
                .to_string();
            assert!(error.contains("not a config_setting"), "{error}");
        }
    }

    #[test]
    fn configurable_select_alias_metadata_is_load_order_independent() {
        let public = Some(vec!["//visibility:public".to_string()]);

        let mut eager = DependencyGraph::new();
        add_configurable_alias(
            &mut eager,
            "//aliases:mode",
            "//conditions:chosen",
            public.clone(),
            &["//conditions:selector"],
        );
        add_config_setting(
            &mut eager,
            "//conditions:chosen",
            public.clone(),
            true,
        );
        add_select_consumer(
            &mut eager,
            "//consumer:target",
            &["//aliases:mode"],
        );
        let consumer = Label::parse("//consumer:target", None, None).unwrap();
        let alias = Label::parse("//aliases:mode", None, None).unwrap();
        let selector =
            Label::parse("//conditions:selector", None, None).unwrap();
        let scope = HashSet::from([consumer.clone()]);
        assert_eq!(
            eager.find_all_missing_in_scope(&scope),
            vec![(alias.clone(), selector.clone())]
        );
        add_config_setting(
            &mut eager,
            "//conditions:selector",
            public.clone(),
            true,
        );
        eager.resolve_within_scope(&scope).unwrap();

        let mut lazy = DependencyGraph::new();
        add_select_consumer(
            &mut lazy,
            "//consumer:target",
            &["//aliases:mode"],
        );
        assert_eq!(
            lazy.find_all_missing_in_scope(&scope),
            vec![(consumer, alias.clone())]
        );
        add_configurable_alias(
            &mut lazy,
            "//aliases:mode",
            "//conditions:chosen",
            public.clone(),
            &["//conditions:selector"],
        );
        let chosen =
            Label::parse("//conditions:chosen", None, None).unwrap();
        assert_eq!(
            lazy.find_all_missing_in_scope(&scope),
            vec![(alias.clone(), chosen), (alias.clone(), selector)]
        );
        add_config_setting(
            &mut lazy,
            "//conditions:chosen",
            public.clone(),
            true,
        );
        add_config_setting(
            &mut lazy,
            "//conditions:selector",
            public,
            true,
        );
        lazy.resolve_within_scope(&scope).unwrap();
    }

    #[test]
    fn configurable_select_alias_nested_keys_are_type_checked() {
        let mut graph = DependencyGraph::new();
        add_configurable_alias(
            &mut graph,
            "//aliases:mode",
            "//conditions:chosen",
            Some(vec!["//visibility:public".to_string()]),
            &["//conditions:not_a_condition"],
        );
        add_config_setting(
            &mut graph,
            "//conditions:chosen",
            Some(vec!["//visibility:public".to_string()]),
            true,
        );
        graph
            .add_node(
                Label::parse(
                    "//conditions:not_a_condition",
                    None,
                    None,
                )
                .unwrap(),
                Box::new(DummyInstance { deps: Vec::new() }),
            )
            .unwrap();
        add_select_consumer(
            &mut graph,
            "//consumer:target",
            &["//aliases:mode"],
        );
        let scope = HashSet::from([
            Label::parse("//consumer:target", None, None).unwrap(),
        ]);
        let error = graph
            .resolve_within_scope(&scope)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a config_setting"), "{error}");
    }

    #[test]
    fn native_configuration_condition_capability_is_narrow() {
        let public = Some(vec!["//visibility:public".to_string()]);

        let mut constraint_graph = DependencyGraph::new();
        let constraint_attrs = HashMap::from([
            (
                "name".to_string(),
                Value::String("linux".to_string()),
            ),
            (
                "constraint_setting".to_string(),
                Value::String(":os".to_string()),
            ),
        ]);
        let constraint = rush_rules::rules_config::ConfigNounRule {
            rule_type: "constraint_value",
        }
        .parse_attributes(&constraint_attrs)
        .unwrap();
        constraint_graph
            .add_node(
                Label::parse("//platform_defs:os", None, None).unwrap(),
                rush_rules::rules_config::ConfigNounRule {
                    rule_type: "constraint_setting",
                }
                .parse_attributes(&HashMap::from([(
                    "name".to_string(),
                    Value::String("os".to_string()),
                )]))
                .unwrap(),
            )
            .unwrap();
        constraint_graph
            .add_node(
                Label::parse("//platform_defs:linux", None, None).unwrap(),
                rush_rules::with_rule_metadata(
                    constraint,
                    public.clone(),
                    true,
                    Vec::new(),
                ),
            )
            .unwrap();
        add_select_consumer(
            &mut constraint_graph,
            "//consumer:target",
            &["//platform_defs:linux"],
        );
        constraint_graph.resolve().unwrap();

        let mut label_flag_graph = DependencyGraph::new();
        let label_flag_attrs = HashMap::from([
            (
                "name".to_string(),
                Value::String("not_a_condition".to_string()),
            ),
            (
                "build_setting_default".to_string(),
                Value::String("//conditions:default_value".to_string()),
            ),
        ]);
        let label_flag = rush_rules::rules_config::LabelFlagRule
            .parse_attributes(&label_flag_attrs)
            .unwrap();
        label_flag_graph
            .add_node(
                Label::parse("//conditions:not_a_condition", None, None)
                    .unwrap(),
                rush_rules::with_rule_metadata(
                    label_flag,
                    public,
                    true,
                    Vec::new(),
                ),
            )
            .unwrap();
        label_flag_graph
            .add_node(
                Label::parse(
                    "//conditions:default_value",
                    None,
                    None,
                )
                .unwrap(),
                Box::new(DummyInstance { deps: Vec::new() }),
            )
            .unwrap();
        add_select_consumer(
            &mut label_flag_graph,
            "//consumer:target",
            &["//conditions:not_a_condition"],
        );
        let error = label_flag_graph.resolve().unwrap_err().to_string();
        assert!(error.contains("not a config_setting"), "{error}");
    }

    #[test]
    fn only_main_default_is_a_reserved_select_key() {
        let mut graph = DependencyGraph::new();
        add_select_consumer(
            &mut graph,
            "//consumer:target",
            &[
                "//conditions:linux",
                "@platforms//os:linux",
                "@platforms//evil:ordinary",
                "//conditions:default",
                "@//conditions:default",
                "@@//conditions:default",
            ],
        );
        let consumer = Label::parse("//consumer:target", None, None).unwrap();
        let scope = HashSet::from([consumer.clone()]);
        assert_eq!(
            graph.find_all_missing_in_scope(&scope),
            vec![
                (
                    consumer.clone(),
                    Label::parse("//conditions:linux", None, None)
                        .unwrap(),
                ),
                (
                    consumer.clone(),
                    Label::parse("@platforms//evil:ordinary", None, None)
                        .unwrap(),
                ),
                (
                    consumer,
                    Label::parse("@platforms//os:linux", None, None)
                        .unwrap(),
                ),
            ]
        );
    }

    #[test]
    fn config_setting_constraints_are_real_metadata_edges() {
        let mut graph = DependencyGraph::new();
        add_constrained_config_setting(
            &mut graph,
            "//cfg:host",
            &["@platforms//os:linux"],
        );
        let owner = Label::parse("//cfg:host", None, None).unwrap();
        let constraint =
            Label::parse("@platforms//os:linux", None, None).unwrap();
        let scope = HashSet::from([owner.clone()]);

        assert_eq!(
            graph.find_all_missing_in_scope(&scope),
            vec![(owner.clone(), constraint.clone())]
        );
        assert_eq!(
            graph
                .get(&owner)
                .unwrap()
                .constraint_value_references,
            vec![constraint]
        );
    }

    #[test]
    fn config_setting_constraint_requires_constraint_value_type() {
        let mut valid = DependencyGraph::new();
        add_constrained_config_setting(
            &mut valid,
            "//cfg:host",
            &["//platform_defs:linux"],
        );
        add_constraint_value(
            &mut valid,
            "//platform_defs:linux",
            Some(vec!["//visibility:public".to_string()]),
        );
        valid.resolve().unwrap();

        let mut wrong = DependencyGraph::new();
        add_constrained_config_setting(
            &mut wrong,
            "//cfg:host",
            &["//platform_defs:linux"],
        );
        add_config_setting(
            &mut wrong,
            "//platform_defs:linux",
            Some(vec!["//visibility:public".to_string()]),
            true,
        );
        let error = wrong.resolve().unwrap_err().to_string();
        assert!(error.contains("constraint_values entry"), "{error}");
        assert!(error.contains("not a constraint_value"), "{error}");
    }

    #[test]
    fn config_setting_constraint_does_not_virtualize_default_label() {
        let mut graph = DependencyGraph::new();
        add_constrained_config_setting(
            &mut graph,
            "//cfg:invalid",
            &["//conditions:default"],
        );
        let owner = Label::parse("//cfg:invalid", None, None).unwrap();
        let scope = HashSet::from([owner.clone()]);

        assert_eq!(
            graph.find_all_missing_in_scope(&scope),
            vec![(
                owner,
                Label::parse("//conditions:default", None, None).unwrap(),
            )]
        );
    }

    #[test]
    fn canonical_default_identity_respects_main_alias_and_repository() {
        let aliases = HashSet::from(["self".to_string()]);
        let mut graph = DependencyGraph::with_main_repository_aliases(aliases);
        let main_owner =
            Label::parse("//conditions:owner", None, None).unwrap();
        let main = rush_rules::with_rule_metadata(
            Box::new(DummyInstance { deps: Vec::new() }),
            None,
            false,
            vec![
                ":default".to_string(),
                "@self//conditions:default".to_string(),
                "@self//conditions:linux".to_string(),
            ],
        );
        graph.add_node(main_owner.clone(), main).unwrap();
        assert_eq!(
            graph
                .get(&main_owner)
                .unwrap()
                .configuration_references,
            vec![Label::parse("//conditions:linux", None, None).unwrap()]
        );

        let external_owner =
            Label::parse("@ext//pkg:owner", None, None).unwrap();
        let external = rush_rules::with_rule_metadata(
            Box::new(DummyInstance { deps: Vec::new() }),
            None,
            false,
            vec!["@ext//conditions:default".to_string()],
        );
        graph.add_node(external_owner.clone(), external).unwrap();
        assert_eq!(
            graph
                .get(&external_owner)
                .unwrap()
                .configuration_references,
            vec![Label::parse(
                "@ext//conditions:default",
                None,
                None,
            )
            .unwrap()]
        );

        let external_unqualified_owner =
            Label::parse("@ext//pkg:unqualified", None, None).unwrap();
        let external_unqualified = rush_rules::with_rule_metadata(
            Box::new(DummyInstance { deps: Vec::new() }),
            None,
            false,
            vec![
                "//conditions:default".to_string(),
                "@//conditions:linux".to_string(),
            ],
        );
        graph
            .add_node(external_unqualified_owner.clone(), external_unqualified)
            .unwrap();
        assert_eq!(
            graph
                .get(&external_unqualified_owner)
                .unwrap()
                .configuration_references,
            vec![Label::parse("//conditions:linux", None, None).unwrap()],
            "an explicit root marker must not inherit the external owner"
        );
    }

    #[test]
    fn alias_actual_default_spellings_remain_required_targets() {
        for (actual, expected) in [
            ("//conditions:default", "//conditions:default"),
            ("@//conditions:default", "//conditions:default"),
            ("@@//conditions:default", "//conditions:default"),
            ("@self//conditions:default", "//conditions:default"),
            (
                "@ext//conditions:default",
                "@ext//conditions:default",
            ),
        ] {
            let aliases = HashSet::from(["self".to_string()]);
            let mut graph =
                DependencyGraph::with_main_repository_aliases(aliases);
            add_alias(
                &mut graph,
                "//aliases:mode",
                actual,
                Some(vec!["//visibility:public".to_string()]),
            );
            add_select_consumer(
                &mut graph,
                "//consumer:target",
                &["//aliases:mode"],
            );

            let consumer =
                Label::parse("//consumer:target", None, None).unwrap();
            let alias = Label::parse("//aliases:mode", None, None).unwrap();
            let expected = Label::parse(expected, None, None).unwrap();
            let scope = HashSet::from([consumer]);
            assert_eq!(
                graph.find_all_missing_in_scope(&scope),
                vec![(alias, expected.clone())],
                "actual spelling {actual}"
            );

            let error = graph
                .resolve_within_scope(&scope)
                .unwrap_err()
                .to_string();
            assert!(error.contains(&expected.to_string()), "{error}");
            assert!(error.contains("doesn't exist"), "{error}");
        }
    }

    #[test]
    fn directly_incompatible_owner_skips_only_condition_visibility() {
        let private = Some(vec!["//visibility:private".to_string()]);

        let mut direct = DependencyGraph::new();
        add_config_setting(
            &mut direct,
            "//conditions:private_mode",
            private.clone(),
            true,
        );
        let incompatible = rush_rules::with_rule_metadata(
            Box::new(
                rush_rules::rules_config::IncompatibleTargetInstance::new(
                    "target".to_string(),
                ),
            ),
            None,
            false,
            vec!["//conditions:private_mode".to_string()],
        );
        let owner = Label::parse("//consumer:target", None, None).unwrap();
        direct.add_node(owner.clone(), incompatible).unwrap();
        direct
            .resolve_within_scope(&HashSet::from([owner]))
            .unwrap();

        let mut through_alias = DependencyGraph::new();
        add_config_setting(
            &mut through_alias,
            "//conditions:private_mode",
            private,
            true,
        );
        add_alias(
            &mut through_alias,
            "//aliases:mode",
            "//conditions:private_mode",
            Some(vec!["//visibility:public".to_string()]),
        );
        let incompatible = rush_rules::with_rule_metadata(
            Box::new(
                rush_rules::rules_config::IncompatibleTargetInstance::new(
                    "target".to_string(),
                ),
            ),
            None,
            false,
            vec!["//aliases:mode".to_string()],
        );
        let owner = Label::parse("//consumer:target", None, None).unwrap();
        through_alias.add_node(owner.clone(), incompatible).unwrap();
        let error = through_alias
            .resolve_within_scope(&HashSet::from([owner]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("private visibility"), "{error}");
    }

    #[test]
    fn unmatched_select_owner_skips_only_condition_visibility() {
        let mut direct = DependencyGraph::new();
        add_config_setting(
            &mut direct,
            "//conditions:private_mode",
            Some(vec!["//visibility:private".to_string()]),
            true,
        );
        let owner = Label::parse("//consumer:target", None, None).unwrap();
        let unresolved = rush_rules::with_rule_metadata(
            Box::new(
                UnconfigurableTargetInstance::from_configurable_error(
                    "target".to_string(),
                    "no matching condition".to_string(),
                ),
            ),
            None,
            false,
            vec!["//conditions:private_mode".to_string()],
        );
        direct.add_node(owner.clone(), unresolved).unwrap();
        direct
            .resolve_within_scope(&HashSet::from([owner]))
            .unwrap();

        let mut through_alias = DependencyGraph::new();
        add_config_setting(
            &mut through_alias,
            "//conditions:private_mode",
            Some(vec!["//visibility:private".to_string()]),
            true,
        );
        add_alias(
            &mut through_alias,
            "//aliases:mode",
            "//conditions:private_mode",
            Some(vec!["//visibility:public".to_string()]),
        );
        let owner = Label::parse("//consumer:target", None, None).unwrap();
        let unresolved = rush_rules::with_rule_metadata(
            Box::new(
                UnconfigurableTargetInstance::from_configurable_error(
                    "target".to_string(),
                    "no matching condition".to_string(),
                ),
            ),
            None,
            false,
            vec!["//aliases:mode".to_string()],
        );
        through_alias.add_node(owner.clone(), unresolved).unwrap();
        let error = through_alias
            .resolve_within_scope(&HashSet::from([owner]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("private visibility"), "{error}");
    }

    #[test]
    fn unconfigurable_select_key_fails_when_visibility_enforcement_is_off() {
        let mut graph = DependencyGraph::new();
        graph
            .visibility_options
            .enforce_config_setting_visibility = false;
        graph
            .add_node(
                Label::parse("//conditions:bad", None, None).unwrap(),
                Box::new(
                    rush_rules::rules_config::UnconfigurableTargetInstance::new(
                        "bad".to_string(),
                        "original condition error".to_string(),
                    ),
                ),
            )
            .unwrap();
        add_select_consumer(
            &mut graph,
            "//consumer:target",
            &["//conditions:bad"],
        );
        let scope = HashSet::from([
            Label::parse("//consumer:target", None, None).unwrap(),
        ]);
        let error = graph
            .resolve_within_scope(&scope)
            .unwrap_err()
            .to_string();
        assert!(error.contains("original condition error"), "{error}");
    }

    #[test]
    fn package_group_package_patterns_match_bazel_6_semantics() {
        let graph = DependencyGraph::new();
        let group = Label::parse("//groups:friends", None, None).unwrap();
        let cases = [
            ("//", "//:root", false, true),
            ("//", "//child:target", false, false),
            ("//foo", "//foo:target", false, true),
            ("//foo", "//foo/child:target", false, false),
            ("//foo/...", "//foo:target", false, true),
            ("//foo/...", "//foo/child:target", false, true),
            ("//foo/...", "//foobar:target", false, false),
            ("//...", "//:root", false, true),
            ("//...", "//any/depth:target", false, true),
            ("-//foo/...", "//foo/child:target", true, true),
            ("private", "//foo:target", false, false),
            ("public", "@other//foo:target", false, true),
            ("//...", "@other//foo:target", false, false),
        ];

        for (specification, candidate, negative, matches) in cases {
            let candidate = Label::parse(candidate, None, None).unwrap();
            assert_eq!(
                graph
                    .package_spec_matches(&group, &candidate, specification)
                    .unwrap(),
                (negative, matches),
                "specification {specification} for candidate {candidate}"
            );
        }
    }

    #[test]
    fn package_group_public_and_reporoot_modes_match_bazel() {
        let group = Label::parse("//groups:friends", None, None).unwrap();
        let external = Label::parse("@other//pkg:target", None, None).unwrap();

        let mut incompatible_flags = DependencyGraph::new();
        incompatible_flags
            .visibility_options
            .package_group_has_public_syntax = false;
        let error = incompatible_flags
            .package_spec_matches(&group, &external, "//pkg")
            .unwrap_err()
            .to_string();
        assert!(error.contains("repository-local //... meaning"), "{error}");

        incompatible_flags
            .visibility_options
            .fix_package_group_reporoot_syntax = false;
        let error = incompatible_flags
            .package_spec_matches(&group, &external, "public")
            .unwrap_err()
            .to_string();
        assert!(error.contains("public/private package"), "{error}");
        assert_eq!(
            incompatible_flags
                .package_spec_matches(&group, &external, "//...")
                .unwrap(),
            (false, true)
        );

        let mut unreferenced = DependencyGraph::new();
        unreferenced
            .visibility_options
            .package_group_has_public_syntax = false;
        add_package_group(&mut unreferenced, "//groups:unused", &[], &[]);
        unreferenced.resolve().unwrap();
    }

    #[test]
    fn reporoot_compatibility_mode_changes_cross_repository_visibility() {
        fn graph() -> DependencyGraph {
            let mut graph = DependencyGraph::new();
            add_package_group(
                &mut graph,
                "//legacy_groups:all",
                &["//..."],
                &[],
            );
            graph
                .add_node(
                    Label::parse("//provider:secret", None, None).unwrap(),
                    Box::new(DummyVisibilityInstance {
                        visibility: vec!["//legacy_groups:all".to_string()],
                    }),
                )
                .unwrap();
            graph
                .add_node(
                    Label::parse(
                        "@other//client:consumer",
                        None,
                        None,
                    )
                    .unwrap(),
                    Box::new(DummyInstance {
                        deps: vec!["@//provider:secret".to_string()],
                    }),
                )
                .unwrap();
            graph
        }

        let mut fixed = graph();
        let error = fixed.resolve().unwrap_err().to_string();
        assert!(error.contains("is not visible"), "{error}");

        let mut legacy = graph();
        legacy
            .visibility_options
            .fix_package_group_reporoot_syntax = false;
        legacy.resolve().unwrap();

        let mut legacy_without_public_syntax = graph();
        legacy_without_public_syntax
            .visibility_options
            .package_group_has_public_syntax = false;
        legacy_without_public_syntax
            .visibility_options
            .fix_package_group_reporoot_syntax = false;
        legacy_without_public_syntax.resolve().unwrap();
    }

    #[test]
    fn package_group_negatives_are_local_before_includes_are_unioned() {
        let mut graph = DependencyGraph::new();
        add_package_group(
            &mut graph,
            "//groups:restricted",
            &["//friends/...", "-//friends/tests/..."],
            &[],
        );
        add_package_group(
            &mut graph,
            "//groups:leaf",
            &["//friends/tests/..."],
            &[],
        );
        add_package_group(&mut graph, "//groups:middle", &[], &[":leaf"]);
        add_package_group(
            &mut graph,
            "//groups:union",
            &["//friends/...", "-//friends/tests/..."],
            &[":middle"],
        );

        let candidate = Label::parse("//friends/tests/unit:consumer", None, None).unwrap();
        let owner = Label::parse("//owner:secret", None, None).unwrap();
        let restricted = Label::parse("//groups:restricted", None, None).unwrap();
        assert!(!graph
            .package_group_contains(
                &NodeKey::LegacyTarget(restricted.clone()),
                &NodeKey::LegacyTarget(candidate.clone()),
                &NodeKey::LegacyTarget(owner.clone()),
                &mut Vec::new(),
                &mut HashMap::new(),
            )
            .unwrap());

        let union = Label::parse("//groups:union", None, None).unwrap();
        assert!(graph
            .package_group_contains(
                &NodeKey::LegacyTarget(union.clone()),
                &NodeKey::LegacyTarget(candidate.clone()),
                &NodeKey::LegacyTarget(owner.clone()),
                &mut Vec::new(),
                &mut HashMap::new(),
            )
            .unwrap());
    }

    #[test]
    fn package_group_specs_anchor_to_the_declaring_repository() {
        let mut graph = DependencyGraph::new();
        add_package_group(
            &mut graph,
            "@external//groups:friends",
            &["//clients/..."],
            &[],
        );
        let group = Label::parse("@external//groups:friends", None, None).unwrap();
        let owner = Label::parse("@external//owner:secret", None, None).unwrap();

        for (candidate, expected) in [
            ("@external//clients/sub:consumer", true),
            ("//clients/sub:consumer", false),
            ("@other//clients/sub:consumer", false),
        ] {
            let candidate = Label::parse(candidate, None, None).unwrap();
            assert_eq!(
                graph
                    .package_group_contains(
                        &NodeKey::LegacyTarget(group.clone()),
                        &NodeKey::LegacyTarget(candidate.clone()),
                        &NodeKey::LegacyTarget(owner.clone()),
                        &mut Vec::new(),
                        &mut HashMap::new(),
                    )
                    .unwrap(),
                expected,
                "candidate {candidate}"
            );
        }
    }

    #[test]
    fn direct_root_visibility_groups_are_loaded_and_validated() {
        let root = Label::parse("//owner:root", None, None).unwrap();
        let group = Label::parse("//groups:missing", None, None).unwrap();
        let scope = HashSet::from([root.clone()]);

        let mut missing = DependencyGraph::new();
        missing
            .add_node(
                root.clone(),
                Box::new(DummyVisibilityInstance {
                    visibility: vec![group.to_string()],
                }),
            )
            .unwrap();
        assert_eq!(
            missing.find_all_missing_in_scope(&scope),
            vec![(root.clone(), group.clone())]
        );
        let error = missing
            .resolve_within_scope(&scope)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown package_group"), "{error}");

        let mut wrong_kind = DependencyGraph::new();
        wrong_kind
            .add_node(
                root.clone(),
                Box::new(DummyVisibilityInstance {
                    visibility: vec![group.to_string()],
                }),
            )
            .unwrap();
        wrong_kind
            .add_node(group, Box::new(DummyInstance { deps: Vec::new() }))
            .unwrap();
        let error = wrong_kind
            .resolve_within_scope(&scope)
            .unwrap_err()
            .to_string();
        assert!(error.contains("rather than a package_group"), "{error}");
        assert!(error.contains("//owner:root"), "{error}");
    }

    #[test]
    fn same_package_visibility_groups_are_loaded_and_validated() {
        let secret = Label::parse("//owner:secret", None, None).unwrap();
        let friend = Label::parse("//owner:friend", None, None).unwrap();
        let group = Label::parse("//groups:missing", None, None).unwrap();
        let scope = HashSet::from([secret.clone(), friend.clone()]);

        let mut missing = DependencyGraph::new();
        missing
            .add_node(
                secret.clone(),
                Box::new(DummyVisibilityInstance {
                    visibility: vec![group.to_string()],
                }),
            )
            .unwrap();
        missing
            .add_node(
                friend.clone(),
                Box::new(DummyInstance {
                    deps: vec![secret.to_string()],
                }),
            )
            .unwrap();
        assert_eq!(
            missing.find_all_missing_in_scope(&scope),
            vec![(secret.clone(), group.clone())]
        );
        let error = missing
            .resolve_within_scope(&scope)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown package_group"), "{error}");

        let mut wrong_kind = DependencyGraph::new();
        wrong_kind
            .add_node(
                secret.clone(),
                Box::new(DummyVisibilityInstance {
                    visibility: vec![group.to_string()],
                }),
            )
            .unwrap();
        wrong_kind
            .add_node(
                friend,
                Box::new(DummyInstance {
                    deps: vec![secret.to_string()],
                }),
            )
            .unwrap();
        wrong_kind
            .add_node(group, Box::new(DummyInstance { deps: Vec::new() }))
            .unwrap();
        let error = wrong_kind
            .resolve_within_scope(&scope)
            .unwrap_err()
            .to_string();
        assert!(error.contains("rather than a package_group"), "{error}");
        assert!(error.contains("//owner:secret"), "{error}");
    }

    #[test]
    fn direct_package_group_includes_are_loaded_and_validated() {
        let root = Label::parse("//groups:root", None, None).unwrap();
        let included =
            Label::parse("//groups:included", None, None).unwrap();
        let scope = HashSet::from([root.clone()]);

        let mut missing = DependencyGraph::new();
        add_package_group(&mut missing, "//groups:root", &[], &[":included"]);
        assert_eq!(
            missing.find_all_missing_in_scope(&scope),
            vec![(root.clone(), included.clone())]
        );
        let error = missing
            .resolve_within_scope(&scope)
            .unwrap_err()
            .to_string();
        assert!(error.contains("includes unknown package_group"), "{error}");

        let mut wrong_kind = DependencyGraph::new();
        add_package_group(
            &mut wrong_kind,
            "//groups:root",
            &[],
            &[":included"],
        );
        wrong_kind
            .add_node(
                included,
                Box::new(DummyInstance { deps: Vec::new() }),
            )
            .unwrap();
        let error = wrong_kind
            .resolve_within_scope(&scope)
            .unwrap_err()
            .to_string();
        assert!(error.contains("rather than a package_group"), "{error}");
        assert!(error.contains("//groups:root"), "{error}");
    }

    #[test]
    fn package_group_visibility_denies_a_loaded_nonmember() {
        let mut graph = DependencyGraph::new();
        add_package_group(
            &mut graph,
            "//groups:friends",
            &["//friends/..."],
            &[],
        );
        let secret = Label::parse("//owner:secret", None, None).unwrap();
        graph
            .add_node(
                secret.clone(),
                Box::new(DummyVisibilityInstance {
                    visibility: vec!["//groups:friends".to_string()],
                }),
            )
            .unwrap();
        graph
            .add_node(
                Label::parse("//outsider:consumer", None, None).unwrap(),
                Box::new(DummyInstance {
                    deps: vec![secret.to_string()],
                }),
            )
            .unwrap();

        let error = graph.resolve().unwrap_err().to_string();
        assert!(error.contains("//outsider:consumer"), "{error}");
        assert!(error.contains("//owner:secret"), "{error}");
        assert!(error.contains("is not visible"), "{error}");
        assert!(error.contains("//groups:friends"), "{error}");
    }

    #[test]
    fn package_group_unknown_wrong_kind_and_cycles_fail_loudly() {
        let candidate = Label::parse("//client:consumer", None, None).unwrap();
        let owner = Label::parse("//owner:secret", None, None).unwrap();

        let mut unknown = DependencyGraph::new();
        add_package_group(&mut unknown, "//groups:outer", &[], &[":missing"]);
        let error = unknown
            .package_group_contains(
                &NodeKey::LegacyTarget(Label::parse("//groups:outer", None, None).unwrap()),
                &NodeKey::LegacyTarget(candidate.clone()),
                &NodeKey::LegacyTarget(owner.clone()),
                &mut Vec::new(),
                &mut HashMap::new(),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("includes unknown package_group"), "{error}");
        assert!(error.contains("//groups:missing"), "{error}");

        let mut wrong_kind = DependencyGraph::new();
        add_package_group(&mut wrong_kind, "//groups:outer", &[], &[":plain"]);
        wrong_kind
            .add_node(
                Label::parse("//groups:plain", None, None).unwrap(),
                Box::new(DummyInstance { deps: Vec::new() }),
            )
            .unwrap();
        let error = wrong_kind
            .package_group_contains(
                &NodeKey::LegacyTarget(Label::parse("//groups:outer", None, None).unwrap()),
                &NodeKey::LegacyTarget(candidate.clone()),
                &NodeKey::LegacyTarget(owner.clone()),
                &mut Vec::new(),
                &mut HashMap::new(),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("rather than a package_group"), "{error}");
        assert!(error.contains("//groups:plain"), "{error}");

        let mut cycle = DependencyGraph::new();
        add_package_group(&mut cycle, "//groups:a", &[], &[":b"]);
        add_package_group(&mut cycle, "//groups:b", &[], &[":a"]);
        let error = cycle
            .package_group_contains(
                &NodeKey::LegacyTarget(Label::parse("//groups:a", None, None).unwrap()),
                &NodeKey::LegacyTarget(candidate.clone()),
                &NodeKey::LegacyTarget(owner.clone()),
                &mut Vec::new(),
                &mut HashMap::new(),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("Package group include cycle"), "{error}");
        assert!(
            error.contains("//groups:a -> //groups:b -> //groups:a"),
            "{error}"
        );
    }

    #[test]
    fn referenced_unconfigurable_package_group_reports_its_original_cause() {
        let mut graph = DependencyGraph::new();
        graph
            .add_node(
                Label::parse("//groups:bad", None, None).unwrap(),
                Box::new(
                    rush_rules::rules_config::UnconfigurableTargetInstance::new(
                        "bad".to_string(),
                        "in call to package_group(), parameter 'packages' got value of type 'select', want 'sequence'"
                            .to_string(),
                    ),
                ),
            )
            .unwrap();
        graph
            .add_node(
                Label::parse("//owner:secret", None, None).unwrap(),
                Box::new(DummyVisibilityInstance {
                    visibility: vec!["//groups:bad".to_string()],
                }),
            )
            .unwrap();
        graph
            .add_node(
                Label::parse("//consumer:use_secret", None, None).unwrap(),
                Box::new(DummyInstance {
                    deps: vec!["//owner:secret".to_string()],
                }),
            )
            .unwrap();

        let error = graph.resolve().unwrap_err().to_string();
        assert!(
            error.contains("Package group //groups:bad cannot be configured"),
            "{error}"
        );
        assert!(error.contains("parameter 'packages'"), "{error}");
        assert!(error.contains("type 'select', want 'sequence'"), "{error}");
        assert!(!error.contains("rather than a package_group"), "{error}");
    }

    #[test]
    fn declared_output_ownership_rejects_duplicate_and_unsafe_paths() {
        let duplicate_label = Label::parse("//pkg:duplicate", None, None).unwrap();
        let mut duplicate_graph = DependencyGraph::new();
        duplicate_graph
            .add_node(
                duplicate_label,
                Box::new(DummyOutputInstance {
                    outputs: vec!["same.txt".to_string(), "same.txt".to_string()],
                }),
            )
            .unwrap();
        let duplicate = duplicate_graph
            .validate_declared_output_ownership()
            .unwrap_err()
            .to_string();
        assert!(duplicate.contains("more than once"), "{duplicate}");

        for output in ["", "/absolute.txt", "../escape.txt"] {
            let mut graph = DependencyGraph::new();
            graph
                .add_node(
                    Label::parse("//pkg:unsafe", None, None).unwrap(),
                    Box::new(DummyOutputInstance {
                        outputs: vec![output.to_string()],
                    }),
                )
                .unwrap();
            let error = graph
                .validate_declared_output_ownership()
                .unwrap_err()
                .to_string();
            assert!(error.contains("//pkg:unsafe"), "{error}");
        }
    }

    #[test]
    fn test_relative_dependency_keeps_workspace() {
        let mut graph = DependencyGraph::new();
        let label = Label {
            workspace: Some("com_google_protobuf".to_string()),
            package: "".to_string(),
            target: "source_context_proto_genproto".to_string(),
        };
        let deps = vec![":source_context_proto".to_string()];
        let instance = Box::new(DummyInstance { deps });

        graph.add_node(label.clone(), instance).unwrap();

        let node = graph.get(&label).unwrap();
        let dep = &node.dependencies[0];
        assert_eq!(dep.package, "");
        assert_eq!(dep.target, "source_context_proto");
        assert_eq!(dep.workspace.as_deref(), Some("com_google_protobuf"));
    }

    #[test]
    fn mem_census_reports_nodes_and_edges() {
        // #39 graph-RSS census: two nodes, lib depends on dep -> the census
        // reports 2 nodes and 1 dependency edge, and the label strings are
        // accounted. Confirms the structural walk wires up.
        let mut graph = DependencyGraph::new();
        let dep_label = Label::parse("//pkg:dep", None, None).unwrap();
        graph
            .add_node(dep_label, Box::new(DummyInstance { deps: vec![] }))
            .unwrap();
        let lib_label = Label::parse("//pkg:lib", None, None).unwrap();
        graph
            .add_node(
                lib_label,
                Box::new(DummyInstance {
                    deps: vec!["//pkg:dep".to_string()],
                }),
            )
            .unwrap();
        let report = graph.mem_census();
        assert!(report.contains("nodes=2"), "report: {}", report);
        assert!(
            report.contains("dependency_edges=1"),
            "report: {}",
            report
        );
        assert!(report.contains("label_string_bytes="));
    }

    #[test]
    fn get_reachable_nodes_handles_missing_deps_without_retaining_all() {
        // #39 graph-RSS: get_reachable_nodes must traverse in-graph deps and
        // simply COUNT missing ones (capped display) — not retain a String
        // pair per missing edge. Behavior: returns the in-graph reachable set,
        // excludes the missing dep, and does not error. (The memory cap itself
        // is internal; this guards the reachability semantics around it.)
        let mut graph = DependencyGraph::new();
        let dep_label = Label::parse("//pkg:dep", None, None).unwrap();
        graph
            .add_node(dep_label.clone(), Box::new(DummyInstance { deps: vec![] }))
            .unwrap();
        let lib_label = Label::parse("//pkg:lib", None, None).unwrap();
        graph
            .add_node(
                lib_label.clone(),
                Box::new(DummyInstance {
                    // one in-graph dep + one that is NOT in the graph (missing)
                    deps: vec!["//pkg:dep".to_string(), "//pkg:absent".to_string()],
                }),
            )
            .unwrap();
        let reachable = graph.get_reachable_nodes(&lib_label).unwrap();
        assert!(reachable.contains(&lib_label));
        assert!(reachable.contains(&dep_label));
        assert_eq!(reachable.len(), 2, "missing //pkg:absent must not be included");
    }
}
