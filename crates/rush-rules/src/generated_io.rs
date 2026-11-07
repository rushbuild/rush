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

/// Write content to `path`, refusing if it looks like a stub.
///
/// Use this from every rush code path that materializes a file whose
/// contents rush computed (including `.bzl`, `.inc`, `.h`, `.cc`
/// headers stashed under the external cache). The marker list above
/// is kept intentionally blunt: false positives are OK (add a real
/// implementation or escape the phrase), false negatives are the bug
/// this is meant to prevent.
pub fn write_generated_file(path: &Path, content: &str) -> Result<()> {
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
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
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
}
