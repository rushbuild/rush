use crate::ast::{BinOp, BindingPattern, Expr, Program, Stmt, UnaryOp};
use crate::builtins::{get_builtins, starlark_repr, starlark_type_name, BuiltinFn};
use crate::lexer::Lexer;
use crate::platform::PlatformConfig;
use crate::token::TokenType;
use anyhow::{anyhow, Context, Result};
use futures::stream::{self, StreamExt};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

// Type aliases for complex types
type LoadWithSymbols = (String, Vec<(String, String)>);
type LoadWithPriority = (String, Vec<(String, String)>, usize);
type LoadPartition = (Vec<LoadWithSymbols>, Vec<LoadWithSymbols>);

fn bzl_loading_environment(name: &str) -> Result<Option<String>> {
    if rush_incremental::tracked::current().is_some() {
        rush_incremental::tracked::getenv(name.as_bytes())
            .map(|value| {
                String::from_utf8(value.to_vec()).context(".bzl loading option is not UTF-8")
            })
            .transpose()
    } else {
        std::env::var(name).map(Some).or_else(|error| match error {
            std::env::VarError::NotPresent => Ok(None),
            _ => Err(anyhow!(".bzl loading option {name} is not UTF-8")),
        })
    }
}

fn bzl_requested_concurrency() -> Result<usize> {
    match bzl_loading_environment("RUSH_BZL_MAX_CONCURRENT")? {
        None => Ok(1),
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|value| *value != 0)
            .context("RUSH_BZL_MAX_CONCURRENT must be a positive integer"),
    }
}

/// Fast hasher for the `visited` pointer-set in estimate_size_internal. The keys are usize pointer
/// addresses (cycle detection); the default SipHash dominated no-op build time (~57% in perf
/// profiling — SipHash on tiny usize keys is absurdly slow). FxHash-style single multiply gives
/// correct, well-distributed hashing at a fraction of the cost. (TF-scale no-op profiling 2026-06-24.)
#[derive(Default)]
pub(crate) struct FxUsizeHasher(u64);
impl std::hash::Hasher for FxUsizeHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        }
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.0 = (self.0 ^ i as u64).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}
/// Pointer-identity visited set for estimate_size_internal (fast hasher, not SipHash).
/// `pub(crate)` so callers sizing many sibling values (e.g. a `.bzl` symbol map)
/// can thread ONE set through [`Value::estimate_size_shared`] and walk a shared
/// closure environment only once instead of once per symbol.
pub(crate) type SizeVisited =
    std::collections::HashSet<usize, std::hash::BuildHasherDefault<FxUsizeHasher>>;

struct LoadPhaseResult {
    skipped_loads: HashSet<String>,
}

// Debug logging helper - only outputs if RUSH_DEBUG=1 is set
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if std::env::var("RUSH_DEBUG").unwrap_or_default() == "1" {
            eprintln!($($arg)*);
        }
    };
}

// Thread-local flag to prevent recursive batch loading
thread_local! {
    static IN_BATCH_LOAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Thread-local flag to track BUILD file context (to disable parallel batch loading for BUILD files)
thread_local! {
    static IN_BUILD_FILE_CONTEXT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatchLoadMode {
    /// Standard behavior: parallel loads unless caller disables them via env.
    Default,
    /// BUILD-safe behavior: detect cycles and only serialize the strongly connected components.
    BuildSafe,
}

const DEFAULT_BUILD_SAFE_DEP_GRAPH_LIMIT: usize = 64;
const TELEMETRY_ENV: &str = "RUSH_BZL_TELEMETRY";
const TELEMETRY_FILE_ENV: &str = "RUSH_BZL_TELEMETRY_FILE";

fn telemetry_env_truthy(var: &str) -> bool {
    match std::env::var(var) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

fn telemetry_requested() -> bool {
    std::env::var(TELEMETRY_FILE_ENV).is_ok() || telemetry_env_truthy(TELEMETRY_ENV)
}

fn telemetry_emit(event: &str, fields: &[(&str, String)]) {
    if !telemetry_requested() {
        return;
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let mut payload = format!("ts={} event={}", timestamp, event);
    for (key, value) in fields {
        payload.push(' ');
        payload.push_str(key);
        payload.push('=');
        payload.push_str(value);
    }
    payload.push('\n');

    if let Ok(path) = std::env::var(TELEMETRY_FILE_ENV) {
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(payload.as_bytes());
        }
    } else {
        eprintln!("[BZT] {}", payload.trim_end());
    }
}

fn record_lazy_loading_telemetry(total: usize, needed: usize, skipped: usize) {
    if total == 0 || !telemetry_requested() {
        return;
    }
    let reduction = (skipped as f64 / total as f64) * 100.0;
    telemetry_emit(
        "lazy_build_filter",
        &[
            ("total", total.to_string()),
            ("needed", needed.to_string()),
            ("skipped", skipped.to_string()),
            ("reduction_pct", format!("{:.2}", reduction)),
        ],
    );
}

fn record_batch_loading_telemetry(
    mode: BatchLoadMode,
    total: usize,
    sequential: usize,
    parallelizable: usize,
    levels: usize,
    max_cycle: usize,
    note: Option<&str>,
) {
    if !telemetry_requested() {
        return;
    }
    let mut fields = vec![
        ("mode", format!("{:?}", mode)),
        ("total", total.to_string()),
        ("sequential", sequential.to_string()),
        ("parallelizable", parallelizable.to_string()),
        ("levels", levels.to_string()),
        ("max_cycle_size", max_cycle.to_string()),
    ];
    if let Some(n) = note {
        fields.push(("note", n.to_string()));
    }
    telemetry_emit("bzl_batch", &fields);
}

/// RAII guard that ensures the BUILD-file context flag is always restored.
///
/// Creating a guard forces sequential .bzl loading for the current thread and
/// guarantees the previous setting is reinstated even if evaluation short-circuits.
pub struct BuildFileContextGuard {
    previous_state: bool,
}

impl BuildFileContextGuard {
    /// Enter BUILD-file context for the current thread and return a guard that
    /// restores the prior setting when dropped.
    pub fn enter() -> Self {
        let previous_state = IN_BUILD_FILE_CONTEXT.get();
        IN_BUILD_FILE_CONTEXT.set(true);
        Self { previous_state }
    }
}

impl Drop for BuildFileContextGuard {
    fn drop(&mut self) {
        IN_BUILD_FILE_CONTEXT.set(self.previous_state);
    }
}

/// An operation whose operands must remain symbolic until `select()` values
/// are resolved for the configured target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurableOp {
    /// Starlark `+` (principally configurable list concatenation).
    Add,
    /// Starlark `|` (principally configurable dictionary union).
    BitOr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ConfigurableKind {
    Int,
    Float,
    String,
    List,
    Dict,
    None,
    Bool,
    Other,
}

impl ConfigurableKind {
    fn name(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Float => "float",
            Self::String => "string",
            Self::List => "list",
            Self::Dict => "dict",
            Self::None => "None",
            Self::Bool => "bool",
            Self::Other => "unsupported value",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModuleExtensionDeclaration {
    pub implementation: Value,
    pub tag_classes: HashMap<String, Value>,
    pub doc: String,
    pub environ: Vec<String>,
    pub os_dependent: bool,
    pub arch_dependent: bool,
    /// Captured loader context, when available; the AST has no source-line
    /// spans, so no line number is invented for this declaration.
    pub source_context: Option<String>,
}

/// Immutable mapping snapshots returned by native rule introspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImmutableMappingKind {
    RuleView,
    RulesView,
    FrozenDict,
}

#[derive(Debug)]
pub struct ImmutableMapping {
    pub(crate) entries: HashMap<String, Value>,
    pub(crate) kind: ImmutableMappingKind,
}

impl ImmutableMapping {
    pub(crate) fn freeze(value: &Value) -> Value {
        match value {
            Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => {
                Value::Tuple(items.iter().map(Self::freeze).collect())
            }
            Value::Dict(entries) => Value::ImmutableMapping(Arc::new(Self {
                entries: entries
                    .iter()
                    .map(|(key, value)| (key.clone(), Self::freeze(value)))
                    .collect(),
                kind: ImmutableMappingKind::FrozenDict,
            })),
            // Rule metadata contains only values already representable by the
            // evaluator. Do not synthesize computed defaults or extra attrs.
            other => other.clone(),
        }
    }

    /// Borrow actual immutable dictionary contents for typed attribute conversion.
    /// Opaque native rule views never qualify as attribute dictionaries.
    pub fn frozen_dictionary(&self) -> Option<&HashMap<String, Value>> {
        (self.kind == ImmutableMappingKind::FrozenDict).then_some(&self.entries)
    }

    // RulesView stores immutable metadata templates, not observable child-view
    // identities. Bazel constructs a fresh ExistingRuleView on each accessor.
    pub(crate) fn exposed_value(&self, value: &Value) -> Value {
        match (self.kind, value) {
            (ImmutableMappingKind::RulesView, Value::ImmutableMapping(template)) => {
                debug_assert_eq!(template.kind, ImmutableMappingKind::RuleView);
                Value::ImmutableMapping(Arc::new(Self {
                    entries: template.entries.clone(),
                    kind: ImmutableMappingKind::RuleView,
                }))
            }
            _ => value.clone(),
        }
    }

    pub(crate) fn copied_entries(&self) -> HashMap<String, Value> {
        self.entries
            .iter()
            .map(|(key, value)| (key.clone(), self.exposed_value(value)))
            .collect()
    }

    pub(crate) fn type_name(&self) -> &'static str {
        match self.kind {
            ImmutableMappingKind::FrozenDict => "dict",
            _ => "Map",
        }
    }

    pub(crate) fn key(&self, key: &Value) -> Result<Option<String>> {
        if self.kind != ImmutableMappingKind::FrozenDict {
            return Ok(match key {
                Value::String(key) => Some(key.clone()),
                _ => None,
            });
        }
        dictionary_key(key).map(Some)
    }

    pub(crate) fn has_method(&self, method: &str) -> bool {
        matches!(method, "get" | "keys" | "values" | "items")
            || self.kind == ImmutableMappingKind::FrozenDict
                && matches!(
                    method,
                    "clear" | "pop" | "popitem" | "setdefault" | "update"
                )
    }
}

// Read-only mapping expansion is distinct from typed dictionary attributes:
// opaque views support ** expansion, but cannot themselves become attributes.
fn dictionary_expansion_value(value: Value) -> Value {
    match value {
        Value::ImmutableMapping(view) => Value::Dict(view.copied_entries()),
        other => other,
    }
}

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    String(String),
    Bool(bool),
    None,
    List(Vec<Value>),
    /// List-valued immutable introspection results; list() makes a mutable copy.
    FrozenList(Vec<Value>),
    /// Immutable Starlark sequence; distinct from mutable lists.
    Tuple(Vec<Value>),
    Dict(HashMap<String, Value>),
    ImmutableMapping(Arc<ImmutableMapping>),
    Function {
        params: Vec<(String, Option<Value>)>, // definition-time default values
        varargs_param: Option<String>,       // *args parameter
        kwargs_param: Option<String>,
        body: Vec<Stmt>,
        closure: Arc<Environment>, // Arc to prevent O(n²) memory growth from cloning
    },
    /// Namespace object for attribute access (e.g., native.cc_library)
    Namespace(HashMap<String, Value>),
    /// WORKSPACE native bindings delegated to this workspace's selected Bazel
    /// repository analyzer. The version is measured lazily, never a claim that
    /// Rush implements that Bazel release's BUILD or execution semantics.
    WorkspaceNativeNamespace {
        attrs: HashMap<String, Value>,
        workspace_root: std::path::PathBuf,
    },
    /// Rule function marker (for native.cc_library, etc.)
    RuleFunction(String),
    /// Opaque source-qualified C++ projection candidate.
    ProjectionRule(Arc<crate::cc_projection_definition::ProjectionRuleDefinition>),
    /// Genuine attr.label_list descriptor; a dictionary cannot forge it.
    AttributeDefinition(Arc<crate::cc_projection_definition::AttributeDefinition>),
    /// Native function marker (for native.glob, native.existing_rules, etc.)
    NativeFunction(String),
    /// Real module-extension declaration. Construction preserves its callable
    /// and metadata; this value is not callable and does not execute extensions.
    ModuleExtension(Arc<ModuleExtensionDeclaration>),
    /// Provider type (created by provider() function)
    Provider {
        name: String,
        fields: Vec<String>,
        /// Public constructors created by provider(init = ...) retain the
        /// Starlark initializer. Raw constructors and ordinary providers use
        /// None.
        init: Option<Box<Value>>,
    },
    /// Provider instance (created by calling a Provider)
    ProviderInstance {
        provider_name: String,
        fields: HashMap<String, Value>,
    },
    /// Select expression for platform-conditional values
    Select(HashMap<String, Value>),
    /// Compact binary composition containing at least one configurable
    /// operand. Keeping an Arc-backed expression DAG avoids distributing
    /// repeated `select() + select()` into an exponential branch tree.
    Configurable {
        op: ConfigurableOp,
        left: Arc<Value>,
        right: Arc<Value>,
    },
    /// Bound method (object.method)
    BoundMethod {
        object: Box<Value>,
        method: String,
    },
    /// Label object (Bazel build target reference)
    Label {
        /// Full label string (e.g., "//foo/bar:baz" or "@repo//foo:bar")
        label: String,
        /// Package path (e.g., "foo/bar")
        package: String,
        /// Target name (e.g., "baz")
        name: String,
        /// Workspace name (empty for main workspace)
        workspace_name: String,
        /// Workspace root path (empty for main workspace)
        workspace_root: String,
    },
    /// Lazy-loaded symbol from circular dependency
    /// When a circular dependency is detected, we return LazyLoad placeholders
    /// that get resolved after the file finishes loading
    /// This should never be seen by user code as it gets replaced before returning
    LazyLoad {
        /// The .bzl file label where this symbol comes from
        bzl_label: String,
        /// The symbol name to load
        symbol_name: String,
    },
    /// Lambda function (anonymous function with single expression body)
    Lambda {
        params: Vec<(String, Option<Value>)>, // creation-time default values
        varargs_param: Option<String>,
        kwargs_param: Option<String>,
        body: Box<Expr>,
        closure: Arc<Environment>,
    },
}

// Match the previous derived equality for every non-tuple variant, while
// allowing tuple equality to compose through lists, dicts, membership, and
// removal. In particular, direct Int/Float equality is intentionally unchanged.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a == b,
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::None, Self::None) => true,
            (Self::Tuple(_), Self::Tuple(_)) => tuple_element_equal(self, other),
            (Self::List(a), Self::List(b))
            | (Self::FrozenList(a), Self::FrozenList(b))
            | (Self::List(a), Self::FrozenList(b))
            | (Self::FrozenList(a), Self::List(b)) => a == b,
            (Self::Dict(a), Self::Dict(b))
            | (Self::Namespace(a), Self::Namespace(b))
            | (Self::Select(a), Self::Select(b)) => a == b,
            (
                Self::Function {
                    params: ap,
                    varargs_param: av,
                    kwargs_param: ak,
                    body: ab,
                    closure: ac,
                },
                Self::Function {
                    params: bp,
                    varargs_param: bv,
                    kwargs_param: bk,
                    body: bb,
                    closure: bc,
                },
            ) => ap == bp && av == bv && ak == bk && ab == bb && ac == bc,
            (
                Self::WorkspaceNativeNamespace {
                    attrs: a,
                    workspace_root: ar,
                },
                Self::WorkspaceNativeNamespace {
                    attrs: b,
                    workspace_root: br,
                },
            ) => a == b && ar == br,
            (Self::RuleFunction(a), Self::RuleFunction(b))
            | (Self::NativeFunction(a), Self::NativeFunction(b)) => a == b,
            (Self::ImmutableMapping(a), Self::ImmutableMapping(b)) => {
                if a.kind == ImmutableMappingKind::FrozenDict && b.kind == a.kind {
                    a.entries == b.entries
                } else {
                    Arc::ptr_eq(a, b)
                }
            }
            (Self::ImmutableMapping(a), Self::Dict(b))
            | (Self::Dict(b), Self::ImmutableMapping(a)) =>
                a.kind == ImmutableMappingKind::FrozenDict && a.entries == *b,
            (Self::ModuleExtension(a), Self::ModuleExtension(b)) => Arc::ptr_eq(a, b),
            (Self::ProjectionRule(a), Self::ProjectionRule(b)) => Arc::ptr_eq(a, b),
            (Self::AttributeDefinition(a), Self::AttributeDefinition(b)) => Arc::ptr_eq(a, b),
            (
                Self::Provider {
                    name: an,
                    fields: af,
                    init: ai,
                },
                Self::Provider {
                    name: bn,
                    fields: bf,
                    init: bi,
                },
            ) => an == bn && af == bf && ai == bi,
            (
                Self::ProviderInstance {
                    provider_name: an,
                    fields: af,
                },
                Self::ProviderInstance {
                    provider_name: bn,
                    fields: bf,
                },
            ) => an == bn && af == bf,
            (
                Self::Configurable {
                    op: ao,
                    left: al,
                    right: ar,
                },
                Self::Configurable {
                    op: bo,
                    left: bl,
                    right: br,
                },
            ) => ao == bo && al == bl && ar == br,
            (
                Self::BoundMethod {
                    object: ao,
                    method: am,
                },
                Self::BoundMethod {
                    object: bo,
                    method: bm,
                },
            ) => ao == bo && am == bm,
            (
                Self::Label {
                    label: al,
                    package: ap,
                    name: an,
                    workspace_name: aw,
                    workspace_root: ar,
                },
                Self::Label {
                    label: bl,
                    package: bp,
                    name: bn,
                    workspace_name: bw,
                    workspace_root: br,
                },
            ) => al == bl && ap == bp && an == bn && aw == bw && ar == br,
            (
                Self::LazyLoad {
                    bzl_label: al,
                    symbol_name: an,
                },
                Self::LazyLoad {
                    bzl_label: bl,
                    symbol_name: bn,
                },
            ) => al == bl && an == bn,
            (
                Self::Lambda {
                    params: ap,
                    varargs_param: av,
                    kwargs_param: ak,
                    body: ab,
                    closure: ac,
                },
                Self::Lambda {
                    params: bp,
                    varargs_param: bv,
                    kwargs_param: bk,
                    body: bb,
                    closure: bc,
                },
            ) => ap == bp && av == bv && ak == bk && ab == bb && ac == bc,
            _ => false,
        }
    }
}

// `Value::Dict` remains string-keyed for compatibility with the evaluator's
// existing representation, but config_setting must distinguish an actual
// Starlark string key from a non-string key whose display happens to be the
// same bytes. Encode that key kind losslessly inside the internal map key.
// Ordinary dict indexing uses the same encoder, while diagnostics and display
// strip the marker back to the user-visible spelling.
const NON_STRING_DICT_KEY_PREFIX: &str = "\0rush-non-string-dict-key:";
const ESCAPED_STRING_DICT_KEY_PREFIX: &str = "\0rush-string-dict-key:";

fn dictionary_tuple_key(values: &[Value]) -> Result<String> {
    let mut payload = String::new();
    for value in values {
        let encoded = dictionary_key(value)?;
        payload.push_str(&encoded.len().to_string());
        payload.push(':');
        payload.push_str(&encoded);
    }
    Ok(format!("{NON_STRING_DICT_KEY_PREFIX}tuple\0{payload}"))
}

fn dictionary_key(value: &Value) -> Result<String> {
    match value {
        Value::Tuple(values) => dictionary_tuple_key(values),
        Value::String(value)
            if value.starts_with(NON_STRING_DICT_KEY_PREFIX)
                || value.starts_with(ESCAPED_STRING_DICT_KEY_PREFIX) =>
        {
            Ok(format!("{ESCAPED_STRING_DICT_KEY_PREFIX}{value}"))
        }
        Value::String(value) => Ok(value.clone()),
        Value::Label {
            label,
            package,
            name,
            workspace_name,
            ..
        } => Ok(format!(
            "{NON_STRING_DICT_KEY_PREFIX}Label\0{}",
            if label.starts_with("@@//") {
                label.clone()
            } else if workspace_name.is_empty() {
                format!("//{package}:{name}")
            } else {
                format!("@{workspace_name}//{package}:{name}")
            }
        )),
        Value::Int(value) => Ok(format!(
            "{NON_STRING_DICT_KEY_PREFIX}number\0{value}"
        )),
        Value::Float(value) => {
            let integral = *value as i64;
            if value.is_finite()
                && *value >= i64::MIN as f64
                && *value < 9_223_372_036_854_775_808.0
                && integral as f64 == *value
            {
                return Ok(format!(
                    "{NON_STRING_DICT_KEY_PREFIX}number\0{integral}"
                ));
            }
            Ok(format!(
                "{NON_STRING_DICT_KEY_PREFIX}float\0{}",
                value
            ))
        }
        other @ (Value::Bool(_) | Value::None) => {
            let kind = match other {
                Value::Bool(_) => "bool",
                Value::None => "NoneType",
                _ => unreachable!("guarded hashable dictionary key"),
            };
            Ok(format!(
                "{NON_STRING_DICT_KEY_PREFIX}{kind}\0{}",
                other.to_string()
            ))
        }
        other => Err(anyhow!(
            "unhashable type: '{}'",
            starlark_type_name(other).unwrap_or("value")
        )),
    }
}

fn non_string_dictionary_key(key: &str) -> Option<(&str, &str)> {
    let encoded = key.strip_prefix(NON_STRING_DICT_KEY_PREFIX)?;
    encoded.split_once('\0')
}

pub(crate) fn visible_dictionary_key(key: &str) -> &str {
    if let Some(key) = key.strip_prefix(ESCAPED_STRING_DICT_KEY_PREFIX) {
        return key;
    }
    non_string_dictionary_key(key)
        .map(|(kind, visible)| {
            if kind == "Label" {
                visible.split('\0').next().unwrap_or(visible)
            } else {
                visible
            }
        })
        .unwrap_or(key)
}

pub(crate) fn dictionary_key_value(key: &str) -> Value {
    if let Some(key) = key.strip_prefix(ESCAPED_STRING_DICT_KEY_PREFIX) {
        return Value::String(key.to_string());
    }
    let Some((kind, visible)) = non_string_dictionary_key(key) else {
        return Value::String(key.to_string());
    };
    match kind {
        "tuple" => {
            // Each nested key is UTF-8 byte-length prefixed, so embedded colons,
            // NULs, and non-ASCII strings cannot collide or split a code point.
            let decode = || -> Option<Vec<Value>> {
                let mut rest = visible;
                let mut values = Vec::new();
                while !rest.is_empty() {
                    let (length, payload) = rest.split_once(':')?;
                    let length = length.parse::<usize>().ok()?;
                    values.push(dictionary_key_value(payload.get(..length)?));
                    rest = payload.get(length..)?;
                }
                Some(values)
            };
            decode().map(Value::Tuple).unwrap_or_else(|| Value::String(key.to_string()))
        }
        "number" => visible
            .parse::<i64>()
            .map(Value::Int)
            .unwrap_or_else(|_| Value::String(visible.to_string())),
        "float" => visible
            .parse::<f64>()
            .map(Value::Float)
            .unwrap_or_else(|_| Value::String(visible.to_string())),
        "bool" => Value::Bool(visible == "True"),
        "NoneType" => Value::None,
        "Label" => {
            let canonical_main = visible.starts_with("@@//");
            let (workspace_name, body) = if let Some(body) = visible.strip_prefix("@@//") {
                (String::new(), body)
            } else if let Some(external) = visible
                .strip_prefix("@@")
                .or_else(|| visible.strip_prefix('@'))
            {
                external
                    .split_once("//")
                    .map(|(workspace, body)| (workspace.to_string(), body))
                    .unwrap_or_else(|| (String::new(), visible))
            } else {
                (String::new(), visible.strip_prefix("//").unwrap_or(visible))
            };
            let (package, name) = body
                .split_once(':')
                .map(|(package, name)| (package.to_string(), name.to_string()))
                .unwrap_or_else(|| {
                    (
                        body.to_string(),
                        body.rsplit('/').next().unwrap_or(body).to_string(),
                    )
                });
            let label = if canonical_main {
                format!("@@//{package}:{name}")
            } else if workspace_name.is_empty() {
                format!("//{package}:{name}")
            } else {
                format!("@{workspace_name}//{package}:{name}")
            };
            Value::Label {
                label,
                package,
                name,
                workspace_root: if workspace_name.is_empty() {
                    String::new()
                } else {
                    format!("external/{workspace_name}")
                },
                workspace_name,
            }
        }
        _ => Value::String(key.to_string()),
    }
}

/// Decode a dictionary key accepted by a label-keyed configuration
/// attribute. Bazel permits either a string or a `Label` key for
/// `config_setting.flag_values`; all other Starlark key types remain invalid.
/// The evaluator's internal string-keyed map uses collision-proof markers,
/// so callers crossing into rush-parse must use this helper rather than copy
/// the raw map key.
pub fn configuration_label_dictionary_key(key: &str) -> Option<String> {
    match dictionary_key_value(key) {
        Value::String(value) => Some(value),
        Value::Label { label, .. } => Some(label),
        _ => None,
    }
}

pub(crate) fn sorted_dictionary_key_values(map: &HashMap<String, Value>) -> Vec<Value> {
    let mut keys: Vec<(&String, Value)> = map
        .keys()
        .map(|key| (key, dictionary_key_value(key)))
        .collect();
    // Sort by the collision-proof internal representation, not only the
    // display text. Starlark keys `1` and `"1"` have the same rendering but
    // remain distinct; a display-only tie would leak HashMap iteration order.
    keys.sort_by(|(left, _), (right, _)| left.cmp(right));
    keys.into_iter().map(|(_, value)| value).collect()
}

#[derive(Debug)]
enum LvalueStep {
    Attribute(String),
    Index(Value),
}

// Internal mutation detection must retain type/representation changes even
// when Starlark tuple equality says the values compare equal (1 vs 1.0).
fn same_value_representation(left: &Value, right: &Value) -> bool {
    let sequence = |left: &[Value], right: &[Value]| {
        left.len() == right.len()
            && left
                .iter()
                .zip(right)
                .all(|(left, right)| same_value_representation(left, right))
    };
    let mapping = |left: &HashMap<String, Value>, right: &HashMap<String, Value>| {
        left.len() == right.len()
            && left.iter().all(|(key, value)| {
                right
                    .get(key)
                    .is_some_and(|other| same_value_representation(value, other))
            })
    };
    let parameters = |left: &[(String, Option<Value>)], right: &[(String, Option<Value>)]| {
        left.len() == right.len() && left.iter().zip(right).all(|((ln, lv), (rn, rv))| {
            ln == rn && match (lv, rv) {
                (None, None) => true,
                (Some(left), Some(right)) => same_value_representation(left, right),
                _ => false,
            }
        })
    };
    match (left, right) {
        (Value::Function { params: lp, varargs_param: lv, kwargs_param: lk, body: lb, closure: lc },
         Value::Function { params: rp, varargs_param: rv, kwargs_param: rk, body: rb, closure: rc }) =>
            parameters(lp, rp) && lv == rv && lk == rk && lb == rb && lc == rc,
        (Value::Lambda { params: lp, varargs_param: lv, kwargs_param: lk, body: lb, closure: lc },
         Value::Lambda { params: rp, varargs_param: rv, kwargs_param: rk, body: rb, closure: rc }) =>
            parameters(lp, rp) && lv == rv && lk == rk && lb == rb && lc == rc,
        (Value::ImmutableMapping(left), Value::ImmutableMapping(right)) => Arc::ptr_eq(left, right),
        (Value::ImmutableMapping(_), Value::Dict(_)) | (Value::Dict(_), Value::ImmutableMapping(_)) => false,
        (Value::FrozenList(left), Value::FrozenList(right)) => sequence(left, right),
        (Value::FrozenList(_), Value::List(_)) | (Value::List(_), Value::FrozenList(_)) => false,
        (Value::Tuple(left), Value::Tuple(right)) | (Value::List(left), Value::List(right)) => {
            sequence(left, right)
        }
        (Value::Dict(left), Value::Dict(right))
        | (Value::Namespace(left), Value::Namespace(right))
        | (Value::Select(left), Value::Select(right)) => mapping(left, right),
        (
            Value::WorkspaceNativeNamespace {
                attrs: left,
                workspace_root: lr,
            },
            Value::WorkspaceNativeNamespace {
                attrs: right,
                workspace_root: rr,
            },
        ) => lr == rr && mapping(left, right),
        (
            Value::ProviderInstance {
                provider_name: ln,
                fields: left,
            },
            Value::ProviderInstance {
                provider_name: rn,
                fields: right,
            },
        ) => ln == rn && mapping(left, right),
        (
            Value::Provider {
                name: ln,
                fields: lf,
                init: li,
            },
            Value::Provider {
                name: rn,
                fields: rf,
                init: ri,
            },
        ) => {
            ln == rn
                && lf == rf
                && match (li, ri) {
                    (Some(left), Some(right)) => same_value_representation(left, right),
                    (None, None) => true,
                    _ => false,
                }
        }
        (
            Value::Configurable {
                op: lo,
                left: ll,
                right: lr,
            },
            Value::Configurable {
                op: ro,
                left: rl,
                right: rr,
            },
        ) => lo == ro && same_value_representation(ll, rl) && same_value_representation(lr, rr),
        (
            Value::BoundMethod {
                object: left,
                method: lm,
            },
            Value::BoundMethod {
                object: right,
                method: rm,
            },
        ) => lm == rm && same_value_representation(left, right),
        _ => left == right,
    }
}

// Use Starlark numeric equality while traversing a tuple. The existing Value
// Direct scalar equality retains its existing type distinction.
fn tuple_element_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Int(integer), Value::Float(float)) | (Value::Float(float), Value::Int(integer)) => {
            float.is_finite()
                && float.fract() == 0.0
                && *float >= i64::MIN as f64
                && *float < 9_223_372_036_854_775_808.0
                && *float as i64 == *integer
        }
        (Value::Tuple(left), Value::Tuple(right)) | (Value::List(left), Value::List(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| tuple_element_equal(left, right))
        }
        (Value::Dict(left), Value::Dict(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right
                        .get(key)
                        .is_some_and(|other| tuple_element_equal(value, other))
                })
        }
        _ => left == right,
    }
}

impl Value {
    fn is_configurable(&self) -> bool {
        matches!(self, Value::Select(_) | Value::Configurable { .. })
    }

    fn configurable_kinds(&self) -> Result<BTreeSet<ConfigurableKind>> {
        let singleton = |kind| BTreeSet::from([kind]);
        match self {
            Value::Int(_) => Ok(singleton(ConfigurableKind::Int)),
            Value::Float(_) => Ok(singleton(ConfigurableKind::Float)),
            Value::String(_) => Ok(singleton(ConfigurableKind::String)),
            Value::List(_) | Value::FrozenList(_) => Ok(singleton(ConfigurableKind::List)),
            Value::Dict(_) => Ok(singleton(ConfigurableKind::Dict)),
            Value::None => Ok(singleton(ConfigurableKind::None)),
            Value::Bool(_) => Ok(singleton(ConfigurableKind::Bool)),
            Value::Select(conditions) => {
                if conditions.is_empty() {
                    return Err(anyhow!("cannot compose an empty select()"));
                }
                let mut kinds = BTreeSet::new();
                for branch in conditions.values() {
                    kinds.extend(branch.configurable_kinds()?);
                }
                Ok(kinds)
            }
            Value::Configurable { op, left, right } => {
                Self::configurable_result_kinds(*op, left, right)
            }
            Value::Tuple(_)
            | Value::Function { .. }
            | Value::Namespace(_)
            | Value::WorkspaceNativeNamespace { .. }
            | Value::RuleFunction(_)
            | Value::NativeFunction(_)
            | Value::ImmutableMapping(_)
            | Value::ProjectionRule(_)
            | Value::AttributeDefinition(_)
            | Value::ModuleExtension(_)
            | Value::Provider { .. }
            | Value::ProviderInstance { .. }
            | Value::BoundMethod { .. }
            | Value::Label { .. }
            | Value::LazyLoad { .. }
            | Value::Lambda { .. } => Ok(singleton(ConfigurableKind::Other)),
        }
    }

    fn configurable_result_kind(
        op: ConfigurableOp,
        left: ConfigurableKind,
        right: ConfigurableKind,
    ) -> Option<ConfigurableKind> {
        match (op, left, right) {
            (ConfigurableOp::Add, ConfigurableKind::Int, ConfigurableKind::Int) => {
                Some(ConfigurableKind::Int)
            }
            (
                ConfigurableOp::Add,
                ConfigurableKind::String,
                ConfigurableKind::String,
            ) => Some(ConfigurableKind::String),
            (ConfigurableOp::Add, ConfigurableKind::List, ConfigurableKind::List)
            | (ConfigurableOp::Add, ConfigurableKind::None, ConfigurableKind::List)
            | (ConfigurableOp::Add, ConfigurableKind::List, ConfigurableKind::None) => {
                Some(ConfigurableKind::List)
            }
            (ConfigurableOp::Add, ConfigurableKind::None, ConfigurableKind::None) => {
                Some(ConfigurableKind::None)
            }
            (ConfigurableOp::BitOr, ConfigurableKind::Dict, ConfigurableKind::Dict) => {
                Some(ConfigurableKind::Dict)
            }
            (ConfigurableOp::BitOr, ConfigurableKind::List, ConfigurableKind::List) => {
                Some(ConfigurableKind::List)
            }
            (ConfigurableOp::BitOr, ConfigurableKind::Int, ConfigurableKind::Int) => {
                Some(ConfigurableKind::Int)
            }
            _ => None,
        }
    }

    fn configurable_result_kinds(
        op: ConfigurableOp,
        left: &Value,
        right: &Value,
    ) -> Result<BTreeSet<ConfigurableKind>> {
        let left_kinds = left.configurable_kinds()?;
        let right_kinds = right.configurable_kinds()?;
        let mut result = BTreeSet::new();

        for left_kind in left_kinds {
            for right_kind in &right_kinds {
                let Some(result_kind) =
                    Self::configurable_result_kind(op, left_kind, *right_kind)
                else {
                    let operator = match op {
                        ConfigurableOp::Add => "+",
                        ConfigurableOp::BitOr => "|",
                    };
                    return Err(anyhow!(
                        "cannot apply configurable {} to {} and {}",
                        operator,
                        left_kind.name(),
                        right_kind.name()
                    ));
                };
                result.insert(result_kind);
            }
        }

        Ok(result)
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::None => false,
            Value::Int(0) => false,
            Value::Float(f) if *f == 0.0 => false,
            Value::String(s) if s.is_empty() => false,
            Value::List(l) | Value::Tuple(l) | Value::FrozenList(l) if l.is_empty() => false,
            Value::Dict(d) if d.is_empty() => false,
            Value::ImmutableMapping(view) if view.entries.is_empty() => false,
            Value::Namespace(m) if m.is_empty() => false,
            _ => true,
        }
    }

    pub fn to_int(&self) -> Result<i64> {
        match self {
            Value::Int(n) => Ok(*n),
            Value::Float(f) => Ok(*f as i64),
            Value::Bool(b) => Ok(if *b { 1 } else { 0 }),
            _ => Err(anyhow!("Cannot convert {:?} to int", self)),
        }
    }

    pub fn to_float(&self) -> Result<f64> {
        match self {
            Value::Int(n) => Ok(*n as f64),
            Value::Float(f) => Ok(*f),
            Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            _ => Err(anyhow!("Cannot convert {:?} to float", self)),
        }
    }

    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        match self {
            Value::Int(n) => n.to_string(),
            Value::Float(f) => f.to_string(),
            Value::String(s) => s.clone(),
            Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
            Value::None => "None".to_string(),
            Value::Tuple(_) => starlark_repr(self),
            Value::List(items) | Value::FrozenList(items) => {
                let items_str: Vec<_> = items.iter().map(|v| v.to_string()).collect();
                format!("[{}]", items_str.join(", "))
            }
            Value::Dict(map) => {
                let pairs: Vec<_> = map
                    .iter()
                    .map(|(k, v)| {
                        format!("{}: {}", visible_dictionary_key(k), v.to_string())
                    })
                    .collect();
                format!("{{{}}}", pairs.join(", "))
            }
            Value::Function { .. } => "<function>".to_string(),
            Value::Namespace(_) | Value::WorkspaceNativeNamespace { .. } => "<namespace>".to_string(),
            Value::RuleFunction(name) => format!("<rule:{}>", name),
            Value::ProjectionRule(_) => "<source-qualified rule>".to_string(),
            Value::AttributeDefinition(_) => "<attr.label_list>".to_string(),
            Value::NativeFunction(name) => format!("<native:{}>", name),
            Value::ImmutableMapping(view) => match view.kind {
                ImmutableMappingKind::RuleView => format!("<native.ExistingRuleView for target '{}'>",
                    view.entries.get("name").map(Value::to_string).unwrap_or_default()),
                ImmutableMappingKind::RulesView => "<native.ExistingRulesView object>".to_string(),
                ImmutableMappingKind::FrozenDict => crate::builtins::starlark_repr(&Value::Dict(view.entries.clone())),
            },
            Value::ModuleExtension(_) => "<unknown object com.google.devtools.build.lib.bazel.bzlmod.AutoValue_ModuleExtension>".to_string(),
            Value::Provider { name, .. } => format!("<provider:{}>", name),
            Value::ProviderInstance {
                provider_name,
                fields,
            } => {
                let field_strs: Vec<_> = fields.keys().map(|k| k.as_str()).collect();
                format!("<{}({})>", provider_name, field_strs.join(", "))
            }
            Value::Select(conditions) => {
                format!("<select with {} conditions>", conditions.len())
            }
            Value::Configurable { op, .. } => match op {
                ConfigurableOp::Add => "<configurable +>".to_string(),
                ConfigurableOp::BitOr => "<configurable-or>".to_string(),
            },
            Value::BoundMethod { object: _, method } => {
                format!("<bound method {}>", method)
            }
            Value::Label { label, .. } => label.clone(),
            Value::LazyLoad {
                bzl_label,
                symbol_name,
            } => {
                format!("<LazyLoad:{}:{}>", bzl_label, symbol_name)
            }
            Value::Lambda {
                params,
                varargs_param,
                kwargs_param,
                ..
            } => {
                let mut param_names: Vec<_> =
                    params.iter().map(|(name, _)| name.as_str()).collect();
                if let Some(name) = varargs_param {
                    param_names.push(name);
                }
                if let Some(name) = kwargs_param {
                    param_names.push(name);
                }
                format!("<lambda({})>", param_names.join(", "))
            }
        }
    }

    /// Estimate the memory footprint of this value for cache accounting.
    pub fn estimate_size(&self) -> usize {
        let mut visited = SizeVisited::default();
        self.estimate_size_internal(&mut visited)
    }

    /// Estimate this value's footprint while sharing a caller-owned `visited`
    /// pointer-set across many sibling values.
    ///
    /// `estimate_size` resets the dedup set per call, so estimating a *map* of
    /// symbols one value at a time re-walks any closure [`Environment`] those
    /// values share (every Starlark function in a loaded `.bzl` closes over the
    /// SAME global/file environment). With a fresh set per symbol that shared
    /// environment is re-traversed once per symbol — O(symbols × env-size),
    /// which dominated the TF-scale no-op (≈75% of analysis: `Value` +
    /// `Environment` `estimate_size_internal`). Threading ONE set through the
    /// whole map collapses that to O(symbols + env-size): the first symbol walks
    /// the shared environment, the rest hit the visited Arc pointer and stop.
    /// (TF-scale no-op profiling 2026-06-24.)
    pub fn estimate_size_shared(&self, visited: &mut SizeVisited) -> usize {
        self.estimate_size_internal(visited)
    }

    fn estimate_arc_value(value: &Arc<Value>, visited: &mut SizeVisited) -> usize {
        let pointer = Arc::as_ptr(value) as usize;
        if visited.insert(pointer) {
            value.estimate_size_internal(visited)
        } else {
            std::mem::size_of::<Arc<Value>>()
        }
    }

    fn estimate_size_internal(&self, visited: &mut SizeVisited) -> usize {
        use std::mem::size_of;

        match self {
            Value::None | Value::Bool(_) | Value::Int(_) | Value::Float(_) => size_of::<Value>(),
            Value::String(s) => size_of::<Value>() + s.capacity(),
            Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => {
                size_of::<Value>()
                    + items.capacity() * size_of::<Value>()
                    + items
                        .iter()
                        .map(|v| v.estimate_size_internal(visited))
                        .sum::<usize>()
            }
            Value::Dict(map) | Value::Namespace(map)
            | Value::WorkspaceNativeNamespace { attrs: map, .. } => {
                size_of::<Value>()
                    + match self {
                        Value::WorkspaceNativeNamespace { workspace_root, .. } => workspace_root.as_os_str().len(),
                        _ => 0,
                    }
                    + map.capacity() * (size_of::<String>() + size_of::<Value>())
                    + map
                        .iter()
                        .map(|(k, v)| k.len() + v.estimate_size_internal(visited))
                        .sum::<usize>()
            }
            Value::Function {
                params,
                varargs_param,
                kwargs_param,
                body,
                closure,
            } => {
                // CRITICAL FIX: Include closure environment size with deduplication!
                // Closure is now Arc<Environment> to prevent O(n²) memory growth
                // Check if we've already counted this Arc to avoid double-counting
                let closure_ptr = Arc::as_ptr(closure) as usize;
                let closure_size = if visited.insert(closure_ptr) {
                    // First time seeing this closure - count it
                    closure.estimate_size_internal(visited)
                } else {
                    // Already counted this closure - just count the Arc overhead
                    size_of::<Arc<Environment>>()
                };

                size_of::<Value>()
                    + params.capacity() * size_of::<(String, Option<Value>)>()
                    + params.iter().map(|(name, default)| name.len()
                        + default.as_ref().map(|value| value.estimate_size_internal(visited)).unwrap_or(0)).sum::<usize>()
                    + varargs_param.as_ref().map(|s| s.len()).unwrap_or(0)
                    + kwargs_param.as_ref().map(|s| s.len()).unwrap_or(0)
                    + body.len() * 128  // Rough estimate for AST nodes
                    + closure_size
            }
            Value::RuleFunction(name) | Value::NativeFunction(name) => {
                size_of::<Value>() + name.len()
            }
            Value::ImmutableMapping(view) => {
                if !visited.insert(Arc::as_ptr(view) as usize) { return size_of::<Value>(); }
                size_of::<Value>() + size_of::<ImmutableMapping>()
                    + view.entries.capacity() * (size_of::<String>() + size_of::<Value>())
                    + view.entries.iter().map(|(key,value)| key.capacity() + value.estimate_size_internal(visited)).sum::<usize>()
            }
            Value::ProjectionRule(candidate) => {
                if !visited.insert(Arc::as_ptr(candidate) as usize) {
                    return size_of::<Value>();
                }
                size_of::<Value>()
                    + candidate.implementation.estimate_size_internal(visited)
                    + candidate
                        .attributes
                        .values()
                        .chain(candidate.options.values())
                        .map(|value| value.estimate_size_internal(visited))
                        .sum::<usize>()
                    + candidate
                        .origin
                        .as_ref()
                        .filter(|origin| visited.insert(Arc::as_ptr(origin) as usize))
                        .map_or(0, |origin| origin.metadata_size())
            }
            Value::AttributeDefinition(attr) => {
                if !visited.insert(Arc::as_ptr(attr) as usize) {
                    return size_of::<Value>();
                }
                size_of::<Value>()
                    + attr
                        .arguments
                        .iter()
                        .map(|(key, value)| key.capacity() + value.estimate_size_internal(visited))
                        .sum::<usize>()
            }
            Value::ModuleExtension(declaration) => {
                if !visited.insert(Arc::as_ptr(declaration) as usize) {
                    return size_of::<Value>();
                }
                size_of::<Value>() + size_of::<ModuleExtensionDeclaration>()
                    + declaration.implementation.estimate_size_internal(visited)
                    + declaration.doc.capacity()
                    + declaration.environ.capacity() * size_of::<String>()
                    + declaration.environ.iter().map(|name| name.capacity()).sum::<usize>()
                    + declaration.source_context.as_ref().map_or(0, String::capacity)
                    + declaration.tag_classes.capacity() * (size_of::<String>() + size_of::<Value>())
                    + declaration.tag_classes.iter().map(|(name, value)| {
                        name.capacity() + value.estimate_size_internal(visited)
                    }).sum::<usize>()
            }
            Value::Provider { name, fields, init } => {
                size_of::<Value>()
                    + name.len()
                    + fields.iter().map(|f| f.len()).sum::<usize>()
                    + init
                        .as_ref()
                        .map(|value| value.estimate_size_internal(visited))
                        .unwrap_or(0)
            }
            Value::ProviderInstance {
                provider_name,
                fields,
            } => {
                size_of::<Value>()
                    + provider_name.len()
                    + fields
                        .iter()
                        .map(|(k, v)| k.len() + v.estimate_size_internal(visited))
                        .sum::<usize>()
            }
            Value::Select(options) => {
                size_of::<Value>()
                    + options.capacity() * (size_of::<String>() + size_of::<Value>())
                    + options
                        .iter()
                        .map(|(k, v)| k.len() + v.estimate_size_internal(visited))
                        .sum::<usize>()
            }
            Value::Configurable { left, right, .. } => {
                size_of::<Value>()
                    + Self::estimate_arc_value(left, visited)
                    + Self::estimate_arc_value(right, visited)
            }
            Value::BoundMethod { object, method } => {
                size_of::<Value>() + object.estimate_size_internal(visited) + method.len()
            }
            Value::Label {
                label,
                package,
                name,
                workspace_name,
                workspace_root,
            } => {
                size_of::<Value>()
                    + label.len()
                    + package.len()
                    + name.len()
                    + workspace_name.len()
                    + workspace_root.len()
            }
            Value::LazyLoad {
                bzl_label,
                symbol_name,
            } => size_of::<Value>() + bzl_label.len() + symbol_name.len(),
            Value::Lambda { params, .. } => {
                size_of::<Value>()
                    + params.capacity() * size_of::<(String, Option<Value>)>()
                    + params.iter().map(|(name, default)| name.len()
                        + default.as_ref().map(|value| value.estimate_size_internal(visited)).unwrap_or(0)).sum::<usize>()
                    + size_of::<Box<Expr>>() // body expression
            }
        }
    }
}

/// #39 graph-RSS census: structural accounting of retained Starlark
/// `Value`s (Codex 019e8ee5). The slope census cleared CcInfo providers
/// as the 82GB hog and pointed at the `.bzl` load/eval layer — where each
/// cached `Value::Function`/`Value::Lambda` retains its `body` AST plus a
/// `closure: Arc<Environment>` of module globals. This walks a set of
/// retained values, deduping closure environments by `Arc::as_ptr` (so a
/// module env shared by N exported functions is counted once), to name
/// which sub-structure dominates: function AST bodies, closure-pinned
/// module globals, or sheer value count.
#[derive(Default, Debug, Clone)]
pub struct ValueCensus {
    pub value_nodes: usize,
    pub string_bytes: usize,
    pub list_elements: usize,
    pub dict_entries: usize,
    pub provider_instances: usize,
    pub function_count: usize,
    pub lambda_count: usize,
    /// Sum of top-level statement count across retained function bodies.
    pub function_body_stmts: usize,
    /// Distinct closure `Arc<Environment>` nodes (ptr-deduped).
    pub unique_closure_envs: usize,
    /// Sum of var-map entries over the distinct closure environments.
    pub closure_env_var_entries: usize,
}

impl ValueCensus {
    /// Walk an iterator of retained values, accumulating structural counts.
    /// Closure environments (and their parents) are deduped across the whole
    /// walk via `seen_envs`, so shared module globals are not multiply counted.
    pub fn of_values<'a>(values: impl Iterator<Item = &'a Value>) -> ValueCensus {
        let mut stats = ValueCensus::default();
        let mut seen_envs: std::collections::HashSet<usize> = std::collections::HashSet::new();
        stats.accumulate(values, &mut seen_envs);
        stats
    }

    /// Accumulate into an existing census using a caller-owned `seen_envs`,
    /// so closure environments shared across separate value sets (e.g.
    /// distinct .bzl cache entries) are deduped globally, not per-call.
    pub fn accumulate<'a>(
        &mut self,
        values: impl Iterator<Item = &'a Value>,
        seen_envs: &mut std::collections::HashSet<usize>,
    ) {
        for v in values {
            self.walk_value(v, seen_envs);
        }
    }

    fn walk_value(&mut self, v: &Value, seen_envs: &mut std::collections::HashSet<usize>) {
        self.value_nodes += 1;
        if let Value::WorkspaceNativeNamespace { workspace_root, .. } = v {
            self.string_bytes += workspace_root.as_os_str().len();
        }
        match v {
            Value::ImmutableMapping(view) => {
                if seen_envs.insert(Arc::as_ptr(view) as usize) {
                    self.dict_entries += view.entries.len();
                    for (key,value) in &view.entries {
                        self.string_bytes += key.len();
                        self.walk_value(value, seen_envs);
                    }
                }
            }
            Value::ModuleExtension(declaration) => {
                if seen_envs.insert(Arc::as_ptr(declaration) as usize) {
                    self.string_bytes += declaration.doc.len()
                        + declaration.environ.iter().map(String::len).sum::<usize>()
                        + declaration.source_context.as_ref().map_or(0, String::len);
                    self.walk_value(&declaration.implementation, seen_envs);
                    for (name, value) in &declaration.tag_classes {
                        self.string_bytes += name.len();
                        self.walk_value(value, seen_envs);
                    }
                }
            }
            Value::String(s) => self.string_bytes += s.len(),
            Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => {
                self.list_elements += items.len();
                for i in items {
                    self.walk_value(i, seen_envs);
                }
            }
            Value::Dict(m) | Value::Namespace(m) | Value::Select(m)
            | Value::WorkspaceNativeNamespace { attrs: m, .. } => {
                self.dict_entries += m.len();
                for (k, val) in m {
                    self.string_bytes += k.len();
                    self.walk_value(val, seen_envs);
                }
            }
            Value::Configurable { left, right, .. } => {
                for value in [left, right] {
                    let key = Arc::as_ptr(value) as usize;
                    if seen_envs.insert(key) {
                        self.walk_value(value, seen_envs);
                    }
                }
            }
            Value::ProviderInstance { fields, .. } => {
                self.provider_instances += 1;
                self.dict_entries += fields.len();
                for (k, val) in fields {
                    self.string_bytes += k.len();
                    self.walk_value(val, seen_envs);
                }
            }
            Value::ProjectionRule(candidate) => {
                if seen_envs.insert(Arc::as_ptr(candidate) as usize) {
                    self.walk_value(&candidate.implementation, seen_envs);
                    for value in candidate
                        .attributes
                        .values()
                        .chain(candidate.options.values())
                    {
                        self.walk_value(value, seen_envs);
                    }
                }
            }
            Value::AttributeDefinition(attr) => {
                if seen_envs.insert(Arc::as_ptr(attr) as usize) {
                    for value in attr.arguments.values() {
                        self.walk_value(value, seen_envs);
                    }
                }
            }
            Value::Function { params, body, closure, .. } => {
                self.function_count += 1;
                self.function_body_stmts += body.len();
                for (_, value) in params { if let Some(value) = value { self.walk_value(value, seen_envs); } }
                self.walk_env(closure, seen_envs);
            }
            Value::Lambda { params, closure, .. } => {
                self.lambda_count += 1;
                for (_, value) in params { if let Some(value) = value { self.walk_value(value, seen_envs); } }
                self.walk_env(closure, seen_envs);
            }
            Value::BoundMethod { object, .. } => self.walk_value(object, seen_envs),
            // Leaf / scalar variants retain no nested Value storage.
            _ => {}
        }
    }

    fn walk_env(&mut self, env: &Arc<Environment>, seen_envs: &mut std::collections::HashSet<usize>) {
        let key = Arc::as_ptr(env) as usize;
        if !seen_envs.insert(key) {
            return; // shared module env already counted
        }
        self.unique_closure_envs += 1;
        if let Some(context) = &env.definition_label_context {
            if seen_envs.insert(Arc::as_ptr(context) as usize) {
                self.string_bytes += context.repository.as_ref().map_or(0, String::len)
                    + context.package.len()
                    + context.repository_mapping.iter()
                        .map(|(apparent, canonical)| apparent.len() + canonical.len())
                        .sum::<usize>();
            }
        }
        // Hold the read guard during recursion: distinct envs have distinct
        // locks, and the seen_envs short-circuit fires before any re-lock of
        // this same env, so there is no deadlock.
        if let Ok(vars) = env.vars.read() {
            self.closure_env_var_entries += vars.len();
            for (_name, val) in vars.iter() {
                self.walk_value(val, seen_envs);
            }
        }
        if let Some(parent) = &env.parent {
            self.walk_env(parent, seen_envs);
        }
    }
}

#[derive(Debug, Clone)]
pub struct Environment {
    // Arc<RwLock> allows multiple functions to share the same variables
    // This fixes the issue where functions defined early can't see symbols defined later
    // (e.g., _with_or at line 17 calling _with_or_dict defined at line 54)
    vars: Arc<std::sync::RwLock<HashMap<String, Value>>>,
    parent: Option<Arc<Environment>>,
    definition_label_context: Option<Arc<DefinitionLabelContext>>,
    /// Set only for a real .bzl evaluation and inherited by its closures.
    /// Label() can initialize definition_label_context in BUILD files too.
    bzl_definition_scope: bool,
    cc_projection_origin: Option<Arc<crate::cc_projection_definition::DefinitionOrigin>>,
}

/// Retain the module namespace used by Label() inside a Starlark closure.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DefinitionLabelContext {
    pub repository: Option<String>,
    pub package: String,
    pub repository_mapping: std::collections::BTreeMap<String, String>,
    pub mapping_is_complete: bool,
}

impl Environment {
    pub(crate) fn cc_projection_origin(
        &self,
    ) -> Option<&Arc<crate::cc_projection_definition::DefinitionOrigin>> {
        self.cc_projection_origin.as_ref()
    }
    pub fn new() -> Self {
        Self {
            vars: Arc::new(std::sync::RwLock::new(HashMap::new())),
            parent: None,
            definition_label_context: None,
            bzl_definition_scope: false,
            cc_projection_origin: None,
        }
    }

    pub fn with_parent(parent: Arc<Environment>) -> Self {
        Self {
            vars: Arc::new(std::sync::RwLock::new(HashMap::new())),
            definition_label_context: parent.definition_label_context.clone(),
            bzl_definition_scope: parent.bzl_definition_scope,
            cc_projection_origin: parent.cc_projection_origin.clone(),
            parent: Some(parent),
        }
    }

    pub fn define(&mut self, name: String, value: Value) {
        self.vars.write().unwrap().insert(name, value);
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        // Try current scope first
        if let Some(value) = self.vars.read().unwrap().get(name) {
            return Some(value.clone());
        }
        // Then try parent scopes
        self.parent.as_ref().and_then(|parent| parent.get(name))
    }

    pub fn set(&mut self, name: &str, value: Value) -> Result<()> {
        // In Starlark, assignment always updates the current scope
        // (cannot modify parent scopes without explicit nonlocal/global, which Starlark lacks)
        self.vars.write().unwrap().insert(name.to_string(), value);
        Ok(())
    }

    /// Export all variables from this environment (including parent scopes)
    pub fn export_all(&self) -> std::collections::HashMap<String, Value> {
        let mut exported = std::collections::HashMap::new();

        // First, get all variables from parent scope (if any)
        if let Some(parent) = &self.parent {
            exported.extend(parent.export_all());
        }

        // Then add/override with variables from this scope
        // NOTE: We export ALL symbols, including private ones (starting with '_')
        // This is necessary because function closures may reference private symbols
        // The filtering of private symbols happens at the load() statement level,
        // not at the environment export level
        for (name, value) in self.vars.read().unwrap().iter() {
            exported.insert(name.clone(), value.clone());
        }

        exported
    }

    /// Estimate memory size of this environment with cycle detection.
    ///
    /// Prevents infinite recursion from circular references in Function closures.
    /// Uses a HashSet to track visited Arc<Environment> pointers.
    pub fn estimate_size_with_cycles(&self) -> usize {
        let mut visited = SizeVisited::default();
        self.estimate_size_internal(&mut visited)
    }

    fn estimate_size_internal(&self, visited: &mut SizeVisited) -> usize {
        use std::mem::size_of;

        // Base size of Environment struct
        let mut size = size_of::<Environment>();
        if let Some(context) = &self.definition_label_context {
            if visited.insert(Arc::as_ptr(context) as usize) {
                size += size_of::<DefinitionLabelContext>()
                    + context.repository.as_ref().map_or(0, String::capacity)
                    + context.package.capacity()
                    + context.repository_mapping.iter()
                        .map(|(apparent, canonical)| apparent.capacity() + canonical.capacity())
                        .sum::<usize>();
            }
        }

        if let Some(origin) = &self.cc_projection_origin {
            if visited.insert(Arc::as_ptr(origin) as usize) {
                size += origin.metadata_size();
            }
        }

        // Size of vars HashMap (behind Arc<RwLock>)
        let vars_guard = self.vars.read().unwrap();
        size += vars_guard.capacity() * (size_of::<String>() + size_of::<Value>());
        for (key, value) in vars_guard.iter() {
            size += key.len();
            // Recursively estimate value size (this may recurse into Function closures)
            size += value.estimate_size_internal(visited);
        }
        drop(vars_guard); // Explicitly drop lock before recursing

        // Follow parent chain if not yet visited (cycle detection)
        if let Some(parent) = &self.parent {
            // Use Arc::as_ptr to get a unique identifier for this environment
            let parent_ptr = Arc::as_ptr(parent) as usize;
            if visited.insert(parent_ptr) {
                // Not visited yet - recurse into parent
                size += parent.estimate_size_internal(visited);
            } else {
                // Already visited - don't recurse (breaks cycle)
                size += size_of::<Arc<Environment>>();
            }
        }

        size
    }
}

impl PartialEq for Environment {
    fn eq(&self, other: &Self) -> bool {
        // Compare by Arc pointer equality for vars (same shared HashMap)
        Arc::ptr_eq(&self.vars, &other.vars)
            && self.parent == other.parent
            && self.definition_label_context == other.definition_label_context
            && self.bzl_definition_scope == other.bzl_definition_scope
    }
}

impl Default for Environment {
    fn default() -> Self {
        Self::new()
    }
}

/// #3 Bzlmod: sentinel name for the value `use_extension(...)` returns — a
/// "module-extension proxy". Method calls on it (`ext.tag(...)`, `ext.install()`)
/// are no-ops in v1 (rush TOLERATES but does not evaluate module extensions), and
/// passing it to `use_repo(ext, ...)` is harmless. The name is reserved so it can
/// never collide with a real rule; the value is only ever produced in a
/// MODULE.bazel context (BUILD eval never calls use_extension), so this is
/// additive and invisible to the certified TF build.
pub(crate) const BZLMOD_EXT_PROXY: &str = "__bzlmod_module_extension_proxy__";

/// rush Value → serde_json::Value for the Starlark `json.encode`. dicts/structs/
/// namespaces become JSON objects, lists arrays, scalars their natural JSON form;
/// non-data values (functions, providers) fall back to a string repr.
fn value_to_json(v: &Value) -> Result<serde_json::Value> {
    Ok(match v {
        Value::ImmutableMapping(view) => {
            let mut object = serde_json::Map::new();
            for (key, value) in &view.entries {
                let Value::String(key) = dictionary_key_value(key) else {
                    anyhow::bail!("json object keys must be strings");
                };
                object.insert(key, value_to_json(value)?);
            }
            serde_json::Value::Object(object)
        }
        Value::ProjectionRule(_) | Value::AttributeDefinition(_) => {
            return Err(anyhow!(
                "opaque rule/attribute declarations are not JSON data values"
            ));
        }
        Value::ModuleExtension(_) => {
            return Err(anyhow!("module-extension declarations are not JSON data values"));
        }
        Value::None => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(n) => serde_json::Value::from(*n),
        Value::Float(f) => serde_json::json!(*f),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => serde_json::Value::Array(
            items
                .iter()
                .map(value_to_json)
                .collect::<Result<Vec<_>>>()?,
        ),
        Value::Dict(map) | Value::Select(map) => {
            let mut obj = serde_json::Map::new();
            for (k, val) in map {
                let Value::String(key) = dictionary_key_value(k) else {
                    return Err(anyhow!("json object keys must be strings"));
                };
                obj.insert(key, value_to_json(val)?);
            }
            serde_json::Value::Object(obj)
        }
        Value::Namespace(map) => {
            let mut obj = serde_json::Map::new();
            for (k, val) in map {
                obj.insert(k.clone(), value_to_json(val)?);
            }
            serde_json::Value::Object(obj)
        }
        Value::WorkspaceNativeNamespace { .. } => {
            return Err(anyhow!("WORKSPACE native namespace is not a JSON data value; read an individual field"));
        }
        Value::ProviderInstance { fields, .. } => {
            let mut obj = serde_json::Map::new();
            for (k, val) in fields {
                obj.insert(k.clone(), value_to_json(val)?);
            }
            serde_json::Value::Object(obj)
        }
        other => serde_json::Value::String(format!("{:?}", other)),
    })
}

/// serde_json::Value → rush Value for the Starlark `json.decode`.
fn json_to_value(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::None,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0))),
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(arr) => Value::List(arr.iter().map(json_to_value).collect()),
        serde_json::Value::Object(obj) => {
            let mut map = HashMap::new();
            for (k, val) in obj {
                map.insert(k.clone(), json_to_value(val));
            }
            Value::Dict(map)
        }
    }
}

/// Allowlist of non-C++ "peripheral language" rule symbols (Python/Java rules).
/// Under RUSH_ENABLE_BZLMOD, a `load()` that FAILS may degrade gracefully to
/// graph-only stubs ONLY when every requested symbol is one of these — protobuf's
/// kitchen-sink BUILD eagerly loads py_proto_library/java_proto_library next to
/// cc_proto_library, but the C++ targets never reference them, so we must not let
/// the unevaluable Python/Java rule machinery abort the C++ build. A failed load
/// requesting ANY C++/proto symbol still fails hard (Codex 019eebc6 "C-prime").
pub(crate) fn is_peripheral_language_symbol(name: &str) -> bool {
    matches!(
        name,
        "py_proto_library"
            | "py_grpc_library"
            | "py_library"
            | "py_binary"
            | "py_test"
            | "py_runtime"
            | "py_runtime_pair"
            | "java_proto_library"
            | "java_lite_proto_library"
            | "java_library"
            | "java_binary"
            | "java_import"
            | "java_plugin"
            | "java_test"
    )
}

/// Captured BUILD rule call
#[derive(Debug, Clone)]
pub struct RuleCall {
    /// Rule type (e.g., "cc_library", "tf_cc_library")
    pub rule_type: String,
    /// Positional arguments
    pub args: Vec<Value>,
    /// Keyword arguments
    pub kwargs: HashMap<String, Value>,
    /// Opaque declaration resolution, kept outside user attribute maps.
    pub cc_projection: Option<Arc<crate::cc_projection_definition::CcProjectionDeclaration>>,
}

/// Internal marker preserving whether a `config_setting` wrote its own
/// visibility before package defaults are applied file-wide.
///
/// Bazel's config-setting compatibility flags need this distinction during
/// analysis: the default mode treats an omitted visibility as public even
/// when `package(default_visibility = ...)` is private, while the strict mode
/// applies the ordinary package default. The marker is added only after BUILD
/// evaluation, so it is never observable through `native.existing_rules()`.
pub const VISIBILITY_EXPLICIT_MARKER: &str = "_rush_visibility_was_explicit";

/// Macro expander function type
/// Takes (name, args, kwargs) and returns expanded rule calls
type MacroExpander = fn(&str, &[Value], &HashMap<String, Value>) -> Result<Vec<RuleCall>>;

fn exports_files_sources(args: &[Value], kwargs: &HashMap<String, Value>) -> Result<Vec<String>> {
    let source_value = if let Some(srcs) = kwargs.get("srcs") {
        srcs
    } else if let Some(files) = kwargs.get("files") {
        files
    } else if let Some(positional_srcs) = args.first() {
        positional_srcs
    } else {
        return Err(anyhow!("exports_files() requires a srcs list"));
    };

    match source_value {
        Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => Ok(s.clone()),
                other => Err(anyhow!(
                    "exports_files() entries must be strings, got {:?}",
                    other
                )),
            })
            .collect(),
        Value::String(s) => Ok(vec![s.clone()]),
        Value::None => Ok(Vec::new()),
        other => Err(anyhow!(
            "exports_files() srcs must be a string or list of strings, got {:?}",
            other
        )),
    }
}

fn exports_files_visibility(kwargs: &HashMap<String, Value>) -> Result<Vec<Value>> {
    let Some(visibility) = kwargs.get("visibility") else {
        return Ok(vec![Value::String("//visibility:public".to_string())]);
    };

    match visibility {
        // Bazel 6.1 treats explicit None exactly like omission for
        // exports_files(): the source target is public, independent of the
        // package's default_visibility.
        Value::None => Ok(vec![Value::String("//visibility:public".to_string())]),
        Value::List(specifications) | Value::Tuple(specifications) | Value::FrozenList(specifications) => {
            if specifications
                .iter()
                .all(|specification| matches!(specification, Value::String(_)))
            {
                Ok(specifications.clone())
            } else {
                Err(anyhow!(
                    "exports_files() visibility must be a list of labels"
                ))
            }
        }
        _ => Err(anyhow!(
            "exports_files() visibility must be a list of labels"
        )),
    }
}

fn source_file_rule_call(file: String, visibility: Vec<Value>) -> RuleCall {
    let mut kwargs = HashMap::new();
    kwargs.insert("name".to_string(), Value::String(file.clone()));
    kwargs.insert("path".to_string(), Value::String(file));
    kwargs.insert("visibility".to_string(), Value::List(visibility));

    RuleCall {
        cc_projection: None,
        rule_type: "source_file".to_string(),
        args: Vec::new(),
        kwargs,
    }
}

fn is_exports_files_function(expr: &Expr) -> bool {
    match expr {
        Expr::Ident(name) => name == "exports_files",
        Expr::Attribute { object, attr } => {
            attr == "exports_files"
                && matches!(object.as_ref(), Expr::Ident(name) if name == "native")
        }
        _ => false,
    }
}

fn collect_literal_exports_files_sources(program: &Program) -> Vec<(String, Vec<Value>)> {
    let mut exports = Vec::new();

    for stmt in &program.statements {
        let Stmt::Expr(expr) = stmt else {
            continue;
        };
        let Expr::Call(call) = expr.as_ref() else {
            continue;
        };
        if !is_exports_files_function(&call.function) {
            continue;
        }

        let source_expr = call
            .kwargs
            .iter()
            .find(|(key, _)| key == "srcs")
            .or_else(|| call.kwargs.iter().find(|(key, _)| key == "files"))
            .map(|(_, expr)| expr)
            .or_else(|| call.args.first());

        let Some(source_expr) = source_expr else {
            continue;
        };
        let Some(files) = literal_string_values(source_expr) else {
            continue;
        };

        let visibility_expr = call
            .kwargs
            .iter()
            .find(|(key, _)| key == "visibility")
            .map(|(_, expr)| expr);
        let visibility = match visibility_expr {
            None | Some(Expr::None) => {
                vec![Value::String("//visibility:public".to_string())]
            }
            Some(expr) => {
                let Some(specifications) = literal_string_values(expr) else {
                    // A dynamic visibility is handled by normal evaluation.
                    // Do not synthesize an incorrectly-public fallback.
                    continue;
                };
                specifications.into_iter().map(Value::String).collect()
            }
        };

        exports.extend(files.into_iter().map(|file| (file, visibility.clone())));
    }

    exports
}

fn literal_string_values(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::String(value) => Some(vec![value.clone()]),
        Expr::List(items) | Expr::Tuple(items) => {
            let mut values = Vec::new();
            for item in items {
                values.extend(literal_string_values(item)?);
            }
            Some(values)
        }
        _ => None,
    }
}

/// Expand tf_custom_op_library macro into cc_binary with implicit dependencies
///
/// The TensorFlow tf_custom_op_library macro adds implicit dependencies:
/// - //tensorflow/core:framework_headers_lib (TensorFlow framework headers)
/// - @com_google_protobuf//:protobuf_headers (Protocol buffers)
/// - @eigen_archive//:eigen3 (Eigen linear algebra library)
///
/// This function simulates the Bazel macro expansion.
fn expand_tf_custom_op_library(
    _name: &str,
    _args: &[Value],
    kwargs: &HashMap<String, Value>,
) -> Result<Vec<RuleCall>> {
    let mut expanded_kwargs = kwargs.clone();

    // Extract existing deps or create empty list
    let mut all_deps = match expanded_kwargs.get("deps") {
        Some(Value::List(deps) | Value::Tuple(deps) | Value::FrozenList(deps)) => deps.clone(),
        _ => Vec::new(),
    };

    // Add TensorFlow implicit dependencies
    // These provide the framework headers (op.h, op_kernel.h, etc.)
    all_deps.push(Value::String(
        "//tensorflow/core:framework_headers_lib".to_string(),
    ));

    // Add protobuf headers (for absl, protobuf types)
    all_deps.push(Value::String(
        "@com_google_protobuf//:protobuf_headers".to_string(),
    ));

    // Add Eigen linear algebra library
    all_deps.push(Value::String("@eigen_archive//:eigen3".to_string()));

    // Update deps in kwargs
    expanded_kwargs.insert("deps".to_string(), Value::List(all_deps));

    // Extract copts and add -fPIC for shared library
    let mut copts = match expanded_kwargs.get("copts") {
        Some(Value::List(opts) | Value::Tuple(opts) | Value::FrozenList(opts)) => opts.clone(),
        _ => Vec::new(),
    };

    // Shared libraries need position-independent code
    if !copts
        .iter()
        .any(|v| matches!(v, Value::String(s) if s == "-fPIC"))
    {
        copts.push(Value::String("-fPIC".to_string()));
    }

    expanded_kwargs.insert("copts".to_string(), Value::List(copts));

    // Extract linkopts and add -shared for shared library
    let mut linkopts = match expanded_kwargs.get("linkopts") {
        Some(Value::List(opts) | Value::Tuple(opts) | Value::FrozenList(opts)) => opts.clone(),
        _ => Vec::new(),
    };

    if !linkopts
        .iter()
        .any(|v| matches!(v, Value::String(s) if s == "-shared"))
    {
        linkopts.push(Value::String("-shared".to_string()));
    }

    expanded_kwargs.insert("linkopts".to_string(), Value::List(linkopts));

    // Generate a cc_binary rule with expanded dependencies
    let rule_call = RuleCall {
        cc_projection: None,
        rule_type: "cc_binary".to_string(),
        args: Vec::new(),
        kwargs: expanded_kwargs,
    };

    Ok(vec![rule_call])
}

/// Expand cc_header_only_library macro (TensorFlow framework).
///
/// cc_header_only_library creates a header-only library that merges deps and extra_deps.
fn expand_cc_header_only_library(
    _name: &str,
    _args: &[Value],
    kwargs: &HashMap<String, Value>,
) -> Result<Vec<RuleCall>> {
    let mut expanded_kwargs = kwargs.clone();

    // Extract existing deps or create empty list
    let mut all_deps = match expanded_kwargs.get("deps") {
        Some(Value::List(deps) | Value::Tuple(deps) | Value::FrozenList(deps)) => deps.clone(),
        _ => Vec::new(),
    };

    // Add extra_deps to the deps list (TensorFlow-specific attribute)
    if let Some(Value::List(extra_deps) | Value::Tuple(extra_deps) | Value::FrozenList(extra_deps)) = expanded_kwargs.get("extra_deps") {
        all_deps.extend(extra_deps.clone());
    }

    // Remove extra_deps attribute (not a standard attribute)
    expanded_kwargs.remove("extra_deps");

    // Update deps with merged list
    if !all_deps.is_empty() {
        expanded_kwargs.insert("deps".to_string(), Value::List(all_deps));
    }

    let rule_call = RuleCall {
        cc_projection: None,
        rule_type: "cc_header_only_library".to_string(),
        args: Vec::new(),
        kwargs: expanded_kwargs,
    };

    Ok(vec![rule_call])
}

/// Expand tf_kernel_library macro into tf_kernel_library rule
///
/// The TensorFlow tf_kernel_library macro creates kernel library targets.
/// Rush's build system recognizes tf_kernel_library as a rule type.
///
/// This function passes through the macro arguments to the rule system.
fn expand_tf_kernel_library(
    _name: &str,
    _args: &[Value],
    kwargs: &HashMap<String, Value>,
) -> Result<Vec<RuleCall>> {
    // Pass through all kwargs to the tf_kernel_library rule (handled as a
    // specialized cc_library by build_cc_library).
    //
    // TF's tf_kernel_library macro DEFAULTS `alwayslink = 1` (and
    // `linkstatic = 1`): OpKernels register via static
    // REGISTER_KERNEL_BUILDER initializers that nothing references by symbol
    // (the runtime resolves kernels by op NAME through the registry). Without
    // --whole-archive the linker drops those objects, so a test that builds a
    // graph node by name (e.g. ShuffleDataset) aborts "No registered OpKernel".
    // Inject the defaults unless the BUILD overrode them, matching Bazel.
    // (Codex gpt-5.5 xhigh thread 019ec55c.)
    let mut kwargs = kwargs.clone();
    kwargs
        .entry("alwayslink".to_string())
        .or_insert(Value::Int(1));
    kwargs
        .entry("linkstatic".to_string())
        .or_insert(Value::Int(1));
    let rule_call = RuleCall {
        cc_projection: None,
        rule_type: "tf_kernel_library".to_string(),
        args: Vec::new(),
        kwargs,
    };

    Ok(vec![rule_call])
}

fn expand_gentbl_cc_library(
    _name: &str,
    _args: &[Value],
    kwargs: &HashMap<String, Value>,
) -> Result<Vec<RuleCall>> {
    // Route to rush's native GentblCcLibraryRule instead of whatever
    // Starlark version is loaded. The Starlark version invokes
    // `mlir-tblgen` via a Bazel tool action rush can't execute directly.
    Ok(vec![RuleCall {
        cc_projection: None,
        rule_type: "gentbl_cc_library".to_string(),
        args: Vec::new(),
        kwargs: kwargs.clone(),
    }])
}

/// #36: map a `tf_gen_op_wrapper(s)_cc` `api_def_srcs` list to the trailing
/// api_def-dirs argument cc_op_gen_main expects (a comma-separated list of
/// directories holding `api_def_*.pbtxt`). The empty form is the literal `,`
/// (matches TF's `api_def_args_str`). The only api_def source on the
/// libtensorflow_cc.so path is `//tensorflow/core/api_def:base_api_def`, whose
/// files live at `tensorflow/core/api_def/base_api/` — pass that workspace-
/// relative dir (the op-gen runs from the workspace root, and the files are real
/// sources, so no genrule input is needed). Unknown api_def sources fail loud
/// (NO STUBS) — rush has no filegroup→dir resolution channel yet.
fn compute_api_def_arg(api_def_srcs: &[String], target: &str) -> Result<String> {
    if api_def_srcs.is_empty() {
        return Ok(",".to_string());
    }
    if api_def_srcs
        .iter()
        .all(|s| s.contains("//tensorflow/core/api_def"))
    {
        return Ok("tensorflow/core/api_def/base_api".to_string());
    }
    Err(anyhow!(
        "tf_gen_op_wrapper(s)_cc '{}': unrecognized api_def_srcs {:?}; only \
         //tensorflow/core/api_def:base_api_def is mapped (→ \
         tensorflow/core/api_def/base_api). Extend compute_api_def_arg when a \
         new api_def source appears on the build path.",
        target, api_def_srcs
    ))
}

/// Expand TF's `tf_gen_op_wrappers_cc` macro (tensorflow.bzl) into native
/// rules rush can build, instead of letting the Starlark body run (it invokes
/// a Bazel tool action + genrule with `$(location)` of outputs that rush
/// doesn't fully evaluate).
///
/// Per `op_lib_name` n (out_ops_file = "ops/"+n):
///   * `tf_cc_binary` tool `ops/<n>_gen_cc` linking the op-gen lib
///     (//tensorflow/cc:cc_op_gen_main) + `<pkg>:<n>_op_lib` (the REGISTER_OP
///     library — must link whole-archive so the op registrations survive),
///   * `genrule` `<n>_genrule` running the tool to write
///     `ops/<n>.{h,cc,_internal.h,_internal.cc}`; cmd uses `set -- $(OUTS)` so
///     we don't depend on `$(location)` of genrule OUTPUTS (only `$(location)`
///     of the tool, which rush does substitute).
/// Generated-file labels resolve through the package output catalog.
/// Then one public `cc_library` <name> bundling every op's `.cc`/`.h` and one
/// `<name>_internal` for the `_internal.*` files, both alwayslink=1 with the
/// macro's default cc/core deps. api_def_srcs are not yet handled (functional_ops
/// and friends have none; math_ops/array_ops do — deferred).
fn expand_tf_gen_op_wrappers_cc(
    _name: &str,
    _args: &[Value],
    kwargs: &HashMap<String, Value>,
) -> Result<Vec<RuleCall>> {
    let get_str = |k: &str| -> Option<String> {
        match kwargs.get(k) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        }
    };
    let get_list = |k: &str| -> Vec<String> {
        match kwargs.get(k) {
            Some(Value::List(l) | Value::Tuple(l) | Value::FrozenList(l)) => l
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    };
    let name = get_str("name")
        .ok_or_else(|| anyhow!("tf_gen_op_wrappers_cc requires 'name' attribute"))?;
    let op_lib_names = get_list("op_lib_names");
    let pkg = get_str("pkg").unwrap_or_default();
    let op_gen = get_str("op_gen").unwrap_or_else(|| "//tensorflow/cc:cc_op_gen_main".to_string());
    let include_internal_ops = match kwargs.get("include_internal_ops") {
        Some(Value::Int(n)) => *n,
        Some(Value::Bool(b)) => i64::from(*b),
        _ => 0,
    };
    let extra_gen_deps = get_list("extra_gen_deps");
    let visibility = kwargs.get("visibility").cloned();
    // Macro default `deps` when the caller doesn't override.
    let user_deps = if kwargs.contains_key("deps") {
        get_list("deps")
    } else {
        vec![
            "//tensorflow/cc:ops".to_string(),
            "//tensorflow/cc:scope".to_string(),
            "//tensorflow/cc:const_op".to_string(),
        ]
    };
    let deps_internal = get_list("deps_internal");
    // #36: api_def_srcs marks ops HIDDEN so the op-gen tool emits them into the
    // `_internal.{h,cc}` (ops::internal namespace) instead of public ops::.
    // cc_op_gen_main's LAST positional arg is a comma-separated list of api_def
    // DIRECTORIES (TF derives `dirname $(locations <api_def_src>)`); the empty
    // form is the literal `,`. Without the dir, hidden control-flow ops
    // (Enter/Exit/NextIteration/…) default to public ops::, and hand-written TF
    // code that references ops::internal::Enter (tensorflow/cc/ops/while_loop.cc)
    // fails to compile. The base api_def files are real source files at
    // tensorflow/core/api_def/base_api/*.pbtxt — the op-gen runs from the
    // workspace root, so the workspace-relative dir path resolves directly (no
    // genrule input needed; they're not generated). See compute_api_def_arg.
    let api_def_arg = compute_api_def_arg(&get_list("api_def_srcs"), &name)?;
    // The macro's if_not_android core deps (rush targets Linux).
    let core_deps = [
        "//tensorflow/core:core_cpu",
        "//tensorflow/core:framework",
        "//tensorflow/core:lib",
        "//tensorflow/core:ops",
        "//tensorflow/core:protos_all_cc",
    ];

    let mut rule_calls = Vec::new();
    let mut subsrcs = get_list("other_srcs");
    let mut subhdrs = get_list("other_hdrs");
    let mut internalsrcs = get_list("other_srcs_internal");
    let mut internalhdrs = get_list("other_hdrs_internal");

    for n in &op_lib_names {
        let out = format!("ops/{}", n);
        let tool = format!("{}_gen_cc", out); // ops/<n>_gen_cc

        // (1) tool tf_cc_binary: cc_op_gen_main + the op-registration lib.
        let mut tool_deps = vec![
            Value::String(op_gen.clone()),
            Value::String(format!("{}:{}_op_lib", pkg, n)),
        ];
        tool_deps.extend(extra_gen_deps.iter().map(|d| Value::String(d.clone())));
        let mut tool_kwargs = HashMap::new();
        tool_kwargs.insert("name".to_string(), Value::String(tool.clone()));
        tool_kwargs.insert("linkstatic".to_string(), Value::Int(1));
        tool_kwargs.insert("deps".to_string(), Value::List(tool_deps));
        tool_kwargs.insert(
            "linkopts".to_string(),
            Value::List(vec![
                Value::String("-lm".to_string()),
                Value::String("-Wl,-ldl".to_string()),
            ]),
        );
        rule_calls.push(RuleCall {
            cc_projection: None,
            rule_type: "tf_cc_binary".to_string(),
            args: Vec::new(),
            kwargs: tool_kwargs,
        });

        // (2) genrule running the tool. `set -- $(OUTS)` avoids $(location) of
        // genrule OUTPUTS; $1=ops/<n>.h, $2=ops/<n>.cc (OUTS order below). The
        // generator derives the _internal.{h,cc} paths from the .h/.cc args and
        // writes all four into the same genfiles dir. The trailing `,` is TF's
        // empty-api-def-dirs argument.
        let outs = vec![
            format!("{}.h", out),
            format!("{}.cc", out),
            format!("{}_internal.h", out),
            format!("{}_internal.cc", out),
        ];
        let cmd = format!(
            "set -- $(OUTS); $(location :{}) \"$1\" \"$2\" {} {}",
            tool, include_internal_ops, api_def_arg
        );
        let mut gen_kwargs = HashMap::new();
        gen_kwargs.insert("name".to_string(), Value::String(format!("{}_genrule", n)));
        gen_kwargs.insert(
            "outs".to_string(),
            Value::List(outs.iter().map(|s| Value::String(s.clone())).collect()),
        );
        gen_kwargs.insert(
            "tools".to_string(),
            Value::List(vec![Value::String(format!(":{}", tool))]),
        );
        gen_kwargs.insert("cmd".to_string(), Value::String(cmd));
        if let Some(v) = &visibility {
            gen_kwargs.insert("visibility".to_string(), v.clone());
        }
        rule_calls.push(RuleCall {
            cc_projection: None,
            rule_type: "genrule".to_string(),
            args: Vec::new(),
            kwargs: gen_kwargs,
        });

        subsrcs.push(format!("{}.cc", out));
        subhdrs.push(format!("{}.h", out));
        internalsrcs.push(format!("{}_internal.cc", out));
        internalhdrs.push(format!("{}_internal.h", out));
    }

    // (3) public cc_library bundling all generated srcs/hdrs.
    let genrule_dep_labels: Vec<Value> = op_lib_names
        .iter()
        .map(|n| Value::String(format!(":{}_genrule", n)))
        .collect();
    let mut lib = HashMap::new();
    lib.insert("name".to_string(), Value::String(name.clone()));
    lib.insert(
        "srcs".to_string(),
        Value::List(subsrcs.iter().map(|s| Value::String(s.clone())).collect()),
    );
    lib.insert(
        "hdrs".to_string(),
        Value::List(subhdrs.iter().map(|s| Value::String(s.clone())).collect()),
    );
    let mut pub_deps: Vec<Value> =
        user_deps.iter().map(|d| Value::String(d.clone())).collect();
    pub_deps.extend(core_deps.iter().map(|s| Value::String(s.to_string())));
    pub_deps.extend(genrule_dep_labels.iter().cloned());
    lib.insert("deps".to_string(), Value::List(pub_deps));
    lib.insert("alwayslink".to_string(), Value::Int(1));
    if let Some(v) = &visibility {
        lib.insert("visibility".to_string(), v.clone());
    }
    rule_calls.push(RuleCall {
        cc_projection: None,
        rule_type: "cc_library".to_string(),
        args: Vec::new(),
        kwargs: lib,
    });

    // (4) internal cc_library.
    let mut ilib = HashMap::new();
    ilib.insert("name".to_string(), Value::String(format!("{}_internal", name)));
    ilib.insert(
        "srcs".to_string(),
        Value::List(
            internalsrcs
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    ilib.insert(
        "hdrs".to_string(),
        Value::List(
            internalhdrs
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    let mut int_deps: Vec<Value> = user_deps
        .iter()
        .chain(deps_internal.iter())
        .map(|d| Value::String(d.clone()))
        .collect();
    int_deps.extend(core_deps.iter().map(|s| Value::String(s.to_string())));
    int_deps.extend(genrule_dep_labels.iter().cloned());
    ilib.insert("deps".to_string(), Value::List(int_deps));
    ilib.insert("alwayslink".to_string(), Value::Int(1));
    rule_calls.push(RuleCall {
        cc_projection: None,
        rule_type: "cc_library".to_string(),
        args: Vec::new(),
        kwargs: ilib,
    });

    Ok(rule_calls)
}

/// Expand TF's SINGULAR `tf_gen_op_wrapper_cc` macro (tensorflow.bzl:1113) into
/// native rules, mirroring the proven plural `expand_tf_gen_op_wrappers_cc`.
///
/// The real Starlark macro emits a genrule whose cmd references the genrule's
/// own outputs AND the tool via `$(location)`:
///   `$(location :<tool>) $(location :<out>.h) $(location :<out>.cc) <internal> ,`
/// rush's genrule executor now substitutes `$(location :<own-output>)` (commit
/// e59956f), but `$(location :<tool>)` only resolves when the tool tf_cc_binary
/// registers an Executable artifact reachable via `direct_artifacts` — which the
/// real-macro graph path does not reliably do for the singular case. The plural
/// native expander sidesteps this entirely (it controls tool + genrule wiring and
/// rewrites the cmd to `set -- $(OUTS); $(location :tool) "$1" "$2"`), so we
/// replicate that here for one op lib:
///   * `tf_cc_binary` tool `<out_ops_file>_gen_cc` linking op_gen + the op libs,
///   * `genrule` `<name>_genrule` (4 outs: .h/.cc/_internal.h/_internal.cc) whose
///     cmd uses `$(OUTS)` for outputs and `$(location :tool)` for the binary.
/// Generated-file labels resolve through the package output catalog.
/// Unlike the plural macro, the singular emits NO bundling cc_library — a separate
/// hand-written cc_library consumes the generated `.cc`/`.h` (e.g. tf2xla/cc:xla_ops).
/// api_def_srcs is deferred (same as the plural expander): the singular TF callers
/// rush builds toward libtensorflow_cc.so (xla_ops_gen, xla_jit_op_gen) pass none.
fn expand_tf_gen_op_wrapper_cc(
    _name: &str,
    _args: &[Value],
    kwargs: &HashMap<String, Value>,
) -> Result<Vec<RuleCall>> {
    let get_str = |k: &str| -> Option<String> {
        match kwargs.get(k) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        }
    };
    let get_list = |k: &str| -> Vec<String> {
        match kwargs.get(k) {
            Some(Value::List(l) | Value::Tuple(l) | Value::FrozenList(l)) => l
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    };
    let name =
        get_str("name").ok_or_else(|| anyhow!("tf_gen_op_wrapper_cc requires 'name'"))?;
    let out_ops_file = get_str("out_ops_file")
        .ok_or_else(|| anyhow!("tf_gen_op_wrapper_cc '{}' requires 'out_ops_file'", name))?;
    let pkg = get_str("pkg").unwrap_or_default();
    let op_gen =
        get_str("op_gen").unwrap_or_else(|| "//tensorflow/cc:cc_op_gen_main".to_string());
    let include_internal_ops = match kwargs.get("include_internal_ops") {
        Some(Value::Int(n)) => *n,
        Some(Value::Bool(b)) => i64::from(*b),
        _ => 0,
    };
    // Macro default `deps` when caller omits: [pkg + ":" + name + "_op_lib"].
    let deps = if kwargs.contains_key("deps") {
        get_list("deps")
    } else {
        vec![format!("{}:{}_op_lib", pkg, name)]
    };
    // #36: pass the api_def directory so HIDDEN ops land in ops::internal (see
    // compute_api_def_arg + the plural expander). Empty → literal `,`.
    let api_def_arg = compute_api_def_arg(&get_list("api_def_srcs"), &name)?;
    let visibility = kwargs.get("visibility").cloned();

    let out = out_ops_file; // e.g. "ops/xla_ops"
    let tool = format!("{}_gen_cc", out); // "ops/xla_ops_gen_cc"

    let mut rule_calls = Vec::new();

    // (1) tool tf_cc_binary: op_gen (cc_op_gen_main) + the op-registration libs.
    let mut tool_deps = vec![Value::String(op_gen)];
    tool_deps.extend(deps.iter().map(|d| Value::String(d.clone())));
    let mut tool_kwargs = HashMap::new();
    tool_kwargs.insert("name".to_string(), Value::String(tool.clone()));
    tool_kwargs.insert("linkstatic".to_string(), Value::Int(1));
    tool_kwargs.insert("deps".to_string(), Value::List(tool_deps));
    tool_kwargs.insert(
        "linkopts".to_string(),
        Value::List(vec![
            Value::String("-lm".to_string()),
            Value::String("-Wl,-ldl".to_string()),
        ]),
    );
    rule_calls.push(RuleCall {
        cc_projection: None,
        rule_type: "tf_cc_binary".to_string(),
        args: Vec::new(),
        kwargs: tool_kwargs,
    });

    // (2) genrule running the tool. `set -- $(OUTS)` avoids $(location) of the
    // genrule OUTPUTS; $1=<out>.h, $2=<out>.cc (OUTS order below). The generator
    // derives the _internal.{h,cc} paths from the .h/.cc args. Trailing `,` is
    // TF's empty-api-def-dirs argument.
    let outs = vec![
        format!("{}.h", out),
        format!("{}.cc", out),
        format!("{}_internal.h", out),
        format!("{}_internal.cc", out),
    ];
    let cmd = format!(
        "set -- $(OUTS); $(location :{}) \"$1\" \"$2\" {} {}",
        tool, include_internal_ops, api_def_arg
    );
    let mut gen_kwargs = HashMap::new();
    gen_kwargs.insert(
        "name".to_string(),
        Value::String(format!("{}_genrule", name)),
    );
    gen_kwargs.insert(
        "outs".to_string(),
        Value::List(outs.iter().map(|s| Value::String(s.clone())).collect()),
    );
    gen_kwargs.insert(
        "tools".to_string(),
        Value::List(vec![Value::String(format!(":{}", tool))]),
    );
    gen_kwargs.insert("cmd".to_string(), Value::String(cmd));
    if let Some(v) = &visibility {
        gen_kwargs.insert("visibility".to_string(), v.clone());
    }
    rule_calls.push(RuleCall {
        cc_projection: None,
        rule_type: "genrule".to_string(),
        args: Vec::new(),
        kwargs: gen_kwargs,
    });

    Ok(rule_calls)
}

fn expand_flatbuffer_cc_library(
    _name: &str,
    _args: &[Value],
    kwargs: &HashMap<String, Value>,
) -> Result<Vec<RuleCall>> {
    // flatbuffer_cc_library(name, srcs, out_prefix="", flatc_args=DEFAULT,
    //                       include_paths=[], ...) expands into:
    //   genrule `{name}_srcs` that runs flatc against each .fbs producing
    //   `{out_prefix}{basename}_generated.h` in the package-rooted genfiles
    //   tree,
    //   cc_library `{name}` with hdrs pointing at the generated files
    //   (includes = ["."]) and a dep on `:{name}_srcs` so the genrule is
    //   forced to execute before the cc_library is consumed.
    // Generated-file labels resolve through the package output catalog.
    //
    // `@flatbuffers//:flatc` is stubbed in rush so `$(location)`
    // resolution for the flatc binary doesn't work. Instead we rely on
    // PATH — `flatc` is the distro-canonical binary name (Debian
    // `flatbuffers-compiler` installs it as /usr/bin/flatc; other
    // distros may land it elsewhere on PATH). If a caller needs to
    // override the binary, they can wrap the build invocation with
    // `PATH=<custom>:$PATH`.
    let name = match kwargs.get("name") {
        Some(Value::String(n)) => n.clone(),
        _ => return Err(anyhow!("flatbuffer_cc_library requires 'name' attribute")),
    };

    let srcs: Vec<String> = match kwargs.get("srcs") {
        Some(Value::List(l) | Value::Tuple(l) | Value::FrozenList(l)) => l
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    if srcs.is_empty() {
        return Err(anyhow!(
            "flatbuffer_cc_library {} has no 'srcs'",
            name
        ));
    }

    let out_prefix = match kwargs.get("out_prefix") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };

    let flatc_args: Vec<String> = match kwargs.get("flatc_args") {
        Some(Value::List(l) | Value::Tuple(l) | Value::FrozenList(l)) => l
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => vec![
            "--no-union-value-namespacing".to_string(),
            "--gen-object-api".to_string(),
        ],
    };

    let visibility = kwargs
        .get("visibility")
        .cloned()
        .unwrap_or(Value::List(vec![Value::String(
            "//visibility:public".to_string(),
        )]));

    // Compute output headers: `{out_prefix}{basename_stripped}_generated.h`.
    let output_headers: Vec<String> = srcs
        .iter()
        .map(|s| {
            // Resolve the FILE basename from a possibly cross-package label src
            // (e.g. "//tensorflow/compiler/mlir/lite/schema:schema.fbs"): take the
            // label target after ':' FIRST, then the path tail, then strip ".fbs".
            // Without the ':' split, rsplit('/') keeps the package fragment and the
            // out name leaks it ("mutable/schema:schema_generated.h"). (Codex 019eebc6.)
            let label_target = s.rsplit_once(':').map(|(_, t)| t).unwrap_or(s.as_str());
            let file = label_target.rsplit('/').next().unwrap_or(label_target);
            let base = file.strip_suffix(".fbs").unwrap_or(file).to_string();
            format!("{}{}_generated.h", out_prefix, base)
        })
        .collect();

    let mut rule_calls = Vec::new();

    // genrule that runs flatc.
    // For each src, flatc writes `<basename>_generated.h` into the output
    // directory (dirname of $@). With out_prefix="mutable/" and
    // out=["mutable/schema_generated.h"], the parent dir resolves to
    // `<gendir>/mutable/` which flatc writes into.
    let args_str = flatc_args.join(" ");
    // rush's genrule substitutes only $@, $(SRCS), $(OUTS), $(GENDIR), and
    // $(location ...). Anything else passes through to `sh -c` unchanged,
    // so `$(dirname X)` works as ordinary POSIX command substitution (no
    // `$$` escape — that would reach sh as a literal $$ and expand to PID).
    let cmd = if srcs.len() == 1 {
        format!("flatc {} -c -o $(dirname $@) $(SRCS)", args_str)
    } else {
        // Multi-src: flatc produces all outputs into the shared gendir
        // under out_prefix. Approximate `$(@D)` as the dirname of the
        // first output and strip out_prefix so each src writes its
        // {out_prefix}{base}_generated.h sibling file.
        let stripped = out_prefix.trim_end_matches('/');
        let loop_dir = if stripped.is_empty() {
            "$(dirname $@)".to_string()
        } else {
            format!("$(dirname $(dirname $@))/{}", stripped)
        };
        // The shell loop var `$f` must survive rush substitution — rush
        // doesn't touch bare `$f`, only `$@`/`$(SRCS)`/etc., so plain `$f`
        // is fine.
        format!(
            "for f in $(SRCS); do flatc {} -c -o {} \"$f\"; done",
            args_str, loop_dir
        )
    };

    let mut genrule_kwargs = HashMap::new();
    genrule_kwargs.insert(
        "name".to_string(),
        Value::String(format!("{}_srcs", name)),
    );
    genrule_kwargs.insert(
        "srcs".to_string(),
        Value::List(srcs.iter().map(|s| Value::String(s.clone())).collect()),
    );
    genrule_kwargs.insert(
        "outs".to_string(),
        Value::List(
            output_headers
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    genrule_kwargs.insert("cmd".to_string(), Value::String(cmd));
    genrule_kwargs.insert("visibility".to_string(), visibility.clone());
    rule_calls.push(RuleCall {
        cc_projection: None,
        rule_type: "genrule".to_string(),
        args: Vec::new(),
        kwargs: genrule_kwargs,
    });

    // cc_library consuming the generated headers. Depend on the genrule via
    // an explicit label so rush's dep-chase pulls the genrule into the build
    // plan and runs flatc before any downstream compile. `includes = ["."]`
    // replicates the Starlark macro's propagation so package-relative
    // `#include` works via `-I<genfiles>/<package>`.
    let mut cc_lib_kwargs = HashMap::new();
    cc_lib_kwargs.insert("name".to_string(), Value::String(name.clone()));
    cc_lib_kwargs.insert(
        "hdrs".to_string(),
        Value::List(
            output_headers
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    cc_lib_kwargs.insert(
        "srcs".to_string(),
        Value::List(
            output_headers
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    cc_lib_kwargs.insert(
        "deps".to_string(),
        Value::List(vec![
            Value::String(format!(":{}_srcs", name)),
            Value::String("@flatbuffers//:runtime_cc".to_string()),
        ]),
    );
    cc_lib_kwargs.insert(
        "includes".to_string(),
        Value::List(vec![Value::String(".".to_string())]),
    );
    cc_lib_kwargs.insert("linkstatic".to_string(), Value::Int(1));
    cc_lib_kwargs.insert("visibility".to_string(), visibility.clone());
    rule_calls.push(RuleCall {
        cc_projection: None,
        rule_type: "cc_library".to_string(),
        args: Vec::new(),
        kwargs: cc_lib_kwargs,
    });

    // filegroup `{name}_includes` mirrors the Starlark macro's filegroup
    // output so callers that pass it as `includes = [other_lib:_includes]`
    // continue to resolve.
    let mut fg_kwargs = HashMap::new();
    fg_kwargs.insert(
        "name".to_string(),
        Value::String(format!("{}_includes", name)),
    );
    fg_kwargs.insert(
        "srcs".to_string(),
        Value::List(srcs.iter().map(|s| Value::String(s.clone())).collect()),
    );
    fg_kwargs.insert("visibility".to_string(), visibility);
    rule_calls.push(RuleCall {
        cc_projection: None,
        rule_type: "filegroup".to_string(),
        args: Vec::new(),
        kwargs: fg_kwargs,
    });

    Ok(rule_calls)
}

fn expand_tf_gen_options_header(
    _name: &str,
    _args: &[Value],
    kwargs: &HashMap<String, Value>,
) -> Result<Vec<RuleCall>> {
    // Preserve the template and setting-label mapping for the native rule.
    // Rendering requires genuine configured BuildSettingInfo values and a
    // verified implementation of TensorFlow's inline template action.
    // The generated-file label belongs to this rule's declared output. The
    // package catalog resolves filename dependencies to the producer.
    Ok(vec![RuleCall {
        cc_projection: None,
        rule_type: "tf_gen_options_header".to_string(),
        args: Vec::new(),
        kwargs: kwargs.clone(),
    }])
}

fn expand_tf_cc_tests(
    _name: &str,
    _args: &[Value],
    _kwargs: &HashMap<String, Value>,
) -> Result<Vec<RuleCall>> {
    // Out-of-scope by design: rush doesn't build test targets. TF's
    // tf_cc_tests expands into many cc_test rules; emitting them would
    // pull the test-only code path into a non-test build. Accept and
    // drop the declaration. See UNIMPLEMENTED.md "test execution".
    Ok(vec![])
}

/// Expand tf_gpu_cc_test / tf_cuda_cc_test (TF 2.17: tf_cuda_cc_test aliases to
/// tf_gpu_cc_test) into the suffixed tf_cc_test targets the real TF macro emits:
/// a `<name>_cpu` (unless the test is gpu-only, i.e. "gpu" already in tags) and a
/// `<name>_gpu`, grouped by a `test_suite` named `<name>`. This is gated to
/// RUSH_ENABLE_ROCM or RUSH_ENABLE_CUDA (see force_macro_expand) so the
/// certified CPU/463 path — which keeps the no-op builtin — stays
/// byte-identical.
/// The `_gpu` target reuses rush's working tf_cc_test build path; its deps gain
/// //tensorflow/core:gpu_runtime (the TF GPU runtime / ROCm StreamExecutor side)
/// and its tags gain requires-gpu/gpu. (Codex 019eebc6.)
fn expand_tf_gpu_cc_test(
    _name: &str,
    _args: &[Value],
    kwargs: &HashMap<String, Value>,
) -> Result<Vec<RuleCall>> {
    let name = match kwargs.get("name") {
        Some(Value::String(s)) => s.clone(),
        _ => return Ok(vec![]),
    };
    let tags: Vec<Value> = match kwargs.get("tags") {
        Some(Value::List(t) | Value::Tuple(t) | Value::FrozenList(t)) => t.clone(),
        _ => vec![],
    };
    let has_gpu_tag = tags
        .iter()
        .any(|v| matches!(v, Value::String(s) if s == "gpu"));

    let mut calls = Vec::new();
    let mut suite_tests = Vec::new();

    // <name>_cpu — emitted unless the test is declared gpu-only.
    if !has_gpu_tag {
        let mut cpu = kwargs.clone();
        let cpu_name = format!("{}_cpu", name);
        cpu.insert("name".to_string(), Value::String(cpu_name.clone()));
        cpu.remove("suffix");
        calls.push(RuleCall {
            cc_projection: None,
            rule_type: "tf_cc_test".to_string(),
            args: vec![],
            kwargs: cpu,
        });
        suite_tests.push(Value::String(format!(":{cpu_name}")));
    }

    // <name>_gpu — deps + gpu_runtime, tags + requires-gpu/gpu.
    let mut gpu = kwargs.clone();
    let gpu_name = format!("{}_gpu", name);
    gpu.insert("name".to_string(), Value::String(gpu_name.clone()));
    let mut gdeps = match kwargs.get("deps") {
        Some(Value::List(d) | Value::Tuple(d) | Value::FrozenList(d)) => d.clone(),
        _ => vec![],
    };
    let gpu_runtime = "//tensorflow/core:gpu_runtime".to_string();
    if !gdeps
        .iter()
        .any(|v| matches!(v, Value::String(s) if *s == gpu_runtime))
    {
        gdeps.push(Value::String(gpu_runtime));
    }
    gpu.insert("deps".to_string(), Value::List(gdeps));
    let mut gtags = tags.clone();
    for t in ["requires-gpu", "gpu"] {
        if !gtags
            .iter()
            .any(|v| matches!(v, Value::String(s) if s == t))
        {
            gtags.push(Value::String(t.to_string()));
        }
    }
    gpu.insert("tags".to_string(), Value::List(gtags));
    gpu.remove("suffix");
    calls.push(RuleCall {
        cc_projection: None,
        rule_type: "tf_cc_test".to_string(),
        args: vec![],
        kwargs: gpu,
    });
    suite_tests.push(Value::String(format!(":{gpu_name}")));

    // Preserve the original macro target. TensorFlow emits a test_suite under
    // `<name>` whose dependencies are the suffixed tests; without it an exact
    // `rush build //pkg:<name>` resolves syntactically but has no graph root.
    let mut suite = HashMap::new();
    suite.insert("name".to_string(), Value::String(name));
    suite.insert("tests".to_string(), Value::List(suite_tests));
    suite.insert("tags".to_string(), Value::List(tags));
    calls.push(RuleCall {
        cc_projection: None,
        rule_type: "test_suite".to_string(),
        args: vec![],
        kwargs: suite,
    });

    Ok(calls)
}

fn expand_check_deps(
    _name: &str,
    _args: &[Value],
    _kwargs: &HashMap<String, Value>,
) -> Result<Vec<RuleCall>> {
    // Out-of-scope by design: rush doesn't enforce Bazel-style
    // dependency-allowlist rules. check_deps is an audit target in
    // TF's build that verifies certain libraries aren't linked from
    // certain callers — rush's goal is to compile the artifacts,
    // not police the graph shape. See UNIMPLEMENTED.md
    // "dependency allowlist audits".
    Ok(vec![])
}

fn expand_cc_library_with_android_deps(
    _name: &str,
    _args: &[Value],
    kwargs: &HashMap<String, Value>,
) -> Result<Vec<RuleCall>> {
    // cc_library_with_android_deps combines deps, android_deps, and common_deps
    // Since Rush doesn't support Android, we ignore android_deps and combine deps + common_deps
    let mut new_kwargs = kwargs.clone();

    // Combine deps and common_deps, ignoring android_deps
    if let (Some(deps), Some(common_deps)) = (kwargs.get("deps"), kwargs.get("common_deps")) {
        let mut combined_deps = Vec::new();
        if let Value::List(d) | Value::Tuple(d) | Value::FrozenList(d) = deps {
            combined_deps.extend(d.clone());
        }
        if let Value::List(cd) | Value::Tuple(cd) | Value::FrozenList(cd) = common_deps {
            combined_deps.extend(cd.clone());
        }
        new_kwargs.insert("deps".to_string(), Value::List(combined_deps));
        new_kwargs.remove("common_deps");
        new_kwargs.remove("android_deps");
    } else if let Some(common_deps) = kwargs.get("common_deps") {
        // Only common_deps provided
        new_kwargs.insert("deps".to_string(), common_deps.clone());
        new_kwargs.remove("common_deps");
        new_kwargs.remove("android_deps");
    }
    // Otherwise just use deps as-is
    new_kwargs.remove("android_deps");

    // Return a cc_library call with the combined deps
    Ok(vec![RuleCall {
        cc_projection: None,
        rule_type: "cc_library".to_string(),
        args: vec![],
        kwargs: new_kwargs,
    }])
}

/// Rewrite a proto-dep label to its `_cc_impl` sibling, for ROOT-1b: an
/// impl-mode `{name}_cc_impl` depends on the `_cc_impl` of each of its
/// protodeps so the aggregate carries the transitive generated-message
/// archives. Handles all Bazel label shapes by appending `_cc_impl` to the
/// target component (`//p:t` → `//p:t_cc_impl`, `:t` → `:t_cc_impl`,
/// `@r//p:t` → `@r//p:t_cc_impl`), expanding the implicit target for
/// colon-less labels (`//p/q` → `//p/q:q_cc_impl`).
fn rewrite_proto_label_to_cc_impl(label: &str) -> String {
    if label.contains(':') {
        // Target component is the trailing `:...` segment.
        format!("{}_cc_impl", label)
    } else if label.starts_with("//") || label.starts_with('@') {
        let last = label.rsplit('/').next().unwrap_or(label);
        format!("{}:{}_cc_impl", label, last)
    } else {
        format!("{}_cc_impl", label)
    }
}

/// Rewrite dependency leaves without selecting branches or distributing a
/// configurable expression into its Cartesian branch product.
fn rewrite_proto_dependencies(value: &Value) -> Result<Value> {
    fn shared(value: &Arc<Value>, memo: &mut HashMap<usize, Arc<Value>>) -> Result<Arc<Value>> {
        let identity = Arc::as_ptr(value) as usize;
        if let Some(rewritten) = memo.get(&identity) {
            return Ok(rewritten.clone());
        }
        let rewritten = Arc::new(walk(value, memo)?);
        memo.insert(identity, rewritten.clone());
        Ok(rewritten)
    }
    fn walk(value: &Value, memo: &mut HashMap<usize, Arc<Value>>) -> Result<Value> {
        match value {
            Value::List(values) | Value::Tuple(values) | Value::FrozenList(values) => {
                let rewritten = values.iter().map(|value| match value {
                    Value::String(label) => Ok(Value::String(rewrite_proto_label_to_cc_impl(label))),
                    Value::Label { .. } => {
                        let mut value = value.clone();
                        if let Value::Label { label, name, .. } = &mut value {
                            *label = rewrite_proto_label_to_cc_impl(label);
                            *name = format!("{name}_cc_impl");
                        }
                        Ok(value)
                    }
                    _ => anyhow::bail!("tf_proto_library protodeps elements must be label strings or Label values"),
                }).collect::<Result<Vec<_>>>()?;
                Ok(Value::List(rewritten))
            }
            Value::Select(branches) => Ok(Value::Select(branches.iter()
                .map(|(condition, branch)| Ok((condition.clone(), walk(branch, memo)?)))
                .collect::<Result<HashMap<_, _>>>()?)),
            Value::Configurable { op: ConfigurableOp::Add, left, right } => Ok(Value::Configurable {
                op: ConfigurableOp::Add,
                left: shared(left, memo)?,
                right: shared(right, memo)?,
            }),
            _ => anyhow::bail!("tf_proto_library protodeps must be a list-like label collection or configurable list concatenation"),
        }
    }
    walk(value, &mut HashMap::new())
}
/// Expand tf_proto_library macro (TensorFlow protocol buffers).
///
/// tf_proto_library creates:
/// - {name}: tf_proto_library rule that runs protoc to generate C++ code
/// - {name}_cc: alias to the same target for cc_library compatibility
/// - {name}_cc_impl: for make_default_target_header_only protos this is a REAL
///   impl-mode tf_proto_library (compiles {name}'s generated .pb.cc into an
///   archive — ROOT-1b); otherwise a plain alias to {name} (which already
///   carries the archive).
///
/// This matches Bazel's tf_proto_library which auto-generates _cc and _cc_impl targets.
fn expand_tf_proto_library(
    _name: &str,
    _args: &[Value],
    kwargs: &HashMap<String, Value>,
) -> Result<Vec<RuleCall>> {
    // Get the target name
    let name = match kwargs.get("name") {
        Some(Value::String(n)) => n.clone(),
        _ => return Err(anyhow!("tf_proto_library requires 'name' attribute")),
    };

    // TSL projects these macro options into different language/target roles.
    // Validate before removing them from the C++ message declaration so a bad
    // option cannot disappear merely because its sibling is not selected.
    let flag = |attribute: &str| -> Result<bool> {
        match kwargs.get(attribute) {
            None | Some(Value::None) => Ok(false),
            Some(Value::Bool(value)) => Ok(*value),
            Some(Value::Int(0)) => Ok(false),
            Some(Value::Int(1)) => Ok(true),
            Some(_) => anyhow::bail!("tf_proto_library {attribute} must be a boolean or 0/1"),
        }
    };
    flag("has_services")?; // Python plugin option, not a C++ message option.
    let create_grpc_library = flag("create_grpc_library")?;
    let mut rule_calls = Vec::new();
    let mut generation_kwargs = kwargs.clone();
    generation_kwargs.remove("has_services");
    generation_kwargs.remove("create_grpc_library");
    generation_kwargs
        .entry("protoc".to_string())
        .or_insert_with(|| Value::String("@com_google_protobuf//:protoc".to_string()));

    // Preserve TSL's actual runtime choice as ordinary configuration metadata.
    // No repository-name predicate or unconditional host protobuf is inferred.
    let runtime = Value::Select(HashMap::from([
        ("@local_xla//xla/tsl:tsl_protobuf_header_only".to_string(),
            Value::List(vec![Value::String("@com_google_protobuf//:protobuf_headers".to_string())])),
        ("//conditions:default".to_string(),
            Value::List(vec![Value::String("@com_google_protobuf//:protobuf".to_string())])),
    ]));
    let explicit_runtime = kwargs.get("cc_libs").cloned().unwrap_or(Value::List(Vec::new()));
    Value::configurable_result_kinds(ConfigurableOp::Add, &explicit_runtime, &runtime)?;
    let runtime_dependencies = Value::Configurable {
        op: ConfigurableOp::Add,
        left: Arc::new(explicit_runtime),
        right: Arc::new(runtime),
    };
    generation_kwargs.insert("cc_libs".to_string(), runtime_dependencies.clone());

    let header_only = matches!(
        kwargs.get("make_default_target_header_only"),
        Some(Value::Bool(true) | Value::Int(1))
    );
    let default_visibility = || {
        kwargs
            .get("visibility")
            .cloned()
            .unwrap_or(Value::List(vec![Value::String(
                "//visibility:public".to_string(),
            )]))
    };

    // #39(C) v2 — bazel-faithful 3-target split for header-only protos.
    //
    // Bazel's tsl `build_config.bzl` keeps three distinct targets: a
    // `_genproto` proto_library that runs protoc, a header-only cc_library, and
    // a `_cc_impl` cc_library that compiles the generated `.pb.cc`. The
    // header-only target — the label consumers actually depend on — pulls the
    // impl under static linking via `if_tsl_link_protobuf([impl])`
    // (build_config.bzl:283/497). rush previously FUSED protoc + header-only API
    // into the single base `{name}`, so the base could not also depend on
    // `{name}_cc_impl`: the impl protodeps `:{name}` for the protoc producer, so
    // base→impl would cycle. #39(C) v1 put the impl on `{name}_cc`, but TF/XLA
    // consumers depend on the BASE label (`protodeps = ["//xla:xla_data_proto"]`,
    // never `:..._cc`), so it was inert — libtensorflow_cc.so came out
    // byte-identical with 67/70 proto `_cc_impl` archives still missing (~813
    // undefined xla::/tensorflow:: proto-message + grpc symbols).
    //
    // v2 splits the fused node, matching Bazel and breaking the cycle:
    //   {name}_genproto_owner : runs protoc, exposes headers, NO archive (this
    //                           is today's base header-only behavior, renamed to
    //                           a private target).
    //   {name}_cc_impl        : compiles `.pb.cc` → archive; protodeps the gen
    //                           owner (NOT the base → no cycle) + each original
    //                           protodep rewritten to its `_cc_impl`.
    //   {name} (base)         : the header-only API consumers reach; EMPTY srcs
    //                           so it runs no protoc and owns no archive, and
    //                           protodeps the gen owner (headers) + the impl
    //                           (archive). Its merged CcInfo therefore carries
    //                           BOTH headers and the impl archive. rush only ever
    //                           builds the monolithic static .so, so the impl dep
    //                           is unconditional (Bazel's if_static == True).
    // Generated `.pb.{h,cc}` paths are keyed on `genfiles/<pkg>/<stem>` (the
    // proto filename), not the target name, so renaming the protoc owner does
    // not move them and `_cc_impl` resolves the same ArtifactIds by path.
    if header_only {
        let gen_owner = format!("{}_genproto_owner", name);

        // gen owner = today's base header-only behavior (protoc + headers, no
        // archive), just under a private name.
        let mut owner_kwargs = generation_kwargs.clone();
        owner_kwargs.insert("name".to_string(), Value::String(gen_owner.clone()));
        owner_kwargs.remove("cc_libs");
        rule_calls.push(RuleCall {
            cc_projection: None,
            rule_type: "tf_proto_library".to_string(),
            args: Vec::new(),
            kwargs: owner_kwargs,
        });

        // base {name}: header-only API. Empty srcs → no protoc, no archive; it
        // exists only to merge the gen owner's headers and the impl's archive
        // into one CcInfo that consumers of `:{name}` transitively link.
        let mut base_kwargs = HashMap::new();
        base_kwargs.insert("name".to_string(), Value::String(name.clone()));
        base_kwargs.insert("srcs".to_string(), Value::List(Vec::new()));
        base_kwargs.insert(
            "protodeps".to_string(),
            Value::List(vec![
                Value::String(format!(":{}", gen_owner)),
                Value::String(format!(":{}_cc_impl", name)),
            ]),
        );
        base_kwargs.insert("visibility".to_string(), default_visibility());
        rule_calls.push(RuleCall {
            cc_projection: None,
            rule_type: "tf_proto_library".to_string(),
            args: Vec::new(),
            kwargs: base_kwargs,
        });

        // {name}_cc_impl: compile the generated `.pb.cc` into the archive.
        // protodeps the gen owner (for the protoc producer — NOT the base, which
        // would cycle) + each original protodep rewritten to its `_cc_impl` so
        // the transitive generated-message archives are pulled in.
        let mut impl_kwargs = HashMap::new();
        impl_kwargs.insert("name".to_string(), Value::String(format!("{}_cc_impl", name)));
        if let Some(srcs) = kwargs.get("srcs") {
            impl_kwargs.insert("srcs".to_string(), srcs.clone());
        }
        let prefix = Value::String(format!(":{}", gen_owner));
        let impl_protodeps = match kwargs.get("protodeps") {
            None => Value::List(vec![prefix]),
            Some(value) => match rewrite_proto_dependencies(value)? {
                Value::List(mut dependencies) => {
                    dependencies.insert(0, prefix);
                    Value::List(dependencies)
                }
                conditional => Value::Configurable {
                    op: ConfigurableOp::Add,
                    left: Arc::new(Value::List(vec![prefix])),
                    right: Arc::new(conditional),
                },
            },
        };
        impl_kwargs.insert("protodeps".to_string(), impl_protodeps);
        impl_kwargs.insert(
            "_rush_tf_proto_generation_owner".to_string(),
            Value::String(format!(":{}", gen_owner)),
        );
        impl_kwargs.insert(
            "_rush_tf_proto_role".to_string(),
            Value::String("cc_impl".to_string()),
        );
        for k in [
            "cc_grpc_version",
            "use_grpc_namespace",
            "cc_api_version",
        ] {
            if let Some(v) = kwargs.get(k) {
                impl_kwargs.insert(k.to_string(), v.clone());
            }
        }
        impl_kwargs.insert("cc_libs".to_string(), runtime_dependencies);
        impl_kwargs.insert("visibility".to_string(), default_visibility());
        rule_calls.push(RuleCall {
            cc_projection: None,
            rule_type: "tf_proto_library".to_string(),
            args: Vec::new(),
            kwargs: impl_kwargs,
        });

        // {name}_cc: plain alias to the base header-only API.
        let mut cc_kwargs = HashMap::new();
        cc_kwargs.insert("name".to_string(), Value::String(format!("{}_cc", name)));
        cc_kwargs.insert("actual".to_string(), Value::String(format!(":{}", name)));
        cc_kwargs.insert("visibility".to_string(), default_visibility());
        rule_calls.push(RuleCall {
            cc_projection: None,
            rule_type: "alias".to_string(),
            args: Vec::new(),
            kwargs: cc_kwargs,
        });
    } else {
        // Non-header-only: the base `{name}` carries the archive itself, so
        // `_cc` and `_cc_impl` are cheap aliases to it.
        rule_calls.push(RuleCall {
            cc_projection: None,
            rule_type: "tf_proto_library".to_string(),
            args: Vec::new(),
            kwargs: generation_kwargs,
        });

        let mut cc_kwargs = HashMap::new();
        cc_kwargs.insert("name".to_string(), Value::String(format!("{}_cc", name)));
        cc_kwargs.insert("actual".to_string(), Value::String(format!(":{}", name)));
        cc_kwargs.insert("visibility".to_string(), default_visibility());
        rule_calls.push(RuleCall {
            cc_projection: None,
            rule_type: "alias".to_string(),
            args: Vec::new(),
            kwargs: cc_kwargs,
        });

        let mut cc_impl_kwargs = HashMap::new();
        cc_impl_kwargs.insert("name".to_string(), Value::String(format!("{}_cc_impl", name)));
        cc_impl_kwargs.insert("actual".to_string(), Value::String(format!(":{}", name)));
        cc_impl_kwargs.insert("visibility".to_string(), default_visibility());
        rule_calls.push(RuleCall {
            cc_projection: None,
            rule_type: "alias".to_string(),
            args: Vec::new(),
            kwargs: cc_impl_kwargs,
        });
    }

    // 4. Create the {name}_genproto alias target (points to base proto)
    let mut genproto_kwargs = HashMap::new();
    genproto_kwargs.insert(
        "name".to_string(),
        Value::String(format!("{}_genproto", name)),
    );
    genproto_kwargs.insert("actual".to_string(), Value::String(format!(":{}", name)));
    genproto_kwargs.insert(
        "visibility".to_string(),
        kwargs
            .get("visibility")
            .cloned()
            .unwrap_or(Value::List(vec![Value::String(
                "//visibility:public".to_string(),
            )])),
    );
    rule_calls.push(RuleCall {
        cc_projection: None,
        rule_type: "alias".to_string(),
        args: Vec::new(),
        kwargs: genproto_kwargs,
    });

    // 5. Create the {name}_cc_genproto alias target (points to {name}_cc)
    let mut cc_genproto_kwargs = HashMap::new();
    cc_genproto_kwargs.insert(
        "name".to_string(),
        Value::String(format!("{}_cc_genproto", name)),
    );
    cc_genproto_kwargs.insert("actual".to_string(), Value::String(format!(":{}_cc", name)));
    cc_genproto_kwargs.insert(
        "visibility".to_string(),
        kwargs
            .get("visibility")
            .cloned()
            .unwrap_or(Value::List(vec![Value::String(
                "//visibility:public".to_string(),
            )])),
    );
    rule_calls.push(RuleCall {
        cc_projection: None,
        rule_type: "alias".to_string(),
        args: Vec::new(),
        kwargs: cc_genproto_kwargs,
    });

    // 6. Create the {name}_cc_headers_only alias target
    // TF's Starlark tf_proto_library (via tf_proto_library_cc → cc_proto_library)
    // exposes a header-only variant at this name. We alias it to the base so
    // dependents who want headers-only get whatever the base target produces.
    let mut cc_headers_kwargs = HashMap::new();
    cc_headers_kwargs.insert(
        "name".to_string(),
        Value::String(format!("{}_cc_headers_only", name)),
    );
    cc_headers_kwargs.insert("actual".to_string(), Value::String(format!(":{}", name)));
    cc_headers_kwargs.insert(
        "visibility".to_string(),
        kwargs
            .get("visibility")
            .cloned()
            .unwrap_or(Value::List(vec![Value::String(
                "//visibility:public".to_string(),
            )])),
    );
    rule_calls.push(RuleCall {
        cc_projection: None,
        rule_type: "alias".to_string(),
        args: Vec::new(),
        kwargs: cc_headers_kwargs,
    });

    // TSL's create_grpc_library declares a separate sibling, depending on the
    // ordinary C++ message library. It does not enable the CC message plugin;
    // only cc_grpc_version does that. Preserve a selected-only refusal for the
    // unimplemented sibling instead of aliasing it to pb-only results.
    if create_grpc_library {
        let stripped = name.strip_suffix("_proto").unwrap_or(&name);
        let mut grpc_kwargs = HashMap::from([
            (
                "name".to_string(),
                Value::String(format!("{}_cc_grpc_proto", stripped)),
            ),
            (
                "_rush_tf_proto_role".to_string(),
                Value::String("grpc_sibling".to_string()),
            ),
            (
                "srcs".to_string(),
                Value::List(vec![Value::String(format!(":{}", name))]),
            ),
            (
                "protodeps".to_string(),
                Value::List(vec![Value::String(format!(":{}_cc", name))]),
            ),
            ("visibility".to_string(), default_visibility()),
        ]);
        if let Some(testonly) = kwargs.get("testonly") {
            grpc_kwargs.insert("testonly".to_string(), testonly.clone());
        }
        rule_calls.push(RuleCall {
            cc_projection: None,
            rule_type: "tf_proto_library".to_string(),
            args: Vec::new(),
            kwargs: grpc_kwargs,
        });
    }

    Ok(rule_calls)
}

pub struct Evaluator {
    env: Environment,
    return_value: Option<Value>,
    break_flag: bool,
    continue_flag: bool,
    platform_config: PlatformConfig,
    /// Built-in functions
    builtins: HashMap<String, BuiltinFn>,
    /// Captured BUILD rule calls
    rule_calls: Vec<RuleCall>,
    /// Package-level visibility applied to rules without an explicit value.
    package_default_visibility: Option<Vec<Value>>,
    /// Whether this BUILD evaluation has already called `package()`.
    package_called: bool,
    /// .bzl file loader (optional, set by BuildOrchestrator)
    bzl_loader: Option<std::sync::Arc<std::sync::Mutex<crate::bzl_loader::BzlLoader>>>,
    /// Current package path (for native.package_name())
    current_package: String,
    /// Macro expanders (for TensorFlow macros, etc.)
    macro_expanders: HashMap<String, MacroExpander>,
    /// Disable parallel batch loading (for BUILD files to avoid deadlocks)
    /// When true, all .bzl loads are sequential instead of parallel batched.
    /// This prevents deadlocks from circular dependencies in BUILD file .bzl loads.
    disable_parallel_batch_load: bool,
    /// Current repository name (e.g. "org_tensorflow") if evaluating a file in an external repo.
    /// Used to resolve "//" to "@repo//" in load() statements.
    current_repository: Option<String>,
    /// #39 graph-RSS: absolute on-disk directory of the package being evaluated
    /// (the dir containing this BUILD file). Used to make `glob()` resolve
    /// against the real package tree for EXTERNAL repos too — previously
    /// external-repo globs fell back to a CWD-relative scan from the workspace
    /// root, following bazel-*/.cache symlink forests (e.g. @local_xla//xla
    /// `glob(["**/*.cc"])` matched millions of paths → OOM).
    current_package_dir: Option<std::path::PathBuf>,
    /// Callback for rule calls (e.g. to trigger immediate fetching of http_archive)
    rule_call_callback: Option<std::sync::Arc<dyn Fn(&RuleCall) + Send + Sync>>,
}

impl Evaluator {
    pub fn new() -> Self {
        let mut macro_expanders = HashMap::new();

        // Register TensorFlow macro expanders
        macro_expanders.insert(
            "tf_custom_op_library".to_string(),
            expand_tf_custom_op_library as MacroExpander,
        );
        macro_expanders.insert(
            "cc_header_only_library".to_string(),
            expand_cc_header_only_library as MacroExpander,
        );
        // GPU tf_cc_test expansion — only fires under RUSH_ENABLE_ROCM (force_macro_expand).
        macro_expanders.insert(
            "tf_gpu_cc_test".to_string(),
            expand_tf_gpu_cc_test as MacroExpander,
        );
        macro_expanders.insert(
            "tf_cuda_cc_test".to_string(),
            expand_tf_gpu_cc_test as MacroExpander,
        );
        macro_expanders.insert(
            "tf_kernel_library".to_string(),
            expand_tf_kernel_library as MacroExpander,
        );
        macro_expanders.insert(
            "tf_proto_library".to_string(),
            expand_tf_proto_library as MacroExpander,
        );
        macro_expanders.insert(
            "tf_gen_options_header".to_string(),
            expand_tf_gen_options_header as MacroExpander,
        );
        macro_expanders.insert(
            "gentbl_cc_library".to_string(),
            expand_gentbl_cc_library as MacroExpander,
        );
        macro_expanders.insert(
            "flatbuffer_cc_library".to_string(),
            expand_flatbuffer_cc_library as MacroExpander,
        );
        macro_expanders.insert(
            "tf_gen_op_wrappers_cc".to_string(),
            expand_tf_gen_op_wrappers_cc as MacroExpander,
        );
        macro_expanders.insert(
            "tf_gen_op_wrapper_cc".to_string(),
            expand_tf_gen_op_wrapper_cc as MacroExpander,
        );
        macro_expanders.insert(
            "tf_cc_tests".to_string(),
            expand_tf_cc_tests as MacroExpander,
        );
        macro_expanders.insert("check_deps".to_string(), expand_check_deps as MacroExpander);
        macro_expanders.insert(
            "cc_library_with_android_deps".to_string(),
            expand_cc_library_with_android_deps as MacroExpander,
        );

        // Add macro expanders for specific rules
        macro_expanders.insert("check_deps".to_string(), expand_check_deps as MacroExpander);
        macro_expanders.insert(
            "tf_gen_op_wrappers_cc".to_string(),
            expand_tf_gen_op_wrappers_cc as MacroExpander,
        );
        macro_expanders.insert(
            "tf_gen_op_wrapper_cc".to_string(),
            expand_tf_gen_op_wrapper_cc as MacroExpander,
        );

        let mut evaluator = Self {
            env: Environment::new(),
            return_value: None,
            break_flag: false,
            continue_flag: false,
            platform_config: PlatformConfig::default(),
            builtins: get_builtins(),
            rule_calls: Vec::new(),
            package_default_visibility: None,
            package_called: false,
            bzl_loader: None,
            current_package: String::new(),
            macro_expanders,
            disable_parallel_batch_load: false, // Default: allow parallel loading
            current_repository: None,
            current_package_dir: None,
            rule_call_callback: None,
        };

        evaluator.env.define(
            "PackageSpecificationInfo".to_string(),
            Value::Provider {
                name: "PackageSpecificationInfo".to_string(),
                fields: vec![],
                init: None,
            },
        );
        evaluator.env.define(
            "RunEnvironmentInfo".to_string(),
            Value::Provider {
                name: "RunEnvironmentInfo".to_string(),
                fields: vec![],
                init: None,
            },
        );

        // Bazel 8+ first-class rule-definition APIs. Rush does not need their
        // execution semantics while loading metadata, but these globals must
        // be callable so compatibility libraries can define symbolic macros
        // and subrules. The set constructor is likewise exposed as an opaque
        // native value until set operations are needed by an analyzed target.
        evaluator.env.define(
            "macro".to_string(),
            Value::NativeFunction("macro".to_string()),
        );
        evaluator.env.define(
            "subrule".to_string(),
            Value::NativeFunction("subrule".to_string()),
        );
        evaluator
            .env
            .define("set".to_string(), Value::NativeFunction("set".to_string()));

        evaluator
    }

    /// #39 graph-RSS: set the absolute on-disk dir of the package being
    /// evaluated, so `glob()` resolves against the real package tree (incl.
    /// external repos) instead of a CWD-relative scan from the workspace root.
    pub fn set_current_package_dir(&mut self, dir: Option<std::path::PathBuf>) {
        if dir.is_some() && self.env.get("repository_name").is_none() {
            // A bare native repository_name is predeclared in BUILD only;
            // loaded modules receive native.repository_name instead. Its
            // value is resolved at call time, not captured from this scope.
            self.env.define("repository_name".into(), Value::NativeFunction("repository_name".into()));
        } else if dir.is_none() && matches!(self.env.get("repository_name"), Some(Value::NativeFunction(name)) if name == "repository_name") {
            self.env.vars.write().unwrap().remove("repository_name");
        }
        self.current_package_dir = dir;
    }

    /// Set a callback to be invoked whenever a rule function is called.
    /// This allows external systems (like WorkspaceLoader) to react to rule calls immediately
    /// (e.g. to fetch http_archive repositories as soon as they are defined).
    pub fn set_rule_call_callback(
        &mut self,
        callback: std::sync::Arc<dyn Fn(&RuleCall) + Send + Sync>,
    ) {
        self.rule_call_callback = Some(callback);
    }

    /// Set the current repository name (e.g. "org_tensorflow")
    pub fn set_current_repository(&mut self, repo: Option<String>) {
        debug_log!("[DEBUG Evaluator] set_current_repository: {:?}", repo);
        self.current_repository = repo;
        self.env.definition_label_context = None;
        self.env.cc_projection_origin = None;
    }

    pub(crate) fn set_cc_projection_origin(
        &mut self,
        origin: Arc<crate::cc_projection_definition::DefinitionOrigin>,
    ) {
        self.env.cc_projection_origin = Some(origin);
    }

    pub(crate) fn set_definition_label_context(&mut self, context: DefinitionLabelContext) {
        self.env.definition_label_context = Some(Arc::new(context));
        self.env.bzl_definition_scope = true;
    }

    fn definition_label_context(&mut self) -> Result<Arc<DefinitionLabelContext>> {
        if let Some(context) = &self.env.definition_label_context {
            return Ok(context.clone());
        }
        let context = if let Some(loader) = &self.bzl_loader {
            loader.lock().map_err(|error| anyhow!("Failed to lock label context: {error}"))?
                .definition_label_context_for_owner(
                    self.current_repository.as_deref(), &self.current_package,
                )?
        } else {
            DefinitionLabelContext {
                repository: self.current_repository.clone(),
                package: self.current_package.clone(),
                repository_mapping: std::collections::BTreeMap::new(),
                mapping_is_complete: false,
            }
        };
        let context = Arc::new(context);
        self.env.definition_label_context = Some(context.clone());
        Ok(context)
    }

    fn closure_environment(&mut self) -> Result<Arc<Environment>> {
        let context = self.definition_label_context()?;
        let mut environment = self.env.clone();
        environment.definition_label_context = Some(context);
        Ok(Arc::new(environment))
    }

    fn qualify_proto_tool_label(&self, requested: &Value) -> Result<String> {
        let input = match requested {
            Value::Label { label, .. } => return Ok(label.clone()),
            Value::String(label) => label,
            _ => anyhow::bail!("tf_proto_library protoc must be a label string or Label"),
        };
        let owner = self
            .current_repository
            .as_deref()
            .unwrap_or("")
            .trim_start_matches('@');
        if input.starts_with("@@") {
            return Ok(input.clone());
        }
        if let Some(explicit) = input.strip_prefix('@') {
            let (repository, target) = explicit
                .split_once("//")
                .context("tf_proto_library protoc repository label must contain //")?;
            let canonical = if repository.is_empty() {
                String::new()
            } else if let Some(loader) = &self.bzl_loader {
                let context = loader
                    .lock()
                    .map_err(|error| anyhow!("Failed to lock proto tool label context: {error}"))?
                    .definition_label_context_for_owner(
                        self.current_repository.as_deref(),
                        &self.current_package,
                    )?;
                match context.repository_mapping.get(repository) {
                    Some(mapped) => mapped.clone(),
                    None if context.mapping_is_complete => {
                        anyhow::bail!("tf_proto_library protoc repository '@{}' is not visible from BUILD repository '{}'", repository, owner);
                    }
                    None => repository.to_string(),
                }
            } else {
                repository.to_string()
            };
            Ok(format!("@@{canonical}//{target}"))
        } else if input.starts_with("//") {
            Ok(format!("@@{owner}{input}"))
        } else {
            Ok(format!(
                "@@{owner}//{}:{}",
                self.current_package,
                input.strip_prefix(':').unwrap_or(input)
            ))
        }
    }
    fn qualify_label_argument(&mut self, arguments: &mut [Value]) -> Result<()> {
        let Some(Value::String(label)) = arguments.first_mut() else {
            return Ok(());
        };
        if label.starts_with("@@") {
            return Ok(());
        }
        let context = self.definition_label_context()?;
        let owner = context.repository.as_deref().unwrap_or("");
        if let Some(explicit) = label.strip_prefix('@') {
            let Some((repository, target)) = explicit.split_once("//") else {
                anyhow::bail!("Label repository reference must contain //: {}", label);
            };
            let canonical = if repository.is_empty() {
                ""
            } else if let Some(mapped) = context.repository_mapping.get(repository) {
                mapped.as_str()
            } else if context.mapping_is_complete {
                anyhow::bail!(
                    "Label repository '@{}' is not visible from defining repository '{}'",
                    repository, owner,
                );
            } else {
                repository
            };
            *label = format!("@@{canonical}//{target}");
        } else if label.starts_with("//") {
            *label = format!("@@{owner}{label}");
        } else if label.starts_with(':') {
            *label = format!("@@{owner}//{}{label}", context.package);
        }
        Ok(())
    }

    pub fn get_var(&self, name: &str) -> Option<Value> {
        self.env.get(name)
    }

    /// Export all variables from the evaluator's environment
    pub fn export_environment(&self) -> HashMap<String, Value> {
        self.env.export_all()
    }

    pub fn platform_config(&self) -> &PlatformConfig {
        &self.platform_config
    }

    pub fn platform_config_mut(&mut self) -> &mut PlatformConfig {
        &mut self.platform_config
    }

    /// Return a read-only view of captured rule calls (for WORKSPACE/BUILD scans)
    pub fn captured_rule_calls(&self) -> &Vec<RuleCall> {
        &self.rule_calls
    }

    /// Register an alias for a repository rule (e.g., tf_http_archive -> http_archive)
    pub fn register_repo_rule_alias(&mut self, alias: &str, target: &str) {
        self.env
            .define(alias.to_string(), Value::RuleFunction(target.to_string()));
    }

    fn existing_label_metadata(&self, value: &Value) -> Value {
        let label_text = |label: &str| {
            if let Some(label) = label.strip_prefix("//") {
                let (package, target) = label
                    .split_once(':')
                    .unwrap_or_else(|| (label, label.rsplit('/').next().unwrap_or(label)));
                if package == self.current_package {
                    format!(":{target}")
                } else {
                    format!("//{package}:{target}")
                }
            } else if label.starts_with('@') || label.starts_with(':') {
                label.to_string()
            } else {
                format!(":{label}")
            }
        };
        match value {
            Value::String(label) => Value::String(label_text(label)),
            Value::Label {
                package,
                name,
                workspace_name,
                ..
            } => {
                let owner = self
                    .current_repository
                    .as_deref()
                    .unwrap_or("")
                    .trim_start_matches('@');
                Value::String(if workspace_name.trim_start_matches('@') == owner {
                    label_text(&format!("//{package}:{name}"))
                } else {
                    format!("@{workspace_name}//{package}:{name}")
                })
            }
            Value::List(values) | Value::Tuple(values) | Value::FrozenList(values) => Value::Tuple(
                values
                    .iter()
                    .map(|value| self.existing_label_metadata(value))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    fn existing_rule_view(&self, rule_call: &RuleCall) -> Value {
        // Keep the existing plural API's supported defaults in one place.
        // This is not a claim to implement every native rule's Bazel schema.
        const DEFAULT_LIST_ATTRS: [&str; 13] = [
            "srcs",
            "hdrs",
            "deps",
            "data",
            "args",
            "copts",
            "defines",
            "includes",
            "linkopts",
            "additional_linker_inputs",
            "tags",
            "features",
            "visibility",
        ];
        let mut entries: HashMap<_, _> = rule_call
            .kwargs
            .iter()
            .filter(|(name, _)| name.chars().next().is_some_and(char::is_alphabetic))
            .map(|(name, value)| {
                let value = if matches!(
                    name.as_str(),
                    "srcs"
                        | "hdrs"
                        | "deps"
                        | "data"
                        | "visibility"
                        | "exports"
                        | "actual"
                        | "tools"
                ) {
                    self.existing_label_metadata(value)
                } else {
                    value.clone()
                };
                (name.clone(), ImmutableMapping::freeze(&value))
            })
            .collect();
        for name in DEFAULT_LIST_ATTRS {
            entries
                .entry(name.to_string())
                .or_insert_with(|| Value::Tuple(Vec::new()));
        }
        entries.insert(
            "kind".to_string(),
            Value::String(rule_call.rule_type.clone()),
        );
        Value::ImmutableMapping(Arc::new(ImmutableMapping {
            entries,
            kind: ImmutableMappingKind::RuleView,
        }))
    }

    /// Register a BUILD rule function (e.g., "cc_library", "tf_cc_library")
    pub fn register_rule_function(&mut self, name: String) {
        // Create a placeholder value - we'll intercept calls in eval_call
        self.env.define(name, Value::None);
    }

    fn workspace_native_bindings() -> HashMap<String, Value> {
        let mut native_attrs = HashMap::new();

        // Register repository rule types
        native_attrs.insert(
            "http_archive".to_string(),
            Value::RuleFunction("http_archive".to_string()),
        );
        native_attrs.insert(
            "http_file".to_string(),
            Value::RuleFunction("http_file".to_string()),
        );
        native_attrs.insert(
            "http_jar".to_string(),
            Value::RuleFunction("http_jar".to_string()),
        );
        native_attrs.insert(
            "maven_jar".to_string(),
            Value::RuleFunction("maven_jar".to_string()),
        );
        native_attrs.insert(
            "java_import_external".to_string(),
            Value::RuleFunction("java_import_external".to_string()),
        );
        native_attrs.insert(
            "git_repository".to_string(),
            Value::RuleFunction("git_repository".to_string()),
        );
        native_attrs.insert(
            "local_repository".to_string(),
            Value::RuleFunction("local_repository".to_string()),
        );
        native_attrs.insert(
            "new_local_repository".to_string(),
            Value::RuleFunction("new_local_repository".to_string()),
        );

        // TensorFlow-specific repository rules (treat as local repositories)
        native_attrs.insert(
            "tf_vendored".to_string(),
            Value::RuleFunction("tf_vendored".to_string()),
        );
        native_attrs.insert(
            "rocm_configure".to_string(),
            Value::RuleFunction("rocm_configure".to_string()),
        );

        // Register other WORKSPACE-specific functions as native functions (metadata-onlys for now)
        native_attrs.insert(
            "bind".to_string(),
            Value::NativeFunction("bind".to_string()),
        );
        native_attrs.insert(
            "register_toolchains".to_string(),
            Value::NativeFunction("register_toolchains".to_string()),
        );
        native_attrs.insert(
            "register_execution_platforms".to_string(),
            Value::NativeFunction("register_execution_platforms".to_string()),
        );

        native_attrs.insert("repository_name".into(), Value::NativeFunction("repository_name".into()));

        // Register rule query functions
        native_attrs.insert(
            "existing_rule".to_string(),
            Value::NativeFunction("existing_rule".to_string()),
        );
        native_attrs.insert(
            "existing_rules".to_string(),
            Value::NativeFunction("existing_rules".to_string()),
        );

        native_attrs
    }

    /// Register the native namespace for WORKSPACE macros
    ///
    /// Provides native.http_archive, native.git_repository, etc. for use in WORKSPACE .bzl files
    pub fn register_workspace_native_namespace(&mut self) {
        self.set_current_package_dir(None);
        let native_attrs = Self::workspace_native_bindings();
        self.env
            .define("native".to_string(), Value::Namespace(native_attrs));
    }

    /// Enable the WORKSPACE-only native version API. Loading an ordinary Rush
    /// module does not resolve or run Bazel; only reading bazel_version probes
    /// the repository analyzer selected by the current tracked client context.
    pub fn set_workspace_repository_context(&mut self, workspace_root: std::path::PathBuf) {
        self.set_current_package_dir(None);
        let attrs = match self.env.get("native") {
            Some(Value::Namespace(attrs)) | Some(Value::WorkspaceNativeNamespace { attrs, .. }) => {
                attrs.clone()
            }
            _ => Self::workspace_native_bindings(),
        };
        self.env.define(
            "native".to_string(),
            Value::WorkspaceNativeNamespace {
                attrs,
                workspace_root,
            },
        );
    }

    /// Register the native namespace for macro support
    ///
    /// Provides native.cc_library, native.cc_binary, etc. for use in .bzl macros
    pub fn register_native_namespace(&mut self) {
        let mut native_attrs = Self::workspace_native_bindings();

        // Register all standard rule types
        let rule_types = vec![
            "cc_library",
            "cc_binary",
            "cc_test",
            "cc_shared_library",
            "proto_library",
            "proto_lang_toolchain",
            "cc_proto_library",
            "java_proto_library",
            "java_lite_proto_library",
            "java_library",
            "java_binary",
            "java_import",
            "java_plugin",
            "java_test",
            // Synthesized @build_bazel_rules_android facade rules. These are
            // captured as real targets and rejected only if they enter the
            // configured target closure; see tensorflow-rules/android.rs.
            "android_library",
            "android_binary",
            "aar_import",
            "cc_import",
            "filegroup",
            "genrule",
            "alias",
            // config_setting via the `native.` namespace: external repos like
            // @org_brotli call `native.config_setting(...)` inside helper
            // macros (compiler_config_setting.bzl::create_msvc_config). It was
            // registered top-level but missing from `native.`, so those .bzl
            // macros failed with "Attribute 'config_setting' not found in
            // namespace" and aborted the BUILD eval.
            "config_setting",
            "label_flag",
            // Configuration-noun natives (metadata-only, like
            // config_setting): Modular's root BUILD declares platform()
            // targets and bazel/internal/cc-toolchain declares toolchain().
            "platform",
            "toolchain",
            "toolchain_type",
            "constraint_setting",
            "constraint_value",
            "package_group",
            "test_suite",
            "py_library",
            "py_binary",
            "py_test",
            "py_runtime",
            "py_runtime_pair",
        ];

        // Bazel 8 removed the native sh_* rules; rules_shell probes
        // `hasattr(native, "sh_binary")` and, when it exists, falls back to
        // a Bazel-6-era wiring that creates an alias cycle with
        // @bazel_tools//tools/bash/runfiles under Bzlmod. Advertise the
        // native sh_* entries only for WORKSPACE-era trees (TensorFlow).
        let bzlmod_only_workspace = std::env::current_dir()
            .map(|d| {
                d.join("MODULE.bazel").exists()
                    && !d.join("WORKSPACE").exists()
                    && !d.join("WORKSPACE.bazel").exists()
            })
            .unwrap_or(false);
        let mut rule_types = rule_types;
        if !bzlmod_only_workspace {
            rule_types.push("sh_binary");
            rule_types.push("sh_test");
        }

        for rule_type in rule_types {
            native_attrs.insert(
                rule_type.to_string(),
                Value::RuleFunction(rule_type.to_string()),
            );
        }

        // Register native functions (not rules)
        native_attrs.insert(
            "glob".to_string(),
            Value::NativeFunction("glob".to_string()),
        );
        native_attrs.insert(
            "existing_rule".to_string(),
            Value::NativeFunction("existing_rule".to_string()),
        );
        native_attrs.insert(
            "existing_rules".to_string(),
            Value::NativeFunction("existing_rules".to_string()),
        );
        native_attrs.insert(
            "exports_files".to_string(),
            Value::NativeFunction("exports_files".to_string()),
        );
        native_attrs.insert(
            "package".to_string(),
            Value::NativeFunction("package".to_string()),
        );
        native_attrs.insert(
            "package_name".to_string(),
            Value::NativeFunction("package_name".to_string()),
        );
        // Bazel 6.3+: native.package_relative_label() (generated pip lock
        // BUILD files and aspect_bazel_lib macros use it).
        native_attrs.insert(
            "package_relative_label".to_string(),
            Value::NativeFunction("package_relative_label".to_string()),
        );

        self.env
            .define("native".to_string(), Value::Namespace(native_attrs));

        // Also register common native functions at top-level for BUILD files
        // (they're accessible both directly and via native. namespace)
        self.env.define(
            "glob".to_string(),
            Value::NativeFunction("glob".to_string()),
        );
        // BUILD files may call package_name() and package_relative_label()
        // bare (without the native. prefix); dir() is a universal builtin.
        self.env.define(
            "package_name".to_string(),
            Value::NativeFunction("package_name".to_string()),
        );
        self.env.define(
            "package_relative_label".to_string(),
            Value::NativeFunction("package_relative_label".to_string()),
        );
        self.env
            .define("dir".to_string(), Value::NativeFunction("dir".to_string()));
        self.env.define(
            "exports_files".to_string(),
            Value::NativeFunction("exports_files".to_string()),
        );
        self.env.define(
            "package".to_string(),
            Value::NativeFunction("package".to_string()),
        );

        // Register common rules at top-level
        self.register_rule_function("cc_library".to_string());
        self.register_rule_function("cc_binary".to_string());
        self.register_rule_function("cc_test".to_string());
        self.register_rule_function("py_library".to_string());
        self.register_rule_function("py_binary".to_string());
        self.register_rule_function("py_test".to_string());
        self.register_rule_function("sh_binary".to_string());
        self.register_rule_function("sh_test".to_string());
        self.register_rule_function("genrule".to_string());
        self.register_rule_function("filegroup".to_string());
        self.register_rule_function("alias".to_string());
        self.register_rule_function("test_suite".to_string());
        self.register_rule_function("proto_library".to_string());
        self.register_rule_function("proto_lang_toolchain".to_string());
        self.register_rule_function("cc_proto_library".to_string());
        self.register_rule_function("java_proto_library".to_string());
        self.register_rule_function("java_lite_proto_library".to_string());
        self.register_rule_function("java_library".to_string());
        self.register_rule_function("java_binary".to_string());
        self.register_rule_function("java_import".to_string());
        self.register_rule_function("java_plugin".to_string());
        self.register_rule_function("java_test".to_string());
        self.register_rule_function("config_setting".to_string());
        self.register_rule_function("label_flag".to_string());
        self.register_rule_function("platform".to_string());
        self.register_rule_function("toolchain".to_string());
        self.register_rule_function("toolchain_type".to_string());
        self.register_rule_function("constraint_setting".to_string());
        self.register_rule_function("constraint_value".to_string());
        self.register_rule_function("package_group".to_string());

        // Register TensorFlow helper functions (loaded from tensorflow.bzl)
        // These are commonly used across TensorFlow BUILD files
        self.env.define(
            "get_compatible_with_portable".to_string(),
            Value::NativeFunction("get_compatible_with_portable".to_string()),
        );
        self.env.define(
            "tensorflow_opensource_extra_deps".to_string(),
            Value::NativeFunction("tensorflow_opensource_extra_deps".to_string()),
        );
        self.env.define(
            "tf_monitoring_framework_deps".to_string(),
            Value::NativeFunction("tf_monitoring_framework_deps".to_string()),
        );
        self.env.define(
            "tf_selective_registration_deps".to_string(),
            Value::NativeFunction("tf_selective_registration_deps".to_string()),
        );
        self.env.define(
            "cc_header_only_library".to_string(),
            Value::NativeFunction("cc_header_only_library".to_string()),
        );

        // Register attr module (Bazel attribute definitions for custom rules)
        // We provide minimal implementations for these out since we're not implementing custom rules, just loading .bzl files
        let mut attr_module = HashMap::new();
        attr_module.insert(
            "string".to_string(),
            Value::NativeFunction("attr.string".to_string()),
        );
        attr_module.insert(
            "label".to_string(),
            Value::NativeFunction("attr.label".to_string()),
        );
        attr_module.insert(
            "label_list".to_string(),
            Value::NativeFunction("attr.label_list".to_string()),
        );
        attr_module.insert(
            "int".to_string(),
            Value::NativeFunction("attr.int".to_string()),
        );
        // FIX #209: Add attr.int_list for TensorFlow's API generation
        attr_module.insert(
            "int_list".to_string(),
            Value::NativeFunction("attr.int_list".to_string()),
        );
        attr_module.insert(
            "bool".to_string(),
            Value::NativeFunction("attr.bool".to_string()),
        );
        attr_module.insert(
            "string_list".to_string(),
            Value::NativeFunction("attr.string_list".to_string()),
        );
        attr_module.insert(
            "string_dict".to_string(),
            Value::NativeFunction("attr.string_dict".to_string()),
        );
        attr_module.insert(
            "string_list_dict".to_string(),
            Value::NativeFunction("attr.string_list_dict".to_string()),
        );
        attr_module.insert(
            "output".to_string(),
            Value::NativeFunction("attr.output".to_string()),
        );
        attr_module.insert(
            "output_list".to_string(),
            Value::NativeFunction("attr.output_list".to_string()),
        );
        attr_module.insert(
            "label_keyed_string_dict".to_string(),
            Value::NativeFunction("attr.label_keyed_string_dict".to_string()),
        );
        // Bazel 7.4+: dict attribute with string keys and Label values
        // (used by @bazel_tools http.bzl _http_archive_attrs "files").
        attr_module.insert(
            "string_keyed_label_dict".to_string(),
            Value::NativeFunction("attr.string_keyed_label_dict".to_string()),
        );
        self.env
            .define("attr".to_string(), Value::Namespace(attr_module));

        // Register config module (Bazel build settings)
        let mut config_module = HashMap::new();
        config_module.insert(
            "string".to_string(),
            Value::NativeFunction("config.string".to_string()),
        );
        config_module.insert(
            "bool".to_string(),
            Value::NativeFunction("config.bool".to_string()),
        );
        config_module.insert(
            "int".to_string(),
            Value::NativeFunction("config.int".to_string()),
        );
        config_module.insert(
            "string_list".to_string(),
            Value::NativeFunction("config.string_list".to_string()),
        );
        // Configuration-transition methods used in attr definitions:
        // attr.label(cfg = config.exec(exec_group=…)) / config.target() /
        // config.none(). rush runs all actions on the host and does not model
        // split configurations, so these are opaque markers — present so rule/attr
        // defs that reference them evaluate (e.g. rules_shell sh_test cfg=config.exec).
        config_module.insert(
            "exec".to_string(),
            Value::NativeFunction("config.exec".to_string()),
        );
        config_module.insert(
            "target".to_string(),
            Value::NativeFunction("config.target".to_string()),
        );
        config_module.insert(
            "none".to_string(),
            Value::NativeFunction("config.none".to_string()),
        );
        self.env
            .define("config".to_string(), Value::Namespace(config_module));

        // Starlark `json` module (json.encode / json.decode / json.encode_indent).
        // protobuf's compiler BUILD uses json.encode; these are real (serde_json),
        // not stubs — see the native-function dispatch for the implementations.
        let mut json_module = HashMap::new();
        json_module.insert(
            "encode".to_string(),
            Value::NativeFunction("json.encode".to_string()),
        );
        json_module.insert(
            "decode".to_string(),
            Value::NativeFunction("json.decode".to_string()),
        );
        json_module.insert(
            "encode_indent".to_string(),
            Value::NativeFunction("json.encode_indent".to_string()),
        );
        self.env
            .define("json".to_string(), Value::Namespace(json_module));
    }

    /// Get captured BUILD rule calls
    pub fn rule_calls(&self) -> &[RuleCall] {
        &self.rule_calls
    }

    fn record_package_metadata(
        &mut self,
        args: &[Value],
        kwargs: &HashMap<String, Value>,
    ) -> Result<()> {
        if !args.is_empty() {
            return Err(anyhow!("package() does not accept positional arguments"));
        }
        if self.package_called {
            return Err(anyhow!("package() may only be called once per BUILD file"));
        }
        self.package_called = true;

        let Some(default_visibility) = kwargs.get("default_visibility") else {
            return Ok(());
        };
        let (Value::List(specifications) | Value::Tuple(specifications) | Value::FrozenList(specifications)) = default_visibility else {
            return Err(anyhow!(
                "package(default_visibility) must be a list of labels"
            ));
        };
        if !specifications
            .iter()
            .all(|specification| matches!(specification, Value::String(_)))
        {
            return Err(anyhow!(
                "package(default_visibility) must be a list of labels"
            ));
        }
        self.package_default_visibility = Some(specifications.clone());
        Ok(())
    }

    fn apply_package_default_visibility(&mut self) {
        let default_visibility = self.package_default_visibility.clone();
        for rule_call in &mut self.rule_calls {
            if rule_call.rule_type == "config_setting" {
                let was_explicit = !matches!(
                    rule_call.kwargs.get("visibility"),
                    None | Some(Value::None)
                );
                rule_call.kwargs.insert(
                    VISIBILITY_EXPLICIT_MARKER.to_string(),
                    Value::Bool(was_explicit),
                );
            }
            // package_group and exports_files create targets, but neither is a
            // rule. Bazel does not attach rule visibility to package groups,
            // and exported files have their own public-by-default semantics.
            if matches!(
                rule_call.rule_type.as_str(),
                "package_group" | "source_file"
            ) {
                continue;
            }
            // Bazel treats visibility = None like an omitted visibility: both
            // inherit the package default. An explicit empty list remains an
            // empty list and therefore grants no cross-package access.
            if matches!(rule_call.kwargs.get("visibility"), None | Some(Value::None)) {
                if let Some(default_visibility) = &default_visibility {
                    rule_call.kwargs.insert(
                        "visibility".to_string(),
                        Value::List(default_visibility.clone()),
                    );
                }
            }
        }
    }

    fn push_source_file_rule_call(&mut self, file: String, visibility: Vec<Value>) -> Result<()> {
        if let Some(existing) = self.rule_calls.iter().find(|rule_call| {
            rule_call.rule_type == "source_file"
                && matches!(
                    rule_call.kwargs.get("name"),
                    Some(Value::String(name)) if name == &file
                )
        }) {
            if existing.kwargs.get("visibility") == Some(&Value::List(visibility)) {
                return Ok(());
            }
            return Err(anyhow!(
                "visibility for exported file '{}' declared twice",
                file
            ));
        }

        let rule_call = source_file_rule_call(file, visibility);
        if let Some(callback) = &self.rule_call_callback {
            callback(&rule_call);
        }
        self.rule_calls.push(rule_call);
        Ok(())
    }

    fn backfill_exports_files_source_file_rule_calls(&mut self, program: &Program) -> Result<()> {
        for (file, visibility) in collect_literal_exports_files_sources(program) {
            self.push_source_file_rule_call(file, visibility)?;
        }
        Ok(())
    }

    /// Set the .bzl file loader (called by BuildOrchestrator)
    pub fn set_bzl_loader(
        &mut self,
        loader: std::sync::Arc<std::sync::Mutex<crate::bzl_loader::BzlLoader>>,
    ) {
        self.bzl_loader = Some(loader);
        self.env.definition_label_context = None;
    }

    pub fn get_rule_calls(&self) -> &Vec<RuleCall> {
        &self.rule_calls
    }

    /// Disable parallel batch loading of .bzl files (for BUILD files to avoid deadlocks)
    ///
    /// When set to true, all .bzl file loads will be sequential instead of parallel batched.
    /// This prevents deadlocks caused by circular dependencies in TensorFlow's BUILD file .bzl loads.
    ///
    /// WORKSPACE files should keep this false (parallel loading is safe and beneficial there).
    /// BUILD files should set this to true to avoid multi-threaded deadlocks.
    pub fn set_disable_parallel_batch_load(&mut self, disable: bool) {
        self.disable_parallel_batch_load = disable;
    }

    /// Set thread-local BUILD file context flag (affects all evaluators in this thread)
    ///
    /// When set to true, ALL evaluators in the current thread will disable parallel batch loading,
    /// including .bzl file evaluators created during BUILD file evaluation.
    /// This prevents deadlocks from circular dependencies in BUILD-related .bzl loads.
    ///
    /// Should be set to true before evaluating a BUILD file and set to false after.
    pub fn set_build_file_context(in_build_context: bool) {
        IN_BUILD_FILE_CONTEXT.set(in_build_context);
    }

    /// Convenience helper that enters BUILD context and returns a guard.
    pub fn build_file_context_guard() -> BuildFileContextGuard {
        BuildFileContextGuard::enter()
    }

    /// Set the current package path (for native.package_name())
    pub fn set_current_package(&mut self, package: String) {
        crate::builtins::clear_package_glob_cache();
        self.current_package = package;
        self.env.definition_label_context = None;
    }

    /// Activate configuration rules and aliases used by `select()`.
    fn maybe_activate_config_setting(&self, rule_call: &RuleCall) {
        let name = match rule_call.kwargs.get("name") {
            Some(Value::String(s)) => s.clone(),
            _ => return,
        };

        if rule_call.rule_type == "config_setting" {
            let string_dict = |attribute: &str, allow_label_key: bool| {
                rule_call
                    .kwargs
                    .get(attribute)
                    .map(|value| {
                        let Value::Dict(values) = value else {
                            unreachable!("config_setting attributes are validated before activation")
                        };
                        values
                            .iter()
                            .map(|(key, value)| {
                                let Value::String(value) = value else {
                                    unreachable!("config_setting attributes are validated before activation")
                                };
                                let key = if allow_label_key {
                                    configuration_label_dictionary_key(key).unwrap_or_else(|| {
                                        unreachable!(
                                            "config_setting attributes are validated before activation"
                                        )
                                    })
                                } else {
                                    key.clone()
                                };
                                (key, value.clone())
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let define_values = string_dict("define_values", false);
            let native_values = string_dict("values", false);
            let constraint_values = rule_call
                .kwargs
                .get("constraint_values")
                .map(|value| {
                    let (Value::List(values) | Value::Tuple(values) | Value::FrozenList(values)) = value else {
                        unreachable!("config_setting attributes are validated before activation")
                    };
                    values
                })
                .map(|values| {
                    values
                        .iter()
                        .map(|value| match value {
                            Value::String(value) => value.clone(),
                            Value::Label { label, .. } => label.clone(),
                            _ => unreachable!(
                                "config_setting attributes are validated before activation"
                            ),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let flag_values = string_dict("flag_values", true);
            crate::config_defines::consider_config_setting_predicates(
                self.current_repository.as_deref(),
                &self.current_package,
                &name,
                &define_values,
                &native_values,
                &constraint_values,
                &flag_values,
            );
            crate::config_defines::refresh_composed_config_settings();
            return;
        }

        if rule_call.rule_type == "constraint_value" {
            crate::config_defines::consider_constraint_value(
                self.current_repository.as_deref(),
                &self.current_package,
                &name,
            );
        }

        let is_declared_build_setting = rule_call.rule_type == "label_flag"
            || matches!(
                rule_call.kwargs.get("__rush_build_setting"),
                Some(Value::Bool(true))
            );
        if is_declared_build_setting {
            let Some(default) = rule_call.kwargs.get("build_setting_default") else {
                return;
            };
            let value = match default {
                Value::Bool(value) => crate::config_defines::BuildSettingValue::Bool(*value),
                Value::String(value) => {
                    crate::config_defines::BuildSettingValue::String(value.clone())
                }
                Value::Int(value) => crate::config_defines::BuildSettingValue::Int(*value),
                _ => return,
            };
            crate::config_defines::consider_typed_build_setting_default(
                self.current_repository.as_deref(),
                &self.current_package,
                &name,
                value,
            );
        }

        if rule_call.rule_type == "alias" {
            if let Some(Value::String(actual)) = rule_call.kwargs.get("actual") {
                crate::config_defines::consider_config_alias(
                    self.current_repository.as_deref(),
                    &self.current_package,
                    &name,
                    actual,
                );
            }
        }
    }

    fn validate_loading_time_rule_attributes(
        rule_type: &str,
        kwargs: &HashMap<String, Value>,
    ) -> Result<()> {
        if rule_type == "config_setting" {
            let mut populated = false;
            for attribute in ["define_values", "values", "flag_values"] {
                let Some(value) = kwargs.get(attribute) else {
                    continue;
                };
                let Value::Dict(values) = value else {
                    return Err(anyhow!(
                        "in call to config_setting(), parameter '{}' got value of type '{}', want 'dict'",
                        attribute,
                        starlark_type_name(value)?
                    ));
                };
                populated |= !values.is_empty();
                for (key, value) in values {
                    if let Some((kind, visible)) = non_string_dictionary_key(key) {
                        if !(attribute == "flag_values" && kind == "Label") {
                            return Err(anyhow!(
                                "config_setting.{} key must be a string, got {} ({})",
                                attribute,
                                visible,
                                kind
                            ));
                        }
                    }
                    if !matches!(value, Value::String(_)) {
                        return Err(anyhow!(
                            "config_setting.{} value for key '{}' must be a string, got {} ({})",
                            attribute,
                            key,
                            starlark_repr(value),
                            starlark_type_name(value)?
                        ));
                    }
                }
            }

            if let Some(value) = kwargs.get("constraint_values") {
                let (Value::List(values) | Value::Tuple(values) | Value::FrozenList(values)) = value else {
                    return Err(anyhow!(
                        "in call to config_setting(), parameter 'constraint_values' got value of type '{}', want 'sequence'",
                        starlark_type_name(value)?
                    ));
                };
                populated |= !values.is_empty();
                for (index, value) in values.iter().enumerate() {
                    if !matches!(value, Value::String(_) | Value::Label { .. }) {
                        return Err(anyhow!(
                            "expected a label for element {} of 'config_setting.constraint_values', got {} ({})",
                            index,
                            starlark_repr(value),
                            starlark_type_name(value)?
                        ));
                    }
                }
            }

            return Ok(());
        }

        if rule_type == "constraint_value" {
            return match kwargs.get("constraint_setting") {
                Some(Value::String(_) | Value::Label { .. }) => Ok(()),
                Some(value) => Err(anyhow!(
                    "in call to constraint_value(), parameter 'constraint_setting' got value of type '{}', want 'label'",
                    starlark_type_name(value)?
                )),
                None => Err(anyhow!(
                    "missing value for mandatory attribute 'constraint_setting' in constraint_value"
                )),
            };
        }

        if rule_type != "package_group" {
            return Ok(());
        }

        // Bazel validates these attributes while loading the package. Preserve
        // the outer value's diagnostic before inspecting sequence elements so
        // a dict containing select(), for example, remains a dict type error.
        for attribute in ["packages", "includes"] {
            let Some(value) = kwargs.get(attribute) else {
                continue;
            };
            let (Value::List(values) | Value::Tuple(values) | Value::FrozenList(values)) = value else {
                return Err(anyhow!(
                    "in call to package_group(), parameter '{}' got value of type '{}', want 'sequence'",
                    attribute,
                    starlark_type_name(value)?
                ));
            };

            for (index, value) in values.iter().enumerate() {
                let valid = matches!(value, Value::String(_))
                    || attribute == "includes" && matches!(value, Value::Label { .. });
                if !valid {
                    return Err(anyhow!(
                        "expected value of type 'string' for element {} of 'package_group.{} argument', but got {} ({})",
                        index,
                        attribute,
                        starlark_repr(value),
                        starlark_type_name(value)?
                    ));
                }
            }
        }
        Ok(())
    }

    /// Get the current package path
    pub fn current_package(&self) -> &str {
        &self.current_package
    }

    fn prioritize_repo_local_loads(&self, loads: &mut Vec<LoadWithSymbols>) {
        if loads.len() <= 1 {
            return;
        }
        loads.sort_by(|(a_label, _), (b_label, _)| {
            let a_priority = self.repo_priority_score(a_label);
            let b_priority = self.repo_priority_score(b_label);
            a_priority
                .cmp(&b_priority)
                .then_with(|| a_label.cmp(b_label))
        });
    }

    fn repo_priority_score(&self, label: &str) -> u8 {
        if label.starts_with(':') {
            return 0;
        }
        if label.starts_with('@') {
            return 2;
        }
        if label.starts_with("//") {
            if !self.current_package.is_empty() {
                let mut prefix = String::from("//");
                prefix.push_str(self.current_package.as_str());
                prefix.push(':');
                if label.starts_with(&prefix) {
                    return 0;
                }
            }
            return 1;
        }
        1
    }

    pub fn eval_program(&mut self, program: &Program) -> Result<()> {
        // Detect consecutive Load statements at the start of the program
        // These are independent and can be loaded in parallel
        let mut consecutive_loads = vec![];
        let mut _first_non_load_idx = 0;

        debug_log!(
            "[DEBUG Evaluator] eval_program called with {} statements, bzl_loader present: {}",
            program.statements.len(),
            self.bzl_loader.is_some()
        );

        for (idx, stmt) in program.statements.iter().enumerate() {
            match stmt {
                Stmt::Load { file, symbols } => {
                    debug_log!("[DEBUG Evaluator] Found load statement #{}: {}", idx, file);
                    // Rewrite label if we are in an external repository
                    let file_to_load = if let Some(repo) = &self.current_repository {
                        if file.starts_with("//") {
                            format!("@@{}//{}", repo, &file[2..])
                        } else {
                            file.clone()
                        }
                    } else {
                        file.clone()
                    };
                    consecutive_loads.push((file_to_load, symbols.clone()));
                }
                // Skip docstrings (string expressions) at the start of the file
                // These are common in .bzl files and shouldn't break consecutive load detection
                Stmt::Expr(expr)
                    if matches!(&**expr, Expr::String(_))
                        && consecutive_loads.is_empty()
                        && idx == 0 =>
                {
                    debug_log!("[DEBUG Evaluator] Skipping docstring at index {}", idx);
                    continue;
                }
                _ => {
                    debug_log!(
                        "[DEBUG Evaluator] First non-load statement at index {}: {:?}",
                        idx,
                        stmt
                    );
                    _first_non_load_idx = idx;
                    break;
                }
            }
        }

        debug_log!(
            "[DEBUG Evaluator] Detected {} consecutive loads at start",
            consecutive_loads.len()
        );

        // If we have multiple consecutive loads at the start, batch-load them when possible
        let in_build_context = IN_BUILD_FILE_CONTEXT.get();
        let has_loader = self.bzl_loader.is_some();
        if consecutive_loads.len() > 1 && has_loader && in_build_context {
            debug_log!(
                "[DEBUG Evaluator] BUILD context detected - using cycle-aware batch loading for {} files",
                consecutive_loads.len()
            );
            self.batch_load_bzl_files(&consecutive_loads, BatchLoadMode::BuildSafe)?;
        } else if consecutive_loads.len() > 1 && has_loader && !self.disable_parallel_batch_load {
            debug_log!(
                "[DEBUG Evaluator] *** TRIGGERING PARALLEL LOAD *** Found {} consecutive load statements - loading in parallel",
                consecutive_loads.len()
            );
            self.batch_load_bzl_files(&consecutive_loads, BatchLoadMode::Default)?;
        } else if consecutive_loads.len() > 1 && has_loader {
            debug_log!(
                "[DEBUG Evaluator] Parallel batch loading disabled via evaluator flag, will load {} files sequentially",
                consecutive_loads.len()
            );
        } else if consecutive_loads.len() > 1 {
            debug_log!(
                "[WARN Evaluator] {} consecutive loads found but bzl_loader is None!",
                consecutive_loads.len()
            );
        } else if consecutive_loads.len() == 1 {
            debug_log!("[DEBUG Evaluator] Only 1 load found, will load sequentially");
        }

        // Now process all statements normally
        // Load statements that were batch-loaded will be cache hits
        // Load statements after non-load statements will be loaded sequentially
        for (_idx, stmt) in program.statements.iter().enumerate() {
            self.eval_statement(stmt)?;
            if self.return_value.is_some() {
                break;
            }
        }

        self.apply_package_default_visibility();

        Ok(())
    }

    /// Evaluate a program while SKIPPING all load() statements.
    ///
    /// This is designed for WORKSPACE .bzl files (workspace2.bzl, workspace3.bzl, etc.)
    /// that have 50+ load() statements at the top which cause memory explosion.
    ///
    /// Instead of loading all dependencies eagerly, we:
    /// 1. Skip all load() statements entirely
    /// 2. Evaluate only function definitions and assignments
    /// 3. Let the WORKSPACE evaluator call the functions (which may fail if they need loaded symbols)
    ///
    /// This reduces memory from 26 GB → <5 GB for TensorFlow WORKSPACE loading.
    pub fn eval_program_skip_loads(&mut self, program: &Program) -> Result<()> {
        debug_log!(
            "[LAZY WORKSPACE] eval_program_skip_loads: Processing {} statements, skipping all load()",
            program.statements.len()
        );

        // Process all statements EXCEPT load() statements
        for (idx, stmt) in program.statements.iter().enumerate() {
            match stmt {
                Stmt::Load { file, .. } => {
                    // SKIP: Don't load .bzl dependencies
                    debug_log!("[LAZY WORKSPACE]   Skipping load() #{}: {}", idx, file);
                    continue;
                }
                _ => {
                    // Evaluate everything else (function definitions, assignments, etc.)
                    self.eval_statement(stmt)?;
                    if self.return_value.is_some() {
                        break;
                    }
                }
            }
        }

        debug_log!("[LAZY WORKSPACE] eval_program_skip_loads complete");
        Ok(())
    }

    /// Evaluate a program with lazy .bzl loading - only load files whose symbols are actually used.
    ///
    /// This dramatically reduces .bzl loading time for BUILD files by skipping unused dependencies.
    /// For example, if a BUILD file loads 14 .bzl files but only uses symbols from 3 of them,
    /// we only load those 3 files instead of all 14.
    ///
    /// Enabled by default for BUILD parsing. Operators can disable it via
    /// `RUSH_LAZY_BUILD_BZLS=0` when investigating eager-loading behavior.
    pub fn eval_program_lazy_loads(&mut self, program: &Program) -> Result<()> {
        debug_log!(
            "[LAZY BUILD] eval_program_lazy_loads: Processing {} statements",
            program.statements.len()
        );

        let load_result = self.load_phase(program)?;
        self.execute_after_load(program, &load_result.skipped_loads)?;
        self.apply_package_default_visibility();
        self.backfill_exports_files_source_file_rule_calls(program)?;

        debug_log!("[LAZY BUILD] eval_program_lazy_loads complete");
        Ok(())
    }

    fn load_phase(&mut self, program: &Program) -> Result<LoadPhaseResult> {
        debug_log!(
            "[DEBUG Evaluator] load_phase called. current_repository: {:?}",
            self.current_repository
        );
        let mut load_statements = Vec::new();
        let mut first_non_load_idx = 0;

        for (idx, stmt) in program.statements.iter().enumerate() {
            match stmt {
                Stmt::Load { file, symbols } => {
                    debug_log!(
                        "[DEBUG Evaluator] Processing load: {} with repo: {:?}",
                        file,
                        self.current_repository
                    );
                    // Rewrite label if we are in an external repository
                    // e.g. inside @org_tensorflow, //foo:bar.bzl -> @org_tensorflow//foo:bar.bzl
                    let file_to_load = if let Some(repo) = &self.current_repository {
                        if file.starts_with("//") {
                            let rewritten = format!("@@{}//{}", repo, &file[2..]);
                            debug_log!(
                                "[DEBUG Evaluator] Rewriting load label in @{}: {} -> {}",
                                repo,
                                file,
                                rewritten
                            );
                            rewritten
                        } else {
                            file.clone()
                        }
                    } else {
                        file.clone()
                    };
                    load_statements.push((file_to_load, symbols.clone()));
                }
                Stmt::Expr(expr)
                    if matches!(&**expr, Expr::String(_))
                        && load_statements.is_empty()
                        && idx == 0 =>
                {
                    continue;
                }
                _ => {
                    first_non_load_idx = idx;
                    break;
                }
            }
        }

        debug_log!(
            "[LAZY BUILD] Found {} load() statements at start of file",
            load_statements.len()
        );

        let mut used_symbols = HashSet::new();
        for stmt in program.statements.iter().skip(first_non_load_idx) {
            self.collect_used_symbols(stmt, &mut used_symbols);
        }

        debug_log!(
            "[LAZY BUILD] Identified {} unique symbols used in BUILD file",
            used_symbols.len()
        );

        let mut needed_loads = Vec::new();
        let mut skipped_loads = Vec::new();

        for (file, symbols) in &load_statements {
            let mut has_used_symbol = false;
            for (local_name, _) in symbols {
                if used_symbols.contains(local_name) {
                    has_used_symbol = true;
                    break;
                }
            }

            if has_used_symbol {
                needed_loads.push((file.clone(), symbols.clone()));
                debug_log!("[LAZY BUILD]   ✓ Loading: {}", file);
            } else {
                skipped_loads.push(file.clone());
                debug_log!("[LAZY BUILD]   ✗ Skipping (symbols not used): {}", file);
            }
        }

        if load_statements.is_empty() {
            debug_log!(
                "[LAZY BUILD] Filtered: {} needed, {} skipped (no load() statements)",
                needed_loads.len(),
                skipped_loads.len()
            );
        } else {
            debug_log!(
                "[LAZY BUILD] Filtered: {} needed, {} skipped ({:.1}% reduction)",
                needed_loads.len(),
                skipped_loads.len(),
                (skipped_loads.len() as f64 / load_statements.len() as f64) * 100.0
            );
        }

        record_lazy_loading_telemetry(
            load_statements.len(),
            needed_loads.len(),
            skipped_loads.len(),
        );

        self.prioritize_repo_local_loads(&mut needed_loads);

        if !needed_loads.is_empty() && self.bzl_loader.is_some() {
            let mode = if IN_BUILD_FILE_CONTEXT.get() {
                BatchLoadMode::BuildSafe
            } else {
                BatchLoadMode::Default
            };
            self.batch_load_bzl_files(&needed_loads, mode)?;
        }

        Ok(LoadPhaseResult {
            skipped_loads: skipped_loads.into_iter().collect(),
        })
    }

    fn execute_after_load(
        &mut self,
        program: &Program,
        skipped_loads: &HashSet<String>,
    ) -> Result<()> {
        for stmt in program.statements.iter() {
            if let Stmt::Load { file, .. } = stmt {
                if skipped_loads.contains(file) {
                    debug_log!("[LAZY BUILD]   Skipping eval of unused load(): {}", file);
                    continue;
                }
            }

            self.eval_statement(stmt)?;
            if self.return_value.is_some() {
                break;
            }
        }

        Ok(())
    }

    /// Recursively collect all symbol names referenced in a statement or expression.
    /// This helps identify which load()ed symbols are actually used.
    fn collect_used_symbols(&self, stmt: &Stmt, used: &mut HashSet<String>) {
        match stmt {
            Stmt::Expr(expr) => self.collect_used_symbols_expr(expr, used),
            Stmt::Assign { target: _, value } => self.collect_used_symbols_expr(value, used),
            Stmt::FunctionDef { body, .. } => {
                for s in body {
                    self.collect_used_symbols(s, used);
                }
            }
            Stmt::Return(Some(expr)) => self.collect_used_symbols_expr(expr, used),
            Stmt::If {
                condition,
                then_body,
                elif_parts,
                else_body,
            } => {
                self.collect_used_symbols_expr(condition, used);
                for s in then_body {
                    self.collect_used_symbols(s, used);
                }
                for (cond, branch) in elif_parts {
                    self.collect_used_symbols_expr(cond, used);
                    for s in branch {
                        self.collect_used_symbols(s, used);
                    }
                }
                if let Some(branch) = else_body {
                    for s in branch {
                        self.collect_used_symbols(s, used);
                    }
                }
            }
            Stmt::For {
                vars: _,
                iterable,
                body,
            } => {
                self.collect_used_symbols_expr(iterable, used);
                for s in body {
                    self.collect_used_symbols(s, used);
                }
            }
            _ => {}
        }
    }

    /// Recursively collect symbol names from expressions
    fn collect_used_symbols_expr(&self, expr: &Expr, used: &mut HashSet<String>) {
        match expr {
            Expr::Ident(name) => {
                used.insert(name.clone());
            }
            Expr::Call(call) => {
                self.collect_used_symbols_expr(&call.function, used);
                for arg in &call.args {
                    self.collect_used_symbols_expr(arg, used);
                }
                for arg in &call.args_unpack {
                    self.collect_used_symbols_expr(arg, used);
                }
                for (_, value) in &call.kwargs {
                    self.collect_used_symbols_expr(value, used);
                }
                for value in &call.kwargs_unpack {
                    self.collect_used_symbols_expr(value, used);
                }
            }
            Expr::List(items) | Expr::Tuple(items) => {
                for item in items {
                    self.collect_used_symbols_expr(item, used);
                }
            }
            Expr::Dict(entries) => {
                for (key, value) in entries {
                    if let Some(k) = key {
                        self.collect_used_symbols_expr(k, used);
                    }
                    self.collect_used_symbols_expr(value, used);
                }
            }
            Expr::Attribute { object, attr: _ } => {
                self.collect_used_symbols_expr(object, used);
            }
            Expr::Index { object, index } => {
                self.collect_used_symbols_expr(object, used);
                self.collect_used_symbols_expr(index, used);
            }
            Expr::Slice {
                object,
                start,
                end,
                step,
            } => {
                self.collect_used_symbols_expr(object, used);
                if let Some(s) = start {
                    self.collect_used_symbols_expr(s, used);
                }
                if let Some(e) = end {
                    self.collect_used_symbols_expr(e, used);
                }
                if let Some(st) = step {
                    self.collect_used_symbols_expr(st, used);
                }
            }
            Expr::UnaryOp { operand, .. } => {
                self.collect_used_symbols_expr(operand, used);
            }
            Expr::BinOp { left, right, .. } => {
                self.collect_used_symbols_expr(left, used);
                self.collect_used_symbols_expr(right, used);
            }
            Expr::IfExpr {
                condition,
                then_expr,
                else_expr,
            } => {
                self.collect_used_symbols_expr(condition, used);
                self.collect_used_symbols_expr(then_expr, used);
                self.collect_used_symbols_expr(else_expr, used);
            }
            Expr::ListComp {
                element,
                for_clauses,
            } => {
                self.collect_used_symbols_expr(element, used);
                // ForClause is (vars, iterable, condition)
                for (_vars, iterable, condition) in for_clauses.iter() {
                    self.collect_used_symbols_expr(iterable, used);
                    if let Some(cond) = condition {
                        self.collect_used_symbols_expr(cond, used);
                    }
                }
            }
            Expr::DictComp {
                key,
                value,
                for_clauses,
            } => {
                self.collect_used_symbols_expr(key, used);
                self.collect_used_symbols_expr(value, used);
                // ForClause is (vars, iterable, condition)
                for (_vars, iterable, condition) in for_clauses.iter() {
                    self.collect_used_symbols_expr(iterable, used);
                    if let Some(cond) = condition {
                        self.collect_used_symbols_expr(cond, used);
                    }
                }
            }
            // Lambdas - collect symbols from body and default values
            Expr::Lambda { params, body, .. } => {
                // Collect from default values
                for (_name, default) in params {
                    if let Some(default_expr) = default {
                        self.collect_used_symbols_expr(default_expr, used);
                    }
                }
                // Collect from body
                self.collect_used_symbols_expr(body, used);
            }
            // Literals don't reference symbols
            Expr::Int(_) | Expr::Float(_) | Expr::String(_) | Expr::Bool(_) | Expr::None => {}
        }
    }

    /// Batch-load multiple .bzl files in parallel using tokio
    ///
    /// This dramatically improves performance for files with many load() statements
    /// at the top, like TensorFlow's tensorflow.bzl (100+ loads).
    ///
    /// Detects circular dependencies and loads cyclic files sequentially to prevent deadlock.
    fn batch_load_bzl_files(
        &mut self,
        loads: &[(String, Vec<(String, String)>)],
        mode: BatchLoadMode,
    ) -> Result<()> {
        if loads.is_empty() {
            return Ok(());
        }

        // FIX: Prevent recursive batch loading (infinite loop protection)
        // When evaluating .bzl files, they may trigger eval_program() which calls
        // batch_load_bzl_files() again. This causes infinite loops with circular dependencies.
        // Instead, nested loads will fall back to sequential loading via eval_statement(Stmt::Load).
        if IN_BATCH_LOAD.get() {
            debug_log!("[DEBUG Evaluator] Skipping recursive batch_load for {} files (will load sequentially)", loads.len());
            return Ok(());
        }

        // Set flag to prevent recursion
        IN_BATCH_LOAD.set(true);

        // RAII guard to ensure flag is cleared on all exit paths
        struct BatchLoadGuard;
        impl Drop for BatchLoadGuard {
            fn drop(&mut self) {
                IN_BATCH_LOAD.set(false);
            }
        }
        let _guard = BatchLoadGuard;

        let loader = match &self.bzl_loader {
            Some(l) => l.clone(),
            None => return Ok(()),
        };
        let requested_concurrent = bzl_requested_concurrency()?;
        if bzl_loading_environment("RUSH_PROFILE_BZL_LIMIT")?.is_some() {
            anyhow::bail!("RUSH_PROFILE_BZL_LIMIT produces incomplete analysis; remove it to load the complete required module graph");
        }
        debug_log!("[DEBUG Evaluator] Batch-loading {} .bzl files", loads.len());
        let total_loads = loads.len();

        if matches!(mode, BatchLoadMode::BuildSafe) {
            let build_safe_limit = std::env::var("RUSH_BUILD_SAFE_DEP_GRAPH_LIMIT")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(DEFAULT_BUILD_SAFE_DEP_GRAPH_LIMIT);
            if loads.len() > build_safe_limit {
                debug_log!(
                    "[DEBUG Evaluator] BUILD-safe batch has {} files (> {} limit) - falling back to sequential loads",
                    loads.len(),
                    build_safe_limit
                );
                self.load_bzl_files_sequential(loader.clone(), loads)?;
                record_batch_loading_telemetry(
                    mode,
                    total_loads,
                    total_loads,
                    0,
                    0,
                    0,
                    Some("build_safe_limit"),
                );
                return Ok(());
            }
        }

        let skip_dep_graph_env = std::env::var("RUSH_SKIP_BZL_DEP_GRAPH")
            .ok()
            .and_then(|v| v.parse::<bool>().ok());

        let dep_graph = match mode {
            BatchLoadMode::BuildSafe => {
                if skip_dep_graph_env == Some(true) {
                    debug_log!(
                        "[DEBUG Evaluator] RUSH_SKIP_BZL_DEP_GRAPH=1 - falling back to sequential BUILD-safe loading"
                    );
                    self.load_bzl_files_sequential(loader.clone(), loads)?;
                    record_batch_loading_telemetry(
                        mode,
                        total_loads,
                        total_loads,
                        0,
                        0,
                        0,
                        Some("skip_dep_graph"),
                    );
                    return Ok(());
                }
                debug_log!(
                    "[DEBUG Evaluator] BUILD-safe mode: constructing dependency graph for {} files",
                    loads.len()
                );
                let graph = self
                    .build_load_dependency_graph(loads, &loader)
                    .map_err(|e| {
                        debug_log!("[ERROR Evaluator] Failed to build dependency graph: {}", e);
                        e
                    })?;
                debug_log!(
                    "[DEBUG Evaluator] Dependency graph built successfully with {} nodes",
                    graph.len()
                );
                graph
            }
            BatchLoadMode::Default => {
                let skip_dep_graph = skip_dep_graph_env.unwrap_or(true);
                if skip_dep_graph {
                    debug_log!("[DEBUG Evaluator] Skipping dependency graph (RUSH_SKIP_BZL_DEP_GRAPH=true) - loading all files in parallel with concurrency limit");
                    HashMap::new()
                } else {
                    debug_log!(
                        "[DEBUG Evaluator] Building dependency graph for {} files...",
                        loads.len()
                    );
                    let graph = self
                        .build_load_dependency_graph(loads, &loader)
                        .map_err(|e| {
                            debug_log!("[ERROR Evaluator] Failed to build dependency graph: {}", e);
                            e
                        })?;
                    debug_log!(
                        "[DEBUG Evaluator] Dependency graph built successfully with {} nodes",
                        graph.len()
                    );
                    graph
                }
            }
        };

        debug_log!("[DEBUG Evaluator] Partitioning loads by cycles...");
        let ((sequential_files, parallel_batches), max_cycle_size) =
            self.partition_loads_by_cycles(&dep_graph, loads);
        debug_log!(
            "[DEBUG Evaluator] Partitioning complete: {} sequential, {} parallel",
            sequential_files.len(),
            parallel_batches.len()
        );

        let sequential_count = sequential_files.len();
        let parallelizable = parallel_batches.len();

        if !sequential_files.is_empty() {
            debug_log!(
                "[DEBUG Evaluator] Found {} files with circular dependencies, loading sequentially",
                sequential_files.len()
            );
            self.load_bzl_files_sequential(loader.clone(), &sequential_files)?;
        }

        if parallel_batches.is_empty() {
            debug_log!("[DEBUG Evaluator] Batch-loading complete (all files had cycles)");
            record_batch_loading_telemetry(
                mode,
                total_loads,
                sequential_count,
                0,
                0,
                max_cycle_size,
                Some("all_cycles"),
            );
            return Ok(());
        }

        let levels = self.schedule_by_levels(&dep_graph, &parallel_batches);

        debug_log!(
            "[DEBUG Evaluator] Scheduling {} independent files by dependency levels",
            parallel_batches.len()
        );

        record_batch_loading_telemetry(
            mode,
            total_loads,
            sequential_count,
            parallelizable,
            levels.len(),
            max_cycle_size,
            None,
        );

        debug_log!(
            "[DEBUG Evaluator] Created {} dependency levels for parallel loading",
            levels.len()
        );

        // Current-thread runtimes cannot enter block_in_place. They and plain
        // synchronous callers use the same admitted sequential loading path.
        let handle = tokio::runtime::Handle::try_current().ok().filter(|handle| {
            matches!(
                handle.runtime_flavor(),
                tokio::runtime::RuntimeFlavor::MultiThread
            )
        });
        if let Some(handle) = handle {
            tokio::task::block_in_place(|| {
                handle.block_on(async {
                    let manager = loader
                        .lock()
                        .map_err(|error| anyhow!("lock .bzl resources: {error}"))?
                        .loading_resource_manager();
                    let cpu_limit = match manager {
                        Some(manager) => manager.stats().await.max_cpu_slots,
                        None => std::thread::available_parallelism()
                            .context("measure available .bzl loader concurrency")?
                            .get(),
                    };
                    if cpu_limit == 0 {
                        anyhow::bail!(".bzl loader has no admitted CPU capacity");
                    }
                    let max_concurrent = requested_concurrent.min(cpu_limit);
                    for (level_idx, level) in levels.iter().enumerate() {
                        crate::bzl_loader::check_ram_floor_before_level(level_idx, level.len())?;
                        // Consume each result immediately. Per-file atomic memory
                        // admission remains authoritative, including recursive loads;
                        // no CPU permit is retained across an import of a child.
                        let context = crate::bzl_loader::BzlLoader::capture_thread_context();
                        let work = stream::iter(level.iter().map(|(file, symbols, _)| {
                            let loader = loader.clone();
                            let file = file.clone();
                            let symbols = symbols.clone();
                            let context = context.clone();
                            async move {
                                struct ContextGuard;
                                impl Drop for ContextGuard {
                                    fn drop(&mut self) {
                                        crate::bzl_loader::BzlLoader::pop_thread_context();
                                    }
                                }
                                let _context = context.map(|context| {
                                    crate::bzl_loader::BzlLoader::push_thread_context_frame(
                                        context,
                                    );
                                    ContextGuard
                                });
                                let result = crate::bzl_loader::BzlLoader::load_bzl_from_evaluator(
                                    loader, &file, &symbols,
                                );
                                (file, symbols, result)
                            }
                        }))
                        .buffer_unordered(max_concurrent);
                        let mut work = std::pin::pin!(work);
                        while let Some((file, symbols, result)) = work.next().await {
                            let exports = result
                                .with_context(|| format!("load required .bzl module {file}"))?;
                            self.import_bzl_exports(&file, exports, &symbols);
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                })
            })?;
        } else {
            for (level_idx, level) in levels.iter().enumerate() {
                crate::bzl_loader::check_ram_floor_before_level(level_idx, level.len())?;
                for (file, symbols, _) in level {
                    let exports = crate::bzl_loader::BzlLoader::load_bzl_from_evaluator(
                        loader.clone(),
                        file,
                        symbols,
                    )
                    .with_context(|| format!("load required .bzl module {file}"))?;
                    self.import_bzl_exports(file, exports, symbols);
                }
            }
        }

        Ok(())
    }

    fn load_bzl_files_sequential(
        &mut self,
        loader: Arc<Mutex<crate::bzl_loader::BzlLoader>>,
        loads: &[(String, Vec<(String, String)>)],
    ) -> Result<()> {
        // Load each file one at a time - used for cyclic groups or explicit fallbacks.
        for (file, symbols) in loads {
            match crate::bzl_loader::BzlLoader::load_bzl_from_evaluator(
                loader.clone(),
                file,
                symbols,
            ) {
                Ok(exported_symbols) => {
                    self.import_bzl_exports(file, exported_symbols, symbols);
                }
                Err(e) => return Err(e.context(format!("load required .bzl module {file}"))),
            }
        }
        Ok(())
    }

    fn import_bzl_exports(
        &mut self,
        file_label: &str,
        exported_symbols: HashMap<String, Value>,
        requested_symbols: &[(String, String)],
    ) {
        if crate::bzl_loader::take_loop_guarded(file_label) {
            for (local_name, value) in exported_symbols.iter() {
                self.env.define(local_name.clone(), value.clone());
            }
            for (local_name, _) in requested_symbols {
                self.register_rule_function(local_name.clone());
            }
        } else {
            for (local_name, value) in exported_symbols {
                self.env.define(local_name, value);
            }
        }
    }

    /// Build a dependency graph for .bzl files by parsing their load() statements
    ///
    /// Returns HashMap<file_label, Vec<dependencies>>
    fn build_load_dependency_graph(
        &self,
        loads: &[(String, Vec<(String, String)>)],
        loader: &Arc<Mutex<crate::bzl_loader::BzlLoader>>,
    ) -> Result<HashMap<String, Vec<String>>> {
        let mut graph = HashMap::new();

        for (file_label, _) in loads {
            // Parse the .bzl file to extract its load statements
            debug_log!(
                "[DEBUG Evaluator] Extracting dependencies for: {}",
                file_label
            );
            let dependencies = match self.extract_load_dependencies_fast(file_label, loader) {
                Ok(deps) => deps,
                Err(scan_err) => {
                    debug_log!(
                        "[DEBUG Evaluator] Fast load scan failed for {} ({}), falling back to parser",
                        file_label,
                        scan_err
                    );
                    self.extract_load_dependencies(file_label, loader)
                        .map_err(|e| {
                            debug_log!(
                                "[ERROR Evaluator] Failed to extract dependencies for {}: {}",
                                file_label,
                                e
                            );
                            anyhow!("Failed to extract dependencies for {}: {}", file_label, e)
                        })?
                }
            };
            debug_log!(
                "[DEBUG Evaluator] Found {} dependencies for {}",
                dependencies.len(),
                file_label
            );
            graph.insert(file_label.clone(), dependencies);
        }

        Ok(graph)
    }

    /// Extract load() dependencies from a .bzl file using a lightweight lexer scan.
    fn extract_load_dependencies_fast(
        &self,
        bzl_label: &str,
        loader: &Arc<Mutex<crate::bzl_loader::BzlLoader>>,
    ) -> Result<Vec<String>> {
        let (bzl_path, fingerprint, cached_deps) = {
            let loader_guard = loader
                .lock()
                .map_err(|e| anyhow!("Failed to lock loader for {}: {}", bzl_label, e))?;
            let path = loader_guard.resolve_bzl_path(bzl_label).map_err(|e| {
                debug_log!(
                    "[ERROR Evaluator] Failed to resolve path for {}: {}",
                    bzl_label,
                    e
                );
                anyhow!("Failed to resolve path for {}: {}", bzl_label, e)
            })?;
            let fingerprint = loader_guard.file_fingerprint_for(&path)?;
            let cached = loader_guard.get_cached_load_dependencies(&path, &fingerprint);
            (path, fingerprint, cached)
        };

        if let Some(deps) = cached_deps {
            debug_log!(
                "[DEBUG Evaluator] Reusing cached load() dependencies for {}",
                bzl_label
            );
            return Ok(deps);
        }

        let source = std::fs::read_to_string(&bzl_path).map_err(|e| {
            debug_log!(
                "[ERROR Evaluator] Failed to read .bzl file {}: {}",
                bzl_path.display(),
                e
            );
            anyhow!("Failed to read .bzl file {}: {}", bzl_path.display(), e)
        })?;

        let deps = Self::scan_load_statement_paths(&source);

        {
            let loader_guard = loader
                .lock()
                .map_err(|e| anyhow!("Failed to lock loader for {}: {}", bzl_label, e))?;
            loader_guard.cache_load_dependencies(bzl_path.clone(), fingerprint, &deps);
        }

        Ok(deps)
    }

    fn scan_load_statement_paths(source: &str) -> Vec<String> {
        let mut lexer = Lexer::new(source);
        let mut dependencies = Vec::new();

        loop {
            let token = lexer.next_token();
            match token.token_type {
                TokenType::Load => {
                    if let Some(path) = Self::consume_next_string_literal(&mut lexer) {
                        dependencies.push(path);
                    }
                }
                TokenType::Eof => break,
                _ => {}
            }
        }

        dependencies
    }

    fn consume_next_string_literal(lexer: &mut Lexer) -> Option<String> {
        loop {
            let token = lexer.next_token();
            match token.token_type {
                TokenType::String(value) => return Some(value),
                TokenType::RightParen | TokenType::Eof => return None,
                _ => continue,
            }
        }
    }

    /// Extract load() dependencies from a .bzl file without evaluating it
    fn extract_load_dependencies(
        &self,
        bzl_label: &str,
        loader: &Arc<Mutex<crate::bzl_loader::BzlLoader>>,
    ) -> Result<Vec<String>> {
        // Resolve and parse the .bzl file (using cache if available)
        debug_log!("[DEBUG Evaluator] Resolving path for: {}", bzl_label);
        let (_bzl_path, program) = {
            let mut loader_guard = loader
                .lock()
                .map_err(|e| anyhow!("Failed to lock loader for {}: {}", bzl_label, e))?;

            let path = loader_guard.resolve_bzl_path(bzl_label).map_err(|e| {
                debug_log!(
                    "[ERROR Evaluator] Failed to resolve path for {}: {}",
                    bzl_label,
                    e
                );
                anyhow!("Failed to resolve path for {}: {}", bzl_label, e)
            })?;
            debug_log!(
                "[DEBUG Evaluator] Resolved {} to {}",
                bzl_label,
                path.display()
            );

            debug_log!("[DEBUG Evaluator] Parsing .bzl file: {}", path.display());
            let prog = loader_guard.parse_bzl_file(&path).map_err(|e| {
                debug_log!(
                    "[ERROR Evaluator] Failed to parse {}: {}",
                    path.display(),
                    e
                );
                anyhow!("Failed to parse {}: {}", path.display(), e)
            })?;
            debug_log!("[DEBUG Evaluator] Successfully parsed {}", path.display());
            (path, prog)
        };

        // Extract load statements from AST
        let mut dependencies = Vec::new();
        for stmt in &program.statements {
            if let Stmt::Load { file, .. } = stmt {
                dependencies.push(file.clone());
            }
        }

        debug_log!(
            "[DEBUG Evaluator] Extracted {} load statements from {}",
            dependencies.len(),
            bzl_label
        );
        Ok(dependencies)
    }

    /// Calculate priority (number of transitive dependents) for critical path scheduling
    fn calculate_priority(
        &self,
        file: &str,
        dep_graph: &HashMap<String, Vec<String>>,
        cache: &mut HashMap<String, usize>,
    ) -> usize {
        calculate_priority_impl(file, dep_graph, cache)
    }

    /// Schedule loads by dependency levels using topological sort
    ///
    /// Returns Vec<Vec<(file, symbols, priority)>> where each inner Vec is a level
    /// that can be loaded in parallel. Files are sorted by priority within each level.
    fn schedule_by_levels(
        &self,
        dep_graph: &HashMap<String, Vec<String>>,
        loads: &[LoadWithSymbols],
    ) -> Vec<Vec<LoadWithPriority>> {
        let mut levels = Vec::new();

        // Build symbol lookup map
        let symbols_map: HashMap<String, Vec<(String, String)>> =
            loads.iter().map(|(f, s)| (f.clone(), s.clone())).collect();

        // Calculate in-degree for each file
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        for (file, _) in loads {
            in_degree.insert(file.clone(), 0);
        }
        for deps in dep_graph.values() {
            for dep in deps {
                if in_degree.contains_key(dep) {
                    *in_degree.get_mut(dep).unwrap() += 1;
                }
            }
        }

        debug_log!("[DEBUG Evaluator] Calculated in-degrees: {:?}", in_degree);

        // Calculate priorities for all files
        let mut priority_cache = HashMap::new();
        let priorities: HashMap<String, usize> = loads
            .iter()
            .map(|(file, _)| {
                let p = self.calculate_priority(file, dep_graph, &mut priority_cache);
                (file.clone(), p)
            })
            .collect();

        debug_log!("[DEBUG Evaluator] Calculated priorities: {:?}", priorities);

        // Topological sort with levels
        let mut remaining = in_degree.clone();

        while !remaining.is_empty() {
            // Find all files with in_degree 0 (no pending dependencies)
            let mut current_level: Vec<_> = remaining
                .iter()
                .filter(|(_, &degree)| degree == 0)
                .map(|(file, _)| {
                    let symbols = symbols_map.get(file).cloned().unwrap_or_default();
                    let priority = *priorities.get(file).unwrap_or(&0);
                    (file.clone(), symbols, priority)
                })
                .collect();

            if current_level.is_empty() {
                debug_log!(
                    "[WARN Evaluator] Cycle detected in topological sort, remaining files: {:?}",
                    remaining.keys().collect::<Vec<_>>()
                );
                break;
            }

            // Sort by priority (descending) - high priority files first
            current_level.sort_by(|a, b| b.2.cmp(&a.2));

            debug_log!(
                "[DEBUG Evaluator] Level {} has {} files (priorities: {})",
                levels.len(),
                current_level.len(),
                current_level
                    .iter()
                    .map(|(f, _, p)| format!("{}={}", f, p))
                    .collect::<Vec<_>>()
                    .join(", ")
            );

            levels.push(current_level.clone());

            // Remove processed files and update in-degrees
            for (file, _, _) in &current_level {
                remaining.remove(file);

                // Reduce in-degree for files depending on this file
                if let Some(deps) = dep_graph.get(file) {
                    for dep in deps {
                        if let Some(degree) = remaining.get_mut(dep) {
                            *degree = degree.saturating_sub(1);
                        }
                    }
                }
            }
        }

        levels
    }

    /// Partition loads into sequential (cyclic) and parallel (acyclic) batches
    ///
    /// Uses Tarjan's algorithm to find strongly connected components (SCCs).
    /// Files in SCCs with size > 1 have circular dependencies and must be loaded sequentially.
    fn partition_loads_by_cycles(
        &self,
        dep_graph: &HashMap<String, Vec<String>>,
        loads: &[LoadWithSymbols],
    ) -> (LoadPartition, usize) {
        // Find strongly connected components using Tarjan's algorithm
        let sccs = self.find_strongly_connected_components(dep_graph);

        // Identify files that are in cycles (SCCs with size > 1)
        let mut cyclic_files: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut max_cycle_size = 0;
        for scc in &sccs {
            if scc.len() > max_cycle_size {
                max_cycle_size = scc.len();
            }
            if scc.len() > 1 {
                // This SCC has a cycle
                debug_log!(
                    "[DEBUG Evaluator] Found SCC with {} files (circular dependency)",
                    scc.len()
                );
                for file in scc {
                    cyclic_files.insert(file.clone());
                }
            }
        }

        // Partition loads
        let mut sequential = Vec::new();
        let mut parallel = Vec::new();

        for (file, symbols) in loads {
            if cyclic_files.contains(file) {
                sequential.push((file.clone(), symbols.clone()));
            } else {
                parallel.push((file.clone(), symbols.clone()));
            }
        }

        (
            (sequential, parallel),
            if max_cycle_size > 1 {
                max_cycle_size
            } else {
                0
            },
        )
    }

    /// Find strongly connected components using Tarjan's algorithm
    fn find_strongly_connected_components(
        &self,
        graph: &HashMap<String, Vec<String>>,
    ) -> Vec<Vec<String>> {
        // Convert labels to compact indices once. The previous implementation
        // cloned and SipHashed String keys through three maps on every DFS edge.
        // Dependencies absent from `graph` have no known outgoing edges and
        // therefore cannot participate in a cycle among this load batch.
        let nodes: Vec<&str> = graph.keys().map(String::as_str).collect();
        let mut index_by_node: HashMap<&str, usize> = HashMap::with_capacity(nodes.len());
        for (index, node) in nodes.iter().copied().enumerate() {
            index_by_node.insert(node, index);
        }
        let edges: Vec<Vec<usize>> = nodes
            .iter()
            .map(|node| {
                graph
                    .get(*node)
                    .into_iter()
                    .flatten()
                    .filter_map(|successor| index_by_node.get(successor.as_str()).copied())
                    .collect()
            })
            .collect();

        struct TarjanState<'a> {
            nodes: &'a [&'a str],
            edges: &'a [Vec<usize>],
            index_counter: usize,
            stack: Vec<usize>,
            indices: Vec<Option<usize>>,
            lowlinks: Vec<usize>,
            on_stack: Vec<bool>,
            sccs: Vec<Vec<String>>,
        }

        impl<'a> TarjanState<'a> {
            fn strongconnect(&mut self, node: usize) {
                // Set the depth index for this node
                self.indices[node] = Some(self.index_counter);
                self.lowlinks[node] = self.index_counter;
                self.index_counter += 1;
                self.stack.push(node);
                self.on_stack[node] = true;

                // Consider successors of node
                for successor_index in 0..self.edges[node].len() {
                    let successor = self.edges[node][successor_index];
                    if self.indices[successor].is_none() {
                        // Successor has not yet been visited; recurse on it.
                        self.strongconnect(successor);
                        self.lowlinks[node] =
                            self.lowlinks[node].min(self.lowlinks[successor]);
                    } else if self.on_stack[successor] {
                        // Successor is in stack and hence in the current SCC.
                        self.lowlinks[node] = self.lowlinks[node]
                            .min(self.indices[successor].expect("visited successor"));
                    }
                }

                // If node is a root node, pop the stack and generate an SCC
                if self.lowlinks[node] == self.indices[node].expect("visited node") {
                    let mut scc = Vec::new();
                    while let Some(w) = self.stack.pop() {
                        self.on_stack[w] = false;
                        scc.push(self.nodes[w].to_string());
                        if w == node {
                            break;
                        }
                    }
                    self.sccs.push(scc);
                }
            }
        }

        let mut state = TarjanState {
            nodes: &nodes,
            edges: &edges,
            index_counter: 0,
            stack: Vec::with_capacity(nodes.len()),
            indices: vec![None; nodes.len()],
            lowlinks: vec![0; nodes.len()],
            on_stack: vec![false; nodes.len()],
            sccs: Vec::with_capacity(nodes.len()),
        };

        // Run Tarjan's algorithm on all nodes
        for node in 0..nodes.len() {
            if state.indices[node].is_none() {
                state.strongconnect(node);
            }
        }

        state.sccs
    }

    fn bind_loop_pattern(&mut self, pattern: &BindingPattern, value: Value) -> Result<()> {
        // Preserve the identity of custom rules that Rush implements concretely.
        // `rule()` itself returns the generic custom-rule marker, but once the
        // upstream .bzl binds that value to `mojo_library`, aliases and wrapper
        // functions must carry the concrete kind through to the BUILD call.
        let value = match (pattern, value) {
            (
                BindingPattern::Identifier(name),
                Value::RuleFunction(rule_type),
            ) if matches!(rule_type.as_str(), "CustomRule" | "custom_rule_stub")
                && matches!(
                    name.as_str(),
                    "mojo_library"
                        | "mojo_binary"
                        | "mojo_test"
                        | "mojo_import"
                        | "modular_versioned_expand_template"
                        | "uv_lock"
                        | "py_binary"
                        | "py_test"
                        | "py_library"
                        | "pycross_wheel_library"
                ) =>
            {
                Value::RuleFunction(format!("custom_rule_stub:{}", name))
            }
            (_, value) => value,
        };

        match pattern {
            BindingPattern::Identifier(name) => {
                self.env.define(name.clone(), value);
                Ok(())
            }
            BindingPattern::Tuple(patterns) => {
                let (Value::List(values) | Value::Tuple(values) | Value::FrozenList(values)) = value else {
                    return Err(anyhow!(
                        "Cannot unpack {:?} into {} bindings",
                        value,
                        patterns.len()
                    ));
                };
                if values.len() != patterns.len() {
                    return Err(anyhow!(
                        "Cannot unpack {} values into {} bindings",
                        values.len(),
                        patterns.len()
                    ));
                }
                for (nested_pattern, nested_value) in
                    patterns.iter().zip(values.into_iter())
                {
                    self.bind_loop_pattern(nested_pattern, nested_value)?;
                }
                Ok(())
            }
        }
    }

    fn collect_lvalue_steps(
        &mut self,
        expr: &Expr,
        steps: &mut Vec<LvalueStep>,
    ) -> Result<String> {
        match expr {
            Expr::Ident(name) => Ok(name.clone()),
            Expr::Attribute { object, attr } => {
                let root = self.collect_lvalue_steps(object, steps)?;
                steps.push(LvalueStep::Attribute(attr.clone()));
                Ok(root)
            }
            Expr::Index { object, index } => {
                let root = self.collect_lvalue_steps(object, steps)?;
                let index = self.eval_expression(index)?;
                steps.push(LvalueStep::Index(index));
                Ok(root)
            }
            _ => Err(anyhow!(
                "Indexed assignment requires a variable-rooted attribute or index expression"
            )),
        }
    }

    fn list_index(index: &Value, len: usize) -> Result<usize> {
        let Value::Int(index) = index else {
            return Err(anyhow!("List indices must be integers, got {:?}", index));
        };
        let normalized = if *index < 0 {
            len as i64 + *index
        } else {
            *index
        };
        if normalized < 0 || normalized as usize >= len {
            return Err(anyhow!(
                "List index out of range: {} (length {})",
                index,
                len
            ));
        }
        Ok(normalized as usize)
    }

    fn descend_lvalue<'a>(
        mut value: &'a mut Value,
        steps: &[LvalueStep],
    ) -> Result<&'a mut Value> {
        for step in steps {
            value = match step {
                LvalueStep::Attribute(attr) => match value {
                    Value::Namespace(fields) | Value::ProviderInstance { fields, .. } => fields
                        .get_mut(attr)
                        .ok_or_else(|| anyhow!("Attribute '{}' not found in assignment", attr))?,
                    _ => {
                        return Err(anyhow!(
                            "Attribute '{}' is not mutable on this value",
                            attr
                        ))
                    }
                },
                LvalueStep::Index(index) => match value {
                    Value::Dict(map) => {
                        let key = dictionary_key(index)?;
                        map.get_mut(&key)
                            .ok_or_else(|| anyhow!("Key {} not found in dictionary", key))?
                    }
                    Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => {
                        let position = Self::list_index(index, items.len())?;
                        &mut items[position]
                    }
                    _ => return Err(anyhow!("Cannot index through non-container value")),
                },
            };
        }
        Ok(value)
    }

    fn assign_nested_index(
        &mut self,
        object: &Expr,
        index: &Expr,
        value: &Expr,
        op: Option<BinOp>,
    ) -> Result<()> {
        let mut steps = Vec::new();
        let root_name = self.collect_lvalue_steps(object, &mut steps)?;
        let index = self.eval_expression(index)?;
        let rhs = self.eval_expression(value)?;
        let mut root_value = self
            .env
            .get(&root_name)
            .ok_or_else(|| anyhow!("Undefined variable: {}", root_name))?
            .clone();
        let container = Self::descend_lvalue(&mut root_value, &steps)?;

        match container {
            Value::ImmutableMapping(_) | Value::FrozenList(_) => anyhow::bail!("cannot mutate immutable native rule metadata"),
            Value::Dict(map) => {
                let key = dictionary_key(&index)?;
                let assigned = if let Some(op) = op {
                    let current = map
                        .get(&key)
                        .ok_or_else(|| anyhow!("Key {} not found in dictionary", key))?
                        .clone();
                    self.apply_binop(&current, op, &rhs)?
                } else {
                    rhs
                };
                map.insert(key, assigned);
            }
            Value::List(items) => {
                let position = Self::list_index(&index, items.len())?;
                let assigned = if let Some(op) = op {
                    self.apply_binop(&items[position], op, &rhs)?
                } else {
                    rhs
                };
                items[position] = assigned;
            }
            _ => {
                return Err(anyhow!(
                    "Cannot perform indexed assignment on non-container value"
                ))
            }
        }

        self.env.set(&root_name, root_value)
    }

    fn eval_statement(&mut self, stmt: &Stmt) -> Result<()> {
        if self.return_value.is_some() || self.break_flag || self.continue_flag {
            return Ok(());
        }

        match stmt {
            Stmt::Expr(expr) => {
                // Special handling for mutation methods (list.append, list.extend, dict.pop)
                // Check if this is a method call on a variable
                if let Expr::Call(call) = &**expr {
                    if let Expr::Attribute { object, attr } = call.function.as_ref() {
                        if let Expr::Ident(var_name) = object.as_ref() {
                            // Check if this is a list method that mutates
                            if attr == "append" || attr == "extend" || attr == "remove" {
                                // Get the current list value
                                if let Some(Value::List(ref mut list)) = self.env.get(var_name) {
                                    let mut new_list = list.clone();

                                    if attr == "append" {
                                        // Append single element
                                        if call.args.len() != 1 {
                                            return Err(anyhow!(
                                                "append() takes exactly one argument"
                                            ));
                                        }
                                        let elem = self.eval_expression(&call.args[0])?;
                                        new_list.push(elem);
                                    } else if attr == "extend" {
                                        // Extend with multiple elements
                                        if call.args.len() != 1 {
                                            return Err(anyhow!(
                                                "extend() takes exactly one argument"
                                            ));
                                        }
                                        let iterable = self.eval_expression(&call.args[0])?;
                                        match iterable {
                                            Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => {
                                                new_list.extend(items);
                                            }
                                            _ => {
                                                return Err(anyhow!(
                                                    "extend() argument must be a list"
                                                ))
                                            }
                                        }
                                    } else if attr == "remove" {
                                        // Remove first occurrence of element
                                        if call.args.len() != 1 {
                                            return Err(anyhow!(
                                                "remove() takes exactly one argument"
                                            ));
                                        }
                                        let elem = self.eval_expression(&call.args[0])?;

                                        // Find and remove first occurrence
                                        if let Some(pos) = new_list.iter().position(|x| x == &elem)
                                        {
                                            new_list.remove(pos);
                                        } else {
                                            return Err(anyhow!("list.remove(x): x not in list"));
                                        }
                                    }

                                    // Update the list in the environment
                                    self.env.set(var_name, Value::List(new_list))?;
                                    return Ok(());
                                }
                            }
                            // Check if this is a dict.pop() method that mutates
                            else if attr == "pop" {
                                // Get the current dict value
                                if let Some(Value::Dict(ref mut dict)) = self.env.get(var_name) {
                                    let mut new_dict = dict.clone();

                                    // Pop requires at least 1 argument (the key)
                                    if call.args.is_empty() {
                                        return Err(anyhow!(
                                            "pop() requires at least one argument"
                                        ));
                                    }

                                    // Get the key
                                    let key_val = self.eval_expression(&call.args[0])?;
                                    let key = dictionary_key(&key_val)?;

                                    // Remove from dict (handled in BoundMethod call for return value)
                                    new_dict.remove(&key);

                                    // Update the dict in the environment
                                    self.env.set(var_name, Value::Dict(new_dict))?;
                                    // Note: The actual return value is handled by eval_expression
                                    // when the result is used in an assignment
                                }
                            }
                            // Check if this is a dict.update() method that mutates
                            else if attr == "update" {
                                // Get the current dict value
                                if let Some(Value::Dict(ref mut dict)) = self.env.get(var_name) {
                                    let mut new_dict = dict.clone();

                                    // update accepts at most one positional dictionary plus
                                    // arbitrary keyword entries. Positional unpacking is
                                    // supported here as well so the argument count is checked
                                    // after expansion.
                                    let mut update_args = Vec::new();
                                    for arg in &call.args {
                                        update_args.push(self.eval_expression(arg)?);
                                    }
                                    for unpack in &call.args_unpack {
                                        match self.eval_expression(unpack)? {
                                            Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => update_args.extend(items),
                                            value => {
                                                return Err(anyhow!(
                                                    "Cannot unpack non-list value in *args: {:?}",
                                                    value
                                                ))
                                            }
                                        }
                                    }
                                    if update_args.len() > 1 {
                                        return Err(anyhow!(
                                            "update() takes at most one positional argument"
                                        ));
                                    }

                                    if let Some(update_val) = update_args.into_iter().next() {
                                        match dictionary_expansion_value(update_val) {
                                            Value::Dict(update_dict) => {
                                                new_dict.extend(update_dict);
                                            }
                                            _ => {
                                                return Err(anyhow!(
                                                    "update() argument must be a dict"
                                                ))
                                            }
                                        }
                                    }

                                    let mut keyword_names = HashSet::new();
                                    let mut immutable_keyword_names = HashSet::new();
                                    for (key, value_expr) in &call.kwargs {
                                        let value = self.eval_expression(value_expr)?;
                                        keyword_names.insert(key.clone());
                                        new_dict.insert(key.clone(), value);
                                    }
                                    for unpack in &call.kwargs_unpack {
                                        let raw = self.eval_expression(unpack)?;
                                        let immutable_mapping =
                                            matches!(&raw, Value::ImmutableMapping(_));
                                        match dictionary_expansion_value(raw) {
                                            Value::Dict(update_dict) => {
                                                for (key, value) in update_dict {
                                                    if immutable_mapping {
                                                        if !matches!(
                                                            dictionary_key_value(&key),
                                                            Value::String(_)
                                                        ) {
                                                            anyhow::bail!("keyword argument names must be strings");
                                                        }
                                                        if keyword_names.contains(&key) {
                                                            anyhow::bail!("multiple values for keyword argument '{key}'");
                                                        }
                                                        immutable_keyword_names.insert(key.clone());
                                                    } else if immutable_keyword_names.contains(&key) {
                                                        anyhow::bail!("multiple values for keyword argument '{key}'");
                                                    }
                                                    keyword_names.insert(key.clone());
                                                    new_dict.insert(key, value);
                                                }
                                            }
                                            value => {
                                                return Err(anyhow!(
                                                    "Cannot unpack non-dict value in function call: {:?}",
                                                    value
                                                ))
                                            }
                                        }
                                    }

                                    // Update the dict in the environment
                                    self.env.set(var_name, Value::Dict(new_dict))?;
                                    return Ok(());
                                }
                            }
                            // dict.setdefault(key, default) as a BARE STATEMENT (return discarded):
                            // must still mutate the dict in place (insert key->default if absent).
                            // The expression/assignment paths handle the returned value; without this
                            // bare-statement arm, `d.setdefault(k, v)` never mutates d — which breaks
                            // e.g. TF 2.17's xla_test macro seeding backend_tags before indexing them.
                            else if attr == "setdefault" {
                                if let Some(Value::Dict(ref dict)) = self.env.get(var_name) {
                                    let mut new_dict = dict.clone();
                                    if call.args.is_empty() {
                                        return Err(anyhow!(
                                            "setdefault() requires at least one argument"
                                        ));
                                    }
                                    let key_val = self.eval_expression(&call.args[0])?;
                                    let key = dictionary_key(&key_val)?;
                                    if new_dict.get(&key).is_none() {
                                        let default = if call.args.len() > 1 {
                                            self.eval_expression(&call.args[1])?
                                        } else {
                                            Value::None
                                        };
                                        new_dict.insert(key, default);
                                        self.env.set(var_name, Value::Dict(new_dict))?;
                                    }
                                    return Ok(());
                                }
                            }
                        }
                    }
                }

                // Default: just evaluate the expression
                self.eval_expression(expr)?;
                Ok(())
            }
            Stmt::Assign { target, value } => {
                // Special handling for dict mutations in assignments (pop, setdefault)
                // e.g., testonly = kwargs.pop("testonly", False)
                if let Expr::Call(call) = &**value {
                    if let Expr::Attribute { object, attr } = call.function.as_ref() {
                        if attr == "pop" {
                            if let Expr::Ident(var_name) = object.as_ref() {
                                if let Some(Value::Dict(ref dict)) = self.env.get(var_name) {
                                    let mut new_dict = dict.clone();

                                    // Pop requires at least 1 argument (the key)
                                    if call.args.is_empty() {
                                        return Err(anyhow!(
                                            "pop() requires at least one argument"
                                        ));
                                    }

                                    // Evaluate key
                                    let key_val = self.eval_expression(&call.args[0])?;
                                    let key = dictionary_key(&key_val)?;

                                    // Get value (or default)
                                    let result_val = if let Some(val) = new_dict.remove(&key) {
                                        val
                                    } else {
                                        // Use default if provided
                                        if call.args.len() > 1 {
                                            self.eval_expression(&call.args[1])?
                                        } else {
                                            Value::None
                                        }
                                    };

                                    // Update the dict in the environment
                                    self.env.set(var_name, Value::Dict(new_dict))?;

                                    self.bind_loop_pattern(target, result_val)?;
                                    return Ok(());
                                }
                            }
                        } else if attr == "setdefault" {
                            if let Expr::Ident(var_name) = object.as_ref() {
                                if let Some(Value::Dict(ref dict)) = self.env.get(var_name) {
                                    let mut new_dict = dict.clone();

                                    // setdefault requires at least 1 argument (the key)
                                    if call.args.is_empty() {
                                        return Err(anyhow!(
                                            "setdefault() requires at least one argument"
                                        ));
                                    }

                                    // Evaluate key
                                    let key_val = self.eval_expression(&call.args[0])?;
                                    let key = dictionary_key(&key_val)?;

                                    // Get value or set default
                                    let result_val = if let Some(val) = new_dict.get(&key) {
                                        // Key exists, return existing value
                                        val.clone()
                                    } else {
                                        // Key doesn't exist, set default and return it
                                        let default = if call.args.len() > 1 {
                                            self.eval_expression(&call.args[1])?
                                        } else {
                                            Value::None
                                        };
                                        new_dict.insert(key, default.clone());
                                        default
                                    };

                                    // Update the dict in the environment
                                    self.env.set(var_name, Value::Dict(new_dict))?;

                                    self.bind_loop_pattern(target, result_val)?;
                                    return Ok(());
                                }
                            }
                        }
                    }
                }

                // Default assignment path
                let val = self.eval_expression(value)?;
                self.bind_loop_pattern(target, val)
            }
            Stmt::AugAssign { target, op, value } => {
                let current = self
                    .env
                    .get(target)
                    .ok_or_else(|| anyhow!("Undefined variable: {}", target))?
                    .clone();
                let rhs = self.eval_expression(value)?;
                if matches!((&current, op), (Value::FrozenList(_), BinOp::Add)) {
                    anyhow::bail!("cannot mutate frozen list from native rule metadata");
                }
                let result = self.apply_binop(&current, *op, &rhs)?;
                self.env.set(target, result)?;
                Ok(())
            }
            Stmt::FunctionDef {
                name,
                params,
                varargs_param,
                kwargs_param,
                body,
            } => {
                // Starlark evaluates defaults left-to-right at definition,
                // even when this function is never called or a caller supplies
                // every argument. Store Values; never rerun the AST in a
                // caller's phase, package, or environment.
                let params = params.iter().map(|(name, default)| {
                    Ok((name.clone(), default.as_ref().map(|expr| self.eval_expression(expr)).transpose()?))
                }).collect::<Result<Vec<_>>>()?;
                let func = Value::Function {
                    params,
                    varargs_param: varargs_param.clone(),
                    kwargs_param: kwargs_param.clone(),
                    body: body.clone(),
                    closure: self.closure_environment()?,
                };
                self.env.define(name.clone(), func);
                Ok(())
            }
            Stmt::If {
                condition,
                then_body,
                elif_parts,
                else_body,
            } => {
                let cond_val = self.eval_expression(condition)?;
                if cond_val.is_truthy() {
                    for stmt in then_body {
                        self.eval_statement(stmt)?;
                    }
                } else {
                    let mut executed = false;
                    for (elif_cond, elif_body) in elif_parts {
                        let elif_val = self.eval_expression(elif_cond)?;
                        if elif_val.is_truthy() {
                            for stmt in elif_body {
                                self.eval_statement(stmt)?;
                            }
                            executed = true;
                            break;
                        }
                    }
                    if !executed {
                        if let Some(else_stmts) = else_body {
                            for stmt in else_stmts {
                                self.eval_statement(stmt)?;
                            }
                        }
                    }
                }
                Ok(())
            }
            Stmt::For {
                vars,
                iterable,
                body,
            } => {
                let iter_val = self.eval_expression(iterable)?;
                let items = match iter_val {
                    Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => items,
                    Value::String(s) => {
                        // Strings are iterable in Starlark (yield 1-char strings).
                        s.chars().map(|c| Value::String(c.to_string())).collect()
                    }
                    // Iterating a dict yields its keys (Starlark). rush's Dict
                    // is a HashMap, so sort keys for determinism (see
                    // builtin_list for the same rationale).
                    Value::Dict(map) => sorted_dictionary_key_values(&map),
                        Value::ImmutableMapping(view) => sorted_dictionary_key_values(&view.entries),
                    _ => return Err(anyhow!("Cannot iterate over {:?}", iter_val)),
                };

                for item in items {
                    self.bind_loop_pattern(vars, item)?;

                    for stmt in body {
                        self.eval_statement(stmt)?;
                        if self.continue_flag {
                            self.continue_flag = false;
                            break;
                        }
                        if self.break_flag {
                            break;
                        }
                    }
                    if self.break_flag {
                        self.break_flag = false;
                        break;
                    }
                }
                Ok(())
            }
            Stmt::Return(expr) => {
                self.return_value = Some(if let Some(e) = expr {
                    self.eval_expression(e)?
                } else {
                    Value::None
                });
                Ok(())
            }
            Stmt::Break => {
                self.break_flag = true;
                Ok(())
            }
            Stmt::Continue => {
                self.continue_flag = true;
                Ok(())
            }
            Stmt::Pass => Ok(()),
            Stmt::IndexAssign {
                object,
                index,
                value,
            } => {
                if !matches!(&**object, Expr::Ident(_)) {
                    return self.assign_nested_index(object, index, value, None);
                }

                // Evaluate the index and value
                let index_val = self.eval_expression(index)?;
                let new_val = self.eval_expression(value)?;

                // For indexed assignment, the object must be a variable we can modify
                // Get the variable name from the object expression
                let var_name = match &**object {
                    Expr::Ident(name) => name,
                    _ => {
                        return Err(anyhow!(
                        "Indexed assignment only supported for variables, not complex expressions"
                    ))
                    }
                };

                // Get the current value of the variable
                let mut current_val = self
                    .env
                    .get(var_name)
                    .ok_or_else(|| anyhow!("Undefined variable: {}", var_name))?
                    .clone();

                // Modify the value at the given index
                match &mut current_val {
                    Value::Dict(ref mut map) => {
                        let key = dictionary_key(&index_val)?;
                        map.insert(key, new_val);
                    }
                    Value::List(ref mut items) => {
                        // List indexed assignment: list[0] = value
                        let idx = match index_val {
                            Value::Int(i) => i,
                            _ => {
                                return Err(anyhow!(
                                    "List indices must be integers, got {:?}",
                                    index_val
                                ))
                            }
                        };

                        // Handle negative indices
                        let actual_idx = if idx < 0 {
                            let len = items.len() as i64;
                            (len + idx) as usize
                        } else {
                            idx as usize
                        };

                        if actual_idx >= items.len() {
                            return Err(anyhow!(
                                "List index out of range: {} (length {})",
                                idx,
                                items.len()
                            ));
                        }

                        items[actual_idx] = new_val;
                    }
                    _ => {
                        return Err(anyhow!(
                            "Cannot perform indexed assignment on {:?}",
                            current_val
                        ))
                    }
                }

                // Set the modified value back
                self.env.set(var_name, current_val)?;
                Ok(())
            }
            Stmt::AugIndexAssign {
                object,
                index,
                op,
                value,
            } => {
                if !matches!(&**object, Expr::Ident(_)) {
                    return self.assign_nested_index(object, index, value, Some(*op));
                }

                // Evaluate the index and new value
                let index_val = self.eval_expression(index)?;
                let rhs_val = self.eval_expression(value)?;

                // For indexed assignment, the object must be a variable we can modify
                let var_name = match &**object {
                    Expr::Ident(name) => name,
                    _ => return Err(anyhow!(
                        "Augmented indexed assignment only supported for variables, not complex expressions"
                    )),
                };

                // Get the current value of the variable
                let mut current_val = self
                    .env
                    .get(var_name)
                    .ok_or_else(|| anyhow!("Undefined variable: {}", var_name))?
                    .clone();

                // Get the current value at index, apply operation, and set back
                match &mut current_val {
                    Value::Dict(ref mut map) => {
                        let key = dictionary_key(&index_val)?;

                        // Get current value at key
                        let current_at_key = map
                            .get(&key)
                            .ok_or_else(|| anyhow!("Key {} not found in dictionary", key))?
                            .clone();

                        // Apply binary operation
                        let new_val = self.apply_binop(&current_at_key, *op, &rhs_val)?;

                        // Set back
                        map.insert(key, new_val);
                    }
                    Value::List(ref mut items) => {
                        // List augmented indexed assignment: list[0] += value
                        let idx = match index_val {
                            Value::Int(i) => i,
                            _ => {
                                return Err(anyhow!(
                                    "List indices must be integers, got {:?}",
                                    index_val
                                ))
                            }
                        };

                        // Handle negative indices
                        let actual_idx = if idx < 0 {
                            let len = items.len() as i64;
                            (len + idx) as usize
                        } else {
                            idx as usize
                        };

                        if actual_idx >= items.len() {
                            return Err(anyhow!(
                                "List index out of range: {} (length {})",
                                idx,
                                items.len()
                            ));
                        }

                        // Get current value, apply operation, set back
                        let current_at_idx = items[actual_idx].clone();
                        let new_val = self.apply_binop(&current_at_idx, *op, &rhs_val)?;
                        items[actual_idx] = new_val;
                    }
                    _ => {
                        return Err(anyhow!(
                            "Cannot perform augmented indexed assignment on {:?}",
                            current_val
                        ))
                    }
                }

                // Set the modified value back
                self.env.set(var_name, current_val)?;
                Ok(())
            }
            Stmt::Load { file, symbols } => {
                // Rewrite label if we are in an external repository
                let file_to_load = if let Some(repo) = &self.current_repository {
                    if file.starts_with("//") {
                        format!("@@{}//{}", repo, &file[2..])
                    } else {
                        file.clone()
                    }
                } else {
                    file.clone()
                };

                // If BzlLoader is available, use it to load the .bzl file
                if let Some(loader) = &self.bzl_loader {
                    // FAST PATH: Check cache first - if already loaded, return immediately
                    // Use non-accounting lookup so repeat-miss counters reflect loader-level
                    // cache behavior (without double-counting from the evaluator pre-pass).
                    let (_bzl_path, cached_symbols_opt) = {
                        let mut loader_guard = loader
                            .lock()
                            .map_err(|e| anyhow!("Failed to lock .bzl loader: {}", e))?;
                        let path = loader_guard.resolve_bzl_path(&file_to_load)?;
                        let cached = loader_guard.get_symbols_cache_subset_for_label_no_stats(
                            &path, &file_to_load, symbols,
                        )?;
                        (path, cached)
                    }; // Lock released

                    if let Some(cached_symbols) = cached_symbols_opt {
                        // Cache hit - import symbols and return immediately (no circular dependency check needed)
                        for (local_name, value) in cached_symbols {
                            self.env.define(local_name, value);
                        }
                        return Ok(());
                    }

                    // ARCHITECTURAL FIX #19: Circular dependency detection moved to loader
                    // The loader now handles circular dependency detection BEFORE adding files to IN_FLIGHT.
                    // This provides complete visibility and correct timing for detection.
                    // The evaluator simply calls load_bzl_from_evaluator() and receives either:
                    // - Real values (if file loaded successfully)
                    // - LazyLoad placeholders (if circular dependency detected by loader)

                    // Proceed with load - loader will handle circular dependency detection
                    let load_result = crate::bzl_loader::BzlLoader::load_bzl_from_evaluator(
                        loader.clone(),
                        &file_to_load,
                        symbols,
                    );

                    match load_result {
                        Ok(exported_symbols) => {
                            self.import_bzl_exports(file, exported_symbols, symbols);

                            // Remove file from IN_FLIGHT tracking (load completed successfully)
                            crate::bzl_loader::mark_bzl_inflight_end(file);

                            Ok(())
                        }
                        Err(e) => {
                            // Remove file from IN_FLIGHT tracking (load failed)
                            crate::bzl_loader::mark_bzl_inflight_end(file);

                            // Fail loudly on .bzl load failures to avoid silent stub behavior
                            // This surfaces missing external repos or unimplemented rule sets early.
                            anyhow::bail!(
                                "Failed to load .bzl file {}: {:#}. Rush does not support fallback registration; please ensure external repos and rule files exist (e.g., run project configuration or disable unsupported features).",
                                file, e
                            )
                        }
                    }
                } else {
                    // No BzlLoader available - register loaded symbols as rule functions
                    // This is the fallback behavior for standalone evaluations
                    anyhow::bail!(
                        "No .bzl loader available to load {} and fallback registration is disabled",
                        file
                    )
                }
            }
        }
    }

    pub fn eval_expression(&mut self, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Int(n) => Ok(Value::Int(*n)),
            Expr::Float(f) => Ok(Value::Float(*f)),
            Expr::String(s) => Ok(Value::String(s.clone())),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::None => Ok(Value::None),
            Expr::Ident(name) => {
                // Check environment first (user-defined variables and functions)
                if let Some(val) = self.env.get(name) {
                    return Ok(val.clone());
                }

                // Predeclared provider values are not module exports unless
                // the module explicitly assigns an alias to them.
                match name.as_str() {
                    "JavaInfo" => return Ok(crate::builtins::native_java_info_provider()),
                    "JavaPluginInfo" => {
                        return Ok(crate::builtins::native_java_plugin_info_provider())
                    }
                    _ => {}
                }

                // Check if it's a builtin function
                if let Some(builtin) = self.builtins.get(name).copied() {
                    if name == "module_extension" && !self.env.bzl_definition_scope {
                        return Err(anyhow!("Undefined variable: module_extension (available only in .bzl definitions)"));
                    }
                    // For namespace-like builtins (config_common, cc_common, etc.),
                    // call them immediately to get the namespace value.
                    // This allows attribute access like config_common.FeatureFlagInfo.
                    const NAMESPACE_BUILTINS: &[&str] = &[
                        "config_common",
                        "config",
                        "cc_common",
                        "platform_common",
                        "proto_common",
                        "java_common",
                        "apple_common",
                        "coverage_common",
                        "testing",
                        "py_internal",
                        "native",
                    ];
                    if NAMESPACE_BUILTINS.contains(&name.as_str()) {
                        return builtin(&[], &[]);
                    }
                    return Ok(Value::NativeFunction(name.clone()));
                }

                Err(anyhow!("Undefined variable: {}", name))
            }
            Expr::List(elements) => {
                let mut items = Vec::new();
                for elem in elements {
                    items.push(self.eval_expression(elem)?);
                }
                Ok(Value::List(items))
            }
            Expr::Dict(pairs) => {
                let mut map = HashMap::new();
                for (key_expr, val_expr) in pairs {
                    match key_expr {
                        Some(key) => {
                            // Regular key: value pair
                            let key_val = self.eval_expression(key)?;
                            let val = self.eval_expression(val_expr)?;
                            let encoded = dictionary_key(&key_val)?;
                            if map.insert(encoded, val).is_some() {
                                return Err(anyhow!(
                                    "dictionary expression has duplicate key: {}",
                                    starlark_repr(&key_val)
                                ));
                            }
                        }
                        None => {
                            // **dict unpacking
                            let dict_val = dictionary_expansion_value(self.eval_expression(val_expr)?);
                            match dict_val {
                                Value::Dict(other_map) => {
                                    // Merge the dictionary
                                    for (k, v) in other_map {
                                        map.insert(k, v);
                                    }
                                }
                                _ => {
                                    return Err(anyhow!(
                                        "Cannot unpack non-dict value: {:?}",
                                        dict_val
                                    ));
                                }
                            }
                        }
                    }
                }
                Ok(Value::Dict(map))
            }
            Expr::BinOp { left, op, right } => {
                // CRITICAL: Handle And/Or with short-circuit evaluation
                // Must not evaluate right side if left side determines the result
                match op {
                    BinOp::And => {
                        let left_val = self.eval_expression(left)?;
                        if !left_val.is_truthy() {
                            // Short-circuit: left is falsy, return it without evaluating right
                            Ok(left_val)
                        } else {
                            // left is truthy, return right (which determines the result)
                            self.eval_expression(right)
                        }
                    }
                    BinOp::Or => {
                        let left_val = self.eval_expression(left)?;
                        if left_val.is_truthy() {
                            // Short-circuit: left is truthy, return it without evaluating right
                            Ok(left_val)
                        } else {
                            // left is falsy, return right (which determines the result)
                            self.eval_expression(right)
                        }
                    }
                    _ => {
                        // All other operators: evaluate both sides then apply
                        let left_val = self.eval_expression(left)?;
                        let right_val = self.eval_expression(right)?;
                        self.apply_binop(&left_val, *op, &right_val)
                    }
                }
            }
            Expr::UnaryOp { op, operand } => {
                let val = self.eval_expression(operand)?;
                match op {
                    UnaryOp::Neg => match val {
                        Value::Int(n) => Ok(Value::Int(-n)),
                        Value::Float(f) => Ok(Value::Float(-f)),
                        _ => Err(anyhow!("Cannot negate {:?}", val)),
                    },
                    UnaryOp::Not => Ok(Value::Bool(!val.is_truthy())),
                    UnaryOp::BitNot => match val {
                        Value::Int(n) => Ok(Value::Int(!n)),
                        _ => Err(anyhow!("Cannot apply bitwise NOT to {:?}", val)),
                    },
                }
            }
            Expr::Call(call) => self.eval_call(
                &call.function,
                &call.args,
                &call.args_unpack,
                &call.kwargs,
                &call.kwargs_unpack,
            ),
            Expr::Index { object, index } => {
                let obj_val = self.eval_expression(object)?;
                let idx_val = self.eval_expression(index)?;
                match (obj_val, idx_val) {
                    (Value::List(items) | Value::Tuple(items) | Value::FrozenList(items), Value::Int(idx)) => {
                        let idx = if idx < 0 {
                            (items.len() as i64 + idx) as usize
                        } else {
                            idx as usize
                        };
                        items
                            .get(idx)
                            .cloned()
                            .ok_or_else(|| anyhow!("Index out of bounds"))
                    }
                    (Value::ImmutableMapping(view), key) => {
                        let found = view.key(&key)?.and_then(|key| view.entries.get(&key));
                        found.map(|value| view.exposed_value(value)).ok_or_else(|| anyhow!("key not found in immutable mapping"))
                    }
                    (Value::Dict(map), key) => {
                        let key_str = dictionary_key(&key)?;
                        map.get(&key_str)
                            .cloned()
                            .ok_or_else(|| anyhow!("Key not found: {}", key_str))
                    }
                    (Value::String(s), Value::Int(idx)) => {
                        let len = s.chars().count() as i64;
                        let mut index = idx;
                        if index < 0 {
                            index += len;
                        }
                        if index < 0 || index >= len {
                            Err(anyhow!("Index out of bounds"))
                        } else {
                            let ch = s.chars().nth(index as usize).unwrap_or_default();
                            Ok(Value::String(ch.to_string()))
                        }
                    }
                    (obj, idx) => Err(anyhow!(
                        "Invalid index operation: object={:?}, index={:?}",
                        obj,
                        idx
                    )),
                }
            }
            Expr::Attribute { object, attr } => {
                let obj_val = self.eval_expression(object)?;

                match obj_val {
                    Value::FrozenList(_) => match attr.as_str() {
                        "append" | "clear" | "extend" | "index" | "insert" | "pop" | "remove" | "count" =>
                            Ok(Value::BoundMethod { object: Box::new(obj_val.clone()), method: attr.clone() }),
                        _ => Err(anyhow!("list has no attribute '{attr}'")),
                    },
                    Value::ImmutableMapping(view) => {
                        if view.has_method(attr) {
                            Ok(Value::BoundMethod { object: Box::new(Value::ImmutableMapping(view)), method: attr.clone() })
                        } else {
                            Err(anyhow!("immutable mapping has no attribute '{attr}'"))
                        }
                    }
                    Value::ModuleExtension(_) => Err(anyhow!("module-extension declarations have no attribute '{attr}'")),
                    Value::WorkspaceNativeNamespace { attrs, workspace_root } => {
                        if attr == "bazel_version" {
                            crate::bzl_loader::workspace_repository_analyzer_version(&workspace_root)
                                .map(Value::String)
                        } else {
                            attrs.get(attr).cloned().ok_or_else(|| anyhow!("Attribute '{}' not found in namespace", attr))
                        }
                    }
                    Value::Namespace(attrs) => {
                        // Look up attribute in namespace
                        attrs
                            .get(attr)
                            .cloned()
                            .ok_or_else(|| anyhow!("Attribute '{}' not found in namespace", attr))
                    }
                    Value::ProviderInstance {
                        fields,
                        provider_name,
                    } => {
                        // Access field from provider instance
                        fields.get(attr).cloned().ok_or_else(|| {
                            anyhow!("Field '{}' not found in provider {}", attr, provider_name)
                        })
                    }
                    Value::String(_) => {
                        // String methods - return bound method
                        match attr.as_str() {
                            "split" | "rsplit" | "strip" | "replace" | "startswith"
                            | "endswith" | "upper" | "lower" | "format" | "join" | "capitalize"
                            | "count" | "find" | "rfind" | "index" | "rindex" | "lstrip"
                            | "rstrip" | "partition" | "rpartition" | "removeprefix"
                            | "removesuffix" | "isdigit" | "isalnum" | "isalpha"
                            | "isspace" | "islower" | "isupper" | "title" | "elems" => {
                                // Return a bound method
                                Ok(Value::BoundMethod {
                                    object: Box::new(obj_val),
                                    method: attr.clone(),
                                })
                            }
                            _ => Err(anyhow!("String has no attribute '{}'", attr)),
                        }
                    }
                    Value::Dict(_) => {
                        // Dict methods - return bound method
                        match attr.as_str() {
                            "keys" | "values" | "items" | "get" | "pop" | "update"
                            | "setdefault" => Ok(Value::BoundMethod {
                                object: Box::new(obj_val),
                                method: attr.clone(),
                            }),
                            _ => Err(anyhow!("Dict has no attribute '{}'", attr)),
                        }
                    }
                    Value::List(_) => {
                        // List methods - return bound method
                        match attr.as_str() {
                            "append" | "extend" | "remove" => Ok(Value::BoundMethod {
                                object: Box::new(obj_val),
                                method: attr.clone(),
                            }),
                            _ => Err(anyhow!("List has no attribute '{}'", attr)),
                        }
                    }
                    Value::Label {
                        label: _label,
                        package,
                        name,
                        workspace_name,
                        workspace_root,
                    } => {
                        // Label attributes: name, package, workspace_name, workspace_root
                        match attr.as_str() {
                            "name" => Ok(Value::String(name)),
                            "package" => Ok(Value::String(package)),
                            "workspace_name" => Ok(Value::String(workspace_name)),
                            "workspace_root" => Ok(Value::String(workspace_root)),
                            _ => Err(anyhow!("Label has no attribute '{}'", attr)),
                        }
                    }
                    Value::None => Err(anyhow!("'NoneType' object has no attribute '{}'", attr)),
                    _ => Err(anyhow!(
                        "Attribute access not supported on type {:?}",
                        obj_val
                    )),
                }
            }
            Expr::ListComp {
                element,
                for_clauses,
            } => {
                // Helper function to recursively evaluate nested for clauses
                fn eval_nested_for(
                    evaluator: &mut Evaluator,
                    element: &Expr,
                    for_clauses: &[crate::ast::ForClause],
                    clause_index: usize,
                    result: &mut Vec<Value>,
                ) -> Result<()> {
                    if clause_index >= for_clauses.len() {
                        // Base case: all for clauses processed, evaluate element
                        let elem_val = evaluator.eval_expression(element)?;
                        result.push(elem_val);
                        return Ok(());
                    }

                    let (vars, iterable_expr, condition) = &for_clauses[clause_index];

                    // Evaluate the iterable
                    let iterable_val = evaluator.eval_expression(iterable_expr.as_ref())?;

                    // Convert iterable to list
                    let items = match iterable_val {
                        Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => items,
                        Value::String(s) => {
                            // Strings are iterable in Python/Starlark
                            s.chars().map(|c| Value::String(c.to_string())).collect()
                        }
                        // Iterating a dict yields its keys (Starlark). rush's
                        // Dict is a HashMap, so sort keys for determinism (see
                        // builtin_list for the same rationale).
                        Value::Dict(map) => sorted_dictionary_key_values(&map),
                        Value::ImmutableMapping(view) => sorted_dictionary_key_values(&view.entries),
                        _ => return Err(anyhow!("Cannot iterate over {:?}", iterable_val)),
                    };

                    // Iterate over items
                    for item in items {
                        // Handle tuple unpacking: for x, y, z in items
                        if vars.len() == 1 {
                            // Single variable: bind directly
                            evaluator.env.define(vars[0].clone(), item);
                        } else {
                            // Multiple variables: unpack tuple/list
                            let values = match item {
                                Value::List(ref values) | Value::Tuple(ref values) | Value::FrozenList(ref values) => values,
                                _ => {
                                    return Err(anyhow!(
                                        "Cannot unpack {:?} into {} variables",
                                        item,
                                        vars.len()
                                    ))
                                }
                            };

                            if values.len() != vars.len() {
                                return Err(anyhow!(
                                    "Cannot unpack {} values into {} variables",
                                    values.len(),
                                    vars.len()
                                ));
                            }

                            // Bind each variable
                            for (var_name, value) in vars.iter().zip(values.iter()) {
                                evaluator.env.define(var_name.clone(), value.clone());
                            }
                        }

                        // Check condition if present
                        let include = if let Some(cond_expr) = condition {
                            let cond_val = evaluator.eval_expression(cond_expr.as_ref())?;
                            cond_val.is_truthy()
                        } else {
                            true
                        };

                        if include {
                            // Recursively process next for clause or evaluate element
                            eval_nested_for(
                                evaluator,
                                element,
                                for_clauses,
                                clause_index + 1,
                                result,
                            )?;
                        }
                    }

                    Ok(())
                }

                let mut result = Vec::new();
                eval_nested_for(self, element, for_clauses, 0, &mut result)?;
                Ok(Value::List(result))
            }
            Expr::DictComp {
                key,
                value,
                for_clauses,
            } => {
                // Helper function to recursively evaluate nested for clauses
                fn eval_nested_for(
                    evaluator: &mut Evaluator,
                    key_expr: &Expr,
                    value_expr: &Expr,
                    for_clauses: &[crate::ast::ForClause],
                    clause_index: usize,
                    result: &mut HashMap<String, Value>,
                ) -> Result<()> {
                    if clause_index >= for_clauses.len() {
                        // Base case: all for clauses processed, evaluate key and value
                        let key_val = evaluator.eval_expression(key_expr)?;
                        let val_val = evaluator.eval_expression(value_expr)?;

                        let key_str = dictionary_key(&key_val)?;

                        result.insert(key_str, val_val);
                        return Ok(());
                    }

                    let (vars, iterable_expr, condition) = &for_clauses[clause_index];

                    // Evaluate the iterable
                    let iterable_val = evaluator.eval_expression(iterable_expr.as_ref())?;

                    // Convert iterable to list
                    let items = match iterable_val {
                        Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => items,
                        Value::String(s) => {
                            // Strings are iterable in Python/Starlark
                            s.chars().map(|c| Value::String(c.to_string())).collect()
                        }
                        // Iterating a dict yields its keys (Starlark). rush's
                        // Dict is a HashMap, so sort keys for determinism (see
                        // builtin_list for the same rationale).
                        Value::Dict(map) => sorted_dictionary_key_values(&map),
                        Value::ImmutableMapping(view) => sorted_dictionary_key_values(&view.entries),
                        _ => return Err(anyhow!("Cannot iterate over {:?}", iterable_val)),
                    };

                    // Iterate over items
                    for item in items {
                        // Handle tuple unpacking: for x, y, z in items
                        if vars.len() == 1 {
                            // Single variable: bind directly
                            evaluator.env.define(vars[0].clone(), item);
                        } else {
                            // Multiple variables: unpack tuple/list
                            let values = match item {
                                Value::List(ref values) | Value::Tuple(ref values) | Value::FrozenList(ref values) => values,
                                _ => {
                                    return Err(anyhow!(
                                        "Cannot unpack {:?} into {} variables",
                                        item,
                                        vars.len()
                                    ))
                                }
                            };

                            if values.len() != vars.len() {
                                return Err(anyhow!(
                                    "Cannot unpack {} values into {} variables",
                                    values.len(),
                                    vars.len()
                                ));
                            }

                            // Bind each variable
                            for (var_name, value) in vars.iter().zip(values.iter()) {
                                evaluator.env.define(var_name.clone(), value.clone());
                            }
                        }

                        // Check condition if present
                        let include = if let Some(cond_expr) = condition {
                            let cond_val = evaluator.eval_expression(cond_expr.as_ref())?;
                            cond_val.is_truthy()
                        } else {
                            true
                        };

                        if include {
                            // Recursively process next for clause or evaluate key/value
                            eval_nested_for(
                                evaluator,
                                key_expr,
                                value_expr,
                                for_clauses,
                                clause_index + 1,
                                result,
                            )?;
                        }
                    }

                    Ok(())
                }

                let mut result = HashMap::new();
                eval_nested_for(self, key, value, for_clauses, 0, &mut result)?;
                Ok(Value::Dict(result))
            }
            Expr::IfExpr {
                condition,
                then_expr,
                else_expr,
            } => {
                // Evaluate condition
                let cond_val = self.eval_expression(condition)?;

                // Return then or else branch based on condition
                if cond_val.is_truthy() {
                    self.eval_expression(then_expr)
                } else {
                    self.eval_expression(else_expr)
                }
            }
            Expr::Tuple(elements) => {
                // Evaluate all tuple elements
                let values: Result<Vec<_>> =
                    elements.iter().map(|e| self.eval_expression(e)).collect();
                Ok(Value::Tuple(values?))
            }
            Expr::Slice {
                object,
                start,
                end,
                step,
            } => {
                let obj_val = self.eval_expression(object)?;

                // Helper to normalize index (handle negative indices)
                let normalize_idx = |idx: i64, len: usize| -> usize {
                    if idx < 0 {
                        (len as i64 + idx).max(0) as usize
                    } else {
                        (idx as usize).min(len)
                    }
                };

                // Evaluate start, end, step
                let start_val = if let Some(s) = start {
                    match self.eval_expression(s)? {
                        Value::Int(i) => Some(i),
                        _ => return Err(anyhow!("Slice indices must be integers")),
                    }
                } else {
                    None
                };

                let end_val = if let Some(e) = end {
                    match self.eval_expression(e)? {
                        Value::Int(i) => Some(i),
                        _ => return Err(anyhow!("Slice indices must be integers")),
                    }
                } else {
                    None
                };

                let step_val = if let Some(s) = step {
                    match self.eval_expression(s)? {
                        Value::Int(i) => i,
                        _ => return Err(anyhow!("Slice step must be integer")),
                    }
                } else {
                    1
                };

                if step_val == 0 {
                    return Err(anyhow!("Slice step cannot be zero"));
                }

                let tuple_slice = matches!(&obj_val, Value::Tuple(_));
                match obj_val {
                    Value::Tuple(items) | Value::FrozenList(items) => {
                        let len = items.len() as i64;
                        let bound = |value: i64| {
                            let value = if value < 0 { value.saturating_add(len) } else { value };
                            if step_val > 0 { value.clamp(0, len) }
                            else { value.clamp(-1, len - 1) }
                        };
                        let mut i = start_val.map(bound).unwrap_or(if step_val > 0 { 0 } else { len - 1 });
                        let end = end_val.map(bound).unwrap_or(if step_val > 0 { len } else { -1 });
                        let mut result = Vec::new();
                        while if step_val > 0 { i < end } else { i > end } {
                            result.push(items[i as usize].clone());
                            let Some(next) = i.checked_add(step_val) else { break };
                            i = next;
                        }
                        Ok(if tuple_slice { Value::Tuple(result) } else { Value::List(result) })
                    }
                    Value::List(items) => {
                        let len = items.len();
                        let start_idx = normalize_idx(start_val.unwrap_or(0), len);
                        let end_idx = normalize_idx(end_val.unwrap_or(len as i64), len);

                        let mut result = Vec::new();
                        if step_val > 0 {
                            let mut i = start_idx;
                            while i < end_idx {
                                if i < items.len() {
                                    result.push(items[i].clone());
                                }
                                i += step_val as usize;
                            }
                        } else {
                            // Negative step: reverse iteration
                            let mut i = start_idx;
                            loop {
                                if i < items.len() {
                                    result.push(items[i].clone());
                                }
                                if i <= end_idx || i == 0 {
                                    break;
                                }
                                i = i.saturating_sub(step_val.unsigned_abs() as usize);
                            }
                        }
                        Ok(Value::List(result))
                    }
                    Value::String(s) => {
                        let chars: Vec<char> = s.chars().collect();
                        let len = chars.len();
                        let start_idx = normalize_idx(start_val.unwrap_or(0), len);
                        let end_idx = normalize_idx(end_val.unwrap_or(len as i64), len);

                        let mut result = String::new();
                        if step_val > 0 {
                            let mut i = start_idx;
                            while i < end_idx {
                                if i < chars.len() {
                                    result.push(chars[i]);
                                }
                                i += step_val as usize;
                            }
                        } else {
                            // Negative step: reverse iteration
                            let mut i = start_idx;
                            loop {
                                if i < chars.len() {
                                    result.push(chars[i]);
                                }
                                if i <= end_idx || i == 0 {
                                    break;
                                }
                                i = i.saturating_sub(step_val.unsigned_abs() as usize);
                            }
                        }
                        Ok(Value::String(result))
                    }
                    _ => Err(anyhow!("Cannot slice {:?}", obj_val)),
                }
            }
            Expr::Lambda {
                params,
                varargs_param,
                kwargs_param,
                body,
            } => {
                // Lambda defaults use the same definition-time value
                // contract as named functions. Container binding retains the
                // evaluator's existing owned-Value cloning semantics.
                let params = params.iter().map(|(name, default)| {
                    Ok((name.clone(), default.as_ref().map(|expr| self.eval_expression(expr)).transpose()?))
                }).collect::<Result<Vec<_>>>()?;
                Ok(Value::Lambda {
                    params,
                    varargs_param: varargs_param.clone(),
                    kwargs_param: kwargs_param.clone(),
                    body: body.clone(),
                    closure: self.closure_environment()?,
                })
            }
        }
    }

    fn tuple_element_order(&self, left: &Value, right: &Value) -> Result<std::cmp::Ordering> {
        use std::cmp::Ordering;
        let compare_integer_float = |integer: i64, float: f64| -> Result<Ordering> {
            if float.is_nan() {
                return Err(anyhow!("comparison involving NaN"));
            }
            if float >= 9_223_372_036_854_775_808.0 {
                return Ok(Ordering::Less);
            }
            if float < i64::MIN as f64 {
                return Ok(Ordering::Greater);
            }
            match integer.cmp(&(float as i64)) {
                Ordering::Equal if float.fract() > 0.0 => Ok(Ordering::Less),
                Ordering::Equal if float.fract() < 0.0 => Ok(Ordering::Greater),
                order => Ok(order),
            }
        };
        match (left, right) {
            (Value::Int(integer), Value::Float(float)) => compare_integer_float(*integer, *float),
            (Value::Float(float), Value::Int(integer)) => {
                compare_integer_float(*integer, *float).map(Ordering::reverse)
            }
            // Nested lists inside a tuple participate in the same recursive
            // ordering. Ordinary top-level List ordering stays unchanged.
            (Value::List(left), Value::List(right)) => {
                for (left, right) in left.iter().zip(right) {
                    if tuple_element_equal(left, right) {
                        continue;
                    }
                    match self.tuple_element_order(left, right)? {
                        Ordering::Equal => continue,
                        order => return Ok(order),
                    }
                }
                Ok(left.len().cmp(&right.len()))
            }
            _ => self.starlark_order(left, right),
        }
    }

    /// Starlark ordering: numbers numerically, strings/bools by natural order,
    /// lists/tuples lexicographically (recursively, element by element, shorter
    /// is less when one is a prefix of the other). Errors on incomparable type
    /// pairs — Starlark does not order across unrelated types. Used by the
    /// `< <= > >=` operators; bazel_features' parse.bzl compares version tuples
    /// like `([7,0,0], True, "")` this way.
    fn starlark_order(&self, a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
        use std::cmp::Ordering;
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
            (Value::Bool(x), Value::Bool(y)) => Ok(x.cmp(y)),
            (Value::String(x), Value::String(y)) => Ok(x.cmp(y)),
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                let x = a.to_float()?;
                let y = b.to_float()?;
                x.partial_cmp(&y).ok_or_else(|| anyhow!("comparison involving NaN"))
            }
            (Value::Tuple(xs), Value::Tuple(ys)) => {
                for (xi, yi) in xs.iter().zip(ys.iter()) {
                    // Equality is enough to skip a common prefix; those values
                    // need not themselves support ordering (e.g. None/dicts).
                    if tuple_element_equal(xi, yi) {
                        continue;
                    }
                    match self.tuple_element_order(xi, yi)? {
                        Ordering::Equal => continue,
                        ord => return Ok(ord),
                    }
                }
                Ok(xs.len().cmp(&ys.len()))
            }
            (Value::List(xs) | Value::FrozenList(xs), Value::List(ys) | Value::FrozenList(ys)) => {
                for (xi, yi) in xs.iter().zip(ys.iter()) {
                    match self.starlark_order(xi, yi)? {
                        Ordering::Equal => continue,
                        ord => return Ok(ord),
                    }
                }
                Ok(xs.len().cmp(&ys.len()))
            }
            _ => Err(anyhow!("unsupported comparison: {:?} vs {:?}", a, b)),
        }
    }

    fn apply_binop(&self, left: &Value, op: BinOp, right: &Value) -> Result<Value> {
        // A select remains symbolic until target configuration. Do not
        // distribute composition over its branches: a chain of N independent
        // two-way selects would retain 2^N values (TF's feature lists reached
        // 161 GiB before BUILD evaluation completed). An Arc-backed expression
        // DAG is linear in the number of operands and is resolved later by the
        // rule-execution bridge.
        if (left.is_configurable() || right.is_configurable())
            && matches!(op, BinOp::Add | BinOp::BitOr)
        {
            let op = match op {
                BinOp::Add => ConfigurableOp::Add,
                BinOp::BitOr => ConfigurableOp::BitOr,
                _ => unreachable!(),
            };
            // Bazel validates a SelectorList's underlying native types when
            // the expression is formed, not only after one branch is chosen.
            // Walk branch *shapes* (a small set of native kinds), never their
            // Cartesian values, so invalid list+string/dict|list expressions
            // still fail during module evaluation without reintroducing the
            // TensorFlow-scale branch explosion.
            Value::configurable_result_kinds(op, left, right)?;
            return Ok(Value::Configurable {
                op,
                left: Arc::new(left.clone()),
                right: Arc::new(right.clone()),
            });
        }

        match op {
            BinOp::Add => match (left, right) {
                (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a + b)),
                (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
                (Value::Int(a), Value::Float(b)) => Ok(Value::Float(*a as f64 + b)),
                (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a + *b as f64)),
                (Value::String(a), Value::String(b)) => Ok(Value::String(format!("{}{}", a, b))),
                (Value::List(a) | Value::FrozenList(a), Value::List(b) | Value::FrozenList(b)) => {
                    let mut result = a.clone();
                    result.extend(b.clone());
                    Ok(Value::List(result))
                }
                (Value::Tuple(a), Value::Tuple(b)) => {
                    let mut result = a.clone();
                    result.extend(b.iter().cloned());
                    Ok(Value::Tuple(result))
                }
                // None + List = List (Starlark treats None as empty list in concatenation)
                (Value::None, Value::List(b)) => Ok(Value::List(b.clone())),
                (Value::List(a), Value::None) => Ok(Value::List(a.clone())),
                (Value::None, Value::None) => Ok(Value::None),
                _ => Err(anyhow!("Cannot add {:?} and {:?}", left, right)),
            },
            BinOp::Sub => match (left, right) {
                (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a - b)),
                (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a - b)),
                (Value::Int(a), Value::Float(b)) => Ok(Value::Float(*a as f64 - b)),
                (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a - *b as f64)),
                // Bazel 8 set difference: rush's set() builtin returns a
                // deduped List, so `set(a) - set(b)` (e.g. modular_py_test's
                // per_test_tags validation) lands here. Keep left elements
                // not present in right, preserving left order.
                (Value::List(a), Value::List(b)) => Ok(Value::List(
                    a.iter().filter(|x| !b.contains(x)).cloned().collect(),
                )),
                _ => Err(anyhow!("Cannot subtract {:?} and {:?}", left, right)),
            },
            BinOp::Mul => match (left, right) {
                (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a * b)),
                (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a * b)),
                (Value::Int(a), Value::Float(b)) => Ok(Value::Float(*a as f64 * b)),
                (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a * *b as f64)),
                (Value::String(s), Value::Int(n)) | (Value::Int(n), Value::String(s)) => {
                    Ok(Value::String(s.repeat(*n as usize)))
                }
                (Value::Tuple(items), Value::Int(n)) | (Value::Int(n), Value::Tuple(items)) => {
                    let mut result = Vec::new();
                    for _ in 0..(*n).max(0) { result.extend(items.iter().cloned()); }
                    Ok(Value::Tuple(result))
                }
                // List repetition: [1, 2] * 3 = [1, 2, 1, 2, 1, 2]
                (Value::List(l) | Value::FrozenList(l), Value::Int(n))
                | (Value::Int(n), Value::List(l) | Value::FrozenList(l)) => {
                    if *n < 0 {
                        // Negative repetition returns empty list (Python/Starlark behavior)
                        Ok(Value::List(Vec::new()))
                    } else {
                        let mut result = Vec::new();
                        for _ in 0..*n {
                            result.extend(l.clone());
                        }
                        Ok(Value::List(result))
                    }
                }
                _ => Err(anyhow!("Cannot multiply {:?} and {:?}", left, right)),
            },
            BinOp::Div => {
                let a = left.to_float()?;
                let b = right.to_float()?;
                Ok(Value::Float(a / b))
            }
            BinOp::FloorDiv => {
                let a = left.to_int()?;
                let b = right.to_int()?;
                Ok(Value::Int(a / b))
            }
            BinOp::Mod => {
                // String formatting if left operand is a string
                if let Value::String(format_str) = &left {
                    // Python/Starlark string formatting: "foo-%s" % value
                    let values: Vec<Value> = match &right {
                        Value::Tuple(items) => items.clone(),
                        Value::String(s) => vec![Value::String(s.clone())],
                        other => vec![(*other).clone()],
                    };

                    // Simple implementation: replace %s, %d, %r one by one
                    let mut result = format_str.clone();
                    let mut value_idx = 0;

                    // Find and replace format specifiers
                    let mut chars: Vec<char> = result.chars().collect();
                    let mut i = 0;
                    while i < chars.len() - 1 {
                        if chars[i] == '%' {
                            let spec = chars[i + 1];
                            if spec == 's' || spec == 'd' || spec == 'r' || spec == 'i' {
                                if value_idx >= values.len() {
                                    return Err(anyhow!("Not enough arguments for format string"));
                                }

                                let replacement = match spec {
                                    's' | 'r' => values[value_idx].to_string(),
                                    'd' | 'i' => {
                                        let val = values[value_idx].to_int()?;
                                        val.to_string()
                                    }
                                    _ => unreachable!(),
                                };

                                // Replace %s, %d, %r, %i with the actual value
                                let before: String = chars[..i].iter().collect();
                                let after: String = chars[i + 2..].iter().collect();
                                result = format!("{}{}{}", before, replacement, after);
                                chars = result.chars().collect();
                                i += replacement.len();
                                value_idx += 1;
                                continue;
                            }
                        }
                        i += 1;
                    }

                    if value_idx < values.len() {
                        return Err(anyhow!("Not all arguments converted in format string"));
                    }

                    Ok(Value::String(result))
                } else {
                    // Numeric modulo
                    let a = left.to_int()?;
                    let b = right.to_int()?;
                    Ok(Value::Int(a % b))
                }
            }
            BinOp::Pow => {
                let a = left.to_float()?;
                let b = right.to_float()?;
                Ok(Value::Float(a.powf(b)))
            }
            BinOp::Eq => Ok(Value::Bool(match (left, right) {
                (Value::Tuple(_), Value::Tuple(_)) => tuple_element_equal(left, right),
                _ => left == right,
            })),
            BinOp::NotEq => Ok(Value::Bool(match (left, right) {
                (Value::Tuple(_), Value::Tuple(_)) => !tuple_element_equal(left, right),
                _ => left != right,
            })),
            BinOp::Lt => Ok(Value::Bool(
                self.starlark_order(left, right)? == std::cmp::Ordering::Less,
            )),
            BinOp::LtE => Ok(Value::Bool(
                self.starlark_order(left, right)? != std::cmp::Ordering::Greater,
            )),
            BinOp::Gt => Ok(Value::Bool(
                self.starlark_order(left, right)? == std::cmp::Ordering::Greater,
            )),
            BinOp::GtE => Ok(Value::Bool(
                self.starlark_order(left, right)? != std::cmp::Ordering::Less,
            )),
            // Bitwise operators (integers), plus Bazel 8 set semantics on
            // rush's List-backed set() representation.
            BinOp::BitAnd => match (left, right) {
                // Set intersection, preserving left order.
                (Value::List(a), Value::List(b)) => {
                    let mut r: Vec<Value> = Vec::new();
                    for x in a {
                        if b.contains(x) && !r.contains(x) {
                            r.push(x.clone());
                        }
                    }
                    Ok(Value::List(r))
                }
                _ => {
                    let a = left.to_int()?;
                    let b = right.to_int()?;
                    Ok(Value::Int(a & b))
                }
            },
            BinOp::BitOr => match (left, right) {
                // Dict union (Bazel 7+/PEP 584 `d1 | d2`): the right operand wins
                // on key collisions. This is the common Bazel .bzl use of `|`
                // (e.g. `attrs = base | {...}`); HashMap::extend overwrites with
                // the later (right) entries.
                (Value::Dict(a), Value::Dict(b)) => {
                    let mut merged = a.clone();
                    merged.extend(b.iter().map(|(k, v)| (k.clone(), v.clone())));
                    Ok(Value::Dict(merged))
                }
                // Set union on the List-backed set() representation,
                // preserving first-seen order.
                (Value::List(a), Value::List(b)) => {
                    let mut r: Vec<Value> = Vec::new();
                    for x in a.iter().chain(b.iter()) {
                        if !r.contains(x) {
                            r.push(x.clone());
                        }
                    }
                    Ok(Value::List(r))
                }
                // Otherwise integer bitwise-or (unchanged behavior).
                _ => {
                    let a = left.to_int()?;
                    let b = right.to_int()?;
                    Ok(Value::Int(a | b))
                }
            },
            BinOp::BitXor => match (left, right) {
                (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a ^ b)),
                _ => Err(anyhow!(
                    "Operator ^ requires integer operands, got {} and {}",
                    starlark_type_name(left)?,
                    starlark_type_name(right)?,
                )),
            },
            BinOp::LeftShift => {
                let a = left.to_int()?;
                let b = right.to_int()?;
                if b < 0 {
                    return Err(anyhow!("Negative shift count"));
                }
                Ok(Value::Int(a << b))
            }
            BinOp::RightShift => {
                let a = left.to_int()?;
                let b = right.to_int()?;
                if b < 0 {
                    return Err(anyhow!("Negative shift count"));
                }
                Ok(Value::Int(a >> b))
            }
            BinOp::And => {
                if !left.is_truthy() {
                    Ok(left.clone())
                } else {
                    Ok(right.clone())
                }
            }
            BinOp::Or => {
                if left.is_truthy() {
                    Ok(left.clone())
                } else {
                    Ok(right.clone())
                }
            }
            BinOp::In => match right {
                Value::Tuple(items) => Ok(Value::Bool(items.iter().any(|item| tuple_element_equal(item, left)))),
                Value::List(items) | Value::FrozenList(items) => Ok(Value::Bool(items.contains(left))),
                Value::String(s) => {
                    let needle = left.to_string();
                    Ok(Value::Bool(s.contains(&needle)))
                }
                Value::ImmutableMapping(view) => Ok(Value::Bool(
                    view.key(left)?.is_some_and(|key| view.entries.contains_key(&key)),
                )),
                Value::Dict(map) => {
                    // Check if key exists in dict
                    let key = dictionary_key(left)?;
                    Ok(Value::Bool(map.contains_key(&key)))
                }
                _ => Err(anyhow!("Cannot check membership in {:?}", right)),
            },
            BinOp::NotIn => match right {
                Value::Tuple(items) => Ok(Value::Bool(!items.iter().any(|item| tuple_element_equal(item, left)))),
                Value::List(items) | Value::FrozenList(items) => Ok(Value::Bool(!items.contains(left))),
                Value::String(s) => {
                    let needle = left.to_string();
                    Ok(Value::Bool(!s.contains(&needle)))
                }
                Value::ImmutableMapping(view) => Ok(Value::Bool(
                    !view.key(left)?.is_some_and(|key| view.entries.contains_key(&key)),
                )),
                Value::Dict(map) => {
                    // Check if key does not exist in dict
                    let key = dictionary_key(left)?;
                    Ok(Value::Bool(!map.contains_key(&key)))
                }
                _ => Err(anyhow!("Cannot check membership in {:?}", right)),
            },
        }
    }

    fn eval_call(
        &mut self,
        function: &Expr,
        args: &[Expr],
        args_unpack: &[Expr],
        kwargs: &[(String, Expr)],
        kwargs_unpack: &[Expr],
    ) -> Result<Value> {
        // Note: Disabled printing function AST as it causes extreme slowness for complex expressions

        // Debug: Log kwargs before processing
        if let Expr::Ident(name) = function {
            debug_log!(
                "[DEBUG eval_call] Function: {}, kwargs len: {}, kwargs_unpack len: {}",
                name,
                kwargs.len(),
                kwargs_unpack.len()
            );
        }

        // Helper to merge args and args_unpack
        let merge_args =
            |base_args: &[Expr], unpack: &[Expr], evaluator: &mut Self| -> Result<Vec<Value>> {
                let mut merged = Vec::new();

                // Add regular args
                for arg_expr in base_args {
                    let val = evaluator.eval_expression(arg_expr)?;
                    merged.push(val);
                }

                // Merge unpacked lists
                for list_expr in unpack {
                    let list_val = evaluator.eval_expression(list_expr)?;
                    match list_val {
                        Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => {
                            merged.extend(items);
                        }
                        Value::Dict(map) => merged.extend(sorted_dictionary_key_values(&map)),
                        _ => {
                            return Err(anyhow!(
                                "Cannot unpack non-iterable value in *args: {:?}",
                                list_val
                            ))
                        }
                    }
                }

                Ok(merged)
            };

        // Helper to merge kwargs and kwargs_unpack
        let merge_kwargs = |base_kwargs: &[(String, Expr)],
                            unpack: &[Expr],
                            evaluator: &mut Self|
         -> Result<HashMap<String, Value>> {
            let mut merged = HashMap::new();
            let mut mapping_keys = HashSet::new();

            // Add regular kwargs
            for (key, val_expr) in base_kwargs {
                let val = evaluator.eval_expression(val_expr)?;
                merged.insert(key.clone(), val);
            }

            // Merge unpacked dicts
            for dict_expr in unpack {
                let raw = evaluator.eval_expression(dict_expr)?;
                let immutable_mapping = matches!(&raw, Value::ImmutableMapping(_));
                let dict_val = dictionary_expansion_value(raw);
                match dict_val {
                    Value::Dict(dict_map) => {
                        for (k, v) in dict_map {
                            if immutable_mapping {
                                if !matches!(dictionary_key_value(&k), Value::String(_)) {
                                    anyhow::bail!("keyword argument names must be strings");
                                }
                                if merged.contains_key(&k) { anyhow::bail!("multiple values for keyword argument '{k}'"); }
                                mapping_keys.insert(k.clone());
                            } else if mapping_keys.contains(&k) && merged.contains_key(&k) {
                                anyhow::bail!("multiple values for keyword argument '{k}'");
                            }
                            merged.insert(k, v);
                        }
                    }
                    _ => {
                        return Err(anyhow!(
                            "Cannot unpack non-dict value in function call: {:?}",
                            dict_val
                        ))
                    }
                }
            }

            Ok(merged)
        };

        // A new declaration constructor must not inherit the legacy last-wins
        // kwargs merger. Reject ambiguous arguments for direct and alias calls.
        let extension_kwargs = |base: &[(String, Expr)], unpack: &[Expr], evaluator: &mut Self|
            -> Result<Vec<(String, Value)>> {
            let mut values = Vec::new();
            let mut names = HashSet::new();
            for (name, expression) in base {
                if !names.insert(name.clone()) {
                    return Err(anyhow!("module_extension() got multiple values for '{name}'"));
                }
                values.push((name.clone(), evaluator.eval_expression(expression)?));
            }
            for expression in unpack {
                let Value::Dict(items) = dictionary_expansion_value(evaluator.eval_expression(expression)?) else {
                    return Err(anyhow!("module_extension() **kwargs must be a dict"));
                };
                for (key, value) in items {
                    let Value::String(name) = dictionary_key_value(&key) else {
                        return Err(anyhow!("module_extension() keyword names must be strings"));
                    };
                    if !names.insert(name.clone()) {
                        return Err(anyhow!("module_extension() got multiple values for '{name}'"));
                    }
                    values.push((name, value));
                }
            }
            Ok(values)
        };

        // Check if this is a function identifier (for built-ins and rule functions)
        if let Expr::Ident(name) = function {
            // Check for built-in functions
            if let Some(builtin) = self.builtins.get(name).copied().filter(|_| {
                (name != "module_extension" && name != "rule") || self.env.get(name).is_none()
            }) {
                if name == "module_extension" {
                    if !self.env.bzl_definition_scope {
                        return Err(anyhow!("Undefined variable: module_extension (available only in .bzl definitions)"));
                    }
                    let args = merge_args(args, args_unpack, self)?;
                    let kwargs = extension_kwargs(kwargs, kwargs_unpack, self)?;
                    return builtin(&args, &kwargs);
                }
                // Phase 5.C.4 commit 106: bump the
                // coarse op counter. `glob` is tracked
                // as its own category so it can be
                // surfaced separately.
                if name == "glob" {
                    crate::bzl_loader::record_starlark_op(
                        crate::bzl_loader::StarlarkOpKind::GlobCall,
                    );
                } else {
                    crate::bzl_loader::record_starlark_op(
                        crate::bzl_loader::StarlarkOpKind::BuiltinCall,
                    );
                }
                // Merge args and args_unpack
                let mut eval_args = merge_args(args, args_unpack, self)?;

                if name == "Label" {
                    self.qualify_label_argument(&mut eval_args)?;
                }

                // Merge kwargs and kwargs_unpack
                let merged_kwargs_map = merge_kwargs(kwargs, kwargs_unpack, self)?;
                if name == "package" {
                    self.record_package_metadata(&eval_args, &merged_kwargs_map)?;
                    return Ok(Value::None);
                }
                if name == "rule" && eval_args.is_empty() {
                    if let Some(candidate) =
                        crate::cc_projection_definition::ProjectionRuleDefinition::candidate(
                            &merged_kwargs_map,
                            self.env.cc_projection_origin.clone(),
                        )
                    {
                        return Ok(Value::ProjectionRule(candidate));
                    }
                }
                let eval_kwargs: Vec<(String, Value)> = merged_kwargs_map.into_iter().collect();

                // Bare `glob(...)` in a BUILD file is package-relative (Bazel
                // semantics). Prefer the absolute package directory installed by
                // starlark_bridge for BOTH main and external repositories. This
                // avoids relying on the process CWD and gives the glob walker the
                // exact package root from which nested BUILD boundaries are pruned.
                if name == "glob" {
                    if let Some(dir) = self.current_package_dir.as_ref() {
                        return crate::builtins::glob_in_package(
                            &eval_args,
                            &eval_kwargs,
                            &dir.to_string_lossy(),
                        );
                    }
                    // Compatibility fallback for evaluator users that install a
                    // main-repository package name but no absolute package dir.
                    if self.current_repository.is_none() && !self.current_package.is_empty() {
                        return crate::builtins::glob_in_package(
                            &eval_args,
                            &eval_kwargs,
                            &self.current_package,
                        );
                    }
                }

                return builtin(&eval_args, &eval_kwargs);
            }

            // Check if this is a registered rule function (BUILD rules)
            // Rule functions are registered but have None as their value
            let func_value = self.env.get(name);
            match &func_value {
                Some(Value::None) => {
                    debug_log!("[DEBUG func_value] {}: None (registered rule)", name)
                }
                Some(Value::Function { .. }) => debug_log!("[DEBUG func_value] {}: Function", name),
                Some(Value::String(s)) => debug_log!("[DEBUG func_value] {}: String({})", name, s),
                Some(Value::RuleFunction(s)) => {
                    debug_log!("[DEBUG func_value] {}: RuleFunction({})", name, s)
                }
                Some(Value::NativeFunction(s)) => {
                    debug_log!("[DEBUG func_value] {}: NativeFunction({})", name, s)
                }
                Some(Value::ProjectionRule(_)) => debug_log!(
                    "[DEBUG func_value] {}: source-qualified projection candidate",
                    name
                ),
                Some(Value::Provider {
                    name: provider_name,
                    fields,
                    init,
                }) => debug_log!(
                    "[DEBUG func_value] {}: Provider({}, fields={}, init={})",
                    name,
                    provider_name,
                    fields.len(),
                    init.is_some()
                ),
                Some(v) => debug_log!("[DEBUG func_value] {}: Other ({:?})", name, v),
                None => debug_log!("[DEBUG func_value] {}: Not found", name),
            }

            // Allowlist: certain macros should always use the rush-native expander,
            // overriding any Starlark-defined function with the same name that was
            // loaded from a .bzl file. This matters for macros whose upstream
            // Starlark implementation pulls in rules rush doesn't yet support
            // (e.g. `proto_gen`) where rush's alias-to-base-target expansion is the
            // correct shape for our native proto compilation path to apply.
            const MACRO_PRIORITY_OVERRIDE: &[&str] = &[
                "tf_proto_library",
                // Preserve TensorFlow's inline template action as a native
                // declaration; configured provider validation precedes writing.
                "tf_gen_options_header",
                // Starlark runs mlir-tblgen via a Bazel tool action; route
                // to rush's native GentblCcLibraryRule which runs
                // mlir-tblgen directly from the system install.
                "gentbl_cc_library",
                // Bazel Skylib implements expand_template as a custom
                // Starlark rule. Route it to Rush's native renderer so its
                // generated file is present in the action graph.
                "expand_template",
                // The synthesized tblgen.bzl defines td_library as a no-op
                // (the def must exist so `load(..., "td_library")`
                // resolves), which erased every *TdFiles/*_td_files target
                // from the graph and with them the transitive tblgen -I
                // roots. Route to rush's native TdLibraryRule, which
                // materializes the targets and exposes `includes` as
                // CcInfo include_dirs for gentbl consumers.
                "td_library",
                // TF's tf_cc_shared_library macro is the OS/version-matrix
                // wrapper over native.cc_shared_library. Its Starlark body
                // uses `names = _get_shared_library_name_os_version_matrix(
                // ...)` plus conditional native.alias/native.cc_shared_library
                // calls that rush's evaluator doesn't fully expand. Route
                // both the TF wrapper and the native rule straight to
                // rush's own CcSharedLibraryRule / TfCcSharedLibraryRule —
                // link-once, dynamic_deps filtering, and the `-shared`
                // link action all live there.
                "tf_cc_shared_library",
                "cc_shared_library",
                // TF loads @flatbuffers//:build_defs.bzl, which is stubbed in
                // rush. Its flatbuffer_cc_library Starlark impl calls
                // native.genrule with a $(location @flatbuffers//:flatc) tool
                // rush can't resolve. Route to our native expander which
                // invokes flatc and emits the genrule, cc_library and
                // includes filegroup.
                "flatbuffer_cc_library",
                // TF's tf_gen_op_wrappers_cc macro builds a per-op codegen
                // tf_cc_binary + a genrule that runs it to emit ops/<n>.{h,cc}
                // then bundles them into cc_library. Route to rush's native
                // expander (expand_tf_gen_op_wrappers_cc) which emits those
                // sub-rules directly.
                "tf_gen_op_wrappers_cc",
                // The SINGULAR tf_gen_op_wrapper_cc (one op lib, no bundling
                // cc_library) has a real Starlark def loaded from tensorflow.bzl,
                // so without this override the loaded def wins and its genrule's
                // `$(location :<tool>)` reaches the shell unresolved (the tool
                // tf_cc_binary isn't registered where the genrule executor can
                // find it). Force rush's native expand_tf_gen_op_wrapper_cc
                // (e.g. tf2xla/cc:xla_ops_gen, xla_jit_op_gen).
                "tf_gen_op_wrapper_cc",
            ];
            // An entry in MACRO_PRIORITY_OVERRIDE forces the rule-call
            // branch even if a Starlark `def` loaded from a .bzl would
            // otherwise win. If a macro expander is registered for the
            // name, the expander runs; otherwise the call is captured
            // verbatim as a RuleCall (see the "Regular rule call" path
            // below) — which is what we want for cc_shared_library /
            // tf_cc_shared_library where rush's native rule parses the
            // attrs directly.
            // Under RUSH_ENABLE_ROCM or RUSH_ENABLE_CUDA, force the GPU
            // tf_cc_test macro expansion
            // (tf_gpu_cc_test/tf_cuda_cc_test → <name>_cpu/<name>_gpu) so a real GPU
            // tf_cc_test can be built. CPU builds leave these to the no-op builtin
            // (byte-identical). (Codex 019eebc6.)
            //
            // Persistent-daemon TrackedIo migration (env slice): this RUSH_ENABLE_ROCM
            // read runs inside the Starlark evaluator's `eval_call`. The evaluator is
            // now brought under a `tracked::scope` at every rush-build invocation site
            // (parse / macro expansion / instantiate / analyze-only), so the read
            // records an `Env` dep through the fail-loud `tracked::getenv_str`.
            let rocm_enabled =
                rush_incremental::tracked::getenv_str("RUSH_ENABLE_ROCM").as_deref() == Some("1");
            let cuda_enabled =
                rush_incremental::tracked::getenv_str("RUSH_ENABLE_CUDA").as_deref() == Some("1");
            let gpu_test_macro = (rocm_enabled || cuda_enabled)
                && matches!(name.as_str(), "tf_gpu_cc_test" | "tf_cuda_cc_test");
            let force_macro_expand =
                MACRO_PRIORITY_OVERRIDE.contains(&name.as_str()) || gpu_test_macro;

            if func_value == Some(Value::None) || force_macro_expand {
                // This is a BUILD rule call - merge arguments first
                let eval_args = merge_args(args, args_unpack, self)?;
                let mut eval_kwargs = merge_kwargs(kwargs, kwargs_unpack, self)?;
                for value in eval_kwargs.values_mut() {
                    if matches!(value, Value::ImmutableMapping(view) if view.kind == ImmutableMappingKind::FrozenDict) {
                        *value = dictionary_expansion_value(value.clone());
                    }
                }
                Self::validate_loading_time_rule_attributes(name, &eval_kwargs)?;

                // Check if this is a macro that needs expansion
                if let Some(expander) = self.macro_expanders.get(name).copied() {
                    // Phase 5.C.4 commit 106: macro
                    // expansion counts as one
                    // MacroInvocation; the rules it
                    // emits count below.
                    crate::bzl_loader::record_starlark_op(
                        crate::bzl_loader::StarlarkOpKind::MacroInvocation,
                    );
                    debug_log!("[MACRO] Expanding {} macro", name);

                    // A proto generator is a declared tool dependency, not a
                    // process-wide protoc search. Qualify string attributes in
                    // the BUILD caller's repository mapping before expansion.
                    if name == "tf_proto_library" {
                        let requested = eval_kwargs.get("protoc").cloned().unwrap_or_else(|| {
                            Value::String("@com_google_protobuf//:protoc".to_string())
                        });
                        let label = self.qualify_proto_tool_label(&requested)?;
                        eval_kwargs.insert("protoc".to_string(), Value::String(label));
                    }
                    // Expand the macro into one or more rule calls
                    let expanded_rules = expander(name, &eval_args, &eval_kwargs)?;

                    debug_log!(
                        "[MACRO] Generated {} rule(s) from {} macro",
                        expanded_rules.len(),
                        name
                    );

                    // Capture all expanded rules
                    for (i, rule) in expanded_rules.iter().enumerate() {
                        Self::validate_loading_time_rule_attributes(&rule.rule_type, &rule.kwargs)?;
                        debug_log!(
                            "[MACRO]   Rule {}: {} with {} deps",
                            i + 1,
                            rule.rule_type,
                            rule.kwargs
                                .get("deps")
                                .and_then(|v| match v {
                                    Value::List(l) => Some(l.len()),
                                    _ => None,
                                })
                                .unwrap_or(0)
                        );
                        self.maybe_activate_config_setting(rule);
                        self.rule_calls.push(rule.clone());
                    }

                    return Ok(Value::None);
                }

                // Phase 5.C.4 commit 106: a captured
                // rule call counts as one
                // RuleInvocation.
                crate::bzl_loader::record_starlark_op(
                    crate::bzl_loader::StarlarkOpKind::RuleInvocation,
                );
                // Regular rule call - capture it
                let rule_call = RuleCall {
                    cc_projection: None,
                    rule_type: name.clone(),
                    args: eval_args.clone(),
                    kwargs: eval_kwargs.clone(),
                };
                debug_log!(
                    "[DEBUG before push] Creating RuleCall: rule_type={}, kwargs keys: {:?}",
                    rule_call.rule_type,
                    rule_call.kwargs.keys().collect::<Vec<_>>()
                );
                self.maybe_activate_config_setting(&rule_call);
                self.rule_calls.push(rule_call);

                // #3 Bzlmod: use_extension(...) returns a module-extension PROXY
                // (a sentinel RuleFunction) so subsequent `ext.tag(...)` /
                // `ext.install()` method calls + `use_repo(ext, ...)` don't crash
                // the MODULE.bazel eval. rush v1 TOLERATES (does not evaluate)
                // module extensions; see BZLMOD_EXT_PROXY.
                if name == "use_extension" {
                    return Ok(Value::RuleFunction(BZLMOD_EXT_PROXY.to_string()));
                }
                return Ok(Value::None);
            }
        }

        // #3 Bzlmod: a method call on a module-extension proxy ident
        // (e.g. `internal_deps.install()`) is a no-op returning the proxy
        // (chainable). Scoped to the sentinel value, so every other value's
        // method-call behavior — and thus BUILD eval / the certified 463 — is
        // unchanged (BUILD files never produce this value).
        if let Expr::Attribute { object, .. } = function {
            if let Expr::Ident(obj_name) = object.as_ref() {
                if let Some(Value::RuleFunction(n)) = self.env.get(obj_name) {
                    if n == BZLMOD_EXT_PROXY {
                        return Ok(Value::RuleFunction(BZLMOD_EXT_PROXY.to_string()));
                    }
                }
            }
        }

        // Regular function call
        let func_val = self.eval_expression(function)?;

        match func_val {
            Value::Function {
                params,
                varargs_param,
                kwargs_param,
                body,
                closure,
            } => {
                // Phase 5.C.4 commit 106: a Value::Function
                // invocation counts as a FunctionCall.
                crate::bzl_loader::record_starlark_op(
                    crate::bzl_loader::StarlarkOpKind::FunctionCall,
                );
                // Create new environment with closure
                let mut func_env = Environment::with_parent(closure.clone());
                let mut mutable_aliases: Vec<(String, String, Value)> = Vec::new();
                fn is_mutable_value(value: &Value) -> bool {
                    match value {
                        Value::Tuple(items) => items.iter().any(is_mutable_value),
                        Value::List(_) | Value::Dict(_) | Value::Namespace(_)
                        | Value::WorkspaceNativeNamespace { .. } | Value::ProviderInstance { .. } => true,
                        _ => false,
                    }
                }

                // Merge args and args_unpack to get all positional arguments
                let all_args = merge_args(args, args_unpack, self)?;

                // Check if we have too many positional arguments (only if no *args)
                if varargs_param.is_none() && all_args.len() > params.len() {
                    return Err(anyhow!(
                        "Too many positional arguments: expected {}, got {}",
                        params.len(),
                        all_args.len()
                    ));
                }

                // Track which parameters have been bound
                let mut bound_params = std::collections::HashSet::new();

                // Bind positional arguments to parameters
                let mut varargs_values = Vec::new();
                for (i, arg_val) in all_args.into_iter().enumerate() {
                    if i < params.len() {
                        // Bind to regular parameter
                        let (param_name, _) = &params[i];
                        if is_mutable_value(&arg_val) {
                            if let Some(Expr::Ident(caller_name)) = args.get(i) {
                                mutable_aliases.push((
                                    param_name.clone(),
                                    caller_name.clone(),
                                    arg_val.clone(),
                                ));
                            }
                        }
                        func_env.define(param_name.clone(), arg_val);
                        bound_params.insert(param_name.clone());
                    } else {
                        // Extra arguments go to *args
                        varargs_values.push(arg_val);
                    }
                }

                // Bind *args if present
                if let Some(varargs_name) = &varargs_param {
                    func_env.define(varargs_name.clone(), Value::Tuple(varargs_values));
                }

                // Separate kwargs into named params and **kwargs
                let mut extra_kwargs = HashMap::new();
                let param_names: Vec<_> = params.iter().map(|(name, _)| name.as_str()).collect();

                // Process regular kwargs
                for (key, val_expr) in kwargs {
                    let val = self.eval_expression(val_expr)?;

                    // If key matches a named parameter, bind it directly
                    if param_names.contains(&key.as_str()) {
                        if is_mutable_value(&val) {
                            if let Expr::Ident(caller_name) = val_expr {
                                mutable_aliases.push((
                                    key.clone(),
                                    caller_name.clone(),
                                    val.clone(),
                                ));
                            }
                        }
                        func_env.define(key.clone(), val);
                        bound_params.insert(key.clone());
                    } else if kwargs_param.is_some() {
                        // Otherwise, add to **kwargs dict
                        extra_kwargs.insert(key.clone(), val);
                    } else {
                        return Err(anyhow!("Unexpected keyword argument: {}", key));
                    }
                }

                // Process **kwargs unpacking
                let mut mapping_keys = HashSet::new();
                for dict_expr in kwargs_unpack {
                    let raw = self.eval_expression(dict_expr)?;
                    let immutable_mapping = matches!(&raw, Value::ImmutableMapping(_));
                    let dict_val = dictionary_expansion_value(raw);
                    match dict_val {
                        Value::Dict(dict_map) => {
                            for (key, val) in dict_map {
                                if immutable_mapping {
                                    if !matches!(dictionary_key_value(&key), Value::String(_)) {
                                        anyhow::bail!("keyword argument names must be strings");
                                    }
                                    if bound_params.contains(&key) || extra_kwargs.contains_key(&key) {
                                        anyhow::bail!("multiple values for keyword argument '{key}'");
                                    }
                                    mapping_keys.insert(key.clone());
                                } else if mapping_keys.contains(&key)
                                    && (bound_params.contains(&key) || extra_kwargs.contains_key(&key)) {
                                    anyhow::bail!("multiple values for keyword argument '{key}'");
                                }
                                // If key matches a named parameter, bind it directly
                                if param_names.contains(&key.as_str()) {
                                    func_env.define(key.clone(), val);
                                    bound_params.insert(key);
                                } else if kwargs_param.is_some() {
                                    // Otherwise, add to **kwargs dict
                                    extra_kwargs.insert(key, val);
                                } else {
                                    return Err(anyhow!("Unexpected keyword argument: {}", key));
                                }
                            }
                        }
                        _ => {
                            return Err(anyhow!(
                                "Cannot unpack non-dict value in function call: {:?}",
                                dict_val
                            ))
                        }
                    }
                }

                // Bind the values captured at definition. Cloning a Value
                // keeps the existing list/dict ownership model; this timing
                // repair does not introduce shared mutable container identity.
                for (param_name, default) in &params {
                    if !bound_params.contains(param_name) {
                        if let Some(value) = default {
                            func_env.define(param_name.clone(), value.clone());
                        } else {
                            return Err(anyhow!("Missing required parameter: {}", param_name));
                        }
                    }
                }

                // Bind **kwargs parameter if present
                if let Some(kwargs_name) = kwargs_param {
                    func_env.define(kwargs_name, Value::Dict(extra_kwargs));
                }

                // Save current environment
                let saved_env = std::mem::replace(&mut self.env, func_env);

                let result = (|| -> Result<Value> {
                    for stmt in body.iter() {
                        self.eval_statement(stmt)?;
                        if self.return_value.is_some() {
                            break;
                        }
                    }
                    Ok(self.return_value.take().unwrap_or(Value::None))
                })();
                let alias_updates: Vec<_> = mutable_aliases
                    .into_iter()
                    .filter(|_| result.is_ok())
                    .filter_map(|(param_name, caller_name, original)| {
                        self.env.get(&param_name).and_then(|updated| {
                            (!same_value_representation(&updated, &original)).then_some((caller_name, updated))
                        })
                    })
                    .collect();

                // Restore environment
                self.env = saved_env;
                for (caller_name, updated) in alias_updates {
                    self.env.set(&caller_name, updated)?;
                }

                result
            }
            Value::ProjectionRule(candidate) => {
                let eval_args = merge_args(args, args_unpack, self)?;
                let eval_kwargs = merge_kwargs(kwargs, kwargs_unpack, self)?;
                let declaration = candidate.resolve_call(self.bzl_loader.as_ref(), &eval_args, &eval_kwargs);
                let rule_call = RuleCall {
                    rule_type: "CustomRule".into(), args: eval_args, kwargs: eval_kwargs,
                    cc_projection: Some(declaration),
                };
                if let Some(callback) = &self.rule_call_callback { callback(&rule_call); }
                self.rule_calls.push(rule_call);
                Ok(Value::None)
            }
            Value::AttributeDefinition(_) => Err(anyhow!("attribute definitions are not callable")),
            Value::RuleFunction(rule_type) => {
                // This is a rule function call (like native.cc_library)
                // Evaluate arguments and capture the rule call
                // IMPORTANT: Use merge_args/merge_kwargs to handle **kwargs unpacking
                let eval_args = merge_args(args, args_unpack, self)?;
                let mut eval_kwargs = merge_kwargs(kwargs, kwargs_unpack, self)?;
                for value in eval_kwargs.values_mut() {
                    if matches!(value, Value::ImmutableMapping(view) if view.kind == ImmutableMappingKind::FrozenDict) {
                        *value = dictionary_expansion_value(value.clone());
                    }
                }
                Self::validate_loading_time_rule_attributes(&rule_type, &eval_kwargs)?;

                // Concrete custom-rule identities are encoded on RuleFunction so
                // they survive load aliases, conditionals, and wrapper functions.
                // Route them through the existing custom-rule registry while
                // retaining the kind as an internal attribute for specialization.
                let mut rule_type = rule_type;
                let is_build_setting = rule_type.starts_with("build_setting:");
                if is_build_setting {
                    rule_type.replace_range(.."build_setting:".len(), "");
                }
                if is_build_setting {
                    eval_kwargs.insert(
                        "__rush_build_setting".to_string(),
                        Value::Bool(true),
                    );
                }
                let rule_type = if let Some(kind) = rule_type.strip_prefix("custom_rule_stub:") {
                    eval_kwargs.insert(
                        "__rush_custom_rule_kind".to_string(),
                        Value::String(kind.to_string()),
                    );
                    "CustomRule".to_string()
                } else {
                    rule_type
                };

                // Capture the rule call
                let rule_call = RuleCall {
                    cc_projection: None,
                    rule_type,
                    args: eval_args,
                    kwargs: eval_kwargs,
                };

                // Invoke callback if registered
                if let Some(callback) = &self.rule_call_callback {
                    callback(&rule_call);
                }

                self.maybe_activate_config_setting(&rule_call);
                self.rule_calls.push(rule_call);

                Ok(Value::None)
            }
            Value::NativeFunction(func_name) => {
                if func_name == "repository_name" {
                    let args = merge_args(args, args_unpack, self)?;
                    let kwargs = merge_kwargs(kwargs, kwargs_unpack, self)?;
                    if !args.is_empty() || !kwargs.is_empty() {
                        anyhow::bail!("native.repository_name() accepts no arguments");
                    }
                    // This is the BUILD caller, independently of a loaded
                    // function's definition Label context or physical root.
                    // A module initializer/WORKSPACE has no BUILD package.
                    if self.current_package_dir.is_none() {
                        anyhow::bail!("native.repository_name() requires a BUILD loading context; it is unavailable during module initialization, WORKSPACE evaluation, or rule analysis");
                    }
                    return Ok(Value::String(format!("@{}", self.current_repository.as_deref().unwrap_or("").trim_start_matches('@'))));
                }
                if func_name == "module_extension" {
                    let args = merge_args(args, args_unpack, self)?;
                    let kwargs = extension_kwargs(kwargs, kwargs_unpack, self)?;
                    return self.builtins.get("module_extension")
                        .context("module_extension constructor is unavailable")?(&args, &kwargs);
                }
                if func_name == "Label" {
                    let mut eval_args = merge_args(args, args_unpack, self)?;
                    self.qualify_label_argument(&mut eval_args)?;
                    let eval_kwargs = merge_kwargs(kwargs, kwargs_unpack, self)?
                        .into_iter().collect::<Vec<_>>();
                    return self.builtins.get("Label")
                        .context("Label builtin is unavailable")?(&eval_args, &eval_kwargs);
                }
                // Existing-rule lookup honors ordinary star argument calls;
                // preserve legacy dispatch behavior for unrelated native APIs.
                let lookup = matches!(func_name.as_str(), "existing_rule" | "existing_rules" | "rule");
                let mut eval_args = Vec::new();
                if lookup { eval_args = merge_args(args, args_unpack, self)?; }
                else { for arg in args { eval_args.push(self.eval_expression(arg)?); } }
                let mut eval_kwargs = HashMap::new();
                if lookup { eval_kwargs = merge_kwargs(kwargs, kwargs_unpack, self)?; }
                else { for (key, expr) in kwargs { eval_kwargs.insert(key.clone(), self.eval_expression(expr)?); } }

                // Handle specific native functions
                match func_name.as_str() {
                    "rule" => {
                        if let Some(candidate) = crate::cc_projection_definition::ProjectionRuleDefinition::candidate(&eval_kwargs, self.env.cc_projection_origin.clone()) {
                            return Ok(Value::ProjectionRule(candidate));
                        }
                        let values = eval_kwargs.into_iter().collect::<Vec<_>>();
                        return self.builtins.get("rule").context("rule builtin missing")?(&eval_args, &values);
                    }
                    // Starlark json module (real, via serde_json).
                    "json.encode" => {
                        let v = eval_args.first().cloned().unwrap_or(Value::None);
                        Ok(Value::String(value_to_json(&v)?.to_string()))
                    }
                    "json.encode_indent" => {
                        let v = eval_args.first().cloned().unwrap_or(Value::None);
                        let j = value_to_json(&v)?;
                        Ok(Value::String(
                            serde_json::to_string_pretty(&j).unwrap_or_else(|_| j.to_string()),
                        ))
                    }
                    "json.decode" => {
                        let s = match eval_args.first() {
                            Some(Value::String(s)) => s.clone(),
                            _ => {
                                return Err(anyhow!("json.decode() requires a string argument"))
                            }
                        };
                        let j: serde_json::Value = serde_json::from_str(&s)
                            .map_err(|e| anyhow!("json.decode: invalid JSON: {}", e))?;
                        Ok(json_to_value(&j))
                    }
                    // Identity wrapper used by TensorFlow to provide mirrored URLs
                    // We normalize to a list of strings for downstream consumption.
                    "set" => {
                        // Starlark set(iterable): deduplicate preserving
                        // insertion order. Represented as a list; the
                        // operations exercised by real BUILD graphs so far
                        // (iteration, sorted(), len(), membership) all work
                        // on the list representation.
                        let mut seen = Vec::new();
                        if let Some(first) = eval_args.first() {
                            let items = match first {
                                Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => items.clone(),
                                Value::Dict(map) => {
                                    sorted_dictionary_key_values(map)
                                }
                                _ => {
                                    return Err(anyhow!(
                                        "set() argument must be iterable"
                                    ))
                                }
                            };
                            for item in items {
                                if !seen.contains(&item) {
                                    seen.push(item);
                                }
                            }
                        }
                        return Ok(Value::List(seen));
                    }
                    "tf_mirror_urls" => {
                        if eval_args.is_empty() {
                            return Ok(Value::List(Vec::new()));
                        }
                        let first = &eval_args[0];
                        let list = match first {
                            Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => items.clone(),
                            Value::String(s) => vec![Value::String(s.clone())],
                            _ => Vec::new(),
                        };
                        Ok(Value::List(list))
                    }
                    "glob" => {
                        // Convert HashMap to Vec for the builtin signature.
                        let kwargs_vec: Vec<(String, Value)> =
                            eval_kwargs.into_iter().collect();
                        // Use the absolute current package directory for main and
                        // external repositories, matching bare glob above.
                        if let Some(dir) = self.current_package_dir.as_ref() {
                            crate::builtins::glob_in_package(
                                &eval_args,
                                &kwargs_vec,
                                &dir.to_string_lossy(),
                            )
                        } else if self.current_repository.is_none()
                            && !self.current_package.is_empty()
                        {
                            crate::builtins::glob_in_package(
                                &eval_args,
                                &kwargs_vec,
                                &self.current_package,
                            )
                        } else if let Some(glob_fn) = self.builtins.get("glob") {
                            glob_fn(&eval_args, &kwargs_vec)
                        } else {
                            Err(anyhow!("glob builtin not found"))
                        }
                    }
                    "existing_rule" => {
                        if eval_args.len() != 1 || !eval_kwargs.is_empty() {
                            anyhow::bail!("native.existing_rule() requires exactly one positional string name");
                        }
                        let Value::String(name) = &eval_args[0] else {
                            anyhow::bail!("native.existing_rule() name must be a string");
                        };
                        // Exact current caller-package name, not a label. The
                        // latest declaration matches the existing plural map.
                        Ok(self.rule_calls.iter().rev().find(|call| {
                            call.rule_type != "source_file"
                                && call.kwargs.get("name") == Some(&Value::String(name.clone()))
                        }).map(|call| self.existing_rule_view(call)).unwrap_or(Value::None))
                    }
                    "existing_rules" => {
                        if !eval_args.is_empty() || !eval_kwargs.is_empty() {
                            anyhow::bail!("native.existing_rules() accepts no arguments");
                        }
                        let mut entries = HashMap::new();
                        for call in &self.rule_calls {
                            if call.rule_type == "source_file" { continue; }
                            if let Some(Value::String(name)) = call.kwargs.get("name") {
                                entries.insert(name.clone(), self.existing_rule_view(call));
                            }
                        }
                        Ok(Value::ImmutableMapping(Arc::new(ImmutableMapping {
                            entries, kind: ImmutableMappingKind::RulesView,
                        })))
                    }
                    "exports_files" => {
                        let visibility = exports_files_visibility(&eval_kwargs)?;
                        for file in exports_files_sources(&eval_args, &eval_kwargs)? {
                            self.push_source_file_rule_call(file, visibility.clone())?;
                        }
                        Ok(Value::None)
                    }
                    "package" => {
                        self.record_package_metadata(&eval_args, &eval_kwargs)?;
                        Ok(Value::None)
                    }
                    "bind" => {
                        // WORKSPACE function: bind(name, actual)
                        // For now, just return None (metadata-only)
                        Ok(Value::None)
                    }
                    "register_toolchains" => {
                        // WORKSPACE function: register_toolchains(toolchain_labels...)
                        // For now, just return None (metadata-only)
                        Ok(Value::None)
                    }
                    "register_execution_platforms" => {
                        // WORKSPACE function: register_execution_platforms(platform_labels...)
                        // For now, just return None (metadata-only)
                        Ok(Value::None)
                    }
                    "package_name" => {
                        // Return the current package path
                        // This is set by the build orchestrator when evaluating BUILD files
                        Ok(Value::String(self.current_package.clone()))
                    }
                    "package_relative_label" => {
                        // This native operation uses the current BUILD caller,
                        // independently of a function's defining Label context.
                        let input = match eval_args.first() {
                            Some(Value::String(input)) => input.clone(),
                            Some(label @ Value::Label { .. }) => return Ok(label.clone()),
                            other => {
                                return Err(anyhow!(
                                    "package_relative_label() requires a label string, got {:?}",
                                    other
                                ))
                            }
                        };
                        let owner = self.current_repository.as_deref()
                            .unwrap_or("").trim_start_matches('@');
                        let full = if input.starts_with("@@") {
                            input
                        } else if let Some(external) = input.strip_prefix('@') {
                            let (repository, target) = external.split_once("//").ok_or_else(|| {
                                anyhow!("package_relative_label() repository reference must contain //: {}", input)
                            })?;
                            let canonical = if repository.is_empty() {
                                String::new()
                            } else if let Some(loader) = &self.bzl_loader {
                                let context = loader.lock()
                                    .map_err(|error| anyhow!("Failed to lock package label context: {error}"))?
                                    .definition_label_context_for_owner(
                                        self.current_repository.as_deref(), &self.current_package,
                                    )?;
                                match context.repository_mapping.get(repository) {
                                    Some(mapped) => mapped.clone(),
                                    None if context.mapping_is_complete => {
                                        anyhow::bail!("Repository '@{}' is not visible from BUILD repository '{}'", repository, owner);
                                    }
                                    None => repository.to_string(),
                                }
                            } else {
                                repository.to_string()
                            };
                            format!("@@{canonical}//{target}")
                        } else if input.starts_with("//") {
                            format!("@@{owner}{input}")
                        } else {
                            let name = input.strip_prefix(':').unwrap_or(&input);
                            format!("@@{owner}//{}:{}", self.current_package, name)
                        };
                        self.builtins.get("Label").context("Label builtin is unavailable")?(
                            &[Value::String(full)], &[],
                        )
                    }
                    "dir" => {
                        // Starlark dir(): the value's field names, sorted.
                        // Struct-like values (provider instances, namespaces,
                        // dicts-as-structs) report their fields; scalar types
                        // report no fields (rush does not model the built-in
                        // method tables as attributes).
                        let mut names: Vec<String> = match eval_args.first() {
                            Some(Value::ProviderInstance { fields, .. }) => {
                                fields.keys().cloned().collect()
                            }
                            Some(Value::Namespace(map)) => map.keys().cloned().collect(),
                            Some(Value::WorkspaceNativeNamespace { attrs, .. }) => {
                                let mut fields: Vec<_> = attrs.keys().cloned().collect();
                                fields.push("bazel_version".to_string());
                                fields
                            }
                            Some(Value::ImmutableMapping(view)) => {
                                let mut methods = vec!["get", "items", "keys", "values"];
                                if view.kind == ImmutableMappingKind::FrozenDict {
                                    methods.extend(["clear", "pop", "popitem", "setdefault", "update"]);
                                }
                                methods.into_iter().map(str::to_string).collect()
                            },
                            Some(Value::Dict(map)) => sorted_dictionary_key_values(map)
                                .into_iter()
                                .filter_map(|key| match key {
                                    Value::String(key) => Some(key),
                                    _ => None,
                                })
                                .collect(),
                            _ => Vec::new(),
                        };
                        names.sort();
                        Ok(Value::List(names.into_iter().map(Value::String).collect()))
                    }
                    // TensorFlow helper functions (from tensorflow.bzl)
                    "get_compatible_with_portable" => {
                        // Returns list of compatible platforms for portable builds
                        // For now, return empty list (all platforms compatible)
                        Ok(Value::List(vec![]))
                    }
                    "tensorflow_opensource_extra_deps" => {
                        // Returns list of extra dependencies for TensorFlow opensource builds
                        // For now, return empty list
                        Ok(Value::List(vec![]))
                    }
                    "tf_monitoring_framework_deps" => {
                        // Returns list of monitoring framework dependencies
                        // For now, return empty list
                        Ok(Value::List(vec![]))
                    }
                    "tf_selective_registration_deps" => {
                        // Returns list of selective registration dependencies
                        // For now, return empty list
                        Ok(Value::List(vec![]))
                    }
                    "cc_header_only_library" => {
                        // TensorFlow macro that creates header-only cc_library targets.
                        // Some BUILD files import this symbol from tensorflow.default.bzl,
                        // which shadows the registered rule_function hook with a native
                        // binding. To keep the macro working, expand it here and capture
                        // the generated rule call(s).
                        if let Some(expander) =
                            self.macro_expanders.get("cc_header_only_library").copied()
                        {
                            let expanded_rules =
                                expander("cc_header_only_library", &eval_args, &eval_kwargs)?;
                            for rule in expanded_rules {
                                self.rule_calls.push(rule);
                            }
                            return Ok(Value::None);
                        }

                        // Fallback: treat as a direct cc_header_only_library rule call
                        self.rule_calls.push(RuleCall {
                            cc_projection: None,
                            rule_type: "cc_header_only_library".to_string(),
                            args: eval_args,
                            kwargs: eval_kwargs,
                        });
                        Ok(Value::None)
                    }
                    // attr.* attribute descriptor functions
                    name if name.starts_with("attr.") => {
                        if name == "attr.label_list" {
                            if !eval_args.is_empty() { anyhow::bail!("attr.label_list accepts keyword arguments only"); }
                            return Ok(Value::AttributeDefinition(Arc::new(crate::cc_projection_definition::AttributeDefinition::label_list(eval_kwargs))));
                        }
                        // Create an attribute descriptor as a dict with the provided kwargs
                        // In real Bazel, these return Attribute objects, but for simplicity
                        // we represent them as dicts with metadata
                        let mut descriptor = HashMap::new();
                        descriptor.insert("_type".to_string(), Value::String(name.to_string()));

                        // Add all kwargs to the descriptor
                        for (key, value) in eval_kwargs {
                            descriptor.insert(key, value);
                        }

                        Ok(Value::Dict(descriptor))
                    }
                    _ => {
                        // Check if this is a builtin function
                        if let Some(builtin_fn) = self.builtins.get(&func_name) {
                            // Convert HashMap to Vec<(String, Value)> for builtin function signature
                            let kwargs_vec: Vec<(String, Value)> =
                                eval_kwargs.into_iter().collect();
                            builtin_fn(&eval_args, &kwargs_vec)
                        } else {
                            // Unknown native function - just return None to avoid breaking evaluation
                            debug_log!(
                                "Warning: Unknown native function '{}', treating as metadata-only",
                                func_name
                            );
                            Ok(Value::None)
                        }
                    }
                }
            }
            Value::Provider { name, fields, init } => {
                // Native and Starlark initializers must return the field
                // dictionary used to construct the provider instance.
                let encoded_instance_fields = if let Some(init) = init {
                    let init_result = match *init {
                        Value::NativeFunction(initializer) => {
                            let native = self.builtins.get(&initializer).copied().ok_or_else(|| {
                                anyhow!("Unknown native provider initializer: {}", initializer)
                            })?;
                            let eval_args = merge_args(args, args_unpack, self)?;
                            let mut named = HashMap::new();
                            for (key, expression) in kwargs {
                                let value = self.eval_expression(expression)?;
                                if named.insert(key.clone(), value).is_some() {
                                    return Err(anyhow!(
                                        "Provider {} received multiple values for argument '{}'",
                                        name, key
                                    ));
                                }
                            }
                            for expression in kwargs_unpack {
                                let Value::Dict(values) = dictionary_expansion_value(self.eval_expression(expression)?) else {
                                    return Err(anyhow!(
                                        "Provider {} cannot unpack a non-dict argument", name
                                    ));
                                };
                                for (key, value) in values {
                                    let Value::String(key) = dictionary_key_value(&key) else {
                                        return Err(anyhow!(
                                            "Provider {} argument names must be strings", name
                                        ));
                                    };
                                    if named.insert(key.clone(), value).is_some() {
                                        return Err(anyhow!(
                                            "Provider {} received multiple values for argument '{}'",
                                            name, key
                                        ));
                                    }
                                }
                            }
                            let eval_kwargs: Vec<_> = named.into_iter().collect();
                            native(&eval_args, &eval_kwargs)
                        }
                        init => {
                            const INIT_BINDING: &str = "__rush_provider_initializer__";
                            let saved_env = self.env.clone();
                            self.env.define(INIT_BINDING.to_string(), init);
                            let init_result = self.eval_call(
                                &Expr::Ident(INIT_BINDING.to_string()),
                                args,
                                args_unpack,
                                kwargs,
                                kwargs_unpack,
                            );
                            self.env = saved_env;
                            init_result
                        }
                    };

                    match init_result? {
                        Value::Dict(values) => values,
                        other => {
                            return Err(anyhow!(
                                "Provider {} initializer must return a dict, got {:?}",
                                name,
                                other
                            ))
                        }
                    }
                } else {
                    // Raw provider constructors accept keyword arguments only.
                    if !args.is_empty() || !args_unpack.is_empty() {
                        return Err(anyhow!(
                            "Provider {} raw constructor accepts keyword arguments only",
                            name
                        ));
                    }
                    merge_kwargs(kwargs, kwargs_unpack, self)?
                };
                let mut instance_fields = HashMap::new();
                for (key, value) in encoded_instance_fields {
                    let Value::String(key) = dictionary_key_value(&key) else {
                        return Err(anyhow!("Provider {} field names must be strings", name));
                    };
                    instance_fields.insert(key, value);
                }

                // Declared provider fields are an allowlist, not a list of
                // mandatory constructor arguments. Omitted fields remain
                // absent from the instance.
                if !fields.is_empty() {
                    for key in instance_fields.keys() {
                        if !fields.contains(key) {
                            return Err(anyhow!("Provider {} has no field '{}'", name, key));
                        }
                    }
                }

                Ok(Value::ProviderInstance {
                    provider_name: name,
                    fields: instance_fields,
                })
            }
            Value::BoundMethod { object, method } => {
                // Call a bound method (e.g., "hello".split(","))
                match object.as_ref() {
                    Value::String(s) => {
                        if method == "elems" {
                            let positional = merge_args(args, args_unpack, self)?;
                            let named = merge_kwargs(kwargs, kwargs_unpack, self)?;
                            if !positional.is_empty() || !named.is_empty() {
                                anyhow::bail!("string.elems() accepts no arguments");
                            }
                            // Bazel 6.5 exposes individual source-byte elements.
                            // Value::String currently stores UTF-8 text, so a
                            // non-ASCII byte element cannot be represented
                            // faithfully. Refuse instead of replacing bytes or
                            // silently iterating Unicode scalar values.
                            if !s.is_ascii() {
                                anyhow::bail!("string.elems() does not yet support non-ASCII byte elements with Rush's UTF-8 string representation");
                            }
                            return Ok(Value::FrozenList(s.bytes().map(|byte|
                                Value::String(char::from(byte).to_string())
                            ).collect()));
                        }
                        // Evaluate arguments
                        let mut eval_args = Vec::new();
                        for arg in args {
                            eval_args.push(self.eval_expression(arg)?);
                        }

                        // Execute string methods
                        match method.as_str() {
                            "split" => {
                                let separator = if eval_args.is_empty() {
                                    " " // Default separator is whitespace
                                } else if let Value::String(sep) = &eval_args[0] {
                                    sep.as_str()
                                } else {
                                    return Err(anyhow!("split() argument must be a string"));
                                };

                                let parts: Vec<Value> = s
                                    .split(separator)
                                    .map(|part| Value::String(part.to_string()))
                                    .collect();
                                Ok(Value::List(parts))
                            }
                            "rsplit" => {
                                // FIX #191: rsplit() - split from right (same as Python's rsplit)
                                let separator = if eval_args.is_empty() {
                                    " " // Default separator is whitespace
                                } else if let Value::String(sep) = &eval_args[0] {
                                    sep.as_str()
                                } else {
                                    return Err(anyhow!("rsplit() argument must be a string"));
                                };

                                let parts: Vec<Value> = s
                                    .rsplit(separator)
                                    .map(|part| Value::String(part.to_string()))
                                    .collect();
                                // rsplit returns parts in reverse order, so reverse them back
                                let mut reversed_parts = parts;
                                reversed_parts.reverse();
                                Ok(Value::List(reversed_parts))
                            }
                            "removeprefix" => {
                                // FIX #196: removeprefix() - remove prefix if present, otherwise return original
                                if eval_args.is_empty() {
                                    return Err(anyhow!("removeprefix() requires 1 argument"));
                                }
                                let prefix = match &eval_args[0] {
                                    Value::String(p) => p.as_str(),
                                    _ => {
                                        return Err(anyhow!(
                                            "removeprefix() argument must be a string"
                                        ))
                                    }
                                };

                                let result = if let Some(stripped) = s.strip_prefix(prefix) {
                                    stripped.to_string()
                                } else {
                                    s.to_string()
                                };
                                Ok(Value::String(result))
                            }
                            "removesuffix" => {
                                // FIX #196: removesuffix() - remove suffix if present, otherwise return original
                                if eval_args.is_empty() {
                                    return Err(anyhow!("removesuffix() requires 1 argument"));
                                }
                                let suffix = match &eval_args[0] {
                                    Value::String(s) => s.as_str(),
                                    _ => {
                                        return Err(anyhow!(
                                            "removesuffix() argument must be a string"
                                        ))
                                    }
                                };

                                let result = if let Some(stripped) = s.strip_suffix(suffix) {
                                    stripped.to_string()
                                } else {
                                    s.to_string()
                                };
                                Ok(Value::String(result))
                            }
                            "strip" => Ok(Value::String(s.trim().to_string())),
                            "lstrip" => Ok(Value::String(s.trim_start().to_string())),
                            "rstrip" => Ok(Value::String(s.trim_end().to_string())),
                            "replace" => {
                                if eval_args.len() < 2 {
                                    return Err(anyhow!("replace() requires 2 arguments"));
                                }
                                let old = match &eval_args[0] {
                                    Value::String(o) => o,
                                    _ => {
                                        return Err(anyhow!(
                                            "replace() first argument must be a string"
                                        ))
                                    }
                                };
                                let new = match &eval_args[1] {
                                    Value::String(n) => n,
                                    _ => {
                                        return Err(anyhow!(
                                            "replace() second argument must be a string"
                                        ))
                                    }
                                };
                                Ok(Value::String(s.replace(old, new)))
                            }
                            "startswith" => {
                                if eval_args.is_empty() {
                                    return Err(anyhow!("startswith() requires an argument"));
                                }
                                let result = match &eval_args[0] {
                                    Value::String(prefix) => s.starts_with(prefix),
                                    Value::Tuple(prefixes) => prefixes.iter().try_fold(
                                        false,
                                        |matched, prefix| match prefix {
                                            Value::String(prefix) => {
                                                Ok(matched || s.starts_with(prefix))
                                            }
                                            _ => Err(anyhow!(
                                                "startswith() prefixes must be strings"
                                            )),
                                        },
                                    )?,
                                    _ => {
                                        return Err(anyhow!(
                                            "startswith() argument must be a string or tuple"
                                        ));
                                    }
                                };
                                Ok(Value::Bool(result))
                            }
                            "endswith" => {
                                if eval_args.is_empty() {
                                    return Err(anyhow!("endswith() requires an argument"));
                                }
                                let result = match &eval_args[0] {
                                    Value::String(suffix) => s.ends_with(suffix),
                                    Value::Tuple(suffixes) => suffixes.iter().try_fold(
                                        false,
                                        |matched, suffix| match suffix {
                                            Value::String(suffix) => {
                                                Ok(matched || s.ends_with(suffix))
                                            }
                                            _ => Err(anyhow!(
                                                "endswith() suffixes must be strings"
                                            )),
                                        },
                                    )?,
                                    _ => {
                                        return Err(anyhow!(
                                            "endswith() argument must be a string or tuple"
                                        ));
                                    }
                                };
                                Ok(Value::Bool(result))
                            }
                            "upper" => Ok(Value::String(s.to_uppercase())),
                            "lower" => Ok(Value::String(s.to_lowercase())),
                            // Starlark string predicates (non-empty + all chars match).
                            // bazel_features' parse.bzl uses s.isdigit(); the siblings
                            // are added together to avoid repeated round-trips.
                            "isdigit" => Ok(Value::Bool(
                                !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()),
                            )),
                            "isalpha" => Ok(Value::Bool(
                                !s.is_empty() && s.chars().all(|c| c.is_alphabetic()),
                            )),
                            "isalnum" => Ok(Value::Bool(
                                !s.is_empty() && s.chars().all(|c| c.is_alphanumeric()),
                            )),
                            "isspace" => Ok(Value::Bool(
                                !s.is_empty() && s.chars().all(|c| c.is_whitespace()),
                            )),
                            "islower" => Ok(Value::Bool(
                                s.chars().any(|c| c.is_lowercase())
                                    && !s.chars().any(|c| c.is_uppercase()),
                            )),
                            "isupper" => Ok(Value::Bool(
                                s.chars().any(|c| c.is_uppercase())
                                    && !s.chars().any(|c| c.is_lowercase()),
                            )),
                            "title" => {
                                // Capitalize the first letter of each run of letters.
                                let mut out = String::with_capacity(s.len());
                                let mut prev_cased = false;
                                for c in s.chars() {
                                    if c.is_alphabetic() {
                                        if prev_cased {
                                            out.extend(c.to_lowercase());
                                        } else {
                                            out.extend(c.to_uppercase());
                                        }
                                        prev_cased = true;
                                    } else {
                                        out.push(c);
                                        prev_cased = false;
                                    }
                                }
                                Ok(Value::String(out))
                            }
                            "capitalize" => {
                                let mut chars = s.chars();
                                match chars.next() {
                                    None => Ok(Value::String(String::new())),
                                    Some(first) => {
                                        let capitalized = first
                                            .to_uppercase()
                                            .chain(chars.as_str().to_lowercase().chars())
                                            .collect();
                                        Ok(Value::String(capitalized))
                                    }
                                }
                            }
                            "count" => {
                                if eval_args.is_empty() {
                                    return Err(anyhow!("count() requires an argument"));
                                }
                                let substring = match &eval_args[0] {
                                    Value::String(sub) => sub,
                                    _ => return Err(anyhow!("count() argument must be a string")),
                                };
                                let count = s.matches(substring).count() as i64;
                                Ok(Value::Int(count))
                            }
                            "find" => {
                                if eval_args.is_empty() {
                                    return Err(anyhow!("find() requires an argument"));
                                }
                                let substring = match &eval_args[0] {
                                    Value::String(sub) => sub,
                                    _ => return Err(anyhow!("find() argument must be a string")),
                                };
                                let result = s.find(substring).map(|i| i as i64).unwrap_or(-1);
                                Ok(Value::Int(result))
                            }
                            "rfind" => {
                                if eval_args.is_empty() {
                                    return Err(anyhow!("rfind() requires an argument"));
                                }
                                let substring = match &eval_args[0] {
                                    Value::String(sub) => sub,
                                    _ => return Err(anyhow!("rfind() argument must be a string")),
                                };
                                let result = s.rfind(substring).map(|i| i as i64).unwrap_or(-1);
                                Ok(Value::Int(result))
                            }
                            "partition" => {
                                // partition(): 3-tuple (before, separator, after) on the
                                // FIRST occurrence; if not found, (original, "", "").
                                // (bazel_features' parse.bzl uses v.partition(" ").)
                                if eval_args.is_empty() {
                                    return Err(anyhow!("partition() requires an argument"));
                                }
                                let separator = match &eval_args[0] {
                                    Value::String(sep) => sep,
                                    _ => {
                                        return Err(anyhow!(
                                            "partition() argument must be a string"
                                        ))
                                    }
                                };
                                if let Some(pos) = s.find(separator) {
                                    let before = &s[..pos];
                                    let after = &s[pos + separator.len()..];
                                    Ok(Value::Tuple(vec![
                                        Value::String(before.to_string()),
                                        Value::String(separator.clone()),
                                        Value::String(after.to_string()),
                                    ]))
                                } else {
                                    Ok(Value::Tuple(vec![
                                        Value::String(s.clone()),
                                        Value::String(String::new()),
                                        Value::String(String::new()),
                                    ]))
                                }
                            }
                            "rpartition" => {
                                // FIX #186: Implement rpartition() for string splitting
                                // Returns a 3-tuple: (before, separator, after) based on LAST occurrence
                                if eval_args.is_empty() {
                                    return Err(anyhow!("rpartition() requires an argument"));
                                }
                                let separator = match &eval_args[0] {
                                    Value::String(sep) => sep,
                                    _ => {
                                        return Err(anyhow!(
                                            "rpartition() argument must be a string"
                                        ))
                                    }
                                };

                                // Find the last occurrence of the separator
                                if let Some(pos) = s.rfind(separator) {
                                    // Separator found - split into (before, sep, after)
                                    let before = &s[..pos];
                                    let after = &s[pos + separator.len()..];
                                    Ok(Value::Tuple(vec![
                                        Value::String(before.to_string()),
                                        Value::String(separator.clone()),
                                        Value::String(after.to_string()),
                                    ]))
                                } else {
                                    // Separator not found - return ("", "", original_string)
                                    Ok(Value::Tuple(vec![
                                        Value::String(String::new()),
                                        Value::String(String::new()),
                                        Value::String(s.clone()),
                                    ]))
                                }
                            }
                            "join" => {
                                if eval_args.is_empty() {
                                    return Err(anyhow!("join() requires an argument"));
                                }
                                let items = match &eval_args[0] {
                                    Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => items,
                                    _ => return Err(anyhow!("join() argument must be a list")),
                                };
                                let strings: Result<Vec<String>> = items
                                    .iter()
                                    .map(|v| match v {
                                        Value::String(s) => Ok(s.clone()),
                                        _ => Err(anyhow!("join() list must contain only strings")),
                                    })
                                    .collect();
                                Ok(Value::String(strings?.join(s)))
                            }
                            "format" => {
                                // Python/Starlark string formatting supports both:
                                // 1. Positional: "Hello {}".format("World")
                                // 2. Named: "Hello {name}".format(name="World")

                                // Evaluate positional args
                                let mut positional_args = Vec::new();
                                for arg in args {
                                    positional_args.push(self.eval_expression(arg)?);
                                }

                                // Evaluate kwargs
                                let mut format_map = std::collections::HashMap::new();
                                for (key, val_expr) in kwargs {
                                    let val = self.eval_expression(val_expr)?;
                                    format_map.insert(key.clone(), val);
                                }

                                // Replace {placeholders} with values
                                let result = s.clone();
                                let chars: Vec<char> = result.chars().collect();
                                let mut i = 0;
                                let mut output = String::new();
                                let mut positional_index = 0;
                                // Starlark forbids mixing automatic {} with
                                // manual {0} numbering in one format string;
                                // silently accepting it produces output Bazel
                                // would reject.
                                let mut used_auto = false;
                                let mut used_manual = false;

                                while i < chars.len() {
                                    // "{{" and "}}" are escaped literal braces.
                                    if chars[i] == '{'
                                        && chars.get(i + 1) == Some(&'{')
                                    {
                                        output.push('{');
                                        i += 2;
                                        continue;
                                    }
                                    if chars[i] == '}'
                                        && chars.get(i + 1) == Some(&'}')
                                    {
                                        output.push('}');
                                        i += 2;
                                        continue;
                                    }
                                    if chars[i] == '{' {
                                        // Find matching '}'
                                        if let Some(close_pos) =
                                            chars[i + 1..].iter().position(|&c| c == '}')
                                        {
                                            let placeholder_end = i + 1 + close_pos;
                                            let placeholder: String =
                                                chars[i + 1..placeholder_end].iter().collect();

                                            // Three placeholder forms: {} takes the
                                            // next positional, {0} takes an explicit
                                            // positional, {name} takes a keyword.
                                            let value = if placeholder.is_empty() {
                                                // Empty placeholder {} - use next positional arg
                                                used_auto = true;
                                                if used_manual {
                                                    return Err(anyhow!(
                                                        "cannot switch from manual field specification to automatic field numbering"
                                                    ));
                                                }
                                                if positional_index < positional_args.len() {
                                                    let val = &positional_args[positional_index];
                                                    positional_index += 1;
                                                    Some(val.clone())
                                                } else {
                                                    return Err(anyhow!(
                                                        "Not enough positional arguments for format string"
                                                    ));
                                                }
                                            } else if placeholder.bytes().all(|b| b.is_ascii_digit()) {
                                                // All-digit field: a manual index. Parsed
                                                // separately from the digit test so an index
                                                // too large for usize reports out-of-range
                                                // instead of falling through to the keyword
                                                // branch and claiming a keyword is missing.
                                                used_manual = true;
                                                if used_auto {
                                                    return Err(anyhow!(
                                                        "cannot switch from automatic field numbering to manual field specification"
                                                    ));
                                                }
                                                let idx = placeholder.parse::<usize>().ok();
                                                if let Some(idx) = idx {
                                                // Explicit index {0}. Report the range
                                                // rather than calling it a missing
                                                // keyword, which is what the old code
                                                // did and what made TF's
                                                // "{0}.test".format(name) read as an
                                                // undefined name.
                                                    match positional_args.get(idx) {
                                                        Some(val) => Some(val.clone()),
                                                        None => {
                                                            return Err(anyhow!(
                                                                "format index {} out of range: {} positional argument(s) given",
                                                                idx,
                                                                positional_args.len()
                                                            ))
                                                        }
                                                    }
                                                } else {
                                                    return Err(anyhow!(
                                                        "format index {} out of range: {} positional argument(s) given",
                                                        placeholder,
                                                        positional_args.len()
                                                    ));
                                                }
                                            } else {
                                                // Named placeholder {name} - look up in kwargs
                                                format_map.get(&placeholder).cloned()
                                            };

                                            if let Some(val) = value {
                                                output.push_str(&val.to_string());
                                                i = placeholder_end + 1;
                                                continue;
                                            } else {
                                                return Err(anyhow!(
                                                    "Missing format argument: {}",
                                                    placeholder
                                                ));
                                            }
                                        }
                                    }
                                    output.push(chars[i]);
                                    i += 1;
                                }

                                Ok(Value::String(output))
                            }
                            _ => Err(anyhow!("Unknown string method: {}", method)),
                        }
                    }
                    Value::FrozenList(items) => {
                        let arguments = merge_args(args, args_unpack, self)?;
                        let keywords = merge_kwargs(kwargs, kwargs_unpack, self)?;
                        if !keywords.is_empty() { anyhow::bail!("list {}() accepts no named arguments", method); }
                        match method.as_str() {
                            "count" if arguments.len() == 1 => Ok(Value::Int(items.iter().filter(|item| **item == arguments[0]).count() as i64)),
                            "index" if !arguments.is_empty() && arguments.len() <= 3 => {
                                let length = items.len() as i64;
                                let bound = |value: Option<&Value>, default: i64| -> Result<usize> {
                                    let index = match value { Some(Value::Int(index)) => *index, None => default,
                                        _ => anyhow::bail!("list index bounds must be integers") };
                                    Ok(if index < 0 { (length + index).max(0) } else { index.min(length) } as usize)
                                };
                                let start = bound(arguments.get(1),0)?;
                                let end = bound(arguments.get(2),length)?;
                                (start..end).find(|index| items[*index] == arguments[0])
                                    .map(|index| Value::Int(index as i64)).ok_or_else(|| anyhow!("item not found in list"))
                            }
                            "count" | "index" => anyhow::bail!("invalid arguments to list {}()", method),
                            _ => anyhow::bail!("cannot mutate frozen list from native rule metadata"),
                        }
                    }
                    Value::ImmutableMapping(view) => {
                        let arguments = merge_args(args, args_unpack, self)?;
                        let keywords = merge_kwargs(kwargs, kwargs_unpack, self)?;
                        match method.as_str() {
                            "get" => {
                                if arguments.is_empty() || arguments.len() > 2
                                    || keywords.keys().any(|name| name != "default")
                                    || (arguments.len() == 2 && keywords.contains_key("default"))
                                {
                                    anyhow::bail!("immutable mapping get() requires a key and optional default");
                                }
                                let default = arguments.get(1).or_else(|| keywords.get("default"))
                                    .cloned().unwrap_or(Value::None);
                                Ok(view.key(&arguments[0])?.and_then(|key| view.entries.get(&key))
                                    .map(|value| view.exposed_value(value)).unwrap_or(default))
                            }
                            "keys" | "values" | "items" => {
                                if !arguments.is_empty() || !keywords.is_empty() {
                                    anyhow::bail!("immutable mapping {}() accepts no arguments", method);
                                }
                                let mut keys: Vec<_> = view.entries.keys().collect();
                                keys.sort();
                                let values = keys.into_iter().map(|key| match method.as_str() {
                                    "keys" => dictionary_key_value(key),
                                    "values" => view.exposed_value(&view.entries[key]),
                                    _ => Value::Tuple(vec![dictionary_key_value(key), view.exposed_value(&view.entries[key])]),
                                }).collect();
                                Ok(if view.kind == ImmutableMappingKind::FrozenDict {
                                    Value::List(values)
                                } else {
                                    Value::FrozenList(values)
                                })
                            }
                            _ => anyhow::bail!("cannot mutate immutable native rule metadata"),
                        }
                    }
                    Value::Dict(map) => {
                        // Execute dict methods
                        // Evaluate arguments first
                        let mut eval_args = Vec::new();
                        for arg in args {
                            eval_args.push(self.eval_expression(arg)?);
                        }

                        match method.as_str() {
                            "keys" => {
                                // Return list of keys
                                let keys: Vec<Value> =
                                    sorted_dictionary_key_values(&map);
                                Ok(Value::List(keys))
                            }
                            "values" => {
                                // Return list of values
                                let values: Vec<Value> = map.values().cloned().collect();
                                Ok(Value::List(values))
                            }
                            "items" => {
                                // Return list of [key, value] tuples
                                let items: Vec<Value> = map
                                    .iter()
                                    .map(|(k, v)| {
                                        Value::Tuple(vec![
                                            dictionary_key_value(k),
                                            v.clone(),
                                        ])
                                    })
                                    .collect();
                                Ok(Value::List(items))
                            }
                            "get" => {
                                // dict.get(key, default) - return value or default if key not found
                                if eval_args.is_empty() {
                                    return Err(anyhow!("get() requires at least one argument"));
                                }
                                let key = dictionary_key(&eval_args[0])?;
                                let default = if eval_args.len() > 1 {
                                    eval_args[1].clone()
                                } else {
                                    Value::None
                                };

                                match map.get(&key) {
                                    Some(val) => Ok(val.clone()),
                                    None => Ok(default),
                                }
                            }
                            "pop" => {
                                // dict.pop(key, default) - return value and remove key from dict
                                // Note: The actual mutation happens in eval_statement
                                // Here we just return the value
                                if eval_args.is_empty() {
                                    return Err(anyhow!("pop() requires at least one argument"));
                                }
                                let key = dictionary_key(&eval_args[0])?;
                                let default = if eval_args.len() > 1 {
                                    eval_args[1].clone()
                                } else {
                                    Value::None
                                };

                                match map.get(&key) {
                                    Some(val) => Ok(val.clone()),
                                    None => Ok(default),
                                }
                            }
                            "update" => {
                                // dict.update([other], **kwargs) - merge entries into this dict
                                // Note: The actual mutation happens in eval_statement
                                // Here we just return None
                                if eval_args.len() > 1 {
                                    return Err(anyhow!(
                                        "update() takes at most one positional argument"
                                    ));
                                }
                                if let Some(update_val) = eval_args.first() {
                                    if !matches!(
                                        dictionary_expansion_value(update_val.clone()),
                                        Value::Dict(_)
                                    ) {
                                        return Err(anyhow!(
                                            "update() argument must be a dict"
                                        ));
                                    }
                                }
                                let _ = merge_kwargs(kwargs, kwargs_unpack, self)?;
                                Ok(Value::None)
                            }
                            "setdefault" => {
                                // dict.setdefault(key, default) - return value or set and return default
                                // Note: The actual mutation happens in eval_statement for assignments
                                // Here we just handle the return value
                                if eval_args.is_empty() {
                                    return Err(anyhow!(
                                        "setdefault() requires at least one argument"
                                    ));
                                }
                                let key = dictionary_key(&eval_args[0])?;
                                let default = if eval_args.len() > 1 {
                                    eval_args[1].clone()
                                } else {
                                    Value::None
                                };

                                match map.get(&key) {
                                    Some(val) => Ok(val.clone()),
                                    None => Ok(default),
                                }
                            }
                            _ => Err(anyhow!("Unknown dict method: {}", method)),
                        }
                    }
                    Value::List(_list) => {
                        // Execute list methods
                        // Note: For list mutation methods (append, extend, remove), the actual mutation
                        // is handled in eval_statement for Stmt::Expr. Here we just handle
                        // the case where they're called in an expression context.
                        match method.as_str() {
                            "append" => {
                                // Evaluate argument
                                if args.len() != 1 {
                                    return Err(anyhow!("append() takes exactly one argument"));
                                }
                                // Just return None - mutation already happened in eval_statement
                                Ok(Value::None)
                            }
                            "extend" => {
                                // Evaluate argument
                                if args.len() != 1 {
                                    return Err(anyhow!("extend() takes exactly one argument"));
                                }
                                // Just return None - mutation already happened in eval_statement
                                Ok(Value::None)
                            }
                            "remove" => {
                                // Evaluate argument
                                if args.len() != 1 {
                                    return Err(anyhow!("remove() takes exactly one argument"));
                                }
                                // Just return None - mutation already happened in eval_statement
                                Ok(Value::None)
                            }
                            _ => Err(anyhow!("Unknown list method: {}", method)),
                        }
                    }
                    _ => Err(anyhow!("Bound method not supported for {:?}", object)),
                }
            }
            Value::Lambda {
                params,
                varargs_param,
                kwargs_param,
                body,
                closure,
            } => {
                // Create new environment with closure
                let mut func_env = Environment::with_parent(closure.clone());

                // Merge args and args_unpack to get all positional arguments
                let all_args = merge_args(args, args_unpack, self)?;

                // Bind the declared positional parameters first. Any remaining
                // arguments are collected by *args when present.
                if all_args.len() > params.len() && varargs_param.is_none() {
                    return Err(anyhow!(
                        "Lambda takes {} arguments but {} were given",
                        params.len(),
                        all_args.len()
                    ));
                }

                // Track which parameters have been bound
                let mut bound_params = std::collections::HashSet::new();

                // Bind positional arguments
                for (i, arg_val) in all_args.iter().take(params.len()).enumerate() {
                    let (param_name, _) = &params[i];
                    func_env.define(param_name.clone(), arg_val.clone());
                    bound_params.insert(param_name.clone());
                }
                if let Some(varargs_name) = &varargs_param {
                    func_env.define(
                        varargs_name.clone(),
                        Value::Tuple(all_args.into_iter().skip(params.len()).collect()),
                    );
                }

                // Bind keyword arguments
                let param_names: Vec<_> = params.iter().map(|(name, _)| name.as_str()).collect();
                let all_kwargs = merge_kwargs(kwargs, kwargs_unpack, self)?;
                let mut extra_kwargs = HashMap::new();

                for (key, val) in all_kwargs {
                    if param_names.contains(&key.as_str()) {
                        if bound_params.contains(&key) {
                            return Err(anyhow!(
                                "Lambda parameter '{}' received multiple values",
                                key
                            ));
                        }
                        func_env.define(key.clone(), val);
                        bound_params.insert(key.clone());
                    } else if kwargs_param.is_some() {
                        extra_kwargs.insert(key, val);
                    } else {
                        return Err(anyhow!("Lambda has no parameter: {}", key));
                    }
                }
                if let Some(kwargs_name) = &kwargs_param {
                    func_env.define(kwargs_name.clone(), Value::Dict(extra_kwargs));
                }

                // Reuse the value captured when the lambda was created.
                for (param_name, default) in &params {
                    if !bound_params.contains(param_name) {
                        if let Some(value) = default {
                            func_env.define(param_name.clone(), value.clone());
                        } else {
                            return Err(anyhow!("Missing required parameter: {}", param_name));
                        }
                    }
                }

                // Save current environment
                let saved_env = std::mem::replace(&mut self.env, func_env);

                // Evaluate lambda body
                let result = self.eval_expression(&body);

                // Restore environment
                self.env = saved_env;

                result
            }
            Value::ModuleExtension(_) => Err(anyhow!("module-extension declarations are not callable; execute extensions through the genuine Bazel repository analyzer")),
            _ => Err(anyhow!("Not a function: {:?}", func_val)),
        }
    }
}

/// Helper function for calculating priority recursively
fn calculate_priority_impl(
    file: &str,
    dep_graph: &HashMap<String, Vec<String>>,
    cache: &mut HashMap<String, usize>,
) -> usize {
    if let Some(&cached) = cache.get(file) {
        return cached;
    }

    let mut priority = 0;

    // Count files that directly or transitively depend on this file
    for (other_file, deps) in dep_graph {
        if deps.contains(&file.to_string()) {
            priority += 1 + calculate_priority_impl(other_file, dep_graph, cache);
        }
    }

    cache.insert(file.to_string(), priority);
    priority
}

impl Default for Evaluator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn eval(source: &str) -> Result<Evaluator> {
        let program = parse(source)?;
        let mut evaluator = Evaluator::new();
        evaluator.eval_program(&program)?;
        Ok(evaluator)
    }

    fn eval_build(source: &str, lazy: bool) -> Result<Evaluator> {
        rush_incremental::tracked::test_scope(std::path::Path::new("."), || {
            let program = parse(source)?;
            let mut evaluator = Evaluator::new();
            evaluator.register_native_namespace();
            if lazy {
                evaluator.eval_program_lazy_loads(&program)?;
            } else {
                evaluator.eval_program(&program)?;
            }
            Ok(evaluator)
        })
    }

    fn eval_loaded_build(definitions: &str, source: &str, lazy: bool) -> Result<Evaluator> {
        let workspace = tempfile::tempdir()?;
        let package_dir = workspace.path().join("pkg");
        std::fs::create_dir_all(&package_dir)?;
        std::fs::write(package_dir.join("defs.bzl"), definitions)?;
        std::fs::write(package_dir.join("BUILD"), source)?;

        rush_incremental::tracked::test_scope(workspace.path(), || {
            let program = parse(source)?;
            let loader = std::sync::Arc::new(std::sync::Mutex::new(
                crate::bzl_loader::BzlLoader::new(workspace.path().to_path_buf()),
            ));
            let mut evaluator = Evaluator::new();
            evaluator.register_native_namespace();
            evaluator.set_current_package("pkg".to_string());
            evaluator.set_current_package_dir(Some(package_dir));
            evaluator.set_bzl_loader(loader);
            let _build_context = Evaluator::build_file_context_guard();
            if lazy {
                evaluator.eval_program_lazy_loads(&program)?;
            } else {
                evaluator.eval_program(&program)?;
            }
            Ok(evaluator)
        })
    }

    fn rule_visibility(evaluator: &Evaluator, rule_type: &str, name: &str) -> Option<Vec<String>> {
        let rule = evaluator.get_rule_calls().iter().find(|rule_call| {
            rule_call.rule_type == rule_type
                && matches!(
                    rule_call.kwargs.get("name"),
                    Some(Value::String(value)) if value == name
                )
        })?;
        let (Value::List(specifications) | Value::Tuple(specifications) | Value::FrozenList(specifications)) = rule.kwargs.get("visibility")? else {
            return None;
        };
        specifications
            .iter()
            .map(|specification| match specification {
                Value::String(value) => Some(value.clone()),
                _ => None,
            })
            .collect()
    }

    fn source_visibility(evaluator: &Evaluator, name: &str) -> Option<Vec<String>> {
        rule_visibility(evaluator, "source_file", name)
    }

    fn visibility_was_explicit(
        evaluator: &Evaluator,
        name: &str,
    ) -> Option<bool> {
        evaluator
            .get_rule_calls()
            .iter()
            .find(|rule_call| {
                rule_call.rule_type == "config_setting"
                    && matches!(
                        rule_call.kwargs.get("name"),
                        Some(Value::String(value)) if value == name
                    )
            })
            .and_then(|rule_call| {
                rule_call
                    .kwargs
                    .get(VISIBILITY_EXPLICIT_MARKER)
                    .and_then(|value| match value {
                        Value::Bool(value) => Some(*value),
                        _ => None,
                    })
            })
    }

    #[test]
    fn exports_files_visibility_matches_bazel_in_eager_and_lazy_evaluation() {
        let source = r#"
package(default_visibility = ["//visibility:private"])
exports_files(["omitted.txt"])
native.exports_files(["none.txt"], visibility = None)
exports_files(["empty.txt"], visibility = [])
native.exports_files(["private.txt"], visibility = ["//visibility:private"])
exports_files(["granted.txt"], visibility = ["//consumer:__pkg__"])
"#;

        for lazy in [false, true] {
            let evaluator = eval_build(source, lazy).unwrap();
            assert_eq!(
                source_visibility(&evaluator, "omitted.txt"),
                Some(vec!["//visibility:public".to_string()])
            );
            assert_eq!(
                source_visibility(&evaluator, "none.txt"),
                Some(vec!["//visibility:public".to_string()])
            );
            assert_eq!(source_visibility(&evaluator, "empty.txt"), Some(vec![]));
            assert_eq!(
                source_visibility(&evaluator, "private.txt"),
                Some(vec!["//visibility:private".to_string()])
            );
            assert_eq!(
                source_visibility(&evaluator, "granted.txt"),
                Some(vec!["//consumer:__pkg__".to_string()])
            );
        }
    }

    #[test]
    fn package_defaults_apply_to_rules_declared_before_and_after_the_call() {
        let template = r#"
filegroup(name = "before", srcs = [])
package_group(name = "friends", packages = ["//consumer"])
exports_files(["public.txt"])
$PACKAGE(default_visibility = ["//visibility:private"])
filegroup(name = "after", srcs = [])
"#;

        for package_function in ["package", "native.package"] {
            let source = template.replace("$PACKAGE", package_function);
            for lazy in [false, true] {
                let evaluator = eval_build(&source, lazy).unwrap();
                for name in ["before", "after"] {
                    assert_eq!(
                        rule_visibility(&evaluator, "filegroup", name),
                        Some(vec!["//visibility:private".to_string()])
                    );
                }
                assert_eq!(
                    rule_visibility(&evaluator, "package_group", "friends"),
                    None,
                    "package groups do not have rule visibility"
                );
                assert_eq!(
                    source_visibility(&evaluator, "public.txt"),
                    Some(vec!["//visibility:public".to_string()])
                );
            }
        }
    }

    #[test]
    fn config_settings_retain_visibility_origin_after_package_defaults() {
        let source = r#"
config_setting(name = "omitted", define_values = {"MODE": "yes"})
config_setting(
    name = "none",
    define_values = {"MODE": "yes"},
    visibility = None,
)
config_setting(
    name = "explicit",
    define_values = {"MODE": "yes"},
    visibility = ["//visibility:private"],
)
package(default_visibility = ["//visibility:private"])
"#;

        for lazy in [false, true] {
            let evaluator = eval_build(source, lazy).unwrap();
            for name in ["omitted", "none", "explicit"] {
                assert_eq!(
                    rule_visibility(&evaluator, "config_setting", name),
                    Some(vec!["//visibility:private".to_string()])
                );
            }
            assert_eq!(
                visibility_was_explicit(&evaluator, "omitted"),
                Some(false)
            );
            assert_eq!(
                visibility_was_explicit(&evaluator, "none"),
                Some(false)
            );
            assert_eq!(
                visibility_was_explicit(&evaluator, "explicit"),
                Some(true)
            );
        }
    }

    #[test]
    fn loaded_macros_can_declare_rules_before_setting_package_defaults() {
        let definitions = r#"
def declare_before():
    native.package_group(
        name = "friends",
        packages = ["//consumer"],
    )
    native.filegroup(name = "before", srcs = [])

def set_package_defaults():
    native.package(default_visibility = ["//visibility:private"])
"#;
        let source = r#"
load("//pkg:defs.bzl", "declare_before", "set_package_defaults")

declare_before()
set_package_defaults()
filegroup(name = "after", srcs = [])
"#;

        for lazy in [false, true] {
            let evaluator = eval_loaded_build(definitions, source, lazy).unwrap();
            for name in ["before", "after"] {
                assert_eq!(
                    rule_visibility(&evaluator, "filegroup", name),
                    Some(vec!["//visibility:private".to_string()])
                );
            }
            assert_eq!(
                rule_visibility(&evaluator, "package_group", "friends"),
                None,
                "package groups do not have rule visibility"
            );
        }
    }

    #[test]
    fn native_namespace_captures_known_java_rules() {
        let source = r#"
native.java_proto_library(name = "proto", deps = [":source_proto"])
native.java_lite_proto_library(name = "lite", deps = [":proto"])
native.java_library(name = "java", srcs = ["Library.java"])
native.java_binary(name = "binary", deps = [":java"])
native.java_import(name = "imported", jars = ["library.jar"])
native.java_plugin(name = "plugin", deps = [":java"])
native.java_test(name = "test", deps = [":java"])
"#;
        let mut evaluator = Evaluator::new();
        evaluator.register_native_namespace();
        let program = crate::parse(source).unwrap();
        evaluator.eval_program(&program).unwrap();

        let calls = evaluator.rule_calls();
        assert_eq!(
            calls
                .iter()
                .map(|call| call.rule_type.as_str())
                .collect::<Vec<_>>(),
            vec![
                "java_proto_library",
                "java_lite_proto_library",
                "java_library",
                "java_binary",
                "java_import",
                "java_plugin",
                "java_test",
            ]
        );
    }

    #[test]
    fn synthesized_android_facade_captures_every_target() {
        let source = r#"
def android_library(**kwargs):
    native.android_library(**kwargs)

def android_binary(**kwargs):
    native.android_binary(**kwargs)

def aar_import(**kwargs):
    native.aar_import(**kwargs)

def flatbuffer_android_library(name):
    native.filegroup(name = name + ".srcjar", srcs = ["generated.srcjar"])
    android_library(name = name, srcs = [":" + name + ".srcjar"])

flatbuffer_android_library(name = "mobile_lib")
android_binary(name = "mobile_app")
aar_import(name = "prebuilt_aar", aar = "app.aar")
cc_library(name = "host_lib")
"#;

        for lazy in [false, true] {
            let evaluator = eval_build(source, lazy).unwrap();
            let calls = evaluator.get_rule_calls();
            for (rule_type, name) in [
                ("android_library", "mobile_lib"),
                ("android_binary", "mobile_app"),
                ("aar_import", "prebuilt_aar"),
                ("filegroup", "mobile_lib.srcjar"),
                ("cc_library", "host_lib"),
            ] {
                assert!(calls.iter().any(|call| {
                    call.rule_type == rule_type
                        && call.kwargs.get("name") == Some(&Value::String(name.to_string()))
                }));
            }
        }
    }

    #[test]
    fn exports_files_duplicate_visibility_matches_bazel() {
        for lazy in [false, true] {
            let evaluator = eval_build(
                r#"
exports_files(["same.txt"], visibility = ["//visibility:public"])
native.exports_files(["same.txt"], visibility = ["//visibility:public"])
exports_files(["within.txt", "within.txt"])
"#,
                lazy,
            )
            .unwrap();
            assert_eq!(
                evaluator
                    .get_rule_calls()
                    .iter()
                    .filter(|rule_call| rule_call.rule_type == "source_file")
                    .count(),
                2
            );

            let error = match eval_build(
                r#"
exports_files(["conflict.txt"], visibility = ["//visibility:public"])
native.exports_files(["conflict.txt"], visibility = ["//visibility:private"])
"#,
                lazy,
            ) {
                Ok(_) => panic!("conflicting duplicate exports must fail"),
                Err(error) => error,
            };
            assert_eq!(
                error.to_string(),
                "visibility for exported file 'conflict.txt' declared twice"
            );
        }
    }

    #[test]
    fn malformed_config_setting_predicates_reject_the_loaded_package() {
        let cases = [
            (
                r#"config_setting(name = "bad", define_values = [])"#,
                "parameter 'define_values' got value of type 'list', want 'dict'",
            ),
            (
                r#"config_setting(name = "bad", values = {"cpu": 7})"#,
                "config_setting.values value for key 'cpu' must be a string",
            ),
            (
                r#"config_setting(name = "bad", flag_values = {"//:flag": True})"#,
                "config_setting.flag_values value for key '//:flag' must be a string",
            ),
            (
                r#"config_setting(name = "bad", constraint_values = "@platforms//os:linux")"#,
                "parameter 'constraint_values' got value of type 'string', want 'sequence'",
            ),
            (
                r#"config_setting(name = "bad", constraint_values = [7])"#,
                "expected a label for element 0 of 'config_setting.constraint_values'",
            ),
        ];

        for lazy in [false, true] {
            for (source, expected) in cases {
                let error = match eval_build(source, lazy) {
                    Ok(_) => panic!("a malformed config_setting must reject its loaded package"),
                    Err(error) => error,
                };
                assert!(
                    error.to_string().contains(expected),
                    "unexpected error for {source}: {error}"
                );
            }
        }
    }

    #[test]
    fn empty_config_setting_is_retained_for_configured_closure_validation() {
        for lazy in [false, true] {
            let evaluator = eval_build(
                r#"
config_setting(name = "empty")
filegroup(name = "unrelated")
"#,
                lazy,
            )
            .unwrap();
            assert!(evaluator.get_rule_calls().iter().any(|call| {
                call.rule_type == "config_setting"
                    && call.kwargs.get("name")
                        == Some(&Value::String("empty".to_string()))
            }));
        }
    }

    #[test]
    fn build_setting_identity_comes_from_rule_definition_not_attribute_spelling() {
        let source = r#"
ordinary_rule = rule(attrs = {
    "build_setting_default": attr.string(),
})
real_scalar_setting = rule(build_setting = config.string())
real_list_setting = rule(build_setting = config.string_list())

ordinary_rule(name = "ordinary", build_setting_default = "forged")
real_scalar_setting(name = "scalar", build_setting_default = "real")
real_list_setting(name = "list", build_setting_default = ["a", "b"])
filegroup(name = "unrelated")
"#;

        for lazy in [false, true] {
            let evaluator = eval_build(source, lazy).unwrap();
            let rule = |name: &str| {
                evaluator
                    .get_rule_calls()
                    .iter()
                    .find(|call| {
                        call.kwargs.get("name")
                            == Some(&Value::String(name.to_string()))
                    })
                    .expect("declared rule")
            };
            assert_ne!(
                rule("ordinary").kwargs.get("__rush_build_setting"),
                Some(&Value::Bool(true)),
                "an ordinary attribute cannot forge build-setting identity"
            );
            for name in ["scalar", "list"] {
                assert_eq!(
                    rule(name).kwargs.get("__rush_build_setting"),
                    Some(&Value::Bool(true)),
                    "rule(build_setting=...) must preserve provider identity"
                );
            }
        }
    }

    #[test]
    fn malformed_package_group_rejects_loaded_package_in_eager_and_lazy_evaluation() {
        let cases = [
            (
                r#"
filegroup(name = "good_sibling", srcs = [])
package_group(
    name = "bad",
    packages = select({"//conditions:default": []}),
)
"#,
                "in call to package_group(), parameter 'packages' got value of type 'select', want 'sequence'",
            ),
            (
                r#"
filegroup(name = "good_sibling", srcs = [])
native.package_group(
    name = "bad",
    includes = [] + select({"//conditions:default": []}),
)
"#,
                "in call to package_group(), parameter 'includes' got value of type 'select', want 'sequence'",
            ),
            (
                r#"
package_group(name = "bad", packages = 7)
filegroup(name = "good_sibling", srcs = [])
"#,
                "in call to package_group(), parameter 'packages' got value of type 'int', want 'sequence'",
            ),
            (
                r#"
package_group(name = "bad", includes = None)
filegroup(name = "good_sibling", srcs = [])
"#,
                "in call to package_group(), parameter 'includes' got value of type 'NoneType', want 'sequence'",
            ),
            (
                r#"
package_group(name = "bad", packages = ["//first", 7])
filegroup(name = "good_sibling", srcs = [])
"#,
                "expected value of type 'string' for element 1 of 'package_group.packages argument', but got 7 (int)",
            ),
            (
                r#"
package_group(
    name = "bad",
    includes = [select({"//conditions:default": "//group:label"})],
)
filegroup(name = "good_sibling", srcs = [])
"#,
                "expected value of type 'string' for element 0 of 'package_group.includes argument', but got select({\"//conditions:default\": \"//group:label\"}) (select)",
            ),
            (
                r#"
package_group(
    name = "bad",
    packages = {"key": select({"//conditions:default": "//first"})},
)
filegroup(name = "good_sibling", srcs = [])
"#,
                "in call to package_group(), parameter 'packages' got value of type 'dict', want 'sequence'",
            ),
        ];

        for lazy in [false, true] {
            for (source, expected) in cases {
                let error = match eval_build(source, lazy) {
                    Ok(_) => panic!("a malformed package_group must reject its loaded package"),
                    Err(error) => error,
                };
                assert_eq!(error.to_string(), expected);
            }
        }
    }

    #[test]
    fn package_group_accepts_constant_lists_in_eager_and_lazy_evaluation() {
        let source = r#"
package_group(
    name = "concat",
    packages = ["//first"] + ["//second"],
    includes = [],
)
package_group(
    name = "dead_select",
    packages = [] if True else select({"//conditions:default": ["//first"]}),
)
package_group(
    name = "comprehension",
    packages = [value for value in ["//first"]],
)
package_group(
    name = "tuples",
    packages = ("//first",),
    includes = (":concat",),
)
filegroup(name = "good_sibling", srcs = [])
"#;

        for lazy in [false, true] {
            let evaluator = eval_build(source, lazy).unwrap();
            assert_eq!(evaluator.rule_calls().len(), 5);
            assert!(evaluator.rule_calls()[..4]
                .iter()
                .all(|rule| rule.rule_type == "package_group"));
        }
    }

    #[test]
    fn package_group_label_elements_match_bazel() {
        let label = Value::Label {
            label: "//group:included".to_string(),
            package: "group".to_string(),
            name: "included".to_string(),
            workspace_name: String::new(),
            workspace_root: String::new(),
        };
        let includes = HashMap::from([("includes".to_string(), Value::List(vec![label.clone()]))]);
        Evaluator::validate_loading_time_rule_attributes("package_group", &includes).unwrap();

        let packages = HashMap::from([("packages".to_string(), Value::List(vec![label]))]);
        let error = Evaluator::validate_loading_time_rule_attributes("package_group", &packages)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "expected value of type 'string' for element 0 of 'package_group.packages argument', but got Label(\"//group:included\") (Label)"
        );
    }

    #[test]
    fn value_census_dedups_shared_closure_env_across_functions() {
        // #39 graph-RSS census invariant: a module closure environment
        // shared by multiple exported functions (the common .bzl shape) is
        // counted ONCE via Arc::as_ptr dedup. Two functions over the same
        // closure Arc → unique_closure_envs=1, not 2; closure_env_var_entries
        // reflects that single env's var count once.
        use crate::ast::Stmt;
        let shared = Arc::new(Environment::new());
        let mk = |closure: Arc<Environment>| Value::Function {
            params: vec![],
            varargs_param: None,
            kwargs_param: None,
            body: vec![Stmt::Pass, Stmt::Pass],
            closure,
        };
        let f1 = mk(Arc::clone(&shared));
        let f2 = mk(Arc::clone(&shared));
        let census = ValueCensus::of_values([&f1, &f2].into_iter());
        assert_eq!(census.function_count, 2);
        assert_eq!(census.function_body_stmts, 4, "2 funcs x 2 stmts");
        assert_eq!(
            census.unique_closure_envs, 1,
            "shared closure Arc counted once"
        );
    }

    #[test]
    fn test_arithmetic() {
        let evaluator = eval("result = 2 + 3 * 4").unwrap();
        assert_eq!(evaluator.get_var("result"), Some(Value::Int(14)));
    }

    #[test]
    fn unmatched_select_survives_module_loading() {
        let evaluator = eval(
            r#"unused = select({"//conditions:rush_test_never": ["unused.cc"]})"#,
        )
        .unwrap();

        assert!(matches!(evaluator.get_var("unused"), Some(Value::Select(_))));
    }

    #[test]
    fn symbolic_select_composes_with_lists_and_dicts() {
        let evaluator = eval(
            r#"
list_value = ["base.cc"] + select({
    "//conditions:rush_test_never": ["conditional.cc"],
})
dict_value = {"base": "yes"} | select({
    "//conditions:rush_test_never": {"conditional": "yes"},
})
"#,
        )
        .unwrap();

        let Some(Value::Configurable { op, left, right }) =
            evaluator.get_var("list_value")
        else {
            panic!("list composition must remain compact and symbolic");
        };
        assert_eq!(op, ConfigurableOp::Add);
        assert_eq!(
            left.as_ref(),
            &Value::List(vec![Value::String("base.cc".to_string())])
        );
        assert!(matches!(right.as_ref(), Value::Select(_)));

        let Some(Value::Configurable { op, left, right }) =
            evaluator.get_var("dict_value")
        else {
            panic!("dict composition must remain compact and symbolic");
        };
        assert_eq!(op, ConfigurableOp::BitOr);
        assert!(matches!(left.as_ref(), Value::Dict(_)));
        assert!(matches!(right.as_ref(), Value::Select(_)));
    }

    #[test]
    fn repeated_select_composition_retains_linear_expression_depth() {
        let evaluator = Evaluator::new();
        let mut value = Value::List(Vec::new());

        // 10 two-way selectors followed by 4-way and 7-way selectors would
        // produce 28,672 leaf lists under Cartesian distribution. The real
        // tf_copts() chain reaches 58,720,256 leaves per invocation.
        for index in 0..12 {
            let branch_count = match index {
                10 => 4,
                11 => 7,
                _ => 2,
            };
            let mut branches = HashMap::from([(
                "//conditions:default".to_string(),
                Value::List(vec![Value::String(format!("feature-{index}"))]),
            )]);
            for branch in 1..branch_count {
                branches.insert(
                    format!("//conditions:rush_test_never_{index}_{branch}"),
                    Value::List(vec![Value::String(format!(
                        "feature-{index}-{branch}"
                    ))]),
                );
            }
            let selector = Value::Select(branches);
            value = evaluator
                .apply_binop(&value, BinOp::Add, &selector)
                .unwrap();
        }

        let mut depth = 0;
        let mut cursor = &value;
        while let Value::Configurable {
            op: ConfigurableOp::Add,
            left,
            right,
        } = cursor
        {
            assert!(matches!(right.as_ref(), Value::Select(_)));
            depth += 1;
            cursor = left;
        }
        assert_eq!(depth, 12);
        assert_eq!(cursor, &Value::List(Vec::new()));
        assert!(
            value.estimate_size() < 200_000,
            "compact selector DAG unexpectedly exceeded 200 KiB"
        );
    }

    #[test]
    fn configurable_composition_rejects_incompatible_branch_types_early() {
        let list_plus_string = match eval(
            r#"value = select({
    "//conditions:default": ["selected.cc"],
    "//conditions:rush_test_never": ["other.cc"],
}) + "not-a-list""#,
        ) {
            Ok(_) => panic!("configurable list + string must fail during evaluation"),
            Err(error) => error,
        };
        assert!(
            format!("{list_plus_string:#}").contains("configurable + to list and string")
        );

        let dict_or_list = match eval(
            r#"value = {"base": "yes"} | select({
    "//conditions:default": ["not-a-dict"],
    "//conditions:rush_test_never": ["also-not-a-dict"],
})"#,
        ) {
            Ok(_) => panic!("configurable dict | list must fail during evaluation"),
            Err(error) => error,
        };
        assert!(format!("{dict_or_list:#}").contains("configurable | to dict and list"));

        let configurable_float = match eval(
            r#"value = select({"//conditions:default": 1.5}) + 2.5"#,
        ) {
            Ok(_) => panic!("configurable float addition must fail before a lossy bridge"),
            Err(error) => error,
        };
        assert!(format!("{configurable_float:#}")
            .contains("configurable + to float and float"));
    }

    #[test]
    fn loaded_proto_text_macro_keeps_generator_and_compilation_targets() -> Result<()> {
        let definitions = r#"
def clean_dep(label):
    return label

def cc_library(**kwargs):
    native.cc_library(**kwargs)

def tf_generate_proto_text_sources(name, srcs_relative_dir, srcs, protodeps = [], deps = [], visibility = None, compatible_with = None):
    out_hdrs = (
        [p.replace(".proto", ".pb_text.h") for p in srcs] +
        [p.replace(".proto", ".pb_text-impl.h") for p in srcs]
    )
    out_srcs = [p.replace(".proto", ".pb_text.cc") for p in srcs]
    native.genrule(
        name = name + "_srcs",
        srcs = srcs + protodeps + [clean_dep("//tensorflow/tools/proto_text:placeholder.txt")],
        outs = out_hdrs + out_srcs,
        visibility = visibility,
        cmd = "$(location //tensorflow/tools/proto_text:gen_proto_text_functions) " + "$(@D) " + srcs_relative_dir + " $(SRCS)",
        tools = [clean_dep("//tensorflow/tools/proto_text:gen_proto_text_functions")],
        compatible_with = compatible_with,
    )
    native.filegroup(
        name = name + "_hdrs",
        srcs = out_hdrs,
        visibility = visibility,
        compatible_with = compatible_with,
    )
    cc_library(
        compatible_with = compatible_with,
        name = name,
        srcs = out_srcs,
        hdrs = out_hdrs,
        visibility = visibility,
        deps = deps,
        alwayslink = 1,
    )
"#;
        for lazy in [false, true] {
            let evaluator = eval_loaded_build(
                definitions,
                r#"
load("//pkg:defs.bzl", "tf_generate_proto_text_sources")
tf_generate_proto_text_sources(name="text",srcs_relative_dir="pkg/",srcs=["nested/value.proto"],protodeps=["//imports:other.proto"],deps=[":real_proto"])
"#,
                lazy,
            )?;
            let calls = evaluator.get_rule_calls();
            assert_eq!(
                calls.len(),
                3,
                "execute the loaded macro instead of a native source replacement"
            );
            let generator = calls
                .iter()
                .find(|call| call.rule_type == "genrule")
                .unwrap();
            assert_eq!(generator.kwargs["name"], Value::String("text_srcs".into()));
            assert_eq!(
                generator.kwargs["tools"],
                Value::List(vec![Value::String(
                    "//tensorflow/tools/proto_text:gen_proto_text_functions".into()
                )])
            );
            assert_eq!(
                generator.kwargs["srcs"],
                Value::List(vec![
                    Value::String("nested/value.proto".into()),
                    Value::String("//imports:other.proto".into()),
                    Value::String("//tensorflow/tools/proto_text:placeholder.txt".into())
                ])
            );
            assert_eq!(
                generator.kwargs["outs"],
                Value::List(vec![
                    Value::String("nested/value.pb_text.h".into()),
                    Value::String("nested/value.pb_text-impl.h".into()),
                    Value::String("nested/value.pb_text.cc".into())
                ])
            );
            let library = calls
                .iter()
                .find(|call| call.rule_type == "cc_library")
                .unwrap();
            assert_eq!(
                library.kwargs["deps"],
                Value::List(vec![Value::String(":real_proto".into())])
            );
            assert_eq!(library.kwargs["alwayslink"], Value::Int(1));
            assert_eq!(
                library.kwargs["srcs"],
                Value::List(vec![Value::String("nested/value.pb_text.cc".into())])
            );
            assert!(calls.iter().any(|call| call.rule_type == "filegroup"
                && call.kwargs["name"] == Value::String("text_hdrs".into())));
            assert!(!calls
                .iter()
                .any(|call| call.rule_type == "tf_generate_proto_text_sources"));
        }
        Ok(())
    }

    #[test]
    fn tf_proto_integer_header_only_retains_separate_implementation() -> Result<()> {
        for (value, split) in [
            (Value::Bool(true), true),
            (Value::Int(1), true),
            (Value::Int(0), false),
        ] {
            let calls = expand_tf_proto_library(
                "tf_proto_library",
                &[],
                &HashMap::from([
                    ("name".into(), Value::String("messages".into())),
                    (
                        "srcs".into(),
                        Value::List(vec![Value::String("message.proto".into())]),
                    ),
                    ("make_default_target_header_only".into(), value),
                ]),
            )?;
            let implementation = calls
                .iter()
                .find(|call| {
                    call.kwargs.get("name") == Some(&Value::String("messages_cc_impl".into()))
                })
                .unwrap();
            assert_eq!(implementation.rule_type == "tf_proto_library", split);
            assert_eq!(
                calls.iter().any(|call| call.kwargs.get("name")
                    == Some(&Value::String("messages_genproto_owner".into()))),
                split
            );
        }
        Ok(())
    }

    #[test]
    fn tf_proto_impl_preserves_label_and_conditional_dependency_structure() -> Result<()> {
        let typed = Value::Label {
            label: "@@repository//dependency:messages".into(),
            package: "dependency".into(),
            name: "messages".into(),
            workspace_name: "repository".into(),
            workspace_root: "external/repository".into(),
        };
        for collection in [
            Value::List(vec![typed.clone()]),
            Value::Tuple(vec![typed.clone()]),
            Value::FrozenList(vec![typed]),
        ] {
            let Value::List(values) = rewrite_proto_dependencies(&collection)? else {
                panic!("expected rewritten native list");
            };
            let Value::Label { label, name, .. } = &values[0] else {
                panic!("typed dependency lost its Label identity");
            };
            assert_eq!(
                label.rsplit_once(':').unwrap().1,
                name,
                "Label target name must agree with its rewritten canonical identity"
            );
            assert_eq!(
                values,
                vec![Value::Label {
                    label: "@@repository//dependency:messages_cc_impl".into(),
                    package: "dependency".into(),
                    name: "messages_cc_impl".into(),
                    workspace_name: "repository".into(),
                    workspace_root: "external/repository".into()
                }]
            );
        }
        let selector = Arc::new(Value::Select(HashMap::from([
            (
                "@@conditions//:chosen".into(),
                Value::Tuple(vec![Value::String("//dependency:messages".into())]),
            ),
            (
                "//conditions:default".into(),
                Value::List(vec![Value::String("local".into())]),
            ),
        ])));
        let dependencies = Value::Configurable {
            op: ConfigurableOp::Add,
            left: selector.clone(),
            right: selector,
        };
        let calls = expand_tf_proto_library(
            "tf_proto_library",
            &[],
            &HashMap::from([
                ("name".into(), Value::String("root".into())),
                (
                    "srcs".into(),
                    Value::List(vec![Value::String("root.proto".into())]),
                ),
                ("make_default_target_header_only".into(), Value::Bool(true)),
                ("protodeps".into(), dependencies.clone()),
            ]),
        )?;
        let owner = calls
            .iter()
            .find(|call| {
                call.kwargs.get("name") == Some(&Value::String("root_genproto_owner".into()))
            })
            .unwrap();
        assert_eq!(
            owner.kwargs.get("protodeps"),
            Some(&dependencies),
            "generation closure must retain original source deps"
        );
        let implementation = calls
            .iter()
            .find(|call| call.kwargs.get("name") == Some(&Value::String("root_cc_impl".into())))
            .unwrap();
        let Some(Value::Configurable {
            left: prefix,
            right,
            ..
        }) = implementation.kwargs.get("protodeps")
        else {
            panic!("missing owner plus conditional impl dependencies");
        };
        assert_eq!(
            prefix.as_ref(),
            &Value::List(vec![Value::String(":root_genproto_owner".into())])
        );
        let Value::Configurable { left, right, .. } = right.as_ref() else {
            panic!("lost concatenation");
        };
        assert!(
            Arc::ptr_eq(left, right),
            "shared metadata must remain shared, not expand exponentially"
        );
        let Value::Select(branches) = left.as_ref() else {
            panic!("lost select");
        };
        assert_eq!(
            branches.get("@@conditions//:chosen"),
            Some(&Value::List(vec![Value::String(
                "//dependency:messages_cc_impl".into()
            )]))
        );
        assert_eq!(
            branches.get("//conditions:default"),
            Some(&Value::List(vec![Value::String("local_cc_impl".into())]))
        );
        assert!(rewrite_proto_dependencies(&Value::List(vec![Value::Bool(true)])).is_err());
        assert!(rewrite_proto_dependencies(&Value::String(":not_a_list".into())).is_err());
        Ok(())
    }
    #[test]
    fn tf_proto_macro_preserves_declared_tool_owner_and_runtime_select() -> Result<()> {
        let mut evaluator = Evaluator::new();
        evaluator.set_current_repository(Some("consumer".into()));
        evaluator.set_current_package("pkg".into());
        let tool = evaluator.qualify_proto_tool_label(&Value::String("//tools:protoc".into()))?;
        assert_eq!(tool, "@@consumer//tools:protoc");
        assert_eq!(
            evaluator.qualify_proto_tool_label(&Value::String("@//tools:protoc".into()))?,
            "@@//tools:protoc"
        );
        let attributes = HashMap::from([
            ("name".into(), Value::String("messages".into())),
            (
                "srcs".into(),
                Value::List(vec![Value::String("nested/value.proto".into())]),
            ),
            ("make_default_target_header_only".into(), Value::Bool(true)),
            ("protoc".into(), Value::String(tool.clone())),
            ("cc_grpc_version".into(), Value::String("1.0".into())),
            (
                "cc_libs".into(),
                Value::List(vec![Value::String(":runtime".into())]),
            ),
        ]);
        let calls = expand_tf_proto_library("tf_proto_library", &[], &attributes)?;
        let generation = calls
            .iter()
            .find(|call| {
                call.kwargs.get("name") == Some(&Value::String("messages_genproto_owner".into()))
            })
            .unwrap();
        let implementation = calls
            .iter()
            .find(|call| call.kwargs.get("name") == Some(&Value::String("messages_cc_impl".into())))
            .unwrap();
        let base = calls
            .iter()
            .find(|call| call.kwargs.get("name") == Some(&Value::String("messages".into())))
            .unwrap();
        assert_eq!(generation.kwargs.get("protoc"), Some(&Value::String(tool)));
        assert!(!generation.kwargs.contains_key("cc_libs"));
        assert!(!base.kwargs.contains_key("protoc"));
        assert!(!implementation.kwargs.contains_key("protoc"));
        for target in [generation, implementation] {
            assert_eq!(
                target.kwargs.get("cc_grpc_version"),
                Some(&Value::String("1.0".into()))
            );
        }
        assert_eq!(
            implementation.kwargs.get("_rush_tf_proto_generation_owner"),
            Some(&Value::String(":messages_genproto_owner".into()))
        );
        let Some(Value::Configurable {
            op: ConfigurableOp::Add,
            left,
            right,
        }) = implementation.kwargs.get("cc_libs")
        else {
            panic!("runtime conditional must remain unresolved metadata");
        };
        assert_eq!(
            left.as_ref(),
            &Value::List(vec![Value::String(":runtime".into())])
        );
        let Value::Select(branches) = right.as_ref() else {
            panic!("missing runtime select");
        };
        assert_eq!(
            branches.get("@local_xla//xla/tsl:tsl_protobuf_header_only"),
            Some(&Value::List(vec![Value::String(
                "@com_google_protobuf//:protobuf_headers".into()
            )]))
        );
        assert_eq!(
            branches.get("//conditions:default"),
            Some(&Value::List(vec![Value::String(
                "@com_google_protobuf//:protobuf".into()
            )]))
        );
        Ok(())
    }
    #[test]
    fn test_function_call() {
        let root = tempfile::tempdir().unwrap();
        rush_incremental::tracked::test_scope(root.path(), || {
            let source = r#"
def add(a, b):
    return a + b

result = add(10, 20)
"#;
            let evaluator = eval(source).unwrap();
            assert_eq!(evaluator.get_var("result"), Some(Value::Int(30)));
        });
    }

    #[test]
    fn test_if_statement() {
        let source = r#"
x = 10
if x > 5:
    result = "big"
else:
    result = "small"
"#;
        let evaluator = eval(source).unwrap();
        assert_eq!(
            evaluator.get_var("result"),
            Some(Value::String("big".to_string()))
        );
    }

    // ---- #39(C) v2: tf_proto_library header-only 3-target split ----
    // Regression test for the proto `_cc_impl` participation fix. For
    // make_default_target_header_only protos the macro must split the (formerly
    // fused) base target into a private protoc-running gen owner, a header-only
    // base API the label consumers reach, and the compiled `_cc_impl` — wired so
    // the base transitively carries the impl archive WITHOUT a base↔impl cycle.
    fn expand_proto(name: &str, header_only: bool) -> Vec<RuleCall> {
        let mut kwargs = HashMap::new();
        kwargs.insert("name".to_string(), Value::String(name.to_string()));
        kwargs.insert(
            "srcs".to_string(),
            Value::List(vec![Value::String(format!("{}.proto", name))]),
        );
        if header_only {
            kwargs.insert(
                "make_default_target_header_only".to_string(),
                Value::Bool(true),
            );
        }
        expand_tf_proto_library(name, &[], &kwargs).expect("expand_tf_proto_library")
    }

    fn find_rule<'a>(rules: &'a [RuleCall], name: &str) -> Option<&'a RuleCall> {
        rules.iter().find(|r| {
            matches!(r.kwargs.get("name"), Some(Value::String(n)) if n == name)
        })
    }

    fn str_list(rc: &RuleCall, key: &str) -> Vec<String> {
        match rc.kwargs.get(key) {
            Some(Value::List(items)) => items
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn tf_proto_macro_separates_cc_message_and_optional_grpc_roles() -> Result<()> {
        for header_only in [false, true] {
            for (has_services, create_grpc_library) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let attrs = HashMap::from([
                    ("name".into(), Value::String("messages_proto".into())),
                    (
                        "srcs".into(),
                        Value::List(vec![Value::String("message.proto".into())]),
                    ),
                    (
                        "protodeps".into(),
                        Value::List(vec![Value::String(":dependency_proto".into())]),
                    ),
                    ("protoc".into(), Value::String("@@tools//:protoc".into())),
                    (
                        "make_default_target_header_only".into(),
                        Value::Bool(header_only),
                    ),
                    ("has_services".into(), Value::Bool(has_services)),
                    (
                        "create_grpc_library".into(),
                        Value::Bool(create_grpc_library),
                    ),
                ]);
                let calls = expand_tf_proto_library("tf_proto_library", &[], &attrs)?;
                let generation_name = if header_only {
                    "messages_proto_genproto_owner"
                } else {
                    "messages_proto"
                };
                let generation = find_rule(&calls, generation_name).unwrap();
                assert_eq!(str_list(generation, "srcs"), ["message.proto"]);
                assert_eq!(generation.kwargs.get("protoc"), attrs.get("protoc"));
                for call in calls.iter().filter(|call| {
                    call.rule_type == "tf_proto_library"
                        && call.kwargs.get("_rush_tf_proto_role")
                            != Some(&Value::String("grpc_sibling".into()))
                }) {
                    assert!(!call.kwargs.contains_key("has_services"));
                    assert!(!call.kwargs.contains_key("create_grpc_library"));
                }
                assert_eq!(
                    generation.kwargs.get("cc_grpc_version"),
                    attrs.get("cc_grpc_version")
                );
                if header_only {
                    let implementation = find_rule(&calls, "messages_proto_cc_impl").unwrap();
                    assert_eq!(
                        implementation.kwargs.get("cc_grpc_version"),
                        attrs.get("cc_grpc_version")
                    );
                    assert!(str_list(implementation, "protodeps")
                        .contains(&":messages_proto_genproto_owner".into()));
                    assert!(str_list(implementation, "protodeps")
                        .contains(&":dependency_proto_cc_impl".into()));
                }
                let sibling = find_rule(&calls, "messages_cc_grpc_proto");
                assert_eq!(sibling.is_some(), create_grpc_library);
                if let Some(sibling) = sibling {
                    assert_eq!(sibling.rule_type, "tf_proto_library");
                    assert_eq!(
                        sibling.kwargs.get("_rush_tf_proto_role"),
                        Some(&Value::String("grpc_sibling".into()))
                    );
                    assert_eq!(str_list(sibling, "srcs"), [":messages_proto"]);
                    assert_eq!(str_list(sibling, "protodeps"), [":messages_proto_cc"]);
                    assert!(
                        !sibling.kwargs.contains_key("actual"),
                        "gRPC cannot alias pb-only results"
                    );
                }
            }
        }
        for attribute in ["has_services", "create_grpc_library"] {
            for invalid in [
                Value::Int(2),
                Value::String("true".into()),
                Value::List(Vec::new()),
            ] {
                let attrs = HashMap::from([
                    ("name".into(), Value::String("invalid_proto".into())),
                    (attribute.into(), invalid),
                ]);
                let error = expand_tf_proto_library("tf_proto_library", &[], &attrs).unwrap_err();
                assert!(error.to_string().contains(attribute), "{error:#}");
            }
        }
        Ok(())
    }

    #[test]
    fn tf_proto_header_only_splits_into_genproto_owner_base_impl() {
        let rules = expand_proto("foo_proto", true);

        // Gen owner: the real protoc-running target (carries the srcs), private.
        let owner = find_rule(&rules, "foo_proto_genproto_owner")
            .expect("genproto_owner emitted for header-only proto");
        assert_eq!(owner.rule_type, "tf_proto_library");
        assert_eq!(str_list(owner, "srcs"), vec!["foo_proto.proto".to_string()]);

        // Base: the header-only API consumers reach. EMPTY srcs (no protoc, no
        // archive) and protodeps BOTH the gen owner (headers) and the impl
        // (archive) — so its merged CcInfo carries both.
        let base = find_rule(&rules, "foo_proto").expect("base target emitted");
        assert_eq!(base.rule_type, "tf_proto_library");
        assert!(
            str_list(base, "srcs").is_empty(),
            "base must have empty srcs (runs no protoc, owns no archive)"
        );
        let base_pd = str_list(base, "protodeps");
        assert!(
            base_pd.contains(&":foo_proto_genproto_owner".to_string()),
            "base must protodep the gen owner (headers): {:?}",
            base_pd
        );
        assert!(
            base_pd.contains(&":foo_proto_cc_impl".to_string()),
            "base must protodep the cc_impl (archive) — the #39(C) fix: {:?}",
            base_pd
        );

        // Impl: real cc_impl-role target; protodeps the OWNER, NOT the base, so
        // there is no base->impl->base cycle.
        let imp = find_rule(&rules, "foo_proto_cc_impl").expect("cc_impl emitted");
        assert_eq!(imp.rule_type, "tf_proto_library");
        assert_eq!(
            imp.kwargs.get("_rush_tf_proto_role"),
            Some(&Value::String("cc_impl".to_string()))
        );
        let imp_pd = str_list(imp, "protodeps");
        assert!(
            imp_pd.contains(&":foo_proto_genproto_owner".to_string()),
            "impl must protodep the gen owner: {:?}",
            imp_pd
        );
        assert!(
            !imp_pd.contains(&":foo_proto".to_string()),
            "impl must NOT protodep the base (would cycle): {:?}",
            imp_pd
        );

        // {name}_cc: plain alias to the base header-only API.
        let cc = find_rule(&rules, "foo_proto_cc").expect("_cc alias emitted");
        assert_eq!(cc.rule_type, "alias");
        assert_eq!(
            cc.kwargs.get("actual"),
            Some(&Value::String(":foo_proto".to_string()))
        );
    }

    #[test]
    fn tf_proto_non_header_only_keeps_base_archive_and_aliases() {
        let rules = expand_proto("bar_proto", false);

        // Non-header-only: the base carries the archive itself (keeps its srcs),
        // and there is NO gen-owner split.
        let base = find_rule(&rules, "bar_proto").expect("base target emitted");
        assert_eq!(base.rule_type, "tf_proto_library");
        assert_eq!(str_list(base, "srcs"), vec!["bar_proto.proto".to_string()]);
        assert!(
            find_rule(&rules, "bar_proto_genproto_owner").is_none(),
            "non-header-only protos must NOT get a gen-owner split"
        );

        // _cc and _cc_impl are cheap aliases to the archive-bearing base.
        let cc = find_rule(&rules, "bar_proto_cc").expect("_cc alias");
        assert_eq!(cc.rule_type, "alias");
        assert_eq!(
            cc.kwargs.get("actual"),
            Some(&Value::String(":bar_proto".to_string()))
        );
        let imp = find_rule(&rules, "bar_proto_cc_impl").expect("_cc_impl alias");
        assert_eq!(imp.rule_type, "alias");
        assert_eq!(
            imp.kwargs.get("actual"),
            Some(&Value::String(":bar_proto".to_string()))
        );
    }

    #[test]
    fn test_for_loop() {
        let source = r#"
sum = 0
for i in [1, 2, 3, 4, 5]:
    sum = sum + i
"#;
        let evaluator = eval(source).unwrap();
        assert_eq!(evaluator.get_var("sum"), Some(Value::Int(15)));
    }

    #[test]
    fn test_list_operations() {
        let source = r#"
items = [1, 2, 3]
first = items[0]
last = items[-1]
"#;
        let evaluator = eval(source).unwrap();
        assert_eq!(evaluator.get_var("first"), Some(Value::Int(1)));
        assert_eq!(evaluator.get_var("last"), Some(Value::Int(3)));
    }

    #[test]
    fn test_list_concatenation() {
        let source = r#"
base = [1, 2, 3]
extra = [4, 5]
combined = base + extra
empty_list = []
with_empty = combined + empty_list
"#;
        let evaluator = eval(source).unwrap();
        assert_eq!(
            evaluator.get_var("combined"),
            Some(Value::List(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(3),
                Value::Int(4),
                Value::Int(5),
            ]))
        );
        assert_eq!(
            evaluator.get_var("with_empty"),
            Some(Value::List(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(3),
                Value::Int(4),
                Value::Int(5),
            ]))
        );
    }

    #[test]
    fn test_string_method_split() {
        let source = r#"
VERSION = "2.15.0"
VERSION_MAJOR = VERSION.split(".")[0]
"#;
        let evaluator = eval(source).unwrap();
        assert_eq!(
            evaluator.get_var("VERSION_MAJOR"),
            Some(Value::String("2".to_string()))
        );
    }

    #[test]
    fn test_native_package_name() {
        let source = r#"
# Test native.package_name() function
pkg = native.package_name()
"#;
        let mut evaluator = Evaluator::new();
        evaluator.set_current_package("tensorflow/examples/label_image".to_string());
        evaluator.register_native_namespace();
        let program = crate::parse(source).unwrap();
        evaluator.eval_program(&program).unwrap();
        assert_eq!(
            evaluator.get_var("pkg"),
            Some(Value::String(
                "tensorflow/examples/label_image".to_string()
            ))
        );
    }

    #[test]
    fn dictionary_label_tuple_keys_keep_their_identity() {
        let evaluator = eval(r#"
key = ("//foo:off", "//conditions:default")
d = {"//foo:a": [":depA"], key: [":portable"]}
value = d[key]
present = key in d
"#).unwrap();
        let Some(Value::Dict(map)) = evaluator.get_var("d") else { panic!("expected dict") };
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("//foo:a"));
        assert!(!map.contains_key("//foo:off"));
        assert!(!map.contains_key("//conditions:default"));
        assert_eq!(evaluator.get_var("present"), Some(Value::Bool(true)));
        assert_eq!(evaluator.get_var("value"), Some(Value::List(vec![Value::String(":portable".into())])));
    }

    #[test]
    fn label_dictionary_keys_round_trip_without_losing_type_or_identity() {
        let source = r#"
label = Label("//pkg:target")
d = {label: "value"}
lookup = d[label]
keys = d.keys()
items = d.items()
iterated = [key for key in d]
listed = list(d)
enumerated = enumerate(d)
rendered = repr(d)
"#;
        let evaluator = eval_build(source, false).unwrap();
        let label = evaluator.get_var("label").expect("label");
        assert_eq!(evaluator.get_var("lookup"), Some(Value::String("value".to_string())));
        assert_eq!(evaluator.get_var("keys"), Some(Value::List(vec![label.clone()])));
        assert_eq!(evaluator.get_var("iterated"), Some(Value::List(vec![label.clone()])));
        assert_eq!(evaluator.get_var("listed"), Some(Value::List(vec![label.clone()])));
        assert_eq!(
            evaluator.get_var("enumerated"),
            Some(Value::List(vec![Value::Tuple(vec![
                Value::Int(0),
                label.clone(),
            ])]))
        );
        let Some(Value::String(rendered)) = evaluator.get_var("rendered") else {
            panic!("repr(dict) must return a string")
        };
        assert!(rendered.contains("Label(\"@@//pkg:target\")"), "{rendered}");
        assert!(!rendered.contains("rush-non-string-dict-key"), "{rendered}");
        assert_eq!(
            evaluator.get_var("items"),
            Some(Value::List(vec![Value::Tuple(vec![
                label,
                Value::String("value".to_string()),
            ])]))
        );
    }

    #[test]
    fn label_dictionary_keys_are_decoded_at_select_boundary() {
        let source = r#"
condition = Label("//:config")
root_condition = Label("@@//:root_config")
selected = select({
    condition: [":chosen"],
    root_condition: [":root_chosen"],
})
"#;
        let evaluator = eval_build(source, false).unwrap();
        let Some(Value::Select(conditions)) = evaluator.get_var("selected") else {
            panic!("select() must remain configurable")
        };
        assert_eq!(conditions.len(), 2);
        assert!(conditions.contains_key("@@//:config"));
        assert!(conditions.contains_key("@@//:root_config"));
        assert!(
            conditions
                .keys()
                .all(|key| !key.contains("rush-non-string-dict-key"))
        );
    }

    #[test]
    fn label_dictionary_identity_is_canonical_and_string_only_consumers_fail_closed() {
        let source = r#"
a = Label("@repo//pkg:target")
b = package_relative_label("@@repo//pkg:target")
d = {a: "value"}
lookup = d[b]
main = Label("//pkg:target")
main_relative = package_relative_label("//pkg:target")
main_explicit = package_relative_label("@//pkg:target")
main_canonical = package_relative_label("@@//pkg:target")
main_passthrough = package_relative_label(main)
main_dictionary = {main: "main value"}
main_lookups = [main_dictionary[main_relative], main_dictionary[main_explicit], main_dictionary[main_canonical], main_dictionary[main_passthrough]]
"#;
        let evaluator = eval_build(source, false).unwrap();
        assert_eq!(
            evaluator.get_var("lookup"),
            Some(Value::String("value".to_string()))
        );
        for variable in ["main_relative", "main_explicit", "main_canonical", "main_passthrough"] {
            assert_eq!(evaluator.get_var(variable), evaluator.get_var("main"), "{variable}");
        }
        assert_eq!(evaluator.get_var("main_lookups"), Some(Value::List(vec![
            Value::String("main value".to_string()); 4
        ])));

        for source in [
            r#"label = Label("//pkg:target")
encoded = json.encode({label: 1})
"#,
            r#"label = Label("//pkg:target")
Info = provider(fields = {label: "doc"})
"#,
        ] {
            let error = match eval_build(source, false) {
                Ok(_) => panic!("string-only dictionary consumer must reject Label keys"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("must be strings"),
                "{source}: {error}"
            );
        }
    }

    #[test]
    fn mutable_dictionary_keys_fail_eagerly_in_literals_and_loaded_modules() {
        for source in [
            r#"d = {["x"]: "bad"}"#,
            r#"d = {}
d[["x"]] = "bad"
"#,
        ] {
            let error = match eval(source) {
                Ok(_) => panic!("list dictionary key must be rejected"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("unhashable type: 'list'"), "{error}");
        }

        let definitions = r#"
_values = {}
_values[["x"]] = "bad"
def emit():
    native.filegroup(name = "root")
"#;
        let build = r#"
load("//pkg:defs.bzl", "emit")
emit()
"#;
        for lazy in [false, true] {
            let error = match eval_loaded_build(definitions, build, lazy) {
                Ok(_) => panic!("loaded module initialization must reject a list key"),
                Err(error) => error,
            };
            assert!(format!("{error:#}").contains("unhashable type: 'list'"), "{error:#}");
        }
    }

    #[test]
    fn mixed_display_equal_dictionary_keys_iterate_deterministically() {
        let source = r#"
d = {1: "int", "1": "string", True: "bool", "True": "string-bool"}
keys = d.keys()
"#;
        let expected = eval(source).unwrap().get_var("keys").unwrap();
        for _ in 0..32 {
            assert_eq!(eval(source).unwrap().get_var("keys"), Some(expected.clone()));
        }
        let Value::List(keys) = expected else {
            panic!("keys must be a list")
        };
        assert_eq!(keys.len(), 4);
        assert!(keys.contains(&Value::Int(1)));
        assert!(keys.contains(&Value::String("1".to_string())));
        assert!(keys.contains(&Value::Bool(true)));
        assert!(keys.contains(&Value::String("True".to_string())));
    }

    #[test]
    fn integral_float_and_int_dictionary_keys_share_numeric_identity() {
        let source = r#"
d = {1: "int", 0: "zero"}
d[1.0] = "float"
d[-0.0] = "negative-zero"
length = len(d)
int_lookup = d[1]
float_lookup = d[1.0]
zero_lookup = d[0]
"#;
        let evaluator = eval(source).unwrap();
        assert_eq!(evaluator.get_var("length"), Some(Value::Int(2)));
        assert_eq!(
            evaluator.get_var("int_lookup"),
            Some(Value::String("float".to_string()))
        );
        assert_eq!(
            evaluator.get_var("float_lookup"),
            Some(Value::String("float".to_string()))
        );
        assert_eq!(
            evaluator.get_var("zero_lookup"),
            Some(Value::String("negative-zero".to_string()))
        );
    }

    #[test]
    fn dictionary_literals_reject_numeric_and_tuple_key_duplicates() {
        for source in [
            r#"d = {1: "int", 1.0: "float"}"#,
            r#"d = {("//cfg:a",): [], ("//cfg:a",): []}"#,
            r#"d = {(1, ("x",)): [], tuple([1, ("x",)]): []}"#,
        ] {
            let error = match eval(source) {
                Ok(_) => panic!("literal duplicate must fail eagerly: {source}"),
                Err(error) => error,
            };
            assert!(
                error
                    .to_string()
                    .contains("dictionary expression has duplicate key"),
                "{source}: {error}"
            );
        }
    }

    #[test]
    fn ordinary_tuple_dictionary_keys_remain_distinct() {
        let source = r#"
d = {
    ("cpp", "executable"): "exe",
    ("cpp", "dynamic_library"): "shared",
}
has_exe = ("cpp", "executable") in d
has_archive = ("cpp", "archive") in d
exe = d[("cpp", "executable")]
shared = d[("cpp", "dynamic_library")]
"#;
        let evaluator = eval(source).unwrap();
        assert_eq!(evaluator.get_var("has_exe"), Some(Value::Bool(true)));
        assert_eq!(evaluator.get_var("has_archive"), Some(Value::Bool(false)));
        assert_eq!(evaluator.get_var("exe"), Some(Value::String("exe".to_string())));
        assert_eq!(
            evaluator.get_var("shared"),
            Some(Value::String("shared".to_string()))
        );
    }

    #[test]
    fn test_native_http_archive_binding() {
        let source = r#"
native.http_archive(
    name = "foo",
    url = "https://example.com/foo.zip",
)
"#;
        let mut evaluator = Evaluator::new();
        evaluator.register_native_namespace();
        let program = crate::parse(source).unwrap();
        evaluator.eval_program(&program).unwrap();
        let calls = evaluator.rule_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].rule_type, "http_archive");
        assert_eq!(
            calls[0].kwargs.get("name").and_then(|v| match v {
                Value::String(s) => Some(s.as_str()),
                _ => None,
            }),
            Some("foo")
        );
    }

    #[test]
    fn test_label_workspace_root_attribute() {
        let source = r#"
# Test Label().workspace_root attribute
label = Label("//:WORKSPACE")
ws_root = label.workspace_root
"#;
        let evaluator = eval(source).unwrap();
        // For main workspace, workspace_root should be empty or "."
        assert!(matches!(
            evaluator.get_var("ws_root"),
            Some(Value::String(s)) if s.is_empty() || s == "."
        ));
    }

    #[test]
    fn test_label_external_workspace_root() {
        let source = r#"
# Test Label() with external workspace
label = Label("@foo//bar:baz")
ws_name = label.workspace_name
ws_root = label.workspace_root
"#;
        let evaluator = eval(source).unwrap();
        assert_eq!(
            evaluator.get_var("ws_name"),
            Some(Value::String("foo".to_string()))
        );
        // Match the default execution layout proved by the Bazel 6.5
        // repository-layout oracle; sibling layout is explicitly unsupported.
        assert_eq!(
            evaluator.get_var("ws_root"),
            Some(Value::String("external/foo".to_string()))
        );
    }

    #[test]
    fn test_dict_keys() {
        let source = r#"
d = {"a": 1, "b": 2, "c": 3}
keys = d.keys()
"#;
        let evaluator = eval(source).unwrap();
        let keys_val = evaluator.get_var("keys").unwrap();
        match keys_val {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
                // Check that all keys are present (order may vary in HashMap)
                let key_strings: Vec<String> = items
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        _ => panic!("Expected string key"),
                    })
                    .collect();
                assert!(key_strings.contains(&"a".to_string()));
                assert!(key_strings.contains(&"b".to_string()));
                assert!(key_strings.contains(&"c".to_string()));
            }
            _ => panic!("Expected list, got {:?}", keys_val),
        }
    }

    #[test]
    fn test_dict_values() {
        let source = r#"
d = {"x": 10, "y": 20}
vals = d.values()
"#;
        let evaluator = eval(source).unwrap();
        let vals = evaluator.get_var("vals").unwrap();
        match vals {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                // Values should be the integers 10 and 20 (order may vary)
                assert!(items.contains(&Value::Int(10)));
                assert!(items.contains(&Value::Int(20)));
            }
            _ => panic!("Expected list, got {:?}", vals),
        }
    }

    #[test]
    fn test_dict_items() {
        let source = r#"
d = {"foo": 100, "bar": 200}
items = d.items()
"#;
        let evaluator = eval(source).unwrap();
        let items_val = evaluator.get_var("items").unwrap();
        match items_val {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                // Each item is an immutable (key, value) tuple.
                for item in items {
                    match item {
                        Value::Tuple(pair) => {
                            assert_eq!(pair.len(), 2);
                            // First element should be string key
                            match &pair[0] {
                                Value::String(k) => {
                                    assert!(k == "foo" || k == "bar");
                                }
                                _ => panic!("Expected string key in tuple"),
                            }
                            // Second element should be int value
                            match &pair[1] {
                                Value::Int(v) => {
                                    assert!(*v == 100 || *v == 200);
                                }
                                _ => panic!("Expected int value in tuple"),
                            }
                        }
                        _ => panic!("Expected tuple, got {:?}", item),
                    }
                }
            }
            _ => panic!("Expected list, got {:?}", items_val),
        }
    }

    #[test]
    fn test_dict_items_with_for_loop() {
        let source = r#"
# Test dict.items() in for loop - common TensorFlow pattern
d = {"x": 1, "y": 2}
result = []
for k, v in d.items():
    result = result + [k + "=" + str(v)]
"#;
        let evaluator = eval(source).unwrap();
        let result = evaluator.get_var("result").unwrap();
        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                // Should have "x=1" and "y=2" in some order
                let strings: Vec<String> = items
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        _ => panic!("Expected string"),
                    })
                    .collect();
                assert!(strings.contains(&"x=1".to_string()));
                assert!(strings.contains(&"y=2".to_string()));
            }
            _ => panic!("Expected list, got {:?}", result),
        }
    }

    #[test]
    fn test_list_append() {
        let source = r#"
# Test list.append() - common TensorFlow pattern
my_list = [1, 2, 3]
my_list.append(4)
my_list.append(5)
"#;
        let evaluator = eval(source).unwrap();
        let list_val = evaluator.get_var("my_list").unwrap();
        match list_val {
            Value::List(items) => {
                assert_eq!(items.len(), 5);
                assert_eq!(items[0], Value::Int(1));
                assert_eq!(items[1], Value::Int(2));
                assert_eq!(items[2], Value::Int(3));
                assert_eq!(items[3], Value::Int(4));
                assert_eq!(items[4], Value::Int(5));
            }
            _ => panic!("Expected list, got {:?}", list_val),
        }
    }

    #[test]
    fn test_list_append_string() {
        let source = r#"
# Test list.append() with strings
args = ["--foo"]
args.append("--bar=baz")
"#;
        let evaluator = eval(source).unwrap();
        let args = evaluator.get_var("args").unwrap();
        match args {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], Value::String("--foo".to_string()));
                assert_eq!(items[1], Value::String("--bar=baz".to_string()));
            }
            _ => panic!("Expected list, got {:?}", args),
        }
    }

    #[test]
    fn test_list_extend() {
        let source = r#"
# Test list.extend() - common TensorFlow pattern
base_list = [1, 2]
base_list.extend([3, 4, 5])
"#;
        let evaluator = eval(source).unwrap();
        let list_val = evaluator.get_var("base_list").unwrap();
        match list_val {
            Value::List(items) => {
                assert_eq!(items.len(), 5);
                assert_eq!(items[0], Value::Int(1));
                assert_eq!(items[1], Value::Int(2));
                assert_eq!(items[2], Value::Int(3));
                assert_eq!(items[3], Value::Int(4));
                assert_eq!(items[4], Value::Int(5));
            }
            _ => panic!("Expected list, got {:?}", list_val),
        }
    }

    #[test]
    fn test_list_extend_empty() {
        let source = r#"
# Test list.extend() with empty list
my_list = [1, 2]
my_list.extend([])
"#;
        let evaluator = eval(source).unwrap();
        let list_val = evaluator.get_var("my_list").unwrap();
        match list_val {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], Value::Int(1));
                assert_eq!(items[1], Value::Int(2));
            }
            _ => panic!("Expected list, got {:?}", list_val),
        }
    }

    #[test]
    fn test_list_append_in_loop() {
        let source = r#"
# Test list.append() in loop - TensorFlow pattern
result = []
for i in [1, 2, 3]:
    result.append(i * 2)
"#;
        let evaluator = eval(source).unwrap();
        let result = evaluator.get_var("result").unwrap();
        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Value::Int(2));
                assert_eq!(items[1], Value::Int(4));
                assert_eq!(items[2], Value::Int(6));
            }
            _ => panic!("Expected list, got {:?}", result),
        }
    }

    #[test]
    fn test_dict_get_with_default() {
        let source = r#"
# Test dict.get() with default - TensorFlow pattern
kwargs = {"name": "foo", "srcs": ["a.cc"]}
testonly = kwargs.get("testonly", False)
name = kwargs.get("name", "default")
"#;
        let evaluator = eval(source).unwrap();
        assert_eq!(evaluator.get_var("testonly"), Some(Value::Bool(false)));
        assert_eq!(
            evaluator.get_var("name"),
            Some(Value::String("foo".to_string()))
        );
    }

    #[test]
    fn test_dict_get_without_default() {
        let source = r#"
# Test dict.get() without default (returns None)
d = {"x": 10}
result = d.get("y")
"#;
        let evaluator = eval(source).unwrap();
        assert_eq!(evaluator.get_var("result"), Some(Value::None));
    }

    #[test]
    fn test_dict_pop_with_default() {
        let source = r#"
# Test dict.pop() with default - TensorFlow pattern
kwargs = {"name": "foo", "testonly": True, "deps": []}
testonly = kwargs.pop("testonly", False)
missing = kwargs.pop("missing_key", "default_value")
# After pop, testonly should be removed from kwargs
"#;
        let evaluator = eval(source).unwrap();
        assert_eq!(evaluator.get_var("testonly"), Some(Value::Bool(true)));
        assert_eq!(
            evaluator.get_var("missing"),
            Some(Value::String("default_value".to_string()))
        );

        // Check that testonly was removed from kwargs
        let kwargs = evaluator.get_var("kwargs").unwrap();
        match kwargs {
            Value::Dict(map) => {
                assert!(!map.contains_key("testonly"));
                assert_eq!(map.len(), 2); // Only "name" and "deps" remain
            }
            _ => panic!("Expected dict, got {:?}", kwargs),
        }
    }

    #[test]
    fn test_dict_pop_list_concatenation() {
        let source = r#"
# Test dict.pop() with list concatenation - common TensorFlow pattern
kwargs = {"tags": ["manual"]}
tags = kwargs.pop("tags", []) + ["new_tag"]
"#;
        let evaluator = eval(source).unwrap();
        let tags = evaluator.get_var("tags").unwrap();
        match tags {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], Value::String("manual".to_string()));
                assert_eq!(items[1], Value::String("new_tag".to_string()));
            }
            _ => panic!("Expected list, got {:?}", tags),
        }
    }

    #[test]
    fn test_dict_get_list_concatenation() {
        let source = r#"
# Test dict.get() with list concatenation - TensorFlow pattern
kwargs = {"features": ["-use_header_modules"]}
features = kwargs.get("features", []) + ["-another_feature"]
"#;
        let evaluator = eval(source).unwrap();
        let features = evaluator.get_var("features").unwrap();
        match features {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], Value::String("-use_header_modules".to_string()));
                assert_eq!(items[1], Value::String("-another_feature".to_string()));
            }
            _ => panic!("Expected list, got {:?}", features),
        }
    }

    #[test]
    fn test_list_remove() {
        let source = r#"
# Test list.remove() - TensorFlow pattern
tags = ["manual", "requires-gpu-nvidia", "nightly"]
tags.remove("requires-gpu-nvidia")
"#;
        let evaluator = eval(source).unwrap();
        let tags = evaluator.get_var("tags").unwrap();
        match tags {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], Value::String("manual".to_string()));
                assert_eq!(items[1], Value::String("nightly".to_string()));
                // Verify "requires-gpu-nvidia" was removed
                assert!(!items.contains(&Value::String("requires-gpu-nvidia".to_string())));
            }
            _ => panic!("Expected list, got {:?}", tags),
        }
    }

    #[test]
    fn test_list_remove_first_occurrence() {
        let source = r#"
# Test list.remove() removes only first occurrence
items = [1, 2, 3, 2, 4]
items.remove(2)
"#;
        let evaluator = eval(source).unwrap();
        let items = evaluator.get_var("items").unwrap();
        match items {
            Value::List(vals) => {
                assert_eq!(vals.len(), 4);
                assert_eq!(vals[0], Value::Int(1));
                assert_eq!(vals[1], Value::Int(3));
                assert_eq!(vals[2], Value::Int(2)); // Second 2 remains
                assert_eq!(vals[3], Value::Int(4));
            }
            _ => panic!("Expected list, got {:?}", items),
        }
    }

    #[test]
    fn test_string_format_simple() {
        let source = r#"
# Test string.format() with keyword arguments
msg = "Hello {name}, you are {age} years old".format(name="Alice", age=30)
"#;
        let evaluator = eval(source).unwrap();
        let msg = evaluator.get_var("msg").unwrap();
        assert_eq!(
            msg,
            Value::String("Hello Alice, you are 30 years old".to_string())
        );
    }

    #[test]
    fn test_string_format_tensorflow_pattern() {
        let source = r#"
# Test string.format() - TensorFlow error message pattern
src = "foo/bar.cc"
dep = "baz/qux"
msg = "{src} cannot depend on {dep}".format(src=src, dep=dep)
"#;
        let evaluator = eval(source).unwrap();
        let msg = evaluator.get_var("msg").unwrap();
        assert_eq!(
            msg,
            Value::String("foo/bar.cc cannot depend on baz/qux".to_string())
        );
    }

    #[test]
    fn test_string_format_multiple_placeholders() {
        let source = r#"
# Test string.format() with multiple placeholders
template = "Processing {file} at {line}:{col}".format(file="test.py", line=42, col=10)
"#;
        let evaluator = eval(source).unwrap();
        let template = evaluator.get_var("template").unwrap();
        assert_eq!(
            template,
            Value::String("Processing test.py at 42:10".to_string())
        );
    }

    #[test]
    fn test_string_format_positional_index() {
        // TensorFlow's importexport/tests/roundtrip/roundtrip.bzl uses
        // "{0}.test".format(name). Indices went to the keyword lookup and
        // failed as "Missing format argument: 0", which stubbed the whole
        // package and surfaced later as an undefined symbol at link.
        let source = r#"
name = "roundtrip"
test_file = "a.pbtxt"
a = "{0}.test".format(name)
b = "$(location {0})".format(test_file)
c = "{1} then {0}".format("second", "first")
d = "{0}/{0}".format("dup")
"#;
        let evaluator = eval(source).unwrap();
        assert_eq!(
            evaluator.get_var("a").unwrap(),
            Value::String("roundtrip.test".to_string())
        );
        assert_eq!(
            evaluator.get_var("b").unwrap(),
            Value::String("$(location a.pbtxt)".to_string())
        );
        // An index selects its argument, so order is free and reuse is legal.
        assert_eq!(
            evaluator.get_var("c").unwrap(),
            Value::String("first then second".to_string())
        );
        assert_eq!(
            evaluator.get_var("d").unwrap(),
            Value::String("dup/dup".to_string())
        );
    }

    #[test]
    fn test_string_format_index_out_of_range_errors() {
        // Report the range rather than substituting something wrong.
        let msg = match eval(r#"x = "{2}".format("only-one")"#) {
            Ok(_) => panic!("expected an out-of-range error"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("out of range"), "unexpected error: {msg}");
    }

    #[test]
    fn test_string_format_rejects_mixed_numbering() {
        // Starlark rejects mixing {} with {0}; accepting it would let a
        // .bzl evaluate here that Bazel refuses.
        for src in [r#"x = "{} {0}".format("a", "b")"#, r#"x = "{0} {}".format("a", "b")"#] {
            let msg = match eval(src) {
                Ok(_) => panic!("mixed numbering must be rejected: {src}"),
                Err(e) => e.to_string(),
            };
            assert!(msg.contains("field"), "unexpected error: {msg}");
        }
    }

    #[test]
    fn test_string_format_huge_index_reports_range() {
        // An index too large for usize must report out-of-range, not claim
        // a keyword named after the digits is missing.
        let msg = match eval(r#"x = "{99999999999999999999999}".format("a")"#) {
            Ok(_) => panic!("expected out-of-range"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("out of range"), "unexpected error: {msg}");
    }

    #[test]
    fn test_string_format_brace_escapes() {
        let source = r#"
a = "{{}}".format()
b = "{{{0}}}".format("x")
c = "100{{%}} of {n}".format(n = 3)
"#;
        let evaluator = eval(source).unwrap();
        assert_eq!(
            evaluator.get_var("a").unwrap(),
            Value::String("{}".to_string())
        );
        assert_eq!(
            evaluator.get_var("b").unwrap(),
            Value::String("{x}".to_string())
        );
        assert_eq!(
            evaluator.get_var("c").unwrap(),
            Value::String("100{%} of 3".to_string())
        );
    }

    #[test]
    fn test_dict_update() {
        let source = r#"
# Test dict.update() - TensorFlow pattern
base = {"name": "foo", "srcs": ["a.cc"]}
updates = {"testonly": True, "deps": ["bar"]}
base.update(updates)
"#;
        let evaluator = eval(source).unwrap();
        let base = evaluator.get_var("base").unwrap();
        match base {
            Value::Dict(map) => {
                assert_eq!(map.len(), 4);
                assert_eq!(map.get("name"), Some(&Value::String("foo".to_string())));
                assert_eq!(map.get("testonly"), Some(&Value::Bool(true)));
                assert!(map.contains_key("deps"));
            }
            _ => panic!("Expected dict, got {:?}", base),
        }
    }

    #[test]
    fn test_dict_update_overwrite() {
        let source = r#"
# Test dict.update() overwrites existing keys
d = {"x": 1, "y": 2}
d.update({"y": 20, "z": 3})
"#;
        let evaluator = eval(source).unwrap();
        let d = evaluator.get_var("d").unwrap();
        match d {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                assert_eq!(map.get("x"), Some(&Value::Int(1)));
                assert_eq!(map.get("y"), Some(&Value::Int(20))); // Overwritten
                assert_eq!(map.get("z"), Some(&Value::Int(3)));
            }
            _ => panic!("Expected dict, got {:?}", d),
        }
    }

    #[test]
    fn test_dict_setdefault_new_key() {
        let source = r#"
# Test dict.setdefault() with new key - TensorFlow pattern
kwargs = {"name": "foo"}
srcs_version = kwargs.setdefault("srcs_version", "PY3")
"#;
        let evaluator = eval(source).unwrap();
        let srcs_version = evaluator.get_var("srcs_version").unwrap();
        assert_eq!(srcs_version, Value::String("PY3".to_string()));

        // Check that key was added to dict
        let kwargs = evaluator.get_var("kwargs").unwrap();
        match kwargs {
            Value::Dict(map) => {
                assert_eq!(
                    map.get("srcs_version"),
                    Some(&Value::String("PY3".to_string()))
                );
            }
            _ => panic!("Expected dict, got {:?}", kwargs),
        }
    }

    #[test]
    fn test_dict_setdefault_existing_key() {
        let source = r#"
# Test dict.setdefault() with existing key returns existing value
d = {"x": 10}
result = d.setdefault("x", 999)
"#;
        let evaluator = eval(source).unwrap();
        let result = evaluator.get_var("result").unwrap();
        assert_eq!(result, Value::Int(10)); // Returns existing value, not default

        // Check that dict was not modified
        let d = evaluator.get_var("d").unwrap();
        match d {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                assert_eq!(map.get("x"), Some(&Value::Int(10)));
            }
            _ => panic!("Expected dict, got {:?}", d),
        }
    }

    #[test]
    fn test_dict_setdefault_none_default() {
        let source = r#"
# Test dict.setdefault() with no default (defaults to None)
d = {}
result = d.setdefault("key")
"#;
        let evaluator = eval(source).unwrap();
        let result = evaluator.get_var("result").unwrap();
        assert_eq!(result, Value::None);

        // Check that key was added with None value
        let d = evaluator.get_var("d").unwrap();
        match d {
            Value::Dict(map) => {
                assert_eq!(map.get("key"), Some(&Value::None));
            }
            _ => panic!("Expected dict, got {:?}", d),
        }
    }
}
