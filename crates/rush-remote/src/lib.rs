// Rush Remote - Remote execution client
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Remote execution client for distributed builds.

#![warn(missing_docs)]

pub mod cas;
pub mod executor;
pub mod http_cache;

/// Standard Remote Execution API wire types, generated from pinned upstreams.
#[allow(missing_docs)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/reapi.rs"));
}

pub use cas::{CasClient, CasStats, Digest, DirectoryManifest};
pub use executor::{RemoteExecutor, RemoteExecutorStats, RemoteStrategy};
pub use http_cache::HttpCache;

/// Remote execution client (gRPC).
///
/// Will implement Remote Execution API v2 protocol.
pub struct RemoteClient {
    _endpoint: String,
}

impl RemoteClient {
    /// Create a new remote client.
    pub fn new(endpoint: String) -> Self {
        RemoteClient {
            _endpoint: endpoint,
        }
    }
}
