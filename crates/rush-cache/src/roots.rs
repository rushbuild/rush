// Rush Cache - the one authority for cache locations
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Where rush's caches live — resolved in exactly one place.
//!
//! Three call sites used to compute this independently: the action
//! cache (`dirs::home_dir()`), the CAS in the action manager (`HOME`),
//! and the CAS in the virtual materializer (`HOME`). They agreed by
//! coincidence, and nothing outside the process could ask where they
//! landed. The benchmark harness paid for that: it "cleared rush's disk
//! cache" before a cold run by deleting a WORKSPACE-local
//! `.cache/rush`, which holds no cache entries at all, so a cold run
//! could silently become a repeat-cold run once real content-addressed
//! restoration existed.
//!
//! One resolver, one override, and a way to ask.

use std::path::PathBuf;

/// Environment override for the whole cache root. `--disk-cache-dir`
/// sets this so the flag moves the ActionResults AND the blobs, rather
/// than splitting them across two roots.
pub const CACHE_ROOT_ENV: &str = "RUSH_CACHE_ROOT";

/// The base directory holding every persistent cache rush owns.
///
/// Resolution order: `$RUSH_CACHE_ROOT`, then `$HOME/.cache/rush`, then
/// `/tmp/rush-cache` for HOME-less environments (some CI and sandbox
/// contexts) so the caches degrade to a working temporary location
/// instead of silently disappearing.
pub fn cache_root() -> PathBuf {
    if let Some(dir) = std::env::var_os(CACHE_ROOT_ENV) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".cache/rush");
        }
    }
    PathBuf::from("/tmp/rush-cache")
}

/// Where serialized `ActionResult` entries live (`<root>/*.cache`).
pub fn action_cache_root() -> PathBuf {
    cache_root()
}

/// Where content-addressed blobs live. `CasStore::new` appends `cas`
/// itself, so this is what to hand it.
pub fn cas_store_root() -> PathBuf {
    cache_root()
}

/// The directory blobs actually land in — what a script must delete to
/// make a build genuinely cold.
pub fn cas_blob_dir() -> PathBuf {
    cache_root().join("cas")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_override_wins_and_cas_hangs_off_the_same_root() {
        // Serialized by construction: this test owns the env var it sets.
        std::env::set_var(CACHE_ROOT_ENV, "/tmp/rush-cache-root-test");
        assert_eq!(cache_root(), PathBuf::from("/tmp/rush-cache-root-test"));
        assert_eq!(
            cas_blob_dir(),
            PathBuf::from("/tmp/rush-cache-root-test/cas")
        );
        assert_eq!(action_cache_root(), cas_store_root());
        std::env::remove_var(CACHE_ROOT_ENV);
    }
}
