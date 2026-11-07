// Rush Repo - Repository Rules
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Repository rule implementations for external dependencies.
//!
//! This crate provides implementations for various repository rules that allow
//! Rush to fetch external dependencies from different sources (HTTP archives,
//! Git repositories, etc.).

#![warn(missing_docs)]

pub mod cache;
pub mod git;
pub mod http;
pub mod http_archive;
pub mod starlark;
pub mod workspace;

pub use cache::RepositoryCache;
pub use git::GitRepository;
pub use http::HttpArchive;
pub use http_archive::{ArchiveType, HttpArchiveManager};
pub use starlark::StarlarkLoader;
pub use workspace::{
    parse_workspace, parse_workspace_content, LoadStatement, RepositoryRule, Value, WorkspaceFile,
};
