//! Build script for `rush-protocol`.
//!
//! Generates Rust message types from `proto/rush_command_v1.proto` using
//! `prost-build`. prost-build emits ONLY the message types; the `service` block in
//! the .proto is ignored (the tonic service traits are generated separately by the
//! `rush-transport-grpc` crate). This keeps `rush-protocol` free of any tonic
//! dependency, as required by the persistent-daemon design.
//!
//! Hermeticity: if `PROTOC` is not already set in the environment and no `protoc`
//! is found on `PATH`, fall back to the `protoc-bin-vendored` binary so the build
//! succeeds without a system protoc.

use std::env;
use std::path::Path;

fn main() {
    let proto_file = "proto/rush_command_v1.proto";
    let proto_dir = "proto";

    println!("cargo:rerun-if-changed={proto_file}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PROTOC");

    // Make the build hermetic: only consult the vendored protoc when the caller
    // has not provided one and none is discoverable on PATH.
    let have_protoc = env::var_os("PROTOC").is_some() || protoc_on_path();
    if !have_protoc {
        match protoc_bin_vendored::protoc_bin_path() {
            Ok(path) => {
                println!(
                    "cargo:warning=rush-protocol: no system protoc found, using vendored protoc at {}",
                    path.display()
                );
                env::set_var("PROTOC", path);
            }
            Err(e) => {
                panic!(
                    "rush-protocol: protoc not found on PATH, PROTOC unset, and vendored \
                     protoc unavailable: {e}"
                );
            }
        }
    }

    let mut config = prost_build::Config::new();
    // Use the `bytes` crate's `Bytes` type for all `bytes` fields so callers get
    // cheap clones and zero-copy framing for the WireFrame challenger transport.
    config.bytes(["."]);

    config
        .compile_protos(&[proto_file], &[proto_dir])
        .expect("rush-protocol: failed to compile rush_command_v1.proto");
}

/// Returns true if a `protoc` executable is discoverable on `PATH`.
fn protoc_on_path() -> bool {
    let Some(path_var) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path_var).any(|dir| {
        let candidate = dir.join("protoc");
        candidate.is_file() || Path::new(&format!("{}", candidate.display())).with_extension("exe").is_file()
    })
}
