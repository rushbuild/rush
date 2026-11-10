//! Exhaustive validation tests — the correctness proof for daemon graph reuse.
//!
//! Each test records a manifest via [`RealTrackedIo`] reading a real temp tree,
//! mutates the world, then asserts [`DependencyManifest::validate`] returns the
//! expected [`Staleness`]. Env tests vary the [`CommandSemantics`] passed to
//! `validate` (never `std::env`), proving the per-command source is authoritative.

use std::fs;
use std::path::Path;

use bytes::Bytes;

use rush_command_core::CommandSemantics;
use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};

use crate::{Dependency, RealTrackedIo, Staleness, TrackedIo};

/// Build a CommandSemantics with the given env vars and `--define`s.
fn semantics(env: &[(&[u8], &[u8])], defines: &[(&str, &str)]) -> CommandSemantics {
    let mut argv: Vec<Bytes> = vec![
        Bytes::from_static(b"rush"),
        Bytes::from_static(b"build"),
        Bytes::from_static(b"//foo:bar"),
    ];
    for (k, v) in defines {
        argv.push(Bytes::from(format!("--define={k}={v}").into_bytes()));
    }
    let environment = env
        .iter()
        .map(|(name, value)| EnvironmentVariable {
            name: Bytes::from(name.to_vec()),
            value: Bytes::from(value.to_vec()),
        })
        .collect();
    let req = RunRequest {
        auth_cookie: Bytes::new(),
        argv,
        working_directory: Bytes::from_static(b"/tmp/proj"),
        environment,
        block_for_lock: true,
        client_build_id: "build-1".to_string(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        workspace_config: Vec::new(),
    };
    CommandSemantics::from_run_request(&req).expect("build semantics")
}

fn empty_semantics() -> CommandSemantics {
    semantics(&[], &[])
}

#[test]
#[cfg(target_os = "linux")]
fn linux_proc_filesystems_dependency_is_exact_and_typed() {
    let current = fs::read("/proc/filesystems").unwrap();
    let bytes = u64::try_from(current.len()).unwrap();
    let digest = *blake3::hash(&current).as_bytes();
    let manifest = crate::DependencyManifest::from_deps(vec![
        Dependency::LinuxProcFilesystems { bytes, digest },
    ]);
    let sem = empty_semantics();

    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);

    let wrong_length = crate::DependencyManifest::from_deps(vec![
        Dependency::LinuxProcFilesystems {
            bytes: bytes.checked_add(1).unwrap(),
            digest,
        },
    ]);
    assert_stale_is(
        &wrong_length.validate_staleness(&sem),
        |dep| matches!(dep, Dependency::LinuxProcFilesystems { .. }),
        "LinuxProcFilesystems length stale",
    );

    let wrong_digest = crate::DependencyManifest::from_deps(vec![
        Dependency::LinuxProcFilesystems {
            bytes,
            digest: [0; 32],
        },
    ]);
    assert_stale_is(
        &wrong_digest.validate_staleness(&sem),
        |dep| matches!(dep, Dependency::LinuxProcFilesystems { .. }),
        "LinuxProcFilesystems digest stale",
    );
    assert_ne!(manifest.fingerprint(), wrong_digest.fingerprint());
}

#[test]
#[cfg(target_os = "linux")]
fn linux_proc_vm_overcommit_memory_dependency_is_exact_and_typed() {
    let current = fs::read("/proc/sys/vm/overcommit_memory").unwrap();
    let [value @ b'0'..=b'2', b'\n'] = current.as_slice() else {
        panic!("unexpected Linux overcommit policy value");
    };
    let manifest = crate::DependencyManifest::from_deps(vec![
        Dependency::LinuxProcVmOvercommitMemory { value: *value },
    ]);
    let sem = empty_semantics();

    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);

    let other = b'0' + ((*value - b'0' + 1) % 3);
    let wrong = crate::DependencyManifest::from_deps(vec![
        Dependency::LinuxProcVmOvercommitMemory { value: other },
    ]);
    assert_stale_is(
        &wrong.validate_staleness(&sem),
        |dep| matches!(dep, Dependency::LinuxProcVmOvercommitMemory { .. }),
        "LinuxProcVmOvercommitMemory stale",
    );
    assert_ne!(manifest.fingerprint(), wrong.fingerprint());
}

#[test]
#[cfg(target_os = "linux")]
fn linux_cgroup_v2_cpu_max_dependency_is_exact_and_typed() {
    let Ok(values) = crate::manifest::read_linux_cgroup_v2_cpu_max_values() else {
        return;
    };
    let manifest = crate::DependencyManifest::from_deps(vec![
        Dependency::LinuxCgroupV2CpuMax {
            values: values.clone(),
        },
    ]);
    let sem = empty_semantics();

    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);

    let mut changed_values = values.clone();
    changed_values[0].0 = changed_values[0].0.checked_add(1).unwrap();
    let changed_value = crate::DependencyManifest::from_deps(vec![
        Dependency::LinuxCgroupV2CpuMax {
            values: changed_values,
        },
    ]);
    assert_stale_is(
        &changed_value.validate_staleness(&sem),
        |dep| matches!(dep, Dependency::LinuxCgroupV2CpuMax { .. }),
        "LinuxCgroupV2CpuMax value stale",
    );

    let mut missing_values = values;
    missing_values.pop();
    let missing_value = crate::DependencyManifest::from_deps(vec![
        Dependency::LinuxCgroupV2CpuMax {
            values: missing_values,
        },
    ]);
    assert_stale_is(
        &missing_value.validate_staleness(&sem),
        |dep| matches!(dep, Dependency::LinuxCgroupV2CpuMax { .. }),
        "LinuxCgroupV2CpuMax missing value stale",
    );
    assert_ne!(manifest.fingerprint(), missing_value.fingerprint());
}

fn assert_stale_is<F: Fn(&Dependency) -> bool>(s: &Staleness, pred: F, what: &str) {
    match s {
        Staleness::Stale { dep, reason } => {
            assert!(pred(dep), "expected {what}, got dep {dep:?} (reason: {reason})");
        }
        Staleness::Fresh => panic!("expected Stale ({what}), got Fresh"),
    }
}

/// 1. Nothing changed → Fresh.
#[test]
fn unchanged_is_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    fs::write(&f, b"hello").unwrap();

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    let _ = io.read_to_string(&f).unwrap();
    let _ = io.read_dir_names(dir.path()).unwrap();
    let _ = io.exists(&f);

    let manifest = io.manifest();
    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);
}

/// 2. Change a recorded file's CONTENT (same length) → Stale (FileContent).
#[test]
fn changed_content_same_length_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    fs::write(&f, b"AAAAA").unwrap();

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    let _ = io.read_file(&f).unwrap();
    let manifest = io.manifest();

    // Same length (5 bytes), different content.
    fs::write(&f, b"BBBBB").unwrap();

    let s = manifest.validate_staleness(&sem);
    assert_stale_is(
        &s,
        |d| matches!(d, Dependency::FileContent { .. }),
        "FileContent stale",
    );
}

/// 3. Create a NEW file in a recorded read_dir'd directory → Stale (DirListing).
///    The glob() / package-boundary case (Codex P0 #4).
#[test]
fn new_file_in_globbed_dir_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    let _ = io.read_dir_names(dir.path()).unwrap();
    let manifest = io.manifest();
    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);

    fs::write(dir.path().join("b.txt"), b"b").unwrap();

    let s = manifest.validate_staleness(&sem);
    assert_stale_is(
        &s,
        |d| matches!(d, Dependency::DirListing { .. }),
        "DirListing stale",
    );
}

/// 4. Delete a recorded file → Stale (FileExistence false).
#[test]
fn deleted_recorded_file_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    fs::write(&f, b"x").unwrap();

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    assert!(io.exists(&f));
    let manifest = io.manifest();

    fs::remove_file(&f).unwrap();

    let s = manifest.validate_staleness(&sem);
    assert_stale_is(
        &s,
        |d| matches!(d, Dependency::FileExistence { exists: true, .. }),
        "FileExistence(true) now-absent stale",
    );
}

/// 5. A file recorded ABSENT gets created → Stale (FileExistence flip).
#[test]
fn absent_then_created_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("later.txt");

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    assert!(!io.exists(&f)); // negative probe recorded
    let manifest = io.manifest();
    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);

    fs::write(&f, b"now here").unwrap();

    let s = manifest.validate_staleness(&sem);
    assert_stale_is(
        &s,
        |d| matches!(d, Dependency::FileExistence { exists: false, .. }),
        "FileExistence(false) now-present stale",
    );
}

/// 6. Change a recorded symlink's target → Stale (Symlink).
#[test]
#[cfg(unix)]
fn changed_symlink_target_is_stale() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("t1"), b"1").unwrap();
    fs::write(dir.path().join("t2"), b"2").unwrap();
    let link = dir.path().join("link");
    symlink(Path::new("t1"), &link).unwrap();

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    let target = io.read_link(&link);
    assert_eq!(target.as_deref(), Some(Path::new("t1")));
    let manifest = io.manifest();
    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);

    fs::remove_file(&link).unwrap();
    symlink(Path::new("t2"), &link).unwrap();

    let s = manifest.validate_staleness(&sem);
    assert_stale_is(
        &s,
        |d| matches!(d, Dependency::Symlink { .. }),
        "Symlink stale",
    );
}

/// 6b. tracked::canonicalize resolves a symlinked path identically to
/// fs::canonicalize, records a Symlink dep per resolved hop, and a swap of the
/// symlink target flips the manifest to Stale (Symlink).
#[test]
#[cfg(unix)]
fn tracked_canonicalize_records_symlink_hops_and_matches_std() {
    use std::os::unix::fs::symlink;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    // real/leaf is a regular file; "link" -> "real" (a directory symlink).
    fs::create_dir(dir.path().join("real")).unwrap();
    fs::write(dir.path().join("real").join("leaf"), b"x").unwrap();
    fs::create_dir(dir.path().join("other")).unwrap();
    fs::write(dir.path().join("other").join("leaf"), b"y").unwrap();
    let link = dir.path().join("link");
    symlink(Path::new("real"), &link).unwrap();

    // Canonicalize <dir>/link/leaf through the tracked IO under a scope.
    let sem = empty_semantics();
    let io: crate::tracked::SharedTrackedIo =
        Arc::new(RealTrackedIo::new(sem.clone(), dir.path()));
    let probe = link.join("leaf");
    let tracked_resolved =
        crate::tracked::scope(io.clone(), || crate::tracked::canonicalize(&probe)).unwrap();

    // Identical resolution to std::fs::canonicalize (logic unchanged).
    let std_resolved = fs::canonicalize(&probe).unwrap();
    assert_eq!(tracked_resolved, std_resolved);

    // A Symlink dep was recorded for the resolved hop.
    let manifest = io.manifest();
    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);

    // Swap the symlink target → graph must be Stale on the Symlink dep.
    fs::remove_file(&link).unwrap();
    symlink(Path::new("other"), &link).unwrap();
    let s = manifest.validate_staleness(&sem);
    assert_stale_is(
        &s,
        |d| matches!(d, Dependency::Symlink { .. }),
        "Symlink stale after canonicalize target swap",
    );
}

#[test]
fn refresh_filesystem_observations_sees_created_repository_and_keeps_negative_evidence() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let sem = empty_semantics();
    let io: crate::tracked::SharedTrackedIo =
        Arc::new(RealTrackedIo::new(sem.clone(), dir.path()));
    let external = dir.path().join("output/external");
    let module = external.join("source/defs.bzl");
    crate::tracked::scope(io.clone(), || {
        assert_eq!(crate::tracked::canonicalize(&module).unwrap_err().kind(),
            std::io::ErrorKind::NotFound);
        let before = io.manifest();
        fs::create_dir_all(module.parent().unwrap()).unwrap();
        fs::write(&module, b"VALUE = 41\n").unwrap();
        // Reproduce the memoized-absence failure before the producer boundary.
        assert!(crate::tracked::canonicalize(&module).is_err());
        crate::tracked::refresh_filesystem_observations();
        assert_eq!(crate::tracked::canonicalize(&module).unwrap(),
            fs::canonicalize(&module).unwrap());
        assert_eq!(crate::tracked::read_file(&module).unwrap().as_ref(), b"VALUE = 41\n");
        let after = io.manifest();
        for dep in before.deps() { assert!(after.deps().contains(dep)); }
        assert!(matches!(after.validate_staleness(&sem), Staleness::Stale { .. }));
        assert!(after.deps().iter().any(|dep| matches!(dep,
            Dependency::FileExistence { path, exists: true, .. } if path == &external)));
    });
    let next: crate::tracked::SharedTrackedIo =
        Arc::new(RealTrackedIo::new(sem.clone(), dir.path()));
    crate::tracked::scope(next.clone(), || {
        crate::tracked::canonicalize(&module).unwrap();
        crate::tracked::read_file(&module).unwrap();
    });
    assert_eq!(next.manifest().validate_staleness(&sem), Staleness::Fresh);
}

#[test]
#[cfg(unix)]
fn refresh_filesystem_observations_does_not_reuse_a_replaced_symlink() {
    use std::os::unix::fs::symlink;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let sem = empty_semantics();
    let io: crate::tracked::SharedTrackedIo =
        Arc::new(RealTrackedIo::new(sem.clone(), dir.path()));
    for name in ["first", "second", "third"] {
        fs::create_dir(dir.path().join(name)).unwrap();
        fs::write(dir.path().join(name).join("defs.bzl"), name).unwrap();
    }
    let link = dir.path().join("source");
    symlink("first", &link).unwrap();
    crate::tracked::scope(io.clone(), || {
        let module = link.join("defs.bzl");
        assert!(crate::tracked::canonicalize(&module).unwrap().ends_with("first/defs.bzl"));
        let before = io.manifest();
        fs::remove_file(&link).unwrap();
        symlink("second", &link).unwrap();
        crate::tracked::refresh_filesystem_observations();
        assert!(crate::tracked::canonicalize(&module).unwrap().ends_with("second/defs.bzl"));
        // Live mode remains in force for subsequent producer mutations.
        fs::remove_file(&link).unwrap();
        symlink("third", &link).unwrap();
        assert!(crate::tracked::canonicalize(&module).unwrap().ends_with("third/defs.bzl"));
        let after = io.manifest();
        for dep in before.deps() { assert!(after.deps().contains(dep)); }
        assert!(matches!(after.validate_staleness(&sem), Staleness::Stale { .. }));
    });
}

/// 7. Change a recorded env var (two different CommandSemantics) → Stale (Env).
#[test]
fn changed_env_var_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let _ = dir; // unused tree; this test is env-only

    let sem_a = semantics(&[(b"CC", b"clang")], &[]);
    let io = RealTrackedIo::new(sem_a.clone(), dir.path());
    let v = io.getenv(b"CC");
    assert_eq!(v.as_deref(), Some(&b"clang"[..]));
    let manifest = io.manifest();

    // Same semantics → fresh.
    assert_eq!(manifest.validate_staleness(&sem_a), Staleness::Fresh);

    // A DIFFERENT command's semantics (CC changed clang -> gcc) → stale.
    let sem_b = semantics(&[(b"CC", b"gcc")], &[]);
    let s = manifest.validate_staleness(&sem_b);
    assert_stale_is(&s, |d| matches!(d, Dependency::Env { .. }), "Env stale");

    // And a command where CC is unset entirely → also stale (Some -> None).
    let sem_c = empty_semantics();
    let s2 = manifest.validate_staleness(&sem_c);
    assert_stale_is(&s2, |d| matches!(d, Dependency::Env { .. }), "Env unset stale");
}

/// 7b. Build option (`--define`) change → Stale (Opt).
#[test]
fn changed_option_is_stale() {
    let sem_a = semantics(&[], &[("GPU", "1")]);
    let dir = tempfile::tempdir().unwrap();
    let io = RealTrackedIo::new(sem_a.clone(), dir.path());
    let v = io.get_option("GPU");
    assert_eq!(v.as_deref(), Some("1"));
    let manifest = io.manifest();
    assert_eq!(manifest.validate_staleness(&sem_a), Staleness::Fresh);

    let sem_b = semantics(&[], &[("GPU", "0")]);
    let s = manifest.validate_staleness(&sem_b);
    assert_stale_is(&s, |d| matches!(d, Dependency::Opt { .. }), "Opt stale");
}

/// 8. Determinism: identical re-read → identical fingerprint; reordered reads of
///    the same observations → SAME fingerprint (manifest is sorted).
#[test]
fn fingerprint_is_deterministic_and_order_independent() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    fs::write(&a, b"alpha").unwrap();
    fs::write(&b, b"beta").unwrap();

    // Reader 1: a, then b, then dir, then env.
    let io1 = RealTrackedIo::new(semantics(&[(b"K", b"v")], &[]), dir.path());
    let _ = io1.read_file(&a).unwrap();
    let _ = io1.read_file(&b).unwrap();
    let _ = io1.read_dir_names(dir.path()).unwrap();
    let _ = io1.getenv(b"K");
    let fp1 = io1.manifest().fingerprint();

    // Reader 2: SAME observations, reverse order, with a duplicate read of a.
    let io2 = RealTrackedIo::new(semantics(&[(b"K", b"v")], &[]), dir.path());
    let _ = io2.getenv(b"K");
    let _ = io2.read_dir_names(dir.path()).unwrap();
    let _ = io2.read_file(&b).unwrap();
    let _ = io2.read_file(&a).unwrap();
    let _ = io2.read_file(&a).unwrap(); // duplicate — must dedup away
    let fp2 = io2.manifest().fingerprint();

    assert_eq!(fp1, fp2, "fingerprint must be order-independent and dedup duplicates");
    assert_eq!(io1.manifest().len(), io2.manifest().len(), "dedup -> same dep count");

    // A genuinely different observation set → different fingerprint.
    let io3 = RealTrackedIo::new(semantics(&[(b"K", b"DIFFERENT")], &[]), dir.path());
    let _ = io3.read_file(&a).unwrap();
    let _ = io3.read_file(&b).unwrap();
    let _ = io3.read_dir_names(dir.path()).unwrap();
    let _ = io3.getenv(b"K");
    assert_ne!(fp1, io3.manifest().fingerprint(), "different env value -> different fp");
}

// ---------------------------------------------------------------------------
// Hardening fixes (Codex review of commit 6112aac).
// ---------------------------------------------------------------------------

/// Fix 1: a FAILED read records a negative existence probe, so a later-created
/// file flips to Stale (the BUILD → BUILD.bazel fallback hole).
#[test]
fn failed_read_records_negative_probe() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("BUILD");

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    assert!(io.read_file(&missing).is_err()); // NotFound → negative probe
    let manifest = io.manifest();
    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);

    // The file analysis fell back from now appears → must be Stale.
    fs::write(&missing, b"cc_library(name='x')").unwrap();
    let s = manifest.validate_staleness(&sem);
    assert_stale_is(
        &s,
        |d| matches!(d, Dependency::FileExistence { exists: false, .. }),
        "negative read probe now-present stale",
    );
}

/// Fix 1 (listing): a FAILED read_dir records a negative probe; a dir that later
/// appears flips to Stale.
#[test]
fn failed_listing_records_negative_probe() {
    let dir = tempfile::tempdir().unwrap();
    let missing_dir = dir.path().join("pkg");

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    assert!(io.read_dir_names(&missing_dir).is_err());
    let manifest = io.manifest();
    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);

    fs::create_dir(&missing_dir).unwrap();
    let s = manifest.validate_staleness(&sem);
    assert_stale_is(
        &s,
        |d| matches!(d, Dependency::FileExistence { exists: false, .. }),
        "negative listing probe now-present stale",
    );
}

/// Fix 2: a path recorded via a RELATIVE argument is normalized to absolute, so
/// validate() run from a DIFFERENT process cwd still re-reads the right file.
#[test]
fn relative_path_is_cwd_independent() {
    let dir = tempfile::tempdir().unwrap();
    // Canonicalize the root: tempdir on some platforms (macOS) is itself under a
    // symlink, and we compare the recorded absolute path below.
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::write(root.join("a.txt"), b"hello").unwrap();

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), &root);
    // Record with a RELATIVE path; it must be joined to the root.
    let _ = io.read_file(Path::new("a.txt")).unwrap();
    let manifest = io.manifest();

    // Recorded path is absolute and rooted at the workspace.
    let recorded = manifest
        .deps()
        .iter()
        .find_map(|d| match d {
            Dependency::FileContent { path, .. } => Some(path.clone()),
            _ => None,
        })
        .expect("a FileContent dep");
    assert!(recorded.is_absolute(), "recorded path must be absolute");
    assert_eq!(recorded, root.join("a.txt"));

    // Validate from a DIFFERENT cwd. The absolute path makes this cwd-independent.
    let prev = std::env::current_dir().unwrap();
    let other = tempfile::tempdir().unwrap();
    std::env::set_current_dir(other.path()).unwrap();
    let verdict = manifest.validate_staleness(&sem);
    std::env::set_current_dir(&prev).unwrap();
    assert_eq!(verdict, Staleness::Fresh, "must validate the rooted file, not a cwd-relative one");
}

/// Fix 3: there is no raw-CommandSemantics accessor; analysis depends on the whole
/// config only via config_fingerprint(), recorded as a ConfigFingerprint dep that
/// flips Stale on any config change.
#[test]
fn config_fingerprint_is_tracked_and_invalidates() {
    let dir = tempfile::tempdir().unwrap();
    let sem_a = semantics(&[(b"CC", b"clang")], &[("GPU", "1")]);
    let io = RealTrackedIo::new(sem_a.clone(), dir.path());
    let fp = io.config_fingerprint();
    assert_eq!(fp, *sem_a.config_fingerprint());
    let manifest = io.manifest();

    // Same config → Fresh.
    assert_eq!(manifest.validate_staleness(&sem_a), Staleness::Fresh);

    // Any config change (here an env value) → Stale via ConfigFingerprint.
    let sem_b = semantics(&[(b"CC", b"gcc")], &[("GPU", "1")]);
    let s = manifest.validate_staleness(&sem_b);
    assert_stale_is(
        &s,
        |d| matches!(d, Dependency::ConfigFingerprint { .. }),
        "ConfigFingerprint stale",
    );
}

#[test]
fn command_environment_snapshots_are_immutable_and_tracked() {
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let sem_a = semantics(&[
        (b"Z", b"old"),
        (b"A", b"first"),
        (b"RAW_\xff", b"value_\xfe"),
        (b"Z", b"client-a"),
    ], &[]);
    let sem_b = semantics(&[(b"Z", b"client-b"), (b"A", b"first")], &[]);
    let sem_default = empty_semantics();
    let io_a = Arc::new(RealTrackedIo::new(sem_a.clone(), dir.path()));
    let io_b = Arc::new(RealTrackedIo::new(sem_b.clone(), dir.path()));
    let io_default = Arc::new(RealTrackedIo::new(sem_default.clone(), dir.path()));
    let expected_a = vec![
        (b"A".to_vec(), b"first".to_vec()),
        (b"RAW_\xff".to_vec(), b"value_\xfe".to_vec()),
        (b"Z".to_vec(), b"client-a".to_vec()),
    ];
    crate::tracked::scope(io_a.clone(), || {
        let mut returned = crate::tracked::command_environment().unwrap();
        assert_eq!(returned, expected_a);
        returned[0].1.clear();
        crate::tracked::scope(io_b.clone(), || {
            assert_eq!(crate::tracked::command_environment().unwrap(), vec![
                (b"A".to_vec(), b"first".to_vec()),
                (b"Z".to_vec(), b"client-b".to_vec()),
            ]);
        });
        assert_eq!(crate::tracked::command_environment().unwrap(), expected_a);
        crate::tracked::scope(io_default.clone(), || {
            assert!(crate::tracked::command_environment().unwrap().is_empty());
        });
        assert_eq!(crate::tracked::command_environment().unwrap(), expected_a);
    });
    assert!(crate::tracked::current().is_none());
    for (io, sem) in [(&io_a, &sem_a), (&io_b, &sem_b), (&io_default, &sem_default)] {
        let manifest = io.manifest();
        let mut expected = vec![Dependency::ConfigFingerprint {
            digest: *sem.config_fingerprint(),
        }];
        expected.extend(rush_command_core::CONFIG_FINGERPRINT_EXCLUDED_ENV.iter()
            .map(|name| Dependency::Env { name: name.to_vec(), value: None }));
        assert_eq!(manifest.deps(), crate::DependencyManifest::from_deps(expected).deps());
        assert_eq!(manifest.validate_staleness(sem), Staleness::Fresh);
    }
    let manifest_a = io_a.manifest();
    for changed in [&sem_b, &sem_default] {
        assert_stale_is(
            &manifest_a.validate_staleness(changed),
            |dep| matches!(dep, Dependency::ConfigFingerprint { .. }),
            "captured command environment changed",
        );
    }
    assert_stale_is(
        &io_b.manifest().validate_staleness(&sem_default),
        |dep| matches!(dep, Dependency::ConfigFingerprint { .. }),
        "omitted client environment resets to empty",
    );
}

#[test]
fn command_environment_tracks_fingerprint_excluded_keys_including_absence() {
    let dir = tempfile::tempdir().unwrap();
    for name in rush_command_core::CONFIG_FINGERPRINT_EXCLUDED_ENV {
        let requests = [
            semantics(&[(b"ORDINARY", b"stable")], &[]),
            semantics(&[(b"ORDINARY", b"stable"), (name, b"first")], &[]),
            semantics(&[(b"ORDINARY", b"stable"), (name, b"second")], &[]),
        ];
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(request.config_fingerprint(), requests[0].config_fingerprint());
            let io = RealTrackedIo::new(request.clone(), dir.path());
            let environment = io.command_environment().unwrap();
            assert_eq!(environment, request.env().iter()
                .map(|(key, value)| (key.to_vec(), value.to_vec())).collect::<Vec<_>>());
            let mut expected = vec![Dependency::ConfigFingerprint {
                digest: *request.config_fingerprint(),
            }];
            expected.extend(rush_command_core::CONFIG_FINGERPRINT_EXCLUDED_ENV.iter()
                .map(|key| Dependency::Env {
                    name: key.to_vec(),
                    value: request.getenv(key).map(<[u8]>::to_vec),
                }));
            let manifest = io.manifest();
            assert_eq!(manifest.deps(), crate::DependencyManifest::from_deps(expected).deps());
            for (other_index, other) in requests.iter().enumerate() {
                if index == other_index {
                    assert_eq!(manifest.validate_staleness(other), Staleness::Fresh);
                } else {
                    assert_stale_is(
                        &manifest.validate_staleness(other),
                        |dep| matches!(dep, Dependency::Env { name: key, .. } if key.as_slice() == *name),
                        "diagnostic environment presence or value changed",
                    );
                }
            }
        }
    }
}

#[test]
fn command_environment_default_is_unsupported() {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use crate::{DependencyManifest, FileKind, ProcessProbeResult, Validated};

    // Leave command_environment unimplemented, as existing IO test doubles do.
    // No other accessor may be called to invent a fallback environment.
    struct NoCommandEnvironment;
    impl TrackedIo for NoCommandEnvironment {
        fn read_file(&self, _: &Path) -> std::io::Result<Bytes> { unreachable!() }
        fn read_to_string(&self, _: &Path) -> std::io::Result<String> { unreachable!() }
        fn exists(&self, _: &Path) -> bool { unreachable!() }
        fn metadata_kind(&self, _: &Path) -> Option<FileKind> { unreachable!() }
        fn metadata_full(&self, _: &Path) -> bool { unreachable!() }
        fn read_link(&self, _: &Path) -> Option<PathBuf> { unreachable!() }
        fn read_dir_names(&self, _: &Path) -> std::io::Result<Vec<OsString>> { unreachable!() }
        fn getenv(&self, _: &[u8]) -> Option<Bytes> { unreachable!() }
        fn get_option(&self, _: &str) -> Option<String> { unreachable!() }
        fn config_fingerprint(&self) -> [u8; 32] { unreachable!() }
        fn probe_process(&self, _: &Path, _: &[Vec<u8>], _: &[Vec<u8>], _: &Path)
            -> std::io::Result<ProcessProbeResult> { unreachable!() }
        fn probe_process_stdout(&self, _: &Path, _: &[Vec<u8>], _: &[Vec<u8>], _: &Path)
            -> std::io::Result<ProcessProbeResult> { unreachable!() }
        fn manifest(&self) -> DependencyManifest { unreachable!() }
        fn mark(&self) -> usize { unreachable!() }
        fn captured_since(&self, _: usize) -> Vec<Dependency> { unreachable!() }
        fn record_deps(&self, _: &[Dependency]) { unreachable!() }
        fn validate_manifest(&self, _: &DependencyManifest) -> Validated { unreachable!() }
    }
    let io: crate::tracked::SharedTrackedIo = std::sync::Arc::new(NoCommandEnvironment);
    let error = crate::tracked::scope(io.clone(), crate::tracked::command_environment).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    let error = crate::tracked::scope(io.clone(), || crate::tracked::fresh_canonical_regular_file_metadata(Path::new("/unused"))).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    let error = crate::tracked::scope(io, || crate::tracked::read_canonical_regular_file(Path::new("/unused"))).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

/// Fix 4a: chmod on a recorded file changes its metadata → Stale (FileMetadata),
/// even though the content is byte-identical.
#[test]
#[cfg(unix)]
fn chmod_changes_metadata_is_stale() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let tool = dir.path().join("protoc");
    fs::write(&tool, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o644)).unwrap();

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    assert!(io.metadata_full(&tool));
    let manifest = io.manifest();
    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);

    // chmod +x — content unchanged, mode changed.
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let s = manifest.validate_staleness(&sem);
    assert_stale_is(
        &s,
        |d| matches!(d, Dependency::FileMetadata { .. }),
        "FileMetadata mode-change stale",
    );
}

/// Fix 4b: a regular file swapped for a symlink with IDENTICAL bytes is caught,
/// because read_file ALSO records the path kind.
#[test]
#[cfg(unix)]
fn file_to_symlink_swap_identical_bytes_is_stale() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("real.txt");
    fs::write(&target, b"same-bytes").unwrap();
    let observed = dir.path().join("observed.txt");
    fs::write(&observed, b"same-bytes").unwrap();

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    let _ = io.read_file(&observed).unwrap(); // records content + kind=File
    let manifest = io.manifest();
    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);

    // Replace the regular file with a symlink to identical content.
    fs::remove_file(&observed).unwrap();
    symlink(&target, &observed).unwrap();
    let s = manifest.validate_staleness(&sem);
    assert_stale_is(
        &s,
        |d| matches!(d, Dependency::FileExistence { kind: Some(crate::FileKind::File), .. }),
        "file->symlink kind-change stale",
    );
}

/// Fix 5: a ProcessProbe whose underlying tool output changes → Stale; unchanged
/// → Fresh. The probe is really run and really re-run on validation.
#[test]
#[cfg(unix)]
fn process_probe_detects_output_change() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    // A tiny tool that cats a sibling "answer" file — so we can change its output
    // without changing the program itself.
    let tool = dir.path().join("probe.sh");
    fs::write(&tool, b"#!/bin/sh\ncat \"$(dirname \"$0\")/answer\"\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let answer = dir.path().join("answer");
    fs::write(&answer, b"v1\n").unwrap();

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    let res = io
        .probe_process(&tool, &[], &[], dir.path())
        .expect("probe runs");
    assert_eq!(res.status, 0);
    assert_eq!(res.stdout, b"v1\n");
    assert!(res.stderr.is_empty());
    let manifest = io.manifest();

    // Re-run identical → Fresh.
    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);

    // Underlying tool output changes → Stale (stdout digest differs).
    fs::write(&answer, b"v2\n").unwrap();
    let s = manifest.validate_staleness(&sem);
    assert_stale_is(
        &s,
        |d| matches!(d, Dependency::ProcessProbe { .. }),
        "ProcessProbe stdout-change stale",
    );
}

#[test]
#[cfg(unix)]
fn stdout_only_process_probe_ignores_stderr_and_validates_once() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("count");
    let tool = dir.path().join("stdout-probe.sh");
    let script = format!(
        "#!/bin/sh\ncounter='{}'\ncount=0\nif [ -f \"$counter\" ]; then IFS= read -r count < \"$counter\"; fi\ncount=$((count + 1))\nprintf '%s\\n' \"$count\" > \"$counter\"\nprintf 'stable\\n'\nprintf 'lifecycle-%s\\n' \"$count\" >&2\n",
        counter.display()
    );
    fs::write(&tool, script).unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    let first = io
        .probe_process_stdout(&tool, &[], &[], dir.path())
        .expect("stdout-only probe runs");
    assert_eq!(first.stdout, b"stable\n");
    assert_eq!(first.stderr, b"lifecycle-1\n");
    let manifest = io.manifest();

    let validated = manifest.validate(&sem);
    assert_eq!(validated.staleness, Staleness::Fresh);
    assert_eq!(
        fs::read_to_string(&counter).unwrap(),
        "2\n",
        "validate must run a process probe once, not again while refreshing"
    );
    assert_eq!(validated.refreshed, manifest);

    assert_eq!(
        validated.refreshed.validate(&sem).staleness,
        Staleness::Fresh
    );
    assert_eq!(fs::read_to_string(&counter).unwrap(), "3\n");
}

/// Fix 6: validate() returns a refreshed manifest with the SAME fingerprint after
/// a metadata-only touch that leaves content identical, so the stored manifest
/// carries up-to-date metadata and content is not re-hashed forever.
#[test]
fn validate_returns_refreshed_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    fs::write(&f, b"contents").unwrap();

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    let _ = io.read_file(&f).unwrap();
    let _ = io.read_dir_names(dir.path()).unwrap();
    let manifest = io.manifest();
    let fp_before = manifest.fingerprint();

    let validated = manifest.validate(&sem);
    assert_eq!(validated.staleness, Staleness::Fresh);
    // The refreshed manifest is equivalent (same fingerprint) — refresh re-captures
    // content/listing digests, which are unchanged for a Fresh manifest.
    assert_eq!(
        validated.refreshed.fingerprint(),
        fp_before,
        "refreshed manifest keeps a stable fingerprint"
    );

    // And it remains usable: validating the refreshed manifest is still Fresh.
    assert_eq!(
        validated.refreshed.validate(&sem).staleness,
        Staleness::Fresh
    );
}

/// Fix 4 (negative metadata): a path observed absent via metadata_full that later
/// appears → Stale.
#[test]
fn metadata_full_negative_probe_flips() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("later.bin");

    let sem = empty_semantics();
    let io = RealTrackedIo::new(sem.clone(), dir.path());
    assert!(!io.metadata_full(&f)); // absent
    let manifest = io.manifest();
    assert_eq!(manifest.validate_staleness(&sem), Staleness::Fresh);

    fs::write(&f, b"x").unwrap();
    let s = manifest.validate_staleness(&sem);
    assert_stale_is(
        &s,
        |d| matches!(d, Dependency::FileMetadata { present: false, .. }),
        "FileMetadata absent->present stale",
    );
}

#[test]
#[cfg(unix)]
fn canonical_regular_file_metadata_is_fresh_and_records_negative_nonregular_inputs() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tool");
    fs::write(&file, b"same bytes").unwrap();
    let io = RealTrackedIo::new(semantics(&[], &[]), dir.path());
    assert!(io.exists(&file));
    let first = io
        .fresh_canonical_regular_file_metadata(&file)
        .unwrap()
        .unwrap();
    let replacement = dir.path().join("new");
    fs::write(&replacement, b"same bytes").unwrap();
    fs::rename(replacement, &file).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).unwrap();
    let second = io
        .fresh_canonical_regular_file_metadata(&file)
        .unwrap()
        .unwrap();
    assert_ne!(first.ino, second.ino);
    assert_eq!(second.mode & 0o777, 0o700);
    assert!(
        matches!(
            io.manifest().validate_staleness(&semantics(&[], &[])),
            Staleness::Stale { .. }
        ),
        "both old and current observations must remain, rather than replacing old dependencies"
    );
    let missing = dir.path().join("missing");
    assert_eq!(
        io.fresh_canonical_regular_file_metadata(&missing)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(io.manifest().deps().iter().any(|dep| matches!(dep,
        Dependency::FileMetadata { path, present: false, .. } if path == &missing)));
    assert!(io
        .fresh_canonical_regular_file_metadata(dir.path())
        .unwrap()
        .is_none());
    let alias = dir.path().join("alias");
    symlink(&file, &alias).unwrap();
    assert!(io
        .fresh_canonical_regular_file_metadata(&alias)
        .unwrap()
        .is_none());
    assert!(io.manifest().deps().iter().any(|dep| matches!(dep,
        Dependency::FileMetadata { path, present: true, mode, .. } if path == &alias && mode & 0o170000 == 0o120000)));
    assert_eq!(
        io.read_canonical_regular_file(&alias).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[test]
#[cfg(unix)]
fn canonical_regular_file_read_records_descriptor_bound_bytes_metadata_and_kind() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tool");
    fs::write(&file, b"exact tool bytes").unwrap();
    let io = RealTrackedIo::new(semantics(&[], &[]), dir.path());
    let read = io.read_canonical_regular_file(&file).unwrap().unwrap();
    assert_eq!(read.contents.as_ref(), b"exact tool bytes");
    assert_eq!(read.metadata.len, 16);
    let deps = io.manifest();
    assert_eq!(deps.len(), 3);
    assert!(deps.deps().iter().any(|dep| matches!(dep,
        Dependency::FileContent { path, digest, .. } if path == &file && *digest == crate::content_digest(b"exact tool bytes"))));
    assert!(deps.deps().iter().any(|dep| matches!(dep,
        Dependency::FileExistence { path, exists: true, kind: Some(crate::FileKind::File) } if path == &file)));
    assert_eq!(
        read.metadata,
        io.fresh_canonical_regular_file_metadata(&file)
            .unwrap()
            .unwrap()
    );
    assert_eq!(
        deps.validate_staleness(&semantics(&[], &[])),
        Staleness::Fresh
    );
}

#[test]
#[cfg(unix)]
fn canonical_regular_file_read_rejects_parent_directory_rename_aba() {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let active = dir.path().join("active");
    let other = dir.path().join("other");
    let saved = dir.path().join("saved");
    fs::create_dir(&active).unwrap();
    fs::create_dir(&other).unwrap();
    let file = active.join("tool");
    fs::write(&file, b"first").unwrap();
    fs::write(other.join("tool"), b"other").unwrap();
    let io = RealTrackedIo::new(semantics(&[], &[]), dir.path());
    let before = io
        .fresh_canonical_regular_file_metadata(&file)
        .unwrap()
        .unwrap();
    let error = io
        .read_canonical_regular_file_with_open_for_test(&file, |path| {
            // Change only the parent directory identity: restoring it leaves the
            // original file's full stat unchanged, defeating path-only pre/post
            // checks. The descriptor really opens the other inode during the ABA.
            fs::rename(&active, &saved)?;
            fs::rename(&other, &active)?;
            let opened = RealTrackedIo::open_canonical_regular_candidate_for_test(path)?;
            fs::rename(&active, &other)?;
            fs::rename(&saved, &active)?;
            let mut captured = Vec::new();
            opened.try_clone()?.read_to_end(&mut captured)?;
            assert_eq!(captured, b"other");
            Ok(opened)
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("changed before descriptor-bound read"),
        "{error}"
    );
    let after = io
        .fresh_canonical_regular_file_metadata(&file)
        .unwrap()
        .unwrap();
    assert_eq!(
        before, after,
        "this control must reproduce identical pre/post pathname stat"
    );
    assert_eq!(fs::read(&file).unwrap(), b"first");
    assert!(
        !io.manifest()
            .deps()
            .iter()
            .any(|dep| matches!(dep, Dependency::FileContent { .. })),
        "bytes from the transient inode must not be admitted under restored metadata"
    );
}

#[test]
#[cfg(unix)]
fn canonical_regular_file_open_races_refuse_fifo_and_symlink_without_blocking() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    const CHILD_ROOT: &str = "RUSH_CANONICAL_FILE_KIND_RACE_CHILD";
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        // Contain the race test in its own child: a regression to blocking
        // File::open must fail this test, not strand a Cargo worker on a FIFO.
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::canonical_regular_file_open_races_refuse_fifo_and_symlink_without_blocking",
                "--nocapture",
            ])
            .env(CHILD_ROOT, dir.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.try_wait().unwrap().is_some() {
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "kind-race child failed: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "kind-race child blocked: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        return;
    };
    let root = std::path::PathBuf::from(root);
    let known_fifo = root.join("known-fifo");
    let cpath = std::ffi::CString::new(known_fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    let io = RealTrackedIo::new(semantics(&[], &[]), &root);
    assert_eq!(
        io.read_canonical_regular_file(&known_fifo)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput,
        "a known FIFO must be an error, never an unsupported-capability fallback"
    );
    assert!(io.manifest().deps().iter().any(|dep| matches!(dep,
        Dependency::FileMetadata { path, present: true, mode, .. } if path == &known_fifo && mode & 0o170000 == 0o010000)));
    let refused = root.join("filesystem-refusal");
    fs::write(&refused, b"regular tool").unwrap();
    let error = io
        .read_canonical_regular_file_with_open_for_test(&refused, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "filesystem refused the actual open",
            ))
        })
        .unwrap_err();
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::Other,
        "a capable backend's I/O refusal must not authorize an ordinary-read fallback"
    );
    let other = root.join("other");
    fs::write(&other, b"other bytes").unwrap();
    for kind in ["fifo", "symlink"] {
        let file = root.join(kind);
        fs::write(&file, b"regular tool").unwrap();
        let io = RealTrackedIo::new(semantics(&[], &[]), &root);
        let error = io
            .read_canonical_regular_file_with_open_for_test(&file, |path| {
                fs::remove_file(path)?;
                if kind == "fifo" {
                    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                    if unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) } != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else {
                    symlink(&other, path)?;
                }
                // Invoke the exact production nonblocking/no-follow opener after
                // the precheck. There is deliberately no FIFO writer anywhere.
                RealTrackedIo::open_canonical_regular_candidate_for_test(path)
            })
            .unwrap_err();
        if kind == "symlink" {
            assert_eq!(error.raw_os_error(), Some(libc::ELOOP));
        } else {
            assert!(
                error
                    .to_string()
                    .contains("changed before descriptor-bound read"),
                "{error}"
            );
        }
        assert!(
            !io.manifest()
                .deps()
                .iter()
                .any(|dep| matches!(dep, Dependency::FileContent { .. })),
            "a swapped nonregular file must be refused before any byte read"
        );
    }
}
