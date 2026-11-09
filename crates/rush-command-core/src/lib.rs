//! `rush-command-core` — the transport-independent service layer for the rush
//! persistent daemon.
//!
//! This crate sits between [`rush_protocol`] (wire messages only) and the future
//! transport adapters (`rush-transport-grpc`, `rush-transport-framed`). It defines:
//!
//! * [`CommandService`] — the single trait both transports call (`run`, `cancel`,
//!   `ping`, `shutdown`). No tonic types, no sockets: the build logic never sees
//!   the transport.
//! * [`CommandSemantics`] — the per-command context (env snapshot, argv/flags,
//!   `--define`s, working dir, output base, and a deterministic config
//!   fingerprint) that **replaces process-global `std::env`/`config_defines`** so
//!   one client's build can never contaminate another's. (Migrating the live read
//!   sites to use it is a later brick — see the type's docs.)
//! * [`CoreService`] — a real impl with honest `ping`/`shutdown` and an honestly
//!   *unwired* `run` (no fake build results).
//!
//! See `docs/design/persistent-daemon-skyframe.md` for the surrounding design.
//!
//! **Scope guard:** this crate is additive scaffolding. It is NOT wired into the
//! CLI and changes no existing build behavior.

mod core;
mod error;
mod semantics;
mod service;

pub use crate::core::CoreService;
pub use crate::error::CommandError;
pub use crate::semantics::{
    populate_workspace_config, BuildDefine, CommandSemantics, RunRequestExt,
    CONFIG_FINGERPRINT_EXCLUDED_ENV,
};
pub use crate::service::{CommandService, RunEventStream};

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::StreamExt;
    use rush_protocol::command::v1::run_event::Event;
    use rush_protocol::command::v1::{
        EnvironmentVariable, PingRequest, RunRequest, ShutdownRequest,
    };
    use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};

    fn env_var(name: &[u8], value: &[u8]) -> EnvironmentVariable {
        EnvironmentVariable {
            name: Bytes::from(name.to_vec()),
            value: Bytes::from(value.to_vec()),
        }
    }

    fn base_request() -> RunRequest {
        RunRequest {
            auth_cookie: Bytes::new(),
            argv: vec![
                Bytes::from_static(b"rush"),
                Bytes::from_static(b"build"),
                Bytes::from_static(b"//foo:bar"),
                Bytes::from_static(b"--define=GPU=1"),
            ],
            working_directory: Bytes::from_static(b"/home/user/proj"),
            environment: vec![
                env_var(b"PATH", b"/usr/bin:/bin"),
                env_var(b"CC", b"clang"),
            ],
            block_for_lock: true,
            client_build_id: "build-1".to_string(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            workspace_config: Vec::new(),
        }
    }

    #[test]
    fn semantics_round_trips_env_and_flags() {
        let req = base_request();
        let sem = CommandSemantics::from_run_request(&req).expect("build semantics");

        // getenv returns the exact bytes the client sent.
        assert_eq!(sem.getenv(b"PATH"), Some(&b"/usr/bin:/bin"[..]));
        assert_eq!(sem.getenv(b"CC"), Some(&b"clang"[..]));
        // Unset vars are None — and crucially we never fall back to std::env.
        assert_eq!(sem.getenv(b"HOME"), None);
        assert_eq!(sem.getenv(b"NONEXISTENT_VAR_12345"), None);

        // argv / command / working dir round-trip.
        assert_eq!(sem.argv().len(), 4);
        assert_eq!(sem.command(), Some("build"));
        assert_eq!(sem.working_directory(), b"/home/user/proj");

        // The --define parsed out correctly.
        assert_eq!(sem.defines().len(), 1);
        assert_eq!(sem.defines()[0].key, "GPU");
        assert_eq!(sem.defines()[0].value, "1");

        // Flags captured (both --define=... is a flag here).
        assert!(sem
            .flags()
            .iter()
            .any(|f| f.as_ref() == b"--define=GPU=1"));
    }

    #[test]
    fn space_separated_define_form_parses() {
        let mut req = base_request();
        req.argv = vec![
            Bytes::from_static(b"rush"),
            Bytes::from_static(b"build"),
            Bytes::from_static(b"--define"),
            Bytes::from_static(b"USE_MOLD=true"),
        ];
        let sem = CommandSemantics::from_run_request(&req).expect("build semantics");
        assert_eq!(sem.defines().len(), 1);
        assert_eq!(sem.defines()[0].key, "USE_MOLD");
        assert_eq!(sem.defines()[0].value, "true");
    }

    #[test]
    fn identical_requests_have_identical_fingerprints() {
        let a = CommandSemantics::from_run_request(&base_request()).unwrap();
        let b = CommandSemantics::from_run_request(&base_request()).unwrap();
        assert_eq!(a.config_fingerprint(), b.config_fingerprint());
    }

    #[test]
    fn client_build_id_does_not_affect_fingerprint() {
        // client_build_id is analysis-irrelevant: changing it must NOT perturb the
        // config fingerprint (otherwise cache reuse would never hit).
        let mut req2 = base_request();
        req2.client_build_id = "a-totally-different-id".to_string();
        let a = CommandSemantics::from_run_request(&base_request()).unwrap();
        let b = CommandSemantics::from_run_request(&req2).unwrap();
        assert_eq!(a.config_fingerprint(), b.config_fingerprint());
    }

    #[test]
    fn different_env_produces_different_fingerprint() {
        let a = CommandSemantics::from_run_request(&base_request()).unwrap();

        let mut req2 = base_request();
        req2.environment = vec![
            env_var(b"PATH", b"/usr/bin:/bin"),
            env_var(b"CC", b"gcc"), // changed clang -> gcc
        ];
        let b = CommandSemantics::from_run_request(&req2).unwrap();

        assert_ne!(
            a.config_fingerprint(),
            b.config_fingerprint(),
            "env change must change the fingerprint (proves no global leakage)"
        );
    }

    #[test]
    fn diagnostic_env_does_not_affect_fingerprint() {
        let a = CommandSemantics::from_run_request(&base_request()).unwrap();

        let mut req2 = base_request();
        req2.environment.push(env_var(
            b"RUSH_ACTION_TIMING",
            b"/tmp/rushbench/edit_src_0.jsonl",
        ));
        req2.environment
            .push(env_var(b"RUSH_PROFILE_PHASES", b"/tmp/rushbench/phases"));
        req2.environment.push(env_var(b"RUSH_REUSE_TIMING", b"1"));
        let b = CommandSemantics::from_run_request(&req2).unwrap();

        assert_eq!(
            a.config_fingerprint(),
            b.config_fingerprint(),
            "diagnostic-only env must not fork daemon analysis reuse"
        );
    }

    #[test]
    fn different_defines_produce_different_fingerprint() {
        let a = CommandSemantics::from_run_request(&base_request()).unwrap();

        let mut req2 = base_request();
        req2.argv = vec![
            Bytes::from_static(b"rush"),
            Bytes::from_static(b"build"),
            Bytes::from_static(b"//foo:bar"),
            Bytes::from_static(b"--define=GPU=0"), // 1 -> 0
        ];
        let b = CommandSemantics::from_run_request(&req2).unwrap();

        assert_ne!(a.config_fingerprint(), b.config_fingerprint());
    }

    #[test]
    fn length_prefix_prevents_env_collision() {
        // { "AB" -> "" } must NOT hash the same as { "A" -> "B" }.
        let mut r1 = base_request();
        r1.environment = vec![env_var(b"AB", b"")];
        let mut r2 = base_request();
        r2.environment = vec![env_var(b"A", b"B")];

        let a = CommandSemantics::from_run_request(&r1).unwrap();
        let b = CommandSemantics::from_run_request(&r2).unwrap();
        assert_ne!(a.config_fingerprint(), b.config_fingerprint());
    }

    #[test]
    fn empty_argv_is_rejected() {
        let mut req = base_request();
        req.argv = vec![];
        let err = CommandSemantics::from_run_request(&req).unwrap_err();
        assert!(matches!(err, CommandError::InvalidRequest(_)));
    }

    #[test]
    fn protocol_major_mismatch_is_rejected() {
        let mut req = base_request();
        req.protocol_major = PROTOCOL_MAJOR + 1;
        let err = CommandSemantics::from_run_request(&req).unwrap_err();
        assert!(matches!(err, CommandError::ProtocolMismatch { .. }));
    }

    #[tokio::test]
    async fn ping_reports_uptime_and_build_counter() {
        let svc = CoreService::new();

        let p1 = svc.ping(PingRequest {}).await;
        // builds_executed starts at zero (no builds run).
        assert_eq!(p1.builds_executed, 0);

        // Uptime is monotonic across two pings.
        let p2 = svc.ping(PingRequest {}).await;
        assert!(p2.uptime_secs >= p1.uptime_secs);

        // The accessor mirrors what ping reports.
        assert_eq!(svc.builds_executed(), p2.builds_executed);
    }

    #[tokio::test]
    async fn run_is_honestly_unwired() {
        let svc = CoreService::new();
        let mut s = svc.run(base_request()).await;

        // First event: an ERROR diagnostic stating run is unwired.
        let first = s.next().await.expect("a first event").expect("ok event");
        match first.event {
            Some(Event::Diagnostic(d)) => {
                assert_eq!(d.message.as_ref(), b"run not yet wired to the build orchestrator");
            }
            other => panic!("expected diagnostic, got {other:?}"),
        }

        // Then: a CommandFinished with EX_SOFTWARE (70), NOT a fake success (0).
        let second = s.next().await.expect("a finished event").expect("ok event");
        match second.event {
            Some(Event::Finished(f)) => assert_eq!(f.exit_code, 70),
            other => panic!("expected finished, got {other:?}"),
        }

        assert!(s.next().await.is_none(), "stream should end after finished");

        // No build ran → counter stays at zero (honesty check).
        assert_eq!(svc.builds_executed(), 0);
    }

    #[tokio::test]
    async fn shutdown_acks() {
        let svc = CoreService::new();
        let resp = svc.shutdown(ShutdownRequest { auth_cookie: Bytes::new() }).await;
        assert!(resp.ack);
    }
}
