// SPDX-License-Identifier: MIT
// Copyright (C) 2026 Luis Chamberlain <mcgrof@kernel.org>
//
// Phase 5.C.4 commit 30: tf_cc_test verifier smoke source.
// Minimal test that doesn't pull in gtest/gmock headers —
// even though tf_cc_test auto-adds //testing:gtest_main as
// a dep, we don't actually #include gtest here. The smoke
// target's purpose is verifier zero-diff on the build path
// (CppCompile + CcLink argv shape), not test framework
// coverage.

#include "hello_lib.h"

int main() {
    print_hello();
    return 0;
}
