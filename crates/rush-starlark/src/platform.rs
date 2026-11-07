use std::collections::HashMap;

/// Platform configuration for select() evaluation
#[derive(Debug, Clone)]
pub struct PlatformConfig {
    /// Active declared configuration conditions.
    conditions: HashMap<String, bool>,
    /// Workspace registry captured when this configuration is created.
    ///
    /// Configuration evaluation can outlive the synchronous thread-local
    /// scope that loaded a BUILD file. Retaining the identity here prevents a
    /// later worker thread from consulting the empty compatibility scope.
    configuration_scope: String,
}

impl PlatformConfig {
    /// Create a new platform configuration with auto-detected platform
    pub fn new() -> Self {
        Self::for_configuration_scope(
            crate::config_defines::current_configuration_scope(),
        )
    }

    /// Create a platform configuration bound to an explicit workspace
    /// registry. Production orchestrators use this constructor at analysis
    /// boundaries; standalone callers may continue to use [`Self::new`].
    pub fn for_configuration_scope(scope: impl Into<String>) -> Self {
        let mut conditions = HashMap::new();

        // Bazel reserves only this fallback. Host OS and CPU selection must
        // use declared config_setting/constraint_value targets, normally
        // backed by canonical @platforms constraints.
        conditions.insert("//conditions:default".to_string(), true);

        Self {
            conditions,
            configuration_scope: scope.into(),
        }
    }

    /// Check if a condition is active
    pub fn matches(&self, condition: &str) -> bool {
        if self.conditions.get(condition).copied().unwrap_or(false) {
            return true;
        }
        // Bazel `--define`-driven config_setting activation: a `select()` key
        // naming a `config_setting(define_values=...)` whose values all match
        // the active `--define`s matches here. Inert (returns false) when no
        // defines are active, so default builds are unchanged. See
        // crate::config_defines.
        crate::config_defines::is_active_config_setting_in_scope(
            &self.configuration_scope,
            condition,
        )
    }

    /// Return whether one matching condition is a strict predicate-set
    /// specialization of another in this configuration's workspace.
    pub fn strictly_specializes(&self, candidate: &str, other: &str) -> bool {
        crate::config_defines::config_condition_strictly_specializes_in_scope(
            &self.configuration_scope,
            candidate,
            other,
        )
    }

    /// Set a custom condition (for testing or custom configurations)
    pub fn set_condition(&mut self, condition: String, active: bool) {
        self.conditions.insert(condition, active);
    }

    /// Get the first matching condition from a list
    pub fn first_match(&self, conditions: &[String]) -> Option<String> {
        for condition in conditions {
            if self.matches(condition) {
                return Some(condition.clone());
            }
        }
        None
    }
}

impl Default for PlatformConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_only_default_is_builtin() {
        let config = PlatformConfig::new();

        // Named //conditions host labels are ordinary graph targets, not
        // entries synthesized from the process host.
        for condition in [
            "//conditions:linux",
            "//conditions:unix",
            "//conditions:macos",
            "//conditions:darwin",
            "//conditions:windows",
            "//conditions:freebsd",
            "//conditions:x86_64",
            "//conditions:aarch64",
            "//conditions:arm64",
            "//conditions:arm",
        ] {
            assert!(!config.conditions.contains_key(condition));
        }

        // Default should always be present
        assert!(config.matches("//conditions:default"));
    }

    #[test]
    fn test_custom_conditions() {
        let mut config = PlatformConfig::new();

        config.set_condition("//conditions:gpu".to_string(), true);
        assert!(config.matches("//conditions:gpu"));

        config.set_condition("//conditions:gpu".to_string(), false);
        assert!(!config.matches("//conditions:gpu"));
    }

    #[test]
    fn test_first_match() {
        let config = PlatformConfig::new();

        let conditions = vec![
            "//conditions:nonexistent".to_string(),
            "//conditions:default".to_string(),
        ];

        let result = config.first_match(&conditions);
        assert_eq!(result, Some("//conditions:default".to_string()));
    }
}
