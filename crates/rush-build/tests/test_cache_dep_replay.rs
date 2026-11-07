// Persistent-daemon Run-wiring (blocker #1) correctness proof.
//
// The daemon keeps process-global `.bzl` and BUILD-file caches. A `.bzl`/BUILD
// loaded in command 1 is a CACHE HIT in command 2. Before this fix a hit returned
// the cached value WITHOUT re-recording the dependencies that load observed, so
// command 2's manifest was INCOMPLETE — a later edit to a cached `.bzl` would not
// invalidate a snapshot reused under that manifest (a WRONG build, served fast).
//
// The fix stores each cache entry's tracked dependency set on the miss path and
// REPLAYS it into the current command's manifest on a hit. This test proves it
// end-to-end through the real orchestrator + the live `with_tracked_scope`
// plumbing:
//
//   command 1: install a fresh tracked IO, parse a BUILD that loads defs.bzl which
//              transitively loads base.bzl (cache MISS) -> the IO records the BUILD
//              content + both `.bzl` file contents.
//   command 2: install a SECOND fresh, empty tracked IO, parse the SAME BUILD on
//              the SAME orchestrator (BUILD-cache + global `.bzl`-cache HIT) -> the
//              replay must make command 2's manifest contain the BUILD file dep AND
//              both transitive `.bzl` file deps, even though zero parsing happened.

use rush_build::BuildOrchestrator;
use rush_incremental::{Dependency, TrackedIo};
use std::fs;
use tempfile::TempDir;

/// Does `deps` contain a FileContent dep whose path ends with `name`?
fn has_file_content(deps: &[Dependency], name: &str) -> bool {
    deps.iter()
        .any(|d| matches!(d, Dependency::FileContent { path, .. } if path.ends_with(name)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_command_cache_hit_replays_transitive_bzl_deps() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let root = temp.path();

    fs::write(root.join("WORKSPACE"), "workspace(name = \"replay_test\")\n")?;

    // base.bzl — the transitive leaf. defs.bzl loads it; the BUILD loads defs.bzl.
    fs::write(
        root.join("base.bzl"),
        "BASE_COPTS = [\"-O2\"]\n",
    )?;
    // defs.bzl loads base.bzl and re-exports a symbol used by the BUILD.
    fs::write(
        root.join("defs.bzl"),
        "load(\"//:base.bzl\", \"BASE_COPTS\")\n\
         def my_lib(name):\n\
         \x20\x20native.cc_library(name = name, copts = BASE_COPTS)\n",
    )?;
    // BUILD loads defs.bzl and calls the macro.
    let build_path = root.join("BUILD");
    fs::write(
        &build_path,
        "load(\"//:defs.bzl\", \"my_lib\")\n\
         my_lib(name = \"hello\")\n",
    )?;

    // ONE orchestrator across both commands: the BUILD-file cache is per-orchestrator
    // and the `.bzl` cache is the process-global singleton — both persist between the
    // two parses, exactly like a daemon reusing them across commands.
    let orchestrator = BuildOrchestrator::with_workspace(root.to_path_buf());

    // ---- command 1: cold parse (cache MISS) ----
    let io1 = orchestrator.install_fresh_tracked_io_for_test();
    let _ = orchestrator.instantiate_build_file(&build_path, None)?;
    let m1 = io1.manifest();
    // Sanity: the cold parse recorded the BUILD + both transitive `.bzl` files.
    assert!(
        has_file_content(m1.deps(), "BUILD"),
        "command 1 recorded BUILD content"
    );
    assert!(
        has_file_content(m1.deps(), "defs.bzl"),
        "command 1 recorded defs.bzl content"
    );
    assert!(
        has_file_content(m1.deps(), "base.bzl"),
        "command 1 recorded the transitive base.bzl content"
    );

    // ---- command 2: warm parse (BUILD-cache + global `.bzl`-cache HIT) ----
    let io2 = orchestrator.install_fresh_tracked_io_for_test();
    assert!(io2.manifest().is_empty(), "command 2 starts with an empty manifest");
    let _ = orchestrator.instantiate_build_file(&build_path, None)?;
    let m2 = io2.manifest();

    // THE proof: the cache hit REPLAYED the dependency set, so command 2's manifest
    // is complete as if the load had happened — it contains the BUILD file dep AND
    // both transitive `.bzl` file deps, despite zero re-parsing.
    assert!(
        !m2.is_empty(),
        "command 2 (all cache hits) still has a non-empty manifest via replay"
    );
    assert!(
        has_file_content(m2.deps(), "BUILD"),
        "command 2 replayed the BUILD file dep (BUILD-cache hit replay)"
    );
    assert!(
        has_file_content(m2.deps(), "defs.bzl"),
        "command 2 replayed the directly-loaded defs.bzl dep (.bzl-cache hit replay)"
    );
    assert!(
        has_file_content(m2.deps(), "base.bzl"),
        "command 2 replayed the TRANSITIVE base.bzl dep (self-composition: \
         the BUILD/defs.bzl dep_set carries base.bzl)"
    );

    Ok(())
}
