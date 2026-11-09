//! Bazel build settings and host-configuration condition activation.
//!
//! TensorFlow's BUILD files gate optional components behind
//! `config_setting`s keyed on Bazel `--define` values, e.g.
//!
//! ```starlark
//! config_setting(
//!     name = "tflite_with_xnnpack_explicit_false",
//!     define_values = {"tflite_with_xnnpack": "false"},
//! )
//! ```
//!
//! and a consumer `select()` that picks `[]` for that condition. Bazel
//! treats `--define K=V` as *configuration* input — rules never see it as a
//! compiler `-D`; it only flows through `config_setting` matching into
//! `select()`. rush mirrors that: this module holds the active defines
//! (seeded from the `RUSH_DEFINES` env var and/or the `--define` CLI flag)
//! and the set of `config_setting` labels whose supported populated
//! `define_values`, native `values`, and canonical `@platforms`
//! `constraint_values` and typed build-setting `flag_values` all match. A
//! `PlatformConfig` consults that set so every select resolver honors the same
//! configuration through one chokepoint.

use crate::ast::{CallExpr, Expr, Program, Stmt};
use once_cell::sync::Lazy;
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;
use rush_command_core::native_options::NativeConfiguration;

/// Active Bazel `--define` build settings (key → value). Seeded lazily from
/// the `RUSH_DEFINES` env var (space/comma-separated `k=v` tokens); the
/// `--define` CLI flag adds to it via [`set_define`].
static ACTIVE_DEFINES: Lazy<RwLock<HashMap<String, HashMap<String, String>>>> = Lazy::new(|| {
    let mut m = HashMap::new();
    if let Ok(s) = std::env::var("RUSH_DEFINES") {
        for tok in s.split([',', ' ']).filter(|t| !t.is_empty()) {
            if let Some((k, v)) = tok.split_once('=') {
                m.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    RwLock::new(HashMap::from([(String::new(), m)]))
});

/// Labels of declared configuration conditions that match the active command
/// and host configuration.
static ACTIVE_CONFIG_SETTINGS: Lazy<RwLock<HashSet<(String, String)>>> =
    Lazy::new(|| RwLock::new(HashSet::new()));

static NATIVE_CONFIGURATIONS: Lazy<RwLock<HashMap<String, NativeConfiguration>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

#[derive(Clone, Debug)]
struct CommandConfigurationEnvironment {
    enable_cuda: bool,
    pycross_python_version: String,
}

impl Default for CommandConfigurationEnvironment {
    fn default() -> Self {
        Self {
            enable_cuda: false,
            pycross_python_version: "3.13".to_string(),
        }
    }
}

/// Environment-derived configuration inputs captured once at command start.
/// Later registry recomputation and graph validation can run after async or
/// worker-thread boundaries, so they must not perform fresh tracked reads.
static COMMAND_CONFIGURATION_ENVIRONMENTS: Lazy<
    RwLock<HashMap<String, CommandConfigurationEnvironment>>,
> = Lazy::new(|| RwLock::new(HashMap::new()));

#[derive(Clone, Debug, Default)]
struct SeenConfigSetting {
    labels: Vec<String>,
    define_values: HashMap<String, String>,
    native_values: HashMap<String, String>,
    constraint_values: Vec<String>,
    flag_values: Vec<(Vec<String>, String)>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum ConfigPredicate {
    Define(String, String),
    Native(String, String),
    Constraint(String),
    Flag(String, String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigurationConditionKind {
    ConfigSetting,
    ConstraintValue,
}

#[derive(Clone, Debug)]
struct ConfigurationConditionMetadata {
    kind: ConfigurationConditionKind,
    predicates: HashSet<ConfigPredicate>,
}

/// Complete predicates for every observed native `config_setting`. Keeping
/// all dimensions together is required because Bazel combines
/// `define_values`, `values`, `constraint_values`, and `flag_values` with
/// logical AND.
static SEEN_CONFIG_SETTINGS: Lazy<RwLock<HashMap<ScopedRuleKey, SeenConfigSetting>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
#[derive(Clone, Debug)]
struct SeenConfigAlias {
    labels: Vec<String>,
    actual_labels: Vec<String>,
}

/// Process-global registries serve multiple workspaces in tests and in a
/// resident daemon. The first component keeps identical Bazel labels in those
/// workspaces independent; the remaining components are repository, package,
/// and target name.
type ScopedRuleKey = (String, String, String, String);

#[derive(Clone, Debug)]
struct SeenBuildSetting {
    labels: Vec<String>,
    value: BuildSettingValue,
}

/// Typed default/current value of a Starlark build setting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuildSettingValue {
    String(String),
    Bool(bool),
    Int(i64),
}

impl BuildSettingValue {
    fn display(&self) -> String {
        match self {
            Self::String(value) => value.clone(),
            // Preserve the evaluator's historical public spelling. Matching
            // is typed below, so Bazel's lowercase `true`/`false` predicate
            // syntax does not depend on this diagnostic/API representation.
            Self::Bool(value) => {
                if *value { "True" } else { "False" }.to_string()
            }
            Self::Int(value) => value.to_string(),
        }
    }

    fn matches_predicate(&self, expected: &str) -> bool {
        match self {
            Self::String(value) => value == expected,
            Self::Bool(value) => expected.parse::<bool>().ok() == Some(*value),
            Self::Int(value) => expected.parse::<i64>().ok() == Some(*value),
        }
    }
}

static SEEN_BUILD_SETTINGS: Lazy<RwLock<HashMap<ScopedRuleKey, SeenBuildSetting>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static SCOPED_BUILD_SETTING_VALUES: Lazy<
    RwLock<HashMap<(String, String), BuildSettingValue>>,
> = Lazy::new(|| RwLock::new(HashMap::new()));
static SEEN_CONFIG_ALIASES: Lazy<RwLock<HashMap<ScopedRuleKey, SeenConfigAlias>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static SEEN_CONSTRAINT_VALUES: Lazy<RwLock<HashMap<ScopedRuleKey, Vec<String>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static COMPOSED_CONFIG_RECOMPUTE: Lazy<std::sync::Mutex<()>> =
    Lazy::new(|| std::sync::Mutex::new(()));

thread_local! {
    /// Identity of the workspace whose BUILD file is currently being
    /// evaluated or instantiated. Empty is the compatibility scope used by
    /// standalone rush-starlark callers and unit tests.
    static CONFIGURATION_SCOPE: std::cell::RefCell<String> =
        const { std::cell::RefCell::new(String::new()) };
}

pub(crate) fn current_configuration_scope() -> String {
    CONFIGURATION_SCOPE.with(|scope| scope.borrow().clone())
}

fn configuration_scope() -> String {
    current_configuration_scope()
}

fn active_defines_for_scope(scope: &str) -> HashMap<String, String> {
    ACTIVE_DEFINES
        .read()
        .unwrap()
        .get(scope)
        .cloned()
        .unwrap_or_default()
}

/// Run one synchronous configuration-analysis region in an isolated
/// workspace namespace. Nested regions restore their caller's scope.
pub fn with_configuration_scope<R>(scope: &str, f: impl FnOnce() -> R) -> R {
    struct RestoreScope(String);
    impl Drop for RestoreScope {
        fn drop(&mut self) {
            CONFIGURATION_SCOPE.with(|scope| {
                scope.replace(std::mem::take(&mut self.0));
            });
        }
    }

    let previous = CONFIGURATION_SCOPE.with(|current| current.replace(scope.to_string()));
    let _restore = RestoreScope(previous);
    f()
}

fn scoped_rule_key(repository: Option<&str>, package: &str, name: &str) -> ScopedRuleKey {
    (
        configuration_scope(),
        repository.unwrap_or_default().to_string(),
        package.to_string(),
        name.to_string(),
    )
}

fn dedup_labels(mut labels: Vec<String>) -> Vec<String> {
    labels.sort();
    labels.dedup();
    labels
}

fn canonical_configuration_label(label: &str) -> String {
    if let Some(rest) = label
        .strip_prefix("@//")
        .or_else(|| label.strip_prefix("@@//"))
    {
        return format!("//{rest}");
    }
    if let Some(rest) = label.strip_prefix("@@") {
        return format!("@{rest}");
    }
    label.to_string()
}

fn resolve_config_alias_label_inner(
    scope: &str,
    label: &str,
    visited: &mut HashSet<String>,
) -> Option<String> {
    let label = canonical_configuration_label(label);
    if !visited.insert(label.clone()) {
        return None;
    }
    let alias = SEEN_CONFIG_ALIASES
        .read()
        .unwrap()
        .iter()
        .find(|(key, alias)| {
            key.0 == scope
                && alias
                    .labels
                    .iter()
                    .any(|candidate| canonical_configuration_label(candidate) == label)
        })
        .map(|(_, alias)| alias.clone());
    let Some(alias) = alias else {
        return Some(label);
    };
    alias
        .actual_labels
        .iter()
        .filter_map(|actual| resolve_config_alias_label_inner(scope, actual, visited))
        .min()
}

fn resolved_config_reference(scope: &str, label: &str) -> Option<String> {
    resolve_config_alias_label_inner(scope, label, &mut HashSet::new())
}

fn config_setting_predicates(
    scope: &str,
    setting: &SeenConfigSetting,
) -> HashSet<ConfigPredicate> {
    let mut predicates = HashSet::new();
    predicates.extend(
        setting
            .define_values
            .iter()
            .map(|(key, value)| ConfigPredicate::Define(key.clone(), value.clone())),
    );
    predicates.extend(setting.native_values.iter().map(|(key, value)| {
        if key == "define" {
            if let Some((name, value)) = value.split_once('=') {
                return ConfigPredicate::Define(name.to_string(), value.to_string());
            }
        }
        ConfigPredicate::Native(key.clone(),
            rush_command_core::native_options::canonical_native_predicate(key, value)
                .unwrap_or_else(|_| value.clone()))
    }));
    predicates.extend(setting.constraint_values.iter().map(|label| {
        ConfigPredicate::Constraint(
            resolved_config_reference(scope, label)
                .unwrap_or_else(|| canonical_configuration_label(label)),
        )
    }));
    predicates.extend(setting.flag_values.iter().map(|(labels, value)| {
        let label = labels
            .iter()
            .map(|label| {
                resolved_config_reference(scope, label)
                    .unwrap_or_else(|| canonical_configuration_label(label))
            })
            .min()
            .unwrap_or_default();
        ConfigPredicate::Flag(label, value.clone())
    }));
    predicates
}

fn condition_predicates_inner(
    scope: &str,
    condition: &str,
    visited: &mut HashSet<String>,
) -> Option<ConfigurationConditionMetadata> {
    let condition = canonical_configuration_label(condition);
    if !visited.insert(condition.clone()) {
        return None;
    }

    if let Some(setting) = SEEN_CONFIG_SETTINGS
        .read()
        .unwrap()
        .iter()
        .find(|(key, setting)| {
            key.0 == scope
                && setting
                    .labels
                    .iter()
                    .any(|label| canonical_configuration_label(label) == condition)
        })
        .map(|(_, setting)| setting.clone())
    {
        return Some(ConfigurationConditionMetadata {
            kind: ConfigurationConditionKind::ConfigSetting,
            predicates: config_setting_predicates(scope, &setting),
        });
    }

    if let Some(alias) = SEEN_CONFIG_ALIASES
        .read()
        .unwrap()
        .iter()
        .find(|(key, alias)| {
            key.0 == scope
                && alias
                    .labels
                    .iter()
                    .any(|label| canonical_configuration_label(label) == condition)
        })
        .map(|(_, alias)| alias.clone())
    {
        for actual in alias.actual_labels {
            if let Some(metadata) = condition_predicates_inner(scope, &actual, visited) {
                return Some(metadata);
            }
        }
        return None;
    }

    let known_constraint = intrinsic_platform_constraint_matches(&condition).is_some()
        || SEEN_CONSTRAINT_VALUES
            .read()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.0 == scope)
            .any(|(_, labels)| {
                labels
                    .iter()
                    .any(|label| canonical_configuration_label(label) == condition)
            });
    known_constraint.then(|| ConfigurationConditionMetadata {
        kind: ConfigurationConditionKind::ConstraintValue,
        predicates: HashSet::from([ConfigPredicate::Constraint(condition)]),
    })
}

/// Return true when Bazel gives matching `candidate` priority over matching
/// `other` in the current workspace configuration scope.
///
/// Among targets of the same condition kind, specialization is a strict
/// predicate-set superset. Across kinds, a config_setting outranks a direct
/// constraint_value regardless of predicate-set comparability. Alias targets
/// inherit both the kind and predicates of their terminal destination.
pub fn config_condition_strictly_specializes(candidate: &str, other: &str) -> bool {
    config_condition_strictly_specializes_in_scope(
        &configuration_scope(),
        candidate,
        other,
    )
}

/// Explicit-scope form of [`config_condition_strictly_specializes`].
pub fn config_condition_strictly_specializes_in_scope(
    scope: &str,
    candidate: &str,
    other: &str,
) -> bool {
    let Some(candidate) =
        condition_predicates_inner(scope, candidate, &mut HashSet::new())
    else {
        return false;
    };
    let Some(other) = condition_predicates_inner(scope, other, &mut HashSet::new()) else {
        return false;
    };
    match (candidate.kind, other.kind) {
        // Bazel gives a matching config_setting priority over a matching direct
        // constraint_value select key even when their predicate sets are equal
        // or incomparable. Aliases inherit the terminal target's kind above.
        (
            ConfigurationConditionKind::ConfigSetting,
            ConfigurationConditionKind::ConstraintValue,
        ) => true,
        (
            ConfigurationConditionKind::ConstraintValue,
            ConfigurationConditionKind::ConfigSetting,
        ) => false,
        _ => {
            candidate.predicates.len() > other.predicates.len()
                && candidate.predicates.is_superset(&other.predicates)
        }
    }
}

fn rule_label_forms(repository: Option<&str>, package: &str, name: &str) -> Vec<String> {
    let repo_relative = if package.is_empty() {
        format!("//:{}", name)
    } else {
        format!("//{}:{}", package, name)
    };
    if let Some(repo) = repository {
        return dedup_labels(if package.is_empty() {
            vec![
                format!("@{}//:{}", repo, name),
                format!("@@{}//:{}", repo, name),
            ]
        } else {
            vec![
                format!("@{}//{}:{}", repo, package, name),
                format!("@@{}//{}:{}", repo, package, name),
            ]
        });
    }
    dedup_labels(vec![repo_relative, format!(":{}", name)])
}

/// Canonical identity forms for configuration-condition targets.
///
/// Unlike build-setting value lookup, condition matching must not publish a
/// package-free `:name` or an external repository's repo-relative `//pkg:name`:
/// either form could satisfy a select in a different package or repository.
fn condition_label_forms(
    repository: Option<&str>,
    package: &str,
    name: &str,
) -> Vec<String> {
    match repository {
        Some(repository) if package.is_empty() => vec![
            format!("@{}//:{}", repository, name),
            format!("@@{}//:{}", repository, name),
        ],
        Some(repository) => vec![
            format!("@{}//{}:{}", repository, package, name),
            format!("@@{}//{}:{}", repository, package, name),
        ],
        None if package.is_empty() => vec![format!("//:{}", name)],
        None => vec![format!("//{}:{}", package, name)],
    }
}

fn reference_label_forms(
    repository: Option<&str>,
    package: &str,
    label: &str,
) -> Vec<String> {
    if label == "//conditions:default" {
        return vec![label.to_string()];
    }
    if let Some(rest) = label
        .strip_prefix("@//")
        .or_else(|| label.strip_prefix("@@//"))
    {
        return vec![format!("//{rest}")];
    }
    if label.starts_with('@') {
        return vec![label.to_string()];
    }
    if let Some(rest) = label.strip_prefix("//") {
        if let Some(repo) = repository {
            return vec![
                format!("@{}//{}", repo, rest),
                format!("@@{}//{}", repo, rest),
            ];
        }
        return vec![label.to_string()];
    }

    let name = label.strip_prefix(':').unwrap_or(label);
    condition_label_forms(repository, package, name)
}

fn canonical_reference_label(
    repository: Option<&str>,
    package: &str,
    label: &str,
) -> String {
    reference_label_forms(repository, package, label)
        .iter()
        .map(|label| canonical_configuration_label(label))
        .min()
        .unwrap_or_else(|| canonical_configuration_label(label))
}

fn configuration_getenv(name: &str) -> Option<String> {
    if rush_incremental::tracked::current().is_some() {
        return rush_incremental::tracked::getenv_str(name);
    }
    #[cfg(test)]
    {
        return std::env::var(name).ok();
    }
    #[cfg(not(test))]
    {
        // Preserve the analysis layer's fail-loud contract outside unit tests.
        rush_incremental::tracked::getenv_str(name)
    }
}

fn activate_generated_repository_settings() {
    let scope = configuration_scope();
    let cuda_env_enabled = COMMAND_CONFIGURATION_ENVIRONMENTS
        .read()
        .unwrap()
        .get(&scope)
        .is_some_and(|environment| environment.enable_cuda);
    let defines = active_defines_for_scope(&scope);
    let mut active = ACTIVE_CONFIG_SETTINGS.write().unwrap();
    let mut insert = |label: &str| active.insert((scope.clone(), label.to_string()));

    let cuda_enabled = defines.get("CUDA_ENABLED").map(String::as_str) == Some("1")
        || cuda_env_enabled;
    if cuda_enabled {
        insert("@local_config_cuda//:enable_cuda");
        insert("@local_config_cuda//:is_cuda_enabled");
        insert("@local_tsl//tsl:is_cuda_enabled");
        insert("@local_tsl//tsl:is_cuda_enabled_and_oss");
        insert("//tsl:is_cuda_enabled");
        insert("//tsl:is_cuda_enabled_and_oss");
        insert("//tensorflow/core/kernels/mlir_generated:is_gpu_enabled");
    }
    match defines.get("CUDA_COMPILER").map(String::as_str) {
        Some("clang") => {
            insert("@local_config_cuda//:is_cuda_compiler_clang");
            insert("@local_config_cuda//cuda:using_clang");
        }
        Some("nvcc") => {
            insert("@local_config_cuda//:is_cuda_compiler_nvcc");
            insert("@local_config_cuda//cuda:using_nvcc");
        }
        _ => {}
    }
}

fn recompute_composed_config_settings() {
    let _guard = COMPOSED_CONFIG_RECOMPUTE.lock().unwrap();
    let scope = configuration_scope();
    let aliases: Vec<SeenConfigAlias> = SEEN_CONFIG_ALIASES
        .read()
        .unwrap()
        .iter()
        .filter(|(key, _)| key.0 == scope)
        .map(|(_, alias)| alias.clone())
        .collect();

    let mut active: HashSet<String> = ACTIVE_CONFIG_SETTINGS
        .read()
        .unwrap()
        .iter()
        .filter(|(active_scope, _)| active_scope == &scope)
        .map(|(_, label)| label.clone())
        .collect();

    loop {
        let mut changed = false;
        for alias in &aliases {
            if alias
                .actual_labels
                .iter()
                .any(|label| {
                    active.contains(label)
                        || intrinsic_platform_constraint_matches(label) == Some(true)
                })
            {
                for label in &alias.labels {
                    changed |= active.insert(label.clone());
                }
            }
        }
        if !changed {
            break;
        }
    }

    ACTIVE_CONFIG_SETTINGS
        .write()
        .unwrap()
        .extend(active.into_iter().map(|label| (scope.clone(), label)));
}

/// Register the default value of a Starlark build-setting rule.
pub fn consider_build_setting_default(
    repository: Option<&str>,
    package: &str,
    name: &str,
    value: &str,
) {
    consider_typed_build_setting_default(
        repository,
        package,
        name,
        BuildSettingValue::String(value.to_string()),
    );
}

/// Register a typed Starlark build-setting default.
pub fn consider_typed_build_setting_default(
    repository: Option<&str>,
    package: &str,
    name: &str,
    value: BuildSettingValue,
) {
    let scope = configuration_scope();
    SEEN_BUILD_SETTINGS.write().unwrap().insert(
        scoped_rule_key(repository, package, name),
        SeenBuildSetting {
            labels: rule_label_forms(repository, package, name),
            value,
        },
    );
    recompute_build_setting_values(&scope);
    recompute_active_config_settings();
    recompute_composed_config_settings();
}

fn recompute_build_setting_values(scope: &str) {
    let seen = SEEN_BUILD_SETTINGS.read().unwrap();
    let aliases: Vec<SeenConfigAlias> = SEEN_CONFIG_ALIASES
        .read()
        .unwrap()
        .iter()
        .filter(|(key, _)| key.0 == scope)
        .map(|(_, alias)| alias.clone())
        .collect();
    let mut values = SCOPED_BUILD_SETTING_VALUES.write().unwrap();
    values.retain(|(value_scope, _), _| value_scope != scope);
    for setting in seen
        .iter()
        .filter(|(key, _)| key.0 == scope)
        .map(|(_, setting)| setting)
    {
        for label in &setting.labels {
            values.insert((scope.to_string(), label.clone()), setting.value.clone());
        }
    }
    loop {
        let mut changed = false;
        for alias in &aliases {
            let actual = alias.actual_labels.iter().find_map(|label| {
                values
                    .get(&(scope.to_string(), label.clone()))
                    .cloned()
            });
            let Some(actual) = actual else {
                continue;
            };
            for label in &alias.labels {
                let key = (scope.to_string(), label.clone());
                if values.get(&key) != Some(&actual) {
                    values.insert(key, actual.clone());
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
}

/// Read a registered build-setting value (string_flag/bool_flag default or
/// CLI override) by any of its label forms, e.g. "//:mojo_base_version".
pub fn build_setting_value(label: &str) -> Option<String> {
    build_setting_value_in_scope(&configuration_scope(), label)
}

/// Read a build-setting value in an explicit workspace configuration scope.
/// Async rule execution must use this form because thread-local analysis scope
/// does not cross an `.await` or worker-thread boundary.
pub fn build_setting_value_in_scope(scope: &str, label: &str) -> Option<String> {
    SCOPED_BUILD_SETTING_VALUES
        .read()
        .unwrap()
        .get(&(scope.to_string(), label.to_string()))
        .map(BuildSettingValue::display)
}

/// Register a `config_setting(flag_values = {...})`.
pub fn consider_flag_config_setting(
    repository: Option<&str>,
    package: &str,
    name: &str,
    flag_values: &HashMap<String, String>,
) {
    if flag_values.is_empty() {
        return;
    }
    let key = scoped_rule_key(repository, package, name);
    let mut seen = SEEN_CONFIG_SETTINGS.write().unwrap();
    let setting = seen.entry(key).or_insert_with(|| SeenConfigSetting {
        labels: condition_label_forms(repository, package, name),
        ..SeenConfigSetting::default()
    });
    setting.flag_values = flag_values
        .iter()
        .map(|(label, value)| {
            (
                reference_label_forms(repository, package, label),
                value.clone(),
            )
        })
        .collect();
    drop(seen);
    recompute_active_config_settings();
    recompute_composed_config_settings();
}

/// Register a Bazel `alias` that may resolve to a configuration condition.
pub fn consider_config_alias(
    repository: Option<&str>,
    package: &str,
    name: &str,
    actual: &str,
) {
    SEEN_CONFIG_ALIASES.write().unwrap().insert(
        scoped_rule_key(repository, package, name),
        SeenConfigAlias {
            labels: condition_label_forms(repository, package, name),
            actual_labels: reference_label_forms(repository, package, actual),
        },
    );
    recompute_build_setting_values(&configuration_scope());
    recompute_active_config_settings();
    recompute_composed_config_settings();
}

/// Register a declared constraint_value as a legal select key. Canonical
/// @platforms values are matched intrinsically; project-defined values remain
/// inactive unless a future target-platform implementation selects them.
pub fn consider_constraint_value(repository: Option<&str>, package: &str, name: &str) {
    SEEN_CONSTRAINT_VALUES.write().unwrap().insert(
        scoped_rule_key(repository, package, name),
        condition_label_forms(repository, package, name),
    );
}

#[cfg(test)]
fn is_known_config_condition(condition: &str) -> bool {
    if condition == "//conditions:default"
        || intrinsic_platform_constraint_matches(condition).is_some()
    {
        return true;
    }
    let scope = configuration_scope();
    SEEN_CONFIG_SETTINGS
        .read()
        .unwrap()
        .iter()
        .filter(|(key, _)| key.0 == scope)
        .any(|(_, setting)| setting.labels.iter().any(|label| label == condition))
        || SEEN_CONFIG_ALIASES
            .read()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.0 == scope)
            .any(|(_, alias)| alias.labels.iter().any(|label| label == condition))
        || SEEN_CONSTRAINT_VALUES
            .read()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.0 == scope)
            .any(|(_, labels)| labels.iter().any(|label| label == condition))
}

/// Re-run alias composition after a direct `config_setting` is registered.
pub fn refresh_composed_config_settings() {
    recompute_composed_config_settings();
}

/// Add/override an active define (from the `--define K=V` CLI flag).
pub fn set_define(key: String, value: String) {
    ACTIVE_DEFINES
        .write()
        .unwrap()
        .entry(configuration_scope())
        .or_default()
        .insert(key, value);
}

/// Per-command reset/install of the active configuration.
///
/// The daemon serves many client commands from one long-lived process, each
/// with its OWN `--define`s (reconstructed into [`CommandSemantics`] from the
/// wire request). The process-global definition registry persists across
/// commands, so select() resolution must rebuild the active set instead of
/// retaining matches from a prior request.
///
/// Call this at the start of every command (both the daemon path and the
/// single-shot CLI) with that command's defines. It (1) replaces ACTIVE_DEFINES
/// with exactly these defines, and (2) recomputes ACTIVE_CONFIG_SETTINGS from
/// scratch against the SEEN_CONFIG_SETTINGS registry, so any config_setting
/// already observed in this process is (de)activated correctly for the new
/// command and host configuration, independent of parse order or BUILD-cache
/// hits. Config settings not yet seen are activated as they are parsed.
///
/// Idempotent. With no defines, declared native-value and platform-constraint
/// settings can still match the host.
pub fn begin_command(defines: &[(String, String)]) {
    begin_command_with_native_options(defines, NativeConfiguration::default());
}

/// Install the immutable command's native option values together with defines.
/// Every workspace scope is reset, including options omitted by this command.
pub fn begin_command_with_native_options(
    defines: &[(String, String)],
    mut native_configuration: NativeConfiguration,
) {
    let scope = configuration_scope();
    for (key, value) in defines {
        native_configuration.defines.entry(key.clone()).or_insert_with(|| value.clone());
    }
    NATIVE_CONFIGURATIONS.write().unwrap().insert(scope.clone(), native_configuration);
    let command_environment = CommandConfigurationEnvironment {
        enable_cuda: configuration_getenv("RUSH_ENABLE_CUDA").as_deref() == Some("1"),
        pycross_python_version: configuration_getenv("RUSH_PY_VERSION")
            .map(|value| value.replace('_', "."))
            .unwrap_or_else(|| "3.13".to_string()),
    };
    COMMAND_CONFIGURATION_ENVIRONMENTS
        .write()
        .unwrap()
        .insert(scope.clone(), command_environment);
    {
        let mut all_active = ACTIVE_DEFINES.write().unwrap();
        let active = all_active.entry(scope).or_default();
        active.clear();
        for (k, v) in defines {
            active.insert(k.clone(), v.clone());
        }
    }
    recompute_active_config_settings();
    recompute_composed_config_settings();
}

/// Discard configuration declarations retained from an earlier command in
/// the current workspace scope, while preserving that scope's active
/// `--define` values.
///
/// BUILD package snapshots are reconstructed from tracked source on every
/// command. Clearing the declaration side first is what makes deleting a
/// package's BUILD file equivalent to replacing that package with an empty
/// snapshot: a config_setting, alias, build-setting default, or constraint
/// value cannot survive merely because no replacement file exists to scan.
pub fn clear_configuration_registry_for_command() {
    let scope = configuration_scope();
    SEEN_CONFIG_SETTINGS
        .write()
        .unwrap()
        .retain(|key, _| key.0 != scope);
    SEEN_CONFIG_ALIASES
        .write()
        .unwrap()
        .retain(|key, _| key.0 != scope);
    SEEN_BUILD_SETTINGS
        .write()
        .unwrap()
        .retain(|key, _| key.0 != scope);
    SEEN_CONSTRAINT_VALUES
        .write()
        .unwrap()
        .retain(|key, _| key.0 != scope);
    SCOPED_BUILD_SETTING_VALUES
        .write()
        .unwrap()
        .retain(|(value_scope, _), _| value_scope != &scope);
    ACTIVE_CONFIG_SETTINGS
        .write()
        .unwrap()
        .retain(|(active_scope, _)| active_scope != &scope);
}

/// Replace one configuration package with an empty snapshot in the current
/// workspace scope. This is used when a previously loaded BUILD file has been
/// deleted or renamed; leaving its declarations in a resident process would
/// let removed conditions or aliases remain active on a later command.
pub fn clear_configuration_package(repository: Option<&str>, package: &str) {
    let scope = configuration_scope();
    let repository = repository.unwrap_or_default();
    let same_package = |key: &ScopedRuleKey| {
        key.0 == scope && key.1 == repository && key.2 == package
    };
    SEEN_CONFIG_SETTINGS
        .write()
        .unwrap()
        .retain(|key, _| !same_package(key));
    SEEN_CONFIG_ALIASES
        .write()
        .unwrap()
        .retain(|key, _| !same_package(key));
    SEEN_BUILD_SETTINGS
        .write()
        .unwrap()
        .retain(|key, _| !same_package(key));
    SEEN_CONSTRAINT_VALUES
        .write()
        .unwrap()
        .retain(|key, _| !same_package(key));
    recompute_build_setting_values(&scope);
    recompute_active_config_settings();
    recompute_composed_config_settings();
}

/// Public entry point to recompute the active config_setting set from the
/// registry against the current defines. Called by the orchestrator's
/// config_setting pre-registration pass after it registers a package's
/// definitions, so a freshly registered cross-package config_setting is
/// activated before any select keyed on it resolves. Idempotent.
pub fn recompute_active_config_settings_public() {
    recompute_active_config_settings();
    recompute_composed_config_settings();
}

/// Rebuild [`ACTIVE_CONFIG_SETTINGS`] from complete observed predicates.
fn recompute_active_config_settings() {
    let scope = configuration_scope();
    let defs = active_defines_for_scope(&scope);
    let native_configuration = NATIVE_CONFIGURATIONS.read().unwrap()
        .get(&scope).cloned().unwrap_or_default();
    let seen = SEEN_CONFIG_SETTINGS.read().unwrap();
    let build_settings = SCOPED_BUILD_SETTING_VALUES.read().unwrap();
    let mut active = ACTIVE_CONFIG_SETTINGS.write().unwrap();
    active.retain(|(active_scope, _)| active_scope != &scope);
    for (_key, setting) in seen.iter().filter(|(key, _)| key.0 == scope) {
        let has_predicate = !setting.define_values.is_empty()
            || !setting.native_values.is_empty()
            || !setting.constraint_values.is_empty()
            || !setting.flag_values.is_empty();
        let defines_match = setting
            .define_values
            .iter()
            .all(|(key, value)| defs.get(key) == Some(value));
        let native_values_match = setting.native_values.iter().all(|(key, wanted)| {
            native_configuration.matches(key, wanted).unwrap_or(false)
        });
        let constraints_match = setting.constraint_values.iter().all(|constraint| {
            resolved_config_reference(&scope, constraint)
                .and_then(|label| intrinsic_platform_constraint_matches(&label))
                == Some(true)
        });
        let flag_values_match = setting.flag_values.iter().all(|(labels, expected)| {
            labels.iter().any(|label| {
                build_settings
                    .get(&(scope.clone(), label.clone()))
                    .is_some_and(|actual| actual.matches_predicate(expected))
            })
        });
        if has_predicate
            && defines_match
            && native_values_match
            && constraints_match
            && flag_values_match
        {
            active.extend(
                setting
                    .labels
                    .iter()
                    .cloned()
                    .map(|label| (scope.clone(), label)),
            );
        }
    }
    drop(active);
    // Generated repository compatibility labels are another scoped predicate
    // source, not one-time inserts. Re-union them after every registry
    // recomputation so parsing or deleting an ordinary config_setting cannot
    // erase CUDA conditions activated for the current command.
    activate_generated_repository_settings();
}

/// True if any define is active (used to bust parse caches that would
/// otherwise serve config-blind cached attribute values).
pub fn has_active_defines() -> bool {
    !active_defines_for_scope(&configuration_scope()).is_empty()
}

/// Snapshot of the active defines (diagnostics).
pub fn active_defines() -> HashMap<String, String> {
    active_defines_for_scope(&configuration_scope())
}

/// Deterministic fingerprint of the active `--define`s for config-sensitive
/// cache keys. Returns the empty string when no defines are active. Values are
/// sorted so the fingerprint is order-independent.
pub fn defines_fingerprint() -> String {
    let defs = active_defines_for_scope(&configuration_scope());
    if defs.is_empty() {
        return String::new();
    }
    let mut kvs: Vec<(&String, &String)> = defs.iter().collect();
    kvs.sort();
    let mut s = String::new();
    for (k, v) in kvs {
        if !s.is_empty() {
            s.push(',');
        }
        s.push_str(k);
        s.push('=');
        s.push_str(v);
    }
    s
}

/// Register a complete literal native `config_setting` predicate.
///
/// Bazel requires every populated predicate dimension to match. Store the
/// dimensions as one definition and recompute instead of independently ORing
/// matches from `define_values`, `values`, and `constraint_values`.
/// `flag_values` match only loaded typed build-setting defaults. Unknown or
/// unsupported build-setting types remain inactive rather than guessing.
pub fn consider_config_setting_predicates(
    repository: Option<&str>,
    package: &str,
    name: &str,
    define_values: &HashMap<String, String>,
    native_values: &HashMap<String, String>,
    constraint_values: &[String],
    flag_values: &HashMap<String, String>,
) {
    SEEN_CONFIG_SETTINGS.write().unwrap().insert(
        scoped_rule_key(repository, package, name),
        SeenConfigSetting {
            labels: condition_label_forms(repository, package, name),
            define_values: define_values.clone(),
            native_values: native_values.clone(),
            constraint_values: constraint_values
                .iter()
                .map(|label| canonical_reference_label(repository, package, label))
                .collect(),
            flag_values: flag_values
                .iter()
                .map(|(label, value)| {
                    (
                        reference_label_forms(repository, package, label),
                        value.clone(),
                    )
                })
                .collect(),
        },
    );
    recompute_active_config_settings();
}

/// Register a `define_values`-only config setting.
pub fn consider_config_setting(
    package: &str,
    name: &str,
    define_values: &HashMap<String, String>,
) {
    consider_config_setting_predicates(
        None,
        package,
        name,
        define_values,
        &HashMap::new(),
        &[],
        &HashMap::new(),
    );
}

/// Evaluate Bazel-native `config_setting(values = {...})` entries whose value
/// is intrinsic to the Rush host. Bazel spells x86-64's legacy CPU value as
/// `k8`; TensorFlow's CUDA stub generators use that setting to select their
/// x86-64 implementation.
pub fn consider_native_config_setting(
    package: &str,
    name: &str,
    values: &HashMap<String, String>,
) {
    consider_config_setting_predicates(
        None,
        package,
        name,
        &HashMap::new(),
        values,
        &[],
        &HashMap::new(),
    );
}

/// Activate a `config_setting(constraint_values = [...])` when every listed
/// canonical `@platforms` constraint matches Rush's host configuration.
///
/// Rush does not synthesize aliases such as `//conditions:linux`. A project
/// that wants that spelling must declare it explicitly and connect it to a
/// real platform constraint, for example:
///
/// ```starlark
/// config_setting(
///     name = "linux",
///     constraint_values = ["@platforms//os:linux"],
/// )
/// ```
pub fn consider_constraint_config_setting(
    package: &str,
    name: &str,
    constraint_values: &[String],
) {
    consider_config_setting_predicates(
        None,
        package,
        name,
        &HashMap::new(),
        &HashMap::new(),
        constraint_values,
        &HashMap::new(),
    );
}

/// True if a canonical select-condition label matches the current build or
/// host configuration. The rule bridge supplies package/repository context
/// before calling this matcher.
pub fn is_active_config_setting(condition: &str) -> bool {
    is_active_config_setting_in_scope(&configuration_scope(), condition)
}

/// Explicit-scope form of [`is_active_config_setting`]. Select evaluators
/// retain their workspace identity and use this form after async or worker
/// thread boundaries, where a thread-local scope cannot be inherited safely.
pub fn is_active_config_setting_in_scope(scope: &str, condition: &str) -> bool {
    // A module can evaluate a select while producing cached exports. Its
    // captured dependency set must therefore invalidate on native options as
    // well as defines; the registry's current value alone is not a cache key.
    if rush_incremental::tracked::current().is_some() {
        rush_incremental::tracked::config_fingerprint();
    }
    let explicit_main = condition
        .strip_prefix("@//")
        .or_else(|| condition.strip_prefix("@@//"))
        .map(|rest| format!("//{}", rest));
    let condition = explicit_main.as_deref().unwrap_or(condition);
    if ACTIVE_CONFIG_SETTINGS
        .read()
        .unwrap()
        .contains(&(scope.to_string(), condition.to_string()))
    {
        return true;
    }

    // A constraint_value is itself a legal select key, but only after its
    // target has been loaded and registered.  This keeps canonical
    // @platforms matching useful without recreating the retired behavior of
    // synthesizing arbitrary labels from their spelling alone.
    let declared_constraint = SEEN_CONSTRAINT_VALUES
        .read()
        .unwrap()
        .iter()
        .filter(|(key, _)| key.0 == scope)
        .any(|(_, labels)| {
            labels.iter().any(|label| {
                canonical_configuration_label(label)
                    == canonical_configuration_label(condition)
            })
        });
    if declared_constraint
        && intrinsic_platform_constraint_matches(condition) == Some(true)
    {
        return true;
    }

    // Preserve Rush's pre-existing rules_pycross default-environment bridge.
    // Modular's generated `_env_python_*` keys are real native alias targets,
    // often backed by Skylib configurable-alias chains whose inner predicates
    // use flag_values (a deliberate fail-closed boundary in this TODO). Match
    // only after that exact alias target has been loaded; arbitrary labels or
    // a config_setting with the same basename are never synthesized.
    let declared_alias = SEEN_CONFIG_ALIASES
        .read()
        .unwrap()
        .iter()
        .filter(|(key, _)| key.0 == scope)
        .find_map(|(_, alias)| {
            alias.labels.iter().any(|label| {
                canonical_configuration_label(label)
                    == canonical_configuration_label(condition)
            }).then_some(alias.clone())
        });
    declared_alias.is_some_and(|alias| {
        alias.actual_labels.iter().any(|actual| {
            is_rules_pycross_environment_config_label(actual)
        })
        && is_pycross_environment_compatibility_match_in_scope(scope, condition)
    })
}

fn is_rules_pycross_environment_config_label(label: &str) -> bool {
    let canonical = canonical_configuration_label(label);
    let Some(rest) = canonical.strip_prefix(
        "@rules_pycross++environments+rules_pycross_all_environments//",
    ) else {
        return false;
    };
    let target = rest.strip_prefix(':').unwrap_or(rest);
    target.starts_with("python_") && target.ends_with("_config")
}

/// Whether a generated rules_pycross alias is the one narrow base host/version
/// form whose pre-existing compatibility bridge may suppress its terminal
/// flag_values diagnostic. GPU and freethreaded groups are never included.
pub fn is_pycross_environment_compatibility_match(condition: &str) -> bool {
    is_pycross_environment_compatibility_match_in_scope(
        &configuration_scope(),
        condition,
    )
}

/// Explicit-scope form used after async boundaries. It consults only the
/// environment snapshot captured by [`begin_command`]; it never performs a
/// fresh tracked environment read.
pub fn is_pycross_environment_compatibility_match_in_scope(
    scope: &str,
    condition: &str,
) -> bool {
    let expected_actual = SEEN_CONFIG_ALIASES
        .read()
        .unwrap()
        .iter()
        .filter(|(key, _)| key.0 == scope)
        .find(|(_, alias)| {
            alias.labels.iter().any(|label| {
                canonical_configuration_label(label)
                    == canonical_configuration_label(condition)
            })
        })
        .is_some_and(|(_, alias)| {
            alias
                .actual_labels
                .iter()
                .any(|actual| is_rules_pycross_environment_config_label(actual))
        });
    if !expected_actual {
        return false;
    }
    let name = condition
        .rsplit_once(':')
        .map(|(_, name)| name)
        .unwrap_or(condition);
    let Some(rest) = name.strip_prefix("_env_python_") else {
        return false;
    };
    let Some((version, triple)) = rest.split_once('_') else {
        return false;
    };
    if triple.ends_with("_gpu") || triple.ends_with("-freethreaded") {
        return false;
    }
    let host_triple = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else {
        return false;
    };
    let active_version = COMMAND_CONFIGURATION_ENVIRONMENTS
        .read()
        .unwrap()
        .get(scope)
        .map(|environment| environment.pycross_python_version.clone())
        .unwrap_or_else(|| "3.13".to_string());
    triple == host_triple && version == active_version
}

/// Return whether a label has the generated rules_pycross environment-alias
/// basename. This is a syntactic guard only; activation additionally requires
/// a loaded native alias and the host/version match above.
pub fn is_pycross_environment_alias_label(condition: &str) -> bool {
    let name = condition
        .rsplit_once(':')
        .map(|(_, name)| name)
        .unwrap_or(condition);
    let Some(rest) = name.strip_prefix("_env_python_") else {
        return false;
    };
    let Some((version, platform)) = rest.split_once('_') else {
        return false;
    };
    let mut version_parts = version.split('.');
    if !matches!(
        (version_parts.next(), version_parts.next(), version_parts.next()),
        (Some(major), Some(minor), None)
            if !major.is_empty()
                && major.chars().all(|c| c.is_ascii_digit())
                && !minor.is_empty()
                && minor.chars().all(|c| c.is_ascii_digit())
    ) {
        return false;
    }
    let base_platform = platform
        .strip_suffix("_nvidia_gpu")
        .or_else(|| platform.strip_suffix("_amd_gpu"))
        .unwrap_or(platform);
    matches!(
        base_platform,
        "x86_64-unknown-linux-gnu"
            | "aarch64-unknown-linux-gnu"
            | "aarch64-apple-darwin"
    )
}

/// Match a canonical platform constraint directly against Rush's host.
///
/// Return `None` for labels outside the supported `@platforms` OS/CPU
/// namespaces so callers cannot accidentally treat an arbitrary label as a
/// host constraint.
fn intrinsic_platform_constraint_matches(condition: &str) -> Option<bool> {
    // Bazel accepts source (@platforms//...) and canonical
    // (@@platforms//...) spellings for the same repository.
    let stripped = condition.trim_start_matches('@');
    if let Some(os) = stripped.strip_prefix("platforms//os:") {
        return Some(match os {
            "linux" => cfg!(target_os = "linux"),
            "macos" | "osx" => cfg!(target_os = "macos"),
            "windows" => cfg!(target_os = "windows"),
            "freebsd" => cfg!(target_os = "freebsd"),
            // android/ios/none/... are never the host platform here.
            _ => false,
        });
    }
    if let Some(cpu) = stripped.strip_prefix("platforms//cpu:") {
        return Some(match cpu {
            "x86_64" => cfg!(target_arch = "x86_64"),
            "aarch64" | "arm64" => cfg!(target_arch = "aarch64"),
            "armv7" | "arm" => cfg!(target_arch = "arm"),
            "s390x" => cfg!(target_arch = "s390x"),
            "riscv64" => cfg!(target_arch = "riscv64"),
            "ppc64le" => {
                cfg!(all(target_arch = "powerpc64", target_endian = "little"))
            }
            _ => false,
        });
    }
    None
}

/// Replace one package's pre-scanned literal configuration declarations.
///
/// This scans top-level `config_setting` calls with supported literal
/// `define_values`, native `values`, canonical `@platforms`
/// `constraint_values`, and deferred `flag_values` predicates. Replacing the package
/// snapshot prevents a daemon from retaining a deleted or edited declaration
/// across commands.
///
/// The scan mirrors the evaluator before it later evaluates the rule. This lets
/// the
/// orchestrator pre-register a package's config_settings BEFORE any select
/// keyed on them resolves, removing the parse-order dependence that made TF's
/// cross-package `//tensorflow/lite:tflite_with_xnnpack_explicit_false` select
/// (referenced from `tensorflow/lite/kernels/BUILD`, declared in
/// `tensorflow/lite/BUILD`) resolve to the wrong branch when the kernels
/// package happened to be evaluated first.
///
/// Only declarations with a literal string `name` and literal supported
/// attributes are registered. Macro-generated conditions remain the normal
/// evaluator's responsibility.
pub fn register_config_settings_from_program(
    repository: Option<&str>,
    package: &str,
    program: &Program,
) {
    let mut settings = HashMap::new();
    let mut aliases = HashMap::new();
    let mut constraint_values = HashMap::new();
    for stmt in &program.statements {
        if let Stmt::Expr(expr) = stmt {
            if let Expr::Call(call) = expr.as_ref() {
                if let Some((key, setting)) = literal_config_setting(repository, package, call) {
                    settings.insert(key, setting);
                }
                if let Some((key, alias)) = literal_config_alias(repository, package, call) {
                    aliases.insert(key, alias);
                }
                if let Some((key, labels)) = literal_constraint_value(repository, package, call) {
                    constraint_values.insert(key, labels);
                }
            }
        }
    }

    let scope = configuration_scope();
    let repository = repository.unwrap_or_default();
    let same_package = |key: &ScopedRuleKey| {
        key.0 == scope && key.1 == repository && key.2 == package
    };
    {
        let mut seen = SEEN_CONFIG_SETTINGS.write().unwrap();
        seen.retain(|key, _| !same_package(key));
        seen.extend(settings);
    }
    {
        let mut seen = SEEN_CONFIG_ALIASES.write().unwrap();
        seen.retain(|key, _| !same_package(key));
        seen.extend(aliases);
    }
    {
        let mut seen = SEEN_BUILD_SETTINGS.write().unwrap();
        seen.retain(|key, _| !same_package(key));
    }
    {
        let mut seen = SEEN_CONSTRAINT_VALUES.write().unwrap();
        seen.retain(|key, _| !same_package(key));
        seen.extend(constraint_values);
    }
    recompute_build_setting_values(&scope);
    recompute_active_config_settings();
    recompute_composed_config_settings();
}

fn call_callee_name(call: &CallExpr) -> Option<&str> {
    match call.function.as_ref() {
        Expr::Ident(name) => Some(name.as_str()),
        _ => None,
    }
}

fn literal_config_setting(
    repository: Option<&str>,
    package: &str,
    call: &CallExpr,
) -> Option<(ScopedRuleKey, SeenConfigSetting)> {
    if call_callee_name(call) != Some("config_setting") {
        return None;
    }
    let mut name: Option<String> = None;
    let mut define_values: Option<HashMap<String, String>> = None;
    let mut native_values: Option<HashMap<String, String>> = None;
    let mut constraint_values: Option<Vec<String>> = None;
    let mut flag_values: Option<HashMap<String, String>> = None;
    for (kw, val) in &call.kwargs {
        match kw.as_str() {
            "name" => {
                if let Expr::String(s) = val {
                    name = Some(s.clone());
                }
            }
            "define_values" => {
                let Expr::Dict(pairs) = val else {
                    return None;
                };
                let mut m = HashMap::new();
                for (k, v) in pairs {
                    let (Some(Expr::String(k)), Expr::String(v)) = (k.as_ref(), v) else {
                        return None;
                    };
                    m.insert(k.clone(), v.clone());
                }
                define_values = Some(m);
            }
            "values" => {
                let Expr::Dict(pairs) = val else {
                    return None;
                };
                let mut m = HashMap::new();
                for (k, v) in pairs {
                    let (Some(Expr::String(k)), Expr::String(v)) = (k.as_ref(), v) else {
                        return None;
                    };
                    m.insert(k.clone(), v.clone());
                }
                native_values = Some(m);
            }
            "constraint_values" => {
                let values = match val {
                    Expr::List(values) | Expr::Tuple(values) => values,
                    _ => return None,
                };
                constraint_values = Some(
                    values
                        .iter()
                        .map(|value| match value {
                            Expr::String(value) => Some(value.clone()),
                            _ => None,
                        })
                        .collect::<Option<Vec<_>>>()?,
                );
            }
            "flag_values" => {
                let Expr::Dict(pairs) = val else {
                    return None;
                };
                let mut values = HashMap::new();
                for (key, value) in pairs {
                    let (Some(Expr::String(key)), Expr::String(value)) =
                        (key.as_ref(), value)
                    else {
                        return None;
                    };
                    values.insert(key.clone(), value.clone());
                }
                flag_values = Some(values);
            }
            _ => {}
        }
    }
    let name = name?;
    let flag_values = flag_values.unwrap_or_default();
    Some((
        scoped_rule_key(repository, package, &name),
        SeenConfigSetting {
            labels: condition_label_forms(repository, package, &name),
            define_values: define_values.unwrap_or_default(),
            native_values: native_values.unwrap_or_default(),
            constraint_values: constraint_values
                .unwrap_or_default()
                .iter()
                .map(|label| canonical_reference_label(repository, package, label))
                .collect(),
            flag_values: flag_values
                .iter()
                .map(|(label, value)| {
                    (
                        reference_label_forms(repository, package, label),
                        value.clone(),
                    )
                })
                .collect(),
        },
    ))
}

fn literal_config_alias(
    repository: Option<&str>,
    package: &str,
    call: &CallExpr,
) -> Option<(ScopedRuleKey, SeenConfigAlias)> {
    if call_callee_name(call) != Some("alias") {
        return None;
    }
    let mut name = None;
    let mut actual = None;
    for (kw, value) in &call.kwargs {
        match (kw.as_str(), value) {
            ("name", Expr::String(value)) => name = Some(value.clone()),
            ("actual", Expr::String(value)) => actual = Some(value.clone()),
            _ => {}
        }
    }
    let name = name?;
    let actual = actual?;
    Some((
        scoped_rule_key(repository, package, &name),
        SeenConfigAlias {
            labels: condition_label_forms(repository, package, &name),
            actual_labels: reference_label_forms(repository, package, &actual),
        },
    ))
}

fn literal_constraint_value(
    repository: Option<&str>,
    package: &str,
    call: &CallExpr,
) -> Option<(ScopedRuleKey, Vec<String>)> {
    if call_callee_name(call) != Some("constraint_value") {
        return None;
    }
    let name = call.kwargs.iter().find_map(|(attribute, value)| {
        (attribute == "name")
            .then_some(value)
            .and_then(|value| match value {
                Expr::String(value) => Some(value.clone()),
                _ => None,
            })
    })?;
    Some((
        scoped_rule_key(repository, package, &name),
        condition_label_forms(repository, package, &name),
    ))
}


/// Collect every `select({...})` condition-key label appearing anywhere in a
/// parsed BUILD `Program`. The orchestrator uses these to know which other
/// packages' config_settings must be pre-registered before this BUILD is
/// evaluated (a select keyed on `//pkg:name` needs `pkg/BUILD`'s config_settings
/// known). Returns absolute (`//pkg:name`) and package-relative (`:name`)
/// condition strings exactly as written. The caller treats only
/// `//conditions:default` as virtual; named `//conditions` targets are scanned
/// like any other package.
pub fn collect_select_condition_labels(program: &Program) -> HashSet<String> {
    let mut out = HashSet::new();
    for stmt in &program.statements {
        collect_select_labels_stmt(stmt, &mut out);
    }
    out
}

fn collect_select_labels_stmt(stmt: &Stmt, out: &mut HashSet<String>) {
    match stmt {
        Stmt::Expr(e) => collect_select_labels_expr(e, out),
        Stmt::Assign { value, .. } | Stmt::AugAssign { value, .. } => {
            collect_select_labels_expr(value, out)
        }
        Stmt::Return(Some(e)) => collect_select_labels_expr(e, out),
        Stmt::FunctionDef { body, .. } | Stmt::For { body, .. } => {
            for s in body {
                collect_select_labels_stmt(s, out);
            }
        }
        Stmt::If {
            then_body,
            elif_parts,
            else_body,
            ..
        } => {
            for s in then_body {
                collect_select_labels_stmt(s, out);
            }
            for (_c, body) in elif_parts {
                for s in body {
                    collect_select_labels_stmt(s, out);
                }
            }
            if let Some(body) = else_body {
                for s in body {
                    collect_select_labels_stmt(s, out);
                }
            }
        }
        _ => {}
    }
}

fn collect_select_labels_expr(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Call(call) => {
            if call_callee_name(call) == Some("select") {
                // First positional arg is the conditions dict.
                if let Some(Expr::Dict(pairs)) = call.args.first() {
                    for (k, _v) in pairs {
                        if let Some(Expr::String(label)) = k.as_ref() {
                            out.insert(label.clone());
                        }
                    }
                }
            }
            for a in &call.args {
                collect_select_labels_expr(a, out);
            }
            for e in &call.args_unpack {
                collect_select_labels_expr(e, out);
            }
            for (_k, v) in &call.kwargs {
                collect_select_labels_expr(v, out);
            }
            for e in &call.kwargs_unpack {
                collect_select_labels_expr(e, out);
            }
        }
        Expr::List(items) | Expr::Tuple(items) => {
            for e in items {
                collect_select_labels_expr(e, out);
            }
        }
        Expr::Dict(pairs) => {
            for (k, v) in pairs {
                if let Some(k) = k {
                    collect_select_labels_expr(k, out);
                }
                collect_select_labels_expr(v, out);
            }
        }
        Expr::BinOp { left, right, .. } => {
            collect_select_labels_expr(left, out);
            collect_select_labels_expr(right, out);
        }
        Expr::UnaryOp { operand, .. } => collect_select_labels_expr(operand, out),
        Expr::Index { object, index } => {
            collect_select_labels_expr(object, out);
            collect_select_labels_expr(index, out);
        }
        Expr::Attribute { object, .. } => collect_select_labels_expr(object, out),
        Expr::IfExpr {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_select_labels_expr(condition, out);
            collect_select_labels_expr(then_expr, out);
            collect_select_labels_expr(else_expr, out);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static CONFIG_STATE_TEST_LOCK: Lazy<std::sync::Mutex<()>> =
        Lazy::new(|| std::sync::Mutex::new(()));

    fn isolate_config_state() -> std::sync::MutexGuard<'static, ()> {
        CONFIG_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn native_options_recompute_per_command_and_remain_workspace_scoped() {
        let _guard = isolate_config_state();
        with_configuration_scope("native-option-scope-a", || {
            begin_command(&[]);
            for (name, values) in [
                ("default", HashMap::from([("apple_platform_type".into(), "macos".into()), ("stamp".into(), "0".into())])),
                ("changed", HashMap::from([("apple_platform_type".into(), "ios".into()), ("copt".into(), "-DSELECTED".into())])),
            ] {
                consider_config_setting_predicates(None, "native_cfg", name, &HashMap::new(), &values, &[], &HashMap::new());
            }
            assert!(is_active_config_setting("//native_cfg:default"));
            assert!(!is_active_config_setting("//native_cfg:changed"));
            let configuration = NativeConfiguration::from_tokens(&["--apple_platform_type=ios".into(), "--copt=-DSELECTED".into()]).unwrap();
            begin_command_with_native_options(&[], configuration);
            assert!(!is_active_config_setting("//native_cfg:default"));
            assert!(is_active_config_setting("//native_cfg:changed"));
            with_configuration_scope("native-option-scope-b", || {
                begin_command(&[]);
                consider_config_setting_predicates(None, "native_cfg", "changed", &HashMap::new(),
                    &HashMap::from([("apple_platform_type".into(), "ios".into())]), &[], &HashMap::new());
                assert!(!is_active_config_setting("//native_cfg:changed"));
            });
            assert!(is_active_config_setting("//native_cfg:changed"));
            begin_command(&[]);
            assert!(is_active_config_setting("//native_cfg:default"));
            assert!(!is_active_config_setting("//native_cfg:changed"));
        });
    }

    #[test]
    fn builtin_default_does_not_activate_configuration_aliases() {
        let _guard = isolate_config_state();
        for (name, package, actual) in [
            (
                "canonical_default_plain",
                "rush_canonical_default_test",
                "//conditions:default",
            ),
            (
                "canonical_default_at_root",
                "rush_canonical_default_test",
                "@//conditions:default",
            ),
            (
                "canonical_default_atat_root",
                "rush_canonical_default_test",
                "@@//conditions:default",
            ),
            (
                "canonical_default_relative",
                "conditions",
                ":default",
            ),
        ] {
            consider_config_alias(None, package, name, actual);
            assert!(!is_active_config_setting(&format!(
                "//{package}:{name}"
            )));
        }
    }

    #[test]
    fn empty_defines_activate_nothing() {
        let _guard = isolate_config_state();
        // No defines set in this isolated path → define_values config_setting
        // is never activated.
        let mut dv = HashMap::new();
        dv.insert("some_unset_key_xyz".to_string(), "false".to_string());
        consider_config_setting("pkg", "cs", &dv);
        assert!(!is_active_config_setting("//pkg:cs"));
        assert!(!is_active_config_setting(":cs"));
    }

    #[test]
    fn begin_command_recomputes_activation_order_independently() {
        let _guard = isolate_config_state();
        // Simulate the daemon hazard: a config_setting is parsed (registered)
        // under one set of defines, then a later command arrives with the
        // define that should activate it. begin_command must activate it from
        // the registry WITHOUT re-parsing (order/cache independence).
        let mut dv = HashMap::new();
        dv.insert("rush_test_xnn_kkk".to_string(), "false".to_string());
        // First seen with the define NOT active → registered but not active.
        begin_command(&[]);
        consider_config_setting("tensorflow/lite", "xnn_false_kkk", &dv);
        assert!(!is_active_config_setting("//tensorflow/lite:xnn_false_kkk"));

        // New command supplies the matching define: recompute activates it
        // from the registry, even though the config_setting is not re-parsed.
        begin_command(&[("rush_test_xnn_kkk".to_string(), "false".to_string())]);
        assert!(is_active_config_setting("//tensorflow/lite:xnn_false_kkk"));
        assert!(!is_active_config_setting(":xnn_false_kkk"));

        // A subsequent command WITHOUT the define must DEactivate it (the set
        // is no longer monotonic — this is what fixes the stale daemon graph).
        begin_command(&[]);
        assert!(!is_active_config_setting("//tensorflow/lite:xnn_false_kkk"));
        assert!(!is_active_config_setting(":xnn_false_kkk"));
    }

    #[test]
    fn generated_cuda_conditions_survive_registry_recomputation() {
        let _guard = isolate_config_state();
        with_configuration_scope("generated-cuda-recompute-test", || {
            clear_configuration_registry_for_command();
            begin_command(&[
                ("CUDA_ENABLED".to_string(), "1".to_string()),
                ("CUDA_COMPILER".to_string(), "clang".to_string()),
            ]);
            for label in [
                "@local_config_cuda//:enable_cuda",
                "//tsl:is_cuda_enabled",
                "@local_config_cuda//:is_cuda_compiler_clang",
            ] {
                assert!(is_active_config_setting(label), "missing {label}");
            }

            consider_config_setting(
                "ordinary",
                "probe",
                &HashMap::from([("mode".to_string(), "on".to_string())]),
            );
            clear_configuration_package(None, "ordinary");
            for label in [
                "@local_config_cuda//:enable_cuda",
                "//tsl:is_cuda_enabled",
                "@local_config_cuda//:is_cuda_compiler_clang",
            ] {
                assert!(
                    is_active_config_setting(label),
                    "registry recomputation erased {label}"
                );
            }

            begin_command(&[]);
            assert!(!is_active_config_setting(
                "@local_config_cuda//:is_cuda_compiler_clang"
            ));
        });
    }

    #[test]
    fn ast_scan_registers_config_settings_and_collects_select_labels() {
        let _guard = isolate_config_state();
        // The cross-package ordering fix: scanning a BUILD's AST must register
        // its config_setting(define_values=...) and surface select() condition
        // labels, so the orchestrator can pre-register a sibling package's
        // config_setting before a select keyed on it resolves.
        let src = r#"
config_setting(
    name = "xnn_false_scan",
    define_values = {"tflite_with_xnnpack_scan": "false"},
)
cc_library(
    name = "lib",
    deps = select({
        "//tensorflow/lite:xnn_false_scan": [],
        "//conditions:default": ["@XNNPACK"],
    }),
)
"#;
        let program = crate::parse(src).expect("parse");
        begin_command(&[("tflite_with_xnnpack_scan".to_string(), "false".to_string())]);
        register_config_settings_from_program(None, "tensorflow/lite", &program);
        recompute_active_config_settings();
        assert!(is_active_config_setting("//tensorflow/lite:xnn_false_scan"));

        let labels = collect_select_condition_labels(&program);
        assert!(labels.contains("//tensorflow/lite:xnn_false_scan"));
        assert!(labels.contains("//conditions:default"));
        begin_command(&[]);
    }

    #[test]
    fn ast_scan_activates_declared_platform_constraint_settings() {
        let _guard = isolate_config_state();
        let host_os = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "osx"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "freebsd") {
            "freebsd"
        } else {
            return;
        };
        let non_host_os = if host_os == "linux" {
            "windows"
        } else {
            "linux"
        };
        let source = format!(
            r#"
config_setting(
    name = "host_constraint_scan",
    constraint_values = ["@platforms//os:{host_os}"],
)
config_setting(
    name = "other_constraint_scan",
    constraint_values = ["@@platforms//os:{non_host_os}"],
)
"#
        );
        let program = crate::parse(&source).expect("parse");
        register_config_settings_from_program(
            None,
            "rush_platform_constraint_scan",
            &program,
        );

        assert!(is_active_config_setting(
            "//rush_platform_constraint_scan:host_constraint_scan"
        ));
        assert!(!is_active_config_setting(
            "//rush_platform_constraint_scan:other_constraint_scan"
        ));
        assert_eq!(
            intrinsic_platform_constraint_matches("//ordinary:label"),
            None
        );
    }

    #[test]
    fn ast_scan_rejects_partially_literal_predicates() {
        let _guard = isolate_config_state();
        let host_os = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "osx"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "freebsd") {
            "freebsd"
        } else {
            return;
        };
        begin_command(&[]);

        for (package, source) in [
            (
                "rush_nonliteral_predicate_scan",
                format!(
                    "config_setting(name = \"host\", define_values = SOME_VAR, \
                     constraint_values = [\"@platforms//os:{host_os}\"])"
                ),
            ),
            (
                "rush_malformed_predicate_scan",
                format!(
                    "config_setting(name = \"host\", define_values = \
                     {{\"wanted\": \"yes\", 1: \"bad\"}}, \
                     constraint_values = [\"@platforms//os:{host_os}\"])"
                ),
            ),
        ] {
            let program = crate::parse(&source).expect("parse");
            register_config_settings_from_program(None, package, &program);
            assert!(
                !is_active_config_setting(&format!("//{package}:host")),
                "a partial literal scan must not weaken the declared predicate"
            );
        }
    }

    #[test]
    fn populated_config_setting_dimensions_are_conjoined() {
        let _guard = isolate_config_state();
        let host_os = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "osx"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "freebsd") {
            "freebsd"
        } else {
            return;
        };
        let other_os = if host_os == "linux" {
            "windows"
        } else {
            "linux"
        };
        let define_match = HashMap::from([(
            "rush_joint_predicate".to_string(),
            "yes".to_string(),
        )]);
        let define_miss = HashMap::from([(
            "rush_joint_predicate".to_string(),
            "no".to_string(),
        )]);
        let native_miss = HashMap::from([(
            "cpu".to_string(),
            "not-a-host-cpu".to_string(),
        )]);
        let empty = HashMap::new();
        let host_constraint = vec![format!("@platforms//os:{host_os}")];
        let other_constraint = vec![format!("@platforms//os:{other_os}")];
        let flag_value_match = HashMap::from([(
            "//rush_joint_flags:mode".to_string(),
            "yes".to_string(),
        )]);
        let flag_value_miss = HashMap::from([(
            "//rush_joint_flags:mode".to_string(),
            "no".to_string(),
        )]);

        begin_command(&[(
            "rush_joint_predicate".to_string(),
            "yes".to_string(),
        )]);
        consider_typed_build_setting_default(
            None,
            "rush_joint_flags",
            "mode",
            BuildSettingValue::String("yes".to_string()),
        );
        for (name, defines, native, constraints, flags) in [
            (
                "all_match",
                &define_match,
                &empty,
                &host_constraint,
                &empty,
            ),
            (
                "define_miss",
                &define_miss,
                &empty,
                &host_constraint,
                &empty,
            ),
            (
                "native_miss",
                &define_match,
                &native_miss,
                &host_constraint,
                &empty,
            ),
            (
                "constraint_miss",
                &define_match,
                &empty,
                &other_constraint,
                &empty,
            ),
            (
                "flag_values_match",
                &define_match,
                &empty,
                &host_constraint,
                &flag_value_match,
            ),
            (
                "flag_values_miss",
                &define_match,
                &empty,
                &host_constraint,
                &flag_value_miss,
            ),
        ] {
            consider_config_setting_predicates(
                None,
                "rush_joint",
                name,
                defines,
                native,
                constraints,
                flags,
            );
        }

        assert!(is_active_config_setting("//rush_joint:all_match"));
        assert!(is_active_config_setting(
            "//rush_joint:flag_values_match"
        ));
        for name in [
            "define_miss",
            "native_miss",
            "constraint_miss",
            "flag_values_miss",
        ] {
            assert!(!is_active_config_setting(&format!(
                "//rush_joint:{name}"
            )));
        }
        begin_command(&[]);
    }

    #[test]
    fn repository_identity_and_package_context_do_not_collide() {
        let _guard = isolate_config_state();
        let host_os = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "osx"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "freebsd") {
            "freebsd"
        } else {
            return;
        };
        let other_os = if host_os == "linux" {
            "windows"
        } else {
            "linux"
        };
        begin_command(&[]);
        consider_config_setting_predicates(
            None,
            "conditions",
            "linux",
            &HashMap::new(),
            &HashMap::new(),
            &[format!("@platforms//os:{other_os}")],
            &HashMap::new(),
        );
        consider_config_setting_predicates(
            Some("ext"),
            "conditions",
            "linux",
            &HashMap::new(),
            &HashMap::new(),
            &[format!("@platforms//os:{host_os}")],
            &HashMap::new(),
        );

        assert!(!is_active_config_setting("//conditions:linux"));
        assert!(!is_active_config_setting(":linux"));
        assert!(is_active_config_setting("@ext//conditions:linux"));
        assert!(is_active_config_setting("@@ext//conditions:linux"));
    }

    #[test]
    fn host_looking_target_names_never_override_declared_predicates() {
        let _guard = isolate_config_state();
        begin_command(&[]);
        let mismatch = HashMap::from([(
            "cpu".to_string(),
            if cfg!(target_arch = "arm") {
                "k8".to_string()
            } else {
                "arm".to_string()
            },
        )]);
        for repository in [None, Some("ext")] {
            consider_config_setting_predicates(
                repository,
                "cfg",
                "linux_x86_64",
                &HashMap::new(),
                &mismatch,
                &[],
                &HashMap::new(),
            );
        }
        assert!(!is_active_config_setting("//cfg:linux_x86_64"));
        assert!(!is_active_config_setting("@ext//cfg:linux_x86_64"));
        assert!(!is_active_config_setting("@@ext//cfg:linux_x86_64"));
    }

    #[test]
    fn loaded_pycross_environment_alias_preserves_narrow_host_compatibility() {
        let _guard = isolate_config_state();
        begin_command(&[]);
        let host_triple = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            "x86_64-unknown-linux-gnu"
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            "aarch64-unknown-linux-gnu"
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            "aarch64-apple-darwin"
        } else {
            return;
        };
        let active_version = configuration_getenv("RUSH_PY_VERSION")
            .map(|value| value.replace('_', "."))
            .unwrap_or_else(|| "3.13".to_string());
        let name = format!("_env_python_{active_version}_{host_triple}");
        let condition = format!("//pycross:{name}");

        assert!(!is_active_config_setting(&condition));
        let environment_config = "@@rules_pycross++environments+rules_pycross_all_environments//:python_3_13_host_config";
        consider_config_alias(None, "pycross", &name, environment_config);
        assert!(is_active_config_setting(&condition));

        let gpu_name = format!("{name}_gpu");
        consider_config_alias(None, "pycross", &gpu_name, environment_config);
        assert!(!is_active_config_setting(&format!("//pycross:{gpu_name}")));

        consider_config_alias(None, "arbitrary", &name, "//cfg:flag_condition");
        assert!(!is_active_config_setting(&format!("//arbitrary:{name}")));

        // The compatibility is target-kind gated. A real config_setting with
        // the same generated basename still obeys its declared predicates.
        let mismatch = HashMap::from([("cpu".to_string(), "definitely-not-host".to_string())]);
        consider_config_setting_predicates(
            None,
            "ordinary",
            &name,
            &HashMap::new(),
            &mismatch,
            &[],
            &HashMap::new(),
        );
        assert!(!is_active_config_setting(&format!("//ordinary:{name}")));
    }

    #[test]
    fn direct_platform_constraint_keys_require_loaded_constraint_value_identity() {
        let _guard = isolate_config_state();
        begin_command(&[]);
        let host_os = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "osx"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "freebsd") {
            "freebsd"
        } else {
            return;
        };
        let other_os = if host_os == "linux" { "windows" } else { "linux" };
        let host_cpu = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else if cfg!(target_arch = "arm") {
            "arm"
        } else {
            return;
        };

        assert!(
            !is_active_config_setting(&format!("@platforms//os:{host_os}")),
            "label spelling alone must not synthesize an unloaded target"
        );
        consider_constraint_value(Some("platforms"), "os", host_os);
        consider_constraint_value(Some("platforms"), "os", other_os);
        consider_constraint_value(Some("platforms"), "cpu", host_cpu);
        consider_constraint_value(Some("other"), "os", host_os);

        assert!(is_active_config_setting(&format!("@platforms//os:{host_os}")));
        assert!(is_active_config_setting(&format!("@@platforms//cpu:{host_cpu}")));
        assert!(!is_active_config_setting(&format!("@platforms//os:{other_os}")));
        assert!(
            !is_active_config_setting(&format!("@other//os:{host_os}")),
            "repository identity is part of canonical constraint matching"
        );
    }

    #[test]
    fn typed_build_setting_registry_predicates_are_workspace_scoped() {
        let _guard = isolate_config_state();
        let expected = HashMap::from([("//flags:mode".to_string(), "on".to_string())]);
        let register = |scope: &str, default: &str| {
            with_configuration_scope(scope, || {
                begin_command(&[]);
                consider_build_setting_default(None, "flags", "mode", default);
                consider_config_setting_predicates(
                    None,
                    "conditions",
                    "enabled",
                    &HashMap::new(),
                    &HashMap::new(),
                    &[],
                    &expected,
                );
                is_active_config_setting("//conditions:enabled")
            })
        };

        assert!(!register("workspace-a", "off"));
        // This is the internal typed registry, not configured-rule admission.
        // ConfigSettingRule keeps non-empty flag_values unsupported until the
        // graph validates declared setting kinds/aliases; its scoped errors are
        // exercised by the configured-closure integration controls.
        assert!(register("workspace-b", "on"));
        assert!(with_configuration_scope("workspace-b", || {
            is_active_config_setting("//conditions:enabled")
        }));
        assert!(!with_configuration_scope("workspace-a", || {
            is_active_config_setting("//conditions:enabled")
        }));
        assert_eq!(
            build_setting_value_in_scope("workspace-b", "//flags:mode").as_deref(),
            Some("on"),
            "async execution uses an explicit workspace scope"
        );
        assert_eq!(
            build_setting_value_in_scope("workspace-a", "//flags:mode").as_deref(),
            Some("off")
        );
        assert!(!register("workspace-b", "off"));
        assert!(register("workspace-a", "on"));
        assert!(!with_configuration_scope("workspace-b", || {
            is_active_config_setting("//conditions:enabled")
        }));
        assert!(with_configuration_scope("workspace-a", || {
            is_active_config_setting("//conditions:enabled")
        }));
        assert_eq!(
            build_setting_value_in_scope("workspace-b", "//flags:mode").as_deref(),
            Some("off")
        );
        assert_eq!(
            build_setting_value_in_scope("workspace-a", "//flags:mode").as_deref(),
            Some("on")
        );
    }

    #[test]
    fn command_defines_are_isolated_between_workspace_scopes() {
        let _guard = isolate_config_state();
        with_configuration_scope("defines-workspace-a", || {
            begin_command(&[("mode".to_string(), "a".to_string())]);
        });
        with_configuration_scope("defines-workspace-b", || {
            begin_command(&[("mode".to_string(), "b".to_string())]);
        });
        assert_eq!(
            with_configuration_scope("defines-workspace-a", active_defines)
                .get("mode")
                .map(String::as_str),
            Some("a")
        );
        assert_eq!(
            with_configuration_scope("defines-workspace-b", active_defines)
                .get("mode")
                .map(String::as_str),
            Some("b")
        );
    }

    #[test]
    fn concurrent_workspaces_resolve_identical_labels_in_their_own_scope() {
        let _guard = isolate_config_state();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads = [("thread-workspace-a", "a"), ("thread-workspace-b", "b")]
            .into_iter()
            .map(|(scope, mode)| {
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    with_configuration_scope(scope, || {
                        begin_command(&[("mode".to_string(), mode.to_string())]);
                        consider_config_setting_predicates(
                            None,
                            "cfg",
                            "enabled",
                            &HashMap::from([("mode".to_string(), mode.to_string())]),
                            &HashMap::new(),
                            &[],
                            &HashMap::new(),
                        );
                        barrier.wait();
                        for _ in 0..100 {
                            assert!(is_active_config_setting("//cfg:enabled"));
                            assert_eq!(
                                active_defines().get("mode").map(String::as_str),
                                Some(mode)
                            );
                            std::thread::yield_now();
                        }
                    });
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().expect("workspace configuration thread");
        }
    }

    #[test]
    fn rescanning_a_package_replaces_edited_and_deleted_literals() {
        let _guard = isolate_config_state();
        let host_os = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "osx"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "freebsd") {
            "freebsd"
        } else {
            return;
        };
        let other_os = if host_os == "linux" {
            "windows"
        } else {
            "linux"
        };
        begin_command(&[]);
        for (source, expected) in [
            (
                format!(
                    "config_setting(name = \"host\", \
                     constraint_values = [\"@platforms//os:{host_os}\"])"
                ),
                true,
            ),
            (
                format!(
                    "config_setting(name = \"host\", \
                     constraint_values = [\"@platforms//os:{other_os}\"])"
                ),
                false,
            ),
            (String::new(), false),
        ] {
            let program = crate::parse(&source).expect("parse");
            register_config_settings_from_program(
                None,
                "rush_rescan",
                &program,
            );
            assert_eq!(
                is_active_config_setting("//rush_rescan:host"),
                expected
            );
        }
    }

    #[test]
    fn rescanning_a_package_retires_deleted_aliases_and_build_settings() {
        let _guard = isolate_config_state();
        let host_os = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "osx"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "freebsd") {
            "freebsd"
        } else {
            return;
        };
        let source = format!(
            r#"
config_setting(name = "host", constraint_values = ["@platforms//os:{host_os}"])
alias(name = "host_alias", actual = ":host")
"#
        );
        let program = crate::parse(&source).expect("parse");
        register_config_settings_from_program(None, "rush_rescan_metadata", &program);
        consider_build_setting_default(None, "rush_rescan_metadata", "mode", "on");
        assert!(is_active_config_setting(
            "//rush_rescan_metadata:host_alias"
        ));
        assert_eq!(
            build_setting_value("//rush_rescan_metadata:mode").as_deref(),
            Some("on")
        );

        let empty = crate::parse("").expect("parse");
        register_config_settings_from_program(None, "rush_rescan_metadata", &empty);
        assert!(!is_known_config_condition(
            "//rush_rescan_metadata:host_alias"
        ));
        assert_eq!(build_setting_value("//rush_rescan_metadata:mode"), None);
    }

    #[test]
    fn command_registry_clear_retires_declarations_but_preserves_scoped_defines() {
        let _guard = isolate_config_state();
        let source = r#"
config_setting(name = "enabled", define_values = {"mode": "on"})
alias(name = "enabled_alias", actual = ":enabled")
constraint_value(name = "custom_constraint", constraint_setting = ":setting")
"#;
        let program = crate::parse(source).expect("parse");

        with_configuration_scope("registry-clear-a", || {
            begin_command(&[("mode".to_string(), "on".to_string())]);
            register_config_settings_from_program(None, "cfg", &program);
            consider_build_setting_default(None, "cfg", "flag", "default");
            assert!(is_active_config_setting("//cfg:enabled"));
            assert!(is_active_config_setting("//cfg:enabled_alias"));
            assert_eq!(build_setting_value("//cfg:flag").as_deref(), Some("default"));
        });
        with_configuration_scope("registry-clear-b", || {
            begin_command(&[("mode".to_string(), "on".to_string())]);
            register_config_settings_from_program(None, "cfg", &program);
        });

        with_configuration_scope("registry-clear-a", || {
            clear_configuration_registry_for_command();
            assert_eq!(active_defines().get("mode").map(String::as_str), Some("on"));
            assert!(!is_active_config_setting("//cfg:enabled"));
            assert!(!is_known_config_condition("//cfg:enabled_alias"));
            assert!(!is_known_config_condition("//cfg:custom_constraint"));
            assert_eq!(build_setting_value("//cfg:flag"), None);
        });
        assert!(with_configuration_scope("registry-clear-b", || {
            is_active_config_setting("//cfg:enabled")
        }));
    }

    #[test]
    fn matching_define_activates_only_canonical_label_identity() {
        let _guard = isolate_config_state();
        set_define("rush_test_flag_abc".to_string(), "false".to_string());
        let mut dv = HashMap::new();
        dv.insert("rush_test_flag_abc".to_string(), "false".to_string());
        consider_config_setting("tensorflow/lite", "cs_false", &dv);
        assert!(is_active_config_setting("//tensorflow/lite:cs_false"));
        assert!(!is_active_config_setting(":cs_false"));

        // A mismatched value must NOT activate.
        let mut dv2 = HashMap::new();
        dv2.insert("rush_test_flag_abc".to_string(), "true".to_string());
        consider_config_setting("tensorflow/lite", "cs_true", &dv2);
        assert!(!is_active_config_setting("//tensorflow/lite:cs_true"));
    }
}
