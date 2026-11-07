// Rush Build - ml_dtypes / Eigen vendored-clone detection (Lane C3)
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 → Lane C step C3 (Pro 2026-05-26): structured
//! detection of the ml_dtypes / eigen_archive vendored clones
//! laid down by `scripts/bootstrap_ml_dtypes_eigen.sh`.
//!
//! Sister module to `verified_execution_shape.rs`
//! (Lane A) and `external_deps.rs` (the existing
//! `system_include_dirs_for_repo` mapper). This module
//! reports the DETECTION OUTCOME as a typed enum so the
//! candidate scanner can:
//!
//!   - Wire the vendored Eigen include root through when
//!     the clone is present.
//!   - Classify the candidate blocker as
//!     `MissingVendoredDependency(ml_dtypes_eigen)` when
//!     the clone is absent — keeping the current behavior
//!     (system Eigen fallback) but surfacing the missing
//!     dep cleanly in the dry-run report.
//!
//! ## Search order
//!
//!   1. Workspace external: `<workspace>/external/<repo>/`
//!      (e.g., `tensorflow/external/ml_dtypes/`). Highest
//!      precedence — operators using a workspace-local
//!      checkout don't need any host-global cache.
//!   2. Host-global clone cache:
//!      `$RUSH_GLOBAL_CLONE_CACHE/<repo>/` (default
//!      `/data/rush-cache/external/<repo>/`). This is what
//!      `bootstrap_ml_dtypes_eigen.sh --apply` writes.
//!   3. (Eigen only) System Eigen at `/usr/include/eigen3`.
//!      Recorded as `SystemFallback` — the float8.h header
//!      may compile but the BO audit shows it usually
//!      doesn't (missing `Traits<float>::BitsType`).
//!   4. Absent. Returns `Missing` with structured evidence.

use std::path::{Path, PathBuf};

/// Detection outcome for the `@ml_dtypes` external repo.
/// This is what `@ml_dtypes//include/float8.h`-consuming
/// targets need to compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MlDtypesStatus {
    /// Workspace-local checkout at `<workspace>/external/ml_dtypes/`.
    /// Field is the include root that propagates to consumers.
    WorkspacePresent { include_root: PathBuf },
    /// Host-global clone at `<global>/ml_dtypes/`.
    /// Field is the include root.
    GlobalCachePresent { include_root: PathBuf },
    /// Clone absent at both locations. The candidate
    /// scanner classifies this as
    /// `MissingVendoredDependency(ml_dtypes)`.
    Missing { evidence: String },
}

/// Detection outcome for the `@eigen_archive` external
/// repo (TF-pinned Eigen at commit
/// `66e8f38891841bf88ee976a316c0c78a52f0cee5`). Required
/// for `@ml_dtypes//include/float8.h` to compile cleanly
/// (the `Traits<float>::BitsType` template path needs
/// this specific Eigen version).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MlDtypesEigenStatus {
    /// Workspace-local checkout at
    /// `<workspace>/external/eigen_archive/` OR the
    /// legacy `<workspace>/external/ml_dtypes_eigen/`
    /// alias. Field is the include root (the directory
    /// containing `Eigen/Core`).
    WorkspacePresent { include_root: PathBuf },
    /// Host-global clone at `<global>/eigen_archive/`.
    GlobalCachePresent { include_root: PathBuf },
    /// Fall back to system Eigen at `/usr/include/eigen3`.
    /// This is the current default but is INSUFFICIENT for
    /// ml_dtypes float8.h on most hosts (the BO audit shows
    /// the `Traits<float>::BitsType` requirement is unmet
    /// by stock system Eigen). The candidate scanner SHOULD
    /// still surface this as
    /// `MissingVendoredDependency(ml_dtypes_eigen)` because
    /// the vendored clone is what TF actually pins.
    SystemFallback { include_root: PathBuf },
    /// Clone absent and system Eigen also missing.
    Missing { evidence: String },
}

impl MlDtypesStatus {
    /// Returns the include root iff the clone is present;
    /// `None` for the `Missing` variant.
    pub fn include_root(&self) -> Option<&Path> {
        match self {
            MlDtypesStatus::WorkspacePresent { include_root }
            | MlDtypesStatus::GlobalCachePresent { include_root } => Some(include_root),
            MlDtypesStatus::Missing { .. } => None,
        }
    }

    /// True iff a vendored clone was found (either
    /// workspace or global cache).
    pub fn is_present(&self) -> bool {
        !matches!(self, MlDtypesStatus::Missing { .. })
    }

    /// If `Missing`, build the structured blocker the
    /// candidate scanner records. Returns `None` for
    /// present variants.
    pub fn to_blocker(&self) -> Option<crate::candidate_blocker::CandidateBlocker> {
        match self {
            MlDtypesStatus::Missing { evidence } => Some(
                crate::candidate_blocker::CandidateBlocker::MissingVendoredDependency {
                    name: "ml_dtypes".to_string(),
                    evidence: evidence.clone(),
                },
            ),
            _ => None,
        }
    }
}

impl MlDtypesEigenStatus {
    /// Returns the include root that resolves consumer
    /// `#include <Eigen/Core>` includes. Always `Some`
    /// except for the `Missing` variant.
    pub fn include_root(&self) -> Option<&Path> {
        match self {
            MlDtypesEigenStatus::WorkspacePresent { include_root }
            | MlDtypesEigenStatus::GlobalCachePresent { include_root }
            | MlDtypesEigenStatus::SystemFallback { include_root } => Some(include_root),
            MlDtypesEigenStatus::Missing { .. } => None,
        }
    }

    /// True iff a vendored clone was found. `SystemFallback`
    /// returns FALSE — the system Eigen path is "Eigen is
    /// installed" but NOT "the TF-pinned ml_dtypes_eigen is
    /// installed", which is what the candidate scanner
    /// needs to know.
    pub fn is_vendored_present(&self) -> bool {
        matches!(
            self,
            MlDtypesEigenStatus::WorkspacePresent { .. }
                | MlDtypesEigenStatus::GlobalCachePresent { .. }
        )
    }

    /// Lane C blocker classification. Per Pro C3 spec:
    /// "If absent: keep current system Eigen behavior BUT
    /// classify candidate blocker as
    /// MissingVendoredDependency(ml_dtypes_eigen)."
    ///
    /// So this returns `Some(MissingVendoredDependency)` for
    /// BOTH `Missing` AND `SystemFallback` variants — the
    /// presence of system Eigen does not satisfy the
    /// vendored-dep contract. Returns `None` only when the
    /// vendored clone is actually present.
    pub fn to_blocker(&self) -> Option<crate::candidate_blocker::CandidateBlocker> {
        match self {
            MlDtypesEigenStatus::Missing { evidence } => Some(
                crate::candidate_blocker::CandidateBlocker::MissingVendoredDependency {
                    name: "ml_dtypes_eigen".to_string(),
                    evidence: evidence.clone(),
                },
            ),
            MlDtypesEigenStatus::SystemFallback { include_root } => Some(
                crate::candidate_blocker::CandidateBlocker::MissingVendoredDependency {
                    name: "ml_dtypes_eigen".to_string(),
                    evidence: format!(
                        "system Eigen at {:?} found but TF-pinned ml_dtypes_eigen \
                         (commit 66e8f38891841bf88ee976a316c0c78a52f0cee5) absent; \
                         float8.h needs Traits<float>::BitsType",
                        include_root
                    ),
                },
            ),
            _ => None,
        }
    }
}

/// Detect ml_dtypes vendored clone presence.
///
/// Search order: workspace external → host-global clone
/// cache. Returns the first match. Returns `Missing`
/// when neither is found.
///
/// `workspace_root` is the rush workspace root (e.g.,
/// `/work/tensorflow`). `global_cache_root`
/// is `$RUSH_GLOBAL_CLONE_CACHE` (typically
/// `/data/rush-cache/external/`).
pub fn detect_ml_dtypes(
    workspace_root: &Path,
    global_cache_root: &Path,
) -> MlDtypesStatus {
    // Witness file: ml_dtypes/include/float8.h is what
    // consumers need.
    let workspace_path = workspace_root.join("external/ml_dtypes");
    if workspace_path.join("include/float8.h").exists() {
        return MlDtypesStatus::WorkspacePresent {
            include_root: workspace_path,
        };
    }

    let global_path = global_cache_root.join("ml_dtypes");
    if global_path.join("include/float8.h").exists() {
        return MlDtypesStatus::GlobalCachePresent {
            include_root: global_path,
        };
    }

    MlDtypesStatus::Missing {
        evidence: format!(
            "ml_dtypes/include/float8.h not found at \
             {:?}/external/ml_dtypes or {:?}/ml_dtypes; \
             run scripts/bootstrap_ml_dtypes_eigen.sh --apply",
            workspace_root, global_cache_root,
        ),
    }
}

/// Detect ml_dtypes-compatible Eigen vendored clone.
///
/// Search order:
///   1. Workspace `external/eigen_archive/` (TF-pinned).
///   2. Workspace `external/ml_dtypes_eigen/` (legacy
///      alias from earlier rush versions).
///   3. Global cache `$RUSH_GLOBAL_CLONE_CACHE/eigen_archive/`.
///   4. System `/usr/include/eigen3/` (records
///      `SystemFallback`).
///   5. Absent (records `Missing`).
pub fn detect_ml_dtypes_eigen(
    workspace_root: &Path,
    global_cache_root: &Path,
) -> MlDtypesEigenStatus {
    let workspace_eigen = workspace_root.join("external/eigen_archive");
    if workspace_eigen.join("Eigen/Core").exists() {
        return MlDtypesEigenStatus::WorkspacePresent {
            include_root: workspace_eigen,
        };
    }

    let workspace_md_eigen = workspace_root.join("external/ml_dtypes_eigen");
    if workspace_md_eigen.join("Eigen/Core").exists() {
        return MlDtypesEigenStatus::WorkspacePresent {
            include_root: workspace_md_eigen,
        };
    }

    let global_eigen = global_cache_root.join("eigen_archive");
    if global_eigen.join("Eigen/Core").exists() {
        return MlDtypesEigenStatus::GlobalCachePresent {
            include_root: global_eigen,
        };
    }

    let system_eigen = PathBuf::from("/usr/include/eigen3");
    if system_eigen.join("Eigen/Core").exists() {
        return MlDtypesEigenStatus::SystemFallback {
            include_root: system_eigen,
        };
    }

    MlDtypesEigenStatus::Missing {
        evidence: format!(
            "Eigen/Core not found at {:?}/external/eigen_archive, \
             {:?}/external/ml_dtypes_eigen, {:?}/eigen_archive, \
             or /usr/include/eigen3; run scripts/bootstrap_ml_dtypes_eigen.sh --apply",
            workspace_root, workspace_root, global_cache_root,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tmpdir")
    }

    /// Pro C3 spec test: when the vendored Eigen clone is
    /// present (workspace or global cache), the stub
    /// reports the include root and does NOT classify a
    /// MissingVendoredDependency blocker.
    #[test]
    fn ml_dtypes_stub_uses_vendored_eigen_when_present() {
        let workspace = tmpdir();
        let cache = tmpdir();

        // Lay down a fake eigen_archive in the global
        // cache.
        let eigen_root = cache.path().join("eigen_archive");
        fs::create_dir_all(eigen_root.join("Eigen")).unwrap();
        fs::write(eigen_root.join("Eigen/Core"), "// stub").unwrap();

        let status = detect_ml_dtypes_eigen(workspace.path(), cache.path());

        assert!(status.is_vendored_present());
        match &status {
            MlDtypesEigenStatus::GlobalCachePresent { include_root } => {
                assert_eq!(include_root, &eigen_root);
            }
            other => panic!("expected GlobalCachePresent, got {:?}", other),
        }
        assert!(status.to_blocker().is_none(),
            "vendored present must NOT yield a blocker");

        // Workspace takes precedence.
        let workspace_eigen = workspace.path().join("external/eigen_archive");
        fs::create_dir_all(workspace_eigen.join("Eigen")).unwrap();
        fs::write(workspace_eigen.join("Eigen/Core"), "// stub").unwrap();

        let status2 = detect_ml_dtypes_eigen(workspace.path(), cache.path());
        assert!(matches!(
            status2,
            MlDtypesEigenStatus::WorkspacePresent { .. }
        ));
    }

    /// Pro C3 spec test: when no vendored clone exists but
    /// system Eigen IS installed, the stub records
    /// `SystemFallback` AND surfaces a
    /// `MissingVendoredDependency(ml_dtypes_eigen)` blocker.
    /// System Eigen is not equivalent to TF-pinned
    /// ml_dtypes_eigen.
    #[test]
    fn ml_dtypes_stub_keeps_system_fallback_when_absent() {
        let workspace = tmpdir();
        let cache = tmpdir();

        // No vendored clone in workspace or cache. Whether
        // system Eigen exists depends on the host. The
        // function's behavior is:
        //   - if /usr/include/eigen3/Eigen/Core exists →
        //     SystemFallback
        //   - else → Missing
        // Both surfaces a blocker (per Pro C3 spec).
        let status = detect_ml_dtypes_eigen(workspace.path(), cache.path());

        match status {
            MlDtypesEigenStatus::SystemFallback { include_root } => {
                assert_eq!(
                    include_root,
                    PathBuf::from("/usr/include/eigen3"),
                    "system fallback path is /usr/include/eigen3"
                );
                // Still classified as a blocker — system
                // Eigen is not the TF-pinned version.
                let blocker = MlDtypesEigenStatus::SystemFallback {
                    include_root: PathBuf::from("/usr/include/eigen3"),
                }
                .to_blocker();
                assert!(blocker.is_some(),
                    "SystemFallback must surface MissingVendoredDependency");
                let b = blocker.unwrap();
                match b {
                    crate::candidate_blocker::CandidateBlocker::MissingVendoredDependency {
                        name, evidence
                    } => {
                        assert_eq!(name, "ml_dtypes_eigen");
                        assert!(evidence.contains("TF-pinned ml_dtypes_eigen"));
                    }
                    other => panic!("expected MissingVendoredDependency, got {:?}", other),
                }
            }
            MlDtypesEigenStatus::Missing { evidence } => {
                // Also acceptable on hosts without system Eigen.
                assert!(evidence.contains("Eigen/Core not found"));
            }
            other => panic!(
                "expected SystemFallback or Missing on host without vendored clone, got {:?}",
                other
            ),
        }
    }

    /// Pro C3 spec test: missing vendored eigen blocker is
    /// reported with structured evidence.
    #[test]
    fn missing_vendored_eigen_blocker_reported() {
        let evidence = "test: Eigen/Core not found anywhere".to_string();
        let status = MlDtypesEigenStatus::Missing {
            evidence: evidence.clone(),
        };

        assert!(!status.is_vendored_present());
        let blocker = status.to_blocker().expect("Missing → blocker");
        match blocker {
            crate::candidate_blocker::CandidateBlocker::MissingVendoredDependency {
                name,
                evidence: e,
            } => {
                assert_eq!(name, "ml_dtypes_eigen");
                assert_eq!(e, evidence);
            }
            other => panic!("expected MissingVendoredDependency, got {:?}", other),
        }
    }

    /// ml_dtypes detection: workspace > global > missing.
    #[test]
    fn ml_dtypes_detection_search_order() {
        let workspace = tmpdir();
        let cache = tmpdir();

        // Missing on both.
        let status = detect_ml_dtypes(workspace.path(), cache.path());
        assert!(matches!(status, MlDtypesStatus::Missing { .. }));
        assert!(status.to_blocker().is_some());

        // Add to global cache.
        let global_md = cache.path().join("ml_dtypes");
        fs::create_dir_all(global_md.join("include")).unwrap();
        fs::write(global_md.join("include/float8.h"), "// stub").unwrap();

        let status = detect_ml_dtypes(workspace.path(), cache.path());
        assert!(matches!(status, MlDtypesStatus::GlobalCachePresent { .. }));
        assert!(status.is_present());

        // Workspace wins.
        let workspace_md = workspace.path().join("external/ml_dtypes");
        fs::create_dir_all(workspace_md.join("include")).unwrap();
        fs::write(workspace_md.join("include/float8.h"), "// stub").unwrap();

        let status = detect_ml_dtypes(workspace.path(), cache.path());
        match status {
            MlDtypesStatus::WorkspacePresent { include_root } => {
                assert_eq!(include_root, workspace_md);
            }
            other => panic!("expected WorkspacePresent, got {:?}", other),
        }
    }

    /// Eigen detection: workspace > workspace alias > global
    /// cache > system > missing.
    #[test]
    fn eigen_detection_search_order() {
        let workspace = tmpdir();
        let cache = tmpdir();

        // Lay down legacy ml_dtypes_eigen alias only.
        let legacy = workspace.path().join("external/ml_dtypes_eigen");
        fs::create_dir_all(legacy.join("Eigen")).unwrap();
        fs::write(legacy.join("Eigen/Core"), "// stub").unwrap();

        let status = detect_ml_dtypes_eigen(workspace.path(), cache.path());
        match status {
            MlDtypesEigenStatus::WorkspacePresent { include_root } => {
                assert_eq!(include_root, legacy);
            }
            other => panic!("expected WorkspacePresent via legacy, got {:?}", other),
        }

        // Modern eigen_archive supersedes the legacy alias.
        let modern = workspace.path().join("external/eigen_archive");
        fs::create_dir_all(modern.join("Eigen")).unwrap();
        fs::write(modern.join("Eigen/Core"), "// stub").unwrap();

        let status = detect_ml_dtypes_eigen(workspace.path(), cache.path());
        match status {
            MlDtypesEigenStatus::WorkspacePresent { include_root } => {
                assert_eq!(include_root, modern);
            }
            other => panic!("expected WorkspacePresent via modern, got {:?}", other),
        }
    }
}
