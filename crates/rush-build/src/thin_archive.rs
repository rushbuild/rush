// Rush Build - thin archive reuse policy
// Copyright (c) 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use std::path::Path;

/// Return whether an existing thin archive may replace this execution.
///
/// Reuse is disabled fail-closed. The removed predicate compared ordered
/// member paths and one global set of defined symbol names, but did not bind
/// each archive-index symbol to its owning member. Swapping definitions among
/// unchanged member paths could therefore retain a stale index. Tool identity,
/// flags, weak and duplicate symbols, object metadata, and alternate formats
/// were also outside that predicate's proof.
pub(crate) fn can_reuse_cc_archive_command(_executable: &Path, _argv: &[String]) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitStatus};
    use tempfile::TempDir;

    fn have_tool(name: &str) -> bool {
        Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {name} >/dev/null 2>&1"))
            .status()
            .is_ok_and(|status| status.success())
    }

    fn compile(dir: &Path, stem: &str, source: &str) -> Result<PathBuf> {
        let source_path = dir.join(format!("{stem}.cc"));
        let object_path = dir.join(format!("{stem}.o"));
        std::fs::write(&source_path, source)?;
        let status = Command::new("g++")
            .args(["-c", source_path.to_str().unwrap(), "-o"])
            .arg(&object_path)
            .status()?;
        anyhow::ensure!(status.success(), "g++ failed for {stem}");
        Ok(object_path)
    }

    fn rebuild_archive(archive: &Path, objects: &[&Path]) -> Result<()> {
        if archive.exists() {
            std::fs::remove_file(archive)?;
        }
        let status = Command::new("ar")
            .args(["rcs", archive.to_str().unwrap(), "--thin"])
            .args(objects)
            .status()?;
        anyhow::ensure!(status.success(), "ar failed");
        Ok(())
    }

    fn link(main: &Path, archive: &Path, output: &Path) -> Result<ExitStatus> {
        Command::new("g++")
            .arg(main)
            .arg(archive)
            .arg("-o")
            .arg(output)
            .status()
            .context("run g++ link")
    }

    fn archive_argv(archive: &Path, objects: &[&Path]) -> Vec<String> {
        let mut argv = vec![
            "rcs".to_string(),
            archive.display().to_string(),
            "--thin".to_string(),
        ];
        argv.extend(objects.iter().map(|path| path.display().to_string()));
        argv
    }

    #[test]
    fn reuse_gate_rejects_valid_malformed_and_changed_commands() -> Result<()> {
        if !have_tool("g++") || !have_tool("ar") {
            return Ok(());
        }
        let temp = TempDir::new()?;
        let object = compile(temp.path(), "member", "int member() { return 1; }\n")?;
        let archive = temp.path().join("libmember.a");
        rebuild_archive(&archive, &[&object])?;
        let argv = archive_argv(&archive, &[&object]);

        assert!(!can_reuse_cc_archive_command(Path::new("ar"), &argv));
        assert!(!can_reuse_cc_archive_command(
            Path::new("different-ar"),
            &argv
        ));

        let mut changed_flags = argv.clone();
        changed_flags[0] = "crsT".to_string();
        assert!(!can_reuse_cc_archive_command(
            Path::new("ar"),
            &changed_flags
        ));

        std::fs::write(&archive, b"!<thin>\nmalformed")?;
        assert!(!can_reuse_cc_archive_command(Path::new("ar"), &argv));
        Ok(())
    }

    #[test]
    fn swapped_symbol_owners_require_archive_rebuild() -> Result<()> {
        if !have_tool("g++") || !have_tool("ar") {
            return Ok(());
        }
        let temp = TempDir::new()?;
        let a = compile(temp.path(), "a", "int foo() { return 1; }\n")?;
        let b = compile(temp.path(), "b", "int bar() { return 2; }\n")?;
        let archive = temp.path().join("libmembers.a");
        rebuild_archive(&archive, &[&a, &b])?;

        let initial_main = temp.path().join("initial-main.cc");
        std::fs::write(
            &initial_main,
            b"int foo(); int main() { return foo() == 1 ? 0 : 1; }\n",
        )?;
        let initial_binary = temp.path().join("initial-main");
        assert!(link(&initial_main, &archive, &initial_binary)?.success());
        assert!(Command::new(&initial_binary).status()?.success());

        compile(temp.path(), "a", "int bar() { return 2; }\n")?;
        compile(temp.path(), "b", "int foo() { return 3; }\n")?;
        let changed_main = temp.path().join("changed-main.cc");
        std::fs::write(
            &changed_main,
            b"int foo(); int main() { return foo() == 3 ? 0 : 1; }\n",
        )?;
        let stale_binary = temp.path().join("stale-main");
        assert!(
            !link(&changed_main, &archive, &stale_binary)?.success(),
            "the stale symbol-to-member index must demonstrate the counterexample"
        );

        let argv = archive_argv(&archive, &[&a, &b]);
        assert!(!can_reuse_cc_archive_command(Path::new("ar"), &argv));
        rebuild_archive(&archive, &[&a, &b])?;
        let rebuilt_binary = temp.path().join("rebuilt-main");
        assert!(link(&changed_main, &archive, &rebuilt_binary)?.success());
        assert!(Command::new(&rebuilt_binary).status()?.success());
        Ok(())
    }
}
