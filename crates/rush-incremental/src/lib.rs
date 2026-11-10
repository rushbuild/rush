//! `rush-incremental` — tracked I/O and the dependency manifest that makes
//! daemon-side graph reuse *admissible*.
//!
//! # Why this exists (Codex P0 risk #1)
//!
//! The rush persistent daemon (see `docs/design/persistent-daemon-skyframe.md`)
//! wants to retain the loaded + analyzed build graph across invocations and reuse
//! it on warm rebuilds. Reuse is only **safe** if rush recorded *every* dependency
//! the analysis observed — every file content it read, every `exists()` probe,
//! every `read_dir` (glob / package-boundary) listing, every environment variable
//! and build option — and can re-check all of them against the current world
//! before serving a cached result. Today analysis does *untracked* filesystem and
//! env reads, so a naive daemon would happily serve a **stale or wrong** build.
//!
//! This crate builds the abstraction that closes that hole:
//!
//! * [`TrackedIo`] — the interface analysis must read the world through. Every
//!   accessor *records* what it observed as it returns it.
//! * [`RealTrackedIo`] — the production impl: real filesystem for file ops, a
//!   [`CommandSemantics`] (never `std::env`) for env/options.
//! * [`DependencyManifest`] — the ordered, deduped, deterministic snapshot of
//!   everything one analysis observed; the daemon's reuse-admission key. It can be
//!   [`fingerprint`](DependencyManifest::fingerprint)ed for cheap equality and
//!   [`validate`](DependencyManifest::validate)d against the current world.
//!
//! **Scope guard:** this crate is additive. It does NOT migrate any existing read
//! site to go through [`TrackedIo`] (a later brick), is not wired into the CLI,
//! and changes no build behavior.

mod dependency;
mod manifest;
mod probe;
pub mod tracked;
mod tracked_io;

pub use crate::dependency::{content_digest, dir_listing_digest, Dependency, FileContentBinding, FileKind, Staleness};
pub use crate::manifest::{DependencyManifest, Validated};
pub use crate::probe::{content_digest_of_regular_file, stat_meta, FileMeta};
pub use crate::tracked_io::{
    CanonicalRegularFileMetadata, CanonicalRegularFileRead, ProcessProbeResult, RealTrackedIo, TrackedIo,
};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tracked_tests;
