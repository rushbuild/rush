//! Closed source-qualified adapters for the two TSL C++ provider projections.
//!
//! These declarations retain loading evidence; they never execute a Starlark
//! implementation or grant a capability from a target name or attribute string.
use crate::{
    ast::{Program, Stmt},
    evaluator::{Environment, Value},
    BzlLoader,
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use rush_incremental::{DependencyManifest, Staleness};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

const TSL_SOURCE_SHA256: &str = "ae8424430a809d30b371e3c4c11a0783c1c4d087a014a73d8439a4977119f918";
/// Version of the closed source/constructor/provider interpretation.
pub const CC_PROJECTION_SEMANTICS_ABI: &str = "rush.cc-projection.v1";

/// The only two native operations admitted by this frontend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CcProjectionKind {
    CompilationParametersV1,
    HeadersV1,
}
impl CcProjectionKind {
    fn attribute(self) -> &'static str {
        match self {
            Self::CompilationParametersV1 => "original_deps",
            Self::HeadersV1 => "deps",
        }
    }
    fn implementation(self) -> &'static str {
        match self {
            Self::CompilationParametersV1 => "_transitive_parameters_library_impl",
            Self::HeadersV1 => "_transitive_hdrs_impl",
        }
    }
    fn declaration(self) -> &'static str {
        match self {
            Self::CompilationParametersV1 => "_transitive_parameters_library",
            Self::HeadersV1 => "_transitive_hdrs",
        }
    }
}

/// Stable definition identity, separate from heap identity and BUILD ownership.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CcProjectionDefinitionIdentity {
    module_label: String,
    source_path: PathBuf,
    source_sha256: String,
    kind: CcProjectionKind,
}
impl CcProjectionDefinitionIdentity {
    /// Actual defining module label, retained through load/reexport aliases.
    pub fn module_label(&self) -> &str {
        &self.module_label
    }
    /// Actual tracked canonical source file.
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }
    /// Qualified source bytes, explicitly SHA256 rather than manifest BLAKE3.
    pub fn source_sha256(&self) -> &str {
        &self.source_sha256
    }
}

/// Opaque out-of-band resolution. No serde or Starlark constructor exists.
#[derive(Clone)]
pub struct CcProjectionDeclaration {
    origin: Option<Arc<DefinitionOrigin>>,
    identity: Option<CcProjectionDefinitionIdentity>,
    refusal: Option<String>,
}
impl CcProjectionDeclaration {
    /// An admitted operation; a refused declaration deliberately has no kind.
    pub fn kind(&self) -> Option<CcProjectionKind> {
        self.identity.as_ref().map(|i| i.kind)
    }
    /// Selected-only configuration error for a retained invalid declaration.
    pub fn refusal_reason(&self) -> Option<&str> {
        self.refusal.as_deref()
    }
    /// Stable source identity of a successful qualification.
    pub fn definition_identity(&self) -> Option<&CcProjectionDefinitionIdentity> {
        self.identity.as_ref()
    }
    fn refused(reason: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            origin: None,
            identity: None,
            refusal: Some(reason.into()),
        })
    }
    /// Requalify a retained provider graph against the current ordinary loader.
    /// The caller supplies its live loader; no loader/environment cycle is held.
    pub fn revalidate(&self, loader: Arc<Mutex<BzlLoader>>) -> Result<()> {
        if let Some(reason) = &self.refusal {
            bail!("{reason}");
        }
        let identity = self
            .identity
            .as_ref()
            .context("missing projection identity")?;
        self.origin
            .as_ref()
            .context("missing projection origin")?
            .validate(&loader)?;
        let symbol = identity.kind.declaration().to_owned();
        let symbols = BzlLoader::load_bzl_from_evaluator(
            loader.clone(),
            &identity.module_label,
            &[(symbol.clone(), symbol.clone())],
        )?;
        let Value::ProjectionRule(candidate) = symbols
            .get(&symbol)
            .context("projection declaration disappeared")?
        else {
            bail!("projection declaration changed type");
        };
        let current = candidate.resolve(Some(&loader));
        ensure!(
            current.identity.as_ref() == Some(identity) && current.refusal.is_none(),
            "projection definition no longer qualifies"
        );
        Ok(())
    }
}

#[derive(Debug)]
struct FunctionShape {
    params: Vec<String>,
    body: Vec<Stmt>,
}
#[derive(Debug)]
struct Admission {
    context: String,
    manifest: DependencyManifest,
}

/// Minted only by a real loader from the exact source string it parsed.
/// It contains no loader, symbols, function or environment back-reference.
#[derive(Debug)]
pub(crate) struct DefinitionOrigin {
    label: String,
    path: PathBuf,
    sha256: String,
    shapes: HashMap<String, FunctionShape>,
    wanted: AtomicBool,
    admission: Mutex<Option<Admission>>,
}
impl DefinitionOrigin {
    pub(crate) fn from_source(
        path: &Path,
        label: &str,
        source: &str,
        program: &Program,
    ) -> Arc<Self> {
        let sha256 = hex::encode(Sha256::digest(source.as_bytes()));
        let mut shapes = HashMap::new();
        if sha256 == TSL_SOURCE_SHA256 {
            for stmt in &program.statements {
                if let Stmt::FunctionDef {
                    name,
                    params,
                    varargs_param,
                    kwargs_param,
                    body,
                } = stmt
                {
                    if matches!(
                        name.as_str(),
                        "_transitive_parameters_library_impl"
                            | "_transitive_hdrs_impl"
                            | "_get_transitive_headers"
                    ) && varargs_param.is_none()
                        && kwargs_param.is_none()
                        && params.iter().all(|(_, default)| default.is_none())
                    {
                        shapes.insert(
                            name.clone(),
                            FunctionShape {
                                params: params.iter().map(|(name, _)| name.clone()).collect(),
                                body: body.clone(),
                            },
                        );
                    }
                }
            }
        }
        Arc::new(Self {
            label: label.into(),
            path: path.into(),
            sha256,
            shapes,
            wanted: AtomicBool::new(false),
            admission: Mutex::new(None),
        })
    }
    pub(crate) fn needs_seal(&self) -> bool {
        self.wanted.load(Ordering::Acquire)
    }
    pub(crate) fn seal(&self, context: String, dependencies: Vec<rush_incremental::Dependency>) {
        if self.wanted.load(Ordering::Acquire) {
            if let Ok(mut guard) = self.admission.lock() {
                *guard = Some(Admission {
                    context,
                    manifest: DependencyManifest::from_deps(dependencies),
                });
            }
        }
    }
    pub(crate) fn metadata_size(&self) -> usize {
        self.label.capacity()
            + self.path.as_os_str().len()
            + self.sha256.capacity()
            + self
                .shapes
                .values()
                .map(|shape| {
                    shape.body.len() * 128
                        + shape.params.iter().map(String::capacity).sum::<usize>()
                })
                .sum::<usize>()
            + self
                .admission
                .lock()
                .ok()
                .and_then(|guard| {
                    guard.as_ref().map(|a| {
                        a.context.capacity()
                            + a.manifest.deps().len()
                                * std::mem::size_of::<rush_incremental::Dependency>()
                    })
                })
                .unwrap_or(0)
    }
    fn context_current(&self, loader: &Arc<Mutex<BzlLoader>>) -> Result<bool> {
        ensure!(
            rush_incremental::tracked::current().is_some(),
            "projection requires tracked command analysis"
        );
        let expected = self
            .admission
            .lock()
            .map_err(|_| anyhow!("projection admission lock poisoned"))?
            .as_ref()
            .map(|admission| admission.context.clone())
            .context("projection module is incomplete")?;
        let current = loader
            .lock()
            .map_err(|_| anyhow!("projection loader lock poisoned"))?
            .cc_projection_context(&self.label, &self.path)?;
        Ok(current == expected)
    }
    fn validate(&self, loader: &Arc<Mutex<BzlLoader>>) -> Result<()> {
        ensure!(
            self.sha256 == TSL_SOURCE_SHA256,
            "unsupported TSL projection source implementation"
        );
        let io = rush_incremental::tracked::current()
            .context("source-qualified projection requires tracked command analysis")?;
        let (context, manifest) = {
            let guard = self
                .admission
                .lock()
                .map_err(|_| anyhow!("projection admission lock poisoned"))?;
            let admission = guard
                .as_ref()
                .context("projection defining module is incomplete or untracked")?;
            ensure!(
                !admission.manifest.is_empty(),
                "projection has no producing input manifest"
            );
            (admission.context.clone(), admission.manifest.clone())
        };
        let before = loader
            .lock()
            .map_err(|_| anyhow!("projection loader lock poisoned"))?
            .cc_projection_context(&self.label, &self.path)?;
        ensure!(
            before == context,
            "projection defining repository context changed"
        );
        let validated = io.validate_manifest(&manifest);
        ensure!(
            matches!(validated.staleness, Staleness::Fresh),
            "projection source closure changed: {:?}",
            validated.staleness
        );
        let current_manifest = validated.refreshed;
        io.record_deps(current_manifest.deps());
        if let Ok(mut guard) = self.admission.lock() {
            if let Some(admission) = guard.as_mut() {
                admission.manifest = current_manifest;
            }
        }
        let source = io.read_file(&self.path)?;
        ensure!(
            hex::encode(Sha256::digest(&source)) == self.sha256,
            "projection source changed after loading"
        );
        let after = loader
            .lock()
            .map_err(|_| anyhow!("projection loader lock poisoned"))?
            .cc_projection_context(&self.label, &self.path)?;
        ensure!(
            before == after,
            "projection context changed during validation"
        );
        Ok(())
    }
    fn function_matches(&self, value: &Value, name: &str) -> bool {
        let Some(shape) = self.shapes.get(name) else {
            return false;
        };
        let Value::Function {
            params,
            varargs_param,
            kwargs_param,
            body,
            closure,
        } = value
        else {
            return false;
        };
        params.len() == shape.params.len()
            && params
                .iter()
                .zip(&shape.params)
                .all(|((name, default), expected)| name == expected && default.is_none())
            && varargs_param.is_none()
            && kwargs_param.is_none()
            && body == &shape.body
            && closure
                .cc_projection_origin()
                .is_some_and(|origin| std::ptr::eq(origin.as_ref(), self))
    }
}

/// A genuine attr.label_list result. User Dict(_type=...) cannot create it.
#[derive(Clone)]
pub struct AttributeDefinition {
    pub(crate) arguments: HashMap<String, Value>,
}
impl AttributeDefinition {
    pub(crate) fn label_list(arguments: HashMap<String, Value>) -> Self {
        Self { arguments }
    }
    fn matches(&self, kind: CcProjectionKind) -> bool {
        if self
            .arguments
            .keys()
            .any(|key| !matches!(key.as_str(), "allow_files" | "allow_empty" | "providers"))
        {
            return false;
        }
        if self.arguments.get("allow_files") != Some(&Value::Bool(true)) {
            return false;
        }
        let empty = self.arguments.get("allow_empty");
        if kind == CcProjectionKind::CompilationParametersV1 && empty != Some(&Value::Bool(true)) {
            return false;
        }
        if kind == CcProjectionKind::HeadersV1 && empty.is_some() {
            return false;
        }
        matches!(self.arguments.get("providers"), Some(Value::List(providers)) if matches!(providers.as_slice(), [Value::NativeFunction(name)] if name == "CcInfo"))
    }
}

/// Retains only the bounded one-label-list CcInfo projection candidate family.
pub struct ProjectionRuleDefinition {
    pub(crate) implementation: Value,
    pub(crate) attributes: HashMap<String, Value>,
    pub(crate) options: HashMap<String, Value>,
    pub(crate) origin: Option<Arc<DefinitionOrigin>>,
    kind: CcProjectionKind,
}
impl ProjectionRuleDefinition {
    pub(crate) fn candidate(
        kwargs: &HashMap<String, Value>,
        origin: Option<Arc<DefinitionOrigin>>,
    ) -> Option<Arc<Self>> {
        let Value::Dict(attributes) = kwargs.get("attrs")? else {
            return None;
        };
        if attributes.len() != 1 {
            return None;
        }
        let kind = if attributes.contains_key("original_deps") {
            CcProjectionKind::CompilationParametersV1
        } else if attributes.contains_key("deps") {
            CcProjectionKind::HeadersV1
        } else {
            return None;
        };
        // Recognize the closed schema for deferred refusal as well as success;
        // the actual kind is never exposed without the later source checks.
        let descriptor = &attributes[kind.attribute()];
        let looks_cc = match descriptor {
            Value::AttributeDefinition(attr) => attr.arguments.get("providers"),
            Value::Dict(attr) => attr.get("providers"),
            _ => None,
        };
        if !matches!(looks_cc, Some(Value::List(providers)) if providers.iter().any(|provider| matches!(provider, Value::NativeFunction(name) if name == "CcInfo") || matches!(provider, Value::Provider { name, .. } if name == "CcInfo")))
        {
            return None;
        }
        if let Some(origin) = &origin {
            origin.wanted.store(true, Ordering::Release);
        }
        Some(Arc::new(Self {
            implementation: kwargs.get("implementation").cloned().unwrap_or(Value::None),
            attributes: attributes.clone(),
            options: kwargs.clone(),
            origin,
            kind,
        }))
    }
    pub(crate) fn resolve_call(
        &self,
        loader: Option<&Arc<Mutex<BzlLoader>>>,
        args: &[Value],
        kwargs: &HashMap<String, Value>,
    ) -> Arc<CcProjectionDeclaration> {
        if !args.is_empty() {
            return CcProjectionDeclaration::refused(
                "CcInfo projection instance accepts keyword arguments only",
            );
        }
        if kwargs.keys().any(|name| {
            name != self.kind.attribute()
                && !matches!(
                    name.as_str(),
                    "name"
                        | "visibility"
                        | "compatible_with"
                        | "target_compatible_with"
                        | "testonly"
                        | "tags"
                )
        }) {
            return CcProjectionDeclaration::refused(
                "unsupported CcInfo projection instance attribute",
            );
        }
        self.resolve(loader)
    }
    pub(crate) fn resolve(
        &self,
        loader: Option<&Arc<Mutex<BzlLoader>>>,
    ) -> Arc<CcProjectionDeclaration> {
        self.resolve_once(loader, true)
    }
    fn resolve_once(
        &self,
        loader: Option<&Arc<Mutex<BzlLoader>>>,
        refresh_context: bool,
    ) -> Arc<CcProjectionDeclaration> {
        let result: Result<Arc<CcProjectionDeclaration>> = (|| {
            let origin = self.qualify_shape()?;
            let loader = loader.context("projection needs its current loader")?;
            if !origin.context_current(loader)? && refresh_context {
                // Discovery may have extended the shared repository registry
                // after this module completed. Reload through normal admission;
                // never bless old exports by merely updating their context stamp.
                let symbol = self.kind.declaration().to_owned();
                let symbols = BzlLoader::load_bzl_from_evaluator(
                    loader.clone(),
                    &origin.label,
                    &[(symbol.clone(), symbol.clone())],
                )?;
                let Value::ProjectionRule(current) = symbols
                    .get(&symbol)
                    .context("projection declaration disappeared during context refresh")?
                else {
                    bail!("projection declaration changed during context refresh");
                };
                let declaration = current.resolve_once(Some(loader), false);
                if let Some(reason) = declaration.refusal_reason() {
                    bail!("{reason}");
                }
                return Ok(declaration);
            }
            origin.validate(loader)?;
            Ok(Arc::new(CcProjectionDeclaration {
                identity: Some(CcProjectionDefinitionIdentity {
                    module_label: origin.label.clone(),
                    source_path: origin.path.clone(),
                    source_sha256: origin.sha256.clone(),
                    kind: self.kind,
                }),
                origin: Some(origin),
                refusal: None,
            }))
        })();
        match result {
            Ok(declaration) => declaration,
            Err(error) => CcProjectionDeclaration::refused(format!(
                "source-qualified CcInfo projection is unsupported: {error:#}"
            )),
        }
    }
    fn qualify_shape(&self) -> Result<Arc<DefinitionOrigin>> {
        let origin = self
            .origin
            .as_ref()
            .context("rule constructor has no genuine defining-module origin")?;

        ensure!(
            self.options
                .keys()
                .all(|key| matches!(key.as_str(), "implementation" | "attrs")),
            "unsupported projection rule() options"
        );
        let Value::AttributeDefinition(descriptor) = &self.attributes[self.kind.attribute()] else {
            bail!("projection schema was not created by genuine attr.label_list");
        };
        ensure!(
            descriptor.matches(self.kind),
            "projection attribute schema/default/provider differs"
        );
        ensure!(
            origin.function_matches(&self.implementation, self.kind.implementation()),
            "projection implementation body/params/origin differs"
        );
        let Value::Function { closure, .. } = &self.implementation else {
            unreachable!()
        };
        let native = |env: &Environment, name: &str| -> bool {
            env.get(name).is_none_or(
                |value| matches!(value, Value::NativeFunction(actual) if actual == name),
            )
        };
        ensure!(
            native(closure, "CcInfo") && native(closure, "depset"),
            "projection native provider/depset binding differs"
        );
        match self.kind {
            CcProjectionKind::CompilationParametersV1 => {
                if let Some(value) = closure.get("cc_common") {
                    ensure!(
                        matches!(value, Value::Namespace(ref attrs) if matches!(attrs.get("create_compilation_context"), Some(Value::NativeFunction(name)) if name == "cc_common.create_compilation_context")),
                        "projection cc_common binding differs"
                    );
                }
            }
            CcProjectionKind::HeadersV1 => {
                ensure!(
                    native(closure, "DefaultInfo"),
                    "projection DefaultInfo binding differs"
                );
                let helper = closure
                    .get("_get_transitive_headers")
                    .context("projection header helper is not defined")?;
                ensure!(
                    origin.function_matches(&helper, "_get_transitive_headers"),
                    "projection header helper body/params/origin differs"
                );
                let Value::Function {
                    closure: helper_closure,
                    ..
                } = helper
                else {
                    unreachable!()
                };
                ensure!(
                    native(&helper_closure, "CcInfo") && native(&helper_closure, "depset"),
                    "projection header-helper free bindings differ"
                );
            }
        }
        Ok(origin.clone())
    }
}

impl std::fmt::Debug for ProjectionRuleDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectionRuleDefinition")
            .field("candidate_kind", &self.kind)
            .field("module", &self.origin.as_ref().map(|origin| &origin.label))
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for AttributeDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut keys = self.arguments.keys().collect::<Vec<_>>();
        keys.sort();
        f.debug_struct("AttributeDefinition")
            .field("constructor", &"attr.label_list")
            .field("arguments", &keys)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for CcProjectionDeclaration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CcProjectionDeclaration")
            .field("identity", &self.identity)
            .field("refusal", &self.refusal)
            .finish_non_exhaustive()
    }
}
