cc_library(
    name = "hello_lib",
    srcs = ["hello_lib.cc"],
    hdrs = ["hello_lib.h"],
)

cc_binary(
    name = "hello",
    srcs = ["main.cc"],
    deps = [":hello_lib"],
)

cc_test(
    name = "hello_test",
    srcs = ["hello_test.cc"],
    deps = [":hello_lib"],
)

# Phase 5.C.4: tf_cc_library verification target. Same source
# files as hello_lib but built through the TF rule for the
# verifier to compare scheduler vs legacy argv on.
tf_cc_library(
    name = "tf_hello_lib",
    srcs = ["hello_lib.cc"],
    hdrs = ["hello_lib.h"],
)

# Phase 5.C.4 commit 8: richer tf_cc_library verification target
# with a cc_library dependency. Per Pro's Q A: tf_cc_library
# legacy build() walks dep NestedSet for header parent dirs;
# scheduler analyze() walks dep CcInfo.compilation.headers.
# This target exercises both walks and lets the verifier
# surface any divergence between the two sources.
tf_cc_library(
    name = "tf_dep_consumer",
    srcs = ["dep_consumer.cc"],
    deps = [":hello_lib"],
)

# Phase 5.C.4 commit 11: proto rule verifier-pipeline target.
# proto_library exposes ProtoFilesInfo; cc_proto_library
# consumes it and declares one ProtocCompile per .proto in the
# closure (currently transitive_sources len == 1).
proto_library(
    name = "greeting_proto",
    srcs = ["greeting.proto"],
)

cc_proto_library(
    name = "greeting_cc_proto",
    deps = [":greeting_proto"],
)

# Phase 5.C.4 commit 14: downstream consumer of cc_proto_library.
# Pro's Q8: proves that the typed CcInfo from a cc_proto_library
# dep flows to the consumer's CppCompile correctly:
#   - argv gains -I<proto output_subdir> via dep
#     CcInfo.compilation.include_dirs walk
#   - declared_inputs include the generated .pb.h via
#     collect_generated_header_inputs walking dep
#     CcInfo.compilation.headers (filtered non-source)
# The verifier runs on this target's CppCompile and asserts
# zero diff against the legacy build's argv + declared inputs.
cc_library(
    name = "uses_greeting_proto",
    srcs = ["uses_greeting_proto.cc"],
    deps = [":greeting_cc_proto"],
)

# Phase 5.C.4 commit 16: tf_proto_library smoke target.
# Pro's Q1: "Cheap maybe, not free. Allowlist tf_proto_library
# only after a smoke target passes zero-diff for the generated
# ProtocCompile / CppCompile / CcArchive chain."
#
# Current state (verified 2026-05-01): tf_proto_library declares
# ProtocCompile in analyze() but does NOT populate
# ExecutableAction; CppCompile and CcArchive on the generated
# .pb.cc are not declared at all. The eligibility predicate
# (commit 15) correctly reports tf_proto_library targets as
# ActionGraphIncomplete — which is the smoke result. Full
# migration parallel to commit 12 is the next concrete step;
# tf_proto_library does NOT join SCHEDULER_RULE_ALLOWLIST_REF
# in this commit.
tf_proto_library(
    name = "tf_smoke_proto",
    srcs = ["tf_smoke.proto"],
)

# Phase 5.C.4 commit 24: downstream consumer of tf_proto_library.
# Pro's commit-24 directive: "cc_library or tf_cc_library
# consumes generated headers from tf_proto_library. Verify
# include dirs and generated header inputs. Only then consider
# allowlist." Mirrors the cc_proto_library downstream-consumer
# proof from commit 14: prove the typed-CcInfo flow propagates
# headers + include_dirs + linker_inputs to dependents.
#
# The verifier checks:
#   - argv contains -I<tf_proto output_dir>/genfiles via
#     CcInfo.compilation.include_dirs walk
#   - declared_inputs include the generated .pb.h via
#     collect_generated_header_inputs walking dep
#     CcInfo.compilation.headers (filtered non-source)
#   - CppCompile spec mirrors zero-diff between scheduler
#     and legacy
cc_library(
    name = "uses_tf_smoke_proto",
    srcs = ["uses_tf_smoke_proto.cc"],
    deps = [":tf_smoke_proto"],
)

# Phase 5.C.4 commit 17: tf_cc_library dep-bearing regression
# target (Pro's Q3 second shape): tf_cc_library depending on
# a generated-header provider (cc_proto_library). Exercises
# the typed-CcInfo.compilation.headers walk through a
# tf_cc_library consumer. Combined with tf_dep_consumer
# (commit 8 — tf_cc_library depending on plain cc_library),
# the two shapes cover the dep-bearing surface Pro wanted
# guarded before tf_cc_library re-joins the allowlist.
#
# Current state (verified 2026-05-01): tf_cc_library is OFF
# the SCHEDULER_RULE_ALLOWLIST_REF (commit 8 downgrade,
# commit 15 enforcement-as-code). Until BOTH dep shapes pass
# the verifier zero-diff, tf_cc_library does NOT re-join.
# Status doc tracks the divergence resolution paths Pro
# listed (modernize legacy build() vs constrain analyze()).
tf_cc_library(
    name = "tf_uses_proto",
    srcs = ["tf_uses_proto.cc"],
    deps = [":greeting_cc_proto"],
)

# Phase 5.C.4 commit 29: tf_cc_binary verifier smoke target.
# Pro's commit-29 directive: "zero-diff required before
# allowlist. verify dep link inputs and generated headers
# if present."
#
# Reuses main.cc (also used by hello cc_binary) — same source
# in two rule kinds lets the verifier compare argv shape
# differences honestly. Depends on hello_lib so the dep
# CcInfo walks (typed header + linker_inputs) have content.
tf_cc_binary(
    name = "tf_hello",
    srcs = ["main.cc"],
    deps = [":hello_lib"],
)

# Phase 5.C.4 commit 30: tf_cc_test verifier smoke target.
# Pro's commit-30 directive: "zero-diff required before
# allowlist. ensure --include-tests path still behaves."
#
# tf_cc_test auto-adds //testing:gtest_main as a dep —
# unique to the TF rule (cc_test doesn't). The dep resolves
# to a stubbed system package on hello_cpp; the actual gtest
# framework isn't pulled in. The smoke target's purpose is
# verifier zero-diff on the build path (CppCompile + CcLink
# argv shape), not gtest framework coverage.
tf_cc_test(
    name = "tf_hello_test_smoke",
    srcs = ["tf_hello_test.cc"],
    deps = [":hello_lib"],
)
