//! End-to-end integration tests for the gRPC-over-Unix-domain-socket transport.
//!
//! These spawn a real tonic server on a temp-dir socket, connect a real client,
//! and exercise Ping (priority 1), the honestly-unwired Run stream (priority 1),
//! cookie auth (priority 2), and metadata-file discovery + lifecycle (priority 3).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use tempfile::tempdir;

use rush_command_core::CoreService;
use rush_protocol::command::v1::run_event::Event;
use rush_protocol::command::v1::{CancelRequest, RunRequest};
use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};

use rush_transport_grpc::{
    serve_unix, AuthCookie, DaemonMetadata, GrpcAdapter, GrpcClient, LocalEndpoint,
    TransportError,
};

fn base_run_request() -> RunRequest {
    RunRequest {
        auth_cookie: Bytes::new(),
        argv: vec![
            Bytes::from_static(b"rush"),
            Bytes::from_static(b"build"),
            Bytes::from_static(b"//foo:bar"),
        ],
        working_directory: Bytes::from_static(b"/tmp"),
        environment: vec![],
        block_for_lock: true,
        client_build_id: "it-1".to_string(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        workspace_config: Vec::new(),
    }
}

/// Spawn a daemon on a socket under `dir`, returning (handle, cookie, sock_path).
async fn spawn_daemon(
    dir: &std::path::Path,
) -> (
    rush_transport_grpc::DaemonHandle,
    AuthCookie,
    std::path::PathBuf,
) {
    let sock = dir.join("rush.sock");
    let cookie = AuthCookie::generate();
    let svc = Arc::new(CoreService::new());
    let adapter = GrpcAdapter::new(svc, cookie.clone());
    let handle = serve_unix(adapter, &sock).await.expect("serve_unix");
    // Give the listener a beat to be accepting.
    wait_for_socket(&sock).await;
    (handle, cookie, sock)
}

async fn wait_for_socket(path: &std::path::Path) {
    for _ in 0..100 {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---- Priority 1: working Ping + honest Run over UDS ----

#[tokio::test]
async fn ping_over_uds_returns_real_uptime() {
    let dir = tempdir().unwrap();
    let (handle, cookie, sock) = spawn_daemon(dir.path()).await;

    let mut client = GrpcClient::connect_unix(&sock, cookie).await.expect("connect");

    let p1 = client.ping().await.expect("ping 1");
    assert_eq!(p1.builds_executed, 0, "no builds ran");

    // Sleep so uptime can advance, then ping again: uptime must be monotonic.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let p2 = client.ping().await.expect("ping 2");
    assert!(
        p2.uptime_secs >= p1.uptime_secs,
        "uptime monotonic: {} >= {}",
        p2.uptime_secs,
        p1.uptime_secs
    );

    handle.abort();
}

#[tokio::test]
async fn run_streams_honest_unwired_result_over_uds() {
    let dir = tempdir().unwrap();
    let (handle, cookie, sock) = spawn_daemon(dir.path()).await;

    let mut client = GrpcClient::connect_unix(&sock, cookie).await.expect("connect");

    let stream = client.run(base_run_request()).await.expect("run call");
    futures::pin_mut!(stream);

    // First event: an ERROR diagnostic, NOT a fake success.
    let first = stream.next().await.expect("a first event").expect("ok");
    match first.event {
        Some(Event::Diagnostic(d)) => {
            assert_eq!(
                d.message.as_ref(),
                b"run not yet wired to the build orchestrator"
            );
        }
        other => panic!("expected diagnostic, got {other:?}"),
    }

    // Then CommandFinished with EX_SOFTWARE (70).
    let second = stream.next().await.expect("a finished event").expect("ok");
    match second.event {
        Some(Event::Finished(f)) => assert_eq!(f.exit_code, 70),
        other => panic!("expected finished, got {other:?}"),
    }

    assert!(stream.next().await.is_none(), "stream ends after finished");

    handle.abort();
}

// ---- Priority 2: auth cookie ----

#[tokio::test]
async fn wrong_cookie_is_rejected_correct_passes() {
    let dir = tempdir().unwrap();
    let (handle, cookie, sock) = spawn_daemon(dir.path()).await;

    // Wrong cookie: Run must be rejected with unauthenticated.
    let wrong = AuthCookie::from_bytes(vec![0u8; 32]);
    let mut bad_client = GrpcClient::connect_unix(&sock, wrong).await.expect("connect");
    let err = bad_client
        .cancel(CancelRequest {
            command_id: Bytes::new(),
            auth_cookie: Bytes::new(),
        })
        .await
        .expect_err("wrong cookie must be rejected");
    match err {
        TransportError::Status(s) => {
            assert_eq!(s.code(), tonic::Code::Unauthenticated, "got {s:?}");
        }
        other => panic!("expected unauthenticated status, got {other:?}"),
    }

    // Correct cookie: cancel succeeds (CoreService returns cancelled=false but a
    // valid response — the point is the auth gate let it through).
    let mut good_client = GrpcClient::connect_unix(&sock, cookie).await.expect("connect");
    let resp = good_client
        .cancel(CancelRequest {
            command_id: Bytes::from_static(b"nope"),
            auth_cookie: Bytes::new(),
        })
        .await
        .expect("correct cookie must pass");
    // No such command in flight → not cancelled, but the call itself succeeded.
    assert!(!resp.cancelled);

    handle.abort();
}

#[tokio::test]
async fn cookie_round_trips_through_disk_with_restrictive_perms() {
    let dir = tempdir().unwrap();
    let private = dir.path().join("private");
    let cookie_path = private.join("cookie");

    let cookie = AuthCookie::generate();
    cookie.write_to(&cookie_path).expect("write cookie");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let fmode = std::fs::metadata(&cookie_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600, "cookie file must be 0600");
        let dmode = std::fs::metadata(&private).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700, "private dir must be 0700");
    }

    let read_back = AuthCookie::read_from(&cookie_path).expect("read cookie");
    assert!(cookie.verify(read_back.as_bytes()), "round-trips identically");
    assert!(!cookie.verify(&[0u8; 32]), "rejects a wrong cookie");
}

// ---- Priority 3: metadata-file discovery + lifecycle ----

#[tokio::test]
async fn metadata_discovery_connects_and_pings() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("rush.sock");
    let cookie_path = dir.path().join("auth").join("cookie");
    let meta_path = dir.path().join("daemon.json");

    // Daemon side: generate cookie, persist it, write metadata atomically.
    let cookie = AuthCookie::generate();
    cookie.write_to(&cookie_path).expect("write cookie");

    let svc = Arc::new(CoreService::new());
    let adapter = GrpcAdapter::new(svc, cookie.clone());
    let handle = serve_unix(adapter, &sock).await.expect("serve_unix");
    wait_for_socket(&sock).await;

    let endpoint = LocalEndpoint::UnixSocket(sock.clone());
    let meta = DaemonMetadata::new(&endpoint, &cookie_path, PROTOCOL_MAJOR, PROTOCOL_MINOR);
    meta.write_atomic(&meta_path).expect("write metadata");

    // Client side: discover purely from the metadata file (no hardcoded path).
    let mut client = GrpcClient::discover(&meta_path).await.expect("discover");
    let p = client.ping().await.expect("ping via discovered daemon");
    assert_eq!(p.builds_executed, 0);

    // Sanity: the metadata reflects the unix_socket transport + protocol.
    let reread = DaemonMetadata::read(&meta_path).unwrap();
    assert_eq!(reread.transport, "unix_socket");
    assert_eq!(reread.protocol_version, format!("{PROTOCOL_MAJOR}.{PROTOCOL_MINOR}"));
    assert_eq!(reread.endpoint, sock.display().to_string());

    handle.abort();
}

#[tokio::test]
async fn stale_socket_is_reclaimed() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("rush.sock");
    let cookie = AuthCookie::generate();

    // Create a stale socket file that nobody is listening on: bind+drop.
    {
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind stale");
        drop(listener);
    }
    assert!(sock.exists(), "stale socket file present");

    // serve_unix must detect the dead socket, remove it, and bind fresh.
    let svc = Arc::new(CoreService::new());
    let adapter = GrpcAdapter::new(svc, cookie.clone());
    let handle = serve_unix(adapter, &sock).await.expect("reclaim stale socket");
    wait_for_socket(&sock).await;

    let mut client = GrpcClient::connect_unix(&sock, cookie).await.expect("connect");
    client.ping().await.expect("ping after reclaim");

    handle.abort();
}

#[tokio::test]
async fn live_socket_owner_is_not_clobbered() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("rush.sock");
    let cookie = AuthCookie::generate();

    let svc = Arc::new(CoreService::new());
    let adapter = GrpcAdapter::new(svc, cookie.clone());
    let handle = serve_unix(adapter, &sock).await.expect("first daemon");
    wait_for_socket(&sock).await;

    // A second serve_unix on the same live socket must refuse, not clobber.
    let svc2 = Arc::new(CoreService::new());
    let adapter2 = GrpcAdapter::new(svc2, cookie.clone());
    let err = serve_unix(adapter2, &sock).await.expect_err("must refuse live owner");
    match err {
        TransportError::Discovery(msg) => assert!(msg.contains("live daemon")),
        other => panic!("expected Discovery error, got {other:?}"),
    }

    handle.abort();
}

#[test]
fn oversized_socket_path_is_rejected_clearly() {
    // A path well past the ~108-byte sun_path limit must error with a clear
    // message rather than a cryptic bind EINVAL.
    let long = std::path::PathBuf::from(format!("/tmp/{}/rush.sock", "x".repeat(120)));
    let err = LocalEndpoint::validate_unix_path(&long).expect_err("too long");
    match err {
        TransportError::SocketPathTooLong { len, limit, .. } => {
            assert!(len > limit, "{len} > {limit}");
        }
        other => panic!("expected SocketPathTooLong, got {other:?}"),
    }
}
