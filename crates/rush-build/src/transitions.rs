// Rush Build - Configuration Transitions
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Dynamic configuration transitions.
//!
//! Allows building targets with different configurations within the same build.
//! For example, building tools for the host platform, then using those tools
//! to cross-compile targets for a different platform.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{BuildConfiguration, CompilationMode, Platform};

/// Configuration transition.
///
/// Describes how to transform a configuration for a target and its dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    /// Transition name.
    pub name: String,

    /// Transition type.
    pub transition_type: TransitionType,

    /// Platform override.
    pub platform: Option<Platform>,

    /// Compilation mode override.
    pub mode: Option<CompilationMode>,

    /// Additional flags to add.
    pub additional_flags: HashMap<String, Vec<String>>,

    /// Features to enable/disable.
    pub feature_overrides: HashMap<String, bool>,
}

/// Type of configuration transition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TransitionType {
    /// Host transition - build for host platform regardless of target.
    Host,

    /// Exec transition - build for execution platform (where build actions run).
    Exec,

    /// Target transition - use specified target configuration.
    Target,

    /// Custom transition with user-defined logic.
    Custom(String),

    /// Split transition - build with multiple configurations.
    Split(Vec<String>),
}

/// Transition rule application.
#[derive(Debug, Clone)]
pub struct TransitionRule {
    /// Which targets this applies to.
    pub target_pattern: String,

    /// Transition to apply.
    pub transition: Arc<Transition>,

    /// Whether to apply to dependencies.
    pub propagate: bool,
}

/// Transition manager.
pub struct TransitionManager {
    /// Registered transitions.
    transitions: HashMap<String, Arc<Transition>>,

    /// Transition rules.
    rules: Vec<TransitionRule>,

    /// Host configuration (for host transitions).
    host_config: BuildConfiguration,

    /// Current target configuration.
    target_config: BuildConfiguration,
}

impl TransitionManager {
    /// Create new transition manager.
    pub fn new(host_config: BuildConfiguration, target_config: BuildConfiguration) -> Self {
        let mut manager = TransitionManager {
            transitions: HashMap::new(),
            rules: Vec::new(),
            host_config,
            target_config,
        };

        // Register built-in transitions
        manager.register_builtin_transitions();

        manager
    }

    /// Register built-in transitions.
    fn register_builtin_transitions(&mut self) {
        // Host transition
        self.register(Arc::new(Transition {
            name: "host".to_string(),
            transition_type: TransitionType::Host,
            platform: None,
            mode: None,
            additional_flags: HashMap::new(),
            feature_overrides: HashMap::new(),
        }));

        // Exec transition (same as host for most cases)
        self.register(Arc::new(Transition {
            name: "exec".to_string(),
            transition_type: TransitionType::Exec,
            platform: None,
            mode: None,
            additional_flags: HashMap::new(),
            feature_overrides: HashMap::new(),
        }));

        // Opt transition (force optimization)
        let mut opt_flags = HashMap::new();
        opt_flags.insert("cflags".to_string(), vec!["-O3".to_string()]);
        opt_flags.insert("cxxflags".to_string(), vec!["-O3".to_string()]);

        self.register(Arc::new(Transition {
            name: "opt".to_string(),
            transition_type: TransitionType::Custom("opt".to_string()),
            platform: None,
            mode: Some(CompilationMode::Release),
            additional_flags: opt_flags,
            feature_overrides: HashMap::new(),
        }));
    }

    /// Register a transition.
    pub fn register(&mut self, transition: Arc<Transition>) {
        self.transitions.insert(transition.name.clone(), transition);
    }

    /// Add a transition rule.
    pub fn add_rule(&mut self, rule: TransitionRule) {
        self.rules.push(rule);
    }

    /// Apply transition to configuration.
    pub fn apply_transition(
        &self,
        base_config: &BuildConfiguration,
        transition: &Transition,
    ) -> Result<BuildConfiguration> {
        let mut new_config = base_config.clone();

        // Apply transition type
        match &transition.transition_type {
            TransitionType::Host => {
                // Use host platform
                new_config.platform = self.host_config.platform.clone();
                new_config.mode = self.host_config.mode.clone();
            }
            TransitionType::Exec => {
                // Use exec platform (same as host for now)
                new_config.platform = self.host_config.platform.clone();
            }
            TransitionType::Target => {
                // Use target platform (no change)
            }
            TransitionType::Custom(_) => {
                // Apply custom overrides
            }
            TransitionType::Split(_) => {
                // Split transitions are handled separately
            }
        }

        // Apply platform override
        if let Some(ref platform) = transition.platform {
            new_config.platform = platform.clone();
        }

        // Apply mode override
        if let Some(ref mode) = transition.mode {
            new_config.mode = mode.clone();
        }

        // Apply additional flags
        for (lang, flags) in &transition.additional_flags {
            match lang.as_str() {
                "cflags" => new_config.cflags.extend(flags.clone()),
                "cxxflags" => new_config.cxxflags.extend(flags.clone()),
                "rustflags" => new_config.rustflags.extend(flags.clone()),
                "goflags" => new_config.goflags.extend(flags.clone()),
                _ => {}
            }
        }

        // Apply feature overrides
        for (feature, enabled) in &transition.feature_overrides {
            new_config.features.insert(feature.clone(), *enabled);
        }

        Ok(new_config)
    }

    /// Find transition for target.
    pub fn find_transition(&self, target: &str) -> Option<Arc<Transition>> {
        for rule in &self.rules {
            if target_matches_pattern(target, &rule.target_pattern) {
                return Some(Arc::clone(&rule.transition));
            }
        }

        None
    }

    /// Get configuration for target with transitions applied.
    pub fn get_config_for_target(
        &self,
        target: &str,
        base_config: &BuildConfiguration,
    ) -> Result<BuildConfiguration> {
        if let Some(transition) = self.find_transition(target) {
            self.apply_transition(base_config, &transition)
        } else {
            Ok(base_config.clone())
        }
    }

    /// Get host configuration.
    pub fn host_config(&self) -> &BuildConfiguration {
        &self.host_config
    }

    /// Get target configuration.
    pub fn target_config(&self) -> &BuildConfiguration {
        &self.target_config
    }

    /// List all registered transitions.
    pub fn list_transitions(&self) -> Vec<String> {
        self.transitions.keys().cloned().collect()
    }

    /// Get transition by name.
    pub fn get_transition(&self, name: &str) -> Option<Arc<Transition>> {
        self.transitions.get(name).cloned()
    }
}

/// Check if target matches pattern.
fn target_matches_pattern(target: &str, pattern: &str) -> bool {
    // Simple pattern matching
    // Supports:
    // - Exact match: "//pkg:target"
    // - Package wildcard: "//pkg:*"
    // - All targets: "//..."

    if pattern == "//..." {
        return true; // Matches everything
    }

    if pattern.ends_with(":*") {
        // Package wildcard
        let pkg = pattern.trim_end_matches(":*");
        target.starts_with(pkg)
    } else if pattern.ends_with("/...") {
        // Recursive package wildcard
        let pkg = pattern.trim_end_matches("/...");
        target.starts_with(pkg)
    } else {
        // Exact match
        target == pattern
    }
}

/// Transition builder for fluent API.
pub struct TransitionBuilder {
    transition: Transition,
}

impl TransitionBuilder {
    /// Create new transition builder.
    pub fn new(name: impl Into<String>) -> Self {
        TransitionBuilder {
            transition: Transition {
                name: name.into(),
                transition_type: TransitionType::Target,
                platform: None,
                mode: None,
                additional_flags: HashMap::new(),
                feature_overrides: HashMap::new(),
            },
        }
    }

    /// Set transition type.
    pub fn transition_type(mut self, transition_type: TransitionType) -> Self {
        self.transition.transition_type = transition_type;
        self
    }

    /// Set platform.
    pub fn platform(mut self, platform: Platform) -> Self {
        self.transition.platform = Some(platform);
        self
    }

    /// Set compilation mode.
    pub fn mode(mut self, mode: CompilationMode) -> Self {
        self.transition.mode = Some(mode);
        self
    }

    /// Add flag.
    pub fn add_flag(mut self, lang: impl Into<String>, flag: impl Into<String>) -> Self {
        self.transition
            .additional_flags
            .entry(lang.into())
            .or_default()
            .push(flag.into());
        self
    }

    /// Set feature.
    pub fn feature(mut self, name: impl Into<String>, enabled: bool) -> Self {
        self.transition
            .feature_overrides
            .insert(name.into(), enabled);
        self
    }

    /// Build transition.
    pub fn build(self) -> Transition {
        self.transition
    }
}

/// Common transition scenarios.
pub mod presets {
    use super::*;

    /// Create host tools transition.
    ///
    /// Use this for targets that are tools used during the build.
    pub fn host_tools() -> Transition {
        TransitionBuilder::new("host_tools")
            .transition_type(TransitionType::Host)
            .mode(CompilationMode::Release)
            .build()
    }

    /// Create cross-compilation transition.
    pub fn cross_compile(target_platform: Platform) -> Transition {
        TransitionBuilder::new("cross")
            .platform(target_platform)
            .build()
    }

    /// Create test transition (unoptimized, debug info).
    pub fn test_config() -> Transition {
        TransitionBuilder::new("test")
            .mode(CompilationMode::Debug)
            .build()
    }

    /// Create release transition (maximum optimization).
    pub fn release_config() -> Transition {
        let mut flags = HashMap::new();
        flags.insert(
            "cflags".to_string(),
            vec!["-O3".to_string(), "-flto".to_string()],
        );
        flags.insert(
            "cxxflags".to_string(),
            vec!["-O3".to_string(), "-flto".to_string()],
        );

        Transition {
            name: "release".to_string(),
            transition_type: TransitionType::Custom("release".to_string()),
            platform: None,
            mode: Some(CompilationMode::Release),
            additional_flags: flags,
            feature_overrides: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_matches_pattern() {
        assert!(target_matches_pattern("//pkg:target", "//pkg:target"));
        assert!(target_matches_pattern("//pkg:target", "//pkg:*"));
        assert!(target_matches_pattern("//pkg:target", "//..."));
        assert!(target_matches_pattern("//pkg/sub:target", "//pkg/..."));

        assert!(!target_matches_pattern("//pkg:target", "//other:*"));
        assert!(!target_matches_pattern("//pkg:target", "//pkg:other"));
    }

    #[test]
    fn test_transition_builder() {
        let transition = TransitionBuilder::new("test")
            .transition_type(TransitionType::Host)
            .mode(CompilationMode::Debug)
            .add_flag("cflags", "-g")
            .feature("test_feature", true)
            .build();

        assert_eq!(transition.name, "test");
        assert_eq!(transition.transition_type, TransitionType::Host);
        assert_eq!(transition.mode, Some(CompilationMode::Debug));
        assert!(transition.additional_flags.contains_key("cflags"));
        assert!(transition.feature_overrides.contains_key("test_feature"));
    }

    #[test]
    fn test_transition_manager() {
        let host_platform = Platform::detect();
        let host_config = BuildConfiguration::default_for_platform(&host_platform);
        let target_config = host_config.clone();

        let manager = TransitionManager::new(host_config, target_config);

        assert!(manager.get_transition("host").is_some());
        assert!(manager.get_transition("exec").is_some());
        assert!(manager.get_transition("nonexistent").is_none());
    }

    #[test]
    fn test_apply_host_transition() {
        let host_platform = Platform::detect();
        let mut target_platform = host_platform.clone();
        target_platform.arch = crate::config::ArchType::Custom("different".to_string());

        let host_config = BuildConfiguration::default_for_platform(&host_platform);
        let target_config = BuildConfiguration::default_for_platform(&target_platform);

        let manager = TransitionManager::new(host_config.clone(), target_config.clone());
        let host_transition = manager.get_transition("host").unwrap();

        let result = manager
            .apply_transition(&target_config, &host_transition)
            .unwrap();

        // Platform should be changed to host
        assert_eq!(result.platform.os, host_config.platform.os);
    }

    #[test]
    fn test_presets() {
        let host_tools = presets::host_tools();
        assert_eq!(host_tools.transition_type, TransitionType::Host);
        assert_eq!(host_tools.mode, Some(CompilationMode::Release));

        let test_config = presets::test_config();
        assert_eq!(test_config.mode, Some(CompilationMode::Debug));
    }
}
