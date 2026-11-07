pub mod alias;
pub mod android;
pub mod cc_header_only_library;
pub mod cc_import;
pub mod cc_rules;
pub mod expand_template;
pub mod filegroup;
pub mod genrule;
pub mod gentbl_cc_library;
pub mod td_library;
pub mod tf_cc_binary;
pub mod tf_cc_library;
pub(crate) mod tf_cc_link;
pub mod tf_cc_shared_library;
pub mod tf_cc_test;
pub mod tf_gen_op_wrappers_cc;
pub mod tf_gen_options_header;
pub mod tf_generate_proto_text_sources;
pub mod tf_proto_library;

pub use alias::AliasRule;
pub use android::UnsupportedAndroidRule;
pub use cc_header_only_library::CcHeaderOnlyLibraryRule;
pub use cc_import::CcImportRule;
pub use cc_rules::{CcBinaryRule, CcLibraryRule};
pub use expand_template::ExpandTemplateRule;
pub use filegroup::FilegroupRule;
pub use genrule::GenruleRule;
pub use gentbl_cc_library::GentblCcLibraryRule;
pub use td_library::TdLibraryRule;
pub use tf_cc_binary::TfCcBinaryRule;
pub use tf_cc_library::TfCcLibraryRule;
pub use tf_cc_shared_library::TfCcSharedLibraryRule;
pub use tf_cc_test::TfCcTestRule;
pub use tf_gen_op_wrappers_cc::TfGenOpWrappersCcRule;
pub use tf_gen_options_header::TfGenOptionsHeaderRule;
pub use tf_generate_proto_text_sources::TfGenerateProtoTextSourcesRule;
pub use tf_proto_library::TfProtoLibraryRule;

use rush_rules::registry::RuleRegistry;

/// Derive a stable object leaf from a resolved TensorFlow C++ source path.
///
/// For relocation of one logical workspace, strip host-specific source/output
/// roots before using rules_cc's injective key encoding. The namespace tag
/// keeps a checked-in source distinct from a generated file at the same
/// root-relative path, while an absolute fallback preserves identity for files
/// supplied from elsewhere. Repository identity belongs in the target output
/// directory and is not encoded here.
pub(crate) fn tf_cc_obj_name_for_resolved_path(
    path: &std::path::Path,
    workspace_root: &std::path::Path,
    output_dir: &std::path::Path,
) -> String {
    let (namespace, relative) = if let Ok(relative) = path.strip_prefix(output_dir) {
        ("out", relative)
    } else if let Ok(relative) = path.strip_prefix(workspace_root) {
        ("src", relative)
    } else {
        ("abs", path)
    };
    let identity = format!("{namespace}/{}", relative.to_string_lossy());
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("obj");
    rush_rules::rules_cc::cc_obj_name_for_src(&identity, stem)
}

/// TensorFlow C++ rules accept CUDA translation units in addition to the
/// canonical C/C++ and assembly suffixes used by core rules_cc.
pub(crate) fn is_tf_cc_compilable_source(raw: &str) -> bool {
    let file = rush_rules::cc_command_spec::source_file_component(raw);
    std::path::Path::new(file)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "c" | "cc" | "cpp" | "cxx" | "c++" | "cu" | "s"
            )
        })
        .unwrap_or(false)
}

/// Header-like `srcs` entries are private compile inputs, not translation
/// units. `.cuh` is the CUDA counterpart to the ordinary C++ header suffixes.
pub(crate) fn is_tf_cc_header_input(raw: &str) -> bool {
    let file = rush_rules::cc_command_spec::source_file_component(raw);
    std::path::Path::new(file)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "h" | "hh" | "hpp" | "hxx" | "h++" | "inc" | "inl" | "ipp" | "tcc" | "def" | "cuh"
            )
        })
        .unwrap_or(false)
}

/// Register all TensorFlow rules with a rule registry
pub fn register_tensorflow_rules(registry: &mut RuleRegistry) {
    registry.register(Box::new(TfCcLibraryRule));
    registry.register(Box::new(TfCcBinaryRule));
    registry.register(Box::new(TfCcTestRule));
    registry.register(Box::new(TfCcSharedLibraryRule));
    registry.register(Box::new(rush_rules::rules_cc::CcSharedLibraryRule));
    registry.register(Box::new(TfProtoLibraryRule));
    registry.register(Box::new(TfGenOpWrappersCcRule));
    registry.register(Box::new(TfGenerateProtoTextSourcesRule));
    registry.register(Box::new(TfGenOptionsHeaderRule));
    registry.register(Box::new(ExpandTemplateRule));
    for rule_type in ["android_library", "android_binary", "aar_import"] {
        registry.register(Box::new(UnsupportedAndroidRule { rule_type }));
    }

    // Also register standard C++ rules
    registry.register(Box::new(CcLibraryRule));
    registry.register(Box::new(CcBinaryRule));
    registry.register(Box::new(CcImportRule));
    registry.register(Box::new(CcHeaderOnlyLibraryRule));

    // Register Python rules (stub implementations for parsing)
    registry.register(Box::new(rush_rules::PyBinaryRule));
    registry.register(Box::new(rush_rules::PyLibraryRule));
    registry.register(Box::new(rush_rules::PyTestRule));

    // Register genrule
    registry.register(Box::new(GenruleRule));

    // FIX #187: Register MLIR TableGen rule
    registry.register(Box::new(GentblCcLibraryRule));
    // #39: td_library — .td source groups + transitive tblgen -I roots
    registry.register(Box::new(TdLibraryRule));

    // Register Bazel utility rules
    registry.register(Box::new(FilegroupRule));
    registry.register(Box::new(AliasRule));

    // Register system library rules
    registry.register(Box::new(rush_rules::rules_system::SystemCcLibraryRule));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_tensorflow_rules() {
        let mut registry = RuleRegistry::new();
        register_tensorflow_rules(&mut registry);

        assert!(registry.has_rule("tf_cc_library"));
        assert!(registry.has_rule("tf_cc_binary"));
        assert!(registry.has_rule("tf_cc_test"));
        assert!(registry.has_rule("android_library"));
        assert!(registry.has_rule("android_binary"));
        assert!(registry.has_rule("aar_import"));
    }

    #[test]
    fn tf_cc_input_kinds_cover_cuda_without_compiling_headers() {
        assert!(is_tf_cc_compilable_source("//pkg:kernel.cu"));
        assert!(is_tf_cc_compilable_source("asm/start.S"));
        assert!(!is_tf_cc_compilable_source(":detail.cuh"));
        assert!(is_tf_cc_header_input(":detail.cuh"));
        assert!(is_tf_cc_header_input("detail.inc"));
        assert!(!is_tf_cc_header_input("libprebuilt.a"));
    }

    #[test]
    fn tf_cc_object_names_survive_relocation_and_namespace_generated_files() {
        let relative = std::path::Path::new("pkg/deep/source.cc");
        let short_workspace = std::path::Path::new("/ws");
        let deep_workspace = std::path::Path::new("/a/host/specific/cache/root/workspace");
        let short_output = std::path::Path::new("/out");
        let deep_output = std::path::Path::new("/a/host/specific/cache/root/output");

        let short_source = tf_cc_obj_name_for_resolved_path(
            &short_workspace.join(relative),
            short_workspace,
            short_output,
        );
        let deep_source = tf_cc_obj_name_for_resolved_path(
            &deep_workspace.join(relative),
            deep_workspace,
            deep_output,
        );
        let short_generated = tf_cc_obj_name_for_resolved_path(
            &short_output.join(relative),
            short_workspace,
            short_output,
        );
        let deep_generated = tf_cc_obj_name_for_resolved_path(
            &deep_output.join(relative),
            deep_workspace,
            deep_output,
        );

        assert_eq!(short_source, deep_source);
        assert_eq!(short_generated, deep_generated);
        assert_ne!(deep_source, deep_generated);
    }
}
