// Integration test: Bazel cc_library `include_prefix` (and its composition
// with `strip_include_prefix`).
//
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT
//
// `include_prefix` makes the headers in `hdrs` includable at
// `<include_prefix>/<rel>`, where `<rel>` is the header's package-relative
// path AFTER `strip_include_prefix` is removed first (the two compose). rush
// stages a `_virtual_includes` farm mirroring Bazel's bin-dir layout and adds
// that root to the consumer's quote-include search path. These tests drive a
// real C++ compile/link/run to prove the prefixed include resolves — no GPU
// needed.

use anyhow::{anyhow, Result};
use rush_build::BuildOrchestrator;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build a throwaway workspace under a unique temp dir.
fn scratch_workspace(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join(format!("rush_include_prefix_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    // Minimal workspace markers.
    fs::write(dir.join("WORKSPACE"), "workspace(name = \"ip_test\")\n").unwrap();
    dir
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// Find a built executable by basename anywhere under `rush-out/`.
fn find_binary(ws: &Path, name: &str) -> Option<PathBuf> {
    fn walk(dir: &Path, name: &str, out: &mut Option<PathBuf>) {
        if out.is_some() {
            return;
        }
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, name, out);
                } else if p.file_name().map(|n| n == name).unwrap_or(false) {
                    *out = Some(p);
                    return;
                }
            }
        }
    }
    let mut out = None;
    walk(&ws.join("rush-out"), name, &mut out);
    out
}

/// include_prefix only: header at `lib/greeter.h`, exposed as
/// `myprefix/greeter.h`; consumer includes `"myprefix/greeter.h"`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_include_prefix_resolves() -> Result<()> {
    let ws = scratch_workspace("basic");

    // Producer package `lib` with include_prefix = "myprefix". The consumer
    // is in another package, so Bazel requires the producer rule itself to be
    // public; omitted rule visibility is package-private.
    write(
        &ws.join("lib/greeter.h"),
        "#pragma once\nconst char* greeting();\n",
    );
    write(
        &ws.join("lib/greeter.cc"),
        "#include \"myprefix/greeter.h\"\nconst char* greeting() { return \"hello-include-prefix\"; }\n",
    );
    write(
        &ws.join("lib/BUILD"),
        r#"
cc_library(
    name = "greeter",
    srcs = ["greeter.cc"],
    hdrs = ["greeter.h"],
    include_prefix = "myprefix",
    visibility = ["//visibility:public"],
)
"#,
    );

    // Consumer binary in package `app`, includes the PREFIXED path.
    write(
        &ws.join("app/main.cc"),
        "#include \"myprefix/greeter.h\"\n#include <cstdio>\nint main() { std::printf(\"%s\\n\", greeting()); return 0; }\n",
    );
    write(
        &ws.join("app/BUILD"),
        r#"
cc_binary(
    name = "app",
    srcs = ["main.cc"],
    deps = ["//lib:greeter"],
)
"#,
    );

    let orchestrator = BuildOrchestrator::with_workspace(ws.clone());
    orchestrator
        .build_with_dependencies(vec!["//app:app".to_string()])
        .await
        .map_err(|e| anyhow!("build of //app:app failed: {}", e))?;

    // The virtual-include farm must contain the header at the prefixed path.
    let vheader = ws.join("rush-out/_virtual_includes/lib/greeter/myprefix/greeter.h");
    assert!(
        vheader.exists(),
        "virtual-include header not staged at {}",
        vheader.display()
    );

    // Binary must build and run with the expected output.
    let bin = find_binary(&ws, "app").ok_or_else(|| anyhow!("binary 'app' not produced"))?;
    let out = Command::new(&bin).output()?;
    assert!(
        out.status.success(),
        "binary failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hello-include-prefix"),
        "unexpected output: {}",
        stdout
    );

    let _ = fs::remove_dir_all(&ws);
    Ok(())
}

/// strip_include_prefix + include_prefix COMPOSE: header sits at
/// `src/include/widget.h`; strip_include_prefix = "src/include" removes that,
/// then include_prefix = "vendor/widget" prepends → exposed as
/// `vendor/widget/widget.h`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_strip_then_include_prefix_compose() -> Result<()> {
    let ws = scratch_workspace("compose");

    write(
        &ws.join("vendorlib/src/include/widget.h"),
        "#pragma once\nint widget_value();\n",
    );
    write(
        &ws.join("vendorlib/widget.cc"),
        "#include \"vendor/widget/widget.h\"\nint widget_value() { return 4242; }\n",
    );
    // This producer is also consumed cross-package and therefore declares the
    // public rule visibility required by Bazel's private default.
    write(
        &ws.join("vendorlib/BUILD"),
        r#"
cc_library(
    name = "widget",
    srcs = ["widget.cc"],
    hdrs = ["src/include/widget.h"],
    strip_include_prefix = "src/include",
    include_prefix = "vendor/widget",
    visibility = ["//visibility:public"],
)
"#,
    );

    write(
        &ws.join("client/main.cc"),
        "#include \"vendor/widget/widget.h\"\n#include <cstdio>\nint main() { std::printf(\"value=%d\\n\", widget_value()); return 0; }\n",
    );
    write(
        &ws.join("client/BUILD"),
        r#"
cc_binary(
    name = "client",
    srcs = ["main.cc"],
    deps = ["//vendorlib:widget"],
)
"#,
    );

    let orchestrator = BuildOrchestrator::with_workspace(ws.clone());
    orchestrator
        .build_with_dependencies(vec!["//client:client".to_string()])
        .await
        .map_err(|e| anyhow!("build of //client:client failed: {}", e))?;

    // Composed path: strip "src/include", prepend "vendor/widget".
    let vheader =
        ws.join("rush-out/_virtual_includes/vendorlib/widget/vendor/widget/widget.h");
    assert!(
        vheader.exists(),
        "composed virtual-include header not staged at {}",
        vheader.display()
    );

    let bin =
        find_binary(&ws, "client").ok_or_else(|| anyhow!("binary 'client' not produced"))?;
    let out = Command::new(&bin).output()?;
    assert!(
        out.status.success(),
        "binary failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("value=4242"), "unexpected output: {}", stdout);

    let _ = fs::remove_dir_all(&ws);
    Ok(())
}
