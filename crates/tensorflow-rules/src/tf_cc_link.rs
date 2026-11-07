use rush_action::ids::{ArtifactId, ArtifactRegistry, LinkerInputId};
use rush_rules::providers::{CcInfo, LinkFragment};
use std::collections::HashSet;
use std::sync::Arc;

const PER_INPUT_FLAGS_UNSUPPORTED: &str = "tf_cc per-input linker flags are not scheduler-modeled";
pub(crate) const UNTYPED_ARTIFACT_REFERENCE_UNSUPPORTED: &str =
    "tf_cc linker argument may name an undeclared artifact";

/// Ordered dependency-link arguments and their complete typed input set.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TfCcLinkPlan {
    pub(crate) argv: Vec<String>,
    pub(crate) declared_inputs: Vec<ArtifactId>,
    pub(crate) unsupported_reason: Option<&'static str>,
}

impl TfCcLinkPlan {
    pub(crate) fn has_typed_link_input(&self) -> bool {
        !self.declared_inputs.is_empty()
    }
}

pub(crate) fn unsupported_raw_link_args(args: &[String]) -> Option<&'static str> {
    args.iter()
        .any(|arg| raw_flag_may_name_artifact(arg))
        .then_some(UNTYPED_ARTIFACT_REFERENCE_UNSUPPORTED)
}

/// Assemble the common dependency portion of TensorFlow C++ link commands.
///
/// Ordinary linker inputs follow the verified rules_cc ordering: alwayslink
/// archives first as individual whole-archive triples, then normal archives
/// in one group. Typed fragments follow in provider order. Explicit Group
/// members retain their order and repetitions; only declared ArtifactIds are
/// deduplicated, with their first occurrence winning. Outside explicit groups,
/// repeated literal archive paths retain the verified first-occurrence behavior
/// used by the legacy TensorFlow link path.
pub(crate) fn assemble_tf_cc_link_plan(
    cc_infos: &[Arc<CcInfo>],
    registry: &ArtifactRegistry,
) -> TfCcLinkPlan {
    let mut plan = TfCcLinkPlan::default();
    let mut seen_inputs = HashSet::new();
    let mut ordinary = Vec::new();

    for cc_info in cc_infos {
        for linker_input_id in cc_info.linking.linker_inputs.iter() {
            let linker_input = registry.linker_input(linker_input_id);
            ordinary.push(rush_verified_core::DepLib::new(
                registry
                    .artifact_path(linker_input.artifact)
                    .display()
                    .to_string(),
                linker_input.alwayslink,
            ));
            record_linker_input(linker_input_id, registry, &mut plan, &mut seen_inputs);
        }
    }

    plan.argv = rush_verified_core::assemble_link_plan(Vec::new(), ordinary, Vec::new(), false);
    let mut ungrouped_fragment_args = Vec::new();
    let mut seen_ungrouped_archives = HashSet::new();

    for cc_info in cc_infos {
        for fragment in cc_info.linking.link_fragments.iter() {
            match fragment {
                LinkFragment::Flag(string_id) => {
                    let flag = registry.strings().str(string_id).to_string();
                    if raw_flag_may_name_artifact(&flag) {
                        plan.unsupported_reason
                            .get_or_insert(UNTYPED_ARTIFACT_REFERENCE_UNSUPPORTED);
                    }
                    ungrouped_fragment_args.push(flag);
                }
                LinkFragment::SearchDir(path_id) => {
                    plan.unsupported_reason
                        .get_or_insert(UNTYPED_ARTIFACT_REFERENCE_UNSUPPORTED);
                    ungrouped_fragment_args
                        .push(format!("-L{}", registry.paths().path(path_id).display()));
                }
                LinkFragment::Library(linker_input_id) => {
                    append_fragment_linker_input(
                        linker_input_id,
                        registry,
                        &mut ungrouped_fragment_args,
                    );
                    record_linker_input(linker_input_id, registry, &mut plan, &mut seen_inputs);
                }
                LinkFragment::Group(linker_input_ids) => {
                    flush_ungrouped_fragment_args(
                        &mut plan.argv,
                        &mut ungrouped_fragment_args,
                        &mut seen_ungrouped_archives,
                    );
                    plan.argv.push("-Wl,--start-group".to_string());
                    for linker_input_id in linker_input_ids.iter().copied() {
                        append_fragment_linker_input(linker_input_id, registry, &mut plan.argv);
                        record_linker_input(linker_input_id, registry, &mut plan, &mut seen_inputs);
                    }
                    plan.argv.push("-Wl,--end-group".to_string());
                }
            }
        }
    }
    flush_ungrouped_fragment_args(
        &mut plan.argv,
        &mut ungrouped_fragment_args,
        &mut seen_ungrouped_archives,
    );

    plan
}

fn flush_ungrouped_fragment_args(
    out: &mut Vec<String>,
    pending: &mut Vec<String>,
    seen_archives: &mut HashSet<String>,
) {
    let args = std::mem::take(pending);
    let mut index = 0;
    while index < args.len() {
        if args[index] == "-Wl,--whole-archive"
            && index + 2 < args.len()
            && args[index + 2] == "-Wl,--no-whole-archive"
            && rush_verified_core::is_archive_path(&args[index + 1])
        {
            if seen_archives.insert(args[index + 1].clone()) {
                out.extend(args[index..index + 3].iter().cloned());
            }
            index += 3;
            continue;
        }
        if !rush_verified_core::is_archive_path(&args[index])
            || seen_archives.insert(args[index].clone())
        {
            out.push(args[index].clone());
        }
        index += 1;
    }
}

fn append_fragment_linker_input(
    linker_input_id: LinkerInputId,
    registry: &ArtifactRegistry,
    argv: &mut Vec<String>,
) {
    let linker_input = registry.linker_input(linker_input_id);
    let path = registry
        .artifact_path(linker_input.artifact)
        .display()
        .to_string();
    if linker_input.alwayslink {
        argv.push("-Wl,--whole-archive".to_string());
        argv.push(path);
        argv.push("-Wl,--no-whole-archive".to_string());
    } else {
        argv.push(path);
    }
}

fn record_linker_input(
    linker_input_id: LinkerInputId,
    registry: &ArtifactRegistry,
    plan: &mut TfCcLinkPlan,
    seen_inputs: &mut HashSet<ArtifactId>,
) {
    let linker_input = registry.linker_input(linker_input_id);
    push_unique(
        &mut plan.declared_inputs,
        seen_inputs,
        linker_input.artifact,
    );
    if let Some(extras) = &linker_input.extras {
        for artifact in extras.additional_inputs.iter().copied() {
            push_unique(&mut plan.declared_inputs, seen_inputs, artifact);
        }
        if !extras.per_input_link_flags.is_empty() {
            plan.unsupported_reason
                .get_or_insert(PER_INPUT_FLAGS_UNSUPPORTED);
        }
    }
}

fn push_unique(out: &mut Vec<ArtifactId>, seen: &mut HashSet<ArtifactId>, artifact: ArtifactId) {
    if seen.insert(artifact) {
        out.push(artifact);
    }
}

fn raw_flag_may_name_artifact(flag: &str) -> bool {
    let flag = flag.trim();
    if flag.starts_with('@')
        || flag.contains(",@")
        || flag.contains("--version-script")
        || flag.contains("--dynamic-list")
        || flag.contains("--script")
        || flag.contains("--retain-symbols-file")
        || flag.contains("--just-symbols")
        || flag == "-T"
        || flag.starts_with("-T")
        || flag.contains(",-T,")
    {
        return true;
    }
    flag.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "-L"
            || (candidate.starts_with("-L") && candidate.len() > 2)
            || candidate == "--library-path"
            || candidate.starts_with("--library-path=")
            || candidate.starts_with("-l:")
            || candidate.starts_with("--library=:")
            || rush_verified_core::is_archive_path(candidate)
            || [".dylib", ".tbd", ".o", ".lo", ".ld", ".lds", ".map"]
                .iter()
                .any(|suffix| candidate.ends_with(suffix))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_action::ids::{
        ArtifactKey, ArtifactKind, ConfigId, LinkerInputData, LinkerInputExtras, TargetId,
    };
    use rush_rules::providers::DirectCcInfo;
    use std::path::Path;

    fn artifact(registry: &mut ArtifactRegistry, path: &str) -> ArtifactId {
        let exec_path = registry.intern_path(Path::new(path));
        registry.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path,
            },
            ArtifactKind::StaticLibrary,
            false,
            TargetId(1),
            None,
        )
    }

    fn linker_input(
        registry: &mut ArtifactRegistry,
        artifact: ArtifactId,
        alwayslink: bool,
        extras: Option<Arc<LinkerInputExtras>>,
    ) -> LinkerInputId {
        registry.intern_linker_input(LinkerInputData {
            owner: TargetId(1),
            artifact,
            alwayslink,
            extras,
        })
    }

    #[test]
    fn preserves_typed_order_and_collects_every_artifact() {
        let mut registry = ArtifactRegistry::new();
        let a = artifact(&mut registry, "/lib/A.a");
        let b = artifact(&mut registry, "/lib/B.a");
        let c = artifact(&mut registry, "/lib/C.a");
        let d = artifact(&mut registry, "/lib/D.a");
        let e = artifact(&mut registry, "/obj/linkstamp.o");
        let a_input = linker_input(&mut registry, a, false, None);
        let b_input = linker_input(&mut registry, b, true, None);
        let c_input = linker_input(
            &mut registry,
            c,
            true,
            Some(Arc::new(LinkerInputExtras {
                additional_inputs: vec![e].into_boxed_slice(),
                per_input_link_flags: Vec::new().into_boxed_slice(),
            })),
        );
        let d_input = linker_input(&mut registry, d, false, None);
        let pthread = registry.intern_string("-pthread");
        let search = registry.intern_path(Path::new("/search"));
        let cc_info = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                linker_inputs: vec![a_input, b_input, a_input],
                link_fragments: vec![
                    LinkFragment::Library(c_input),
                    LinkFragment::Group(vec![d_input, d_input].into_boxed_slice()),
                    LinkFragment::Library(c_input),
                    LinkFragment::Flag(pthread),
                    LinkFragment::SearchDir(search),
                ],
                ..DirectCcInfo::default()
            },
            std::iter::empty(),
        );

        let plan = assemble_tf_cc_link_plan(&[cc_info], &registry);

        assert_eq!(
            plan.argv,
            vec![
                "-Wl,--whole-archive",
                "/lib/B.a",
                "-Wl,--no-whole-archive",
                "-Wl,--start-group",
                "/lib/A.a",
                "-Wl,--end-group",
                "-Wl,--whole-archive",
                "/lib/C.a",
                "-Wl,--no-whole-archive",
                "-Wl,--start-group",
                "/lib/D.a",
                "/lib/D.a",
                "-Wl,--end-group",
                "-pthread",
                "-L/search",
            ]
        );
        assert_eq!(plan.declared_inputs, vec![a, b, c, e, d]);
        assert_eq!(
            plan.unsupported_reason,
            Some(UNTYPED_ARTIFACT_REFERENCE_UNSUPPORTED)
        );
    }

    #[test]
    fn flags_unmodeled_per_input_and_raw_artifact_shapes() {
        let mut registry = ArtifactRegistry::new();
        let library = artifact(&mut registry, "/lib/flagged.a");
        let per_input_flag = registry.intern_string("-Wl,--as-needed");
        let linker_input = linker_input(
            &mut registry,
            library,
            false,
            Some(Arc::new(LinkerInputExtras {
                additional_inputs: Vec::new().into_boxed_slice(),
                per_input_link_flags: vec![per_input_flag].into_boxed_slice(),
            })),
        );
        let raw_artifact = registry.intern_string("/opaque/libraw.a");
        let cc_info = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                linker_inputs: vec![linker_input],
                link_fragments: vec![LinkFragment::Flag(raw_artifact)],
                ..DirectCcInfo::default()
            },
            std::iter::empty(),
        );

        let plan = assemble_tf_cc_link_plan(&[cc_info], &registry);

        assert_eq!(plan.unsupported_reason, Some(PER_INPUT_FLAGS_UNSUPPORTED));
        assert!(plan.argv.contains(&"/opaque/libraw.a".to_string()));
    }

    #[test]
    fn raw_artifact_flags_are_unsupported_without_an_earlier_reason() {
        let mut registry = ArtifactRegistry::new();
        let raw_artifact = registry.intern_string("/opaque/libraw.a");
        let cc_info = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                link_fragments: vec![
                    LinkFragment::Flag(raw_artifact),
                    LinkFragment::Flag(raw_artifact),
                ],
                ..DirectCcInfo::default()
            },
            std::iter::empty(),
        );

        let plan = assemble_tf_cc_link_plan(&[cc_info], &registry);

        assert_eq!(
            plan.unsupported_reason,
            Some(UNTYPED_ARTIFACT_REFERENCE_UNSUPPORTED)
        );
        assert_eq!(plan.argv, vec!["/opaque/libraw.a"]);
        assert_eq!(
            unsupported_raw_link_args(&["@link.rsp".to_string()]),
            Some(UNTYPED_ARTIFACT_REFERENCE_UNSUPPORTED)
        );
    }

    #[test]
    fn catches_common_file_bearing_linker_option_spellings() {
        for arg in [
            "-Wl,@link.params",
            "-Wl,--version-script,$(location //pkg:script)",
            "--dynamic-list=/opaque/symbols",
            "-Wl,-T,/opaque/script",
            "-Tscript.ld",
            "/opaque/libversioned.so.1",
            "-l:libexact.a",
            "-Wl,-l:libexact.a",
            "-L/opaque/lib",
            "-Wl,-L,/opaque/lib",
            "--library-path=/opaque/lib",
            "-Wl,--whole-archive,/opaque/libpacked.a,--no-whole-archive",
            "-Wl,/opaque/packed.o,-z,defs",
        ] {
            assert_eq!(
                unsupported_raw_link_args(&[arg.to_string()]),
                Some(UNTYPED_ARTIFACT_REFERENCE_UNSUPPORTED),
                "{arg}"
            );
        }
        assert_eq!(
            unsupported_raw_link_args(&[
                "-s".to_string(),
                "-pthread".to_string(),
                "-Wl,-z,defs".to_string(),
            ]),
            None
        );
    }
}
