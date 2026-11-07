// SPDX-License-Identifier: MIT
// Copyright (C) 2026 Luis Chamberlain <mcgrof@kernel.org>
//
// Phase 5.C.4 commit 8: tf_cc_library depending on cc_library —
// surfaces the typed-CcInfo-vs-NestedSet header walk divergence
// that Pro's Q A flagged.

#include "hello_lib.h"

void invoke_dep() {
    print_hello();
}
