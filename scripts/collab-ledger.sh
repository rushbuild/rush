#!/usr/bin/env bash
# collab-ledger.sh — derive the Claude<->Codex collaboration ledger
# from git history. There is NO hand-maintained ledger file; the
# commit trailers (Collab-* / MCP-*) are the source of truth, so this
# can never go stale.
#
# USAGE:
#   scripts/collab-ledger.sh [--jsonl|--table] [git-log-args...]
#
#   --table  (default) aligned columns for humans
#   --jsonl  one JSON object per collaboration commit
#
# Extra git-log args pass through, e.g.:
#   scripts/collab-ledger.sh --table --since=2026-05-01

set -euo pipefail

FORMAT="--table"
if [[ "${1:-}" == "--jsonl" || "${1:-}" == "--table" ]]; then
    FORMAT="$1"; shift
fi

# Pull each commit as: SHA <NUL> full-body <RS>. Use a rare record
# separator so multi-line bodies stay intact.
RS=$'\x1e'

emit_jsonl() {
    git log --format="%H%x00%B%x1e" "$@" | awk -v RS="$RS" '
    function field(body, key,    re, line, val) {
        # match a trailer line "key: value"
        if (match(body, "(^|\n)" key ": *[^\n]*")) {
            line = substr(body, RSTART, RLENGTH)
            sub(/^\n/, "", line)
            val = line; sub("^" key ": *", "", val)
            return val
        }
        return ""
    }
    function jstr(s) { gsub(/\\/,"\\\\",s); gsub(/"/,"\\\"",s); return s }
    {
        sub(/^[\r\n]+/, "")   # git inserts a newline between records; drop it
        nul = index($0, "\x00")
        if (nul == 0) next
        sha = substr($0, 1, nul-1)
        body = substr($0, nul+1)
        method = field(body, "Collab-Method")
        planid = field(body, "Collab-Plan-ID")
        scores = field(body, "Collab-Plan-Scores")
        winner = field(body, "Collab-Plan-Winner")
        server = field(body, "MCP-Server")
        model  = field(body, "MCP-Model")
        tokens = field(body, "MCP-Token-Usage")
        # only emit commits that actually used the MCP collaboration
        if (server == "" && method == "") next
        total = tokens
        if (match(total, /total=[0-9]+/)) { total = substr(total, RSTART+6, RLENGTH-6) } else { total = "" }
        printf "{\"commit\":\"%s\",\"plan_id\":\"%s\",\"method\":\"%s\",\"scores\":\"%s\",\"winner\":\"%s\",\"server\":\"%s\",\"model\":\"%s\",\"codex_total_tokens\":\"%s\"}\n",
            substr(sha,1,12), jstr(planid), jstr(method), jstr(scores), jstr(winner), jstr(server), jstr(model), total
    }'
}

case "$FORMAT" in
    --jsonl)
        emit_jsonl "$@"
        ;;
    --table|*)
        {
            printf "COMMIT\tPLAN_ID\tMETHOD\tWINNER\tSCORES\tMODEL\tCODEX_TOKENS\n"
            emit_jsonl "$@" | jq -r '[.commit, (.plan_id//"-"), (.method//"-"), (.winner//"-"), (.scores//"-"), (.model//"-"), (.codex_total_tokens//"-")] | @tsv'
        } | column -t -s $'\t'
        ;;
esac
