// Bridge between rush-starlark and rush-parse
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use anyhow::{anyhow, Context, Result};
use rush_profiling::profile_scope;
use rush_starlark::{parse, BuildFileContextGuard, Evaluator, RuleCall, Value as StarlarkValue};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::{validate_unique_rule_names, BuildFile, Rule, Value};

fn debug_logging_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("RUSH_DEBUG").as_deref() == Ok("1"))
}

// Debug logging helper - only outputs if RUSH_DEBUG=1 is set
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if debug_logging_enabled() {
            eprintln!($($arg)*);
        }
    };
}

/// Get current process memory usage in MB
fn get_memory_mb() -> usize {
    if !debug_logging_enabled() {
        return 0;
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if line.starts_with("VmRSS:") {
                    if let Some(kb_str) = line.split_whitespace().nth(1) {
                        if let Ok(kb) = kb_str.parse::<usize>() {
                            return kb / 1024; // Convert KB to MB
                        }
                    }
                }
            }
        }
    }
    0 // Unsupported platform
}

/// Determine whether lazy BUILD .bzl loading should run.
///
/// Defaults to enabled (sequential BUILD parsing is the slow path). Operators
/// can disable laziness with `RUSH_LAZY_BUILD_BZLS=0/false`.
fn lazy_build_loading_enabled() -> bool {
    match std::env::var("RUSH_LAZY_BUILD_BZLS") {
        Ok(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            match normalized.as_str() {
                "" | "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                other => {
                    debug_log!(
                        "[LAZY BUILD] Unrecognized RUSH_LAZY_BUILD_BZLS='{}' (defaulting to enabled)",
                        other
                    );
                    true
                }
            }
        }
        Err(_) => true,
    }
}

/// Parse a BUILD file with a dedicated BzlLoader instance (for parallel parsing)
///
/// This is the new API for parallel BUILD parsing - each task gets its own BzlLoader
/// instance that shares the GlobalBzlCache. No Arc<Mutex> contention!
pub fn parse_build_file_with_loader(
    build_path: &Path,
    mut bzl_loader: rush_starlark::BzlLoader,
) -> Result<Arc<BuildFile>> {
    profile_scope!("parse_build_file_with_loader");
    let mem_start = get_memory_mb();
    debug_log!(
        "[MEM] 📊 BUILD parsing START: {} (baseline: {} MB)",
        build_path.display(),
        mem_start
    );

    // Persistent-daemon brick 1b (step 5c): read the BUILD file content through
    // the tracked analysis IO so its content (+ kind) is recorded as an analysis
    // dependency. Fail-loud if no `tracked::scope` is installed on this thread
    // (the orchestrator wraps the parse region / spawn-blocking boundary so it is).
    let content = rush_incremental::tracked::read_to_string(build_path)
        .with_context(|| format!("Failed to read BUILD file: {}", build_path.display()))?;

    let mem_after_read = get_memory_mb();
    debug_log!(
        "[MEM]   After read: {} MB (+{} MB)",
        mem_after_read,
        mem_after_read - mem_start
    );

    // Parse the BUILD file to AST
    let program = parse(&content)
        .with_context(|| format!("Failed to parse BUILD file: {}", build_path.display()))?;

    let mem_after_parse = get_memory_mb();
    debug_log!(
        "[MEM]   After AST parse: {} MB (+{} MB)",
        mem_after_parse,
        mem_after_parse - mem_after_read
    );

    // Create evaluator and register common BUILD rule functions
    let mut evaluator = Evaluator::new();

    // Disable parallel batch loading for BUILD files to avoid deadlocks from circular .bzl dependencies
    // WORKSPACE files keep parallel loading enabled (where it's safe and beneficial)
    evaluator.set_disable_parallel_batch_load(true);

    // RAII guard so nested .bzl loads remain sequential within this BUILD evaluation.
    let _build_context_guard = BuildFileContextGuard::enter();

    // Register native namespace for macro support
    evaluator.register_native_namespace();

    // Set .bzl loader - direct access, no lock needed!
    let mut pushed_ctx = false;
    if let Some(parent) = build_path.parent() {
        let pkg = parent.to_path_buf();
        bzl_loader.set_current_package(pkg.clone());

        // Detect which repository this BUILD file belongs to
        let workspace_root = bzl_loader.get_workspace_root().clone();
        let explicit_repo_root = bzl_loader.get_current_repo_root().clone();
        let explicit_repo_name = bzl_loader.get_current_repo_name().map(str::to_owned);
        let (detected_repo_root, repo_name) = if let Some(repo_name) = explicit_repo_name {
            // The orchestrator has already resolved the label's repository.
            // Preserve that identity even when an external alias shares the
            // main repository's path, or several names share one path.
            (explicit_repo_root, Some(repo_name))
        } else {
            let mut detected = workspace_root.clone();
            let mut best_match_len = workspace_root.as_os_str().len();
            let mut detected_name = None;

            // Find the longest matching prefix (most specific repo)
            // Lock-free iteration over DashMap
            // Canonicalize build_path to handle ".." segments which cause starts_with to fail.
            // Tracked canonicalize: each symlink hop records a Symlink dep (logic identical
            // to fs::canonicalize) so a symlinked workspace/repo path invalidates the graph.
            let check_path = rush_incremental::tracked::canonicalize(build_path)
                .unwrap_or(build_path.to_path_buf());

            debug_log!(
                "[DEBUG starlark_bridge] Checking build path: {}",
                check_path.display()
            );
            debug_log!(
                "[DEBUG starlark_bridge] External repos count: {}",
                bzl_loader.external_repos.len()
            );

            for entry in bzl_loader.external_repos.iter() {
                let path = entry.value();
                let canonical_repo_path =
                    rush_incremental::tracked::canonicalize(path).unwrap_or(path.clone());
                let is_match = check_path.starts_with(&canonical_repo_path);
                debug_log!("[DEBUG starlark_bridge] Comparing with repo {}: {} (canonical: {}) -> match: {}", entry.key(), path.display(), canonical_repo_path.display(), is_match);

                if is_match {
                    let path_len = canonical_repo_path.as_os_str().len();
                    if path_len > best_match_len {
                        detected = canonical_repo_path;
                        best_match_len = path_len;
                        detected_name = Some(entry.key().clone());
                        debug_log!(
                            "[DEBUG starlark_bridge] BUILD file {} detected in external repo: {}",
                            build_path.display(),
                            path.display()
                        );
                    }
                }
            }
            (detected, detected_name)
        };

        // Set the repo root so subsequent .bzl loads resolve correctly
        bzl_loader.set_current_repo_root(detected_repo_root.clone());

        // Set the current repository name on the evaluator
        // This ensures that load() statements in the BUILD file are resolved correctly
        // (e.g. load("//foo:bar.bzl") -> load("@repo//foo:bar.bzl"))
        debug_log!("[DEBUG starlark_bridge] repo_name: {:?}", repo_name);
        if let Some(name) = repo_name {
            debug_log!(
                "[DEBUG starlark_bridge] Setting current_repository to: {}",
                name
            );
            evaluator.set_current_repository(Some(name));
        } else {
            debug_log!("[DEBUG starlark_bridge] repo_name is None, not setting current_repository");
        }

        // Set the evaluator's current_package (REPO-RELATIVE, e.g.
        // "third_party/icu/data") so native.package_name() and — critically —
        // package-relative glob() resolve correctly. Previously only the
        // bzl_loader got the package; the evaluator's stayed "", which is why
        // glob() was package-blind and a subpackage glob like
        // glob(["icu_conversion_data.c.gz.*"]) returned 0 files. Canonicalize
        // both sides so the strip_prefix is robust to "."/".." segments.
        let rel_pkg = {
            let canon_pkg =
                rush_incremental::tracked::canonicalize(&pkg).unwrap_or_else(|_| pkg.clone());
            let canon_root = rush_incremental::tracked::canonicalize(&detected_repo_root)
                .unwrap_or_else(|_| detected_repo_root.clone());
            canon_pkg
                .strip_prefix(&canon_root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default()
        };
        evaluator.set_current_package(rel_pkg);

        // #39 graph-RSS: give the evaluator the package's ABSOLUTE on-disk dir
        // so external-repo glob() scans the real package tree instead of a
        // CWD-relative scan from the workspace root (which followed
        // bazel-*/.cache symlink forests and OOM'd on @local_xla//xla's
        // glob(["**/*.cc"])). `pkg` is the dir containing this BUILD file.
        evaluator.set_current_package_dir(Some(pkg.clone()));

        // Push thread-local context for this BUILD evaluation
        rush_starlark::BzlLoader::push_thread_context(detected_repo_root, pkg);
        pushed_ctx = true;
    }

    // Wrap in Arc<Mutex> only for evaluator compatibility
    let loader_arc = std::sync::Arc::new(std::sync::Mutex::new(bzl_loader));
    evaluator.set_bzl_loader(loader_arc);

    // Register common C++ rules
    evaluator.register_rule_function("cc_library".to_string());
    evaluator.register_rule_function("cc_binary".to_string());
    evaluator.register_rule_function("cc_test".to_string());
    // FIX #200: Register cc_import for precompiled libraries
    evaluator.register_rule_function("cc_import".to_string());

    // Register TensorFlow-specific rules
    evaluator.register_rule_function("tf_cc_library".to_string());
    evaluator.register_rule_function("tf_cc_binary".to_string());
    evaluator.register_rule_function("tf_cc_test".to_string());
    evaluator.register_rule_function("tf_gen_op_wrappers_cc".to_string());
    evaluator.register_rule_function("tf_gen_op_libs".to_string());
    evaluator.register_rule_function("tf_cc_shared_object".to_string());
    evaluator.register_rule_function("tf_cc_shared_library".to_string());
    evaluator.register_rule_function("cc_shared_library".to_string());
    evaluator.register_rule_function("tf_cuda_library".to_string());
    evaluator.register_rule_function("tf_gpu_cc_test".to_string());
    // FIX #201: Register cc_header_only_library for header-only targets
    evaluator.register_rule_function("cc_header_only_library".to_string());

    // Register protocol buffer rules
    evaluator.register_rule_function("proto_library".to_string());
    evaluator.register_rule_function("cc_proto_library".to_string());
    evaluator.register_rule_function("java_proto_library".to_string());
    evaluator.register_rule_function("java_lite_proto_library".to_string());
    evaluator.register_rule_function("java_library".to_string());
    evaluator.register_rule_function("java_binary".to_string());
    evaluator.register_rule_function("java_import".to_string());
    evaluator.register_rule_function("java_plugin".to_string());
    evaluator.register_rule_function("java_test".to_string());
    // FIX #182: Register TensorFlow proto rule (removed from builtins)
    evaluator.register_rule_function("tf_proto_library".to_string());

    // Register Bazel utility rules
    evaluator.register_rule_function("filegroup".to_string());
    evaluator.register_rule_function("genrule".to_string());
    evaluator.register_rule_function("alias".to_string());
    evaluator.register_rule_function("config_setting".to_string());

    // Register Python rules (FIX #179: needed for TensorFlow Python targets)
    evaluator.register_rule_function("py_binary".to_string());
    evaluator.register_rule_function("py_library".to_string());
    evaluator.register_rule_function("py_test".to_string());

    let mem_before_eval = get_memory_mb();
    debug_log!(
        "[MEM]   After evaluator setup: {} MB (+{} MB)",
        mem_before_eval,
        mem_before_eval - mem_after_parse
    );

    // Evaluate the BUILD file
    let use_lazy_loading = lazy_build_loading_enabled();

    eprintln!(
        "⏳ [EVAL START] Evaluating BUILD file: {}",
        build_path.display()
    );
    eprintln!("   Lazy loading: {}", use_lazy_loading);
    let eval_start_time = std::time::Instant::now();

    let eval_result = if use_lazy_loading {
        debug_log!("[LAZY BUILD] Using lazy .bzl loading (set RUSH_LAZY_BUILD_BZLS=0 to disable)");
        evaluator.eval_program_lazy_loads(&program)
    } else {
        debug_log!("[LAZY BUILD] Lazy BUILD loading disabled via RUSH_LAZY_BUILD_BZLS=0");
        evaluator.eval_program(&program)
    };

    let eval_duration = eval_start_time.elapsed();
    eprintln!(
        "✅ [EVAL DONE] BUILD evaluation completed in {:.2}s",
        eval_duration.as_secs_f64()
    );

    if let Err(e) = eval_result {
        debug_log!("[ERROR bzl_loader] BUILD evaluation failed: {:?}", e);
        if pushed_ctx {
            rush_starlark::BzlLoader::pop_thread_context();
        }
        return Err(e)
            .with_context(|| format!("Failed to evaluate BUILD file: {}", build_path.display()));
    }

    // Pop thread context after successful evaluation
    if pushed_ctx {
        rush_starlark::BzlLoader::pop_thread_context();
    }

    let mem_after_eval = get_memory_mb();
    debug_log!(
        "[MEM]   After evaluation: {} MB (+{} MB) ⚠️",
        mem_after_eval,
        mem_after_eval - mem_before_eval
    );

    // Extract rules from captured rule calls
    let rules = extract_rules(&evaluator)?;
    validate_unique_rule_names(&rules, build_path)?;

    let mem_after_extract = get_memory_mb();
    debug_log!(
        "[MEM]   After extract {} rules: {} MB (+{} MB)",
        rules.len(),
        mem_after_extract,
        mem_after_extract - mem_after_eval
    );

    let build_file = Arc::new(BuildFile {
        path: build_path.to_path_buf(),
        rules,
    });

    let mem_final = get_memory_mb();
    let mem_total_delta = mem_final - mem_start;
    debug_log!(
        "[MEM] 📊 BUILD parsing COMPLETE: {} MB total (+{} MB from start)",
        mem_final,
        mem_total_delta
    );
    if mem_total_delta > 100 {
        debug_log!(
            "[MEM] ⚠️  WARNING: Large memory increase ({} MB) for BUILD file!",
            mem_total_delta
        );
    }

    Ok(build_file)
}

/// Parse a BUILD file using the full rush-starlark implementation with .bzl loader support
///
/// DEPRECATED: Use parse_build_file_with_loader() for better parallel performance.
/// This function wraps BzlLoader in Arc<Mutex> which causes serialization.
pub fn parse_build_file_with_bzl_loader(
    build_path: &Path,
    bzl_loader: Option<std::sync::Arc<std::sync::Mutex<rush_starlark::BzlLoader>>>,
) -> Result<Arc<BuildFile>> {
    let mem_start = get_memory_mb();
    debug_log!(
        "[MEM] 📊 BUILD parsing START: {} (baseline: {} MB)",
        build_path.display(),
        mem_start
    );

    let content = std::fs::read_to_string(build_path)
        .with_context(|| format!("Failed to read BUILD file: {}", build_path.display()))?;

    let mem_after_read = get_memory_mb();
    debug_log!(
        "[MEM]   After read: {} MB (+{} MB)",
        mem_after_read,
        mem_after_read - mem_start
    );

    // Parse the BUILD file to AST
    let program = parse(&content)
        .with_context(|| format!("Failed to parse BUILD file: {}", build_path.display()))?;

    let mem_after_parse = get_memory_mb();
    debug_log!(
        "[MEM]   After AST parse: {} MB (+{} MB)",
        mem_after_parse,
        mem_after_parse - mem_after_read
    );

    // Create evaluator and register common BUILD rule functions
    let mut evaluator = Evaluator::new();

    // Disable parallel batch loading for BUILD files to avoid deadlocks from circular .bzl dependencies
    // WORKSPACE files keep parallel loading enabled (where it's safe and beneficial)
    evaluator.set_disable_parallel_batch_load(true);

    // RAII guard so nested .bzl loads remain sequential within this BUILD evaluation.
    let _build_context_guard = BuildFileContextGuard::enter();

    // Register native namespace for macro support
    evaluator.register_native_namespace();

    // Set .bzl loader if provided
    // Also push a per-thread context so that // and : label resolution during
    // BUILD evaluation uses the correct repository/package roots without
    // relying on shared mutable state.
    let mut pushed_ctx = false;
    if let Some(loader) = bzl_loader {
        let (repo_root, package_dir) = if let Some(parent) = build_path.parent() {
            let mut loader_guard = loader
                .lock()
                .map_err(|e| anyhow::anyhow!("Failed to lock .bzl loader: {}", e))?;
            let pkg = parent.to_path_buf();
            loader_guard.set_current_package(pkg.clone());

            // Detect which repository this BUILD file belongs to
            // Check if build_path is inside any external repository
            // Find the MOST SPECIFIC (longest) matching path to handle nested repos
            let workspace_root = loader_guard.get_workspace_root().clone();
            let detected_repo_root = {
                let mut detected = workspace_root.clone();
                let mut best_match_len = workspace_root.as_os_str().len();

                // Find the longest matching prefix (most specific repo)
                // Lock-free iteration over DashMap
                for entry in loader_guard.external_repos.iter() {
                    let path = entry.value();
                    if build_path.starts_with(path) {
                        let path_len = path.as_os_str().len();
                        if path_len > best_match_len {
                            detected = path.clone();
                            best_match_len = path_len;
                            debug_log!(
                                "[DEBUG starlark_bridge] BUILD file {} detected in external repo: {}",
                                build_path.display(),
                                path.display()
                            );
                        }
                    }
                }
                detected
            };

            // Set the repo root so subsequent .bzl loads resolve correctly
            loader_guard.set_current_repo_root(detected_repo_root.clone());

            (Some(detected_repo_root), Some(pkg))
        } else {
            (None, None)
        };

        // Push thread-local context for this BUILD evaluation
        if let (Some(r), Some(p)) = (repo_root, package_dir) {
            rush_starlark::BzlLoader::push_thread_context(r, p);
            pushed_ctx = true;
        }

        evaluator.set_bzl_loader(loader);
    }

    // Register common C++ rules
    evaluator.register_rule_function("cc_library".to_string());
    evaluator.register_rule_function("cc_binary".to_string());
    evaluator.register_rule_function("cc_test".to_string());

    // Register TensorFlow-specific rules
    evaluator.register_rule_function("tf_cc_library".to_string());
    evaluator.register_rule_function("tf_cc_binary".to_string());
    evaluator.register_rule_function("tf_cc_test".to_string());
    evaluator.register_rule_function("tf_gen_op_wrappers_cc".to_string());
    evaluator.register_rule_function("tf_gen_op_libs".to_string());
    evaluator.register_rule_function("tf_cc_shared_object".to_string());
    evaluator.register_rule_function("tf_cc_shared_library".to_string());
    evaluator.register_rule_function("cc_shared_library".to_string());
    evaluator.register_rule_function("tf_cuda_library".to_string());
    evaluator.register_rule_function("tf_gpu_cc_test".to_string());

    // Register protocol buffer rules
    evaluator.register_rule_function("proto_library".to_string());
    evaluator.register_rule_function("cc_proto_library".to_string());
    evaluator.register_rule_function("java_proto_library".to_string());
    evaluator.register_rule_function("java_lite_proto_library".to_string());
    evaluator.register_rule_function("java_library".to_string());
    evaluator.register_rule_function("java_binary".to_string());
    evaluator.register_rule_function("java_import".to_string());
    evaluator.register_rule_function("java_plugin".to_string());
    evaluator.register_rule_function("java_test".to_string());
    // FIX #182: Register TensorFlow proto rule (removed from builtins)
    evaluator.register_rule_function("tf_proto_library".to_string());

    // Register Bazel utility rules
    evaluator.register_rule_function("filegroup".to_string());
    evaluator.register_rule_function("genrule".to_string());
    evaluator.register_rule_function("alias".to_string());
    evaluator.register_rule_function("config_setting".to_string());

    // Register Python rules (FIX #179: needed for TensorFlow Python targets)
    evaluator.register_rule_function("py_binary".to_string());
    evaluator.register_rule_function("py_library".to_string());
    evaluator.register_rule_function("py_test".to_string());

    // Note: exports_files, package, and licenses are NOT user rule
    // functions. exports_files emits internal source_file targets, while
    // package records rule defaults and licenses remain metadata-only.

    let mem_before_eval = get_memory_mb();
    debug_log!(
        "[MEM]   After evaluator setup: {} MB (+{} MB)",
        mem_before_eval,
        mem_before_eval - mem_after_parse
    );

    // Evaluate the BUILD file
    // Use lazy loading by default to skip unused .bzl files (opt-out via env)
    let use_lazy_loading = lazy_build_loading_enabled();

    let eval_result = if use_lazy_loading {
        debug_log!("[LAZY BUILD] Using lazy .bzl loading (set RUSH_LAZY_BUILD_BZLS=0 to disable)");
        evaluator.eval_program_lazy_loads(&program)
    } else {
        debug_log!("[LAZY BUILD] Lazy BUILD loading disabled via RUSH_LAZY_BUILD_BZLS=0");
        evaluator.eval_program(&program)
    };

    if let Err(e) = eval_result {
        debug_log!("[ERROR bzl_loader] BUILD evaluation failed: {:?}", e);
        // Pop thread context before returning
        if pushed_ctx {
            rush_starlark::BzlLoader::pop_thread_context();
        }
        return Err(e)
            .with_context(|| format!("Failed to evaluate BUILD file: {}", build_path.display()));
    }

    // Pop thread context after successful evaluation
    if pushed_ctx {
        rush_starlark::BzlLoader::pop_thread_context();
    }

    let mem_after_eval = get_memory_mb();
    debug_log!(
        "[MEM]   After evaluation: {} MB (+{} MB) ⚠️",
        mem_after_eval,
        mem_after_eval - mem_before_eval
    );

    // Extract rules from captured rule calls
    let rules = extract_rules(&evaluator)?;
    validate_unique_rule_names(&rules, build_path)?;

    let mem_after_extract = get_memory_mb();
    debug_log!(
        "[MEM]   After extract {} rules: {} MB (+{} MB)",
        rules.len(),
        mem_after_extract,
        mem_after_extract - mem_after_eval
    );

    let build_file = Arc::new(BuildFile {
        path: build_path.to_path_buf(),
        rules,
    });

    let mem_final = get_memory_mb();
    let mem_total_delta = mem_final - mem_start;
    debug_log!(
        "[MEM] 📊 BUILD parsing COMPLETE: {} MB total (+{} MB from start)",
        mem_final,
        mem_total_delta
    );
    if mem_total_delta > 100 {
        debug_log!(
            "[MEM] ⚠️  WARNING: Large memory increase ({} MB) for BUILD file!",
            mem_total_delta
        );
    }

    Ok(build_file)
}

/// Parse a BUILD file using the full rush-starlark implementation (without .bzl loader)
///
/// This is a convenience wrapper around parse_build_file_with_bzl_loader
pub fn parse_build_file_with_starlark(build_path: &Path) -> Result<Arc<BuildFile>> {
    parse_build_file_with_bzl_loader(build_path, None)
}

/// Parse a BUILD file lazily for a specific target
///
/// This function uses lazy evaluation to extract only the requested target
/// without loading all .bzl files eagerly. This dramatically reduces memory
/// consumption and time-to-first-action for large projects like TensorFlow.
///
/// # Arguments
/// * `build_path` - Path to the BUILD file
/// * `target_name` - Name of the target to build (e.g., "build_pip_package")
///
/// # Returns
/// A BuildFile containing only the requested target (initially)
///
/// # Example
/// ```no_run
/// use rush_parse::starlark_bridge::parse_build_file_lazy;
/// use std::path::Path;
///
/// let build_file = parse_build_file_lazy(
///     Path::new("BUILD"),
///     "my_target"
/// ).unwrap();
/// ```
pub fn parse_build_file_lazy(build_path: &Path, target_name: &str) -> Result<Arc<BuildFile>> {
    debug_log!(
        "[LAZY] Lazy parsing BUILD file: {} for target: {}",
        build_path.display(),
        target_name
    );

    // Read BUILD file
    let content = std::fs::read_to_string(build_path)
        .with_context(|| format!("Failed to read BUILD file: {}", build_path.display()))?;

    // Parse to AST only (don't evaluate yet)
    debug_log!("[LAZY] Parsing BUILD file to AST (no evaluation)");
    let program = parse(&content)
        .with_context(|| format!("Failed to parse BUILD file: {}", build_path.display()))?;

    // Use LazyBuildAnalysis to extract target without loading .bzl files
    debug_log!("[LAZY] Analyzing BUILD file for target: {}", target_name);
    let analysis = rush_starlark::LazyBuildAnalysis::analyze(build_path, target_name, &program)?;

    debug_log!("[LAZY] Analysis complete:");
    debug_log!("[LAZY]   - Found target: {}", analysis.target);
    debug_log!(
        "[LAZY]   - Immediate actions: {}",
        analysis.immediate_actions.len()
    );
    debug_log!(
        "[LAZY]   - Required .bzl files: {}",
        analysis.required_bzl_symbols.len()
    );
    debug_log!("[LAZY]   - Pending deps: {}", analysis.pending_deps.len());

    // For now, we'll do a simplified conversion:
    // Create a partial BuildFile with just the target definition
    // In a full implementation, this would integrate with WorkFirstOrchestrator

    // Convert target definition to Rule
    let rules = if let Some(target_def) = analysis.target_definition {
        vec![convert_target_definition_to_rule(&target_def)?]
    } else {
        vec![]
    };

    debug_log!(
        "[LAZY] Lazy parsing complete - returning {} rules",
        rules.len()
    );

    Ok(Arc::new(BuildFile {
        path: build_path.to_path_buf(),
        rules,
    }))
}

/// Convert a TargetDefinition to a Rule
fn convert_target_definition_to_rule(target_def: &rush_starlark::TargetDefinition) -> Result<Rule> {
    let mut attributes = HashMap::new();

    // Convert all attributes from Expr to Value
    for (key, expr) in &target_def.attributes {
        attributes.insert(key.clone(), convert_expr_to_value(expr)?);
    }

    // Check if this is a macro that needs expansion
    let (final_rule_type, final_attributes) =
        expand_macro_if_needed(&target_def.rule_type, &target_def.name, attributes)?;

    Ok(Rule {
        name: target_def.name.clone(),
        rule_type: final_rule_type,
        attributes: final_attributes,
    })
}

/// Expand macros during lazy parsing
///
/// This handles TensorFlow macros like tf_custom_op_library that add implicit dependencies.
fn expand_macro_if_needed(
    rule_type: &str,
    _name: &str,
    mut attributes: HashMap<String, Value>,
) -> Result<(String, HashMap<String, Value>)> {
    match rule_type {
        "tf_custom_op_library" => {
            debug_log!("[MACRO LAZY] Expanding tf_custom_op_library during lazy parsing");

            // Add implicit TensorFlow dependencies
            let mut deps = match attributes.get("deps") {
                Some(Value::List(d)) => d.clone(),
                _ => Vec::new(),
            };

            // Add framework headers, protobuf, and Eigen
            deps.push(Value::String(
                "//tensorflow/core:framework_headers_lib".to_string(),
            ));
            deps.push(Value::String(
                "@com_google_protobuf//:protobuf_headers".to_string(),
            ));
            deps.push(Value::String("@eigen_archive//:eigen3".to_string()));

            attributes.insert("deps".to_string(), Value::List(deps));

            // Add -fPIC for shared library
            let mut copts = match attributes.get("copts") {
                Some(Value::List(c)) => c.clone(),
                _ => Vec::new(),
            };
            if !copts
                .iter()
                .any(|v| matches!(v, Value::String(s) if s == "-fPIC"))
            {
                copts.push(Value::String("-fPIC".to_string()));
            }
            attributes.insert("copts".to_string(), Value::List(copts));

            // Add -shared for shared library
            let mut linkopts = match attributes.get("linkopts") {
                Some(Value::List(l)) => l.clone(),
                _ => Vec::new(),
            };
            if !linkopts
                .iter()
                .any(|v| matches!(v, Value::String(s) if s == "-shared"))
            {
                linkopts.push(Value::String("-shared".to_string()));
            }
            attributes.insert("linkopts".to_string(), Value::List(linkopts));

            debug_log!(
                "[MACRO LAZY] Expanded tf_custom_op_library -> cc_binary with {} deps",
                match attributes.get("deps") {
                    Some(Value::List(d)) => d.len(),
                    _ => 0,
                }
            );

            // Transform into cc_binary
            Ok(("cc_binary".to_string(), attributes))
        }
        "cc_header_only_library" => {
            debug_log!("[MACRO LAZY] Expanding cc_header_only_library during lazy parsing");

            // Merge deps and extra_deps
            let mut all_deps = match attributes.get("deps") {
                Some(Value::List(d)) => d.clone(),
                _ => Vec::new(),
            };

            // Add extra_deps if present
            if let Some(Value::List(extra_deps)) = attributes.get("extra_deps") {
                all_deps.extend(extra_deps.clone());
            }

            // Remove extra_deps (not a standard cc_library attribute)
            attributes.remove("extra_deps");

            // Update deps with merged list
            if !all_deps.is_empty() {
                attributes.insert("deps".to_string(), Value::List(all_deps));
            }

            // Remove srcs to ensure header-only library
            attributes.remove("srcs");

            debug_log!(
                "[MACRO LAZY] Expanded cc_header_only_library -> cc_library with {} deps",
                match attributes.get("deps") {
                    Some(Value::List(d)) => d.len(),
                    _ => 0,
                }
            );

            // Transform into cc_library (header-only)
            Ok(("cc_library".to_string(), attributes))
        }
        // Other macros can be added here
        _ => {
            // Not a macro, return as-is
            Ok((rule_type.to_string(), attributes))
        }
    }
}

/// Convert a rush-starlark Expr to a rush-parse Value
fn convert_expr_to_value(expr: &rush_starlark::Expr) -> Result<Value> {
    use rush_starlark::Expr;

    match expr {
        Expr::String(s) => Ok(Value::String(s.clone())),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::List(items) => {
            let converted: Result<Vec<_>> = items.iter().map(convert_expr_to_value).collect();
            Ok(Value::List(converted?))
        }
        Expr::Int(n) => Ok(Value::String(n.to_string())),
        _ => {
            // For now, convert unknown expressions to string representation
            Ok(Value::String(format!("{:?}", expr)))
        }
    }
}

/// Extract rule definitions from evaluated Starlark environment
fn extract_rules(evaluator: &Evaluator) -> Result<Vec<Rule>> {
    profile_scope!("extract_rules");
    let mut rules = Vec::new();

    // Convert captured rule calls to Rule structs
    for rule_call in evaluator.rule_calls() {
        let rule = convert_rule_call(rule_call)?;
        rules.push(rule);
    }

    Ok(rules)
}

/// Convert a RuleCall to a Rule
fn convert_rule_call(rule_call: &RuleCall) -> Result<Rule> {
    let mut attributes = HashMap::new();

    // Debug: Log kwargs keys
    debug_log!(
        "[DEBUG convert_rule_call] Rule type: {}",
        rule_call.rule_type
    );
    debug_log!(
        "[DEBUG convert_rule_call] kwargs keys: {:?}",
        rule_call.kwargs.keys().collect::<Vec<_>>()
    );

    // Extract name attribute (required for all rules)
    let name = rule_call
        .kwargs
        .get("name")
        .ok_or_else(|| {
            debug_log!(
                "[ERROR] Rule {} missing 'name' attribute. Available keys: {:?}",
                rule_call.rule_type,
                rule_call.kwargs.keys().collect::<Vec<_>>()
            );
            anyhow::anyhow!(
                "Rule {} missing required 'name' attribute",
                rule_call.rule_type
            )
        })?
        .to_string()
        .trim_matches('"')
        .to_string();

    // Convert all kwargs to attributes
    for (key, value) in &rule_call.kwargs {
        let converted = if rule_call.rule_type == "expand_template" && key == "substitutions" {
            convert_expand_template_substitutions(value)?
        } else if rule_call.rule_type == "config_setting" && key == "flag_values" {
            convert_config_setting_flag_values(value)?
        } else if (rule_call.rule_type == "config_setting"
            && matches!(
                key.as_str(),
                "define_values" | "values" | "constraint_values"
            )) || key == "build_setting_default"
        {
            // These attributes feed configuration matching again on a cached
            // BUILD replay. Preserve their exact scalar/container types; the
            // generic legacy bridge deliberately summarizes ordinary dicts.
            convert_configurable_value(value)?
        } else if key == "visibility" && matches!(value, StarlarkValue::None) {
            // The generic legacy bridge renders None as the string "None".
            // Visibility needs the native value so the graph can distinguish
            // visibility = None from a label named "None".
            Value::None
        } else if rule_call.rule_type == "cc_binary" && key == "linkshared" {
            // Generic scalar integers intentionally remain string summaries in
            // the legacy bridge. `linkshared` is a typed bool attribute for
            // which Bazel also accepts 0/1, so preserve a literal integer here
            // without making quoted strings or unrelated attributes native.
            match value {
                StarlarkValue::Int(value) => Value::Int(*value),
                _ => convert_value(value)?,
            }
        } else {
            convert_value(value)?
        };
        attributes.insert(key.clone(), converted);
    }

    Ok(Rule {
        name,
        rule_type: rule_call.rule_type.clone(),
        attributes,
    })
}

fn convert_expand_template_substitutions(value: &StarlarkValue) -> Result<Value> {
    if let StarlarkValue::Configurable { op, left, right } = value {
        return Ok(Value::Configurable {
            op: *op,
            left: Box::new(convert_expand_template_substitutions(left)?),
            right: Box::new(convert_expand_template_substitutions(right)?),
        });
    }

    if let StarlarkValue::Select(conditions) = value {
        let converted = conditions
            .iter()
            .map(|(condition, branch)| {
                Ok((
                    condition.clone(),
                    convert_expand_template_substitutions(branch)?,
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        return Ok(Value::Select(converted));
    }

    let substitutions = match value {
        StarlarkValue::Dict(values) => values,
        other => {
            return Err(anyhow::anyhow!(
                "expand_template substitutions must be a string dictionary, got {}",
                other.to_string()
            ))
        }
    };

    let mut entries = Vec::with_capacity(substitutions.len());
    for (key, value) in substitutions {
        let StarlarkValue::String(value) = value else {
            return Err(anyhow::anyhow!(
                "expand_template substitution for {:?} must be a string",
                key
            ));
        };
        entries.push(Value::List(vec![
            Value::String(key.clone()),
            Value::String(value.clone()),
        ]));
    }
    entries.sort_by(|left, right| match (left, right) {
        (Value::List(left), Value::List(right)) => {
            format!("{:?}", left.first()).cmp(&format!("{:?}", right.first()))
        }
        _ => std::cmp::Ordering::Equal,
    });
    Ok(Value::List(entries))
}

/// #39 graph-RSS: bounded one-line rendering of an already-converted attr
/// `Value`, used to summarize non-`select` dict attributes without the
/// unbounded `Debug` of Starlark function closures. Nested collections are
/// summarized by length rather than expanded, keeping output O(entries).
fn render_converted_short(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Int(value) => value.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::List(items) => format!("[{} items]", items.len()),
        Value::Dict(items) => format!("{{{} entries}}", items.len()),
        Value::None => "None".to_string(),
        Value::Select(m) => format!("select({} conditions)", m.len()),
        Value::Configurable { .. } => "<configurable>".to_string(),
    }
}

/// Preserve native operand types while a compact configurable expression is
/// still waiting for target configuration. The ordinary legacy conversion
/// intentionally summarizes generic dictionaries and scalar integers as
/// strings, but doing that inside `select(...) + ...` changes integer addition
/// into string concatenation and makes valid dictionary unions fail.
fn convert_configurable_value(value: &StarlarkValue) -> Result<Value> {
    match value {
        StarlarkValue::String(value) => Ok(Value::String(value.clone())),
        StarlarkValue::Int(value) => Ok(Value::Int(*value)),
        StarlarkValue::Bool(value) => Ok(Value::Bool(*value)),
        StarlarkValue::None => Ok(Value::None),
        StarlarkValue::List(values) | StarlarkValue::Tuple(values) => Ok(Value::List(
            values
                .iter()
                .map(convert_configurable_value)
                .collect::<Result<Vec<_>>>()?,
        )),
        StarlarkValue::Dict(values) => Ok(Value::Dict(
            values
                .iter()
                .map(|(key, value)| {
                    Ok((key.clone(), convert_configurable_value(value)?))
                })
                .collect::<Result<HashMap<_, _>>>()?,
        )),
        StarlarkValue::Select(values) => Ok(Value::Select(
            values
                .iter()
                .map(|(condition, value)| {
                    Ok((condition.clone(), convert_configurable_value(value)?))
                })
                .collect::<Result<HashMap<_, _>>>()?,
        )),
        StarlarkValue::Configurable { op, left, right } => Ok(Value::Configurable {
            op: *op,
            left: Box::new(convert_configurable_value(left)?),
            right: Box::new(convert_configurable_value(right)?),
        }),
        StarlarkValue::Float(_) => Err(anyhow!(
            "configurable float operands are not supported by the legacy parse bridge"
        )),
        // These values are not valid arithmetic/union operands, but retaining
        // the existing bounded markers keeps error rendering finite if one
        // reaches this bridge after evaluator-side validation.
        other => convert_value(other),
    }
}

fn convert_config_setting_flag_values(value: &StarlarkValue) -> Result<Value> {
    let StarlarkValue::Dict(values) = value else {
        return convert_configurable_value(value);
    };
    Ok(Value::Dict(
        values
            .iter()
            .map(|(key, value)| {
                let key = rush_starlark::evaluator::configuration_label_dictionary_key(key)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "config_setting.flag_values key must be a string or Label"
                        )
                    })?;
                Ok((key, convert_configurable_value(value)?))
            })
            .collect::<Result<HashMap<_, _>>>()?,
    ))
}

/// Convert rush-starlark Value to rush-parse Value
fn convert_value(value: &StarlarkValue) -> Result<Value> {
    match value {
        StarlarkValue::String(s) => Ok(Value::String(s.clone())),
        StarlarkValue::Bool(b) => Ok(Value::Bool(*b)),
        StarlarkValue::List(items) | StarlarkValue::Tuple(items) => {
            let converted: Result<Vec<_>> = items.iter().map(convert_value).collect();
            Ok(Value::List(converted?))
        }
        StarlarkValue::Dict(map) => {
            // Check if this is a select() expression
            // In Bazel/Starlark, select() returns a dict with condition keys
            if is_select_dict(map) {
                let mut select_map = HashMap::new();
                for (key, val) in map {
                    select_map.insert(key.clone(), convert_value(val)?);
                }
                Ok(Value::Select(select_map))
            } else {
                // #39 graph-RSS / OOM fix: do NOT `format!("{:?}", map)` here.
                // A dict attribute value can be a Starlark `Function`, whose
                // `Debug` recursively renders its body AST *and* its
                // `closure: Arc<Environment>` — i.e. the whole module-globals
                // map, transitively through every nested function's closure.
                // That yields effectively unbounded strings: instantiating
                // tensorflow/core/platform's BUILD this way grew rush's
                // analyze-only RSS to 87GB and OOM-killed the 91GB host
                // (located via per-dep RSS markers, commit 293). Render a
                // bounded key=value summary instead, routing each value through
                // `convert_value` (which already collapses
                // functions/namespaces/providers/labels to short markers).
                // Faithful for the scalar/string entries rules actually read;
                // bounded for the rest. Keys sorted for deterministic output.
                let mut parts: Vec<String> = Vec::with_capacity(map.len());
                for (k, v) in map {
                    let rendered = match convert_value(v) {
                        Ok(cv) => render_converted_short(&cv),
                        Err(_) => "<unconvertible>".to_string(),
                    };
                    parts.push(format!("{}: {}", k, rendered));
                }
                parts.sort();
                Ok(Value::String(format!("{{{}}}", parts.join(", "))))
            }
        }
        StarlarkValue::Int(n) => Ok(Value::String(n.to_string())),
        StarlarkValue::Float(f) => Ok(Value::String(f.to_string())),
        StarlarkValue::None => Ok(Value::String("None".to_string())),
        StarlarkValue::Function { .. } => Ok(Value::String("<function>".to_string())),
        StarlarkValue::Namespace(_) | StarlarkValue::WorkspaceNativeNamespace { .. } => {
            Ok(Value::String("<namespace>".to_string()))
        }
        StarlarkValue::RuleFunction(name) => Ok(Value::String(format!("<rule:{}>", name))),
        StarlarkValue::NativeFunction(name) => Ok(Value::String(format!("<native:{}>", name))),
        StarlarkValue::Provider { name, .. } => Ok(Value::String(format!("<provider:{}>", name))),
        StarlarkValue::ProviderInstance { provider_name, .. } => {
            Ok(Value::String(format!("<{} instance>", provider_name)))
        }
        StarlarkValue::Select(map) => {
            let mut select_map = HashMap::new();
            for (key, val) in map {
                select_map.insert(key.clone(), convert_value(val)?);
            }
            Ok(Value::Select(select_map))
        }
        StarlarkValue::Configurable { .. } => convert_configurable_value(value),
        StarlarkValue::BoundMethod { object, method } => Ok(Value::String(format!(
            "<bound method {}.{}>",
            object.to_string(),
            method
        ))),
        StarlarkValue::Label { label, .. } => Ok(Value::String(label.clone())),
        StarlarkValue::LazyLoad { bzl_label, symbol_name } => {
            Err(anyhow!(
                "Internal error: LazyLoad value for symbol '{}' from '{}' should have been resolved before use",
                symbol_name, bzl_label
            ))
        }
        StarlarkValue::Lambda { .. } => Ok(Value::String("<lambda>".to_string())),
    }
}

/// Check if a dict represents a select() expression
fn is_select_dict(map: &HashMap<String, StarlarkValue>) -> bool {
    // A select dict commonly includes the reserved default condition. Other
    // branches can be ordinary config_setting or constraint_value labels.
    map.keys().any(|k| k.starts_with("//conditions:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_convert_string() {
        let value = StarlarkValue::String("test".to_string());
        let converted = convert_value(&value).unwrap();
        matches!(converted, Value::String(s) if s == "test");
    }

    #[test]
    fn test_convert_bool() {
        let value = StarlarkValue::Bool(true);
        let converted = convert_value(&value).unwrap();
        matches!(converted, Value::Bool(true));
    }

    #[test]
    fn tuple_attributes_keep_existing_sequence_conversion() {
        let tuple = StarlarkValue::Tuple(vec![
            StarlarkValue::String("a.cc".to_string()),
            StarlarkValue::Tuple(vec![StarlarkValue::String("nested.h".to_string())]),
        ]);
        let Value::List(items) = convert_value(&tuple).unwrap() else {
            panic!("expected sequence attribute")
        };
        assert!(matches!(&items[0], Value::String(value) if value == "a.cc"));
        assert!(
            matches!(&items[1], Value::List(nested) if matches!(&nested[0], Value::String(value) if value == "nested.h"))
        );
        let Value::List(items) = convert_configurable_value(&StarlarkValue::Tuple(vec![
            StarlarkValue::Int(7),
            StarlarkValue::Tuple(vec![StarlarkValue::Bool(true)]),
        ]))
        .unwrap() else {
            panic!("expected configurable sequence attribute")
        };
        assert!(matches!(items[0], Value::Int(7)));
        assert!(matches!(&items[1], Value::List(nested) if matches!(nested[0], Value::Bool(true))));
    }

    #[test]
    fn test_convert_list() {
        let value = StarlarkValue::List(vec![
            StarlarkValue::String("a".to_string()),
            StarlarkValue::String("b".to_string()),
        ]);
        let converted = convert_value(&value).unwrap();
        matches!(converted, Value::List(_));
    }

    #[test]
    fn configuration_rule_conversion_preserves_predicate_types() {
        let rule_call = RuleCall {
            rule_type: "config_setting".to_string(),
            args: Vec::new(),
            kwargs: HashMap::from([
                (
                    "name".to_string(),
                    StarlarkValue::String("host".to_string()),
                ),
                (
                    "values".to_string(),
                    StarlarkValue::Dict(HashMap::from([(
                        "cpu".to_string(),
                        StarlarkValue::String("k8".to_string()),
                    )])),
                ),
                (
                    "constraint_values".to_string(),
                    StarlarkValue::List(vec![StarlarkValue::String(
                        "@platforms//os:linux".to_string(),
                    )]),
                ),
            ]),
        };

        let converted = convert_rule_call(&rule_call).expect("configuration rule conversion");
        assert!(matches!(
            converted.attributes.get("values"),
            Some(Value::Dict(values))
                if matches!(values.get("cpu"), Some(Value::String(value)) if value == "k8")
        ));
        assert!(matches!(
            converted.attributes.get("constraint_values"),
            Some(Value::List(values))
                if matches!(values.as_slice(), [Value::String(value)] if value == "@platforms//os:linux")
        ));
    }

    #[test]
    fn compact_configurable_conversion_preserves_native_operands() {
        let value = StarlarkValue::Configurable {
            op: rush_starlark::ConfigurableOp::Add,
            left: std::sync::Arc::new(StarlarkValue::Select(HashMap::from([(
                "//conditions:default".to_string(),
                StarlarkValue::Int(1),
            )]))),
            right: std::sync::Arc::new(StarlarkValue::Int(2)),
        };

        let Value::Configurable { left, right, .. } = convert_value(&value).unwrap() else {
            panic!("compact configurable expression must remain symbolic");
        };
        assert!(matches!(left.as_ref(), Value::Select(branches)
            if matches!(branches.get("//conditions:default"), Some(Value::Int(1)))));
        assert!(matches!(right.as_ref(), Value::Int(2)));

        let dict_value = StarlarkValue::Configurable {
            op: rush_starlark::ConfigurableOp::BitOr,
            left: std::sync::Arc::new(StarlarkValue::Dict(HashMap::from([(
                "left".to_string(),
                StarlarkValue::String("one".to_string()),
            )]))),
            right: std::sync::Arc::new(StarlarkValue::Select(HashMap::from([(
                "//conditions:default".to_string(),
                StarlarkValue::Dict(HashMap::from([(
                    "right".to_string(),
                    StarlarkValue::String("two".to_string()),
                )])),
            )]))),
        };
        let Value::Configurable { left, right, .. } = convert_value(&dict_value).unwrap() else {
            panic!("compact configurable dict union must remain symbolic");
        };
        assert!(matches!(left.as_ref(), Value::Dict(items) if items.contains_key("left")));
        assert!(matches!(right.as_ref(), Value::Select(branches)
            if matches!(branches.get("//conditions:default"), Some(Value::Dict(items)) if items.contains_key("right"))));
    }

    #[test]
    fn test_is_select_dict() {
        let mut map = HashMap::new();
        map.insert(
            "@platforms//os:linux".to_string(),
            StarlarkValue::String("linux_val".to_string()),
        );
        map.insert(
            "//conditions:default".to_string(),
            StarlarkValue::String("default_val".to_string()),
        );

        assert!(is_select_dict(&map));
    }

    #[test]
    fn test_is_not_select_dict() {
        let mut map = HashMap::new();
        map.insert(
            "key1".to_string(),
            StarlarkValue::String("val1".to_string()),
        );
        map.insert(
            "key2".to_string(),
            StarlarkValue::String("val2".to_string()),
        );

        assert!(!is_select_dict(&map));
    }

    #[test]
    fn expand_template_substitutions_preserve_select() {
        let substitutions = StarlarkValue::Dict(HashMap::from([(
            "@TOKEN@".to_string(),
            StarlarkValue::String("replacement".to_string()),
        )]));
        let value = StarlarkValue::Select(HashMap::from([(
            "//conditions:rush_select_test_never".to_string(),
            substitutions,
        )]));

        let converted = convert_expand_template_substitutions(&value).unwrap();
        let Value::Select(conditions) = converted else {
            panic!("expand_template substitutions must remain configurable");
        };
        assert!(matches!(
            conditions.get("//conditions:rush_select_test_never"),
            Some(Value::List(entries)) if entries.len() == 1
        ));
    }

    #[test]
    fn test_rule_call_capture() {
        // Create a simple BUILD file content (single-line format)
        let build_content = r#"cc_library(name = "test_lib", srcs = ["test.cc"], hdrs = ["test.h"], deps = [":base"])
"#;

        // Write to a temporary file
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(build_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        // Parse the BUILD file
        let result = rush_incremental::tracked::test_scope(
            temp_file.path().parent().unwrap(),
            || parse_build_file_with_starlark(temp_file.path()),
        );
        assert!(
            result.is_ok(),
            "Failed to parse BUILD file: {:?}",
            result.err()
        );

        let build_file = result.unwrap();
        assert_eq!(build_file.rules.len(), 1);

        let rule = &build_file.rules[0];
        assert_eq!(rule.rule_type, "cc_library");
        assert_eq!(rule.name, "test_lib");
        assert!(rule.attributes.contains_key("name"));
        assert!(rule.attributes.contains_key("srcs"));
        assert!(rule.attributes.contains_key("hdrs"));
        assert!(rule.attributes.contains_key("deps"));
    }

    #[test]
    fn captures_known_unsupported_native_java_rules() {
        let build_content = r#"
java_proto_library(name = "proto", deps = [":source_proto"])
java_lite_proto_library(name = "lite", deps = [":proto"])
native.java_library(name = "java", srcs = ["Library.java"])
java_binary(name = "binary", deps = [":java"])
java_import(name = "imported", jars = ["library.jar"])
java_plugin(name = "plugin", deps = [":java"])
native.java_test(name = "test", deps = [":java"])
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(build_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let build_file = rush_incremental::tracked::test_scope(
            temp_file.path().parent().expect("temporary BUILD parent"),
            || parse_build_file_with_starlark(temp_file.path()),
        )
        .unwrap();
        assert_eq!(
            build_file
                .rules
                .iter()
                .map(|rule| (rule.rule_type.as_str(), rule.name.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("java_proto_library", "proto"),
                ("java_lite_proto_library", "lite"),
                ("java_library", "java"),
                ("java_binary", "binary"),
                ("java_import", "imported"),
                ("java_plugin", "plugin"),
                ("java_test", "test"),
            ]
        );
    }

    #[test]
    fn test_multiple_rules() {
        let build_content = r#"cc_library(name = "lib1", srcs = ["lib1.cc"])
cc_binary(name = "bin1", srcs = ["main.cc"], deps = [":lib1"])
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(build_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let result = rush_incremental::tracked::test_scope(
            temp_file.path().parent().unwrap(),
            || parse_build_file_with_starlark(temp_file.path()),
        );
        assert!(result.is_ok());

        let build_file = result.unwrap();
        assert_eq!(build_file.rules.len(), 2);

        assert_eq!(build_file.rules[0].rule_type, "cc_library");
        assert_eq!(build_file.rules[0].name, "lib1");

        assert_eq!(build_file.rules[1].rule_type, "cc_binary");
        assert_eq!(build_file.rules[1].name, "bin1");
    }

    #[test]
    fn cc_binary_linkshared_preserves_native_literals_only() {
        let build_content = r#"cc_binary(name = "int_shared", linkshared = 1)
cc_binary(name = "bool_shared", linkshared = True)
cc_binary(name = "quoted_shared", linkshared = "1")
cc_library(name = "ordinary_int", alwayslink = 1)
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(build_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let build_file = rush_incremental::tracked::test_scope(
            temp_file.path().parent().unwrap(),
            || parse_build_file_with_starlark(temp_file.path()),
        )
        .unwrap();
        assert!(matches!(
            build_file.rules[0].attributes.get("linkshared"),
            Some(Value::Int(1))
        ));
        assert!(matches!(
            build_file.rules[1].attributes.get("linkshared"),
            Some(Value::Bool(true))
        ));
        assert!(matches!(
            build_file.rules[2].attributes.get("linkshared"),
            Some(Value::String(value)) if value == "1"
        ));
        assert!(matches!(
            build_file.rules[3].attributes.get("alwayslink"),
            Some(Value::String(value)) if value == "1"
        ));
    }

    #[test]
    fn test_duplicate_target_name_rejected() {
        let build_content = r#"cc_binary(name = "x", srcs = ["main.cc"])
cc_library(name = "x", srcs = ["lib.cc"])
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(build_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let error = rush_incremental::tracked::test_scope(
            temp_file.path().parent().unwrap(),
            || parse_build_file_with_starlark(temp_file.path()),
        )
        .expect_err("duplicate target names must be rejected");
        let message = format!("{error:#}");
        assert!(
            message.contains("duplicate target name 'x'"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains(&temp_file.path().display().to_string()),
            "error must identify the BUILD file: {message}"
        );
    }

    #[test]
    fn test_multiline_build_file() {
        // Test multi-line BUILD file format (Bazel standard)
        let build_content = r#"cc_library(
    name = "multiline_lib",
    srcs = ["lib.cc"],
    hdrs = ["lib.h"],
    deps = ["//base:common"],
)

cc_binary(
    name = "app",
    srcs = ["main.cc"],
    deps = [":multiline_lib"],
)
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(build_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let result = rush_incremental::tracked::test_scope(
            temp_file.path().parent().unwrap(),
            || parse_build_file_with_starlark(temp_file.path()),
        );
        if let Err(e) = &result {
            println!("Parse error: {:?}", e);
        }
        assert!(result.is_ok(), "Failed to parse multi-line BUILD file");

        let build_file = result.unwrap();
        assert_eq!(build_file.rules.len(), 2);

        // Verify first rule
        let lib_rule = &build_file.rules[0];
        assert_eq!(lib_rule.rule_type, "cc_library");
        assert_eq!(lib_rule.name, "multiline_lib");
        assert!(lib_rule.attributes.contains_key("srcs"));
        assert!(lib_rule.attributes.contains_key("hdrs"));
        assert!(lib_rule.attributes.contains_key("deps"));

        // Verify second rule
        let bin_rule = &build_file.rules[1];
        assert_eq!(bin_rule.rule_type, "cc_binary");
        assert_eq!(bin_rule.name, "app");
        assert!(bin_rule.attributes.contains_key("deps"));
    }

    #[test]
    #[ignore] // Requires tensorflow_scale_test BUILD file
    fn test_parse_tensorflow_scale_test_build() {
        use std::path::Path;

        let build_path = Path::new("../../examples/tensorflow_scale_test/BUILD");
        if !build_path.exists() {
            println!("Skipping test - BUILD file not found at {:?}", build_path);
            return;
        }

        let result = parse_build_file_with_starlark(build_path);
        match &result {
            Ok(build_file) => {
                println!("✓ Successfully parsed BUILD file!");
                println!("  Found {} rules", build_file.rules.len());
                if !build_file.rules.is_empty() {
                    println!(
                        "  First rule: {} ({})",
                        build_file.rules[0].name, build_file.rules[0].rule_type
                    );
                    if build_file.rules.len() > 1 {
                        println!(
                            "  Last rule: {} ({})",
                            build_file.rules.last().unwrap().name,
                            build_file.rules.last().unwrap().rule_type
                        );
                    }
                }
            }
            Err(e) => {
                println!("✗ Failed to parse BUILD file: {:?}", e);
            }
        }

        assert!(result.is_ok(), "Failed to parse BUILD file");
        let build_file = result.unwrap();
        // tensorflow_scale_test has 100 kernel_lib targets + 1 binary target + 1 base:common
        assert!(
            build_file.rules.len() >= 100,
            "Expected >= 100 rules, got {}",
            build_file.rules.len()
        );
    }

    #[test]
    #[ignore] // Requires actual TensorFlow source
    fn test_parse_real_tensorflow_build() {
        use std::path::{Path, PathBuf};

        // Use TF_PATH env var, or default to ../tensorflow relative to repo root
        let tf_path = std::env::var("TF_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("../tensorflow"));

        let build_path = tf_path.join("tensorflow/c/eager/BUILD");
        if !build_path.exists() {
            println!(
                "Skipping - TensorFlow BUILD not found at {}",
                build_path.display()
            );
            println!("Set TF_PATH environment variable or clone TensorFlow to ../tensorflow");
            return;
        }

        println!("Attempting to parse real TensorFlow BUILD file...");
        let result = parse_build_file_with_starlark(&build_path);

        match &result {
            Ok(build_file) => {
                println!("✓ Parsed {} rules!", build_file.rules.len());
                for (i, rule) in build_file.rules.iter().take(5).enumerate() {
                    println!("  {}. {} ({})", i + 1, rule.name, rule.rule_type);
                }
            }
            Err(e) => {
                println!("✗ Parse failed (expected - missing features):");
                println!("  {:?}", e);
                println!("\nThis helps identify what features to implement next!");
            }
        }
    }
}
