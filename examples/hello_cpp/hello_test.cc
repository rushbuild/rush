// SPDX-License-Identifier: MIT
// Copyright (C) 2026 Luis Chamberlain <mcgrof@kernel.org>
//
// Phase 5.C.4 verifier smoke target: links against hello_lib and
// exits 0. The point is to give --use-scheduler --verify-scheduler-commands
// a cc_test target to compare legacy vs scheduler argv on.

#include "hello_lib.h"

int main(int /*argc*/, char** /*argv*/) {
    print_hello();
    return 0;
}
