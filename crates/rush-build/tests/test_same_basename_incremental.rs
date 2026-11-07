//! Same-basename cross-package incremental-correctness regression test.
//! Copyright © 2026 Luis Chamberlain. All rights reserved.
//! SPDX-License-Identifier: MIT
//!
//! Locks in the invariant the rushbench wide_Nk shape exercises and that
//! TensorFlow / Triton hit in the wild: a workspace with N packages that EACH
//! contain a `cc_library` whose only source is a file with the SAME basename
//! (`lib.cc` in `pkg_0/`, `pkg_1/`, ... — distinct paths, colliding basename).
//!
//! `cc_obj_pkg_ext` maps a bare same-package src `lib.cc` to the key
//! `(pkg="", stem="lib", ext="cc")` — IDENTICAL across every package — so all
//! N translation units share one object NAME (`0.3.lib2.cc.o`). They do not
//! collide on disk only because `BuildContext::target_output_dir` qualifies the
//! object path by the target's PACKAGE (`rush-out/pkg_<i>/lib/...`). The whole
//! source→object freshness mapping therefore rests on that package
//! qualification: if it ever regressed (or a stale object from an older
//! object-naming scheme aliased the freshness ledger), editing `pkg_N/lib.cc`
//! would silently fail to recompile `pkg_N`'s object — a latent
//! incremental-correctness bug masked on real single-file targets like
//! `hello`, exposed only by N same-basename srcs.
//!
//! Contract verified end-to-end through the real rush binary on the
//! `--daemon=off` scan path (the path the bench finding reproduced on):
//!   1. clean `build //...` produces one distinct object per package
//!   2. a no-op rebuild changes nothing (stable freshness)
//!   3. editing ONE package's `lib.cc` recompiles EXACTLY that package's
//!      object — and the dependent binary — and nothing else
//!   4. editing a header shared (by basename) across packages recompiles only
//!      the package that actually includes it (each package owns its own copy)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn rush_binary() -> PathBuf {
    // Integration tests run from `crates/rush-build/`; the workspace root is
    // two levels up, and the release binary lives under target/release/.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target")
        .join("release")
        .join("rush")
}

/// Generate a minimal rushbench-shaped workspace: `packages` packages, each a
/// `cc_library` named `lib` with a single same-basename `lib.cc` that includes
/// `headers` per-package headers and returns their sum; plus a root cc_binary
/// `bench` calling every `lib_<i>_sum()`. Mirrors benchmarks/rushbench/gen.py.
fn gen_workspace(root: &Path, packages: usize, headers: usize) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join("WORKSPACE"), "workspace(name = \"rbench\")\n").unwrap();

    // Root cc_binary depending on all libraries.
    let mut decls = String::new();
    let mut calls = String::new();
    let mut deps = String::new();
    for i in 0..packages {
        decls.push_str(&format!("int lib_{i}_sum();\n"));
        calls.push_str(&format!("  total += lib_{i}_sum();\n"));
        deps.push_str(&format!("        \"//pkg_{i}:lib\",\n"));
    }
    std::fs::write(
        root.join("main.cc"),
        format!(
            "#include <cstdio>\n{decls}\nint main() {{\n  long total = 0;\n{calls}\
             \n  std::printf(\"rbench total=%ld\\n\", total);\n  return 0;\n}}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("BUILD"),
        format!("cc_binary(\n    name = \"bench\",\n    srcs = [\"main.cc\"],\n    deps = [\n{deps}    ],\n)\n"),
    )
    .unwrap();

    for i in 0..packages {
        let pkg = root.join(format!("pkg_{i}"));
        std::fs::create_dir_all(&pkg).unwrap();

        let mut includes = String::new();
        let mut terms = String::new();
        let mut hdr_attr = String::new();
        for h in 0..headers {
            includes.push_str(&format!("#include \"hdr_{h}.h\"\n"));
            if h > 0 {
                terms.push_str(" + ");
            }
            terms.push_str(&format!("pkg_{i}::kHdr{h}"));
            if h > 0 {
                hdr_attr.push_str(", ");
            }
            hdr_attr.push_str(&format!("\"hdr_{h}.h\""));

            let val = (i.wrapping_mul(1315423911).wrapping_add(h.wrapping_mul(2654435761))) & 0xFFFF;
            std::fs::write(
                pkg.join(format!("hdr_{h}.h")),
                format!(
                    "#ifndef PKG_{i}_HDR_{h}_H_\n#define PKG_{i}_HDR_{h}_H_\n\
                     namespace pkg_{i} {{\nconstexpr int kHdr{h} = {val};\n\
                     }}\n#endif\n"
                ),
            )
            .unwrap();
        }
        if terms.is_empty() {
            terms.push('0');
        }
        // SAME basename `lib.cc` in every package — the collision class.
        std::fs::write(
            pkg.join("lib.cc"),
            format!("{includes}\nint lib_{i}_sum() {{\n  return {terms};\n}}\n"),
        )
        .unwrap();
        std::fs::write(
            pkg.join("BUILD"),
            format!(
                "cc_library(\n    name = \"lib\",\n    srcs = [\"lib.cc\"],\n\
                     hdrs = [{hdr_attr}],\n    visibility = [\"//visibility:public\"],\n)\n"
            ),
        )
        .unwrap();
    }
}

/// Run `rush build //... --daemon=off` in `root`, returning success.
fn rush_build(rush: &Path, root: &Path) -> bool {
    let output = Command::new(rush)
        .arg("build")
        .arg("//...")
        .arg("--daemon=off")
        .current_dir(root)
        .output()
        .expect("failed to spawn rush");
    if !output.status.success() {
        eprintln!(
            "rush build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    output.status.success()
}

/// Map of every per-package library object path -> its content bytes, so we can
/// detect EXACTLY which translation units recompiled across a rebuild.
fn snapshot_objects(root: &Path, packages: usize) -> BTreeMap<String, Vec<u8>> {
    let mut map = BTreeMap::new();
    for i in 0..packages {
        let dir = root.join("rush-out").join(format!("pkg_{i}")).join("lib");
        // Each package's library compiles to exactly one object; find it by
        // extension rather than hard-coding the (verified-core) object name.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) == Some("o") {
                    let bytes = std::fs::read(&p).unwrap();
                    map.insert(format!("pkg_{i}:{}", p.file_name().unwrap().to_string_lossy()), bytes);
                }
            }
        }
    }
    map
}

fn changed_packages(
    before: &BTreeMap<String, Vec<u8>>,
    after: &BTreeMap<String, Vec<u8>>,
) -> Vec<String> {
    let mut changed = Vec::new();
    for (k, v) in after {
        match before.get(k) {
            Some(old) if old == v => {}
            _ => changed.push(k.clone()),
        }
    }
    changed.sort();
    changed
}

#[test]
fn same_basename_cross_package_edit_recompiles_only_that_package() {
    let rush = rush_binary();
    if !rush.exists() {
        eprintln!(
            "skipping: rush binary not built at {}; run `cargo build --release --bin rush` first",
            rush.display()
        );
        return;
    }
    // Honor the global clone cache if the harness sets one (keeps the test off
    // the network) — purely environmental, no effect on the assertions.
    let tmp = std::env::temp_dir().join(format!("rush_samebasename_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);

    const PACKAGES: usize = 6;
    const HEADERS: usize = 3;
    gen_workspace(&tmp, PACKAGES, HEADERS);

    // 1) clean build — one distinct object per package.
    assert!(rush_build(&rush, &tmp), "clean build //... must succeed");
    let baseline = snapshot_objects(&tmp, PACKAGES);
    assert_eq!(
        baseline.len(),
        PACKAGES,
        "each of the {PACKAGES} same-basename packages must produce its own object \
         (got {baseline:?})"
    );
    let bench = tmp.join("rush-out").join("bench");
    let bench_before = std::fs::read(&bench).expect("bench binary must exist after build");

    // 2) no-op rebuild changes nothing.
    assert!(rush_build(&rush, &tmp), "no-op rebuild must succeed");
    let after_noop = snapshot_objects(&tmp, PACKAGES);
    assert!(
        changed_packages(&baseline, &after_noop).is_empty(),
        "a no-op rebuild must recompile NOTHING; changed: {:?}",
        changed_packages(&baseline, &after_noop)
    );

    // 3) edit pkg_2/lib.cc — ONLY pkg_2's object (and the binary) recompiles.
    let edited = tmp.join("pkg_2").join("lib.cc");
    let src = std::fs::read_to_string(&edited).unwrap();
    std::fs::write(
        &edited,
        src.replace(
            "int lib_2_sum() {",
            "int lib_2_sum() {\n  volatile int rush_edit_marker = 0x5A5A;\n  (void)rush_edit_marker;",
        ),
    )
    .unwrap();
    assert!(rush_build(&rush, &tmp), "rebuild after pkg_2 edit must succeed");
    let after_edit = snapshot_objects(&tmp, PACKAGES);
    let changed = changed_packages(&baseline, &after_edit);
    assert_eq!(
        changed,
        vec!["pkg_2:0.3.lib2.cc.o".to_string()],
        "editing pkg_2/lib.cc must recompile EXACTLY pkg_2's object and nothing else; \
         changed: {changed:?}"
    );
    let bench_after = std::fs::read(&bench).expect("bench binary must exist");
    assert_ne!(
        bench_before, bench_after,
        "the dependent binary must relink after a transitive source edit"
    );

    // 4) edit pkg_4's header — only pkg_4 (the package that includes it)
    // recompiles; the same-basename hdr_0.h in other packages is untouched.
    let baseline2 = snapshot_objects(&tmp, PACKAGES);
    let hdr = tmp.join("pkg_4").join("hdr_0.h");
    let hsrc = std::fs::read_to_string(&hdr).unwrap();
    std::fs::write(&hdr, hsrc.replace("constexpr int kHdr0 =", "constexpr int kHdr0 = 7 +")).unwrap();
    assert!(rush_build(&rush, &tmp), "rebuild after pkg_4 header edit must succeed");
    let after_hdr = snapshot_objects(&tmp, PACKAGES);
    assert_eq!(
        changed_packages(&baseline2, &after_hdr),
        vec!["pkg_4:0.3.lib2.cc.o".to_string()],
        "editing pkg_4/hdr_0.h must recompile only pkg_4's object (its own header copy)"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
