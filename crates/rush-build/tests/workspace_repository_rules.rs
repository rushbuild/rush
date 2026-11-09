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

/// A WORKSPACE load is already a genuine consumer of its exact .bzl file. It
/// does not authorize unrelated graph targets or every other file in the repo.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires explicitly configured RUSH_BAZEL_PATH pointing to Bazel 6.5.0"]
async fn workspace_loaded_module_admits_exact_source_without_loading_broken_root_build() {
    let binary = PathBuf::from(std::env::var_os("RUSH_BAZEL_PATH").expect("set exact Bazel 6.5.0"));
    assert!(binary.is_absolute());
    let version = Command::new(&binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), "bazel 6.5.0");
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let cache = rush_build::cache_paths::workspace_cache_root(root);
    assert!(cache.starts_with(root));
    let _shutdown = BazelShutdown {
        binary, output: cache.join("bazel_output_base"), workspace: root.to_path_buf(),
    };
    std::fs::write(root.join("BUILD"), "").unwrap();
    std::fs::write(root.join("rules.bzl"), r#"
def _impl(ctx):
    ctx.file("defs.bzl", "VALUE = 42\n")
    ctx.file("unloaded.bzl", "VALUE = 42\n")
    ctx.file("nested/BUILD", "")
    ctx.file("nested/subdir/child.bzl", "VALUE = 43\n")
    ctx.file("BUILD.bazel", 'load("@undefined_docs//:defs.bzl", "docs")\ndocs(name = "broken")\n')
source_repository = repository_rule(implementation = _impl)
"#).unwrap();
    let workspace = r#"workspace(name = "loaded_module_control")
load("//:rules.bzl", "source_repository")
source_repository(name = "source_fixture")
source_repository(name = "other_fixture")
load("@source_fixture//:defs.bzl", "VALUE")
load("@source_fixture//nested:subdir/child.bzl", CHILD = "VALUE")
"#;
    std::fs::write(root.join("WORKSPACE"), workspace).unwrap();
    let orchestrator = BuildOrchestrator::with_workspace(root.to_path_buf());
    let io = orchestrator.install_fresh_tracked_io_for_test();
    let source = tokio::task::block_in_place(|| rush_incremental::tracked::scope(io, || {
        let source = rush_starlark::BzlLoader::fetch_declared_workspace_repository_for_module(
            root, "source_fixture", "@source_fixture//:defs.bzl",
        ).unwrap().unwrap();
        let mut loader = rush_starlark::BzlLoader::new(root.to_path_buf());
        let values = loader.load_bzl("@source_fixture//:defs.bzl", &[("VALUE".into(), "VALUE".into())], None).unwrap();
        assert_eq!(values["VALUE"], rush_starlark::Value::Int(42));
        source
    }));
    assert!(std::fs::read_to_string(source.join("BUILD.bazel")).unwrap().contains("@undefined_docs"));
    let proof_path = cache.join("bazel_output_base/external/@source_fixture.rush-workspace-inputs-v4.json");
    let proof_bytes = std::fs::read(&proof_path).unwrap();
    let proof: serde_json::Value = serde_json::from_slice(&proof_bytes).unwrap();
    assert_eq!(proof["version"], 4);
    assert_eq!(proof["authority"], "LoadedWorkspaceClosure");
    assert!(proof["definition"]["loaded_modules"].as_array().unwrap().iter()
        .any(|module| module["label"] == "@source_fixture//:defs.bzl"));
    // A second exact module in the frozen closure reuses this same authority,
    // retaining package:target even when target includes a subdirectory.
    let io = orchestrator.install_fresh_tracked_io_for_test();
    tokio::task::block_in_place(|| rush_incremental::tracked::scope(io, || {
        let mut loader = rush_starlark::BzlLoader::new(root.to_path_buf());
        let values = loader.load_bzl("@source_fixture//nested:subdir/child.bzl", &[("VALUE".into(), "VALUE".into())], None).unwrap();
        assert_eq!(values["VALUE"], rush_starlark::Value::Int(43));
    }));
    assert_eq!(std::fs::read(&proof_path).unwrap(), proof_bytes);
    // The graph route must perform its actual target fetch and fail on the
    // broken BUILD even though a source-only invocation proof already exists.
    let io = orchestrator.install_fresh_tracked_io_for_test();
    let error = tokio::task::block_in_place(|| rush_incremental::tracked::scope(io, || {
        rush_starlark::BzlLoader::fetch_declared_workspace_repository_for_label(
            root, "source_fixture", Some("@source_fixture//:all"),
        ).unwrap_err()
    }));
    assert!(format!("{error:#}").contains("fetch @source_fixture//:all"));
    assert_eq!(std::fs::read(&proof_path).unwrap(), proof_bytes, "a failed fetch cannot publish authority");
    for (repo, module) in [
        ("source_fixture", "@source_fixture//:unloaded.bzl"),
        ("other_fixture", "@other_fixture//:defs.bzl"),
    ] {
        let io = orchestrator.install_fresh_tracked_io_for_test();
        let error = tokio::task::block_in_place(|| rush_incremental::tracked::scope(io, || {
            rush_starlark::BzlLoader::fetch_declared_workspace_repository_for_module(root, repo, module).unwrap_err()
        }));
        assert!(format!("{error:#}").contains("fetch "), "{module}: {error:#}");
    }
    // A failing module query must invalidate the old closure without using its
    // still-present marker/source as success evidence.
    std::fs::write(root.join("WORKSPACE"), format!("{workspace}load(\"@source_fixture//:missing.bzl\", \"MISSING\")\n")).unwrap();
    let io = orchestrator.install_fresh_tracked_io_for_test();
    let error = tokio::task::block_in_place(|| rush_incremental::tracked::scope(io, || {
        rush_starlark::BzlLoader::fetch_declared_workspace_repository_for_module(root, "source_fixture", "@source_fixture//:defs.bzl").unwrap_err()
    }));
    assert!(format!("{error:#}").contains("repository declaration query"));
    assert_eq!(std::fs::read(&proof_path).unwrap(), proof_bytes);
    assert_eq!(std::fs::read_to_string(source.join("defs.bzl")).unwrap(), "VALUE = 42\n");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires explicitly configured RUSH_BAZEL_PATH pointing to Bazel 6.5.0"]
async fn implicit_system_adapter_yields_to_declared_workspace_module_and_target() {
    use bytes::Bytes;
    use rush_command_core::CommandSemantics;
    use rush_incremental::{tracked, RealTrackedIo};
    use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
    use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use std::os::unix::ffi::OsStrExt;
    use std::sync::{Arc, Mutex};
    let binary = PathBuf::from(std::env::var_os("RUSH_BAZEL_PATH").expect("set exact Bazel 6.5.0"));
    assert!(binary.is_absolute());
    let version = Command::new(&binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), "bazel 6.5.0");
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let cache = rush_build::cache_paths::workspace_cache_root(root);
    assert!(cache.starts_with(root));
    let _shutdown = BazelShutdown { binary: binary.clone(), output: cache.join("bazel_output_base"), workspace: root.to_owned() };
    std::fs::write(root.join("BUILD"), "load(\"@com_google_absl//:defs.bzl\", \"VALUE\")\ncc_library(name = \"main_value_%s\" % VALUE)\nexports_files([\"rules.bzl\", \"source.txt\", \"fix.patch\"])\n").unwrap();
    std::fs::write(root.join("rules.bzl"), r##"
def _impl(ctx):
    source = int(ctx.read(Label("//:source.txt")).strip())
    ctx.file("value.h", "#define PATCHED 0\n")
    ctx.patch(ctx.path(Label("//:fix.patch")), strip = 1)
    patched = int(ctx.read("value.h").strip().split(" ")[-1])
    value = source + patched + int(ctx.os.environ.get("IMPLICIT_SOURCE_ENV", "0"))
    ctx.file("defs.bzl", "VALUE = %d\n" % value)
    ctx.file("BUILD", 'filegroup(name = "module", srcs = ["defs.bzl"])\n')
    ctx.file("absl/strings/value.h", "int declared_value();\n")
    ctx.file("absl/strings/value.cc", "int declared_value() { return %d; }\n" % value)
    ctx.file("absl/strings/BUILD", 'cc_library(name = "strings", srcs = ["value.cc"], hdrs = ["value.h"], visibility = ["//visibility:public"])\n')
source_repository = repository_rule(implementation = _impl, environ = ["IMPLICIT_SOURCE_ENV"])
"##).unwrap();
    std::fs::write(root.join("WORKSPACE"), "workspace(name = \"implicit_precedence_control\")\nload(\"//:rules.bzl\", \"source_repository\")\nsource_repository(name = \"com_google_absl\")\n").unwrap();
    std::fs::write(root.join("source.txt"), "40\n").unwrap();
    let patch = |value| format!("--- a/value.h\n+++ b/value.h\n@@ -1 +1 @@\n-#define PATCHED 0\n+#define PATCHED {value}\n");
    std::fs::write(root.join("fix.patch"), patch(1)).unwrap();
    let orchestrator = BuildOrchestrator::with_workspace(root.to_owned());
    let adapter = cache.join("external/com_google_absl");
    assert!(adapter.join("absl/strings/BUILD").exists());
    // This is deliberately conflicting data in a known implicit adapter, not
    // a fetched source or a requested-function fallback.
    std::fs::write(adapter.join("defs.bzl"), "VALUE = 99\n").unwrap();
    let adapter_build = std::fs::read(adapter.join("absl/strings/BUILD")).unwrap();
    let io = orchestrator.install_fresh_tracked_io_for_test();
    // Resolve the module through the actual orchestrator first. This changes
    // its loader's source root while the async registry may still hold the old
    // adapter path; that old registration must retain its implicit origin.
    let main = orchestrator.instantiate_build_file(&root.join("BUILD"), None).unwrap();
    assert!(main.iter().any(|instance| instance.target_name() == "main_value_41"));
    let target = Label::parse("@com_google_absl//absl/strings:strings", None, None).unwrap();
    let mut graph = DependencyGraph::new();
    assert!(orchestrator.repository_manager.resolve_missing_dependency(&orchestrator, &mut graph, &target).await.unwrap());
    let source = orchestrator.external_repo_paths.lock().await["com_google_absl"].clone();
    assert!(source.starts_with(cache.join("bazel_output_base/external")));
    assert_eq!(std::fs::read_to_string(source.join("defs.bzl")).unwrap(), "VALUE = 41\n");
    assert!(std::fs::read_to_string(source.join("absl/strings/BUILD")).unwrap().contains("srcs = [\"value.cc\"]"));
    let instances = orchestrator.instantiate_build_file(&source.join("absl/strings/BUILD"), Some(&source)).unwrap();
    assert!(instances.iter().any(|instance| instance.target_name() == "strings"));
    // The module route must independently bypass an explicitly origin-tagged
    // adapter, including with a new genuine captured client environment.
    let alternate_cache = root.join("alternate-command-cache");
    let _alternate_shutdown = BazelShutdown { binary: binary.clone(), output: alternate_cache.join("bazel_output_base"), workspace: root.to_owned() };
    let persistent_loader = Arc::new(Mutex::new(rush_starlark::BzlLoader::new_with_global_cache(
        root.to_owned(), rush_starlark::bzl_loader::GlobalBzlCache::new(),
    )));
    persistent_loader.lock().unwrap().add_implicit_system_adapter("com_google_absl".into(), adapter.clone());
    let module_value = |environment_value: &str, command_cache: &std::path::Path| {
        let mut environment = std::env::vars_os().filter(|(name, _)| name != "IMPLICIT_SOURCE_ENV" && name != "RUSH_WORKSPACE_CACHE_ROOT")
            .map(|(name, value)| EnvironmentVariable {
                name: Bytes::copy_from_slice(name.as_os_str().as_bytes()),
                value: Bytes::copy_from_slice(value.as_os_str().as_bytes()),
            }).collect::<Vec<_>>();
        environment.push(EnvironmentVariable { name: Bytes::from_static(b"IMPLICIT_SOURCE_ENV"), value: Bytes::from(environment_value.to_owned()) });
        environment.push(EnvironmentVariable { name: Bytes::from_static(b"RUSH_WORKSPACE_CACHE_ROOT"), value: Bytes::copy_from_slice(command_cache.as_os_str().as_bytes()) });
        let request = RunRequest {
            auth_cookie: Bytes::new(), argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"build"), Bytes::from_static(b"//:test")],
            working_directory: Bytes::from(root.to_str().unwrap().to_owned()), environment,
            block_for_lock: true, client_build_id: "implicit-source-control".into(),
            protocol_major: PROTOCOL_MAJOR, protocol_minor: PROTOCOL_MINOR, workspace_config: vec![],
        };
        let io = Arc::new(RealTrackedIo::new(CommandSemantics::from_run_request(&request).unwrap(), root));
        tokio::task::block_in_place(|| tracked::scope(io, || {
            rush_starlark::BzlLoader::load_bzl_from_evaluator(persistent_loader.clone(), "@com_google_absl//:defs.bzl", &[("VALUE".into(), "VALUE".into())])
                .map(|values| values["VALUE"].clone())
        }))
    };
    assert_eq!(module_value("0", &cache).unwrap(), rush_starlark::Value::Int(41));
    let manifest = io.manifest();
    std::fs::write(root.join("fix.patch"), patch(2)).unwrap();
    assert!(matches!(io.validate_manifest(&manifest).staleness, Staleness::Stale { .. }));
    assert_eq!(module_value("0", &cache).unwrap(), rush_starlark::Value::Int(42));
    std::fs::write(root.join("source.txt"), "41\n").unwrap();
    assert_eq!(module_value("0", &cache).unwrap(), rush_starlark::Value::Int(43));
    assert_eq!(module_value("1", &cache).unwrap(), rush_starlark::Value::Int(44));
    // Reuse this loader and its resident global exports with a different
    // immutable client cache root and repository-rule environment. The old
    // genuine path remains managed authority, never an explicit user source.
    let first_source = persistent_loader.lock().unwrap().external_repo_path("com_google_absl").unwrap();
    assert!(persistent_loader.lock().unwrap().is_managed_workspace_source("com_google_absl", &first_source));
    assert_eq!(module_value("2", &alternate_cache).unwrap(), rush_starlark::Value::Int(45));
    let second_source = persistent_loader.lock().unwrap().external_repo_path("com_google_absl").unwrap();
    assert_ne!(first_source, second_source);
    assert!(second_source.starts_with(alternate_cache.join("bazel_output_base/external")));
    assert_eq!(module_value("1", &cache).unwrap(), rush_starlark::Value::Int(44));
    assert_eq!(std::fs::read(adapter.join("absl/strings/BUILD")).unwrap(), adapter_build);
    assert_eq!(std::fs::read_to_string(adapter.join("defs.bzl")).unwrap(), "VALUE = 99\n");
    // Unknown/deleted declarations and missing source inputs fail rather than
    // returning either the old genuine materialization or adapter value99.
    std::fs::remove_file(root.join("source.txt")).unwrap();
    assert!(module_value("1", &cache).is_err());
    std::fs::write(root.join("source.txt"), "41\n").unwrap();
    std::fs::write(root.join("WORKSPACE"), "workspace(name = \"implicit_precedence_control\")\n").unwrap();
    assert!(module_value("1", &cache).is_err());
    orchestrator.install_fresh_tracked_io_for_test();
    assert!(orchestrator.repository_manager.resolve_missing_dependency(&orchestrator, &mut graph, &target).await.is_err());
    let unknown = Label::parse("@unavailable_declared_source//:target", None, None).unwrap();
    assert!(orchestrator.repository_manager.resolve_missing_dependency(&orchestrator, &mut graph, &unknown).await.is_err());
    assert!(!graph.has_node(&target));
    assert!(!graph.has_node(&unknown));
    // Removing WORKSPACE entirely must not reinterpret a known managed root as
    // an explicit/no-WORKSPACE compatibility registration in either loader.
    std::fs::remove_file(root.join("WORKSPACE")).unwrap();
    assert!(module_value("1", &cache).is_err());
    orchestrator.install_fresh_tracked_io_for_test();
    assert!(orchestrator.repository_manager.resolve_missing_dependency(&orchestrator, &mut graph, &target).await.is_err());
    // A transition to Bzlmod likewise needs a new supported ownership proof;
    // neither graph nor module loading may reuse the former WORKSPACE bytes.
    std::fs::write(root.join("MODULE.bazel"), "module(name = \"authority_transition\")\n").unwrap();
    let error = module_value("1", &cache).unwrap_err();
    assert!(format!("{error:#}").contains("declared WORKSPACE repository fetch cannot use Bzlmod discovery"), "{error:#}");
    orchestrator.install_fresh_tracked_io_for_test();
    let error = orchestrator.repository_manager.resolve_missing_dependency(&orchestrator, &mut graph, &target).await.unwrap_err();
    assert!(format!("{error:#}").contains("declared WORKSPACE repository fetch cannot use Bzlmod discovery"), "{error:#}");
    assert!(!graph.has_node(&target));
}
