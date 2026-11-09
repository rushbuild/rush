// Rush Build - genuine WORKSPACE repository-rule integration control
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#![cfg(unix)]

use rush_build::dependency_graph::{DependencyGraph, Label};
use rush_build::BuildOrchestrator;
use rush_incremental::{Staleness, TrackedIo};
use rush_parse::HttpArchive;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct BazelShutdown {
    binary: PathBuf,
    output: PathBuf,
    workspace: PathBuf,
}

impl Drop for BazelShutdown {
    fn drop(&mut self) {
        let _ = Command::new(&self.binary)
            .arg("--output_base")
            .arg(&self.output)
            .arg("shutdown")
            .current_dir(&self.workspace)
            .status();
    }
}

fn archive() -> Vec<u8> {
    let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gzip);
    for (path, contents) in [
        ("upstream/header.h", "#define SOURCE_VALUE 40\n"),
        (
            "upstream/BUILD.bazel",
            "fail(\"upstream BUILD must be replaced\")\n",
        ),
        ("upstream/defs/BUILD", ""),
        ("upstream/defs/subdir/child.bzl", "VALUE = 42\n"),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(contents.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, path, contents.as_bytes())
            .unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}

fn patch(value: u32) -> String {
    format!("--- a/header.h\n+++ b/header.h\n@@ -1 +1 @@\n-#define SOURCE_VALUE 40\n+#define SOURCE_VALUE {value}\n")
}

/// Run explicitly with an exact pinned Bazel executable. This invokes real
/// repository-rule evaluation and patch application; it is separate from unit
/// schema fixtures and does not compile the third-party workload.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires explicitly configured RUSH_BAZEL_PATH pointing to Bazel 6.5.0"]
async fn declared_workspace_archive_applies_external_overlay_and_patch_and_refreshes() {
    let binary = PathBuf::from(
        std::env::var_os("RUSH_BAZEL_PATH").expect("set the exact Bazel 6.5.0 executable"),
    );
    assert!(binary.is_absolute());
    let version = Command::new(&binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "bazel 6.5.0"
    );
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let cache = rush_build::cache_paths::workspace_cache_root(root);
    assert!(
        cache.starts_with(root),
        "this control requires a task-owned workspace cache"
    );
    let _shutdown = BazelShutdown {
        binary,
        output: cache.join("bazel_output_base"),
        workspace: root.to_path_buf(),
    };
    let owner = root.join("owner");
    std::fs::create_dir(&owner).unwrap();
    std::fs::write(
        owner.join("WORKSPACE"),
        "workspace(name = \"overlay_owner\")\n",
    )
    .unwrap();
    std::fs::write(
        owner.join("BUILD"),
        "exports_files([\"overlay.BUILD\", \"fix.patch\", \"alternate.patch\"])\n",
    )
    .unwrap();
    let overlay = "cc_library(name = \"headers\", hdrs = [\"header.h\"], visibility = [\"//visibility:public\"])\n";
    std::fs::write(owner.join("overlay.BUILD"), overlay).unwrap();
    std::fs::write(owner.join("fix.patch"), patch(41)).unwrap();
    std::fs::write(owner.join("alternate.patch"), patch(43)).unwrap();
    std::fs::write(
        root.join("BUILD"),
        "exports_files([\"defs.bzl\", \"sub.bzl\"])\n",
    )
    .unwrap();
    std::fs::write(
        root.join("sub.bzl"),
        "def selected_patch():\n    return \"fix.patch\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("defs.bzl"),
        r#"load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
load("//:sub.bzl", "selected_patch")
def declare_source(url, sha):
    http_archive(
        name = "source_fixture",
        urls = [url],
        sha256 = sha,
        strip_prefix = "upstream",
        build_file = "@overlay_owner//:overlay.BUILD",
        patches = ["@overlay_owner//:" + selected_patch()],
        patch_args = ["-p1"],
    )
"#,
    )
    .unwrap();
    let payload = archive();
    let sha = hex::encode(Sha256::digest(&payload));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut bytes = [0u8; 1024];
                let count = stream.read(&mut bytes).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&bytes[..count]);
                assert!(request.len() < 16384);
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(&payload).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    let url = format!("http://{address}/source.tar.gz");
    std::fs::write(
        root.join("WORKSPACE"),
        format!(
            r#"workspace(name = "repository_rule_control")
local_repository(name = "overlay_owner", path = "owner")
load("//:defs.bzl", "declare_source")
declare_source(url = {url:?}, sha = {sha:?})
load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
http_archive(name="nested_first_fetch", urls=[{url:?}], sha256={sha:?}, strip_prefix="upstream", build_file="@overlay_owner//:overlay.BUILD")
"#
        ),
    )
    .unwrap();
    // A legacy marker-free adapter must be left untouched and unused.
    let legacy = cache.join("external/source_fixture");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("BUILD"), "legacy adapter sentinel\n").unwrap();
    let orchestrator = BuildOrchestrator::with_workspace(root.to_path_buf());
    let io = orchestrator.install_fresh_tracked_io_for_test();
    // The parsed tuple intentionally lacks patches and has an owner-qualified
    // overlay. Success must come from the original WORKSPACE repository rule.
    orchestrator
        .repository_manager
        .register_http_archive(HttpArchive {
            name: "source_fixture".into(),
            urls: vec![url],
            sha256: Some(sha),
            strip_prefix: Some("upstream".into()),
            build_file: Some("@overlay_owner//:overlay.BUILD".into()),
            build_file_content: None,
            repo_mapping: HashMap::new(),
        });
    let target = Label::parse("@source_fixture//:headers", None, None).unwrap();
    let mut graph = DependencyGraph::new();
    assert!(orchestrator
        .repository_manager
        .resolve_missing_dependency(&orchestrator, &mut graph, &target)
        .await
        .unwrap());
    let source = orchestrator.external_repo_paths.lock().await["source_fixture"].clone();
    assert!(source.starts_with(cache.join("bazel_output_base/external")));
    assert_eq!(
        std::fs::read_to_string(source.join("BUILD.bazel")).unwrap(),
        overlay
    );
    assert_eq!(
        std::fs::read_to_string(source.join("header.h")).unwrap(),
        "#define SOURCE_VALUE 41\n"
    );
    assert_eq!(
        std::fs::read_to_string(legacy.join("BUILD")).unwrap(),
        "legacy adapter sentinel\n"
    );
    let manifest = io.manifest();
    assert!(matches!(
        io.validate_manifest(&manifest).staleness,
        Staleness::Fresh
    ));
    std::fs::write(owner.join("fix.patch"), patch(42)).unwrap();
    assert!(matches!(
        io.validate_manifest(&manifest).staleness,
        Staleness::Stale { .. }
    ));
    orchestrator.install_fresh_tracked_io_for_test();
    assert!(orchestrator
        .repository_manager
        .resolve_missing_dependency(&orchestrator, &mut graph, &target)
        .await
        .unwrap());
    assert_eq!(
        std::fs::read_to_string(source.join("header.h")).unwrap(),
        "#define SOURCE_VALUE 42\n"
    );
    // The old patch remains unchanged. Only a transitive loaded module
    // changes which existing patch the actual repository declaration selects.
    std::fs::write(
        root.join("sub.bzl"),
        "def selected_patch():\n    return \"alternate.patch\"\n",
    )
    .unwrap();
    orchestrator.install_fresh_tracked_io_for_test();
    assert!(orchestrator
        .repository_manager
        .resolve_missing_dependency(&orchestrator, &mut graph, &target)
        .await
        .unwrap());
    assert_eq!(
        std::fs::read_to_string(source.join("header.h")).unwrap(),
        "#define SOURCE_VALUE 43\n"
    );
    let changed_overlay =
        "cc_library(name = \"headers\", hdrs = [\"header.h\"], tags = [\"changed_overlay\"])\n";
    std::fs::write(owner.join("overlay.BUILD"), changed_overlay).unwrap();
    orchestrator.install_fresh_tracked_io_for_test();
    assert!(orchestrator
        .repository_manager
        .resolve_missing_dependency(&orchestrator, &mut graph, &target)
        .await
        .unwrap());
    assert_eq!(
        std::fs::read_to_string(source.join("BUILD.bazel")).unwrap(),
        changed_overlay
    );
    assert_eq!(
        std::fs::read_to_string(legacy.join("BUILD")).unwrap(),
        "legacy adapter sentinel\n"
    );
    // First materialization retains package:target and accepts an unexported
    // source module: defs/BUILD is empty, subdir has no BUILD, child.bzl is not
    // a query/fetch target. Package materialization then normal load must work.
    let nested_io = orchestrator.install_fresh_tracked_io_for_test();
    tokio::task::block_in_place(|| {
        rush_incremental::tracked::scope(nested_io, || {
            let mut loader = rush_starlark::BzlLoader::new(root.to_path_buf());
            let symbols = loader
                .load_bzl(
                    "@nested_first_fetch//defs:subdir/child.bzl",
                    &[("VALUE".into(), "VALUE".into())],
                    None,
                )
                .unwrap();
            assert_eq!(symbols["VALUE"], rush_starlark::Value::Int(42));
            assert!(!cache
                .join("bazel_output_base/external/nested_first_fetch/defs/subdir/BUILD")
                .exists());
        })
    });
    // Bazel sync alone returns success for this deleted declaration and leaves
    // the old source+marker. An authoritative query must refuse its admission.
    std::fs::write(
        root.join("WORKSPACE"),
        "workspace(name = \"repository_rule_control\")\n",
    )
    .unwrap();
    orchestrator.install_fresh_tracked_io_for_test();
    let error = orchestrator
        .repository_manager
        .resolve_missing_dependency(&orchestrator, &mut graph, &target)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("declaration query"));
    assert_eq!(
        std::fs::read_to_string(source.join("header.h")).unwrap(),
        "#define SOURCE_VALUE 43\n"
    );
    server.abort();
    let _ = server.await;
}
