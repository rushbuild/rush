// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Validate only the native proto SOURCE facet reached through declared
//! generation-import edges. This is no descriptor-output implementation or
//! reusable execution certificate. The driver must skip both body and scheduler
//! for admitted metadata targets and recheck before publishing their sources.

use crate::analysis_snapshot::AnalysisSnapshot;
use crate::dependency_graph::{DependencyGraph, Label};
use anyhow::{ensure, Context, Result};
use rush_action::ids::{ArtifactId, ArtifactKind, ArtifactRegistry, ConfigId, TargetId};
use rush_incremental::CanonicalRegularFileMetadata;
use rush_rules::providers::{ProtoFilesInfo, ProtoSourceImport, ProviderSet};
use rush_rules::ProtoSourceMetadataDeclaration;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};

/// Current read observations, not a token permitting cached source reuse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObservedProtoSource {
    pub(crate) path: PathBuf,
    pub(crate) canonical_path: PathBuf,
    pub(crate) digest: [u8; 32],
    pub(crate) metadata: CanonicalRegularFileMetadata,
    pub(crate) configuration: [u8; 32],
}

/// Call inside the driver's explicit tracked command scope. Canonicalization
/// records link hops; the same-descriptor read records content/full metadata.
/// No ordinary read fallback is allowed for an unsupported descriptor backend.
pub(crate) fn observe_tracked_source(path: &Path) -> Result<ObservedProtoSource> {
    use rush_incremental::tracked;
    let io = tracked::current().context("proto source observation needs tracked command IO")?;
    let configuration = io.config_fingerprint();
    ensure!(path.is_absolute(), "proto SOURCE path must be absolute");
    io.refresh_filesystem_observations();
    let canonical_path = tracked::canonicalize(path)
        .with_context(|| format!("resolve declared proto SOURCE {}", path.display()))?;
    let read = io
        .read_canonical_regular_file(&canonical_path)?
        .context("proto SOURCE admission requires descriptor-bound regular-file reads")?;
    io.refresh_filesystem_observations();
    ensure!(
        tracked::canonicalize(path)? == canonical_path
            && io
                .fresh_canonical_regular_file_metadata(&canonical_path)?
                .as_ref()
                == Some(&read.metadata)
            && io.config_fingerprint() == configuration,
        "proto SOURCE changed during observation: {}",
        path.display()
    );
    Ok(ObservedProtoSource {
        path: path.to_owned(),
        canonical_path,
        digest: *blake3::hash(&read.contents).as_bytes(),
        metadata: read.metadata,
        configuration,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdmittedProtoFile {
    pub(crate) artifact: ArtifactId,
    pub(crate) owner: TargetId,
    pub(crate) import_root: PathBuf,
    pub(crate) import_path: PathBuf,
    pub(crate) observation: ObservedProtoSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdmittedProtoSource {
    pub(crate) target: TargetId,
    pub(crate) direct_sources: Vec<ArtifactId>,
    /// First occurrence of actual source IDs in provider declaration order.
    pub(crate) sources: Vec<AdmittedProtoFile>,
}

pub(crate) type AdmittedProtoSources = HashMap<usize, AdmittedProtoSource>;

struct View<'a> {
    graph: &'a DependencyGraph,
    order: &'a [Label],
    targets: &'a HashMap<String, TargetId>,
    providers: &'a HashMap<String, ProviderSet>,
    repositories: &'a HashMap<String, PathBuf>,
    main: &'a Path,
    configuration: [u8; 32],
}

/// `declared_output_owners` must come from the complete tracked declaration
/// catalogs for the candidate packages, including non-SOURCE target self-routes,
/// unselected/implicit/symbolic outputs. A graph/registry-only scan is insufficient.
/// `observe_source` must perform actual tracked current-command reads; use
/// `observe_tracked_source` within the driver's tracked scope, not a remembered
/// record. Invocation is before any writer, including currently inactive nodes.
pub(crate) fn admit_generation_sources(
    snapshot: &AnalysisSnapshot,
    explicit_roots: &[Label],
    registry: &ArtifactRegistry,
    pending_targets: &HashSet<TargetId>,
    config: ConfigId,
    declared_output_owners: &HashMap<Label, Label>,
    mut observe_source: impl FnMut(&Path) -> Result<ObservedProtoSource>,
) -> Result<AdmittedProtoSources> {
    ensure!(
        snapshot.key.config_fingerprint == *snapshot.built_under.config_fingerprint(),
        "proto SOURCE snapshot configuration is inconsistent"
    );
    admit(
        View {
            graph: &snapshot.graph,
            order: &snapshot.build_order,
            targets: &snapshot.target_id_by_label,
            providers: &snapshot.analysis_providers,
            repositories: &snapshot.resolver_repo_paths,
            main: &snapshot.key.workspace_realpath,
            configuration: snapshot.key.config_fingerprint,
        },
        explicit_roots,
        registry,
        pending_targets,
        config,
        declared_output_owners,
        &mut observe_source,
    )
}

/// Recompute the whole facet map with fresh observations before commit. No
/// special token or successful old read can discharge this current check.
pub(crate) fn recheck_generation_sources(
    expected: &AdmittedProtoSources,
    snapshot: &AnalysisSnapshot,
    explicit_roots: &[Label],
    registry: &ArtifactRegistry,
    pending_targets: &HashSet<TargetId>,
    config: ConfigId,
    declared_output_owners: &HashMap<Label, Label>,
    observe_source: impl FnMut(&Path) -> Result<ObservedProtoSource>,
) -> Result<()> {
    let current = admit_generation_sources(
        snapshot,
        explicit_roots,
        registry,
        pending_targets,
        config,
        declared_output_owners,
        observe_source,
    )?;
    ensure!(
        current == *expected,
        "proto SOURCE facet changed before commit"
    );
    Ok(())
}

fn relative_literal(raw: &str, allow_dot: bool) -> Result<PathBuf> {
    ensure!(
        !raw.is_empty()
            && !raw.contains(['\0', '\\', ':', '@'])
            && !Path::new(raw).is_absolute()
            && raw
                .split('/')
                .all(|part| !part.is_empty() && part != ".." && (allow_dot || part != ".")),
        "proto SOURCE requires a normalized literal relative path: {raw}"
    );
    Ok(Path::new(raw)
        .components()
        .filter(|part| !matches!(part, Component::CurDir))
        .collect())
}

fn root<'a>(view: &'a View<'_>, label: &Label) -> Result<&'a Path> {
    let root = match &label.workspace {
        Some(owner) => view
            .repositories
            .get(owner)
            .with_context(|| format!("missing exact proto repository root for @{owner}"))?
            .as_path(),
        None => view.main,
    };
    ensure!(
        root.is_absolute()
            && !root
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir)),
        "proto SOURCE repository root is not absolute/normalized: {label}"
    );
    Ok(root)
}

fn resolve(view: &View<'_>, owner: &Label, raw: &str) -> Result<Label> {
    ensure!(
        !raw.is_empty() && raw.trim() == raw,
        "invalid proto dependency spelling"
    );
    Ok(view.graph.canonicalize_label(Label::parse(
        raw,
        owner.workspace.as_deref(),
        Some(&owner.package),
    )?))
}

fn dependencies(view: &View<'_>, owner: &Label, raw: &[String]) -> Result<Vec<Label>> {
    let mut seen = HashSet::new();
    raw.iter()
        .map(|raw| {
            let label = resolve(view, owner, raw)?;
            ensure!(
                seen.insert(label.clone()),
                "repeated resolved proto dependency: {owner}"
            );
            Ok(label)
        })
        .collect()
}

/// Only a fresh, error-preserving NotFound result proves absence. Other
/// failures are not permission to skip a package boundary or search candidate.
fn proven_absent(path: &Path) -> Result<bool> {
    let io = rush_incremental::tracked::current()
        .context("proto path admission needs tracked command IO")?;
    io.refresh_filesystem_observations();
    match io.fresh_canonical_regular_file_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error).with_context(|| format!("probe proto path {}", path.display())),
        // A noncanonical path, symlink, directory, or unsupported capability is
        // not proven absent. Its caller must observe a regular file or refuse.
        Ok(_) => Ok(false),
    }
}

pub(crate) fn check_source_package_boundaries(
    repository: &Path,
    package: &str,
    source: &Path,
) -> Result<()> {
    ensure!(
        repository.is_absolute()
            && !repository
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir)),
        "invalid proto repository root"
    );
    let package = if package.is_empty() {
        PathBuf::new()
    } else {
        relative_literal(package, false)?
    };
    ensure!(
        !source.as_os_str().is_empty()
            && source
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            && source.extension() == Some(std::ffi::OsStr::new("proto")),
        "invalid literal proto SOURCE path"
    );
    let mut directory = repository.join(package);
    for component in source
        .parent()
        .context("proto SOURCE has no parent")?
        .components()
    {
        ensure!(
            matches!(component, Component::Normal(_)),
            "invalid proto source parent"
        );
        directory.push(component.as_os_str());
        for name in ["BUILD", "BUILD.bazel"] {
            let boundary = directory.join(name);
            ensure!(
                proven_absent(&boundary)?,
                "proto SOURCE crosses a package boundary or unproved path: {}",
                boundary.display()
            );
        }
    }
    Ok(())
}

/// Check the actual ordered search policy for every declared import. This does
/// not discover undeclared imports; the real compiler must separately attest
/// its consumed dependencies before its outputs can be committed.
pub(crate) fn check_proto_import_search(
    proto: &ProtoFilesInfo,
    registry: &ArtifactRegistry,
    config: ConfigId,
) -> Result<()> {
    let io = rush_incremental::tracked::current()
        .context("proto import search admission needs tracked command IO")?;
    validate_import_winners(
        proto,
        registry,
        config,
        io.config_fingerprint(),
        &mut observe_tracked_source,
    )
}

fn validate_import_winners(
    proto: &ProtoFilesInfo,
    registry: &ArtifactRegistry,
    config: ConfigId,
    configuration: [u8; 32],
    observe: &mut impl FnMut(&Path) -> Result<ObservedProtoSource>,
) -> Result<()> {
    proto.validate_declared_imports(registry, config)?;
    let roots = proto
        .ordered_import_roots()
        .context("unknown proto import search order")?;
    let imports = proto
        .declared_imports
        .as_ref()
        .context("unknown declared proto imports")?;
    for source in imports.iter() {
        let expected = registry.artifact_path(source.artifact);
        let mut found = false;
        for root in &roots {
            let candidate = root.join(&source.import_path);
            if proven_absent(&candidate)? {
                continue;
            }
            ensure!(
                candidate == expected,
                "proto import search shadows declared SOURCE {} with {}",
                expected.display(),
                candidate.display()
            );
            let observed = observe(&candidate)?;
            ensure!(
                observed.path == expected
                    && observed.canonical_path.is_absolute()
                    && observed.metadata.mode & 0o170000 == 0o100000
                    && observed.configuration == configuration,
                "proto import winner belongs to another path/command or is not regular"
            );
            found = true;
            break;
        }
        ensure!(
            found,
            "declared proto import has no proven search winner: {}",
            expected.display()
        );
    }
    Ok(())
}

fn layouts(
    view: &View<'_>,
    label: &Label,
    declaration: ProtoSourceMetadataDeclaration<'_>,
) -> Result<Vec<(Label, PathBuf, PathBuf, PathBuf)>> {
    let repository = root(view, label)?;
    let package = if label.package.is_empty() {
        PathBuf::new()
    } else {
        relative_literal(&label.package, false)?
    };
    let import_root = match declaration.strip_import_prefix {
        None | Some("") => repository.to_owned(),
        Some(prefix) if prefix.starts_with('/') => {
            let prefix = &prefix[1..];
            repository.join(if prefix.is_empty() {
                PathBuf::new()
            } else {
                relative_literal(prefix, true)?
            })
        }
        Some(prefix) => repository
            .join(&package)
            .join(relative_literal(prefix, true)?),
    };
    let mut seen = HashSet::new();
    declaration
        .sources
        .iter()
        .map(|raw| {
            let relative = rush_rules::rules_cc::proto_source_relative_path(
                raw, &label.package, label.workspace.as_deref(),
            )?;
            ensure!(
                relative.extension() == Some(std::ffi::OsStr::new("proto"))
                    && seen.insert(relative.clone()),
                "invalid/repeated direct proto SOURCE: {label}"
            );
            check_source_package_boundaries(repository, &label.package, &relative)?;
            let path = repository.join(&package).join(&relative);
            let import_path = path
                .strip_prefix(&import_root)
                .context("proto SOURCE is outside its declared strip prefix")?
                .to_owned();
            ensure!(
                !import_path.as_os_str().is_empty(),
                "empty proto import path"
            );
            let source_label = Label {
                workspace: label.workspace.clone(),
                package: label.package.clone(),
                target: relative.to_str().context("non-UTF8 proto SOURCE target")?.to_owned(),
            };
            Ok((source_label, path, import_root.clone(), import_path))
        })
        .collect()
}

fn proto_only(providers: &ProviderSet) -> Result<&std::sync::Arc<ProtoFilesInfo>> {
    let ProviderSet {
        cc_info,
        cc_shared,
        files,
        proto_files,
        default_info,
        runfiles,
        custom,
    } = providers;
    ensure!(
        cc_info.is_none()
            && cc_shared.is_none()
            && files.is_none()
            && default_info.is_none()
            && runfiles.is_none()
            && custom.is_empty(),
        "proto SOURCE facet has non-proto provider authority"
    );
    proto_files
        .as_ref()
        .context("proto SOURCE facet has no ProtoFilesInfo")
}

fn admit(
    view: View<'_>,
    explicit_roots: &[Label],
    registry: &ArtifactRegistry,
    pending_targets: &HashSet<TargetId>,
    config: ConfigId,
    declared_output_owners: &HashMap<Label, Label>,
    observe: &mut impl FnMut(&Path) -> Result<ObservedProtoSource>,
) -> Result<AdmittedProtoSources> {
    let mut indices = HashMap::new();
    let mut owners = HashMap::new();
    for (index, label) in view.order.iter().enumerate() {
        ensure!(
            indices.insert(label.clone(), index).is_none(),
            "duplicate proto build-order label"
        );
        let target = *view
            .targets
            .get(&label.to_string())
            .context("missing exact proto TargetId")?;
        ensure!(
            owners.insert(target, label).is_none(),
            "aliased proto snapshot TargetIds"
        );
    }
    for label in explicit_roots {
        let label = view.graph.canonicalize_label(label.clone());
        let node = view
            .graph
            .get(&label)
            .context("explicit proto root is outside graph")?;
        ensure!(
            node.rule_instance.proto_source_metadata().is_none(),
            "explicit proto roots require unsupported descriptor outputs: {label}"
        );
    }
    let mut generation = HashMap::<Label, HashSet<Label>>::new();
    let mut queue = VecDeque::new();
    for label in view.order {
        let node = view
            .graph
            .get(label)
            .context("missing generation consumer node")?;
        let raw = node.rule_instance.generation_only_proto_dependencies();
        if raw.is_empty() {
            continue;
        }
        let selected = dependencies(&view, label, &raw)?;
        for dependency in &selected {
            ensure!(
                *indices
                    .get(dependency)
                    .context("generation proto source outside scoped order")?
                    < indices[label],
                "generation proto SOURCE must precede its consumer: {label} -> {dependency}"
            );
            ensure!(
                node.dependencies.contains(dependency),
                "generation-only proto edge is absent from graph: {label} -> {dependency}"
            );
            ensure!(
                node.rule_instance
                    .dependencies()
                    .iter()
                    .any(|raw| resolve(&view, label, raw).is_ok_and(|label| label == *dependency)),
                "generation-only proto edge is absent from literal dependencies"
            );
            queue.push_back(dependency.clone());
        }
        generation.insert(label.clone(), selected.into_iter().collect());
    }
    if queue.is_empty() {
        return Ok(HashMap::new());
    }
    ensure!(
        !explicit_roots.is_empty(),
        "proto SOURCE admission requires explicit requested roots"
    );
    ensure!(
        explicit_roots
            .iter()
            .all(|root| indices.contains_key(&view.graph.canonicalize_label(root.clone()))),
        "proto SOURCE requested root is outside scoped order"
    );
    ensure!(
        config == ConfigId::DEFAULT,
        "proto SOURCE facet requires explicit native DEFAULT config"
    );
    let mut selected = HashSet::new();
    while let Some(label) = queue.pop_front() {
        if !selected.insert(label.clone()) {
            continue;
        }
        let node = view
            .graph
            .get(&label)
            .context("missing proto SOURCE prerequisite node")?;
        let declaration = node
            .rule_instance
            .proto_source_metadata()
            .with_context(|| {
                format!("generation prerequisite is not native proto SOURCE metadata: {label}")
            })?;
        ensure!(
            node.rule_instance.dependencies().as_slice() == declaration.dependencies,
            "proto SOURCE raw dependencies differ from its declaration: {label}"
        );
        let expected = dependencies(&view, &label, declaration.dependencies)?;
        ensure!(
            node.dependencies.iter().eq(expected.iter()),
            "proto SOURCE exact graph dependencies differ from declaration: {label}"
        );
        let index = *indices
            .get(&label)
            .context("proto SOURCE is outside scoped order")?;
        for dependency in expected {
            ensure!(
                *indices
                    .get(&dependency)
                    .context("proto dependency outside scoped order")?
                    < index,
                "proto SOURCE dependency must precede consumer (cycle or invalid order): {label}"
            );
            queue.push_back(dependency);
        }
    }
    for label in view.order {
        let node = view
            .graph
            .get(label)
            .context("missing source-lane consumer")?;
        for dependency in node
            .dependencies
            .iter()
            .filter(|dep| selected.contains(*dep))
        {
            ensure!(selected.contains(label) || generation.get(label).is_some_and(|deps| deps.contains(dependency)),
                "proto SOURCE facet also has an ordinary runtime/descriptor consumer: {label} -> {dependency}");
        }
    }
    let packages: HashSet<_> = selected
        .iter()
        .map(|label| (label.workspace.clone(), label.package.clone()))
        .collect();
    // Cross-check every visible graph declaration in each relevant package, not
    // only admitted/source nodes. Catalog-only siblings are supplied by root.
    for (label, node) in view
        .graph
        .iter_target_nodes()
        .filter(|(label, _)| packages.contains(&(label.workspace.clone(), label.package.clone())))
    {
        if node.rule_instance.rule_type() != "source_file" {
            let routed = crate::native_identity_forwarder::resolve_actual(
                view.graph, label, node.rule_instance.as_ref(),
            )?;
            let owner = routed.as_ref().unwrap_or(label);
            ensure!(
                declared_output_owners.get(label) == Some(owner),
                "proto SOURCE catalog omits non-SOURCE target self-route: {label}"
            );
        }
        for output in node.rule_instance.declared_outputs() {
            let output = DependencyGraph::normalize_declared_output(label, &output)?;
            let output_label = Label {
                workspace: label.workspace.clone(),
                package: label.package.clone(),
                target: output.to_str().context("non-UTF8 output label")?.to_owned(),
            };
            ensure!(
                declared_output_owners.get(&output_label) == Some(label),
                "proto SOURCE catalog disagrees with declared producer: {output_label}"
            );
        }
    }
    let action_owners: HashSet<_> = registry
        .declared_actions()
        .map(|(_, action)| action.owner_target)
        .collect();
    let mut observations = HashMap::<PathBuf, ObservedProtoSource>::new();
    let mut admitted = AdmittedProtoSources::new();
    for (index, label) in view
        .order
        .iter()
        .enumerate()
        .filter(|(_, label)| selected.contains(*label))
    {
        let node = view
            .graph
            .get(label)
            .context("missing selected proto SOURCE")?;
        let rule = node.rule_instance.as_ref();
        let declaration = rule
            .proto_source_metadata()
            .context("proto SOURCE declaration disappeared")?;
        let target = *view
            .targets
            .get(&label.to_string())
            .context("missing proto SOURCE target")?;
        ensure!(
            rule.target_name() == label.target
                && rule.configuration_error().is_none()
                && rule.analysis_only_dependencies().is_empty()
                && rule.exec_only_dependencies().is_empty()
                && rule.generation_only_proto_dependencies().is_empty()
                && rule.declared_outputs().is_empty()
                && rule.undeclared_source_inputs().is_empty()
                && rule.identity_artifact_forwarder().is_none()
                && rule.cc_projection_kind().is_none()
                && rule.options_header_declaration().is_none()
                && rule.providers().is_empty(),
            "proto SOURCE has additional own semantics: {label}"
        );
        ensure!(
            !pending_targets.contains(&target) && !action_owners.contains(&target),
            "proto SOURCE owns actions or pending writers: {label}"
        );
        let proto = proto_only(
            view.providers
                .get(&label.to_string())
                .context("missing proto SOURCE providers")?,
        )?;
        proto.validate_declared_imports(registry, config)?;
        let actual_layouts: HashMap<_, _> = proto
            .declared_imports
            .as_ref()
            .context("unknown proto SOURCE import layout")?
            .iter()
            .map(|layout| (layout.artifact, layout))
            .collect();
        let expected = layouts(&view, label, declaration)?;
        ensure!(
            proto.direct_sources.len() == expected.len(),
            "proto SOURCE direct declaration count differs: {label}"
        );
        let mut direct = HashSet::new();
        for (&id, (source_label, path, import_root, import_path)) in
            proto.direct_sources.iter().zip(&expected)
        {
            ensure!(
                !declared_output_owners.contains_key(source_label),
                "declared producer/non-SOURCE target collides with proto SOURCE: {source_label}"
            );
            ensure!(
                direct.insert(id) && registry.artifact_path(id) == path.as_path(),
                "proto SOURCE direct path/identity differs from literal declaration: {label}"
            );
            ensure!(
                actual_layouts.get(&id)
                    == Some(&ProtoSourceImport {
                        artifact: id,
                        import_root: import_root.clone(),
                        import_path: import_path.clone(),
                    }),
                "proto SOURCE direct import root/path differs from strip declaration: {label}"
            );
        }
        let mut closure = direct.clone();
        for dependency in dependencies(&view, label, declaration.dependencies)? {
            let dependency_index = indices[&dependency];
            let dependency = admitted
                .get(&dependency_index)
                .context("proto SOURCE dependency was not admitted first")?;
            for source in &dependency.sources {
                ensure!(
                    actual_layouts.get(&source.artifact)
                        == Some(&ProtoSourceImport {
                            artifact: source.artifact,
                            import_root: source.import_root.clone(),
                            import_path: source.import_path.clone(),
                        }),
                    "proto SOURCE transitive import layout differs from its prerequisite"
                );
            }
            closure.extend(dependency.sources.iter().map(|source| source.artifact));
        }
        let sources: Vec<_> = proto
            .transitive_sources
            .iter()
            .filter({
                let mut seen = HashSet::new();
                move |id| seen.insert(*id)
            })
            .collect();
        ensure!(
            sources.iter().copied().collect::<HashSet<_>>() == closure,
            "proto SOURCE provider includes undeclared source closure: {label}"
        );
        ensure!(
            !(0..registry.artifact_count())
                .map(|index| ArtifactId(index as u32))
                .any(|id| registry.artifact(id).owner_target == target && !direct.contains(&id)),
            "proto SOURCE owns unexpected artifacts: {label}"
        );
        let files = sources
            .into_iter()
            .map(|artifact| {
                let data = registry.artifact(artifact);
                ensure!(
                    owners.contains_key(&data.owner_target),
                    "proto SOURCE owner is outside current snapshot"
                );
                let path = registry.artifact_path(artifact);
                if !observations.contains_key(path) {
                    let observed = observe(path)?;
                    ensure!(
                        observed.path == path
                            && observed.canonical_path.is_absolute()
                            && observed.metadata.mode & 0o170000 == 0o100000
                            && observed.configuration == view.configuration,
                        "proto SOURCE observation belongs to another path/command"
                    );
                    observations.insert(path.to_owned(), observed);
                }
                let layout = actual_layouts
                    .get(&artifact)
                    .context("missing proto SOURCE layout")?;
                Ok(AdmittedProtoFile {
                    artifact,
                    owner: data.owner_target,
                    import_root: layout.import_root.clone(),
                    import_path: layout.import_path.clone(),
                    observation: observations[path].clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        admitted.insert(
            index,
            AdmittedProtoSource {
                target,
                direct_sources: proto.direct_sources.to_vec(),
                sources: files,
            },
        );
    }
    // Native metadata dependencies and generation consumers can have different
    // root orders; validate both, even when the demand plan leaves them inactive.
    for label in view
        .order
        .iter()
        .filter(|label| selected.contains(*label) || generation.contains_key(*label))
    {
        let proto = view
            .providers
            .get(&label.to_string())
            .and_then(ProviderSet::get_proto_files)
            .with_context(|| {
                format!("generation SOURCE consumer has no ProtoFilesInfo: {label}")
            })?;
        if generation.contains_key(label) {
            let repository = root(&view, label)?;
            let package = repository.join(&label.package);
            for id in &proto.direct_sources {
                let path = registry.artifact_path(*id);
                let relative = path.strip_prefix(&package)
                    .context("generation proto SOURCE is outside its package")?;
                check_source_package_boundaries(repository, &label.package, relative)?;
                let source = Label {
                    workspace: label.workspace.clone(), package: label.package.clone(),
                    target: relative.to_str().context("non-UTF8 generation source")?.to_owned(),
                };
                ensure!(!declared_output_owners.contains_key(&source),
                    "declared producer/non-SOURCE target collides with generation proto SOURCE: {source}");
            }
        }
        validate_import_winners(proto, registry, config, view.configuration, observe)?;
    }
    Ok(admitted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_rules::analysis_context::{AnalysisContext, StubIncludeResolver};
    use rush_rules::context::Toolchain;
    use rush_rules::{BuildRule, RuleInstance, Value};

    struct GenerationConsumer {
        name: String,
        dependencies: Vec<String>,
        imports: Vec<String>,
    }
    #[async_trait::async_trait]
    impl RuleInstance for GenerationConsumer {
        fn target_name(&self) -> &str {
            &self.name
        }
        fn rule_type(&self) -> &str {
            "fixture_generation_consumer"
        }
        fn dependencies(&self) -> Vec<String> {
            self.dependencies.clone()
        }
        fn generation_only_proto_dependencies(&self) -> Vec<String> {
            self.imports.clone()
        }
        async fn build(
            &self,
            _: &rush_rules::context::BuildContext,
        ) -> Result<Vec<rush_rules::Artifact>> {
            anyhow::bail!("structural fixture must never execute a writer")
        }
    }

    struct Fixture {
        _temporary: tempfile::TempDir,
        main: PathBuf,
        source: PathBuf,
        graph: DependencyGraph,
        order: Vec<Label>,
        targets: HashMap<String, TargetId>,
        providers: HashMap<String, ProviderSet>,
        repositories: HashMap<String, PathBuf>,
        registry: ArtifactRegistry,
        catalog: HashMap<Label, Label>,
    }
    impl Fixture {
        fn new() -> Result<Self> {
            let temporary = tempfile::TempDir::new()?;
            let main = temporary.path().join("main");
            let protobuf = temporary.path().join("protobuf");
            let source = protobuf.join("src/google/protobuf/any.proto");
            std::fs::create_dir_all(&main)?;
            std::fs::create_dir_all(source.parent().unwrap())?;
            std::fs::write(&source, "syntax = \"proto3\"; message Any {}\n")?;
            let mut graph = DependencyGraph::new();
            let mut registry = ArtifactRegistry::new();
            let mut order = Vec::new();
            let mut targets = HashMap::new();
            let mut providers = HashMap::new();
            let mut catalog = HashMap::new();
            for (name, target) in [("any", TargetId(1)), ("copy", TargetId(2))] {
                let label = Label::parse(&format!("@protobuf//:{name}"), None, None)?;
                let rule =
                    rush_rules::rules_cc::ProtoLibraryRule.parse_attributes(&HashMap::from([
                        ("name".into(), Value::String(name.into())),
                        (
                            "srcs".into(),
                            Value::List(vec![Value::String(
                                "src/google/protobuf/any.proto".into(),
                            )]),
                        ),
                        ("strip_import_prefix".into(), Value::String("src".into())),
                    ]))?;
                let resolver = StubIncludeResolver::new(protobuf.clone());
                let toolchain = Toolchain::default();
                let mut ctx = AnalysisContext::new(
                    target,
                    ConfigId::DEFAULT,
                    "",
                    Some("protobuf"),
                    Path::new("/output"),
                    &mut registry,
                    &resolver,
                    &toolchain,
                    &protobuf,
                    &[],
                );
                rule.validate_analysis_dependencies(&ctx)?;
                let result = rule.analyze(&mut ctx, &[]);
                ensure!(
                    ctx.native_namespace_analysis_error().is_none(),
                    "native proto fixture failed"
                );
                graph.add_node(label.clone(), rule)?;
                providers.insert(label.to_string(), result);
                targets.insert(label.to_string(), target);
                catalog.insert(label.clone(), label.clone());
                order.push(label);
            }
            let generation = Label::parse("//:generate", None, None)?;
            let dependencies: Vec<_> = order.iter().map(ToString::to_string).collect();
            graph.add_node(
                generation.clone(),
                Box::new(GenerationConsumer {
                    name: "generate".into(),
                    dependencies: dependencies.clone(),
                    imports: dependencies,
                }),
            )?;
            let own_source = main.join("message.proto");
            std::fs::write(&own_source, "syntax = \"proto3\"; message Message {}\n")?;
            let own_path = registry.intern_path(&own_source);
            let own_id = registry.intern_artifact(
                rush_action::ids::ArtifactKey {
                    config: ConfigId::DEFAULT,
                    exec_path: own_path,
                },
                ArtifactKind::Source,
                true,
                TargetId(3),
                None,
            );
            let proto = ProtoFilesInfo::from_declared_sources_and_deps(
                vec![ProtoSourceImport {
                    artifact: own_id,
                    import_root: main.clone(),
                    import_path: "message.proto".into(),
                }],
                order.iter().map(|label| {
                    providers[&label.to_string()]
                        .get_proto_files()
                        .unwrap()
                        .clone()
                }),
            )?;
            providers.insert(
                generation.to_string(),
                ProviderSet::empty().with_proto_files(proto),
            );
            targets.insert(generation.to_string(), TargetId(3));
            catalog.insert(generation.clone(), generation.clone());
            order.push(generation);
            Ok(Self {
                _temporary: temporary,
                main,
                source,
                graph,
                order,
                targets,
                providers,
                repositories: HashMap::from([("protobuf".into(), protobuf)]),
                registry,
                catalog,
            })
        }
        fn view(&self, configuration: [u8; 32]) -> View<'_> {
            View {
                graph: &self.graph,
                order: &self.order,
                targets: &self.targets,
                providers: &self.providers,
                repositories: &self.repositories,
                main: &self.main,
                configuration,
            }
        }
        fn admit(
            &self,
            roots: &[Label],
            pending: &HashSet<TargetId>,
            config: ConfigId,
        ) -> Result<AdmittedProtoSources> {
            admit(
                self.view(rush_incremental::tracked::config_fingerprint()),
                roots,
                &self.registry,
                pending,
                config,
                &self.catalog,
                &mut observe_tracked_source,
            )
        }
        fn roots(&self) -> Vec<Label> {
            vec![self.order[2].clone()]
        }
    }

    #[test]
    fn proto_source_admission_uses_actual_native_sources_and_shared_snapshot_owners() -> Result<()>
    {
        let fixture = Fixture::new()?;
        rush_incremental::tracked::test_scope(&fixture.main, || -> Result<()> {
            let admitted = fixture.admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)?;
            assert_eq!(admitted.len(), 2);
            for index in [0, 1] {
                let record = &admitted[&index];
                assert_eq!(record.target, TargetId(index as u32 + 1));
                assert_eq!(record.sources.len(), 1);
                assert_eq!(record.direct_sources, [record.sources[0].artifact]);
                assert_eq!(record.sources[0].owner, TargetId(1));
                assert_eq!(record.sources[0].observation.path, fixture.source);
                assert_eq!(
                    record.sources[0].import_path,
                    Path::new("google/protobuf/any.proto")
                );
                assert_eq!(
                    record.sources[0].import_root,
                    fixture.repositories["protobuf"].join("src")
                );
            }
            assert_eq!(admitted[&0].sources, admitted[&1].sources);
            let again = fixture.admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)?;
            assert_eq!(admitted, again);
            assert_eq!(fixture.registry.declared_action_count(), 0);
            // Actual tracked content/full stat observations, not a synthetic IO provider.
            let manifest = rush_incremental::tracked::current_required().manifest();
            assert!(manifest.deps().iter().any(|dep| matches!(dep,
                rush_incremental::Dependency::FileContent { path, .. }
                if path == &admitted[&0].sources[0].observation.canonical_path)));
            assert!(manifest.deps().iter().any(|dep| matches!(dep,
                rush_incremental::Dependency::FileMetadata { path, .. }
                if path == &admitted[&0].sources[0].observation.canonical_path)));
            Ok(())
        })
    }

    #[test]
    fn proto_source_admission_refuses_explicit_mixed_runtime_config_and_pending_requests(
    ) -> Result<()> {
        let mut fixture = Fixture::new()?;
        rush_incremental::tracked::test_scope(&fixture.main, || -> Result<()> {
            for roots in [
                vec![fixture.order[0].clone()],
                vec![fixture.order[2].clone(), fixture.order[0].clone()],
            ] {
                assert!(fixture
                    .admit(&roots, &HashSet::new(), ConfigId::DEFAULT)
                    .unwrap_err()
                    .to_string()
                    .contains("descriptor outputs"));
            }
            assert!(fixture
                .admit(&fixture.roots(), &HashSet::new(), ConfigId(7))
                .is_err());
            assert!(fixture
                .admit(
                    &fixture.roots(),
                    &HashSet::from([TargetId(1)]),
                    ConfigId::DEFAULT
                )
                .is_err());
            Ok(())
        })?;
        let ordinary = Label::parse("//:ordinary", None, None)?;
        fixture.graph.add_node(
            ordinary.clone(),
            Box::new(GenerationConsumer {
                name: "ordinary".into(),
                dependencies: vec![fixture.order[0].to_string()],
                imports: Vec::new(),
            }),
        )?;
        fixture.targets.insert(ordinary.to_string(), TargetId(4));
        fixture.order.push(ordinary.clone());
        rush_incremental::tracked::test_scope(&fixture.main, || -> Result<()> {
            let error = fixture
                .admit(
                    &[fixture.order[2].clone(), ordinary],
                    &HashSet::new(),
                    ConfigId::DEFAULT,
                )
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("ordinary runtime/descriptor consumer"),
                "{error:#}"
            );
            Ok(())
        })
    }

    #[test]
    fn proto_source_admission_refuses_catalog_non_source_routes_and_extra_provider_authority(
    ) -> Result<()> {
        let mut fixture = Fixture::new()?;
        let source_label = Label::parse("@protobuf//:src/google/protobuf/any.proto", None, None)?;
        // A catalog-only same-name non-SOURCE self-route must reject even if it
        // has not been instantiated or registered and a prior file exists.
        fixture
            .catalog
            .insert(source_label.clone(), source_label.clone());
        rush_incremental::tracked::test_scope(&fixture.main, || -> Result<()> {
            let error = fixture
                .admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)
                .unwrap_err();
            assert!(
                error.to_string().contains("collides with proto SOURCE"),
                "{error:#}"
            );
            Ok(())
        })?;
        fixture.catalog.remove(&source_label);
        let label = fixture.order[0].to_string();
        let mixed =
            fixture.providers[&label]
                .clone()
                .with_files(rush_rules::providers::FilesInfo::new(vec![fixture
                    .source
                    .clone()]));
        fixture.providers.insert(label, mixed);
        rush_incremental::tracked::test_scope(&fixture.main, || -> Result<()> {
            assert!(fixture
                .admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)
                .unwrap_err()
                .to_string()
                .contains("non-proto provider authority"));
            Ok(())
        })
    }

    #[test]
    fn proto_source_admission_readmission_observes_edits_removal_and_wrong_command() -> Result<()> {
        let fixture = Fixture::new()?;
        rush_incremental::tracked::test_scope(&fixture.main, || -> Result<()> {
            let first = fixture.admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)?;
            std::fs::write(&fixture.source, "syntax = \"proto3\"; message Changed {}\n")?;
            let second = fixture.admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)?;
            assert_ne!(first, second);
            let mut wrong_command = rush_incremental::tracked::config_fingerprint();
            wrong_command[0] ^= 1;
            let error = admit(
                fixture.view(wrong_command),
                &fixture.roots(),
                &fixture.registry,
                &HashSet::new(),
                ConfigId::DEFAULT,
                &fixture.catalog,
                &mut observe_tracked_source,
            )
            .unwrap_err();
            assert!(error.to_string().contains("another path/command"));
            std::fs::remove_file(&fixture.source)?;
            assert!(fixture
                .admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)
                .is_err());
            Ok(())
        })
    }

    #[test]
    fn proto_source_rejects_subpackage_boundaries_and_records_missing_boundaries() -> Result<()> {
        let fixture = Fixture::new()?;
        rush_incremental::tracked::test_scope(&fixture.main, || -> Result<()> {
            fixture.admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)?;
            let boundary = fixture.repositories["protobuf"].join("src/google/BUILD.bazel");
            let manifest = rush_incremental::tracked::current_required().manifest();
            assert!(manifest.deps().iter().any(|dep| matches!(dep,
                rush_incremental::Dependency::FileMetadata { path, present: false, .. }
                if path == &boundary)));
            std::fs::write(
                &boundary,
                "genrule(name='writer', outs=['protobuf/any.proto'])\n",
            )?;
            let error = fixture
                .admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)
                .unwrap_err();
            assert!(error.to_string().contains("package boundary"), "{error:#}");
            assert!(
                fixture.source.is_file(),
                "prior source/output bytes must remain"
            );
            std::fs::remove_file(&boundary)?;
            fixture.admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)?;
            std::fs::write(
                boundary.with_file_name("BUILD"),
                "filegroup(name='boundary')\n",
            )?;
            assert!(fixture
                .admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)
                .unwrap_err()
                .to_string()
                .contains("package boundary"));
            Ok(())
        })
    }

    #[test]
    fn proto_source_search_rejects_undeclared_earlier_root_shadow() -> Result<()> {
        let fixture = Fixture::new()?;
        rush_incremental::tracked::test_scope(&fixture.main, || -> Result<()> {
            fixture.admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)?;
            let shadow = fixture.main.join("google/protobuf/any.proto");
            let manifest = rush_incremental::tracked::current_required().manifest();
            assert!(manifest.deps().iter().any(|dep| matches!(dep,
                rush_incremental::Dependency::FileMetadata { path, present: false, .. }
                if path == &shadow)));
            std::fs::create_dir_all(shadow.parent().unwrap())?;
            std::fs::copy(&fixture.source, &shadow)?;
            let error = fixture
                .admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)
                .unwrap_err();
            assert!(
                error.to_string().contains("shadows declared SOURCE"),
                "{error:#}"
            );
            assert!(fixture.source.is_file());
            std::fs::remove_file(&shadow)?;
            fixture.admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)?;
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn proto_source_search_refuses_earlier_symlink_even_with_same_bytes() -> Result<()> {
        let fixture = Fixture::new()?;
        rush_incremental::tracked::test_scope(&fixture.main, || -> Result<()> {
            fixture.admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)?;
            let shadow = fixture.main.join("google/protobuf/any.proto");
            std::fs::create_dir_all(shadow.parent().unwrap())?;
            std::os::unix::fs::symlink(&fixture.source, &shadow)?;
            assert!(fixture
                .admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)
                .unwrap_err()
                .to_string()
                .contains("shadows declared SOURCE"));
            std::fs::remove_file(&shadow)?;
            fixture.admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)?;
            Ok(())
        })
    }

    #[test]
    fn proto_source_generation_own_inputs_refuse_nested_packages_and_producers() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let source = fixture.main.join("nested/message.proto");
        std::fs::create_dir_all(source.parent().unwrap())?;
        std::fs::write(&source, "syntax = \"proto3\"; message Message {}\n")?;
        let path = fixture.registry.intern_path(&source);
        let id = fixture.registry.intern_artifact(
            rush_action::ids::ArtifactKey { config: ConfigId::DEFAULT, exec_path: path },
            ArtifactKind::Source, true, TargetId(3), None,
        );
        let proto = ProtoFilesInfo::from_declared_sources_and_deps(
            vec![ProtoSourceImport { artifact: id, import_root: fixture.main.clone(),
                import_path: "nested/message.proto".into() }],
            fixture.order[..2].iter().map(|label|
                fixture.providers[&label.to_string()].get_proto_files().unwrap().clone()),
        )?;
        fixture.providers.insert(fixture.order[2].to_string(),
            ProviderSet::empty().with_proto_files(proto));
        rush_incremental::tracked::test_scope(&fixture.main, || -> Result<()> {
            fixture.admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)?;
            let boundary = fixture.main.join("nested/BUILD.bazel");
            std::fs::write(&boundary, "genrule(name='writer', outs=['message.proto'])\n")?;
            let error = fixture.admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)
                .unwrap_err();
            assert!(error.to_string().contains("package boundary"), "{error:#}");
            std::fs::remove_file(boundary)?;
            Ok(())
        })?;
        let source_label = Label::parse("//:nested/message.proto", None, None)?;
        fixture.catalog.insert(source_label.clone(), source_label);
        rush_incremental::tracked::test_scope(&fixture.main, || -> Result<()> {
            let error = fixture.admit(&fixture.roots(), &HashSet::new(), ConfigId::DEFAULT)
                .unwrap_err();
            assert!(error.to_string().contains("generation proto SOURCE"), "{error:#}");
            Ok(())
        })
    }
}
