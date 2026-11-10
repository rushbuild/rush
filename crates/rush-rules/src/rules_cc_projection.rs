// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Inactive, closed C++ provider projections for qualified Starlark definitions.
//!
//! These factories are not registered or exported as Starlark native globals.
//! A source-qualified frontend must select them through its opaque declaration.
//! Target dependency edges remain analysis edges until the orchestrator separately
//! implements artifact-demand execution; this module does not activate pruning.

use crate::analysis_context::AnalysisContext;
use crate::context::BuildContext;
use crate::providers::{
    dep_label_matches_repository_exact, flatten_transitive_define_ids, CcInfo, DefaultInfo,
    DirectCcInfo, FilesInfo, ProviderSet,
};
use crate::{Artifact, ArtifactType, BuildRule, RuleInstance, Value};
use anyhow::{anyhow, ensure, Context, Result};
use async_trait::async_trait;
use rush_action::ids::{ArtifactRegistry, ConfigId};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::sync::Arc;

/// Semantic operation, not authority to qualify an arbitrary custom rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CcProjectionKind {
    CompilationParametersV1,
    HeadersV1,
}

/// Construct only after the frontend has qualified the actual definition.
/// Neither the factory name nor a user attribute grants that qualification.
pub struct CcProjectionRule(CcProjectionKind);

impl CcProjectionRule {
    pub fn parameters() -> Self {
        Self(CcProjectionKind::CompilationParametersV1)
    }

    pub fn headers() -> Self {
        Self(CcProjectionKind::HeadersV1)
    }
}

impl CcProjectionKind {
    fn dependency_attribute(self) -> &'static str {
        match self {
            Self::CompilationParametersV1 => "original_deps",
            Self::HeadersV1 => "deps",
        }
    }

    fn rule_type(self) -> &'static str {
        match self {
            Self::CompilationParametersV1 => "cc_compilation_parameters_projection",
            Self::HeadersV1 => "cc_headers_projection",
        }
    }
}

impl BuildRule for CcProjectionRule {
    fn name(&self) -> &str {
        self.0.rule_type()
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = match attrs.get("name") {
            Some(Value::String(name)) if !name.is_empty() => name.clone(),
            _ => return Err(anyhow!("C++ projection requires a nonempty target name")),
        };
        let attribute = self.0.dependency_attribute();
        let dependencies = match attrs.get(attribute) {
            None => Vec::new(),
            Some(Value::List(values)) => values
                .iter()
                .map(|value| match value {
                    Value::String(label) if !label.is_empty() => Ok(label.clone()),
                    _ => Err(anyhow!(
                        "C++ projection {attribute} requires resolved label strings"
                    )),
                })
                .collect::<Result<Vec<_>>>()?,
            Some(_) => {
                return Err(anyhow!(
                    "C++ projection {attribute} requires a resolved label list"
                ))
            }
        };
        Ok(Box::new(CcProjectionInstance {
            name,
            kind: self.0,
            dependencies,
        }))
    }
}

struct CcProjectionInstance {
    name: String,
    kind: CcProjectionKind,
    dependencies: Vec<String>,
}

/// Bazel depset union: preserve the supplied dependency/field traversal order,
/// deduplicating values at their first occurrence. Do not change the global
/// OrderedNestedSet contract (linking and ordinary native fields differ).
fn union<T: Copy + Eq + Hash>(values: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(*value))
        .collect()
}

impl CcProjectionInstance {
    fn inputs<'a>(
        &self,
        providers: impl IntoIterator<Item = (&'a str, &'a ProviderSet)>,
        package: &str,
        repository: Option<&str>,
    ) -> Result<Vec<Arc<CcInfo>>> {
        let providers: Vec<_> = providers.into_iter().collect();
        let mut selected = HashSet::new();
        let mut infos = Vec::new();
        for dependency in &self.dependencies {
            let matches: Vec<_> = providers
                .iter()
                .enumerate()
                .filter(|(_, (label, _))| {
                    dep_label_matches_repository_exact(label, dependency, package, repository)
                })
                .collect();
            ensure!(matches.len() == 1,
                "C++ projection {} requires exactly one repository-qualified CcInfo prerequisite for {dependency} (found {})",
                self.name, matches.len());
            let (index, (_, provider)) = matches[0];
            ensure!(
                selected.insert(index),
                "C++ projection repeats prerequisite {dependency}"
            );
            let cc = provider.get_cc_info().with_context(|| {
                format!("C++ projection prerequisite {dependency} has no CcInfo")
            })?;
            ensure!(
                cc.compilation
                    .framework_include_dirs
                    .iter()
                    .next()
                    .is_none(),
                "C++ projection does not support nonempty framework include roots: {dependency}"
            );
            infos.push(cc.clone());
        }
        Ok(infos)
    }

    fn project(
        &self,
        infos: &[Arc<CcInfo>],
        registry: &ArtifactRegistry,
        config: ConfigId,
    ) -> Result<ProviderSet> {
        match self.kind {
            CcProjectionKind::CompilationParametersV1 => {
                let direct = DirectCcInfo {
                    defines: flatten_transitive_define_ids(
                        infos.iter().map(|cc| cc.compilation.defines.inner()),
                    ),
                    include_dirs: union(
                        infos
                            .iter()
                            .flat_map(|cc| cc.compilation.include_dirs.iter()),
                    ),
                    quote_include_dirs: union(
                        infos
                            .iter()
                            .flat_map(|cc| cc.compilation.quote_include_dirs.iter()),
                    ),
                    system_include_dirs: union(
                        infos
                            .iter()
                            .flat_map(|cc| cc.compilation.system_include_dirs.iter()),
                    ),
                    // Nonempty framework roots are refused before projection;
                    // the typed bucket is never reinterpreted as ordinary -I.
                    ..Default::default()
                };
                ensure!(
                    direct
                        .defines
                        .iter()
                        .all(|id| (id.0 as usize) < registry.strings().len()),
                    "C++ projection has a define outside the current registry"
                );
                ensure!(
                    direct
                        .include_dirs
                        .iter()
                        .chain(&direct.quote_include_dirs)
                        .chain(&direct.system_include_dirs)
                        .all(|id| (id.0 as usize) < registry.paths().len()),
                    "C++ projection has an include root outside the current registry"
                );
                // No CcInfo merge-all: headers/textual/local defines/linking
                // and DefaultInfo are intentionally absent from this result.
                Ok(
                    ProviderSet::empty().with_cc_info(CcInfo::from_declared_direct_and_deps(
                        direct,
                        Vec::<Arc<CcInfo>>::new(),
                    )),
                )
            }
            CcProjectionKind::HeadersV1 => {
                let mut headers = Vec::new();
                for cc in infos {
                    let view = cc.compilation.bazel_headers.as_ref().context(
                        "C++ header projection requires an explicit producer-qualified Bazel header view")?;
                    headers.extend(view.iter());
                }
                let headers = union(headers);
                for &id in &headers {
                    ensure!(
                        (id.0 as usize) < registry.artifact_count(),
                        "C++ header projection has an artifact outside the current registry"
                    );
                    let data = registry.artifact(id);
                    ensure!(
                        registry.find_artifact_by_path(registry.artifact_path(id), config)
                            == Some(id),
                        "C++ header projection artifact belongs to another configuration"
                    );
                    if data.is_source {
                        ensure!(
                            data.owner_action_declared.is_none() && data.owner_action.is_none(),
                            "C++ header projection source unexpectedly has a producer"
                        );
                    } else {
                        let producer = data
                            .owner_action_declared
                            .context("C++ header projection derived file has no actual producer")?;
                        ensure!(
                            (producer.0 as usize) < registry.declared_action_count(),
                            "C++ header projection has an unknown producer"
                        );
                        let action = registry.declared_action(producer);
                        ensure!(
                            action.owner_target == data.owner_target
                                && action.outputs.contains(&id),
                            "C++ header projection file has a mismatched producer"
                        );
                    }
                }
                let paths = headers
                    .iter()
                    .map(|&id| registry.artifact_path(id).to_owned())
                    .collect();
                // FilesInfo is the legacy native file-view compatibility for
                // these same DefaultInfo files, not a new CcInfo contribution.
                Ok(ProviderSet::empty()
                    .with_files(FilesInfo::new(paths))
                    .with_default_info(DefaultInfo { files: headers }))
            }
        }
    }
}

#[async_trait]
impl RuleInstance for CcProjectionInstance {
    fn target_name(&self) -> &str {
        &self.name
    }
    fn rule_type(&self) -> &str {
        self.kind.rule_type()
    }
    fn dependencies(&self) -> Vec<String> {
        self.dependencies.clone()
    }
    fn analysis_only_dependencies(&self) -> Vec<String> {
        self.dependencies.clone()
    }
    fn cc_projection_kind(&self) -> Option<CcProjectionKind> {
        Some(self.kind)
    }

    fn validate_analysis_dependencies(&self, ctx: &AnalysisContext) -> Result<()> {
        let infos = self.inputs(
            ctx.labeled_dep_providers()
                .iter()
                .map(|(label, providers)| (label.as_str(), providers.as_ref())),
            ctx.package(),
            ctx.workspace(),
        )?;
        self.project(&infos, ctx.registry(), ctx.config_id())
            .map(|_| ())
    }

    fn analyze(&self, ctx: &mut AnalysisContext, _deps: &[Arc<ProviderSet>]) -> ProviderSet {
        let result = self
            .inputs(
                ctx.labeled_dep_providers()
                    .iter()
                    .map(|(label, providers)| (label.as_str(), providers.as_ref())),
                ctx.package(),
                ctx.workspace(),
            )
            .and_then(|infos| self.project(&infos, ctx.registry(), ctx.config_id()));
        match result {
            Ok(providers) => providers,
            Err(error) => {
                ctx.fail_native_namespace_analysis(format!(
                    "C++ projection {}: {error:#}",
                    self.name
                ));
                ProviderSet::empty()
            }
        }
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let package = context
            .package_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let infos = self.inputs(
            context
                .dependency_providers_ordered
                .iter()
                .map(|(label, providers)| (label.as_str(), providers)),
            &package,
            context.repository.as_deref(),
        )?;
        let registry = context
            .artifact_registry
            .as_ref()
            .context("C++ projection has no analysis registry")?
            .lock()
            .await;
        let providers = self.project(&infos, &registry, ConfigId::DEFAULT)?;
        let mut artifacts = Vec::new();
        if let Some(files) = providers.get_default_info() {
            for &id in &files.files {
                let path = registry.artifact_path(id).to_owned();
                ensure!(
                    std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()),
                    "forwarded C++ header is missing; its real producer must run: {}",
                    path.display()
                );
                artifacts.push(Artifact {
                    path,
                    artifact_type: ArtifactType::Header,
                });
            }
        }
        Ok(artifacts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis_context::StubIncludeResolver;
    use crate::context::Toolchain;
    use crate::providers::LinkFragment;
    use rush_action::ids::{ArtifactId, ArtifactKey, ArtifactKind, LinkerInputData, TargetId};
    use std::path::Path;

    fn source(registry: &mut ArtifactRegistry, path: &str) -> ArtifactId {
        let exec_path = registry.intern_path(Path::new(path));
        registry.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path,
            },
            ArtifactKind::Header,
            true,
            TargetId(1),
            None,
        )
    }

    fn instance(kind: CcProjectionKind, dependencies: &[&str]) -> Box<dyn RuleInstance> {
        CcProjectionRule(kind)
            .parse_attributes(&HashMap::from([
                ("name".into(), Value::String("projection".into())),
                (
                    kind.dependency_attribute().into(),
                    Value::List(
                        dependencies
                            .iter()
                            .map(|label| Value::String((*label).into()))
                            .collect(),
                    ),
                ),
            ]))
            .unwrap()
    }

    #[test]
    fn parameters_preserve_ordered_union_but_filter_header_file_and_link_authority() -> Result<()> {
        let mut registry = ArtifactRegistry::new();
        let header = source(&mut registry, "/actual/public.h");
        let textual = source(&mut registry, "/actual/text.inc");
        let library_path = registry.intern_path(Path::new("/actual/libunused.a"));
        let library = registry.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: library_path,
            },
            ArtifactKind::StaticLibrary,
            true,
            TargetId(1),
            None,
        );
        let linker = registry.intern_linker_input(LinkerInputData {
            owner: TargetId(1),
            artifact: library,
            alwayslink: true,
            extras: None,
        });
        let a = registry.intern_string("A=1");
        let b = registry.intern_string("B=2");
        let local = registry.intern_string("PRIVATE=1");
        let flag = registry.intern_string("-lunused");
        let first = registry.intern_path(Path::new("/first"));
        let second = registry.intern_path(Path::new("/second"));
        let left = CcInfo::from_native_direct_and_deps(
            DirectCcInfo {
                headers: vec![header],
                textual_headers: vec![textual],
                defines: vec![a],
                local_defines: vec![local],
                include_dirs: vec![first],
                quote_include_dirs: vec![second],
                system_include_dirs: vec![first],
                linker_inputs: vec![linker],
                link_fragments: vec![LinkFragment::Flag(flag)],
                ..Default::default()
            },
            Vec::<Arc<CcInfo>>::new(),
        );
        let right = CcInfo::from_native_direct_and_deps(
            DirectCcInfo {
                defines: vec![b, a],
                include_dirs: vec![second, first],
                quote_include_dirs: vec![first],
                system_include_dirs: vec![second],
                ..Default::default()
            },
            vec![left.clone()],
        );
        let labels = vec![
            (
                "@@owner//pkg:left".into(),
                Arc::new(ProviderSet::empty().with_cc_info(left)),
            ),
            (
                "@@owner//pkg:right".into(),
                Arc::new(ProviderSet::empty().with_cc_info(right)),
            ),
        ];
        let rule = crate::with_rule_metadata(
            instance(
                CcProjectionKind::CompilationParametersV1,
                &[":right", ":left"],
            ),
            Some(vec!["//visibility:public".into()]),
            true,
            Vec::new(),
        );
        assert_eq!(rule.analysis_only_dependencies(), [":right", ":left"]);
        assert_eq!(
            rule.cc_projection_kind(),
            Some(CcProjectionKind::CompilationParametersV1)
        );
        let resolver = StubIncludeResolver::new("/source".into());
        let toolchain = Toolchain::default();
        let mut ctx = AnalysisContext::new(
            TargetId(2),
            ConfigId::DEFAULT,
            "pkg",
            Some("owner"),
            Path::new("/output"),
            &mut registry,
            &resolver,
            &toolchain,
            Path::new("/source"),
            &[],
        )
        .with_labeled_dep_providers(&labels);
        rule.validate_analysis_dependencies(&ctx)?;
        let result = rule.analyze(&mut ctx, &[]);
        assert!(ctx.native_namespace_analysis_error().is_none());
        assert!(ctx.registry().actions_of(TargetId(2)).is_empty());
        let cc = result.get_cc_info().unwrap();
        assert_eq!(cc.compilation.defines.iter().collect::<Vec<_>>(), [a, b]);
        assert_eq!(
            cc.compilation.include_dirs.iter().collect::<Vec<_>>(),
            [second, first]
        );
        assert_eq!(
            cc.compilation.quote_include_dirs.iter().collect::<Vec<_>>(),
            [first, second]
        );
        assert_eq!(
            cc.compilation
                .system_include_dirs
                .iter()
                .collect::<Vec<_>>(),
            [second, first]
        );
        assert!(cc
            .compilation
            .framework_include_dirs
            .iter()
            .next()
            .is_none());
        assert!(cc.compilation.headers.iter().next().is_none());
        assert!(cc.compilation.textual_headers.iter().next().is_none());
        assert!(cc.compilation.local_defines.is_empty());
        assert!(cc.linking.linker_inputs.iter().next().is_none());
        assert!(cc.linking.link_fragments.iter().next().is_none());
        assert!(result.get_default_info().is_none() && result.get_files().is_none());
        Ok(())
    }

    #[test]
    fn parameters_keep_postorder_define_overrides_and_declared_root_order() -> Result<()> {
        let mut registry = ArtifactRegistry::new();
        let shared = registry.intern_string("SHARED=1");
        let first = registry.intern_string("OVERRIDE=1");
        let second = registry.intern_string("OVERRIDE=2");
        let local = registry.intern_string("PRIVATE=1");
        let leaf = CcInfo::from_native_direct_and_deps(
            DirectCcInfo {
                defines: vec![shared],
                local_defines: vec![local],
                ..Default::default()
            },
            Vec::<Arc<CcInfo>>::new(),
        );
        let left = CcInfo::from_native_direct_and_deps(
            DirectCcInfo {
                defines: vec![first, shared],
                ..Default::default()
            },
            vec![leaf.clone()],
        );
        let right = CcInfo::from_native_direct_and_deps(
            DirectCcInfo {
                defines: vec![second, shared],
                ..Default::default()
            },
            vec![leaf],
        );
        let rule = CcProjectionInstance {
            name: "parameters".into(),
            kind: CcProjectionKind::CompilationParametersV1,
            dependencies: Vec::new(),
        };
        for (roots, expected) in [
            (
                vec![left.clone(), right.clone()],
                vec![shared, first, second],
            ),
            (vec![right, left], vec![shared, second, first]),
        ] {
            let projected = rule.project(&roots, &registry, ConfigId::DEFAULT)?;
            let cc = projected.get_cc_info().unwrap();
            assert_eq!(cc.compilation.defines.iter().collect::<Vec<_>>(), expected);
            let ordinary = crate::providers::flatten_transitive_defines(
                registry.strings(),
                roots.iter().map(|cc| cc.compilation.defines.inner()),
            );
            let projected = crate::providers::flatten_transitive_defines(
                registry.strings(),
                [cc.compilation.defines.inner()],
            );
            assert_eq!(projected, ordinary);
            assert!(!cc.compilation.defines.iter().any(|id| id == local));
            assert!(cc.compilation.local_defines.is_empty());
        }
        Ok(())
    }

    #[tokio::test]
    async fn headers_distinguish_native_textual_view_and_forward_real_generated_producer(
    ) -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let resolver = StubIncludeResolver::new(root.into());
        let toolchain = Toolchain::default();
        for file in ["public.h", "native.inc", "custom.inc"] {
            std::fs::write(root.join(file), "// real source header fixture\n")?;
        }
        let mut registry = ArtifactRegistry::new();
        let normal = source(&mut registry, root.join("public.h").to_str().unwrap());
        let native_textual = source(&mut registry, root.join("native.inc").to_str().unwrap());
        let custom_textual = source(&mut registry, root.join("custom.inc").to_str().unwrap());
        let generated;
        let producer;
        {
            let mut ctx = AnalysisContext::new(
                TargetId(4),
                ConfigId::DEFAULT,
                "pkg",
                Some("owner"),
                root,
                &mut registry,
                &resolver,
                &toolchain,
                root,
                &[],
            );
            generated = ctx.register_generated_artifact(
                &root.join("generated.inc"),
                ArtifactKind::Header,
                None,
            );
            producer = ctx.declare_action("GenRule", Vec::new(), vec![generated])?;
        }
        let custom = CcInfo::from_declared_direct_and_deps(
            DirectCcInfo {
                headers: vec![normal],
                textual_headers: vec![custom_textual],
                ..Default::default()
            },
            Vec::<Arc<CcInfo>>::new(),
        );
        let native = CcInfo::from_native_direct_and_deps(
            DirectCcInfo {
                headers: vec![normal],
                textual_headers: vec![native_textual, generated],
                ..Default::default()
            },
            vec![custom],
        );
        let labels = vec![(
            "@@owner//pkg:original".into(),
            Arc::new(ProviderSet::empty().with_cc_info(native)),
        )];
        let rule = instance(CcProjectionKind::HeadersV1, &[":original"]);
        {
            let mut ctx = AnalysisContext::new(
                TargetId(5),
                ConfigId::DEFAULT,
                "pkg",
                Some("owner"),
                root,
                &mut registry,
                &resolver,
                &toolchain,
                root,
                &[],
            )
            .with_labeled_dep_providers(&labels);
            rule.validate_analysis_dependencies(&ctx)?;
            let result = rule.analyze(&mut ctx, &[]);
            assert!(result.get_cc_info().is_none());
            assert_eq!(
                result.get_default_info().unwrap().files,
                [normal, native_textual, generated]
            );
            assert_eq!(result.get_files().unwrap().files.len(), 3);
            assert!(!result
                .get_default_info()
                .unwrap()
                .files
                .contains(&custom_textual));
            assert!(ctx.registry().actions_of(TargetId(5)).is_empty());
            assert_eq!(ctx.registry().producer(generated), Some(producer));
            assert_eq!(ctx.registry().declared_action_count(), 1);
        }
        let context = BuildContext::new(root.into(), root.join("out"))
            .with_package("pkg".into())
            .with_repository(Some("owner".into()))
            .with_dependencies(
                HashMap::new(),
                labels
                    .iter()
                    .map(|(label, provider)| (label.clone(), provider.as_ref().clone()))
                    .collect(),
            )
            .with_artifact_registry(Arc::new(tokio::sync::Mutex::new(registry)));
        let error = rule.build(&context).await.unwrap_err();
        assert!(format!("{error:#}").contains("real producer must run"));
        assert!(
            !root.join("generated.inc").exists(),
            "projection must not invent producer bytes"
        );
        Ok(())
    }

    #[test]
    fn projections_refuse_unqualified_views_missing_foreign_and_framework_inputs() -> Result<()> {
        let mut registry = ArtifactRegistry::new();
        let framework = registry.intern_path(Path::new("/actual/Frameworks"));
        let resolver = StubIncludeResolver::new("/source".into());
        let toolchain = Toolchain::default();
        for (kind, label, provider, dependencies, expected) in [
            (
                CcProjectionKind::HeadersV1,
                "@@owner//pkg:dep",
                CcInfo::from_direct_and_deps(DirectCcInfo::default(), Vec::<Arc<CcInfo>>::new()),
                vec![":dep"],
                "producer-qualified",
            ),
            (
                CcProjectionKind::CompilationParametersV1,
                "@@foreign//pkg:dep",
                CcInfo::empty(),
                vec![":dep"],
                "exactly one",
            ),
            (
                CcProjectionKind::CompilationParametersV1,
                "@@owner//pkg:dep",
                CcInfo::empty(),
                vec![":missing"],
                "exactly one",
            ),
            (
                CcProjectionKind::CompilationParametersV1,
                "@@owner//pkg:dep",
                CcInfo::empty(),
                vec![":dep", "//pkg:dep"],
                "repeats",
            ),
            (
                CcProjectionKind::CompilationParametersV1,
                "@@owner//pkg:dep",
                CcInfo::from_direct_and_deps(
                    DirectCcInfo {
                        framework_include_dirs: vec![framework],
                        ..Default::default()
                    },
                    Vec::<Arc<CcInfo>>::new(),
                ),
                vec![":dep"],
                "framework",
            ),
        ] {
            let labels = vec![(
                label.into(),
                Arc::new(ProviderSet::empty().with_cc_info(provider)),
            )];
            let rule = instance(kind, &dependencies);
            let ctx = AnalysisContext::new(
                TargetId(2),
                ConfigId::DEFAULT,
                "pkg",
                Some("owner"),
                Path::new("/out"),
                &mut registry,
                &resolver,
                &toolchain,
                Path::new("/source"),
                &[],
            )
            .with_labeled_dep_providers(&labels);
            let error = rule.validate_analysis_dependencies(&ctx).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
        for kind in [
            CcProjectionKind::CompilationParametersV1,
            CcProjectionKind::HeadersV1,
        ] {
            for bad in [
                Value::String(":dep".into()),
                Value::List(vec![Value::Int(3)]),
                Value::None,
            ] {
                assert!(CcProjectionRule(kind)
                    .parse_attributes(&HashMap::from([
                        ("name".into(), Value::String("bad".into())),
                        (kind.dependency_attribute().into(), bad),
                    ]))
                    .is_err());
            }
            let labels = vec![("@@owner//pkg:dep".into(), Arc::new(ProviderSet::empty()))];
            let rule = instance(kind, &[":dep"]);
            let ctx = AnalysisContext::new(
                TargetId(2),
                ConfigId::DEFAULT,
                "pkg",
                Some("owner"),
                Path::new("/out"),
                &mut registry,
                &resolver,
                &toolchain,
                Path::new("/source"),
                &[],
            )
            .with_labeled_dep_providers(&labels);
            assert!(rule
                .validate_analysis_dependencies(&ctx)
                .unwrap_err()
                .to_string()
                .contains("no CcInfo"));
        }
        Ok(())
    }
    #[test]
    fn headers_refuse_foreign_configuration_and_unproduced_derived_file() -> Result<()> {
        let resolver = StubIncludeResolver::new("/source".into());
        let toolchain = Toolchain::default();
        let mut registry = ArtifactRegistry::new();
        let source_header = source(&mut registry, "/source/public.h");
        let generated;
        {
            let mut ctx = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                "pkg",
                Some("owner"),
                Path::new("/out"),
                &mut registry,
                &resolver,
                &toolchain,
                Path::new("/source"),
                &[],
            );
            generated = ctx.register_generated_artifact(
                Path::new("/out/missing.inc"),
                ArtifactKind::Header,
                None,
            );
        }
        for (header, config, message) in [
            (source_header, ConfigId(1), "another configuration"),
            (generated, ConfigId::DEFAULT, "no actual producer"),
        ] {
            let cc = CcInfo::from_native_direct_and_deps(
                DirectCcInfo {
                    headers: vec![header],
                    ..Default::default()
                },
                Vec::<Arc<CcInfo>>::new(),
            );
            let labels = vec![(
                "@@owner//pkg:dep".into(),
                Arc::new(ProviderSet::empty().with_cc_info(cc)),
            )];
            let rule = instance(CcProjectionKind::HeadersV1, &[":dep"]);
            let ctx = AnalysisContext::new(
                TargetId(2),
                config,
                "pkg",
                Some("owner"),
                Path::new("/out"),
                &mut registry,
                &resolver,
                &toolchain,
                Path::new("/source"),
                &[],
            )
            .with_labeled_dep_providers(&labels);
            let error = rule.validate_analysis_dependencies(&ctx).unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
            assert!(ctx.registry().actions_of(TargetId(2)).is_empty());
        }
        Ok(())
    }
}
