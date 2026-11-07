#!/usr/bin/env bash
# Rush formal-verification + agentic-maintenance: spec-sensitivity gate.
# Copyright © 2026 Luis Chamberlain. SPDX-License-Identifier: MIT
#
# Detects SPEC-SENSITIVE changes and ROUTES them to human Spec-Review. It does
# NOT try to prove semantic weakening — it conservatively flags a diff that
# touches a spec-owned contract/oracle in a contract/oracle-shaped way, or any
# tcb/cfg file, and then requires the commit(s) to carry the
#   Spec-Change: <kind>   (!= none)
#   Spec-Review: human:<handle>:<YYYY-MM-DD>:<ticket-or-pr>
# trailers. Local git hooks are bypassable (--no-verify), so the SAME check is
# meant to run in CI over the PR commit range — that is the authoritative gate.
#
# Usage:
#   scripts/verify/spec-gate.sh --cached              # staged diff (pre-commit)
#   scripts/verify/spec-gate.sh --range <A>..<B>      # PR commit range (CI)
#   scripts/verify/spec-gate.sh --range <A>..<B> --require-trailers
#
# Exit codes: 0 = no spec-sensitive change, or sensitive + properly reviewed.
#             1 = spec-sensitive change missing Spec-Change/Spec-Review.
#             2 = usage / environment error.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
MANIFEST="${REPO_ROOT}/ci/spec-owned.tsv"

MODE=""
RANGE=""
REQUIRE_TRAILERS=0

while [ $# -gt 0 ]; do
    case "$1" in
        --cached) MODE="cached"; shift ;;
        --range) MODE="range"; RANGE="${2:-}"; shift 2 ;;
        --require-trailers) REQUIRE_TRAILERS=1; shift ;;
        -h|--help) grep '^#' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "spec-gate: unknown arg: $1" >&2; exit 2 ;;
    esac
done

[ -n "$MODE" ] || { echo "spec-gate: pass --cached or --range <A>..<B>" >&2; exit 2; }
[ -f "$MANIFEST" ] || { echo "spec-gate: missing manifest $MANIFEST" >&2; exit 2; }
if [ "$MODE" = "range" ] && [ -z "$RANGE" ]; then
    echo "spec-gate: --range needs an argument like origin/main..HEAD" >&2; exit 2
fi

# ---- Codex detection heuristics (VERBATIM regexes from the design) ---------
contract_re='^[+-].*(#\[(requires|ensures|invariant|variant|trusted|erasure|logic|predicate|extern_spec)\b|logic[[:space:]]+fn|predicate[[:space:]]+fn|assume!\(|trusted|proof_assert!\()'
oracle_re='(^-.*(assert(_eq|_ne)?!|prop_assert|proptest!|#\[test\]|golden|fixture))|(^\+.*(#\[ignore\]|prop_assume!|todo!\(|unimplemented!\(|return;))'
tcb_re='^(rust-toolchain(\.toml)?|why3find\.json|ci/creusot-toolchain\.lock|ci/dafny-toolchain\.lock|Cargo\.lock|\.cargo/config\.toml)$'
# Dafny addendum (model kind): a .dfy spec laboratory / golden model. Any edit
# to a model is spec-sensitive; the regex below is recorded as evidence. It
# also flags the proof-soundness escape hatches ({:axiom}, {:verify false},
# assume) that would let a model "verify" vacuously.
model_re='^[+-].*((requires|ensures|invariant|decreases|predicate|function|lemma|method|datatype|ghost|forall|exists|assume)|\{:(axiom|verify false|extern|compile false)\})'

# diff_for_file <path> : print the -U0 cached/range diff for one path.
diff_for_file() {
    local path="$1"
    if [ "$MODE" = "cached" ]; then
        git diff --cached -U0 -- "$path"
    else
        git diff -U0 "$RANGE" -- "$path"
    fi
}

# changed_files : list of paths changed in the selected diff.
changed_files() {
    if [ "$MODE" = "cached" ]; then
        git diff --cached --name-only
    else
        git diff --name-only "$RANGE"
    fi
}

# ---- Build path->kind map from the manifest --------------------------------
declare -A KIND_OF
while IFS=$'\t' read -r scope kind path; do
    [ -z "${scope:-}" ] && continue
    case "$scope" in \#*) continue ;; esac
    [ -z "${path:-}" ] && continue
    KIND_OF["$path"]="$kind"
done < "$MANIFEST"

SENSITIVE=0
declare -a REASONS=()

while IFS= read -r f; do
    [ -z "$f" ] && continue
    kind="${KIND_OF[$f]:-}"

    # tcb / cfg manifest files: ANY change is spec-sensitive.
    if [ "$kind" = "tcb" ] || [ "$kind" = "cfg" ]; then
        SENSITIVE=1
        REASONS+=("$f: $kind file changed (toolchain/config is part of the TCB)")
        continue
    fi

    # The Codex tcb_re also catches well-known toolchain/lock files even if a
    # project forgot to list them in the manifest. Match on basename+path.
    if printf '%s\n' "$f" | grep -Eq "$tcb_re"; then
        SENSITIVE=1
        REASONS+=("$f: matches TCB path pattern")
        continue
    fi

    # contract files: flag if the diff matches the contract regex.
    if [ "$kind" = "contract" ]; then
        if diff_for_file "$f" | grep -Eq "$contract_re"; then
            SENSITIVE=1
            REASONS+=("$f: contract-sensitive diff (requires/ensures/logic/trusted/assume/proof_assert)")
        fi
        continue
    fi

    # model files (Dafny addendum): ANY change to a .dfy spec laboratory /
    # golden model is spec-sensitive — the model defines the abstract
    # properties the Creusot contract mirrors, so weakening it silently
    # weakens the whole refinement chain. The model_re hit is recorded as
    # evidence (esp. proof escape hatches).
    if [ "$kind" = "model" ]; then
        SENSITIVE=1
        local_hit=$(diff_for_file "$f" | grep -Eo "$model_re" | head -1 || true)
        REASONS+=("$f: model (.dfy) changed — spec-sensitive${local_hit:+ (evidence: $local_hit)}")
        continue
    fi

    # oracle files: flag if the diff matches the oracle-weakening regex.
    if [ "$kind" = "oracle" ]; then
        if diff_for_file "$f" | grep -Eq "$oracle_re"; then
            SENSITIVE=1
            REASONS+=("$f: oracle-weakening diff (removed assert/test, or added ignore/todo/unimplemented/early-return)")
        fi
        continue
    fi
done < <(changed_files)

if [ "$SENSITIVE" -eq 0 ]; then
    echo "spec-gate: no spec-sensitive change detected."
    exit 0
fi

echo "spec-gate: SPEC-SENSITIVE change detected:"
for r in "${REASONS[@]}"; do echo "  - $r"; done

# ---- Verify the routing trailers when asked (CI over a commit range) -------
# In --cached/pre-commit mode the commit message does not exist yet, so we can
# only WARN. CI must re-run with --range --require-trailers to enforce.
if [ "$REQUIRE_TRAILERS" -eq 0 ]; then
    echo
    echo "spec-gate: this is the routing signal, not a verdict."
    echo "  The commit MUST set:  Spec-Change: <contract|oracle|trusted|toolchain|cfg>"
    echo "                        Spec-Review: human:<handle>:<YYYY-MM-DD>:<ticket-or-pr>"
    echo "  Local hooks are bypassable; CI re-runs with --range --require-trailers."
    exit 0
fi

# Enforcing mode: every commit in the range must carry valid trailers.
MISSING=0
spec_change_re='^Spec-Change:[[:space:]]*(impl-only|contract|oracle|trusted|toolchain|cfg)[[:space:]]*$'
spec_review_re='^Spec-Review:[[:space:]]*human:[^:]+:[0-9]{4}-[0-9]{2}-[0-9]{2}:.+$'

for c in $(git rev-list "$RANGE"); do
    msg="$(git log -1 --format=%B "$c")"
    sc="$(printf '%s\n' "$msg" | grep -E '^Spec-Change:' || true)"
    sr="$(printf '%s\n' "$msg" | grep -E '^Spec-Review:' || true)"

    if ! printf '%s\n' "$sc" | grep -Eq "$spec_change_re" \
       || printf '%s\n' "$sc" | grep -Eq '^Spec-Change:[[:space:]]*none'; then
        echo "spec-gate: commit $c: missing/invalid 'Spec-Change' (must be != none)"
        MISSING=1
    fi
    if ! printf '%s\n' "$sr" | grep -Eq "$spec_review_re"; then
        echo "spec-gate: commit $c: missing/invalid 'Spec-Review: human:<handle>:<date>:<ticket>'"
        MISSING=1
    fi
done

if [ "$MISSING" -ne 0 ]; then
    echo "spec-gate: FAILED — spec-sensitive change without human Spec-Review." >&2
    exit 1
fi

echo "spec-gate: spec-sensitive change is properly routed through Spec-Review."
exit 0
