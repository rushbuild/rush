// SPDX-License-Identifier: MIT
// Copyright (C) 2026 Luis Chamberlain <mcgrof@kernel.org>
//
// Phase 5.C.4 commit 24: downstream consumer of tf_proto_library.
// Pro's commit-24 directive: "cc_library or tf_cc_library
// consumes generated headers; verifier passes; downstream
// CppCompile gets proto output include dir; generated producer
// chain remains valid."
//
// Consumes the generated .pb.h from the tf_smoke_proto target
// (commit 22's non-grpc tf_proto_library smoke). The consumer's
// CppCompile must:
//   - get -I<output_dir>/genfiles on argv via the typed
//     CcInfo.compilation.include_dirs walk
//   - declare the generated .pb.h as an input via
//     collect_generated_header_inputs (commit 14 fix)
//   - compile cleanly against the protobuf-generated TfSmoke
//     class

#include "tf_smoke.pb.h"

void uses_tf_smoke() {
    hello_cpp::tf_smoke::TfSmoke s;
    s.set_id(42);
}
