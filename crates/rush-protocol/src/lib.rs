//! `rush-protocol` — the shared wire schema for the rush persistent daemon.
//!
//! This crate contains ONLY the `rush.command.v1` protobuf message types (generated
//! by `prost-build` at compile time) plus the declared protocol version. It is
//! deliberately transport-independent: it does NOT depend on `tonic`. The tonic
//! service traits (the gRPC server/client stubs) are generated in a separate
//! `rush-transport-grpc` crate from the same `.proto`, so the build logic and the
//! framed-protobuf challenger transport can share these message types without
//! pulling in a gRPC dependency.
//!
//! See `docs/design/persistent-daemon-skyframe.md` ("IPC transport — gRPC/protobuf
//! over UDS") for the architecture this crate is the first brick of.

/// Generated `rush.command.v1` message types.
pub mod command {
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/rush.command.v1.rs"));
    }
}

/// Major protocol version. Bumped on incompatible wire changes.
pub const PROTOCOL_MAJOR: u32 = 1;

/// Minor protocol generation. Clients require an exact major/minor match and
/// discover daemons in a generation-specific directory.
pub const PROTOCOL_MINOR: u32 = 1;

#[cfg(test)]
mod tests {
    use super::command::v1::{
        run_event::Event, CommandFinished, EnvironmentVariable, RunEvent, RunRequest,
        WorkspaceConfigFile,
    };
    use super::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use prost::Message;

    #[test]
    fn run_request_round_trips() {
        let req = RunRequest {
            auth_cookie: b"\x00\x01\x02secret".to_vec().into(),
            argv: vec![
                b"rush".to_vec().into(),
                b"build".to_vec().into(),
                // Non-UTF-8 argument: proves `bytes` (not `string`) handling.
                vec![0xff, 0xfe, 0x2f].into(),
            ],
            working_directory: b"/home/user/proj".to_vec().into(),
            environment: vec![EnvironmentVariable {
                name: b"PATH".to_vec().into(),
                value: b"/usr/bin:/bin".to_vec().into(),
            }],
            block_for_lock: true,
            client_build_id: "build-42".to_string(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            workspace_config: vec![WorkspaceConfigFile {
                name: b".bazelrc".to_vec().into(),
                // Non-UTF-8 content: proves `bytes` (not `string`) handling.
                content: vec![b'b', b'u', b'i', b'l', b'd', b' ', 0xff].into(),
            }],
        };

        let mut buf = Vec::new();
        req.encode(&mut buf).expect("encode RunRequest");
        let decoded = RunRequest::decode(&buf[..]).expect("decode RunRequest");

        assert_eq!(req, decoded);
        assert_eq!(decoded.argv.len(), 3);
        assert_eq!(&decoded.argv[2][..], &[0xff, 0xfe, 0x2f]);
        assert_eq!(decoded.protocol_major, PROTOCOL_MAJOR);
        assert_eq!(decoded.protocol_minor, PROTOCOL_MINOR);
        assert_eq!(decoded.workspace_config.len(), 1);
        assert_eq!(&decoded.workspace_config[0].name[..], b".bazelrc");
        assert_eq!(&decoded.workspace_config[0].content[..], &[b'b', b'u', b'i', b'l', b'd', b' ', 0xff]);
    }

    #[test]
    fn run_event_finished_round_trips() {
        let event = RunEvent {
            event: Some(Event::Finished(CommandFinished {
                exit_code: 7,
                command_id: b"cmd-abc".to_vec().into(),
            })),
        };

        let mut buf = Vec::new();
        event.encode(&mut buf).expect("encode RunEvent");
        let decoded = RunEvent::decode(&buf[..]).expect("decode RunEvent");

        assert_eq!(event, decoded);
        match decoded.event {
            Some(Event::Finished(f)) => {
                assert_eq!(f.exit_code, 7);
                assert_eq!(&f.command_id[..], b"cmd-abc");
            }
            other => panic!("expected Finished variant, got {other:?}"),
        }
    }
}
