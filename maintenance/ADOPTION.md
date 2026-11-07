# Adoption record

This source reset adopts the Chores guide and pins its portable starter at
revision `19d0bb6f3de6500177887a7ceb41afe9da6dc554`. PIN.json records every
included file's SHA-256. The public skill download matched the included skill.
The bundled MIT notice covers the starter. The upstream policy schema is
preserved unchanged; Rush's separate draft lives at policy.proposed.json.

The draft records repository, branch, protected paths and maintenance classes.
Model limits, account route, evidence producer/runner bindings, required-test
records, expiry and human routing must be filled from the actual integration
before activation. The prose test map is a specification for those records;
empty required_tests arrays in the draft are not an acceptance gate.

The one-commit import is a source distribution boundary requested by the owner.
It preserves the implementation, fixtures and license notices while excluding
old session notes, operational documentation, compiler outputs and caches.
Machine-specific optional Modular probes now accept RUSH_MODULAR_ROOT; source
examples use their Cargo workspace path. Historical private receipts remain
with the old repository. The portable commit-message hook checks narrative,
DCO and receipt completeness without requiring a contributor's private logs.

Validation of metadata, ownership, pinning and documentation does not certify
the engine. Establish the frozen release candidate's build and behavior gates
on an approved runner before announcing a tested binary or performance result.
