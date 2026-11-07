// Rush Build - Proto Rules
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Protocol Buffer build rules (proto_library, cc_proto_library).

use anyhow::Result;
use rush_action::{Action, Artifact, Command};
use rush_parse::Value;
use rush_rules::rules_system::protobuf_protoc_path;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Proto library (collection of .proto files).
pub struct ProtoLibrary {
    /// Library name.
    pub name: String,
    /// Proto source files.
    pub srcs: Vec<String>,
    /// Dependencies (other proto_library targets).
    pub deps: Vec<String>,
    /// Prefix to add to import paths.
    pub import_prefix: Option<String>,
    /// Prefix to strip from import paths.
    pub strip_import_prefix: Option<String>,
}

impl ProtoLibrary {
    /// Parse from BUILD file attributes.
    pub fn from_rule(rule: &rush_parse::Rule) -> Result<Self> {
        let name = rule.name.clone();

        let srcs = extract_string_list(&rule.attributes, "srcs").unwrap_or_default();
        let deps = extract_string_list(&rule.attributes, "deps").unwrap_or_default();

        let import_prefix = match rule.attributes.get("import_prefix") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let strip_import_prefix = match rule.attributes.get("strip_import_prefix") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        Ok(ProtoLibrary {
            name,
            srcs,
            deps,
            import_prefix,
            strip_import_prefix,
        })
    }

    /// Get all proto source paths.
    pub fn get_sources(&self, workspace_root: &Path, package: &str) -> Vec<PathBuf> {
        self.srcs
            .iter()
            .map(|src| workspace_root.join(package).join(src))
            .collect()
    }
}

/// C++ proto library (generates C++ code from proto files).
pub struct CcProtoLibrary {
    /// Library name.
    pub name: String,
    /// Dependencies (proto_library targets).
    pub deps: Vec<String>,
}

impl CcProtoLibrary {
    /// Parse from BUILD file attributes.
    pub fn from_rule(rule: &rush_parse::Rule) -> Result<Self> {
        let name = rule.name.clone();
        let deps = extract_string_list(&rule.attributes, "deps").unwrap_or_default();

        Ok(CcProtoLibrary { name, deps })
    }

    /// Generate C++ code from proto files.
    ///
    /// Runs protoc to generate .pb.h and .pb.cc files from .proto sources.
    /// Returns paths to generated .pb.cc files that need to be compiled.
    pub fn generate_cpp_code(
        &self,
        proto_files: &[PathBuf],
        workspace_root: &Path,
        output_dir: &Path,
    ) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let genfiles_dir = output_dir.join("genfiles");
        std::fs::create_dir_all(&genfiles_dir)?;

        let mut generated_headers = Vec::new();
        let mut generated_sources = Vec::new();

        // Under Bzlmod, generate with the from-source protoc + protoc-gen-cpp plugin
        // that rush built from the BCR protobuf sources (in the workspace rush-out),
        // never the host's apt protoc — so protobuf C++ is produced by the compiler
        // rush built from source. Falls back to system protoc when those binaries are
        // absent or Bzlmod is off (the certified TF/463 path is unchanged).
        let bzlmod =
            rush_incremental::tracked::getenv_str("RUSH_ENABLE_BZLMOD").as_deref() == Some("1");
        let from_src_protoc = workspace_root.join("rush-out").join("protoc_minimal");
        let from_src_plugin = workspace_root.join("rush-out").join("protoc-gen-cpp");
        let use_from_src = bzlmod && from_src_protoc.exists();

        for proto_file in proto_files {
            // Run protoc to generate C++ code
            let mut cmd = if use_from_src {
                let mut c = std::process::Command::new(&from_src_protoc);
                if from_src_plugin.exists() {
                    c.arg(format!(
                        "--plugin=protoc-gen-cpp={}",
                        from_src_plugin.display()
                    ));
                }
                c
            } else {
                std::process::Command::new(protobuf_protoc_path())
            };
            let output = cmd
                // proto3 optional fields need this flag on protoc < 3.15 (e.g. Ubuntu 22.04's
                // apt protoc 3.12); newer protoc defaults it on and accepts the flag harmlessly.
                // Bazel's cc_proto rules pass it unconditionally — match that so rush builds TF's
                // proto3-optional protos (e.g. stablehlo quantization_config.proto) on any protoc.
                .arg("--experimental_allow_proto3_optional")
                .arg(format!("--cpp_out={}", genfiles_dir.display()))
                .arg(format!("--proto_path={}", workspace_root.display()))
                .arg(proto_file)
                .output()?;

            if !output.status.success() {
                anyhow::bail!(
                    "protoc failed for {}: {}",
                    proto_file.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            // Determine output file names
            // protoc preserves the directory structure relative to proto_path
            // e.g., for workspace_root/test_rush/test.proto with --proto_path=workspace_root,
            // it generates genfiles_dir/test_rush/test.pb.{cc,h}
            let proto_rel_path = proto_file
                .strip_prefix(workspace_root)
                .unwrap_or(proto_file);

            let proto_base = proto_rel_path.with_extension(""); // Remove .proto extension
            let proto_name = proto_base.to_string_lossy();

            let generated_cc = genfiles_dir.join(format!("{}.pb.cc", proto_name));
            let generated_h = genfiles_dir.join(format!("{}.pb.h", proto_name));

            if !generated_cc.exists() {
                anyhow::bail!(
                    "protoc did not generate expected file: {} (looked for {}, proto was at {})",
                    generated_cc.display(),
                    proto_name,
                    proto_file.display()
                );
            }

            generated_sources.push(generated_cc);
            generated_headers.push(generated_h);
        }

        Ok((generated_sources, generated_headers))
    }

    /// Create compile actions for generated .pb.cc files.
    pub fn create_compile_actions(
        &self,
        generated_sources: &[PathBuf],
        genfiles_dir: &Path,
        obj_dir: &Path,
    ) -> Result<Vec<Action>> {
        let mut actions = Vec::new();

        for src in generated_sources {
            let src_name = src
                .file_stem()
                .ok_or_else(|| anyhow::anyhow!("Invalid source file"))?
                .to_string_lossy();

            let obj_file = obj_dir.join(format!("{}.o", src_name));

            // Compile command: g++ -c -I<genfiles_dir> -std=c++17 <src> -o <obj>
            let args = vec![
                "-c".to_string(),
                format!("-I{}", genfiles_dir.display()),
                "-std=c++17".to_string(),
                src.display().to_string(),
                "-o".to_string(),
                obj_file.display().to_string(),
            ];

            let command = Command {
                executable: PathBuf::from("g++"),
                args,
                working_dir: None,
                execution_info: rush_action::ExecutionInfo::default(),
            };

            let inputs = vec![Artifact::Source(src.clone())];
            let env = HashMap::new();
            let action_id = Action::compute_id(&inputs, &command, &env)?;

            actions.push(Action {
                id: action_id,
                inputs,
                outputs: vec![Artifact::Source(obj_file)],
                command,
                env,
                mnemonic: "CcProtoCompile".to_string(),
            });
        }

        Ok(actions)
    }
}

// Helper functions

fn extract_string_list(attrs: &HashMap<String, Value>, key: &str) -> Result<Vec<String>> {
    match attrs.get(key) {
        Some(Value::List(list)) => Ok(list
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect()),
        Some(_) => anyhow::bail!("Attribute '{}' must be a list", key),
        None => anyhow::bail!("Missing attribute '{}'", key),
    }
}
