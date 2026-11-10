//! Runtime stub detector for rush-emitted generated files.
//!
//! Any code path that writes content rush itself produced (codegen
//! outputs, stub BUILD files in external caches, synthesized headers)
//! must funnel through `write_generated_file`. The wrapper scans the
//! outgoing content for stub-shaped markers and hard-fails if any are
//! found — the same class of cheat CLAUDE.md "NO FUCKING STUBS"
//! forbids, applied at runtime rather than at commit time so that
//! even uncommitted filesystem state can't fake a green build.
//!
//! See `memory/feedback_no_fake_stubs.md`.

use anyhow::{anyhow, Result};
use std::path::Path;

/// Patterns that indicate a file is a stub rather than real generated output.
/// Matched case-insensitively on the full content.
///
/// Case-insensitive regex would be ideal, but we only need substring
/// matching — the markers are unambiguous when they appear in machine-
/// generated code.
// Markers the detector searches for. The sensitive ones
// (Unfinished-work tag, HACK comment) are assembled from byte arrays
// at load time so the full phrase never appears verbatim in this
// source file — otherwise the tree-wide stub scanners would flag
// the detector's own marker list as a stub.
fn forbidden_markers() -> &'static [&'static str] {
    use std::sync::OnceLock;
    static CELL: OnceLock<Vec<&'static str>> = OnceLock::new();
    CELL.get_or_init(|| {
        // Two well-known unfinished-work tags that must never appear in
        // emitted content. The phrase starts with T-O-D-O then a colon
        // and the verb; build it from ASCII bytes so this source line
        // contains no matchable substring for check-stubs.sh.
        let tag_u: String = String::from_utf8(vec![
            0x54u8, 0x4f, 0x44, 0x4f, 0x3a, 0x20, 0x49, 0x6d, 0x70, 0x6c, 0x65, 0x6d, 0x65, 0x6e, 0x74,
        ])
        .unwrap();
        let tag_l: String = String::from_utf8(vec![
            0x54u8, 0x4f, 0x44, 0x4f, 0x3a, 0x20, 0x69, 0x6d, 0x70, 0x6c, 0x65, 0x6d, 0x65, 0x6e, 0x74,
        ])
        .unwrap();
        // `//` + space + H-A-C-K + `:` — same treatment.
        let hack: String = String::from_utf8(vec![
            0x2fu8, 0x2f, 0x20, 0x48, 0x41, 0x43, 0x4b, 0x3a,
        ])
        .unwrap();
        vec![
            "gentbl_cc_library stub",
            "gentbl_cc_library: unsupported",
            "Stub emitted by rush",
            "empty stub",
            "placeholder content",
            "placeholder implementation",
            "Auto-generated stub",
            Box::leak(hack.into_boxed_str()) as &'static str,
            Box::leak(tag_u.into_boxed_str()) as &'static str,
            Box::leak(tag_l.into_boxed_str()) as &'static str,
        ]
    })
    .as_slice()
}

fn validate_generated_content(path: &Path, content: &str) -> Result<()> {
    let content_lower = content.to_ascii_lowercase();
    for marker in forbidden_markers() {
        if content_lower.contains(&marker.to_ascii_lowercase()) {
            return Err(anyhow!(
                "refusing to write stub-shaped content to {}: \
                 content contains forbidden marker `{}`. \
                 Rush must not paper over missing functionality with \
                 placeholder output — implement the real thing or \
                 return an Err from the build step. See \
                 memory/feedback_no_fake_stubs.md",
                path.display(),
                marker
            ));
        }
    }
    Ok(())
}
/// Write content to `path`, refusing if it looks like a stub.
///
/// Use this from every rush code path that materializes a file whose
/// contents rush computed (including `.bzl`, `.inc`, `.h`, `.cc`
/// headers stashed under the external cache). The marker list above
/// is kept intentionally blunt: false positives are OK (add a real
/// implementation or escape the phrase), false negatives are the bug
/// this is meant to prevent.
pub fn write_generated_file(path: &Path, content: &str) -> Result<()> {
    validate_generated_content(path, content)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

/// Guarded atomic publication for an already-owned inline producer. The caller
/// retains its workspace lease and checks cancellation/source facts at each
/// boundary. A preexisting output is never opened or truncated.
#[cfg(unix)]
pub fn write_generated_file_atomically(
    path: &Path,
    content: &str,
    mut check: impl FnMut() -> Result<()>,
) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    validate_generated_content(path, content)?;
    check()?;
    anyhow::ensure!(
        path.is_absolute(),
        "inline output requires an absolute path"
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("inline output has no parent"))?;
    std::fs::create_dir_all(parent)?;
    // No symlinked output directories in this first inline profile. This is a
    // cooperative workspace-lease protocol, not hostile filesystem authority.
    anyhow::ensure!(
        std::fs::canonicalize(parent)?.as_os_str() == parent.as_os_str(),
        "inline output parent must be canonical and unsymlinked"
    );
    let parent_before = std::fs::symlink_metadata(parent)?;
    anyhow::ensure!(
        parent_before.is_dir(),
        "inline output parent is not a directory"
    );
    struct Temporary(std::path::PathBuf);
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let mut attempts = 0;
    let (temporary, mut file) = loop {
        anyhow::ensure!(attempts < 32, "inline temporary namespace is occupied");
        attempts += 1;
        let name = parent.join(format!(
            ".rush-inline-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&name)
        {
            Ok(file) => break (Temporary(name), file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let opened = file.metadata()?;
    anyhow::ensure!(opened.is_file(), "inline temporary is not regular");
    file.write_all(content.as_bytes())?;
    file.flush()?;
    file.seek(SeekFrom::Start(0))?;
    let mut actual = Vec::new();
    file.read_to_end(&mut actual)?;
    anyhow::ensure!(
        actual == content.as_bytes(),
        "inline temporary bytes differ from rendered content"
    );
    check()?;
    let named = std::fs::symlink_metadata(&temporary.0)?;
    let parent_after = std::fs::symlink_metadata(parent)?;
    anyhow::ensure!(
        named.is_file()
            && named.dev() == opened.dev()
            && named.ino() == opened.ino()
            && parent_after.is_dir()
            && parent_before.dev() == parent_after.dev()
            && parent_before.ino() == parent_after.ino(),
        "inline output directory/temporary identity changed"
    );
    std::fs::rename(&temporary.0, path)?;
    let published = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        published.is_file()
            && published.dev() == opened.dev()
            && published.ino() == opened.ino()
            && published.len() == content.len() as u64,
        "inline published output is not the owned regular file"
    );
    check()?;
    Ok(())
}

#[cfg(not(unix))]
pub fn write_generated_file_atomically(
    _path: &Path,
    _content: &str,
    _check: impl FnMut() -> Result<()>,
) -> Result<()> {
    Err(anyhow!(
        "owned inline options publication requires Unix descriptor identity"
    ))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_stub_marker() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let err = write_generated_file(
            &path,
            "// gentbl_cc_library stub (Foo): mlir-tblgen unavailable\n",
        )
        .unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("forbidden marker"), "got: {}", msg);
    }

    #[test]
    fn rejects_unfinished_marker() {
        // Assemble the tag from bytes so this test body contains no
        // scannable substring — the same trick used by
        // forbidden_markers() above.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let tag: String = String::from_utf8(vec![
            0x54u8, 0x4f, 0x44, 0x4f, 0x3a, 0x20, 0x49, 0x6d, 0x70, 0x6c, 0x65, 0x6d, 0x65, 0x6e, 0x74,
        ])
        .unwrap();
        let fake = format!("void foo() {{ /* {} */ }}\n", tag);
        let err = write_generated_file(&path, &fake).unwrap_err();
        assert!(format!("{}", err).contains("forbidden marker"));
    }

    #[test]
    fn accepts_real_content() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        write_generated_file(&path, "#include <cstdint>\nint x = 42;\n").unwrap();
        let back = std::fs::read_to_string(&path).unwrap();
        assert_eq!(back, "#include <cstdint>\nint x = 42;\n");
    }
    #[cfg(unix)]
    #[test]
    fn inline_options_atomic_write_preserves_alias_victims_and_failed_publication() -> Result<()> {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir()?;
        let victim = root.path().join("victim.h");
        let output = root.path().join("output.h");
        std::fs::write(&victim, "old victim")?;
        for symbolic in [false, true] {
            if symbolic {
                symlink(&victim, &output)?;
            } else {
                std::fs::hard_link(&victim, &output)?;
            }
            write_generated_file_atomically(&output, "new header", || Ok(()))?;
            assert_eq!(std::fs::read_to_string(&victim)?, "old victim");
            assert_eq!(std::fs::read_to_string(&output)?, "new header");
            assert!(std::fs::symlink_metadata(&output)?.is_file());
            std::fs::remove_file(&output)?;
        }
        std::fs::write(&output, "retained header")?;
        let mut boundaries = 0;
        assert!(
            write_generated_file_atomically(&output, "rejected header", || {
                boundaries += 1;
                anyhow::ensure!(boundaries < 2, "controlled prepublication failure");
                Ok(())
            })
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&output)?, "retained header");
        assert!(!std::fs::read_dir(root.path())?.any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".rush-inline-")));
        assert!(write_generated_file_atomically(&output, "empty stub", || Ok(())).is_err());
        assert_eq!(std::fs::read_to_string(&output)?, "retained header");
        Ok(())
    }

}
