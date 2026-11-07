//! Correctness proof for the `tracked` context infrastructure.
//!
//! These gate the whole TrackedIo migration: they prove [`scope`] installs +
//! restores (including nesting), the fail-loud free functions record into the
//! installed IO, a read outside any scope panics, and [`bind`] correctly carries
//! the shared `Arc` across a real `std::thread::spawn` (the spawn_blocking /
//! thread::spawn boundary) while an un-`bind`-ed spawn fails loud.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;

use rush_command_core::CommandSemantics;
use rush_protocol::command::v1::RunRequest;
use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};

use crate::tracked;
use crate::tracked::SharedTrackedIo;
use crate::{Dependency, DependencyManifest, RealTrackedIo};

fn empty_semantics() -> CommandSemantics {
    let req = RunRequest {
        auth_cookie: Bytes::new(),
        argv: vec![
            Bytes::from_static(b"rush"),
            Bytes::from_static(b"build"),
            Bytes::from_static(b"//foo:bar"),
        ],
        working_directory: Bytes::from_static(b"/tmp/proj"),
        environment: Vec::new(),
        block_for_lock: true,
        client_build_id: "build-1".to_string(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        workspace_config: Vec::new(),
    };
    CommandSemantics::from_run_request(&req).expect("build semantics")
}

fn new_io(root: &Path) -> SharedTrackedIo {
    Arc::new(RealTrackedIo::new(empty_semantics(), root))
}

/// Does `manifest` contain a FileContent dep whose path ends with `name`?
fn has_file_content(manifest: &DependencyManifest, name: &str) -> bool {
    manifest
        .deps()
        .iter()
        .any(|d| matches!(d, Dependency::FileContent { path, .. } if path.ends_with(name)))
}

/// 1. `scope` installs and restores; nesting restores the OUTER io, not None.
#[test]
fn scope_installs_and_restores_with_nesting() {
    let dir = tempfile::tempdir().unwrap();
    assert!(tracked::current().is_none(), "no io before any scope");

    let outer = new_io(dir.path());
    let inner = new_io(dir.path());

    tracked::scope(outer.clone(), || {
        assert!(tracked::current().is_some(), "outer installed");
        let outer_ptr = Arc::as_ptr(&tracked::current().unwrap()) as *const ();

        tracked::scope(inner.clone(), || {
            let inner_ptr = Arc::as_ptr(&tracked::current().unwrap()) as *const ();
            assert_ne!(outer_ptr, inner_ptr, "inner io is distinct");
        });

        // After the inner scope exits we must be back on OUTER, not None.
        let restored = tracked::current().expect("outer restored, not None");
        assert_eq!(
            Arc::as_ptr(&restored) as *const (),
            outer_ptr,
            "nested scope restored the outer io"
        );
    });

    assert!(tracked::current().is_none(), "io cleared after outermost scope");
}

/// 1b. The drop-guard restores even if the body panics.
#[test]
fn scope_restores_on_panic() {
    let dir = tempfile::tempdir().unwrap();
    let io = new_io(dir.path());
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tracked::scope(io, || {
            assert!(tracked::current().is_some());
            panic!("boom");
        });
    }));
    assert!(r.is_err(), "panic propagated");
    assert!(
        tracked::current().is_none(),
        "io restored to None even after a panic in the body"
    );
}

/// 2. A free function inside `scope` returns the content AND the shared IO's
///    manifest afterward contains the FileContent dep.
#[test]
fn free_fn_inside_scope_reads_and_records() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.txt");
    fs::write(&p, b"hello world").unwrap();

    let io = new_io(dir.path());
    let content = tracked::scope(io.clone(), || tracked::read_file(&p).unwrap());
    assert_eq!(&content[..], b"hello world");

    let manifest = io.manifest();
    assert!(
        has_file_content(&manifest, "a.txt"),
        "manifest records the FileContent dep for the tracked read"
    );
}

/// 3. A free function OUTSIDE any scope PANICS (fail-loud).
#[test]
#[should_panic(expected = "no analysis TrackedIo installed")]
fn free_fn_outside_scope_panics() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("nope.txt");
    let _ = tracked::read_file(&p);
}

/// 4. `bind` propagates context across a real `std::thread::spawn`: the read
///    succeeds on the spawned thread AND records into the SAME shared Arc.
#[test]
fn bind_propagates_across_thread_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("crossing.txt");
    fs::write(&p, b"on another thread").unwrap();

    let io = new_io(dir.path());
    let p2 = p.clone();

    let content = tracked::scope(io.clone(), || {
        let handle = std::thread::spawn(tracked::bind(move || tracked::read_file(&p2).unwrap()));
        handle.join().expect("spawned thread joined")
    });
    assert_eq!(&content[..], b"on another thread");

    // Snapshot AFTER join: the spawned-thread read recorded into the shared Arc.
    let manifest = io.manifest();
    assert!(
        has_file_content(&manifest, "crossing.txt"),
        "bind carried the shared Arc so the spawned read recorded into it"
    );
}

/// 5. A bare `std::thread::spawn` WITHOUT `bind` PANICS on the spawned thread
///    (the thread-local is empty there) — an un-propagated boundary fails loud.
#[test]
fn bare_spawn_without_bind_panics_on_spawned_thread() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("orphan.txt");
    fs::write(&p, b"x").unwrap();

    let io = new_io(dir.path());
    let p2 = p.clone();

    // Silence the default panic hook noise from the spawned thread.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let join_res = tracked::scope(io, || {
        // No `bind`: the spawned thread has an empty thread-local.
        let handle = std::thread::spawn(move || tracked::exists(&p2));
        handle.join()
    });

    std::panic::set_hook(prev_hook);

    assert!(
        join_res.is_err(),
        "the un-bound spawned read must panic (fail-loud), surfacing as a join Err"
    );
}

/// 6. Two reads on two threads (both via `bind`, sharing one Arc) both land in
///    the manifest after join (concurrent recording into the shared Mutex works).
#[test]
fn two_bound_threads_both_record_into_shared_arc() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    fs::write(&a, b"alpha").unwrap();
    fs::write(&b, b"beta").unwrap();

    let io = new_io(dir.path());
    let a2 = a.clone();
    let b2 = b.clone();

    tracked::scope(io.clone(), || {
        let h1 = std::thread::spawn(tracked::bind(move || {
            tracked::read_file(&a2).unwrap();
        }));
        let h2 = std::thread::spawn(tracked::bind(move || {
            tracked::read_file(&b2).unwrap();
        }));
        h1.join().unwrap();
        h2.join().unwrap();
    });

    let manifest = io.manifest();
    assert!(has_file_content(&manifest, "a.txt"), "thread 1 read recorded");
    assert!(has_file_content(&manifest, "b.txt"), "thread 2 read recorded");
}

// ---------------------------------------------------------------------------
// Capture-since-mark + replay primitives (daemon Run-wiring blocker #1).
// ---------------------------------------------------------------------------

/// 7. `mark`/`captured_since` capture exactly the deps recorded between the mark
///    and the read-back, and `record_deps` replays a pre-existing dep set into the
///    recorded list (appended; deduped at manifest time).
#[test]
fn mark_capture_and_record_deps_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    fs::write(&a, b"alpha").unwrap();
    fs::write(&b, b"beta").unwrap();

    let io = new_io(dir.path());

    let captured = tracked::scope(io.clone(), || {
        // Read a BEFORE the mark — must NOT be captured.
        tracked::read_file(&a).unwrap();
        let mark = tracked::mark().expect("mark inside scope");
        // Read b AFTER the mark — must be captured.
        tracked::read_file(&b).unwrap();
        tracked::captured_since(mark)
    });

    // The captured slice is exactly b's observations (FileContent + kind), not a's.
    assert!(
        captured
            .iter()
            .any(|d| matches!(d, Dependency::FileContent { path, .. } if path.ends_with("b.txt"))),
        "captured_since contains the post-mark read of b"
    );
    assert!(
        !captured
            .iter()
            .any(|d| matches!(d, Dependency::FileContent { path, .. } if path.ends_with("a.txt"))),
        "captured_since does NOT contain the pre-mark read of a"
    );

    // Replay the captured b-deps into a FRESH io (simulating a cross-command HIT):
    // the fresh manifest must now contain b's file dep even though no read happened.
    let io2 = new_io(dir.path());
    tracked::scope(io2.clone(), || {
        assert!(io2.manifest().is_empty(), "fresh io starts empty");
        tracked::replay_deps(&captured);
    });
    let m2 = io2.manifest();
    assert!(
        has_file_content(&m2, "b.txt"),
        "replay_deps injected b's file dep into the fresh manifest"
    );
    assert!(!m2.is_empty(), "replayed manifest is non-empty");
}

/// 8. `mark`/`captured_since`/`replay_deps` are no-ops outside a scope (harmless),
///    and `record_deps` dedups via the manifest on replay.
#[test]
fn capture_replay_are_noops_outside_scope_and_dedup() {
    assert!(tracked::mark().is_none(), "mark outside scope is None");
    assert!(
        tracked::captured_since(0).is_empty(),
        "captured_since outside scope is empty"
    );
    // Must not panic (unlike the fail-loud read free fns).
    tracked::replay_deps(&[Dependency::Env {
        name: b"X".to_vec(),
        value: None,
    }]);

    // Replaying the same dep twice dedups at manifest snapshot.
    let dir = tempfile::tempdir().unwrap();
    let io = new_io(dir.path());
    let dep = Dependency::Env {
        name: b"CC".to_vec(),
        value: Some(b"gcc".to_vec()),
    };
    tracked::scope(io.clone(), || {
        tracked::replay_deps(std::slice::from_ref(&dep));
        tracked::replay_deps(std::slice::from_ref(&dep));
    });
    let m = io.manifest();
    let env_count = m
        .deps()
        .iter()
        .filter(|d| matches!(d, Dependency::Env { name, .. } if name == b"CC"))
        .count();
    assert_eq!(env_count, 1, "duplicate replayed deps are deduped in the manifest");
}
