# Maintenance queue

Tasks below are proposed work. Claim one with an acceptance condition and budget
before implementation. All remain open until their evidence is recorded.

| Priority | Task | Completion condition |
| --- | --- | --- |
| P0 | Establish this source release's build/test baseline | Locked workspace check and component suites on an approved runner; classify every failure/skip |
| P0 | Revalidate imported workload status | Recover exact historical TF/Modular ledgers, rerun the relevant gates, reconcile all runtime denominators and update docs/status with current evidence |
| P0 | Run the Chores supervised pilot | One real repair plus denied, stale, missing-test and interrupted cases; patch and evidence retained |
| P0 | Qualify provider/account and GitHub adapters | Both clients use explicit routes; quota exhaustion pauses; writes enforce reviewed head/target and reconcile receipts |
| P1 | Add PR polling and bounded dispatch | Deduplication, exclusive claims, budgets, auth loss and restart tests; human adopts schedule |
| P1 | Extract Kbuild receipt/provenance modules | One private-module move with unchanged snapshot encoding and rejection tests |
| P1 | Extract orchestration phases | Preserve graph publication and freshness; affected command/no-op tests pass |
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
