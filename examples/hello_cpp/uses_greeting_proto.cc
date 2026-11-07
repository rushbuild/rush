// SPDX-License-Identifier: MIT
// Copyright (C) 2026 Luis Chamberlain <mcgrof@kernel.org>
//
// Phase 5.C.4 commit 14: downstream consumer of cc_proto_library.
// Pro's Q8: prove that typed CcInfo.compilation.headers and
// CcInfo.compilation.include_dirs propagate to a dependent
// cc_library's CppCompile. The verifier runs on this target's
// CppCompile and checks:
//   - argv contains -I<proto output_subdir>
//   - declared_inputs include the generated .pb.h
// Includes greeting.pb.h via the include_dir cc_proto_library
// puts on its CcInfo.compilation.include_dirs (the per-target
// rush-out subdir where protoc emits the .pb.h/.pb.cc).

#include "greeting.pb.h"

void use_greeting() {
    hello_cpp::Greeting g;
    g.set_name("rush");
    g.set_text("hello");
}
