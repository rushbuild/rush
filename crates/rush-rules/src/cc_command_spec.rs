// Rush Rules - Command-spec builders for scheduler verification
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 verifier substrate: typed builders for the four
//! action kinds the scheduler currently executes.
//!
//! ChatGPT Pro's commit 3 in the verification ladder. Each spec
//! captures the typed inputs an argv-assembly call needs (paths,
//! flags, defines, etc.) and produces a [`NormalizedCommandSpec`]
//! via `to_normalized_command_spec()`. The legacy `build()` path
//! and the scheduler `analyze()` path both construct one of these
//! and run it through the same builder — divergence of the
//! produced argv is then a divergence of the input data, which
//! the verifier surfaces.
//!
//! ## Scope
//!
//! Only the four already-scheduler-capable rule actions:
//!
//!   - [`CcCompileCommandSpec`] — cc_library / cc_binary CppCompile
//!     (and CCompile for `.c` sources)
//!   - [`CcArchiveCommandSpec`] — cc_library CcArchive
//!   - [`CcLinkCommandSpec`] — cc_binary CcLink
//!
//! tf_cc_library / cc_test / gentbl / proto rules etc. are NOT
//! covered here. They join the verification ladder when their
//! own analyze() / build() paths are migrated to populate
//! ExecutableAction.
//!
//! ## What this is NOT
//!
//! - Not a universal "every rule's command builder" abstraction.
//!   Pro: "do not over-abstract" + "this is Rust, the borrow
//!   checker will punish performative elegance, as is tradition."
//! - Not a cache-key derivation. OKR 4 work; deferred.
//! - Not a refactor of the legacy `build()` paths in this
//!   commit. The spec types stand alone; commit 4 wires both
//!   paths through them as part of `--verify-scheduler-commands`.

use rush_action::command_spec::NormalizedCommandSpec;
use std::path::{Path, PathBuf};

/// Drop the explicit, exact host-system include root `-I/usr/include` from the
/// compile argv.
///
/// rush's system-package stubs (sqlite/gif/png/jsoncpp/snappy/…) inject
/// `-I/usr/include`. When the host has Debian's libabsl-dev installed
/// (`/usr/include/absl` = inline namespace `debian7`), a TU whose collected
/// include list happens to place `-I/usr/include` before the TF-pinned
/// Abseil `-I` resolves `absl/base/options.h` to `debian7` instead of the
/// pinned `lts_20230125` — producing ABI-incompatible static libs (e.g.
/// `tsl::crc32c::Extend(absl::debian7::Cord)` vs the rest of TF's
/// `absl::lts_20230125::Cord`) that fail to link.
///
/// The explicit `-I/usr/include` is redundant: GCC already searches
/// `/usr/include` by default as the LAST system include dir — after every
/// `-I` and `-isystem` dir and after the libstdc++/gcc fixed-include dirs. So
/// dropping it (a) keeps system headers like `sqlite3.h` / `gif_lib.h`
/// resolvable via the default search, (b) makes the pinned Abseil `-I` win
/// over the host `debian7` absl (matching Bazel, which never puts
/// `/usr/include/absl` on the search path), and (c) — unlike re-adding it as
/// an explicit `-isystem /usr/include` — does NOT break libstdc++'s
/// `#include_next` chains (e.g. `<cstdlib>` → `stdlib.h`), which a reordered
/// system search would.
///
/// Exact-match only: `/usr/include/jsoncpp`, `/usr/include/x86_64-linux-gnu`,
/// etc. stay as `-I` (they carry no absl and aren't the default-searched
/// root). MUST be applied identically in PASS-A (analyze inline argv) and
/// PASS-B (the normalized spec) so the verifier's argv stays byte-identical.
///
/// Also ensures the TF-pinned Abseil include root is on the `-I` search path
/// (see [`ensure_pinned_absl_include`]): dropping `/usr/include` only fixes
/// TUs that already carry the pinned `-I` (it then beats the default
/// `/usr/include/absl` debian7 search). A TU that includes Abseil but has NO
/// pinned `-I` at all (several tsl/platform libs) would otherwise resolve absl
/// to the host debian7 via GCC's built-in default search. rush compiles ALL of
/// TF against the one pinned Abseil, so injecting its `-I` everywhere restores
/// Bazel's effective isolation (where `@com_google_absl` is the only visible
/// Abseil) and guarantees a single absl ABI. (Codex gpt-5.5 xhigh 019e83bc.)
pub fn demote_host_system_includes(argv: &mut Vec<String>) {
    argv.retain(|a| a != "-I/usr/include");
    ensure_pinned_absl_include(argv);
}

/// Path to the TF-pinned Abseil clone (`lts_20230125`), if materialized.
fn pinned_absl_root() -> Option<PathBuf> {
    let cache = std::env::var("RUSH_GLOBAL_CLONE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/rush-cache/external"));
    let root = cache.join("com_google_absl");
    if root.join("absl/base/options.h").is_file() {
        Some(root)
    } else {
        None
    }
}

/// Ensure the pinned-Abseil `-I` is present on a C++ compile argv (idempotent;
/// no-op if the clone is absent or the flag is already present). Any `-I`
/// position beats GCC's built-in default `/usr/include/absl` (debian7) search,
/// so appending is sufficient — there is no competing absl `-I` once
/// `/usr/include` is demoted. Guarded so a host without the pinned clone (or a
/// non-TF build) is unaffected → keeps default builds / the verifier gates
/// byte-identical when the clone is absent.
fn ensure_pinned_absl_include(argv: &mut Vec<String>) {
    if let Some(root) = pinned_absl_root() {
        let flag = format!("-I{}", root.display());
        if !argv.contains(&flag) {
            argv.push(flag);
        }
    }
}

/// Phase 5.C.4 commit 20: typed include-layout policy for
/// C++ compile actions. Replaces the
/// `bazel_workspace_includes: bool` + four related fields
/// (`workspace_root`, `extra_workspace_paths`, `genfiles_path`,
/// `linux_extra_includes`) on [`CcCompileCommandSpec`] with one
/// enum that makes invalid combinations harder.
///
/// Pro's Q6 directive: "A single boolean has already served its
/// purpose: announcing that the abstraction is wrong while still
/// compiling. Very considerate of it. ... I prefer the enum
/// because it makes invalid combinations harder. Rust can help
/// prevent nonsense here, which is one of the few times type
/// systems get to feel smug for a good reason."
///
/// Future variants live here: hermetic toolchain include roots,
/// sysroot-anchored compiles, cross-compile prefixes, external
/// repository overlay paths. Adding them as new enum variants
/// keeps the call sites pattern-matching honest — a new compile
/// shape can't pretend to be `BazelPackageRooted` without
/// failing exhaustiveness.
#[derive(Debug, Clone)]
pub enum IncludeLayout {
    /// cc_library / cc_binary: Bazel package-rooted compile.
    /// argv emission appends:
    ///   - `-D_GNU_SOURCE` (on Linux)
    ///   - `-I<linux_extra_includes...>` (on Linux, post-exists filter)
    ///   - `-I<workspace_root>`
    ///   - `-I<extra_workspace_paths...>`
    ///   - `-I<genfiles_path>`
    /// after `direct_include_paths` and `dep_include_paths`.
    BazelPackageRooted {
        /// Workspace root — emitted as `-I<workspace_root>`.
        workspace_root: PathBuf,
        /// External workspace roots (same Vec PASS B's
        /// BuildContext supplies). Emitted as `-I<extra>` per
        /// entry.
        extra_workspace_paths: Vec<PathBuf>,
        /// `<output_dir>/genfiles` — emitted as
        /// `-I<genfiles>` so generated `.pb.h` / `.h.inc`
        /// resolve through the package-rooted convention.
        genfiles_path: PathBuf,
        /// Linux extra include dirs (after `Path::exists()`
        /// check). Caller filters; spec stores the post-filter
        /// list.
        linux_extra_includes: Vec<PathBuf>,
    },
    /// Standalone generated-source compile (cc_proto_library
    /// generated `.pb.cc`, future grpc `.grpc.pb.cc`, etc.). The
    /// only `-I` flags emitted are from `direct_include_paths`
    /// and `dep_include_paths`. No `-D_GNU_SOURCE`, no linux
    /// extras, no workspace/genfiles trailers — the compile is
    /// not package-rooted and doesn't need the Bazel layout.
    Standalone,
}

/// Inputs to one C++ compile action — enough for the spec to
/// produce a deterministic argv via
/// `to_normalized_command_spec()`. Both legacy `build()` and
/// scheduler `analyze()` construct one of these from their
/// respective contexts (BuildContext / AnalysisContext) and
/// pass it to the same builder.
#[derive(Debug, Clone)]
pub struct CcCompileCommandSpec {
    /// `"CppCompile"` (or `"CCompile"` if `is_c_source`). Surfaces
    /// in the NormalizedCommandSpec.
    pub mnemonic: String,
    /// Path to the compiler binary — `toolchain.cxx` for C++
    /// sources, `toolchain.cc` for C sources.
    pub executable: PathBuf,
    /// Working directory; `None` means the build's exec root.
    pub cwd: Option<PathBuf>,
    /// True for `.c` sources, false for `.cc` / `.cpp` / `.cxx`.
    pub is_c_source: bool,
    /// True when this TU has any `@llvm-project` dep and so must compile
    /// `-fno-rtti` to match LLVM/MLIR's upstream cmake default — otherwise
    /// references to typeinfo for LLVM base classes (`llvm::cl::Option`,
    /// `llvm::detail::format_adapter`, …) link-fail because the LLVM
    /// archives never emit them. Set identically here (PASS-B) and in the
    /// analyze pass (PASS-A) for scheduler verifier parity.
    pub wants_no_rtti: bool,
    /// Source path (absolute, as it would be passed to gcc on
    /// argv).
    pub src_path: PathBuf,
    /// Object output path (absolute).
    pub obj_path: PathBuf,
    /// Ordered rule-side `copts`, after automatic language-specific flags.
    /// Explicit language and standard choices are passed to the compiler.
    pub copts: Vec<String>,
    /// `toolchain.cxxflags` — applied for non-C sources.
    pub cxxflags: Vec<String>,
    /// `toolchain.cflags` — applied for C sources.
    pub cflags: Vec<String>,
    /// Rule-side `defines` attribute; emitted as `-D<def>`.
    pub defines: Vec<String>,
    /// Transitive `defines` contributed by dependencies' `CcInfo`,
    /// already postorder/value-deduped by
    /// `providers::flatten_transitive_defines`. Bazel's `defines`
    /// attribute is transitive, so these are emitted (as `-D<def>`)
    /// BEFORE the rule's own `defines`, with own defines suppressed
    /// when a dep already contributed the same exact value.
    pub dep_defines: Vec<String>,
    /// Direct include paths — `self.includes` resolved against
    /// the rule's package. Caller picks the resolution
    /// convention; the spec stores the resolved PathBuf.
    pub direct_include_paths: Vec<PathBuf>,
    /// Quote-only include paths. These satisfy `#include "..."` without
    /// letting an owned/generated package header shadow a system
    /// `#include <...>` with the same basename.
    pub direct_quote_include_paths: Vec<PathBuf>,
    /// Transitive include paths from dep `CcCompilationContext`.
    /// Deduplicated against `direct_include_paths` at argv build
    /// time (matches legacy `args.contains` dedup).
    pub dep_include_paths: Vec<PathBuf>,
    /// Phase 5.C.4 commit 20: typed include-layout policy.
    /// Replaces the previous `bazel_workspace_includes: bool` +
    /// `workspace_root` / `extra_workspace_paths` /
    /// `genfiles_path` / `linux_extra_includes` quartet with one
    /// enum variant per shape. See [`IncludeLayout`] for the
    /// per-variant semantics.
    pub include_layout: IncludeLayout,
    /// Declared input artifacts as paths. For the verifier's
    /// declared-input comparison.
    pub declared_input_paths: Vec<PathBuf>,
    /// Declared output paths. For the verifier's
    /// declared-output comparison.
    pub declared_output_paths: Vec<PathBuf>,
    /// Toolchain identity string (`gcc-12.2.0`, `clang 18`, …).
    /// `None` when the builder doesn't know.
    pub toolchain_identity: Option<String>,
}

/// The explicit GCC/Clang header-search subset admitted by the native
/// execution namespace. Parse the supplied normalized compile arguments,
/// preserving toolchain and rule search order in analysis and execution.
/// Preserve the existing host SDK assumptions. Implicit SDK file values still
/// require compiler input discovery; this parser covers declared source roots.
pub(crate) struct NamespaceIncludeSearch {
    pub quote: Vec<PathBuf>,
    pub ordinary: Vec<PathBuf>,
    pub system: Vec<PathBuf>,
    pub forced: Vec<String>,
}

impl NamespaceIncludeSearch {
    pub fn from_argv(argv: &[String], cwd: &Path) -> anyhow::Result<Self> {
        let mut search = Self {
            quote: Vec::new(), ordinary: Vec::new(), system: Vec::new(), forced: Vec::new(),
        };
        let mut index = 0;
        while index < argv.len() {
            let arg = &argv[index];
            // These operands are data, even when they look like options.
            if matches!(arg.as_str(), "-D" | "-U" | "-o" | "-target" | "--target") {
                index += 2;
                continue;
            }
            if let Some(language) = arg.strip_prefix("-x") {
                let language = if language.is_empty() {
                    index += 1;
                    argv.get(index).ok_or_else(|| anyhow::anyhow!("missing operand for -x"))?.as_str()
                } else { language };
                if !matches!(language, "c" | "c++" | "none") {
                    anyhow::bail!("native execution namespace does not admit source language {language:?}");
                }
                index += 1;
                continue;
            }
            if arg.starts_with('@') || arg == "-I-"
                || arg.starts_with("-idirafter") || arg.starts_with("-iprefix")
                || arg.starts_with("-iwithprefix") || arg.starts_with("-isysroot")
                || arg.starts_with("--sysroot") || arg.starts_with("-nostdinc")
                || arg.starts_with("-Wp,") || arg == "-Xpreprocessor"
                || arg.starts_with("-ivfsoverlay") || arg.starts_with("-iframework")
                || arg.starts_with("-F") || arg.starts_with("-imacros")
                || arg.starts_with("-include-pch") || arg.starts_with("-fmodule")
                || arg.starts_with("--include") || arg.starts_with("-B")
                || arg.starts_with("-imultilib") || arg.starts_with("-imultiarch")
                || arg.starts_with("-internal-isystem") || arg.starts_with("-cxx-isystem")
                || arg.starts_with("-working-directory") || arg.starts_with("--config")
                || arg == "-Xclang" || arg.starts_with("-fplugin")
                || arg.starts_with("-specs") || arg.starts_with("--specs") || arg == "-wrapper"
                || arg == "-Xassembler" || arg == "-Xlinker" || arg == "-mllvm"
                || arg.starts_with("-Wa,") || arg.starts_with("-M")
                // GCC dump-name operands can look like search options. Refuse
                // the family rather than interpreting its operand as a root.
                || arg.starts_with("-dumpbase") || arg.starts_with("--dumpbase")
                || arg.starts_with("-dumpdir") || arg.starts_with("--dumpdir")
            {
                anyhow::bail!("native execution namespace does not admit header-search option {arg:?}");
            }
            for flag in ["-iquote", "-isystem", "-include", "-I"] {
                let Some(suffix) = arg.strip_prefix(flag) else { continue; };
                let value = if suffix.is_empty() {
                    index += 1;
                    argv.get(index).ok_or_else(|| anyhow::anyhow!("missing operand for {flag}"))?
                } else { suffix };
                if value.is_empty() || value.starts_with('=') || value.starts_with('-') || value.contains("$SYSROOT") {
                    anyhow::bail!("native execution namespace does not admit {flag} operand {value:?}");
                }
                if flag == "-include" {
                    search.forced.push(value.to_string());
                } else {
                    let path = Path::new(value);
                    let path = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
                    let roots = match flag {
                        "-iquote" => &mut search.quote,
                        "-isystem" => &mut search.system,
                        _ => &mut search.ordinary,
                    };
                    if !roots.contains(&path) { roots.push(path); }
                }
                break;
            }
            index += 1;
        }
        // GCC ignores an ordinary -I which also names an explicit system
        // directory: it retains the latter's system-search position.
        search.ordinary.retain(|path| !search.system.contains(path));
        Ok(search)
    }
}

impl CcCompileCommandSpec {
    /// Phase 5.C.4 commit 12: minimal constructor for a
    /// generated-source C++ compile (cc_proto_library's CppCompile
    /// on a `.pb.cc`). Mirrors what `compile_proto_source` does at
    /// build time: `cxx -c -fPIC -std=c++17 [cxxflags...]
    /// -I<genfiles_root> -I<parent_of_src> -o <obj> <src>`.
    ///
    /// Pro's Q2 call: "use CcCompileCommandSpec, with a
    /// generated-proto helper. Do not inline a separate C++
    /// compile command builder." The .pb.cc compile is still a
    /// C++ compile; routing it through the same spec struct as
    /// cc_library prevents the verifier from drifting against a
    /// parallel command builder. The only difference from a
    /// cc_library compile is that the standalone proto compile
    /// does NOT carry `-I<workspace_root>` / `-I<genfiles>` /
    /// linux extras / `-D_GNU_SOURCE` — `bazel_workspace_includes`
    /// is set to false to gate those off.
    ///
    /// A legacy direct `/usr/include` entry is still assembled here for older
    /// system-protobuf shapes, but normalization demotes it out of the final
    /// argv via [`demote_host_system_includes`].
    pub fn for_generated_cpp(
        cxx: PathBuf,
        cxxflags: Vec<String>,
        src_path: PathBuf,
        obj_path: PathBuf,
        declared_input_paths: Vec<PathBuf>,
        // genfiles ROOT — so a generated .pb.cc's import-relative
        // `#include "<repo>/<path>.pb.h"` resolves. Pass the genfiles dir
        // protoc wrote into; empty PathBuf to opt out.
        genfiles_root: PathBuf,
        // MAIN owned execution namespace, when explicitly supplied by context.
        cwd: Option<PathBuf>,
    ) -> Self {
        let parent = src_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(""));
        let mut direct_include_paths: Vec<PathBuf> = Vec::new();
        if !genfiles_root.as_os_str().is_empty() {
            direct_include_paths.push(genfiles_root);
        }
        direct_include_paths.push(parent);
        direct_include_paths.push(PathBuf::from("/usr/include"));
        Self {
            mnemonic: "CppCompile".to_string(),
            executable: cxx,
            cwd,
            is_c_source: false,
            wants_no_rtti: false,
            src_path: src_path.clone(),
            obj_path: obj_path.clone(),
            copts: Vec::new(),
            cxxflags,
            cflags: Vec::new(),
            defines: Vec::new(),
            dep_defines: Vec::new(),
            direct_include_paths,
            direct_quote_include_paths: Vec::new(),
            dep_include_paths: Vec::new(),
            include_layout: IncludeLayout::Standalone,
            declared_input_paths,
            declared_output_paths: vec![obj_path],
            toolchain_identity: None,
        }
    }

    /// Phase 5.C.4 commit 19: cc_test minimal compile spec.
    ///
    /// cc_test's compile argv is structurally different from
    /// cc_library / cc_binary / generated-proto compile:
    ///
    ///   cxxflags + copts + -D defines + -c src -o obj
    ///
    /// No `-fPIC`, no `-std=c++17`, no `-I` flags, no
    /// `-D_GNU_SOURCE`, no linux_extra_includes, no
    /// workspace/genfiles trailers. cxxflags is emitted FIRST
    /// (vs CcCompileCommandSpec where they ride after copts).
    /// The src and obj also appear in cc_test's order
    /// (`-c src -o obj`), not CcCompileCommandSpec's
    /// `-o obj src`.
    ///
    /// Pro's Q4 directive: "all C++ compile-like actions should
    /// go through the same command-spec machinery. The verifier
    /// exists because parallel command construction drifts.
    /// Leaving cc_test inline is not fatal but it is the exact
    /// sort of small inconsistency that becomes a 'why is this
    /// target special?' afternoon." Pro also warned: "Do not
    /// force `for_generated_cpp` if the name becomes
    /// semantically weird."
    ///
    /// Returns `NormalizedCommandSpec` directly (not a
    /// `CcCompileCommandSpec` value) because cc_test's argv
    /// shape doesn't fit into the package-rooted include /
    /// -fPIC / -std=c++17 prelude `to_normalized_command_spec`
    /// emits. Sharing happens at the helper-naming /
    /// module-location level: this lives next to
    /// `for_generated_cpp` in `cc_command_spec.rs`, callers
    /// don't construct argv inline, future cc_test argv
    /// changes happen here once.
    pub fn for_test_compile(
        cxx: PathBuf,
        cxxflags: Vec<String>,
        copts: Vec<String>,
        defines: Vec<String>,
        dep_defines: Vec<String>,
        src_path: PathBuf,
        obj_path: PathBuf,
        declared_input_paths: Vec<PathBuf>,
        // Set before normalization/keying; None retains legacy execution.
        cwd: Option<PathBuf>,
    ) -> NormalizedCommandSpec {
        let mut argv: Vec<String> = Vec::new();
        argv.extend(cxxflags.iter().cloned());
        // Drop unexpanded `$(VAR)` make-variable copts (see the other
        // to_normalized_command_spec) so they don't reach g++ as literals.
        argv.extend(copts.iter().filter(|o| !o.contains("$(")).cloned());
        // Transitive dep defines (Bazel postorder) before own direct
        // defines — shared merge keeps this identical to analyze().
        for d in crate::providers::merged_compile_defines(&dep_defines, &defines) {
            argv.push(format!("-D{}", d));
        }
        argv.push("-c".to_string());
        argv.push(src_path.display().to_string());
        argv.push("-o".to_string());
        argv.push(obj_path.display().to_string());

        let mut spec = NormalizedCommandSpec {
            mnemonic: "CppCompile".to_string(),
            executable: cxx,
            cwd,
            argv,
            env: Vec::new(),
            declared_inputs: declared_input_paths,
            declared_outputs: vec![obj_path],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
        };
        spec.normalize();
        spec
    }

    /// Produce the canonical [`NormalizedCommandSpec`] for this
    /// compile action. argv is assembled in the same order both
    /// legacy `compile_source` and scheduler `analyze()` use, so
    /// any divergence in the produced argv is a divergence in
    /// the input data.
    pub fn to_normalized_command_spec(&self) -> NormalizedCommandSpec {
        let mut argv: Vec<String> = Vec::new();
        argv.push("-c".to_string());
        argv.push("-fPIC".to_string());
        // Phase 5.C.4 commit 20: linux_extras + -D_GNU_SOURCE
        // are part of the BazelPackageRooted layout. Standalone
        // compiles (cc_proto_library generated .pb.cc, etc.)
        // skip them entirely — only direct/dep include paths
        // emit for those.
        let linux_extras: &[PathBuf] = match &self.include_layout {
            IncludeLayout::BazelPackageRooted {
                linux_extra_includes,
                ..
            } => linux_extra_includes.as_slice(),
            IncludeLayout::Standalone => &[],
        };
        let bazel_layout =
            matches!(self.include_layout, IncludeLayout::BazelPackageRooted { .. });
        if !self.is_c_source {
            argv.push("-std=c++17".to_string());
            // Phase 5.C.5h: force-include <cstdint> for C++ TUs under the
            // bazel-rooted layout, byte-identical to the analyze pass
            // (rules_cc.rs cc_library@636 / cc_test@1496). gcc-13+/libstdc++
            // no longer transitively include <cstdint>, so TF/LLVM/MLIR
            // headers using int32_t etc. without including it (e.g. stablehlo
            // math_utils.h) fail "'int32_t' has not been declared". Gated on
            // bazel_layout so Standalone proto .pb.cc compiles — whose analyze
            // path does NOT add it — stay byte-identical (PASS-A/PASS-B parity).
            // Toolchain workaround, NOT a TF source patch. Assembly TUs
            // (.s/.S) are excluded — they cannot include the C++ header
            // (e.g. @boringssl//:crypto's *-apple.S); is_c_source only
            // catches .c, so assembly needs an explicit guard.
            if bazel_layout && !is_assembly_source_path(&self.src_path) {
                argv.push("-include".to_string());
                argv.push("cstdint".to_string());
            }
            // -fno-rtti for @llvm-project-consuming C++ TUs, placed right after
            // the cstdint block so PASS-A (analyze inline argv) and PASS-B
            // (this spec) stay byte-identical. Matches LLVM's -fno-rtti build
            // (else typeinfo for llvm::cl::Option / format_adapter is undefined
            // at link — e.g. //tensorflow/compiler/mlir/lite:converter-gen).
            if self.wants_no_rtti {
                argv.push("-fno-rtti".to_string());
            }
        }
        if cfg!(target_os = "linux") && bazel_layout {
            argv.push("-D_GNU_SOURCE".to_string());
            for p in linux_extras {
                argv.push(format!("-I{}", p.display()));
            }
        }
        // Defaults precede explicit rule options. In particular, a .c input
        // may deliberately use -x c++ and -std=c++11; its filename must not
        // erase that standard or let a later default override it.
        if self.is_c_source {
            argv.extend(self.cflags.iter().cloned());
        } else {
            argv.extend(self.cxxflags.iter().cloned());
        }
        for opt in &self.copts {
            // rush does not model Bazel make-variables; an unexpanded
            // `$(VAR)` copt (e.g. TF's `$(STACK_FRAME_UNLIMITED)`) would
            // otherwise reach g++ as a literal input file ("linker input
            // file not found"). These are warning/frame-size controls — an
            // empty expansion (drop) is the safe faithful degradation.
            if opt.contains("$(") {
                continue;
            }
            argv.push(opt.clone());
        }
        // Transitive dep defines first (Bazel postorder), then own
        // direct defines minus exact dups — via the shared merge so
        // this matches the inline analyze()/compile_source() paths.
        for d in crate::providers::merged_compile_defines(&self.dep_defines, &self.defines) {
            argv.push(format!("-D{}", d));
        }
        for p in &self.direct_quote_include_paths {
            argv.push(format!("-iquote{}", p.display()));
        }
        for p in &self.direct_include_paths {
            argv.push(format!("-I{}", p.display()));
        }
        // Dedup -I against direct entries to mirror legacy
        // `args.contains()` dedup.
        let mut argv_seen: std::collections::HashSet<String> =
            argv.iter().cloned().collect();
        for p in &self.dep_include_paths {
            let flag = format!("-I{}", p.display());
            if argv_seen.insert(flag.clone()) {
                argv.push(flag);
            }
        }
        // Phase 5.C.4 commit 20: package-rooted include trio
        // (workspace_root, extra_workspace_paths, genfiles_path)
        // emitted only for BazelPackageRooted layout. Standalone
        // generated-source compiles emit only their
        // direct_include_paths and dep_include_paths.
        if let IncludeLayout::BazelPackageRooted {
            workspace_root,
            extra_workspace_paths,
            genfiles_path,
            ..
        } = &self.include_layout
        {
            argv.push(format!("-I{}", workspace_root.display()));
            for p in extra_workspace_paths {
                argv.push(format!("-I{}", p.display()));
            }
            argv.push(format!("-I{}", genfiles_path.display()));
        }
        demote_host_system_includes(&mut argv);
        argv.push("-o".to_string());
        argv.push(self.obj_path.display().to_string());
        argv.push(self.src_path.display().to_string());

        let mut spec = NormalizedCommandSpec {
            mnemonic: self.mnemonic.clone(),
            executable: self.executable.clone(),
            cwd: self.cwd.clone(),
            argv,
            env: Vec::new(),
            declared_inputs: self.declared_input_paths.clone(),
            declared_outputs: self.declared_output_paths.clone(),
            toolchain_identity: self.toolchain_identity.clone(),
            param_files: Vec::new(),
            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
        };
        spec.normalize();
        spec
    }
}

/// Inputs to one `ar rcs <lib> --thin` static-archive action.
#[derive(Debug, Clone)]
pub struct CcArchiveCommandSpec {
    /// Always `"CcArchive"`.
    pub mnemonic: String,
    /// `toolchain.ar`.
    pub executable: PathBuf,
    pub cwd: Option<PathBuf>,
    /// Output `.a` path (absolute).
    pub lib_path: PathBuf,
    /// Object input paths (absolute), in declaration order. The
    /// scheduler walks these in DAG topological order, so
    /// caller is responsible for ordering.
    pub object_paths: Vec<PathBuf>,
    pub declared_input_paths: Vec<PathBuf>,
    pub declared_output_paths: Vec<PathBuf>,
    pub toolchain_identity: Option<String>,
}

impl CcArchiveCommandSpec {
    pub fn to_normalized_command_spec(&self) -> NormalizedCommandSpec {
        let mut argv: Vec<String> = Vec::with_capacity(3 + self.object_paths.len());
        argv.push("rcs".to_string());
        argv.push(self.lib_path.display().to_string());
        argv.push("--thin".to_string());
        for o in &self.object_paths {
            argv.push(o.display().to_string());
        }
        let mut spec = NormalizedCommandSpec {
            mnemonic: self.mnemonic.clone(),
            executable: self.executable.clone(),
            cwd: self.cwd.clone(),
            argv,
            env: Vec::new(),
            declared_inputs: self.declared_input_paths.clone(),
            declared_outputs: self.declared_output_paths.clone(),
            toolchain_identity: self.toolchain_identity.clone(),
            param_files: Vec::new(),
            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
        };
        spec.normalize();
        spec
    }
}

/// Inputs to one C++ link action.
///
/// Matches `cc_binary::link_binary`'s argv shape:
///
/// ```text
/// cxx <obj1> <obj2> ... <driver_flags...> <dep_flags...> <linkopts...> <ldflags...> -o <bin>
/// ```
///
/// `dep_flags` covers the typed-CcInfo walk: per-LinkerInputId
/// archive paths (with `--whole-archive`/`--no-whole-archive`
/// wrapping for `alwayslink`), plus
/// `LinkFragment::{Flag,SearchDir,Library,Group}` entries.
/// Pre-deduplicated by the caller against duplicate archive paths.
#[derive(Debug, Clone)]
pub struct CcLinkCommandSpec {
    /// Always `"CcLink"`.
    pub mnemonic: String,
    /// `toolchain.cxx`.
    pub executable: PathBuf,
    pub cwd: Option<PathBuf>,
    /// Object input paths (absolute) — emitted first.
    pub object_paths: Vec<PathBuf>,
    /// Driver-only link flags, such as `-fuse-ld=mold`.
    pub driver_flags: Vec<String>,
    /// Pre-built dep flag stream from
    /// `CcInfo.linking.{linker_inputs,link_fragments}` walk.
    /// Order significant; deduped at construction time.
    pub dep_flags: Vec<String>,
    /// Rule-side `linkopts` attribute.
    pub linkopts: Vec<String>,
    /// `toolchain.ldflags`.
    pub ldflags: Vec<String>,
    /// Final binary output path (absolute).
    pub bin_path: PathBuf,
    pub declared_input_paths: Vec<PathBuf>,
    pub declared_output_paths: Vec<PathBuf>,
    pub toolchain_identity: Option<String>,
}

impl CcLinkCommandSpec {
    pub fn to_normalized_command_spec(&self) -> NormalizedCommandSpec {
        let mut argv: Vec<String> = Vec::new();
        for o in &self.object_paths {
            argv.push(o.display().to_string());
        }
        argv.extend(self.driver_flags.iter().cloned());
        argv.extend(self.dep_flags.iter().cloned());
        argv.extend(self.linkopts.iter().cloned());
        argv.extend(self.ldflags.iter().cloned());
        argv.push("-o".to_string());
        argv.push(self.bin_path.display().to_string());
        let mut spec = NormalizedCommandSpec {
            mnemonic: self.mnemonic.clone(),
            executable: self.executable.clone(),
            cwd: self.cwd.clone(),
            argv,
            env: Vec::new(),
            declared_inputs: self.declared_input_paths.clone(),
            declared_outputs: self.declared_output_paths.clone(),
            toolchain_identity: self.toolchain_identity.clone(),
            param_files: Vec::new(),
            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
        };
        spec.normalize();
        spec
    }
}

/// Phase 5.C.4 Batch AZF-2 (Pro 2026-05-21 candidate-quarantine
/// directive): shared compile-eligible source predicate.
///
/// Returns `true` for paths that should produce a CppCompile
/// action: real source files in `cc_library.srcs`. Returns
/// `false` for headers and label-form refs that may appear in
/// srcs for textual-hdrs-style purposes but must not be
/// compiled as translation units.
///
/// This predicate is the SINGLE source of truth for the
/// "should we compile this src?" question. All three pipelines
/// must consult it:
///
///   - legacy runtime build (`crates/rush-build/src/lib.rs`
///     `build_cc_library` and similar) — so `g++ -c <file>` is
///     only invoked on real sources.
///   - legacy command-spec builder
///     (`crates/rush-rules/src/rules_cc.rs`
///     `build_legacy_command_specs`) — so the spec produced
///     for the verifier matches the runtime.
///   - scheduler analyze
///     (`crates/rush-rules/src/rules_cc.rs::analyze`) — so
///     the scheduler's declared CppCompile set matches both
///     legacy paths.
///
/// Three-way agreement is enforced via this predicate. Pro's
/// 2026-05-21 directive explicitly warned against fixing only
/// the spec builder — that would create a fake-verifier
/// situation where the spec lies about what the runtime does.
///
/// Eligible extensions (case-insensitive):
///
///   - `.c`   — C source
///   - `.cc`, `.cpp`, `.cxx`, `.c++`, `.C` — C++ source
///   - `.s`, `.S` — assembly source
///
/// Ineligible (false):
///
///   - `.h`, `.hpp`, `.hxx`, `.h++`, `.inc` — headers
///   - label-form refs (starts with `//` or `@`)
///   - empty / extensionless paths
///   - any other extension (defensive default-deny)
/// True for assembly sources (`.s` / `.S`, case-insensitive).
///
/// Assembly TUs ARE compiled (see [`is_compilable_source`]) but must NOT
/// receive C++-only flags. In particular `-include cstdint` (the gcc-13+
/// transitive-include workaround, phase 5.C.5h) makes the preprocessor try to
/// open the C++ `<cstdint>` header for an assembly compile and fails with
/// "cstdint: No such file or directory" (e.g. @boringssl//:crypto's
/// chacha-armv4-apple.S). `is_c_source` only excludes `.c`, so assembly would
/// otherwise be misclassified as C++. Both the analyze pass (rules_cc) and the
/// legacy spec gate cstdint on `!is_c_source && !is_assembly_source_path(..)`
/// so PASS-A/PASS-B stay byte-identical.
pub fn is_assembly_source_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("s"))
        .unwrap_or(false)
}

pub fn is_compilable_source(src: &str) -> bool {
    // Eligibility is decided by the FILE COMPONENT's extension, NOT by label
    // syntax (ROOT-3). A label-form src can be either:
    //   - a cross-package HEADER (`//pkg:mutex.h`, `@repo//pkg:foo.h`) — the
    //     textual-hdrs idiom — which must NOT be compiled, OR
    //   - a cross-package SOURCE (`//xla/tsl/platform:env.cc`, an
    //     `exports_files`'d .cc) which Bazel DOES compile into the consuming
    //     target's archive, and rush must too. Rejecting all label-form srcs
    //     (the prior behavior) silently dropped these exported sources — e.g.
    //     tsl/platform/default:env's `//xla/tsl/platform:env.cc` /
    //     `file_system.cc` / `threadpool.cc` — leaving tsl::Env::* /
    //     FileSystem::* undefined at link.
    // The src resolver (rules_cc) turns the label into the real on-disk path;
    // here we only gate on the file component's extension, so header labels
    // stay false and source labels become true.
    //
    // SCOPE (ROOT-3 v1): only `//pkg:file` workspace labels (resolvable against
    // the current target's repo root) and plain package-relative filenames are
    // accepted. `@repo//...` external-repo source labels and `:name`
    // same-package label refs are still rejected here — `@repo//` needs an
    // external-repo-root mapping not yet threaded into the cc src resolver, and
    // accepting it with naive resolution caused a PASS-A/PASS-B divergence
    // (e.g. @local_tsl//tsl/platform:tracing.cc vs the package-local
    // tracing.cc). Those are a documented follow-up; rejecting them here keeps
    // the prior (skip) behavior and avoids the divergence.
    if src.starts_with('@') || src.starts_with(':') {
        return false;
    }
    let file_comp = source_file_component(src);
    let ext = match std::path::Path::new(file_comp)
        .extension()
        .and_then(|e| e.to_str())
    {
        Some(e) => e.to_ascii_lowercase(),
        None => return false,
    };
    matches!(
        ext.as_str(),
        "c" | "cc" | "cpp" | "cxx" | "c++" | "s"
    )
}

/// The file component of a (possibly label-form) src string: the part after
/// the last `:` for `:name` / `//pkg:name` / `@repo//pkg:name`, otherwise the
/// string itself. Used by `is_compilable_source` (extension gate) and by the
/// cc src resolver (rules_cc) to recover the basename/stem of a cross-package
/// source label. Does NOT resolve to a path — purely string surgery.
pub fn source_file_component(src: &str) -> &str {
    match src.rsplit_once(':') {
        Some((_, name)) => name,
        None => src,
    }
}

/// Phase 5.C.4 helper: canonical include-path resolution against
/// a rule's package. Used by both legacy `build()` paths
/// (`context.source_path` was the prior helper) and scheduler
/// `analyze()` paths to produce identical PathBuf output for
/// the same `(workspace_root, package, include)` triple. Lifted
/// out of either path's inline logic so divergence in include
/// resolution can't hide between legacy and scheduler.
pub fn resolve_include_under_package(
    workspace_root: &Path,
    package: &str,
    include: &Path,
) -> PathBuf {
    if include.is_absolute() {
        return include.to_path_buf();
    }
    let pkg_dir = if package.is_empty() {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(package)
    };
    if include == Path::new(".") {
        pkg_dir
    } else {
        pkg_dir.join(include)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn namespace_include_search_refuses_gcc_dump_operands_after_real_tool_control() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        std::fs::create_dir(root.join("fake")).unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        let fake = root.join("fake/value.h");
        let real = root.join("real/value.h");
        std::fs::write(&fake, "#define VALUE 41\n").unwrap();
        std::fs::write(&real, "#define VALUE 42\n").unwrap();
        let source = root.join("main.cc");
        let executable = root.join("program");
        std::fs::write(
            &source,
            "#include <value.h>\nint main() { return VALUE; }\n",
        )
        .unwrap();
        let compile_and_run = |options: &[String], expected: i32| {
            match std::fs::remove_file(&executable) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => panic!("remove old fixture executable: {error}"),
            }
            let mut arguments = vec!["-std=c++17".to_owned()];
            arguments.extend_from_slice(options);
            arguments.extend([
                source.to_str().unwrap().to_owned(),
                "-o".to_owned(),
                executable.to_str().unwrap().to_owned(),
            ]);
            let compiled = std::process::Command::new("/usr/bin/g++")
                .args(&arguments)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(
                compiled.status.success(),
                "actual GCC rejected {arguments:?}: {}",
                String::from_utf8_lossy(&compiled.stderr)
            );
            assert!(executable.is_file());
            assert_eq!(
                std::process::Command::new(&executable)
                    .current_dir(root)
                    .status()
                    .unwrap()
                    .code(),
                Some(expected),
                "actual header selected for {arguments:?}"
            );
            arguments
        };
        // Both roots exist. Without a dump operand, the first real -I wins.
        compile_and_run(&["-Ifake".into(), "-Ireal".into()], 41);
        for option in [
            "-dumpbase",
            "-dumpbase-ext",
            "-dumpdir",
            "--dumpbase",
            "--dumpbase-ext",
            "--dumpdir",
        ] {
            for value in [42, 43] {
                std::fs::write(&real, format!("#define VALUE {value}\n")).unwrap();
                let options = vec![option.to_owned(), "-Ifake".into(), "-Ireal".into()];
                // The first -I-looking token is dump-name data, not a root.
                let arguments = compile_and_run(&options, value);
                let error = NamespaceIncludeSearch::from_argv(&arguments, root)
                    .err()
                    .expect("dump operand must refuse before literal header selection");
                assert!(error.to_string().contains(option), "{error:#}");
                assert_eq!(std::fs::read(&fake).unwrap(), b"#define VALUE 41\n");
            }
            // Prefix refusal also covers joined spellings conservatively;
            // these are refusal controls, not extra admitted GCC syntax.
            for joined in [format!("{option}=ignored"), format!("{option}ignored")] {
                assert!(NamespaceIncludeSearch::from_argv(
                    &[joined, "-Ifake".into(), "-Ireal".into()],
                    root,
                )
                .is_err());
            }
        }
    }

    fn compile_spec_min(is_c_source: bool) -> CcCompileCommandSpec {
        CcCompileCommandSpec {
            mnemonic: if is_c_source {
                "CCompile".to_string()
            } else {
                "CppCompile".to_string()
            },
            executable: PathBuf::from(if is_c_source { "/usr/bin/gcc" } else { "/usr/bin/g++" }),
            cwd: None,
            is_c_source,
            wants_no_rtti: false,
            src_path: PathBuf::from("/ws/src.cc"),
            obj_path: PathBuf::from("/ws/out/obj.o"),
            copts: Vec::new(),
            cxxflags: Vec::new(),
            cflags: Vec::new(),
            defines: Vec::new(),
            dep_defines: Vec::new(),
            direct_include_paths: Vec::new(),
            direct_quote_include_paths: Vec::new(),
            dep_include_paths: Vec::new(),
            declared_input_paths: vec![PathBuf::from("/ws/src.cc")],
            declared_output_paths: vec![PathBuf::from("/ws/out/obj.o")],
            toolchain_identity: None,
            include_layout: IncludeLayout::BazelPackageRooted {
                workspace_root: PathBuf::from("/ws"),
                extra_workspace_paths: Vec::new(),
                genfiles_path: PathBuf::from("/ws/out/genfiles"),
                linux_extra_includes: Vec::new(),
            },
        }
    }

    #[test]
    fn cc_compile_spec_basic_argv_shape() {
        let spec = compile_spec_min(false);
        let n = spec.to_normalized_command_spec();
        assert_eq!(n.executable, PathBuf::from("/usr/bin/g++"));
        // Ends with -o <obj> <src>; src and obj come last.
        let argv = &n.argv;
        assert_eq!(argv[argv.len() - 3], "-o");
        assert_eq!(argv[argv.len() - 2], "/ws/out/obj.o");
        assert_eq!(argv[argv.len() - 1], "/ws/src.cc");
        // -c -fPIC at the start, -std=c++17 follows for C++.
        assert_eq!(argv[0], "-c");
        assert_eq!(argv[1], "-fPIC");
        assert_eq!(argv[2], "-std=c++17");
    }

    #[test]
    fn cc_compile_spec_c_source_omits_std_cpp17_uses_cflags() {
        let mut spec = compile_spec_min(true);
        spec.cflags = vec!["-Wall".to_string()];
        spec.cxxflags = vec!["-Wno-unused".to_string()]; // SHOULD NOT appear
        let n = spec.to_normalized_command_spec();
        assert!(!n.argv.iter().any(|a| a == "-std=c++17"));
        assert!(n.argv.iter().any(|a| a == "-Wall"));
        assert!(!n.argv.iter().any(|a| a == "-Wno-unused"));
    }

    #[test]
    fn cc_compile_spec_dep_include_dedup_against_direct() {
        let mut spec = compile_spec_min(false);
        spec.direct_include_paths = vec![PathBuf::from("/ws/inc")];
        spec.dep_include_paths = vec![
            PathBuf::from("/ws/inc"),     // duplicate of direct → suppressed
            PathBuf::from("/ws/dep_inc"), // unique → kept
        ];
        let n = spec.to_normalized_command_spec();
        let i_count = n
            .argv
            .iter()
            .filter(|a| a == &&"-I/ws/inc".to_string())
            .count();
        assert_eq!(i_count, 1, "duplicate -I must be deduped");
        assert!(n.argv.iter().any(|a| a == "-I/ws/dep_inc"));
    }

    #[test]
    fn cc_compile_spec_keeps_quote_only_includes_off_angle_path() {
        let mut spec = compile_spec_min(false);
        spec.direct_quote_include_paths = vec![PathBuf::from("/ws/pkg")];
        let normalized = spec.to_normalized_command_spec();
        assert!(normalized
            .argv
            .contains(&"-iquote/ws/pkg".to_string()));
        assert!(!normalized.argv.contains(&"-I/ws/pkg".to_string()));
    }

    #[test]
    fn cc_compile_spec_preserves_explicit_standard_and_language_options() {
        let mut spec_c = compile_spec_min(true);
        spec_c.copts = vec![
            "-x".to_string(),
            "c++".to_string(),
            "-std=c++11".to_string(),
            "-Wall".to_string(),
        ];
        spec_c.cflags = vec!["-std=c11".to_string()];
        spec_c.cxxflags = vec!["-DWRONG_AUTOMATIC_LANGUAGE".to_string()];
        let n = spec_c.to_normalized_command_spec();
        assert!(n
            .argv
            .windows(5)
            .any(|w| w == ["-std=c11", "-x", "c++", "-std=c++11", "-Wall"]));
        assert!(!n.argv.iter().any(|a| a == "-DWRONG_AUTOMATIC_LANGUAGE"));

        let mut spec_cpp = compile_spec_min(false);
        spec_cpp.copts = vec![
            "-xc".to_string(),
            "-std=c11".to_string(),
            "-include".to_string(),
            "-std=c++11".to_string(), // an option operand, not a standard
        ];
        spec_cpp.cxxflags = vec!["-std=c++17".to_string()];
        let n = spec_cpp.to_normalized_command_spec();
        assert!(n
            .argv
            .windows(5)
            .any(|w| w == ["-std=c++17", "-xc", "-std=c11", "-include", "-std=c++11"]));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cc_compile_spec_real_compiler_honors_explicit_language_and_standard() {
        let temp = tempfile::tempdir().unwrap();
        let toolchain = crate::context::Toolchain::default();
        for (name, copts, expected_cpp) in [
            ("joined.c", vec!["-xc++", "-std=c++11"], Some(201103)),
            ("separated.c", vec!["-x", "c++", "-std=c++11"], Some(201103)),
            ("explicit.cc", vec!["-std=c++11"], Some(201103)),
            ("default.c", vec![], None),
            ("default.cc", vec![], Some(201703)),
        ] {
            let is_c = name.ends_with(".c");
            let src = temp.path().join(name);
            let obj = temp.path().join(format!("{name}.o"));
            let language_check = expected_cpp.map(|version| format!(
                "#if !defined(__cplusplus) || __cplusplus != {version}L\n#error wrong C++ standard\n#endif\n"
            )).unwrap_or_else(|| "#ifdef __cplusplus\n#error expected C\n#endif\n".into());
            std::fs::write(&src, format!("{language_check}\n#if defined(RUSH_AUTO_C) != {} || defined(RUSH_AUTO_CPP) != {}\n#error wrong automatic flags\n#endif\nint rush_option_value(void) {{ return 42; }}\n", usize::from(is_c), usize::from(!is_c))).unwrap();
            let mut spec = compile_spec_min(is_c);
            spec.executable = if is_c {
                toolchain.cc.clone()
            } else {
                toolchain.cxx.clone()
            };
            spec.include_layout = IncludeLayout::Standalone;
            spec.src_path = src;
            spec.obj_path = obj.clone();
            spec.cflags = vec!["-std=c11".into(), "-DRUSH_AUTO_C=1".into()];
            spec.cxxflags = vec!["-std=c++17".into(), "-DRUSH_AUTO_CPP=1".into()];
            spec.copts = copts.iter().map(|value| (*value).to_owned()).collect();
            let command = spec.to_normalized_command_spec();
            let result = std::process::Command::new(&command.executable)
                .args(&command.argv)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{name}: {:?}: {}",
                command.argv,
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(obj.metadata().unwrap().len() > 0);
        }

        // Do not erase an incompatible explicit standard to make the command
        // succeed. GCC may warn while Clang rejects it: preserve the selected
        // compiler's own result rather than inventing language compatibility.
        let src = temp.path().join("incompatible.c");
        std::fs::write(&src, "int rush_option_value(void) { return 42; }\n").unwrap();
        let direct = std::process::Command::new(&toolchain.cc)
            .args(["-c", "-std=c++11"])
            .arg(&src)
            .arg("-o")
            .arg(temp.path().join("direct.o"))
            .output()
            .unwrap();
        let mut spec = compile_spec_min(true);
        spec.executable = toolchain.cc;
        spec.include_layout = IncludeLayout::Standalone;
        spec.src_path = src;
        spec.obj_path = temp.path().join("incompatible.o");
        spec.copts = vec!["-std=c++11".into()];
        let command = spec.to_normalized_command_spec();
        assert!(command.argv.contains(&"-std=c++11".into()));
        let actual = std::process::Command::new(&command.executable)
            .args(&command.argv)
            .output()
            .unwrap();
        assert_eq!(
            actual.status.code(),
            direct.status.code(),
            "compiler refusal must be preserved: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
    }

    // Phase 5.C.4 commit 20: IncludeLayout variants exercise.
    // Pro's Q6 directive: "I prefer the enum because it makes
    // invalid combinations harder. Rust can help prevent
    // nonsense here." Tests assert:
    //   - BazelPackageRooted emits the trailing -I trio
    //     (-I<workspace_root>, -I<extra>, -I<genfiles>).
    //   - Standalone emits NO trailing -I, NO -D_GNU_SOURCE,
    //     NO linux_extras.

    #[test]
    fn include_layout_bazel_package_rooted_emits_trailing_i_trio() {
        let mut spec = compile_spec_min(false);
        spec.include_layout = IncludeLayout::BazelPackageRooted {
            workspace_root: PathBuf::from("/ws"),
            extra_workspace_paths: vec![PathBuf::from("/ws/external/foo")],
            genfiles_path: PathBuf::from("/ws/out/genfiles"),
            linux_extra_includes: Vec::new(),
        };
        let n = spec.to_normalized_command_spec();
        assert!(n.argv.iter().any(|a| a == "-I/ws"));
        assert!(n.argv.iter().any(|a| a == "-I/ws/external/foo"));
        assert!(n.argv.iter().any(|a| a == "-I/ws/out/genfiles"));
    }

    #[test]
    fn include_layout_standalone_omits_trailing_i_trio() {
        let mut spec = compile_spec_min(false);
        spec.include_layout = IncludeLayout::Standalone;
        // direct_include_paths drives the only -I emission.
        spec.direct_include_paths = vec![PathBuf::from("/ws/proto_out")];
        let n = spec.to_normalized_command_spec();
        // direct -I emitted
        assert!(n.argv.iter().any(|a| a == "-I/ws/proto_out"));
        // No workspace / extra / genfiles trailers.
        assert!(!n.argv.iter().any(|a| a == "-I/ws"));
        assert!(!n.argv.iter().any(|a| a == "-I/ws/out/genfiles"));
        // No -D_GNU_SOURCE on Linux (gated on BazelPackageRooted).
        assert!(!n.argv.iter().any(|a| a == "-D_GNU_SOURCE"));
    }

    #[test]
    fn include_layout_for_generated_cpp_uses_standalone() {
        // for_generated_cpp constructor sets IncludeLayout::Standalone;
        // verify the resulting spec lacks the package-rooted trailers and keeps
        // only standalone direct includes after host-system include demotion.
        let spec = CcCompileCommandSpec::for_generated_cpp(
            PathBuf::from("/usr/bin/g++"),
            Vec::new(),
            PathBuf::from("/ws/out/proto/greeting.pb.cc"),
            PathBuf::from("/ws/out/proto/greeting.pb.o"),
            vec![
                PathBuf::from("/ws/out/proto/greeting.pb.cc"),
                PathBuf::from("/ws/out/proto/greeting.pb.h"),
            ],
            PathBuf::new(), // genfiles_root: opt out for this standalone case
            None,
        );
        assert!(matches!(spec.include_layout, IncludeLayout::Standalone));
        let n = spec.to_normalized_command_spec();
        // Direct include: parent of the generated source. /usr/include is
        // deliberately demoted out of normalized compile argv.
        assert!(n.argv.iter().any(|a| a == "-I/ws/out/proto"));
        assert!(!n.argv.iter().any(|a| a == "-I/usr/include"));
        // No package-rooted trailers
        assert!(!n.argv.iter().any(|a| a == "-D_GNU_SOURCE"));
        assert!(!n.argv.iter().any(|a| a.starts_with("-I/ws") && !a.starts_with("-I/ws/out/proto")));
    }

    #[test]
    fn cc_archive_spec_emits_thin_archive_argv() {
        let spec = CcArchiveCommandSpec {
            mnemonic: "CcArchive".to_string(),
            executable: PathBuf::from("/usr/bin/ar"),
            cwd: None,
            lib_path: PathBuf::from("/ws/out/libfoo.a"),
            object_paths: vec![
                PathBuf::from("/ws/out/foo.o"),
                PathBuf::from("/ws/out/bar.o"),
            ],
            declared_input_paths: vec![
                PathBuf::from("/ws/out/foo.o"),
                PathBuf::from("/ws/out/bar.o"),
            ],
            declared_output_paths: vec![PathBuf::from("/ws/out/libfoo.a")],
            toolchain_identity: None,
        };
        let n = spec.to_normalized_command_spec();
        assert_eq!(
            n.argv,
            vec![
                "rcs".to_string(),
                "/ws/out/libfoo.a".to_string(),
                "--thin".to_string(),
                "/ws/out/foo.o".to_string(),
                "/ws/out/bar.o".to_string(),
            ]
        );
        assert_eq!(n.executable, PathBuf::from("/usr/bin/ar"));
    }

    #[test]
    fn cc_link_spec_objects_first_then_dep_then_linkopts_then_o() {
        let spec = CcLinkCommandSpec {
            mnemonic: "CcLink".to_string(),
            executable: PathBuf::from("/usr/bin/g++"),
            cwd: None,
            object_paths: vec![PathBuf::from("/ws/out/main.o")],
            driver_flags: vec!["-fuse-ld=mold".to_string()],
            dep_flags: vec!["/ws/out/libfoo.a".to_string()],
            linkopts: vec!["-pthread".to_string()],
            ldflags: vec!["-Wl,-z,relro".to_string()],
            bin_path: PathBuf::from("/ws/out/hello"),
            declared_input_paths: vec![
                PathBuf::from("/ws/out/main.o"),
                PathBuf::from("/ws/out/libfoo.a"),
            ],
            declared_output_paths: vec![PathBuf::from("/ws/out/hello")],
            toolchain_identity: None,
        };
        let n = spec.to_normalized_command_spec();
        assert_eq!(
            n.argv,
            vec![
                "/ws/out/main.o".to_string(),
                "-fuse-ld=mold".to_string(),
                "/ws/out/libfoo.a".to_string(),
                "-pthread".to_string(),
                "-Wl,-z,relro".to_string(),
                "-o".to_string(),
                "/ws/out/hello".to_string(),
            ]
        );
    }

    #[test]
    fn resolve_include_dot_collapses_to_pkg_dir() {
        let r = resolve_include_under_package(
            Path::new("/ws"),
            "foo/bar",
            Path::new("."),
        );
        assert_eq!(r, PathBuf::from("/ws/foo/bar"));
    }

    #[test]
    fn resolve_include_relative_joins_under_pkg() {
        let r = resolve_include_under_package(
            Path::new("/ws"),
            "foo",
            Path::new("inc"),
        );
        assert_eq!(r, PathBuf::from("/ws/foo/inc"));
    }

    #[test]
    fn resolve_include_absolute_passthrough() {
        let r = resolve_include_under_package(
            Path::new("/ws"),
            "foo",
            Path::new("/abs/inc"),
        );
        assert_eq!(r, PathBuf::from("/abs/inc"));
    }

    #[test]
    fn resolve_include_empty_package() {
        let r = resolve_include_under_package(
            Path::new("/ws"),
            "",
            Path::new("inc"),
        );
        assert_eq!(r, PathBuf::from("/ws/inc"));
    }

    // ====================================================================
    // AZF-2: shared compile-eligible-source predicate
    // ====================================================================

    /// AZF-2: real compile sources return true.
    #[test]
    fn azf2_is_compilable_source_accepts_cc_sources() {
        assert!(is_compilable_source("foo.cc"));
        assert!(is_compilable_source("foo.cpp"));
        assert!(is_compilable_source("foo.cxx"));
        assert!(is_compilable_source("foo.c++"));
        assert!(is_compilable_source("foo.c"));
        assert!(is_compilable_source("foo.s"));
        assert!(is_compilable_source("foo.S"));
        assert!(is_compilable_source("foo.CC")); // case-insensitive
    }

    /// AZF-2: headers and label-form refs return false.
    /// This is the case Pro's directive specifically targets:
    /// `platform_strings_computed.h` must NOT produce a
    /// CppCompile action in legacy spec, legacy runtime, OR
    /// scheduler analyze.
    #[test]
    fn azf2_header_in_srcs_not_compiled() {
        assert!(!is_compilable_source("platform_strings_computed.h"));
        assert!(!is_compilable_source("foo.h"));
        assert!(!is_compilable_source("foo.hpp"));
        assert!(!is_compilable_source("foo.hxx"));
        assert!(!is_compilable_source("foo.h++"));
        assert!(!is_compilable_source("foo.inc"));
        assert!(!is_compilable_source("foo.H")); // case-insensitive
    }

    /// AZF-2: label-form refs in srcs (Bazel's
    /// cross-package header idiom) are not compilable.
    /// This is the mutex case from commit 239.
    #[test]
    fn azf2_label_form_srcs_not_compilable() {
        assert!(!is_compilable_source("//tsl/platform:mutex.h"));
        assert!(!is_compilable_source("@nsync//:nsync_cv.h"));
        assert!(!is_compilable_source(":local_target"));
    }

    /// ROOT-3: `//pkg:file.cc` workspace SOURCE labels (the exports_files
    /// cross-package idiom, e.g. tsl/platform/default:env's
    /// `//xla/tsl/platform:env.cc`) ARE compilable — gated on the file
    /// component's extension. Header labels and `@repo//`/`:`-form labels stay
    /// non-compilable (the latter two are a documented ROOT-3 follow-up).
    #[test]
    fn root3_workspace_source_labels_compilable() {
        assert!(is_compilable_source("//xla/tsl/platform:env.cc"));
        assert!(is_compilable_source("//xla/tsl/platform:file_system.cc"));
        assert!(!is_compilable_source("//xla/tsl/platform:env.h"));
        // @repo// + :name source labels deferred (external-repo-root mapping):
        assert!(!is_compilable_source("@local_tsl//tsl/platform:tracing.cc"));
        assert!(!is_compilable_source(":env.cc"));
    }

    /// AZF-2: extensionless and empty paths return false
    /// (defensive default-deny).
    #[test]
    fn azf2_extensionless_paths_not_compilable() {
        assert!(!is_compilable_source(""));
        assert!(!is_compilable_source("foo"));
        assert!(!is_compilable_source("README"));
    }

    /// AZF-2: unknown extensions return false (default-deny).
    /// If a target legitimately needs an unusual extension
    /// compiled, the predicate must be updated explicitly —
    /// silently accepting unknown extensions risks accidental
    /// compilation of data files, manifests, etc.
    #[test]
    fn azf2_unknown_extensions_default_deny() {
        assert!(!is_compilable_source("foo.txt"));
        assert!(!is_compilable_source("foo.md"));
        assert!(!is_compilable_source("foo.json"));
        assert!(!is_compilable_source("foo.bzl"));
        assert!(!is_compilable_source("foo.proto"));
        assert!(!is_compilable_source("foo.def"));
    }

    /// AZF-2: full path components don't fool the predicate —
    /// the extension is what matters, regardless of directory
    /// prefix.
    #[test]
    fn azf2_full_paths_respected() {
        assert!(is_compilable_source("tsl/platform/default/mutex.cc"));
        assert!(!is_compilable_source(
            "tsl/platform/default/mutex_data.h"
        ));
        assert!(!is_compilable_source(
            "tensorflow/core/platform/platform_strings_computed.h"
        ));
    }
}
