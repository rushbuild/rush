use rush_starlark::{parse, BzlLoader, Evaluator, GlobalBzlCache, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Barrier, Mutex};

const SOURCE: &str = r#"
def labels():
    return [str(Label("//cfg:mode")), str(Label(":local")), str(Label("@dep//cfg:mode"))]
"#;

fn setup(global: Option<GlobalBzlCache>) -> (tempfile::TempDir, Arc<Mutex<BzlLoader>>) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("WORKSPACE"), "workspace(name = \"main\")\n").unwrap();
    let shared = root.path().join("shared");
    std::fs::create_dir_all(shared.join("defs")).unwrap();
    std::fs::write(shared.join("defs/module.bzl"), SOURCE).unwrap();
    let mut loader = BzlLoader::new_with_all(root.path().to_path_buf(), None, global);
    for (owner, dependency) in [("owner_a", "dep_a"), ("owner_b", "dep_b")] {
        loader.add_external_repo(owner.to_string(), shared.clone());
        loader.set_repo_mapping_for(owner, HashMap::from([("dep".to_string(), dependency.to_string())]));
    }
    (root, Arc::new(Mutex::new(loader)))
}

fn evaluate(loader: &Arc<Mutex<BzlLoader>>, source: &str) -> Value {
    let mut evaluator = Evaluator::new();
    evaluator.set_current_package("consumer".to_string());
    evaluator.set_bzl_loader(loader.clone());
    evaluator.eval_program(&parse(source).unwrap()).unwrap();
    evaluator.get_var("result").unwrap()
}

fn labels(loader: &Arc<Mutex<BzlLoader>>, owner: &str) -> Value {
    evaluate(loader, &format!("load(\"@@{owner}//defs:module.bzl\", \"labels\")\nresult = labels()\n"))
}

fn expected(owner: &str, dependency: &str) -> Value {
    Value::List(vec![
        Value::String(format!("@@{owner}//cfg:mode")),
        Value::String(format!("@@{owner}//defs:local")),
        Value::String(format!("@@{dependency}//cfg:mode")),
    ])
}

fn analysis(root: &Path, body: impl FnOnce()) {
    rush_incremental::tracked::test_scope(root, || {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let _guard = runtime.enter();
        body();
    });
}

#[test]
fn local_cache_keeps_two_owners_of_one_physical_module_distinct() {
    let (root, loader) = setup(None);
    analysis(root.path(), || {
        for _ in 0..3 {
            assert_eq!(labels(&loader, "owner_a"), expected("owner_a", "dep_a"));
            assert_eq!(labels(&loader, "owner_b"), expected("owner_b", "dep_b"));
        }
    });
    assert_eq!(loader.lock().unwrap().cache_stats().symbol_cache_entries, 2);
}

#[test]
fn global_cache_preserves_owners_on_cold_warm_and_cross_command_loads() {
    let cache = GlobalBzlCache::new();
    let (root, loader) = setup(Some(cache.clone()));
    for _ in 0..2 {
        analysis(root.path(), || {
            for _ in 0..2 {
                assert_eq!(labels(&loader, "owner_a"), expected("owner_a", "dep_a"));
                assert_eq!(labels(&loader, "owner_b"), expected("owner_b", "dep_b"));
            }
        });
    }
    let stats = cache.cache_stats();
    assert_eq!(stats.symbol_cache_entries, 2);
    assert_eq!(stats.estimate_symbols_size_calls, 2, "warm loads must reuse evaluated modules");
}

#[test]
fn mapping_change_retires_module_and_cached_reexports_without_source_edits() {
    for global in [false, true] {
        let (root, loader) = setup(global.then(GlobalBzlCache::new));
        std::fs::write(root.path().join("bridge.bzl"), "load(\"@@owner_a//defs:module.bzl\", _labels = \"labels\")\nlabels = _labels\n").unwrap();
        analysis(root.path(), || {
            let bridge = "load(\"@@//:bridge.bzl\", \"labels\")\nresult = labels()\n";
            assert_eq!(evaluate(&loader, bridge), expected("owner_a", "dep_a"));
            loader.lock().unwrap().set_repo_mapping_for("owner_a", HashMap::from([("dep".to_string(), "changed_dep".to_string())]));
            assert_eq!(evaluate(&loader, bridge), expected("owner_a", "changed_dep"));
            assert_eq!(labels(&loader, "owner_a"), expected("owner_a", "changed_dep"));
            assert_eq!(labels(&loader, "owner_b"), expected("owner_b", "dep_b"));
        });
    }
}

#[test]
fn simultaneous_loads_of_shared_source_do_not_share_owner_or_placeholders() {
    let cache = GlobalBzlCache::new();
    let (root, loader) = setup(Some(cache.clone()));
    let start = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for (owner, dependency) in [("owner_a", "dep_a"), ("owner_b", "dep_b")] {
        let loader = loader.clone();
        let start = start.clone();
        let root = root.path().to_path_buf();
        workers.push(std::thread::spawn(move || {
            analysis(&root, || {
                start.wait();
                for _ in 0..5 {
                    assert_eq!(labels(&loader, owner), expected(owner, dependency));
                }
            });
        }));
    }
    start.wait();
    for worker in workers { worker.join().unwrap(); }
    assert_eq!(cache.cache_stats().symbol_cache_entries, 2);
    assert_eq!(cache.cache_stats().estimate_symbols_size_calls, 2);
}

#[test]
fn old_path_only_entries_cannot_supply_an_owner_qualified_load() {
    let cache = GlobalBzlCache::new();
    let (root, loader) = setup(Some(cache.clone()));
    let path = std::fs::canonicalize(root.path().join("shared/defs/module.bzl")).unwrap();
    cache.put_symbols_public(path, HashMap::from([("labels".to_string(), Value::Int(99))]));
    analysis(root.path(), || {
        assert_eq!(labels(&loader, "owner_a"), expected("owner_a", "dep_a"));
    });
    assert_eq!(cache.cache_stats().symbol_cache_entries, 2);
}

#[test]
fn qualified_cache_keeps_physical_source_invalidation_and_missing_source_errors() {
    let cache = GlobalBzlCache::new();
    let (root, loader) = setup(Some(cache));
    let source = root.path().join("shared/defs/module.bzl");
    analysis(root.path(), || {
        assert_eq!(labels(&loader, "owner_a"), expected("owner_a", "dep_a"));
    });
    std::fs::write(&source, "def labels():\n    return \"edited\"\n").unwrap();
    analysis(root.path(), || {
        assert_eq!(labels(&loader, "owner_a"), Value::String("edited".to_string()));
    });
    std::fs::remove_file(&source).unwrap();
    analysis(root.path(), || {
        let error = BzlLoader::load_bzl_from_evaluator(loader.clone(), "@@owner_a//defs:module.bzl", &[("labels".to_string(), "labels".to_string())]).unwrap_err();
        assert_eq!(error.to_string(), format!(".bzl file not found: {}", source.display()));
    });
    std::fs::write(&source, SOURCE).unwrap();
    analysis(root.path(), || {
        assert_eq!(labels(&loader, "owner_a"), expected("owner_a", "dep_a"));
    });
}

#[test]
fn explicit_main_module_does_not_inherit_the_external_build_callers_mapping() {
    let (root, loader) = setup(Some(GlobalBzlCache::new()));
    std::fs::write(root.path().join("bridge.bzl"), "load(\"@dep//defs:module.bzl\", _labels = \"labels\")\nlabels = _labels\n").unwrap();
    {
        let mut loader = loader.lock().unwrap();
        loader.set_current_repo_name(Some("consumer".to_string()));
        loader.set_repo_mapping_for("", HashMap::from([("dep".to_string(), "owner_a".to_string())]));
        loader.set_repo_mapping_for("consumer", HashMap::from([("dep".to_string(), "owner_b".to_string())]));
    }
    analysis(root.path(), || {
        for _ in 0..2 {
            assert_eq!(evaluate(&loader, "load(\"@@//:bridge.bzl\", \"labels\")\nresult = labels()\n"), expected("owner_a", "dep_a"));
        }
    });
}

#[test]
fn module_address_package_survives_subdirectory_and_relative_loads() {
    let (root, loader) = setup(Some(GlobalBzlCache::new()));
    let directory = root.path().join("shared/defs/subdirectory");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("child.bzl"), "load(\":module.bzl\", _labels = \"labels\")\nlabels = _labels\nlocal = str(Label(\":sibling\"))\n").unwrap();
    analysis(root.path(), || {
        let mut evaluator = Evaluator::new();
        evaluator.set_bzl_loader(loader.clone());
        evaluator.eval_program(&parse("load(\"@@owner_a//defs:subdirectory/child.bzl\", \"local\", \"labels\")\nresult = labels()\n").unwrap()).unwrap();
        assert_eq!(evaluator.get_var("local"), Some(Value::String("@@owner_a//defs:sibling".to_string())));
        assert_eq!(evaluator.get_var("result"), Some(expected("owner_a", "dep_a")));
    });
}

#[cfg(unix)]
#[test]
fn individual_source_symlinks_keep_logical_owner_package_and_load_targets() {
    let cache = GlobalBzlCache::new();
    let (root, loader) = setup(Some(cache));
    let shared_source = root.path().join("shared/defs/module.bzl");
    {
        let mut loader = loader.lock().unwrap();
        for owner in ["owner_a", "owner_b"] {
            let repository = root.path().join(owner);
            std::fs::create_dir_all(repository.join("defs")).unwrap();
            std::os::unix::fs::symlink(&shared_source, repository.join("defs/module.bzl")).unwrap();
            loader.add_external_repo(owner.to_string(), repository);
        }
    }
    analysis(root.path(), || {
        for _ in 0..2 {
            assert_eq!(labels(&loader, "owner_a"), expected("owner_a", "dep_a"));
            assert_eq!(labels(&loader, "owner_b"), expected("owner_b", "dep_b"));
        }
    });
}

fn replacement_source(root: &Path) -> std::path::PathBuf {
    let replacement = root.join("replacement");
    std::fs::create_dir_all(replacement.join("defs")).unwrap();
    std::fs::write(replacement.join("defs/module.bzl"), "def labels():\n    return \"replacement\"\n").unwrap();
    replacement
}

fn assert_parent_and_direct(loader: &Arc<Mutex<BzlLoader>>, expected: Value) {
    for _ in 0..2 {
        assert_eq!(evaluate(loader, "load(\"@@//:bridge.bzl\", \"labels\")\nresult = labels()\n"), expected);
        assert_eq!(labels(loader, "owner_a"), expected);
    }
}

#[test]
fn changed_repository_root_retires_cached_parent_reexports_and_direct_loads() {
    for global in [false, true] {
        let (root, loader) = setup(global.then(GlobalBzlCache::new));
        std::fs::write(root.path().join("bridge.bzl"), "load(\"@@owner_a//defs:module.bzl\", _labels = \"labels\")\nlabels = _labels\n").unwrap();
        analysis(root.path(), || assert_parent_and_direct(&loader, expected("owner_a", "dep_a")));
        let replacement = replacement_source(root.path());
        loader.lock().unwrap().add_external_repo("owner_a".to_string(), replacement);
        analysis(root.path(), || assert_parent_and_direct(&loader, Value::String("replacement".to_string())));
        assert!(root.path().join("shared/defs/module.bzl").is_file(), "old physical inputs remain intact");
    }
}

#[cfg(unix)]
#[test]
fn repointed_registered_root_symlink_retires_cached_parent_reexports() {
    for global in [false, true] {
        let (root, loader) = setup(global.then(GlobalBzlCache::new));
        let registered = root.path().join("registered_owner");
        std::os::unix::fs::symlink(root.path().join("shared"), &registered).unwrap();
        loader.lock().unwrap().add_external_repo("owner_a".to_string(), registered.clone());
        std::fs::write(root.path().join("bridge.bzl"), "load(\"@@owner_a//defs:module.bzl\", _labels = \"labels\")\nlabels = _labels\n").unwrap();
        analysis(root.path(), || assert_parent_and_direct(&loader, expected("owner_a", "dep_a")));
        let replacement = replacement_source(root.path());
        std::fs::remove_file(&registered).unwrap();
        std::os::unix::fs::symlink(replacement, &registered).unwrap();
        analysis(root.path(), || assert_parent_and_direct(&loader, Value::String("replacement".to_string())));
        assert!(root.path().join("shared/defs/module.bzl").is_file());
    }
}

#[test]
fn changed_repository_alias_retires_cached_parent_reexports() {
    for global in [false, true] {
        let (root, loader) = setup(global.then(GlobalBzlCache::new));
        {
            let loader = loader.lock().unwrap();
            for (owner, marker) in [("owner_a", "first"), ("owner_b", "second")] {
                let repository = root.path().join(owner);
                std::fs::create_dir_all(repository.join("route")).unwrap();
                std::fs::write(repository.join("route/module.bzl"), format!("def labels():\n    return \"{marker}\"\n")).unwrap();
                loader.external_repos.insert(owner.to_string(), repository);
            }
        }
        loader.lock().unwrap().add_repo_alias("route", "owner_a");
        std::fs::write(root.path().join("bridge.bzl"), "load(\"//route:module.bzl\", _labels = \"labels\")\nlabels = _labels\n").unwrap();
        let bridge = "load(\"@@//:bridge.bzl\", \"labels\")\nresult = labels()\n";
        analysis(root.path(), || {
            for _ in 0..2 { assert_eq!(evaluate(&loader, bridge), Value::String("first".to_string())); }
        });
        loader.lock().unwrap().add_repo_alias("route", "owner_b");
        analysis(root.path(), || {
            for _ in 0..2 {
                assert_eq!(evaluate(&loader, bridge), Value::String("second".to_string()));
                assert_eq!(evaluate(&loader, "load(\"@@owner_b//route:module.bzl\", \"labels\")\nresult = labels()\n"), Value::String("second".to_string()));
            }
        });
    }
}

#[cfg(unix)]
fn with_build_load_context<R>(root: &Path, package: &Path, owner: &str, body: impl FnOnce() -> R) -> R {
    struct RestoreContext;
    impl Drop for RestoreContext {
        fn drop(&mut self) { BzlLoader::pop_thread_context(); }
    }
    BzlLoader::push_thread_context_frame(rush_starlark::bzl_loader::BzlContextFrame {
        repo_root: root.to_path_buf(), package_dir: package.to_path_buf(),
        repository: Some(owner.to_string()), load_stack: Vec::new(), workspace_repository_context: None,
    });
    let _restore = RestoreContext;
    body()
}

#[cfg(unix)]
#[test]
fn symlinked_roots_keep_relative_loads_and_logical_package_aliases() {
    for global in [false, true] {
        let (root, loader) = setup(global.then(GlobalBzlCache::new));
        let physical = root.path().join("shared");
        std::os::unix::fs::symlink(physical.join("defs"), physical.join("package_alias")).unwrap();
        let outside = root.path().join("individual-source.bzl");
        std::fs::write(&outside, SOURCE).unwrap();
        std::os::unix::fs::symlink(&outside, physical.join("defs/linked.bzl")).unwrap();
        for owner in ["owner_a", "owner_b"] {
            let logical = root.path().join(format!("logical-{owner}"));
            std::os::unix::fs::symlink(&physical, &logical).unwrap();
            loader.lock().unwrap().add_external_repo(owner.into(), logical);
        }
        // Cold, warm and cross-command caches must all use the logical owner
        // and package, including two owners of the same physical source.
        for _ in 0..2 {
            analysis(root.path(), || {
                for (owner, dependency) in [("owner_a", "dep_a"), ("owner_b", "dep_b")] {
                    let logical = root.path().join(format!("logical-{owner}"));
                    for package in ["defs", "package_alias"] {
                        {
                            let mut state = loader.lock().unwrap();
                            state.set_current_repo_root(logical.clone());
                            state.set_current_repo_name(Some(owner.into()));
                            state.set_current_package(logical.join(package));
                        }
                        let expected = Value::List(vec![
                            Value::String(format!("@@{owner}//cfg:mode")),
                            Value::String(format!("@@{owner}//{package}:local")),
                            Value::String(format!("@@{dependency}//cfg:mode")),
                        ]);
                        with_build_load_context(&logical, &logical.join(package), owner, || {
                            for filename in ["module.bzl", ":module.bzl", "linked.bzl", ":linked.bzl"] {
                                for _ in 0..2 {
                                    let source = format!("load({filename:?}, \"labels\")\nresult = labels()\n");
                                    assert_eq!(evaluate(&loader, &source), expected, "{owner}/{package}/{filename}");
                                }
                            }
                        });
                    }
                }
            });
        }
    }
}

#[cfg(unix)]
#[test]
fn relative_load_package_outside_registered_root_is_refused() {
    let (root, loader) = setup(Some(GlobalBzlCache::new()));
    let physical = root.path().join("shared");
    let logical = root.path().join("logical-owner");
    std::os::unix::fs::symlink(&physical, &logical).unwrap();
    let outside = root.path().join("escape");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("module.bzl"), SOURCE).unwrap();
    loader.lock().unwrap().add_external_repo("owner_a".into(), logical.clone());
    analysis(root.path(), || {
        for package in [outside, logical.join("../escape")] {
            let mut state = loader.lock().unwrap();
            state.set_current_repo_root(logical.clone());
            state.set_current_repo_name(Some("owner_a".into()));
            state.set_current_package(package.clone());
            drop(state);
            let error = with_build_load_context(&logical, &package, "owner_a", || {
                BzlLoader::load_bzl_from_evaluator(loader.clone(), ":module.bzl", &[("labels".into(), "labels".into())]).unwrap_err()
            });
            assert!(format!("{error:#}").contains("outside its repository root"), "{error:#}");
        }
    });
}

#[cfg(unix)]
#[test]
fn relative_load_records_registered_root_symlink_witness() {
    use rush_incremental::{tracked, Dependency, RealTrackedIo, Staleness, TrackedIo};
    let (root, loader) = setup(Some(GlobalBzlCache::new()));
    let physical = root.path().join("shared");
    let logical = root.path().join("logical-owner");
    std::os::unix::fs::symlink(&physical, &logical).unwrap();
    {
        let mut state = loader.lock().unwrap();
        state.add_external_repo("owner_a".into(), logical.clone());
        state.set_current_repo_root(logical.clone());
        state.set_current_repo_name(Some("owner_a".into()));
        state.set_current_package(logical.join("defs"));
    }
    let semantics = rush_command_core::CommandSemantics::from_process();
    let io = Arc::new(RealTrackedIo::new(semantics.clone(), root.path()));
    tracked::scope(io.clone(), || {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let _entered = runtime.enter();
        with_build_load_context(&logical, &logical.join("defs"), "owner_a", || {
            assert_eq!(evaluate(&loader, "load(\"module.bzl\", \"labels\")\nresult = labels()\n"), expected("owner_a", "dep_a"));
        });
    });
    assert!(io.manifest().deps().iter().any(|dependency| matches!(dependency,
        Dependency::Symlink { path, .. } if path == &logical)),
        "logical repository symlink was not retained as an analysis dependency");
    let manifest = io.manifest();
    assert_eq!(manifest.validate(&semantics).staleness, Staleness::Fresh);
    let replacement = root.path().join("equal-byte-replacement");
    std::fs::create_dir_all(replacement.join("defs")).unwrap();
    std::fs::write(replacement.join("defs/module.bzl"), SOURCE).unwrap();
    std::fs::remove_file(&logical).unwrap();
    std::os::unix::fs::symlink(&replacement, &logical).unwrap();
    assert_ne!(manifest.validate(&semantics).staleness, Staleness::Fresh,
        "equal source bytes must not conceal a changed repository root witness");
}
