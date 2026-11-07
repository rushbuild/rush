// FIX #187: MLIR TableGen C++ library generation rule (gentbl_cc_library)
//
// This rule generates C++ code from MLIR TableGen (.td) definition files.
// TableGen is a domain-specific language used by LLVM/MLIR to generate
// repetitive C++ code for operations, types, dialects, passes, etc.
//
// Typical usage in TensorFlow/XLA:
//   gentbl_cc_library(
//       name = "ops_inc_gen",
//       tbl_outs = [
//           (["-gen-op-decls"], "ops.h.inc"),
//           (["-gen-op-defs"], "ops.cc.inc"),
//       ],
//       tblgen = "@llvm-project//mlir:mlir-tblgen",
//       td_file = "ops.td",
//       td_srcs = ["//other:files.td"],
//       deps = ["@llvm-project//mlir:OpBaseTdFiles"],
//   )

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rush_rules::{
    context::BuildContext, extract_string, extract_string_list, providers::ProviderSet, Artifact,
    ArtifactType, BuildRule, RuleInstance, Value,
};
use std::collections::HashMap;

pub struct GentblCcLibraryRule;

#[async_trait]
impl BuildRule for GentblCcLibraryRule {
    fn name(&self) -> &str {
        "gentbl_cc_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;

        // Parse tbl_outs: list of tuples (args_list, output_file)
        // Example: [(["-gen-op-decls"], "ops.h.inc"), (["-gen-op-defs"], "ops.cc.inc")]
        let mut tbl_outs = Vec::new();
        if let Some(Value::List(outs_list)) = attrs.get("tbl_outs") {
            for out_item in outs_list {
                match out_item {
                    Value::List(tuple_items) if tuple_items.len() == 2 => {
                        // First element: list of tblgen arguments
                        let args = match &tuple_items[0] {
                            Value::List(arg_list) => arg_list
                                .iter()
                                .filter_map(|v| v.as_string().map(|s| s.to_string()))
                                .collect::<Vec<String>>(),
                            _ => {
                                return Err(anyhow!(
                                    "tbl_outs tuple first element must be a list of strings"
                                ))
                            }
                        };

                        // Second element: output file path
                        let output_file = match &tuple_items[1] {
                            Value::String(s) => s.clone(),
                            _ => {
                                return Err(anyhow!(
                                    "tbl_outs tuple second element must be a string"
                                ))
                            }
                        };

                        tbl_outs.push((args, output_file));
                    }
                    _ => return Err(anyhow!("tbl_outs must be a list of 2-element tuples")),
                }
            }
        }

        // Other attributes
        let tblgen = extract_string(attrs, "tblgen").ok(); // Optional
        let td_file = extract_string(attrs, "td_file").ok(); // Optional
        let td_srcs = extract_string_list(attrs, "td_srcs").unwrap_or_default();
        let deps = extract_string_list(attrs, "deps").unwrap_or_default();
        // `strip_include_prefix` on gentbl_cc_library strips a subpath
        // from the generated files' include prefix so consumers can
        // `#include "<name>.inc"` instead of "<pkg-subdir>/<name>.inc".
        // Common values are `.` (expose package dir) and things like
        // `"mhlo/IR/"` (expose the gendir/mhlo/IR subdir). We honor
        // both by joining the prefix onto the gendir at provider
        // emission time.
        let strip_include_prefix = extract_string(attrs, "strip_include_prefix").ok();

        Ok(Box::new(GentblCcLibraryInstance {
            name,
            tbl_outs,
            tblgen,
            td_file,
            td_srcs,
            deps,
            strip_include_prefix,
            gendir: std::sync::Mutex::new(None),
        }))
    }

    fn documentation(&self) -> &str {
        "Generate C++ code from MLIR TableGen definitions"
    }
}

struct GentblCcLibraryInstance {
    name: String,
    tbl_outs: Vec<(Vec<String>, String)>, // (tblgen_args, output_file)
    tblgen: Option<String>,               // Reference to mlir-tblgen tool
    td_file: Option<String>,              // Main .td file
    td_srcs: Vec<String>,                 // Additional .td dependencies
    deps: Vec<String>,                    // Dependencies on other td_library targets
    /// Optional prefix to strip from the generated files' include
    /// paths when emitting CcInfo.include_dirs for downstream
    /// consumers. Values like `"."` (package root) or `"mhlo/IR/"`
    /// (subpackage root) are honored; empty / absent means the
    /// gendir itself is exposed unmodified.
    strip_include_prefix: Option<String>,
    /// Genfiles root directory, populated in `build()`. Downstream
    /// cc_library targets that `#include "<pkg>/<file>.inc"` (the
    /// shape mlir-tblgen outputs; Bazel would expose this via the
    /// gentbl rule's `strip_include_prefix`) need this on -I. A
    /// `Mutex<Option<PathBuf>>` bridges `build()` → `providers()`
    /// the same way `CcSharedLibraryInstance` does — `providers()`
    /// takes `&self` and has no access to `BuildContext`, so the
    /// build phase has to stash the path somewhere the provider
    /// phase can read it.
    gendir: std::sync::Mutex<Option<std::path::PathBuf>>,
}

#[async_trait]
impl RuleInstance for GentblCcLibraryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        // Dependencies include td_srcs, deps, and tblgen tool
        let mut all_deps = Vec::new();
        all_deps.extend(self.td_srcs.clone());
        all_deps.extend(self.deps.clone());
        if let Some(ref tblgen) = self.tblgen {
            all_deps.push(tblgen.clone());
        }
        all_deps
    }

    fn declared_outputs(&self) -> Vec<String> {
        self.tbl_outs
            .iter()
            .map(|(_, output)| output.clone())
            .collect()
    }

    /// `tblgen` is an exec-config tool: it must be built (it generates this
    /// rule's headers) but its providers/artifacts must NOT flow into a
    /// consumer's runtime link. The mlir-tblgen tool is an LLVM/MLIR binary
    /// carrying its own `main()`; without this exclusion it leaked into
    /// cc_test links (e.g. plugin_c_api_test / sparse_core_ops_utils_test ran
    /// the tool's `main` — printing an empty `module {}` — instead of gtest).
    /// td_srcs (.td files) and deps stay target-config.
    fn exec_only_dependencies(&self) -> Vec<String> {
        self.tblgen.iter().cloned().collect()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let mut artifacts = Vec::new();

        if self.tbl_outs.is_empty() {
            return Ok(artifacts);
        }

        // Locate the tablegen binary. The rule's `tblgen` attribute picks
        // the flavor: `@llvm-project//mlir:mlir-tblgen` is the default
        // (C++ dialect / op / pass codegen), `@llvm-project//mlir:mlir-pdll`
        // handles PDLL pattern sources (`tensorflow/core/transforms/*/pdll/*.pdll`),
        // and `@llvm-project//llvm:llvm-tblgen` is the lower-level LLVM
        // variant some TF targets also use. Prefer the TF-pinned LLVM
        // clone's build tree; fall back to Debian's system binaries.
        // Probe order for the tblgen binary (same as rush's other
        // llvm-project probes): workspace-local cache first, then the
        // host-global clone cache (RUSH_GLOBAL_CLONE_CACHE, default
        // /data/rush-cache/external), then Debian's system
        // libmlir-N-dev binaries.
        //
        // The clone's `mlir-tblgen` and the system `/usr/lib/llvm-19/
        // bin/mlir-tblgen` are NOT interchangeable: the system
        // version emits `::llvm::LogicalResult` in generated
        // `.cc.inc` files, while TF-pinned LLVM's headers still have
        // `LogicalResult` only in the `mlir::` namespace. A mismatch
        // at this layer fails the consuming cc_library compile with
        // `'LogicalResult' in namespace 'llvm' does not name a type`.
        // Matching the clone's tblgen to the clone's headers avoids
        // the skew.
        let cache_root_env = std::env::var("RUSH_WORKSPACE_CACHE_ROOT").ok();
        let global_clone_root = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
            .unwrap_or_else(|_| "/data/rush-cache/external".to_string());
        // Tblgen resolution has two sources: known upstream binaries
        // (mlir-tblgen / llvm-tblgen / mlir-pdll — probed in the clone
        // cache and system paths below) and custom cc_binary labels
        // (e.g. //tensorflow/compiler/mlir/lite/quantization/tools:
        // op_quant_spec_getters_gen). For the latter, rush registers
        // the `tblgen` attribute as a dep via `dependencies()`, so by
        // the time build() runs, the consumer cc_binary has been built
        // and its Executable artifact is in context.dependency_artifacts.
        // Look it up and exec it directly — no hand-written fallback.
        // See memory: feedback_no_fake_stubs.md.
        let tblgen_label = self.tblgen.as_deref().unwrap_or("");
        let is_known_tblgen = tblgen_label.contains(":mlir-pdll")
            || tblgen_label.contains(":llvm-tblgen")
            || tblgen_label.contains(":mlir-tblgen");
        let custom_tblgen_bin: Option<std::path::PathBuf> = if !is_known_tblgen
            && !tblgen_label.is_empty()
        {
            let artifacts_for_dep = context.direct_artifacts(tblgen_label);
            let exe = artifacts_for_dep.and_then(|arts| {
                arts.iter()
                    .find(|a| matches!(a.artifact_type, ArtifactType::Executable))
                    .map(|a| a.path.clone())
            });
            match exe {
                Some(p) => Some(p),
                None => {
                    // The dep either wasn't built, didn't produce an
                    // Executable artifact, or the label didn't resolve
                    // to a cc_binary in the graph. Fail hard so the
                    // real cause is visible — never fall back to a
                    // hand-written stub.
                    return Err(anyhow!(
                        "gentbl_cc_library `{}`: custom tblgen dep `{}` produced no \
                         Executable artifact (dep artifacts present: {}). Either the \
                         cc_binary target didn't get built before this rule ran, or \
                         the label doesn't resolve to a cc_binary. Fix the dep-chase; \
                         do NOT emit placeholder .inc content.",
                        self.name,
                        tblgen_label,
                        artifacts_for_dep.map(|a| a.len()).unwrap_or(0)
                    ));
                }
            }
        } else {
            None
        };
        let which_tool: &str = self
            .tblgen
            .as_deref()
            .map(|t| {
                if t.contains(":mlir-pdll") {
                    "mlir-pdll"
                } else if t.contains(":llvm-tblgen") {
                    "llvm-tblgen"
                } else {
                    "mlir-tblgen"
                }
            })
            .unwrap_or("mlir-tblgen");
        let cache_root = cache_root_env.clone();
        let clone_candidates: Vec<std::path::PathBuf> = {
            let mut v = Vec::new();
            if let Some(ref r) = cache_root_env {
                v.push(
                    std::path::PathBuf::from(r)
                        .join(format!("external/llvm-project/build/bin/{}", which_tool)),
                );
            }
            v.push(std::path::PathBuf::from(&global_clone_root)
                .join(format!("llvm-project/build/bin/{}", which_tool)));
            v
        };
        // If the tblgen attr pointed at a custom cc_binary, we already
        // resolved its Executable artifact above; use that directly
        // instead of the upstream-probe path.
        let tblgen = custom_tblgen_bin.clone().or_else(|| {
            clone_candidates
                .into_iter()
                .find(|p| p.exists())
                .or_else(|| {
                    ["/usr/lib/llvm-19/bin",
                     "/usr/lib/llvm-18/bin",
                     "/usr/lib/llvm-17/bin"]
                        .iter()
                        .map(|d| std::path::PathBuf::from(d).join(which_tool))
                        .find(|p| p.exists())
                })
        });

        let package_str = context
            .package_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| String::from(""));
        let gendir = context.output_path(&format!("genfiles/{}", package_str));
        std::fs::create_dir_all(&gendir)?;
        // Stash the gendir so providers() can expose it to downstream
        // cc_library targets via CcInfo.include_dirs. mlir-tblgen
        // emits headers with paths like `<pkg>/<name>.h.inc`, so
        // `#include "<pkg>/<name>.h.inc"` from a sibling .cc needs the
        // genfiles root on -I. Bazel's `strip_include_prefix = "."`
        // on the gentbl rule has the same net effect.
        if let Ok(mut slot) = self.gendir.lock() {
            *slot = Some(gendir.clone());
        }

        // Resolve td_file path. The attribute can be either:
        //   (a) a plain relative filename (e.g. "ops.td") — joined against
        //       the current package, or
        //   (b) a workspace-relative label pointing at a file, e.g.
        //       "//xla/mlir_hlo:mhlo/IR/hlo_ops.td" — the package is
        //       everything between `//` and `:`, and the target is the
        //       path after `:` (which itself contains slashes because
        //       Bazel lets file labels carry subdirs).
        // For (b) we resolve against workspace_root directly since those
        // labels can reach into a sibling package within the same
        // workspace. External-workspace labels (`@repo//pkg:file`) aren't
        // emitted today; add handling if they appear.
        let td_file_path = self.td_file.as_ref().map(|f| {
            if let Some(rest) = f.strip_prefix("//") {
                if let Some(colon) = rest.find(':') {
                    let pkg = &rest[..colon];
                    let target = &rest[colon + 1..];
                    return context.workspace_root.join(pkg).join(target);
                }
            }
            context.source_path(f)
        });

        // Include paths passed to tblgen: the package dir itself (for
        // sibling .td files), every registered external workspace root,
        // and the system MLIR include dir which carries upstream MLIR
        // dialect .td definitions the TF .td files `include`.
        let mut include_args: Vec<String> = Vec::new();
        if let Some(ref pkg) = context.package_path {
            include_args.push(format!(
                "-I{}",
                context.workspace_root.join(pkg).display()
            ));
            // Also add td include anchors derived from td_file.
            // Bazel's gentbl_cc_library effectively passes both the
            // td_file's directory AND its declaring package onto
            // tblgen's -I so that:
            //   include "sibling.td"       — resolved via td_file's dir
            //   include "pkg-relative/X.td" — resolved via td_file's pkg
            // Label-shaped td_file attrs (`//xla/mlir_hlo:mhlo/IR/hlo_ops.td`)
            // encode a package explicitly; plain filenames inherit the
            // current rule's package.
            if let Some(ref td_rel) = self.td_file {
                let (td_pkg_dir, td_file_dir): (std::path::PathBuf, std::path::PathBuf) =
                    if let Some(rest) = td_rel.strip_prefix("//") {
                        if let Some(colon) = rest.find(':') {
                            let ext_pkg = &rest[..colon];
                            let target = &rest[colon + 1..];
                            let pkg_dir = context.workspace_root.join(ext_pkg);
                            let file_dir = pkg_dir.join(target);
                            let file_parent = file_dir.parent()
                                .map(|p| p.to_path_buf())
                                .unwrap_or_else(|| pkg_dir.clone());
                            (pkg_dir, file_parent)
                        } else {
                            let this_pkg = context.workspace_root.join(pkg);
                            (this_pkg.clone(), this_pkg)
                        }
                    } else {
                        let this_pkg = context.workspace_root.join(pkg);
                        let joined = this_pkg.join(td_rel);
                        let file_parent = joined
                            .parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|| this_pkg.clone());
                        (this_pkg, file_parent)
                    };
                include_args.push(format!("-I{}", td_file_dir.display()));
                if td_pkg_dir != td_file_dir {
                    include_args.push(format!("-I{}", td_pkg_dir.display()));
                }
            }
        }
        include_args.push(format!("-I{}", context.workspace_root.display()));
        for extra in &context.extra_proto_paths {
            include_args.push(format!("-I{}", extra.display()));
        }
        // td_library dep include roots come BEFORE the clone/system MLIR
        // fallbacks (Codex 019ebd35): Bazel's gentbl_rule orders direct
        // roots, then td dep roots in declared order, then nothing — the
        // hardcoded roots below are rush's fallback for graphs that
        // predate native td_library, and must not shadow dep-declared
        // roots (e.g. the pinned llvm-project clone's mhlo vs
        // @local_xla//xla/mlir_hlo).
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            for (_dep_label, providers) in &context.dependency_providers {
                if let Some(cc_info) = providers.get_cc_info() {
                    for path_id in cc_info.compilation.include_dirs.iter() {
                        let p = reg.paths().path(path_id);
                        include_args.push(format!("-I{}", p.display()));
                    }
                }
            }
            drop(reg);
        }
        // MLIR include tree used by tblgen for `include "mlir/IR/..."`.
        // Prefer the TF-pinned clone's source tree (matches the
        // clone-built mlir-tblgen above); fall back to the system MLIR
        // headers.
        // MLIR .td include tree — probe both the workspace-local cache
        // and the host-global clone before falling back to the system
        // libmlir-N-dev layout. Must match the tblgen binary chosen
        // above for namespace consistency.
        let mut mlir_inc_candidates: Vec<std::path::PathBuf> = Vec::new();
        if let Some(ref root) = cache_root {
            mlir_inc_candidates.push(
                std::path::PathBuf::from(root).join("external/llvm-project/mlir/include"),
            );
        }
        mlir_inc_candidates.push(
            std::path::PathBuf::from(&global_clone_root).join("llvm-project/mlir/include"),
        );
        for clone_mlir_inc in mlir_inc_candidates {
            if clone_mlir_inc.join("mlir/IR/Types.h").exists() {
                include_args.push(format!("-I{}", clone_mlir_inc.display()));
                break;
            }
        }
        for sys_inc in ["/usr/lib/llvm-19/include",
                        "/usr/lib/llvm-18/include",
                        "/usr/lib/llvm-17/include"] {
            let p = std::path::PathBuf::from(sys_inc);
            if p.exists() {
                include_args.push(format!("-I{}", p.display()));
                break;
            }
        }

        // (dep CcInfo include roots — td_library `includes` — are added
        // ABOVE, before the MLIR clone/system fallbacks, so dep-declared
        // roots win the resolution order.)
        // Additional td-include roots derived from the workspace layout
        // TF BUILD files `include "mhlo/IR/hlo_ops.td"` (from
        // `third_party/xla/xla/mlir_hlo`) and `include "stablehlo/...td"`
        // (from the @stablehlo repo). Those repos don't always appear
        // in `deps` of a gentbl rule even when its td sources transitively
        // reference them. Scan the workspace tree (not a hardcoded
        // /data/tensorflow) for the usual homes and add any that exist.
        let workspace = context.workspace_root.as_path();
        let mut td_roots_candidates: Vec<std::path::PathBuf> = vec![
            workspace.join("third_party/xla"),
            workspace.join("third_party/xla/xla/mlir_hlo"),
            workspace.join("third_party/xla/third_party/tsl"),
        ];
        // Any external repo rush has materialized (notably @stablehlo)
        // whose td files are consumed by `include "<repo>/..."` needs
        // its root on -I as well. The dep_providers loop above already
        // picks up CcInfo include_dirs; this is the belt-and-suspenders
        // for repos that register no CcInfo at all.
        if let Some(ref r) = cache_root {
            td_roots_candidates.push(std::path::PathBuf::from(r).join("external/stablehlo"));
        }
        td_roots_candidates
            .push(std::path::PathBuf::from(&global_clone_root).join("stablehlo"));
        for root in td_roots_candidates {
            if root.exists() {
                include_args.push(format!("-I{}", root.display()));
            }
        }

        for (args, output_file) in &self.tbl_outs {
            let output_path = gendir.join(output_file);
            if let Some(parent) = output_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let artifact_type = if output_file.ends_with(".h")
                || output_file.ends_with(".h.inc")
                || output_file.ends_with(".hpp.inc")
            {
                ArtifactType::Header
            } else {
                ArtifactType::Generated
            };

            // Run the tblgen binary for real. No stub fallback — if the
            // tool isn't on disk or exits non-zero, fail the build with
            // the actual stderr so the cause is visible at its true
            // site instead of being buried under a downstream "not
            // declared" error three `.cc` files later. See
            // feedback_no_fake_stubs.md: writing placeholder `.inc`
            // content to paper over tool failure is the exact cheat
            // that's forbidden.
            let tblgen_bin = tblgen
                .as_ref()
                .ok_or_else(|| anyhow!(
                    "gentbl_cc_library `{}`: tblgen binary `{}` not found on disk \
                     (probed clone cache and system llvm-N/bin paths). Install or \
                     build the tool — do not emit a placeholder .inc.",
                    self.name, which_tool
                ))?;
            let td = td_file_path.as_ref().ok_or_else(|| anyhow!(
                "gentbl_cc_library `{}`: td_file attribute missing",
                self.name
            ))?;
            if !td.exists() {
                return Err(anyhow!(
                    "gentbl_cc_library `{}`: td_file {} not on disk",
                    self.name, td.display()
                ));
            }
            let output = std::process::Command::new(tblgen_bin)
                .args(args)
                .args(&include_args)
                .arg(td)
                .arg("-o")
                .arg(&output_path)
                .output()?;
            if !output.status.success() {
                return Err(anyhow!(
                    "gentbl_cc_library `{}`: {} {:?} -> {} failed: {}",
                    self.name,
                    tblgen_bin.display(),
                    args,
                    output_file,
                    String::from_utf8_lossy(&output.stderr)
                ));
            }

            artifacts.push(Artifact {
                path: output_path,
                artifact_type,
            });
        }

        Ok(artifacts)
    }

    /// Phase 5.B.3.e + 5.C0: analyze() exposes the genfiles
    /// directory as `-I` AND declares one MlirTblgen action per
    /// `tbl_outs` entry. Each tbl_outs is a (tblgen_args,
    /// output_file) pair; mlir-tblgen runs once per pair, taking
    /// the .td file as input and producing one .h.inc / .cpp.inc
    /// output. Output paths use just the tbl_outs filename
    /// (without gendir prefix) since the gendir isn't computed
    /// until build() time — for producer validation, the
    /// lightweight identity is enough.
    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> ProviderSet {
        use rush_action::ids::ArtifactKind;
        use rush_rules::providers::{CcInfo, DirectCcInfo};
        use std::path::Path;

        // 5.C0 P1: declare MlirTblgen actions for each tbl_outs
        // entry, with output paths matching what build() actually
        // writes. build() puts files at:
        //   <output_dir>/genfiles/<package>/<out_file>
        // analyze() registers the same path via
        // predicted_output_path so the producer registry uses the
        // path consumers will look up (not just the bare
        // tbl_outs filename, which was the P1 bug).
        let mut generated_textual_headers = Vec::with_capacity(self.tbl_outs.len());
        if let Some(td_file) = &self.td_file {
            let td_aid =
                ctx.register_source_artifact(Path::new(td_file), ArtifactKind::Source);
            let pkg = ctx.package().to_string();
            for (_args, out_file) in &self.tbl_outs {
                let rel_out = format!("genfiles/{}/{}", pkg, out_file);
                let output_kind = if out_file.ends_with(".h")
                    || out_file.ends_with(".h.inc")
                    || out_file.ends_with(".hpp.inc")
                {
                    ArtifactKind::Header
                } else {
                    ArtifactKind::Generated
                };
                let out_aid = ctx.register_generated_artifact(
                    Path::new(&rel_out),
                    output_kind,
                    None,
                );
                generated_textual_headers.push(out_aid);
                let _ = ctx.declare_action(
                    "MlirTblgen",
                    vec![td_aid],
                    vec![out_aid],
                );
            }
        }

        // Compute the exposed gendir include dir DIRECTLY from the
        // analysis context, not from the build()-populated `self.gendir`
        // mutex slot. Under --scheduler-authoritative the analyze pass is
        // the provider source and build() never runs first, so the slot is
        // None — which left the generated .inc include dir off downstream
        // consumers' -I. For a repo-root package (e.g. @stablehlo) the
        // genfiles-root -I still resolves `pkg/.../X.h.inc`, masking the bug;
        // for a NESTED package (@local_xla//xla/mlir_hlo) the consumer needs
        // the package-qualified `genfiles/xla/mlir_hlo` -I, which only this
        // CcInfo supplies — without it gml_st_ops.cc fails to find
        // gml_st/IR/gml_st_dialect.h.inc. The path mirrors build()'s
        // `output_path("genfiles/<pkg>")` and the per-out registration above,
        // so analyze (PASS-A) and legacy (PASS-B) consumers read an identical
        // include dir.
        let pkg = ctx.package().to_string();
        let gendir = ctx.predicted_output_path(Path::new(&format!("genfiles/{}", pkg)));
        let include_dir =
            compute_include_dir(&gendir, self.strip_include_prefix.as_deref());
        let direct_include_dirs: Vec<_> = vec![ctx.intern_path(include_dir.as_path())];
        let direct = DirectCcInfo {
            textual_headers: generated_textual_headers,
            include_dirs: direct_include_dirs,
            ..DirectCcInfo::default()
        };
        let cc_info = CcInfo::from_direct_and_deps(direct, rush_rules::providers::extract_cc_infos(dep_providers).into_iter());
        ProviderSet::empty().with_cc_info(cc_info)
    }

    fn rule_type(&self) -> &str {
        "gentbl_cc_library"
    }
}

/// Compute the exposed include dir from gendir + strip_include_prefix.
fn compute_include_dir(gendir: &std::path::Path, strip_include_prefix: Option<&str>) -> std::path::PathBuf {
    match strip_include_prefix {
        None | Some("") | Some(".") => gendir.to_path_buf(),
        Some(prefix) => {
            let trimmed = prefix.trim_matches('/');
            if trimmed.is_empty() {
                gendir.to_path_buf()
            } else {
                gendir.join(trimmed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gentbl_cc_library() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("ops_inc_gen".to_string()));

        // Construct tbl_outs: [(["-gen-op-decls"], "ops.h.inc")]
        let tbl_outs = Value::List(vec![Value::List(vec![
            Value::List(vec![Value::String("-gen-op-decls".to_string())]),
            Value::String("ops.h.inc".to_string()),
        ])]);
        attrs.insert("tbl_outs".to_string(), tbl_outs);

        attrs.insert(
            "tblgen".to_string(),
            Value::String("@llvm-project//mlir:mlir-tblgen".to_string()),
        );
        attrs.insert("td_file".to_string(), Value::String("ops.td".to_string()));

        let rule = GentblCcLibraryRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "ops_inc_gen");
        assert_eq!(instance.declared_outputs(), vec!["ops.h.inc"]);
    }

    #[test]
    fn test_gentbl_with_multiple_outs() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("dialect_gen".to_string()));

        // Multiple outputs
        let tbl_outs = Value::List(vec![
            Value::List(vec![
                Value::List(vec![Value::String("-gen-op-decls".to_string())]),
                Value::String("ops.h.inc".to_string()),
            ]),
            Value::List(vec![
                Value::List(vec![Value::String("-gen-op-defs".to_string())]),
                Value::String("ops.cc.inc".to_string()),
            ]),
        ]);
        attrs.insert("tbl_outs".to_string(), tbl_outs);

        let rule = GentblCcLibraryRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "dialect_gen");
        assert_eq!(
            instance.declared_outputs(),
            vec!["ops.h.inc", "ops.cc.inc"]
        );
    }
}
