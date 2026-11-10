// Rush Build - Inactive compiler discovery request plumbing
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Inactive, per-command compile policy and pending declaration validation.
//!
//! Nothing calls this decoder from the orchestrator yet. No existing fatal
//! proof, execution, cache, snapshot or publication path changes. A validated
//! declaration/order is not proof of completed execution or filesystem reads.

use anyhow::{anyhow, ensure, Result};
use rush_action::action_dag::ActionDag;
use rush_action::ids::{ArtifactRegistry, DeclaredActionId};
use rush_command_core::CommandSemantics;
use rush_rules::context::{NativeCompileMode, PendingCompileDeclaration};
use std::collections::HashSet;

pub(crate) const COMPILE_DISCOVERY_ENV: &[u8] = b"RUSH_CPP_DISCOVERY";

/// Decode only this immutable client's bytes. Absence resets to Literal;
/// unknown, empty or non-UTF8 values refuse instead of inheriting daemon state.
/// Calling this does not enable any execution or change a cache policy.
pub(crate) fn requested_compile_mode(command: &CommandSemantics) -> Result<NativeCompileMode> {
    match command.getenv(COMPILE_DISCOVERY_ENV) {
        None | Some(b"literal") => Ok(NativeCompileMode::Literal),
        Some(b"uncached-diagnostic-v1") => Ok(NativeCompileMode::UncachedDiagnosticV1),
        Some(_) => Err(anyhow!(
            "RUSH_CPP_DISCOVERY requires literal or uncached-diagnostic-v1"
        )),
    }
}

/// Check a pending record against its build registry and a caller-reported
/// producer sequence. This only checks structural ordering: the later manager
/// must independently own execution/completion and post-producer file reads.
/// It returns no ready request, certificate, cache key or execution capability.
pub(crate) fn validate_reported_producer_order(
    registry: &ArtifactRegistry,
    pending: &PendingCompileDeclaration,
    completed: &[DeclaredActionId],
) -> Result<()> {
    let current = PendingCompileDeclaration::from_registry(
        registry,
        pending.action(),
        pending.target(),
        pending.config(),
        pending.source(),
        pending.object(),
    )?;
    ensure!(
        &current == pending,
        "pending compile declaration identity changed"
    );
    // The DAG includes incomplete actions, so it detects structural cycles
    // without pretending this pending compile is schedulable.
    ActionDag::build(registry)?;
    let mut seen = HashSet::new();
    for &id in completed {
        ensure!(
            (id.0 as usize) < registry.declared_action_count(),
            "reported producer is not in this registry"
        );
        ensure!(
            id != pending.action() && !seen.contains(&id),
            "reported producer sequence repeats or includes the pending compile"
        );
        let action = registry.declared_action(id);
        for &input in action.inputs.iter() {
            let data = registry.artifact(input);
            if !data.is_source {
                let producer = data.owner_action_declared.ok_or_else(|| {
                    anyhow!("reported producer consumes an input without a declared producer")
                })?;
                ensure!(
                    seen.contains(&producer),
                    "reported producer runs before its declared input producer"
                );
            }
        }
        seen.insert(id);
    }
    ensure!(
        pending
            .required_producers()
            .iter()
            .all(|id| seen.contains(id)),
        "pending compile still has unfinished declared producers"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rush_action::ids::{
        ActionDeclarationStatus, ArtifactId, ArtifactKey, ArtifactKind, ConfigId, DeclaredAction,
        TargetId,
    };
    use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
    use rush_rules::analysis_context::{AnalysisContext, StubIncludeResolver};
    use rush_rules::context::{BuildContext, PendingCompileDeclarations, Toolchain};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn request(value: Option<&[u8]>) -> CommandSemantics {
        CommandSemantics::from_run_request(&RunRequest {
            argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"build")],
            environment: value
                .map(|value| {
                    vec![EnvironmentVariable {
                        name: Bytes::from_static(COMPILE_DISCOVERY_ENV),
                        value: Bytes::copy_from_slice(value),
                    }]
                })
                .unwrap_or_default(),
            protocol_major: rush_protocol::PROTOCOL_MAJOR,
            protocol_minor: rush_protocol::PROTOCOL_MINOR,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn compiler_discovery_requests_isolate_mode_and_reset_without_daemon_environment() {
        let diagnostic = request(Some(b"uncached-diagnostic-v1"));
        let reset = request(None);
        let literal = request(Some(b"literal"));
        assert_eq!(
            requested_compile_mode(&diagnostic).unwrap(),
            NativeCompileMode::UncachedDiagnosticV1
        );
        assert_eq!(
            requested_compile_mode(&reset).unwrap(),
            NativeCompileMode::Literal
        );
        assert_eq!(
            requested_compile_mode(&literal).unwrap(),
            NativeCompileMode::Literal
        );
        assert_ne!(diagnostic.config_fingerprint(), reset.config_fingerprint());
        for value in [
            b"".as_slice(),
            b"diagnostic",
            b"UNCACHED-DIAGNOSTIC-V1",
            b"literal ",
            b"\xff",
        ] {
            assert!(requested_compile_mode(&request(Some(value))).is_err());
        }
        // Revisit the first request after others in the same process: no
        // process-global selection or sticky per-daemon mode exists.
        assert_eq!(
            requested_compile_mode(&diagnostic).unwrap(),
            NativeCompileMode::UncachedDiagnosticV1
        );
    }

    fn artifact(
        reg: &mut ArtifactRegistry,
        path: &str,
        source: bool,
        kind: ArtifactKind,
        target: TargetId,
        config: ConfigId,
    ) -> ArtifactId {
        let exec_path = reg.intern_path(Path::new(path));
        reg.intern_artifact(
            ArtifactKey { config, exec_path },
            kind,
            source,
            target,
            None,
        )
    }

    fn declare(
        reg: &mut ArtifactRegistry,
        owner: TargetId,
        mnemonic: &str,
        inputs: Vec<ArtifactId>,
        outputs: Vec<ArtifactId>,
        incomplete: bool,
    ) -> DeclaredActionId {
        let mnemonic = reg.intern_string(mnemonic);
        reg.declare_action(DeclaredAction {
            owner_target: owner,
            mnemonic,
            inputs: inputs.into_boxed_slice(),
            outputs: outputs.into_boxed_slice(),
            status: if incomplete {
                ActionDeclarationStatus::Incomplete {
                    reason: "pending compiler input discovery",
                }
            } else {
                ActionDeclarationStatus::Complete
            },
        })
        .unwrap()
    }

    struct Fixture {
        registry: ArtifactRegistry,
        source: ArtifactId,
        object: ArtifactId,
        compile: DeclaredActionId,
        source_producer: DeclaredActionId,
        header_producer: DeclaredActionId,
    }

    fn fixture(prefix: &str) -> Fixture {
        let mut registry = ArtifactRegistry::new();
        let config = ConfigId(7);
        let source = artifact(
            &mut registry,
            &format!("{prefix}/generated.cc"),
            false,
            ArtifactKind::Generated,
            TargetId(1),
            config,
        );
        let header = artifact(
            &mut registry,
            &format!("{prefix}/generated.h"),
            false,
            ArtifactKind::Header,
            TargetId(2),
            config,
        );
        let object = artifact(
            &mut registry,
            &format!("{prefix}/object.o"),
            false,
            ArtifactKind::Object,
            TargetId(3),
            config,
        );
        let source_producer = declare(
            &mut registry,
            TargetId(1),
            "Genrule",
            vec![],
            vec![source],
            false,
        );
        let header_producer = declare(
            &mut registry,
            TargetId(2),
            "Genrule",
            vec![source],
            vec![header],
            false,
        );
        let compile = declare(
            &mut registry,
            TargetId(3),
            "CppCompile",
            vec![source, header],
            vec![object],
            true,
        );
        Fixture {
            registry,
            source,
            object,
            compile,
            source_producer,
            header_producer,
        }
    }

    fn pending(f: &Fixture) -> PendingCompileDeclaration {
        PendingCompileDeclaration::from_registry(
            &f.registry,
            f.compile,
            TargetId(3),
            ConfigId(7),
            f.source,
            f.object,
        )
        .unwrap()
    }

    #[test]
    fn compiler_discovery_pending_identity_and_actual_producer_edges_remain_structural() {
        let f = fixture("/build-a");
        let p = pending(&f);
        assert_eq!(
            p.required_producers(),
            &[f.source_producer, f.header_producer]
        );
        validate_reported_producer_order(&f.registry, &p, &[f.source_producer, f.header_producer])
            .unwrap();
        for order in [
            vec![],
            vec![f.source_producer],
            vec![f.header_producer, f.source_producer],
            vec![f.source_producer, f.source_producer, f.header_producer],
            vec![f.source_producer, f.header_producer, f.compile],
        ] {
            assert!(validate_reported_producer_order(&f.registry, &p, &order).is_err());
        }
        assert!(PendingCompileDeclaration::from_registry(
            &f.registry,
            f.compile,
            TargetId(99),
            ConfigId(7),
            f.source,
            f.object
        )
        .is_err());
        assert!(PendingCompileDeclaration::from_registry(
            &f.registry,
            f.compile,
            TargetId(3),
            ConfigId(8),
            f.source,
            f.object
        )
        .is_err());
        let other = fixture("/build-b");
        assert!(validate_reported_producer_order(
            &other.registry,
            &p,
            &[other.source_producer, other.header_producer]
        )
        .is_err());
        let alias = fixture("/build-a/.");
        assert!(
            validate_reported_producer_order(
                &alias.registry,
                &p,
                &[alias.source_producer, alias.header_producer]
            )
            .is_err(),
            "pending declaration identity retains exact input/output spellings"
        );
        let mut records = PendingCompileDeclarations::default();
        records.record(p.clone()).unwrap();
        assert!(records.record(pending(&other)).is_err());
        assert_eq!(
            records.get(f.compile),
            Some(&p),
            "conflict must not replace the old request"
        );
        assert!(matches!(
            f.registry.declared_action(f.compile).status,
            ActionDeclarationStatus::Incomplete { .. }
        ));
    }

    #[test]
    fn compiler_discovery_context_transport_does_not_clear_fatal_namespace_errors() {
        let mut f = fixture("/build");
        let mut declarations = PendingCompileDeclarations::default();
        let root = Path::new("/build");
        let resolver = StubIncludeResolver::new(root.to_path_buf());
        let toolchain = Toolchain::from_environment_lookup(|_| None);
        {
            let mut analysis = AnalysisContext::new(
                TargetId(3),
                ConfigId(7),
                "",
                None,
                root,
                &mut f.registry,
                &resolver,
                &toolchain,
                root,
                &[],
            );
            assert_eq!(analysis.native_compile_mode(), NativeCompileMode::Literal);
            assert!(analysis
                .record_pending_compile(f.compile, f.source, f.object)
                .is_err());
            let mut analysis = analysis.with_native_compile_discovery(
                NativeCompileMode::UncachedDiagnosticV1,
                &mut declarations,
            );
            analysis.fail_native_namespace_analysis("literal proof failed");
            analysis
                .record_pending_compile(f.compile, f.source, f.object)
                .unwrap();
            assert_eq!(
                analysis.native_namespace_analysis_error(),
                Some("literal proof failed")
            );
        }
        let record = declarations.get(f.compile).unwrap();
        assert_eq!(record.source_path(), Path::new("/build/generated.cc"));
        let build = BuildContext::new(PathBuf::from("/build"), PathBuf::from("/out"));
        assert_eq!(build.native_compile_mode(), NativeCompileMode::Literal);
        assert!(build.pending_compile_declarations().is_none());
        let build = build.with_native_compile_discovery(
            NativeCompileMode::UncachedDiagnosticV1,
            Arc::new(declarations),
        );
        assert_eq!(
            build
                .pending_compile_declarations()
                .unwrap()
                .get(f.compile)
                .unwrap()
                .object_path(),
            Path::new("/build/object.o")
        );
    }

    #[test]
    fn compiler_discovery_pending_missing_producer_and_cycles_refuse() {
        let mut registry = ArtifactRegistry::new();
        let config = ConfigId(2);
        let source = artifact(
            &mut registry,
            "/missing.cc",
            false,
            ArtifactKind::Generated,
            TargetId(1),
            config,
        );
        let object = artifact(
            &mut registry,
            "/object.o",
            false,
            ArtifactKind::Object,
            TargetId(2),
            config,
        );
        let compile = declare(
            &mut registry,
            TargetId(2),
            "CppCompile",
            vec![source],
            vec![object],
            true,
        );
        assert!(PendingCompileDeclaration::from_registry(
            &registry,
            compile,
            TargetId(2),
            config,
            source,
            object
        )
        .is_err());
        // Completing the declarations can expose a real cycle, not readiness.
        let producer = declare(
            &mut registry,
            TargetId(1),
            "Genrule",
            vec![object],
            vec![source],
            false,
        );
        let pending = PendingCompileDeclaration::from_registry(
            &registry,
            compile,
            TargetId(2),
            config,
            source,
            object,
        )
        .unwrap();
        assert!(validate_reported_producer_order(&registry, &pending, &[producer]).is_err());
    }
}
