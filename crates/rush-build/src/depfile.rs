// Rush Build - Dependency File Parser compatibility API
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Compatibility IO wrapper for compiler dependency parsing.
//!
//! The pure byte parser lives in `rush-action::compiler_discovery`. Parsing a
//! positive filename list does not install graph edges, certify read values or
//! authorize action-cache/retained reuse; that lifecycle remains pending.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub use rush_action::compiler_discovery::{parse_depfile_bytes, DepFileEntry};

/// Read a GCC/Clang depfile as bytes and parse the admitted single-rule grammar.
/// Callers needing tracked IO must supply tracked bytes to `parse_depfile_bytes`
/// instead. This compatibility helper does not certify file ownership/freshness.
pub fn parse_depfile(path: &Path) -> Result<DepFileEntry> {
    let bytes = fs::read(path)
        .with_context(|| format!("Failed to read dependency file: {}", path.display()))?;
    parse_depfile_bytes(&bytes)
}

#[cfg(test)]
fn parse_depfile_content(content: &str) -> Result<DepFileEntry> {
    parse_depfile_bytes(content.as_bytes())
}

/// Extract header dependencies from a .d file (excluding the source file itself).
///
/// Legacy extension-filtering utility only. Compiler discovery must use the
/// complete dependency list, including extensionless/generated/non-header files;
/// this filtered projection is not discovery or cache admission authority.
pub fn extract_headers(entry: &DepFileEntry) -> Vec<PathBuf> {
    // First dependency is usually the source file, rest are headers
    // But to be safe, filter by common header extensions
    entry
        .dependencies
        .iter()
        .filter(|path| {
            if let Some(ext) = path.extension() {
                matches!(
                    ext.to_str().unwrap_or(""),
                    "h" | "hh" | "hpp" | "hxx" | "H" | "inc"
                )
            } else {
                false
            }
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_depfile() {
        let content = "foo.o: foo.cc foo.h bar.h";
        let entry = parse_depfile_content(content).unwrap();

        assert_eq!(entry.target, PathBuf::from("foo.o"));
        assert_eq!(entry.dependencies.len(), 3);
        assert_eq!(entry.dependencies[0], PathBuf::from("foo.cc"));
        assert_eq!(entry.dependencies[1], PathBuf::from("foo.h"));
        assert_eq!(entry.dependencies[2], PathBuf::from("bar.h"));
    }

    #[test]
    fn test_parse_depfile_with_continuations() {
        let content = "foo.o: foo.cc \\\n  foo.h \\\n  bar.h";
        let entry = parse_depfile_content(content).unwrap();

        assert_eq!(entry.target, PathBuf::from("foo.o"));
        assert_eq!(entry.dependencies.len(), 3);
    }

    #[test]
    fn test_parse_depfile_with_paths() {
        let content = "build/foo.o: src/foo.cc include/foo.h include/bar.h";
        let entry = parse_depfile_content(content).unwrap();

        assert_eq!(entry.target, PathBuf::from("build/foo.o"));
        assert_eq!(entry.dependencies[0], PathBuf::from("src/foo.cc"));
        assert_eq!(entry.dependencies[1], PathBuf::from("include/foo.h"));
    }

    #[test]
    fn test_parse_depfile_with_spaces() {
        let content = "foo.o: foo.cc my\\ header.h";
        let entry = parse_depfile_content(content).unwrap();

        assert_eq!(entry.dependencies.len(), 2);
        assert_eq!(entry.dependencies[1], PathBuf::from("my header.h"));
    }

    #[test]
    fn test_extract_headers() {
        let entry = DepFileEntry {
            target: PathBuf::from("foo.o"),
            dependencies: vec![
                PathBuf::from("foo.cc"),
                PathBuf::from("foo.h"),
                PathBuf::from("bar.hpp"),
                PathBuf::from("baz.hxx"),
            ],
        };

        let headers = extract_headers(&entry);
        assert_eq!(headers.len(), 3);
        assert!(headers.contains(&PathBuf::from("foo.h")));
        assert!(headers.contains(&PathBuf::from("bar.hpp")));
        assert!(headers.contains(&PathBuf::from("baz.hxx")));
    }

    #[test]
    fn test_extract_headers_filters_source() {
        let entry = DepFileEntry {
            target: PathBuf::from("foo.o"),
            dependencies: vec![
                PathBuf::from("foo.cc"),
                PathBuf::from("foo.cpp"),
                PathBuf::from("foo.h"),
            ],
        };

        let headers = extract_headers(&entry);
        // Should only include .h, not .cc or .cpp
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0], PathBuf::from("foo.h"));
    }

    #[test]
    fn test_invalid_depfile_no_colon() {
        let content = "foo.o foo.cc foo.h";
        let result = parse_depfile_content(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing colon"));
    }

    #[test]
    fn test_invalid_depfile_no_deps() {
        let content = "foo.o: ";
        let result = parse_depfile_content(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_real_gcc_output() {
        // Real output from: g++ -MMD -MF foo.o.d -MT foo.o -c foo.cc
        let content = r#"foo.o: foo.cc foo.h \
 /usr/include/stdc-predef.h /usr/include/c++/11/iostream \
 /usr/include/x86_64-linux-gnu/c++/11/bits/c++config.h"#;

        let entry = parse_depfile_content(content).unwrap();
        assert_eq!(entry.target, PathBuf::from("foo.o"));
        assert!(entry.dependencies.len() >= 3);
        assert!(entry.dependencies.contains(&PathBuf::from("foo.cc")));
        assert!(entry.dependencies.contains(&PathBuf::from("foo.h")));
    }

    #[test]
    fn test_windows_style_paths() {
        let content = r#"build\foo.obj: src\foo.cpp include\foo.h"#;
        let entry = parse_depfile_content(content).unwrap();

        assert_eq!(entry.target, PathBuf::from(r#"build\foo.obj"#));
        assert_eq!(entry.dependencies[0], PathBuf::from(r#"src\foo.cpp"#));
    }
}
