// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Qualify the known TensorFlow options renderer before configured admission.
//! This supplies a source-bound request, not provider or execution authority.

use crate::dependency_graph::Label;
use anyhow::{ensure, Context, Result};
use rush_incremental::{tracked, CanonicalRegularFileMetadata};
use rush_rules::providers::{OptionsHeaderDeclaration, OptionsHeaderRequest};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

const TF_RENDERER_SHA256: &str = "2cda09b16b6e5576633221c68b64902e81b9925140ea5f4a292713047d9d25e6";
const SKYLIB_CONSTRUCTOR_SHA256: &str =
    "a86a8bb728baddc62a3e63222fa4e9f9355143ff8ae7b6527db247e73350f78d";
const PROVIDER_KEY: &str = "@bazel_skylib//rules:common_settings.bzl%BuildSettingInfo";
// Genuine Bazel 6.5 analysis of the pinned TF 2.17 renderer and Skylib 1.3
// constructor qualifies this identity independently of the admission query.
const TF_IMPLEMENTATION_HASH: &str =
    "89ca4d13600a0764c897d3be8a2c20a26412549fcdb607106ef67e71dd0aeb1e";

/// Prepare a MAIN consumer from its actual native declaration. The caller must
/// separately establish the selected SOURCE artifact and genuine configured
/// values; this function does not admit aliases, generated templates or writers.
pub(crate) fn qualified_tf_request(
    workspace: &Path,
    label: &Label,
    declaration: OptionsHeaderDeclaration,
    repo_roots: &HashMap<String, PathBuf>,
) -> Result<OptionsHeaderRequest> {
    ensure!(
        tracked::current().is_some(),
        "options renderer qualification requires captured analysis IO"
    );
    let request = declaration_request(workspace, label, declaration)?;
    let constructor_root = repo_roots
        .get("bazel_skylib")
        .context("options renderer requires the actual materialized bazel_skylib source root")?;
    let main = RootBinding::capture(workspace)?;
    let constructor = RootBinding::capture(constructor_root)?;
    let renderer = SourcePin::capture(
        &workspace.join("tensorflow/tensorflow.bzl"),
        TF_RENDERER_SHA256,
    )?;
    let provider = SourcePin::capture(
        &constructor_root.join("rules/common_settings.bzl"),
        SKYLIB_CONSTRUCTOR_SHA256,
    )?;
    // Bookend both sources together: the second read must not hide replacement
    // of the first source or either logical repository binding.
    renderer.verify()?;
    provider.verify()?;
    main.verify()?;
    constructor.verify()?;
    Ok(request)
}

fn declaration_request(
    workspace: &Path,
    label: &Label,
    declaration: OptionsHeaderDeclaration,
) -> Result<OptionsHeaderRequest> {
    ensure!(
        workspace.is_absolute(),
        "options renderer requires an absolute MAIN root"
    );
    ensure!(
        label.workspace.is_none(),
        "options renderer supports MAIN consumers only"
    );
    validate_label(label)?;
    let template = parse_label(&declaration.template, &label.package)?;
    ensure!(
        template.workspace.is_none(),
        "options renderer supports MAIN SOURCE template labels only"
    );
    let output = parse_label(&declaration.output_header, &label.package)?;
    ensure!(
        output.workspace.is_none() && output.package == label.package,
        "options output must belong to its consumer's MAIN package"
    );
    let mut settings = BTreeMap::new();
    for (setting, identifier) in declaration.build_settings {
        let setting = parse_label(&setting, &label.package)?.to_string();
        ensure!(
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "options renderer requires literal nonempty macro identifiers"
        );
        ensure!(
            settings.insert(setting, identifier).is_none(),
            "options declaration repeats a setting through different label spellings"
        );
    }
    Ok(OptionsHeaderRequest {
        consumer_label: label.to_string(),
        template_label: template.to_string(),
        output_label: output.to_string(),
        settings,
        // Keep the exact absolute logical artifact spelling. Canonical source
        // witnesses are separate from native ArtifactId/path ownership.
        template_path: workspace.join(&template.package).join(&template.target),
        provider_key: PROVIDER_KEY.into(),
        expected_implementation_hash: TF_IMPLEMENTATION_HASH.into(),
    })
}

fn parse_label(text: &str, package: &str) -> Result<Label> {
    let label = Label::parse(text, None, Some(package))?;
    validate_label(&label)?;
    Ok(label)
}

fn validate_label(label: &Label) -> Result<()> {
    normal_relative(&label.package, true)?;
    normal_relative(&label.target, false)?;
    if let Some(repository) = &label.workspace {
        ensure!(
            !repository.is_empty()
                && repository
                    .bytes()
                    .all(|b| { b.is_ascii_alphanumeric() || b"._+-~".contains(&b) }),
            "options label has an unsupported repository spelling"
        );
    }
    Ok(())
}

fn normal_relative(text: &str, allow_empty: bool) -> Result<()> {
    ensure!(
        (allow_empty && text.is_empty())
            || (!text.is_empty()
                && text
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != "..")
                && !text
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || "\\:@'\"()[],".contains(c))),
        "options label package/target must use normal relative path components"
    );
    Ok(())
}

struct RootBinding {
    logical: PathBuf,
    canonical: PathBuf,
}

impl RootBinding {
    fn capture(logical: &Path) -> Result<Self> {
        ensure!(
            logical.is_absolute(),
            "options source root must be absolute"
        );
        tracked::refresh_filesystem_observations();
        let canonical = tracked::canonicalize(logical)
            .with_context(|| format!("resolve options source root {}", logical.display()))?;
        ensure!(
            tracked::metadata_kind(&canonical) == Some(rush_incremental::FileKind::Dir),
            "options source root is not a directory"
        );
        Ok(Self {
            logical: logical.to_path_buf(),
            canonical,
        })
    }

    fn verify(&self) -> Result<()> {
        tracked::refresh_filesystem_observations();
        ensure!(
            tracked::canonicalize(&self.logical)? == self.canonical,
            "options logical source root changed during qualification"
        );
        Ok(())
    }
}

#[derive(Debug)]
struct SourcePin {
    logical: PathBuf,
    canonical: PathBuf,
    metadata: CanonicalRegularFileMetadata,
    expected_sha256: String,
}

impl SourcePin {
    fn capture(logical: &Path, expected_sha256: &str) -> Result<Self> {
        ensure!(
            logical.is_absolute(),
            "options source pin requires an absolute path"
        );
        tracked::refresh_filesystem_observations();
        let canonical = tracked::canonicalize(logical)
            .with_context(|| format!("resolve required options source {}", logical.display()))?;
        let read = tracked::read_canonical_regular_file(&canonical)?
            .context("options qualification requires descriptor-bound regular source reads")?;
        ensure!(
            hex::encode(Sha256::digest(&read.contents)) == expected_sha256,
            "options source differs from its qualified SHA256 pin: {}",
            logical.display()
        );
        let pin = Self {
            logical: logical.to_path_buf(),
            canonical,
            metadata: read.metadata,
            expected_sha256: expected_sha256.into(),
        };
        pin.verify()?;
        Ok(pin)
    }

    fn verify(&self) -> Result<()> {
        tracked::refresh_filesystem_observations();
        ensure!(
            tracked::canonicalize(&self.logical)? == self.canonical,
            "options logical source binding changed during qualification"
        );
        let read = tracked::read_canonical_regular_file(&self.canonical)?
            .context("options qualification requires descriptor-bound regular source reads")?;
        ensure!(
            read.metadata == self.metadata
                && hex::encode(Sha256::digest(&read.contents)) == self.expected_sha256,
            "options pinned source identity or contents changed during qualification"
        );
        tracked::refresh_filesystem_observations();
        ensure!(
            tracked::canonicalize(&self.logical)? == self.canonical,
            "options logical source binding changed during descriptor read"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration() -> OptionsHeaderDeclaration {
        OptionsHeaderDeclaration {
            template: "options.h.tpl".into(),
            output_header: "options.h".into(),
            build_settings: BTreeMap::from([(
                "//tensorflow:enable_registration_v2".into(),
                "REGISTRATION_V2".into(),
            )]),
        }
    }

    #[test]
    fn native_options_header_request_keeps_logical_artifacts_and_exact_mapping() -> Result<()> {
        let consumer = Label::parse(
            "//tensorflow/core/framework/registration:gen_registration_options",
            None,
            None,
        )?;
        let request = declaration_request(Path::new("/logical/main"), &consumer, declaration())?;
        assert_eq!(request.consumer_label, consumer.to_string());
        assert_eq!(
            request.template_label,
            "//tensorflow/core/framework/registration:options.h.tpl"
        );
        assert_eq!(
            request.output_label,
            "//tensorflow/core/framework/registration:options.h"
        );
        assert_eq!(
            request.template_path,
            Path::new("/logical/main/tensorflow/core/framework/registration/options.h.tpl")
        );
        assert_eq!(request.settings, declaration().build_settings);
        assert_eq!(request.expected_implementation_hash, TF_IMPLEMENTATION_HASH);
        assert_eq!(request.provider_key, PROVIDER_KEY);
        for spelling in [
            ":options.h.tpl",
            "@//tensorflow/core/framework/registration:options.h.tpl",
            "@@//tensorflow/core/framework/registration:options.h.tpl",
        ] {
            let mut alternate = declaration();
            alternate.template = spelling.into();
            assert_eq!(
                declaration_request(Path::new("/logical/main"), &consumer, alternate)?,
                request
            );
        }
        Ok(())
    }

    #[test]
    fn native_options_header_request_refuses_foreign_and_ambiguous_paths() -> Result<()> {
        let consumer = Label::parse("//pkg:options", None, None)?;
        for template in [
            "@other//pkg:options.tpl",
            "../options.tpl",
            "/options.tpl",
            "nested//options.tpl",
            ":",
        ] {
            let mut value = declaration();
            value.template = template.into();
            assert!(
                declaration_request(Path::new("/main"), &consumer, value).is_err(),
                "{template}"
            );
        }
        for output in [
            "../options.h",
            "//other:options.h",
            "@other//pkg:options.h",
            "nested/./options.h",
        ] {
            let mut value = declaration();
            value.output_header = output.into();
            assert!(
                declaration_request(Path::new("/main"), &consumer, value).is_err(),
                "{output}"
            );
        }
        let mut duplicate = declaration();
        duplicate.build_settings = BTreeMap::from([
            (":flag".into(), "ONE".into()),
            ("@//pkg:flag".into(), "TWO".into()),
        ]);
        assert!(declaration_request(Path::new("/main"), &consumer, duplicate).is_err());
        let external = Label::parse("@owner//pkg:options", None, None)?;
        assert!(declaration_request(Path::new("/main"), &external, declaration()).is_err());
        assert!(declaration_request(Path::new("relative"), &consumer, declaration()).is_err());
        Ok(())
    }

    #[test]
    fn native_options_header_pin_rejects_changed_bytes_and_identity() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.bzl");
        std::fs::write(&source, b"pin fixture bytes")?;
        let expected = hex::encode(Sha256::digest(b"pin fixture bytes"));
        tracked::test_scope(temp.path(), || -> Result<()> {
            let pin = SourcePin::capture(&source, &expected)?;
            std::fs::write(&source, b"changed pin bytes")?;
            assert!(pin.verify().is_err());
            assert!(SourcePin::capture(&source, &expected).is_err());
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn native_options_header_pin_refuses_same_byte_symlink_retarget() -> Result<()> {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir()?;
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::write(&first, b"same bytes")?;
        std::fs::write(&second, b"same bytes")?;
        let logical = temp.path().join("source.bzl");
        symlink(&first, &logical)?;
        let expected = hex::encode(Sha256::digest(b"same bytes"));
        tracked::test_scope(temp.path(), || -> Result<()> {
            let pin = SourcePin::capture(&logical, &expected)?;
            std::fs::remove_file(&logical)?;
            symlink(&second, &logical)?;
            assert!(pin
                .verify()
                .unwrap_err()
                .to_string()
                .contains("logical source binding"));
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn native_options_header_root_binding_refuses_same_byte_root_retarget() -> Result<()> {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir()?;
        for name in ["first", "second"] {
            std::fs::create_dir(temp.path().join(name))?;
            std::fs::write(temp.path().join(name).join("source.bzl"), b"same bytes")?;
        }
        let logical = temp.path().join("main");
        symlink(temp.path().join("first"), &logical)?;
        tracked::test_scope(temp.path(), || -> Result<()> {
            let root = RootBinding::capture(&logical)?;
            std::fs::remove_file(&logical)?;
            symlink(temp.path().join("second"), &logical)?;
            assert!(root.verify().is_err());
            Ok(())
        })
    }

    #[test]
    fn native_options_header_requires_registered_constructor_materialization() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let consumer = Label::parse("//pkg:options", None, None)?;
        tracked::test_scope(temp.path(), || -> Result<()> {
            let error =
                qualified_tf_request(temp.path(), &consumer, declaration(), &HashMap::new())
                    .unwrap_err();
            assert!(error
                .to_string()
                .contains("actual materialized bazel_skylib"));
            Ok(())
        })
    }

    #[test]
    fn native_options_header_constructor_wrong_pin_and_missing_source_refuse() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let constructor = temp.path().join("common_settings.bzl");
        std::fs::write(&constructor, b"unqualified constructor bytes")?;
        tracked::test_scope(temp.path(), || -> Result<()> {
            assert!(SourcePin::capture(&constructor, SKYLIB_CONSTRUCTOR_SHA256)
                .unwrap_err()
                .to_string()
                .contains("qualified SHA256"));
            assert!(
                SourcePin::capture(&temp.path().join("absent"), SKYLIB_CONSTRUCTOR_SHA256).is_err()
            );
            Ok(())
        })
    }
}
