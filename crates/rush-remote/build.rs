// SPDX-License-Identifier: MIT
fn main() {
    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-env-changed=PROTOC");
    if std::env::var_os("PROTOC").is_none() {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path().unwrap());
    }
    // Protobuf comments contain non-Rust examples; retain their documentation
    // in the original schemas rather than generating invalid Rust doctests.
    let mut messages = prost_build::Config::new();
    messages.disable_comments(&["."]);
    tonic_build::configure()
        .build_server(false)
        .disable_comments(".")
        .include_file("reapi.rs")
        .compile_protos_with_config(
            messages,
            &[
                "proto/build/bazel/remote/execution/v2/remote_execution.proto",
                "proto/google/bytestream/bytestream.proto",
            ],
            &["proto"],
        )
        .expect("compile pinned Remote Execution API protos");
}
