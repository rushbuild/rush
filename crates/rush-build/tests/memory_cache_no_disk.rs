//! Memory-only cache mode must not open or maintain a persistent CAS.

use rush_action::{Action, Artifact, Command, ExecutionInfo};
use rush_build::{action_manager::RushActionManager, BuildConfig, BuildOrchestrator};
use rush_cache::{cas::CasStore, ActionCache};
use rush_exec::LocalStrategy;
use rush_resource::ResourceManager;
use rush_rules::context::ActionManager as _;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

fn copy_action(src: &Path, out: &Path) -> Action {
    let command = Command {
        executable: "/bin/sh".into(),
        args: vec![
            "-c".to_string(),
            format!("cp {} {}", src.display(), out.display()),
        ],
        working_dir: None,
        execution_info: ExecutionInfo::default(),
    };
    let inputs = vec![Artifact::Source(src.to_path_buf())];
    let outputs = vec![Artifact::Source(out.to_path_buf())];
    let id = Action::compute_id(&inputs, &command, &HashMap::new()).unwrap();
    Action {
        id,
        command,
        env: HashMap::new(),
        inputs,
        outputs,
        mnemonic: "CppCompile".to_string(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_cache_never_opens_or_maintains_cas() {
    let root = tempfile::tempdir().unwrap();
    let cache_root = root.path().join("cache");
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    // This integration-test binary owns these process-global variables.
    std::env::set_var(rush_cache::roots::CACHE_ROOT_ENV, &cache_root);
    std::env::set_var("RUSH_VIRTUAL_ARTIFACTS", "1");
    std::env::remove_var("RUSH_USE_SCHEDULER");

    let manager = RushActionManager::new(
        Arc::new(ActionCache::new()),
        Arc::new(LocalStrategy),
        ResourceManager::new().unwrap(),
    );
    let src = work.join("input.txt");
    let out = work.join("output.o");
    std::fs::write(&src, b"memory-only\n").unwrap();
    let action = copy_action(&src, &out);

    let first = manager.execute(action.clone()).await.unwrap();
    assert!(!first.cached);
    assert_eq!(std::fs::read(&out).unwrap(), b"memory-only\n");
    assert!(
        !cache_root.exists(),
        "executing with a memory cache must not create the persistent cache root"
    );

    // A missing output cannot be restored without a CAS. Re-execute instead
    // of serving stale metadata or registering a virtual artifact.
    std::fs::remove_file(&out).unwrap();
    let second = manager.execute(action).await.unwrap();
    assert!(
        !second.cached,
        "a missing output must conservatively re-execute"
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"memory-only\n");
    assert!(!rush_action::virtual_artifacts::is_registered(&out));
    assert!(
        !cache_root.exists(),
        "the safe re-execution path must still leave the CAS unopened"
    );

    // Seed a valid pre-existing blob, then demand a zero-byte CAS budget. If
    // memory-mode maintenance accidentally runs, it evicts this blob.
    let seed = work.join("seed.bin");
    std::fs::write(&seed, b"keep-this-blob").unwrap();
    let digest =
        rush_action::artifact_value::ContentDigest(*blake3::hash(b"keep-this-blob").as_bytes());
    let cas = CasStore::new(&cache_root).unwrap();
    cas.store_file(&seed, &digest).unwrap();
    assert!(cas.has(&digest));

    std::env::set_var("RUSH_CAS_MAX_BYTES", "0");
    let workspace = work.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let orchestrator =
        BuildOrchestrator::try_with_config(workspace, BuildConfig::Fastbuild).unwrap();
    orchestrator.gc_disk_cache_if_needed().await;
    assert!(
        cas.has(&digest),
        "memory-mode cache maintenance must not inspect or evict an existing CAS"
    );

    std::env::remove_var("RUSH_CAS_MAX_BYTES");
    std::env::remove_var("RUSH_VIRTUAL_ARTIFACTS");
    std::env::remove_var(rush_cache::roots::CACHE_ROOT_ENV);
}
