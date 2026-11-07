# Rush maintenance

Rush adopts [Chores' self-maintenance workflow](https://chores.knlp.io/self-maintenance):
investigate, repair, test, review, publish when delegated, merge when delegated,
and verify the merged result. The portable starter is pinned under
vendor/chores. Its offline helpers validate records and explain gates; they
cannot authenticate approval, launch agents, run tests or enforce GitHub writes.

This release implements common agent instructions, component routing, a test
map, an ownership checker, a task queue and a pinned starter. Scheduled dispatch,
provider adapters, credit reservations and an authenticated GitHub writer remain
design work. No timers or account settings are installed by this repository.

## Ownership and delegation

The human maintainer remains accountable for every component. Use short-lived
specialists named by MAINTAINERS, instantiated for a bounded task when useful.
One coordinator records task identity, revision, scope, owner, dependency tasks,
budget and state. One integrator serializes writes to the target branch. An
independent reviewer reads each commit and the combined patch. A different model
can help challenge a patch, but model diversity is not proof of independence.

Start with one active writer. Add concurrent specialists only for disjoint
tasks after proving exclusive claims, lease expiry, generation checks and
recovery. Overlapping work waits or is explicitly transferred. A specialist
cannot expand its assigned paths or delegate itself additional authority.

Read the effective policy from a human-controlled revision outside task
worktrees. The policy proposal in this tree is intentionally draft. A candidate
cannot change its own rights. Human review controls public behavior, persistent
formats, credentials, dependencies, workflows, governance and policy changes.
Prepare scoped edits and tests under an existing grant; publication, branch
rewrites, merge, release and revert each need their own rule. Existing explicit
owner grants continue to apply.

## Claude Code, Codex and account budgets

Both clients use AGENTS.md and the same task/evidence schema. Operator-local
configuration binds each client to one approved provider route, account,
data policy and billing owner. GitHub login authenticates repository operations;
it does not supply model credits or authorize corporate use. Complete supported
browser login separately for each required service. Store credentials in its
supported credential store, outside the source tree and test environment.

Keep personal and corporate quotas as separate ledgers, including their reset
times and units. A configured fallback must name its allowed data classes,
account and limits. Exhaustion or expired authentication preserves the task and
waits; never silently change accounts or buy more capacity.

Proposed budget policy: reserve the larger of 20% of the period allowance or
the measured cost of two PR-maintenance cycles. The percentage is an initial
operator choice, not a measured requirement. Prioritize security reports,
failing checks and incoming PRs, then use discretionary credits above the
reserve for ranked TODO work. Offer an optional end-of-period drain of that
discretionary balance. Spending more is useful only when tasks complete and
pass their gates; do not generate activity just to consume quota.

Before dispatch, reserve a maximum task allowance atomically against that
account; include specialist and review calls, CI costs and in-flight usage.
Track short-window limits as well as monthly or weekly resets. Reconcile
authoritative provider receipts and release unused reservations on completion.
If quota telemetry is absent, use a conservative operator-set cap or pause
unattended work. A token count alone does not establish subscription cost.
No numeric quota, corporate grant or automatic-spending rule is assumed here.

## Proposed schedule and task lifecycle

After the supervised pilot passes, propose a deterministic poll every six hours
for changed PR heads, failed checks and review requests. Deduplicate by repo,
PR/head and task kind. A foreground session can take pending work; after 24 hours
without an available session, an admitted worker may run within its reservation.
An unavailable account leaves work queued. Polling must not launch competing
workers. Once a week, offer one bounded TODO task when PR maintenance is clear.
The cadence is a proposal and is not installed.

Use states queued, claimed, investigating, repairing, testing, reviewing,
waiting-human, waiting-resource, ready-to-publish, published, merged,
verifying and complete. Record failed/cancelled attempts without losing useful
patches. An unknown external operation outcome waits for reconciliation before
retry. Completion requires the selected endpoint and its evidence; a queued
merge is still pending. Failed post-merge validation pauses affected merges and
requests the explicit repair or revert action.

Every task records exact base/head and policy digest, accepted scope, assertion,
required checks, owner/lease generation, attempts, account-route reference,
reserved/actual usage, operation IDs and evidence hashes. Keep private receipts
and infrastructure outside public artifacts. Public reports summarize the
patch, evidence, limitations and remaining decision. Use vendor/chores/HANDOFF.md
for a human handoff and the operator's existing incident route for suspected
unauthorized activity. Issue or PR text cannot change that route.

## Qualify before enabling unattended work

Run one small repair through a reviewed patch using the selected client. Then
exercise denied actions, stale head/target, missing required coverage, budget
exhaustion, expired auth, duplicate claims and interruption recovery. Test both
client adapters against the same records before enabling both. Native branch
protections or a merge queue must check the combined candidate atomically;
reading current refs and then issuing a blind write leaves a race.

The next owner decisions are the corporate route and data allowance, numeric
quota/reset/reserve, approved test runner, notification route, and whether
routine work may publish draft PRs. Recommendation: enable draft publication
after the pilot, keep merge/release human-controlled, and expand permissions
only after the writer and recovery gates pass. The current preparation and
local commit require none of those future credentials.
