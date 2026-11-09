# Maintenance queue

Tasks below are proposed work. Claim one with an acceptance condition and budget
before implementation. All remain open until their evidence is recorded.

| Priority | Task | Completion condition |
| --- | --- | --- |
| P0 | Repair TensorFlow predeclared provider loading | Preserve initialized evaluator globals, export native Java providers correctly, load the real rules_java module and complete fresh TF 2.17 framework builds on both runners; use no placeholder providers |
| P0 | Establish this source release's build/test baseline | Locked workspace check and component suites on an approved runner; classify every failure/skip |
| P0 | Complete the workload acceptance matrix | Recover exact TF/Modular ledgers, run the relevant gates, reconcile runtime denominators and update docs/status with candidate-bound evidence |
| P0 | Run the Chores supervised pilot | One real repair plus denied, stale, missing-test and interrupted cases; patch and evidence retained |
| P0 | Qualify provider/account and GitHub adapters | Both clients use explicit routes; quota exhaustion pauses; writes enforce reviewed head/target and reconcile receipts |
| P1 | Add PR polling and bounded dispatch | Deduplication, exclusive claims, budgets, auth loss and restart tests; human adopts schedule |
| P1 | Preflight Linux capture credentials | Reject unsupported credential states before tracing; qualify the same mapped-namespace profile from Make/capture through assembly/replay, preserving full groups, capabilities and output checks |
| P1 | Qualify concurrent Linux directory observations | Model enumeration/mutation dependencies, pass directory-identity and cross-owner-checkpoint regressions, and complete fresh assembly/replay with parity on both runners; keep unsafe observations rejected |
| P1 | Extract Kbuild receipt/provenance modules | One private-module move with unchanged snapshot encoding and rejection tests |
| P1 | Extract orchestration phases | Preserve graph publication and freshness; affected command/no-op tests pass |
| P1 | Extend remote-execution qualification | Qualify a second backend, TLS and nonempty instances; add recovery/cancellation and declared dynamic dependencies; resolve the retained HIP runtime failure before claiming GPU parity |
| P1 | Extend Buck2 performance coverage | Add optimized/compilation-heavy graphs, varied edits, a second measured host and remote timings; preserve exact work, output and invalidation checks |
| P1 | Qualify daemon build-option handling | Reproduce job-limit and disk-cache-disable routing across daemon and in-process commands; require matching requested behavior |
| P1 | Split observation syscall handlers | Preserve process/descriptor state and incomplete-capture rejection |
| P2 | Separate C/C++ compile and link planners | Preserve argv ordering, archive semantics and toolchain selection |
| P2 | Separate evaluator and module-loader responsibilities | Preserve cycle handling, concurrency, cache and accounting behavior |
| P2 | Publish reproducible compatibility/performance reports | Exact target/configuration/commands, artifact parity, samples and limitations |

Route tasks through MAINTAINERS. Keep branch/head, owner, dependencies, findings,
checks, review state and next action in the operational queue. Do not rewrite
this proposal into a list of presumed successes.

Completed on 2026-11-06: the replacement status docs were published, the
separate tracker received its relocation notice, and its GitHub archived state
was verified. See [the status index](docs/status/README.md) for the record.

Completed on 2026-11-08: profile and remove repeated graph preparation and
retained-artifact copying in the local Buck2 comparison. Verify sixteen daemon
controls and publish twenty samples per engine/mode at two graph sizes. Rush
has lower medians in all eight rows; retain the scope, overlapping edit ranges
and inherited test failures in [the report](docs/benchmarks/buck2.md).
