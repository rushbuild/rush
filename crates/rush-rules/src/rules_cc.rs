// Rush Build - C++ Build Rules
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! C++ build rule implementations (cc_library, cc_binary, cc_test)

use crate::context::BuildContext;
use crate::providers::{CcInfo, CcSharedLibraryInfo, ProviderSet};
use crate::{
    extract_string, extract_string_list, Artifact, ArtifactType, BuildRule,
    RequiredTool, RuleInstance, Value,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use rush_action::{Action, Artifact as ActionArtifact, Command, ExecutionInfo};
use rush_incremental::{content_digest_of_regular_file, stat_meta};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt;

const CXX_ENV: &str = "RUSH_CXX";
const TF_CRITICAL_CXX_ENV: &str = "RUSH_TF_CRITICAL_CXX";
const TF_BATCH_UTIL_SOURCE_SUFFIX: &str = "/tensorflow/core/util/batch_util.cc";
const TF_GENERATED_CC_OPS_DIR: &str = "/rush-out/genfiles/tensorflow/cc/ops/";

fn selected_cxx_for_source(src_path: &Path) -> Option<PathBuf> {
    let source = src_path.to_string_lossy();
    let generated_cc_op = source.contains(TF_GENERATED_CC_OPS_DIR)
        && src_path.extension().and_then(|extension| extension.to_str()) == Some("cc");
    if source.ends_with(TF_BATCH_UTIL_SOURCE_SUFFIX) || generated_cc_op {
        if let Some(critical_cxx) =
            std::env::var_os(TF_CRITICAL_CXX_ENV).filter(|value| !value.is_empty())
        {
            return Some(PathBuf::from(critical_cxx));
        }
    }

    std::env::var_os(CXX_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Resolve a `srcs`/`hdrs` entry that may be a plain filename or a Bazel label.
///
/// In TF BUILD files, cross-package label references like
/// `//tsl/platform:mutex.h` appear in `srcs`/`hdrs` and mean "the file
/// `mutex.h` in package `tsl/platform` of the current workspace." Bare names
/// like `mutex.cc` resolve relative to the target's own package. We honor
/// both.
///
/// Does not follow external `@repo//...` labels (those are handled via
/// provider propagation from the dep on the external target, not by resolving
/// the label here).
fn resolve_src_label(context: &BuildContext, s: &str) -> PathBuf {
    if let Some(after_at) = s.strip_prefix('@') {
        // External-repo label: @repo//pkg:file. We don't resolve this locally;
        // fall back to treating it as a relative path, which will fail with a
        // clear ENOENT downstream if actually used.
        let _ = after_at;
        return context.source_path(s);
    }
    if let Some(rest) = s.strip_prefix("//") {
        // In-workspace label like `//tsl/platform:mutex.h`.
        if let Some((pkg, name)) = rest.split_once(':') {
            return context.workspace_root.join(pkg).join(name);
        }
        // `//pkg/name` shorthand (target name == last path segment).
        return context.workspace_root.join(rest);
    }
    if let Some(name) = s.strip_prefix(':') {
        // `:name` same-package label.
        return context.source_path(name);
    }
    context.source_path(s)
}

/// ROOT-3: resolve a cc_library `srcs` entry (possibly a cross-package label)
/// to its on-disk absolute path, against the CURRENT TARGET's repo root.
/// `ws_root` is `AnalysisContext::workspace_root()` / `BuildContext.workspace_root`
/// — already the EXTERNAL repo root for external targets (e.g. @local_tsl), so
/// `//xla/tsl/platform:env.cc` for a target whose BUILD lives under that repo
/// resolves correctly to `<ws_root>/xla/tsl/platform/env.cc`. `pkg` is the
/// current package (for `:name` / plain relative forms). Label forms:
///   `//pkg2:file` -> ws_root/pkg2/file
///   `//pkg2/sub`  -> ws_root/pkg2/sub        (target == last segment, rare for srcs)
///   `:name`       -> ws_root/pkg/name
///   `@repo//...`  -> ws_root/pkg/<file-component>  (NOT properly resolved —
///                    external-repo SOURCE labels are out of ROOT-3's `//...`
///                    scope; this yields a path that the caller's exists()
///                    check rejects, so it is skipped rather than miscompiled.)
///   plain rel     -> ws_root/pkg/rel  (or ws_root/rel when pkg is empty)
/// This mirrors `resolve_src_label` but takes (ws_root, pkg) explicitly so both
/// the `AnalysisContext` (analyze) and `BuildContext` (legacy) compile loops can
/// share identical resolution — keeping PASS-A/PASS-B byte-identical.
fn resolve_cc_src_path(ws_root: &std::path::Path, pkg: &str, raw: &str) -> std::path::PathBuf {
    // An already-absolute src path (e.g. SourceFileInstance now exposes the absolute
    // path of a cross-package //pkg:file.cc label in FilesInfo, commit f6c14ef) is
    // returned verbatim — never re-joined under a package, which would mangle it.
    if std::path::Path::new(raw).is_absolute() {
        return std::path::PathBuf::from(raw);
    }
    if let Some(rest) = raw.strip_prefix("//") {
        if let Some((p, name)) = rest.split_once(':') {
            return ws_root.join(p).join(name);
        }
        return ws_root.join(rest);
    }
    if let Some(name) = raw.strip_prefix(':') {
        return if pkg.is_empty() {
            ws_root.join(name)
        } else {
            ws_root.join(pkg).join(name)
        };
    }
    if raw.starts_with('@') {
        let fc = crate::cc_command_spec::source_file_component(raw);
        return if pkg.is_empty() {
            ws_root.join(fc)
        } else {
            ws_root.join(pkg).join(fc)
        };
    }
    if pkg.is_empty() {
        ws_root.join(raw)
    } else {
        ws_root.join(pkg).join(raw)
    }
}

/// Object-file name for a cc_library source identity, collision-safe for
/// cross-package label srcs. Callers choose the identity appropriate to their
/// rule; this helper makes the resulting `(pkg, stem, ext)` key encoding
/// injective, but does not canonicalize raw paths or labels itself. ROOT-3: a
/// target may list BOTH a same-package file and a
/// cross-package `//pkg:file` source that share a basename (e.g.
/// tsl/platform/default:tracing has `tracing.cc` AND
/// `//tsl/platform:tracing.cc`), which would both compile to `tracing.o` and
/// collide on a single output path — making the verifier pair the two
/// CppCompile actions inconsistently (a spurious divergence).
///
/// VERIFICATION TRACK (cc-obj-name-v1): the previous encoding
/// `format!("{}_{}.o", pkg.replace('/', "_"), file_stem)` was NOT injective —
/// the `/`→`_` rewrite maps `//a/b:x.cc` and `//a_b:x.cc` to the SAME
/// `a_b_x.o` (a distinct collision vector from the .cc/.h same-stem clobber the
/// header-skip already addressed). Codex (gpt-5.5 xhigh, thread 019ec87c) found
/// this latent bug. The naming now delegates to
/// `rush_verified_core::CcObjKey::object_name`, a PURE, deterministic,
/// length-prefixed + injectively-escaped encoding that is proven (Creusot,
/// gated) and witnessed (proptest oracle) to map distinct `(pkg, stem, ext)`
/// triples to distinct object names. Object files are internal build artifacts,
/// so the changed (but still deterministic) names are safe; analyze() and
/// legacy() still derive byte-identical names because they feed identical
/// `(raw, file_stem)` inputs through this one helper.
pub fn cc_obj_name_for_src(raw: &str, file_stem: &str) -> String {
    let (pkg, ext) = cc_obj_pkg_ext(raw);
    rush_verified_core::cc_obj_key(pkg, file_stem, ext).object_name()
}

/// Decompose a `srcs` entry into the `(package, extension)` pair that, together
/// with the resolved file stem, uniquely identifies a translation unit. The
/// stem is supplied by the caller (it comes from the filesystem-resolved path,
/// so analyze/legacy agree); here we recover the package qualifier and the
/// source extension from the raw label/filename string.
///
/// `//pkg:file.cc`  -> ("pkg", "cc")        (cross-package label: pkg qualifies)
/// `:file.cc`       -> ("",    "cc")        (same-package label)
/// `dir/file.cc`    -> ("",    "cc")        (same-package relative path)
/// `file`           -> ("",    "")          (no extension)
///
/// Same-package sources carry an empty package, exactly as the pre-verification
/// code left them unqualified; the injective encoding keeps them distinct from
/// a cross-package source of the same basename because the package length tag
/// differs.
fn cc_obj_pkg_ext(raw: &str) -> (&str, &str) {
    // An ABSOLUTE src path (a cross-package //pkg:file.cc label resolved to its
    // absolute path in FilesInfo, commit f6c14ef) does NOT start with "//", so the
    // label parse below would yield pkg "" — colliding with a same-basename local
    // src ("tracing.cc") and producing the SAME object name (two distinct sources ->
    // one object path -> ar duplicate-member -> "multiple definition", e.g.
    // tsl/platform/default:tracing's ["tracing.cc", "//tsl/platform:tracing.cc"]).
    // Disambiguate by the DIRECTORY component instead. (Codex 019eebc6.)
    if raw.starts_with('/') {
        let (dir, file) = raw.rsplit_once('/').unwrap_or(("", raw));
        let ext = file.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
        return (dir, ext);
    }
    // A package-relative subpath (`dir/file.cc`, NOT a `//pkg:file` or `:file`
    // label) carries its OWN disambiguating directory. A cc_library whose srcs
    // glob multiple subdirectories that each contain a same-basename file —
    // e.g. @triton//:TritonDialects globs lib/Dialect/{Triton,TritonGPU,
    // TritonNvidiaGPU}/IR/*.cpp, so THREE distinct `Ops.cpp` exist — would
    // otherwise all key on (pkg="", stem="Ops", ext="cpp") and collapse to one
    // object (`0.3.Ops3.cpp.o`); only the last compiled survives in the
    // archive, so the core Triton dialect's `MLIR_DEFINE_EXPLICIT_TYPE_ID`
    // statics (AddPtrOp/LoadOp/StoreOp/DotOp TypeIDs) were silently dropped and
    // libtensorflow_cc.so failed to dlopen on `mlir::detail::TypeIDResolver<
    // mlir::triton::AddPtrOp>::id`. Use the directory component as the
    // disambiguator (mirrors the absolute-path branch above, Codex 019eebc6),
    // so the three Ops.cpp produce distinct object names. Labels (`//`, `:`,
    // `@`) are handled by the package qualifier below and never reach here with
    // a bare directory.
    if !raw.starts_with("//") && !raw.starts_with('@') && !raw.starts_with(':') {
        if let Some((dir, file)) = raw.rsplit_once('/') {
            let ext = file.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
            return (dir, ext);
        }
    }
    let pkg = raw
        .strip_prefix("//")
        .and_then(|rest| rest.split_once(':'))
        .map(|(pkg, _)| pkg)
        .unwrap_or("");
    // Extension of the filename component (after the last '/' or ':').
    let file_component = raw.rsplit([':', '/']).next().unwrap_or(raw);
    let ext = file_component
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or("");
    (pkg, ext)
}

/// True for a `srcs` entry that is a Bazel label (`//pkg:f`, `:f`, `@r//p:f`)
/// rather than a package-relative filename. Used to decide whether to register
/// the resolved ABSOLUTE path (cross-package) vs the raw relative string
/// (same-package, preserving PASS-A/PASS-B parity with pre-ROOT-3 behavior).
fn is_label_src(raw: &str) -> bool {
    raw.starts_with("//") || raw.starts_with('@') || raw.starts_with(':')
}

/// A `srcs` entry that is a PREBUILT linker input (`.a` static archive or
/// `.so` shared library) rather than a translation unit to compile. Returns
/// `Some(is_static)`.
///
/// Bazel models prebuilt libraries via `cc_import`; rush's external stubs
/// (e.g. `@tf_runtime`'s real `libtfrt.a`, built by bootstrap_tf_runtime.sh)
/// instead list the archive directly in `srcs`. Routing it through
/// `CcInfo.linker_inputs` (this function's callers) lands it in the consumer
/// link's global `--start-group` alongside every other archive — so a static
/// lib's back-references to symbols defined in OTHER archives (e.g. libtfrt.a's
/// async_value.o -> pinned-absl `BlockingCounter`) resolve. Passing it as a raw
/// `linkopts` string instead emits it AFTER the closed `--end-group`, where
/// those back-references go unresolved. Compilable srcs are never archives, so
/// this is a no-op for ordinary cc_library targets.
fn prebuilt_lib_src(raw: &str) -> Option<bool> {
    if raw.starts_with('-') {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    if lower.ends_with(".a") {
        return Some(true);
    }
    if lower.ends_with(".so") || lower.contains(".so.") {
        return Some(false);
    }
    None
}

/// Whether a target's TUs should compile `-fno-rtti`.
///
/// #34 (Codex gpt-5.5 xhigh, thread 019e8623): this used to return true for any
/// target with an `@llvm-project` dep, on the theory that LLVM base-class
/// typeinfo was absent from the archives so consumers had to drop RTTI to avoid
/// referencing it (the //tensorflow/compiler/mlir/lite:converter-gen link). That
/// is WRONG on two counts: (1) bazel `copts` are per-target, NOT transitive, so
/// "has an @llvm-project dep" never implies the consumer is built `-fno-rtti` —
/// the upstream llvm-project-overlay `llvm_copts` is just
/// `$(STACK_FRAME_UNLIMITED)`, with no `-fno-rtti`; consumers (converter-gen,
/// XLA hlo) compile with the toolchain default (RTTI on). (2) rush's pinned LLVM
/// is built `-DLLVM_ENABLE_RTTI=ON` (scripts/bootstrap_llvm_project_pinned.sh),
/// so the archives DO define `llvm::cl::Option` / `llvm::detail::format_adapter`
/// typeinfo — the converter-gen link motivation is moot. Forcing `-fno-rtti`
/// instead BREAKS XLA code that legitimately needs RTTI: `xla/hlo/ir/
/// hlo_casting_utils.h` uses `typeid(T)` unconditionally and `dynamic_cast` in
/// non-NDEBUG builds (hlo_module_importer). So: never force `-fno-rtti` from a
/// deps heuristic. (The bazel-faithful long-term answer is a real per-target
/// copts/features channel; until then, no consumer gets `-fno-rtti`.) Kept as a
/// function — rather than deleting the three call sites — so the analyze pass,
/// the legacy command-spec, and compile_source stay byte-identical (verifier
/// parity); they now all uniformly omit `-fno-rtti`.
fn deps_want_llvm_no_rtti(_deps: &[String]) -> bool {
    false
}

/// Optional `--gcc-install-dir=<dir>` flag for HIP device (`.cu.cc`/`.cu`)
/// compiles, from `RUSH_GPU_GCC_INSTALL_DIR`.
///
/// Why this exists: hipcc/clang resolves libstdc++ against the host's newest
/// gcc by default. On gcc-13+ the distro-hardened `<bits/c++config.h>` makes
/// `__glibcxx_assert` unconditionally reference the `__host__`-only
/// `__glibcxx_assert_fail` (`_GLIBCXX_VERBOSE_ASSERT` is forced to 1, and even
/// the non-`_GLIBCXX_ASSERTIONS` constant-evaluated path calls a `__host__`
/// inline), so a `__host__ __device__` function like `std::polar` (pulled in by
/// the complex64/complex128 GPU cwise kernels) fails the device pass with
/// "reference to __host__ function '__glibcxx_assert_fail' in __host__
/// __device__ function". No `-D` flag escapes this — both macro branches land
/// on a host-only symbol. `-D_GLIBCXX_NO_ASSERTIONS` (kept below) only suffices
/// on older libstdc++. The faithful fix, mirroring a Bazel ROCm crosstool which
/// pins a single gcc, is to point the device compile at a device-compatible
/// gcc's libstdc++ (e.g. gcc-12, whose disabled-assert path is `__builtin_abort`
/// — device-callable). Configured via env so no host path is hardcoded; absent
/// → no flag (unchanged behavior).
fn gpu_device_gcc_install_dir_flag() -> Option<String> {
    match std::env::var("RUSH_GPU_GCC_INSTALL_DIR") {
        Ok(dir) if !dir.trim().is_empty() => Some(format!("--gcc-install-dir={}", dir.trim())),
        _ => None,
    }
}

fn cuda_enabled() -> bool {
    std::env::var("RUSH_ENABLE_CUDA").as_deref() == Ok("1")
        && std::env::var("RUSH_ENABLE_ROCM").as_deref() != Ok("1")
}

fn cuda_toolkit_path() -> String {
    std::env::var("RUSH_CUDA_PATH")
        .or_else(|_| std::env::var("CUDA_HOME"))
        .or_else(|_| std::env::var("CUDA_PATH"))
        .unwrap_or_else(|_| "/usr/local/cuda".to_string())
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn cuda_device_compiler_is_nvcc() -> bool {
    cuda_device_compiler()
        .file_name()
        .map(|name| name.to_string_lossy().starts_with("nvcc"))
        .unwrap_or(false)
}

fn normalize_cuda_compile_flags(args: &mut Vec<String>) {
    if !cuda_device_compiler_is_nvcc() {
        return;
    }

    let mut normalized = Vec::with_capacity(args.len() + 1);
    normalized.push("--forward-unknown-to-host-compiler".to_owned());
    normalized.push("--expt-relaxed-constexpr".to_owned());
    normalized.push("--expt-extended-lambda".to_owned());
    let mut input = args.drain(..).peekable();
    while let Some(arg) = input.next() {
        if arg == "-x" && input.peek().map(String::as_str) == Some("cuda") {
            input.next();
            normalized.push("-x".to_owned());
            normalized.push("cu".to_owned());
            continue;
        }
        if arg == "-x=cuda" {
            normalized.push("-x".to_owned());
            normalized.push("cu".to_owned());
            continue;
        }
        if arg == "--no-cuda-version-check"
            || arg == "--forward-unknown-to-host-compiler"
            || arg == "--expt-relaxed-constexpr"
            || arg == "--expt-extended-lambda"
        {
            continue;
        }
        if arg == "--cuda-path" {
            input.next();
            continue;
        }
        if arg.starts_with("--cuda-path=") {
            continue;
        }
        if arg == "--cuda-gpu-arch" {
            if let Some(arch) = input.next() {
                normalized.push(format!("-arch={arch}"));
            }
            continue;
        }
        if let Some(arch) = arg.strip_prefix("--cuda-gpu-arch=") {
            normalized.push(format!("-arch={arch}"));
            continue;
        }
        normalized.push(arg);
    }
    drop(input);
    *args = normalized;
}

fn cuda_device_compiler() -> PathBuf {
    if std::env::var_os("RUSH_CUDA_CXX").is_none() && executable_on_path("clang++").is_none() {
        let nvcc = PathBuf::from(cuda_toolkit_path()).join("bin/nvcc");
        if nvcc.is_file() {
            return nvcc;
        }
    }

    std::env::var("RUSH_CUDA_CXX")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("clang++"))
}

fn append_unique_flag(flags: &mut Vec<String>, flag: String) {
    if !flags.iter().any(|existing| existing == &flag) {
        flags.push(flag);
    }
}

/// Add NVIDIA host-wide selection flags and Clang CUDA flags for device TUs.
/// Returns true only when `source` is a CUDA device translation unit.
fn append_cuda_compile_flags(flags: &mut Vec<String>, source: &Path) -> bool {
    if !cuda_enabled() {
        return false;
    }
    let toolkit = cuda_toolkit_path();
    append_unique_flag(flags, "-DGOOGLE_CUDA=1".to_string());
    append_unique_flag(flags, format!("-I{toolkit}/include"));

    let source = source.to_string_lossy();
    let is_device = source.ends_with(".cu.cc") || source.ends_with(".cu");
    if is_device {
        let arch = std::env::var("RUSH_CUDA_ARCH").unwrap_or_else(|_| "sm_80".to_string());
        for flag in [
            "-DEIGEN_USE_GPU".to_string(),
            "-x".to_string(),
            "cuda".to_string(),
            format!("--cuda-path={toolkit}"),
            format!("--cuda-gpu-arch={arch}"),
            "--no-cuda-version-check".to_string(),
        ] {
            append_unique_flag(flags, flag);
        }
    }
    is_device
}

/// Expand `filegroup` (and other `FilesInfo`-providing) labels appearing
/// in a cc_library's `srcs` into their constituent files, spliced at the
/// label's position in `srcs` declaration order. A `filegroup` whose
/// `srcs` are file labels — the TF `framework_internal_impl_srcs` pattern
/// (`//tensorflow/core/util:framework_internal_impl_srcs`) — otherwise
/// contributes ZERO compile sources, because a label like
/// `:framework_internal_impl_srcs` is not a `.cc` string and the per-src
/// compile loop skips it (`mirror_pad_mode.cc` never compiled → the
/// op-codegen tool link fails with `undefined reference to
/// GetMirrorPadModeAttrString`).
///
/// `lookup(label)` returns the label's files (already ABSOLUTE — each
/// filegroup resolved its own plain srcs against ITS workspace root, so a
/// cross-repo `@local_tsl//tsl/util:...srcs` nested filegroup needs no
/// re-resolution here) or None if the label provides no files (a plain
/// cross-package `.cc` file label, or a non-files target) — in which case
/// the label is kept verbatim so the existing label-`.cc` handling in the
/// per-src loop still applies. Absolute file paths pass through
/// `resolve_cc_src_path` unchanged (an absolute path replaces the
/// package join), and `file_stem` of the object name is unaffected, so
/// PASS-A / PASS-B / runtime stay byte-identical when fed the same
/// expansion. No sort, no dedup (Bazel preserves declaration order).
pub fn expand_filegroup_srcs(
    srcs: &[String],
    lookup: impl Fn(&str) -> Option<Vec<std::path::PathBuf>>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(srcs.len());
    for s in srcs {
        if is_label_src(s) {
            if let Some(files) = lookup(s) {
                // Only splice RESOLVED (absolute) file paths — real filegroup
                // members and generated outputs store their FilesInfo as
                // absolute paths. A single cross-package SOURCE-FILE label
                // (e.g. //tsl/platform:file_system.cc) returns a FilesInfo of
                // just the bare basename ("file_system.cc"), which LOSES the
                // label's package: downstream resolve_cc_src_path would then
                // resolve it against the CONSUMER's package
                // (tsl/platform/default/file_system.cc → absent → silently
                // dropped, leaving tsl::FileSystem::* undefined at link). So
                // when the lookup yields only relative/bare names, KEEP the
                // original label — resolve_cc_src_path handles `//pkg:f.cc` by
                // its own package correctly. (Also fixes label source files
                // that previously matched a same-named file in the consumer's
                // package by accident, e.g. //tsl/platform:env.cc compiling the
                // wrong tsl/platform/default/env.cc.)
                if !files.is_empty() && files.iter().all(|f| f.is_absolute()) {
                    for f in files {
                        out.push(f.to_string_lossy().into_owned());
                    }
                    continue;
                }
            }
        }
        out.push(s.clone());
    }
    out
}

/// `deps` PLUS any label-shaped entries in `srcs`. Bazel treats a filegroup
/// or cross-package source/header label in `srcs` as a dependency (the
/// referenced target is built and its files become inputs). cc_library already
/// folds these in via its own dependencies(); cc_binary/cc_test did NOT, so a
/// filegroup label in a test's srcs was never analyzed and
/// expand_filegroup_srcs could not splice its members (the label reached
/// compute_id as a literal path → "open input //...:...: No such file").
fn deps_with_src_labels(deps: &[String], srcs: &[String]) -> Vec<String> {
    let mut out = deps.to_vec();
    for s in srcs {
        if (s.starts_with("//") || s.starts_with(':') || s.starts_with('@'))
            && !out.contains(s)
        {
            out.push(s.clone());
        }
    }
    out
}

/// Whether a `srcs` entry is a header-like owned input rather than a
/// translation unit. Keep this deliberately extension-based: other non-source
/// entries (prebuilt archives, linker scripts, data files) must not acquire an
/// implicit target edge merely because they are not compilable C/C++ sources.
fn is_cc_header_input(raw: &str) -> bool {
    let file = crate::cc_command_spec::source_file_component(raw);
    let extension = Path::new(file)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    matches!(
        extension.as_deref(),
        Some(
            "h" | "hh" | "hpp" | "hxx" | "h++" | "inc" | "inl" | "def" | "ipp"
                | "tcc" | "cuh"
        )
    )
}

/// Expand native `cc_binary` source labels, including the implicit `:<file>`
/// label for a bare header in `srcs`. The latter is how a clean-tree genrule
/// output such as `gen.h` reaches the binary: its generated-output alias
/// exposes an absolute `FilesInfo` path before the file exists on disk.
///
/// Only absolute provider paths are spliced. A source-file label's relative
/// `FilesInfo` member has lost its owning package, so retaining the original
/// spelling lets `resolve_cc_src_path` resolve that label correctly.
fn expand_cc_binary_srcs(
    srcs: &[String],
    lookup: impl Fn(&str) -> Option<Vec<PathBuf>>,
) -> Vec<String> {
    let mut out = Vec::with_capacity(srcs.len());
    for src in srcs {
        let implicit;
        let label = if is_label_src(src) {
            Some(src.as_str())
        } else if !Path::new(src).is_absolute() && is_cc_header_input(src) {
            implicit = format!(":{src}");
            Some(implicit.as_str())
        } else {
            None
        };
        if let Some(label) = label {
            if let Some(files) = lookup(label) {
                if !files.is_empty() && files.iter().all(|path| path.is_absolute()) {
                    out.extend(
                        files
                            .into_iter()
                            .map(|path| path.to_string_lossy().into_owned()),
                    );
                    continue;
                }
            }
        }
        out.push(src.clone());
    }
    out
}

fn cc_binary_owned_header_paths(
    workspace_root: &Path,
    package: &str,
    expanded_srcs: &[String],
) -> Vec<PathBuf> {
    let mut headers: Vec<PathBuf> = expanded_srcs
        .iter()
        .filter(|src| is_cc_header_input(src))
        .map(|src| resolve_cc_src_path(workspace_root, package, src))
        .collect();
    headers.sort();
    headers.dedup();
    headers
}

fn shared_repository_output_root(output_dir: &Path) -> PathBuf {
    if output_dir
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "external")
    {
        return output_dir
            .parent()
            .and_then(Path::parent)
            .unwrap_or(output_dir)
            .to_path_buf();
    }
    output_dir.to_path_buf()
}

fn generated_header_genfiles_root(output_dir: &Path, header: &Path) -> Option<PathBuf> {
    let shared_root = shared_repository_output_root(output_dir);
    let relative = header.strip_prefix(&shared_root).ok()?;
    let mut components = relative.components();
    let first = components.next()?;
    if first.as_os_str() == "genfiles" {
        return Some(shared_root.join("genfiles"));
    }
    if first.as_os_str() != "external" {
        return None;
    }
    let repository = components.next()?;
    if components.next()?.as_os_str() != "genfiles" {
        return None;
    }
    Some(
        shared_root
            .join("external")
            .join(repository.as_os_str())
            .join("genfiles"),
    )
}

fn cc_binary_owned_header_quote_dirs(
    workspace_root: &Path,
    output_dir: &Path,
    package: &str,
    headers: &[PathBuf],
) -> Vec<PathBuf> {
    let source_package_root = workspace_root.join(package);
    let consumer_genfiles_root = output_dir.join("genfiles");
    let mut package_roots = Vec::new();
    let mut repository_roots = Vec::new();
    for header in headers {
        if let Some(genfiles_root) = generated_header_genfiles_root(output_dir, header) {
            // Every generated repository gets its own quote root for normal
            // repo-relative includes (`"p/h.h"`). A same-repository header
            // also gets the consumer package root so a declared `dir/h.h`
            // resolves by that package-relative spelling, not as
            // `dir/dir/h.h` from the immediate parent. Package roots must
            // precede repository roots: otherwise an unrelated
            // `<genfiles>/dir/h.h` can shadow the owned
            // `<genfiles>/<package>/dir/h.h`.
            if genfiles_root == consumer_genfiles_root {
                let package_root = genfiles_root.join(package);
                if header.starts_with(&package_root) {
                    package_roots.push(package_root);
                }
            }
            repository_roots.push(genfiles_root);
        } else if header.starts_with(&source_package_root) {
            // Quote-only is important: `-I<package>` would let a checked-in
            // header such as time.h shadow the system's `<time.h>`.
            package_roots.push(source_package_root.clone());
        }
    }
    package_roots.sort();
    package_roots.dedup();
    repository_roots.sort();
    repository_roots.dedup();
    package_roots.extend(repository_roots);
    package_roots
}

/// Append label-shaped `data` entries to a dependency list. Bazel builds
/// `data` targets and stages their outputs into the consumer's runfiles,
/// so they ARE dependencies for build-graph purposes. Bare-filename data
/// (source files) are not deps — they're staged by path at runfiles time,
/// not built — so only `:`/`//`/`@` labels are registered here.
fn append_data_labels(out: &mut Vec<String>, data: &[String]) {
    for d in data {
        if (d.starts_with(':') || d.starts_with("//") || d.starts_with('@'))
            && !out.contains(d)
        {
            out.push(d.clone());
        }
    }
}

#[derive(Debug, Default, Clone)]
struct SinkCompileHeaderInputs {
    generated: Vec<PathBuf>,
    source: Vec<PathBuf>,
    source_by_basename: HashMap<String, Vec<PathBuf>>,
    include_roots: Vec<PathBuf>,
}

impl SinkCompileHeaderInputs {
    fn rebuild_source_index(&mut self) {
        self.source_by_basename.clear();
        for path in &self.source {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                self.source_by_basename
                    .entry(name.to_string())
                    .or_default()
                    .push(path.clone());
            }
        }
        for paths in self.source_by_basename.values_mut() {
            paths.sort();
            paths.dedup();
        }
    }
}

async fn collect_sink_compile_header_inputs(context: &BuildContext) -> SinkCompileHeaderInputs {
    let mut inputs = SinkCompileHeaderInputs::default();
    if let Some(reg_arc) = context.artifact_registry.as_ref() {
        let reg = reg_arc.lock().await;
        let cc_infos: Vec<std::sync::Arc<crate::providers::CcInfo>> = context
            .dependency_providers
            .values()
            .filter_map(|p| p.cc_info.clone())
            .collect();
        inputs.generated = crate::providers::collect_generated_header_paths(&reg, &cc_infos);
        for cc_info in &cc_infos {
            inputs.include_roots.extend(cc_info.compilation.include_dirs.iter()
                .map(|path| reg.paths().path(path).to_path_buf()));
            for h_aid in cc_info
                .compilation
                .headers
                .iter()
                .chain(cc_info.compilation.textual_headers.iter())
            {
                if reg.artifact(h_aid).is_source {
                    inputs.source.push(reg.artifact_path(h_aid).to_path_buf());
                }
            }
        }
    }
    inputs.generated.sort();
    inputs.generated.dedup();
    inputs.source.sort();
    inputs.source.dedup();
    inputs.include_roots.sort();
    inputs.include_roots.dedup();
    inputs.rebuild_source_index();
    inputs
}

#[derive(Default)]
struct CachedSinkHeaderInputs {
    registry: Option<std::sync::Arc<tokio::sync::Mutex<rush_action::ids::ArtifactRegistry>>>,
    compilations: Vec<std::sync::Arc<crate::providers::CcCompilationContext>>,
    inputs: std::sync::Arc<SinkCompileHeaderInputs>,
}

async fn cached_sink_compile_header_inputs(
    context: &BuildContext,
    cache: &tokio::sync::Mutex<Option<CachedSinkHeaderInputs>>,
) -> std::sync::Arc<SinkCompileHeaderInputs> {
    use std::sync::Arc;
    // Compilation providers and their nested sets are immutable. Retain their
    // Arcs with the cache key, so allocator address reuse cannot imitate a hit.
    let mut compilations: Vec<_> = context
        .dependency_providers
        .values()
        .filter_map(|p| p.cc_info.as_ref().map(|cc| Arc::clone(&cc.compilation)))
        .collect();
    compilations.sort_unstable_by_key(|cc| Arc::as_ptr(cc) as usize);
    compilations.dedup_by(|a, b| Arc::ptr_eq(a, b));
    let mut slot = cache.lock().await;
    if let Some(saved) = slot.as_ref() {
        let same_registry = match (&saved.registry, &context.artifact_registry) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        if same_registry
            && saved.compilations.len() == compilations.len()
            && saved
                .compilations
                .iter()
                .zip(&compilations)
                .all(|(a, b)| Arc::ptr_eq(a, b))
        {
            return Arc::clone(&saved.inputs);
        }
    }
    let inputs = Arc::new(collect_sink_compile_header_inputs(context).await);
    *slot = Some(CachedSinkHeaderInputs {
        registry: context.artifact_registry.clone(),
        compilations,
        inputs: Arc::clone(&inputs),
    });
    inputs
}

#[cfg(test)]
mod sink_header_cache_tests {
    use super::*;
    use crate::providers::DirectCcInfo;
    use rush_action::ids::{ArtifactKey, ArtifactKind, ArtifactRegistry, ConfigId, TargetId};
    use std::sync::Arc;

    fn header(registry: &mut ArtifactRegistry, path: &Path) -> rush_action::ids::ArtifactId {
        let exec_path = registry.paths_mut().intern(path);
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

    #[tokio::test]
    async fn retain_layout_but_reread_header_content_and_replace_changed_providers() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let first = root.join("first.h");
        let second = root.join("second.h");
        let source = root.join("main.cc");
        std::fs::write(&source, "#include \"first.h\"\n").unwrap();
        std::fs::write(&first, "// first\n").unwrap();
        std::fs::write(&second, "// second\n").unwrap();
        let mut registry = ArtifactRegistry::new();
        let a = header(&mut registry, &first);
        let b = header(&mut registry, &second);
        let cc = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                headers: vec![a, b],
                ..Default::default()
            },
            std::iter::empty(),
        );
        let mut context = BuildContext::new(root.into(), root.join("out"));
        context.artifact_registry = Some(Arc::new(tokio::sync::Mutex::new(registry)));
        context
            .dependency_providers
            .insert("//:lib".into(), ProviderSet::empty().with_cc_info(cc));
        let cache = tokio::sync::Mutex::new(None);
        let inputs = cached_sink_compile_header_inputs(&context, &cache).await;
        assert_eq!(
            reachable_source_headers_for_sink_source(&source, &inputs),
            vec![first.clone()]
        );
        std::fs::write(&first, "#include \"second.h\"\n").unwrap();
        let reused = cached_sink_compile_header_inputs(&context, &cache).await;
        assert!(Arc::ptr_eq(&inputs, &reused));
        let reachable = reachable_source_headers_for_sink_source(&source, &reused);
        assert!(reachable.contains(&first) && reachable.contains(&second));
        let replacement = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                headers: vec![b],
                ..Default::default()
            },
            std::iter::empty(),
        );
        context.dependency_providers.insert(
            "//:lib".into(),
            ProviderSet::empty().with_cc_info(replacement),
        );
        let changed = cached_sink_compile_header_inputs(&context, &cache).await;
        assert!(!Arc::ptr_eq(&inputs, &changed));
        assert_eq!(changed.source, vec![second]);

        let mut other_registry = ArtifactRegistry::new();
        header(&mut other_registry, &root.join("unused.h"));
        let remapped = root.join("remapped.h");
        header(&mut other_registry, &remapped);
        context.artifact_registry = Some(Arc::new(tokio::sync::Mutex::new(other_registry)));
        let remapped_inputs = cached_sink_compile_header_inputs(&context, &cache).await;
        assert!(!Arc::ptr_eq(&changed, &remapped_inputs));
        assert_eq!(remapped_inputs.source, vec![remapped]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cached_include_layout_rechecks_virtual_farm_retargets() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = root.join("main.cc");
        let first = root.join("headers/first.h");
        let second = root.join("headers/second.h");
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::write(&source, "#include \"api/first.h\"\n").unwrap();
        // Equal bytes preserve content-based early cutoff after a retarget.
        std::fs::write(&first, "#define VALUE 1\n").unwrap();
        std::fs::write(&second, "#define VALUE 1\n").unwrap();
        let virtual_root = stage_virtual_includes(root, &root.join("out"), "", "api",
            &["headers/first.h".to_string()], "api", Some("headers")).unwrap();
        let virtual_header = virtual_root.join("api/first.h");
        let mut registry = ArtifactRegistry::new();
        let first_id = header(&mut registry, &first);
        let second_id = header(&mut registry, &second);
        let include_root = registry.intern_path(&virtual_root);
        let cc = CcInfo::from_direct_and_deps(DirectCcInfo {
            headers: vec![first_id, second_id], include_dirs: vec![include_root],
            ..Default::default()
        }, std::iter::empty());
        let mut context = BuildContext::new(root.to_path_buf(), root.join("out"));
        context.artifact_registry = Some(Arc::new(tokio::sync::Mutex::new(registry)));
        context.dependency_providers.insert("//:headers".into(), ProviderSet::empty().with_cc_info(cc));
        let cache = tokio::sync::Mutex::new(None);
        let original = cached_sink_compile_header_inputs(&context, &cache).await;
        assert_eq!(original.include_roots, vec![virtual_root]);
        assert_eq!(reachable_source_headers_for_sink_source(&source, &original), vec![first]);
        let original_id = super::sink_header_input_tests::closure_action_id(&source, &original);
        std::fs::remove_file(&virtual_header).unwrap();
        std::os::unix::fs::symlink(&second, &virtual_header).unwrap();
        let retargeted = cached_sink_compile_header_inputs(&context, &cache).await;
        assert!(Arc::ptr_eq(&original, &retargeted), "only immutable provider layout is retained");
        assert_eq!(reachable_source_headers_for_sink_source(&source, &retargeted), vec![second.clone()]);
        let retargeted_id = super::sink_header_input_tests::closure_action_id(&source, &retargeted);
        assert_eq!(retargeted_id, original_id, "equal header values preserve early cutoff");
        std::fs::write(&second, "#define VALUE 2\n").unwrap();
        assert_ne!(super::sink_header_input_tests::closure_action_id(&source, &retargeted), retargeted_id);
    }

}

fn include_tokens_from_file(path: &Path) -> Option<Vec<String>> {
    let body = std::fs::read_to_string(path).ok()?;
    let mut tokens = Vec::new();
    for line in body.lines() {
        let Some(after_hash) = line.trim_start().strip_prefix('#') else {
            continue;
        };
        let directive = after_hash.trim_start();
        let after_directive = if let Some(rest) = directive.strip_prefix("include_next") {
            rest
        } else if let Some(rest) = directive.strip_prefix("include") {
            rest
        } else {
            continue;
        };
        let operand = match after_directive.chars().next() {
            Some(c) if c.is_whitespace() => after_directive.trim_start(),
            Some('"') | Some('<') => after_directive,
            Some(_) | None => continue,
        };
        if let Some(rest) = operand.strip_prefix('"') {
            let end = rest.find('"')?;
            tokens.push(rest[..end].to_string());
        } else if let Some(rest) = operand.strip_prefix('<') {
            let end = rest.find('>')?;
            tokens.push(rest[..end].to_string());
        } else if !operand.is_empty() {
            return None;
        }
    }
    Some(tokens)
}

fn matching_source_headers_for_include_token(
    token: &str,
    header_inputs: &SinkCompileHeaderInputs,
    source_identities: &mut Option<HashMap<PathBuf, Vec<PathBuf>>>,
) -> Vec<PathBuf> {
    let token_path = Path::new(token);
    let mut matching = if !token.contains('/') {
        header_inputs.source_by_basename.get(token).cloned().unwrap_or_default()
    } else {
        header_inputs.source.iter().filter(|header| header.ends_with(token_path))
            .cloned().collect()
    };
    if !header_inputs.include_roots.is_empty() {
        // Only the include-root layout is cached. Both the staged symlinks and
        // declared source-header identities are resolved anew for this walk;
        // a farm retarget must not reuse a previous physical-header decision.
        for root in &header_inputs.include_roots {
            let Ok(resolved) = std::fs::canonicalize(root.join(token_path)) else {
                continue;
            };
            let identities = source_identities.get_or_insert_with(|| {
                let mut identities: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
                for header in &header_inputs.source {
                    if let Ok(identity) = std::fs::canonicalize(header) {
                        identities.entry(identity).or_default().push(header.clone());
                    }
                }
                identities
            });
            if let Some(headers) = identities.get(&resolved) {
                matching.extend(headers.iter().cloned());
            }
        }
    }
    matching.sort();
    matching.dedup();
    matching
}

fn reachable_source_headers_for_sink_source(
    src_path: &Path,
    header_inputs: &SinkCompileHeaderInputs,
) -> Vec<PathBuf> {
    if header_inputs.source.is_empty() {
        return Vec::new();
    }

    let mut selected = Vec::new();
    let mut selected_set = HashSet::new();
    let mut visited_files = HashSet::new();
    let mut queue = vec![src_path.to_path_buf()];
    let mut source_identities = None;

    while let Some(path) = queue.pop() {
        if !visited_files.insert(path.clone()) {
            continue;
        }
        let Some(tokens) = include_tokens_from_file(&path) else {
            return header_inputs.source.clone();
        };
        for token in &tokens {
            for header in matching_source_headers_for_include_token(token, header_inputs, &mut source_identities) {
                if selected_set.contains(&header) {
                    continue;
                }
                selected_set.insert(header.clone());
                selected.push(header.clone());
                queue.push(header);
            }
        }
    }

    selected.sort();
    selected.dedup();
    selected
}

#[cfg(test)]
mod sink_header_input_tests {
    use super::*;

    fn source_inputs(source: Vec<PathBuf>) -> SinkCompileHeaderInputs {
        let mut inputs = SinkCompileHeaderInputs {
            generated: Vec::new(),
            source,
            source_by_basename: HashMap::new(),
            include_roots: Vec::new(),
        };
        inputs.source.sort();
        inputs.source.dedup();
        inputs.rebuild_source_index();
        inputs
    }

    #[test]
    fn sink_source_omits_unreachable_dep_source_header() {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("main.cc");
        let hdr = td.path().join("pkg").join("private.h");
        std::fs::create_dir_all(hdr.parent().unwrap()).unwrap();
        std::fs::write(&src, "int main() { return 0; }\n").unwrap();
        std::fs::write(&hdr, "#pragma once\n").unwrap();

        let selected = reachable_source_headers_for_sink_source(&src, &source_inputs(vec![hdr]));
        assert!(selected.is_empty());
    }

    #[test]
    fn sink_source_keeps_direct_and_transitive_dep_source_headers() {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("main.cc");
        let hdr_dir = td.path().join("pkg");
        let a = hdr_dir.join("a.h");
        let b = hdr_dir.join("b.h");
        let c = hdr_dir.join("c.h");
        std::fs::create_dir_all(&hdr_dir).unwrap();
        std::fs::write(&src, "#include \"pkg/a.h\"\nint main() { return A; }\n").unwrap();
        std::fs::write(&a, "#pragma once\n#include \"b.h\"\n#define A B\n").unwrap();
        std::fs::write(&b, "#pragma once\n#define B 1\n").unwrap();
        std::fs::write(&c, "#pragma once\n#define C 2\n").unwrap();

        let selected = reachable_source_headers_for_sink_source(
            &src,
            &source_inputs(vec![a.clone(), b.clone(), c]),
        );
        assert_eq!(selected, vec![a, b]);
    }

    #[test]
    fn sink_source_keeps_all_dep_source_headers_on_macro_include() {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("main.cc");
        let a = td.path().join("a.h");
        let b = td.path().join("b.h");
        std::fs::write(&src, "#define HDR \"a.h\"\n#include HDR\n").unwrap();
        std::fs::write(&a, "#pragma once\n").unwrap();
        std::fs::write(&b, "#pragma once\n").unwrap();

        let selected = reachable_source_headers_for_sink_source(
            &src,
            &source_inputs(vec![a.clone(), b.clone()]),
        );
        assert_eq!(selected, vec![a, b]);
    }

    pub(super) fn closure_action_id(source: &Path, headers: &SinkCompileHeaderInputs) -> rush_action::ActionId {
        let mut inputs = vec![ActionArtifact::Source(source.to_path_buf())];
        inputs.extend(reachable_source_headers_for_sink_source(source, headers)
            .into_iter().map(ActionArtifact::Source));
        Action::compute_id(&inputs, &Command {
            executable: PathBuf::from("c++"),
            args: vec!["-c".to_string(), source.display().to_string()],
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        }, &HashMap::new()).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn remapped_source_headers_key_content_without_keying_unreachable_private_headers() {
        for (header_name, strip_prefix, include_token) in [
            ("include/mapped.h", None, "api/include/mapped.h"),
            ("private/mapped.h", Some("private"), "api/mapped.h"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("source");
            let output = temp.path().join("out");
            let source = temp.path().join("main.cc");
            let mapped = root.join(header_name);
            let next = root.join("next.h");
            let private = root.join("unrelated.h");
            std::fs::create_dir_all(mapped.parent().unwrap()).unwrap();
            std::fs::write(&source, format!("#include \"{include_token}\"\n")).unwrap();
            std::fs::write(&mapped, "#include \"next.h\"\n#define VALUE 40\n").unwrap();
            std::fs::write(&next, "#define NEXT 2\n").unwrap();
            std::fs::write(&private, "#define PRIVATE 1\n").unwrap();
            let virtual_root = stage_virtual_includes(&root, &output, "", "mapped",
                &[header_name.to_string()], "api", strip_prefix).unwrap();
            let virtual_header = virtual_root.join(include_token);
            assert!(std::fs::symlink_metadata(&virtual_header).unwrap().file_type().is_symlink());
            assert_eq!(std::fs::canonicalize(&virtual_header).unwrap(), std::fs::canonicalize(&mapped).unwrap());
            let mut inputs = source_inputs(vec![mapped.clone(), next.clone(), private.clone()]);
            inputs.include_roots.push(virtual_root);
            let reachable = reachable_source_headers_for_sink_source(&source, &inputs);
            assert!(reachable.contains(&mapped) && reachable.contains(&next));
            assert!(!reachable.contains(&private));
            let original = closure_action_id(&source, &inputs);
            assert_eq!(closure_action_id(&source, &inputs), original);
            std::fs::write(&private, "#define PRIVATE 999\n").unwrap();
            assert_eq!(closure_action_id(&source, &inputs), original);
            std::fs::write(&mapped, "#include \"next.h\"\n#define VALUE 41\n").unwrap();
            let edited = closure_action_id(&source, &inputs);
            assert_ne!(edited, original);
            std::fs::remove_file(&mapped).unwrap();
            std::fs::write(&mapped, "#include \"next.h\"\n#define VALUE 42\n").unwrap();
            assert_ne!(closure_action_id(&source, &inputs), edited);
        }
    }

}

/// Stage Bazel's `include_prefix` virtual-include farm for a cc_library and
/// return the virtual-include root (absolute) to put on the `-I` search path.
///
/// `include_prefix` makes each header in `hdrs` includable at
/// `<include_prefix>/<rel>`, where `<rel>` is the header's package-relative
/// path AFTER `strip_include_prefix` is removed first (they compose: strip
/// then prefix). Bazel materializes this in a per-target
/// `_virtual_includes/<package>/<target>/` tree and adds the root to the
/// quote-include path. rush mirrors that exactly: it symlinks each real
/// header into the farm at its remapped path and hands back the root so the
/// caller can push it into the include search path (which feeds both this
/// library's own compile and the CcInfo seen by dependents).
///
/// Returns `None` when nothing was staged (no on-disk headers found),
/// so the caller doesn't add a bogus `-I`.
fn stage_virtual_includes(
    repo_root: &std::path::Path,
    output_dir: &std::path::Path,
    package: &str,
    target: &str,
    hdrs: &[String],
    include_prefix: &str,
    strip_include_prefix: Option<&str>,
) -> Option<std::path::PathBuf> {
    use std::path::{Path, PathBuf};
    if include_prefix.is_empty() {
        return None;
    }
    // Normalize strip to a package-relative form (trim a leading "/" so the
    // repo-root and package-relative spellings strip identically).
    let strip = strip_include_prefix.map(|s| s.trim_start_matches('/').to_string());
    // Per-target farm root, mirroring Bazel's `_virtual_includes/<target>/`.
    let vroot = output_dir
        .join("_virtual_includes")
        .join(package)
        .join(target);
    let pkg_dir = if package.is_empty() {
        repo_root.to_path_buf()
    } else {
        repo_root.join(package)
    };
    let mut staged_any = false;
    for hdr in hdrs {
        // Skip label-shaped hdrs entries (generated/cross-package headers);
        // their producers stage them, and we only remap plain source headers.
        if hdr.starts_with(':') || hdr.starts_with("//") || hdr.starts_with('@') {
            continue;
        }
        let raw_path = Path::new(hdr);
        let (real_path, logical_rel) = if raw_path.is_absolute() {
            let logical = raw_path
                .strip_prefix(repo_root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| {
                    raw_path
                        .file_name()
                        .map(PathBuf::from)
                        .unwrap_or_default()
                });
            (raw_path.to_path_buf(), logical)
        } else {
            (pkg_dir.join(raw_path), raw_path.to_path_buf())
        };
        let stripped: PathBuf = match &strip {
            Some(p) if !p.is_empty() && p != "." => logical_rel
                .strip_prefix(p)
                .map(|r| r.to_path_buf())
                .unwrap_or_else(|_| logical_rel.clone()),
            _ => logical_rel,
        };
        let virtual_rel = Path::new(include_prefix).join(&stripped);
        let link_path = vroot.join(&virtual_rel);
        if !real_path.exists() {
            // Header may be generated elsewhere or live in a header-only
            // filegroup; skip rather than fail — a genuine missing include
            // surfaces precisely at compile time.
            continue;
        }
        if let Some(parent) = link_path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                continue;
            }
        }
        // Re-point idempotently so a moved source is picked up on rerun.
        if link_path.symlink_metadata().is_ok() {
            let _ = std::fs::remove_file(&link_path);
        }
        #[cfg(unix)]
        let res = std::os::unix::fs::symlink(&real_path, &link_path);
        #[cfg(not(unix))]
        let res = std::fs::copy(&real_path, &link_path).map(|_| ());
        if res.is_ok() {
            staged_any = true;
        }
    }
    if staged_any {
        Some(vroot)
    } else {
        None
    }
}

/// cc_library rule
pub struct CcLibraryRule;

#[async_trait]
impl BuildRule for CcLibraryRule {
    fn name(&self) -> &str {
        "cc_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs")?;
        let mut hdrs = extract_string_list(attrs, "hdrs")?;
        for header in extract_string_list(attrs, "textual_hdrs")? {
            if !hdrs.contains(&header) {
                hdrs.push(header);
            }
        }
        let deps = extract_string_list(attrs, "deps")?;
        let copts = extract_string_list(attrs, "copts")?;
        let linkopts = extract_string_list(attrs, "linkopts")?;
        let mut includes = extract_string_list(attrs, "includes")?;
        let mut defines = extract_string_list(attrs, "defines")?;
        // `local_defines` apply to THIS target's own sources (Bazel does not propagate
        // them, unlike `defines`). rush appends them to `defines` so they reach the own
        // compile; the minor over-propagation to consumers is benign for TF/XLA's value
        // feature-flags — e.g. xla/stream_executor/cuda:ptx_compiler_support sets
        // LIBNVPTXCOMPILER_SUPPORT=false via local_defines, undefined (compile error)
        // without this.
        defines.extend(extract_string_list(attrs, "local_defines")?);
        // `data` — runtime files/targets staged into a consumer's runfiles.
        // Bazel republishes a cc_library's `data` through its
        // DefaultInfo.default_runfiles so it flows to any dependent binary/
        // test (see RunfilesInfo). Captured here; resolved to runfile
        // entries during analyze(), and the labels are added to
        // dependencies() so the referenced targets actually get built.
        let data = extract_string_list(attrs, "data")?;

        // `strip_include_prefix = "."` exposes the package dir as an
        // include root so headers in `hdrs = ["sub/foo.h"]` become
        // `#include "sub/foo.h"`. Bazel treats this as an implicit
        // `includes = ["."]` contribution — rush pipes it through the
        // same path by just appending "." to includes when the
        // attribute is set (and not redundant). The orchestrator's
        // CcInfo merge resolves "." to the package's absolute dir so
        // consumers get the right -I.
        //
        // Bazel's `include_prefix` is the inverse (prepend a prefix
        // to each header's include path); rush doesn't re-implement
        // that yet — the ducc stub works around it with a symlink
        // farm. A future pass could generate the prefix via a
        // sibling header alias dir, but it's out of scope for the
        // mlir_hlo surface that triggered this change.
        let strip_include_prefix = attrs
            .get("strip_include_prefix")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            });
        if let Some(ref prefix) = strip_include_prefix {
            if prefix == "." && !includes.iter().any(|s| s == ".") {
                includes.push(".".to_string());
            }
        }

        // `include_prefix` is the inverse of `strip_include_prefix`: each
        // header in `hdrs` is made includable at
        // `<include_prefix>/<repo-relative-path>` (after `strip_include_prefix`
        // is removed first, if both are set). Bazel materializes this in a
        // per-target `_virtual_includes/<target>/` directory under the bin
        // dir and adds that dir to the quote-include search path. rush stages
        // the same virtual-include farm in the orchestrator
        // (OutputTree::virtual_includes_dir + stage_virtual_includes) and
        // exposes the root via the consumer include search path — see
        // resolve_and_build_dep / resolve_external_dep. We capture the raw
        // attribute here so the orchestrator can read it from the rule.
        let include_prefix = attrs.get("include_prefix").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        });

        // `alwayslink = 1` in TF BUILDs (e.g. tf_gen_op_libs `<op>_op_lib`).
        // rush's Starlark→rules attr lowering stringifies scalar ints/bools, so
        // `1` arrives here as `String("1")` (not `Int`/`Bool`) — accept all three
        // forms. Truthy iff not empty / "0" / "false".
        let alwayslink = match attrs.get("alwayslink") {
            Some(Value::Bool(b)) => *b,
            Some(Value::Int(i)) => *i != 0,
            Some(Value::String(s)) => {
                let t = s.trim();
                !(t.is_empty() || t == "0" || t.eq_ignore_ascii_case("false"))
            }
            _ => false,
        };

        Ok(Box::new(CcLibraryInstance {
            name,
            srcs,
            hdrs,
            deps,
            copts,
            linkopts,
            includes,
            strip_include_prefix,
            include_prefix,
            defines,
            alwayslink,
            data,
        }))
    }

    fn documentation(&self) -> &str {
        "C++ library rule - compiles C++ sources into a static or shared library"
    }
}

/// Instance of a cc_library rule
pub struct CcLibraryInstance {
    name: String,
    srcs: Vec<String>,
    hdrs: Vec<String>,
    deps: Vec<String>,
    copts: Vec<String>,
    linkopts: Vec<String>,
    includes: Vec<String>,
    /// `strip_include_prefix` — headers under <repo>/<prefix>/… are exposed with
    /// <prefix> stripped, so the compile (and dependents) need -I<repo>/<prefix>.
    /// "/foo" is repo-root-relative, "." is the package dir, else package-relative.
    /// protobuf uses "/src" (google/protobuf/*.h resolve only with -I<repo>/src).
    strip_include_prefix: Option<String>,
    /// `include_prefix` — the inverse of `strip_include_prefix`. Each header
    /// in `hdrs` is exposed to consumers at `<include_prefix>/<rel>`, where
    /// `<rel>` is the header's repo/package-relative path with
    /// `strip_include_prefix` removed first (they compose: strip then
    /// prefix). The orchestrator stages a `_virtual_includes` farm with the
    /// headers laid out at the prefixed path and adds that root to the
    /// consumer include search path (see OutputTree::virtual_includes_dir).
    /// e.g. @farmhash_gpu_archive sets `include_prefix="third_party/farmhash_gpu"`
    /// so its `src/farmhash_gpu.h` resolves as
    /// `#include "third_party/farmhash_gpu/src/farmhash_gpu.h"`.
    include_prefix: Option<String>,
    defines: Vec<String>,
    /// `alwayslink = 1` — consumers must whole-archive this lib's .a so its
    /// static initializers survive (TF's tf_gen_op_libs sets this on every
    /// `<op>_op_lib` so REGISTER_OP registrations populate the OpRegistry; the
    /// op-wrapper generator tool reads that registry — without alwayslink the
    /// REGISTER_OP objects are stripped and codegen emits EMPTY wrappers).
    alwayslink: bool,
    /// `data` — runtime deps/files. Republished into this library's
    /// runfiles (RunfilesInfo) so they reach any dependent binary/test,
    /// and added to dependencies() so the targets get built.
    data: Vec<String>,
}

#[async_trait]
impl RuleInstance for CcLibraryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        // cc_library deps, plus any label-shaped entries in srcs/hdrs.
        //
        // TF BUILD files commonly pass filegroups and cross-package header
        // labels directly in `srcs`/`hdrs`, not `deps`, e.g.
        //   `srcs = [":lib_internal_private_headers"]`. Bazel treats those
        // as dependencies (the referenced target gets built, and its files
        // end up as inputs). We need the same: registering them here pulls
        // the referenced targets into the dep graph and lets artifact /
        // CcInfo propagation reach downstream targets through the
        // orchestrator.
        //
        // Additionally, a bare-filename entry in `hdrs` (no `:`/`//`/`@`
        // prefix) is also treated as an implicit `:<name>` dep in the same
        // package. Resolve these file labels through the package catalog
        // so generated headers like `options.h` pull their declaring
        // tf_gen_options_header producer into the dependency graph. If no
        // generated-file or rule target exists, the missing-dep path uses
        // the on-disk source-file check, so plain local headers still work.
        let mut out: Vec<String> = self.deps.clone();
        // `data` labels are dependencies too: Bazel builds them and stages
        // their outputs into runfiles. Register them so they're built and
        // their providers reach analyze() for runfiles republishing.
        append_data_labels(&mut out, &self.data);
        for s in self.srcs.iter().chain(self.hdrs.iter()) {
            if s.starts_with(':') || s.starts_with("//") || s.starts_with('@') {
                if !out.contains(s) {
                    out.push(s.clone());
                }
            }
        }
        for h in self.hdrs.iter().chain(self.srcs.iter()) {
            if h.starts_with(':') || h.starts_with("//") || h.starts_with('@') {
                continue;
            }
            // Only .h/.inc/.def/.pb.h etc (non-compilable) in srcs should
            // trigger file-label lookup; compilable sources are resolved
            // on-disk by compile_source. We widen the original hdrs-only
            // scan to include srcs that end with `.inc`/`.h`/`.def` so that
            // generated files referenced by name (e.g. gentbl_cc_library
            // outputs like `"passes/foo.h.inc"`) pull in the sibling
            // generator through the package output catalog. Without
            // this, the gentbl target is parsed but never built, and the
            // consumer's `#include` fails against an empty genfiles dir.
            let looks_like_header = h.ends_with(".h")
                || h.ends_with(".inc")
                || h.ends_with(".def")
                || h.ends_with(".pb.h");
            if self.srcs.contains(h) && !self.hdrs.contains(h) && !looks_like_header {
                continue;
            }
            let implicit = format!(":{}", h);
            if !out.contains(&implicit) {
                out.push(implicit);
            }
        }
        out
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        // Patch set 2b: cc_library returns DIRECT artifacts only — its
        // own headers, objects, .a, generated outputs. Transitive
        // propagation happens at the orchestrator level via NestedSet
        // and consumers walk via `iter_transitive_artifacts`. The old
        // "for dep ... for artifact in dep_artifacts { push.clone() }"
        // pattern was the dominant O(N²) memory cost at TF-rung-11
        // scale: every cc_library allocated a fresh Vec<Artifact> the
        // size of its transitive closure, and aggregator targets near
        // tensorflow_cc tipped RSS from 4 GB to 57 GB across ~270
        // build events (see docs/memory-pressure-analysis.md).
        let mut artifacts = Vec::new();

        // If header-only library (no sources), just track headers
        if self.srcs.is_empty() {
            for hdr in &self.hdrs {
                let hdr_path = context.source_path(hdr);
                artifacts.push(Artifact {
                    path: hdr_path,
                    artifact_type: ArtifactType::Header,
                });
            }
            return Ok(artifacts);
        }

        // Create output directory for this target (package-qualified to
        // avoid the cross-package same-name collision class; see
        // BuildContext::target_output_dir).
        let target_output_dir = context.target_output_dir(&self.name);
        std::fs::create_dir_all(&target_output_dir)?;

        // SOURCE-header inputs for every CppCompile in this target: own `hdrs`
        // (resolved per-package) plus transitive dep SOURCE headers. Computed
        // ONCE here (the set is identical for every src) and passed to
        // compile_source so a header edit invalidates the object's action.id.
        let header_inputs = self.collect_source_header_inputs(context).await;

        // Compile each compilable source file to object file.
        // TF BUILD files often put .h files and cross-package label refs like
        // `//tsl/platform:mutex.h` in `srcs`. Bazel treats those as "owned by
        // this target but not compiled" — we match that: only compile files
        // whose extension is one of the known C/C++ source extensions, and
        // resolve label-shaped entries through resolve_src_label before use.
        let mut indexed_object_files = Vec::new();
        let mut indexed_artifacts = Vec::new();
        let mut compile_jobs = Vec::new();
        // Expand filegroup-label srcs into their files (see
        // expand_filegroup_srcs) so a cc_library whose `srcs` are
        // filegroups (TF's framework_internal_impl) compiles its real
        // sources. Runtime build() mirrors analyze()/legacy-spec.
        let expanded_srcs =
            expand_filegroup_srcs(&self.srcs, |label| context.get_dependency_files(label));
        let is_compilable_source = |src: &str| {
            let lower = src.to_ascii_lowercase();
            lower.ends_with(".cc")
                || lower.ends_with(".cpp")
                || lower.ends_with(".cxx")
                || lower.ends_with(".c")
                || lower.ends_with(".cu")
                || lower.ends_with(".s")
                || lower.ends_with(".S")
        };
        let resolve_existing_source = |src: &str| -> Option<PathBuf> {
            let mut src_path = resolve_src_label(context, src);
            // If the source-tree path doesn't exist, the `srcs` entry may be a
            // same-package GENRULE OUTPUT (e.g. the tf_gen_op_wrappers_cc
            // `ops/<group>.cc` op-wrapper sources). Resolve it from genfiles:
            // `<output_dir>/genfiles/<pkg>/<src>`, the layout the genrule's
            // build() writes. By build()/legacy time the genrule has already
            // executed, so on-disk existence is the signal here; analyze()
            // (PASS-A, before the genrule runs) makes the SAME compile decision
            // via the genrule's declared output artifact (#39(1) — see
            // docs/milestones/libtensorflow_cc_so_39_undefined_symbols.md). A
            // label-form entry whose output is owned by a *different-package*
            // dependency still resolves nowhere here and is correctly skipped.
            if !src_path.exists() {
                let pkg = context.package_path.clone().unwrap_or_default();
                let gen_abs = context.output_dir.join("genfiles").join(&pkg).join(src);
                if gen_abs.exists() {
                    src_path = gen_abs;
                } else {
                    return None;
                }
            }
            Some(src_path)
        };
        let source_prune_enabled =
            !context.materialize_static_libraries() && !context.dirty_paths().is_empty();
        let source_prune_enabled = if source_prune_enabled {
            let compilable_sources: HashSet<PathBuf> = expanded_srcs
                .iter()
                .filter(|src| prebuilt_lib_src(src).is_none())
                .filter(|src| is_compilable_source(src))
                .filter_map(|src| resolve_existing_source(src))
                .collect();
            context
                .dirty_paths()
                .iter()
                .all(|dirty| compilable_sources.contains(dirty))
        } else {
            false
        };
        for (source_index, src) in expanded_srcs.iter().enumerate() {
            // Prebuilt linker input (.a/.so) listed in srcs — not a TU. Expose
            // it as the library artifact so the consumer link picks it up via
            // the artifact sweep; analyze() registers the matching
            // CcInfo.linker_input. See prebuilt_lib_src.
            if let Some(is_static) = prebuilt_lib_src(src) {
                let lib_path = resolve_src_label(context, src);
                if lib_path.is_file() {
                    indexed_artifacts.push((
                        source_index,
                        Artifact {
                            path: lib_path,
                            artifact_type: if is_static {
                                ArtifactType::StaticLibrary
                            } else {
                                ArtifactType::SharedLibrary
                            },
                        },
                    ));
                }
                continue;
            }
            // Skip non-source files (headers etc. are exposed via -I paths,
            // not compiled as TUs).
            if !is_compilable_source(src) {
                continue;
            }
            let Some(src_path) = resolve_existing_source(src) else {
                continue;
            };

            // #39 link fix: object name must be collision-safe for
            // cross-package label srcs. A target may list BOTH a same-package
            // `tracing.cc` AND `//tsl/platform:tracing.cc` (tsl/platform/
            // default:tracing does); naming both from the bare file stem gives
            // them the same `tracing.o` path — the second compile overwrites
            // the first, and the archive obj-list then carries `tracing.o`
            // twice (multiple definition at link, default's SetCurrentThreadName
            // /GetLogDir lost). cc_obj_name_for_src qualifies a `//pkg:file`
            // label by its package (`/`→`_`) → `tsl_platform_tracing.o`,
            // distinct from default's `tracing.o`. This makes runtime build()
            // byte-identical to analyze() (rules_cc.rs:709) and the legacy
            // command-spec (rules_cc.rs:1127), preserving verifier parity.
            let obj_stem = src_path
                .file_stem()
                .ok_or_else(|| anyhow!("Invalid source file: {}", src))?
                .to_string_lossy()
                .to_string();
            let obj_path = target_output_dir.join(cc_obj_name_for_src(src, &obj_stem));

            if source_prune_enabled
                && !context.path_is_dirty(&src_path)
                && obj_path.exists()
            {
                indexed_object_files.push((source_index, obj_path.clone()));
                indexed_artifacts.push((
                    source_index,
                    Artifact {
                        path: obj_path,
                        artifact_type: ArtifactType::Object,
                    },
                ));
                continue;
            }

            compile_jobs.push((source_index, src.clone(), src_path, obj_path));
        }

        // A large cc_library can contain hundreds of translation units after
        // filegroup expansion. Target-level wavefront parallelism collapses at
        // these graph funnels unless the target also exposes its independent
        // compile actions concurrently. RushActionManager still owns the global
        // resource permits, so this local bound limits ready futures rather than
        // bypassing --jobs.
        let max_compile_in_flight = std::env::var("RUSH_BUILD_JOBS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|parallelism| parallelism.get())
                    .unwrap_or(1)
            })
            .max(1);
        // A single expanded filegroup can otherwise occupy every global
        // compiler permit before another ready target submits its critical
        // compile. Keep a measured fair share available across targets while
        // still giving large cc_library funnels most of the machine.
        let default_compile_window = max_compile_in_flight
            .saturating_mul(7)
            .saturating_add(11)
            / 12;
        let max_compile_in_flight = std::env::var("RUSH_CC_LIBRARY_COMPILE_WINDOW")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .map(|value| value.min(max_compile_in_flight))
            .unwrap_or(default_compile_window.max(1));
        use futures::StreamExt as _;
        let mut compile_results = futures::stream::iter(compile_jobs.into_iter().map(
            |(source_index, src, src_path, obj_path)| {
                let this = self;
                let context = context;
                let header_inputs = header_inputs.as_slice();
                async move {
                    let result = this
                        .compile_source(context, &src_path, &obj_path, header_inputs)
                        .await
                        .with_context(|| {
                            format!(
                                "cc_library compile failed: target={} src={} (src_path={}, obj_path={}, pkg={:?}, ws={})",
                                this.name,
                                src,
                                src_path.display(),
                                obj_path.display(),
                                context.package_path,
                                context.workspace_root.display()
                            )
                        });
                    (source_index, obj_path, result)
                }
            },
        ))
        .buffer_unordered(max_compile_in_flight)
        .collect::<Vec<_>>()
        .await;

        // Completion order is timing-dependent. Restore source order before
        // constructing artifacts and archive argv so builds remain deterministic.
        compile_results.sort_by_key(|(source_index, _, _)| *source_index);
        for (source_index, obj_path, result) in compile_results {
            result?;
            indexed_object_files.push((source_index, obj_path.clone()));
            indexed_artifacts.push((
                source_index,
                Artifact {
                    path: obj_path,
                    artifact_type: ArtifactType::Object,
                },
            ));
        }
        indexed_object_files.sort_by_key(|(source_index, _)| *source_index);
        indexed_artifacts.sort_by_key(|(source_index, _)| *source_index);
        let object_files: Vec<PathBuf> = indexed_object_files
            .into_iter()
            .map(|(_, path)| path)
            .collect();
        artifacts.extend(indexed_artifacts.into_iter().map(|(_, artifact)| artifact));

        // Create static library from object files when a downstream link (or
        // the requested root itself) needs the archive. Provider-only library
        // builds can skip this; their object artifacts still carry the changed
        // source semantics through the active graph.
        if !object_files.is_empty() && context.materialize_static_libraries() {
            // Some TF targets name themselves with a slash (e.g.
            // `ir/dtensor_attributes`) which would otherwise leak into
            // the output filename and push the .a file into a sibling
            // directory that hasn't been created — `ar` then fails with
            // ENOENT. Flatten the name so both `target_output_dir` and
            // the archive filename stay in-place.
            let safe_name = self.name.replace('/', "_");
            let lib_name = format!("lib{}.a", safe_name);
            let lib_path = target_output_dir.join(&lib_name);

            self.create_static_library(context, &object_files, &lib_path)
                .await?;

            artifacts.push(Artifact {
                path: lib_path,
                artifact_type: ArtifactType::StaticLibrary,
            });
        }

        Ok(artifacts)
    }

    /// Phase 5.B.3.e + 5.C0: analyze() builds the typed CcInfo
    /// AND declares the compile + archive actions for this
    /// library. Declaration registers the producer/consumer
    /// graph in the registry; orchestrator's post-analysis sweep
    /// validates it.
    fn analyze(
        &self,
        ctx: &mut crate::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        use crate::providers::{DirectCcInfo, LinkFragment};
        use rush_action::ids::ArtifactKind;
        use std::path::Path;

        // Register each header at its RESOLVED, package-qualified path so that
        // same-basename headers across packages (e.g. every rushbench package's
        // own `hdr_0.h`; the class TF/Triton hit) get DISTINCT artifacts. The
        // bare relative form `Path::new(h)` interned identically across packages
        // (the registry dedups by (config, exec_path)), collapsing N packages'
        // headers onto one artifact and a single freshness-ledger entry that
        // stat'd a cwd-relative path — so editing one package's header left the
        // ledger CLEAN and the cold-no-op certificate served a false no-op.
        // Source headers are NOT consumed for include-dir derivation (only
        // GENERATED headers are, via `!is_source` in collect_generated_header_*),
        // so the only effect of this resolution is correct per-header freshness.
        let hdr_pkg = ctx.package().to_string();
        let expanded_hdrs = expand_filegroup_srcs(&self.hdrs, |label| {
            ctx.get_dependency_provider(label)
                .and_then(|p| p.get_files())
                .map(|fi| fi.files.clone())
        });
        let direct_headers: Vec<_> = expanded_hdrs
            .iter()
            .map(|h| {
                let resolved =
                    resolve_cc_src_path(ctx.workspace_root(), &hdr_pkg, h);
                ctx.register_source_artifact(&resolved, ArtifactKind::Header)
            })
            .collect();
        let mut direct_include_dirs: Vec<_> = self
            .includes
            .iter()
            .map(|inc| ctx.resolve_package_include(Path::new(inc)))
            .collect();
        // Bazel `includes = [...]` projects each entry onto BOTH the source root
        // AND the genfiles (bin) root, and BOTH projections propagate to
        // consumers via CcInfo. So a target that `#include "<inc>/.../foo.h.inc"`
        // — directly OR transitively through a dep that declared `includes` —
        // finds a tblgen/genrule-GENERATED header under `<genfiles>/<pkg>/<inc>`.
        // Pushing the genfiles projection into direct_include_dirs (not just the
        // local argv) is what makes it flow into CcInfo.include_dirs and reach
        // transitive consumers. Without it, @triton TritonTransforms (no own
        // `includes`, deps=[":TritonDialects"]) inherited only the SOURCE
        // `-I include` from TritonDialects' CcInfo and died on Dialect.h.inc; the
        // genfiles `-I include` must ride the same CcInfo. Empty package
        // (repo-root BUILD, the @triton case) → `<genfiles>/<inc>`.
        {
            let pkg = ctx.package().to_string();
            for inc in &self.includes {
                if Path::new(inc).is_absolute() {
                    continue;
                }
                let rel = if pkg.is_empty() {
                    Path::new("genfiles").join(inc)
                } else {
                    Path::new("genfiles").join(&pkg).join(inc)
                };
                let gen_inc = ctx.predicted_output_path(&rel);
                direct_include_dirs.push(ctx.intern_path(gen_inc.as_path()));
            }
        }
        // strip_include_prefix exports an include dir to CONSUMERS (Bazel: a
        // dependent sees the headers with <prefix> stripped). e.g. utf8_validity
        // (strip_include_prefix="/third_party/utf8_range", NO `includes` attr)
        // must put -I<repo>/third_party/utf8_range into its CcInfo so a consumer
        // like protobuf_lite (any_lite.cc) finds bare `utf8_validity.h`.
        // ctx.workspace_root is the (external) repo root for the target, so the
        // repo-relative "/…" form resolves directly; resolve_package_include
        // passes an absolute path through unchanged. (Only "/…" handled here —
        // the form protobuf uses; "." / package-relative can follow if needed.)
        if let Some(prefix) = self.strip_include_prefix.as_deref() {
            if let Some(rel) = prefix.strip_prefix('/') {
                let dir = ctx.workspace_root().join(rel);
                if dir.is_dir() {
                    direct_include_dirs.push(ctx.resolve_package_include(&dir));
                }
            }
        }
        // `include_prefix` (the inverse of strip_include_prefix): expose this
        // library's headers to its own compile AND to consumers at
        // `<include_prefix>/<rel>`, where `<rel>` is the header's
        // package-relative path with `strip_include_prefix` removed first
        // (they compose). Bazel materializes a per-target `_virtual_includes`
        // tree and puts that root on the quote-include path; rush stages the
        // same farm (symlinks) under the build output dir and pushes the root
        // into `direct_include_dirs` — which becomes both this compile's `-I`
        // flags and the CcInfo.include_dirs seen by dependents. This replaces
        // the old per-dep file-copy workarounds (e.g. ducc/farmhash_gpu).
        if let Some(prefix) = self.include_prefix.as_deref() {
            if let Some(vroot) = stage_virtual_includes(
                ctx.workspace_root(),
                ctx.output_dir(),
                ctx.package(),
                &self.name,
                &expanded_hdrs,
                prefix,
                self.strip_include_prefix.as_deref(),
            ) {
                direct_include_dirs.push(ctx.resolve_package_include(&vroot));
            }
        }
        let direct_defines: Vec<_> = self
            .defines
            .iter()
            .map(|d| ctx.intern_string(d))
            .collect();
        let direct_link_fragments: Vec<LinkFragment> = self
            .linkopts
            .iter()
            .map(|f| LinkFragment::Flag(ctx.intern_string(f)))
            .collect();

        // Phase 5.C0: declare the compile + archive action graph
        // for this library, and expose the .a via linker_inputs so
        // cross-target validation chains. Generated artifacts go
        // through register_generated_artifact which uses
        // predicted_output_path internally — paths in the registry
        // match what build() creates on disk, so consumers reading
        // through the typed linker_inputs path get the same file
        // path as the legacy artifact NestedSet sweep.
        let mut direct_linker_inputs: Vec<rush_action::ids::LinkerInputId> = Vec::new();
        // Phase 5.C.4: cc_library's public output (DefaultInfo)
        // is the .a — never the intermediate .o files. Captured
        // alongside lib_aid below; threaded into the ProviderSet
        // at the bottom of analyze().
        let mut public_lib_aid: Option<rush_action::ids::ArtifactId> = None;

        // Prebuilt `.a`/`.so` linker inputs declared directly in `srcs`
        // (e.g. @tf_runtime's bootstrap-built libtfrt.a). Register each as a
        // source-artifact linker input so it flows through
        // CcInfo.linker_inputs into the consumer link's global --start-group
        // — NOT as a raw linkopt emitted after the closed group, where a
        // static archive's back-references (libtfrt.a -> pinned-absl
        // BlockingCounter) would go unresolved. Resolve absolute paths under
        // an immutable borrow first, then register (the registrars take &mut).
        // See prebuilt_lib_src.
        let prebuilt_libs: Vec<(std::path::PathBuf, bool)> = {
            let ws_root = ctx.workspace_root().to_path_buf();
            let pkg = ctx.package().to_string();
            self.srcs
                .iter()
                .filter_map(|s| {
                    prebuilt_lib_src(s).map(|is_static| {
                        (resolve_cc_src_path(&ws_root, &pkg, s), is_static)
                    })
                })
                .filter(|(p, _)| p.is_file())
                .collect()
        };
        for (lib_path, is_static) in &prebuilt_libs {
            let kind = if *is_static {
                ArtifactKind::StaticLibrary
            } else {
                ArtifactKind::SharedLibrary
            };
            let aid = ctx.register_source_artifact(lib_path, kind);
            let lid = ctx.register_linker_input(aid, self.alwayslink);
            direct_linker_inputs.push(lid);
            if public_lib_aid.is_none() {
                public_lib_aid = Some(aid);
            }
        }

        if !self.srcs.is_empty() {
            // Phase 5.C0 P0: generated headers from dep
            // CcCompilationContexts go into every CppCompile
            // input set so producer validation (pass 3) chains
            // through. See collect_generated_header_inputs
            // docstring for the array_ops.h-class bug this
            // catches.
            let dep_cc_infos = crate::providers::extract_cc_infos(dep_providers);

            // SOURCE-header compile inputs. A CppCompile must depend on the
            // headers its TU includes, or editing a header never changes the
            // object's action.id (content hash over inputs) → the cache serves a
            // stale object and the edit silently fails to recompile. Bazel makes
            // a header a declared input for exactly this reason. The target's own
            // `hdrs` (direct_headers) plus every transitively-reachable dep SOURCE
            // header form the set; generated headers already arrive via
            // generated_header_inputs above (which filters `!is_source`), so this
            // is the complementary `is_source` half. Over-inclusion is safe: an
            // unincluded header that changes just forces a recompile (correct,
            // never stale). Deduped against the generated set is unnecessary —
            // the two are disjoint by the is_source partition.
            let mut source_header_inputs: Vec<rush_action::ids::ArtifactId> = Vec::new();
            let mut seen_hdr: crate::nested_set::FastHashSet<
                rush_action::ids::ArtifactId,
            > = crate::nested_set::FastHashSet::default();
            let mut generated_header_inputs: Vec<rush_action::ids::ArtifactId> = Vec::new();
            let mut seen_generated_hdr: crate::nested_set::FastHashSet<
                rush_action::ids::ArtifactId,
            > = crate::nested_set::FastHashSet::default();
            for h_aid in direct_headers.iter().copied() {
                if seen_hdr.insert(h_aid) {
                    source_header_inputs.push(h_aid);
                }
            }
            let dep_header_roots = dep_cc_infos
                .iter()
                .flat_map(|dep_cc| {
                    [
                        std::sync::Arc::clone(&dep_cc.compilation.headers),
                        std::sync::Arc::clone(&dep_cc.compilation.textual_headers),
                    ]
                })
                .collect();
            let dep_headers = crate::nested_set::NestedSet::new(Vec::new(), dep_header_roots);
            for h_aid in dep_headers.iter() {
                if ctx.registry().artifact(h_aid).is_source {
                    if seen_hdr.insert(h_aid) {
                        source_header_inputs.push(h_aid);
                    }
                } else if seen_generated_hdr.insert(h_aid) {
                    generated_header_inputs.push(h_aid);
                }
            }

            // Phase 5.C.4 step 2b CppCompile migration. Collect
            // immutable inputs once before the per-src loop:
            // toolchain references, dep include-dir paths,
            // workspace_root, extra_proto_paths, genfiles dir,
            // direct include dirs. These are the same for every
            // src in this target, so amortizing the collection
            // outside the loop keeps the per-src work bounded.
            // The borrow-collect pattern (read &-borrowed strings
            // into owned Vec<String> before any &mut intern call)
            // is the canonical way through Rust's borrow checker
            // for mixed read/intern in analysis_context.
            let dep_include_paths: Vec<String> = dep_cc_infos
                .iter()
                .flat_map(|cc| {
                    cc.compilation
                        .include_dirs
                        .iter()
                        .map(|pid| ctx.registry().paths().path(pid).display().to_string())
                        .collect::<Vec<_>>()
                })
                .collect();
            let direct_include_paths: Vec<String> = direct_include_dirs
                .iter()
                .map(|pid| ctx.registry().paths().path(*pid).display().to_string())
                .collect();
            // Transitive dep `defines` (Bazel postorder) — emitted before
            // own direct defines so consumers compile with the same `-D`s
            // Bazel propagates via CcInfo.
            let dep_defines: Vec<String> = crate::providers::flatten_transitive_defines(
                ctx.registry().strings(),
                dep_cc_infos.iter().map(|cc| cc.compilation.defines.inner()),
            );
            let workspace_root_str = ctx.workspace_root().display().to_string();
            let extra_proto_strs: Vec<String> = ctx
                .extra_proto_paths()
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            let genfiles_str = ctx
                .predicted_output_path(Path::new("genfiles"))
                .display()
                .to_string();
            let cxxflags: Vec<String> = ctx.toolchain().cxxflags.clone();
            let cflags: Vec<String> = ctx.toolchain().cflags.clone();
            let cxx_pathbuf = ctx.toolchain().cxx.clone();
            let cc_pathbuf = ctx.toolchain().cc.clone();

            // Expand filegroup-label srcs into their files BEFORE the
            // per-src loop (the closure's immutable borrow of `ctx` ends
            // when expand_filegroup_srcs returns its owned Vec, so the
            // loop body can re-borrow `ctx` mutably). PASS-A counterpart
            // of build_legacy_command_specs' expansion — same shared
            // dep_label_matches → byte-identical source set / argv.
            let expanded_srcs = expand_filegroup_srcs(&self.srcs, |label| {
                ctx.get_dependency_provider(label)
                    .and_then(|p| p.get_files())
                    .map(|fi| fi.files.clone())
            });
            let mut obj_artifacts: Vec<_> = Vec::with_capacity(expanded_srcs.len());
            for src in &expanded_srcs {
                // AZF-2 (Pro 2026-05-21): only declare a
                // CppCompile action for real compile sources.
                // Headers (.h/.hpp/.inc) and label-form refs
                // may appear in srcs for textual-hdrs-style
                // cross-package inclusion but must NOT be
                // compiled. Three-way shared predicate keeps
                // the spec, runtime, and scheduler analyze in
                // sync — Pro 2026-05-21 warned against
                // fixing only the spec builder.
                if !crate::cc_command_spec::is_compilable_source(src) {
                    continue;
                }
                // Match build_legacy_command_specs (rules_cc.rs
                // ~line 730) which skips srcs whose resolved
                // workspace-relative path does not exist on disk.
                // Without this filter, label-form entries like
                // `"//tsl/platform:mutex.h"` survive into the
                // declared-action graph: file_stem() retains the
                // `:` separator, producing obj names like
                // `"platform:mutex.o"` → scheduler/legacy argv
                // divergence (verifier hard-stop class fired
                // post-commit-238 on
                // @local_tsl//tsl/platform/default:mutex). The
                // check is the same one source_path-aware paths
                // already pass.
                let pkg_check = ctx.package();
                // ROOT-3: resolve label-form srcs (cross-package exported
                // sources like //xla/tsl/platform:env.cc) against the target's
                // repo root. Non-label filenames resolve exactly as before.
                let mut src_path_check =
                    resolve_cc_src_path(ctx.workspace_root(), pkg_check, src);
                // #39(1): a srcs entry that is NOT a checked-in file may be a
                // same-package GENRULE OUTPUT (the tf_gen_op_wrappers_cc
                // `ops/<group>.cc` op-wrapper sources). Resolve it via the
                // genrule's DECLARED output artifact (registered by
                // GenruleInstance::analyze) — filesystem-INDEPENDENT, since
                // PASS-A runs before the genrule executes. build()/legacy reach
                // the SAME decision via on-disk existence (the genrule has run
                // by then), so the resulting compile argv is identical →
                // PASS-A/PASS-B parity. A label whose output is owned by a
                // different-package dep resolves nowhere and is skipped.
                let generated_src_aid = if src_path_check.exists() {
                    None
                } else {
                    let rel = Path::new("genfiles").join(ctx.package()).join(src);
                    let gen_abs = ctx.predicted_output_path(&rel);
                    match ctx.registry().find_artifact_by_path(&gen_abs, ctx.config_id()) {
                        Some(aid) => {
                            src_path_check = gen_abs;
                            Some(aid)
                        }
                        None => continue,
                    }
                };
                // Register the source at its RESOLVED, package-qualified path
                // (`src_path_check`, already filesystem-checked above) for BOTH
                // label and same-package filename forms. A generated src uses the
                // genrule's producer artifact id directly.
                //
                // INCREMENTAL-CORRECTNESS FIX (same-basename cross-package srcs):
                // the same-package branch previously registered the BARE relative
                // string `Path::new(src)` (e.g. `"lib.cc"`). The ArtifactRegistry
                // interns paths verbatim and dedups by (config, exec_path), so N
                // packages that each list a same-basename src — `pkg_0/lib.cc`,
                // `pkg_1/lib.cc`, … (rushbench wide_Nk; and the class TF/Triton
                // hit) — ALL collapsed onto ONE artifact keyed on `"lib.cc"`. The
                // freshness ledger then stat'd the bare relative `"lib.cc"`
                // (resolved against the process cwd = workspace root, where no
                // such file exists → recorded `present=false`), so editing any
                // one package's source left the ledger CLEAN and the cold-no-op
                // certificate served a false no-op: the edit did NOT recompile.
                // `Action::compute_id` likewise content-hashed the wrong (or a
                // nonexistent) file. Registering the resolved per-package path —
                // exactly what the label branch already did — gives each package's
                // source a DISTINCT artifact whose ledger entry and action-id
                // content-hash track the real file on disk.
                //
                // Parity is preserved: the compile argv uses `src_path_check`
                // (line ~1213), not this artifact's stored path, and the object
                // name is derived from the raw `src` via cc_obj_name_for_src
                // (line ~1154) — neither changes. Only the freshness/input
                // tracking, which was buggy, is corrected.
                let src_aid = if let Some(aid) = generated_src_aid {
                    aid
                } else {
                    ctx.register_source_artifact(&src_path_check, ArtifactKind::Source)
                };
                // Match build()'s object-file layout:
                // <output_dir>/<self.name>/<src_stem>.o (cc_library
                // build() does target_output_dir.join(format!("{}.o",
                // file_stem)) where target_output_dir =
                // output_dir.join(&self.name)). Phase 5.C.2 caught
                // the prior "obj/<stem>.o" registration as a stat
                // miss against the actual on-disk file.
                //
                // We also use the resolved src_path_check (which
                // was filesystem-checked above) for file_stem so
                // we get the same stem build_legacy_command_specs
                // produces — `src_path.file_stem()` on the
                // absolute resolved path, not on the raw src
                // string. Identical to legacy's behavior for
                // package-relative filenames; defensive parity
                // for label-form entries the exists() check
                // already skipped.
                let obj_name = ctx.target_output_rel(
                    &self.name,
                    &cc_obj_name_for_src(
                        src,
                        src_path_check
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("obj"),
                    ),
                );
                let obj_aid = ctx.register_generated_artifact(
                    Path::new(&obj_name),
                    ArtifactKind::Object,
                    None,
                );
                obj_artifacts.push(obj_aid);
                let mut compile_inputs = vec![src_aid];
                compile_inputs.extend(generated_header_inputs.iter().copied());
                compile_inputs.extend(source_header_inputs.iter().copied());
                // is_c_source drives BOTH the action mnemonic and the
                // C-vs-C++ flag/toolchain choice. The mnemonic MUST match
                // build_legacy_command_specs (PASS-B), which uses "CCompile"
                // for .c sources and "CppCompile" otherwise. PASS-A
                // previously hardcoded "CppCompile" even for .c, so every
                // C-source action (e.g. all 448 of @boringssl//:crypto)
                // key-mismatched the legacy "CCompile" spec and showed as a
                // scheduler-only divergence under
                // --scheduler-authoritative=verified.
                let is_c_source = Path::new(src)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("c"))
                    .unwrap_or(false);
                let compile_mnemonic =
                    if is_c_source { "CCompile" } else { "CppCompile" };
                if let Ok(compile_id) = ctx.declare_action(
                    compile_mnemonic,
                    compile_inputs,
                    vec![obj_aid],
                ) {
                    // Phase 5.C.4 step 2b: assemble compile argv mirroring
                    // cc_library's compile_source at build time.

                    // Match build()'s context.source_path:
                    // <workspace_root>/<package>/<src>. The
                    // registry stores src artifacts at the raw
                    // path the rule registered (relative
                    // package-local), but the compiler runs
                    // against the absolute path so cwd
                    // independence is preserved.
                    // #39(1) STEP B fix: use the STEP-B-resolved source path
                    // (`src_path_check`) for the compile argv — source-tree for
                    // checked-in srcs, ROOT-3 absolute for cross-package label
                    // srcs, and the genrule's genfiles output for generated
                    // op-wrapper srcs (tf_gen_op_wrappers_cc `ops/<group>.cc`).
                    // Re-resolving the raw `src` here always yields the
                    // source-tree path, which does NOT exist for a generated src
                    // → PASS-A would dispatch a compile of a nonexistent file
                    // while build()/legacy (PASS-B) compile the genfiles path
                    // (rules_cc.rs ~1143). That is the #39(1) divergence the
                    // array_ops verification build hit. `src_path_check` is the
                    // single resolution all three passes now agree on.
                    let src_path_str = src_path_check.display().to_string();
                    let obj_path_str = ctx
                        .registry()
                        .artifact_path(obj_aid)
                        .display()
                        .to_string();

                    let mut argv_strs: Vec<String> = Vec::new();
                    argv_strs.push("-c".to_string());
                    argv_strs.push("-fPIC".to_string());
                    if !is_c_source {
                        argv_strs.push("-std=c++17".to_string());
                        // gcc-13+/libstdc++ no longer transitively include
                        // <cstdint>, so TF/LLVM/MLIR headers using int32_t etc.
                        // without including it (e.g. stablehlo math_utils.h)
                        // fail "'int32_t' has not been declared". Force-include
                        // it for C++ TUs (toolchain workaround, NOT a TF source
                        // patch). C sources keep their own headers, and
                        // assembly (.s/.S) is excluded — it cannot include the
                        // C++ header (e.g. @boringssl//:crypto *-apple.S).
                        if !crate::cc_command_spec::is_assembly_source_path(
                            Path::new(src),
                        ) {
                            argv_strs.push("-include".to_string());
                            argv_strs.push("cstdint".to_string());
                        }
                        // -fno-rtti for @llvm-project consumers, placed right
                        // after the cstdint block (inside !is_c_source) so this
                        // analyze argv (PASS-A) stays byte-identical to the
                        // CcCompileCommandSpec (PASS-B). Matches LLVM's
                        // -fno-rtti build (converter-gen typeinfo link).
                        if deps_want_llvm_no_rtti(&self.deps) {
                            argv_strs.push("-fno-rtti".to_string());
                        }
                    }
                    #[cfg(target_os = "linux")]
                    {
                        argv_strs.push("-D_GNU_SOURCE".to_string());
                        for extra in [
                            "/usr/include/jsoncpp",
                            "/usr/include/x86_64-linux-gnu",
                        ] {
                            if std::path::Path::new(extra).exists() {
                                argv_strs.push(format!("-I{}", extra));
                            }
                        }
                    }
                    // Filter copts for C-vs-C++ language compatibility.
                    for opt in &self.copts {
                        // Drop unexpanded `$(VAR)` make-variable copts (e.g.
                        // TF's `$(STACK_FRAME_UNLIMITED)`) — rush doesn't model
                        // make-vars, and a literal $(...) reaches g++ as a
                        // missing input file. Empty expansion is the safe
                        // degradation (these are warning/frame-size controls).
                        if opt.contains("$(") {
                            continue;
                        }
                        if is_c_source && opt.starts_with("-std=c++") {
                            continue;
                        }
                        if !is_c_source
                            && opt.starts_with("-std=c")
                            && !opt.starts_with("-std=c++")
                        {
                            continue;
                        }
                        argv_strs.push(opt.clone());
                    }
                    // GPU/ROCm define routing. Two scopes under RUSH_ENABLE_ROCM:
                    //  (1) HOST-WIDE header/type selection: every C++ TU gets
                    //      -DTENSORFLOW_USE_ROCM=1 + -D__HIP_PLATFORM_AMD__ so host
                    //      StreamExecutor .cc (gpu_types.h etc.) take the HIP branch instead of
                    //      the CUDA #else (cuComplex.h). Probe-verified: host g++ compiles the HIP
                    //      branch. This mirrors TF's tf_copts/if_rocm config-wide define.
                    //  (2) GPU-SOURCE-ONLY: a .cu.cc/.cu additionally gets the Eigen-GPU defines +
                    //      --offload-arch and is compiled by hipcc (see the exe selection below).
                    //      Eigen-GPU on a plain host g++ TU would break it, so it stays per-source.
                    let rocm_enabled =
                        std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1");
                    let is_gpu_source = rocm_enabled
                        && (src.ends_with(".cu.cc") || src.ends_with(".cu"));
                    if rocm_enabled {
                        // -I/opt/rocm/include so host .cc that take the HIP branch resolve the
                        // internal hip/ header tree (hip_complex.h -> hip/hip_common.h ...).
                        for f in [
                            "-DTENSORFLOW_USE_ROCM=1",
                            "-D__HIP_PLATFORM_AMD__",
                            "-DEIGEN_USE_HIP",
                            "-I/opt/rocm/include",
                        ] {
                            if !argv_strs.iter().any(|a| a == f) {
                                argv_strs.push(f.to_string());
                            }
                        }
                    }
                    if is_gpu_source {
                        for f in [
                    "-DEIGEN_USE_GPU",
                    "-DEIGEN_USE_HIP",
                    "--offload-arch=gfx1100",
                    // gcc-15 libstdc++ auto-enables _GLIBCXX_ASSERTIONS when not __OPTIMIZE__
                    // (c++config.h), so std::complex's __glibcxx_assert calls __host__
                    // __glibcxx_assert_fail — which hipcc's device pass rejects from a
                    // __host__ __device__ fn. Disable libstdc++ assertions for device code so
                    // std::complex GPU kernels (cwise etc.) compile. (Codex 019eebc6.)
                    "-D_GLIBCXX_NO_ASSERTIONS",
                ] {
                            if !argv_strs.iter().any(|a| a == f) {
                                argv_strs.push(f.to_string());
                            }
                        }
                        if let Some(flag) = gpu_device_gcc_install_dir_flag() {
                            if !argv_strs.iter().any(|a| *a == flag) {
                                argv_strs.push(flag);
                            }
                        }
                    }
                    let is_cuda_source =
                        append_cuda_compile_flags(&mut argv_strs, Path::new(src));
                    if is_cuda_source {
                        normalize_cuda_compile_flags(&mut argv_strs);
                    }
                    if !is_c_source {
                        argv_strs.extend(cxxflags.iter().cloned());
                    } else {
                        argv_strs.extend(cflags.iter().cloned());
                    }
                    for d in crate::providers::merged_compile_defines(&dep_defines, &self.defines) {
                        argv_strs.push(format!("-D{}", d));
                    }
                    for ip in &direct_include_paths {
                        argv_strs.push(format!("-I{}", ip));
                    }
                    // Dedup -I against direct entries to mirror
                    // build()'s args.contains() check.
                    let mut argv_seen: std::collections::HashSet<String> =
                        argv_strs.iter().cloned().collect();
                    for ip in &dep_include_paths {
                        let flag = format!("-I{}", ip);
                        if argv_seen.insert(flag.clone()) {
                            argv_strs.push(flag);
                        }
                    }
                    argv_strs.push(format!("-I{}", workspace_root_str));
                    for extra in &extra_proto_strs {
                        argv_strs.push(format!("-I{}", extra));
                    }
                    argv_strs.push(format!("-I{}", genfiles_str));
                    // Demote /usr/include to -isystem (after all -I) so the
                    // pinned Abseil -I wins over the host's debian7 absl. Same
                    // transform as PASS-B (CcCompileCommandSpec) → verifier parity.
                    crate::cc_command_spec::demote_host_system_includes(&mut argv_strs);
                    argv_strs.push("-o".to_string());
                    argv_strs.push(obj_path_str);
                    let selected_cxx = selected_cxx_for_source(Path::new(&src_path_str));
                    argv_strs.push(src_path_str);

                    let exe_pid = if is_cuda_source {
                        ctx.intern_path(&cuda_device_compiler())
                    } else if is_gpu_source {
                        ctx.intern_path(Path::new("/opt/rocm/bin/hipcc"))
                    } else if is_c_source {
                        ctx.intern_path(&cc_pathbuf)
                    } else if let Some(selected_cxx) = selected_cxx {
                        ctx.intern_path(&selected_cxx)
                    } else {
                        ctx.intern_path(&cxx_pathbuf)
                    };
                    let argv: Vec<rush_action::ids::StringId> = argv_strs
                        .iter()
                        .map(|s| ctx.intern_string(s))
                        .collect();
                    ctx.set_executable_action(
                        compile_id,
                        rush_action::ids::ExecutableAction {
                            executable: exe_pid,
                            argv: argv.into_boxed_slice(),
                            env: Box::new([]),
                            working_dir: None,
                            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                        },
                    );
                }
            }
            // AZF-2 (Pro 2026-05-21): only declare the
            // CcArchive when at least one CppCompile action
            // was emitted. If every src was filtered out by
            // is_compilable_source (e.g. a cc_library with
            // only headers in srcs), there are no .o inputs
            // to archive — declaring an empty archive would
            // produce a declared action with 0 inputs that
            // legacy doesn't match, breaking verifier
            // symmetry.
            if obj_artifacts.is_empty() {
                // Skip archive declaration; rule legitimately
                // declares zero compile actions. The
                // NoDeclaredActions class (commit 206) covers
                // this case: scheduler accepts as no-op,
                // provider info still flows via CcInfo.
            } else {
            // Match build()'s archive path layout (package-qualified;
            // see BuildContext::target_output_dir):
            // <output_dir>/<package>/<self.name>/lib<safe_name>.a where
            // safe_name swaps '/' for '_' (some labels include
            // package separators in the target name).
            let safe_name = self.name.replace('/', "_");
            let lib_rel =
                ctx.target_output_rel(&self.name, &format!("lib{}.a", safe_name));
            let lib_aid = ctx.register_generated_artifact(
                Path::new(&lib_rel),
                ArtifactKind::StaticLibrary,
                None,
            );
            // Phase 5.C.4 step 2b: declare CcArchive AND populate
            // its ExecutableAction so the phase 5.C.4 scheduler
            // can run it without falling through to legacy
            // build(). argv mirrors what
            // `cc_library::create_static_library` constructs at
            // build time: `ar rcs <lib_path> --thin <obj1_path> ...`. We
            // collect path strings before the mutating intern
            // calls to avoid the &/&mut borrow-checker conflict
            // on AnalysisContext. Path strings are interned
            // through `ctx.intern_string` so the registry's
            // string interner deduplicates across actions.
            let archive_inputs = obj_artifacts.clone();
            let lib_path_str = ctx.registry().artifact_path(lib_aid).display().to_string();
            let obj_path_strs: Vec<String> = obj_artifacts
                .iter()
                .map(|aid| ctx.registry().artifact_path(*aid).display().to_string())
                .collect();
            let ar_pathbuf = ctx.toolchain().ar.clone();
            if let Ok(archive_id) = ctx.declare_action(
                "CcArchive",
                archive_inputs,
                vec![lib_aid],
            ) {
                let ar_path = ctx.intern_path(&ar_pathbuf);
                let mut argv: Vec<rush_action::ids::StringId> =
                    Vec::with_capacity(3 + obj_path_strs.len());
                argv.push(ctx.intern_string("rcs"));
                argv.push(ctx.intern_string(&lib_path_str));
                argv.push(ctx.intern_string("--thin"));
                for s in &obj_path_strs {
                    argv.push(ctx.intern_string(s));
                }
                ctx.set_executable_action(
                    archive_id,
                    rush_action::ids::ExecutableAction {
                        executable: ar_path,
                        argv: argv.into_boxed_slice(),
                        env: Box::new([]),
                        working_dir: None,
                        env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                    },
                );
            }
            // Expose the .a as a linker input, honoring `alwayslink`: when set
            // (e.g. TF's tf_gen_op_libs `<op>_op_lib` carrying REGISTER_OP), the
            // consuming binary's CcLink must --whole-archive it so its static
            // registrations survive. Cross-target validation (Pass 3) finds the
            // CcArchive action as producer when cc_binary's CcLink declares this
            // dep's .a as an input.
            let lid = ctx.register_linker_input(lib_aid, self.alwayslink);
            direct_linker_inputs.push(lid);
            public_lib_aid = Some(lib_aid);
            } // close AZF-2 `else { ... }` guard on obj_artifacts.is_empty()
        }

        let direct = DirectCcInfo {
            headers: direct_headers,
            include_dirs: direct_include_dirs,
            defines: direct_defines,
            link_fragments: direct_link_fragments,
            linker_inputs: direct_linker_inputs,
            ..DirectCcInfo::default()
        };
        let cc_info = CcInfo::from_direct_and_deps(direct, crate::providers::extract_cc_infos(dep_providers).into_iter());
        let mut providers = ProviderSet::empty().with_cc_info(cc_info);
        if let Some(aid) = public_lib_aid {
            providers = providers.with_default_info(crate::providers::DefaultInfo {
                files: vec![aid],
            });
        }
        providers
    }

    fn rule_type(&self) -> &str {
        "cc_library"
    }

    /// Phase 5.C.4 commit 4: legacy spec builder for the
    /// verifier. Mirrors what `compile_source` and
    /// `create_static_library` would assemble at build time,
    /// but stops at the spec — never spawns. Returns one
    /// `(key, NormalizedCommandSpec)` per CppCompile (one per
    /// src) plus one for CcArchive.
    async fn build_legacy_command_specs(
        &self,
        context: &BuildContext,
    ) -> Vec<(crate::SchedulerActionKey, rush_action::command_spec::NormalizedCommandSpec)> {
        use crate::cc_command_spec::{
            CcArchiveCommandSpec, CcCompileCommandSpec, resolve_include_under_package,
        };
        use crate::SchedulerActionKey;
        let mut specs = Vec::new();
        if self.srcs.is_empty() {
            return specs;
        }

        // Collect dep include paths from typed CcInfo. Same
        // walk as compile_source uses. Also collect generated
        // header paths via commit-27's centralized helper.
        // Phase 5.C.4 commit 14: declared_input_paths must include
        // dep .pb.h (and other generated headers) — the action
        // graph reads them via include resolution. Commit 27:
        // helper extraction; was previously inlined here, also
        // in cc_binary, tf_cc_library, with a latent gap in
        // cc_test.
        let mut dep_include_paths: Vec<std::path::PathBuf> = Vec::new();
        let mut dep_generated_header_paths: Vec<std::path::PathBuf> = Vec::new();
        // SOURCE-header declared inputs (PASS-B mirror of analyze()'s
        // source_header_inputs). Must equal the PASS-A set or the verifier flags
        // a declared-input divergence. A CppCompile depending on the headers it
        // includes is also what makes a header edit recompile the object.
        let mut dep_source_header_paths: Vec<std::path::PathBuf> = Vec::new();
        // Transitive dep `defines` (Bazel postorder) for this library's
        // own compile — collected from the ordered dep source so it
        // matches analyze()'s flatten.
        let mut dep_defines: Vec<String> = Vec::new();
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            // Include-dir walk stays inline (Pro: "Keep [the
            // header helper] separate from include-dir
            // construction. Header inputs and include search
            // paths are related, but not the same thing.").
            for (_dep_key, providers) in &context.dependency_providers_ordered {
                if let Some(cc_info) = providers.get_cc_info() {
                    for path_id in cc_info.compilation.include_dirs.iter() {
                        let p = reg.paths().path(path_id).to_path_buf();
                        if !dep_include_paths.contains(&p) {
                            dep_include_paths.push(p);
                        }
                    }
                }
            }
            let ordered_cc_infos: Vec<std::sync::Arc<crate::providers::CcInfo>> = context
                .dependency_providers_ordered
                .iter()
                .filter_map(|(_k, p)| p.cc_info.clone())
                .collect();
            dep_defines = crate::providers::flatten_transitive_defines(
                reg.strings(),
                ordered_cc_infos.iter().map(|cc| cc.compilation.defines.inner()),
            );
            // Generated-header walk via the commit-27 helper.
            // BuildContext.dependency_providers is HashMap<String,
            // ProviderSet> (not Arc-wrapped); collect cc_info Arcs
            // by walking the map directly.
            let cc_infos: Vec<std::sync::Arc<crate::providers::CcInfo>> = context
                .dependency_providers
                .values()
                .filter_map(|p| p.cc_info.clone())
                .collect();
            dep_generated_header_paths =
                crate::providers::collect_generated_header_paths(&reg, &cc_infos);
            // Dep SOURCE headers (is_source half; the generated half is above).
            for dep_cc in &cc_infos {
                for h_aid in dep_cc.compilation.headers.iter() {
                    if reg.artifact(h_aid).is_source {
                        let p = reg.artifact_path(h_aid).to_path_buf();
                        if !dep_source_header_paths.contains(&p) {
                            dep_source_header_paths.push(p);
                        }
                    }
                }
                for h_aid in dep_cc.compilation.textual_headers.iter() {
                    if reg.artifact(h_aid).is_source {
                        let p = reg.artifact_path(h_aid).to_path_buf();
                        if !dep_source_header_paths.contains(&p) {
                            dep_source_header_paths.push(p);
                        }
                    }
                }
            }
        }
        // This target's OWN headers, resolved per-package — the PASS-B mirror of
        // analyze()'s `direct_headers`. Combined with the dep source headers
        // these complete the source-header declared-input set.
        let own_pkg = context
            .package_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let mut source_header_paths: Vec<std::path::PathBuf> = self
            .hdrs
            .iter()
            .map(|h| resolve_cc_src_path(&context.workspace_root, &own_pkg, h))
            .collect();
        source_header_paths.extend(dep_source_header_paths);
        source_header_paths.sort();
        source_header_paths.dedup();

        // Direct include paths from self.includes. Use the
        // canonical resolver so legacy and scheduler agree on
        // "." special-casing.
        let pkg = context
            .package_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let direct_include_paths: Vec<std::path::PathBuf> = self
            .includes
            .iter()
            .map(|inc| {
                resolve_include_under_package(
                    &context.workspace_root,
                    &pkg,
                    std::path::Path::new(inc),
                )
            })
            .collect();

        // Linux extra includes — runtime exists() check, same
        // as compile_source.
        let mut linux_extra_includes: Vec<std::path::PathBuf> = Vec::new();
        if cfg!(target_os = "linux") {
            for extra in ["/usr/include/jsoncpp", "/usr/include/x86_64-linux-gnu"] {
                if std::path::Path::new(extra).exists() {
                    linux_extra_includes.push(std::path::PathBuf::from(extra));
                }
            }
        }

        let target_output_dir = context.target_output_dir(&self.name);
        // Expand filegroup-label srcs (PASS-B). Same shared
        // dep_label_matches + ordered walk as analyze() (PASS-A) so the
        // emitted CppCompile spec set is byte-identical.
        let expanded_srcs =
            expand_filegroup_srcs(&self.srcs, |label| context.get_dependency_files(label));
        let mut object_paths: Vec<std::path::PathBuf> = Vec::new();
        for src in &expanded_srcs {
            // AZF-2 (Pro 2026-05-21): only emit a CppCompile
            // spec for real compile sources. Headers
            // (.h/.hpp/.inc) and label-form refs may appear in
            // srcs for textual-hdrs-style cross-package
            // inclusion but must NOT be compiled. Three-way
            // shared predicate keeps the spec, runtime, and
            // scheduler analyze in sync.
            if !crate::cc_command_spec::is_compilable_source(src) {
                continue;
            }
            // ROOT-3: resolve cross-package label srcs (e.g.
            // //xla/tsl/platform:env.cc) against the target's repo root —
            // byte-identical to analyze()'s resolve_cc_src_path so PASS-A/PASS-B
            // match. Non-label filenames keep context.source_path unchanged.
            let mut src_path = if is_label_src(src) {
                let pkg_legacy = context
                    .package_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                resolve_cc_src_path(&context.workspace_root, &pkg_legacy, src)
            } else {
                context.source_path(src)
            };
            // #39(1): same-package genrule OUTPUT (tf_gen_op_wrappers_cc
            // `ops/<group>.cc`) → resolve from genfiles. Identical to build()
            // (the genrule has run by legacy-exec time); analyze() reaches the
            // same decision via the declared artifact → PASS-A/PASS-B parity.
            if !src_path.exists() {
                let pkg = context.package_path.clone().unwrap_or_default();
                let gen_abs = context.output_dir.join("genfiles").join(&pkg).join(src);
                if gen_abs.exists() {
                    src_path = gen_abs;
                } else {
                    continue;
                }
            }
            let obj_stem = src_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("obj");
            // ROOT-3: source-label-qualified obj name (matches analyze) so a
            // same-package + cross-package src sharing a basename don't collide.
            let obj_path = target_output_dir.join(cc_obj_name_for_src(src, obj_stem));
            object_paths.push(obj_path.clone());

            let is_c_source = src_path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("c"))
                .unwrap_or(false);

            // GPU/ROCm per-source routing (PASS-B spec) — mirror compile_source_impl so the
            // verifier sees the same exe (hipcc) + ROCm flags for a .cu.cc/.cu, not a host
            // g++ mismatch. Gated by RUSH_ENABLE_ROCM.
            let rocm_enabled =
                std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1");
            let is_gpu_source = rocm_enabled && {
                let s = src_path.to_string_lossy();
                s.ends_with(".cu.cc") || s.ends_with(".cu")
            };
            let mut spec_copts = self.copts.clone();
            // Host-wide ROCm header/type defines (all C++ TUs) + GPU-source-only Eigen/offload.
            if rocm_enabled {
                for f in [
                    "-DTENSORFLOW_USE_ROCM=1",
                    "-D__HIP_PLATFORM_AMD__",
                    "-DEIGEN_USE_HIP",
                    "-I/opt/rocm/include",
                ] {
                    if !spec_copts.iter().any(|c| c == f) {
                        spec_copts.push(f.to_string());
                    }
                }
            }
            if is_gpu_source {
                for f in [
                    "-DEIGEN_USE_GPU",
                    "-DEIGEN_USE_HIP",
                    "--offload-arch=gfx1100",
                    // gcc-15 libstdc++ auto-enables _GLIBCXX_ASSERTIONS when not __OPTIMIZE__
                    // (c++config.h), so std::complex's __glibcxx_assert calls __host__
                    // __glibcxx_assert_fail — which hipcc's device pass rejects from a
                    // __host__ __device__ fn. Disable libstdc++ assertions for device code so
                    // std::complex GPU kernels (cwise etc.) compile. (Codex 019eebc6.)
                    "-D_GLIBCXX_NO_ASSERTIONS",
                ] {
                    if !spec_copts.iter().any(|c| c == f) {
                        spec_copts.push(f.to_string());
                    }
                }
                if let Some(flag) = gpu_device_gcc_install_dir_flag() {
                    if !spec_copts.iter().any(|c| *c == flag) {
                        spec_copts.push(flag);
                    }
                }
            }
            let is_cuda_source = append_cuda_compile_flags(&mut spec_copts, &src_path);
            if is_cuda_source {
                normalize_cuda_compile_flags(&mut spec_copts);
            }

            let compile = CcCompileCommandSpec {
                mnemonic: if is_c_source {
                    "CCompile".to_string()
                } else {
                    "CppCompile".to_string()
                },
                executable: if is_cuda_source {
                    cuda_device_compiler()
                } else if is_gpu_source {
                    std::path::PathBuf::from("/opt/rocm/bin/hipcc")
                } else if is_c_source {
                    context.toolchain.cc.clone()
                } else {
                    selected_cxx_for_source(&src_path)
                        .unwrap_or_else(|| context.toolchain.cxx.clone())
                },
                cwd: None,
                is_c_source,
                wants_no_rtti: deps_want_llvm_no_rtti(&self.deps),
                src_path: src_path.clone(),
                obj_path: obj_path.clone(),
                copts: spec_copts,
                cxxflags: context.toolchain.cxxflags.clone(),
                cflags: context.toolchain.cflags.clone(),
                defines: self.defines.clone(),
                dep_defines: dep_defines.clone(),
                direct_include_paths: direct_include_paths.clone(),
                direct_quote_include_paths: Vec::new(),
                dep_include_paths: dep_include_paths.clone(),
                include_layout: crate::cc_command_spec::IncludeLayout::BazelPackageRooted {
                    workspace_root: context.workspace_root.clone(),
                    extra_workspace_paths: context.extra_proto_paths.to_vec(),
                    genfiles_path: context.output_dir.join("genfiles"),
                    linux_extra_includes: linux_extra_includes.clone(),
                },
                // Phase 5.C.4 commit 14: declared_input_paths
                // mirrors what the scheduler analyze() declares
                // — the .cc src plus dep generated headers the
                // action will actually read via -I. Without the
                // generated-header inputs, the verifier's
                // declared-input comparison flags
                // SchedulerOnly-only entries.
                declared_input_paths: {
                    let mut inputs = vec![src_path.clone()];
                    inputs.extend(dep_generated_header_paths.iter().cloned());
                    inputs.extend(source_header_paths.iter().cloned());
                    inputs
                },
                declared_output_paths: vec![obj_path.clone()],
                toolchain_identity: None,
            };
            specs.push((
                SchedulerActionKey {
                    mnemonic: compile.mnemonic.clone(),
                    primary_output_path: obj_path,
                },
                compile.to_normalized_command_spec(),
            ));
        }

        // CcArchive — one per cc_library that has any objects.
        if !object_paths.is_empty() {
            let safe_name = self.name.replace('/', "_");
            let lib_name = format!("lib{}.a", safe_name);
            let lib_path = target_output_dir.join(&lib_name);
            let archive = CcArchiveCommandSpec {
                mnemonic: "CcArchive".to_string(),
                executable: context.toolchain.ar.clone(),
                cwd: None,
                lib_path: lib_path.clone(),
                object_paths: object_paths.clone(),
                declared_input_paths: object_paths.clone(),
                declared_output_paths: vec![lib_path.clone()],
                toolchain_identity: None,
            };
            specs.push((
                SchedulerActionKey {
                    mnemonic: "CcArchive".to_string(),
                    primary_output_path: lib_path,
                },
                archive.to_normalized_command_spec(),
            ));
        }

        specs
    }
}

impl CcLibraryInstance {
    /// SOURCE-header inputs for this target's compiles: own `hdrs` resolved
    /// per-package, plus every transitively-reachable dep SOURCE header. These
    /// are added to each CppCompile action's inputs so a header edit changes the
    /// object's action.id (content-hashed cache key) and forces a recompile —
    /// the legacy build() analogue of analyze()'s `source_header_inputs`.
    async fn collect_source_header_inputs(&self, context: &BuildContext) -> Vec<PathBuf> {
        let own_pkg = context
            .package_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        // A label-shaped `hdrs` entry names a TARGET (typically a genrule
        // or filegroup), not a file in this package. Resolving it as a
        // source path yields `<ws>/<pkg>/<target-name>`, which never
        // exists — the real artifact is under genfiles, and the target
        // name need not even match the output filename. The compile then
        // silently dropped it (the `hp.exists()` guard below), so editing
        // the generator changed the header on disk while this target's
        // own objects kept their cache hit: a stale build with no error.
        // Resolve those entries through the dependency's DIRECT artifacts
        // instead. Direct, not transitive: a cc_library dep would
        // otherwise drag its whole closure into every action id.
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut unresolved_labels: Vec<&str> = Vec::new();
        for h in &self.hdrs {
            let label_shaped =
                h.starts_with(':') || h.starts_with("//") || h.starts_with('@');
            if label_shaped {
                match context.direct_artifacts(h) {
                    Some(arts) => paths.extend(arts.iter().map(|a| a.path.clone())),
                    // Do NOT fall through to "maybe it is a source path".
                    // That guess produced <ws>/<pkg>/<target-name>, which
                    // cannot exist, and the exists() filter then dropped
                    // the header from the compile's inputs — a stale
                    // build with no diagnostic. Record it instead.
                    None => unresolved_labels.push(h.as_str()),
                }
                continue;
            }
            paths.push(resolve_cc_src_path(&context.workspace_root, &own_pkg, h));
        }
        if !unresolved_labels.is_empty() {
            eprintln!(
                "⚠️  {}: {} hdrs label(s) did not resolve to a dependency and \
                 are NOT compile inputs ({}). Editing them will not recompile \
                 this target.",
                self.name,
                unresolved_labels.len(),
                unresolved_labels.join(", ")
            );
        }
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            for providers in context.dependency_providers.values() {
                if let Some(cc_info) = providers.get_cc_info() {
                    for h_aid in cc_info
                        .compilation
                        .headers
                        .iter()
                        .chain(cc_info.compilation.textual_headers.iter())
                    {
                        // Generated headers count too. Filtering on
                        // `is_source` excluded exactly the headers whose
                        // content changes without a source edit, which is
                        // the case a cache key most needs to see.
                        paths.push(reg.artifact_path(h_aid).to_path_buf());
                    }
                }
            }
        }
        paths.sort();
        paths.dedup();
        paths
    }

    /// Compile a single source file to an object file
    async fn compile_source(
        &self,
        context: &BuildContext,
        src_path: &Path,
        obj_path: &Path,
        // SOURCE-header inputs (own `hdrs` + transitive dep source headers),
        // computed ONCE per target by the caller. Added to the action inputs so
        // a header edit changes action.id and recompiles this object.
        header_inputs: &[PathBuf],
    ) -> Result<()> {
        let mut args = Vec::new();

        // Determine C vs. C++ based on source extension. Bazel picks the
        // C compiler for .c sources (so it can accept flags like
        // `-std=c11`, `-Wmissing-prototypes`) and the C++ compiler for
        // .cc/.cpp/.cxx. Rush was unconditionally invoking cxx with
        // `-std=c++17`, which made @boringssl's .c files fail with
        // "command-line option `-Wmissing-prototypes' is valid for
        // C/ObjC but not for C++" once the copts came from boringssl's
        // BUILD.
        let is_c_source = src_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("c"))
            .unwrap_or(false);

        // Add standard flags
        args.push("-c".to_string());
        args.push("-fPIC".to_string());
        if !is_c_source {
            args.push("-std=c++17".to_string());
            // Match analyze(): force-include <cstdint> for C++ TUs (gcc-13+
            // dropped the transitive include; TF/LLVM use int32_t without it).
            // Kept byte-identical with the analyze path for verifier parity.
            // Assembly (.s/.S) excluded — it cannot include the C++ header.
            if !crate::cc_command_spec::is_assembly_source_path(src_path) {
                args.push("-include".to_string());
                args.push("cstdint".to_string());
            }
            // -fno-rtti for @llvm-project consumers (matches the analyze pass +
            // CcCompileCommandSpec + CcBinaryInstance::compile_source) so the
            // non-scheduler build() also produces RTTI-consistent objects.
            if deps_want_llvm_no_rtti(&self.deps) {
                args.push("-fno-rtti".to_string());
            }
        }
        // Bazel's default feature set on Linux defines _GNU_SOURCE for both
        // C and C++ compiles, exposing pthread_rwlock_t, O_CLOEXEC, etc.
        // Several TF external deps (@boringssl, @curl, …) assume that
        // presence.
        #[cfg(target_os = "linux")]
        {
            args.push("-D_GNU_SOURCE".to_string());
            // Debian-specific include dirs the TF dep tree commonly
            // hard-codes via unqualified `#include "foo/bar.h"` instead
            // of setting -I in the dep's BUILD (which rush would have
            // plumbed through CcInfo). These are cheap and harmless
            // when missing — gcc just ignores unknown -I paths.
            for extra in [
                "/usr/include/jsoncpp",        // #include "json/json.h"
                "/usr/include/x86_64-linux-gnu", // multi-arch: curl, gif, etc.
            ] {
                if std::path::Path::new(extra).exists() {
                    args.push(format!("-I{}", extra));
                }
            }
        }

        // Honor strip_include_prefix: headers under <repo>/<prefix>/… are exposed
        // with <prefix> stripped, so the compile needs -I<repo>/<prefix>. Derive
        // the repo root from src_path (= <repo>/<package>/<rest>) by trimming the
        // "/<package>/…" suffix. "/foo" is repo-relative, "." is the package dir,
        // else package-relative. protobuf's "/src" makes google/protobuf/*.h (and
        // absl) resolve. Gated under RUSH_ENABLE_BZLMOD (certified path unchanged).
        if std::env::var("RUSH_ENABLE_BZLMOD").as_deref() == Ok("1") {
            if let Some(prefix) = &self.strip_include_prefix {
                let sp = src_path.to_string_lossy();
                let pkg = context
                    .package_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                let repo_root = if !pkg.is_empty() {
                    sp.rfind(&format!("/{}/", pkg))
                        .map(|idx| std::path::PathBuf::from(&sp[..idx]))
                } else {
                    src_path.parent().map(|p| p.to_path_buf())
                };
                if let Some(root) = repo_root {
                    let inc = if let Some(rel) = prefix.strip_prefix('/') {
                        root.join(rel)
                    } else if prefix == "." {
                        root.join(&pkg)
                    } else {
                        root.join(&pkg).join(prefix)
                    };
                    if inc.is_dir() {
                        args.push(format!("-I{}", inc.display()));
                    }
                }
            }
        }

        // Add user-specified compiler options. Filter out C++-only flags
        // when compiling C, and vice-versa, because Bazel-style BUILD
        // rules often set `copts` for the primary language of the
        // target but include a single .c helper file alongside.
        for opt in &self.copts {
            // Drop unexpanded `$(VAR)` make-variable copts (see other copts
            // loops) so a literal $(...) never reaches g++.
            if opt.contains("$(") {
                continue;
            }
            if is_c_source && opt.starts_with("-std=c++") {
                continue;
            }
            if !is_c_source && opt.starts_with("-std=c") && !opt.starts_with("-std=c++") {
                continue;
            }
            args.push(opt.clone());
        }

        // Add toolchain flags
        if !is_c_source {
            for flag in &context.toolchain.cxxflags {
                args.push(flag.clone());
            }
        } else {
            for flag in &context.toolchain.cflags {
                args.push(flag.clone());
            }
        }

        // Add defines: transitive dep defines (Bazel `defines` is
        // transitive via CcInfo) first, then own direct defines minus
        // exact dups — same merge the analyze()/PASS-B paths use.
        let dep_defines: Vec<String> = if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            let ordered_cc_infos: Vec<std::sync::Arc<crate::providers::CcInfo>> = context
                .dependency_providers_ordered
                .iter()
                .filter_map(|(_k, p)| p.cc_info.clone())
                .collect();
            crate::providers::flatten_transitive_defines(
                reg.strings(),
                ordered_cc_infos.iter().map(|cc| cc.compilation.defines.inner()),
            )
        } else {
            Vec::new()
        };
        for define in crate::providers::merged_compile_defines(&dep_defines, &self.defines) {
            args.push(format!("-D{}", define));
        }

        // Add include paths from this target
        for include in &self.includes {
            let include_path = context.source_path(include);
            args.push(format!("-I{}", include_path.display()));
        }
        // `includes` also projects onto the genfiles (bin) root, so generated
        // headers (tblgen `.inc`) under <genfiles>/<pkg>/<inc> resolve. Mirror
        // the analyze (PASS-A) genfiles_include_strs so PASS-B agrees. See the
        // @triton TritonDialects note in analyze().
        {
            let pkg = context
                .package_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let genfiles_base = context.output_dir.join("genfiles");
            for include in &self.includes {
                if std::path::Path::new(include).is_absolute() {
                    continue;
                }
                let gen_inc = if pkg.is_empty() {
                    genfiles_base.join(include)
                } else {
                    genfiles_base.join(&pkg).join(include)
                };
                let flag = format!("-I{}", gen_inc.display());
                if !args.contains(&flag) {
                    args.push(flag);
                }
            }
        }

        // Honor Bazel `include_prefix` for this library's OWN compile (the
        // legacy build()/compile_source path; the typed analyze() path stages
        // the same farm and threads it via CcInfo). A source in this library
        // can `#include "<include_prefix>/..."`, so stage the virtual-include
        // farm (symlinks) and add its root to -I. Mirrors strip_include_prefix
        // below; the two compose (strip then prefix). See stage_virtual_includes.
        if let Some(prefix) = self.include_prefix.as_deref() {
            let pkg = context
                .package_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let expanded_hdrs = expand_filegroup_srcs(&self.hdrs, |label| {
                context.get_dependency_files(label)
            });
            if let Some(vroot) = stage_virtual_includes(
                &context.workspace_root,
                &context.output_dir,
                &pkg,
                &self.name,
                &expanded_hdrs,
                prefix,
                self.strip_include_prefix.as_deref(),
            ) {
                let flag = format!("-I{}", vroot.display());
                if !args.contains(&flag) {
                    args.push(flag);
                }
            }
        }

        // Add include paths from dependencies. Iterate the whole
        // dependency_providers/dependency_artifacts maps (not just self.deps)
        // because the orchestrator already populates them with this target's
        // direct-dep providers/artifacts, and those providers already have
        // CcInfo transitively merged from their own deps. This sidesteps
        // cross-workspace `:relative` dep-string resolution issues.
        //
        // Phase 5.B.3.e: typed-only path. Every CcInfo now carries
        // a compilation context (no Option), and the orchestrator
        // always plumbs an artifact_registry handle, so the
        // typed-vs-flat fallback branching is gone.
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            for (_dep_key, providers) in &context.dependency_providers_ordered {
                if let Some(cc_info) = providers.get_cc_info() {
                    for path_id in cc_info.compilation.include_dirs.iter() {
                        let p = reg.paths().path(path_id);
                        let flag = format!("-I{}", p.display());
                        if !args.contains(&flag) {
                            args.push(flag);
                        }
                    }
                }
            }
        }
        // Intentionally do NOT add -I<parent-of-each-header-artifact> here.
        // Same reason as self.hdrs above: exported source-file labels
        // like `@local_tsl//tsl/platform:snappy.h` materialize as file
        // artifacts with the workspace-relative path as their literal
        // location. Adding their parent dirs to -I exposes
        // package-local header names (tsl/platform/snappy.h etc.) that
        // shadow system headers of the same basename. Any include path a
        // target legitimately needs comes in via CcInfo.include_dirs
        // (from `includes = [...]` attributes on cc_library rules), which
        // we already consume above.

        // Bazel's default include convention is `#include "pkg/path/file.h"`
        // resolved from the workspace root (handled below via -I<ws_root>).
        // We intentionally do NOT add -I<parent-of-each-hdr> here: that
        // would put package-local directories on the include path, which
        // shadows same-named system headers (e.g. tsl/platform/snappy.h
        // was masking /usr/include/snappy.h on port.cc's
        // `#include "snappy.h"`). Targets that rely on package-local
        // header names would need `includes = [...]` set explicitly in
        // their BUILD rule, which Bazel already requires for the same
        // reason.

        // Bazel-style include convention: every `#include "pkg/path/file.h"`
        // resolves relative to the workspace root. Add the target's workspace
        // root plus every registered external workspace root (same rationale
        // as the extra_proto_paths wiring for protoc) so cross-repo includes
        // like `#include "tsl/framework/device_type.h"` work.
        args.push(format!("-I{}", context.workspace_root.display()));
        for extra in context.extra_proto_paths.iter() {
            args.push(format!("-I{}", extra.display()));
        }
        // Generated .pb.h files from proto deps live under
        // <output_dir>/genfiles/ with the same workspace-relative layout
        // the generator saw. Put the genfiles root on the -I path so
        // `#include "tsl/protobuf/histogram.pb.h"` etc. resolve.
        let genfiles_dir = context.output_dir.join("genfiles");
        args.push(format!("-I{}", genfiles_dir.display()));

        // Demote /usr/include to -isystem (after all -I) so the pinned Abseil
        // -I wins over the host's debian7 absl (matches PASS-A/PASS-B).
        crate::cc_command_spec::demote_host_system_includes(&mut args);

        // GPU/ROCm per-source routing (legacy build() path; mirrors the scheduler
        // CppCompile assemblies). A .cu.cc / .cu is a HIP device source: add the HIP/Eigen
        // define set so the kernel's `#if GOOGLE_CUDA || TENSORFLOW_USE_ROCM` body is
        // enabled and Eigen uses hip_runtime (not cuda_runtime), and compile it with hipcc.
        // Without this the legacy host g++ compiles the .cu.cc with the GPU body #if'd out —
        // a hollow object. Other sources (.cc, generated .pb.cc) stay on the host compiler.
        // Gated by RUSH_ENABLE_ROCM, so CPU builds (and the certified 463 corpus) are unchanged.
        let rocm_enabled = std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1");
        let is_gpu_source = rocm_enabled && {
            let s = src_path.to_string_lossy();
            s.ends_with(".cu.cc") || s.ends_with(".cu")
        };
        // Host-wide ROCm header/type defines (every C++ TU under ROCm) so host StreamExecutor
        // .cc take the HIP branch; Eigen-GPU + offload-arch + hipcc stay GPU-source-only.
        if rocm_enabled {
            for f in [
                "-DTENSORFLOW_USE_ROCM=1",
                "-D__HIP_PLATFORM_AMD__",
                "-DEIGEN_USE_HIP",
                "-I/opt/rocm/include",
            ] {
                if !args.iter().any(|a| a == f) {
                    args.push(f.to_string());
                }
            }
        }
        if is_gpu_source {
            // -D_GLIBCXX_NO_ASSERTIONS: gcc-15 libstdc++ auto-enables _GLIBCXX_ASSERTIONS when not
            // __OPTIMIZE__, so std::complex emits a __host__ __glibcxx_assert_fail in device code,
            // which hipcc's device pass rejects. This is the LEGACY runtime compile path
            // (compile_source) that GPU .cu.cc kernels (cwise etc.) actually execute through. (Codex 019eebc6.)
            for f in [
                "-DEIGEN_USE_GPU",
                "-DEIGEN_USE_HIP",
                "--offload-arch=gfx1100",
                "-D_GLIBCXX_NO_ASSERTIONS",
            ] {
                if !args.iter().any(|a| a == f) {
                    args.push(f.to_string());
                }
            }
            if let Some(flag) = gpu_device_gcc_install_dir_flag() {
                if !args.iter().any(|a| *a == flag) {
                    args.push(flag);
                }
            }
        }
        let is_cuda_source = append_cuda_compile_flags(&mut args, src_path);
        if is_cuda_source {
            normalize_cuda_compile_flags(&mut args);
        }

        // Add output and input files
        args.push("-o".to_string());
        args.push(obj_path.to_string_lossy().to_string());
        args.push(src_path.to_string_lossy().to_string());

        // Debug: dump the full compile argv (RUSH_DUMP_CC_ARGV=1) to trace which
        // -I flags reach a given source (e.g. tracking down system-header shadows).
        if std::env::var("RUSH_DUMP_CC_ARGV").is_ok() {
            eprintln!(
                "RUSH_CC_ARGV src={} args={:?}",
                src_path.display(),
                args
            );
        }

        let executable = if is_cuda_source {
            cuda_device_compiler()
        } else if is_gpu_source {
            std::path::PathBuf::from("/opt/rocm/bin/hipcc")
        } else if is_c_source {
            context.toolchain.cc.clone()
        } else {
            selected_cxx_for_source(src_path).unwrap_or_else(|| context.toolchain.cxx.clone())
        };
        let command = Command {
            executable,
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };

        // Action inputs: the .cc plus the headers it may include. A header MUST
        // be an input or its content never enters action.id (the cache key), so
        // editing a header would not recompile this object — a stale build. The
        // legacy build() path (which `--daemon=off` uses) runs through this method
        // rather than the scheduler spec, so the header inputs are added HERE too,
        // mirroring analyze()'s source_header_inputs. We add the target's own
        // `hdrs` (resolved per-package) and every transitively-reachable dep
        // SOURCE header; compute_id content-hashes each. Over-inclusion is safe
        // (forces recompile, never stale); a missing path is skipped so a stale
        // resolution cannot break the compile.
        let mut inputs = vec![ActionArtifact::Source(src_path.to_path_buf())];
        for hp in header_inputs {
            if hp.exists() {
                inputs.push(ActionArtifact::Source(hp.clone()));
            }
        }

        // Compute action ID
        let action_id = Action::compute_id(&inputs, &command, &HashMap::new())?;
        let outputs = vec![ActionArtifact::Source(obj_path.to_path_buf())];

        let action = Action {
            id: action_id,
            command,
            env: HashMap::new(),
            inputs,
            outputs,
            mnemonic: "CppCompile".to_string(),
        };

        let result = context.execute_action(action).await?;

        if result.exit_code != 0 {
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(anyhow!(
                "Compilation failed for {}: {}",
                src_path.display(),
                stderr
            ));
        }

        Ok(())
    }

    /// Create a static library from object files using ar
    async fn create_static_library(
        &self,
        context: &BuildContext,
        object_files: &[PathBuf],
        lib_path: &Path,
    ) -> Result<()> {
        // `ar rcs` replaces members by name but never removes stale-named
        // ones, so a renamed object (cc_obj_key transition) leaves the OLD
        // member behind → duplicate-symbol link error. Build fresh by
        // removing any pre-existing archive first (mirrors the sync
        // create_static_library + the CcArchive executor pre-remove).
        // NOTE: the stale-member pre-remove (`ar rcs` replaces members by name
        // but never removes members absent from the current obj list) was
        // previously done HERE, before execute_action. That unconditional
        // delete defeated the action cache: RushActionManager's cache-hit gate
        // re-checks that the declared output exists on disk, and since we'd
        // just removed it, every archive re-ran on every build (255 redundant
        // `ar` invocations on a TF no-op). The pre-remove now lives in the
        // executor (LocalStrategy / LocalActionRunner), which only fires it
        // right before actually spawning `ar` — i.e. on a cache MISS (changed
        // obj set). On a cache HIT the inputs are byte-identical, so the
        // existing archive is correct and is left untouched.
        let mut args = Vec::new();
        args.push("rcs".to_string());
        args.push(lib_path.to_string_lossy().to_string());
        args.push("--thin".to_string());

        for obj in object_files {
            args.push(obj.to_string_lossy().to_string());
        }

        let command = Command {
            executable: PathBuf::from("ar"),
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };

        let inputs: Vec<ActionArtifact> = object_files
            .iter()
            .map(|p| ActionArtifact::Source(p.clone()))
            .collect();

        let action_id = Action::compute_id(&inputs, &command, &HashMap::new())?;
        let outputs = vec![ActionArtifact::Source(lib_path.to_path_buf())];

        let action = Action {
            id: action_id,
            command,
            env: HashMap::new(),
            inputs,
            outputs,
            mnemonic: "CppArchive".to_string(),
        };

        let result = context.execute_action(action).await?;

        if result.exit_code != 0 {
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(anyhow!("Archive creation failed: {}", stderr));
        }

        Ok(())
    }
}

/// cc_binary rule
pub struct CcBinaryRule;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CcBinaryLinkoptReference {
    start: usize,
    end: usize,
    kind: &'static str,
    label: String,
}

fn parse_cc_binary_linkopt_references(flag: &str) -> Result<Vec<CcBinaryLinkoptReference>> {
    let mut references = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative_start) = flag[cursor..].find("$(") {
        let start = cursor + relative_start;
        let expression_start = start + 2;
        let relative_end = flag[expression_start..].find(')').ok_or_else(|| {
            anyhow!(
                "cc_binary linkopts entry {:?} has an unterminated make variable",
                flag,
            )
        })?;
        let end = expression_start + relative_end + 1;
        let expression = &flag[expression_start..end - 1];
        let mut words = expression.split_whitespace();
        let keyword = words.next().unwrap_or_default();
        match keyword {
            "location" | "execpath" => {
                let label = words.next().ok_or_else(|| {
                    anyhow!(
                        "cc_binary linkopts entry {:?} has $({}) without a label",
                        flag,
                        keyword,
                    )
                })?;
                if words.next().is_some() {
                    return Err(anyhow!(
                        "cc_binary linkopts entry {:?} has malformed $({}); expected one label",
                        flag,
                        expression,
                    ));
                }
                references.push(CcBinaryLinkoptReference {
                    start,
                    end,
                    kind: if keyword == "location" {
                        "location"
                    } else {
                        "execpath"
                    },
                    label: label.to_string(),
                });
            }
            "locations" | "execpaths" => {
                return Err(anyhow!(
                    "cc_binary linkopts entry {:?} uses unsupported plural $({}); use a singular location or execpath reference",
                    flag,
                    expression,
                ));
            }
            malformed
                if malformed.starts_with("location:")
                    || malformed.starts_with("execpath:") =>
            {
                return Err(anyhow!(
                    "cc_binary linkopts entry {:?} has malformed $({}); expected `$({} <label>)`",
                    flag,
                    expression,
                    malformed.split_once(':').map(|(kind, _)| kind).unwrap_or(malformed),
                ));
            }
            _ => {}
        }
        cursor = end;
    }
    Ok(references)
}

/// Expand singular Bazel location macros in `cc_binary.linkopts`.
///
/// The resolver returns the exact direct files exposed by the referenced
/// prerequisite together with the caller's typed identity for each file. A
/// singular macro must resolve to exactly one direct file; transitive artifacts
/// and guessed source paths are deliberately outside this contract.
pub fn expand_cc_binary_linkopts<T, F>(
    linkopts: &[String],
    mut resolve: F,
) -> Result<(Vec<String>, Vec<T>)>
where
    F: FnMut(&str) -> Result<Vec<(PathBuf, T)>>,
{
    let mut expanded = Vec::with_capacity(linkopts.len());
    let mut referenced_inputs = Vec::new();
    for flag in linkopts {
        let references = parse_cc_binary_linkopt_references(flag)?;
        if references.is_empty() {
            expanded.push(flag.clone());
            continue;
        }

        let mut output = String::with_capacity(flag.len());
        let mut cursor = 0usize;
        for reference in references {
            output.push_str(&flag[cursor..reference.start]);
            let files = resolve(&reference.label).with_context(|| {
                format!(
                    "cc_binary linkopts cannot expand $({} {})",
                    reference.kind, reference.label,
                )
            })?;
            if files.len() != 1 {
                return Err(anyhow!(
                    "cc_binary linkopts $({} {}) resolved to {} direct files; singular references require exactly one",
                    reference.kind,
                    reference.label,
                    files.len(),
                ));
            }
            let (path, identity) = files.into_iter().next().ok_or_else(|| {
                anyhow!(
                    "cc_binary linkopts $({} {}) resolved no direct file",
                    reference.kind,
                    reference.label,
                )
            })?;
            output.push_str(&path.to_string_lossy());
            referenced_inputs.push(identity);
            cursor = reference.end;
        }
        output.push_str(&flag[cursor..]);
        expanded.push(output);
    }
    Ok((expanded, referenced_inputs))
}

fn extract_cc_binary_linkshared(attrs: &HashMap<String, Value>) -> Result<bool> {
    match attrs.get("linkshared") {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::Int(0)) => Ok(false),
        Some(Value::Int(1)) => Ok(true),
        Some(value) => Err(anyhow!(
            "cc_binary attribute 'linkshared' must be a bool or 0/1 int, got {:?}",
            value,
        )),
    }
}

#[async_trait]
impl BuildRule for CcBinaryRule {
    fn name(&self) -> &str {
        "cc_binary"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs")?;
        let deps = extract_string_list(attrs, "deps")?;
        let copts = extract_string_list(attrs, "copts")?;
        let linkopts = extract_string_list(attrs, "linkopts")?;
        for linkopt in &linkopts {
            parse_cc_binary_linkopt_references(linkopt)?;
        }
        let linkshared = extract_cc_binary_linkshared(attrs)?;
        let mut defines = extract_string_list(attrs, "defines")?;
        // `local_defines` apply to THIS target's own sources (Bazel does not propagate
        // them, unlike `defines`). rush appends them to `defines` so they reach the own
        // compile; the minor over-propagation to consumers is benign for TF/XLA's value
        // feature-flags — e.g. xla/stream_executor/cuda:ptx_compiler_support sets
        // LIBNVPTXCOMPILER_SUPPORT=false via local_defines, undefined (compile error)
        // without this.
        defines.extend(extract_string_list(attrs, "local_defines")?);
        let data = extract_string_list(attrs, "data")?;

        Ok(Box::new(CcBinaryInstance {
            header_inputs_cache: Default::default(),
            name,
            srcs,
            deps,
            copts,
            linkopts,
            linkshared,
            defines,
            data,
        }))
    }

    fn documentation(&self) -> &str {
        "C++ binary rule - compiles and links C++ sources into an executable"
    }
}

/// Instance of a cc_binary rule
pub struct CcBinaryInstance {
    header_inputs_cache: tokio::sync::Mutex<Option<CachedSinkHeaderInputs>>,
    name: String,
    srcs: Vec<String>,
    deps: Vec<String>,
    copts: Vec<String>,
    linkopts: Vec<String>,
    /// Link the target as a manually loaded shared object rather than an
    /// executable. Bazel accepts boolean and 0/1 integer attribute forms.
    linkshared: bool,
    defines: Vec<String>,
    /// `data` — runtime deps republished into runfiles + built via deps.
    data: Vec<String>,
}

const CC_BINARY_RESPONSE_FILE_THRESHOLD: usize = 120_000;
const CC_BINARY_DRIVER_STATE_HEADROOM: usize = 16 * 1024;
const CC_BINARY_UNSAFE_RESPONSE_PATH: &str =
    "cc_binary response path cannot be represented by -Wl,@file";
const CC_BINARY_UNREPRESENTABLE_RESPONSE_ARGUMENT: &str =
    "cc_binary linker argument cannot be represented in a response file";
const CC_BINARY_OPAQUE_DRIVER_ARGUMENT: &str =
    "cc_binary opaque driver argument prevents semantics-preserving spill";
const CC_BINARY_EXTERNAL_RESPONSE_FILE: &str =
    "cc_binary external response-file input is not declared";
const CC_BINARY_RESPONSE_PRODUCER_COLLISION: &str =
    "cc_binary response path collides with a stamped build output";
const CC_BINARY_RESPONSE_CONTENT_COLLISION: &str =
    "cc_binary content-addressed response input has unexpected contents";
const CC_BINARY_RESPONSE_PATH_TYPE: &str =
    "cc_binary content-addressed response input is not an immutable regular file";
const CC_BINARY_OVERSIZED_RESIDUAL: &str =
    "cc_binary residual driver argv exceeds the spill limit";
const CC_BINARY_OVERSIZED_DRIVER_STATE: &str =
    "cc_binary compiler-driver option state exceeds the spill limit";
static CC_BINARY_RESPONSE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Build the compiler-driver flag stream shared by scheduler analysis, the
/// legacy verifier, and runtime execution. `linkshared` is a driver concern:
/// keeping `-shared` outside user linkopts preserves its attribute semantics
/// and keeps it out of linker response files.
fn cc_binary_link_driver_flags(
    object_files: &[PathBuf],
    dep_flags: &[String],
    linkopts: &[String],
    ldflags: &[String],
    linkshared: bool,
) -> Vec<String> {
    let mut flags =
        fast_linker_driver_flags(object_files, dep_flags, linkopts, ldflags);
    if linkshared {
        flags.push("-shared".to_string());
    }
    flags
}

fn cc_binary_analysis_linkopt_files(
    ctx: &crate::analysis_context::AnalysisContext,
    label: &str,
) -> Result<Vec<(PathBuf, rush_action::ids::ArtifactId)>> {
    let providers = ctx
        .labeled_dep_providers()
        .iter()
        .find(|(dependency_label, _)| {
            crate::providers::dep_label_matches_repository_exact(
                dependency_label,
                label,
                ctx.package(),
                ctx.workspace(),
            )
        })
        .map(|(_, providers)| providers)
        .ok_or_else(|| {
            anyhow!(
                "label {} is not an exact declared prerequisite of this cc_binary",
                label,
            )
        })?;
    let default = providers.get_default_info().ok_or_else(|| {
        anyhow!(
            "cc_binary linkopts label {} does not expose DefaultInfo files",
            label,
        )
    })?;
    Ok(default
        .files
        .iter()
        .map(|artifact| {
            (
                ctx.registry().artifact_path(*artifact).to_path_buf(),
                *artifact,
            )
        })
        .collect())
}

fn cc_binary_runtime_linkopt_files(
    context: &BuildContext,
    label: &str,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let package = context
        .package_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let mut matched = None;
    for (dependency_label, artifacts) in &context.dependency_artifacts {
        if !crate::providers::dep_label_matches_repository_exact(
            dependency_label,
            label,
            &package,
            context.repository.as_deref(),
        ) {
            continue;
        }
        if matched.is_some() {
            return Err(anyhow!(
                "cc_binary linkopts label {} ambiguously matches multiple exact declared prerequisites",
                label,
            ));
        }
        matched = Some(artifacts);
    }
    let artifacts = matched.ok_or_else(|| {
        anyhow!(
            "label {} is not an exact declared prerequisite of this cc_binary",
            label,
        )
    })?;
    Ok(artifacts
        .direct()
        .iter()
        .map(|artifact| (artifact.path.clone(), artifact.path.clone()))
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CcBinaryResponseFile {
    path: PathBuf,
    contents: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CcBinaryLinkAtom {
    DriverKnown {
        inline: Vec<String>,
    },
    DriverBarrier {
        inline: Vec<String>,
    },
    Unsupported {
        inline: Vec<String>,
        reason: &'static str,
    },
    Linker {
        inline: Vec<String>,
        response: Vec<String>,
    },
}

impl CcBinaryLinkAtom {
    fn inline(&self) -> &[String] {
        match self {
            Self::DriverKnown { inline }
            | Self::DriverBarrier { inline }
            | Self::Unsupported { inline, .. }
            | Self::Linker { inline, .. } => inline,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CcBinaryLogicalLinkPlan {
    atoms: Vec<CcBinaryLinkAtom>,
}

/// Preserve whether a dependency argument came from a structured CcInfo
/// fragment or an arbitrary user `linkopts` string. The string alone is not
/// enough: e.g. `-lm` is a driver library option, while Clang also has paired
/// driver options beginning with `-l` such as `-lazy_framework`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CcBinaryDepArg {
    Structured(String),
    Opaque(String),
}

impl CcBinaryDepArg {
    fn structured(value: impl Into<String>) -> Self {
        Self::Structured(value.into())
    }

    fn opaque(value: impl Into<String>) -> Self {
        Self::Opaque(value.into())
    }

    fn value(&self) -> &str {
        match self {
            Self::Structured(value) | Self::Opaque(value) => value,
        }
    }

    fn is_structured(&self) -> bool {
        matches!(self, Self::Structured(_))
    }
}

fn cc_binary_dep_arg_values(args: &[CcBinaryDepArg]) -> Vec<String> {
    args.iter().map(|arg| arg.value().to_string()).collect()
}

impl CcBinaryLogicalLinkPlan {
    fn push_driver(&mut self, inline: Vec<String>) {
        self.atoms.push(CcBinaryLinkAtom::DriverKnown { inline });
    }

    fn push_barrier(&mut self, inline: Vec<String>) {
        self.atoms.push(CcBinaryLinkAtom::DriverBarrier { inline });
    }

    fn push_unsupported(&mut self, inline: Vec<String>, reason: &'static str) {
        self.atoms
            .push(CcBinaryLinkAtom::Unsupported { inline, reason });
    }

    fn push_linker(&mut self, inline: Vec<String>, response: Vec<String>) {
        self.atoms.push(CcBinaryLinkAtom::Linker { inline, response });
    }

    fn inline_argv(&self) -> Vec<String> {
        self.atoms
            .iter()
            .flat_map(|atom| atom.inline().iter().cloned())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CcBinaryLinkTransport {
    Inline {
        spawn_argv: Vec<String>,
    },
    LinkerResponse {
        spawn_argv: Vec<String>,
        response_files: Vec<CcBinaryResponseFile>,
    },
    Unsupported {
        logical_argv: Vec<String>,
        reason: &'static str,
        logical_bytes: usize,
        spawn_bytes: usize,
        largest_inline_arg: usize,
        driver_state_bytes: usize,
    },
}

fn normalized_cc_binary_link_arg(arg: &str) -> String {
    normalize_bazel_origin_link_arg(arg)
}

fn cc_binary_driver_state_component_bytes(arg: &str) -> usize {
    // GCC renders each option component in single quotes and expands one
    // apostrophe byte to the four-byte sequence '\''. Include quotes and a
    // conservative separator byte for every component.
    arg.len()
        .saturating_add(3)
        .saturating_add(
            arg.bytes()
                .filter(|byte| *byte == b'\'')
                .count()
                .saturating_mul(3),
        )
}

fn cc_binary_known_driver_state_arg_bytes(arg: &str) -> usize {
    // GCC canonicalizes joined -B and -L forms into an option component plus
    // its value. The explicit extra component matters when either is repeated.
    if let Some(value) = arg.strip_prefix("-B").filter(|_| arg.len() > 2) {
        return cc_binary_driver_state_component_bytes("-B")
            .saturating_add(cc_binary_driver_state_component_bytes(value));
    }
    if let Some(value) = arg.strip_prefix("-L").filter(|_| arg.len() > 2) {
        return cc_binary_driver_state_component_bytes("-L")
            .saturating_add(cc_binary_driver_state_component_bytes(value));
    }
    cc_binary_driver_state_component_bytes(arg)
}

fn cc_binary_opaque_driver_state_arg_bound(arg: &str) -> usize {
    // Unknown driver syntax may split or re-quote its joined value. Four bytes
    // per byte plus component framing is the fail-closed upper bound.
    arg.len().saturating_mul(4).saturating_add(3)
}

fn cc_binary_library_driver_state_bytes(inline: &[String]) -> Option<usize> {
    // GCC 15's offload forwarding may synthesize a
    // `-foffload-options=-l_GCC_<name>` COLLECT_GCC_OPTIONS component for
    // selected driver-level libraries (today m/stdc++/gfortran). The original
    // -l component itself is otherwise omitted from the collect2 environment.
    // Charge both the original spelling and a synthesized component for every
    // library so newer GCC targets remain fail-closed.
    let original = inline.iter().fold(0usize, |bytes, arg| {
        bytes.saturating_add(cc_binary_driver_state_component_bytes(arg))
    });
    Some(original.saturating_add(
        cc_binary_library_synthesized_driver_state_bytes(inline)?,
    ))
}

fn cc_binary_library_synthesized_driver_state_bytes(
    inline: &[String],
) -> Option<usize> {
    let name = match inline {
        [flag] => flag.strip_prefix("-l").filter(|name| !name.is_empty())?,
        [flag, name] if flag == "-l" => name.as_str(),
        _ => return None,
    };
    let synthesized_prefix = "-foffload-options=-l_GCC_";
    let synthesized = synthesized_prefix
        .len()
        .saturating_add(name.len())
        .saturating_add(3)
        .saturating_add(
            name.bytes()
                .filter(|byte| *byte == b'\'')
                .count()
                .saturating_mul(3),
        );
    Some(synthesized)
}

fn cc_binary_driver_state_base_bytes(binary_path: &Path) -> usize {
    let binary_path = binary_path.to_string_lossy();
    "COLLECT_GCC_OPTIONS="
        .len()
        .saturating_add(1)
        .saturating_add(CC_BINARY_DRIVER_STATE_HEADROOM)
        .saturating_add(cc_binary_driver_state_component_bytes(
            binary_path.as_ref(),
        ))
}

fn cc_binary_driver_state_bytes<'a>(
    args: impl IntoIterator<Item = &'a str>,
    binary_path: &Path,
) -> usize {
    // GCC 16 and older place the encoded option list in one environment string
    // passed to collect2. Linux limits one argv/env string to 32 pages, so keep
    // the same 120 KiB ceiling used for the spawn argv. For a spilled command,
    // count every residual spawn token, reserve 16 KiB for target defaults, and
    // count the output path again for synthesized -dumpdir. Clang does not use
    // this environment variable; the bound is harmless.
    let mut bytes = cc_binary_driver_state_base_bytes(binary_path);
    for arg in args {
        bytes = bytes.saturating_add(cc_binary_known_driver_state_arg_bytes(arg));
    }
    bytes
}

fn cc_binary_inline_driver_state_bytes(
    plan: &CcBinaryLogicalLinkPlan,
    binary_path: &Path,
) -> usize {
    let mut bytes = cc_binary_driver_state_base_bytes(binary_path);
    for atom in &plan.atoms {
        match atom {
            CcBinaryLinkAtom::DriverKnown { inline } => {
                if let Some(library_bytes) =
                    cc_binary_library_driver_state_bytes(inline)
                {
                    bytes = bytes.saturating_add(library_bytes);
                } else {
                    for arg in inline {
                        bytes = bytes.saturating_add(
                            cc_binary_known_driver_state_arg_bytes(arg),
                        );
                    }
                }
            }
            CcBinaryLinkAtom::DriverBarrier { inline } => {
                for arg in inline {
                    bytes = bytes.saturating_add(
                        cc_binary_opaque_driver_state_arg_bound(arg),
                    );
                    // A joined -l token may be either a real library switch
                    // or an opaque Clang option. For the former GCC can add a
                    // longer offload-forwarding component. Charging it here
                    // is a safe false positive for spellings such as
                    // -lifetime-* and prevents a short repeated -lm argv from
                    // overflowing collect2 before the barrier gate runs.
                    bytes = bytes.saturating_add(
                        cc_binary_library_synthesized_driver_state_bytes(
                            std::slice::from_ref(arg),
                        )
                        .unwrap_or(0),
                    );
                }
            }
            CcBinaryLinkAtom::Unsupported { .. } => {}
            CcBinaryLinkAtom::Linker { .. } => {}
        }
    }
    bytes
}

fn cc_binary_driver_option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-o"
            | "--sysroot"
            | "-B"
            | "-isysroot"
            | "--target"
            | "-target"
            | "-resource-dir"
            | "--gcc-toolchain"
            | "-gcc-toolchain"
            | "-fuse-ld"
            | "-stdlib"
            | "-rtlib"
            | "-unwindlib"
    )
}

fn cc_binary_stateful_driver_option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-x" | "-specs" | "-mllvm" | "-Xclang" | "-Xassembler" | "-Xpreprocessor"
    )
}

fn cc_binary_known_driver_flag(arg: &str) -> bool {
    matches!(
        arg,
        "-pthread"
            | "-shared"
            | "-static"
            | "-static-pie"
            | "-static-libstdc++"
            | "-static-libgcc"
            | "-shared-libgcc"
            | "-pie"
            | "-no-pie"
            | "-nostdlib"
            | "-nodefaultlibs"
            | "-nostartfiles"
            | "-nolibc"
            | "-rdynamic"
            | "-symbolic"
            | "--coverage"
            | "-v"
            | "-save-temps"
            | "-gz"
            | "-r"
            | "-s"
            | "-fPIC"
            | "-fpic"
            | "-fPIE"
            | "-fpie"
            | "-flto"
            | "-fno-lto"
            | "-m32"
            | "-m64"
            | "-no-canonical-prefixes"
            | "-Qunused-arguments"
    ) || [
        "--sysroot=",
        "--target=",
        "-target=",
        "-resource-dir=",
        "--gcc-toolchain=",
        "-gcc-toolchain=",
        "-fuse-ld=",
        "-stdlib=",
        "-rtlib=",
        "-unwindlib=",
        "-march=",
        "-mcpu=",
        "-mtune=",
        "-mabi=",
        "-flto=",
        "-save-temps=",
        "-gz=",
    ]
    .iter()
    .any(|prefix| arg.starts_with(prefix))
        || (arg.starts_with("-B") && arg.len() > 2)
}

/// Append compiler-driver link arguments whose origin is either a typed linker
/// stream (`non_options_are_linker`) or an opaque rule/toolchain flag stream.
/// Unknown syntax is a barrier: an oversized command containing one cannot be
/// spilled without changing what the compiler driver sees.
fn append_cc_binary_link_args(
    plan: &mut CcBinaryLogicalLinkPlan,
    args: &[String],
    non_options_are_linker: bool,
) {
    let normalized: Vec<String> = args
        .iter()
        .map(|arg| normalized_cc_binary_link_arg(arg))
        .collect();
    let has_external_response = normalized.iter().any(|arg| {
        arg.starts_with('@')
            || arg
                .strip_prefix("-Wl,")
                .map_or(false, |rest| rest.split(',').any(|field| field.starts_with('@')))
            || arg
                .strip_prefix("--for-linker=")
                .map_or(false, |value| value.starts_with('@'))
    });
    if has_external_response {
        plan.push_unsupported(normalized, CC_BINARY_EXTERNAL_RESPONSE_FILE);
        return;
    }
    let mut index = 0usize;
    while index < normalized.len() {
        let arg = &normalized[index];
        if let Some(rest) = arg.strip_prefix("-Wl,") {
            plan.push_linker(
                vec![arg.clone()],
                rest.split(',').map(str::to_string).collect(),
            );
            index += 1;
            continue;
        }
        if matches!(arg.as_str(), "-Xlinker" | "--for-linker") {
            if let Some(value) = normalized.get(index + 1) {
                let inline = vec![arg.clone(), value.clone()];
                if value.starts_with('@') {
                    plan.push_barrier(inline);
                } else {
                    plan.push_linker(inline, vec![value.clone()]);
                }
                index += 2;
            } else {
                plan.push_barrier(vec![arg.clone()]);
                index += 1;
            }
            continue;
        }
        if let Some(value) = arg.strip_prefix("--for-linker=") {
            plan.push_linker(vec![arg.clone()], vec![value.to_string()]);
            index += 1;
            continue;
        }
        if arg == "-l" || arg == "-L" {
            if let Some(value) = normalized.get(index + 1) {
                let inline = vec![arg.clone(), value.clone()];
                if value.starts_with('@') {
                    plan.push_barrier(inline);
                } else {
                    plan.push_driver(inline);
                }
                index += 2;
            } else {
                plan.push_barrier(vec![arg.clone()]);
                index += 1;
            }
            continue;
        }
        // Raw -l/-L spellings are compiler-driver options, even when their
        // values ultimately reach ld. Moving them behind -Wl,@ changes what
        // GCC records and may change library search/order. Joined `-lfoo` is
        // only unambiguous in the typed dependency stream: opaque streams can
        // contain Clang options such as -lazy_framework and
        // -lifetime-safety-*.
        if (arg.starts_with("-L") || (non_options_are_linker && arg.starts_with("-l")))
            && arg.len() > 2
        {
            plan.push_driver(vec![arg.clone()]);
            index += 1;
            continue;
        }
        if matches!(arg.as_str(), "-framework" | "-weak_framework" | "-F") {
            if let Some(value) = normalized.get(index + 1) {
                let inline = vec![arg.clone(), value.clone()];
                if value.starts_with('@') {
                    plan.push_barrier(inline);
                } else {
                    plan.push_driver(inline);
                }
                index += 2;
            } else {
                plan.push_barrier(vec![arg.clone()]);
                index += 1;
            }
            continue;
        }
        if arg.starts_with("-F") && arg.len() > 2 {
            plan.push_driver(vec![arg.clone()]);
            index += 1;
            continue;
        }
        if cc_binary_driver_option_takes_value(arg) {
            if let Some(value) = normalized.get(index + 1) {
                let inline = vec![arg.clone(), value.clone()];
                if value.starts_with('@') {
                    plan.push_barrier(inline);
                } else {
                    plan.push_driver(inline);
                }
                index += 2;
            } else {
                plan.push_barrier(vec![arg.clone()]);
                index += 1;
            }
            continue;
        }
        if cc_binary_stateful_driver_option_takes_value(arg) {
            let mut barrier = vec![arg.clone()];
            if let Some(value) = normalized.get(index + 1) {
                barrier.push(value.clone());
                index += 2;
            } else {
                index += 1;
            }
            plan.push_barrier(barrier);
            continue;
        }
        if cc_binary_known_driver_flag(arg) {
            plan.push_driver(vec![arg.clone()]);
            index += 1;
            continue;
        }
        if non_options_are_linker && !arg.starts_with('-') && !arg.starts_with('@') {
            plan.push_linker(vec![arg.clone()], vec![arg.clone()]);
            index += 1;
            continue;
        }

        plan.push_barrier(normalized[index..].to_vec());
        break;
    }
}

fn append_cc_binary_dep_args(
    plan: &mut CcBinaryLogicalLinkPlan,
    dep_args: &[CcBinaryDepArg],
) {
    let mut index = 0usize;
    while index < dep_args.len() {
        let structured = dep_args[index].is_structured();
        let end = dep_args[index..]
            .iter()
            .position(|arg| arg.is_structured() != structured)
            .map_or(dep_args.len(), |offset| index + offset);
        let run: Vec<String> = dep_args[index..end]
            .iter()
            .map(|arg| arg.value().to_string())
            .collect();
        append_cc_binary_link_args(plan, &run, structured);
        index = end;
    }
}

fn cc_binary_logical_link_plan_with_dep_args(
    object_paths: &[String],
    driver_flags: &[String],
    dep_args: &[CcBinaryDepArg],
    linkopts: &[String],
    ldflags: &[String],
    extra_linkopts: &[String],
    binary_path: &str,
) -> CcBinaryLogicalLinkPlan {
    let mut plan = CcBinaryLogicalLinkPlan::default();
    for object in object_paths {
        let object = normalized_cc_binary_link_arg(object);
        plan.push_linker(vec![object.clone()], vec![object]);
    }
    // Fast-linker selection can also contribute -Wl tuning flags. Parse this
    // stream instead of blessing every token as driver-only so an environment
    // override cannot smuggle an undeclared nested @response file past the
    // ordinary link-option checks.
    append_cc_binary_link_args(&mut plan, driver_flags, false);
    append_cc_binary_dep_args(&mut plan, dep_args);
    append_cc_binary_link_args(&mut plan, linkopts, false);
    append_cc_binary_link_args(&mut plan, ldflags, false);
    // Runtime constructs this stream itself (currently ROCm -L/-Wl/-l
    // tokens), so non-option paths have linker provenance. Raw compiler-driver
    // switches still remain driver-side; only explicit -Wl/-Xlinker forms
    // spill.
    append_cc_binary_link_args(&mut plan, extra_linkopts, true);
    plan.push_driver(vec![
        "-o".to_string(),
        normalized_cc_binary_link_arg(binary_path),
    ]);
    plan
}

#[cfg(test)]
fn cc_binary_logical_link_plan(
    object_paths: &[String],
    driver_flags: &[String],
    dep_flags: &[String],
    linkopts: &[String],
    ldflags: &[String],
    extra_linkopts: &[String],
    binary_path: &str,
) -> CcBinaryLogicalLinkPlan {
    let dep_args: Vec<CcBinaryDepArg> = dep_flags
        .iter()
        .cloned()
        .map(CcBinaryDepArg::structured)
        .collect();
    cc_binary_logical_link_plan_with_dep_args(
        object_paths,
        driver_flags,
        &dep_args,
        linkopts,
        ldflags,
        extra_linkopts,
        binary_path,
    )
}

fn cc_binary_response_root(workspace_root: &Path) -> PathBuf {
    // Target outputs live below BuildContext.output_dir. Keep generated action
    // inputs in Rush's reserved workspace cache instead, so no Bazel output
    // label can alias and clobber them.
    workspace_root
        .join(".rush-cache")
        .join("action-inputs")
        .join("cc-binary")
}

fn cc_binary_response_path(response_root: &Path, contents: &str) -> PathBuf {
    response_root.join(format!(
        "{}.rsp",
        blake3::hash(contents.as_bytes()).to_hex()
    ))
}

fn cc_binary_response_stamp_path(response_path: &Path) -> PathBuf {
    let mut stamp_path = response_path.as_os_str().to_os_string();
    stamp_path.push(".rushid");
    PathBuf::from(stamp_path)
}

fn reject_stamped_cc_binary_response_paths(
    response_files: &[CcBinaryResponseFile],
) -> Result<()> {
    for response_file in response_files {
        let stamp_path = cc_binary_response_stamp_path(&response_file.path);
        match std::fs::read(&stamp_path) {
            Ok(bytes)
                if rush_action::artifact_value::ArtifactFingerprint::parse_stamp(&bytes)
                    .is_some() =>
            {
                return Err(anyhow!(
                    "{}: {} has producer stamp {}",
                    CC_BINARY_RESPONSE_PRODUCER_COLLISION,
                    response_file.path.display(),
                    stamp_path.display(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(anyhow!(
                    "cannot inspect cc_binary response producer stamp {}: {}",
                    stamp_path.display(),
                    error,
                ));
            }
        }
    }
    Ok(())
}

fn verify_cc_binary_response_contents(
    response_file: &CcBinaryResponseFile,
    bytes: &[u8],
) -> Result<()> {
    if bytes == response_file.contents.as_bytes() {
        return Ok(());
    }
    Err(anyhow!(
        "{}: {}",
        CC_BINARY_RESPONSE_CONTENT_COLLISION,
        response_file.path.display(),
    ))
}

async fn read_existing_cc_binary_response_file(
    response_file: &CcBinaryResponseFile,
) -> Result<Option<Vec<u8>>> {
    let metadata = match tokio::fs::symlink_metadata(&response_file.path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || !metadata.permissions().readonly()
    {
        return Err(anyhow!(
            "{}: {}",
            CC_BINARY_RESPONSE_PATH_TYPE,
            response_file.path.display(),
        ));
    }
    let bytes = tokio::fs::read(&response_file.path).await?;
    // Recheck after the pathname-based open/read. This rejects a symlink or
    // non-regular replacement during reuse without ever chmod'ing whatever
    // object the name happened to reference.
    let metadata = tokio::fs::symlink_metadata(&response_file.path).await?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || !metadata.permissions().readonly()
    {
        return Err(anyhow!(
            "{}: {}",
            CC_BINARY_RESPONSE_PATH_TYPE,
            response_file.path.display(),
        ));
    }
    verify_cc_binary_response_contents(response_file, &bytes)?;
    Ok(Some(bytes))
}

async fn materialize_cc_binary_response_file(
    response_file: &CcBinaryResponseFile,
) -> Result<()> {
    if let Some(parent) = response_file.path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if read_existing_cc_binary_response_file(response_file)
        .await?
        .is_some()
    {
        return Ok(());
    }

    // Publish a complete immutable inode in one operation. create_new keeps
    // temporary writers distinct; hard_link is the no-replace publication
    // primitive, so concurrent same-body writers either win or verify the
    // already-published blob without exposing a partially written final file.
    let file_name = response_file
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!(CC_BINARY_UNSAFE_RESPONSE_PATH))?;
    let (mut temp, temp_path) = loop {
        let sequence =
            CC_BINARY_RESPONSE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = response_file.path.with_file_name(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            sequence,
        ));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .await
        {
            Ok(temp) => break (temp, temp_path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    };
    if let Err(error) = temp.write_all(response_file.contents.as_bytes()).await {
        drop(temp);
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(error.into());
    }
    if let Err(error) = temp.sync_all().await {
        drop(temp);
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(error.into());
    }
    let mut permissions = temp.metadata().await?.permissions();
    permissions.set_readonly(true);
    temp.set_permissions(permissions).await?;
    drop(temp);

    let publish = std::fs::hard_link(&temp_path, &response_file.path);
    let _ = tokio::fs::remove_file(&temp_path).await;
    match publish {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match read_existing_cc_binary_response_file(response_file).await? {
                Some(_) => Ok(()),
                None => Err(anyhow!(
                    "{}: {} disappeared during publication",
                    CC_BINARY_RESPONSE_PATH_TYPE,
                    response_file.path.display(),
                )),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn flush_cc_binary_linker_span(
    response_root: &Path,
    response_tokens: &mut Vec<String>,
    spawn_argv: &mut Vec<String>,
    response_files: &mut Vec<CcBinaryResponseFile>,
) {
    if response_tokens.is_empty() {
        return;
    }
    let mut contents = String::new();
    for arg in response_tokens.drain(..) {
        contents.push_str(&quote_response_file_arg(&arg));
        contents.push('\n');
    }
    let path = cc_binary_response_path(response_root, &contents);
    spawn_argv.push(format!("-Wl,@{}", path.display()));
    response_files.push(CcBinaryResponseFile { path, contents });
}

/// Choose the transport for a fully assembled logical `cc_binary` link argv.
///
/// Large links replace each contiguous, provenance-known linker span in place
/// with an ld response file. Driver arguments retain their exact order and
/// multiplicity. Any opaque driver barrier disables spilling for the entire
/// command because specs and response-expanded driver arguments can inspect
/// or rewrite inputs that appeared before the barrier. Both the actual spawn
/// and GCC's re-encoded COLLECT_GCC_OPTIONS state are bounded.
fn cc_binary_link_transport_in(
    plan: &CcBinaryLogicalLinkPlan,
    binary_path: &Path,
    response_root: &Path,
) -> CcBinaryLinkTransport {
    let logical_argv = plan.inline_argv();
    let argv_bytes: usize = logical_argv.iter().map(|arg| arg.len() + 1).sum();
    let largest_logical_arg = logical_argv
        .iter()
        .map(|arg| arg.len() + 1)
        .max()
        .unwrap_or(0);
    let inline_driver_state_bytes =
        cc_binary_inline_driver_state_bytes(plan, binary_path);
    if let Some(reason) = plan.atoms.iter().find_map(|atom| match atom {
        CcBinaryLinkAtom::Unsupported { reason, .. } => Some(*reason),
        _ => None,
    }) {
        return CcBinaryLinkTransport::Unsupported {
            logical_argv,
            reason,
            logical_bytes: argv_bytes,
            spawn_bytes: argv_bytes,
            largest_inline_arg: largest_logical_arg,
            driver_state_bytes: inline_driver_state_bytes,
        };
    }
    if argv_bytes <= CC_BINARY_RESPONSE_FILE_THRESHOLD
        && inline_driver_state_bytes <= CC_BINARY_RESPONSE_FILE_THRESHOLD
    {
        return CcBinaryLinkTransport::Inline {
            spawn_argv: logical_argv,
        };
    }

    if plan
        .atoms
        .iter()
        .any(|atom| matches!(atom, CcBinaryLinkAtom::DriverBarrier { .. }))
    {
        return CcBinaryLinkTransport::Unsupported {
            logical_argv,
            reason: CC_BINARY_OPAQUE_DRIVER_ARGUMENT,
            logical_bytes: argv_bytes,
            spawn_bytes: argv_bytes,
            largest_inline_arg: largest_logical_arg,
            driver_state_bytes: inline_driver_state_bytes,
        };
    }

    // GNU libiberty's response parser is C-string based. A NUL carried in a
    // Rust/Starlark String would truncate the response body and could silently
    // drop the rest of this linker span. Inline exec rejects the same token,
    // but once spilling is required we must fail before materialization.
    if plan.atoms.iter().any(|atom| match atom {
        CcBinaryLinkAtom::Linker { response, .. } => {
            response.iter().any(|arg| arg.contains('\0'))
        }
        _ => false,
    }) {
        return CcBinaryLinkTransport::Unsupported {
            logical_argv,
            reason: CC_BINARY_UNREPRESENTABLE_RESPONSE_ARGUMENT,
            logical_bytes: argv_bytes,
            spawn_bytes: argv_bytes,
            largest_inline_arg: largest_logical_arg,
            driver_state_bytes: inline_driver_state_bytes,
        };
    }

    if response_root.to_str().is_none()
        || response_root.to_string_lossy().contains(',')
    {
        return CcBinaryLinkTransport::Unsupported {
            logical_argv,
            reason: CC_BINARY_UNSAFE_RESPONSE_PATH,
            logical_bytes: argv_bytes,
            spawn_bytes: argv_bytes,
            largest_inline_arg: largest_logical_arg,
            driver_state_bytes: inline_driver_state_bytes,
        };
    }

    let mut spawn_argv = Vec::new();
    let mut response_files = Vec::new();
    let mut response_tokens = Vec::new();
    for atom in &plan.atoms {
        match atom {
            CcBinaryLinkAtom::DriverKnown { inline } => {
                flush_cc_binary_linker_span(
                    response_root,
                    &mut response_tokens,
                    &mut spawn_argv,
                    &mut response_files,
                );
                spawn_argv.extend(inline.iter().cloned());
            }
            CcBinaryLinkAtom::DriverBarrier { inline } => {
                debug_assert!(false, "driver barriers are rejected before spilling");
                spawn_argv.extend(inline.iter().cloned());
            }
            CcBinaryLinkAtom::Unsupported { inline, .. } => {
                debug_assert!(false, "unsupported atoms are rejected before spilling");
                spawn_argv.extend(inline.iter().cloned());
            }
            CcBinaryLinkAtom::Linker { response, .. } => {
                response_tokens.extend(response.iter().cloned());
            }
        }
    }
    flush_cc_binary_linker_span(
        response_root,
        &mut response_tokens,
        &mut spawn_argv,
        &mut response_files,
    );

    let spawn_bytes: usize = spawn_argv.iter().map(|arg| arg.len() + 1).sum();
    let largest_inline_arg = spawn_argv
        .iter()
        .map(|arg| arg.len() + 1)
        .max()
        .unwrap_or(0);
    let hidden_library_state_bytes = plan.atoms.iter().fold(0usize, |bytes, atom| {
        let CcBinaryLinkAtom::DriverKnown { inline } = atom else {
            return bytes;
        };
        bytes.saturating_add(
            cc_binary_library_synthesized_driver_state_bytes(inline)
                .unwrap_or(0),
        )
    });
    let driver_state_bytes = cc_binary_driver_state_bytes(
        spawn_argv.iter().map(String::as_str),
        binary_path,
    )
    .saturating_add(hidden_library_state_bytes);
    if response_files.is_empty()
        || spawn_bytes > CC_BINARY_RESPONSE_FILE_THRESHOLD
        || largest_inline_arg > CC_BINARY_RESPONSE_FILE_THRESHOLD
    {
        return CcBinaryLinkTransport::Unsupported {
            logical_argv,
            reason: CC_BINARY_OVERSIZED_RESIDUAL,
            logical_bytes: argv_bytes,
            spawn_bytes,
            largest_inline_arg,
            driver_state_bytes,
        };
    }
    if driver_state_bytes > CC_BINARY_RESPONSE_FILE_THRESHOLD {
        return CcBinaryLinkTransport::Unsupported {
            logical_argv,
            reason: CC_BINARY_OVERSIZED_DRIVER_STATE,
            logical_bytes: argv_bytes,
            spawn_bytes,
            largest_inline_arg,
            driver_state_bytes,
        };
    }

    CcBinaryLinkTransport::LinkerResponse {
        spawn_argv,
        response_files,
    }
}

#[cfg(test)]
fn cc_binary_link_transport(
    plan: &CcBinaryLogicalLinkPlan,
    binary_path: &Path,
) -> CcBinaryLinkTransport {
    cc_binary_link_transport_in(
        plan,
        binary_path,
        Path::new(".rush-cache/action-inputs/cc-binary"),
    )
}

#[async_trait]
impl RuleInstance for CcBinaryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        let mut out = deps_with_src_labels(&self.deps, &self.srcs);
        // A bare header in `srcs` is also a same-package file label. Adding
        // the implicit edge lets the graph resolve a clean-tree generated
        // output to its producer; checked-in headers continue through the
        // existing source-file fallback.
        for src in &self.srcs {
            if !Path::new(src).is_absolute()
                && !is_label_src(src)
                && is_cc_header_input(src)
            {
                let implicit = format!(":{src}");
                if !out.contains(&implicit) {
                    out.push(implicit);
                }
            }
        }
        append_data_labels(&mut out, &self.data);
        out
    }

    fn validate_analysis_dependencies(
        &self,
        ctx: &crate::analysis_context::AnalysisContext,
    ) -> Result<()> {
        let _ = expand_cc_binary_linkopts(&self.linkopts, |label| {
            cc_binary_analysis_linkopt_files(ctx, label)
        })?;
        Ok(())
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let mut artifacts = Vec::new();

        // Create output directory for this target
        let target_output_dir = context.output_dir.join(format!("{}_objs", self.name));
        std::fs::create_dir_all(&target_output_dir)?;

        // Compile each source file to object file.
        let mut object_files = Vec::new();
        // Sink compiles match analyze()/PASS-B for generated headers, while
        // keeping dependency source headers only when this source's textual
        // include graph can actually reach them. That preserves correctness for
        // a binary source that includes a dep header without making unrelated
        // private header edits poison the root object cache key.
        let expanded_srcs =
            expand_cc_binary_srcs(&self.srcs, |l| context.get_dependency_files(l));
        let package = context
            .package_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let owned_header_paths = cc_binary_owned_header_paths(
            &context.workspace_root,
            &package,
            &expanded_srcs,
        );
        let owned_header_quote_dirs = cc_binary_owned_header_quote_dirs(
            &context.workspace_root,
            &context.output_dir,
            &package,
            &owned_header_paths,
        );
        let header_inputs = cached_sink_compile_header_inputs(context, &self.header_inputs_cache).await;
        for src in &expanded_srcs {
            // Skip non-compilable srcs (headers in srcs) — see the PCH-clobber
            // note above; compiling a .h emits a precompiled header that
            // collides with the same-stem .cc object.
            if !crate::cc_command_spec::is_compilable_source(src) {
                continue;
            }
            let src_path = context.source_path(src);

            // Determine object file name
            let obj_name = Path::new(src)
                .file_stem()
                .ok_or_else(|| anyhow!("Invalid source file: {}", src))?
                .to_string_lossy()
                .to_string();
            let obj_path = target_output_dir.join(format!("{}.o", obj_name));

            // Compile the source file
            let mut compile_header_inputs = header_inputs.generated.clone();
            compile_header_inputs.extend(owned_header_paths.iter().cloned());
            compile_header_inputs
                .extend(reachable_source_headers_for_sink_source(&src_path, &header_inputs));
            compile_header_inputs.sort();
            compile_header_inputs.dedup();
            if header_inputs.generated.is_empty()
                && owned_header_paths.is_empty()
                && !context.dirty_paths().is_empty()
                && obj_path.exists()
                && !context.path_is_dirty(&src_path)
                && compile_header_inputs
                    .iter()
                    .all(|p| !context.path_is_dirty(p))
            {
                object_files.push(obj_path);
                continue;
            }
            self.compile_source(
                context,
                &src_path,
                &obj_path,
                &compile_header_inputs,
                &owned_header_quote_dirs,
            )
            .await?;

            object_files.push(obj_path);
        }

        // Link object files into the target's public output.
        let binary_path = context.output_dir.join(&self.name);
        self.link_binary(context, &object_files, &binary_path)
            .await?;

        artifacts.push(Artifact {
            path: binary_path,
            artifact_type: if self.linkshared {
                ArtifactType::SharedLibrary
            } else {
                ArtifactType::Executable
            },
        });

        Ok(artifacts)
    }

    /// Phase 5.C0: declare CppCompile per source + CcLink for the
    /// final output. A `linkshared` binary publishes a SharedLibrary
    /// through DefaultInfo, but deliberately does not expose CcInfo:
    /// Bazel treats this form as manually loaded and never links it
    /// into dependent binaries.
    ///
    /// Cross-target inputs: the CcLink action declares each dep
    /// `LinkerInputId`'s `artifact` ArtifactId as an input.
    /// tf_cc_library exposes its .a via linker_inputs (and
    /// declares a CcArchive action that produces it), so pass 3
    /// (every generated input has a producer) chains correctly.
    /// cc_library doesn't currently expose its .a in
    /// linker_inputs (only via the artifact NestedSet), so its
    /// .a is invisible to this declaration — that's a known
    /// limitation tracked for a future commit.
    fn analyze(
        &self,
        ctx: &mut crate::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        use rush_action::ids::ArtifactKind;
        use std::path::Path;
        if !self.srcs.is_empty() {
            // Phase 5.C0 P0: include generated headers from dep
            // CcCompilationContexts in CppCompile inputs.
            let dep_cc_infos = crate::providers::extract_cc_infos(dep_providers);
            let generated_header_inputs =
                crate::providers::collect_generated_header_inputs(
                    ctx.registry(),
                    &dep_cc_infos,
                );
            let expanded_srcs = expand_cc_binary_srcs(&self.srcs, |label| {
                ctx.get_dependency_provider(label)
                    .and_then(|providers| providers.get_files())
                    .map(|files| files.files.clone())
            });
            let package = ctx.package().to_string();
            let owned_header_paths = cc_binary_owned_header_paths(
                ctx.workspace_root(),
                &package,
                &expanded_srcs,
            );
            // Path-keyed interning reuses an existing generated artifact. In
            // particular, a FilesInfo path forwarded by a generated-output
            // alias retains the genrule's ArtifactId and producer instead of
            // being reclassified as an unrelated source header.
            let mut owned_header_inputs: Vec<_> = owned_header_paths
                .iter()
                .map(|path| ctx.register_source_artifact(path, ArtifactKind::Header))
                .collect();
            owned_header_inputs.sort_by(|left, right| {
                ctx.registry()
                    .artifact_path(*left)
                    .cmp(ctx.registry().artifact_path(*right))
            });
            owned_header_inputs.dedup();
            let mut compile_header_inputs = generated_header_inputs;
            compile_header_inputs.extend(owned_header_inputs);
            compile_header_inputs.sort_by(|left, right| {
                ctx.registry()
                    .artifact_path(*left)
                    .cmp(ctx.registry().artifact_path(*right))
            });
            compile_header_inputs.dedup();
            let owned_header_quote_paths: Vec<String> =
                cc_binary_owned_header_quote_dirs(
                    ctx.workspace_root(),
                    ctx.output_dir(),
                    &package,
                    &owned_header_paths,
                )
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            // Phase 5.C.4 step 2b cc_binary CppCompile migration.
            // Same borrow-collect pattern as cc_library
            // (48e1b78): immutable reads (dep paths, toolchain
            // values, workspace_root, extras) before the per-src
            // loop, then intern + set_executable_action per src.
            // cc_binary has no `includes` attribute, so no
            // direct_include_paths — that's the one shape
            // difference from cc_library's CppCompile.
            let dep_include_paths: Vec<String> = dep_cc_infos
                .iter()
                .flat_map(|cc| {
                    cc.compilation
                        .include_dirs
                        .iter()
                        .map(|pid| ctx.registry().paths().path(pid).display().to_string())
                        .collect::<Vec<_>>()
                })
                .collect();
            // Transitive dep `defines` (Bazel postorder) for cc_binary's
            // own compile — same flatten as cc_library/cc_test.
            let dep_defines: Vec<String> = crate::providers::flatten_transitive_defines(
                ctx.registry().strings(),
                dep_cc_infos.iter().map(|cc| cc.compilation.defines.inner()),
            );
            let workspace_root_str = ctx.workspace_root().display().to_string();
            let extra_proto_strs: Vec<String> = ctx
                .extra_proto_paths()
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            let genfiles_str = ctx
                .predicted_output_path(Path::new("genfiles"))
                .display()
                .to_string();
            let cxxflags: Vec<String> = ctx.toolchain().cxxflags.clone();
            let cflags: Vec<String> = ctx.toolchain().cflags.clone();
            let cxx_pathbuf = ctx.toolchain().cxx.clone();
            let cc_pathbuf = ctx.toolchain().cc.clone();

            let mut obj_artifacts: Vec<_> = Vec::with_capacity(self.srcs.len());
            for src in &expanded_srcs {
                // Skip headers in srcs (PCH-clobber; see build path). Don't
                // declare a CppCompile for a non-TU.
                if !crate::cc_command_spec::is_compilable_source(src) {
                    continue;
                }
                // Register the source at its package-qualified absolute path so a
                // same-basename src in another package (e.g. each package's own
                // `main.cc`) gets a DISTINCT artifact whose freshness ledger entry
                // and action-id content-hash track the real file — see the
                // same-basename incremental-correctness fix in cc_library::analyze.
                // The compile argv uses its own `src_path_abs` below, so parity is
                // unchanged.
                let pkg_for_src = ctx.package().to_string();
                let src_abs_for_reg = if pkg_for_src.is_empty() {
                    ctx.workspace_root().join(src)
                } else {
                    ctx.workspace_root().join(&pkg_for_src).join(src)
                };
                let src_aid =
                    ctx.register_source_artifact(&src_abs_for_reg, ArtifactKind::Source);
                let obj_name = format!(
                    "{}_objs/{}.o",
                    self.name,
                    Path::new(src)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("obj"),
                );
                let obj_aid = ctx.register_generated_artifact(
                    Path::new(&obj_name),
                    ArtifactKind::Object,
                    None,
                );
                obj_artifacts.push(obj_aid);
                let mut compile_inputs = vec![src_aid];
                compile_inputs.extend(
                    compile_header_inputs
                        .iter()
                        .copied()
                        .filter(|input| *input != src_aid),
                );
                // Mnemonic must match cc_binary's build_legacy_command_specs
                // (PASS-B), which uses "CCompile" for .c sources — same
                // C-vs-C++ key-alignment as the cc_library boringssl fix.
                let is_c_source = Path::new(src)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("c"))
                    .unwrap_or(false);
                let compile_mnemonic =
                    if is_c_source { "CCompile" } else { "CppCompile" };
                if let Ok(compile_id) =
                    ctx.declare_action(compile_mnemonic, compile_inputs, vec![obj_aid])
                {

                    let pkg = ctx.package();
                    let src_path_abs = if pkg.is_empty() {
                        ctx.workspace_root().join(src)
                    } else {
                        ctx.workspace_root().join(pkg).join(src)
                    };
                    let src_path_str = src_path_abs.display().to_string();
                    let obj_path_str = ctx
                        .registry()
                        .artifact_path(obj_aid)
                        .display()
                        .to_string();

                    let mut argv_strs: Vec<String> = Vec::new();
                    argv_strs.push("-c".to_string());
                    argv_strs.push("-fPIC".to_string());
                    if !is_c_source {
                        argv_strs.push("-std=c++17".to_string());
                        // gcc-13+/libstdc++ no longer transitively include
                        // <cstdint>, so TF/LLVM/MLIR headers using int32_t etc.
                        // without including it (e.g. stablehlo math_utils.h)
                        // fail "'int32_t' has not been declared". Force-include
                        // it for C++ TUs (toolchain workaround, NOT a TF source
                        // patch). C sources keep their own headers, and
                        // assembly (.s/.S) is excluded — it cannot include the
                        // C++ header (e.g. @boringssl//:crypto *-apple.S).
                        if !crate::cc_command_spec::is_assembly_source_path(
                            Path::new(src),
                        ) {
                            argv_strs.push("-include".to_string());
                            argv_strs.push("cstdint".to_string());
                        }
                        // -fno-rtti for @llvm-project consumers, placed right
                        // after the cstdint block (inside !is_c_source) so this
                        // analyze argv (PASS-A) stays byte-identical to the
                        // CcCompileCommandSpec (PASS-B). Matches LLVM's
                        // -fno-rtti build (converter-gen typeinfo link).
                        if deps_want_llvm_no_rtti(&self.deps) {
                            argv_strs.push("-fno-rtti".to_string());
                        }
                    }
                    #[cfg(target_os = "linux")]
                    {
                        argv_strs.push("-D_GNU_SOURCE".to_string());
                        for extra in [
                            "/usr/include/jsoncpp",
                            "/usr/include/x86_64-linux-gnu",
                        ] {
                            if std::path::Path::new(extra).exists() {
                                argv_strs.push(format!("-I{}", extra));
                            }
                        }
                    }
                    for opt in &self.copts {
                        // Drop unexpanded `$(VAR)` make-variable copts (e.g.
                        // TF's `$(STACK_FRAME_UNLIMITED)`) — rush doesn't model
                        // make-vars, and a literal $(...) reaches g++ as a
                        // missing input file. Empty expansion is the safe
                        // degradation (these are warning/frame-size controls).
                        if opt.contains("$(") {
                            continue;
                        }
                        if is_c_source && opt.starts_with("-std=c++") {
                            continue;
                        }
                        if !is_c_source
                            && opt.starts_with("-std=c")
                            && !opt.starts_with("-std=c++")
                        {
                            continue;
                        }
                        argv_strs.push(opt.clone());
                    }
                    // GPU/ROCm define routing. Two scopes under RUSH_ENABLE_ROCM:
                    //  (1) HOST-WIDE header/type selection: every C++ TU gets
                    //      -DTENSORFLOW_USE_ROCM=1 + -D__HIP_PLATFORM_AMD__ so host
                    //      StreamExecutor .cc (gpu_types.h etc.) take the HIP branch instead of
                    //      the CUDA #else (cuComplex.h). Probe-verified: host g++ compiles the HIP
                    //      branch. This mirrors TF's tf_copts/if_rocm config-wide define.
                    //  (2) GPU-SOURCE-ONLY: a .cu.cc/.cu additionally gets the Eigen-GPU defines +
                    //      --offload-arch and is compiled by hipcc (see the exe selection below).
                    //      Eigen-GPU on a plain host g++ TU would break it, so it stays per-source.
                    let rocm_enabled =
                        std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1");
                    let is_gpu_source = rocm_enabled
                        && (src.ends_with(".cu.cc") || src.ends_with(".cu"));
                    if rocm_enabled {
                        // -I/opt/rocm/include so host .cc that take the HIP branch resolve the
                        // internal hip/ header tree (hip_complex.h -> hip/hip_common.h ...).
                        for f in [
                            "-DTENSORFLOW_USE_ROCM=1",
                            "-D__HIP_PLATFORM_AMD__",
                            "-DEIGEN_USE_HIP",
                            "-I/opt/rocm/include",
                        ] {
                            if !argv_strs.iter().any(|a| a == f) {
                                argv_strs.push(f.to_string());
                            }
                        }
                    }
                    if is_gpu_source {
                        for f in [
                    "-DEIGEN_USE_GPU",
                    "-DEIGEN_USE_HIP",
                    "--offload-arch=gfx1100",
                    // gcc-15 libstdc++ auto-enables _GLIBCXX_ASSERTIONS when not __OPTIMIZE__
                    // (c++config.h), so std::complex's __glibcxx_assert calls __host__
                    // __glibcxx_assert_fail — which hipcc's device pass rejects from a
                    // __host__ __device__ fn. Disable libstdc++ assertions for device code so
                    // std::complex GPU kernels (cwise etc.) compile. (Codex 019eebc6.)
                    "-D_GLIBCXX_NO_ASSERTIONS",
                ] {
                            if !argv_strs.iter().any(|a| a == f) {
                                argv_strs.push(f.to_string());
                            }
                        }
                        if let Some(flag) = gpu_device_gcc_install_dir_flag() {
                            if !argv_strs.iter().any(|a| *a == flag) {
                                argv_strs.push(flag);
                            }
                        }
                    }
                    let is_cuda_source =
                        append_cuda_compile_flags(&mut argv_strs, Path::new(src));
                    if is_cuda_source {
                        normalize_cuda_compile_flags(&mut argv_strs);
                    }
                    if !is_c_source {
                        argv_strs.extend(cxxflags.iter().cloned());
                    } else {
                        argv_strs.extend(cflags.iter().cloned());
                    }
                    for d in crate::providers::merged_compile_defines(&dep_defines, &self.defines) {
                        argv_strs.push(format!("-D{}", d));
                    }
                    let mut argv_seen: std::collections::HashSet<String> =
                        argv_strs.iter().cloned().collect();
                    for ip in &owned_header_quote_paths {
                        let flag = format!("-iquote{}", ip);
                        if argv_seen.insert(flag.clone()) {
                            argv_strs.push(flag);
                        }
                    }
                    for ip in &dep_include_paths {
                        let flag = format!("-I{}", ip);
                        if argv_seen.insert(flag.clone()) {
                            argv_strs.push(flag);
                        }
                    }
                    argv_strs.push(format!("-I{}", workspace_root_str));
                    for extra in &extra_proto_strs {
                        argv_strs.push(format!("-I{}", extra));
                    }
                    argv_strs.push(format!("-I{}", genfiles_str));
                    // Demote /usr/include to -isystem (after all -I) so the
                    // pinned Abseil -I wins over the host's debian7 absl. Same
                    // transform as PASS-B (CcCompileCommandSpec) → verifier parity.
                    crate::cc_command_spec::demote_host_system_includes(&mut argv_strs);
                    argv_strs.push("-o".to_string());
                    argv_strs.push(obj_path_str);
                    argv_strs.push(src_path_str);

                    let exe_pid = if is_cuda_source {
                        ctx.intern_path(&cuda_device_compiler())
                    } else if is_gpu_source {
                        ctx.intern_path(Path::new("/opt/rocm/bin/hipcc"))
                    } else if is_c_source {
                        ctx.intern_path(&cc_pathbuf)
                    } else {
                        ctx.intern_path(&cxx_pathbuf)
                    };
                    let argv: Vec<rush_action::ids::StringId> = argv_strs
                        .iter()
                        .map(|s| ctx.intern_string(s))
                        .collect();
                    ctx.set_executable_action(
                        compile_id,
                        rush_action::ids::ExecutableAction {
                            executable: exe_pid,
                            argv: argv.into_boxed_slice(),
                            env: Box::new([]),
                            working_dir: None,
                            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                        },
                    );
                }
            }
            // Collect dep linker-input artifacts as cross-target
            // CcLink inputs. Each LinkerInputId derefs through
            // the registry to its underlying ArtifactId (the .a /
            // .so the linker actually consumes).
            let mut link_inputs: Vec<rush_action::ids::ArtifactId> = obj_artifacts.clone();
            for dep_cc in dep_cc_infos.iter() {
                for lid in dep_cc.linking.linker_inputs.iter() {
                    let li = ctx.registry().linker_input(lid);
                    link_inputs.push(li.artifact);
                }
            }
            let output_kind = if self.linkshared {
                ArtifactKind::SharedLibrary
            } else {
                ArtifactKind::Executable
            };
            let bin_aid = ctx.register_generated_artifact(
                Path::new(&self.name),
                output_kind,
                None,
            );
            let (expanded_linkopts, linkopt_inputs) =
                match expand_cc_binary_linkopts(&self.linkopts, |label| {
                    cc_binary_analysis_linkopt_files(ctx, label)
                }) {
                    Ok(expanded) => expanded,
                    Err(_) => {
                        let _ = ctx.declare_incomplete_action(
                            "CcLink",
                            link_inputs,
                            vec![bin_aid],
                            "cc_binary linkopts location resolution failed",
                        );
                        return ProviderSet::empty().with_default_info(
                            crate::providers::DefaultInfo {
                                files: vec![bin_aid],
                            },
                        );
                    }
                };
            for input in linkopt_inputs {
                if !link_inputs.contains(&input) {
                    link_inputs.push(input);
                }
            }

            // Phase 5.C.4 step 2b cc_binary CcLink migration.
            // Mirrors what link_binary assembles at build time.
            // The dep walk for linker_inputs / link_fragments
            // builds dep_flags pre-deduplicated against itself
            // (legacy build() drops duplicate archive paths to
            // avoid `multiple definition` linker errors when
            // multiple direct deps transitively carry the same
            // .a). Borrow-collect: gather all dep / obj / linkopts
            // / toolchain strings before the final intern pass.
            let obj_path_strs: Vec<String> = obj_artifacts
                .iter()
                .map(|aid| ctx.registry().artifact_path(*aid).display().to_string())
                .collect();
            let object_paths: Vec<std::path::PathBuf> = obj_artifacts
                .iter()
                .map(|aid| ctx.registry().artifact_path(*aid).to_path_buf())
                .collect();
            // Walk dep CcInfo.linking. Each dep contributes:
            //   linker_inputs: per LinkerInputId, the underlying
            //     .a / .so artifact path. alwayslink wraps with
            //     -Wl,--whole-archive ... -Wl,--no-whole-archive.
            //   link_fragments: Flag (raw flag string), SearchDir
            //     (-L<path>), Library (.a path via linker_input),
            //     Group (-Wl,--start-group ... -Wl,--end-group).
            // dep_flags is dedup'd against duplicate archive
            // paths (matches build()'s args.contains-style dedup).
            let mut dep_args: Vec<CcBinaryDepArg> = Vec::new();
            let mut seen_dep_lib_paths: HashSet<String> = HashSet::new();
            {
                use crate::providers::LinkFragment;
                let reg = ctx.registry();
                for dep_cc in dep_cc_infos.iter() {
                    for lid in dep_cc.linking.linker_inputs.iter() {
                        let li = reg.linker_input(lid);
                        let lib_path = reg.artifact_path(li.artifact).display().to_string();
                        if li.alwayslink {
                            dep_args.push(CcBinaryDepArg::structured(
                                "-Wl,--whole-archive",
                            ));
                            if seen_dep_lib_paths.insert(lib_path.clone()) {
                                dep_args.push(CcBinaryDepArg::structured(lib_path));
                            }
                            dep_args.push(CcBinaryDepArg::structured(
                                "-Wl,--no-whole-archive",
                            ));
                        } else if seen_dep_lib_paths.insert(lib_path.clone()) {
                            dep_args.push(CcBinaryDepArg::structured(lib_path));
                        }
                    }
                    for frag in dep_cc.linking.link_fragments.iter() {
                        match frag {
                            LinkFragment::Flag(sid) => {
                                dep_args.push(CcBinaryDepArg::opaque(
                                    reg.strings().str(sid).to_string(),
                                ));
                            }
                            LinkFragment::SearchDir(pid) => {
                                let p = reg.paths().path(pid);
                                dep_args.push(CcBinaryDepArg::structured(
                                    format!("-L{}", p.display()),
                                ));
                            }
                            LinkFragment::Library(lid) => {
                                let li = reg.linker_input(lid);
                                let p = reg.artifact_path(li.artifact).display().to_string();
                                if seen_dep_lib_paths.insert(p.clone()) {
                                    dep_args.push(CcBinaryDepArg::structured(p));
                                }
                            }
                            LinkFragment::Group(lids) => {
                                dep_args.push(CcBinaryDepArg::structured(
                                    "-Wl,--start-group",
                                ));
                                for lid in lids.iter() {
                                    let li = reg.linker_input(*lid);
                                    let p = reg.artifact_path(li.artifact).display().to_string();
                                    dep_args.push(CcBinaryDepArg::structured(p));
                                }
                                dep_args.push(CcBinaryDepArg::structured(
                                    "-Wl,--end-group",
                                ));
                            }
                        }
                    }
                }
            }
            let bin_path_str = ctx.registry().artifact_path(bin_aid).display().to_string();
            let ldflags: Vec<String> = ctx.toolchain().ldflags.clone();
            let cxx_pathbuf_link = ctx.toolchain().cxx.clone();
            let dep_flags = cc_binary_dep_arg_values(&dep_args);

            let driver_flags = cc_binary_link_driver_flags(
                &object_paths,
                &dep_flags,
                &expanded_linkopts,
                &ldflags,
                self.linkshared,
            );
            // Dedup repeated archive paths / whole-archive triples in the
            // transitive link-fragment stream (e.g. libLLVMSupport.a folded
            // in by every @llvm-project//{llvm,mlir}:* stub via
            // shared_link_flags_for_stub). A static archive whole-archived
            // more than once double-includes its objects → "multiple
            // definition". Mirrors cc_binary build()/cc_shared_library; the
            // legacy spec path applies the same dedup so the verifier stays
            // byte-identical.
            // #39: wrap the archive closure in --start-group/--end-group so
            // a single-pass linker resolves inter-archive back-references
            // regardless of order (matches link_binary + the legacy CcLink
            // spec below; keep all three byte-identical for the verifier).
            let grouped_dep_args = group_cc_binary_dep_args(dep_args);
            let plan = cc_binary_logical_link_plan_with_dep_args(
                &obj_path_strs,
                &driver_flags,
                &grouped_dep_args,
                &expanded_linkopts,
                &ldflags,
                &[],
                &bin_path_str,
            );
            let response_root = cc_binary_response_root(ctx.workspace_root());
            let transport = cc_binary_link_transport_in(
                &plan,
                Path::new(&bin_path_str),
                &response_root,
            );
            // Every non-inline transport shape is fenced from typed execution.
            // Generic cc_binary remains globally de-admitted because runtime
            // ROCm additions and NestedSet fallback archives are not shared
            // with this analysis path yet and can change the final threshold.
            match transport {
                CcBinaryLinkTransport::Inline { spawn_argv } => {
                    if let Ok(link_id) =
                        ctx.declare_action("CcLink", link_inputs, vec![bin_aid])
                    {
                        let exe_pid = ctx.intern_path(&cxx_pathbuf_link);
                        let argv: Vec<rush_action::ids::StringId> = spawn_argv
                            .iter()
                            .map(|s| ctx.intern_string(s))
                            .collect();
                        ctx.set_executable_action(
                            link_id,
                            rush_action::ids::ExecutableAction {
                                executable: exe_pid,
                                argv: argv.into_boxed_slice(),
                                env: Box::new([]),
                                working_dir: None,
                                env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                            },
                        );
                    }
                }
                CcBinaryLinkTransport::LinkerResponse { .. } => {
                    let _ = ctx.declare_incomplete_action(
                        "CcLink",
                        link_inputs,
                        vec![bin_aid],
                        "cc_binary response-file materialization is not typed",
                    );
                }
                CcBinaryLinkTransport::Unsupported { reason, .. } => {
                    let _ = ctx.declare_incomplete_action(
                        "CcLink",
                        link_inputs,
                        vec![bin_aid],
                        reason,
                    );
                }
            }
            // Phase 5.C.4: cc_binary's public output (DefaultInfo)
            // is the executable or manually loaded shared object. Without
            // this the scheduler
            // would synthesize the target's NestedSet from every
            // declared action output — including the .o files —
            // and consumers of the binary's NestedSet would see
            // a +1 .o entry the legacy build() never returned.
            return ProviderSet::empty()
                .with_default_info(crate::providers::DefaultInfo {
                    files: vec![bin_aid],
                });
        }
        ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "cc_binary"
    }

    /// Phase 5.C.4 commit 4: legacy spec builder for the
    /// verifier. Mirrors `compile_source` per src + `link_binary`
    /// for the final binary.
    async fn build_legacy_command_specs(
        &self,
        context: &BuildContext,
    ) -> Vec<(crate::SchedulerActionKey, rush_action::command_spec::NormalizedCommandSpec)> {
        use crate::cc_command_spec::{CcCompileCommandSpec, CcLinkCommandSpec};
        use crate::providers::LinkFragment;
        use crate::SchedulerActionKey;
        let mut specs = Vec::new();
        if self.srcs.is_empty() {
            return specs;
        }
        let (expanded_linkopts, linkopt_input_paths) =
            match expand_cc_binary_linkopts(&self.linkopts, |label| {
                cc_binary_runtime_linkopt_files(context, label)
            }) {
                Ok(expanded) => expanded,
                Err(_) => return specs,
            };

        // Dep flags assembled the same way link_binary builds
        // them: per-LinkerInputId archive paths (with
        // alwayslink wrapping) + LinkFragment::{Flag, SearchDir,
        // Library, Group} entries. Pre-deduplicated against
        // duplicate archive paths.
        let mut dep_args: Vec<CcBinaryDepArg> = Vec::new();
        // dep_include_paths for compile spec.
        let mut dep_include_paths: Vec<std::path::PathBuf> = Vec::new();
        // Phase 5.C.4 commit 27: dep generated headers via the
        // centralized collect_generated_header_paths helper.
        let mut dep_generated_header_paths: Vec<std::path::PathBuf> = Vec::new();
        // Declared link inputs collected alongside dep_flags so
        // the legacy spec's declared_input_paths matches what
        // the scheduler analyze() declares (objs + dep .a's).
        let mut dep_link_input_paths: Vec<std::path::PathBuf> = Vec::new();
        // Transitive dep `defines` (Bazel postorder) for cc_binary's own
        // compile — collected from the ordered dep source to match analyze().
        let mut dep_defines: Vec<String> = Vec::new();
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            // Generated-header walk via the commit-27 helper.
            // Include-dirs / linker-inputs walks stay inline
            // (separate concerns).
            let cc_infos: Vec<std::sync::Arc<crate::providers::CcInfo>> = context
                .dependency_providers
                .values()
                .filter_map(|p| p.cc_info.clone())
                .collect();
            dep_generated_header_paths =
                crate::providers::collect_generated_header_paths(&reg, &cc_infos);

            let ordered_cc_infos: Vec<std::sync::Arc<crate::providers::CcInfo>> = context
                .dependency_providers_ordered
                .iter()
                .filter_map(|(_k, p)| p.cc_info.clone())
                .collect();
            dep_defines = crate::providers::flatten_transitive_defines(
                reg.strings(),
                ordered_cc_infos.iter().map(|cc| cc.compilation.defines.inner()),
            );

            for (_dep_key, providers) in &context.dependency_providers_ordered {
                if let Some(cc_info) = providers.get_cc_info() {
                    for path_id in cc_info.compilation.include_dirs.iter() {
                        let p = reg.paths().path(path_id).to_path_buf();
                        if !dep_include_paths.contains(&p) {
                            dep_include_paths.push(p);
                        }
                    }
                    let linkc = &cc_info.linking;
                    for lid in linkc.linker_inputs.iter() {
                        let li = reg.linker_input(lid);
                        let lib_path_str =
                            reg.artifact_path(li.artifact).display().to_string();
                        let lib_pathbuf =
                            reg.artifact_path(li.artifact).to_path_buf();
                        if !dep_link_input_paths.contains(&lib_pathbuf) {
                            dep_link_input_paths.push(lib_pathbuf);
                        }
                        if li.alwayslink {
                            dep_args.push(CcBinaryDepArg::structured(
                                "-Wl,--whole-archive",
                            ));
                            dep_args.push(CcBinaryDepArg::structured(lib_path_str));
                            dep_args.push(CcBinaryDepArg::structured(
                                "-Wl,--no-whole-archive",
                            ));
                        } else if !dep_args
                            .iter()
                            .any(|arg| arg.value() == lib_path_str)
                        {
                            dep_args.push(CcBinaryDepArg::structured(lib_path_str));
                        }
                    }
                    for frag in linkc.link_fragments.iter() {
                        match frag {
                            LinkFragment::Flag(sid) => {
                                dep_args.push(CcBinaryDepArg::opaque(
                                    reg.strings().str(sid).to_string(),
                                ));
                            }
                            LinkFragment::SearchDir(pid) => {
                                let p = reg.paths().path(pid);
                                dep_args.push(CcBinaryDepArg::structured(
                                    format!("-L{}", p.display()),
                                ));
                            }
                            LinkFragment::Library(lid) => {
                                let li = reg.linker_input(lid);
                                let p = reg.artifact_path(li.artifact).display().to_string();
                                if !dep_args.iter().any(|arg| arg.value() == p) {
                                    dep_args.push(CcBinaryDepArg::structured(p));
                                }
                            }
                            LinkFragment::Group(lids) => {
                                dep_args.push(CcBinaryDepArg::structured(
                                    "-Wl,--start-group",
                                ));
                                for lid in lids.iter() {
                                    let li = reg.linker_input(*lid);
                                    let p = reg.artifact_path(li.artifact).display().to_string();
                                    dep_args.push(CcBinaryDepArg::structured(p));
                                }
                                dep_args.push(CcBinaryDepArg::structured(
                                    "-Wl,--end-group",
                                ));
                            }
                        }
                    }
                }
            }
        }

        let mut linux_extra_includes: Vec<std::path::PathBuf> = Vec::new();
        if cfg!(target_os = "linux") {
            for extra in ["/usr/include/jsoncpp", "/usr/include/x86_64-linux-gnu"] {
                if std::path::Path::new(extra).exists() {
                    linux_extra_includes.push(std::path::PathBuf::from(extra));
                }
            }
        }

        let target_output_dir = context.output_dir.join(format!("{}_objs", self.name));
        let mut object_paths: Vec<std::path::PathBuf> = Vec::new();
        // Expand explicit labels and implicit bare-header labels exactly as
        // analyze() and runtime build() do.
        let expanded_srcs =
            expand_cc_binary_srcs(&self.srcs, |l| context.get_dependency_files(l));
        let package = context
            .package_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let owned_header_paths = cc_binary_owned_header_paths(
            &context.workspace_root,
            &package,
            &expanded_srcs,
        );
        let owned_header_quote_dirs = cc_binary_owned_header_quote_dirs(
            &context.workspace_root,
            &context.output_dir,
            &package,
            &owned_header_paths,
        );
        for src in &expanded_srcs {
            // Headers (.h/.inc) listed in srcs are owned-but-not-compiled
            // (Bazel semantics) — compiling one with `-c` emits a PCH that,
            // sharing the .cc's file stem, clobbers the real object → link
            // "file format not recognized". Mirrors cc_library's filter.
            if !crate::cc_command_spec::is_compilable_source(src) {
                continue;
            }
            let src_path = context.source_path(src);
            let obj_name = src_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "obj".to_string());
            let obj_path = target_output_dir.join(format!("{}.o", obj_name));
            object_paths.push(obj_path.clone());

            let is_c_source = src_path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("c"))
                .unwrap_or(false);

            // GPU/ROCm per-source routing (PASS-B spec) — mirror compile_source_impl so the
            // verifier sees the same exe (hipcc) + ROCm flags for a .cu.cc/.cu, not a host
            // g++ mismatch. Gated by RUSH_ENABLE_ROCM.
            let rocm_enabled =
                std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1");
            let is_gpu_source = rocm_enabled && {
                let s = src_path.to_string_lossy();
                s.ends_with(".cu.cc") || s.ends_with(".cu")
            };
            let mut spec_copts = self.copts.clone();
            // Host-wide ROCm header/type defines (all C++ TUs) + GPU-source-only Eigen/offload.
            if rocm_enabled {
                for f in [
                    "-DTENSORFLOW_USE_ROCM=1",
                    "-D__HIP_PLATFORM_AMD__",
                    "-DEIGEN_USE_HIP",
                    "-I/opt/rocm/include",
                ] {
                    if !spec_copts.iter().any(|c| c == f) {
                        spec_copts.push(f.to_string());
                    }
                }
            }
            if is_gpu_source {
                for f in [
                    "-DEIGEN_USE_GPU",
                    "-DEIGEN_USE_HIP",
                    "--offload-arch=gfx1100",
                    // gcc-15 libstdc++ auto-enables _GLIBCXX_ASSERTIONS when not __OPTIMIZE__
                    // (c++config.h), so std::complex's __glibcxx_assert calls __host__
                    // __glibcxx_assert_fail — which hipcc's device pass rejects from a
                    // __host__ __device__ fn. Disable libstdc++ assertions for device code so
                    // std::complex GPU kernels (cwise etc.) compile. (Codex 019eebc6.)
                    "-D_GLIBCXX_NO_ASSERTIONS",
                ] {
                    if !spec_copts.iter().any(|c| c == f) {
                        spec_copts.push(f.to_string());
                    }
                }
                if let Some(flag) = gpu_device_gcc_install_dir_flag() {
                    if !spec_copts.iter().any(|c| *c == flag) {
                        spec_copts.push(flag);
                    }
                }
            }
            let is_cuda_source = append_cuda_compile_flags(&mut spec_copts, &src_path);
            if is_cuda_source {
                normalize_cuda_compile_flags(&mut spec_copts);
            }

            let compile = CcCompileCommandSpec {
                mnemonic: if is_c_source {
                    "CCompile".to_string()
                } else {
                    "CppCompile".to_string()
                },
                executable: if is_cuda_source {
                    cuda_device_compiler()
                } else if is_gpu_source {
                    std::path::PathBuf::from("/opt/rocm/bin/hipcc")
                } else if is_c_source {
                    context.toolchain.cc.clone()
                } else {
                    context.toolchain.cxx.clone()
                },
                cwd: None,
                is_c_source,
                wants_no_rtti: deps_want_llvm_no_rtti(&self.deps),
                src_path: src_path.clone(),
                obj_path: obj_path.clone(),
                copts: spec_copts,
                cxxflags: context.toolchain.cxxflags.clone(),
                cflags: context.toolchain.cflags.clone(),
                defines: self.defines.clone(),
                dep_defines: dep_defines.clone(),
                direct_include_paths: Vec::new(),
                direct_quote_include_paths: owned_header_quote_dirs.clone(),
                dep_include_paths: dep_include_paths.clone(),
                include_layout: crate::cc_command_spec::IncludeLayout::BazelPackageRooted {
                    workspace_root: context.workspace_root.clone(),
                    extra_workspace_paths: context.extra_proto_paths.to_vec(),
                    genfiles_path: context.output_dir.join("genfiles"),
                    linux_extra_includes: linux_extra_includes.clone(),
                },
                // Commit 14: include dep generated headers in
                // declared_inputs so the verifier's declared-
                // input comparison matches the scheduler side.
                declared_input_paths: {
                    let mut inputs = vec![src_path.clone()];
                    inputs.extend(dep_generated_header_paths.iter().cloned());
                    inputs.extend(
                        owned_header_paths
                            .iter()
                            .filter(|path| *path != &src_path)
                            .cloned(),
                    );
                    inputs
                },
                declared_output_paths: vec![obj_path.clone()],
                toolchain_identity: None,
            };
            specs.push((
                SchedulerActionKey {
                    mnemonic: compile.mnemonic.clone(),
                    primary_output_path: obj_path,
                },
                compile.to_normalized_command_spec(),
            ));
        }

        let bin_path = context.output_dir.join(&self.name);
        // Declared inputs for CcLink: object files + dep linker
        // archive paths (matching what scheduler analyze()
        // declares — see cc_binary's analyze() block where
        // link_inputs.extend(li.artifact for lid in linker_inputs)).
        let mut link_declared_inputs: Vec<std::path::PathBuf> = object_paths.clone();
        link_declared_inputs.extend(dep_link_input_paths.iter().cloned());
        for input in linkopt_input_paths {
            if !link_declared_inputs.contains(&input) {
                link_declared_inputs.push(input);
            }
        }
        let dep_flags = cc_binary_dep_arg_values(&dep_args);
        let grouped_dep_args = group_cc_binary_dep_args(dep_args);
        let grouped_dep_flags = cc_binary_dep_arg_values(&grouped_dep_args);
        let link = CcLinkCommandSpec {
            mnemonic: "CcLink".to_string(),
            executable: context.toolchain.cxx.clone(),
            cwd: None,
            object_paths: object_paths.clone(),
            driver_flags: cc_binary_link_driver_flags(
                &object_paths,
                &dep_flags,
                &expanded_linkopts,
                &context.toolchain.ldflags,
                self.linkshared,
            ),
            // Same archive/whole-archive dedup AND --start-group wrap as
            // analyze() above — keep the legacy CcLink spec byte-identical for
            // the scheduler verifier.
            dep_flags: grouped_dep_flags,
            linkopts: expanded_linkopts,
            ldflags: context.toolchain.ldflags.clone(),
            bin_path: bin_path.clone(),
            declared_input_paths: link_declared_inputs,
            declared_output_paths: vec![bin_path.clone()],
            toolchain_identity: None,
        };
        let object_args: Vec<String> = link
            .object_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        let bin_path_arg = link.bin_path.display().to_string();
        let plan = cc_binary_logical_link_plan_with_dep_args(
            &object_args,
            &link.driver_flags,
            &grouped_dep_args,
            &link.linkopts,
            &link.ldflags,
            &[],
            &bin_path_arg,
        );
        let mut link_spec = link.to_normalized_command_spec();
        // Record the actual response transport for the CPU typed-input shape.
        // Runtime-only ROCm and NestedSet fallback deltas remain unverified,
        // so generic cc_binary stays globally de-admitted.
        let response_root = cc_binary_response_root(&context.workspace_root);
        match cc_binary_link_transport_in(&plan, &bin_path, &response_root) {
            CcBinaryLinkTransport::Inline { spawn_argv } => {
                link_spec.argv = spawn_argv;
            }
            CcBinaryLinkTransport::LinkerResponse {
                spawn_argv,
                response_files,
            } => {
                link_spec.argv = spawn_argv;
                link_spec.param_files = response_files
                    .into_iter()
                    .map(|response_file| {
                        (
                            response_file.path,
                            blake3::hash(response_file.contents.as_bytes())
                                .to_hex()
                                .to_string(),
                        )
                    })
                    .collect();
            }
            CcBinaryLinkTransport::Unsupported { logical_argv, .. } => {
                // NormalizedCommandSpec has no failure-status field. Retain the
                // exact logical command and claim no response materialization;
                // generic cc_binary remains globally de-admitted.
                link_spec.argv = logical_argv;
            }
        }
        link_spec.normalize();
        specs.push((
            SchedulerActionKey {
                mnemonic: "CcLink".to_string(),
                primary_output_path: bin_path,
            },
            link_spec,
        ));

        specs
    }
}

impl CcBinaryInstance {
    /// Compile a single source file to an object file
    async fn compile_source(
        &self,
        context: &BuildContext,
        src_path: &Path,
        obj_path: &Path,
        // Dep generated headers plus any source headers reachable from this TU.
        // Unrelated dependency source headers are keyed by their producer
        // libraries, not by this sink compile.
        header_inputs: &[PathBuf],
        // Quote-only repository/package roots of header-like entries owned
        // through `srcs`. These resolve generated package-relative spellings
        // without allowing an owned header to shadow a system angle include.
        owned_header_quote_dirs: &[PathBuf],
    ) -> Result<()> {
        let mut args = Vec::new();

        // Add standard flags
        args.push("-c".to_string());
        if self.linkshared {
            args.push("-fPIC".to_string());
        }
        args.push("-std=c++17".to_string());

        // LLVM/MLIR headers are built with `-fno-rtti` upstream (LLVM's
        // cmake default), so their classes declare vtables but never
        // emit typeinfo. Any TU that `#include`s them and uses classes
        // like `llvm::cl::opt<T>` / `llvm::detail::format_adapter` in
        // a position needing typeinfo (e.g. std::type_info operands of
        // vague-linkage vtables) will link-fail with "undefined
        // reference to typeinfo for llvm::…" — the symbol simply
        // doesn't exist in libLLVM*.a. Match LLVM's setting by
        // disabling RTTI on compile units that have any @llvm-project
        // dep in their chain. Bazel's `cc_library(copts = [...])`
        // propagation would carry this; rush's CcInfo doesn't (no
        // compile_copts field yet), so we sniff the direct-deps list.
        let wants_llvm_no_rtti = self.deps.iter().any(|d| d.starts_with("@llvm-project"));
        if wants_llvm_no_rtti {
            args.push("-fno-rtti".to_string());
        }

        // Add user-specified compiler options
        for opt in &self.copts {
            args.push(opt.clone());
        }

        // Add toolchain flags
        for flag in &context.toolchain.cxxflags {
            args.push(flag.clone());
        }

        // Add defines: transitive dep defines (Bazel `defines` is
        // transitive via CcInfo) first, then own direct defines.
        let dep_defines: Vec<String> = if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            let ordered_cc_infos: Vec<std::sync::Arc<crate::providers::CcInfo>> = context
                .dependency_providers_ordered
                .iter()
                .filter_map(|(_k, p)| p.cc_info.clone())
                .collect();
            crate::providers::flatten_transitive_defines(
                reg.strings(),
                ordered_cc_infos.iter().map(|cc| cc.compilation.defines.inner()),
            )
        } else {
            Vec::new()
        };
        for define in crate::providers::merged_compile_defines(&dep_defines, &self.defines) {
            args.push(format!("-D{}", define));
        }

        // Add include paths from dependencies. Phase 5.B.3.d:
        // prefer typed `compilation.include_dirs` (PathIds via the
        // registry) when available, fall back to flat
        // 5.B.3.e: typed-only.
        //
        // The header-parent-dir loop below pushes one `-I` per transitive
        // header; for a TF GPU workload that depends on the aggregate
        // //tensorflow/core:tensorflow target, the transitive header set is
        // enormous (tens of thousands of headers, most sharing a parent dir),
        // so without de-duplication the compile command overflows the exec
        // argument list (`posix_spawn: Argument list too long` from g++ when it
        // re-execs cc1plus). De-dup every `-I` flag (preserving first-seen
        // order) — the unique directory count is small. This is a pure
        // reduction (same set of search dirs, no behavior change) so smaller
        // CPU compiles are unaffected aside from dropping redundant flags.
        let mut seen_includes: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut push_include = |args: &mut Vec<String>, flag: String| {
            if seen_includes.insert(flag.clone()) {
                args.push(flag);
            }
        };
        for path in owned_header_quote_dirs {
            args.push(format!("-iquote{}", path.display()));
        }
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            for dep in &self.deps {
                if let Some(providers) = context.get_dependency_providers(dep) {
                    if let Some(cc_info) = providers.get_cc_info() {
                        for path_id in cc_info.compilation.include_dirs.iter() {
                            let p = reg.paths().path(path_id);
                            push_include(&mut args, format!("-I{}", p.display()));
                        }
                    }
                }
            }
        }
        // Headers via transitive artifacts (unchanged by 5.B.3.d —
        // this reads from the rush-rules Artifact NestedSet, not
        // CcInfo). Phase 5.E will fold these into the typed
        // compilation context once the action graph in 5.C
        // produces ArtifactIds for headers.
        for dep in &self.deps {
            if let Some(iter) = context.iter_transitive_artifacts(dep) {
                for artifact in iter {
                    if artifact.artifact_type == ArtifactType::Header {
                        if let Some(parent) = artifact.path.parent() {
                            push_include(&mut args, format!("-I{}", parent.display()));
                        }
                    }
                }
            }
        }

        // A generic cc_binary is a Bazel-package-rooted compile on every
        // backend, not only when a GPU environment toggle happens to be set.
        // Keep the runtime fallback aligned with analyze() and
        // build_legacy_command_specs(): dependency include dirs first, then the
        // main workspace root, external workspace roots, and genfiles. Without
        // this base a CPU fallback cannot resolve repository-relative includes
        // such as "tensorflow/core/platform/enable_tf2_utils.h", even though
        // the scheduler action already carries the correct IncludeLayout.
        push_include(&mut args, format!("-I{}", context.workspace_root.display()));
        for extra in &context.extra_proto_paths {
            push_include(&mut args, format!("-I{}", extra.display()));
        }
        push_include(
            &mut args,
            format!("-I{}", context.output_path("genfiles").display()),
        );

        // Under RUSH_ENABLE_ROCM, additionally expose the XLA/TSL roots and
        // ROCm host-wide defines. The package-rooted base above is shared with
        // CPU and CUDA compiles.
        if std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1") {
            push_include(
                &mut args,
                format!("-I{}", context.workspace_root.join("third_party/xla").display()),
            );
            push_include(
                &mut args,
                format!(
                    "-I{}",
                    context
                        .workspace_root
                        .join("third_party/xla/third_party/tsl")
                        .display()
                ),
            );
            for d in ["-DTENSORFLOW_USE_ROCM=1", "-D__HIP_PLATFORM_AMD__", "-I/opt/rocm/include"] {
                args.push(d.to_string());
            }
        }
        if cuda_enabled() {
            append_cuda_compile_flags(&mut args, src_path);
        }

        // Add output and input files
        args.push("-o".to_string());
        args.push(obj_path.to_string_lossy().to_string());
        args.push(src_path.to_string_lossy().to_string());

        let command = Command {
            executable: context.toolchain.cxx.clone(),
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };

        // Action inputs: the source, dependency-generated headers, and any
        // dependency source headers this TU can textually reach. Dependency
        // source headers that only affect producer libraries stay out of this
        // sink key, avoiding root object misses on private header edits.
        let mut inputs = vec![ActionArtifact::Source(src_path.to_path_buf())];
        for hp in header_inputs {
            // Keep required headers in the action identity even when a producer
            // failed to materialize one. Action::compute_id then reports the
            // missing input instead of allowing a stale object cache hit.
            inputs.push(ActionArtifact::Source(hp.clone()));
        }

        // Compute action ID
        let action_id = Action::compute_id(&inputs, &command, &HashMap::new())?;
        let outputs = vec![ActionArtifact::Source(obj_path.to_path_buf())];

        let action = Action {
            id: action_id,
            command,
            env: HashMap::new(),
            inputs,
            outputs,
            mnemonic: "CppCompile".to_string(),
        };

        let result = context.execute_action(action).await?;

        if result.exit_code != 0 {
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(anyhow!(
                "Compilation failed for {}: {}",
                src_path.display(),
                stderr
            ));
        }

        Ok(())
    }

    /// Link object files and dependencies into an executable
    async fn link_binary(
        &self,
        context: &BuildContext,
        object_files: &[PathBuf],
        binary_path: &Path,
    ) -> Result<()> {
        let object_args: Vec<String> = object_files
            .iter()
            .map(|object| object.to_string_lossy().to_string())
            .collect();

        // Accumulate the dep-derived link flags as a single stream so we
        // can post-process them to drop duplicate archive references.
        // Multiple direct deps can transitively carry the same
        // `libLLVMSupport.a` path (e.g. the `llvm:Support`,
        // `llvm:TableGen`, `mlir:TableGen` trio all fold it in via
        // `shared_link_flags_for_stub`); without dedup ld would see the
        // archive twice and emit `multiple definition of
        // llvm::StringError::StringError(…)` on every
        // `--whole-archive`-wrapped symbol.
        let mut dep_args: Vec<CcBinaryDepArg> = Vec::new();
        let mut input_files = object_files.to_vec();
        let mut input_seen: std::collections::HashSet<std::path::PathBuf> =
            input_files.iter().cloned().collect();
        let (expanded_linkopts, linkopt_inputs) =
            expand_cc_binary_linkopts(&self.linkopts, |label| {
                cc_binary_runtime_linkopt_files(context, label)
            })?;
        for input in linkopt_inputs {
            if input_seen.insert(input.clone()) {
                input_files.push(input);
            }
        }

        // Phase 5.B.3.d link consumer migration: prefer typed
        // 5.B.3.e: typed-only path. CcInfo always has linking
        // populated; orchestrator always plumbs the registry.
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            for dep in &self.deps {
                if let Some(providers) = context.get_dependency_providers(dep) {
                    if let Some(cc_info) = providers.get_cc_info() {
                        let linkc = &cc_info.linking;
                        for lid in linkc.linker_inputs.iter() {
                            let li = reg.linker_input(lid);
                            let lib_path = reg.artifact_path(li.artifact).to_path_buf();
                            if li.alwayslink {
                                dep_args.push(CcBinaryDepArg::structured(
                                    "-Wl,--whole-archive",
                                ));
                                dep_args.push(CcBinaryDepArg::structured(
                                    lib_path.to_string_lossy().to_string(),
                                ));
                                dep_args.push(CcBinaryDepArg::structured(
                                    "-Wl,--no-whole-archive",
                                ));
                            } else {
                                dep_args.push(CcBinaryDepArg::structured(
                                    lib_path.to_string_lossy().to_string(),
                                ));
                            }
                            input_seen.insert(lib_path.clone());
                            input_files.push(lib_path);
                        }
                        use crate::providers::LinkFragment;
                        for frag in linkc.link_fragments.iter() {
                            match frag {
                                LinkFragment::Flag(sid) => {
                                    dep_args.push(CcBinaryDepArg::opaque(
                                        reg.strings().str(sid).to_string(),
                                    ));
                                }
                                LinkFragment::SearchDir(pid) => {
                                    let p = reg.paths().path(pid);
                                    dep_args.push(CcBinaryDepArg::structured(
                                        format!("-L{}", p.display()),
                                    ));
                                }
                                LinkFragment::Library(lid) => {
                                    let li = reg.linker_input(lid);
                                    let p = reg.artifact_path(li.artifact).to_path_buf();
                                    dep_args.push(CcBinaryDepArg::structured(
                                        p.to_string_lossy().to_string(),
                                    ));
                                }
                                LinkFragment::Group(lids) => {
                                    dep_args.push(CcBinaryDepArg::structured(
                                        "-Wl,--start-group",
                                    ));
                                    for lid in lids.iter() {
                                        let li = reg.linker_input(*lid);
                                        let p = reg.artifact_path(li.artifact).to_path_buf();
                                        dep_args.push(CcBinaryDepArg::structured(
                                            p.to_string_lossy().to_string(),
                                        ));
                                    }
                                    dep_args.push(CcBinaryDepArg::structured(
                                        "-Wl,--end-group",
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            drop(reg);
        }
        // Outer for-loop ends. Continue with the transitive
        // StaticLibrary sweep below — that's a separate concern
        // operating on the artifact NestedSet, not CcInfo.
        for dep in &self.deps {

            // Fallback/complement: pick up StaticLibrary artifacts from
            // the dep's TRANSITIVE NestedSet — a cc_binary needs to
            // link every .a in its transitive closure even when CcInfo
            // libraries don't carry them. Patch set 2b: we walk the
            // depset DAG with Arc-identity dedup (each shared dep is
            // visited once) instead of the prior per-cc_library
            // pre-flattened Vec which was the O(N²) memory blow-up.
            if let Some(iter) = context.iter_transitive_artifacts(dep) {
                for artifact in iter {
                    if artifact.artifact_type == ArtifactType::StaticLibrary
                        && input_seen.insert(artifact.path.clone())
                    {
                        dep_args.push(CcBinaryDepArg::structured(
                            artifact.path.to_string_lossy().to_string(),
                        ));
                        input_files.push(artifact.path.clone());
                    }
                }
            }
        }

        let dep_flags = cc_binary_dep_arg_values(&dep_args);
        let driver_flags = cc_binary_link_driver_flags(
            object_files,
            &dep_flags,
            &expanded_linkopts,
            &context.toolchain.ldflags,
            self.linkshared,
        );

        // Dedup dep_flags with archive-aware semantics (shared helper —
        // see dedup_archive_link_flags).
        //
        // #39: wrap the archive closure in --start-group/--end-group so the
        // single-pass linker resolves inter-archive back-references regardless
        // of listing order. A deps-only cc_binary (e.g.
        // //tensorflow/core/distributed_runtime/rpc:grpc_testlib_server, srcs=[],
        // deps=[:grpc_testlib_server_main]) pulls a 180+-archive transitive
        // closure that CANNOT be topologically ordered, so a bare archive list
        // left back-references (riegeli/mlir/eager) "undefined" even though the
        // defining archives were on the line. tf_cc_binary already groups its
        // libs (push_link_lib_argv); core cc_binary did not. The linker ignores
        // the group markers for non-archive tokens, so wrapping the whole deduped
        // block is safe.
        let grouped_dep_args = group_cc_binary_dep_args(dep_args);

        // Under RUSH_ENABLE_ROCM, a GPU cc_binary that statically links the TF
        // runtime references the ROCm runtime (libamdhip64, librocblas) from
        // the StreamExecutor / GPU kernels. The @local_config_rocm linkopts
        // don't reliably reach a plain cc_binary's link_fragments, so link the
        // runtime explicitly (-rpath so the binary finds them at run time);
        // they come after the archive closure so the static refs resolve. CPU
        // builds (the certified 463 corpus) are unaffected.
        let rocm_link = std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1");
        let mut extra_linkopts = Vec::new();
        if rocm_link {
            extra_linkopts.push("-L/opt/rocm/lib".to_string());
            extra_linkopts.push("-Wl,-rpath,/opt/rocm/lib".to_string());
            extra_linkopts.push("-lamdhip64".to_string());
            extra_linkopts.push("-lrocblas".to_string());
        }

        let binary_path_arg = binary_path.to_string_lossy().to_string();
        let plan = cc_binary_logical_link_plan_with_dep_args(
            &object_args,
            &driver_flags,
            &grouped_dep_args,
            &expanded_linkopts,
            &context.toolchain.ldflags,
            &extra_linkopts,
            &binary_path_arg,
        );

        let mut inputs: Vec<ActionArtifact> = input_files
            .iter()
            .map(|p| ActionArtifact::Source(p.clone()))
            .collect();
        let mut action_env = HashMap::new();

        // The transitive archive closure of a TF binary can be thousands of
        // .a paths; passing them directly overflows the exec argument list.
        // Spill only provenance-known linker spans, preserving every driver
        // argument in place. Reject an oversized residual before Action
        // construction rather than silently changing compiler semantics.
        let response_root = cc_binary_response_root(&context.workspace_root);
        let spawn_argv = match cc_binary_link_transport_in(
            &plan,
            binary_path,
            &response_root,
        ) {
            CcBinaryLinkTransport::Inline { spawn_argv } => spawn_argv,
            CcBinaryLinkTransport::LinkerResponse {
                spawn_argv,
                response_files,
            } => {
                // Response bodies live in a content-addressed mini-CAS under
                // the workspace's reserved .rush-cache namespace, outside the
                // target-output registry. Publish each immutable blob
                // atomically, reject corruption and legacy output sidecars,
                // and carry one ordered body digest in the action environment
                // as a defense-in-depth key of the bytes the linker consumes.
                reject_stamped_cc_binary_response_paths(&response_files)?;
                let mut response_set_hasher = blake3::Hasher::new();
                for response_file in &response_files {
                    let response_bytes = response_file.contents.as_bytes();
                    response_set_hasher
                        .update(&(response_bytes.len() as u64).to_le_bytes());
                    response_set_hasher.update(response_bytes);
                    materialize_cc_binary_response_file(response_file).await?;
                    // Response bytes are real action inputs, so content-only
                    // changes select a new path and miss the action cache.
                    inputs.push(ActionArtifact::Source(response_file.path.clone()));
                }
                reject_stamped_cc_binary_response_paths(&response_files)?;
                action_env.insert(
                    "RUSH_CC_BINARY_RESPONSE_SET_BLAKE3".to_string(),
                    response_set_hasher.finalize().to_hex().to_string(),
                );
                spawn_argv
            }
            CcBinaryLinkTransport::Unsupported {
                reason,
                logical_bytes,
                spawn_bytes,
                largest_inline_arg,
                driver_state_bytes,
                ..
            } => {
                return Err(anyhow!(
                    "cc_binary link argv cannot be spilled safely ({}): logical={} bytes, \
                     residual={} bytes, largest inline argument={} bytes, driver state={} bytes, \
                     limit={} bytes",
                    reason,
                    logical_bytes,
                    spawn_bytes,
                    largest_inline_arg,
                    driver_state_bytes,
                    CC_BINARY_RESPONSE_FILE_THRESHOLD,
                ));
            }
        };
        let command = Command {
            executable: context.toolchain.cxx.clone(),
            args: spawn_argv,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };
        let action_id = Action::compute_id(&inputs, &command, &action_env)?;

        let outputs = vec![ActionArtifact::Source(binary_path.to_path_buf())];

        let action = Action {
            id: action_id,
            command,
            env: action_env,
            inputs,
            outputs,
            mnemonic: "CppLink".to_string(),
        };

        let result = context.execute_action(action).await?;

        if result.exit_code != 0 {
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(anyhow!("Linking failed: {}", stderr));
        }

        Ok(())
    }
}

#[cfg(test)]
mod cc_binary_response_tests {
    use super::*;
    use crate::analysis_context::{AnalysisContext, StubIncludeResolver};
    use crate::context::{ActionManager, Toolchain};
    use crate::nested_set::NestedSet;
    use crate::providers::{DefaultInfo, DirectCcInfo, FilesInfo};
    use rush_action::ids::{
        ArtifactKey, ArtifactKind, ArtifactRegistry, ConfigId, TargetId,
    };
    use rush_action::ActionResult;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct CapturingActionManager {
        actions: Mutex<Vec<Action>>,
    }

    struct ExecutingActionManager;

    #[derive(Default)]
    struct RecordingExecutingActionManager {
        actions: Mutex<Vec<Action>>,
    }

    #[async_trait]
    impl ActionManager for CapturingActionManager {
        async fn execute(&self, action: Action) -> anyhow::Result<ActionResult> {
            let recomputed = Action::compute_id(
                &action.inputs,
                &action.command,
                &action.env,
            )?;
            assert_eq!(
                action.id, recomputed,
                "captured Action must preserve its documented identity invariant"
            );
            let result = ActionResult {
                action_id: action.id.clone(),
                exit_code: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
                outputs: action.outputs.clone(),
                output_records: Vec::new(),
                duration: Duration::ZERO,
                cached: false,
            };
            self.actions.lock().unwrap().push(action);
            Ok(result)
        }
    }

    #[async_trait]
    impl ActionManager for ExecutingActionManager {
        async fn execute(&self, action: Action) -> anyhow::Result<ActionResult> {
            let recomputed = Action::compute_id(
                &action.inputs,
                &action.command,
                &action.env,
            )?;
            assert_eq!(action.id, recomputed);
            let started = std::time::Instant::now();
            let mut command = tokio::process::Command::new(&action.command.executable);
            command.args(&action.command.args);
            command.envs(&action.env);
            if let Some(working_dir) = &action.command.working_dir {
                command.current_dir(working_dir);
            }
            let output = command.output().await?;
            Ok(ActionResult {
                action_id: action.id,
                exit_code: output.status.code().unwrap_or(-1),
                stdout: output.stdout,
                stderr: output.stderr,
                outputs: action.outputs,
                output_records: Vec::new(),
                duration: started.elapsed(),
                cached: false,
            })
        }
    }

    #[async_trait]
    impl ActionManager for RecordingExecutingActionManager {
        async fn execute(&self, action: Action) -> anyhow::Result<ActionResult> {
            let recomputed = Action::compute_id(
                &action.inputs,
                &action.command,
                &action.env,
            )?;
            assert_eq!(action.id, recomputed);
            self.actions.lock().unwrap().push(action.clone());
            let started = std::time::Instant::now();
            let mut command = tokio::process::Command::new(&action.command.executable);
            command.args(&action.command.args);
            command.envs(&action.env);
            if let Some(working_dir) = &action.command.working_dir {
                command.current_dir(working_dir);
            }
            let output = command.output().await?;
            Ok(ActionResult {
                action_id: action.id,
                exit_code: output.status.code().unwrap_or(-1),
                stdout: output.stdout,
                stderr: output.stderr,
                outputs: action.outputs,
                output_records: Vec::new(),
                duration: started.elapsed(),
                cached: false,
            })
        }
    }

    fn binary_with_linkopt(linkopt: String) -> CcBinaryInstance {
        CcBinaryInstance {
            header_inputs_cache: Default::default(),
            name: "tool".to_string(),
            srcs: Vec::new(),
            deps: Vec::new(),
            copts: Vec::new(),
            linkopts: vec![linkopt],
            linkshared: false,
            defines: Vec::new(),
            data: Vec::new(),
        }
    }

    fn binary_with_generated_header() -> CcBinaryInstance {
        CcBinaryInstance {
            header_inputs_cache: Default::default(),
            name: "tool".to_string(),
            srcs: vec!["main.cc".to_string(), "gen.h".to_string()],
            deps: Vec::new(),
            copts: Vec::new(),
            linkopts: Vec::new(),
            linkshared: false,
            defines: Vec::new(),
            data: Vec::new(),
        }
    }

    fn parsed_binary_attrs(name: &str, linkshared: Option<Value>) -> HashMap<String, Value> {
        let mut attrs = HashMap::from([
            ("name".to_string(), Value::String(name.to_string())),
            (
                "srcs".to_string(),
                Value::List(vec![Value::String("extension.cc".to_string())]),
            ),
        ]);
        if let Some(value) = linkshared {
            attrs.insert("linkshared".to_string(), value);
        }
        attrs
    }

    #[test]
    fn linkshared_accepts_bool_and_int_and_rejects_other_types() {
        let cases = [
            (Value::Bool(true), true),
            (Value::Int(1), true),
            (Value::Bool(false), false),
            (Value::Int(0), false),
        ];
        for (index, (value, expected_shared)) in cases.into_iter().enumerate() {
            let name = format!("binding_{index}.so");
            let instance = CcBinaryRule
                .parse_attributes(&parsed_binary_attrs(&name, Some(value)))
                .unwrap();
            let workspace = PathBuf::from("/workspace");
            let output = PathBuf::from("/output");
            let resolver = StubIncludeResolver::new(workspace.clone());
            let toolchain = Toolchain::default();
            let extra_paths = Vec::new();
            let mut registry = ArtifactRegistry::new();
            let providers = {
                let mut context = AnalysisContext::new(
                    TargetId(1),
                    ConfigId::DEFAULT,
                    "pkg",
                    None,
                    &output,
                    &mut registry,
                    &resolver,
                    &toolchain,
                    &workspace,
                    &extra_paths,
                );
                instance.analyze(&mut context, &[])
            };

            let default = providers
                .get_default_info()
                .expect("cc_binary must publish its named output");
            assert_eq!(default.files.len(), 1);
            let output_id = default.files[0];
            assert_eq!(
                registry.artifact(output_id).kind,
                if expected_shared {
                    ArtifactKind::SharedLibrary
                } else {
                    ArtifactKind::Executable
                }
            );
            assert_eq!(
                registry.artifact_path(output_id),
                output.join(&name),
                "the target name is the output filename, including its .so suffix",
            );
            assert!(providers.get_cc_info().is_none());
            assert!(providers.get_cc_shared().is_none());

            let (link_id, _) = registry
                .declared_actions()
                .find(|(_, action)| {
                    registry.strings().str(action.mnemonic) == "CcLink"
                })
                .expect("cc_binary must declare its link action");
            let link_spec =
                rush_action::command_spec::NormalizedCommandSpec::from_registry_action(
                    &registry,
                    link_id,
                )
                .expect("inline link must have an executable payload");
            assert_eq!(
                link_spec.argv.iter().filter(|arg| *arg == "-shared").count(),
                usize::from(expected_shared),
            );
        }

        for invalid in [
            Value::Int(-1),
            Value::Int(2),
            Value::String("1".to_string()),
            Value::List(Vec::new()),
        ] {
            let error = CcBinaryRule
                .parse_attributes(&parsed_binary_attrs("binding.so", Some(invalid)))
                .err()
                .expect("linkshared values outside bool or integer 0/1 must be rejected");
            assert!(error
                .to_string()
                .contains("linkshared' must be a bool or 0/1 int"));
        }
    }

    #[test]
    fn linkopt_location_parser_rejects_malformed_and_plural_forms() {
        for linkopt in [
            "$(location :version.lds",
            "$(location)",
            "$(location:version.lds)",
            "$(location :one :two)",
            "$(locations :version.lds)",
            "$(execpaths :version.lds)",
        ] {
            let mut attrs = parsed_binary_attrs("binding.so", Some(Value::Int(1)));
            attrs.insert(
                "linkopts".to_string(),
                Value::List(vec![Value::String(linkopt.to_string())]),
            );
            let error = CcBinaryRule
                .parse_attributes(&attrs)
                .err()
                .expect("unsupported location syntax must fail during rule parsing");
            assert!(error.to_string().contains("cc_binary linkopts entry"));
        }

        let untouched = vec!["-Wl,-rpath,$(CUSTOM_ROOT)/lib".to_string()];
        let (expanded, inputs) = expand_cc_binary_linkopts::<(), _>(
            &untouched,
            |_| unreachable!("an unrelated make variable is not a location reference"),
        )
        .unwrap();
        assert_eq!(expanded, untouched);
        assert!(inputs.is_empty());
    }

    #[test]
    fn linkopt_location_expansion_is_singular_and_fail_loud() {
        let linkopts = vec![
            "-Wl,--version-script,$(location :version.lds)".to_string(),
            "$(execpath :exports.map)".to_string(),
        ];
        let (expanded, inputs) = expand_cc_binary_linkopts(&linkopts, |label| {
            Ok(vec![(
                PathBuf::from(format!("/out/{}", label.trim_start_matches(':'))),
                label.to_string(),
            )])
        })
        .unwrap();
        assert_eq!(
            expanded,
            vec![
                "-Wl,--version-script,/out/version.lds",
                "/out/exports.map",
            ]
        );
        assert_eq!(inputs, vec![":version.lds", ":exports.map"]);

        let unknown = expand_cc_binary_linkopts::<(), _>(
            &["$(location :missing)".to_string()],
            |_| Err(anyhow!("not a declared prerequisite")),
        )
        .unwrap_err();
        assert!(format!("{unknown:#}").contains("not a declared prerequisite"));

        let multi = expand_cc_binary_linkopts(
            &["$(location :many)".to_string()],
            |_| {
                Ok(vec![
                    (PathBuf::from("/out/one"), ()),
                    (PathBuf::from("/out/two"), ()),
                ])
            },
        )
        .unwrap_err();
        assert!(multi.to_string().contains("resolved to 2 direct files"));
    }

    #[test]
    fn linkshared_default_info_is_not_a_dependent_link_input() {
        let workspace = PathBuf::from("/workspace");
        let output = PathBuf::from("/output");
        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = Toolchain::default();
        let extra_paths = Vec::new();
        let mut registry = ArtifactRegistry::new();
        let extension = CcBinaryInstance {
            header_inputs_cache: Default::default(),
            name: "binding.so".to_string(),
            srcs: vec!["extension.cc".to_string()],
            deps: Vec::new(),
            copts: Vec::new(),
            linkopts: Vec::new(),
            linkshared: true,
            defines: Vec::new(),
            data: Vec::new(),
        };
        let extension_providers = {
            let mut context = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                "pkg",
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &extra_paths,
            );
            extension.analyze(&mut context, &[])
        };
        let shared_id = extension_providers.get_default_info().unwrap().files[0];
        let extension_providers = Arc::new(extension_providers);
        let labeled = vec![(
            "//pkg:binding.so".to_string(),
            Arc::clone(&extension_providers),
        )];
        let consumer = CcBinaryInstance {
            header_inputs_cache: Default::default(),
            name: "consumer".to_string(),
            srcs: vec!["main.cc".to_string()],
            deps: vec![":binding.so".to_string()],
            copts: Vec::new(),
            linkopts: Vec::new(),
            linkshared: false,
            defines: Vec::new(),
            data: Vec::new(),
        };
        {
            let mut context = AnalysisContext::new(
                TargetId(2),
                ConfigId::DEFAULT,
                "pkg",
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &extra_paths,
            )
            .with_labeled_dep_providers(&labeled);
            let _ = consumer.analyze(&mut context, &[extension_providers]);
        }

        let consumer_link = registry
            .declared_actions()
            .find(|(_, action)| {
                action.owner_target == TargetId(2)
                    && registry.strings().str(action.mnemonic) == "CcLink"
            })
            .map(|(_, action)| action)
            .expect("consumer must declare a link action");
        assert!(!consumer_link.inputs.contains(&shared_id));
    }

    #[tokio::test]
    async fn linkshared_scheduler_and_legacy_specs_match() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let package = workspace.join("pkg");
        let output = temp.path().join("output");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("extension.cc"), "extern int rush_symbol;\n").unwrap();
        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = Toolchain::default();
        let extra_paths = Vec::new();
        let mut registry = ArtifactRegistry::new();
        let rule = CcBinaryInstance {
            header_inputs_cache: Default::default(),
            name: "binding.so".to_string(),
            srcs: vec!["extension.cc".to_string()],
            deps: Vec::new(),
            copts: Vec::new(),
            linkopts: Vec::new(),
            linkshared: true,
            defines: Vec::new(),
            data: Vec::new(),
        };
        {
            let mut context = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                "pkg",
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &extra_paths,
            );
            let _ = rule.analyze(&mut context, &[]);
        }
        let scheduler_specs: HashMap<_, _> = registry
            .declared_actions()
            .filter_map(|(id, action)| {
                let mnemonic = registry.strings().str(action.mnemonic).to_string();
                rush_action::command_spec::NormalizedCommandSpec::from_registry_action(
                    &registry,
                    id,
                )
                .map(|spec| (mnemonic, spec))
            })
            .collect();
        let shared_registry = Arc::new(tokio::sync::Mutex::new(registry));
        let build_context = BuildContext::new(workspace, output)
            .with_package(PathBuf::from("pkg"))
            .with_toolchain(toolchain)
            .with_artifact_registry(shared_registry);
        let legacy_specs = rule.build_legacy_command_specs(&build_context).await;
        for (key, legacy) in legacy_specs {
            let scheduler = scheduler_specs
                .get(&key.mnemonic)
                .unwrap_or_else(|| panic!("missing scheduler {} spec", key.mnemonic));
            assert_eq!(scheduler, &legacy);
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn runtime_linkshared_build_uses_pic_and_shared_linking() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let package = workspace.join("pkg");
        let output = temp.path().join("output");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("extension.cc"),
            "extern \"C\" int rush_extension_value() { return 42; }\n",
        )
        .unwrap();
        let manager = Arc::new(RecordingExecutingActionManager::default());
        let context = BuildContext::new(workspace, output.clone())
            .with_package(PathBuf::from("pkg"))
            .with_action_manager(manager.clone());
        let rule = CcBinaryInstance {
            header_inputs_cache: Default::default(),
            name: "binding.so".to_string(),
            srcs: vec!["extension.cc".to_string()],
            deps: Vec::new(),
            copts: Vec::new(),
            linkopts: Vec::new(),
            linkshared: true,
            defines: Vec::new(),
            data: Vec::new(),
        };

        let artifacts = rule.build(&context).await.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].artifact_type, ArtifactType::SharedLibrary);
        assert_eq!(artifacts[0].path, output.join("binding.so"));
        assert!(artifacts[0].path.is_file());

        let actions = manager.actions.lock().unwrap();
        let compile = actions
            .iter()
            .find(|action| action.mnemonic == "CppCompile")
            .expect("runtime must compile the extension");
        assert!(compile.command.args.iter().any(|arg| arg == "-fPIC"));
        let link = actions
            .iter()
            .find(|action| action.mnemonic == "CppLink")
            .expect("runtime must link the extension");
        assert_eq!(
            link.command.args.iter().filter(|arg| *arg == "-shared").count(),
            1,
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn external_generated_version_script_is_exact_wired_and_linked() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("external-repo");
        let package = workspace.join("pkg");
        let output = temp.path().join("output/external/repo-repo");
        let version_path = output.join("genfiles/pkg/binding-version.lds");
        let dynamic_list_path = package.join("binding-dynamic.list");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(version_path.parent().unwrap()).unwrap();
        std::fs::write(
            package.join("extension.cc"),
            "extern \"C\" int rush_extension_value() { return 42; }\n",
        )
        .unwrap();
        std::fs::write(
            &version_path,
            "{ global: rush_extension_value; local: *; };\n",
        )
        .unwrap();
        std::fs::write(&dynamic_list_path, "{ rush_extension_value; };\n").unwrap();

        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = Toolchain::default();
        let extra_paths = Vec::new();
        let mut registry = ArtifactRegistry::new();
        let version_artifact;
        let dynamic_list_artifact;
        {
            let mut genrule = AnalysisContext::new(
                TargetId(2),
                ConfigId::DEFAULT,
                "pkg",
                Some("repo"),
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &extra_paths,
            );
            version_artifact = genrule.register_generated_artifact(
                Path::new("genfiles/pkg/binding-version.lds"),
                ArtifactKind::Generated,
                None,
            );
            genrule
                .declare_incomplete_action(
                    "GenRule",
                    Vec::new(),
                    vec![version_artifact],
                    "synthetic version-script generator",
                )
                .unwrap();
        }
        {
            let mut source = AnalysisContext::new(
                TargetId(3),
                ConfigId::DEFAULT,
                "pkg",
                Some("repo"),
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &extra_paths,
            );
            dynamic_list_artifact =
                source.register_source_artifact(&dynamic_list_path, ArtifactKind::Source);
        }
        assert_eq!(registry.artifact_path(version_artifact), version_path);
        let version_provider = Arc::new(
            ProviderSet::empty()
                .with_files(FilesInfo::new(vec![version_path.clone()]))
                .with_default_info(DefaultInfo {
                    files: vec![version_artifact],
                }),
        );
        let dynamic_list_provider = Arc::new(
            ProviderSet::empty().with_default_info(DefaultInfo {
                files: vec![dynamic_list_artifact],
            }),
        );
        let labeled = vec![
            (
                "@repo//pkg:binding-version.lds".to_string(),
                Arc::clone(&version_provider),
            ),
            (
                "@repo//pkg:binding-dynamic.list".to_string(),
                Arc::clone(&dynamic_list_provider),
            ),
        ];
        let mut rule = CcBinaryInstance {
            header_inputs_cache: Default::default(),
            name: "binding.so".to_string(),
            srcs: vec!["extension.cc".to_string()],
            deps: vec![
                ":binding-version.lds".to_string(),
                "binding-dynamic.list".to_string(),
            ],
            copts: Vec::new(),
            linkopts: vec![
                "-Wl,--version-script".to_string(),
                "$(location :binding-version.lds)".to_string(),
                "-Wl,--dynamic-list".to_string(),
                "$(execpath binding-dynamic.list)".to_string(),
            ],
            linkshared: true,
            defines: Vec::new(),
            data: Vec::new(),
        };
        {
            let dep_providers = vec![
                Arc::clone(&version_provider),
                Arc::clone(&dynamic_list_provider),
            ];
            let mut analysis = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                "pkg",
                Some("repo"),
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &extra_paths,
            )
            .with_labeled_dep_providers(&labeled);
            for spelling in [
                ":binding-version.lds",
                "binding-version.lds",
                "//pkg:binding-version.lds",
                "@repo//pkg:binding-version.lds",
            ] {
                assert_eq!(
                    cc_binary_analysis_linkopt_files(&analysis, spelling).unwrap(),
                    vec![(version_path.clone(), version_artifact)],
                );
            }
            rule.validate_analysis_dependencies(&analysis).unwrap();
            let _ = rule.analyze(&mut analysis, &dep_providers);
        }

        let (scheduler_link_id, scheduler_link_action) = registry
            .declared_actions()
            .find(|(_, action)| {
                action.owner_target == TargetId(1)
                    && registry.strings().str(action.mnemonic) == "CcLink"
            })
            .expect("cc_binary must declare CcLink");
        assert!(scheduler_link_action.inputs.contains(&version_artifact));
        assert!(scheduler_link_action.inputs.contains(&dynamic_list_artifact));
        let scheduler_spec =
            rush_action::command_spec::NormalizedCommandSpec::from_registry_action(
                &registry,
                scheduler_link_id,
            )
            .expect("location-aware CcLink must have an executable payload");
        assert!(scheduler_spec
            .argv
            .iter()
            .any(|argument| argument == &version_path.display().to_string()));
        assert!(!scheduler_spec
            .argv
            .iter()
            .any(|argument| argument.contains("$(location")));

        let dependency_artifacts = HashMap::from([
            (
                "@repo//pkg:binding-version.lds".to_string(),
                NestedSet::new(
                    vec![Artifact {
                        path: version_path.clone(),
                        artifact_type: ArtifactType::Generated,
                    }],
                    Vec::new(),
                ),
            ),
            (
                "@repo//pkg:binding-dynamic.list".to_string(),
                NestedSet::new(
                    vec![Artifact {
                        path: dynamic_list_path.clone(),
                        artifact_type: ArtifactType::Source,
                    }],
                    Vec::new(),
                ),
            ),
        ]);
        let manager = Arc::new(RecordingExecutingActionManager::default());
        let shared_registry = Arc::new(tokio::sync::Mutex::new(registry));
        let context = BuildContext::new(workspace, output.clone())
            .with_package(PathBuf::from("pkg"))
            .with_repository(Some("repo".to_string()))
            .with_toolchain(toolchain)
            .with_dependencies(
                dependency_artifacts,
                vec![(
                    "@repo//pkg:binding-version.lds".to_string(),
                    version_provider.as_ref().clone_forwarded(),
                ), (
                    "@repo//pkg:binding-dynamic.list".to_string(),
                    dynamic_list_provider.as_ref().clone_forwarded(),
                )],
            )
            .with_artifact_registry(Arc::clone(&shared_registry))
            .with_action_manager(manager.clone());
        let real_linkopts = rule.linkopts.clone();
        for spelling in [
            ":binding-version.lds",
            "binding-version.lds",
            "//pkg:binding-version.lds",
            "@repo//pkg:binding-version.lds",
        ] {
            assert_eq!(
                cc_binary_runtime_linkopt_files(&context, spelling).unwrap(),
                vec![(version_path.clone(), version_path.clone())],
            );
            rule.linkopts = vec![format!("$(location {spelling})")];
            let spelling_link = rule
                .build_legacy_command_specs(&context)
                .await
                .into_iter()
                .find(|(key, _)| key.mnemonic == "CcLink")
                .map(|(_, spec)| spec)
                .expect("legacy verifier must preserve exact external label identity");
            assert!(spelling_link.argv.contains(&version_path.display().to_string()));
            assert!(spelling_link.declared_inputs.contains(&version_path));
        }
        rule.linkopts = real_linkopts;
        let legacy_link = rule
            .build_legacy_command_specs(&context)
            .await
            .into_iter()
            .find(|(key, _)| key.mnemonic == "CcLink")
            .map(|(_, spec)| spec)
            .expect("legacy verifier must describe CcLink");
        assert_eq!(scheduler_spec, legacy_link);

        let artifacts = rule.build(&context).await.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].artifact_type, ArtifactType::SharedLibrary);
        assert!(artifacts[0].path.is_file());
        let actions = manager.actions.lock().unwrap();
        let runtime_link = actions
            .iter()
            .find(|action| action.mnemonic == "CppLink")
            .expect("runtime must execute the shared link");
        assert!(runtime_link.inputs.iter().any(|input| match input {
            ActionArtifact::Source(path) => path == &version_path,
            ActionArtifact::Derived { output_path, .. } => output_path == &version_path,
        }));
        assert!(runtime_link.inputs.iter().any(|input| match input {
            ActionArtifact::Source(path) => path == &dynamic_list_path,
            ActionArtifact::Derived { output_path, .. } => output_path == &dynamic_list_path,
        }));
        assert!(runtime_link
            .command
            .args
            .iter()
            .any(|argument| argument == &version_path.display().to_string()));
        assert!(!runtime_link
            .command
            .args
            .iter()
            .any(|argument| argument.contains("$(location")));
    }

    #[test]
    fn bare_header_adds_only_the_implicit_header_dependency() {
        let rule = binary_with_generated_header();
        assert_eq!(rule.dependencies(), vec![":gen.h"]);
        assert!(is_cc_header_input("detail.cuh"));
        assert!(is_cc_header_input("inline.inl"));

        let archive = CcBinaryInstance {
            header_inputs_cache: Default::default(),
            srcs: vec!["prebuilt.a".to_string()],
            ..binary_with_generated_header()
        };
        assert!(archive.dependencies().is_empty());
    }

    #[tokio::test]
    async fn generated_src_header_preserves_producer_and_verifier_parity() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let package_dir = workspace.join("p");
        let output_dir = temp.path().join("out");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(
            package_dir.join("main.cc"),
            "#include \"gen.h\"\nint main() { return 0; }\n",
        )
        .unwrap();

        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = Toolchain::default();
        let extra_paths = Vec::new();
        let mut registry = ArtifactRegistry::new();
        let generated_header;
        let producer_action;
        let generated_header_path;
        {
            let mut producer = AnalysisContext::new(
                TargetId(2),
                ConfigId::DEFAULT,
                "p",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &extra_paths,
            );
            generated_header = producer.register_generated_artifact(
                Path::new("genfiles/p/gen.h"),
                ArtifactKind::Generated,
                None,
            );
            generated_header_path = producer
                .registry()
                .artifact_path(generated_header)
                .to_path_buf();
            producer_action = producer
                .declare_incomplete_action(
                    "GenRule",
                    Vec::new(),
                    vec![generated_header],
                    "test generator",
                )
                .unwrap();
        }
        std::fs::create_dir_all(generated_header_path.parent().unwrap()).unwrap();
        std::fs::write(&generated_header_path, "#pragma once\n").unwrap();

        let files_provider = Arc::new(
            ProviderSet::empty().with_files(FilesInfo::new(vec![
                generated_header_path.clone(),
            ])),
        );
        let labeled_providers = vec![(
            "//p:gen.h".to_string(),
            Arc::clone(&files_provider),
        )];
        let rule = binary_with_generated_header();
        {
            let mut consumer = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                "p",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &extra_paths,
            )
            .with_labeled_dep_providers(&labeled_providers);
            let _ = rule.analyze(&mut consumer, &[Arc::clone(&files_provider)]);
        }

        assert_eq!(registry.producer(generated_header), Some(producer_action));
        assert!(!registry.artifact(generated_header).is_source);
        let (compile_id, compile_action) = registry
            .declared_actions()
            .find(|(_, action)| registry.strings().str(action.mnemonic) == "CppCompile")
            .expect("cc_binary declares a compile action");
        assert!(compile_action.inputs.contains(&generated_header));
        let scheduler_spec =
            rush_action::command_spec::NormalizedCommandSpec::from_registry_action(
                &registry,
                compile_id,
            )
            .expect("compile action has an executable payload");
        let include_flag = format!("-iquote{}", output_dir.join("genfiles/p").display());
        assert!(scheduler_spec.argv.contains(&include_flag));

        let shared_registry = Arc::new(tokio::sync::Mutex::new(registry));
        let build_context = BuildContext::new(workspace, output_dir)
            .with_package(PathBuf::from("p"))
            .with_toolchain(toolchain)
            .with_dependencies(
                HashMap::new(),
                vec![("//p:gen.h".to_string(), files_provider.as_ref().clone())],
            )
            .with_artifact_registry(shared_registry);
        let legacy_specs = rule.build_legacy_command_specs(&build_context).await;
        let legacy_compile = &legacy_specs
            .iter()
            .find(|(key, _)| key.mnemonic == "CppCompile")
            .expect("legacy specs contain the compile")
            .1;
        assert_eq!(&scheduler_spec, legacy_compile);
    }

    #[tokio::test]
    async fn runtime_build_resolves_nested_generated_header_from_files_info() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let package_dir = workspace.join("p");
        let output_dir = temp.path().join("out");
        let repository_genfiles_root = output_dir.join("genfiles");
        let generated_package_root = repository_genfiles_root.join("p");
        let generated_header = generated_package_root.join("dir/gen.h");
        let competing_header = repository_genfiles_root.join("dir/gen.h");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::create_dir_all(generated_header.parent().unwrap()).unwrap();
        std::fs::create_dir_all(competing_header.parent().unwrap()).unwrap();
        std::fs::write(
            package_dir.join("main.cc"),
            "#include \"dir/gen.h\"\nint main() { return GENERATED_VALUE; }\n",
        )
        .unwrap();
        std::fs::write(&generated_header, "#define GENERATED_VALUE 0\n").unwrap();
        std::fs::write(
            &competing_header,
            "#error repository root must not shadow the owned package header\n",
        )
        .unwrap();

        let provider = ProviderSet::empty()
            .with_files(FilesInfo::new(vec![generated_header.clone()]));
        let manager = Arc::new(ExecutingActionManager);
        let context = BuildContext::new(workspace, output_dir)
            .with_package(PathBuf::from("p"))
            .with_dependencies(
                HashMap::new(),
                vec![("//p:dir/gen.h".to_string(), provider)],
            )
            .with_action_manager(manager);
        let rule = CcBinaryInstance {
            header_inputs_cache: Default::default(),
            srcs: vec!["main.cc".to_string(), "dir/gen.h".to_string()],
            ..binary_with_generated_header()
        };
        let legacy_specs = rule.build_legacy_command_specs(&context).await;
        let compile = &legacy_specs
            .iter()
            .find(|(key, _)| key.mnemonic == "CppCompile")
            .expect("legacy specs contain the compile")
            .1;
        assert!(compile.argv.contains(&format!(
            "-iquote{}",
            generated_package_root.display()
        )));
        let package_quote = format!("-iquote{}", generated_package_root.display());
        let repository_quote = format!("-iquote{}", repository_genfiles_root.display());
        let package_quote_index = compile
            .argv
            .iter()
            .position(|arg| arg == &package_quote)
            .unwrap();
        let repository_quote_index = compile
            .argv
            .iter()
            .position(|arg| arg == &repository_quote)
            .unwrap();
        assert!(package_quote_index < repository_quote_index);
        assert!(!compile.argv.contains(&format!(
            "-I{}",
            generated_package_root.display()
        )));
        assert!(!compile.argv.contains(&format!(
            "-iquote{}",
            generated_header.parent().unwrap().display()
        )));

        let artifacts = rule.build(&context).await.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert!(artifacts[0].path.is_file());
        let status = std::process::Command::new(&artifacts[0].path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn runtime_build_resolves_cross_repository_generated_header() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let package_dir = workspace.join("app");
        let output_dir = temp.path().join("out");
        let external_genfiles = output_dir.join("external/repo-r/genfiles");
        let generated_header = external_genfiles.join("p/gen.h");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::create_dir_all(generated_header.parent().unwrap()).unwrap();
        std::fs::write(
            package_dir.join("main.cc"),
            "#include \"p/gen.h\"\nint main() { return GENERATED_VALUE; }\n",
        )
        .unwrap();
        std::fs::write(&generated_header, "#define GENERATED_VALUE 0\n").unwrap();

        let provider = ProviderSet::empty()
            .with_files(FilesInfo::new(vec![generated_header]));
        let context = BuildContext::new(workspace, output_dir)
            .with_package(PathBuf::from("app"))
            .with_dependencies(
                HashMap::new(),
                vec![("@r//p:gen.h".to_string(), provider)],
            )
            .with_action_manager(Arc::new(ExecutingActionManager));
        let rule = CcBinaryInstance {
            header_inputs_cache: Default::default(),
            srcs: vec!["main.cc".to_string(), "@r//p:gen.h".to_string()],
            ..binary_with_generated_header()
        };
        let legacy_specs = rule.build_legacy_command_specs(&context).await;
        let compile = &legacy_specs
            .iter()
            .find(|(key, _)| key.mnemonic == "CppCompile")
            .expect("legacy specs contain the compile")
            .1;
        assert!(compile
            .argv
            .contains(&format!("-iquote{}", external_genfiles.display())));
        assert!(!compile
            .argv
            .contains(&format!("-I{}", external_genfiles.display())));

        let artifacts = rule.build(&context).await.unwrap();
        let status = std::process::Command::new(&artifacts[0].path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn runtime_cc_binary_uses_package_rooted_include_layout_on_cpu() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let package_dir = workspace.join("app");
        let dependency_header = workspace.join("dep/public.h");
        let external_workspace = temp.path().join("external/repo-r");
        let output_dir = temp.path().join("out");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::create_dir_all(dependency_header.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&external_workspace).unwrap();
        std::fs::write(
            package_dir.join("main.cc"),
            "#include \"dep/public.h\"\nint main() { return DEP_VALUE; }\n",
        )
        .unwrap();
        std::fs::write(&dependency_header, "#define DEP_VALUE 0\n").unwrap();

        let mut registry = ArtifactRegistry::new();
        let header_exec_path = registry.intern_path(&dependency_header);
        let header_artifact = registry.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: header_exec_path,
            },
            ArtifactKind::Header,
            true,
            TargetId(2),
            None,
        );
        let dependency_cc_info = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                headers: vec![header_artifact],
                ..DirectCcInfo::default()
            },
            std::iter::empty::<Arc<CcInfo>>(),
        );
        let dependency_provider = ProviderSet::empty().with_cc_info(dependency_cc_info);
        let manager = Arc::new(RecordingExecutingActionManager::default());
        let shared_registry = Arc::new(tokio::sync::Mutex::new(registry));
        let context = BuildContext::new(workspace.clone(), output_dir.clone())
            .with_package(PathBuf::from("app"))
            .with_dependencies(
                HashMap::new(),
                vec![("//dep:public".to_string(), dependency_provider)],
            )
            .with_extra_proto_paths(vec![external_workspace.clone()])
            .with_artifact_registry(shared_registry)
            .with_action_manager(manager.clone());
        let rule = CcBinaryInstance {
            header_inputs_cache: Default::default(),
            name: "tool".to_string(),
            srcs: vec!["main.cc".to_string()],
            deps: vec!["//dep:public".to_string()],
            copts: Vec::new(),
            linkopts: Vec::new(),
            linkshared: false,
            defines: Vec::new(),
            data: Vec::new(),
        };

        let legacy_compile = rule
            .build_legacy_command_specs(&context)
            .await
            .into_iter()
            .find(|(key, _)| key.mnemonic == "CppCompile")
            .map(|(_, spec)| spec)
            .expect("legacy specs contain the compile");
        for include in [
            workspace.clone(),
            external_workspace.clone(),
            output_dir.join("genfiles"),
        ] {
            assert!(legacy_compile
                .argv
                .contains(&format!("-I{}", include.display())));
        }

        let artifacts = rule.build(&context).await.unwrap();
        let status = std::process::Command::new(&artifacts[0].path)
            .status()
            .unwrap();
        assert!(status.success());

        let actions = manager.actions.lock().unwrap();
        let compile = actions
            .iter()
            .find(|action| action.mnemonic == "CppCompile")
            .expect("runtime must execute the compile");
        for include in [
            workspace,
            external_workspace,
            output_dir.join("genfiles"),
        ] {
            let flag = format!("-I{}", include.display());
            assert_eq!(
                compile.command.args.iter().filter(|arg| *arg == &flag).count(),
                1,
                "package-rooted include must appear exactly once: {flag}"
            );
        }
        assert!(compile.inputs.iter().any(|input| {
            matches!(input, ActionArtifact::Source(path) if path == &dependency_header)
        }));
    }

    #[tokio::test]
    async fn owned_source_header_cannot_shadow_system_angle_header() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let package_dir = workspace.join("p");
        let output_dir = temp.path().join("out");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(
            package_dir.join("main.cc"),
            "#include <time.h>\nint main() { time_t value = 0; return int(value); }\n",
        )
        .unwrap();
        std::fs::write(
            package_dir.join("time.h"),
            "#error local time.h must not satisfy an angle include\n",
        )
        .unwrap();

        let package_root = package_dir.clone();
        let context = BuildContext::new(workspace, output_dir)
            .with_package(PathBuf::from("p"))
            .with_action_manager(Arc::new(ExecutingActionManager));
        let rule = CcBinaryInstance {
            header_inputs_cache: Default::default(),
            srcs: vec!["main.cc".to_string(), "time.h".to_string()],
            ..binary_with_generated_header()
        };
        let legacy_specs = rule.build_legacy_command_specs(&context).await;
        let compile = &legacy_specs
            .iter()
            .find(|(key, _)| key.mnemonic == "CppCompile")
            .expect("legacy specs contain the compile")
            .1;
        assert!(compile
            .argv
            .contains(&format!("-iquote{}", package_root.display())));
        assert!(!compile
            .argv
            .contains(&format!("-I{}", package_root.display())));

        let artifacts = rule.build(&context).await.unwrap();
        let status = std::process::Command::new(&artifacts[0].path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn linker_only_plan(args: Vec<String>) -> CcBinaryLogicalLinkPlan {
        let mut plan = CcBinaryLogicalLinkPlan::default();
        for arg in args {
            plan.push_linker(vec![arg.clone()], vec![arg]);
        }
        plan
    }

    fn expected_response_path(contents: &str) -> PathBuf {
        cc_binary_response_path(
            Path::new(".rush-cache/action-inputs/cc-binary"),
            contents,
        )
    }

    #[test]
    fn response_transport_pins_threshold_quoting_and_content_identity() {
        let binary_path = Path::new("out/tool");
        let inline = vec!["x".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD - 1)];
        let inline_plan = linker_only_plan(inline.clone());
        assert_eq!(
            cc_binary_link_transport(&inline_plan, binary_path),
            CcBinaryLinkTransport::Inline { spawn_argv: inline }
        );

        let over_exact = vec!["x".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD)];
        assert_eq!(
            over_exact.iter().map(|arg| arg.len() + 1).sum::<usize>(),
            CC_BINARY_RESPONSE_FILE_THRESHOLD + 1
        );
        let over_exact_plan = linker_only_plan(over_exact);
        assert!(matches!(
            cc_binary_link_transport(&over_exact_plan, binary_path),
            CcBinaryLinkTransport::LinkerResponse { .. }
        ));
        assert_eq!(
            cc_binary_driver_state_component_bytes("''''"),
            4 + 3 + 3 * 4
        );

        let quoted = vec![
            "a b".to_string(),
            "a\"b".to_string(),
            "a\\b".to_string(),
            "x".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD),
        ];
        let quoted_plan = linker_only_plan(quoted);
        let CcBinaryLinkTransport::LinkerResponse {
            spawn_argv,
            response_files,
        } = cc_binary_link_transport(&quoted_plan, binary_path)
        else {
            panic!("large argv must spill");
        };
        assert_eq!(response_files.len(), 1);
        let response = &response_files[0];
        assert!(response.contents.starts_with("\"a b\"\n\"a\\\"b\"\n\"a\\\\b\"\n"));
        assert!(response.contents.ends_with('\n'));
        assert_eq!(response.path, expected_response_path(&response.contents));
        assert_eq!(
            spawn_argv,
            vec![format!("-Wl,@{}", response.path.display())]
        );

        let CcBinaryLinkTransport::LinkerResponse {
            response_files: exe_files,
            ..
        } = cc_binary_link_transport(&quoted_plan, Path::new("out/tool.exe"))
        else {
            panic!("large argv must spill");
        };
        assert_eq!(response.path, exe_files[0].path);
    }

    #[test]
    fn response_transport_preserves_order_and_empty_wl_fields() {
        let object = format!("/objects/{}.o", "x".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD));
        let output = "out/tool";
        let plan = cc_binary_logical_link_plan(
            &[object.clone()],
            &["-fuse-ld=bfd".to_string(), "-pthread".to_string(), "-pthread".to_string()],
            &["-Wl,a,,b".to_string()],
            &[
                "-no-canonical-prefixes".to_string(),
                "-resource-dir=/clang/resources".to_string(),
                "-Wl,--after-driver".to_string(),
            ],
            &[],
            &[],
            output,
        );
        let CcBinaryLinkTransport::LinkerResponse {
            spawn_argv,
            response_files,
        } = cc_binary_link_transport(&plan, Path::new(output))
        else {
            panic!("provenance-known linker spans must spill");
        };

        assert_eq!(response_files.len(), 3);
        assert_eq!(response_files[0].contents, format!("\"{object}\"\n"));
        assert_eq!(
            response_files[0].path,
            expected_response_path(&response_files[0].contents)
        );
        assert_eq!(response_files[1].contents, "\"a\"\n\"\"\n\"b\"\n");
        assert_eq!(
            response_files[1].path,
            expected_response_path(&response_files[1].contents)
        );
        assert_eq!(response_files[2].contents, "\"--after-driver\"\n");
        assert_eq!(
            response_files[2].path,
            expected_response_path(&response_files[2].contents)
        );
        assert_eq!(
            spawn_argv,
            vec![
                format!("-Wl,@{}", response_files[0].path.display()),
                "-fuse-ld=bfd".to_string(),
                "-pthread".to_string(),
                "-pthread".to_string(),
                format!("-Wl,@{}", response_files[1].path.display()),
                "-no-canonical-prefixes".to_string(),
                "-resource-dir=/clang/resources".to_string(),
                format!("-Wl,@{}", response_files[2].path.display()),
                "-o".to_string(),
                output.to_string(),
            ]
        );

        assert!(matches!(
            cc_binary_link_transport_in(
                &plan,
                Path::new(output),
                Path::new("out/with,comma"),
            ),
            CcBinaryLinkTransport::Unsupported {
                reason: CC_BINARY_UNSAFE_RESPONSE_PATH,
                ..
            }
        ));
    }

    #[test]
    fn response_transport_rejects_global_driver_barriers() {
        let output = "out/tool";
        let object = format!("/objects/{}.o", "x".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD));
        let cases = [
            vec!["-specs", "custom.specs"],
            vec!["-specs=custom.specs"],
            vec!["-lifetime-safety-max-cfg-blocks=10"],
            vec!["-lazy_framework", "Cocoa"],
            vec!["--unknown-driver-option"],
        ];

        for raw_args in cases {
            let args: Vec<String> = raw_args.into_iter().map(str::to_string).collect();
            let plan = cc_binary_logical_link_plan(
                std::slice::from_ref(&object),
                &[],
                &[],
                &args,
                &[],
                &["-Wl,--after-barrier".to_string()],
                output,
            );
            assert!(matches!(
                cc_binary_link_transport(&plan, Path::new(output)),
                CcBinaryLinkTransport::Unsupported {
                    reason: CC_BINARY_OPAQUE_DRIVER_ARGUMENT,
                    ..
                }
            ));

            let mut small_plan = CcBinaryLogicalLinkPlan::default();
            append_cc_binary_link_args(&mut small_plan, &args, false);
            assert_eq!(
                cc_binary_link_transport(&small_plan, Path::new(output)),
                CcBinaryLinkTransport::Inline {
                    spawn_argv: args.clone(),
                }
            );
        }

        // Propagated cc_library linkopts are opaque LinkFragment::Flag values,
        // not a typed linker stream. Preserve that provenance so a paired
        // Clang option cannot be split by a response marker before its value.
        let propagated = vec![
            CcBinaryDepArg::opaque("-lazy_framework"),
            CcBinaryDepArg::opaque("Cocoa"),
        ];
        let propagated_plan = cc_binary_logical_link_plan_with_dep_args(
            &[object],
            &[],
            &propagated,
            &[],
            &[],
            &[],
            output,
        );
        assert!(matches!(
            cc_binary_link_transport(&propagated_plan, Path::new(output)),
            CcBinaryLinkTransport::Unsupported {
                reason: CC_BINARY_OPAQUE_DRIVER_ARGUMENT,
                ..
            }
        ));
    }

    #[test]
    fn response_transport_rejects_undeclared_external_response_files() {
        let output = "out/tool";
        let cases = [
            vec!["@driver.rsp"],
            vec!["-Xlinker", "@driver.rsp"],
            vec!["-l", "@driver.rsp"],
            vec!["-L", "@driver.rsp"],
            vec!["-framework", "@driver.rsp"],
            vec!["--sysroot", "@driver.rsp"],
            vec!["-Wl,@linker.rsp"],
            vec!["--for-linker=@linker.rsp"],
            vec!["--unknown-driver-option", "@driver.rsp"],
        ];
        for raw_args in cases {
            let args: Vec<String> = raw_args.into_iter().map(str::to_string).collect();
            let mut plan = CcBinaryLogicalLinkPlan::default();
            append_cc_binary_link_args(&mut plan, &args, false);
            assert!(matches!(
                cc_binary_link_transport(&plan, Path::new(output)),
                CcBinaryLinkTransport::Unsupported {
                    reason: CC_BINARY_EXTERNAL_RESPONSE_FILE,
                    ..
                }
            ));
        }

        let fast_linker_plan = cc_binary_logical_link_plan(
            &[],
            &["-Wl,--threads=1,@external.rsp".to_string()],
            &[],
            &[],
            &[],
            &[],
            output,
        );
        assert!(matches!(
            cc_binary_link_transport(&fast_linker_plan, Path::new(output)),
            CcBinaryLinkTransport::Unsupported {
                reason: CC_BINARY_EXTERNAL_RESPONSE_FILE,
                ..
            }
        ));
    }

    #[test]
    fn response_transport_rejects_nul_only_when_spilling_is_required() {
        let binary_path = Path::new("out/tool");
        let nul = "--defsym=before\0after".to_string();
        let small = linker_only_plan(vec![nul.clone()]);
        assert_eq!(
            cc_binary_link_transport(&small, binary_path),
            CcBinaryLinkTransport::Inline {
                spawn_argv: vec![nul.clone()],
            }
        );

        let large = linker_only_plan(vec![
            nul,
            "x".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD),
        ]);
        assert!(matches!(
            cc_binary_link_transport(&large, binary_path),
            CcBinaryLinkTransport::Unsupported {
                reason: CC_BINARY_UNREPRESENTABLE_RESPONSE_ARGUMENT,
                ..
            }
        ));
    }

    #[test]
    fn response_transport_keeps_raw_libraries_driver_side() {
        let output = "out/tool";
        let object = format!("/objects/{}.o", "x".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD));
        let plan = cc_binary_logical_link_plan(
            &[object.clone()],
            &[],
            &["-lpthread".to_string()],
            &[
                "-Xlinker".to_string(),
                "--as-needed".to_string(),
                "--for-linker=--hash-style=gnu".to_string(),
                "-Wl,--gc-sections".to_string(),
            ],
            &[],
            &[],
            output,
        );
        let CcBinaryLinkTransport::LinkerResponse {
            spawn_argv,
            response_files,
        } = cc_binary_link_transport(&plan, Path::new(output))
        else {
            panic!("typed libraries and ordinary linker options must spill");
        };
        assert_eq!(response_files.len(), 2);
        assert_eq!(
            response_files[0].contents,
            format!("\"{object}\"\n")
        );
        assert_eq!(
            response_files[1].contents,
            "\"--as-needed\"\n\"--hash-style=gnu\"\n\"--gc-sections\"\n"
        );
        assert_eq!(
            spawn_argv,
            vec![
                format!("-Wl,@{}", response_files[0].path.display()),
                "-lpthread".to_string(),
                format!("-Wl,@{}", response_files[1].path.display()),
                "-o".to_string(),
                output.to_string(),
            ]
        );

        let raw_driver_options = vec![
            "-l".to_string(),
            "m".to_string(),
            "-L/lib".to_string(),
            "-framework".to_string(),
            "Cocoa".to_string(),
            "-F/Frameworks".to_string(),
        ];
        let mut raw_plan = CcBinaryLogicalLinkPlan::default();
        append_cc_binary_link_args(&mut raw_plan, &raw_driver_options, true);
        assert!(raw_plan
            .atoms
            .iter()
            .all(|atom| matches!(atom, CcBinaryLinkAtom::DriverKnown { .. })));
        assert_eq!(raw_plan.inline_argv(), raw_driver_options);
    }

    #[test]
    fn response_transport_pins_residual_and_driver_state_limits() {
        let binary_path = Path::new("out/tool");
        let marker_bytes = format!(
            "-Wl,@{}",
            expected_response_path("\"linker\"\n").display()
        )
        .len()
            + 1;
        let driver_len = CC_BINARY_RESPONSE_FILE_THRESHOLD - marker_bytes - 1;

        let make_plan = |extra: usize| {
            let mut plan = CcBinaryLogicalLinkPlan::default();
            let linker = "l".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD);
            plan.push_linker(vec![linker.clone()], vec![linker]);
            plan.push_driver(vec!["d".repeat(driver_len + extra)]);
            plan
        };

        let exact = cc_binary_link_transport(&make_plan(0), binary_path);
        assert!(matches!(
            exact,
            CcBinaryLinkTransport::Unsupported {
                reason: CC_BINARY_OVERSIZED_DRIVER_STATE,
                spawn_bytes,
                driver_state_bytes,
                ..
            } if spawn_bytes == CC_BINARY_RESPONSE_FILE_THRESHOLD
                && driver_state_bytes > CC_BINARY_RESPONSE_FILE_THRESHOLD
        ));

        let over = cc_binary_link_transport(&make_plan(1), binary_path);
        assert!(matches!(
            over,
            CcBinaryLinkTransport::Unsupported {
                spawn_bytes,
                ..
            } if spawn_bytes == CC_BINARY_RESPONSE_FILE_THRESHOLD + 1
        ));
    }

    #[test]
    fn response_transport_rejects_oversized_collect2_state() {
        let output = "out/tool";
        let search_dirs = vec!["-Lx".to_string(); 12_000];
        let search_plan = cc_binary_logical_link_plan(
            &["main.o".to_string()],
            &[],
            &[],
            &search_dirs,
            &[],
            &[],
            output,
        );
        let logical_argv = search_plan.inline_argv();
        assert!(
            logical_argv.iter().map(|arg| arg.len() + 1).sum::<usize>()
                < CC_BINARY_RESPONSE_FILE_THRESHOLD
        );
        assert!(
            cc_binary_inline_driver_state_bytes(&search_plan, Path::new(output))
                > CC_BINARY_RESPONSE_FILE_THRESHOLD
        );
        assert!(matches!(
            cc_binary_link_transport(&search_plan, Path::new(output)),
            CcBinaryLinkTransport::Unsupported {
                reason: CC_BINARY_OVERSIZED_DRIVER_STATE,
                ..
            }
        ));

        // GCC 15 synthesizes a distinct -foffload-options component for each
        // explicit -lm. The raw argv remains small here, but an inline driver
        // invocation would overflow collect2's single environment string.
        let libraries = vec!["-lm".to_string(); 4_600];
        let library_plan = cc_binary_logical_link_plan(
            &["main.o".to_string()],
            &[],
            &libraries,
            &[],
            &[],
            &[],
            output,
        );
        assert!(
            library_plan
                .inline_argv()
                .iter()
                .map(|arg| arg.len() + 1)
                .sum::<usize>()
                < CC_BINARY_RESPONSE_FILE_THRESHOLD
        );
        assert!(
            cc_binary_inline_driver_state_bytes(
                &library_plan,
                Path::new(output),
            ) > CC_BINARY_RESPONSE_FILE_THRESHOLD
        );
        assert!(matches!(
            cc_binary_link_transport(&library_plan, Path::new(output)),
            CcBinaryLinkTransport::Unsupported {
                reason: CC_BINARY_OVERSIZED_DRIVER_STATE,
                ..
            }
        ));

        // The same spelling in opaque user linkopts remains a driver barrier
        // because Clang has unrelated joined -l* options. Its conservative
        // state bound must still account for GCC's synthesized -l state.
        let opaque_library_plan = cc_binary_logical_link_plan(
            &["main.o".to_string()],
            &[],
            &[],
            &libraries,
            &[],
            &[],
            output,
        );
        assert!(
            cc_binary_inline_driver_state_bytes(
                &opaque_library_plan,
                Path::new(output),
            ) > CC_BINARY_RESPONSE_FILE_THRESHOLD
        );
        assert!(matches!(
            cc_binary_link_transport(&opaque_library_plan, Path::new(output)),
            CcBinaryLinkTransport::Unsupported {
                reason: CC_BINARY_OPAQUE_DRIVER_ARGUMENT,
                driver_state_bytes,
                ..
            } if driver_state_bytes > CC_BINARY_RESPONSE_FILE_THRESHOLD
        ));

        let driver_flags = vec!["-pthread".to_string(); 10_000];
        let driver_plan = cc_binary_logical_link_plan(
            &["main.o".to_string()],
            &driver_flags,
            &[],
            &[],
            &[],
            &[],
            output,
        );
        assert!(
            driver_plan
                .inline_argv()
                .iter()
                .map(|arg| arg.len() + 1)
                .sum::<usize>()
                < CC_BINARY_RESPONSE_FILE_THRESHOLD
        );
        assert!(matches!(
            cc_binary_link_transport(&driver_plan, Path::new(output)),
            CcBinaryLinkTransport::Unsupported {
                reason: CC_BINARY_OVERSIZED_DRIVER_STATE,
                ..
            }
        ));

        let opaque_flags = vec!["-g".to_string(); 27_000];
        let mut opaque_plan = CcBinaryLogicalLinkPlan::default();
        append_cc_binary_link_args(&mut opaque_plan, &opaque_flags, false);
        assert!(
            opaque_plan
                .inline_argv()
                .iter()
                .map(|arg| arg.len() + 1)
                .sum::<usize>()
                < CC_BINARY_RESPONSE_FILE_THRESHOLD
        );
        assert!(matches!(
            cc_binary_link_transport(&opaque_plan, Path::new(output)),
            CcBinaryLinkTransport::Unsupported {
                reason: CC_BINARY_OPAQUE_DRIVER_ARGUMENT,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn spilled_response_bytes_are_action_inputs_and_change_the_id() {
        let temp = tempfile::tempdir().unwrap();
        let output_dir = temp.path().join("out");
        let manager = Arc::new(CapturingActionManager::default());
        let workspace_root = temp.path().join("workspace");
        let context = BuildContext::new(workspace_root.clone(), output_dir.clone())
            .with_action_manager(manager.clone());
        let binary_path = output_dir.join("tool");
        tokio::fs::create_dir_all(&output_dir).await.unwrap();
        let legacy_collision = output_dir.join("tool.link.rsp");
        tokio::fs::write(&legacy_collision, b"declared target output")
            .await
            .unwrap();

        let first = binary_with_linkopt(format!(
            "-Wl,--defsym,marker={}",
            "a".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD)
        ));
        let second = binary_with_linkopt(format!(
            "-Wl,--defsym,marker={}",
            "b".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD)
        ));
        first
            .link_binary(&context, &[], &binary_path)
            .await
            .unwrap();
        second
            .link_binary(&context, &[], &binary_path)
            .await
            .unwrap();

        let actions = manager.actions.lock().unwrap();
        assert_eq!(actions.len(), 2);
        assert_ne!(actions[0].command.args, actions[1].command.args);
        let response_root = cc_binary_response_root(&workspace_root);
        let response_paths: Vec<Vec<PathBuf>> = actions
            .iter()
            .map(|action| {
                let paths: Vec<PathBuf> = action
                    .inputs
                    .iter()
                    .filter_map(|input| match input {
                        ActionArtifact::Source(path) if path.starts_with(&response_root) => {
                            Some(path.clone())
                        }
                        _ => None,
                    })
                    .collect();
                assert!(
                    !paths.is_empty(),
                    "spilled action must declare response inputs"
                );
                paths
            })
            .collect();
        assert_ne!(response_paths[0], response_paths[1]);
        for (action, paths) in actions.iter().zip(&response_paths) {
            for response_path in paths {
                assert!(action
                    .command
                    .args
                    .iter()
                    .any(|arg| arg == &format!("-Wl,@{}", response_path.display())));
            }
        }
        assert_ne!(actions[0].env, actions[1].env);
        assert!(actions[0]
            .env
            .contains_key("RUSH_CC_BINARY_RESPONSE_SET_BLAKE3"));
        assert_ne!(actions[0].id, actions[1].id);
        let first_id = actions[0].id.clone();
        drop(actions);

        assert_eq!(
            tokio::fs::read(&legacy_collision).await.unwrap(),
            b"declared target output"
        );
        for response_path in response_paths.iter().flatten() {
            let bytes = tokio::fs::read(response_path).await.unwrap();
            assert_eq!(
                response_path.file_stem().unwrap().to_string_lossy(),
                blake3::hash(&bytes).to_hex().to_string()
            );
            assert!(tokio::fs::metadata(response_path)
                .await
                .unwrap()
                .permissions()
                .readonly());
        }

        // Reusing an existing correct immutable blob keeps the action ID.
        first
            .link_binary(&context, &[], &binary_path)
            .await
            .unwrap();
        let actions = manager.actions.lock().unwrap();
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[2].id, first_id);
        drop(actions);

        // A retired producer sidecar still fails closed before the mini-CAS
        // publishes anything. Its presence marks a collision between reserved
        // response state and an old output location.
        let stamped = binary_with_linkopt(format!(
            "-Wl,--defsym,marker={}",
            "c".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD)
        ));
        let stamped_driver_flags = fast_linker_driver_flags(
            &[],
            &[],
            &stamped.linkopts,
            &context.toolchain.ldflags,
        );
        let stamped_dep_flags = group_link_flags(Vec::new());
        let stamped_plan = cc_binary_logical_link_plan(
            &[],
            &stamped_driver_flags,
            &stamped_dep_flags,
            &stamped.linkopts,
            &[],
            &[],
            &binary_path.to_string_lossy(),
        );
        let CcBinaryLinkTransport::LinkerResponse {
            response_files: stamped_files,
            ..
        } = cc_binary_link_transport_in(
            &stamped_plan,
            &binary_path,
            &response_root,
        )
        else {
            panic!("stamped fixture must spill");
        };
        tokio::fs::create_dir_all(stamped_files[0].path.parent().unwrap())
            .await
            .unwrap();
        let stale_stamp = rush_action::artifact_value::ArtifactFingerprint([7u8; 32])
            .to_stamp();
        tokio::fs::write(
            cc_binary_response_stamp_path(&stamped_files[0].path),
            stale_stamp,
        )
        .await
        .unwrap();
        let error = stamped
            .link_binary(&context, &[], &binary_path)
            .await
            .expect_err("a stamped response blob must fail before publication");
        assert!(error
            .to_string()
            .contains(CC_BINARY_RESPONSE_PRODUCER_COLLISION));
        assert!(!stamped_files[0].path.exists());
        assert_eq!(manager.actions.lock().unwrap().len(), 3);

        // A corrupt blob under a valid content-addressed name fails before an
        // action is admitted; materialization never truncates it in place.
        let corrupt_path = response_paths[1]
            .iter()
            .find(|path| !response_paths[0].contains(path))
            .expect("changed response body must select a distinct blob");
        let mut permissions = tokio::fs::metadata(corrupt_path)
            .await
            .unwrap()
            .permissions();
        permissions.set_readonly(false);
        tokio::fs::set_permissions(corrupt_path, permissions)
            .await
            .unwrap();
        tokio::fs::write(corrupt_path, b"corrupt").await.unwrap();
        let mut permissions = tokio::fs::metadata(corrupt_path)
            .await
            .unwrap()
            .permissions();
        permissions.set_readonly(true);
        tokio::fs::set_permissions(corrupt_path, permissions)
            .await
            .unwrap();
        let error = second
            .link_binary(&context, &[], &binary_path)
            .await
            .expect_err("a corrupt content-addressed response must fail closed");
        assert!(error
            .to_string()
            .contains(CC_BINARY_RESPONSE_CONTENT_COLLISION));
        assert_eq!(tokio::fs::read(corrupt_path).await.unwrap(), b"corrupt");
        assert_eq!(manager.actions.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn response_blob_publication_is_atomic_for_concurrent_writers() {
        let temp = tempfile::tempdir().unwrap();
        let contents = format!("\"{}\"\n", "x".repeat(64 * 1024));
        let response_file = CcBinaryResponseFile {
            path: cc_binary_response_path(temp.path(), &contents),
            contents,
        };
        let first = response_file.clone();
        let second = response_file.clone();
        let (first_result, second_result) = tokio::join!(
            materialize_cc_binary_response_file(&first),
            materialize_cc_binary_response_file(&second),
        );
        first_result.unwrap();
        second_result.unwrap();
        assert_eq!(
            tokio::fs::read(&response_file.path).await.unwrap(),
            response_file.contents.as_bytes()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn response_blob_rejects_symlinks_without_touching_the_victim() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("responses");
        tokio::fs::create_dir_all(&root).await.unwrap();
        let contents = "\"--gc-sections\"\n".to_string();
        let response_file = CcBinaryResponseFile {
            path: cc_binary_response_path(&root, &contents),
            contents: contents.clone(),
        };
        let victim = temp.path().join("victim");
        tokio::fs::write(&victim, contents.as_bytes())
            .await
            .unwrap();
        let permissions_before = tokio::fs::metadata(&victim)
            .await
            .unwrap()
            .permissions();
        symlink(&victim, &response_file.path).unwrap();

        let error = materialize_cc_binary_response_file(&response_file)
            .await
            .expect_err("a response blob symlink must fail closed");
        assert!(error.to_string().contains(CC_BINARY_RESPONSE_PATH_TYPE));
        assert_eq!(
            tokio::fs::read(&victim).await.unwrap(),
            contents.as_bytes()
        );
        assert_eq!(
            tokio::fs::metadata(&victim)
                .await
                .unwrap()
                .permissions()
                .readonly(),
            permissions_before.readonly()
        );
    }

    #[tokio::test]
    async fn unsupported_transport_never_reaches_the_action_manager() {
        let temp = tempfile::tempdir().unwrap();
        let output_dir = temp.path().join("out");
        let manager = Arc::new(CapturingActionManager::default());
        let context = BuildContext::new(temp.path().join("workspace"), output_dir.clone())
            .with_action_manager(manager.clone());
        let rule = binary_with_linkopt("opaque".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD));
        let binary_path = output_dir.join("tool");

        let error = rule
            .link_binary(&context, &[], &binary_path)
            .await
            .expect_err("an oversized opaque driver suffix must fail closed");
        assert!(error
            .to_string()
            .contains("cc_binary link argv cannot be spilled safely"));
        assert!(manager.actions.lock().unwrap().is_empty());
        assert!(!output_dir.join("tool.link.rsp").exists());

        let external = binary_with_linkopt("-Wl,@external.rsp".to_string());
        let error = external
            .link_binary(&context, &[], &binary_path)
            .await
            .expect_err("an undeclared response-file input must fail closed");
        assert!(error
            .to_string()
            .contains(CC_BINARY_EXTERNAL_RESPONSE_FILE));
        assert!(manager.actions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires a host C++ compiler and linker"]
    async fn linker_response_executes_above_the_threshold() {
        let temp = tempfile::tempdir().unwrap();
        let output_dir = temp.path().join("out");
        tokio::fs::create_dir_all(&output_dir).await.unwrap();
        let source_path = temp.path().join("main.cc");
        let object_path = temp.path().join("main.o");
        tokio::fs::write(&source_path, b"int main() { return 0; }\n")
            .await
            .unwrap();

        let toolchain = Toolchain::default();
        let compile = std::process::Command::new(&toolchain.cxx)
            .arg("-c")
            .arg(&source_path)
            .arg("-o")
            .arg(&object_path)
            .output()
            .unwrap();
        assert!(
            compile.status.success(),
            "fixture compile failed: {}",
            String::from_utf8_lossy(&compile.stderr)
        );

        let linkopts: Vec<String> = (0..6_000)
            .map(|index| format!("-Wl,--defsym,rush_response_pad_{index}=1"))
            .collect();
        assert!(
            linkopts.iter().map(|arg| arg.len() + 1).sum::<usize>()
                > CC_BINARY_RESPONSE_FILE_THRESHOLD
        );
        let rule = CcBinaryInstance {
            header_inputs_cache: Default::default(),
            name: "tool".to_string(),
            srcs: Vec::new(),
            deps: Vec::new(),
            copts: Vec::new(),
            linkopts,
            linkshared: false,
            defines: Vec::new(),
            data: Vec::new(),
        };
        let context = BuildContext::new(temp.path().join("workspace"), output_dir.clone())
            .with_toolchain(toolchain)
            .with_action_manager(Arc::new(ExecutingActionManager));
        let binary_path = output_dir.join("tool");
        rule.link_binary(&context, &[object_path], &binary_path)
            .await
            .unwrap();

        assert!(binary_path.is_file());
        assert!(std::process::Command::new(&binary_path)
            .status()
            .unwrap()
            .success());
        let response_root = cc_binary_response_root(&context.workspace_root);
        let response_files: Vec<PathBuf> = std::fs::read_dir(response_root)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().and_then(|ext| ext.to_str()) == Some("rsp")
            })
            .collect();
        // A detected driver-only flag such as -fuse-ld=mold may split the
        // object and user-linker spans into two response files. Both shapes
        // exercise the real compiler/linker transport.
        assert!(!response_files.is_empty());
        let response_bodies: String = response_files
            .iter()
            .map(|path| std::fs::read_to_string(path).unwrap())
            .collect();
        assert!(response_bodies.contains("rush_response_pad_0"));
        assert!(response_bodies.contains("rush_response_pad_5999"));
    }

    #[tokio::test]
    async fn legacy_spec_records_the_response_path_and_content_digest() {
        let temp = tempfile::tempdir().unwrap();
        let manager = Arc::new(CapturingActionManager::default());
        let context = BuildContext::new(
            temp.path().join("workspace"),
            temp.path().join("out"),
        )
        .with_action_manager(manager);
        let mut first = binary_with_linkopt(format!(
            "-Wl,--defsym,marker={}",
            "a".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD)
        ));
        first.srcs.push("main.cc".to_string());
        let mut second = binary_with_linkopt(format!(
            "-Wl,--defsym,marker={}",
            "b".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD)
        ));
        second.srcs.push("main.cc".to_string());

        let first_specs = first.build_legacy_command_specs(&context).await;
        let second_specs = second.build_legacy_command_specs(&context).await;
        let first_link = &first_specs
            .iter()
            .find(|(key, _)| key.mnemonic == "CcLink")
            .expect("legacy specs must contain the final link")
            .1;
        let second_link = &second_specs
            .iter()
            .find(|(key, _)| key.mnemonic == "CcLink")
            .expect("legacy specs must contain the final link")
            .1;

        assert_ne!(first_link.argv, second_link.argv);
        assert!(!first_link.param_files.is_empty());
        assert_eq!(first_link.param_files.len(), second_link.param_files.len());
        assert_ne!(
            first_link
                .param_files
                .iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>(),
            second_link
                .param_files
                .iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>()
        );
        assert!(first_link
            .param_files
            .iter()
            .zip(&second_link.param_files)
            .any(|((_, first_digest), (_, second_digest))| first_digest != second_digest));
        for (path, _) in &first_link.param_files {
            let response_arg = format!("-Wl,@{}", path.display());
            assert!(first_link.argv.iter().any(|arg| arg == &response_arg));
        }
        for (path, digest) in first_link
            .param_files
            .iter()
            .chain(&second_link.param_files)
        {
            assert_eq!(
                path.file_stem().unwrap().to_string_lossy(),
                digest.as_str()
            );
        }

        // Runtime has two additional legacy-only inputs today: ROCm flags and
        // the NestedSet archive fallback. With both absent, the spec digest
        // must describe the exact response bytes that link_binary writes.
        if std::env::var("RUSH_ENABLE_ROCM").as_deref() != Ok("1") {
            let object_path = context.output_dir.join("tool_objs/main.o");
            tokio::fs::create_dir_all(object_path.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(&object_path, b"object fixture")
                .await
                .unwrap();
            first
                .link_binary(
                    &context,
                    &[object_path],
                    &context.output_dir.join("tool"),
                )
                .await
                .unwrap();
            for (path, digest) in &first_link.param_files {
                let runtime_bytes = tokio::fs::read(path).await.unwrap();
                assert_eq!(*digest, blake3::hash(&runtime_bytes).to_hex().to_string());
            }
        }
    }

    #[test]
    fn typed_response_shape_is_incomplete_and_has_no_executable_payload() {
        let mut registry = ArtifactRegistry::new();
        let resolver = StubIncludeResolver::new(PathBuf::from("/ws"));
        let toolchain = Toolchain::default();
        let workspace_root = PathBuf::from("/ws");
        let output_dir = PathBuf::from("/ws/rush-out");
        let extra_paths = Vec::new();
        let mut context = AnalysisContext::new(
            TargetId(1),
            ConfigId::DEFAULT,
            "",
            None,
            &output_dir,
            &mut registry,
            &resolver,
            &toolchain,
            &workspace_root,
            &extra_paths,
        );
        let mut rule = binary_with_linkopt(format!(
            "-Wl,--defsym,marker={}",
            "x".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD)
        ));
        rule.srcs.push("main.cc".to_string());
        let providers = rule.analyze(&mut context, &[]);
        let default = providers
            .get_default_info()
            .expect("cc_binary must publish its executable");
        assert_eq!(default.files.len(), 1);
        drop(context);

        let (link_id, _, reason) = registry
            .incomplete_actions()
            .find(|(_, action, _)| {
                registry.strings().str(action.mnemonic) == "CcLink"
            })
            .expect("oversized link must be declared incomplete");
        assert_eq!(
            reason,
            "cc_binary response-file materialization is not typed"
        );
        assert!(registry.executable_action(link_id).is_none());

        let mut unsupported_registry = ArtifactRegistry::new();
        let mut unsupported_context = AnalysisContext::new(
            TargetId(2),
            ConfigId::DEFAULT,
            "",
            None,
            &output_dir,
            &mut unsupported_registry,
            &resolver,
            &toolchain,
            &workspace_root,
            &extra_paths,
        );
        let mut unsupported =
            binary_with_linkopt("opaque".repeat(CC_BINARY_RESPONSE_FILE_THRESHOLD));
        unsupported.srcs.push("main.cc".to_string());
        let _ = unsupported.analyze(&mut unsupported_context, &[]);
        drop(unsupported_context);
        let (unsupported_id, _, unsupported_reason) = unsupported_registry
            .incomplete_actions()
            .find(|(_, action, _)| {
                unsupported_registry.strings().str(action.mnemonic) == "CcLink"
            })
            .expect("oversized opaque link must be declared incomplete");
        assert_eq!(
            unsupported_reason,
            "cc_binary opaque driver argument prevents semantics-preserving spill"
        );
        assert!(unsupported_registry
            .executable_action(unsupported_id)
            .is_none());
    }
}

/// cc_test rule - C++ test binary (treated same as cc_binary)
pub struct CcTestRule;

#[async_trait]
impl BuildRule for CcTestRule {
    fn name(&self) -> &str {
        "cc_test"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        // cc_test has same attributes as cc_binary
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs")?;
        let deps = extract_string_list(attrs, "deps")?;
        let copts = extract_string_list(attrs, "copts")?;
        let linkopts = extract_string_list(attrs, "linkopts")?;
        let mut defines = extract_string_list(attrs, "defines")?;
        // `local_defines` apply to THIS target's own sources (Bazel does not propagate
        // them, unlike `defines`). rush appends them to `defines` so they reach the own
        // compile; the minor over-propagation to consumers is benign for TF/XLA's value
        // feature-flags — e.g. xla/stream_executor/cuda:ptx_compiler_support sets
        // LIBNVPTXCOMPILER_SUPPORT=false via local_defines, undefined (compile error)
        // without this.
        defines.extend(extract_string_list(attrs, "local_defines")?);
        let data = extract_string_list(attrs, "data")?;

        Ok(Box::new(CcTestInstance {
            name,
            srcs,
            deps,
            copts,
            linkopts,
            defines,
            data,
        }))
    }

    fn documentation(&self) -> &str {
        "C++ test rule - compiles and links C++ test sources into a test executable"
    }
}

/// Instance of a cc_test rule
pub struct CcTestInstance {
    name: String,
    srcs: Vec<String>,
    deps: Vec<String>,
    copts: Vec<String>,
    linkopts: Vec<String>,
    defines: Vec<String>,
    /// `data` — runtime deps republished into runfiles + built via deps.
    data: Vec<String>,
}

#[async_trait]
impl RuleInstance for CcTestInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        let mut out = deps_with_src_labels(&self.deps, &self.srcs);
        append_data_labels(&mut out, &self.data);
        out
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        // cc_test builds same as cc_binary - just creates an executable
        // In a full implementation, we would also run the test, but for now
        // we just compile and link it like a binary
        let mut artifacts = Vec::new();

        // Create output directory for this target
        let target_output_dir = context.output_dir.join(format!("{}_objs", self.name));
        std::fs::create_dir_all(&target_output_dir)?;

        // Compile each source file to object file.
        let mut object_files = Vec::new();
        // Keep runtime inputs aligned with analyze()/PASS-B for generated
        // headers, and only carry dependency source headers that the test source
        // can textually reach.
        let header_inputs = collect_sink_compile_header_inputs(context).await;
        let expanded_srcs =
            expand_filegroup_srcs(&self.srcs, |l| context.get_dependency_files(l));
        for src in &expanded_srcs {
            // Skip non-compilable srcs (headers in srcs) — see the PCH-clobber
            // note above; compiling a .h emits a precompiled header that
            // collides with the same-stem .cc object.
            if !crate::cc_command_spec::is_compilable_source(src) {
                continue;
            }
            let src_path = context.source_path(src);

            // Determine object file name
            let obj_name = Path::new(src)
                .file_stem()
                .ok_or_else(|| anyhow!("Invalid source file: {}", src))?
                .to_string_lossy()
                .to_string();
            let obj_path = target_output_dir.join(format!("{}.o", obj_name));

            // Compile the source file
            let mut compile_header_inputs = header_inputs.generated.clone();
            compile_header_inputs
                .extend(reachable_source_headers_for_sink_source(&src_path, &header_inputs));
            if header_inputs.generated.is_empty()
                && !context.dirty_paths().is_empty()
                && obj_path.exists()
                && !context.path_is_dirty(&src_path)
                && compile_header_inputs
                    .iter()
                    .all(|p| !context.path_is_dirty(p))
            {
                object_files.push(obj_path);
                continue;
            }
            self.compile_source(context, &src_path, &obj_path, &compile_header_inputs)
                .await?;

            object_files.push(obj_path);
        }

        // Link object files into test executable
        let test_binary_path = context.output_dir.join(&self.name);
        self.link_binary(context, &object_files, &test_binary_path)
            .await?;

        artifacts.push(Artifact {
            path: test_binary_path,
            artifact_type: ArtifactType::Executable,
        });

        Ok(artifacts)
    }

    /// Phase 5.C0 + 5.C.4: declare CppCompile per source + CcLink
    /// for the test executable AND populate ExecutableAction so the
    /// scheduler can dispatch them. cc_test's argv is intentionally
    /// minimal compared to cc_library/cc_binary (just cxxflags +
    /// copts + defines + -c -o -src for compile, no -fPIC /
    /// -std=c++17 / -D_GNU_SOURCE / transitive includes). Both
    /// scheduler analyze() and legacy compile_source produce the
    /// same minimal shape so the verifier sees zero divergence.
    fn analyze(
        &self,
        ctx: &mut crate::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        use rush_action::ids::ArtifactKind;
        use std::path::Path;
        if !self.srcs.is_empty() {
            // P0: generated headers in compile inputs.
            let dep_cc_infos = crate::providers::extract_cc_infos(dep_providers);
            let generated_header_inputs =
                crate::providers::collect_generated_header_inputs(
                    ctx.registry(),
                    &dep_cc_infos,
                );

            // Pre-collect immutable strings before per-src loop.
            // Same borrow-collect pattern as cc_library / cc_binary.
            let cxxflags: Vec<String> = ctx.toolchain().cxxflags.clone();
            let cxx_pathbuf = ctx.toolchain().cxx.clone();
            // Transitive dep `defines` (Bazel postorder) for the test's
            // own compile. Pre-collected here (like cxxflags) so the
            // per-src emit matches the legacy for_test_compile spec.
            let dep_test_defines: Vec<String> = crate::providers::flatten_transitive_defines(
                ctx.registry().strings(),
                dep_cc_infos.iter().map(|cc| cc.compilation.defines.inner()),
            );
            // cc_test link: collect dep linker_input paths +
            // link_fragments inline. Mirrors cc_test::link_binary's
            // legacy argv shape.
            let mut dep_link_input_paths: Vec<std::path::PathBuf> = Vec::new();
            let mut seen_dep_link_input_paths: HashSet<std::path::PathBuf> =
                HashSet::new();
            let mut dep_flags: Vec<String> = Vec::new();
            let mut seen_dep_lib_paths: HashSet<String> = HashSet::new();
            {
                use crate::providers::LinkFragment;
                let reg = ctx.registry();
                for dep_cc in dep_cc_infos.iter() {
                    for lid in dep_cc.linking.linker_inputs.iter() {
                        let li = reg.linker_input(lid);
                        let lib_str =
                            reg.artifact_path(li.artifact).display().to_string();
                        let lib_pathbuf = reg.artifact_path(li.artifact).to_path_buf();
                        if seen_dep_link_input_paths.insert(lib_pathbuf.clone()) {
                            dep_link_input_paths.push(lib_pathbuf);
                        }
                        if li.alwayslink {
                            dep_flags.push("-Wl,--whole-archive".to_string());
                            if seen_dep_lib_paths.insert(lib_str.clone()) {
                                dep_flags.push(lib_str);
                            }
                            dep_flags.push("-Wl,--no-whole-archive".to_string());
                        } else if seen_dep_lib_paths.insert(lib_str.clone()) {
                            dep_flags.push(lib_str);
                        }
                    }
                    for frag in dep_cc.linking.link_fragments.iter() {
                        match frag {
                            LinkFragment::Flag(sid) => {
                                dep_flags.push(reg.strings().str(sid).to_string());
                            }
                            LinkFragment::SearchDir(pid) => {
                                dep_flags.push(format!(
                                    "-L{}",
                                    reg.paths().path(pid).display()
                                ));
                            }
                            LinkFragment::Library(lid) => {
                                let li = reg.linker_input(lid);
                                let p = reg.artifact_path(li.artifact).display().to_string();
                                if seen_dep_lib_paths.insert(p.clone()) {
                                    dep_flags.push(p);
                                }
                            }
                            LinkFragment::Group(lids) => {
                                dep_flags.push("-Wl,--start-group".to_string());
                                for lid in lids.iter() {
                                    let li = reg.linker_input(*lid);
                                    let p =
                                        reg.artifact_path(li.artifact).display().to_string();
                                    dep_flags.push(p);
                                }
                                dep_flags.push("-Wl,--end-group".to_string());
                            }
                        }
                    }
                }
            }
            let ldflags: Vec<String> = ctx.toolchain().ldflags.clone();

            let mut obj_artifacts: Vec<_> = Vec::with_capacity(self.srcs.len());
            let expanded_srcs = expand_filegroup_srcs(&self.srcs, |l| {
                ctx.get_dependency_provider(l)
                    .and_then(|p| p.get_files())
                    .map(|fi| fi.files.clone())
            });
            for src in &expanded_srcs {
                // Skip headers in srcs (PCH-clobber; see build path). Don't
                // declare a CppCompile for a non-TU.
                if !crate::cc_command_spec::is_compilable_source(src) {
                    continue;
                }
                // Register the source at its package-qualified absolute path so a
                // same-basename src in another package (e.g. each package's own
                // `main.cc`) gets a DISTINCT artifact whose freshness ledger entry
                // and action-id content-hash track the real file — see the
                // same-basename incremental-correctness fix in cc_library::analyze.
                // The compile argv uses its own `src_path_abs` below, so parity is
                // unchanged.
                let pkg_for_src = ctx.package().to_string();
                let src_abs_for_reg = if pkg_for_src.is_empty() {
                    ctx.workspace_root().join(src)
                } else {
                    ctx.workspace_root().join(&pkg_for_src).join(src)
                };
                let src_aid =
                    ctx.register_source_artifact(&src_abs_for_reg, ArtifactKind::Source);
                let obj_name = format!(
                    "{}_objs/{}.o",
                    self.name,
                    Path::new(src)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("obj"),
                );
                let obj_aid = ctx.register_generated_artifact(
                    Path::new(&obj_name),
                    ArtifactKind::Object,
                    None,
                );
                obj_artifacts.push(obj_aid);
                let mut compile_inputs = vec![src_aid];
                compile_inputs.extend(generated_header_inputs.iter().copied());
                if let Ok(compile_id) = ctx.declare_action(
                    "CppCompile",
                    compile_inputs,
                    vec![obj_aid],
                ) {
                    let pkg = ctx.package();
                    let src_path_abs = if pkg.is_empty() {
                        ctx.workspace_root().join(src)
                    } else {
                        ctx.workspace_root().join(pkg).join(src)
                    };
                    let src_path_str = src_path_abs.display().to_string();
                    let obj_path_str = ctx
                        .registry()
                        .artifact_path(obj_aid)
                        .display()
                        .to_string();

                    // cc_test's minimal compile argv:
                    //   cxxflags + copts + -D defines + -c src -o obj
                    let mut argv_strs: Vec<String> = Vec::new();
                    argv_strs.extend(cxxflags.iter().cloned());
                    argv_strs.extend(self.copts.iter().cloned());
                    for d in crate::providers::merged_compile_defines(
                        &dep_test_defines,
                        &self.defines,
                    ) {
                        argv_strs.push(format!("-D{}", d));
                    }
                    argv_strs.push("-c".to_string());
                    argv_strs.push(src_path_str);
                    argv_strs.push("-o".to_string());
                    argv_strs.push(obj_path_str);

                    let exe_pid = ctx.intern_path(&cxx_pathbuf);
                    let argv: Vec<rush_action::ids::StringId> = argv_strs
                        .iter()
                        .map(|s| ctx.intern_string(s))
                        .collect();
                    ctx.set_executable_action(
                        compile_id,
                        rush_action::ids::ExecutableAction {
                            executable: exe_pid,
                            argv: argv.into_boxed_slice(),
                            env: Box::new([]),
                            working_dir: None,
                            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                        },
                    );
                }
            }

            // CcLink declaration + ExecutableAction.
            let mut link_inputs: Vec<rush_action::ids::ArtifactId> = obj_artifacts.clone();
            for dep_cc in dep_cc_infos.iter() {
                for lid in dep_cc.linking.linker_inputs.iter() {
                    let li = ctx.registry().linker_input(lid);
                    link_inputs.push(li.artifact);
                }
            }
            let bin_aid = ctx.register_generated_artifact(
                Path::new(&self.name),
                ArtifactKind::Executable,
                None,
            );
            // Collect obj path strings for link argv.
            let obj_path_strs: Vec<String> = obj_artifacts
                .iter()
                .map(|aid| ctx.registry().artifact_path(*aid).display().to_string())
                .collect();
            let object_paths: Vec<std::path::PathBuf> = obj_artifacts
                .iter()
                .map(|aid| ctx.registry().artifact_path(*aid).to_path_buf())
                .collect();
            let bin_path_str = ctx.registry().artifact_path(bin_aid).display().to_string();

            if let Ok(link_id) = ctx.declare_action("CcLink", link_inputs, vec![bin_aid]) {
                // cc_test's link argv:
                //   objs + dep_flags + linkopts + ldflags + -o bin
                let mut argv_strs: Vec<String> = Vec::new();
                argv_strs.extend(obj_path_strs);
                argv_strs.extend(fast_linker_driver_flags(
                    &object_paths,
                    &dep_flags,
                    &self.linkopts,
                    &ldflags,
                ));
                argv_strs.extend(dep_flags.iter().cloned());
                argv_strs.extend(self.linkopts.iter().cloned());
                argv_strs.extend(ldflags.iter().cloned());
                argv_strs.push("-o".to_string());
                argv_strs.push(bin_path_str);
                normalize_bazel_origin_link_args(&mut argv_strs);

                let exe_pid = ctx.intern_path(&cxx_pathbuf);
                let argv: Vec<rush_action::ids::StringId> = argv_strs
                    .iter()
                    .map(|s| ctx.intern_string(s))
                    .collect();
                ctx.set_executable_action(
                    link_id,
                    rush_action::ids::ExecutableAction {
                        executable: exe_pid,
                        argv: argv.into_boxed_slice(),
                        env: Box::new([]),
                        working_dir: None,
                        env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                    },
                );
            }

            // Phase 5.C.4: cc_test's public output (DefaultInfo)
            // is the test executable.
            return ProviderSet::empty().with_default_info(
                crate::providers::DefaultInfo {
                    files: vec![bin_aid],
                },
            );
        }
        ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "cc_test"
    }

    /// Phase 5.C.4 commit 4: legacy spec builder mirrors
    /// compile_source + link_binary. cc_test's argv is minimal
    /// (no -fPIC / -std / -D_GNU_SOURCE / transitive includes)
    /// so the spec builder is much shorter than cc_binary's.
    async fn build_legacy_command_specs(
        &self,
        context: &BuildContext,
    ) -> Vec<(crate::SchedulerActionKey, rush_action::command_spec::NormalizedCommandSpec)> {
        use crate::cc_command_spec::CcCompileCommandSpec;
        use crate::providers::LinkFragment;
        use crate::SchedulerActionKey;
        use rush_action::command_spec::NormalizedCommandSpec;
        let mut specs = Vec::new();
        if self.srcs.is_empty() {
            return specs;
        }

        // Collect dep_flags + dep_link_input_paths from typed
        // CcInfo (mirrors what cc_test's link_binary derives via
        // NestedSet at runtime — typed source is what matches
        // scheduler analyze()).
        let mut dep_flags: Vec<String> = Vec::new();
        let mut dep_link_input_paths: Vec<std::path::PathBuf> = Vec::new();
        // Phase 5.C.4 commit 27: close cc_test's latent
        // dep-generated-header gap (same class as commits 14, 25
        // for cc_library / cc_binary / tf_cc_library). cc_test in
        // hello_cpp didn't currently have a generated-header dep
        // so the verifier hadn't surfaced the bug, but the helper
        // is now centralized — apply uniformly to prevent the
        // fourth occurrence.
        let mut dep_generated_header_paths: Vec<std::path::PathBuf> = Vec::new();
        // Transitive dep `defines` for the test's own compile. Bazel's
        // `defines` attribute is transitive, so the test .cc must see
        // the same `-D`s its deps export (e.g. eigen_contraction_kernel's
        // TENSORFLOW_USE_MKLDNN_CONTRACTION_KERNEL — the test body is
        // `#if defined(...)`-guarded). Collected from the ORDERED dep
        // source so this matches analyze()'s flatten exactly.
        let mut dep_test_defines: Vec<String> = Vec::new();
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            // Generated-header walk via the commit-27 helper.
            // BuildContext.dependency_providers is HashMap<String,
            // ProviderSet> (not Arc-wrapped); collect cc_info Arcs
            // by walking the map directly.
            let cc_infos: Vec<std::sync::Arc<crate::providers::CcInfo>> = context
                .dependency_providers
                .values()
                .filter_map(|p| p.cc_info.clone())
                .collect();
            dep_generated_header_paths =
                crate::providers::collect_generated_header_paths(&reg, &cc_infos);

            let ordered_cc_infos: Vec<std::sync::Arc<crate::providers::CcInfo>> = context
                .dependency_providers_ordered
                .iter()
                .filter_map(|(_k, p)| p.cc_info.clone())
                .collect();
            dep_test_defines = crate::providers::flatten_transitive_defines(
                reg.strings(),
                ordered_cc_infos.iter().map(|cc| cc.compilation.defines.inner()),
            );

            for (_dep_key, providers) in &context.dependency_providers_ordered {
                if let Some(cc_info) = providers.get_cc_info() {
                    let linkc = &cc_info.linking;
                    for lid in linkc.linker_inputs.iter() {
                        let li = reg.linker_input(lid);
                        let lib_str =
                            reg.artifact_path(li.artifact).display().to_string();
                        let lib_pathbuf = reg.artifact_path(li.artifact).to_path_buf();
                        if !dep_link_input_paths.contains(&lib_pathbuf) {
                            dep_link_input_paths.push(lib_pathbuf);
                        }
                        if li.alwayslink {
                            dep_flags.push("-Wl,--whole-archive".to_string());
                            dep_flags.push(lib_str);
                            dep_flags.push("-Wl,--no-whole-archive".to_string());
                        } else if !dep_flags.contains(&lib_str) {
                            dep_flags.push(lib_str);
                        }
                    }
                    for frag in linkc.link_fragments.iter() {
                        match frag {
                            LinkFragment::Flag(sid) => {
                                dep_flags.push(reg.strings().str(sid).to_string());
                            }
                            LinkFragment::SearchDir(pid) => {
                                dep_flags.push(format!(
                                    "-L{}",
                                    reg.paths().path(pid).display()
                                ));
                            }
                            LinkFragment::Library(lid) => {
                                let li = reg.linker_input(lid);
                                let p =
                                    reg.artifact_path(li.artifact).display().to_string();
                                if !dep_flags.contains(&p) {
                                    dep_flags.push(p);
                                }
                            }
                            LinkFragment::Group(lids) => {
                                dep_flags.push("-Wl,--start-group".to_string());
                                for lid in lids.iter() {
                                    let li = reg.linker_input(*lid);
                                    let p = reg
                                        .artifact_path(li.artifact)
                                        .display()
                                        .to_string();
                                    dep_flags.push(p);
                                }
                                dep_flags.push("-Wl,--end-group".to_string());
                            }
                        }
                    }
                }
            }
        }

        let target_output_dir = context.output_dir.join(format!("{}_objs", self.name));
        let mut object_paths: Vec<std::path::PathBuf> = Vec::new();
        // Expand filegroup-label srcs to their member files (mirrors
        // cc_library); a filegroup label otherwise stays a literal path.
        let expanded_srcs =
            expand_filegroup_srcs(&self.srcs, |l| context.get_dependency_files(l));
        for src in &expanded_srcs {
            // Headers (.h/.inc) listed in srcs are owned-but-not-compiled
            // (Bazel semantics) — compiling one with `-c` emits a PCH that,
            // sharing the .cc's file stem, clobbers the real object → link
            // "file format not recognized". Mirrors cc_library's filter.
            if !crate::cc_command_spec::is_compilable_source(src) {
                continue;
            }
            let src_path = context.source_path(src);
            let obj_name = src_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "obj".to_string());
            let obj_path = target_output_dir.join(format!("{}.o", obj_name));
            object_paths.push(obj_path.clone());

            // Phase 5.C.4 commit 19: cc_test compile spec via
            // CcCompileCommandSpec::for_test_compile.
            // Phase 5.C.4 commit 27: declared_input_paths now
            // includes dep generated headers (was just [src_path]
            // — latent bug class same as commits 14, 25).
            let mut compile_inputs = vec![src_path.clone()];
            compile_inputs.extend(dep_generated_header_paths.iter().cloned());
            let spec = CcCompileCommandSpec::for_test_compile(
                context.toolchain.cxx.clone(),
                context.toolchain.cxxflags.clone(),
                self.copts.clone(),
                self.defines.clone(),
                dep_test_defines.clone(),
                src_path.clone(),
                obj_path.clone(),
                compile_inputs,
            );
            specs.push((
                SchedulerActionKey {
                    mnemonic: "CppCompile".to_string(),
                    primary_output_path: obj_path,
                },
                spec,
            ));
        }

        // CcLink spec.
        let bin_path = context.output_dir.join(&self.name);
        let mut link_argv: Vec<String> = Vec::new();
        for o in &object_paths {
            link_argv.push(o.display().to_string());
        }
        let test_linkopts = self.linkopts.clone();
        link_argv.extend(fast_linker_driver_flags(
            &object_paths,
            &dep_flags,
            &test_linkopts,
            &context.toolchain.ldflags,
        ));
        link_argv.extend(dep_flags.iter().cloned());
        link_argv.extend(self.linkopts.iter().cloned());
        link_argv.extend(context.toolchain.ldflags.iter().cloned());
        link_argv.push("-o".to_string());
        link_argv.push(bin_path.display().to_string());
        normalize_bazel_origin_link_args(&mut link_argv);

        let mut link_declared_inputs: Vec<std::path::PathBuf> = object_paths.clone();
        link_declared_inputs.extend(dep_link_input_paths.iter().cloned());
        let mut link_spec = NormalizedCommandSpec {
            mnemonic: "CcLink".to_string(),
            executable: context.toolchain.cxx.clone(),
            cwd: None,
            argv: link_argv,
            env: Vec::new(),
            declared_inputs: link_declared_inputs,
            declared_outputs: vec![bin_path.clone()],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
        };
        link_spec.normalize();
        specs.push((
            SchedulerActionKey {
                mnemonic: "CcLink".to_string(),
                primary_output_path: bin_path,
            },
            link_spec,
        ));

        specs
    }
}

impl CcTestInstance {
    /// Compile a single source file to an object file
    async fn compile_source(
        &self,
        context: &BuildContext,
        src_path: &Path,
        obj_path: &Path,
        // Dep generated headers plus any source headers reachable from this TU.
        header_inputs: &[PathBuf],
    ) -> Result<()> {
        // #39 test-corpus: cc_test compiles like cc_binary/cc_library — it needs
        // the FULL TF include layout (workspace-root + dep CcInfo include_dirs +
        // genfiles + pinned absl/protobuf, -fPIC/-std=c++17/-D_GNU_SOURCE), NOT
        // the old minimal argv (which left every test unable to find even its
        // own package's headers, e.g. "tensorflow/core/platform/enable_tf2_utils.h").
        // Mirrors CcLibraryInstance::compile_source (Codex 019eb36b).
        let mut args = Vec::new();
        let is_c_source = src_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("c"))
            .unwrap_or(false);
        args.push("-c".to_string());
        args.push("-fPIC".to_string());
        if !is_c_source {
            args.push("-std=c++17".to_string());
            if !crate::cc_command_spec::is_assembly_source_path(src_path) {
                args.push("-include".to_string());
                args.push("cstdint".to_string());
            }
            if deps_want_llvm_no_rtti(&self.deps) {
                args.push("-fno-rtti".to_string());
            }
        }
        #[cfg(target_os = "linux")]
        {
            args.push("-D_GNU_SOURCE".to_string());
            for extra in ["/usr/include/jsoncpp", "/usr/include/x86_64-linux-gnu"] {
                if std::path::Path::new(extra).exists() {
                    args.push(format!("-I{}", extra));
                }
            }
        }
        for opt in &self.copts {
            // Drop unexpanded `$(VAR)` make-variable copts (see other copts
            // loops) so a literal $(...) never reaches g++.
            if opt.contains("$(") {
                continue;
            }
            if is_c_source && opt.starts_with("-std=c++") {
                continue;
            }
            if !is_c_source && opt.starts_with("-std=c") && !opt.starts_with("-std=c++") {
                continue;
            }
            args.push(opt.clone());
        }
        if !is_c_source {
            for flag in &context.toolchain.cxxflags {
                args.push(flag.clone());
            }
        } else {
            for flag in &context.toolchain.cflags {
                args.push(flag.clone());
            }
        }
        // Transitive dep defines (Bazel `defines` is transitive via CcInfo)
        // before own direct defines — same merge as analyze()/PASS-B.
        let dep_defines: Vec<String> = if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            let ordered_cc_infos: Vec<std::sync::Arc<crate::providers::CcInfo>> = context
                .dependency_providers_ordered
                .iter()
                .filter_map(|(_k, p)| p.cc_info.clone())
                .collect();
            crate::providers::flatten_transitive_defines(
                reg.strings(),
                ordered_cc_infos.iter().map(|cc| cc.compilation.defines.inner()),
            )
        } else {
            Vec::new()
        };
        for define in crate::providers::merged_compile_defines(&dep_defines, &self.defines) {
            args.push(format!("-D{}", define));
        }
        // Transitive dep include dirs from ordered CcInfo (NOT a HashMap walk —
        // order matters; matches cc_library).
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            for (_dep_key, providers) in &context.dependency_providers_ordered {
                if let Some(cc_info) = providers.get_cc_info() {
                    for path_id in cc_info.compilation.include_dirs.iter() {
                        let flag = format!("-I{}", reg.paths().path(path_id).display());
                        if !args.contains(&flag) {
                            args.push(flag);
                        }
                    }
                }
            }
        }
        // Workspace-root + external roots + genfiles (Bazel #include "pkg/..."
        // convention) — this is what resolves the test's own package headers.
        args.push(format!("-I{}", context.workspace_root.display()));
        for extra in context.extra_proto_paths.iter() {
            args.push(format!("-I{}", extra.display()));
        }
        args.push(format!("-I{}", context.output_dir.join("genfiles").display()));
        crate::cc_command_spec::demote_host_system_includes(&mut args);

        if std::env::var("RUSH_DUMP_CC_ARGV").is_ok() {
            eprintln!("RUSH_CC_ARGV(bin) src={} args={:?}", src_path.display(), args);
        }

        args.push("-o".to_string());
        args.push(obj_path.to_string_lossy().to_string());
        args.push(src_path.to_string_lossy().to_string());

        let command = Command {
            executable: if is_c_source {
                context.toolchain.cc.clone()
            } else {
                context.toolchain.cxx.clone()
            },
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };

        // Action inputs: the source, dependency-generated headers, and any
        // dependency source headers this TU can textually reach. Dependency
        // source headers that only affect producer libraries stay out of this
        // sink key, avoiding root object misses on private header edits.
        let mut inputs = vec![ActionArtifact::Source(src_path.to_path_buf())];
        for hp in header_inputs {
            if hp.exists() {
                inputs.push(ActionArtifact::Source(hp.clone()));
            }
        }

        // Compute action ID
        let action_id = Action::compute_id(&inputs, &command, &HashMap::new())?;
        let outputs = vec![ActionArtifact::Source(obj_path.to_path_buf())];

        let action = Action {
            id: action_id,
            command,
            env: HashMap::new(),
            inputs,
            outputs,
            mnemonic: "CppCompile".to_string(),
        };

        let result = context.execute_action(action).await?;

        if result.exit_code != 0 {
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(anyhow!(
                "Compilation failed for {}: {}",
                src_path.display(),
                stderr
            ));
        }

        Ok(())
    }

    /// Link object files into test executable
    async fn link_binary(
        &self,
        context: &BuildContext,
        object_files: &[PathBuf],
        binary_path: &Path,
    ) -> Result<()> {
        let mut args = Vec::new();

        // Add object files
        for obj in object_files {
            args.push(obj.to_string_lossy().to_string());
        }

        let mut input_files = object_files.to_vec();

        // #39 test-corpus: alwayslink-FIRST link ordering (Codex 019eb3f9).
        // In TF's monolithic-static build the SAME source can be compiled into
        // both a standalone cc_library (e.g. :sparse_tensor → libsparse_tensor.a)
        // AND an `alwayslink=1` rollup (framework_internal_impl, via filegroups).
        // A test that deps on both then has sparse_tensor.o reachable from each.
        // GNU ld only *multiply-defines* when BOTH archive members are extracted:
        // the alwayslink lib is force-included (--whole-archive), and if the
        // standalone .a is scanned BEFORE it, ld pulls the standalone's member to
        // satisfy the test's direct reference, then --whole-archive forces the
        // rollup's copy too → "multiple definition". Bazel avoids this purely by
        // ORDER (verified by Codex's repro): emit the --whole-archive (alwayslink)
        // libs FIRST so they define the shared symbols up front, then the normal
        // archives lazily in a --start-group; the normal lib's duplicate member is
        // never extracted because the symbol is already defined. No pruning, so
        // partial-overlap is handled correctly (non-duplicate members still pull).
        //
        // The --start-group around the normal archives is still needed: GNU ld is
        // single-pass and a member like tsl::PosixFileSystem / tsl::crc32c::Extend
        // is referenced by an archive appearing earlier on the line. The -lgtest /
        // -lbenchmark / -labsl_* / -ldouble-conversion flags from the system-lib
        // stubs (CcInfo link_fragments) are collected as pass-through and emitted
        // after the archives that reference them.
        let mut whole_archive_libs: Vec<std::path::PathBuf> = Vec::new();
        let mut whole_archive_seen: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        let mut normal_libs: Vec<std::path::PathBuf> = Vec::new();
        let mut normal_seen: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        let mut passthrough_flags: Vec<String> = Vec::new();
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            for dep in &self.deps {
                if let Some(providers) = context.get_dependency_providers(dep) {
                    if let Some(cc_info) = providers.get_cc_info() {
                        let linkc = &cc_info.linking;
                        for lid in linkc.linker_inputs.iter() {
                            let li = reg.linker_input(lid);
                            let lib_path = reg.artifact_path(li.artifact).to_path_buf();
                            if li.alwayslink {
                                push_unique_path(
                                    &mut whole_archive_libs,
                                    &mut whole_archive_seen,
                                    lib_path.clone(),
                                );
                            } else if !whole_archive_seen.contains(&lib_path) {
                                push_unique_path(
                                    &mut normal_libs,
                                    &mut normal_seen,
                                    lib_path.clone(),
                                );
                            }
                            input_files.push(lib_path);
                        }
                        use crate::providers::LinkFragment;
                        for frag in linkc.link_fragments.iter() {
                            match frag {
                                LinkFragment::Flag(sid) => {
                                    passthrough_flags.push(reg.strings().str(sid).to_string());
                                }
                                LinkFragment::SearchDir(pid) => {
                                    passthrough_flags
                                        .push(format!("-L{}", reg.paths().path(pid).display()));
                                }
                                LinkFragment::Library(lid) => {
                                    let li = reg.linker_input(lid);
                                    let p = reg.artifact_path(li.artifact).to_path_buf();
                                    if !whole_archive_seen.contains(&p) {
                                        push_unique_path(
                                            &mut normal_libs,
                                            &mut normal_seen,
                                            p,
                                        );
                                    }
                                }
                                LinkFragment::Group(lids) => {
                                    for lid in lids.iter() {
                                        let li = reg.linker_input(*lid);
                                        let p = reg.artifact_path(li.artifact).to_path_buf();
                                        if !whole_archive_seen.contains(&p) {
                                            push_unique_path(
                                                &mut normal_libs,
                                                &mut normal_seen,
                                                p,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            drop(reg);
        }

        // Transitive walk via NestedSet — every other .a in the closure.
        for dep in &self.deps {
            if let Some(iter) = context.iter_transitive_artifacts(dep) {
                for artifact in iter {
                    if artifact.artifact_type == ArtifactType::StaticLibrary
                        && !whole_archive_seen.contains(&artifact.path)
                        && normal_seen.insert(artifact.path.clone())
                    {
                        normal_libs.push(artifact.path.clone());
                        input_files.push(artifact.path.clone());
                    }
                }
            }
        }

        // A library can be advertised alwayslink by one dep and plain by another;
        // the alwayslink (whole-archive) treatment wins, so drop any such path
        // from the normal bucket to avoid double-listing the same archive.
        normal_libs.retain(|p| !whole_archive_seen.contains(p));

        // Emit whole-archive (alwayslink) libs FIRST so they define shared symbols
        // before the lazy normal archives are scanned.
        for lib in &whole_archive_libs {
            args.push("-Wl,--whole-archive".to_string());
            args.push(lib.to_string_lossy().to_string());
            args.push("-Wl,--no-whole-archive".to_string());
        }
        if !normal_libs.is_empty() {
            args.push("-Wl,--start-group".to_string());
            for lib in &normal_libs {
                args.push(lib.to_string_lossy().to_string());
            }
            args.push("-Wl,--end-group".to_string());
        }
        // tf_cc_test harness: tsl/platform:test_main (linked by every TF cc_test)
        // unconditionally calls benchmark::Initialize / RunSpecifiedBenchmarks /
        // PrintDefaultHelp. Google Benchmark is only a TRANSITIVE dep of test_main
        // and its -lbenchmark system-lib linkopt does NOT reach this link via any
        // dep CcInfo link_fragment (gtest works only because gtest_main is a
        // DIRECT test dep). When libtest_main.a is in the closure, append the
        // system libbenchmark arm (Debian libbenchmark-dev; same HONEST system-lib
        // model as the -lgtest arm). ONLY -lbenchmark (NOT -lbenchmark_main — it
        // defines its own main(), which would collide with test_main.o's main()).
        // Pushed into passthrough_flags so it lands AFTER the archives (below),
        // where ld.bfd resolves test_main.o's benchmark::* references.
        let links_test_main = normal_libs
            .iter()
            .chain(whole_archive_libs.iter())
            .any(|p| p.to_string_lossy().contains("test_main"))
            || input_files
                .iter()
                .any(|p| p.to_string_lossy().contains("test_main"));
        if links_test_main && !passthrough_flags.iter().any(|f| f == "-lbenchmark") {
            passthrough_flags.push("-lbenchmark".to_string());
            passthrough_flags.push("-lpthread".to_string());
        }

        let mut dep_flags: Vec<String> = Vec::new();
        if !whole_archive_libs.is_empty() {
            dep_flags.push("-Wl,--whole-archive".to_string());
            for lib in &whole_archive_libs {
                dep_flags.push(lib.to_string_lossy().to_string());
            }
            dep_flags.push("-Wl,--no-whole-archive".to_string());
        }
        if !normal_libs.is_empty() {
            dep_flags.push("-Wl,--start-group".to_string());
            for lib in &normal_libs {
                dep_flags.push(lib.to_string_lossy().to_string());
            }
            dep_flags.push("-Wl,--end-group".to_string());
        }

        let mut test_linkopts = self.linkopts.clone();
        test_linkopts.extend(passthrough_flags.iter().cloned());
        args.extend(fast_linker_driver_flags(
            object_files,
            &dep_flags,
            &test_linkopts,
            &context.toolchain.ldflags,
        ));

        // Pass-through -l*/-L*/link flags (gtest/benchmark/absl) after archives.
        for f in &passthrough_flags {
            args.push(f.clone());
        }

        // Add user-specified linker options
        for opt in &self.linkopts {
            args.push(opt.clone());
        }

        // Add toolchain linker flags
        for flag in &context.toolchain.ldflags {
            args.push(flag.clone());
        }

        // Add output file
        args.push("-o".to_string());
        args.push(binary_path.to_string_lossy().to_string());
        normalize_bazel_origin_link_args(&mut args);

        // De-dup repeated archive paths on the link line. The same .a can be
        // reached by BOTH the dep CcInfo linker_inputs walk and the transitive
        // NestedSet sweep (e.g. liberror_codes_proto_impl_cc_impl.a), and with
        // --start-group wrapping a duplicate path causes "multiple definition"
        // (the member is extracted from each occurrence). This mirrors the
        // final archive de-dup cc_shared_library performs on its link args.
        normalize_bazel_origin_link_args(&mut args);
        let args = dedup_archive_link_flags(args);

        let command = Command {
            executable: context.toolchain.cxx.clone(),
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };

        let inputs: Vec<ActionArtifact> = input_files
            .iter()
            .map(|p| ActionArtifact::Source(p.clone()))
            .collect();

        let action_id = Action::compute_id(&inputs, &command, &HashMap::new())?;
        let outputs = vec![ActionArtifact::Source(binary_path.to_path_buf())];

        let action = Action {
            id: action_id,
            command,
            env: HashMap::new(),
            inputs,
            outputs,
            mnemonic: "CppLink".to_string(),
        };

        let result = context.execute_action(action).await?;

        if result.exit_code != 0 {
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(anyhow!("Linking failed: {}", stderr));
        }

        Ok(())
    }
}

/// filegroup rule - Simple file collection
pub struct FilegroupRule;

#[async_trait]
impl BuildRule for FilegroupRule {
    fn name(&self) -> &str {
        "filegroup"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs")?;

        Ok(Box::new(FilegroupInstance { name, srcs }))
    }

    fn documentation(&self) -> &str {
        "Filegroup rule - groups files together for dependencies"
    }
}

pub struct FilegroupInstance {
    name: String,
    srcs: Vec<String>,
}

#[async_trait]
impl RuleInstance for FilegroupInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        // Label-form srcs (nested filegroups, cross-package file labels)
        // are dependencies: they must be analyzed so their FilesInfo is
        // available when this filegroup's analyze() splices them. Bazel
        // treats a filegroup's label srcs the same way.
        self.srcs
            .iter()
            .filter(|s| is_label_src(s))
            .cloned()
            .collect()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        // Filegroups just collect source files - no compilation.
        //
        // A label-shaped src names a TARGET (a nested filegroup, a
        // cross-package file label, a generated file). Joining it onto
        // the package directory produced paths with the label's own
        // punctuation in them — `<pkg>/:collect-cc-coverage.sh` — which
        // cannot exist. Those phantom paths reached the freshness
        // ledger's required-present set, where a permanently absent
        // entry means the fast no-op baseline is never installed: 31 of
        // 5,767 required paths on a whole-repo Modular build, from 5
        // filegroups, cost every later no-op a full stat walk. Resolve
        // labels through the dependency's artifacts, exactly as this
        // rule's own analyze() already splices nested FilesInfo.
        let mut artifacts = Vec::new();
        for src in &self.srcs {
            if src.starts_with(':') || src.starts_with("//") || src.starts_with('@') {
                // A syntactically valid label that cannot be resolved is
                // an ERROR, not an empty contribution. Silently dropping
                // it is how the two stale-build bugs stayed invisible:
                // the filegroup would simply publish fewer files and
                // every consumer would agree that nothing was missing.
                let Some(dep_artifacts) = context.direct_artifacts(src) else {
                    return Err(anyhow::anyhow!(
                        "filegroup '{}': srcs label {} did not resolve to a \
                         dependency. It must name a target this rule depends \
                         on; an unresolvable label silently drops files from \
                         the group and from every consumer's inputs.",
                        self.name,
                        src
                    ));
                };
                artifacts.extend(dep_artifacts.iter().cloned());
                continue;
            }
            artifacts.push(Artifact {
                path: context.source_path(src),
                artifact_type: ArtifactType::Source,
            });
        }
        Ok(artifacts)
    }

    fn analyze(
        &self,
        ctx: &mut crate::analysis_context::AnalysisContext,
        _dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        use crate::providers::FilesInfo;
        // Expose the filegroup's files as ABSOLUTE paths, in `srcs`
        // declaration order, with nested filegroup labels spliced at
        // their position. Recursion is natural: a nested filegroup was
        // analyzed first and exposes its own already-absolute FilesInfo,
        // which we forward verbatim. Storing absolute paths means a
        // consumer (cc_library) needn't re-resolve cross-repo
        // `@repo//pkg:file` labels — each filegroup resolved its own
        // plain files against ITS workspace root. Bazel's filegroup
        // DefaultInfo.files is the analogue.
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        for s in &self.srcs {
            if is_label_src(s) {
                if let Some(p) = ctx.get_dependency_provider(s) {
                    if let Some(fi) = p.get_files() {
                        files.extend(fi.files.iter().cloned());
                    }
                }
                // A label with no FilesInfo (e.g. a non-filegroup target)
                // contributes nothing — same as Bazel, where only
                // files-providing srcs land in the group.
            } else {
                files.push(resolve_cc_src_path(
                    ctx.workspace_root(),
                    ctx.package(),
                    s,
                ));
            }
        }
        ProviderSet::empty().with_files(FilesInfo::new(files))
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "filegroup"
    }
}

/// tf_cc_library - TensorFlow C++ library (alias to cc_library)
pub struct TfCcLibraryRule;

#[async_trait]
impl BuildRule for TfCcLibraryRule {
    fn name(&self) -> &str {
        "tf_cc_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        // tf_cc_library is just cc_library with TensorFlow defaults
        CcLibraryRule.parse_attributes(attrs)
    }

    fn documentation(&self) -> &str {
        "TensorFlow C++ library - same as cc_library with TF-specific defaults"
    }
}

/// tf_cuda_library - TensorFlow CUDA library (treat as cc_library for now)
pub struct TfCudaLibraryRule;

#[async_trait]
impl BuildRule for TfCudaLibraryRule {
    fn name(&self) -> &str {
        "tf_cuda_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        // For now, treat CUDA libraries as regular C++ libraries
        // TODO: Add actual CUDA compilation support
        CcLibraryRule.parse_attributes(attrs)
    }

    fn documentation(&self) -> &str {
        "TensorFlow CUDA library - currently treated as cc_library"
    }
}

/// tf_cc_test - TensorFlow C++ test (alias to cc_binary)
pub struct TfCcTestRule;

#[async_trait]
impl BuildRule for TfCcTestRule {
    fn name(&self) -> &str {
        "tf_cc_test"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        // tf_cc_test is like cc_binary (builds an executable)
        CcBinaryRule.parse_attributes(attrs)
    }

    fn documentation(&self) -> &str {
        "TensorFlow C++ test - builds test executable"
    }
}

/// proto_library - Protocol buffer library
pub struct ProtoLibraryRule;

#[async_trait]
impl BuildRule for ProtoLibraryRule {
    fn name(&self) -> &str {
        "proto_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs")?;
        let deps = extract_string_list(attrs, "deps")?;

        Ok(Box::new(ProtoLibraryInstance { name, srcs, deps }))
    }

    fn documentation(&self) -> &str {
        "Proto library - defines protocol buffer files"
    }
}

pub struct ProtoLibraryInstance {
    name: String,
    srcs: Vec<String>,
    deps: Vec<String>,
}

#[async_trait]
impl RuleInstance for ProtoLibraryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        self.deps.clone()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        // Proto library just tracks .proto files - no compilation yet
        // Actual proto compilation happens in cc_proto_library
        let mut artifacts = Vec::new();
        for src in &self.srcs {
            let proto_path = context.source_path(src);
            artifacts.push(Artifact {
                path: proto_path,
                artifact_type: ArtifactType::Source,
            });
        }
        Ok(artifacts)
    }

    /// Phase 5.C0 P2: proto_library exposes ProtoFilesInfo so
    /// downstream cc_proto_library can enumerate the .proto
    /// closure via the typed path (no raw artifact NestedSet
    /// walks). Direct sources are this rule's `srcs`; transitive
    /// coverage chains through dep proto_library
    /// ProtoFilesInfo.
    fn analyze(
        &self,
        ctx: &mut crate::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        use crate::providers::ProtoFilesInfo;
        use rush_action::ids::ArtifactKind;
        use std::path::Path;
        let direct: Vec<_> = self
            .srcs
            .iter()
            .map(|s| ctx.register_source_artifact(Path::new(s), ArtifactKind::Source))
            .collect();
        // Dep ProtoFilesInfo arcs from upstream proto_library
        // deps. extract_proto_files mirrors extract_cc_infos —
        // pulls every dep's proto_files slot, dropping deps
        // that don't expose this provider (cc_library deps,
        // etc.). This is the typed-path enumeration that
        // replaces the legacy raw artifact NestedSet walk for
        // proto closures.
        let dep_pfis: Vec<std::sync::Arc<ProtoFilesInfo>> = dep_providers
            .iter()
            .filter_map(|p| p.proto_files.clone())
            .collect();
        let pfi = ProtoFilesInfo::from_direct_and_deps(direct, dep_pfis);
        ProviderSet::empty().with_proto_files(pfi)
    }

    fn rule_type(&self) -> &str {
        "proto_library"
    }
}

/// cc_proto_library - C++ code generation from proto_library
pub struct CcProtoLibraryRule;

#[async_trait]
impl BuildRule for CcProtoLibraryRule {
    fn name(&self) -> &str {
        "cc_proto_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let deps = extract_string_list(attrs, "deps")?;

        Ok(Box::new(CcProtoLibraryInstance { name, deps }))
    }

    fn documentation(&self) -> &str {
        "C++ proto library - generates C++ code from proto_library"
    }
}

pub struct CcProtoLibraryInstance {
    name: String,
    deps: Vec<String>, // Should be proto_library targets
}

/// Phase 5.C.5a: import-path-relative output layout for generated proto
/// headers. bazel (and rush's tf_proto_library) lay a generated `.pb.h`
/// at `<genfiles>/<proto-import-path>.pb.h` — e.g. an @riegeli proto at
/// `<repo>/riegeli/records/records_metadata.proto` →
/// `genfiles/riegeli/records/records_metadata.pb.h`, matching the
/// `#include "riegeli/records/records_metadata.pb.h"` consumers use.
/// cc_proto_library previously wrote FLAT under `rush-out/<target>/`,
/// breaking those repo-relative includes. Compute the import-relative
/// path by stripping the longest matching proto-import root (the target
/// workspace root, or an external repo root from extra_proto_paths).
fn proto_import_rel(
    abs_proto: &std::path::Path,
    workspace_root: &std::path::Path,
    extra_roots: &[std::path::PathBuf],
) -> std::path::PathBuf {
    let mut roots: Vec<&std::path::Path> = vec![workspace_root];
    roots.extend(extra_roots.iter().map(|p| p.as_path()));
    let mut best: Option<std::path::PathBuf> = None;
    let mut best_len = 0usize;
    for root in roots {
        if let Ok(rel) = abs_proto.strip_prefix(root) {
            let l = root.as_os_str().len();
            if best.is_none() || l > best_len {
                best = Some(rel.to_path_buf());
                best_len = l;
            }
        }
    }
    best.unwrap_or_else(|| {
        abs_proto
            .file_name()
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
    })
}

/// Under RUSH_ENABLE_BZLMOD, the proto IMPORT ROOT for the target repo (the dir a
/// proto's import/logical path is relative to), else None. This is repo-agnostic:
/// protobuf lays its protos under <repo>/src (strip_import_prefix="/src"), while most
/// BCR repos (googleapis, …) put protos at repo-root-relative paths (google/<api>/X.proto),
/// so the import root is the repo root. Used by the legacy cc_proto_library build() path
/// to drive the from-source protoc with the canonical import root (so the descriptor /
/// logical name is e.g. google/protobuf/X.proto or google/type/X.proto, never src/…),
/// lay the generated .pb.{h,cc} at the matching canonical genfiles path, and resolve
/// from-source headers in the generated-.pb.cc compile.
fn bzlmod_proto_import_root(workspace_root: &std::path::Path) -> Option<std::path::PathBuf> {
    if std::env::var("RUSH_ENABLE_BZLMOD").as_deref() != Ok("1") {
        return None;
    }
    // protobuf uses strip_import_prefix="/src"; everything else is repo-root relative.
    if workspace_root.file_name().and_then(|n| n.to_str()) == Some("protobuf") {
        Some(workspace_root.join("src"))
    } else {
        Some(workspace_root.to_path_buf())
    }
}

/// The from-source protobuf runtime's `src` dir (`<protobuf-repo>/src`) among the
/// external proto roots, if present — needed both as a `--proto_path` (so cross-repo
/// `import "google/protobuf/*.proto"` resolves) and as a `-I` for the protobuf runtime
/// headers every generated `.pb.cc` includes.
fn bzlmod_protobuf_runtime_src(
    extra_proto_paths: &[std::path::PathBuf],
) -> Option<std::path::PathBuf> {
    for path in extra_proto_paths {
        let repository = path.file_name().and_then(|name| name.to_str());
        if matches!(repository, Some("protobuf" | "com_google_protobuf")) {
            let src = path.join("src");
            if src.join("google/protobuf/message.h").is_file() {
                return Some(src);
            }
            if path.join("google/protobuf/message.h").is_file() {
                return Some(path.clone());
            }
        }
    }

    let pinned = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/data/rush-cache/external"))
        .join("com_google_protobuf");
    if !pinned.join("libprotobuf.a").is_file() {
        return None;
    }
    let include_root = std::fs::read_to_string(pinned.join("INCLUDE_ROOT"))
        .ok()
        .map(|value| std::path::PathBuf::from(value.trim()))
        .filter(|path| path.join("google/protobuf/message.h").is_file())
        .unwrap_or_else(|| pinned.join("src"));
    include_root
        .join("google/protobuf/message.h")
        .is_file()
        .then_some(include_root)
}

#[async_trait]
impl RuleInstance for CcProtoLibraryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        self.deps.clone()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let mut artifacts = Vec::new();

        // Create output directory
        let target_output_dir = context.output_dir.join(&self.name);
        std::fs::create_dir_all(&target_output_dir)?;

        // Get .proto files from dependencies (transitive — proto
        // libraries chain via protodeps and a cc_proto_library at the
        // top consumes the full closure).
        let mut proto_files = Vec::new();
        for dep in &self.deps {
            if let Some(iter) = context.iter_transitive_artifacts(dep) {
                for artifact in iter {
                    if artifact.path.extension().and_then(|s| s.to_str()) == Some("proto") {
                        proto_files.push(artifact.path.clone());
                    }
                }
            }
        }

        // Import-path-relative genfiles layout — MUST match analyze() and
        // build_legacy_command_specs (proto_import_rel) so the verifier
        // sees identical pb_h/pb_cc paths.
        let genfiles_dir = context.output_dir.join("genfiles");
        std::fs::create_dir_all(&genfiles_dir)?;
        // Generate C++ code from each .proto file
        for proto_file in &proto_files {
            // Bzlmod: strip the repo's import root so the generated .pb.{h,cc} land at
            // the canonical genfiles/<logical>.pb.* (google/protobuf/X.pb.* for protobuf,
            // google/type/X.pb.* for googleapis, …) — matching what the from-source protoc,
            // driven with --proto_path=<import-root>, writes. proto_file may be repo-root-
            // relative, so try both the absolute and the as-given path against the root.
            let import_rel = match bzlmod_proto_import_root(&context.workspace_root) {
                Some(root) => {
                    let abs = if proto_file.is_absolute() {
                        proto_file.to_path_buf()
                    } else {
                        context.workspace_root.join(proto_file)
                    };
                    abs.strip_prefix(&root)
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|_| {
                            proto_import_rel(
                                proto_file,
                                &context.workspace_root,
                                &context.extra_proto_paths,
                            )
                        })
                }
                None => proto_import_rel(
                    proto_file,
                    &context.workspace_root,
                    &context.extra_proto_paths,
                ),
            };
            let import_base = import_rel.with_extension("");
            let pb_h =
                genfiles_dir.join(format!("{}.pb.h", import_base.display()));
            let pb_cc =
                genfiles_dir.join(format!("{}.pb.cc", import_base.display()));

            // Run protoc to generate C++ code (cpp_out=genfiles), routed
            // through the action cache so an unchanged .proto is a no-op HIT
            // (stable .pb.cc/.pb.h mtimes → no downstream .o/.a churn).
            self.generate_proto_cpp(context, proto_file, &genfiles_dir, &pb_h, &pb_cc)
                .await?;

            // Add generated files as artifacts
            artifacts.push(Artifact {
                path: pb_h,
                artifact_type: ArtifactType::Header,
            });
            artifacts.push(Artifact {
                path: pb_cc,
                artifact_type: ArtifactType::Generated,
            });
        }

        // Compile generated .pb.cc files
        // First collect all .cc files to avoid borrow checker issues
        let cc_files: Vec<_> = artifacts
            .iter()
            .filter(|a| a.path.extension().and_then(|s| s.to_str()) == Some("cc"))
            .map(|a| a.path.clone())
            .collect();

        let mut object_files = Vec::new();
        for cc_file in cc_files {
            let obj_name = cc_file
                .file_stem()
                .ok_or_else(|| anyhow!("Invalid generated file"))?
                .to_string_lossy()
                .to_string();
            let obj_path = target_output_dir.join(format!("{}.o", obj_name));

            self.compile_proto_source(context, &cc_file, &obj_path)
                .await?;

            object_files.push(obj_path.clone());
            artifacts.push(Artifact {
                path: obj_path,
                artifact_type: ArtifactType::Object,
            });
        }

        // Create static library from compiled protos only when the build graph
        // needs a material archive. Header/object propagation does not.
        if !object_files.is_empty() && context.materialize_static_libraries() {
            let safe_name = self.name.replace('/', "_");
            let lib_path = target_output_dir.join(format!("lib{}.a", safe_name));
            self.create_static_library(context, &object_files, &lib_path)
                .await?;
            artifacts.push(Artifact {
                path: lib_path,
                artifact_type: ArtifactType::StaticLibrary,
            });
        }

        Ok(artifacts)
    }

    /// Phase 5.C0 P2 + 5.C.4 commit 11/12: declare the full
    /// Protoc → CppCompile → CcArchive chain for each .proto in
    /// the dep transitive closure, populate `ExecutableAction`
    /// on each, and expose the result via typed providers.
    ///
    /// Per .proto:
    ///   - ProtocCompile: src_proto → (pb_h, pb_cc)
    ///       executable: the resolved protoc (see protobuf_protoc_path)
    ///       argv: --proto_path=<src_parent> --proto_path=<workspace>
    ///             --cpp_out=<output_dir/self.name> <abs_proto>
    ///   - CppCompile: (pb_cc, pb_h) → obj
    ///       executable: toolchain.cxx
    ///       argv: -c -fPIC -std=c++17 [cxxflags...]
    ///             -I<parent_of_pb_cc> -I/usr/include
    ///             -o <obj> <pb_cc>
    ///       Both pb_cc AND pb_h declared as inputs — pb_cc
    ///       #include's pb_h, so the action graph must show
    ///       the dependency. Argv only carries pb_cc.
    ///
    /// Plus one CcArchive over the full obj set:
    ///   - CcArchive: [obj1, obj2, ...] → lib.a
    ///       executable: toolchain.ar
    ///       argv: rcs <lib_path> <obj1> <obj2> ...
    ///
    /// Providers exposed:
    ///   - DefaultInfo.files = [.a only] — no .o leakage to
    ///     consumers (commit 9 invariant).
    ///   - CcInfo.compilation.headers = [.pb.h per .proto].
    ///     Downstream cc_library that depends on this proto
    ///     library reads .pb.h via the typed path.
    ///   - CcInfo.compilation.include_dirs = [<output_dir>/genfiles]
    ///     so import-relative #include "<proto-import-path>.pb.h" resolves at
    ///     compile time.
    ///   - CcInfo.linking.linker_inputs = [.a as LinkerInput].
    ///     Downstream cc_binary's CcLink finds the proto .a in
    ///     its dep linker_inputs walk.
    ///
    /// `-I/usr/include` for system protobuf is the host-system
    /// shortcut for hello_cpp-scale workloads (mirrors legacy
    /// compile_proto_source). Hermetic / TF-scale protobuf
    /// include resolution is a separate follow-up — DO NOT
    /// solve inside this commit. See docs/item-5-phase-c-status.md
    /// commit 12 section.
    fn analyze(
        &self,
        ctx: &mut crate::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        use rush_action::ids::{ArtifactId, ArtifactKind, ExecutableAction, StringId};
        use std::path::Path;

        let mut direct_headers: Vec<ArtifactId> = Vec::new();
        let mut direct_include_dirs: Vec<rush_action::ids::PathId> = Vec::new();
        let mut obj_aids: Vec<ArtifactId> = Vec::new();
        let mut output_subdir_interned: bool = false;

        // Bzlmod @protobuf: generate with the from-source protoc_minimal +
        // protoc-gen-cpp (built into ctx.output_dir()) rather than system protoc,
        // and resolve generated .pb.* under the import root <protobuf-root>/src.
        // Gated by RUSH_ENABLE_BZLMOD and the binaries' presence — falls back to
        // the existing system-protoc shape otherwise (TF/463 path unchanged). The
        // scheduler runs these actions via LocalActionRunner, bypassing the
        // execute_action include-hygiene funnel, so the argv is fixed here at source.
        let bzlmod_proto = std::env::var("RUSH_ENABLE_BZLMOD").as_deref() == Ok("1");
        let from_src_protoc = ctx.output_dir().join("protoc_minimal");
        let from_src_plugin = ctx.output_dir().join("protoc-gen-cpp");
        let use_from_src = bzlmod_proto && from_src_protoc.exists();
        let protobuf_root: Option<std::path::PathBuf> = if bzlmod_proto {
            let ws = ctx.workspace_root().to_path_buf();
            if ws.file_name().and_then(|n| n.to_str()) == Some("protobuf") {
                Some(ws)
            } else {
                ctx.extra_proto_paths()
                    .iter()
                    .find(|p| p.file_name().and_then(|n| n.to_str()) == Some("protobuf"))
                    .cloned()
            }
        } else {
            None
        };
        let protobuf_runtime_src =
            bzlmod_protobuf_runtime_src(ctx.extra_proto_paths());
        if let Some(runtime_src) = &protobuf_runtime_src {
            direct_include_dirs.push(ctx.intern_path(runtime_src));
        }

        // Walk every dep's ProtoFilesInfo's transitive_sources.
        // Each is an ArtifactId of a .proto source registered by
        // some proto_library.
        for dp in dep_providers {
            if let Some(pfi) = dp.get_proto_files() {
                for src_aid in pfi.transitive_sources.iter() {
                    let data = ctx.registry().artifact(src_aid);
                    let src_path = ctx
                        .registry()
                        .paths()
                        .path(data.exec_path)
                        .to_path_buf();
                    let stem = src_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("proto")
                        .to_string();

                    // ----- import-path-relative genfiles layout -----
                    // Compute abs_proto first so the generated .pb.h lands
                    // at genfiles/<proto-import-path>.pb.h (matching the
                    // repo-relative `#include` consumers use), not flat
                    // under rush-out/<target>/. See proto_import_rel.
                    let abs_proto = if src_path.is_absolute() {
                        src_path.clone()
                    } else {
                        let pkg = ctx.package();
                        if pkg.is_empty() {
                            ctx.workspace_root().join(&src_path)
                        } else {
                            ctx.workspace_root().join(pkg).join(&src_path)
                        }
                    };
                    let proto_parent = abs_proto
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| std::path::PathBuf::from(""));
                    // For @protobuf, the import root is <protobuf-root>/src (so
                    // empty.proto → google/protobuf/empty.pb.h, not src/google/...).
                    let import_rel = match &protobuf_root {
                        Some(pr) => abs_proto
                            .strip_prefix(pr.join("src"))
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|_| {
                                proto_import_rel(
                                    &abs_proto,
                                    ctx.workspace_root(),
                                    ctx.extra_proto_paths(),
                                )
                            }),
                        None => proto_import_rel(
                            &abs_proto,
                            ctx.workspace_root(),
                            ctx.extra_proto_paths(),
                        ),
                    };
                    let import_base = import_rel.with_extension("");
                    let import_base_str = import_base.to_string_lossy();
                    let genfiles_dir = ctx.output_dir().join("genfiles");

                    // Generated artifact registration (paths land
                    // absolute under output_dir via predicted_output_path).
                    // pb_h/pb_cc go under genfiles/<import-rel>; obj stays
                    // flat under the target dir (not #included).
                    let pb_h_rel = format!("genfiles/{}.pb.h", import_base_str);
                    let pb_cc_rel = format!("genfiles/{}.pb.cc", import_base_str);
                    let obj_rel = format!("{}/{}.pb.o", self.name, stem);
                    let pb_h_aid = ctx.register_generated_artifact(
                        Path::new(&pb_h_rel),
                        ArtifactKind::Header,
                        None,
                    );
                    let pb_cc_aid = ctx.register_generated_artifact(
                        Path::new(&pb_cc_rel),
                        ArtifactKind::Generated,
                        None,
                    );
                    let obj_aid = ctx.register_generated_artifact(
                        Path::new(&obj_rel),
                        ArtifactKind::Object,
                        None,
                    );
                    direct_headers.push(pb_h_aid);
                    obj_aids.push(obj_aid);

                    // Intern the genfiles root as include_dir once so
                    // downstream `#include "<import-path>.pb.h"` resolves.
                    if !output_subdir_interned {
                        let pid = ctx.intern_path(&genfiles_dir);
                        direct_include_dirs.push(pid);
                        output_subdir_interned = true;
                    }

                    // ----- ProtocCompile: src → (pb_h, pb_cc) -----
                    if let Ok(protoc_id) = ctx.declare_action(
                        "ProtocCompile",
                        vec![src_aid],
                        vec![pb_h_aid, pb_cc_aid],
                    ) {
                        // Repo/import roots FIRST so protoc lays output at
                        // the import-relative path; src_parent LAST as a
                        // sibling-`import "foo.proto"` compatibility fallback.
                        let mut argv_strs: Vec<String> = Vec::new();
                        if use_from_src {
                            // from-source C++ codegen plugin (protoc_minimal has no
                            // built-in cpp generator; --cpp_out drives the plugin).
                            argv_strs.push(format!(
                                "--plugin=protoc-gen-cpp={}",
                                from_src_plugin.display()
                            ));
                        }
                        // @protobuf: protoc canonicalizes the file to its on-disk path
                        // and strips a matching --proto_path prefix to form the logical
                        // (descriptor) name. So pass ONLY <root>/src as the prefix root
                        // → canonical google/protobuf/X.proto. <root> itself and the
                        // proto's parent dir are ALSO prefixes (yielding src/google/...
                        // or a bare name), so they must be excluded; other repo roots
                        // aren't prefixes of the protobuf protos, so keep them for
                        // cross-repo imports.
                        if let Some(pr) = &protobuf_root {
                            // ONLY <root>/src — matching the verified smoke invocation.
                            // protoc's logical-name resolution with many --proto_path
                            // yields the src/ prefix; a single import root gives the
                            // canonical google/protobuf/X.proto. protobuf's base protos
                            // import only sibling google/protobuf/*.proto (same root).
                            argv_strs.push(format!(
                                "--proto_path={}",
                                pr.join("src").display()
                            ));
                        } else {
                            argv_strs.push(format!(
                                "--proto_path={}",
                                ctx.workspace_root().display()
                            ));
                            for extra in ctx.extra_proto_paths() {
                                argv_strs
                                    .push(format!("--proto_path={}", extra.display()));
                            }
                            argv_strs.push(format!(
                                "--proto_path={}",
                                proto_parent.display()
                            ));
                        }
                        // proto3 optional fields need this on protoc < 3.15 (e.g. Ubuntu
                        // 22.04's apt protoc 3.12); newer protoc defaults it on + accepts the
                        // flag harmlessly. Bazel's cc_proto passes it unconditionally — match
                        // that so TF's proto3-optional protos build on any protoc version.
                        argv_strs.push("--experimental_allow_proto3_optional".to_string());
                        argv_strs
                            .push(format!("--cpp_out={}", genfiles_dir.display()));
                        // For @protobuf, pass the file as the path RELATIVE to its
                        // import root (google/protobuf/X.proto). protoc names a file
                        // found via --proto_path by the path AS GIVEN, so this yields
                        // the canonical descriptor name regardless of the action CWD
                        // (rush runs the action with CWD=<root>, which would otherwise
                        // make protoc emit the CWD-relative src/google/... name).
                        let proto_arg = match &protobuf_root {
                            Some(pr) => abs_proto
                                .strip_prefix(pr.join("src"))
                                .map(|p| p.to_path_buf())
                                .unwrap_or_else(|_| abs_proto.clone()),
                            None => abs_proto.clone(),
                        };
                        argv_strs.push(proto_arg.display().to_string());

                        let exe_pid = if use_from_src {
                            ctx.intern_path(&from_src_protoc)
                        } else {
                            // The compiler rush RESOLVES (RUSH_PROTOC / PROTOC /
                            // the pinned bootstrap protoc / PATH) — never a bare
                            // "protoc", which PATH-resolves at exec time and can
                            // be a different protobuf than the headers these
                            // sources are compiled against. That divergence
                            // generated riegeli's records_metadata.pb.h with
                            // protoc 3.12 while the build compiled it against the
                            // pinned 3.21 headers, and the version guard failed on
                            // a target that names neither. Interning the resolved
                            // path also puts WHICH protoc ran into the action key.
                            ctx.intern_path(&crate::rules_system::protobuf_protoc_path())
                        };
                        // Do NOT override CWD: with a single import root --proto_path=
                        // <root>/src and the absolute proto file, protoc names it
                        // canonically (google/protobuf/X.proto). Setting CWD=<root>/src
                        // perversely makes protoc emit the src/-prefixed name instead
                        // (verified: the same argv canonicalizes correctly from any
                        // other CWD).
                        let argv: Vec<StringId> = argv_strs
                            .iter()
                            .map(|s| ctx.intern_string(s))
                            .collect();
                        ctx.set_executable_action(
                            protoc_id,
                            ExecutableAction {
                                executable: exe_pid,
                                argv: argv.into_boxed_slice(),
                                env: Box::new([]),
                                working_dir: None,
                                env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                            },
                        );
                    }

                    // ----- CppCompile: (pb_cc, pb_h) → obj -----
                    // Per Pro's correctness directive: BOTH pb_cc
                    // AND pb_h are declared as inputs. pb_cc
                    // #include's pb_h via -I<output_subdir>; the
                    // declared-input set must reflect the actual
                    // file dependency, not just argv.
                    let pb_cc_abs =
                        ctx.registry().artifact_path(pb_cc_aid).to_path_buf();
                    let obj_abs = ctx.registry().artifact_path(obj_aid).to_path_buf();
                    let pb_cc_parent = pb_cc_abs
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| std::path::PathBuf::from(""));
                    let cxxflags: Vec<String> = ctx.toolchain().cxxflags.clone();
                    let cxx_pathbuf = ctx.toolchain().cxx.clone();

                    if let Ok(compile_id) = ctx.declare_action(
                        "CppCompile",
                        vec![pb_cc_aid, pb_h_aid],
                        vec![obj_aid],
                    ) {
                        let mut argv_strs: Vec<String> = Vec::new();
                        argv_strs.push("-c".to_string());
                        argv_strs.push("-fPIC".to_string());
                        argv_strs.push("-std=c++17".to_string());
                        argv_strs.extend(cxxflags.iter().cloned());
                        // genfiles ROOT first so the generated .pb.cc's
                        // import-relative `#include "<repo>/<path>.pb.h"`
                        // resolves; the .pb.cc's own dir + /usr/include follow.
                        argv_strs.push(format!("-I{}", genfiles_dir.display()));
                        argv_strs.push(format!("-I{}", pb_cc_parent.display()));
                        if bzlmod_proto {
                            // from-source protobuf/absl headers. The scheduler runs
                            // this CppCompile via LocalActionRunner, bypassing the
                            // execute_action include-hygiene funnel, so add the
                            // external repo roots (and protobuf's <root>/src +
                            // utf8_range) here at source; NO system /usr/include.
                            for extra in ctx.extra_proto_paths() {
                                argv_strs.push(format!("-I{}", extra.display()));
                            }
                            if let Some(runtime_src) = &protobuf_runtime_src {
                                argv_strs
                                    .push(format!("-I{}", runtime_src.display()));
                            }
                            if let Some(pr) = &protobuf_root {
                                argv_strs
                                    .push(format!("-I{}", pr.join("src").display()));
                                let utf8 =
                                    pr.join("third_party").join("utf8_range");
                                if utf8.is_dir() {
                                    argv_strs
                                        .push(format!("-I{}", utf8.display()));
                                }
                            }
                        } else if let Some(runtime_src) = &protobuf_runtime_src {
                            argv_strs.push(format!("-I{}", runtime_src.display()));
                        } else {
                            argv_strs.push("-I/usr/include".to_string());
                        }
                        argv_strs.push("-o".to_string());
                        argv_strs.push(obj_abs.display().to_string());
                        argv_strs.push(pb_cc_abs.display().to_string());

                        let exe_pid = ctx.intern_path(&cxx_pathbuf);
                        let argv: Vec<StringId> = argv_strs
                            .iter()
                            .map(|s| ctx.intern_string(s))
                            .collect();
                        ctx.set_executable_action(
                            compile_id,
                            ExecutableAction {
                                executable: exe_pid,
                                argv: argv.into_boxed_slice(),
                                env: Box::new([]),
                                working_dir: None,
                                env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                            },
                        );
                    }
                }
            }
        }

        // ----- CcArchive: [obj...] → lib.a -----
        let mut public_lib_aid: Option<ArtifactId> = None;
        let mut direct_linker_inputs: Vec<rush_action::ids::LinkerInputId> =
            Vec::new();
        if !obj_aids.is_empty() {
            let safe_name = self.name.replace('/', "_");
            let lib_rel = format!("{}/lib{}.a", self.name, safe_name);
            let lib_aid = ctx.register_generated_artifact(
                Path::new(&lib_rel),
                ArtifactKind::StaticLibrary,
                None,
            );
            let lib_path_str =
                ctx.registry().artifact_path(lib_aid).display().to_string();
            let obj_path_strs: Vec<String> = obj_aids
                .iter()
                .map(|aid| ctx.registry().artifact_path(*aid).display().to_string())
                .collect();
            let ar_pathbuf = ctx.toolchain().ar.clone();

            if let Ok(archive_id) =
                ctx.declare_action("CcArchive", obj_aids.clone(), vec![lib_aid])
            {
                let ar_path = ctx.intern_path(&ar_pathbuf);
                let mut argv: Vec<StringId> =
                    Vec::with_capacity(3 + obj_path_strs.len());
                argv.push(ctx.intern_string("rcs"));
                argv.push(ctx.intern_string(&lib_path_str));
                argv.push(ctx.intern_string("--thin"));
                for s in &obj_path_strs {
                    argv.push(ctx.intern_string(s));
                }
                ctx.set_executable_action(
                    archive_id,
                    ExecutableAction {
                        executable: ar_path,
                        argv: argv.into_boxed_slice(),
                        env: Box::new([]),
                        working_dir: None,
                        env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                    },
                );
            }

            // Linker input so cc_binary's CcLink finds the .a
            // when the proto library is a transitive dep.
            let lid = ctx.register_linker_input(lib_aid, /*alwayslink*/ false);
            direct_linker_inputs.push(lid);
            public_lib_aid = Some(lib_aid);
        }

        // ----- CcInfo with typed provider exposure -----
        let direct = crate::providers::DirectCcInfo {
            headers: direct_headers,
            include_dirs: direct_include_dirs,
            linker_inputs: direct_linker_inputs,
            ..crate::providers::DirectCcInfo::default()
        };
        let cc_info = crate::providers::CcInfo::from_direct_and_deps(
            direct,
            crate::providers::extract_cc_infos(dep_providers).into_iter(),
        );
        let mut providers = ProviderSet::empty().with_cc_info(cc_info);
        if let Some(aid) = public_lib_aid {
            // DefaultInfo.files = .a only (commit 9 invariant —
            // no .o leakage to transitive consumers).
            providers = providers.with_default_info(crate::providers::DefaultInfo {
                files: vec![aid],
            });
        }
        providers
    }

    fn rule_type(&self) -> &str {
        "cc_proto_library"
    }

    /// Phase 5.C.4 commit 55: cc_proto_library invokes
    /// `protoc` at action execution time to generate
    /// `.pb.cc` / `.pb.h` from `.proto` sources. The
    /// orchestrator's tool probe gates dispatch on protoc
    /// availability; without it, MissingTool fires.
    fn scheduler_required_tools(&self) -> Vec<RequiredTool> {
        vec![RequiredTool::Protoc]
    }

    /// Phase 5.C.4 commit 11/12: legacy specs for the full
    /// Protoc → CppCompile → CcArchive chain. Mirrors what
    /// `generate_proto_cpp`, `compile_proto_source`, and
    /// `create_static_library` emit at build time — argv must
    /// match exactly so the verifier produces zero diffs.
    ///
    /// For each .proto in the dep transitive closure:
    ///   - one ProtocCompile spec (key: .pb.h primary output)
    ///   - one CppCompile spec on the generated .pb.cc
    ///     (declared_inputs = [.pb.cc, .pb.h]; argv carries
    ///     pb_cc only). Built via
    ///     `CcCompileCommandSpec::for_generated_cpp` so the
    ///     generated-source compile rides the same spec
    ///     machinery as cc_library/cc_binary CppCompile.
    /// Plus one CcArchive over the obj set (key: .a primary).
    async fn build_legacy_command_specs(
        &self,
        context: &BuildContext,
    ) -> Vec<(crate::SchedulerActionKey, rush_action::command_spec::NormalizedCommandSpec)> {
        use crate::cc_command_spec::{CcArchiveCommandSpec, CcCompileCommandSpec};
        use crate::SchedulerActionKey;
        use rush_action::command_spec::NormalizedCommandSpec;
        use std::path::PathBuf;

        let mut specs = Vec::new();

        // Walk dep transitive .proto artifacts — same as build().
        let mut proto_files: Vec<PathBuf> = Vec::new();
        for dep in &self.deps {
            if let Some(iter) = context.iter_transitive_artifacts(dep) {
                for artifact in iter {
                    if artifact.path.extension().and_then(|s| s.to_str())
                        == Some("proto")
                    {
                        proto_files.push(artifact.path.clone());
                    }
                }
            }
        }

        let target_output_dir = context.output_dir.join(&self.name);
        let workspace_root = context.workspace_root.clone();
        let cxxflags = context.toolchain.cxxflags.clone();
        let cxx = context.toolchain.cxx.clone();
        let ar = context.toolchain.ar.clone();

        let genfiles_dir = context.output_dir.join("genfiles");
        let extra_proto_paths = context.extra_proto_paths.clone();
        let mut obj_paths: Vec<PathBuf> = Vec::new();

        for proto_file in &proto_files {
            let stem = proto_file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("proto");
            // Import-path-relative genfiles layout — MUST match analyze()
            // and build() (proto_import_rel) or the verifier diverges.
            let import_rel =
                proto_import_rel(proto_file, &workspace_root, &extra_proto_paths);
            let import_base = import_rel.with_extension("");
            let pb_h =
                genfiles_dir.join(format!("{}.pb.h", import_base.display()));
            let pb_cc =
                genfiles_dir.join(format!("{}.pb.cc", import_base.display()));
            let obj_path = target_output_dir.join(format!("{}.pb.o", stem));
            obj_paths.push(obj_path.clone());

            // ----- ProtocCompile spec (mirrors generate_proto_cpp) -----
            // Repo/import roots FIRST, src parent LAST (sibling-import
            // compat), cpp_out=genfiles.
            let parent = proto_file
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from(""));

            let mut argv: Vec<String> = Vec::new();
            // proto3 optional: required on protoc < 3.15, harmless on newer (Bazel always passes it).
            argv.push("--experimental_allow_proto3_optional".to_string());
            argv.push(format!("--proto_path={}", workspace_root.display()));
            for extra in extra_proto_paths.iter() {
                argv.push(format!("--proto_path={}", extra.display()));
            }
            argv.push(format!("--proto_path={}", parent.display()));
            argv.push(format!("--cpp_out={}", genfiles_dir.display()));
            argv.push(proto_file.display().to_string());

            let mut protoc_spec = NormalizedCommandSpec {
                mnemonic: "ProtocCompile".to_string(),
                executable: PathBuf::from("protoc"),
                cwd: None,
                argv,
                env: Vec::new(),
                declared_inputs: vec![proto_file.clone()],
                declared_outputs: vec![pb_h.clone(), pb_cc.clone()],
                toolchain_identity: None,
                param_files: Vec::new(),
                env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
            };
            protoc_spec.normalize();
            specs.push((
                SchedulerActionKey {
                    mnemonic: "ProtocCompile".to_string(),
                    primary_output_path: pb_h.clone(),
                },
                protoc_spec,
            ));

            // ----- CppCompile spec on .pb.cc (mirrors
            //       compile_proto_source via for_generated_cpp) -----
            // declared_inputs is [.pb.cc, .pb.h] per Pro's
            // correctness directive: pb_cc #include's pb_h, the
            // action graph must show that even though only pb_cc
            // appears on argv.
            let compile = CcCompileCommandSpec::for_generated_cpp(
                cxx.clone(),
                cxxflags.clone(),
                pb_cc.clone(),
                obj_path.clone(),
                vec![pb_cc.clone(), pb_h.clone()],
                genfiles_dir.clone(),
            );
            specs.push((
                SchedulerActionKey {
                    mnemonic: "CppCompile".to_string(),
                    primary_output_path: obj_path,
                },
                compile.to_normalized_command_spec(),
            ));
        }

        // ----- CcArchive over the obj set (mirrors create_static_library) -----
        if !obj_paths.is_empty() {
            let safe_name = self.name.replace('/', "_");
            let lib_path =
                target_output_dir.join(format!("lib{}.a", safe_name));
            let mut declared_inputs = obj_paths.clone();
            // Match cc_library: declared_inputs is the obj set;
            // declared_outputs is the .a.
            declared_inputs.sort();
            let archive = CcArchiveCommandSpec {
                mnemonic: "CcArchive".to_string(),
                executable: ar.clone(),
                cwd: None,
                lib_path: lib_path.clone(),
                object_paths: obj_paths.clone(),
                declared_input_paths: obj_paths.clone(),
                declared_output_paths: vec![lib_path.clone()],
                toolchain_identity: None,
            };
            specs.push((
                SchedulerActionKey {
                    mnemonic: "CcArchive".to_string(),
                    primary_output_path: lib_path,
                },
                archive.to_normalized_command_spec(),
            ));
        }

        specs
    }
}

impl CcProtoLibraryInstance {
    /// Generate C++ code from .proto file using protoc
    /// Generate `.pb.h`/`.pb.cc` from one `.proto` via the action cache.
    ///
    /// Mirrors `tf_proto_library`'s `GenProto` action (the legacy build()
    /// path that DOES go through `context.execute_action` →
    /// `RushActionManager::execute`, with the d2ff3a1 producer-stamp HIT
    /// gate) rather than running `protoc` directly with
    /// `std::process::Command`. The raw-exec version re-fired protoc on
    /// every no-op (no freshness check), rewriting `.pb.cc`/`.pb.h` and
    /// cascading into a fresh `.o` and a fresh `.a` (new mtime/ino) — which
    /// defeated the cc_shared_library link-plan cache (its key folds each
    /// advertised archive's stat_meta mtime/ctime). Routing through
    /// `execute_action` makes an unchanged `.proto` a clean cache HIT
    /// (action.id folds the full argv AND the .proto source CONTENT, see
    /// Action::compute_id), so protoc is NOT re-run and the generated files'
    /// mtimes stay stable. A touched `.proto` changes the content hash → MISS
    /// → regenerates, so correctness is preserved.
    async fn generate_proto_cpp(
        &self,
        context: &BuildContext,
        proto_file: &Path,
        output_dir: &Path,
        pb_h: &Path,
        pb_cc: &Path,
    ) -> Result<()> {
        // Bzlmod: generate with the from-source protoc_minimal + protoc-gen-cpp plugin
        // (built into context.output_dir from BCR source), not system protoc. The
        // target repo's import root goes FIRST so protoc names the proto canonically
        // (google/protobuf/X.proto, google/type/X.proto, …); the protobuf runtime src
        // and the other external repo roots follow only to resolve cross-repo imports
        // (e.g. googleapis' `import "google/protobuf/any.proto"`). The roots are disjoint
        // (each proto lives under exactly one), so there's no naming ambiguity.
        let import_root = bzlmod_proto_import_root(&context.workspace_root);
        let pb_runtime_src = bzlmod_protobuf_runtime_src(&context.extra_proto_paths);
        let from_src_protoc = context.output_dir.join("protoc_minimal");
        let from_src_plugin = context.output_dir.join("protoc-gen-cpp");
        let use_from_src = import_root.is_some() && from_src_protoc.exists();

        // Build protoc argv (identical shape to the previous raw exec).
        let executable = if use_from_src {
            from_src_protoc.clone()
        } else {
            // The compiler rush RESOLVES, not a bare "protoc" that PATH
            // resolves at exec time. This is the path that actually GENERATES
            // cc_proto_library sources, and it disagreed with the pinned
            // protobuf headers those sources are compiled against: riegeli's
            // records_metadata.pb.h came out of protoc 3.12 while the build
            // compiled it against 3.21, and the version guard failed on a
            // target that mentions neither protobuf nor riegeli.
            crate::rules_system::protobuf_protoc_path()
        };
        let mut args: Vec<String> = Vec::new();
        if use_from_src {
            args.push(format!(
                "--plugin=protoc-gen-cpp={}",
                from_src_plugin.display()
            ));
        }

        if let Some(root) = &import_root {
            // Target repo's import root FIRST → canonical logical/descriptor name.
            args.push(format!("--proto_path={}", root.display()));
            // protobuf runtime src for `import "google/protobuf/*.proto"`.
            if let Some(pb) = &pb_runtime_src {
                if pb != root {
                    args.push(format!("--proto_path={}", pb.display()));
                }
            }
            // Other external repo roots for any further cross-repo imports.
            for extra in context.extra_proto_paths.iter() {
                if Some(extra) != import_root.as_ref() {
                    args.push(format!("--proto_path={}", extra.display()));
                }
            }
        } else {
            // Repo/import roots FIRST so protoc lays output at the
            // import-relative path (genfiles/<repo-rel>.pb.h); the src parent
            // goes LAST only as a sibling-`import "foo.proto"` fallback.
            args.push(format!("--proto_path={}", context.workspace_root.display()));
            for extra in context.extra_proto_paths.iter() {
                args.push(format!("--proto_path={}", extra.display()));
            }
            if let Some(parent) = proto_file.parent() {
                args.push(format!("--proto_path={}", parent.display()));
            }
        }

        // Set C++ output directory (caller passes the genfiles root)
        args.push(format!("--cpp_out={}", output_dir.display()));

        // Add the .proto file
        args.push(proto_file.to_string_lossy().to_string());

        let command = Command {
            executable,
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };

        // Input is the .proto SOURCE (content-hashed by Action::compute_id), so
        // a touched-but-identical .proto HITs while an edited one MISSes. Both
        // generated files are declared outputs so the producer-stamp gate guards
        // them.
        let inputs = vec![ActionArtifact::Source(proto_file.to_path_buf())];
        let outputs = vec![
            ActionArtifact::Source(pb_cc.to_path_buf()),
            ActionArtifact::Source(pb_h.to_path_buf()),
        ];
        let action_id = Action::compute_id(&inputs, &command, &HashMap::new())?;
        let action = Action {
            id: action_id,
            command,
            env: HashMap::new(),
            inputs,
            outputs,
            mnemonic: "GenProto".to_string(),
        };

        let result = context.execute_action(action).await.with_context(|| {
            format!(
                "execute_action(protoc) failed for src={} cpp_out={}",
                proto_file.display(),
                output_dir.display()
            )
        })?;

        if result.exit_code != 0 {
            return Err(anyhow!(
                "protoc failed for {}: {}",
                proto_file.display(),
                String::from_utf8_lossy(&result.stderr)
            ));
        }

        if !pb_cc.exists() {
            return Err(anyhow!(
                "protoc did not generate expected file: {} (proto: {})",
                pb_cc.display(),
                proto_file.display()
            ));
        }

        Ok(())
    }

    /// Compile generated .pb.cc file
    /// Compile one generated `.pb.cc` to its `.o` via the action cache.
    ///
    /// Routed through `context.execute_action` (mnemonic `CppCompile`), like
    /// the cc_library / tf_proto_library compile paths, so an unchanged
    /// `.pb.cc` (and `.pb.h`) is a cache HIT and the `.o` is NOT rewritten on
    /// a no-op. The sibling `.pb.h` is declared as an input even though it is
    /// not on the argv: the `.pb.cc` `#include`s it, so a regenerated header
    /// must invalidate this compile.
    async fn compile_proto_source(
        &self,
        context: &BuildContext,
        src_path: &Path,
        obj_path: &Path,
    ) -> Result<()> {
        let mut args: Vec<String> = Vec::new();
        args.push("-c".to_string());
        args.push("-fPIC".to_string());
        args.push("-std=c++17".to_string());

        // Add toolchain flags
        for flag in &context.toolchain.cxxflags {
            args.push(flag.clone());
        }

        // Add include path for generated headers. The genfiles ROOT (not
        // the .pb.cc's parent) is what resolves the import-relative
        // `#include "<repo>/<path>.pb.h"` the generated .pb.cc emits.
        args.push(format!("-I{}", context.output_dir.join("genfiles").display()));
        if let Some(parent) = src_path.parent() {
            args.push(format!("-I{}", parent.display()));
        }

        // Bzlmod: from-source protobuf/absl headers (no system protobuf). Every
        // generated .pb.cc includes the protobuf runtime headers (google/protobuf/*.h,
        // which live at <protobuf-repo>/src), plus abseil from the external repo roots.
        match bzlmod_proto_import_root(&context.workspace_root) {
            Some(_) => {
                if let Some(pb) = bzlmod_protobuf_runtime_src(&context.extra_proto_paths) {
                    args.push(format!("-I{}", pb.display()));
                    // <protobuf>/third_party/utf8_range (pb is <protobuf>/src).
                    if let Some(root) = pb.parent() {
                        let utf8 = root.join("third_party").join("utf8_range");
                        if utf8.is_dir() {
                            args.push(format!("-I{}", utf8.display()));
                        }
                    }
                }
        for extra in context.extra_proto_paths.iter() {
            args.push(format!("-I{}", extra.display()));
        }
            }
            None => {
                // The protobuf runtime headers that match the protoc which
                // generated this source. Hardcoding /usr/include here while
                // generation used the pinned protoc is what produced
                // "generated by a newer version of protoc" on riegeli.
                for dir in crate::rules_system::protobuf_runtime_include_dirs() {
                    args.push(format!("-I{}", dir.display()));
                }
            }
        }

        args.push("-o".to_string());
        args.push(obj_path.to_string_lossy().to_string());
        args.push(src_path.to_string_lossy().to_string());

        let command = Command {
            executable: context.toolchain.cxx.clone(),
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };

        // The .pb.cc #includes its sibling .pb.h; declare the header as a
        // freshness input (it is not on the argv) so a regenerated header
        // re-fires this compile. The header may not exist for header-only
        // protos, so only declare it when present.
        let mut inputs = vec![ActionArtifact::Source(src_path.to_path_buf())];
        // src is `<base>.pb.cc`; strip the trailing `.cc` (one extension)
        // leaving `<base>.pb`, then append `.h` → the `<base>.pb.h` sibling.
        let pb_h = if src_path.extension().and_then(|s| s.to_str()) == Some("cc") {
            let stem = src_path.with_extension(""); // `<base>.pb`
            PathBuf::from(format!("{}.h", stem.display()))
        } else {
            src_path.with_extension("h")
        };
        if pb_h.exists() {
            inputs.push(ActionArtifact::Source(pb_h));
        }
        let outputs = vec![ActionArtifact::Source(obj_path.to_path_buf())];
        let action_id = Action::compute_id(&inputs, &command, &HashMap::new())?;
        let action = Action {
            id: action_id,
            command,
            env: HashMap::new(),
            inputs,
            outputs,
            mnemonic: "CppCompile".to_string(),
        };

        let result = context.execute_action(action).await.with_context(|| {
            format!(
                "execute_action(g++ proto compile) failed for src={} obj={}",
                src_path.display(),
                obj_path.display()
            )
        })?;

        if result.exit_code != 0 {
            return Err(anyhow!(
                "Compilation failed for {}: {}",
                src_path.display(),
                String::from_utf8_lossy(&result.stderr)
            ));
        }

        // A virtual cache hit legitimately leaves the object CAS-backed
        // (stamp on disk, bytes in the registry) — consumers materialize
        // it on demand. Absent AND unregistered is still a producer bug.
        if !rush_action::virtual_artifacts::present_or_virtual(&obj_path) {
            return Err(anyhow!(
                "proto compile produced no object: {} (src {})",
                obj_path.display(),
                src_path.display()
            ));
        }

        Ok(())
    }

    /// Create static library from object files via the action cache.
    ///
    /// Routed through `context.execute_action` (mnemonic `CppArchive`) so an
    /// unchanged object set is a cache HIT and the `.a` is left UNTOUCHED —
    /// its mtime/ino stay stable, which is what lets the downstream
    /// cc_shared_library link-plan cache HIT on a no-op. The stale-member
    /// pre-remove (`ar rcs` never drops members absent from the current obj
    /// list) is NOT done eagerly here: doing so defeated the action cache's
    /// output-present HIT gate and re-ran `ar` on every build. The pre-remove
    /// now lives in the executor (LocalStrategy / LocalActionRunner), which
    /// only fires it right before actually spawning `ar` — i.e. on a cache
    /// MISS. Mirrors `CcLibraryInstance::create_static_library`.
    async fn create_static_library(
        &self,
        context: &BuildContext,
        object_files: &[PathBuf],
        lib_path: &Path,
    ) -> Result<()> {
        let mut args = vec![
            "rcs".to_string(),
            lib_path.to_string_lossy().to_string(),
            "--thin".to_string(),
        ];
        for obj in object_files {
            args.push(obj.to_string_lossy().to_string());
        }

        let command = Command {
            executable: PathBuf::from("ar"),
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };

        let inputs: Vec<ActionArtifact> = object_files
            .iter()
            .map(|p| ActionArtifact::Source(p.clone()))
            .collect();
        let outputs = vec![ActionArtifact::Source(lib_path.to_path_buf())];
        let action_id = Action::compute_id(&inputs, &command, &HashMap::new())?;
        let action = Action {
            id: action_id,
            command,
            env: HashMap::new(),
            inputs,
            outputs,
            mnemonic: "CppArchive".to_string(),
        };

        let result = context.execute_action(action).await?;

        if result.exit_code != 0 {
            return Err(anyhow!(
                "Archive creation failed: {}",
                String::from_utf8_lossy(&result.stderr)
            ));
        }

        Ok(())
    }
}

/// test_suite rule - Groups multiple tests together (stub)
pub struct TestSuiteRule;

#[async_trait]
impl BuildRule for TestSuiteRule {
    fn name(&self) -> &str {
        "test_suite"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let tests = extract_string_list(attrs, "tests").unwrap_or_default();

        Ok(Box::new(TestSuiteInstance { name, tests }))
    }

    fn documentation(&self) -> &str {
        "Test suite rule - groups multiple test targets together (stub implementation)"
    }
}

/// Instance of a test_suite rule
pub struct TestSuiteInstance {
    name: String,
    tests: Vec<String>,
}

#[async_trait]
impl RuleInstance for TestSuiteInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        // test_suite depends on all the tests it contains
        self.tests.clone()
    }

    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
        // Test suite implementation:
        // Groups tests together for the test runner.
        // No build artifacts are produced by the suite itself.
        // No Command::new execution required.
        Ok(Vec::new()) // HACK: Metadata-only rule, returns empty artifacts
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "test_suite"
    }
}

/// package_group rule - Defines reusable package visibility sets.
pub struct PackageGroupRule;

#[async_trait]
impl BuildRule for PackageGroupRule {
    fn name(&self) -> &str {
        "package_group"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let packages = extract_string_list(attrs, "packages")?;
        let includes = extract_string_list(attrs, "includes")?;

        Ok(Box::new(PackageGroupInstance {
            name,
            packages,
            includes,
        }))
    }

    fn documentation(&self) -> &str {
        "Package group rule - defines visibility groups for access control"
    }
}

pub struct PackageGroupInstance {
    name: String,
    packages: Vec<String>,
    includes: Vec<String>,
}

#[async_trait]
impl RuleInstance for PackageGroupInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        Vec::new()
    }

    fn package_group_packages(&self) -> Option<&[String]> {
        Some(&self.packages)
    }

    fn package_group_includes(&self) -> Option<&[String]> {
        Some(&self.includes)
    }

    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
        // Package group implementation:
        // Defines visibility groups for access control.
        // Metadata-only rule, no build artifacts.
        // No Command::new execution required.
        Ok(Vec::new()) // HACK: Metadata-only rule, returns empty artifacts
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "package_group"
    }
}

/// Internal source-file target emitted for each `exports_files` entry.
pub struct SourceFileRule;

#[async_trait]
impl BuildRule for SourceFileRule {
    fn name(&self) -> &str {
        "source_file"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let path = attrs
            .get("path")
            .and_then(|v| v.as_string())
            .map(|s| s.to_string())
            .unwrap_or_else(|| name.clone());

        if path.starts_with(':') || path.starts_with("//") || path.starts_with('@') {
            return Err(anyhow!("source_file path must be package-relative: {}", path));
        }
        let path_obj = Path::new(&path);
        if path_obj.is_absolute() || path_obj.components().any(|c| c == std::path::Component::ParentDir) {
            return Err(anyhow!("source_file path must stay within its package: {}", path));
        }

        Ok(Box::new(SourceFileInstance { name, path }))
    }

    fn documentation(&self) -> &str {
        "Internal source-file rule emitted from exports_files entries"
    }
}

pub struct SourceFileInstance {
    name: String,
    path: String,
}

impl SourceFileInstance {
    pub fn new(name: String, path: String) -> Self {
        Self { name, path }
    }
}

#[async_trait]
impl RuleInstance for SourceFileInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        Vec::new()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let path = context.source_path(&self.path);
        if !path.exists() {
            return Err(anyhow!(
                "exported source file target {} not found at {}",
                self.name,
                path.display()
            ));
        }

        Ok(vec![Artifact {
            path,
            artifact_type: ArtifactType::Source,
        }])
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
            .with_files(crate::providers::FilesInfo::new(vec![PathBuf::from(&self.path)]))
    }

    fn analyze(
        &self,
        ctx: &mut crate::analysis_context::AnalysisContext,
        _dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        use rush_action::ids::ArtifactKind;
        // Expose the ABSOLUTE source path in FilesInfo (not the bare package-relative
        // name). A genrule consuming this file via a cross-package label src
        // ($(SRCS)/$(location <src>)) runs from the workspace root, so a bare
        // "schema.fbs" doesn't resolve — which broke the flatbuffer_cc_library flatc
        // genrule (input not found -> flatc usage error). General source_file fix for
        // any genrule with a cross-package file label in srcs. (Codex 019eebc6.)
        let path = resolve_cc_src_path(ctx.workspace_root(), ctx.package(), &self.path);
        let aid = ctx.register_source_artifact(&path, ArtifactKind::Source);
        ProviderSet::empty()
            .with_files(crate::providers::FilesInfo::new(vec![path]))
            .with_default_info(crate::providers::DefaultInfo { files: vec![aid] })
    }

    fn rule_type(&self) -> &str {
        "source_file"
    }
}

/// Compatibility rule for legacy `exports_files` captures.
///
/// Normal BUILD evaluation treats `exports_files` as a native function and
/// emits one `source_file` target per listed file. This rule is retained only
/// for older parse paths that already materialize an `exports_files` RuleCall.
pub struct ExportsFilesRule;

#[async_trait]
impl BuildRule for ExportsFilesRule {
    fn name(&self) -> &str {
        "exports_files"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let files = extract_string_list(attrs, "srcs")
            .or_else(|_| extract_string_list(attrs, "files"))
            .unwrap_or_default();
        Ok(Box::new(ExportsFilesInstance { files }))
    }

    fn documentation(&self) -> &str {
        "Compatibility metadata for exports_files; per-file targets are source_file rules"
    }
}

pub struct ExportsFilesInstance {
    files: Vec<String>,
}

#[async_trait]
impl RuleInstance for ExportsFilesInstance {
    fn target_name(&self) -> &str {
        "exports_files"
    }

    fn dependencies(&self) -> Vec<String> {
        Vec::new()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let mut artifacts = Vec::with_capacity(self.files.len());
        for file in &self.files {
            let path = context.source_path(file);
            if !path.exists() {
                return Err(anyhow!(
                    "exported source file target {} not found at {}",
                    file,
                    path.display()
                ));
            }
            artifacts.push(Artifact {
                path,
                artifact_type: ArtifactType::Source,
            });
        }
        Ok(artifacts)
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty().with_files(crate::providers::FilesInfo::new(
            self.files.iter().map(PathBuf::from).collect(),
        ))
    }

    fn rule_type(&self) -> &str {
        "exports_files"
    }
}

// ===========================================================================
// cc_shared_library — Bazel's native shared-library rule.
// ===========================================================================
//
// Compared to `cc_binary(linkshared = True)`, `cc_shared_library` gives the
// build author explicit control over symbol ownership in a multi-`.so` world:
//
//   * `roots`: cc_library targets whose transitive closure this `.so`
//     should export (own statically).
//   * `deps`: additional cc_library targets whose transitive closure should
//     also be linked statically.
//   * `dynamic_deps`: other `cc_shared_library` targets this one should link
//     against dynamically. Any cc_library the dynamic_dep exports is
//     pruned from this target's static-link set — that's how Bazel prevents
//     ODR violations from the same cc_library getting baked into two
//     sibling .so's.
//   * `additional_linker_inputs`: files the linker needs at link time
//     (version scripts, exported-symbols lists). Like Bazel, these are
//     made available on disk but NOT auto-flagged — the real
//     `--version-script` / `-unexported_symbols_list` flags arrive through
//     `user_link_flags` (TF folds its per-OS `linkopts` select() in there).
//     A legacy `-Wl,--version-script=` auto-wrap survives only as a
//     back-compat fallback for ELF targets whose author supplied no
//     `--version-script` flag at all (see build() Phase 5 for the gate).
//   * `user_link_flags`: linker flags, passed through as-is.
//   * `exports_filter`: Bazel glob-pattern filter over which transitive
//     roots are exported. Rush currently treats everything reachable from
//     `roots` + `deps` as "owned" and doesn't filter further; documenting
//     the limitation here so consumers aren't surprised.
//
// Link-once invariant: a cc_library (identified by its emitted static
// archive path) may be linked statically into at most one
// `cc_shared_library` across the whole build. Rush enforces this via a
// module-local `LINK_ONCE_REGISTRY`: each `cc_shared_library`'s build
// claims every static lib it's about to pull in; any conflict (a lib
// already claimed by a different owner) aborts the build. This mirrors
// Bazel's "this rule enforces that a given cc_library ends up in at most
// one cc_shared_library" guarantee.

use std::sync::{Mutex, OnceLock};

/// Global map from static-library artifact path to the
/// `cc_shared_library` label that claimed it statically. Populated by
/// `CcSharedLibraryInstance::build`. Used to enforce the link-once
/// invariant across all shared libraries in the build.
///
/// Entries persist for the duration of the rush process; a subsequent
/// build within the same process sees the prior claims. That's the
/// correct semantic (the link-once check is a global property of the
/// build, not of a single target), but it means a unit test that
/// exercises this twice in the same process needs to clear the
/// registry between runs — see `clear_link_once_registry_for_tests`.
static LINK_ONCE_REGISTRY: OnceLock<Mutex<HashMap<PathBuf, String>>> =
    OnceLock::new();

fn link_once_registry() -> &'static Mutex<HashMap<PathBuf, String>> {
    LINK_ONCE_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Try to claim `lib_path` for `owner` (a `cc_shared_library` label).
///
/// Returns `Ok(())` if the claim is fresh or already held by the same
/// owner (idempotent — a rebuild of the same cc_shared_library target
/// must not trip on its own previous claim). Returns `Err` if the path
/// is already claimed by a *different* owner — that's the link-once
/// violation Bazel rejects at analysis time.
fn claim_for_shared_library(lib_path: &Path, owner: &str) -> Result<()> {
    let mut reg = link_once_registry()
        .lock()
        .map_err(|e| anyhow!("link-once registry poisoned: {}", e))?;
    match reg.get(lib_path) {
        Some(existing) if existing != owner => Err(anyhow!(
            "link-once violation: static library '{}' is already owned by \
             cc_shared_library '{}'; cannot also link it statically into '{}'. \
             One of the two shared libraries should add the other to its \
             `dynamic_deps` so the symbols come from a single .so.",
            lib_path.display(),
            existing,
            owner
        )),
        _ => {
            reg.insert(lib_path.to_path_buf(), owner.to_string());
            Ok(())
        }
    }
}

/// Append the plain (non-alwayslink) static archives to a
/// `cc_shared_library` link line, wrapping them in
/// `-Wl,--start-group ... -Wl,--end-group` when there is more than one so GNU
/// ld re-scans the bucket until every cross-archive reference resolves (#39(B);
/// see the call site in [`CcSharedLibraryInstance::build`] for the full ld
/// single-pass rationale). The grouping is inert for 0 or 1 archives (a group
/// of one resolves nothing on its second scan). Each archive path is also
/// recorded in `action_inputs` so the build graph tracks it as an input.
fn append_plain_static_libs_grouped(
    args: &mut Vec<String>,
    action_inputs: &mut Vec<PathBuf>,
    static_libs: &[PathBuf],
) {
    let grouped = static_libs.len() > 1;
    if grouped {
        args.push("-Wl,--start-group".to_string());
    }
    for lib in static_libs {
        args.push(lib.display().to_string());
        action_inputs.push(lib.clone());
    }
    if grouped {
        args.push("-Wl,--end-group".to_string());
    }
}

/// Test-only helper to reset the link-once registry between cases
/// running in the same process. Not part of the public API.
#[doc(hidden)]
pub fn clear_link_once_registry_for_tests() {
    if let Some(m) = LINK_ONCE_REGISTRY.get() {
        if let Ok(mut r) = m.lock() {
            r.clear();
        }
    }
}

/// Expand Bazel `$(location <label>)` / `$(execpath <label>)` "Make
/// variable" references inside a single linker flag string, using
/// `resolve` to turn a label into its on-disk path.
///
/// TF's `tf_cc_shared_library` macro folds per-OS `linkopts` (which use
/// `$(location ...)` to point at version scripts) into the
/// `cc_shared_library`'s `user_link_flags`. Rush resolves the `select()`
/// but does NOT otherwise expand `$(location ...)`, so without this the
/// literal string `$(location //tensorflow:tf_framework_version_script.lds)`
/// reaches `ld` ("cannot open linker script file $(location ...)").
///
/// Only single-label `location`/`execpath` are handled — that is all TF's
/// shared-library linkopts use. Plural forms (`locations`/`execpaths`) and
/// unknown `$(...)` make-vars are left verbatim for the caller/linker.
fn expand_link_flag_locations<F: Fn(&str) -> PathBuf>(flag: &str, resolve: &F) -> String {
    const KEYWORDS: [&str; 2] = ["$(location ", "$(execpath "];
    let mut out = String::with_capacity(flag.len());
    let mut rest = flag;
    loop {
        // Find the earliest occurrence of any supported keyword.
        let next = KEYWORDS
            .iter()
            .filter_map(|kw| rest.find(kw).map(|idx| (idx, kw.len())))
            .min_by_key(|(idx, _)| *idx);
        let (idx, kwlen) = match next {
            Some(v) => v,
            None => {
                out.push_str(rest);
                break;
            }
        };
        out.push_str(&rest[..idx]);
        let after = &rest[idx + kwlen..];
        match after.find(')') {
            Some(close) => {
                let label = after[..close].trim();
                out.push_str(&resolve(label).display().to_string());
                rest = &after[close + 1..];
            }
            None => {
                // Malformed (no closing paren) — emit the remainder as-is.
                out.push_str(&rest[idx..]);
                break;
            }
        }
    }
    out
}

/// Convert Bazel's shell-escaped `$ORIGIN` linkopts to direct argv syntax.
///
/// TensorFlow's CUDA rules emit rpaths such as
/// `-Wl,-rpath,'$$ORIGIN/../nvidia/cudnn/lib'`. Bazel expands those through a
/// shell, which strips the quotes and collapses `$$` to `$`. Rush invokes the
/// compiler driver directly, so forwarding the spelling verbatim records the
/// quote and doubled dollar characters in DT_RUNPATH. Limit normalization to
/// arguments containing the dynamic loader's ORIGIN token; every other linker
/// argument remains byte-for-byte unchanged.
fn normalize_bazel_origin_link_arg(flag: &str) -> String {
    if !flag.contains("$ORIGIN") && !flag.contains("${ORIGIN}") {
        return flag.to_string();
    }

    let mut normalized = String::with_capacity(flag.len());
    let mut chars = flag.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\'' | '"' => {}
            '\\' if chars.peek() == Some(&'$') => {}
            '$' if chars.peek() == Some(&'$') => {
                chars.next();
                normalized.push('$');
            }
            _ => normalized.push(character),
        }
    }
    normalized
}

fn normalize_bazel_origin_link_args(args: &mut [String]) {
    for argument in args {
        *argument = normalize_bazel_origin_link_arg(argument);
    }
}

#[cfg(test)]
mod expand_link_flag_locations_tests {
    use super::{
        cc_obj_name_for_src, expand_link_flag_locations, normalize_bazel_origin_link_arg,
    };
    use std::path::PathBuf;

    fn fake_resolve(label: &str) -> PathBuf {
        // Mimic source_path: //pkg:file.lds -> pkg/file.lds
        let stripped = label.trim_start_matches('@').trim_start_matches('/');
        PathBuf::from(stripped.replace(':', "/"))
    }

    #[test]
    fn triton_same_basename_srcs_get_distinct_object_names() {
        // @triton//:TritonDialects globs lib/Dialect/{Triton,TritonGPU,
        // TritonNvidiaGPU}/IR/*.cpp — three distinct `Ops.cpp`. Each must
        // compile to a DISTINCT object, or the core Triton dialect's
        // MLIR_DEFINE_EXPLICIT_TYPE_ID statics get clobbered and the .so won't
        // dlopen. The directory component disambiguates package-relative srcs.
        let a = cc_obj_name_for_src("lib/Dialect/Triton/IR/Ops.cpp", "Ops");
        let b = cc_obj_name_for_src("lib/Dialect/TritonGPU/IR/Ops.cpp", "Ops");
        let c = cc_obj_name_for_src("lib/Dialect/TritonNvidiaGPU/IR/Ops.cpp", "Ops");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn relative_subpath_srcs_disambiguate_by_dir() {
        // Two same-basename files in different subdirs of one package.
        assert_ne!(
            cc_obj_name_for_src("foo/x.cc", "x"),
            cc_obj_name_for_src("bar/x.cc", "x"),
        );
        // A bare same-package file (no subdir) keeps its unqualified name.
        assert_eq!(
            cc_obj_name_for_src("x.cc", "x"),
            cc_obj_name_for_src("x.cc", "x"),
        );
    }

    #[test]
    fn expands_single_location() {
        let out = expand_link_flag_locations(
            "-Wl,--version-script,$(location //tensorflow:tf_framework_version_script.lds)",
            &fake_resolve,
        );
        assert_eq!(
            out,
            "-Wl,--version-script,tensorflow/tf_framework_version_script.lds"
        );
    }

    #[test]
    fn expands_execpath_and_leaves_plain_flags() {
        assert_eq!(
            expand_link_flag_locations("-Wl,-soname,libfoo.so", &fake_resolve),
            "-Wl,-soname,libfoo.so"
        );
        assert_eq!(
            expand_link_flag_locations("$(execpath //a:b.map)", &fake_resolve),
            "a/b.map"
        );
    }

    #[test]
    fn expands_multiple_in_one_flag() {
        let out = expand_link_flag_locations(
            "$(location //a:x.lds):$(location //b:y.lds)",
            &fake_resolve,
        );
        assert_eq!(out, "a/x.lds:b/y.lds");
    }

    #[test]
    fn malformed_no_close_paren_left_verbatim() {
        let s = "-Wl,--version-script,$(location //a:x.lds";
        assert_eq!(expand_link_flag_locations(s, &fake_resolve), s);
    }

    #[test]
    fn unknown_makevar_left_verbatim() {
        let s = "-L$(GENDIR)/foo";
        assert_eq!(expand_link_flag_locations(s, &fake_resolve), s);
    }

    #[test]
    fn normalizes_shell_escaped_origin_rpaths() {
        assert_eq!(
            normalize_bazel_origin_link_arg(
                "-Wl,-rpath,'$$ORIGIN/../cuda':'$${ORIGIN}/../cudnn'"
            ),
            "-Wl,-rpath,$ORIGIN/../cuda:${ORIGIN}/../cudnn"
        );
        assert_eq!(
            normalize_bazel_origin_link_arg(r"-Wl,-rpath,\$ORIGIN/lib"),
            "-Wl,-rpath,$ORIGIN/lib"
        );
    }

    #[test]
    fn preserves_direct_origin_and_unrelated_linkopts() {
        assert_eq!(
            normalize_bazel_origin_link_arg("-Wl,-rpath,$ORIGIN/lib"),
            "-Wl,-rpath,$ORIGIN/lib"
        );
        assert_eq!(
            normalize_bazel_origin_link_arg("-Wl,--defsym='VALUE=1'"),
            "-Wl,--defsym='VALUE=1'"
        );
    }
}

#[cfg(test)]
mod append_plain_static_libs_grouped_tests {
    //! #39(B) regression: `cc_shared_library`'s plain (non-alwayslink) static
    //! archives must be wrapped in `-Wl,--start-group ... -Wl,--end-group` when
    //! there is more than one, so GNU ld re-scans the bucket until every
    //! cross-archive reference resolves. Without the group, a one-pass ld
    //! leaves out-of-order members undefined in the .so (manifested as
    //! `tsl::PosixFileSystem` undefined symbols in `libtensorflow_cc.so`).
    use super::append_plain_static_libs_grouped;
    use std::path::PathBuf;

    fn run(libs: &[&str]) -> (Vec<String>, Vec<PathBuf>) {
        let mut args = Vec::new();
        let mut inputs = Vec::new();
        let paths: Vec<PathBuf> = libs.iter().map(PathBuf::from).collect();
        append_plain_static_libs_grouped(&mut args, &mut inputs, &paths);
        (args, inputs)
    }

    #[test]
    fn multiple_archives_are_wrapped_in_one_group() {
        let (args, inputs) = run(&["a/libfoo.a", "b/libbar.a", "c/libbaz.a"]);
        assert_eq!(
            args,
            vec![
                "-Wl,--start-group".to_string(),
                "a/libfoo.a".to_string(),
                "b/libbar.a".to_string(),
                "c/libbaz.a".to_string(),
                "-Wl,--end-group".to_string(),
            ],
            "3 plain archives must be wrapped in exactly one start/end-group"
        );
        // Exactly one group — not a group per archive.
        assert_eq!(args.iter().filter(|a| *a == "-Wl,--start-group").count(), 1);
        assert_eq!(args.iter().filter(|a| *a == "-Wl,--end-group").count(), 1);
        // Every archive is also recorded as a build-graph input.
        assert_eq!(inputs.len(), 3);
    }

    #[test]
    fn two_archives_still_grouped() {
        let (args, _) = run(&["a/libfoo.a", "b/libbar.a"]);
        assert_eq!(args.first().map(String::as_str), Some("-Wl,--start-group"));
        assert_eq!(args.last().map(String::as_str), Some("-Wl,--end-group"));
    }

    #[test]
    fn single_archive_not_grouped() {
        // A group of one resolves nothing on its second scan, so it is inert
        // noise — emit the bare path.
        let (args, inputs) = run(&["a/libfoo.a"]);
        assert_eq!(args, vec!["a/libfoo.a".to_string()]);
        assert_eq!(inputs, vec![PathBuf::from("a/libfoo.a")]);
    }

    #[test]
    fn empty_emits_nothing() {
        let (args, inputs) = run(&[]);
        assert!(args.is_empty());
        assert!(inputs.is_empty());
    }
}

/// Deduplicate a transitive link stream and place it in one linker group.
///
/// External adapters may contribute their own archive groups through
/// `linkopts`. A consumer-level group already provides the same fixed-point
/// archive scan over the entire stream, so retaining those delimiters would
/// create nested groups, which GNU gold rejects.
fn group_link_flags(flags: Vec<String>) -> Vec<String> {
    let deduped = dedup_archive_link_flags(flags);
    let mut grouped = Vec::with_capacity(deduped.len() + 2);
    grouped.push("-Wl,--start-group".to_string());
    grouped.extend(
        deduped
            .into_iter()
            .filter(|flag| flag != "-Wl,--start-group" && flag != "-Wl,--end-group"),
    );
    grouped.push("-Wl,--end-group".to_string());
    grouped
}

/// Archive-deduplicate and group dependency arguments without erasing whether
/// each token came from structured CcInfo or arbitrary propagated `linkopts`.
/// The flat `group_link_flags` result remains the command-spec representation;
/// this form is the response-transport authority.
fn group_cc_binary_dep_args(flags: Vec<CcBinaryDepArg>) -> Vec<CcBinaryDepArg> {
    let mut seen_paths: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let is_archive_path = |arg: &CcBinaryDepArg| -> bool {
        let value = arg.value();
        if value.starts_with('-') {
            return false;
        }
        let ext = std::path::Path::new(value)
            .extension()
            .and_then(|extension| extension.to_str());
        matches!(ext, Some("a") | Some("so")) || value.contains(".so.")
    };
    let mut deduped = Vec::with_capacity(flags.len());
    let mut index = 0usize;
    while index < flags.len() {
        if flags[index].value() == "-Wl,--whole-archive"
            && index + 2 < flags.len()
            && flags[index + 2].value() == "-Wl,--no-whole-archive"
            && is_archive_path(&flags[index + 1])
        {
            if seen_paths.insert(flags[index + 1].value().to_string()) {
                deduped.extend_from_slice(&flags[index..index + 3]);
            }
            index += 3;
            continue;
        }
        if is_archive_path(&flags[index]) {
            if seen_paths.insert(flags[index].value().to_string()) {
                deduped.push(flags[index].clone());
            }
        } else {
            deduped.push(flags[index].clone());
        }
        index += 1;
    }

    let mut grouped = Vec::with_capacity(deduped.len() + 2);
    grouped.push(CcBinaryDepArg::structured("-Wl,--start-group"));
    grouped.extend(deduped.into_iter().filter(|arg| {
        arg.value() != "-Wl,--start-group"
            && arg.value() != "-Wl,--end-group"
    }));
    grouped.push(CcBinaryDepArg::structured("-Wl,--end-group"));
    grouped
}

#[cfg(test)]
mod dedup_archive_link_flags_tests {
    use super::{
        cc_binary_dep_arg_values, dedup_archive_link_flags,
        group_cc_binary_dep_args, group_link_flags, CcBinaryDepArg,
    };

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tagged_grouping_matches_flat_argv_and_preserves_opaque_flags() {
        let tagged = vec![
            CcBinaryDepArg::structured("-Wl,--whole-archive"),
            CcBinaryDepArg::structured("/x/libA.a"),
            CcBinaryDepArg::structured("-Wl,--no-whole-archive"),
            CcBinaryDepArg::opaque("-lazy_framework"),
            CcBinaryDepArg::opaque("Cocoa"),
            CcBinaryDepArg::structured("/x/libB.a"),
            CcBinaryDepArg::structured("/x/libA.a"),
        ];
        let flat = cc_binary_dep_arg_values(&tagged);
        let grouped = group_cc_binary_dep_args(tagged);
        assert_eq!(
            cc_binary_dep_arg_values(&grouped),
            group_link_flags(flat)
        );
        assert!(grouped.iter().any(|arg| {
            matches!(arg, CcBinaryDepArg::Opaque(value) if value == "-lazy_framework")
        }));
        assert!(grouped.iter().any(|arg| {
            matches!(arg, CcBinaryDepArg::Opaque(value) if value == "Cocoa")
        }));
    }

    #[test]
    fn cross_bucket_whole_archive_then_plain() {
        // libLLVMSupport.a arrives whole-archive-wrapped (alwayslink) first,
        // then plain (normal bucket). The plain duplicate must be dropped;
        // the whole-archive inclusion wins.
        let out = dedup_archive_link_flags(v(&[
            "-shared",
            "-o",
            "out/libtf.so.2.15.0",
            "-Wl,--whole-archive",
            "/x/libLLVMSupport.a",
            "-Wl,--no-whole-archive",
            "/x/libOther.a",
            "/x/libLLVMSupport.a",
        ]));
        assert_eq!(
            out,
            v(&[
                "-shared",
                "-o",
                "out/libtf.so.2.15.0",
                "-Wl,--whole-archive",
                "/x/libLLVMSupport.a",
                "-Wl,--no-whole-archive",
                "/x/libOther.a",
            ])
        );
    }

    #[test]
    fn plain_then_whole_archive_keeps_first() {
        // First occurrence wins regardless of form.
        let out = dedup_archive_link_flags(v(&[
            "/x/a.a",
            "-Wl,--whole-archive",
            "/x/a.a",
            "-Wl,--no-whole-archive",
        ]));
        assert_eq!(out, v(&["/x/a.a"]));
    }

    #[test]
    fn flags_and_libs_preserved_and_idempotent_flags_kept() {
        let out = dedup_archive_link_flags(v(&[
            "-lpthread", "/x/a.a", "-lpthread", "/x/a.a", "-Wl,-soname,libtf.so",
        ]));
        // -lpthread kept both times (idempotent flags untouched); a.a deduped.
        assert_eq!(
            out,
            v(&["-lpthread", "/x/a.a", "-lpthread", "-Wl,-soname,libtf.so"])
        );
    }

    #[test]
    fn output_so_preserved() {
        // The output .so (archive-like) is unique -> preserved in place.
        let out = dedup_archive_link_flags(v(&["-o", "out/libtf.so.2.15.0", "/x/a.a"]));
        assert_eq!(out, v(&["-o", "out/libtf.so.2.15.0", "/x/a.a"]));
    }

    #[test]
    fn outer_group_flattens_transitive_groups() {
        let out = group_link_flags(v(&[
            "/x/a.a",
            "-Wl,--start-group",
            "/x/b.a",
            "-Wl,--end-group",
            "-lpthread",
        ]));
        assert_eq!(
            out,
            v(&[
                "-Wl,--start-group",
                "/x/a.a",
                "/x/b.a",
                "-lpthread",
                "-Wl,--end-group",
            ])
        );
    }
}

/// Archive-aware de-duplication of a linker flag list.
///
/// Removes a repeated archive path — and its surrounding
/// `-Wl,--whole-archive` / `-Wl,--no-whole-archive` wrap, if present — when
/// the same archive has already been emitted. Non-path flags (`-l…`, `-L…`,
/// `-Wl,...` control directives, object files) are preserved in order and
/// never deduped (they are idempotent on ld's command line, or order
/// matters). The first occurrence wins, so a `--whole-archive`-wrapped
/// archive emitted before a later plain mention keeps its whole-archive
/// inclusion — exactly what `alwayslink` semantics require.
///
/// A single `seen_paths` namespace means this also collapses the same
/// archive arriving through different buckets (plain vs alwayslink) or via
/// duplicated transitive link fragments — the cause of the thousands of
/// "multiple definition of llvm::sys::Watchdog" errors when linking
/// libtensorflow_framework.so (libLLVMSupport.a reached the link both
/// whole-archive-wrapped and plain).
/// Quote a single argument for a GNU-compatible `@response-file`.
///
/// GCC-family drivers and linkers split on unquoted whitespace, honour
/// single/double quotes, and treat backslash as an escape. We emit each
/// argument double-quoted with `\` and `"` escaped — robust for paths
/// containing spaces or quotes while staying a no-surprise identity for the
/// dash-flags and rush-out paths that dominate a real link line.
fn quote_response_file_arg(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// True for tokens the *compiler driver* (g++) must interpret itself — `ld`
/// rejects them (e.g. `-pthread`, `-shared`, `-fPIC`). These stay on the g++
/// command line; everything else is a linker token.
fn is_driver_only_link_flag(t: &str) -> bool {
    const EXACT: &[&str] = &["-pthread", "-shared", "-static-libstdc++", "-static-libgcc"];
    const PREFIX: &[&str] = &[
        "-f", "-m", "-static", "-shared-libgcc", "-pie", "-no-pie", "-nostdlib",
        "-nodefaultlibs", "-nostartfiles", "--sysroot", "-specs", "-B",
    ];
    EXACT.contains(&t) || PREFIX.iter().any(|p| t.starts_with(p))
}

/// Partition a C++ link argv into `(driver_flags, ld_tokens)` for the
/// `-Wl,@<file>` ARG_MAX workaround. See the `cc_binary` and
/// `cc_shared_library` call sites for the execution rationale.
///
/// * Driver-only flags ([`is_driver_only_link_flag`]) and the `-o <out>` pair
///   stay on the g++ command line and are **deduplicated** — 15k repeated
///   `-pthread` tokens would otherwise re-bloat `COLLECT_GCC_OPTIONS` past the
///   per-string `MAX_ARG_STRLEN` limit, defeating the workaround.
/// * Every linker token (objects, archives, `--whole-archive` groups,
///   `-soname`, `-rpath`, `--version-script`, `-l`/`-L`) goes into `ld_tokens`
///   in *ld syntax*: `-Wl,a,b,c` is split into `a b c` (the `-Wl,` prefix
///   stripped). Original order is preserved so `--whole-archive` /
///   `--no-whole-archive` grouping stays intact.
fn partition_link_argv(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut driver_flags: Vec<String> = Vec::new();
    let mut seen_driver: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ld_tokens: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let a = &args[i];
        if a == "-o" && i + 1 < args.len() {
            driver_flags.push("-o".to_string());
            driver_flags.push(args[i + 1].clone());
            i += 2;
            continue;
        }
        if let Some(rest) = a.strip_prefix("-Wl,") {
            for tok in rest.split(',') {
                if !tok.is_empty() {
                    ld_tokens.push(tok.to_string());
                }
            }
            i += 1;
            continue;
        }
        if is_driver_only_link_flag(a) {
            if seen_driver.insert(a.clone()) {
                driver_flags.push(a.clone());
            }
            i += 1;
            continue;
        }
        ld_tokens.push(a.clone());
        i += 1;
    }
    (driver_flags, ld_tokens)
}

pub fn dedup_archive_link_flags(flags: Vec<String>) -> Vec<String> {
    let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut deduped: Vec<String> = Vec::with_capacity(flags.len());
    let is_archive_path = |s: &str| -> bool {
        if s.starts_with('-') {
            return false;
        }
        let ext = std::path::Path::new(s).extension().and_then(|e| e.to_str());
        matches!(ext, Some("a") | Some("so")) || s.contains(".so.")
    };
    let mut i = 0usize;
    while i < flags.len() {
        let flag = &flags[i];
        // Pattern: [--whole-archive, <path>, --no-whole-archive]
        if flag == "-Wl,--whole-archive"
            && i + 2 < flags.len()
            && flags[i + 2] == "-Wl,--no-whole-archive"
            && is_archive_path(&flags[i + 1])
        {
            let path = &flags[i + 1];
            if seen_paths.insert(path.clone()) {
                deduped.push(flags[i].clone());
                deduped.push(flags[i + 1].clone());
                deduped.push(flags[i + 2].clone());
            }
            i += 3;
            continue;
        }
        if is_archive_path(flag) {
            if seen_paths.insert(flag.clone()) {
                deduped.push(flag.clone());
            }
        } else {
            deduped.push(flag.clone());
        }
        i += 1;
    }
    deduped
}

fn push_unique_path(
    ordered: &mut Vec<std::path::PathBuf>,
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
    path: std::path::PathBuf,
) {
    if seen.insert(path.clone()) {
        ordered.push(path);
    }
}

/// `nm` type codes that count as a *strong* defining symbol — i.e. a definition
/// GNU `ld` would extract an archive member to satisfy (so its presence in a
/// dynamic_dep `.so` is reliable evidence the dep pulled, and thus owns, that
/// archive).
///
/// #39 weak-symbol false positive: the previous prune counted ANY single-letter
/// nm type as ownership evidence, but weak (`W`/`V`/`w`/`v`) and GNU-unique
/// (`u`) symbols are ubiquitous template/inline instantiations (`Eigen::*`,
/// `std::*`, `__gnu_cxx::*`, `tsl::strings::AlphaNum`) that every TU emits, so
/// `libtensorflow_framework.so` carries thousands of them from its OWN objects.
/// Counting those made almost every advertised archive look "owned" — e.g.
/// `libcaptured_function.a` (23 strong + 3366 weak globals) matched 2545 weak
/// framework exports and 0 strong, so it was pruned and `CapturedFunction` went
/// undefined; `libstats_utils.a` likewise (framework never even pulled
/// `stats_utils.o`). A weak undefined reference resolves to 0 (no load failure)
/// and weak definitions are interposable, so weak symbols are not evidence of
/// member extraction. We therefore use an allowlist of strong types only.
///
/// Allowlist (Codex-reviewed): `T` text, `D`/`G` data, `B`/`S` bss, `R` rodata,
/// `C` common, `I`/`i` IFUNC. Excluded: weak `W`/`V`/`w`/`v`, GNU-unique `u`,
/// debug `n`/`N`, and lowercase locals (which `nm -g` already drops).
fn is_strong_evidence_nm_type(ty: &str) -> bool {
    matches!(ty, "T" | "D" | "B" | "R" | "C" | "G" | "S" | "I" | "i")
}

/// Collect the (mangled) symbol names a binary or archive actually
/// *defines*, via `nm --defined-only`.
///
/// #39 symbol-aware dynamic-dep prune: a `cc_shared_library`'s
/// `CcSharedLibraryInfo.exports` lists the archive PATHS that dependency
/// placed on its own static link line — but GNU `ld` only extracts archive
/// members that satisfy a reference, so an advertised archive can contribute
/// ZERO symbols to the produced `.so`. Pruning such an archive from a
/// consumer by path drops its definitions from BOTH shared libraries,
/// leaving the symbols undefined at load (observed: `xla_data_proto_cc_impl.a`
/// is on `libtensorflow_framework.so`'s link line yet framework exports none
/// of `xla::ShapeProto`'s members — pruning it from `libtensorflow_cc.so`
/// produced 1166 real undefined symbols). We therefore prune only archives
/// whose symbols the dynamic_dep `.so` genuinely defines.
///
/// `extra_args` lets callers pass the right `nm` mode: `-D` for a shared
/// object's dynamic symbol table, `-g` for an archive's global symbols.
/// `strong_only` keeps only [`is_strong_evidence_nm_type`] definitions — pass
/// `true` for the archive-ownership-evidence side (`-g`) so ubiquitous weak
/// template symbols don't produce false "owned" verdicts, and `false` for the
/// dynamic_dep `.so` provider side (`-D`), where a weak DEFINED export still
/// satisfies a runtime relocation.
/// Returns an empty set if `nm` is unavailable or errors; callers treat an
/// empty dynamic-symbol set as "cannot prove ownership" and fall back to the
/// historical path-based prune.
fn nm_defined_symbols(
    nm: &str,
    file: &Path,
    extra_args: &[&str],
    strong_only: bool,
) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let mut cmd = std::process::Command::new(nm);
    cmd.arg("--defined-only");
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.arg(file);
    let Ok(out) = cmd.output() else {
        return set;
    };
    if !out.status.success() {
        return set;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // `nm` defined-symbol lines have the BSD shape "<addr> <type> <name>"
        // (a single-letter type such as T/t/W/D/B/R). Archive member-header
        // lines ("foo.o:") and blank separators have a different shape and
        // are skipped. Mangled names contain no spaces, so the name is the
        // third and final whitespace field.
        let mut it = line.split_whitespace();
        let (Some(_addr), Some(ty), Some(name), None) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        // When `strong_only`, drop weak/unique/debug types so they can't act as
        // archive-ownership evidence (see `is_strong_evidence_nm_type`).
        if ty.len() == 1 && (!strong_only || is_strong_evidence_nm_type(ty)) {
            set.insert(name.to_string());
        }
    }
    set
}

/// #39 link-speed: choose a PARALLEL linker driver flag in place of the (slow,
/// single-threaded) GNU bfd `ld` default. The `cc_shared_library` mega-link of
/// libtensorflow_cc.so (1.5 GB, thousands of archives inside one
/// `--start-group`) runs ~110 min under bfd — one CPU-bound process, no
/// parallelism, and the start-group forces repeated full re-scans. mold / lld
/// are multi-threaded and resolve symbols globally (no re-scan), cutting that
/// to minutes. Honors `RUSH_LINKER` (`mold`|`lld`|`gold`|`bfd`); otherwise
/// auto-detects `mold > lld > gold`. Returns `None` (→ bfd, unchanged) when none
/// is installed or `RUSH_LINKER=bfd`, so hosts without a fast linker are
/// unaffected. mold is "compatible with GNU ld" and handles `--start-group` /
/// `--whole-archive` / `--version-script` / `-soname` identically.
pub fn fast_linker_driver_flag() -> Option<String> {
    use std::sync::OnceLock;

    fn have(bin: &str) -> bool {
        std::process::Command::new(bin)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    match std::env::var("RUSH_LINKER").ok().as_deref() {
        Some("bfd") | Some("ld") => None,
        Some("mold") => Some("-fuse-ld=mold".to_string()),
        Some("lld") => Some("-fuse-ld=lld".to_string()),
        Some("gold") => Some("-fuse-ld=gold".to_string()),
        _ => {
            static AUTO_DETECTED: OnceLock<Option<String>> = OnceLock::new();
            AUTO_DETECTED
                .get_or_init(|| {
                    if have("ld.mold") || have("mold") {
                        Some("-fuse-ld=mold".to_string())
                    } else if have("ld.lld") {
                        Some("-fuse-ld=lld".to_string())
                    } else if have("ld.gold") {
                        Some("-fuse-ld=gold".to_string())
                    } else {
                        None
                    }
                })
                .clone()
        }
    }
}

// Producer-stamp promotion helpers removed: stamps carry
// ArtifactFingerprints (values) now, not producer ActionIds, and the
// only caller of this dead block was itself unreferenced.

fn link_input_has_archive_or_library_flag(flag: &str) -> bool {
    if flag.starts_with("-l") {
        return true;
    }
    if flag.contains("--threads") || flag.contains("--no-threads") {
        return true;
    }
    if flag.contains(".a") || flag.contains(".so.") {
        return true;
    }
    matches!(
        flag,
        "-Wl,--whole-archive" | "-Wl,--no-whole-archive" | "-Wl,--start-group" | "-Wl,--end-group"
    )
}

fn link_argv_approx_len(
    object_paths: &[PathBuf],
    dep_flags: &[String],
    linkopts: &[String],
    ldflags: &[String],
) -> usize {
    object_paths
        .iter()
        .map(|p| p.as_os_str().len() + 1)
        .sum::<usize>()
        + dep_flags
            .iter()
            .chain(linkopts.iter())
            .chain(ldflags.iter())
            .map(|s| s.len() + 1)
            .sum::<usize>()
        + 8
}

fn mold_small_link_threads_flag(
    linker_flag: Option<&str>,
    object_paths: &[PathBuf],
    dep_flags: &[String],
    linkopts: &[String],
    ldflags: &[String],
) -> Option<String> {
    if linker_flag != Some("-fuse-ld=mold") {
        return None;
    }

    match parse_mold_small_link_threads_override(
        std::env::var("RUSH_MOLD_SMALL_LINK_THREADS").ok(),
    ) {
        MoldSmallLinkThreadsOverride::Disabled => return None,
        MoldSmallLinkThreadsOverride::Flag(flag) => return Some(flag),
        MoldSmallLinkThreadsOverride::Unset => {}
    }

    if object_paths.len() < 128 {
        return None;
    }
    let approx_len = link_argv_approx_len(object_paths, dep_flags, linkopts, ldflags);
    if approx_len < 16 * 1024 {
        return None;
    }
    if approx_len > 64 * 1024 {
        return None;
    }
    if dep_flags
        .iter()
        .chain(linkopts.iter())
        .chain(ldflags.iter())
        .any(|flag| link_input_has_archive_or_library_flag(flag))
    {
        return None;
    }

    Some("-Wl,--threads=3".to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MoldSmallLinkThreadsOverride {
    Unset,
    Disabled,
    Flag(String),
}

fn parse_mold_small_link_threads_override(raw: Option<String>) -> MoldSmallLinkThreadsOverride {
    let Some(raw) = raw else {
        return MoldSmallLinkThreadsOverride::Unset;
    };
    let value = raw.trim();
    if value.is_empty() || value == "0" || value.eq_ignore_ascii_case("off") {
        return MoldSmallLinkThreadsOverride::Disabled;
    }
    if value.eq_ignore_ascii_case("no-threads") {
        return MoldSmallLinkThreadsOverride::Flag("-Wl,--no-threads".to_string());
    }
    MoldSmallLinkThreadsOverride::Flag(format!("-Wl,--threads={value}"))
}

fn fast_linker_driver_flags(
    object_paths: &[PathBuf],
    dep_flags: &[String],
    linkopts: &[String],
    ldflags: &[String],
) -> Vec<String> {
    let driver_flag = fast_linker_driver_flag();
    let mut out = Vec::new();
    if let Some(flag) = driver_flag.as_ref() {
        out.push(flag.clone());
    }
    if let Some(flag) = mold_small_link_threads_flag(
        driver_flag.as_deref(),
        object_paths,
        dep_flags,
        linkopts,
        ldflags,
    ) {
        out.push(flag);
    }
    out
}

/// Strong (`U`) undefined symbol names referenced across `files`, batched into
/// `nm --undefined-only` invocations (chunked to stay under `ARG_MAX` with the
/// thousands of archives a top-level `cc_shared_library` can carry).
///
/// #39 partial-export prune: this is the "what does the consumer actually
/// NEED?" side of the symbol-aware dynamic-dep decision. Only STRONG undefined
/// (`U`) symbols count — a weak undefined (`w`/`v`) resolves to 0 with no load
/// failure, so it is not evidence the consumer requires a definition and must
/// not force-retain an archive the dynamic_dep otherwise owns.
fn nm_undefined_symbols_batch(nm: &str, files: &[PathBuf]) -> std::collections::HashSet<String> {
    use rayon::prelude::*;
    if files.is_empty() {
        return std::collections::HashSet::new();
    }
    // Fix B: scan the 256-file chunks in PARALLEL (rayon). For the full
    // //tensorflow:tensorflow_cc closure the "definite" (non-advertised) set is
    // ~2300 archives → ~9 chunks; running them concurrently turns the dominant
    // remaining MISS-path serial cost into a parallel one. Correctness-neutral:
    // each chunk's parse is independent and the per-chunk sets are unioned (the
    // same union the old sequential loop produced).
    files
        .par_chunks(256)
        .map(|chunk| {
            let mut set = std::collections::HashSet::new();
            let mut cmd = std::process::Command::new(nm);
            cmd.arg("--undefined-only");
            for f in chunk {
                cmd.arg(f);
            }
            let Ok(out) = cmd.output() else {
                return set;
            };
            if !out.status.success() {
                return set;
            }
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                // `nm --undefined-only` lines have the BSD shape "<type> <name>"
                // with NO address column (e.g. "                 U _ZN...").
                // Per-member ("foo.o:") and per-file ("path/lib.a:") headers and
                // blank separators have a different field shape and are skipped.
                // Only strong undefined (`U`) are real requirements — weak
                // undefined print as `w`/`v` and are intentionally ignored.
                let mut it = line.split_whitespace();
                if let (Some(ty), Some(name), None) = (it.next(), it.next(), it.next()) {
                    if ty == "U" {
                        set.insert(name.to_string());
                    }
                }
            }
            set
        })
        .reduce(std::collections::HashSet::new, |mut a, b| {
            a.extend(b);
            a
        })
}

/// Strong global symbol names DEFINED across `files`, batched like
/// [`nm_undefined_symbols_batch`]. Same evidence rules as
/// [`nm_defined_symbols`] with `strong_only = true`, but over many files in
/// one `nm -g --defined-only` invocation (we only need the union, not
/// per-file attribution, for the consumer-need computation).
fn nm_defined_symbols_batch(nm: &str, files: &[PathBuf]) -> std::collections::HashSet<String> {
    use rayon::prelude::*;
    if files.is_empty() {
        return std::collections::HashSet::new();
    }
    // Fix B: parallel over 256-file chunks (see nm_undefined_symbols_batch).
    files
        .par_chunks(256)
        .map(|chunk| {
            let mut set = std::collections::HashSet::new();
            let mut cmd = std::process::Command::new(nm);
            cmd.arg("--defined-only");
            cmd.arg("-g");
            for f in chunk {
                cmd.arg(f);
            }
            let Ok(out) = cmd.output() else {
                return set;
            };
            if !out.status.success() {
                return set;
            }
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let mut it = line.split_whitespace();
                let (Some(_addr), Some(ty), Some(name), None) =
                    (it.next(), it.next(), it.next(), it.next())
                else {
                    continue;
                };
                if ty.len() == 1 && is_strong_evidence_nm_type(ty) {
                    set.insert(name.to_string());
                }
            }
            set
        })
        .reduce(std::collections::HashSet::new, |mut a, b| {
            a.extend(b);
            a
        })
}

/// #39 partial-export prune (the consumer-side, per-symbol fix). Decide which
/// dynamic_dep-advertised archives a `cc_shared_library` must KEEP on its OWN
/// static link line, even though a `dynamic_dep` nominally "owns" them.
///
/// The earlier all-or-nothing rule ([`dynamic_dep_provides_archive`]) pruned an
/// advertised archive whenever the dynamic_dep `.so` exported *any one* of its
/// strong symbols. But GNU `ld` extracts archive *members* individually, so a
/// `.so` can pull SOME members of an archive and not others: e.g.
/// `libgraph.a` has 71 strong globals, `libtensorflow_framework.so` exports 35
/// of them (the `Graph`/`Node` members its own code references) and NONE of the
/// 36 `CostModel::*` (no internal referrer pulled `costmodel.o`). Pruning the
/// whole archive then left `CostModel::*` undefined in `libtensorflow_cc.so`.
///
/// The fix is per-symbol coverage: keep an advertised archive iff it DEFINES a
/// strong symbol the consumer actually REFERENCES that the dynamic_dep does NOT
/// export. `ld`'s member-level extraction (with the dynamic `.so` ahead of the
/// static archive in link order — see the caller) then pulls only the orphaned
/// members (`costmodel.o`) and resolves the already-exported ones dynamically,
/// so no duplicate definitions arise.
///
/// `never_pulled` carries the conservative safety net from the old predicate:
/// archives whose symbols the `.so` exports NONE of (it dropped them entirely,
/// e.g. `xla_data_proto_cc_impl`) are always kept regardless of the demand
/// analysis — pruning them is exactly the 1166-undefined regression this whole
/// machinery exists to avoid.
///
/// Pure over precomputed symbol sets so the decision is unit-testable. The loop
/// is a monotone fixpoint (kept set only grows; each kept archive's own
/// undefined refs become new demand that can pull in further archives), so it
/// terminates in at most `advertised.len()` rounds.
fn advertised_archives_to_keep(
    advertised: &[PathBuf],
    adv_defs: &HashMap<PathBuf, std::collections::HashSet<String>>,
    adv_undefs: &HashMap<PathBuf, std::collections::HashSet<String>>,
    never_pulled: &std::collections::HashSet<PathBuf>,
    definite_defined: &std::collections::HashSet<String>,
    definite_undef: &std::collections::HashSet<String>,
    dyn_defined: &std::collections::HashSet<String>,
) -> std::collections::HashSet<PathBuf> {
    // Everything the consumer can satisfy WITHOUT any advertised archive:
    // its own (non-advertised) static defs plus the dynamic_dep exports.
    let mut all_defined: std::collections::HashSet<String> = definite_defined.clone();
    all_defined.extend(dyn_defined.iter().cloned());
    // Demand = strong references the consumer can't yet satisfy.
    let mut needed: std::collections::HashSet<String> = definite_undef
        .iter()
        .filter(|s| !all_defined.contains(*s))
        .cloned()
        .collect();

    let mut kept: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // Fold an archive into the kept set: its defs satisfy demand, its own
    // undefined refs become new demand (unless already satisfiable).
    let absorb = |p: &PathBuf,
                      kept: &mut std::collections::HashSet<PathBuf>,
                      needed: &mut std::collections::HashSet<String>,
                      all_defined: &mut std::collections::HashSet<String>| {
        kept.insert(p.clone());
        if let Some(defs) = adv_defs.get(p) {
            for s in defs {
                needed.remove(s);
                all_defined.insert(s.clone());
            }
        }
        if let Some(undefs) = adv_undefs.get(p) {
            for s in undefs {
                if !all_defined.contains(s) {
                    needed.insert(s.clone());
                }
            }
        }
    };

    // Seed: the dynamic_dep dropped the never-pulled archives entirely, so the
    // consumer must link them — they're live from the start.
    for p in advertised {
        if never_pulled.contains(p) {
            absorb(p, &mut kept, &mut needed, &mut all_defined);
        }
    }
    // Monotone fixpoint over the partially/fully-pulled remainder.
    loop {
        let mut changed = false;
        for p in advertised {
            if kept.contains(p) {
                continue;
            }
            let Some(defs) = adv_defs.get(p) else {
                continue;
            };
            if defs.iter().any(|s| needed.contains(s)) {
                absorb(p, &mut kept, &mut needed, &mut all_defined);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    kept
}

/// Version tag for the symbol-aware-prune fixpoint algorithm. Bump whenever the
/// nm-evidence rules or [`advertised_archives_to_keep`] semantics change so a
/// stale on-disk link-plan cache (computed by an older rush) is never reused.
const LINK_PLAN_CACHE_VERSION: u32 = 1;

/// Cached RESULT of the cc_shared_library symbol-aware-prune fixpoint (Phase 2
/// of [`CcSharedLibraryInstance::build`]).
///
/// The fixpoint is correctness-preserving but EXPENSIVE: for
/// `//tensorflow:tensorflow_cc` it runs one `nm` per advertised archive
/// (~195, the cc.so case) or per the full 2877-archive closure (>1 hr serial),
/// then a monotone fixpoint, on EVERY build — turning even a no-op rebuild into
/// a >20 min wall. But its inputs are entirely determined by the file
/// content/metadata of the dynamic_dep `.so`s, the advertised archives, and the
/// definite (non-advertised) static inputs, plus the role partition (which
/// archives are advertised vs definite) and the `nm` tool identity. So if those
/// are unchanged across builds, the result (`prune_set` + `kept_advertised`) is
/// unchanged and can be reused verbatim, SKIPPING the nm scan + fixpoint
/// entirely — turning the no-op into a near-zero no-op.
///
/// Correctness is sacred: the cache key (see [`compute_link_plan_cache_key`])
/// covers every fixpoint input via the same freshness universe as the action
/// layer: regular files at or under the digest cap use a content digest, while
/// larger / non-regular paths fall back to `stat_meta`. Any archive/`.so`/tool
/// change, or any change to the advertised / definite path sets, mismatches
/// the key → the fixpoint re-runs. A content-identical rewrite of a digestable
/// archive now stays a hit; oversized files still take the conservative
/// metadata path.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedLinkPlan {
    key: String,
    prune: Vec<PathBuf>,
    kept_advertised: Vec<PathBuf>,
}

const LINK_PLAN_CONTENT_DIGEST_MAX_LEN: u64 = 8 * 1024 * 1024;

/// Identity of the resolved `nm` tool for the cache key: its absolute path
/// plus either a content digest or fallback metadata. A persistent cache must
/// invalidate if `RUSH_NM`/`PATH` later resolves `nm` to a different binary
/// (Codex review: hashing the bare string `"nm"` is insufficient when the
/// tool itself can change underneath the cache).
fn nm_tool_identity(nm: &str) -> String {
    // Resolve to an absolute on-disk path when possible (PATH lookup), else use
    // the spelled name. Reuse the same content-vs-metadata identity we use for
    // the cached link-plan inputs.
    let resolved: PathBuf = which_on_path(nm).unwrap_or_else(|| PathBuf::from(nm));
    path_identity_string(&resolved)
}

/// Minimal `which`: resolve `name` against `PATH` when it has no slash, else use
/// it literally. Returns the first existing match.
fn which_on_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return if p.exists() { Some(p) } else { None };
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|c| c.exists())
}

fn path_identity_string(path: &Path) -> String {
    let m = stat_meta(path);
    if let Some(digest) =
        content_digest_of_regular_file(path, LINK_PLAN_CONTENT_DIGEST_MAX_LEN)
    {
        return format!("{}|digest:{}", path.display(), digest_hex(&digest));
    }
    format!(
        "{}|meta:{}:{}:{}:{}:{}:{}:{}",
        path.display(),
        m.present as u8,
        m.len,
        m.mode,
        m.mtime_ns,
        m.ctime_ns,
        m.dev,
        m.ino
    )
}

/// Append a path's identity (content digest when available, otherwise
/// `FileMeta`) to the cache-key hasher.
fn hash_path_meta(hasher: &mut blake3::Hasher, p: &Path) {
    hasher.update(p.as_os_str().as_encoded_bytes());
    hasher.update(&[0]); // field separator
    if let Some(digest) =
        content_digest_of_regular_file(p, LINK_PLAN_CONTENT_DIGEST_MAX_LEN)
    {
        hasher.update(b"digest");
        hasher.update(&digest);
    } else {
        let m = stat_meta(p);
        hasher.update(b"meta");
        hasher.update(&[m.present as u8]);
        hasher.update(&m.len.to_le_bytes());
        hasher.update(&m.mode.to_le_bytes());
        hasher.update(&m.mtime_ns.to_le_bytes());
        hasher.update(&m.ctime_ns.to_le_bytes());
        hasher.update(&m.dev.to_le_bytes());
        hasher.update(&m.ino.to_le_bytes());
    }
    hasher.update(&[0xff]); // record terminator
}

fn digest_hex(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes.iter() {
        out.push(HEX[(*b >> 4) as usize] as char);
        out.push(HEX[(*b & 0x0f) as usize] as char);
    }
    out
}

/// Compute the cache key for the symbol-aware-prune fixpoint result. Covers
/// every fixpoint input, with the role partition (advertised vs definite)
/// preserved as SEPARATE tagged key sections — an archive moving between the two
/// with identical file metadata can change the correct result, so they must not
/// be merged into one union (Codex review).
///
/// `dynamic_dep_sos` is keyed as a set (sorted/deduped — symbols are unioned).
/// `advertised` is keyed in its canonical (already sorted/deduped) order.
/// `definite` is sorted/deduped before keying (batched-nm results are unioned).
fn compute_link_plan_cache_key(
    nm: &str,
    dynamic_dep_sos: &[PathBuf],
    advertised: &[PathBuf],
    definite: &[PathBuf],
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rush-link-plan-v");
    hasher.update(&LINK_PLAN_CACHE_VERSION.to_le_bytes());
    hasher.update(b"\x00nm\x00");
    hasher.update(nm_tool_identity(nm).as_bytes());

    let mut sos: Vec<&PathBuf> = dynamic_dep_sos.iter().collect();
    sos.sort();
    sos.dedup();
    hasher.update(b"\x00dynamic_dep_sos\x00");
    for p in sos {
        hash_path_meta(&mut hasher, p);
    }

    // Canonicalize advertised order INSIDE the key (sort+dedup) rather than
    // trusting the caller's ordering. The advertised set is the role-partitioned
    // subset (candidate_libs ∪ candidate_alwayslink) ∩ dynamic_exported; the
    // caller sorts it, but its assembly walks `dependency_providers` whose
    // iteration order is not stable across builds (e.g. a fresh --daemon=off
    // re-analysis), so two analyses of an UNCHANGED file set can hand us the
    // same 558 archives in a different sequence. Keying that sequence verbatim
    // made the cache MISS on an unchanged closure (observed: identical `definite`
    // digest, differing `advertised` digest, same count). Sorting here makes the
    // key depend only on the SET + each archive's FileMeta — order-independent —
    // so an unchanged closure HITs regardless of analysis iteration order. The
    // role partition is still honored: advertised vs definite remain separate
    // tagged sections (an archive moving roles still changes the key).
    let mut adv: Vec<&PathBuf> = advertised.iter().collect();
    adv.sort();
    adv.dedup();
    hasher.update(b"\x00advertised\x00");
    for p in adv.iter() {
        hash_path_meta(&mut hasher, p);
    }

    let mut defs: Vec<&PathBuf> = definite.iter().collect();
    defs.sort();
    defs.dedup();
    hasher.update(b"\x00definite\x00");
    for p in defs.iter() {
        hash_path_meta(&mut hasher, p);
    }

    let key = hasher.finalize().to_hex().to_string();

    // Diagnostic (env-gated): print per-section sub-digests + counts so a
    // cache MISS on a closure that "should" be unchanged can be attributed to
    // the exact section that differs (nm tool / dynamic_dep .so set / advertised
    // partition / definite set). Off by default; zero cost on the hot path.
    if std::env::var("RUSH_DEBUG_LINKPLAN").as_deref() == Ok("1") {
        let sub = |label: &str, paths: &[&PathBuf]| {
            let mut h = blake3::Hasher::new();
            for p in paths {
                hash_path_meta(&mut h, p);
            }
            eprintln!(
                "[linkplan-key] {label}: {} entries, digest {}",
                paths.len(),
                &h.finalize().to_hex().to_string()[..16]
            );
        };
        let mut sos2: Vec<&PathBuf> = dynamic_dep_sos.iter().collect();
        sos2.sort();
        sos2.dedup();
        // Content-only digest (paths, no FileMeta) to separate "the SET of
        // paths differs" from "a path's FileMeta differs".
        let content = |label: &str, paths: &[&PathBuf]| {
            let mut h = blake3::Hasher::new();
            for p in paths {
                h.update(p.as_os_str().as_encoded_bytes());
                h.update(b"\n");
            }
            eprintln!(
                "[linkplan-key] {label} CONTENT digest {}",
                &h.finalize().to_hex().to_string()[..16]
            );
        };
        eprintln!("[linkplan-key] nm: {}", nm_tool_identity(nm));
        sub("dynamic_dep_sos", &sos2);
        sub("advertised", &adv); // sorted+deduped, matches the keyed order
        content("advertised", &adv);
        sub("definite", &defs);
        eprintln!("[linkplan-key] FULL KEY {key}");
        // Dump the sorted advertised paths so two runs can be diffed exactly.
        if let Ok(dir) = std::env::var("RUSH_DEBUG_LINKPLAN_DUMP") {
            let body: String =
                adv.iter().map(|p| format!("{}\n", p.display())).collect();
            let _ = std::fs::write(format!("{dir}/advertised.txt"), body);
        }
    }

    key
}

/// Per-archive strong defs + strong undefs for the advertised set, computed in
/// PARALLEL (rayon). #39 link-prep speed: this scan ran one `nm` invocation per
/// advertised archive SERIALLY — >1 hr for the full //tensorflow:tensorflow_cc
/// closure. Each archive's nm read is independent, so it parallelizes cleanly
/// across cores (mirrors the FreshnessLedger's parallel stat). Correctness-
/// neutral: identical per-archive results, just concurrent. Returns
/// `(adv_defs, adv_undefs, never_pulled)` with the same semantics as the old
/// serial loop.
#[allow(clippy::type_complexity)]
fn scan_advertised_archives_parallel(
    nm: &str,
    advertised: &[PathBuf],
    dyn_defined: &std::collections::HashSet<String>,
) -> (
    HashMap<PathBuf, std::collections::HashSet<String>>,
    HashMap<PathBuf, std::collections::HashSet<String>>,
    std::collections::HashSet<PathBuf>,
) {
    use rayon::prelude::*;
    let scanned: Vec<(
        PathBuf,
        std::collections::HashSet<String>,
        std::collections::HashSet<String>,
        bool,
    )> = advertised
        .par_iter()
        .map(|p| {
            let defs = nm_defined_symbols(nm, p, &["-g"], true);
            let never = !dynamic_dep_provides_archive(dyn_defined, &defs);
            let undefs = nm_undefined_symbols_batch(nm, std::slice::from_ref(p));
            (p.clone(), defs, undefs, never)
        })
        .collect();

    let mut adv_defs: HashMap<PathBuf, std::collections::HashSet<String>> = HashMap::new();
    let mut adv_undefs: HashMap<PathBuf, std::collections::HashSet<String>> = HashMap::new();
    let mut never_pulled: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for (p, defs, undefs, never) in scanned {
        if never {
            never_pulled.insert(p.clone());
        }
        adv_defs.insert(p.clone(), defs);
        adv_undefs.insert(p, undefs);
    }
    (adv_defs, adv_undefs, never_pulled)
}

/// Pure decision behind the #39 symbol-aware dynamic-dep prune: should we
/// prune (subtract) an advertised archive because the dynamic_dep `.so`
/// genuinely provides it?
///
/// GNU `ld` extracts archive *members* (object files) individually — it pulls
/// the `.o`s that satisfy a reference and drops the rest. So a dynamic_dep
/// either pulled an archive's members (and thus exports its symbols) or
/// dropped the whole archive (exports none of them). The clean signal is
/// therefore presence/absence, NOT full coverage: real shared libraries
/// legitimately leave a few internal/hidden symbols unexported per archive, so
/// requiring *every* symbol to be exported would refuse to prune almost
/// anything and double-link the dep's whole static closure into the consumer.
///
/// Returns `true` (prune) iff the `.so` exports at least one of the archive's
/// global symbols — evidence the archive was pulled into that `.so` and is
/// genuinely owned there. Returns `false` (keep) iff the `.so` exports NONE of
/// them — the archive was dropped entirely (e.g. `xla_data_proto_cc_impl`: on
/// framework's link line but no member referenced, so framework defines none of
/// `xla::ShapeProto`), so this consumer must link it or the symbols are
/// undefined at load. An archive with no global symbols is vacuously prunable
/// (nothing to lose).
///
/// NOTE: `archive_syms` must be the STRONG-only set (callers pass `strong_only
/// = true` to [`nm_defined_symbols`] for the `-g` evidence read). Weak/unique
/// template symbols are not extraction evidence and would otherwise make this
/// return `true` for archives the `.so` never pulled — the #39 weak-symbol
/// false positive that dropped `stats_utils`/`captured_function`.
fn dynamic_dep_provides_archive(
    dyn_defined: &std::collections::HashSet<String>,
    archive_syms: &std::collections::HashSet<String>,
) -> bool {
    if archive_syms.is_empty() {
        return true;
    }
    archive_syms.iter().any(|s| dyn_defined.contains(s))
}

/// `cc_shared_library` rule.
pub struct CcSharedLibraryRule;

#[async_trait]
impl BuildRule for CcSharedLibraryRule {
    fn name(&self) -> &str {
        "cc_shared_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        // Bazel defaults every list attr to [] when unset — mirror that here
        // via unwrap_or_default rather than propagating extract errors.
        let roots = extract_string_list(attrs, "roots").unwrap_or_default();
        let deps = extract_string_list(attrs, "deps").unwrap_or_default();
        let dynamic_deps = extract_string_list(attrs, "dynamic_deps").unwrap_or_default();
        let additional_linker_inputs =
            extract_string_list(attrs, "additional_linker_inputs").unwrap_or_default();
        let user_link_flags = extract_string_list(attrs, "user_link_flags").unwrap_or_default();
        let exports_filter = extract_string_list(attrs, "exports_filter").unwrap_or_default();
        // `shared_lib_name` lets TF wrappers override the produced `.so`
        // filename (e.g. `libtensorflow_cc.so.2.15.0`). Bazel's native
        // rule also honors this attribute.
        let shared_lib_name = extract_string(attrs, "shared_lib_name").ok();

        Ok(Box::new(CcSharedLibraryInstance {
            name,
            roots,
            deps,
            dynamic_deps,
            additional_linker_inputs,
            user_link_flags,
            exports_filter,
            shared_lib_name,
            build_result: Mutex::new(None),
        }))
    }

    fn documentation(&self) -> &str {
        "Shared library rule — links a subset of the cc_library graph into \
         a .so, with link-once enforcement and dynamic_deps support."
    }
}

/// Snapshot of what a CcSharedLibrary build produced, used to bridge
/// build() → providers() (which is called by the orchestrator after
/// build() on the same instance).
#[derive(Debug, Clone)]
struct CcSharedLibBuildResult {
    dynamic_library: PathBuf,
    exports: Vec<PathBuf>,
    link_flags: Vec<String>,
}

/// Instance of a `cc_shared_library` rule.
pub struct CcSharedLibraryInstance {
    name: String,
    roots: Vec<String>,
    deps: Vec<String>,
    dynamic_deps: Vec<String>,
    additional_linker_inputs: Vec<String>,
    user_link_flags: Vec<String>,
    #[allow(dead_code)]
    exports_filter: Vec<String>,
    shared_lib_name: Option<String>,
    build_result: Mutex<Option<CcSharedLibBuildResult>>,
}

#[async_trait]
impl RuleInstance for CcSharedLibraryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        // All four attr groups contribute edges into the build graph —
        // roots and deps provide the static-link inputs, dynamic_deps
        // provide the `CcSharedLibraryInfo` we subtract exports from,
        // and additional_linker_inputs resolve to file artifacts the
        // linker needs at link time (version scripts, export lists).
        let mut all = Vec::with_capacity(
            self.roots.len()
                + self.deps.len()
                + self.dynamic_deps.len()
                + self.additional_linker_inputs.len(),
        );
        all.extend(self.roots.iter().cloned());
        all.extend(self.deps.iter().cloned());
        all.extend(self.dynamic_deps.iter().cloned());
        all.extend(self.additional_linker_inputs.iter().cloned());
        all
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        // === Phase 1: Collect what our dynamic_deps already export ===
        //
        // Each `dynamic_deps` entry is another `cc_shared_library` whose
        // `CcSharedLibraryInfo` tells us:
        //   * the .so we should link against at runtime,
        //   * the set of `.a` paths it owns statically (prune from ours),
        //   * link flags to forward to our consumers.
        let mut dynamic_exported: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();
        let mut dynamic_dep_sos: Vec<PathBuf> = Vec::new();
        let mut dynamic_dep_link_flags: Vec<String> = Vec::new();
        for dd in &self.dynamic_deps {
            let Some(providers) = context.get_dependency_providers(dd) else {
                // Missing provider is non-fatal: the dep may have been
                // stubbed out. The worst case is we end up double-linking
                // its libs statically, which surfaces as a clean
                // link-once violation below rather than a silent ODR bug.
                continue;
            };
            let Some(info) = providers.get_cc_shared() else {
                continue;
            };
            dynamic_dep_sos.push(info.dynamic_library.clone());
            for e in &info.exports {
                dynamic_exported.insert(e.clone());
            }
            for f in &info.link_flags {
                dynamic_dep_link_flags.push(f.clone());
            }
        }

        // === Phase 2: Gather static-link candidates from roots + deps ===
        //
        // Rush's orchestrator has already transitively merged each
        // direct-dep's CcInfo.libraries before calling us, so pulling
        // from every target in `roots ∪ deps` gives the full transitive
        // static-lib set without re-walking the graph ourselves.
        let mut candidate_libs: Vec<PathBuf> = Vec::new();
        let mut candidate_alwayslink: Vec<PathBuf> = Vec::new();
        let mut transitive_link_flags: Vec<String> = Vec::new();
        // Phase 5.B.3.d: prefer typed linking context when available.
        // 5.B.3.e: typed-only path. CcInfo always has linking
        // populated.
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            for target in self.roots.iter().chain(self.deps.iter()) {
                let Some(providers) = context.get_dependency_providers(target) else {
                    continue;
                };
                let Some(cc_info) = providers.get_cc_info() else {
                    continue;
                };
                let linkc = &cc_info.linking;
                for lid in linkc.linker_inputs.iter() {
                    let li = reg.linker_input(lid);
                    let lib_path = reg.artifact_path(li.artifact).to_path_buf();
                    if li.alwayslink {
                        candidate_alwayslink.push(lib_path);
                    } else {
                        candidate_libs.push(lib_path);
                    }
                }
                use crate::providers::LinkFragment;
                for frag in linkc.link_fragments.iter() {
                    if let LinkFragment::Flag(sid) = frag {
                        transitive_link_flags.push(reg.strings().str(sid).to_string());
                    }
                    // Library/Group/SearchDir handled via linker_inputs
                    // above or not emitted by cc_library outputs.
                }
            }
            drop(reg);
        }

        // Subtract dynamic_deps' exports — those come from the other .so at
        // runtime. #39: this is SYMBOL-aware, not by archive path. A
        // dynamic_dep advertises (in CcSharedLibraryInfo.exports) the archive
        // PATHS it placed on its own static link line, but GNU `ld` only
        // extracts archive members that satisfy a reference — so an advertised
        // archive may contribute ZERO symbols to the produced `.so`. Pruning
        // such an archive by path drops its definitions from THIS .so too,
        // leaving the symbols undefined in BOTH (see `nm_defined_symbols`
        // docs / xla_data_proto_cc_impl). We prune an advertised archive only
        // when the dynamic_dep `.so` genuinely defines all of its symbols.
        let nm_tool = std::env::var("RUSH_NM").unwrap_or_else(|_| "nm".to_string());
        let needle_dbg = std::env::var("RUSH_DEBUG_NEEDLE").as_deref() == Ok("1");
        let mut prune_set: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();
        // Archives a dynamic_dep advertises (so it "owns" them under the
        // link-once invariant) but that we KEEP anyway because the dep's .so
        // doesn't actually export their symbols. These are intentionally
        // double-linked, so Phase 3 link-once enforcement must NOT claim them:
        // the owning .so's claim is spurious when it doesn't really provide the
        // symbols, and a duplicate definition across two .so's is benign
        // interposition whereas the alternative (undefined at load) is fatal.
        let mut kept_advertised: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();
        if !dynamic_exported.is_empty() {
            // Union of symbols actually defined by the dynamic_dep .so's
            // (one `nm -D` per dep — there are typically one or two).
            let mut dyn_defined: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for so in &dynamic_dep_sos {
                // Provider side: a weak DEFINED dynsym still satisfies the
                // loader, so count all defined exports (strong_only = false).
                dyn_defined.extend(nm_defined_symbols(&nm_tool, so, &["-D"], false));
            }
            // Archive paths a dynamic_dep advertises AND that we'd otherwise
            // link statically. Decide each once.
            let mut advertised: Vec<PathBuf> = candidate_libs
                .iter()
                .chain(candidate_alwayslink.iter())
                .filter(|p| dynamic_exported.contains(*p))
                .cloned()
                .collect();
            advertised.sort();
            advertised.dedup();
            // A real .so always defines symbols; an empty set means `nm`
            // failed or is missing. Fall back to the historical path-based
            // prune so behavior is unchanged when symbol data is unavailable.
            let nm_ok = !dyn_defined.is_empty();
            if !nm_ok && !advertised.is_empty() {
                eprintln!(
                    "⚠️  cc_shared_library {}: nm unavailable or dynamic_dep .so exported no symbols; \
                     falling back to path-based dynamic-dep prune (set RUSH_NM or install binutils \
                     for the symbol-aware prune)",
                    self.name
                );
            }
            if !nm_ok {
                // No symbol data: historical path-based prune (own all advertised).
                for p in &advertised {
                    prune_set.insert(p.clone());
                }
            } else {
                // #39 partial-export prune. The old all-or-nothing rule pruned an
                // advertised archive whenever the dynamic_dep `.so` exported ANY
                // one of its strong symbols — but `ld` extracts archive MEMBERS
                // individually, so a `.so` can pull some members of an archive and
                // not others (e.g. framework.so exports `libgraph.a`'s Graph/Node
                // but not its `CostModel::*`). Pruning the whole archive then left
                // the un-pulled members' symbols undefined. We instead keep an
                // advertised archive iff it DEFINES a strong symbol the consumer
                // actually REFERENCES that the dynamic_dep does NOT export.
                //
                // The consumer's own (non-advertised) static inputs: what it can
                // define itself, and what it references. Demand the dynamic_dep
                // covers is resolved at runtime, so it is subtracted inside the
                // decision. Plain archives over-report undefined (members that
                // won't be pulled), which only ever errs toward KEEPING an
                // archive — the safe direction (fewer undefined, never more).
                // Computed up front because it (with `advertised` + the
                // dynamic_dep `.so`s + `nm` identity) is the complete input set
                // of the fixpoint, hence the link-plan cache key below.
                let advertised_set: std::collections::HashSet<&PathBuf> =
                    advertised.iter().collect();
                let definite: Vec<PathBuf> = candidate_libs
                    .iter()
                    .chain(candidate_alwayslink.iter())
                    .filter(|p| !advertised_set.contains(*p))
                    .cloned()
                    .collect();

                // === Fix A: link-plan fixpoint cache ===
                //
                // The nm scan + fixpoint below is the dominant no-op cost
                // (>20 min for //tensorflow:tensorflow_cc, the serial >1 hr scan
                // for the full closure). Its result (`prune_set` +
                // `kept_advertised`) is a pure function of the file
                // content/metadata of the dynamic_dep `.so`s, the advertised
                // archives, and the definite static inputs, plus the role
                // partition and the `nm` tool identity. Cache it on disk keyed on
                // exactly those inputs (via stat_meta — the action layer's
                // freshness universe) so an unchanged closure reuses the plan and
                // SKIPS the scan entirely. Any change mismatches the key →
                // re-run. Honor RUSH_NO_LINK_PLAN_CACHE=1 to force a recompute.
                let cache_disabled =
                    std::env::var("RUSH_NO_LINK_PLAN_CACHE").as_deref() == Ok("1");
                let cache_key = compute_link_plan_cache_key(
                    &nm_tool,
                    &dynamic_dep_sos,
                    &advertised,
                    &definite,
                );
                let plan_cache_path = context
                    .output_dir
                    .join(&self.name)
                    .join(format!(
                        "{}.linkplan.json",
                        self.shared_lib_name
                            .clone()
                            .unwrap_or_else(|| format!("lib{}.so", self.name))
                    ));
                if !cache_disabled {
                    if let Ok(bytes) = std::fs::read(&plan_cache_path) {
                        if let Ok(plan) = serde_json::from_slice::<CachedLinkPlan>(&bytes) {
                            if plan.key == cache_key {
                                eprintln!(
                                    "⚡ cc_shared_library {}: link-plan cache HIT \
                                     ({} advertised archives unchanged) — skipping nm scan + \
                                     symbol-aware-prune fixpoint",
                                    self.name,
                                    advertised.len()
                                );
                                for p in plan.prune {
                                    prune_set.insert(p);
                                }
                                for p in plan.kept_advertised {
                                    kept_advertised.insert(p);
                                }
                                // Cache hit: prune_set + kept_advertised fully
                                // populated, nothing left to do for this dep.
                                candidate_libs.retain(|p| !prune_set.contains(p));
                                candidate_alwayslink.retain(|p| !prune_set.contains(p));
                                return self
                                    .finish_link(
                                        context,
                                        candidate_libs,
                                        candidate_alwayslink,
                                        transitive_link_flags,
                                        dynamic_dep_sos,
                                        dynamic_dep_link_flags,
                                        kept_advertised,
                                    )
                                    .await;
                            }
                        }
                    }
                }

                eprintln!(
                    "🔎 cc_shared_library {}: link-plan cache MISS — scanning {} advertised \
                     archives (parallel nm) + symbol-aware-prune fixpoint",
                    self.name,
                    advertised.len()
                );

                // Per-archive strong defs (evidence side: weak/unique template
                // symbols would falsely look "owned") and strong undefined refs
                // (so a kept archive's own demand can pull further archives).
                // Archives the `.so` exports NONE of — it dropped them entirely
                // (e.g. xla_data_proto_cc_impl): always keep (old safety net).
                //
                // Fix B: this per-archive scan is run in PARALLEL (rayon) —
                // previously one serial nm invocation per archive (>1 hr for the
                // full closure). Correctness-neutral (identical results).
                let (adv_defs, adv_undefs, never_pulled) =
                    scan_advertised_archives_parallel(&nm_tool, &advertised, &dyn_defined);

                let definite_defined = nm_defined_symbols_batch(&nm_tool, &definite);
                let definite_undef = nm_undefined_symbols_batch(&nm_tool, &definite);

                let keep = advertised_archives_to_keep(
                    &advertised,
                    &adv_defs,
                    &adv_undefs,
                    &never_pulled,
                    &definite_defined,
                    &definite_undef,
                    &dyn_defined,
                );
                for p in &advertised {
                    if keep.contains(p) {
                        if needle_dbg {
                            let why = if never_pulled.contains(p) {
                                "dynamic_dep .so exports NONE of its strong symbols (never pulled)"
                            } else {
                                "defines a consumer-referenced strong symbol the dynamic_dep .so \
                                 does NOT export (partial-export member)"
                            };
                            eprintln!("[NEEDLE/ccsl] KEEP {} ({})", p.display(), why);
                        }
                        kept_advertised.insert(p.clone());
                    } else {
                        prune_set.insert(p.clone());
                    }
                }

                // Persist the fixpoint result (Fix A). Next build with an
                // unchanged closure hits the cache above and skips the whole
                // scan + fixpoint. Best-effort: a write failure just means the
                // next build recomputes (correct, slower).
                if !cache_disabled {
                    let plan = CachedLinkPlan {
                        key: cache_key,
                        prune: prune_set.iter().cloned().collect(),
                        kept_advertised: kept_advertised.iter().cloned().collect(),
                    };
                    if let Some(dir) = plan_cache_path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    match serde_json::to_vec(&plan) {
                        Ok(bytes) => {
                            if let Err(e) = std::fs::write(&plan_cache_path, &bytes) {
                                eprintln!(
                                    "⚠️  cc_shared_library {}: could not write link-plan cache {}: {} \
                                     (next build will recompute)",
                                    self.name,
                                    plan_cache_path.display(),
                                    e
                                );
                            }
                        }
                        Err(e) => eprintln!(
                            "⚠️  cc_shared_library {}: link-plan serialize failed: {}",
                            self.name, e
                        ),
                    }
                }
            }
        }
        candidate_libs.retain(|p| !prune_set.contains(p));
        candidate_alwayslink.retain(|p| !prune_set.contains(p));

        self.finish_link(
            context,
            candidate_libs,
            candidate_alwayslink,
            transitive_link_flags,
            dynamic_dep_sos,
            dynamic_dep_link_flags,
            kept_advertised,
        )
        .await
    }

    fn providers(&self) -> ProviderSet {
        let mut set = ProviderSet::empty();
        if let Ok(slot) = self.build_result.lock() {
            if let Some(ref r) = *slot {
                set = set.with_cc_shared(CcSharedLibraryInfo::new(
                    r.dynamic_library.clone(),
                    r.exports.clone(),
                    r.link_flags.clone(),
                ));
            }
        }
        set
    }

    /// Phase 5.C0: declare the CcLinkShared action that produces
    /// the .so. The transitive static-library inputs come from
    /// `roots` and `deps` via dep CcInfo's linker_inputs. Recording
    /// those ArtifactIds gives the target frontier exact producer
    /// barriers even while the link itself stays on legacy
    /// execution.
    fn analyze(
        &self,
        ctx: &mut crate::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        use rush_action::ids::ArtifactKind;
        use std::path::Path;

        let mut action_inputs = Vec::new();
        for cc_info in crate::providers::extract_cc_infos(dep_providers) {
            for linker_input_id in cc_info.linking.linker_inputs.iter() {
                let linker_input = ctx.registry().linker_input(linker_input_id);
                action_inputs.push(linker_input.artifact);
                if let Some(extras) = &linker_input.extras {
                    action_inputs.extend(extras.additional_inputs.iter().copied());
                }
            }
        }
        let mut seen_inputs = std::collections::HashSet::new();
        action_inputs.retain(|artifact| seen_inputs.insert(*artifact));

        let so_name = self
            .shared_lib_name
            .clone()
            .unwrap_or_else(|| format!("lib{}.so", self.name));
        let so_aid = ctx.register_generated_artifact(
            Path::new(&so_name),
            ArtifactKind::SharedLibrary,
            None,
        );
        // The inputs are complete enough to model cross-target archive
        // ordering, but the executable payload still lives in legacy
        // build(). Keep the declaration Incomplete so the action scheduler
        // cannot dispatch a command that has not been migrated.
        let _ = ctx.declare_incomplete_action(
            "CcLinkShared",
            action_inputs,
            vec![so_aid],
            "legacy CcLinkShared execution payload not migrated",
        );
        ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "cc_shared_library"
    }
}

impl CcSharedLibraryInstance {
    /// Phases 3–6 of [`CcSharedLibraryInstance::build`]: link-once enforcement,
    /// linker-arg assembly, the link spawn (through the action cache), and
    /// stashing the build result for `providers()`. Split out so the Fix A
    /// link-plan cache HIT path can reach the link directly without re-running
    /// the symbol-aware-prune analysis. `prune_set` has already been applied to
    /// the candidate buckets by the caller.
    #[allow(clippy::too_many_arguments)]
    async fn finish_link(
        &self,
        context: &BuildContext,
        candidate_libs: Vec<PathBuf>,
        candidate_alwayslink: Vec<PathBuf>,
        transitive_link_flags: Vec<String>,
        dynamic_dep_sos: Vec<PathBuf>,
        dynamic_dep_link_flags: Vec<String>,
        kept_advertised: std::collections::HashSet<PathBuf>,
    ) -> Result<Vec<Artifact>> {
        // Dedup while preserving first-seen order (linker-order-sensitive
        // for --start-group/--end-group patterns, though we don't emit
        // those — keeping stable order matches Bazel's observable
        // behavior and helps action-cache determinism).
        fn dedup(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
            let mut seen = std::collections::HashSet::new();
            v.retain(|p| seen.insert(p.clone()));
            v
        }
        let static_libs = dedup(candidate_libs);
        let static_alwayslink = dedup(candidate_alwayslink);

        // === Phase 3: Link-once enforcement ===
        //
        // Before we spend time on a linker invocation, claim every
        // library we're about to bake in. If another cc_shared_library
        // already claimed the same path, error out with a Bazel-style
        // violation message that names both owners.
        let full_label = match context.package_path.as_ref() {
            Some(p) => format!("//{}:{}", p.display(), self.name),
            None => format!("//:{}", self.name),
        };
        for lib in static_libs.iter().chain(static_alwayslink.iter()) {
            // Skip archives we intentionally kept despite a dynamic_dep
            // advertising them (#39 symbol-aware prune): the dep's .so doesn't
            // actually export their symbols, so this .so must link them too —
            // a deliberate double-link, not a link-once violation.
            if kept_advertised.contains(lib) {
                continue;
            }
            claim_for_shared_library(lib, &full_label)?;
        }

        // === Phase 4: Resolve additional_linker_inputs to on-disk paths ===
        //
        // These are typically file labels (version scripts, exported
        // symbols lists). Each label resolves through rush's source_file
        // target to a Source artifact whose path we hand to the linker.
        let resolve_label_to_path = |label: &str| -> PathBuf {
            if let Some(arts) = context.direct_artifacts(label) {
                if let Some(a) = arts.first() {
                    return a.path.clone();
                }
            }
            // Fallback for bare filenames with no registered artifact:
            // resolve relative to the current package.
            context.source_path(label)
        };
        let mut linker_input_files: Vec<PathBuf> = Vec::new();
        for ali in &self.additional_linker_inputs {
            if let Some(arts) = context.direct_artifacts(ali) {
                for a in arts {
                    linker_input_files.push(a.path.clone());
                }
            } else {
                linker_input_files.push(resolve_label_to_path(ali));
            }
        }
        // Expand `$(location ...)` / `$(execpath ...)` in user_link_flags
        // up front (TF's per-OS version-script linkopts arrive here as
        // unexpanded `$(location //tensorflow:...lds)`). Every downstream
        // use — the version-script gate, the referenced-file check, and the
        // final flag push — operates on the EXPANDED flags so paths reach
        // `ld` literally and basename matching works.
        let user_link_flags: Vec<String> = self
            .user_link_flags
            .iter()
            .map(|f| expand_link_flag_locations(f, &resolve_label_to_path))
            .collect();

        // === Phase 5: Build the linker invocation ===
        //
        // Output: `<output_dir>/<name>/<shared_lib_name>` where
        // shared_lib_name defaults to `lib<name>.so`.
        let so_filename = self
            .shared_lib_name
            .clone()
            .unwrap_or_else(|| format!("lib{}.so", self.name));
        let so_dir = context.output_dir.join(&self.name);
        std::fs::create_dir_all(&so_dir)?;
        let so_path = so_dir.join(&so_filename);

        let mut args: Vec<String> = Vec::new();
        args.push("-shared".to_string());
        args.push("-fPIC".to_string());
        // #39 link-speed: drive a parallel linker (mold/lld) for this mega-link
        // instead of single-threaded bfd ld (~110min → minutes). No-op when no
        // fast linker is installed (RUSH_LINKER=bfd or none found).
        let fast_linker_flag = fast_linker_driver_flag();
        if let Some(linker_flag) = fast_linker_flag.as_ref() {
            args.push(linker_flag.clone());
        }
        args.push(format!("-Wl,-soname,{}", so_filename));
        args.push("-o".to_string());
        args.push(so_path.display().to_string());

        // Object/static-archive inputs. alwayslink libs go inside
        // --whole-archive / --no-whole-archive so their static
        // initializers (e.g. op registrations in TF) aren't dropped
        // by the linker's normal static-lib inclusion rules.
        let mut action_inputs: Vec<PathBuf> = Vec::new();
        for lib in &static_alwayslink {
            args.push("-Wl,--whole-archive".to_string());
            args.push(lib.display().to_string());
            args.push("-Wl,--no-whole-archive".to_string());
            action_inputs.push(lib.clone());
        }

        // #39 partial-export prune (link-order half): emit the dynamic_dep
        // `.so`s BEFORE the plain static archive group. GNU ld records a .so's
        // exported symbols when it scans it, then extracts a later static
        // archive member only to satisfy a STILL-undefined reference. With
        // framework.so ahead of e.g. libgraph.a, ld resolves the symbols
        // framework already exports (Graph/Node) dynamically and pulls ONLY the
        // orphaned members (costmodel.o) from the partially-exported archive we
        // kept in the prune above — so that kept archive contributes no
        // duplicate definitions. Wrap in `--push-state,--no-as-needed` /
        // `--pop-state`: moving the .so ahead of its first reference would
        // otherwise let `--as-needed` drop its DT_NEEDED before the later static
        // refs are seen, re-introducing the undefined-at-load the prune fixed.
        let mut seen_rpath_dirs: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();
        if !dynamic_dep_sos.is_empty() {
            args.push("-Wl,--push-state,--no-as-needed".to_string());
            for so in &dynamic_dep_sos {
                args.push(so.display().to_string());
                action_inputs.push(so.clone());
                if let Some(dir) = so.parent() {
                    if seen_rpath_dirs.insert(dir.to_path_buf()) {
                        args.push(format!("-Wl,-rpath,{}", dir.display()));
                    }
                }
            }
            args.push("-Wl,--pop-state".to_string());
        }

        // #39(B): wrap the PLAIN (non-alwayslink) static archives in
        // `-Wl,--start-group ... -Wl,--end-group`. GNU ld is single-pass over
        // archives: a plain archive member is pulled only if an
        // already-included object references it at the moment ld scans that
        // archive. With thousands of TF archives in arbitrary topological
        // order, a member defining e.g. `tsl::PosixFileSystem::*` (in
        // `tsl/platform/default/env/libenv.a`) is referenced by an archive
        // that appears LATER on the line, so ld never extracts it → the
        // symbol is left undefined in the .so (#39 ldd -r). Bazel's
        // cc_shared_library avoids this by whole-archiving all top-level
        // roots; the lighter, equivalent fix here is a `--start-group` around
        // the plain bucket, which makes ld re-scan the group until no new
        // references resolve — order-independent, and it does NOT force-retain
        // unreferenced members (so the .so stays as lean as plain linking,
        // unlike blanket whole-archive). alwayslink archives stay in their
        // own `--whole-archive` block above (static initializers must survive
        // regardless of references). Inert when there are 0/1 plain archives.
        append_plain_static_libs_grouped(&mut args, &mut action_inputs, &static_libs);

        // (dynamic_dep `.so`s are emitted ABOVE, before the plain static group —
        // see the #39 partial-export link-order note.)

        // Version scripts and exported-symbols lists. Bazel's native
        // rule does NOT auto-wrap `additional_linker_inputs` in any flag —
        // they are merely files made available to the link, and the actual
        // `-Wl,--version-script,...` / `-Wl,-unexported_symbols_list,...`
        // flags come from `linkopts` (which TF's tf_cc_shared_library macro
        // folds into `user_link_flags`, with `select()` + `$(location)`
        // already resolved per-OS).
        //
        // We mirror that: every input is added to `action_inputs` so it is
        // on disk, but we DON'T synthesize a flag for a file the author
        // already references via an explicit `user_link_flag`. This is the
        // crux of the libtensorflow_framework.so fix — TF lists BOTH
        // tf_framework_version_script.lds (the Linux/default version script)
        // and tf_private_symbols.lds (the *macOS* `-unexported_symbols_list`,
        // glob `*` syntax) in additional_linker_inputs, but only the former
        // is referenced by the resolved Linux `--version-script` flag.
        // Blindly version-scripting every `.lds` fed the macOS symbol list
        // to GNU ld → "ignoring invalid character '*' in script".
        //
        // The legacy auto-wrap survives only as a back-compat fallback for
        // BUILD files that put a version script in additional_linker_inputs
        // WITHOUT supplying any `--version-script` user_link_flag — and only
        // on ELF/GNU-ld targets (never macOS ld64, which has no
        // `--version-script`).
        let author_has_version_script =
            user_link_flags.iter().any(|f| f.contains("--version-script"));
        let mut uses_version_script = author_has_version_script;
        // True when `fname` appears as a comma/equals-delimited operand of
        // some user_link_flag (basename match — bounded so `foo.lds` does
        // not collide with `old_foo.lds`).
        let referenced_by_user_flag = |fname: &std::ffi::OsStr| -> bool {
            user_link_flags.iter().any(|flag| {
                flag.split([',', '=']).any(|tok| {
                    std::path::Path::new(tok).file_name() == Some(fname)
                })
            })
        };
        // GNU `--version-script` is meaningless to macOS ld64; only emit the
        // fallback on ELF toolchains.
        let elf_target = !cfg!(target_os = "macos");
        for input in &linker_input_files {
            action_inputs.push(input.clone());
            let fname = input.file_name().unwrap_or_default();
            if referenced_by_user_flag(fname) {
                // The author's explicit flag already points at this file
                // (e.g. `-Wl,--version-script,<abs>/tf_framework_version_script.lds`).
                // Adding anything here would duplicate the flag or pass the
                // file positionally (ld would try to link it as an object).
                continue;
            }
            let ext = input.extension().and_then(|e| e.to_str()).unwrap_or("");
            match ext {
                // Version-script-looking extension, no author-supplied
                // `--version-script` anywhere: synthesize the wrapper as a
                // best-effort crutch on ELF targets.
                "lds" | "ld" | "map" | "ver" if elf_target && !author_has_version_script => {
                    args.push(format!("-Wl,--version-script={}", input.display()));
                    uses_version_script = true;
                }
                // Version-script-looking extension that the author did NOT
                // reference, while a `--version-script` flag already exists
                // (or non-ELF target): this is an OS-specific extra (e.g.
                // tf_private_symbols.lds on Linux). Keep it as an input only;
                // do NOT pass any flag or it corrupts the link.
                "lds" | "ld" | "map" | "ver" => {}
                _ => {
                    // Unknown extension — pass as a positional input and
                    // let the linker decide (it will treat .o as objects,
                    // .a as archives, etc.).
                    args.push(input.display().to_string());
                }
            }
        }
        // GNU ld silently tolerates exact symbol names in a version script
        // when the corresponding optional library is absent. lld rejects
        // those names unless --undefined-version is present. Bazel-generated
        // TensorFlow scripts intentionally contain such optional names (for
        // example libjpeg helpers), so preserve GNU/Bazel behavior without
        // weakening ordinary undefined-symbol checks.
        if fast_linker_flag.as_deref() == Some("-fuse-ld=lld") && uses_version_script {
            args.push("-Wl,--undefined-version".to_string());
        }

        // Transitive link flags from deps (e.g. `-lrt`, `-lpthread`
        // that some system cc_library stubs emit).
        for f in &transitive_link_flags {
            args.push(f.clone());
        }
        // Plus dynamic_deps' own forwarded link flags.
        for f in &dynamic_dep_link_flags {
            args.push(f.clone());
        }
        // Finally, user-supplied link flags (last so they can override).
        // These are the `$(location ...)`-expanded flags computed above.
        for f in &user_link_flags {
            args.push(f.clone());
        }

        // GPU/ROCm: the produced .so (framework.so / cc.so) carries direct
        // references to the ROCm runtime (the StreamExecutor ROCm backend +
        // EIGEN_USE_HIP kernels compiled under RUSH_ENABLE_ROCM). In TF the
        // runtime arrives as @local_config_rocm//rocm:rocm cc_library `srcs`
        // (libamdhip64.so via :hip, librocblas.so via :rocblas, etc. — each
        // .so is a SRC of a linkstatic=1 cc_library, so Bazel adds DT_NEEDED on
        // it) plus a `-Wl,-rpath,.../rocm/lib` from rocm_rpath. rush's graph
        // links the rocm *_plugin/_executor STATIC archives (which CALL into
        // the runtime) but does not pull the runtime .so srcs into DT_NEEDED,
        // so the framework .so links (refs sit in whole-archived members) but
        // won't dlopen (RTLD_NOW dies on e.g. hipStreamSynchronize). Inject the
        // ROCm runtime libs whose symbols are actually left undefined on the
        // .so: libamdhip64 (hip*/__hip*/hsa via it) and librocblas
        // (rocblas_get_version_string*). This mirrors the proven GPU cc_binary
        // injection (commit f6736b7, link_binary above). `--no-as-needed`
        // forces the DT_NEEDED entry even when the linker sees the references
        // only inside whole-archived members. rpath lets the loaded .so find
        // the runtime at /opt/rocm/lib. Gated on RUSH_ENABLE_ROCM so the CPU
        // path is untouched; deduped against any user/transitive flags.
        if std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1") {
            for f in [
                "-L/opt/rocm/lib".to_string(),
                "-Wl,--no-as-needed".to_string(),
                "-lamdhip64".to_string(),
                "-lrocblas".to_string(),
                "-Wl,--as-needed".to_string(),
                "-Wl,-rpath,/opt/rocm/lib".to_string(),
            ] {
                if !args.contains(&f) {
                    args.push(f);
                }
            }
        }

        // Toolchain-level ldflags (workspace-wide defaults).
        for flag in &context.toolchain.ldflags {
            args.push(flag.clone());
        }

        // Final archive-aware de-dup. The static buckets are PathBuf-deduped
        // within themselves, but the SAME archive can still reach the link
        // through more than one channel — e.g. libLLVMSupport.a arriving both
        // `--whole-archive`-wrapped (alwayslink bucket) and plain (normal
        // bucket or a transitive link fragment). A single namespace collapse
        // (first occurrence wins) prevents "multiple definition" errors while
        // preserving the fixed `-shared -o <out>` prefix (each unique).
        let args = dedup_archive_link_flags(args);

        // The action key is computed from the FULL argv so any change to a
        // link flag, object, or archive invalidates the cache — independent
        // of whether the spawned argv is later spilled to a response file.
        let command = Command {
            executable: context.toolchain.cxx.clone(),
            args: args.clone(),
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };
        let inputs: Vec<ActionArtifact> = action_inputs
            .iter()
            .map(|p| ActionArtifact::Source(p.clone()))
            .collect();
        let action_id = Action::compute_id(&inputs, &command, &HashMap::new())?;

        // ARG_MAX guard. A cc_shared_library that bakes in thousands of
        // objects/archives (e.g. //tensorflow:tensorflow_cc, deps=195)
        // overflows the kernel's per-string limit and the compiler driver
        // dies with `posix_spawn: Argument list too long` before collect2
        // even starts.
        //
        // A *driver*-level response file (`g++ @file`) does NOT fix this: the
        // driver expands @file in-process, then both (a) re-packs every switch
        // into the single COLLECT_GCC_OPTIONS environment string and (b)
        // execs collect2 with the full expanded argv — and COLLECT_GCC_OPTIONS
        // alone blows past MAX_ARG_STRLEN (128 KiB = 32*PAGE_SIZE, which is
        // NOT tunable; raising RLIMIT_STACK does not help). Confirmed
        // empirically + via Codex gpt-5.5.
        //
        // The fix Bazel-equivalently uses is a *linker* response file passed
        // through the driver as a single `-Wl,@<file>` token. The driver does
        // not expand it (the token starts with `-Wl,`, not `@`); collect2
        // expands it but, because an at-file was supplied, re-packs the
        // expanded linker argv into its OWN temp response file before exec'ing
        // `ld` (the terminal process). So no exec ever carries the giant argv,
        // and COLLECT_GCC_OPTIONS stays tiny because the bulk are no longer
        // driver switches.
        //
        // Partition above a conservative argv-byte threshold:
        //   * driver-only flags (`-pthread`, `-shared`, `-fPIC`, `-static*`,
        //     `-f*`, `-m*`, `-o <out>`, …) stay on the g++ command line,
        //     DEDUPED — 15k repeated `-pthread`s would themselves re-bloat
        //     COLLECT_GCC_OPTIONS past 128 KiB;
        //   * every linker token (objects, archives, `--whole-archive`
        //     groups, `-soname`, `-rpath`, `--version-script`, `-l`/`-L`)
        //     goes into `<so>.params` in *ld syntax* — `-Wl,` prefixes
        //     stripped and comma-split — preserving original order so
        //     `--whole-archive` grouping (TF op-registration static inits)
        //     is intact;
        //   * the spawned argv is `<driver flags> -Wl,@<so>.params`.
        // The action key is still the FULL argv (above), so cache correctness
        // is independent of this transport. Small links keep their literal argv.
        const LINK_PARAMS_FILE_THRESHOLD: usize = 128 * 1024;
        let argv_bytes: usize = args.iter().map(|a| a.len() + 1).sum();
        let command = if argv_bytes > LINK_PARAMS_FILE_THRESHOLD {
            // Partition driver-only flags (deduplicated) from linker tokens;
            // see `partition_link_argv`. ld-token order is preserved so the
            // `--whole-archive` grouping stays intact.
            let (driver_flags, ld_tokens) = partition_link_argv(&args);
            let params_path = so_dir.join(format!("{}.params", so_filename));
            let mut body = String::with_capacity(argv_bytes + ld_tokens.len());
            for t in &ld_tokens {
                body.push_str(&quote_response_file_arg(t));
                body.push('\n');
            }
            std::fs::write(&params_path, body).with_context(|| {
                format!("failed to write link params file {}", params_path.display())
            })?;
            let mut spawn_args = driver_flags;
            spawn_args.push(format!("-Wl,@{}", params_path.display()));
            Command {
                executable: context.toolchain.cxx.clone(),
                args: spawn_args,
                working_dir: None,
                execution_info: ExecutionInfo::default(),
            }
        } else {
            command
        };

        let outputs = vec![ActionArtifact::Source(so_path.clone())];
        let action = Action {
            id: action_id,
            command,
            env: HashMap::new(),
            inputs,
            outputs,
            mnemonic: "CppSharedLink".to_string(),
        };

        if std::env::var("RUSH_EMIT_SHARED_LINK_ONLY").as_deref() == Ok("1") {
            // #39(C) validation aid (env-gated, diagnostic ONLY — never the
            // default build path): the full //tensorflow:tensorflow_cc analysis
            // graph (~80GB resident in rush) plus the terminal cc.so `ld` link
            // (~12GB) together OOM a 91GB host. Writing the exact link command
            // here and RETURNING before spawning the linker lets rush finish and
            // exit, releasing its 80GB graph; the emitted command is then run
            // standalone (12GB, fits comfortably) to produce the .so. The
            // `.params` response file was already written above, so the emitted
            // command is fully self-contained.
            let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
            let mut cmd_line = quote(&action.command.executable.display().to_string());
            for a in &action.command.args {
                cmd_line.push(' ');
                cmd_line.push_str(&quote(a));
            }
            let cmd_path = so_dir.join(format!("{}.linkcmd", so_filename));
            std::fs::write(&cmd_path, format!("{}\n", cmd_line)).with_context(|| {
                format!("failed to write link command file {}", cmd_path.display())
            })?;
            eprintln!(
                "🧪 RUSH_EMIT_SHARED_LINK_ONLY=1: wrote standalone link command to {} and SKIPPED spawning the linker for {} (run it after rush exits to free the analysis graph first)",
                cmd_path.display(),
                so_filename
            );
        } else {
            let result = context
                .execute_action(action)
                .await
                .with_context(|| {
                    format!("cc_shared_library link spawn failed for {}", so_filename)
                })?;
            if result.exit_code != 0 {
                return Err(anyhow!(
                    "cc_shared_library link failed for {}: {}",
                    so_filename,
                    String::from_utf8_lossy(&result.stderr)
                ));
            }
        }

        // === Phase 6: Stash build output for providers() ===
        //
        // Downstream consumers (other cc_shared_library with us in
        // `dynamic_deps`, or cc_binary that links us) need:
        //   * our .so path (to pass as a linker input),
        //   * the set of `.a` paths we own (to subtract from their
        //     own static-link sets),
        //   * a link flag pointing at our .so (so `cc_binary` can just
        //     pick it up from CcSharedLibraryInfo.link_flags).
        let mut exports = Vec::with_capacity(static_libs.len() + static_alwayslink.len());
        exports.extend(static_libs.iter().cloned());
        exports.extend(static_alwayslink.iter().cloned());
        let mut consumer_link_flags = Vec::new();
        consumer_link_flags.push(so_path.display().to_string());
        if let Some(dir) = so_path.parent() {
            consumer_link_flags.push(format!("-Wl,-rpath,{}", dir.display()));
        }

        if let Ok(mut slot) = self.build_result.lock() {
            *slot = Some(CcSharedLibBuildResult {
                dynamic_library: so_path.clone(),
                exports,
                link_flags: consumer_link_flags,
            });
        }

        Ok(vec![Artifact {
            path: so_path,
            artifact_type: ArtifactType::SharedLibrary,
        }])
    }
}

// =====================================================================
// Phase 5.C.4 commit 13 — cc_proto_library structural tests
// =====================================================================
//
// Pro's directive: the zero-diff verifier proves command equivalence
// for one workload; structural tests prove the provider/action shape
// does not regress later when someone "simplifies" it with the
// optimism of a person who has never linked C++. Belt-and-suspenders-
// and-a-safety-net because generated code is where build systems go
// to lose their dignity.
//
// Each test invokes cc_proto_library::analyze() against a fake
// proto_library dep and asserts on:
//   - DefaultInfo.files (must be [.a only])
//   - CcInfo.compilation.headers (must include .pb.h)
//   - CcInfo.compilation.include_dirs (must include output_subdir)
//   - CcInfo.linking.linker_inputs (must include .a)
//   - declared actions (mnemonic + outputs + inputs shape)

#[cfg(test)]
mod symbol_aware_prune_tests {
    use super::{dynamic_dep_provides_archive, is_strong_evidence_nm_type};
    use std::collections::HashSet;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // #39: the dynamic_dep .so exports the archive's symbols → the archive was
    // pulled into that .so and is genuinely owned there → prune (provided=true).
    #[test]
    fn prune_when_so_exports_archive_symbols() {
        let dyn_defined = set(&["_ZA", "_ZB", "_ZC"]);
        let archive = set(&["_ZA", "_ZB"]);
        assert!(dynamic_dep_provides_archive(&dyn_defined, &archive));
    }

    // The xla_data_proto_cc_impl case: the dynamic_dep advertises the archive
    // by path but `ld` never pulled the member, so the .so exports NONE of its
    // symbols → keep (provided=false): pruning would leave them undefined in
    // both .so's.
    #[test]
    fn keep_when_so_exports_none_of_archive() {
        let dyn_defined = set(&["_ZA"]);
        let archive = set(&["_ZShapeProto_default_instance_", "_ZShapeProtoCopyFrom"]);
        assert!(!dynamic_dep_provides_archive(&dyn_defined, &archive));
    }

    // Partial export (≥1 symbol present) means the archive's members WERE
    // pulled — the dep owns it. Prune to avoid double-linking a whole core
    // archive (e.g. libgraph.a) just because a few internal symbols are hidden.
    #[test]
    fn prune_when_so_exports_archive_partially() {
        let dyn_defined = set(&["_ZA", "_ZB"]);
        let archive = set(&["_ZA", "_ZInternalHidden", "_ZAlsoHidden"]);
        assert!(dynamic_dep_provides_archive(&dyn_defined, &archive));
    }

    // An archive with no global symbols contributes nothing; pruning it is
    // vacuously safe (matches historical path-prune for empty members).
    #[test]
    fn prune_empty_archive() {
        let dyn_defined = set(&["_ZA"]);
        let archive: HashSet<String> = HashSet::new();
        assert!(dynamic_dep_provides_archive(&dyn_defined, &archive));
    }

    // No dynamic-dep symbol data at all (nm failed) → no overlap, so a
    // non-empty archive is reported not-provided; callers separately fall back
    // to the path-based prune when the whole dyn-defined set is empty.
    #[test]
    fn not_provided_for_nonempty_archive_when_so_symbols_unknown() {
        let dyn_defined: HashSet<String> = HashSet::new();
        let archive = set(&["_ZA"]);
        assert!(!dynamic_dep_provides_archive(&dyn_defined, &archive));
    }

    // #39 weak-symbol allowlist: strong defining types (text/data/bss/rodata/
    // common/ifunc) count as extraction evidence; weak (W/V), GNU-unique (u),
    // debug (n/N) and lowercase locals do not.
    #[test]
    fn strong_evidence_type_allowlist() {
        for ty in ["T", "D", "B", "R", "C", "G", "S", "I", "i"] {
            assert!(is_strong_evidence_nm_type(ty), "{ty} should be strong");
        }
        for ty in ["W", "V", "w", "v", "u", "n", "N", "t", "d", "b", "r", "U"] {
            assert!(!is_strong_evidence_nm_type(ty), "{ty} should NOT be strong");
        }
    }

    // The stats_utils / captured_function regression, at the decision layer:
    // framework.so exports ONLY the archive's ubiquitous weak template symbols
    // (Eigen/std/AlphaNum) and none of its strong payload. With the `-g` read
    // strong-filtered (weak symbols dropped before this call), the archive's
    // evidence set contains only strong syms framework lacks → keep, not prune.
    #[test]
    fn keep_when_so_exports_only_weak_template_symbols() {
        // framework's dynsym: the weak template instantiations it carries from
        // its own TUs, plus unrelated strong syms.
        let dyn_defined = set(&[
            "_ZN5Eigen3fixILi1EEE",
            "_ZN3tsl7strings8AlphaNumC1EPKc",
            "_ZN10tensorflow9unrelatedEv",
        ]);
        // The archive's STRONG-only evidence set (weak syms already filtered by
        // nm_defined_symbols(strong_only=true)): the real stats_utils payload.
        let archive = set(&[
            "_ZN10tensorflow4data11stats_utils14kExamplesCountE",
            "_ZN10tensorflow4data11stats_utils20BufferSizeScalarNameE",
        ]);
        assert!(!dynamic_dep_provides_archive(&dyn_defined, &archive));
    }
}

#[cfg(test)]
mod partial_export_prune_tests {
    use super::advertised_archives_to_keep;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    // The CostModel/full_type case. framework.so exports SOME of libgraph.a
    // (Graph/Node) but not CostModel::InitFromGraph, which the consumer
    // references. The old all-or-nothing rule pruned the whole archive; the
    // per-symbol fixpoint must KEEP it (it defines a consumer-referenced symbol
    // the dynamic_dep does not export).
    #[test]
    fn keep_partially_exported_archive_with_orphan_consumed_symbol() {
        let advertised = vec![pb("libgraph.a")];
        let mut adv_defs = HashMap::new();
        adv_defs.insert(pb("libgraph.a"), set(&["Graph", "Node", "CostModel_InitFromGraph"]));
        let mut adv_undefs = HashMap::new();
        adv_undefs.insert(pb("libgraph.a"), HashSet::new());
        let never_pulled = HashSet::new(); // framework exports Graph/Node → pulled
        let definite_defined = HashSet::new();
        let definite_undef = set(&["CostModel_InitFromGraph"]); // consumer needs it
        let dyn_defined = set(&["Graph", "Node"]); // framework exports these only

        let keep = advertised_archives_to_keep(
            &advertised,
            &adv_defs,
            &adv_undefs,
            &never_pulled,
            &definite_defined,
            &definite_undef,
            &dyn_defined,
        );
        assert!(keep.contains(&pb("libgraph.a")));
    }

    // Fully covered: the dynamic_dep exports every consumer-referenced symbol
    // of the archive → prune (the old, correct, lean behavior is preserved).
    #[test]
    fn prune_when_dynamic_dep_covers_all_consumed_symbols() {
        let advertised = vec![pb("libfoo.a")];
        let mut adv_defs = HashMap::new();
        adv_defs.insert(pb("libfoo.a"), set(&["A", "B"]));
        let mut adv_undefs = HashMap::new();
        adv_undefs.insert(pb("libfoo.a"), HashSet::new());
        let never_pulled = HashSet::new();
        let definite_defined = HashSet::new();
        let definite_undef = set(&["A", "B"]);
        let dyn_defined = set(&["A", "B"]);

        let keep = advertised_archives_to_keep(
            &advertised,
            &adv_defs,
            &adv_undefs,
            &never_pulled,
            &definite_defined,
            &definite_undef,
            &dyn_defined,
        );
        assert!(keep.is_empty());
    }

    // Safety net: an archive the dynamic_dep dropped entirely (exports none of)
    // is always kept regardless of demand — the xla_data_proto_cc_impl /
    // 1166-undefined regression guard.
    #[test]
    fn never_pulled_archive_is_always_kept() {
        let advertised = vec![pb("libxla.a")];
        let mut adv_defs = HashMap::new();
        adv_defs.insert(pb("libxla.a"), set(&["ShapeProto"]));
        let mut adv_undefs = HashMap::new();
        adv_undefs.insert(pb("libxla.a"), HashSet::new());
        let mut never_pulled = HashSet::new();
        never_pulled.insert(pb("libxla.a"));
        let definite_defined = HashSet::new();
        let definite_undef = HashSet::new(); // not even referenced by a definite input
        let dyn_defined = HashSet::new();

        let keep = advertised_archives_to_keep(
            &advertised,
            &adv_defs,
            &adv_undefs,
            &never_pulled,
            &definite_defined,
            &definite_undef,
            &dyn_defined,
        );
        assert!(keep.contains(&pb("libxla.a")));
    }

    // Transitive demand: keeping libA (defines a consumed symbol) creates a new
    // reference to libB's symbol, which must in turn pull libB. Both are
    // partially exported (not never_pulled), so only the fixpoint — not the
    // seed — discovers libB.
    #[test]
    fn fixpoint_pulls_transitively_needed_archive() {
        let advertised = vec![pb("libA.a"), pb("libB.a")];
        let mut adv_defs = HashMap::new();
        adv_defs.insert(pb("libA.a"), set(&["SymA", "ExpA"]));
        adv_defs.insert(pb("libB.a"), set(&["SymB", "ExpB"]));
        let mut adv_undefs = HashMap::new();
        adv_undefs.insert(pb("libA.a"), set(&["SymB"])); // libA references libB's symbol
        adv_undefs.insert(pb("libB.a"), HashSet::new());
        let never_pulled = HashSet::new();
        let definite_defined = HashSet::new();
        let definite_undef = set(&["SymA"]);
        // framework exports the "Exp*" members of each (→ provides=true, not
        // never_pulled) but neither SymA nor SymB.
        let dyn_defined = set(&["ExpA", "ExpB"]);

        let keep = advertised_archives_to_keep(
            &advertised,
            &adv_defs,
            &adv_undefs,
            &never_pulled,
            &definite_defined,
            &definite_undef,
            &dyn_defined,
        );
        assert!(keep.contains(&pb("libA.a")));
        assert!(keep.contains(&pb("libB.a")));
    }
}

#[cfg(test)]
mod link_plan_cache_key_tests {
    //! Fix A correctness: the link-plan cache key must change whenever any
    //! fixpoint input changes — including an archive moving between the
    //! `advertised` and `definite` roles (Codex: the role partition is part of
    //! the identity, since advertised archives are prune-eligible and definite
    //! ones are not), and any content/metadata change to a keyed file. A stable
    //! key for a stable closure is what makes the no-op a true no-op.
    use super::compute_link_plan_cache_key;
    use std::io::Write;
    use std::path::PathBuf;

    fn write_tmp(dir: &std::path::Path, name: &str, body: &[u8]) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body).unwrap();
        p
    }

    #[test]
    fn key_is_stable_for_unchanged_inputs() {
        let td = tempfile::tempdir().unwrap();
        let so = write_tmp(td.path(), "dep.so", b"so");
        let a = write_tmp(td.path(), "adv.a", b"adv");
        let d = write_tmp(td.path(), "def.a", b"def");
        let k1 = compute_link_plan_cache_key("nm", &[so.clone()], &[a.clone()], &[d.clone()]);
        let k2 = compute_link_plan_cache_key("nm", &[so], &[a], &[d]);
        assert_eq!(k1, k2, "identical closure must yield identical key");
    }

    #[test]
    fn key_changes_when_archive_content_changes() {
        let td = tempfile::tempdir().unwrap();
        let so = write_tmp(td.path(), "dep.so", b"so");
        let a = write_tmp(td.path(), "adv.a", b"archiveA");
        let k1 = compute_link_plan_cache_key("nm", &[so.clone()], &[a.clone()], &[]);
        // Rewrite with different bytes but the same length. The new digest-
        // aware key must still invalidate, even if the file metadata only
        // changed in mtime/ctime.
        write_tmp(td.path(), "adv.a", b"archiveB");
        let k2 = compute_link_plan_cache_key("nm", &[so], &[a], &[]);
        assert_ne!(k1, k2, "archive content change must invalidate the key");
    }

    #[test]
    fn key_stays_stable_for_same_content_rewrite() {
        let td = tempfile::tempdir().unwrap();
        let so = write_tmp(td.path(), "dep.so", b"so");
        let a = write_tmp(td.path(), "adv.a", b"archive");
        let k1 = compute_link_plan_cache_key("nm", &[so.clone()], &[a.clone()], &[]);
        // Rewriting the same bytes changes mtime/ctime, but the digest-aware
        // key should still hit because the content is unchanged.
        write_tmp(td.path(), "adv.a", b"archive");
        let k2 = compute_link_plan_cache_key("nm", &[so], &[a], &[]);
        assert_eq!(k1, k2, "same-content rewrite should not invalidate the key");
    }

    #[test]
    fn key_changes_when_archive_moves_advertised_to_definite() {
        // Same set of files, but one archive moves role. The correct prune
        // decision can differ (advertised is prune-eligible, definite is not),
        // so the key MUST differ even though every FileMeta is identical.
        let td = tempfile::tempdir().unwrap();
        let so = write_tmp(td.path(), "dep.so", b"so");
        let a = write_tmp(td.path(), "x.a", b"x");
        let as_advertised = compute_link_plan_cache_key("nm", &[so.clone()], &[a.clone()], &[]);
        let as_definite = compute_link_plan_cache_key("nm", &[so], &[], &[a]);
        assert_ne!(
            as_advertised, as_definite,
            "role partition (advertised vs definite) must be part of the key"
        );
    }

    #[test]
    fn key_is_independent_of_advertised_order() {
        // Regression: the advertised set is assembled by walking
        // dependency_providers, whose iteration order is not stable across
        // builds (a fresh --daemon=off re-analysis handed the same 558 archives
        // in a different sequence, making the cache MISS on an unchanged
        // closure). The key must depend only on the SET + each archive's
        // FileMeta, not the sequence.
        let td = tempfile::tempdir().unwrap();
        let a = write_tmp(td.path(), "a.a", b"a");
        let b = write_tmp(td.path(), "b.a", b"b");
        let c = write_tmp(td.path(), "c.a", b"c");
        let k1 = compute_link_plan_cache_key(
            "nm",
            &[],
            &[a.clone(), b.clone(), c.clone()],
            &[],
        );
        let k2 = compute_link_plan_cache_key("nm", &[], &[c, a, b], &[]);
        assert_eq!(k1, k2, "advertised order must not affect the key");
    }

    #[test]
    fn key_changes_when_nm_tool_changes() {
        let td = tempfile::tempdir().unwrap();
        let a = write_tmp(td.path(), "adv.a", b"adv");
        let k_nm = compute_link_plan_cache_key("nm", &[], &[a.clone()], &[]);
        let k_other = compute_link_plan_cache_key("llvm-nm", &[], &[a], &[]);
        assert_ne!(k_nm, k_other, "nm tool identity must be part of the key");
    }
}

#[cfg(test)]
mod cc_proto_library_tests {
    use super::*;
    use crate::analysis_context::{AnalysisContext, StubIncludeResolver};
    use crate::context::Toolchain;
    use crate::providers::{ProtoFilesInfo, ProviderSet};
    use rush_action::ids::{
        ArtifactId, ArtifactKey, ArtifactKind, ArtifactRegistry, ConfigId, TargetId,
    };
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    const TEST_WS: &str = "/ws";
    const TEST_OUT: &str = "/ws/rush-out";

    /// Pre-register a .proto source artifact and build the
    /// ProtoFilesInfo arc that cc_proto_library will consume —
    /// stand-in for `proto_library::analyze()` having run earlier.
    fn fake_proto_dep(
        reg: &mut ArtifactRegistry,
        proto_rel: &str,
    ) -> Arc<ProtoFilesInfo> {
        let p = reg.intern_path(Path::new(proto_rel));
        let aid = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: p,
            },
            ArtifactKind::Source,
            /*is_source*/ true,
            /*owner_target*/ TargetId(2),
            /*owner_action*/ None,
        );
        ProtoFilesInfo::from_direct_and_deps(vec![aid], std::iter::empty())
    }

    /// Run cc_proto_library::analyze() with one fake proto dep and
    /// return the resulting ProviderSet. Registry is exposed
    /// separately for declared-action assertions.
    fn analyze_one_proto(reg: &mut ArtifactRegistry) -> ProviderSet {
        let pfi = fake_proto_dep(reg, "greeting.proto");
        let dep = Arc::new(ProviderSet::empty().with_proto_files(pfi));
        let resolver = StubIncludeResolver::new(PathBuf::from(TEST_WS));
        let toolchain = Toolchain::default();
        let extra: Vec<PathBuf> = Vec::new();
        let ws = PathBuf::from(TEST_WS);
        let out = PathBuf::from(TEST_OUT);
        let mut ctx = AnalysisContext::new(
            TargetId(1),
            ConfigId::DEFAULT,
            "",
            None,
            &out,
            reg,
            &resolver,
            &toolchain,
            &ws,
            &extra,
        );
        let rule = CcProtoLibraryInstance {
            name: "greeting_cc_proto".to_string(),
            deps: vec![":greeting_proto".to_string()],
        };
        rule.analyze(&mut ctx, &[dep])
    }

    #[test]
    fn default_info_files_is_static_library_only() {
        let mut reg = ArtifactRegistry::new();
        let providers = analyze_one_proto(&mut reg);
        let di = providers
            .get_default_info()
            .expect("cc_proto_library populates DefaultInfo");
        assert_eq!(
            di.files.len(),
            1,
            "exactly one public output (the .a)"
        );
        let kind = reg.artifact(di.files[0]).kind;
        assert!(
            matches!(kind, ArtifactKind::StaticLibrary),
            "DefaultInfo public output must be StaticLibrary, got {:?}",
            kind
        );
    }

    #[test]
    fn default_info_does_not_expose_object_or_pb_cc() {
        // Commit 9 invariant on the public surface: no
        // intermediate .o leakage to consumers, and no .pb.cc
        // either (Generated kind). DefaultInfo is the typed-
        // public-output set; the dispatcher's
        // public_outputs_override uses it to synthesize the
        // target's NestedSet.
        let mut reg = ArtifactRegistry::new();
        let providers = analyze_one_proto(&mut reg);
        let di = providers.get_default_info().unwrap();
        for &aid in di.files.iter() {
            let kind = reg.artifact(aid).kind;
            assert!(
                !matches!(kind, ArtifactKind::Object),
                "DefaultInfo must not expose .o (Object)"
            );
            assert!(
                !matches!(kind, ArtifactKind::Generated),
                "DefaultInfo must not expose .pb.cc (Generated)"
            );
        }
    }

    #[test]
    fn cc_info_compilation_headers_includes_pb_h() {
        let mut reg = ArtifactRegistry::new();
        let providers = analyze_one_proto(&mut reg);
        let cc = providers
            .get_cc_info()
            .expect("cc_proto_library populates CcInfo");
        let headers: Vec<ArtifactId> = cc.compilation.headers.iter().collect();
        assert!(
            !headers.is_empty(),
            "CcInfo.compilation.headers must include the generated .pb.h"
        );
        let has_pb_h = headers.iter().any(|&aid| {
            let k = reg.artifact(aid).kind;
            let p = reg.artifact_path(aid).to_path_buf();
            matches!(k, ArtifactKind::Header)
                && p.to_string_lossy().contains(".pb.h")
        });
        assert!(
            has_pb_h,
            "CcInfo.compilation.headers must contain the generated .pb.h \
             so downstream cc_library can resolve `#include \"<stem>.pb.h\"`"
        );
    }

    #[test]
    fn cc_info_compilation_include_dirs_has_genfiles_root() {
        // Downstream consumers need the genfiles root on their compile -I list
        // because generated proto headers are laid out import-relative under
        // genfiles/<proto-import-path>.pb.h.
        let mut reg = ArtifactRegistry::new();
        let providers = analyze_one_proto(&mut reg);
        let cc = providers.get_cc_info().unwrap();
        let dirs: Vec<PathBuf> = cc
            .compilation
            .include_dirs
            .iter()
            .map(|pid| reg.paths().path(pid).to_path_buf())
            .collect();
        let expected = PathBuf::from(format!("{}/genfiles", TEST_OUT));
        assert!(
            dirs.contains(&expected),
            "CcInfo.compilation.include_dirs must contain {:?}, got {:?}",
            expected,
            dirs
        );
    }

    #[test]
    fn cc_info_linking_linker_inputs_has_static_library() {
        // Downstream cc_binary's CcLink walks dep
        // CcInfo.linking.linker_inputs to find proto .a archives.
        let mut reg = ArtifactRegistry::new();
        let providers = analyze_one_proto(&mut reg);
        let cc = providers.get_cc_info().unwrap();
        let lids: Vec<_> = cc.linking.linker_inputs.iter().collect();
        assert_eq!(
            lids.len(),
            1,
            "exactly one linker input (the .a) per cc_proto_library"
        );
        let data = reg.linker_input(lids[0]);
        assert!(
            !data.alwayslink,
            "cc_proto_library .a is alwayslink=false"
        );
        let kind = reg.artifact(data.artifact).kind;
        assert!(
            matches!(kind, ArtifactKind::StaticLibrary),
            "linker input must reference the .a, got {:?}",
            kind
        );
    }

    #[test]
    fn declares_protoc_compile_with_pb_h_and_pb_cc_outputs() {
        let mut reg = ArtifactRegistry::new();
        let _ = analyze_one_proto(&mut reg);
        let mut found = false;
        for (_id, action) in reg.declared_actions() {
            if reg.strings().str(action.mnemonic) != "ProtocCompile" {
                continue;
            }
            found = true;
            assert_eq!(
                action.outputs.len(),
                2,
                "ProtocCompile has 2 outputs (.pb.h + .pb.cc)"
            );
            let kinds_paths: Vec<(ArtifactKind, String)> = action
                .outputs
                .iter()
                .map(|&aid| {
                    (
                        reg.artifact(aid).kind,
                        reg.artifact_path(aid).display().to_string(),
                    )
                })
                .collect();
            assert!(
                kinds_paths.iter().any(|(k, p)| {
                    matches!(k, ArtifactKind::Header) && p.contains(".pb.h")
                }),
                "ProtocCompile must output .pb.h, got {:?}",
                kinds_paths
            );
            assert!(
                kinds_paths.iter().any(|(k, p)| {
                    matches!(k, ArtifactKind::Generated) && p.contains(".pb.cc")
                }),
                "ProtocCompile must output .pb.cc, got {:?}",
                kinds_paths
            );
        }
        assert!(found, "ProtocCompile action must be declared");
    }

    #[test]
    fn declares_cppcompile_with_pb_cc_and_pb_h_inputs() {
        // Pro's correctness directive: pb_cc #include's pb_h
        // via -I<output_subdir>; the action graph must show
        // pb_h as an input even though only pb_cc rides on argv.
        let mut reg = ArtifactRegistry::new();
        let _ = analyze_one_proto(&mut reg);
        let mut found = false;
        for (_id, action) in reg.declared_actions() {
            if reg.strings().str(action.mnemonic) != "CppCompile" {
                continue;
            }
            found = true;
            let inputs: Vec<(ArtifactKind, String)> = action
                .inputs
                .iter()
                .map(|&aid| {
                    (
                        reg.artifact(aid).kind,
                        reg.artifact_path(aid).display().to_string(),
                    )
                })
                .collect();
            assert!(
                inputs.iter().any(|(k, p)| {
                    matches!(k, ArtifactKind::Generated) && p.contains(".pb.cc")
                }),
                "CppCompile must declare .pb.cc as input, got {:?}",
                inputs
            );
            assert!(
                inputs.iter().any(|(k, p)| {
                    matches!(k, ArtifactKind::Header) && p.contains(".pb.h")
                }),
                "CppCompile must declare .pb.h as input — pb.cc \
                 #include's pb.h, declared inputs must reflect that, \
                 got {:?}",
                inputs
            );
        }
        assert!(found, "CppCompile action must be declared");
    }

    #[test]
    fn declares_ccarchive_with_object_inputs_and_static_library_output() {
        let mut reg = ArtifactRegistry::new();
        let _ = analyze_one_proto(&mut reg);
        let archives: Vec<_> = reg
            .declared_actions()
            .filter(|(_id, action)| {
                reg.strings().str(action.mnemonic) == "CcArchive"
            })
            .map(|(_id, action)| (action.inputs.clone(), action.outputs.clone()))
            .collect();
        assert_eq!(
            archives.len(),
            1,
            "exactly one CcArchive per cc_proto_library"
        );
        let (inputs, outputs) = &archives[0];
        // All inputs are Object kind.
        for &aid in inputs.iter() {
            let k = reg.artifact(aid).kind;
            assert!(
                matches!(k, ArtifactKind::Object),
                "CcArchive input must be Object, got {:?} for {}",
                k,
                reg.artifact_path(aid).display()
            );
        }
        assert!(
            !inputs.is_empty(),
            "CcArchive must have at least one .o input"
        );
        // Single StaticLibrary output.
        assert_eq!(outputs.len(), 1, "CcArchive emits one .a");
        let out_kind = reg.artifact(outputs[0]).kind;
        assert!(matches!(out_kind, ArtifactKind::StaticLibrary));
    }

    #[test]
    fn declared_action_chain_has_three_mnemonics() {
        // Sanity: cc_proto_library declares exactly 3 distinct
        // mnemonics for one .proto — ProtocCompile, CppCompile,
        // CcArchive. Number of action instances depends on the
        // .proto count; the mnemonic SET is invariant.
        let mut reg = ArtifactRegistry::new();
        let _ = analyze_one_proto(&mut reg);
        let mnemonics: std::collections::HashSet<String> = reg
            .declared_actions()
            .map(|(_id, a)| reg.strings().str(a.mnemonic).to_string())
            .collect();
        assert!(
            mnemonics.contains("ProtocCompile"),
            "missing ProtocCompile, got {:?}",
            mnemonics
        );
        assert!(
            mnemonics.contains("CppCompile"),
            "missing CppCompile, got {:?}",
            mnemonics
        );
        assert!(
            mnemonics.contains("CcArchive"),
            "missing CcArchive, got {:?}",
            mnemonics
        );
    }

    /// #39(1) regression: a `cc_library` whose `srcs` entry is a same-package
    /// GENRULE OUTPUT (the tf_gen_op_wrappers_cc `ops/<group>.cc`) — NOT a
    /// checked-in file — must compile the genrule's genfiles artifact, and the
    /// PASS-A (analyze) compile argv must carry that genfiles path, never the
    /// source-tree path.
    ///
    /// Pre-fix the argv assembler re-resolved the raw `src` via
    /// `resolve_cc_src_path`, yielding the nonexistent source-tree path
    /// `tensorflow/cc/ops/array_ops.cc`; the scheduler then ran
    /// `g++ -c .../ops/array_ops.cc` (cc1plus: No such file or directory) and
    /// produced an empty `libarray_ops.a` — one root of the
    /// libtensorflow_cc.so #39 under-link (`MatrixBandPart` undefined). build()
    /// and build_legacy_command_specs() (PASS-B) already compiled the
    /// redirected path, so this was also a latent PASS-A/PASS-B divergence
    /// masked by gate targets having no generated srcs. The fix points the
    /// analyze() argv at `src_path_check`, the single resolution all three
    /// passes share. See commits d18b35d (STEP B) and c5da0ca (STEP B fix).
    #[test]
    fn cc_library_generated_src_compiles_genfiles_path_not_source_tree() {
        let pkg = "tensorflow/cc";
        let src = "ops/array_ops.cc";
        // The genrule writes its output at <output_dir>/genfiles/<pkg>/<src>;
        // `predicted_output_path("genfiles"/pkg/src)` resolves to the same abs
        // path the cc_library resolver looks up via find_artifact_by_path.
        let gen_abs = PathBuf::from(format!("{}/genfiles/{}/{}", TEST_OUT, pkg, src));
        // Source-tree path — what the buggy re-resolution produced; never on disk.
        let src_tree = format!("{}/{}/{}", TEST_WS, pkg, src);

        let mut reg = ArtifactRegistry::new();
        // Stand in for GenruleInstance::analyze() having declared the generated
        // .cc as a producer artifact (commit cab97b6, STEP A).
        let gen_pid = reg.intern_path(&gen_abs);
        let gen_aid = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: gen_pid,
            },
            ArtifactKind::Generated,
            /*is_source*/ false,
            /*owner_target (the genrule)*/ TargetId(2),
            /*owner_action*/ None,
        );

        let resolver = StubIncludeResolver::new(PathBuf::from(TEST_WS));
        let toolchain = Toolchain::default();
        let extra: Vec<PathBuf> = Vec::new();
        let ws = PathBuf::from(TEST_WS);
        let out = PathBuf::from(TEST_OUT);
        {
            let mut ctx = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                pkg,
                None,
                &out,
                &mut reg,
                &resolver,
                &toolchain,
                &ws,
                &extra,
            );
            let rule = CcLibraryInstance {
                name: "array_ops".to_string(),
                srcs: vec![src.to_string()],
                hdrs: vec![],
                deps: vec![],
                copts: vec![],
                linkopts: vec![],
                includes: vec![],
                strip_include_prefix: None,
                include_prefix: None,
                defines: vec![],
                alwayslink: false,
                data: vec![],
            };
            let _ = rule.analyze(&mut ctx, &[]);
        }

        let mut found = false;
        for (id, action) in reg.declared_actions() {
            if reg.strings().str(action.mnemonic) != "CppCompile" {
                continue;
            }
            found = true;
            // (a) the genrule's generated artifact is the declared src input.
            assert!(
                action.inputs.iter().any(|&aid| aid == gen_aid),
                "CppCompile must declare the genrule's generated .cc artifact \
                 as input (the producer edge), inputs={:?}",
                action
                    .inputs
                    .iter()
                    .map(|&a| reg.artifact_path(a).display().to_string())
                    .collect::<Vec<_>>()
            );
            // (b) the compile argv carries the genfiles path, NOT the
            // nonexistent source-tree path.
            let exec = reg
                .executable_action(id)
                .expect("CppCompile must carry an ExecutableAction (PASS-A)");
            let argv: Vec<String> = exec
                .argv
                .iter()
                .map(|s| reg.strings().str(*s).to_string())
                .collect();
            assert!(
                argv.iter().any(|a| a == &gen_abs.display().to_string()),
                "argv must compile the genfiles path {}, got {:?}",
                gen_abs.display(),
                argv
            );
            assert!(
                !argv.iter().any(|a| a == &src_tree),
                "argv must NOT compile the nonexistent source-tree path {}, \
                 got {:?}",
                src_tree,
                argv
            );
        }
        assert!(
            found,
            "cc_library with a generated src must declare a CppCompile action"
        );
    }
}

#[cfg(test)]
mod link_argv_partition_tests {
    //! Regression tests for the #38 cc_shared_library final-link ARG_MAX
    //! workaround: the link argv is partitioned into driver flags + a linker
    //! response file passed as a single `-Wl,@<file>` token. See
    //! `partition_link_argv` / `quote_response_file_arg`.
    use super::{
        is_driver_only_link_flag, mold_small_link_threads_flag,
        parse_mold_small_link_threads_override, partition_link_argv,
        quote_response_file_arg, MoldSmallLinkThreadsOverride,
    };
    use std::path::PathBuf;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn response_file_quoting_escapes_backslash_and_quote() {
        assert_eq!(quote_response_file_arg("/x/lib.a"), "\"/x/lib.a\"");
        assert_eq!(quote_response_file_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_response_file_arg("a\\b"), "\"a\\\\b\"");
        // A path containing a space stays a single response-file token.
        assert_eq!(quote_response_file_arg("a b"), "\"a b\"");
    }

    #[test]
    fn pthread_is_driver_flag_not_linker() {
        // The bug that surfaced empirically: ld rejects `-pthread`
        // ("unrecognized option"). It must be classified driver-only.
        assert!(is_driver_only_link_flag("-pthread"));
        assert!(is_driver_only_link_flag("-shared"));
        assert!(is_driver_only_link_flag("-fPIC"));
        assert!(is_driver_only_link_flag("-static-libstdc++"));
        // Linker-level tokens are NOT driver-only.
        assert!(!is_driver_only_link_flag("/x/libfoo.a"));
        assert!(!is_driver_only_link_flag("-lpthread"));
        assert!(!is_driver_only_link_flag("-L/x"));
        assert!(!is_driver_only_link_flag("--whole-archive"));
    }

    #[test]
    fn partition_basic_shape() {
        let (driver, ld) = partition_link_argv(&v(&[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libtensorflow_cc.so.2.15.0",
            "-o",
            "/out/libtensorflow_cc.so.2.15.0",
            "-Wl,--whole-archive",
            "/x/liba.a",
            "-Wl,--no-whole-archive",
            "/x/libb.a",
            "-pthread",
            "-lm",
            "-L/sys",
        ]));
        // Driver line: only driver-interpreted flags + the -o pair.
        assert_eq!(
            driver,
            v(&["-shared", "-fPIC", "-o", "/out/libtensorflow_cc.so.2.15.0", "-pthread"])
        );
        // ld response file: -Wl, prefixes stripped + comma-split, objects/-l/-L
        // verbatim, original order preserved (whole-archive grouping intact).
        assert_eq!(
            ld,
            v(&[
                "-soname",
                "libtensorflow_cc.so.2.15.0",
                "--whole-archive",
                "/x/liba.a",
                "--no-whole-archive",
                "/x/libb.a",
                "-lm",
                "-L/sys",
            ])
        );
    }

    #[test]
    fn partition_dedups_driver_flags() {
        // 15k repeated -pthread tokens (one per cc_library) must collapse to a
        // single driver-line copy, else COLLECT_GCC_OPTIONS re-bloats past
        // MAX_ARG_STRLEN and defeats the whole workaround.
        let mut args = vec!["-shared".to_string(), "-o".to_string(), "/o/x.so".to_string()];
        for _ in 0..15_000 {
            args.push("-pthread".to_string());
            args.push("/x/obj.o".to_string());
        }
        let (driver, ld) = partition_link_argv(&args);
        assert_eq!(driver.iter().filter(|f| *f == "-pthread").count(), 1);
        // Every object is still present in the ld response file (not deduped).
        assert_eq!(ld.iter().filter(|t| *t == "/x/obj.o").count(), 15_000);
        assert!(!ld.iter().any(|t| t == "-pthread"));
    }

    #[test]
    fn partition_handles_rpath_and_version_script_wl_forms() {
        let (driver, ld) = partition_link_argv(&v(&[
            "-shared",
            "-o",
            "/o/x.so",
            "-Wl,-rpath,/runtime/dir",
            "-Wl,--version-script=/o/x.lds",
            "/x/obj.o",
        ]));
        assert_eq!(driver, v(&["-shared", "-o", "/o/x.so"]));
        assert_eq!(
            ld,
            v(&["-rpath", "/runtime/dir", "--version-script=/o/x.lds", "/x/obj.o"])
        );
    }

    #[test]
    fn small_link_threads_defaults_only_for_mold_object_heavy_links() {
        let object_paths: Vec<PathBuf> = (0..256)
            .map(|i| {
                PathBuf::from(format!(
                    "/tmp/rushbench/object_{i:04}_with_a_really_long_name_to_cross_the_size_threshold.o"
                ))
            })
            .collect();
        assert_eq!(
            mold_small_link_threads_flag(
                Some("-fuse-ld=mold"),
                &object_paths,
                &[],
                &[],
                &[],
            ),
            Some("-Wl,--threads=3".to_string())
        );
        assert_eq!(
            mold_small_link_threads_flag(Some("-fuse-ld=lld"), &object_paths, &[], &[], &[]),
            None
        );
        assert_eq!(
            mold_small_link_threads_flag(
                Some("-fuse-ld=mold"),
                &object_paths,
                &["/x/libfoo.a".to_string()],
                &[],
                &[],
            ),
            None
        );
    }

    #[test]
    fn small_link_threads_override_parser_honors_expected_values() {
        assert_eq!(
            parse_mold_small_link_threads_override(None),
            MoldSmallLinkThreadsOverride::Unset
        );
        assert_eq!(
            parse_mold_small_link_threads_override(Some("".to_string())),
            MoldSmallLinkThreadsOverride::Disabled
        );
        assert_eq!(
            parse_mold_small_link_threads_override(Some("off".to_string())),
            MoldSmallLinkThreadsOverride::Disabled
        );
        assert_eq!(
            parse_mold_small_link_threads_override(Some("no-threads".to_string())),
            MoldSmallLinkThreadsOverride::Flag("-Wl,--no-threads".to_string())
        );
        assert_eq!(
            parse_mold_small_link_threads_override(Some("3".to_string())),
            MoldSmallLinkThreadsOverride::Flag("-Wl,--threads=3".to_string())
        );
    }
}
