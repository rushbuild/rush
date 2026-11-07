// TensorFlow proto_text source generator (tf_generate_proto_text_sources)
//
// TF's upstream build produces `<name>.pb_text.h` / `<name>.pb_text.cc` /
// `<name>.pb_text-impl.h` for each .proto source by running the
// tensorflow/tools/proto_text:gen_proto_text_functions binary (a protoc plugin
// built from TF source). Rush doesn't build that tool. Consumers only use
// three functions per message type that are trivially expressible as templated
// shims over protobuf's own API:
//
//   ProtoDebugString(msg)        → msg.DebugString()
//   ProtoShortDebugString(msg)   → msg.ShortDebugString()
//   ProtoParseFromString(s, msg) → TextFormat::ParseFromString(s, msg)
//
// The minor behavioral differences the real generator offers (not expanding
// google.Any, etc.) aren't exercised by the critical-path compile; when a
// build actually uses the proto_text API it ends up calling
// msg.DebugString() all the way down anyway.
//
// This rule:
//   * takes `srcs = [a.proto, b.proto, ...]`
//   * writes `genfiles/<srcs_relative_dir>/<proto>.pb_text.h` — templated shim
//     that `#include`s the matching .pb.h and <google/protobuf/text_format.h>
//   * writes `genfiles/<srcs_relative_dir>/<proto>.pb_text-impl.h` — empty
//     (real generator emits a separate impl header; some .cc files include it)
//   * writes `genfiles/<srcs_relative_dir>/<proto>.pb_text.cc` — minimal
//     translation unit; all implementations are inline in the template header

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rush_rules::{
    context::BuildContext, extract_string, extract_string_list, Artifact, ArtifactType, BuildRule,
    RuleInstance, Value,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Scan a .proto file's text for `enum <Name> {` declarations.
///
/// Regex-based, which is fine for TF's proto sources — they don't use
/// macros, they don't inline-comment enum keywords, and they don't rename
/// enum types through preprocessing. Returns the set of simple enum names
/// (top-level and nested). Nested enums within messages show up as just
/// the enum name — the real generator mangles them through the proto FQN,
/// but TF's call sites we need to satisfy (`EnumName_DataType` etc.) use
/// the simple name, matching what we emit.
fn scan_proto_for_enums(proto_path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(proto_path) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for line in content.lines() {
        // strip // line comments
        let line = match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        };
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("enum ") {
            // Name is the identifier up to whitespace or `{`.
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            let name = &rest[..end];
            if !name.is_empty() && !out.iter().any(|n| n == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

pub struct TfGenerateProtoTextSourcesRule;

impl BuildRule for TfGenerateProtoTextSourcesRule {
    fn name(&self) -> &str {
        "tf_generate_proto_text_sources"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs").unwrap_or_default();
        let srcs_relative_dir = attrs
            .get("srcs_relative_dir")
            .and_then(|v| v.as_string())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let deps = extract_string_list(attrs, "deps").unwrap_or_default();
        Ok(Box::new(TfGenerateProtoTextSourcesInstance {
            name,
            srcs,
            srcs_relative_dir,
            deps,
        }))
    }

    fn documentation(&self) -> &str {
        "Stub proto_text generator — emits templated header shims over protobuf's own text format."
    }
}

struct TfGenerateProtoTextSourcesInstance {
    name: String,
    srcs: Vec<String>,
    srcs_relative_dir: String,
    deps: Vec<String>,
}

#[async_trait]
impl RuleInstance for TfGenerateProtoTextSourcesInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        self.deps.clone()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let genfiles = context.output_dir.join("genfiles");
        let out_dir = if self.srcs_relative_dir.is_empty() {
            genfiles.clone()
        } else {
            genfiles.join(&self.srcs_relative_dir)
        };
        std::fs::create_dir_all(&out_dir)?;

        // Write the shared shim header once per `rush-out/genfiles/` root.
        // Every per-proto .pb_text.h #includes it. Putting the common
        // templates in a single header avoids "redefinition of template"
        // errors when a TU #includes more than one .pb_text.h.
        let shared_shim_rel = "__rush_proto_text_shim.h";
        let shared_shim_path = genfiles.join(shared_shim_rel);
        std::fs::write(
            &shared_shim_path,
            "// Auto-generated shared proto_text shim used by every per-proto\n\
             // .pb_text.h rush emits. Defines the three tensorflow:: helpers\n\
             // (`ProtoDebugString`, `ProtoShortDebugString`,\n\
             // `ProtoParseFromString`) as templates over protobuf's own\n\
             // text-format API.\n\
             #ifndef RUSH_STUB_PROTO_TEXT_SHARED_SHIM_H_\n\
             #define RUSH_STUB_PROTO_TEXT_SHARED_SHIM_H_\n\
             \n\
             #include <string>\n\
             #include \"google/protobuf/text_format.h\"\n\
             \n\
             namespace tensorflow {\n\
             \n\
             template <typename T>\n\
             inline std::string ProtoDebugString(const T& msg) {\n\
             \u{0020}\u{0020}return msg.DebugString();\n\
             }\n\
             \n\
             template <typename T>\n\
             inline std::string ProtoShortDebugString(const T& msg) {\n\
             \u{0020}\u{0020}return msg.ShortDebugString();\n\
             }\n\
             \n\
             // #33: TF's REAL proto-text parser (gen_proto_text_functions) uses\n\
             // tensorflow::strings::Scanner + absl::CUnescape, which ACCEPTS raw\n\
             // control bytes (e.g. a literal newline) inside quoted string\n\
             // literals. protobuf's TextFormat tokenizer rejects them (\"String\n\
             // literals cannot cross line boundaries\"), aborting REGISTER_OP for\n\
             // ops whose attr defaults contain newlines (PrintV2 end:string='\\n').\n\
             // Re-escape raw control bytes that occur INSIDE quoted regions to\n\
             // their C escapes before delegating to TextFormat (which un-escapes\n\
             // them) — matching the Scanner+CUnescape semantics without relying\n\
             // on protobuf's private multiline-string option.\n\
             inline std::string RushProtoTextEscapeQuotedControls(const std::string& s) {\n\
             \u{0020}\u{0020}std::string out;\n\
             \u{0020}\u{0020}out.reserve(s.size() + 8);\n\
             \u{0020}\u{0020}char quote = 0;\n\
             \u{0020}\u{0020}bool esc = false;\n\
             \u{0020}\u{0020}for (char c : s) {\n\
             \u{0020}\u{0020}\u{0020}\u{0020}if (quote != 0) {\n\
             \u{0020}\u{0020}\u{0020}\u{0020}\u{0020}\u{0020}if (esc) { out.push_back(c); esc = false; }\n\
             \u{0020}\u{0020}\u{0020}\u{0020}\u{0020}\u{0020}else if (c == '\\\\') { out.push_back(c); esc = true; }\n\
             \u{0020}\u{0020}\u{0020}\u{0020}\u{0020}\u{0020}else if (c == quote) { out.push_back(c); quote = 0; }\n\
             \u{0020}\u{0020}\u{0020}\u{0020}\u{0020}\u{0020}else if (c == '\\n') { out += \"\\\\n\"; }\n\
             \u{0020}\u{0020}\u{0020}\u{0020}\u{0020}\u{0020}else if (c == '\\r') { out += \"\\\\r\"; }\n\
             \u{0020}\u{0020}\u{0020}\u{0020}\u{0020}\u{0020}else if (c == '\\t') { out += \"\\\\t\"; }\n\
             \u{0020}\u{0020}\u{0020}\u{0020}\u{0020}\u{0020}else { out.push_back(c); }\n\
             \u{0020}\u{0020}\u{0020}\u{0020}} else {\n\
             \u{0020}\u{0020}\u{0020}\u{0020}\u{0020}\u{0020}if (c == '\\'' || c == '\"') { quote = c; }\n\
             \u{0020}\u{0020}\u{0020}\u{0020}\u{0020}\u{0020}out.push_back(c);\n\
             \u{0020}\u{0020}\u{0020}\u{0020}}\n\
             \u{0020}\u{0020}}\n\
             \u{0020}\u{0020}return out;\n\
             }\n\
             \n\
             template <typename T>\n\
             inline bool ProtoParseFromString(const std::string& s, T* msg) {\n\
             \u{0020}\u{0020}return ::google::protobuf::TextFormat::ParseFromString(\n\
             \u{0020}\u{0020}\u{0020}\u{0020}RushProtoTextEscapeQuotedControls(s), msg);\n\
             }\n\
             \n\
             }  // namespace tensorflow\n\
             \n\
             #endif  // RUSH_STUB_PROTO_TEXT_SHARED_SHIM_H_\n",
        )?;

        let mut artifacts = Vec::new();
        for src in &self.srcs {
            // `src` is a proto filename relative to the target's package.
            // Strip the .proto extension; the output filenames use the stem.
            let stem = Path::new(src)
                .file_stem()
                .ok_or_else(|| anyhow!("Invalid proto src: {}", src))?
                .to_string_lossy()
                .to_string();

            // Scan the .proto source for enum declarations so we can emit
            // the per-enum `EnumName_<EnumType>(x)` shims TF's .cc files
            // call alongside `ProtoDebugString` / `ProtoParseFromString`.
            // The real generator walks a FileDescriptor; a regex over the
            // raw .proto text is sufficient for TF's proto style (no
            // macros, no pathological formatting).
            let src_path = context.source_path(src);
            let enum_names = scan_proto_for_enums(&src_path);

            // The `.pb.h` from the matching cc_proto_library lives at
            // `genfiles/<workspace-relative-proto-path>.pb.h` — the same
            // layout protoc wrote it to. Use `srcs_relative_dir` as the
            // workspace-relative prefix for #include.
            let pb_h_include = if self.srcs_relative_dir.is_empty() {
                format!("{}.pb.h", stem)
            } else {
                format!("{}{}.pb.h", self.srcs_relative_dir, stem)
            };

            let header_path = out_dir.join(format!("{}.pb_text.h", stem));
            let header_impl_path = out_dir.join(format!("{}.pb_text-impl.h", stem));
            let cc_path = out_dir.join(format!("{}.pb_text.cc", stem));

            let guard = format!(
                "RUSH_STUB_{}_PB_TEXT_H_",
                stem.to_ascii_uppercase().replace(['-', '.'], "_")
            );
            let mut enum_shims = String::new();
            for ename in &enum_names {
                enum_shims.push_str(&format!(
                    "inline const std::string& EnumName_{enum_type}({enum_type} value) {{\n\
                     \u{0020}\u{0020}return {enum_type}_Name(value);\n\
                     }}\n\n",
                    enum_type = ename
                ));
            }

            let header_body = format!(
                "// Auto-generated per-proto proto_text shim. Pulls in the\n\
                 // shared ProtoDebugString / ProtoShortDebugString /\n\
                 // ProtoParseFromString templates from the single shared\n\
                 // shim header, then adds a per-enum `EnumName_<T>` wrapper\n\
                 // for every enum declared in the source .proto.\n\
                 #ifndef {guard}\n\
                 #define {guard}\n\
                 \n\
                 #include <string>\n\
                 #include \"{pb_h}\"\n\
                 #include \"__rush_proto_text_shim.h\"\n\
                 \n\
                 namespace tensorflow {{\n\
                 \n\
                 {enum_shims}\
                 }}  // namespace tensorflow\n\
                 \n\
                 #endif  // {guard}\n",
                guard = guard,
                pb_h = pb_h_include,
                enum_shims = enum_shims,
            );
            std::fs::write(&header_path, &header_body)?;

            let header_impl_guard = format!(
                "RUSH_STUB_{}_PB_TEXT_IMPL_H_",
                stem.to_ascii_uppercase().replace(['-', '.'], "_")
            );
            let header_impl_body = format!(
                "// Auto-generated proto_text impl header shim (empty — all\n\
                 // implementations inline in the matching .pb_text.h).\n\
                 #ifndef {guard}\n\
                 #define {guard}\n\
                 #endif  // {guard}\n",
                guard = header_impl_guard
            );
            std::fs::write(&header_impl_path, &header_impl_body)?;

            // .cc: minimal translation unit. cc_library needs *something*
            // compilable; we emit an include of the generated header so
            // the resulting .o has a debuggable section and the header
            // inlines get instantiated at least once.
            let cc_body = format!(
                "// Auto-generated proto_text shim translation unit.\n\
                 // All shim implementations are inline templates in the\n\
                 // corresponding .pb_text.h header.\n\
                 #include \"{base_include}.pb_text.h\"\n",
                base_include = if self.srcs_relative_dir.is_empty() {
                    stem.clone()
                } else {
                    format!("{}{}", self.srcs_relative_dir, stem)
                }
            );
            std::fs::write(&cc_path, &cc_body)?;

            artifacts.push(Artifact {
                path: header_path,
                artifact_type: ArtifactType::Header,
            });
            artifacts.push(Artifact {
                path: header_impl_path,
                artifact_type: ArtifactType::Header,
            });
            artifacts.push(Artifact {
                path: cc_path,
                artifact_type: ArtifactType::Generated,
            });
        }

        Ok(artifacts)
    }

    fn providers(&self) -> rush_rules::providers::ProviderSet {
        rush_rules::providers::ProviderSet::empty()
    }

    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> rush_rules::providers::ProviderSet {
        use rush_action::ids::ArtifactKind;
        use rush_rules::providers::{
            extract_cc_infos, CcInfo, DefaultInfo, DirectCcInfo, FilesInfo, ProviderSet,
        };

        let output_dir = Path::new("genfiles").join(&self.srcs_relative_dir);
        let mut headers = Vec::with_capacity(self.srcs.len() * 2);
        let mut outputs = Vec::with_capacity(self.srcs.len() * 3);
        let mut files = Vec::with_capacity(self.srcs.len() * 3);

        for src in &self.srcs {
            let stem = Path::new(src)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(src);
            for (suffix, kind, is_header) in [
                (".pb_text.h", ArtifactKind::Header, true),
                (".pb_text-impl.h", ArtifactKind::Header, true),
                (".pb_text.cc", ArtifactKind::Generated, false),
            ] {
                let rel = output_dir.join(format!("{}{}", stem, suffix));
                let artifact = ctx.register_generated_artifact(&rel, kind, None);
                if is_header {
                    headers.push(artifact);
                }
                outputs.push(artifact);
                files.push(ctx.predicted_output_path(&rel));
            }
        }

        let _ = ctx.declare_incomplete_action(
            "ProtoText",
            Vec::new(),
            outputs.clone(),
            "tf_generate_proto_text_sources still executes through legacy BuildContext",
        );

        let include_dir = ctx.predicted_output_path(Path::new("genfiles"));
        let direct = DirectCcInfo {
            headers,
            include_dirs: vec![ctx.intern_path(&include_dir)],
            ..DirectCcInfo::default()
        };
        let cc_info =
            CcInfo::from_direct_and_deps(direct, extract_cc_infos(dep_providers).into_iter());

        ProviderSet::empty()
            .with_cc_info(cc_info)
            .with_files(FilesInfo::new(files))
            .with_default_info(DefaultInfo { files: outputs })
    }

    fn rule_type(&self) -> &str {
        "tf_generate_proto_text_sources"
    }
}
