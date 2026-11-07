#!/bin/bash
# Aggressive stub detection script
# ZERO TOLERANCE for stubs/lies in code

set -e

STUB_COUNT=0

echo "Searching for FORBIDDEN patterns..."
echo ""

# Pattern 1: "stub" in comments/strings.
#
# This pattern is deliberately broad — false positives get excluded
# below one by one so the signal stays honest. Every exclusion is a
# case we've triaged and decided is not a lie:
#
#   * `register_system_stub` / `SystemCcLibrary` / `system stub` —
#     rush's well-known mechanism for synthesizing cc_library nodes
#     for external repos whose headers come from the system (absl,
#     llvm, etc.). The mechanism is intentional, documented, and
#     produces real include paths via CcInfo; "stub" is its name,
#     not its character.
#   * `register_absl_*_stub`, `register_local_tsl_external_stubs` —
#     specific instances of the above synthesis path.
#   * `Stub BUILD for` — .bzl/BUILD files written into the external
#     cache for allowed categories (see CLAUDE.md "Exceptions" and
#     UNIMPLEMENTED.md).
#   * `stub-shaped`, `stub detection`, `stub detector`, `stub scanner`,
#     `stub markers`, `stub target`, `stub synthesis` — scanner /
#     detector meta-terminology.
#   * `unstub`, `stubbed`, `test_stub` — historical non-matches.
#
# If you add an exclusion, also add an UNIMPLEMENTED.md entry or a
# one-sentence justification in the commit message.
echo "❌ Pattern 1: 'stub' or 'Stub' in comments/strings (after exclusions)"
MATCHES=$(grep -rn --include="*.rs" --include="*.bzl" -i "stub" crates/ src/ 2>/dev/null \
    | grep -v "unstub" \
    | grep -v "test_stub" \
    | grep -v "stubbed" \
    | grep -v "register_system_stub" \
    | grep -v "register_absl_stub\|register_absl_strings_stub" \
    | grep -v "register_local_tsl_external_stubs" \
    | grep -v "SystemCcLibrary" \
    | grep -v "system stub\|system stubs" \
    | grep -v "Stub BUILD for\|stub BUILD for" \
    | grep -v "Stub.*build_defs.bzl\|stub.*build_defs.bzl" \
    | grep -v "stub-shaped" \
    | grep -v "stub detection\|stub detector\|stub scanner" \
    | grep -v "stub markers\|stub-shaped" \
    | grep -v "stub target\|stub targets" \
    | grep -v "stub synthesis\|stubbing as" \
    | grep -v "stub synthesizer\|Synthesizing stub" \
    || true)
if [ -n "$MATCHES" ]; then
    echo "$MATCHES" | head -20
    STUB_COUNT=$((STUB_COUNT + 1))
fi
echo ""

# Pattern 2: Functions with just "pass" in .bzl files
echo "❌ Pattern 2: Functions that just 'pass' in .bzl files (excluding test fixtures)"
MATCHES=$(find crates/ src/ -name "*.bzl" -exec grep -Hn "^\s*pass\s*$" {} \; 2>/dev/null || true)
if [ -n "$MATCHES" ]; then
    echo "$MATCHES" | head -20
    STUB_COUNT=$((STUB_COUNT + 1))
fi
echo ""

# Pattern 3: Empty Vec::new() with no logic
echo "❌ Pattern 3: Functions returning empty Vec::new() with no logic"
MATCHES=$(grep -rn --include="*.rs" "fn.*Vec<.*>.*{.*Vec::new().*}" crates/ src/ 2>/dev/null | grep -v "test" | grep -v "Default::" || true)
if [ -n "$MATCHES" ]; then
    echo "$MATCHES" | head -20
    STUB_COUNT=$((STUB_COUNT + 1))
fi
echo ""

# Pattern 4: "not implemented" lies
echo "❌ Pattern 4: 'not implemented' or 'unimplemented' in LIES (not honest errors)"
MATCHES=$(grep -rn --include="*.rs" -i "not implemented\|unimplemented\|todo.*implement" crates/ src/ 2>/dev/null | \
    grep -v "UNIMPLEMENTED.md" | \
    grep -v "bail!" | \
    grep -v "panic!" | \
    grep -v "TODO:" | \
    grep -v "# WARNING:" | \
    grep -v "///.*NOT IMPLEMENTED" | \
    grep -v "\".*not implemented" | \
    grep -v "fail(\"" | \
    grep -v "// .*not implemented" | \
    grep -v "Skipped (" | \
    grep -v "# .*auto-detection not implemented" | \
    grep -v "This rule type is not implemented" | \
    grep -v "(not implemented)" | \
    grep -v "generated_io.rs" | \
    grep -v "unimplemented rule sets" || true)
if [ -n "$MATCHES" ]; then
    echo "$MATCHES" | head -20
    STUB_COUNT=$((STUB_COUNT + 1))
fi
echo ""

# Pattern 5: "does nothing" / "no-op" in a lying context.
#
# Honest no-ops (platform branches, accepted-and-ignored metadata rules)
# stay — we exclude phrases that mean "by-design no-op" below.
# Lying no-ops ("TODO: will implement later, for now do nothing") are the
# target.
echo "❌ Pattern 5: Functions with 'does nothing' or 'no-op' LIES (not honest metadata-only)"
MATCHES=$(grep -rn --include="*.rs" --include="*.bzl" -i "does nothing\|no-op\|noop" crates/ src/ 2>/dev/null | \
    grep -v "metadata-only" | \
    grep -v "DISABLED" | \
    grep -v "Pass statement" | \
    grep -v "Out-of-scope by design" | \
    grep -v "outside rush's" | \
    grep -v "by design" | \
    grep -v "Accepted and ignored" | \
    grep -v "accepted and ignored" | \
    grep -v "No-op on non-Linux" | \
    grep -v "Dead \`builtin" | \
    grep -v "no-op stubs that allow .bzl" | \
    grep -v "no-op codegen macros" | \
    grep -v "is a no-op macro" | \
    grep -v "no-op placeholder" | \
    grep -v "no-op hook for rules_cc" | \
    grep -v "generate_cc is a no-op" | \
    grep -v "no-op so the BUILD evaluation" | \
    grep -v "Stub for config_googleapis - no-op" || true)
if [ -n "$MATCHES" ]; then
    echo "$MATCHES" | head -20
    STUB_COUNT=$((STUB_COUNT + 1))
fi
echo ""

# Pattern 6: Auto-generated stub files in .cache
echo "❌ Pattern 6: Auto-generated stub files"
MATCHES=$(grep -rn --include="*.bzl" "Auto-generated stub" .cache/ 2>/dev/null || true)
if [ -n "$MATCHES" ]; then
    echo "$MATCHES" | head -20
    STUB_COUNT=$((STUB_COUNT + 1))
fi
echo ""

# Pattern 7: Stub-shaped content in rush-emitted generated files.
# Runtime content under rush-out/genfiles or rush-out/genfiles-* is
# produced by rush itself — stubs there are the exact cheat that
# bit us in the 2026-04-21 session (fake op_quant_spec_getters.inc
# etc.). Scan a representative set of workspace output dirs the
# rush CI exercises. Extend RUSH_OUTPUT_DIRS if you point rush at
# a different output root.
echo "❌ Pattern 7: Stub markers in rush-emitted generated files"
DIRS_TO_SCAN="${RUSH_OUTPUT_DIRS:-rush-out /data/tensorflow/rush-out}"
for d in $DIRS_TO_SCAN; do
    if [ -d "$d" ]; then
        MATCHES=$(grep -rln --include="*.inc" --include="*.h" --include="*.cc" \
            -E "gentbl_cc_library stub|Stub emitted by rush|empty stub|// HACK:|placeholder content|placeholder implementation" \
            "$d" 2>/dev/null | head -20 || true)
        if [ -n "$MATCHES" ]; then
            echo "Files under $d:"
            echo "$MATCHES"
            STUB_COUNT=$((STUB_COUNT + 1))
        fi
    fi
done
echo ""

# Pattern 8: Source patches on the consumer workspace (TF tree). These
# are uncommitted `sed`/python rewrites applied during iteration to
# work around upstream bugs; they must live in a committed patch file
# under third_party_patches/ or scripts/ rather than a shell one-liner.
echo "❌ Pattern 8: Unversioned consumer-workspace patches"
for ws in ${RUSH_CONSUMER_WORKSPACES:-/data/tensorflow}; do
    if [ -d "$ws/.git" ]; then
        # git diff against HEAD picks up local mutations we never want
        # in a "green" CI state — if the env setup requires patches,
        # they should be applied by a committed script that also makes
        # the change reproducible.
        unstaged=$(cd "$ws" && git diff --name-only 2>/dev/null | head -5)
        if [ -n "$unstaged" ]; then
            echo "Unversioned edits in $ws:"
            echo "$unstaged"
            STUB_COUNT=$((STUB_COUNT + 1))
        fi
    fi
done
echo ""

if [ $STUB_COUNT -gt 0 ]; then
    echo ""
    echo "╔════════════════════════════════════════════════════════════╗"
    echo "║  ⛔ STUB DETECTION FAILED - STUBS FOUND                   ║"
    echo "╚════════════════════════════════════════════════════════════╝"
    echo ""
    echo "STUBS = LIES. NO EXCEPTIONS."
    echo ""
    echo "Every stub MUST either:"
    echo "  1. Be implemented properly"
    echo "  2. Fail loudly with anyhow::bail!() explaining what's missing"
    echo "  3. Be documented in UNIMPLEMENTED.md as intentionally unsupported"
    echo ""
    exit 1
else
    echo "✅ No stubs found - code is honest about limitations"
fi
