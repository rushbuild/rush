// Rush Build - Phase 4 virtual artifact end-to-end flow
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! The flagship Phase 4 scenario, end to end through the real
//! RushActionManager + LocalStrategy + CAS:
//!
//! 1. producer executes (MISS), commits records + CAS blobs;
//! 2. outputs deleted, producer looked up again → VIRTUAL hit: stamps
//!    republished, bytes stay CAS-backed, file stays absent;
//! 3. a consumer that MISSes JIT-materializes the virtual input before
//!    executing;
//! 4. a consumer that HITs virtually keeps everything CAS-backed.
//!
//! Lives in its own integration-test binary because
//! `virtual_artifacts::enabled()` and the CAS root latch process-global
//! env (`RUSH_VIRTUAL_ARTIFACTS`, `HOME`) on first use.

use rush_action::{Action, Artifact, Command, ExecutionInfo};
use rush_build::action_manager::RushActionManager;
use rush_cache::{ActionCache, CacheConfig};
use rush_exec::LocalStrategy;
use rush_resource::ResourceManager;
use rush_rules::context::ActionManager as _;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

fn shell_action(
    script: &str,
    mnemonic: &str,
    inputs: &[&Path],
    outputs: &[&Path],
) -> Action {
    let command = Command {
        executable: "/bin/sh".into(),
        args: vec!["-c".to_string(), script.to_string()],
        working_dir: None,
        execution_info: ExecutionInfo::default(),
    };
    let inputs: Vec<Artifact> = inputs
        .iter()
        .map(|p| Artifact::Source(p.to_path_buf()))
        .collect();
    let outputs: Vec<Artifact> = outputs
        .iter()
        .map(|p| Artifact::Source(p.to_path_buf()))
        .collect();
    let id = Action::compute_id(&inputs, &command, &HashMap::new()).unwrap();
    Action {
        id,
        command,
        env: HashMap::new(),
        inputs,
        outputs,
        mnemonic: mnemonic.to_string(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn virtual_hit_then_jit_materialization() {
    let home = tempfile::tempdir().unwrap();
    // Latch the process-global env BEFORE anything reads it: the CAS
    // roots (manager + materializer) derive from HOME, and enabled()
    // caches the flag. This is the only test in this binary.
    std::env::remove_var(rush_cache::roots::CACHE_ROOT_ENV);
    std::env::set_var("HOME", home.path());
    std::env::set_var("RUSH_VIRTUAL_ARTIFACTS", "1");
    std::env::remove_var("RUSH_USE_SCHEDULER");
    assert!(rush_action::virtual_artifacts::enabled());

    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let src = work.join("src.txt");
    std::fs::write(&src, b"payload-bytes\n").unwrap();
    let obj = work.join("lib.o");
    let bin = work.join("app");

    let manager = RushActionManager::new(
        Arc::new(ActionCache::with_disk(CacheConfig::default()).unwrap()),
        Arc::new(LocalStrategy),
        ResourceManager::new().unwrap(),
    );

    // 1. Producer MISS: executes for real, commits stamps + CAS blobs.
    let produce = shell_action(
        &format!("cp {} {}", src.display(), obj.display()),
        "CppCompile",
        &[&src],
        &[&obj],
    );
    let r1 = manager.execute(produce.clone()).await.unwrap();
    assert_eq!(r1.exit_code, 0);
    assert!(!r1.cached);
    assert!(obj.is_file());
    assert_eq!(r1.output_records.len(), 1);

    // 2. Output deleted → the lookup VIRTUAL-hits: its verified value stays
    //    process-local and its bytes stay in the CAS.
    std::fs::remove_file(&obj).unwrap();
    let r2 = manager.execute(produce.clone()).await.unwrap();
    assert!(r2.cached, "second lookup must be a cache hit");
    assert!(
        !obj.exists(),
        "virtual hit must keep the output CAS-backed, not materialize it"
    );
    assert!(rush_action::virtual_artifacts::is_registered(&obj));
    assert!(
        !work.join("lib.o.rushid").exists(),
        "virtual hit must not publish output-adjacent Rush state"
    );

    // 3. Consumer MISS: the absent input is hashed via its verified virtual
    //    record, then JIT-materialized from the CAS before the shell runs.
    let consume = shell_action(
        &format!("cat {} > {}", obj.display(), bin.display()),
        "CppLink",
        &[&obj],
        &[&bin],
    );
    let r3 = manager.execute(consume.clone()).await.unwrap();
    assert_eq!(
        r3.exit_code,
        0,
        "consumer must succeed against a JIT-materialized input"
    );
    assert!(obj.is_file(), "JIT must have materialized the input");
    assert_eq!(std::fs::read(&bin).unwrap(), b"payload-bytes\n");

    // 4. Wipe both outputs: producer AND consumer now virtual-hit —
    //    a fully CAS-backed no-op, zero bytes rematerialized.
    std::fs::remove_file(&obj).unwrap();
    std::fs::remove_file(&bin).unwrap();
    let r5 = manager.execute(produce).await.unwrap();
    assert!(r5.cached);
    let r6 = manager.execute(consume).await.unwrap();
    assert!(r6.cached, "consumer must hit: its virtual input is unchanged");
    assert!(!obj.exists() && !bin.exists(), "everything stays CAS-backed");
    assert!(rush_action::virtual_artifacts::is_registered(&bin));

    // The requested-output sweep can still get the final binary back.
    let restored = rush_build::virtual_materializer::ensure_path_materialized(
        &bin,
        rush_action::virtual_artifacts::MaterializePurpose::TopLevel,
    )
    .unwrap();
    assert_eq!(restored, b"payload-bytes\n".len() as u64);
    assert_eq!(std::fs::read(&bin).unwrap(), b"payload-bytes\n");

    thin_archive_with_virtual_members(&work, &manager).await;
    reexecution_clears_stale_registration(&work, &manager).await;
    concurrent_jit_of_one_shared_input(&work, &manager).await;
}

/// A PRESENT thin archive whose members are virtual must still get its
/// members materialized: the two come from separate actions with
/// independent hit gates, and a thin archive only NAMES its members, so
/// a linker handed one without them fails.
async fn thin_archive_with_virtual_members(work: &Path, manager: &RushActionManager) {
    let member = work.join("m1.o");
    let archive = work.join("libm.a");

    let compile = shell_action(
        &format!("printf member-bytes > {}", member.display()),
        "CppCompile",
        &[],
        &[&member],
    );
    // Commit the member through the manager so its bytes reach the CAS.
    assert_eq!(manager.execute(compile.clone()).await.unwrap().exit_code, 0);

    let ar = shell_action(
        &format!(
            "cd {} && rm -f libm.a && ar rcsT libm.a m1.o",
            work.display()
        ),
        "CppArchive",
        &[&member],
        &[&archive],
    );
    assert_eq!(manager.execute(ar).await.unwrap().exit_code, 0);
    assert!(archive.is_file());

    // Delete ONLY the object (a disk sweep, a partial clean), then let
    // the member's action virtual-hit so it is registered-and-absent.
    std::fs::remove_file(&member).unwrap();
    assert!(manager.execute(compile).await.unwrap().cached);
    assert!(!member.exists());

    // The archive is present, so the naive short-circuit would report
    // "nothing to do" and hand the linker a dangling member list.
    rush_build::virtual_materializer::ensure_path_materialized(
        &archive,
        rush_action::virtual_artifacts::MaterializePurpose::Jit,
    )
    .unwrap();
    assert!(
        member.is_file(),
        "a present thin archive must still materialize its virtual members"
    );
    assert_eq!(std::fs::read(&member).unwrap(), b"member-bytes");
}

/// A re-executed action must drop the stale virtual record for its
/// output: otherwise `present_or_virtual` would keep masking a tool that
/// exits 0 without writing, and a later sweep would resurrect the
/// PREVIOUS build's bytes under the new action's name.
async fn reexecution_clears_stale_registration(work: &Path, manager: &RushActionManager) {
    let out = work.join("stale.o");
    std::fs::write(&out, b"v1").unwrap();
    let v1 = shell_action(
        &format!("printf v1 > {}", out.display()),
        "CppCompile",
        &[],
        &[&out],
    );
    assert_eq!(manager.execute(v1.clone()).await.unwrap().exit_code, 0);
    std::fs::remove_file(&out).unwrap();
    assert!(manager.execute(v1).await.unwrap().cached);
    assert!(rush_action::virtual_artifacts::is_registered(&out));

    // A DIFFERENT action (edited source ⇒ new id) that exits 0 and
    // writes nothing must not inherit the old registration.
    let v2 = shell_action("true # v2 writes nothing", "CppCompile", &[], &[&out]);
    assert_eq!(manager.execute(v2).await.unwrap().exit_code, 0);
    assert!(
        !rush_action::virtual_artifacts::present_or_virtual(&out),
        "a re-executed action must clear the stale virtual record so the \
         rule's produced-nothing guard still fires"
    );
}

/// Many consumers of one shared virtual input materialize concurrently:
/// the restore must be serialized per path and the published bytes must
/// be exactly the recorded content.
async fn concurrent_jit_of_one_shared_input(work: &Path, manager: &RushActionManager) {
    let shared = work.join("shared.o");
    let seed = work.join("shared.seed");
    let payload: Vec<u8> = (0..(4 * 1024 * 1024)).map(|i| (i % 251) as u8).collect();
    std::fs::write(&seed, &payload).unwrap();
    let produce = shell_action(
        &format!("cp {} {}", seed.display(), shared.display()),
        "CppCompile",
        &[&seed],
        &[&shared],
    );
    assert_eq!(manager.execute(produce.clone()).await.unwrap().exit_code, 0);
    std::fs::remove_file(&shared).unwrap();
    assert!(manager.execute(produce).await.unwrap().cached);
    assert!(!shared.exists());

    let mut handles = Vec::new();
    for _ in 0..16 {
        let p = shared.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            rush_build::virtual_materializer::ensure_path_materialized(
                &p,
                rush_action::virtual_artifacts::MaterializePurpose::Jit,
            )
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    assert_eq!(
        std::fs::read(&shared).unwrap(),
        payload,
        "concurrent materialization must publish exactly the recorded bytes"
    );
}
