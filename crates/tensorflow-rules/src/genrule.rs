use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rush_rules::{
    context::BuildContext, extract_bool, extract_string, extract_string_list, providers::ProviderSet,
    Artifact, ArtifactType, BuildRule, RuleInstance, Value,
};
use std::collections::HashMap;
use tokio::process::Command;

/// Genrule - executes a shell command to generate files
pub struct GenruleRule;

#[async_trait]
impl BuildRule for GenruleRule {
    fn name(&self) -> &str {
        "genrule"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs").unwrap_or_default();
        let outs = extract_string_list(attrs, "outs")?;
        let cmd = extract_string(attrs, "cmd")?;
        let tools = extract_string_list(attrs, "tools").unwrap_or_default();
        let testonly = extract_bool(attrs, "testonly")?.unwrap_or(false);

        Ok(Box::new(GenruleInstance {
            name,
            srcs,
            outs,
            cmd,
            tools,
            testonly,
        }))
    }

    fn documentation(&self) -> &str {
        "Generate files using a shell command"
    }
}

struct GenruleInstance {
    name: String,
    srcs: Vec<String>,
    outs: Vec<String>,
    cmd: String,
    tools: Vec<String>,
    #[allow(dead_code)]
    testonly: bool,
}

// Only spellings with known repository identity can grant a NEW equivalent
// reference. Named @repo is apparent; BuildContext has no mapping authority.
fn canonical_tool_reference(raw: &str, package: &str, repository: Option<&str>) -> Option<String> {
    if raw.is_empty() || raw.chars().any(char::is_whitespace) {
        return None;
    }
    if let Some(rest) = raw.strip_prefix("@@") {
        let (owner, _) = rest.split_once("//")?;
        if owner.contains('@') {
            return None;
        }
        return Some(raw.to_owned());
    }
    if let Some(rest) = raw.strip_prefix("@//") {
        return Some(format!("@@//{rest}"));
    }
    if raw.starts_with('@') {
        return None;
    }
    let owner = repository.unwrap_or("");
    if raw.starts_with("//") {
        Some(format!("@@{owner}{raw}"))
    } else if raw.starts_with(':') {
        Some(format!("@@{owner}//{package}{raw}"))
    } else if !raw.contains(':') {
        Some(format!("@@{owner}//{package}:{raw}"))
    } else {
        None
    }
}

fn expand_equivalent_tool_references(
    command: &mut String,
    declarations: &[String],
    tool_paths: &mut HashMap<String, std::path::PathBuf>,
    context: &BuildContext,
    package: &str,
) -> Result<()> {
    use rush_rules::providers::dep_label_matches_repository_exact;
    for keyword in ["location", "execpath"] {
        let prefix = format!("$({keyword} ");
        let mut offset = 0;
        while let Some(relative) = command[offset..].find(&prefix) {
            let start = offset + relative;
            let argument = start + prefix.len();
            let Some(end) = command[argument..].find(')').map(|end| argument + end) else {
                break;
            };
            let requested = &command[argument..end];
            // Preserve the existing literal-declaration resolution path,
            // including legacy mappings for which this context has no proof.
            if declarations.iter().any(|label| label == requested) {
                offset = end + 1;
                continue;
            }
            let Some(identity) =
                canonical_tool_reference(requested, package, context.repository.as_deref())
            else {
                offset = end + 1;
                continue;
            };
            let declared: Vec<_> = declarations
                .iter()
                .filter_map(|label| {
                    let declared =
                        canonical_tool_reference(label, package, context.repository.as_deref())?;
                    dep_label_matches_repository_exact(
                        &declared,
                        &identity,
                        package,
                        context.repository.as_deref(),
                    )
                    .then_some(label)
                })
                .collect();
            if declared.is_empty() {
                offset = end + 1;
                continue;
            }
            anyhow::ensure!(
                declared.len() == 1,
                "Genrule equivalent tool reference {requested:?} has ambiguous declared identities"
            );
            // Stored dependency-map labels are canonical by the driver contract.
            // Never use direct_artifacts' legacy basename/suffix fallback to
            // authorize an equivalent spelling or a different repository.
            let matches: Vec<_> = context
                .dependency_artifacts
                .iter()
                .filter(|(label, _)| {
                    (label.starts_with("//") || label.starts_with('@'))
                        && label.contains(':')
                        && dep_label_matches_repository_exact(
                            label,
                            &identity,
                            package,
                            context.repository.as_deref(),
                        )
                })
                .collect();
            anyhow::ensure!(matches.len() == 1,
                "Genrule tool reference {requested:?} requires one exact canonical dependency identity");
            let executables: Vec<_> = matches[0]
                .1
                .direct()
                .iter()
                .filter(|artifact| artifact.artifact_type == ArtifactType::Executable)
                .collect();
            anyhow::ensure!(
                executables.len() == 1,
                "Genrule tool reference {requested:?} requires exactly one direct executable"
            );
            let path = &executables[0].path;
            // Also replace any legacy-mapped value for this declaration so the
            // actual bound executable, rather than an alias name, is an input.
            tool_paths.insert(declared[0].clone(), path.clone());
            let replacement = path
                .to_str()
                .ok_or_else(|| anyhow!("Genrule equivalent tool path must be UTF-8"))?
                .to_owned();
            command.replace_range(start..=end, &replacement);
            offset = start + replacement.len();
        }
    }
    Ok(())
}

#[async_trait]
impl RuleInstance for GenruleInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        // Genrule depends on its tools AND any label-form `srcs` (filegroups,
        // cross-package file labels, generated-file labels). Bazel treats a
        // genrule's label srcs as dependencies: the referenced target is built
        // and its files become the genrule's inputs (resolvable via
        // `$(SRCS)` / `$(location[s] <label>)`). Without registering them here
        // the filegroup is never built and its `FilesInfo` never reaches this
        // rule, so a label like `:conversion_files` is mistaken for a literal
        // source file (icu merge_conversion_data).
        let mut deps = self.tools.clone();
        for s in &self.srcs {
            if (s.starts_with(':') || s.starts_with("//") || s.starts_with('@'))
                && !deps.contains(s)
            {
                deps.push(s.clone());
            }
        }
        deps
    }

    /// The plain-file `srcs`. A genrule runs its command directly in
    /// `build()` and declares only an incomplete action whose input set
    /// is empty, so these paths reach no action-input set; without
    /// reporting them here, editing one is invisible to change detection
    /// and the genrule's output goes stale silently. Label-shaped entries
    /// are excluded: those are dependencies (see `dependencies` above)
    /// and are tracked through the producing target's own outputs.
    fn undeclared_source_inputs(&self) -> Vec<String> {
        self.srcs
            .iter()
            .filter(|s| {
                !(s.starts_with(':') || s.starts_with("//") || s.starts_with('@'))
            })
            .cloned()
            .collect()
    }

    /// A genrule's `outs` are exactly the generated files a dependent may
    /// name by label (`srcs = [":conversion_data.c"]`).
    fn declared_outputs(&self) -> Vec<String> {
        self.outs.clone()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        println!("🔧 Generating files for {}", self.name);

        // Create output directory for generated files
        let package_str = context
            .package_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| String::from(""));
        let gendir = context.output_path(&format!("genfiles/{}", package_str));
        tokio::fs::create_dir_all(&gendir).await?;

        // Collect source file paths, expanding label-form srcs (filegroups,
        // cross-package file labels) to their constituent files. A filegroup
        // label resolves via its `FilesInfo` (set by FilegroupInstance::analyze,
        // 5.C.5u) which `get_dependency_files` returns as absolute paths. Plain
        // filenames resolve under the package as before. `label_files` keeps a
        // per-label map for `$(location[s] <label>)` make-var expansion below.
        let mut src_paths = Vec::new();
        let mut label_files: HashMap<String, Vec<std::path::PathBuf>> = HashMap::new();
        for src in &self.srcs {
            if src.starts_with(':') || src.starts_with("//") || src.starts_with('@') {
                let files = context.get_dependency_files(src).unwrap_or_default();
                if files.is_empty() {
                    return Err(anyhow!(
                        "Genrule '{}' src label resolved to no files (filegroup not \
                         built or exposes no FilesInfo): {}",
                        self.name,
                        src
                    ));
                }
                src_paths.extend(files.iter().cloned());
                label_files.insert(src.clone(), files);
            } else {
                let src_path = context.source_path(src);
                if !src_path.exists() {
                    return Err(anyhow!("Source file not found: {}", src_path.display()));
                }
                src_paths.push(src_path.clone());
                // A plain file in `srcs` is also a label for make-variable
                // expansion. Preserve the declared spelling used by
                // `$(location in.txt)` and accept Bazel's same-package
                // `:in.txt` and `//pkg:in.txt` spellings without treating the
                // file as a target dependency.
                label_files.insert(src.clone(), vec![src_path.clone()]);
                label_files.insert(format!(":{}", src), vec![src_path.clone()]);
                let canonical = if package_str.is_empty() {
                    format!("//:{}", src)
                } else {
                    format!("//{}:{}", package_str, src)
                };
                label_files.insert(canonical, vec![src_path]);
            }
        }

        // Collect output file paths
        let mut out_paths = Vec::new();
        for out in &self.outs {
            let out_path = gendir.join(out);

            // Create parent directory if needed
            if let Some(parent) = out_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            out_paths.push(out_path);
        }

        // Get tool paths from dependency artifacts
        let mut tool_paths = HashMap::new();
        for tool_label in &self.tools {
            if let Some(artifacts) = context.direct_artifacts(tool_label) {
                for artifact in artifacts {
                    if matches!(artifact.artifact_type, ArtifactType::Executable) {
                        tool_paths.insert(tool_label.clone(), artifact.path.clone());
                        break;
                    }
                }
            }
        }

        // Expand variables in command
        let mut expanded_cmd = self.cmd.clone();

        // Bazel's `$$` escape shields a literal `$` from make-var expansion
        // (e.g. nvshmem_prebuilt's `for src in $(SRCS); do cp -L $$src`).
        // Swap it to a sentinel FIRST so no later $-substitution ($@, $<,
        // $(...)) can partially match inside an escaped sequence; restored
        // just before execution.
        const DOLLAR_SENTINEL: &str = "\u{1}RUSH_DOLLAR\u{1}";
        expanded_cmd = expanded_cmd.replace("$$", DOLLAR_SENTINEL);

        // $(SRCS) - space-separated list of source files
        let srcs_str = src_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        expanded_cmd = expanded_cmd.replace("$(SRCS)", &srcs_str);

        // $< - first source file (Bazel compatibility).
        if let Some(first_src) = src_paths.first() {
            expanded_cmd = expanded_cmd.replace("$<", &first_src.display().to_string());
        }

        // $(OUTS) - space-separated list of output files
        let outs_str = out_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        expanded_cmd = expanded_cmd.replace("$(OUTS)", &outs_str);

        // $@ - first output file (Bazel compatibility)
        if !out_paths.is_empty() {
            expanded_cmd = expanded_cmd.replace("$@", &out_paths[0].display().to_string());
        }

        // $(GENDIR) - generated files directory
        expanded_cmd = expanded_cmd.replace("$(GENDIR)", &gendir.display().to_string());

        // $(RULEDIR) - the output directory for THIS rule's package. In Bazel
        // this is `$(GENDIR)/<package-of-rule>`; rush's `gendir` is already
        // `rush-out/genfiles/<package>` (the outs are joined onto it), so
        // $(RULEDIR) maps to the same `gendir`. Many hand-written TF genrules
        // write outputs as `$(RULEDIR)/<sub>/<file>` (e.g. the MLIR
        // quantized_function_library header), so without this the literal
        // `$(RULEDIR)` reaches the shell as a failed command substitution.
        expanded_cmd = expanded_cmd.replace("$(RULEDIR)", &gendir.display().to_string());

        // Resolve equivalent tool references against declared, exact canonical
        // dependency identities before preserving literal legacy substitutions.
        expand_equivalent_tool_references(
            &mut expanded_cmd, &self.tools, &mut tool_paths, context, &package_str,
        )?;

        // $(location //path:target) / $(execpath //path:target) - path to a
        // tool executable. Bazel's `execpath` is the exec-root-relative path;
        // rush runs genrules from the workspace root with absolute tool paths,
        // so both expand to the same resolved executable path here.
        for (tool_label, tool_path) in &tool_paths {
            let tool_str = tool_path.display().to_string();
            for kw in ["location", "execpath"] {
                let var = format!("$({} {})", kw, tool_label);
                expanded_cmd = expanded_cmd.replace(&var, &tool_str);
            }
        }

        // $(location[s] <src-label>) / $(execpath[s] <src-label>) — the files a
        // label src (e.g. a filegroup) expands to. Plural (`locations`/
        // `execpaths`) joins all files space-separated; singular yields the
        // same (Bazel requires singular to name a single-file target). Plural
        // forms are substituted first so a singular pass can't partially match.
        // icu merge_conversion_data uses `$(locations :conversion_files)`.
        for (label, files) in &label_files {
            let joined = files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" ");
            for kw in ["locations", "execpaths", "location", "execpath"] {
                let var = format!("$({} {})", kw, label);
                expanded_cmd = expanded_cmd.replace(&var, &joined);
            }
        }

        // $(location[s]/execpath[s] :<own-output>) — a genrule may reference its
        // OWN declared outputs by label (Bazel resolves these to the output's
        // exec path). TF's `tf_gen_op_wrapper_cc` (singular) op-wrapper genrule
        // does exactly this: `$(location :tool) $(location :ops/<n>.h)
        // $(location :ops/<n>.cc) <internal> ,` — the .h/.cc are this genrule's
        // own outs. Without this they reach the shell as failed command
        // substitutions. Map each declared `out` to its resolved out_path, in
        // both `:<out>` and `//<package>:<out>` label forms. Plural keywords are
        // substituted before singular so a singular pass can't partially match.
        // Paths are absolute, matching the tool/src-label substitutions above.
        for (out, out_path) in self.outs.iter().zip(out_paths.iter()) {
            let path_str = out_path.display().to_string();
            let mut labels = vec![format!(":{}", out)];
            if !package_str.is_empty() {
                labels.push(format!("//{}:{}", package_str, out));
            }
            for label in &labels {
                for kw in ["locations", "execpaths", "location", "execpath"] {
                    let var = format!("$({} {})", kw, label);
                    expanded_cmd = expanded_cmd.replace(&var, &path_str);
                }
            }
        }

        // $(@D): Bazel's output-directory make-var. With a single out it is
        // that out's directory; with multiple outs it is the rule's package
        // output dir (nvshmem_prebuilt's `cp -L $$src $(@D)` relies on this).
        let at_d = if out_paths.len() == 1 {
            out_paths[0]
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| gendir.clone())
        } else {
            gendir.clone()
        };
        expanded_cmd = expanded_cmd.replace("$(@D)", &at_d.display().to_string());

        // Bazel parity: an unexpanded $(location...) family var is an error,
        // never something the shell should see (a tool that resolved to no
        // executable, a label outside srcs/tools/outs). Check BEFORE
        // restoring `$$` escapes so escaped text can't false-positive.
        for kw in ["location ", "locations ", "execpath ", "execpaths "] {
            let needle = format!("$({}", kw);
            if let Some(pos) = expanded_cmd.find(&needle) {
                let tail: String =
                    expanded_cmd[pos..].chars().take(80).collect();
                return Err(anyhow::anyhow!(
                    "Genrule '{}': could not expand `{}` — the label is not a \
                     resolvable src/tool/out of this rule",
                    self.name,
                    tail
                ));
            }
        }

        // LAST: restore Bazel's `$$` escapes as a literal `$` for the shell.
        expanded_cmd = expanded_cmd.replace(DOLLAR_SENTINEL, "$");

        println!("  Command: {}", expanded_cmd);

        // Execute the command
        let (success, stderr) = if context.native_compile_mode().is_owned_always_run() {
            let inputs: Vec<_> = src_paths.iter().chain(tool_paths.values())
                .map(|path| rush_action::Artifact::Source(path.clone())).collect();
            let command = rush_action::Command { executable: std::path::PathBuf::from("sh"),
                args: vec!["-c".into(),expanded_cmd.clone()],
                working_dir: Some(context.native_execution_root().unwrap_or(&context.workspace_root).to_owned()),
                execution_info: rush_action::ExecutionInfo::default() };
            let env = std::collections::HashMap::new();
            let id = rush_action::Action::compute_id(&inputs,&command,&env)?;
            let result = context.execute_action(rush_action::Action { id,command,env,inputs,
                outputs: out_paths.iter().cloned().map(rush_action::Artifact::Source).collect(),
                mnemonic: "GenRule".into() }).await?;
            (result.exit_code == 0, result.stderr)
        } else {
            let output = Command::new("sh")
                .arg("-c")
                .arg(&expanded_cmd)
                .current_dir(context.native_execution_root().unwrap_or(&context.workspace_root))
                .output()
                .await?;
            (output.status.success(),output.stderr)
        };

        if !success {
            return Err(anyhow!(
                "Genrule command failed: {}\nstderr: {}",
                expanded_cmd,
                String::from_utf8_lossy(&stderr)
            ));
        }

        // Verify all output files were created
        for out_path in &out_paths {
            if !out_path.exists() {
                return Err(anyhow!(
                    "Genrule did not create expected output file: {}",
                    out_path.display()
                ));
            }
        }

        println!("  ✅ Generated {} file(s)", out_paths.len());
        for out_path in &out_paths {
            println!("     - {}", out_path.display());
        }

        // Return artifacts for generated files
        Ok(out_paths
            .into_iter()
            .map(|path| Artifact {
                path,
                artifact_type: ArtifactType::Generated,
            })
            .collect())
    }

    /// #39(1): declare this genrule's `outs` as generated artifacts and expose
    /// them via `FilesInfo`, so a consumer (e.g. a `cc_library` whose `srcs`
    /// are these genrule outputs — the `tf_gen_op_wrappers_cc` op-wrapper
    /// libraries) can resolve them at analyze time, INDEPENDENT of filesystem
    /// existence. PASS-A (analyze) runs before the genrule executes, so the
    /// generated file isn't on disk yet; the declaration here is what gives the
    /// consumer a filesystem-independent producer edge (matching PASS-B/build).
    ///
    /// The action remains Incomplete because generic genrule tools and label
    /// sources are still resolved through the legacy BuildContext. This keeps
    /// execution on build() while allowing action-aware consumers to depend on
    /// the genrule target directly instead of retaining every coarse BUILD edge.
    ///
    /// The genfiles layout mirrors the runtime `build()` (which writes outs to
    /// `<output_dir>/genfiles/<pkg>/<out>`): each out is registered at
    /// `genfiles/<pkg>/<out>` so `ArtifactRegistry::find_artifact_by_path`
    /// resolves it.
    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        _dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        use rush_action::ids::ArtifactKind;
        use std::path::Path;
        let mut files = Vec::with_capacity(self.outs.len());
        let mut output_artifacts = Vec::with_capacity(self.outs.len());
        for out in &self.outs {
            let rel = Path::new("genfiles").join(ctx.package()).join(out);
            let artifact =
                ctx.register_generated_artifact(&rel, ArtifactKind::Generated, None);
            output_artifacts.push(artifact);
            files.push(ctx.predicted_output_path(&rel));
        }
        let _ = ctx.declare_incomplete_action(
            "GenRule",
            Vec::new(),
            output_artifacts.clone(),
            "genrule inputs and command require legacy BuildContext resolution",
        );
        ProviderSet::empty()
            .with_files(rush_rules::providers::FilesInfo::new(files))
            .with_default_info(rush_rules::providers::DefaultInfo {
                files: output_artifacts,
            })
    }

    fn providers(&self) -> ProviderSet {
        // Genrule provides generated files to dependents
        ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "genrule"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A genrule's plain-file srcs must be reported as undeclared
    /// source inputs: the rule runs its command directly in build() and
    /// its incomplete action has no inputs, so nothing else puts those
    /// paths in front of change detection. Editing one used to run ZERO
    /// actions and leave a stale output with no error.
    #[test]
    fn plain_srcs_are_reported_as_undeclared_inputs() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("gen".to_string()));
        attrs.insert(
            "srcs".to_string(),
            Value::List(vec![
                Value::String("seed.txt".to_string()),
                Value::String("sub/other.txt".to_string()),
                // Label-shaped entries are DEPENDENCIES, tracked through
                // the producing target's outputs; they must not appear.
                Value::String(":other_target".to_string()),
                Value::String("//pkg:file".to_string()),
                Value::String("@repo//pkg:file".to_string()),
            ]),
        );
        attrs.insert(
            "outs".to_string(),
            Value::List(vec![Value::String("out.h".to_string())]),
        );
        attrs.insert("cmd".to_string(), Value::String("cp $< $@".to_string()));

        let instance = GenruleRule.parse_attributes(&attrs).unwrap();
        let mut inputs = instance.undeclared_source_inputs();
        inputs.sort();
        assert_eq!(inputs, vec!["seed.txt", "sub/other.txt"]);
        // And the labels are still dependencies.
        let deps = instance.dependencies();
        assert!(deps.contains(&":other_target".to_string()));
        assert!(deps.contains(&"//pkg:file".to_string()));
    }

    #[test]
    fn test_parse_genrule() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("gen_version".to_string()));
        attrs.insert(
            "outs".to_string(),
            Value::List(vec![Value::String("version.h".to_string())]),
        );
        attrs.insert(
            "cmd".to_string(),
            Value::String("echo '#define VERSION \"1.0.0\"' > $@".to_string()),
        );

        let rule = GenruleRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "gen_version");
    }

    #[test]
    fn test_genrule_with_tools() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "name".to_string(),
            Value::String("compile_proto".to_string()),
        );
        attrs.insert(
            "srcs".to_string(),
            Value::List(vec![Value::String("schema.proto".to_string())]),
        );
        attrs.insert(
            "outs".to_string(),
            Value::List(vec![
                Value::String("schema.pb.h".to_string()),
                Value::String("schema.pb.cc".to_string()),
            ]),
        );
        attrs.insert(
            "cmd".to_string(),
            Value::String("$(location //tools:protoc) $(SRCS)".to_string()),
        );
        attrs.insert(
            "tools".to_string(),
            Value::List(vec![Value::String("//tools:protoc".to_string())]),
        );

        let rule = GenruleRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "compile_proto");
        assert_eq!(instance.dependencies().len(), 1);
    }

    #[tokio::test]
    async fn plain_src_location_expands_to_the_declared_file_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let package = workspace.join("p");
        let output = temp.path().join("out");
        tokio::fs::create_dir_all(&package).await.unwrap();
        let payload = b"rush-location-bytes\nnot-empty\n";
        tokio::fs::write(package.join("in.txt"), payload)
            .await
            .unwrap();

        let rule = GenruleInstance {
            name: "g".to_string(),
            srcs: vec!["in.txt".to_string()],
            outs: vec!["out.txt".to_string()],
            cmd: "test \"$(location in.txt)\" = \"$(execpath :in.txt)\" \
                  && test \"$(location //p:in.txt)\" = \
                          \"$(execpath in.txt)\" \
                  && cat \"$(location in.txt)\" > \"$@\""
                .to_string(),
            tools: Vec::new(),
            testonly: false,
        };
        let context = BuildContext::new(workspace, output)
            .with_package(std::path::PathBuf::from("p"));

        let artifacts = rule.build(&context).await.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(tokio::fs::read(&artifacts[0].path).await.unwrap(), payload);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tool_reference_identity_tests {
    use super::*;
    use rush_rules::context::{ActionManager, NativeCompileMode, PendingCompileDeclarations};
    use rush_rules::nested_set::NestedSet;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct ExecuteReal {
        requests: Mutex<Vec<(Vec<PathBuf>, Vec<String>)>>,
    }
    #[async_trait]
    impl ActionManager for ExecuteReal {
        async fn execute(&self, action: rush_action::Action) -> Result<rush_action::ActionResult> {
            let inputs = action
                .inputs
                .iter()
                .map(|artifact| match artifact {
                    rush_action::Artifact::Source(path) => path.clone(),
                    rush_action::Artifact::Derived { output_path, .. } => output_path.clone(),
                })
                .collect::<Vec<_>>();
            anyhow::ensure!(
                inputs.iter().all(|path| path.is_file()),
                "real fixture input missing"
            );
            self.requests
                .lock()
                .unwrap()
                .push((inputs, action.command.args.clone()));
            let start = std::time::Instant::now();
            let mut command = tokio::process::Command::new(&action.command.executable);
            command.args(&action.command.args).envs(&action.env);
            if let Some(cwd) = &action.command.working_dir {
                command.current_dir(cwd);
            }
            let output = command.output().await?;
            Ok(rush_action::ActionResult {
                action_id: action.id,
                exit_code: output.status.code().unwrap_or(-1),
                stdout: output.stdout,
                stderr: output.stderr,
                outputs: action.outputs,
                output_records: Vec::new(),
                duration: start.elapsed(),
                cached: false,
            })
        }
    }

    fn actual_tool(root: &Path, name: &str, value: i32) -> Result<PathBuf> {
        let source = root.join(format!("{name}.c"));
        let executable = root.join(name);
        std::fs::write(
            &source,
            format!("#include <stdio.h>\nint main(void){{puts(\"tool{value}\");return 0;}}\n"),
        )?;
        let output = std::process::Command::new("/usr/bin/cc")
            .args([
                source.as_os_str(),
                std::ffi::OsStr::new("-o"),
                executable.as_os_str(),
            ])
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "actual tool fixture compiler failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        anyhow::ensure!(executable.is_file(), "compiler produced no real executable");
        Ok(executable)
    }

    fn context(
        root: &Path,
        package: &str,
        repository: Option<&str>,
        entries: Vec<(&str, Vec<Artifact>)>,
        manager: Arc<ExecuteReal>,
    ) -> BuildContext {
        let mut result = BuildContext::new(root.to_owned(), root.join("output"))
            .with_package(package.into())
            .with_repository(repository.map(str::to_owned))
            .with_native_compile_discovery(
                NativeCompileMode::UncachedDiagnosticV1,
                Arc::new(PendingCompileDeclarations::default()),
            )
            .with_action_manager(manager);
        result.dependency_artifacts = entries
            .into_iter()
            .map(|(label, artifacts)| (label.to_owned(), NestedSet::new(artifacts, Vec::new())))
            .collect();
        result
    }
    fn executable(path: &Path) -> Artifact {
        Artifact {
            path: path.to_owned(),
            artifact_type: ArtifactType::Executable,
        }
    }
    fn rule(tools: &[&str], cmd: &str) -> GenruleInstance {
        GenruleInstance {
            name: "generate".into(),
            srcs: Vec::new(),
            outs: vec!["out.txt".into()],
            tools: tools.iter().map(|label| (*label).to_owned()).collect(),
            cmd: cmd.into(),
            testonly: false,
        }
    }
    fn output(ctx: &BuildContext) -> PathBuf {
        ctx.output_dir
            .join("genfiles")
            .join(ctx.package_path.as_deref().unwrap())
            .join("out.txt")
    }

    #[tokio::test]
    async fn canonical_main_and_relative_tool_refs_execute_real_declared_tool() -> Result<()> {
        let temporary = tempfile::TempDir::new()?;
        let root = temporary.path();
        let tool = actual_tool(root, "real-tool", 42)?;
        let manager = Arc::new(ExecuteReal::default());
        let ctx = context(
            root,
            "tools",
            None,
            vec![("//tools:gen", vec![executable(&tool)])],
            manager.clone(),
        );
        for requested in ["//tools:gen", "@//tools:gen", ":gen", "gen"] {
            let command = format!(
                "\"$(location {requested})\" > \"$@\"; \"$(execpath {requested})\" >> \"$@\""
            );
            rule(&["@@//tools:gen"], &command).build(&ctx).await?;
            assert_eq!(std::fs::read(output(&ctx))?, b"tool42\ntool42\n");
            let records = manager.requests.lock().unwrap();
            let (inputs, argv) = records.last().unwrap();
            assert_eq!(
                inputs,
                &[tool.clone()],
                "the strict bound executable must be the actual input"
            );
            assert!(argv.iter().any(|arg| arg.contains(tool.to_str().unwrap())));
            assert!(!argv
                .iter()
                .any(|arg| arg.contains("$(location ") || arg.contains("$(execpath ")));
        }
        // Legacy named/apparent and remapped exact-declaration strings remain
        // a compatibility path; they do not acquire new canonical equivalences.
        let remapped = context(
            root,
            "consumer",
            None,
            vec![("@implib_so//:make_stub", vec![executable(&tool)])],
            manager.clone(),
        );
        rule(
            &["//third_party/implib_so:make_stub"],
            "\"$(location //third_party/implib_so:make_stub)\" > \"$@\"",
        )
        .build(&remapped)
        .await?;
        assert_eq!(std::fs::read(output(&remapped))?, b"tool42\n");
        let named = context(
            root,
            "consumer",
            Some("owner"),
            vec![("@canonical//:gen", vec![executable(&tool)])],
            manager.clone(),
        );
        rule(
            &["@apparent//:gen"],
            "\"$(location @apparent//:gen)\" > \"$@\"",
        )
        .build(&named)
        .await?;
        assert_eq!(std::fs::read(output(&named))?, b"tool42\n");
        rule(
            &["@@//tools:gen"],
            "printf '%s' '$$(location //tools:gen)' > \"$@\"",
        )
        .build(&ctx)
        .await?;
        assert_eq!(std::fs::read(output(&ctx))?, b"$(location //tools:gen)");
        Ok(())
    }

    #[tokio::test]
    async fn canonical_external_tool_refs_bind_current_owner_without_foreign_fallback() -> Result<()>
    {
        let temporary = tempfile::TempDir::new()?;
        let root = temporary.path();
        let tool = actual_tool(root, "owner-tool", 42)?;
        let foreign = actual_tool(root, "foreign-tool", 99)?;
        let manager = Arc::new(ExecuteReal::default());
        let ctx = context(
            root,
            "pkg",
            Some("owner"),
            vec![
                ("@owner//pkg:gen", vec![executable(&tool)]),
                ("//pkg:gen", vec![executable(&foreign)]),
            ],
            manager.clone(),
        );
        for requested in ["//pkg:gen", ":gen", "gen"] {
            rule(
                &["@@owner//pkg:gen"],
                &format!("\"$(location {requested})\" > \"$@\""),
            )
            .build(&ctx)
            .await?;
            assert_eq!(std::fs::read(output(&ctx))?, b"tool42\n");
            let records = manager.requests.lock().unwrap();
            assert_eq!(records.last().unwrap().0, [tool.clone()]);
        }
        let before = manager.requests.lock().unwrap().len();
        let old = std::fs::read(output(&ctx))?;
        for requested in [
            "@//pkg:gen",
            "@@//pkg:gen",
            "@@foreign//pkg:gen",
            "@owner//pkg:gen",
            "//other:gen",
        ] {
            let error = rule(
                &["@@owner//pkg:gen"],
                &format!("\"$(location {requested})\" > \"$@\""),
            )
            .build(&ctx)
            .await
            .expect_err("foreign/unproved reference must not select tempting same-basename tool");
            assert!(error.to_string().contains("Genrule"), "{error:#}");
            assert_eq!(manager.requests.lock().unwrap().len(), before);
            assert_eq!(std::fs::read(output(&ctx))?, old);
        }
        Ok(())
    }

    #[tokio::test]
    async fn equivalent_tool_refs_refuse_missing_ambiguous_and_unlisted_before_execution(
    ) -> Result<()> {
        let temporary = tempfile::TempDir::new()?;
        let root = temporary.path();
        let tool = actual_tool(root, "real-tool", 42)?;
        let other = root.join("other-real-tool");
        std::fs::copy(&tool, &other)?;
        let manager = Arc::new(ExecuteReal::default());
        let base = context(
            root,
            "pkg",
            None,
            vec![("//pkg:gen", vec![executable(&tool)])],
            manager.clone(),
        );
        rule(&["@@//pkg:gen"], "\"$(location :gen)\" > \"$@\"")
            .build(&base)
            .await?;
        let old = std::fs::read(output(&base))?;
        let before = manager.requests.lock().unwrap().len();
        let cases = vec![
            (
                vec!["@@//pkg:gen"],
                "//pkg:gen",
                vec![("@foreign//pkg:gen", vec![executable(&tool)])],
            ),
            (
                vec!["@@//pkg:gen"],
                "//pkg:gen",
                vec![("//other:gen", vec![executable(&tool)])],
            ),
            (
                vec!["@@//pkg:gen"],
                "//pkg:gen",
                vec![("//pkg:gen", Vec::new())],
            ),
            (
                vec!["@@//pkg:gen"],
                "//pkg:gen",
                vec![("//pkg:gen", vec![executable(&tool), executable(&other)])],
            ),
            (
                vec!["@@//pkg:gen", "//pkg:gen"],
                ":gen",
                vec![("//pkg:gen", vec![executable(&tool)])],
            ),
            (
                vec!["@@//pkg:gen"],
                ":gen",
                vec![
                    ("//pkg:gen", vec![executable(&tool)]),
                    ("@@//pkg:gen", vec![executable(&other)]),
                ],
            ),
            (
                vec!["@@//pkg:gen"],
                "//pkg:unlisted",
                vec![("//pkg:gen", vec![executable(&tool)])],
            ),
            (
                vec!["@apparent//:gen"],
                "@@canonical//:gen",
                vec![("@canonical//:gen", vec![executable(&tool)])],
            ),
            (
                vec!["//third_party/implib_so:make_stub"],
                "@@//third_party/implib_so:make_stub",
                vec![("@implib_so//:make_stub", vec![executable(&tool)])],
            ),
        ];
        for (declared, requested, entries) in cases {
            let ctx = context(root, "pkg", None, entries, manager.clone());
            let error = rule(&declared, &format!("\"$(location {requested})\" > \"$@\""))
                .build(&ctx)
                .await
                .expect_err("unsupported reference must not execute");
            assert!(error.to_string().contains("Genrule"), "{error:#}");
            assert_eq!(manager.requests.lock().unwrap().len(), before);
            assert_eq!(std::fs::read(output(&ctx))?, old);
        }
        for command in [
            "$(location  //pkg:gen) > $@",
            "$(location //pkg:gen > $@",
            "$(execpath //pkg:missing) > $@",
        ] {
            assert!(rule(&["@@//pkg:gen"], command).build(&base).await.is_err());
            assert_eq!(manager.requests.lock().unwrap().len(), before);
            assert_eq!(std::fs::read(output(&base))?, old);
        }
        // A transitive executable is not the declared tool's direct output.
        let mut transitive = context(root, "pkg", None, Vec::new(), manager.clone());
        transitive.dependency_artifacts.insert(
            "//pkg:gen".into(),
            NestedSet::new(
                Vec::new(),
                vec![NestedSet::new(vec![executable(&tool)], Vec::new())],
            ),
        );
        assert!(rule(&["@@//pkg:gen"], "$(location :gen) > $@")
            .build(&transitive)
            .await
            .is_err());
        assert_eq!(manager.requests.lock().unwrap().len(), before);
        assert_eq!(std::fs::read(output(&transitive))?, old);
        Ok(())
    }
}
