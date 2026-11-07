# Maintenance evidence

These are required assertions for future maintenance, not passing test claims.
MAINTAINERS supplies first-line commands; the effect of the change selects
additional checks. Run them on the approved runner with its resource limits.

| Change | Required evidence |
| --- | --- |
| Documentation/routing | ownership checker, changed links, command existence, no unsupported capability claims |
| Analysis/rules | affected crate tests; select, repository mapping, dependency and command identity cases |
| Cache/incremental | clean/no-op/edit/delete/restore; negative probes, symlink and directory membership; corruption rejection |
| Capture/replay | successful and interrupted capture, exact ownership, undeclared access rejection, Make-free replay and artifact parity |
| Execution/admission | real process exit/cancellation/descendants, descriptor handling, memory/CPU pressure and failed cleanup |
| Command/transport | CLI stdout/JSON, authentication boundary, protocol compatibility, daemon restart and UDS integration |
| Release/dependencies | locked workspace check, affected suites, license review, package smoke, reproducible build scope |
| Performance | matched baseline/candidate, fixed inputs/tools, samples/distribution, artifact parity and measured bottleneck |

Bind results to source tree, command, toolchain, environment and runner identity.
Report pass, fail, skipped, unavailable, inconclusive, cancelled and not-run
separately. A returned success from a test that skipped its real assertion is
not coverage. Baseline failures require a control at the same environment.
Required missing coverage blocks merge. Post-merge checks run on the actual
merged revision; changed inputs invalidate affected earlier evidence.
