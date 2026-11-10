pub mod analysis_context;
pub mod cc_command_spec;
pub mod context;
pub mod generated_io;
pub mod nested_set;
pub mod providers;
pub mod python_runtime;
pub mod registry;
pub mod rules_cc;
pub mod rules_config;
pub mod rules_proto_toolchain;
pub mod rules_python;
pub mod rules_sh;
pub mod rules_system;

// Re-export Python rules
pub use rules_python::{PyBinaryRule, PyLibraryRule, PyTestRule};
// Re-export Shell rules
pub use rules_sh::{ShBinaryRule, ShTestRule};
// Re-export Config rules
pub use rules_config::ConfigSettingRule;
// Re-export System library rules
pub use rules_system::{SystemCcLibraryRule, detect_system_protobuf};

use anyhow::Result;
use async_trait::async_trait;
use context::BuildContext;
use providers::ProviderSet;
use std::collections::HashMap;
use std::path::PathBuf;

/// Value type for rule attributes (simplified from Starlark Value)
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Int(i64),
    Bool(bool),
    List(Vec<Value>),
    Dict(HashMap<String, Value>),
    None,
}

impl Value {
    pub fn as_string(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            // Bazel treats integer 1/0 as truthy/falsy in boolean
            // attribute contexts (TF BUILD files routinely write
            // `has_services = 1` rather than `has_services = True`).
            Value::Int(n) => Some(*n != 0),
            // Rush's Starlark evaluator currently surfaces integer
            // attribute literals as `String("1")` / `String("0")`
            // when the BUILD file context types them numerically — so
            // accept those textual forms too (and the obvious "true"/
            // "false" / "yes"/"no" spellings). Without this branch,
            // attrs like `has_services = 1` return None and downstream
            // code silently defaults to false, skipping flags that
            // gate code generation (grpc, proto-text, etc.). See
            // memory/feedback_no_fake_stubs.md "silent skip".
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&Vec<Value>> {
        match self {
            Value::List(list) => Some(list),
            _ => None,
        }
    }
}

/// Artifacts produced by a build rule
#[derive(Debug, Clone)]
pub struct Artifact {
    pub path: PathBuf,
    pub artifact_type: ArtifactType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactType {
    Source,
    Object,
    StaticLibrary,
    SharedLibrary,
    Executable,
    Header,
    Generated,
}

/// Build rule trait - all rules must implement this
#[async_trait]
pub trait BuildRule: Send + Sync {
    /// Rule name (e.g., "cc_library", "tf_cc_library")
    fn name(&self) -> &str;

    /// Parse rule attributes from attribute map
    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>>;

    /// Get rule documentation
    fn documentation(&self) -> &str {
        "No documentation available"
    }
}

/// Instance of a rule with specific attributes
#[async_trait]
pub trait RuleInstance: Send + Sync {
    /// Get the target name
    fn target_name(&self) -> &str;

    /// Get dependencies
    fn dependencies(&self) -> Vec<String>;

    /// Literal options-consumer declaration, when implemented by this rule.
    /// The orchestrator separately qualifies source and configured providers.
    fn options_header_declaration(&self) -> Option<providers::OptionsHeaderDeclaration> {
        None
    }

    /// Resolved visibility specifications for this target.
    ///
    /// `None` means no visibility list was resolved: the package has no
    /// default, and the target omitted visibility or set it to `None`. The
    /// dependency graph applies Bazel's private fallback to ordinary rule
    /// targets. `Some(&[])` is distinct and also grants no cross-package
    /// access. The rule-execution bridge wraps instances with the target's
    /// explicit visibility or package default so individual rule
    /// implementations do not need to own this common Bazel attribute.
    fn visibility(&self) -> Option<&[String]> {
        None
    }

    /// Whether this target wrote an explicit, non-`None` visibility attribute.
    ///
    /// This differs from [`visibility`](Self::visibility), which may contain a
    /// package default. Bazel's config-setting migration mode treats omitted
    /// visibility as public even when the package default is private, so the
    /// dependency graph must retain the origin of the resolved value.
    fn visibility_was_explicit(&self) -> bool {
        false
    }

    /// Labels used as keys by configurable `select()` expressions on this
    /// target. They are analysis metadata edges: Rush loads and validates the
    /// referenced conditions without treating them as provider/build-order
    /// dependencies.
    fn configuration_conditions(&self) -> &[String] {
        &[]
    }

    /// Constraint-value labels named by a native `config_setting`.
    ///
    /// These are configuration metadata edges rather than build dependencies,
    /// but the graph must load them and require `constraint_value` targets so a
    /// misspelling or wrong-typed label cannot silently affect `select()`.
    fn configuration_constraint_values(&self) -> &[String] {
        &[]
    }

    /// Constraint-setting label owned by a native `constraint_value`.
    ///
    /// This is configuration metadata, not a provider/build-order edge. The
    /// dependency graph loads it when the value enters the configured closure
    /// and verifies that aliases terminate at a `constraint_setting` target.
    fn configuration_constraint_setting(&self) -> Option<&str> {
        None
    }

    /// Whether this instance is a native `alias()` that may wrap a select key.
    /// Provider-forwarding rules such as `label_flag` are not aliases for
    /// configuration-condition validation even when they share execution
    /// plumbing with ordinary aliases.
    fn is_configuration_alias(&self) -> bool {
        false
    }

    /// Whether this target can be used as a `select()` condition.
    ///
    /// Native `config_setting` and `constraint_value` targets provide this
    /// capability, as can narrowly identified Starlark rules such as Skylib's
    /// `config_setting_group`. Checking a capability instead of a rule-name
    /// allowlist mirrors Bazel's ConfigMatchingProvider contract without
    /// accepting every graph-only custom rule as a condition.
    fn is_configuration_condition(&self) -> bool {
        false
    }

    /// Whether a directly incompatible target suppresses visibility checks on
    /// the `select()` keys used by its other attributes.
    ///
    /// Bazel still loads and type-checks those keys before constructing the
    /// incompatible configured target, but returns before its ordinary
    /// config-condition visibility loop. Only the dedicated incompatible
    /// placeholder overrides this hook.
    fn suppresses_configuration_condition_visibility(&self) -> bool {
        false
    }

    /// Package specifications declared by a native `package_group` target.
    /// Non-package-group rules return `None`; an empty package group returns
    /// `Some(&[])` and therefore grants no package access by itself.
    fn package_group_packages(&self) -> Option<&[String]> {
        None
    }

    /// Labels of package groups transitively included by this package group.
    fn package_group_includes(&self) -> Option<&[String]> {
        None
    }

    /// Exec-config (tool) subset of [`dependencies`] — edges that exist so
    /// this target can be BUILT (a genrule/tablegen tool, etc.) but whose
    /// providers and artifacts must NOT flow into a TARGET-config consumer's
    /// runtime closure. Bazel models this via `cfg = "exec"`; rush
    /// approximates it per-rule. Default: none (every dep is target-config).
    ///
    /// The orchestrator excludes these from a consumer's transitive artifact
    /// NestedSet and from the provider slice passed to `analyze()`, while
    /// still building them first (they remain in [`dependencies`]). This
    /// prevents a tool cc_library that carries `main()` (e.g.
    /// `//tensorflow/cc:cc_op_gen_main`, or an mlir-tblgen tool) from leaking
    /// into a `cc_test` link and shadowing `gtest_main`.
    fn exec_only_dependencies(&self) -> Vec<String> {
        Vec::new()
    }

    /// Workspace source files this target consumes WITHOUT declaring an
    /// action for them, as package-relative or label strings.
    ///
    /// The freshness ledger builds its path universe from declared
    /// actions plus registered source artifacts. A rule that runs its
    /// tool directly in `build()` — genrule is the canonical case — may
    /// declare an action with an incomplete input set, so its `srcs`
    /// entered neither set: editing one was invisible, the no-op
    /// certificate reported "workspace unchanged", and the build served
    /// a stale output with no error. Rules in that shape report their
    /// inputs here so change detection can see them.
    ///
    /// Default: none, which is correct for every rule whose inputs are
    /// already declared action inputs.
    fn undeclared_source_inputs(&self) -> Vec<String> {
        Vec::new()
    }

    /// Files this rule DECLARES it will generate in the package genfiles
    /// namespace, as package-relative names (a genrule's `outs`, and anything
    /// else that materializes a label-addressable package-genfiles file).
    ///
    /// Bazel lets a label name a generated FILE rather than a target:
    ///
    /// ```python
    /// genrule(name = "merge", outs = ["conversion_data.c"], ...)
    /// cc_library(name = "data", srcs = [":conversion_data.c"])
    /// ```
    ///
    /// `:conversion_data.c` is not a target — it is the genrule's output,
    /// and Bazel resolves it through an output-file-to-producer map. Rush
    /// looked for a target of that name, did not find one, and failed the
    /// build with "which doesn't exist in the graph". Reporting outputs
    /// here lets the graph builder resolve such a label to the rule that
    /// produces it.
    ///
    /// Default: none, which is correct for every rule whose outputs are
    /// named only by its own target label.
    fn declared_outputs(&self) -> Vec<String> {
        Vec::new()
    }

    /// A configuration-time error retained while loading the package.
    ///
    /// Bazel permits an unconfigurable target to coexist with usable siblings,
    /// but fails as soon as that target enters the requested configured target
    /// closure. The orchestrator checks this hook over its scoped build order
    /// before analysis, so `--analyze-only` and normal builds report the same
    /// error without forcing every rule's `analyze()` API to become fallible.
    fn configuration_error(&self) -> Option<&str> {
        None
    }

    /// True only when the retained error is the deferred flag_values boundary.
    /// Graph validation may discharge it using exact per-command Bazel proof.
    fn configuration_error_needs_feature_flag_proof(&self) -> bool { false }

    /// Validate dependency-sensitive attributes immediately before analysis.
    ///
    /// Most rule attributes are fully checked while the BUILD file is parsed.
    /// A few Bazel contracts also depend on the providers of an exact labeled
    /// prerequisite. This fallible hook runs after those providers have been
    /// assembled but before the infallible [`analyze`] API declares actions,
    /// so malformed configured targets fail equally under normal builds and
    /// `--analyze-only` instead of becoming incomplete action graphs.
    fn validate_analysis_dependencies(
        &self,
        _ctx: &crate::analysis_context::AnalysisContext,
    ) -> Result<()> {
        Ok(())
    }

    /// Build this target
    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>>;

    /// Get provider information (for dependent targets).
    ///
    /// Returns a typed [`ProviderSet`] (phase 5.A — replacing
    /// the previous `HashMap<String, Provider>`). Well-known
    /// providers (CcInfo, CcSharedLibraryInfo, FilesInfo) live
    /// in named slots; user-defined providers go through the
    /// `custom` map. Empty by default — most rules don't expose
    /// providers to dependents.
    ///
    /// Phase 5.B migration: prefer overriding [`analyze`] for
    /// rules that produce CcInfo. `providers()` stays as the
    /// fallback for rules that have no typed-context migration
    /// (or that produce only FilesInfo / Custom).
    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }

    /// Phase 5.B.3 typed-analysis entry point.
    ///
    /// Default impl delegates to [`providers`] for backwards
    /// compatibility with rules that haven't been migrated to the
    /// `AnalysisContext` + `CcInfo::from_direct_and_deps` shape
    /// yet. Migrated rules (cc_library is first, in 5.B.3.b)
    /// override this to:
    ///
    ///   - resolve their direct `includes = [...]` attrs through
    ///     `ctx.resolve_package_include()` BEFORE provider merge
    ///     (Q6 fix);
    ///   - register source/generated artifacts via `ctx.register_*`
    ///     so the registry's `ConfigId`-keyed collision protection
    ///     applies (Q5 fix);
    ///   - call `CcInfo::from_direct_and_deps(direct, deps)` to
    ///     build the typed `compilation` and `linking` contexts
    ///     that propagate via `Arc::clone`-shared `NestedSet` /
    ///     `OrderedNestedSet` children (5.B.3.a).
    ///
    /// `dep_providers` is the dep ProviderSet `Arc`s the
    /// orchestrator has already built (from `built_providers`).
    /// Each rule extracts the typed providers it cares about
    /// (CcInfo, ProtoFilesInfo, …) from the slice. Phase 5.C0 P2
    /// expanded this from a CcInfo-only slice so cc_proto_library
    /// can enumerate dep .proto sources via the typed
    /// `ProtoFilesInfo` path without raw artifact NestedSet
    /// walks.
    fn analyze(
        &self,
        _ctx: &mut crate::analysis_context::AnalysisContext,
        _dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        self.providers()
    }

    /// Get the rule type name (e.g., "cc_library", "tf_proto_library")
    fn rule_type(&self) -> &str {
        "unknown"
    }

    /// Phase 5.C.4 commit 4: build legacy `NormalizedCommandSpec`s
    /// for the actions this rule would execute via the legacy
    /// `build()` path. Used by `--verify-scheduler-commands` to
    /// diff against the scheduler's materialized specs.
    ///
    /// Returns `Vec<(SchedulerActionKey, NormalizedCommandSpec)>`
    /// where `SchedulerActionKey` is `(mnemonic, primary_output_path)`
    /// — the verifier matches scheduler actions back to legacy
    /// specs by this key.
    ///
    /// Default impl returns an empty Vec, meaning "this rule kind
    /// isn't covered by the verifier." cc_library and cc_binary
    /// override in this commit; tf_cc_library / cc_test / etc.
    /// override later when their migrations land.
    ///
    /// Async because legacy spec construction needs to read the
    /// shared `artifact_registry` for dep `CcInfo.compilation.include_dirs`
    /// — same pattern as the legacy `compile_source` /
    /// `link_binary` functions.
    async fn build_legacy_command_specs(
        &self,
        _context: &context::BuildContext,
    ) -> Vec<(SchedulerActionKey, rush_action::command_spec::NormalizedCommandSpec)> {
        Vec::new()
    }

    /// Phase 5.C.4 commit 31a: per-target shape eligibility.
    /// Returns `None` when the rule's specific shape (attribute
    /// combination) is scheduler-verified; returns `Some(reason)`
    /// when the rule kind is on the allowlist but THIS target
    /// carries an unverified shape.
    ///
    /// Pro's commit-31a directive: "rule-kind allowlist alone is
    /// too blunt. tf_proto_library is allowlisted but grpc
    /// support is deferred. Add a shape predicate before any
    /// grpc target can accidentally go scheduler path."
    ///
    /// The default impl returns `None` — most rules have a
    /// uniform shape (cc_library is verified for all attribute
    /// combinations). Rules with attribute-driven shape variants
    /// override:
    ///
    ///   - `tf_proto_library` returns `Some(...)` when
    ///     `has_services` or `create_grpc_library` is set, since
    ///     the grpc path lacks runtime verifier coverage as of
    ///     commit 31a.
    ///   - Future migrations add their own returns when a shape
    ///     is allowlisted-but-unverified.
    ///
    /// The orchestrator's eligibility predicate consumes this
    /// return: a `Some` blocks dispatch with
    /// `SchedulerEligibilityReason::UnsupportedShape`. The reason
    /// string is `&'static str` — fixed at compile time, no
    /// per-target interpolation. If you need to interpolate,
    /// use a const string per shape.
    fn scheduler_shape_unsupported(&self) -> Option<&'static str> {
        None
    }

    /// Phase 5.C.4 commit 55: declare the host tools this
    /// rule's analyze() / build() needs at action execution
    /// time.
    ///
    /// Replaces the orchestrator's hardcoded
    /// `required_tool_for_rule` map (commit 48) with rule-
    /// local declarations. Pro's directive: "Rule-local
    /// declaration via trait method is the right shape.
    /// Third-party rules, shape-specific tools, and future
    /// toolchain work all want rule-owned declarations, not
    /// another global switchboard."
    ///
    /// Default: `Vec::new()` — rules declare nothing. Most
    /// rule types (cc_library, cc_binary, cc_test) compose
    /// from primitives (cxx, ar, ld) the orchestrator probes
    /// independently; they don't need to declare. Codegen-
    /// bearing rules (cc_proto_library, tf_proto_library,
    /// gentbl_cc_library) override and declare the codegen
    /// tools they depend on.
    ///
    /// Shape-specific overrides are encouraged: a single
    /// rule kind can return different `RequiredTool` lists
    /// based on attributes (tf_proto_library returns
    /// `[Protoc]` for non-grpc, `[Protoc, GrpcCppPlugin]`
    /// when has_services / create_grpc_library is set).
    fn scheduler_required_tools(&self) -> Vec<RequiredTool> {
        Vec::new()
    }
}

/// Attach resolved target visibility to an arbitrary rule instance.
///
/// Visibility is a common Bazel attribute, but Rush's rule implementations
/// intentionally own only rule-specific execution data. This transparent
/// decorator preserves that separation while exposing the parsed metadata at
/// the dependency-graph boundary.
pub fn with_visibility(
    inner: Box<dyn RuleInstance>,
    visibility: Option<Vec<String>>,
) -> Box<dyn RuleInstance> {
    let visibility_was_explicit = visibility.is_some();
    with_rule_metadata(
        inner,
        visibility,
        visibility_was_explicit,
        Vec::new(),
    )
}

/// Attach common loading/analysis metadata to an arbitrary rule instance.
pub fn with_rule_metadata(
    inner: Box<dyn RuleInstance>,
    visibility: Option<Vec<String>>,
    visibility_was_explicit: bool,
    configuration_conditions: Vec<String>,
) -> Box<dyn RuleInstance> {
    if visibility.is_none()
        && !visibility_was_explicit
        && configuration_conditions.is_empty()
    {
        return inner;
    }
    Box::new(RuleMetadataInstance {
        inner,
        visibility,
        visibility_was_explicit,
        configuration_conditions,
    })
}

struct RuleMetadataInstance {
    inner: Box<dyn RuleInstance>,
    visibility: Option<Vec<String>>,
    visibility_was_explicit: bool,
    configuration_conditions: Vec<String>,
}

#[async_trait]
impl RuleInstance for RuleMetadataInstance {
    fn target_name(&self) -> &str {
        self.inner.target_name()
    }

    fn dependencies(&self) -> Vec<String> {
        self.inner.dependencies()
    }

    fn options_header_declaration(&self) -> Option<providers::OptionsHeaderDeclaration> {
        self.inner.options_header_declaration()
    }

    fn visibility(&self) -> Option<&[String]> {
        self.visibility
            .as_deref()
            .or_else(|| self.inner.visibility())
    }

    fn visibility_was_explicit(&self) -> bool {
        self.visibility_was_explicit || self.inner.visibility_was_explicit()
    }

    fn configuration_conditions(&self) -> &[String] {
        if self.configuration_conditions.is_empty() {
            self.inner.configuration_conditions()
        } else {
            &self.configuration_conditions
        }
    }

    fn configuration_constraint_values(&self) -> &[String] {
        self.inner.configuration_constraint_values()
    }

    fn configuration_constraint_setting(&self) -> Option<&str> {
        self.inner.configuration_constraint_setting()
    }

    fn is_configuration_alias(&self) -> bool {
        self.inner.is_configuration_alias()
    }

    fn is_configuration_condition(&self) -> bool {
        self.inner.is_configuration_condition()
    }

    fn suppresses_configuration_condition_visibility(&self) -> bool {
        self.inner
            .suppresses_configuration_condition_visibility()
    }

    fn package_group_packages(&self) -> Option<&[String]> {
        self.inner.package_group_packages()
    }

    fn package_group_includes(&self) -> Option<&[String]> {
        self.inner.package_group_includes()
    }

    fn exec_only_dependencies(&self) -> Vec<String> {
        self.inner.exec_only_dependencies()
    }

    fn undeclared_source_inputs(&self) -> Vec<String> {
        self.inner.undeclared_source_inputs()
    }

    fn declared_outputs(&self) -> Vec<String> {
        self.inner.declared_outputs()
    }

    fn configuration_error(&self) -> Option<&str> {
        self.inner.configuration_error()
    }

    fn configuration_error_needs_feature_flag_proof(&self) -> bool {
        self.inner.configuration_error_needs_feature_flag_proof()
    }

    fn validate_analysis_dependencies(
        &self,
        ctx: &crate::analysis_context::AnalysisContext,
    ) -> Result<()> {
        self.inner.validate_analysis_dependencies(ctx)
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        self.inner.build(context).await
    }

    fn providers(&self) -> ProviderSet {
        self.inner.providers()
    }

    fn analyze(
        &self,
        ctx: &mut crate::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        self.inner.analyze(ctx, dep_providers)
    }

    fn rule_type(&self) -> &str {
        self.inner.rule_type()
    }

    async fn build_legacy_command_specs(
        &self,
        context: &context::BuildContext,
    ) -> Vec<(SchedulerActionKey, rush_action::command_spec::NormalizedCommandSpec)> {
        self.inner.build_legacy_command_specs(context).await
    }

    fn scheduler_shape_unsupported(&self) -> Option<&'static str> {
        self.inner.scheduler_shape_unsupported()
    }

    fn scheduler_required_tools(&self) -> Vec<RequiredTool> {
        self.inner.scheduler_required_tools()
    }
}

/// Phase 5.C.4 commit 55: well-known host tools that rules
/// can declare via `RuleInstance::scheduler_required_tools`.
///
/// Variants intentionally mirror `rush_resource::ToolId`
/// (orchestrator-side cache key) but live in `rush-rules`
/// so the trait stays dep-clean. The orchestrator maps
/// RequiredTool → ToolId at the eligibility-predicate
/// boundary.
///
/// Adding a new tool means extending this enum AND the
/// orchestrator's mapping function. Strings would skip the
/// mapping but lose exhaustive-match correctness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredTool {
    /// `protoc` — Protocol Buffers compiler.
    Protoc,
    /// `grpc_cpp_plugin` — gRPC code generator (protoc plugin).
    GrpcCppPlugin,
    /// `mlir-tblgen` — MLIR table generator.
    MlirTblgen,
    /// `llvm-tblgen` — LLVM table generator.
    LlvmTblgen,
    /// System C++ compiler (g++ / clang++).
    Cxx,
    /// `ar` — archiver.
    Ar,
    /// Linker — invoked directly for shared/static link.
    Linker,
}

impl RequiredTool {
    /// Phase 5.C.4 commit 55: human-readable tool name. Used
    /// by the orchestrator's MissingTool eligibility reason
    /// surface ("install protoc", "install grpc_cpp_plugin").
    pub fn name(&self) -> &'static str {
        match self {
            RequiredTool::Protoc => "protoc",
            RequiredTool::GrpcCppPlugin => "grpc_cpp_plugin",
            RequiredTool::MlirTblgen => "mlir-tblgen",
            RequiredTool::LlvmTblgen => "llvm-tblgen",
            RequiredTool::Cxx => "cxx",
            RequiredTool::Ar => "ar",
            RequiredTool::Linker => "ld",
        }
    }

    /// Phase 5.C.4 commit 61: lookup by tool name string.
    /// Inverse of `name()`. Returns `None` for unknown
    /// names. Used by the action-graph explain wire-up
    /// to convert
    /// [`SchedulerEligibilityReason::MissingTool`]'s static-
    /// str payload into the canonical RequiredTool variant
    /// for [`SchedulerSupportLevel::MissingTool`].
    pub fn from_name(name: &str) -> Option<RequiredTool> {
        match name {
            "protoc" => Some(RequiredTool::Protoc),
            "grpc_cpp_plugin" => Some(RequiredTool::GrpcCppPlugin),
            "mlir-tblgen" => Some(RequiredTool::MlirTblgen),
            "llvm-tblgen" => Some(RequiredTool::LlvmTblgen),
            "cxx" => Some(RequiredTool::Cxx),
            "ar" => Some(RequiredTool::Ar),
            "ld" => Some(RequiredTool::Linker),
            _ => None,
        }
    }
}

/// Phase 5.C.4 commit 56: unified vocabulary for "what's
/// the scheduler-support state of this target?"
///
/// Pro's directive: "Right now some of those concepts exist
/// in parallel but not canonically. Unify the language
/// before 5.C.5 or the cache work, otherwise you will end
/// up with four slightly different 'not runnable' concepts
/// arguing in logs."
///
/// Replaces the parallel vocabularies that had grown across
/// preflight ([`SchedulerEligibilityReason`] in rush-build),
/// legacy census ([`SchedulerVerdict`] in rush-action), and
/// future action-graph explain. All those surfaces map
/// their internal types into this enum for display + JSON
/// output, so CI consumers and operators see one
/// vocabulary.
///
/// Variants intentionally MIRROR
/// `SchedulerEligibilityReason` (rush-build) plus
/// `LegacyRequired` for the legacy-census case where a
/// target's rule kind is allowlisted but the target's
/// actions need rule.build() (e.g. system_cc_library
/// header pseudo-rules).
///
/// JSON shape (consistent with commit 54b's enum
/// normalization):
///
///   {"kind": "verified"}
///   {"kind": "rule_kind_unverified"}
///   {"kind": "unsupported_shape", "data": {"reason": "..."}}
///   {"kind": "missing_tool", "data": {"tool": "protoc"}}
///   {"kind": "action_graph_incomplete"}
///   {"kind": "legacy_required", "data": {"reason": "..."}}
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum SchedulerSupportLevel {
    /// Target is on the allowlist, shape is verified, action
    /// graph is complete, all required tools usable. The
    /// scheduler will dispatch.
    Verified,
    /// Rule kind is not on the verified allowlist. Hard
    /// floor; even installing tools or completing the action
    /// graph wouldn't unblock dispatch until the rule kind
    /// gains verifier coverage.
    RuleKindUnverified,
    /// Rule kind is allowlisted but THIS target carries an
    /// attribute combination whose shape hasn't been
    /// verifier-zero-diff-verified
    /// (e.g. tf_proto_library with grpc enabled). Migration
    /// concern.
    UnsupportedShape {
        /// Reason string from the rule's
        /// `scheduler_shape_unsupported`. Static — no
        /// per-target interpolation.
        reason: &'static str,
    },
    /// Rule kind allowlisted, shape supported, but a required
    /// host tool is missing or unusable. Install concern,
    /// distinct from migration gaps.
    MissingTool {
        /// The first unusable required tool. (When multiple
        /// tools are unusable, the `compute_missing_tool`
        /// helper picks declaration-order-first; this carries
        /// that pick.)
        tool: RequiredTool,
    },
    /// Above gates clean, but the action-level Ready set
    /// doesn't include this target. Structural gap (missing
    /// ExecutableAction, Incomplete-status).
    ActionGraphIncomplete,
    /// Target needs to take the legacy `rule.build()` path
    /// regardless of the gates above (e.g. system_cc_library
    /// header pseudo-rules; future legacy census uses this
    /// for "scheduler-eligible but legacy-built today"
    /// targets).
    LegacyRequired {
        /// Reason string explaining why legacy is needed.
        reason: &'static str,
    },
    /// Phase 5.C.4 Batch ABR-2 (commit 215): rule kind +
    /// shape + tools are clean, but a source artifact in the
    /// target's transitive closure is not materialized on
    /// disk. Distinct from `ActionGraphIncomplete` (no
    /// executable wiring) and `MissingTool` (host tool
    /// missing). The reason string names the blocker
    /// category from the
    /// `MaterializationCompletenessReport`; payload-level
    /// detail lives in the report itself.
    ExternalRepoSourceMissing {
        /// Static reason category (e.g.
        /// `"external repo source file missing"`,
        /// `"external repo directory not materialized"`).
        reason: &'static str,
    },
    /// Phase 5.C.4 Batch AJ2-2 (commit 232): target is a
    /// Rush-generated EXTERNAL STUB cc_library. The stub
    /// redirects to a system library (`linkopts = ["-lXXX"]`)
    /// or provides only header includes — distinct from
    /// `ExternalRepoSourceMissing` (which signals an
    /// incomplete fetch / missing payload).
    ///
    /// Per Pro's 2026-05-21 stub-aware-lane directive: this
    /// is INTENTIONAL behavior, not a failure. The scheduler
    /// treats stubs as provider-only / no-action targets
    /// (closer to `system_cc_library`) — they participate in
    /// the dep graph for link/include propagation but have
    /// nothing to compile.
    ///
    /// MUST NOT be collapsed into:
    ///   - `ActionGraphIncomplete` (the action graph is
    ///     intentionally empty, not incomplete)
    ///   - `MissingTool` (no host tool is missing)
    ///   - `RuleKindUnverified` (cc_library IS allowlisted —
    ///     the rule kind is fine, the SHAPE is the stub)
    ///   - `ExternalRepoSourceMissing` (no source is
    ///     missing — the stub intentionally declares none)
    ExternalSystemStub {
        /// Stub kind label (e.g. `"system_library"`,
        /// `"header_only"`, `"feature_disabled"`,
        /// `"python_stub"`, `"misc"`). Comes from
        /// `ExternalRepoStubKind::label()`.
        kind: &'static str,
    },
}

impl SchedulerSupportLevel {
    /// Phase 5.C.4 commit 56: human-readable label for
    /// non-JSON display. format_action_key_spec /
    /// format_preflight_report use this for inline strings.
    pub fn label(&self) -> String {
        match self {
            SchedulerSupportLevel::Verified => "Verified".to_string(),
            SchedulerSupportLevel::RuleKindUnverified => {
                "RuleKindUnverified".to_string()
            }
            SchedulerSupportLevel::UnsupportedShape { reason } => {
                format!("UnsupportedShape({})", reason)
            }
            SchedulerSupportLevel::MissingTool { tool } => {
                format!("MissingTool({})", tool.name())
            }
            SchedulerSupportLevel::ActionGraphIncomplete => {
                "ActionGraphIncomplete".to_string()
            }
            SchedulerSupportLevel::LegacyRequired { reason } => {
                format!("LegacyRequired({})", reason)
            }
            SchedulerSupportLevel::ExternalRepoSourceMissing { reason } => {
                format!("ExternalRepoSourceMissing({})", reason)
            }
            SchedulerSupportLevel::ExternalSystemStub { kind } => {
                format!("ExternalSystemStub({})", kind)
            }
        }
    }

    /// Phase 5.C.4 commit 56: is this target dispatch-ready?
    /// True iff Verified or ExternalSystemStub.
    /// ExternalSystemStub (AJ2-2 commit 232): stubs dispatch
    /// as provider-only no-ops — they have no actions to
    /// execute but participate in the dep graph for include
    /// and link propagation. The scheduler "handles" them by
    /// doing nothing, mirroring the NoDeclaredActions →
    /// Verified mapping from commit 206.
    pub fn is_dispatchable(&self) -> bool {
        matches!(
            self,
            SchedulerSupportLevel::Verified
                | SchedulerSupportLevel::ExternalSystemStub { .. }
        )
    }
}

/// Phase 5.C.4 commit 4: matches a scheduler-side action to its
/// legacy counterpart. The (mnemonic, primary_output_path) pair
/// is unique within a target — every CppCompile produces exactly
/// one .o, every CcArchive produces exactly one .a, every CcLink
/// produces exactly one binary.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct SchedulerActionKey {
    pub mnemonic: String,
    pub primary_output_path: std::path::PathBuf,
}

/// Helper function to extract string from attributes
pub fn extract_string(attrs: &HashMap<String, Value>, key: &str) -> Result<String> {
    attrs
        .get(key)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Missing or invalid attribute: {}", key))
}

/// Helper function to extract optional string from attributes
pub fn extract_string_opt(attrs: &HashMap<String, Value>, key: &str) -> Option<String> {
    attrs
        .get(key)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
}

/// Helper function to extract string list from attributes
pub fn extract_string_list(attrs: &HashMap<String, Value>, key: &str) -> Result<Vec<String>> {
    match attrs.get(key) {
        Some(Value::List(items)) => items
            .iter()
            .map(|v| {
                v.as_string()
                    .map(|s| s.to_string())
                    .ok_or_else(|| anyhow::anyhow!("List item is not a string"))
            })
            .collect(),
        // Handle None values
        Some(Value::None) => Ok(Vec::new()),
        // For other types (String used as placeholder, Dict for select, etc.), return empty
        // This handles cases where macros produce complex expressions that will be resolved later
        // Also handle literal "None" string which can occur from unresolved variables
        Some(Value::String(s)) if s.starts_with("select(") || s.contains("_copts") || s == "None" => {
            Ok(Vec::new())
        }
        Some(Value::Dict(_)) => {
            // Dict might be an unresolved select, return empty
            Ok(Vec::new())
        }
        Some(other) => Err(anyhow::anyhow!(
            "Attribute {} is not a list (got {:?})",
            key,
            other
        )),
        None => Ok(Vec::new()),
    }
}

/// Helper function to extract optional boolean from attributes
pub fn extract_bool(attrs: &HashMap<String, Value>, key: &str) -> Result<Option<bool>> {
    match attrs.get(key) {
        Some(value) => Ok(value.as_bool()),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_string() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("test".to_string()));

        let result = extract_string(&attrs, "name").unwrap();
        assert_eq!(result, "test");
    }

    #[test]
    fn test_extract_string_list() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "srcs".to_string(),
            Value::List(vec![
                Value::String("a.cc".to_string()),
                Value::String("b.cc".to_string()),
            ]),
        );

        let result = extract_string_list(&attrs, "srcs").unwrap();
        assert_eq!(result, vec!["a.cc", "b.cc"]);
    }

    // Phase 5.C.4 commit 56: SchedulerSupportLevel tests.
    // Pin canonical labels + dispatch predicate + JSON shape.

    #[test]
    fn support_level_label_round_trips() {
        assert_eq!(SchedulerSupportLevel::Verified.label(), "Verified");
        assert_eq!(
            SchedulerSupportLevel::RuleKindUnverified.label(),
            "RuleKindUnverified"
        );
        assert_eq!(
            SchedulerSupportLevel::UnsupportedShape { reason: "grpc" }.label(),
            "UnsupportedShape(grpc)"
        );
        assert_eq!(
            SchedulerSupportLevel::MissingTool {
                tool: RequiredTool::Protoc
            }
            .label(),
            "MissingTool(protoc)"
        );
        assert_eq!(
            SchedulerSupportLevel::ActionGraphIncomplete.label(),
            "ActionGraphIncomplete"
        );
        assert_eq!(
            SchedulerSupportLevel::LegacyRequired {
                reason: "header pseudo"
            }
            .label(),
            "LegacyRequired(header pseudo)"
        );
        // Phase 5.C.4 Batch ABR-2 (commit 215): new
        // ExternalRepoSourceMissing variant. Label includes
        // the static reason category.
        assert_eq!(
            SchedulerSupportLevel::ExternalRepoSourceMissing {
                reason: "external repo source file missing"
            }
            .label(),
            "ExternalRepoSourceMissing(external repo source file missing)"
        );
    }

    #[test]
    fn support_level_only_verified_is_dispatchable() {
        assert!(SchedulerSupportLevel::Verified.is_dispatchable());
        assert!(!SchedulerSupportLevel::RuleKindUnverified.is_dispatchable());
        assert!(!SchedulerSupportLevel::UnsupportedShape { reason: "x" }
            .is_dispatchable());
        assert!(!SchedulerSupportLevel::MissingTool {
            tool: RequiredTool::Protoc
        }
        .is_dispatchable());
        assert!(!SchedulerSupportLevel::ActionGraphIncomplete.is_dispatchable());
        assert!(!SchedulerSupportLevel::LegacyRequired { reason: "x" }
            .is_dispatchable());
        // Phase 5.C.4 Batch ABR-2 (commit 215): the new
        // blocker variant must NOT be dispatchable.
        assert!(!SchedulerSupportLevel::ExternalRepoSourceMissing {
            reason: "external repo source file missing"
        }
        .is_dispatchable());
    }

    /// Phase 5.C.4 Batch ABR-2 (commit 215): the new
    /// SchedulerSupportLevel variant serializes with the
    /// canonical kind tag and reason payload.
    #[test]
    fn support_level_external_repo_source_missing_json_shape() {
        let v = SchedulerSupportLevel::ExternalRepoSourceMissing {
            reason: "external repo source file missing",
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
        assert_eq!(parsed["kind"], "external_repo_source_missing");
        assert_eq!(
            parsed["data"]["reason"],
            "external repo source file missing"
        );
    }

    // AJ2-2 SchedulerSupportLevel::ExternalSystemStub tests

    /// AJ2-2 Pro directive test: ExternalSystemStub is NOT
    /// ActionGraphIncomplete. They are different variants
    /// even though both signal "no actions to execute" —
    /// stubs are INTENTIONAL no-action, action_graph_incomplete
    /// is a structural gap.
    #[test]
    fn aj2_system_stub_not_action_graph_incomplete() {
        let stub =
            SchedulerSupportLevel::ExternalSystemStub { kind: "system_library" };
        let agi = SchedulerSupportLevel::ActionGraphIncomplete;
        assert_ne!(stub, agi);
    }

    /// AJ2-2 Pro directive test: ExternalSystemStub is NOT
    /// the ExternalRepoSourceMissing missing-source class.
    /// Stubs are intentional; missing sources are failures.
    #[test]
    fn aj2_system_stub_not_missing_source() {
        let stub =
            SchedulerSupportLevel::ExternalSystemStub { kind: "system_library" };
        let missing = SchedulerSupportLevel::ExternalRepoSourceMissing {
            reason: "external repo source file missing",
        };
        assert_ne!(stub, missing);
    }

    /// AJ2-2 Pro directive test: ExternalSystemStub serializes
    /// with the canonical snake_case kind tag.
    #[test]
    fn aj2_system_stub_support_level_serializes() {
        let v = SchedulerSupportLevel::ExternalSystemStub {
            kind: "system_library",
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
        assert_eq!(parsed["kind"], "external_system_stub");
        assert_eq!(parsed["data"]["kind"], "system_library");
    }

    /// AJ2-2: ExternalSystemStub is dispatchable (treated as
    /// no-op success, like NoDeclaredActions / Verified).
    /// All other variants stay non-dispatchable.
    #[test]
    fn aj2_system_stub_is_dispatchable_like_verified() {
        let stub_system = SchedulerSupportLevel::ExternalSystemStub {
            kind: "system_library",
        };
        let stub_header = SchedulerSupportLevel::ExternalSystemStub {
            kind: "header_only",
        };
        let stub_disabled = SchedulerSupportLevel::ExternalSystemStub {
            kind: "feature_disabled",
        };
        assert!(stub_system.is_dispatchable());
        assert!(stub_header.is_dispatchable());
        assert!(stub_disabled.is_dispatchable());
        // Other variants remain non-dispatchable.
        assert!(!SchedulerSupportLevel::ExternalRepoSourceMissing {
            reason: "x"
        }
        .is_dispatchable());
    }

    /// AJ2-2: label() carries the kind for diagnostic output.
    #[test]
    fn aj2_system_stub_label_includes_kind() {
        let v = SchedulerSupportLevel::ExternalSystemStub {
            kind: "system_library",
        };
        assert_eq!(v.label(), "ExternalSystemStub(system_library)");
    }
}
