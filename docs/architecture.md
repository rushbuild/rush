# Component boundaries

Rush separates workspace loading and rule evaluation from actions, execution,
and stored outputs. MAINTAINERS names review responsibilities for the existing
crates. These responsibilities are implemented as routing data; automatic task
dispatch and component-specific persistent agents are proposed work.

The command and transport crates accept user requests. Workspace, repository,
parse and Starlark crates resolve their build descriptions. Rule crates produce
actions. The build crate orchestrates analysis and execution. Action, graph and
incremental crates define dependency and value identities. Execution, sandbox
and resource crates run and admit work. Cache and remote crates store or move
results. Observation and external snapshot code capture and replay externally
analyzed graphs. Profiling supplies measurements across these boundaries.

Keep cache identity, freshness validation and resource admission as shared
services. Component extraction must preserve these invariants across native
and external graphs. Do not create a separate kernel build engine or duplicate
GNU Make's evaluator.

## Extraction order

Source inspection identifies these large modules as initial candidates. Counts
describe the imported source, not a complexity or performance measurement.

| Module | Approximate lines | First seams |
| --- | ---: | --- |
| rush-build/src/kbuild_snapshot.rs | 40,236 | records, provenance, eligibility, environment and descriptor checks |
| rush-build/src/build_orchestrator_ext.rs | 23,422 | loading, analysis, closure, graph publication, execution |
| rush-observe/src/linux.rs | 18,267 | syscall families, process/descriptor state, receipt validation |
| rush-rules/src/rules_cc.rs | 14,576 | compile planning, link planning, toolchain selection |
| rush-starlark/src/evaluator.rs | 11,109 | values, builtins, evaluation state |

First inventory public interfaces and write characterization tests around the
chosen seam. Move one cohesive responsibility into a private module, retaining
its interface and serialization. Require unchanged command ordering, failure
behavior and cache identities where relevant. Review the move separately from
semantic changes. Measure before attempting an optimization. Avoid replacing
one oversized file with a single oversized child module.
