#!/usr/bin/env bash
# Rush formal-verification track: "verified fn has no unverified twin" check.
# Copyright © 2026 Luis Chamberlain. SPDX-License-Identifier: MIT
#
# The anti-vacuity/TCB-correspondence rule requires that the PRODUCTION object
# naming path actually goes through the proven-injective verified-core helper —
# not a hand-rolled copy that quietly diverges from the proof. This script
# greps rush-rules to assert exactly that, for the cc-obj-name-v1 scope:
#
#   1. cc_obj_name_for_src MUST delegate to rush_verified_core::cc_obj_key /
#      CcObjKey::object_name (the verified encoding).
#   2. The legacy non-injective pattern `pkg.replace('/', "_")` building an
#      object name MUST NOT reappear in the production crate (an "unverified
#      twin" of the verified function).
#
# Exit 0 = production path is wired to the verified core; 1 = a twin/divergence.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
RULES="${REPO_ROOT}/crates/rush-rules/src/rules_cc.rs"

[ -f "$RULES" ] || { echo "no-unverified-twin: missing $RULES" >&2; exit 1; }

fail=0

# (1) The helper must delegate to the verified core.
if ! grep -Eq 'rush_verified_core::(cc_obj_key|CcObjKey)' "$RULES"; then
    echo "no-unverified-twin: FAIL — cc_obj_name_for_src does not delegate to"
    echo "  rush_verified_core (the proven-injective object-name encoding)."
    fail=1
else
    echo "no-unverified-twin: OK — object naming delegates to rush-verified-core."
fi

# (2) The legacy non-injective object-name construction must be gone.
#     `.replace('/', "_")` immediately feeding a `.o` format is the fingerprint
#     of the buggy encoding. Match the pattern on a single line.
if grep -En "replace\('/',[[:space:]]*\"_\"\).*\.o" "$RULES" >/dev/null; then
    echo "no-unverified-twin: FAIL — legacy non-injective object-name pattern"
    echo "  (pkg.replace('/', \"_\") -> .o) reappeared in the production crate:"
    grep -En "replace\('/',[[:space:]]*\"_\"\).*\.o" "$RULES"
    fail=1
fi

if [ "$fail" -ne 0 ]; then
    exit 1
fi
echo "no-unverified-twin: PASS — no unverified object-name twin in rush-rules."
exit 0
