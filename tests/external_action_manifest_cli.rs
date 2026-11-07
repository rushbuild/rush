// Rush - External action-manifest CLI integration tests
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

#[cfg(unix)]
fn inherit_fd(command: &mut Command, fd: libc::c_int) {
    use std::os::unix::process::CommandExt;

    // Clear CLOEXEC only in the forked child. Changing the parent descriptor
    // would let an unrelated concurrently spawned test process inherit it.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn roots(temp: &TempDir) -> (PathBuf, PathBuf) {
    let src = temp.path().join("source");
    let obj = temp.path().join("output");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&obj).unwrap();
    (src, obj)
}

fn write_manifest(path: &Path, src: &Path, obj: &Path, cacheable: bool) {
    let records = [
        json!({
            "record": "header",
            "schema": "org.kernel.kbuild.actions",
            "version": 1,
            "roots": {
                "src": src,
                "obj": obj,
                "tools": {"shell": "/bin"}
            },
            "preexisting_obj_inputs": []
        }),
        json!({
            "record": "action",
            "id": "write-result",
            "mnemonic": "Generate",
            "commands": [{
                "program": {"root": "shell", "path": "sh"},
                "argv": ["-c", "printf cli-first; printf cli-warning >&2; printf cli-ok > result.txt"]
            }, {
                "program": {"root": "shell", "path": "sh"},
                "argv": ["-c", "printf cli-second"]
            }],
            "cwd": {"root": "obj", "path": "."},
            "env": {},
            "env_policy": "inherit_host",
            "normal_inputs": [{"root": "shell", "path": "sh"}],
            "order_only_inputs": [],
            "outputs": [{
                "path": {"root": "obj", "path": "result.txt"},
                "kind": "file"
            }],
            "resource": "other",
            "depfile": null,
            "config_deps": [],
            "restat": false,
            "always_run": false,
            "cacheable": cacheable
        }),
        json!({"record": "final", "action_count": 1}),
    ];
    let body = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{}\n", body)).unwrap();
}

fn write_lock_child_manifest(path: &Path, src: &Path, obj: &Path) {
    write_manifest(path, src, obj, false);
    let mut records = fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    records[1]["commands"] = json!([{
        "program": {"root": "shell", "path": "sh"},
        "argv": ["-c", "sleep 30 >/dev/null 2>&1 & child=$!; printf '%s' \"$child\" > child.pid; printf cli-ok > result.txt"]
    }]);
    records[1]["outputs"] = json!([{
        "path": {"root": "obj", "path": "result.txt"},
        "kind": "file"
    }, {
        "path": {"root": "obj", "path": "child.pid"},
        "kind": "file"
    }]);
    let body = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{}\n", body)).unwrap();
}

#[test]
fn build_manifest_runs_without_a_workspace() {
    let temp = TempDir::new().unwrap();
    let (src, obj) = roots(&temp);
    let manifest = temp.path().join("actions.jsonl");
    write_manifest(&manifest, &src, &obj, false);

    let output = Command::new(env!("CARGO_BIN_EXE_rush"))
        .current_dir(temp.path())
        .env("RUST_LOG", "error")
        .args(["build-manifest", manifest.to_str().unwrap(), "-j", "1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(obj.join("result.txt")).unwrap(),
        "cli-ok"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cli-firstcli-second"));
    assert!(stdout.contains("Executed 1 actions"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cli-warning"));
}

#[test]
fn json_error_format_emits_a_stable_unsupported_code() {
    let temp = TempDir::new().unwrap();
    let (src, obj) = roots(&temp);
    let manifest = temp.path().join("unsupported.jsonl");
    write_manifest(&manifest, &src, &obj, true);

    let output = Command::new(env!("CARGO_BIN_EXE_rush"))
        .current_dir(temp.path())
        .env("RUST_LOG", "error")
        .args([
            "build-manifest",
            manifest.to_str().unwrap(),
            "--error-format=json",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let diagnostic: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(diagnostic["schema"], "rush.action_manifest.error");
    assert_eq!(diagnostic["version"], 1);
    assert_eq!(diagnostic["code"], "unsupported_cache");
    assert_eq!(diagnostic["feature"], "cacheable");
}

#[test]
fn json_error_format_keeps_successful_command_stdout_out_of_the_diagnostic() {
    let temp = TempDir::new().unwrap();
    let (src, obj) = roots(&temp);
    let manifest = temp.path().join("failed-actions.jsonl");
    write_manifest(&manifest, &src, &obj, false);

    let mut records = fs::read_to_string(&manifest)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    records[1]["commands"] = json!([{
        "program": {"root": "shell", "path": "sh"},
        "argv": ["-c", "printf json-prefix-without-newline"]
    }, {
        "program": {"root": "shell", "path": "sh"},
        "argv": ["-c", "exit 7"]
    }]);
    let body = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&manifest, format!("{}\n", body)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_rush"))
        .current_dir(temp.path())
        .env("RUST_LOG", "error")
        .args([
            "build-manifest",
            manifest.to_str().unwrap(),
            "--error-format=json",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let diagnostic: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(diagnostic["schema"], "rush.action_manifest.error");
    assert_eq!(diagnostic["code"], "execution_failed");
    assert!(!output
        .stdout
        .windows(b"json-prefix-without-newline".len())
        .any(|window| window == b"json-prefix-without-newline"));
    assert!(output
        .stderr
        .windows(b"json-prefix-without-newline".len())
        .any(|window| window == b"json-prefix-without-newline"));
}

#[test]
fn multi_command_action_retains_each_commands_full_log() {
    let temp = TempDir::new().unwrap();
    let (src, obj) = roots(&temp);
    let manifest = temp.path().join("logged-actions.jsonl");
    let log_dir = temp.path().join("logs");
    write_manifest(&manifest, &src, &obj, false);

    let output = Command::new(env!("CARGO_BIN_EXE_rush"))
        .current_dir(temp.path())
        .env("RUST_LOG", "error")
        .env("RUSH_ACTION_LOG_DIR", &log_dir)
        .args(["build-manifest", manifest.to_str().unwrap(), "-j", "1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let mut stdout_logs = fs::read_dir(&log_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".stdout.log"))
        .map(|path| fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>();
    stdout_logs.sort();
    assert_eq!(stdout_logs, ["cli-first", "cli-second"]);
}

#[cfg(unix)]
fn assert_signal_terminates_the_imported_group(signal: i32, exit_code: i32) {
    let temp = TempDir::new().unwrap();
    let (src, obj) = roots(&temp);
    let manifest = temp.path().join("interrupt-actions.jsonl");
    let background_pid = temp.path().join("background.pid");
    write_manifest(&manifest, &src, &obj, false);

    let body = fs::read_to_string(&manifest).unwrap();
    let mut records = body
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    records[1]["commands"] = json!([{
        "program": {"root": "shell", "path": "sh"},
        "argv": [
            "-c",
            format!(
                "sleep 30 & child=$!; printf '%s' \"$child\" > '{}'; wait",
                background_pid.display()
            )
        ]
    }]);
    let body = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&manifest, format!("{}\n", body)).unwrap();

    let mut rush = Command::new(env!("CARGO_BIN_EXE_rush"))
        .current_dir(temp.path())
        .env("RUST_LOG", "error")
        .args(["build-manifest", manifest.to_str().unwrap(), "-j", "1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut pid = None;
    for _ in 0..500 {
        if let Ok(text) = fs::read_to_string(&background_pid) {
            if let Ok(parsed) = text.parse::<i32>() {
                pid = Some(parsed);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = match pid {
        Some(pid) => pid,
        None => {
            let _ = rush.kill();
            let _ = rush.wait();
            panic!("compound shell did not report its background pid");
        }
    };

    assert_eq!(unsafe { libc::kill(rush.id() as i32, signal) }, 0);
    let status = rush.wait().unwrap();
    assert_eq!(status.code(), Some(exit_code));

    let mut gone = false;
    for _ in 0..500 {
        let result = unsafe { libc::kill(pid, 0) };
        if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(gone, "background child {pid} survived Rush interruption");
}

#[cfg(unix)]
#[test]
fn interrupt_terminates_the_imported_compound_shell_group() {
    assert_signal_terminates_the_imported_group(libc::SIGINT, 130);
}

#[cfg(unix)]
#[test]
fn hangup_terminates_the_imported_compound_shell_group() {
    assert_signal_terminates_the_imported_group(libc::SIGHUP, 129);
}

#[cfg(unix)]
#[test]
fn quit_terminates_the_imported_compound_shell_group() {
    assert_signal_terminates_the_imported_group(libc::SIGQUIT, 131);
}

#[cfg(unix)]
#[test]
fn terminate_terminates_the_imported_compound_shell_group() {
    assert_signal_terminates_the_imported_group(libc::SIGTERM, 143);
}

#[cfg(unix)]
#[test]
fn inherited_output_directory_lock_is_retained() {
    use std::os::fd::AsRawFd;

    let temp = TempDir::new().unwrap();
    let (src, obj) = roots(&temp);
    let manifest = temp.path().join("locked-actions.jsonl");
    write_manifest(&manifest, &src, &obj, false);

    let directory = fs::File::open(&obj).unwrap();
    let fd = directory.as_raw_fd();
    // SAFETY: fd is a live directory descriptor owned by `directory`.
    assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);

    let fd_string = fd.to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_rush"));
    command
        .current_dir(temp.path())
        .env("RUST_LOG", "error")
        .args([
            "build-manifest",
            manifest.to_str().unwrap(),
            "-j",
            "1",
            "--output-lock-fd",
            &fd_string,
        ]);
    inherit_fd(&mut command, fd);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(obj.join("result.txt")).unwrap(),
        "cli-ok"
    );

    let competitor = fs::File::open(&obj).unwrap();
    assert_eq!(
        unsafe { libc::flock(competitor.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB,) },
        -1,
        "Rush must not unlock the wrapper's inherited open-file description"
    );
}

#[cfg(unix)]
#[test]
fn action_child_cannot_retain_the_inherited_output_lock() {
    use std::os::fd::AsRawFd;

    let temp = TempDir::new().unwrap();
    let (src, obj) = roots(&temp);
    let manifest = temp.path().join("lock-child-actions.jsonl");
    write_lock_child_manifest(&manifest, &src, &obj);

    let directory = fs::File::open(&obj).unwrap();
    let fd = directory.as_raw_fd();
    assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);

    let mut command = Command::new(env!("CARGO_BIN_EXE_rush"));
    command
        .current_dir(temp.path())
        .env("RUST_LOG", "error")
        .args([
            "build-manifest",
            manifest.to_str().unwrap(),
            "-j",
            "1",
            "--output-lock-fd",
            &fd.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    inherit_fd(&mut command, fd);
    let rush = command.spawn().unwrap();
    drop(directory);
    let output = rush.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let child_pid = fs::read_to_string(obj.join("child.pid"))
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();
    assert_eq!(
        unsafe { libc::kill(child_pid, 0) },
        0,
        "the background child must still be alive when lock release is checked"
    );
    let competitor = fs::File::open(&obj).unwrap();
    let lock_result = unsafe { libc::flock(competitor.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert!(
        lock_result == 0,
        "an action child retained Rush's inherited output-lock descriptor: {}",
        std::io::Error::last_os_error()
    );
    unsafe {
        libc::kill(child_pid, libc::SIGKILL);
    }
}

#[cfg(unix)]
#[test]
fn direct_output_directory_lock_contention_fails_closed() {
    use std::os::fd::AsRawFd;

    let temp = TempDir::new().unwrap();
    let (src, obj) = roots(&temp);
    let manifest = temp.path().join("contended-actions.jsonl");
    write_manifest(&manifest, &src, &obj, false);

    let directory = fs::File::open(&obj).unwrap();
    assert_eq!(
        unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let output = Command::new(env!("CARGO_BIN_EXE_rush"))
        .current_dir(temp.path())
        .env("RUST_LOG", "error")
        .args([
            "build-manifest",
            manifest.to_str().unwrap(),
            "--error-format=json",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let diagnostic: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(diagnostic["code"], "output_root_busy");
    assert!(!obj.join("result.txt").exists());
}

#[cfg(unix)]
#[test]
fn inherited_lock_fd_for_the_wrong_directory_fails_closed() {
    use std::os::fd::AsRawFd;

    let temp = TempDir::new().unwrap();
    let (src, obj) = roots(&temp);
    let manifest = temp.path().join("wrong-lock-actions.jsonl");
    write_manifest(&manifest, &src, &obj, false);

    let wrong_directory = fs::File::open(&src).unwrap();
    let fd = wrong_directory.as_raw_fd();
    assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let mut command = Command::new(env!("CARGO_BIN_EXE_rush"));
    command
        .current_dir(temp.path())
        .env("RUST_LOG", "error")
        .args([
            "build-manifest",
            manifest.to_str().unwrap(),
            "--output-lock-fd",
            &fd.to_string(),
            "--error-format=json",
        ]);
    inherit_fd(&mut command, fd);
    let output = command.output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    let diagnostic: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(diagnostic["code"], "output_root_busy");
    assert!(!obj.join("result.txt").exists());
}

#[cfg(unix)]
#[test]
fn inherited_lock_rejects_a_replaced_output_root() {
    use std::os::fd::AsRawFd;

    let temp = TempDir::new().unwrap();
    let (src, obj) = roots(&temp);
    let manifest = temp.path().join("replaced-root-actions.jsonl");
    write_manifest(&manifest, &src, &obj, false);

    let original_directory = fs::File::open(&obj).unwrap();
    let fd = original_directory.as_raw_fd();
    assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);
    fs::rename(&obj, temp.path().join("old-output")).unwrap();
    fs::create_dir(&obj).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_rush"));
    command
        .current_dir(temp.path())
        .env("RUST_LOG", "error")
        .args([
            "build-manifest",
            manifest.to_str().unwrap(),
            "--output-lock-fd",
            &fd.to_string(),
            "--error-format=json",
        ]);
    inherit_fd(&mut command, fd);
    let output = command.output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    let diagnostic: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(diagnostic["code"], "output_root_busy");
    assert!(!obj.join("result.txt").exists());
}
