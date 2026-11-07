//! The one-shot CLI must load a build workspace exactly once.

use std::process::Command;

#[test]
fn one_shot_analysis_loads_workspace_once() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("WORKSPACE"), b"# minimal workspace\n").unwrap();
    std::fs::write(
        workspace.join("BUILD"),
        b"filegroup(name = \"sample\", srcs = [\"input.txt\"])\n",
    )
    .unwrap();
    std::fs::write(workspace.join("input.txt"), b"input\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_rush"))
        .arg("build")
        .arg("--daemon=off")
        .arg("--disk-cache=false")
        .arg("--analyze-only")
        .arg("//:sample")
        .env("RUSH_CACHE_ROOT", root.path().join("cache"))
        .current_dir(&workspace)
        .output()
        .expect("run one-shot rush analysis");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "one-shot analysis failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
    assert_eq!(
        stderr.matches("📋 Loading WORKSPACE from:").count(),
        1,
        "one-shot analysis must load WORKSPACE exactly once; stderr:\n{}",
        stderr
    );
}
