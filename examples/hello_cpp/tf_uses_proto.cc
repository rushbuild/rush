// SPDX-License-Identifier: MIT
// Copyright (C) 2026 Luis Chamberlain <mcgrof@kernel.org>
//
// Phase 5.C.4 commit 17: tf_cc_library depending on a
// generated-header provider (cc_proto_library). Pro's second
// shape from Q3:
//
//   tf_cc_library depends on a generated-header provider
//   CppCompile declared inputs include the generated header
//   producer chain is valid
//
// This exercises the typed-CcInfo header walk through a
// tf_cc_library consumer. cc_proto_library exposes the
// generated .pb.h via CcInfo.compilation.headers; the
// regression target proves tf_cc_library either picks it up
// correctly or surfaces the same class of divergence the
// tf_dep_consumer target caught (commit 8).

#include "greeting.pb.h"

void tf_uses_proto() {
    hello_cpp::Greeting g;
    g.set_name("tf_cc_library");
    g.set_text("regression target");
}
