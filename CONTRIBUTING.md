# Contributing

Start with a reproducible problem or a precise acceptance condition. Check the
existing issue and PR queue before duplicating work. Discuss changes to public
behavior, persistent formats, security or component boundaries with a maintainer.

Keep each commit independently understandable and usable. Include the regression
test with its fix, preserve original authorship, and separate unrelated cleanup.
When a later patch repairs an earlier unmerged patch, fold it into the owner.
Use `Fixes:` only after verifying the actual introducing commit.

Write an imperative subject and a narrative body explaining the problem and
remedy. Keep message lines at most 80 characters. State which checks actually
ran and which remain missing. Every contribution needs a `Signed-off-by:` under
the [Developer Certificate of Origin](https://developercertificate.org/).
AI-assisted changes disclose the agent in the PR and commit receipt. Preserve
the Rush receipt fields: AI-Agent, AI-Session-ID, AI-Task-Type,
AI-Context-Tokens, AI-Handoff-From, AI-Handoff-To, AI-Thought-Trace,
Generated-by and Signed-off-by. Use `unavailable` when usage cannot be verified;
never estimate it. Keep detailed provider receipts private. Record no private
reasoning traces. Generated-by immediately precedes Signed-off-by.

## Validate the change

Select crate tests through MAINTAINERS. Run `cargo check --workspace --locked
-j8` and affected tests on the approved runner. Use non-mutating formatting and
lint checks; inspect the existing baseline before attributing a failure.
The inherited `make pre-commit` runs formatting that changes files. Do not
silently include those changes in an unrelated patch.

For dependency, cache or replay changes, test clean execution, unchanged reuse,
source/header changes, missing outputs and restoration, failure cleanup and
the affected invalidation cases. For process/sandbox changes, test real child
process behavior. Keep physical platform claims tied to that platform's results.
The maintenance test map adds more specific required assertions.

## Submit and review

Target `main` with an atomic branch. Open AI-assisted PRs as drafts; include the
problem, remedy, exact base/head, tests, missing coverage, AI disclosure and a
short explanation of the affected component. For complex behavior, attach a
self-contained HTML explainer tied to the current head. Refresh it after changes.
A human reviews the diff and marks the PR ready.

Review each commit and the aggregate change, including executable examples,
workflows, dependencies, error paths and concurrency. A rewritten head needs a
new review and affected checks. Required native checks and human approval control
merge. Never weaken a gate to complete a task. After merge, verify the resulting
revision; preserve evidence and escalate if it fails.
