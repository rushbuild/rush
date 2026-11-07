#!/bin/bash
# Separates two distinct staleness bugs.
#
# BUG A (change detection): editing a genrule `srcs` file is not noticed at all.
# BUG B (missing input):    even when the build DOES run and the generated header
#                           changes on disk, the compile of a same-target source
#                           that #includes it is NOT invalidated.
#
# `ticker.cc` belongs to an unrelated target and is edited every round purely to
# force rush past the no-op certificate, so BUG B can be observed on its own.
set -u
S=${RUSH_REGRESSION_TMP:-$(mktemp -d)}
RUSH=${RUSH:?set RUSH to the rush binary under test}
ROOT=$S/hdrinput; rm -rf "$ROOT"; mkdir -p "$ROOT"
W=$ROOT/ws; mkdir -p "$W"

cat > "$W/WORKSPACE" <<'EOF'
workspace(name = "hdrinput")
EOF
cat > "$W/BUILD" <<'EOF'
genrule(
    name = "gen_hdr",
    srcs = ["seed.txt"],
    outs = ["gen_value.h"],
    cmd = "sed 's/VALUE /#define GEN_VALUE /' $(SRCS) > $@",
)

cc_library(
    name = "core",
    srcs = ["core.cc"],
    hdrs = ["core.h", ":gen_hdr"],
)

cc_library(
    name = "ticker",
    srcs = ["ticker.cc"],
    hdrs = ["ticker.h"],
)

cc_binary(
    name = "app",
    srcs = ["main.cc"],
    deps = [":core", ":ticker"],
)
EOF
cat > "$W/core.h" <<'EOF'
#pragma once
int core_value();
EOF
cat > "$W/core.cc" <<'EOF'
#include "core.h"
#include "gen_value.h"
int core_value() { return GEN_VALUE; }
EOF
cat > "$W/ticker.h" <<'EOF'
#pragma once
int ticker();
EOF
cat > "$W/main.cc" <<'EOF'
#include <cstdio>
#include "core.h"
#include "ticker.h"
int main() { printf("%d\n", core_value() + ticker() * 0); return 0; }
EOF

round() { # $1 label, $2 seed value, $3 ticker value, $4 expected
  echo "VALUE $2" > "$W/seed.txt"
  printf '#include "ticker.h"\nint ticker() { return %s; }\n' "$3" > "$W/ticker.cc"
  ( cd "$W" && RUSH_ACTION_TRACE="$ROOT/$1.trace" \
      "$RUSH" build --daemon off //:app > "$ROOT/$1.log" 2>&1 )
  local hdr; hdr=$(sed -n 's/.*GEN_VALUE //p' "$W/rush-out/genfiles/gen_value.h" 2>/dev/null)
  local got; got=$("$W/rush-out/app" 2>/dev/null)
  local core_state="HIT(not recompiled)"
  grep -q "MISS.*core2.cc.o" "$ROOT/$1.trace" 2>/dev/null && core_state="MISS(recompiled)"
  local v="OK"; [ "$got" != "$4" ] && v="*** STALE, expected $4 ***"
  printf '  %-10s gen_value.h=%-4s core.cc compile=%-20s app=%-5s %s\n' \
     "$1" "$hdr" "$core_state" "$got" "$v"
}

echo "Each round edits BOTH seed.txt (genrule input) and ticker.cc (unrelated,"
echo "always detected) so the build always runs. Only the header value changes"
echo "what app should print."
echo
round r1 10 1 10
round r2 20 2 20
round r3 30 3 30
echo
echo "=== CONTROL: identical sources, fresh out-dir + fresh HOME cache ==="
rm -rf "$W/rush-out" "$W/.cache"
( cd "$W" && HOME=$ROOT/fakehome RUSH_ACTION_TRACE="$ROOT/ctl.trace" \
    "$RUSH" build --daemon off //:app > "$ROOT/ctl.log" 2>&1 )
echo "  control    app=$("$W/rush-out/app" 2>/dev/null)  (expected 30)"
