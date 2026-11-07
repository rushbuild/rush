//! Build script for `rush-transport-grpc`.
//!
//! Generates the tonic gRPC service stubs (client + server) from the SAME
//! `rush_command_v1.proto` that `rush-protocol` compiles for its message types.
//!
//! The message types are NOT regenerated here: we map the `rush.command.v1`
//! protobuf package onto the already-generated `rush_protocol::command::v1`
//! module via `extern_path`. tonic-build then emits ONLY the service traits and
//! the client/server stubs, which reference `rush_protocol`'s message structs
//! directly. This keeps a single source of truth for the wire types and keeps
//! `rush-protocol` free of any tonic dependency.
//!
//! Hermeticity mirrors `rush-protocol/build.rs`: if `PROTOC` is unset and no
//! `protoc` is on `PATH`, fall back to the vendored binary.

use std::env;
use std::path::Path;

fn main() {
    // The proto lives in the rush-protocol crate (single source of truth).
    let proto_file = "../rush-protocol/proto/rush_command_v1.proto";
    let proto_dir = "../rush-protocol/proto";

    println!("cargo:rerun-if-changed={proto_file}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PROTOC");

    ensure_protoc();

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        // Reuse rush-protocol's generated messages instead of regenerating them.
        // Every message in package `rush.command.v1` is found under this module.
        .extern_path(".rush.command.v1", "::rush_protocol::command::v1")
        // Match rush-protocol's `bytes`-typed fields so the service signatures
        // accept the exact same message structs.
        .bytes(["."])
        .compile_protos(&[proto_file], &[proto_dir])
        .expect("rush-transport-grpc: failed to compile rush_command_v1.proto with tonic-build");
}

/// Make protoc discoverable, falling back to the vendored binary (hermetic).
fn ensure_protoc() {
    let have_protoc = env::var_os("PROTOC").is_some() || protoc_on_path();
    if have_protoc {
        return;
    }
    match protoc_bin_vendored::protoc_bin_path() {
        Ok(path) => {
            println!(
                "cargo:warning=rush-transport-grpc: no system protoc found, using vendored protoc at {}",
                path.display()
            );
            env::set_var("PROTOC", path);
        }
        Err(e) => panic!(
            "rush-transport-grpc: protoc not found on PATH, PROTOC unset, and vendored \
             protoc unavailable: {e}"
        ),
    }
}

fn protoc_on_path() -> bool {
    let Some(path_var) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path_var).any(|dir| {
        let candidate = dir.join("protoc");
        candidate.is_file()
            || Path::new(&format!("{}", candidate.display()))
                .with_extension("exe")
                .is_file()
    })
}
