#!/bin/bash
# Which source edits does `rush build` actually notice?
# For each file, mutate it, rebuild, and report whether ANY action ran and
# whether the produced binary is correct.
set -u
S=${RUSH_REGRESSION_TMP:-$(mktemp -d)}
RUSH=${RUSH:?set RUSH to the rush binary under test}
ROOT=$S/sens
rm -rf "$ROOT"; mkdir -p "$ROOT"
W=$ROOT/ws; mkdir -p "$W"

cat > "$W/WORKSPACE" <<'EOF'
workspace(name = "sens")
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

cc_binary(
    name = "app",
    srcs = ["main.cc"],
    deps = [":core"],
)
EOF
cat > "$W/seed.txt" <<'EOF'
VALUE 100
EOF
cat > "$W/core.h" <<'EOF'
#pragma once
#define BONUS 0
int core_value();
EOF
cat > "$W/core.cc" <<'EOF'
#include "core.h"
#include "gen_value.h"
int core_value() { return GEN_VALUE + BONUS; }
EOF
cat > "$W/main.cc" <<'EOF'
#include <cstdio>
#include "core.h"
int main() { printf("%d\n", core_value()); return 0; }
EOF

build() {
  ( cd "$W" && RUSH_ACTION_TRACE="$ROOT/$1.trace" \
      "$RUSH" build --daemon off //:app > "$ROOT/$1.log" 2>&1 )
}

report() { # $1 label, $2 expected
  local n="<none>"
  [ -f "$ROOT/$1.trace" ] && n=$(wc -l < "$ROOT/$1.trace" | tr -d ' ')
  local got; got=$("$W/rush-out/app" 2>/dev/null)
  local verdict="OK"
  [ "$got" != "$2" ] && verdict="*** STALE (expected $2) ***"
  printf '  %-34s actions_run=%-7s app=%-6s %s\n' "$1" "$n" "$got" "$verdict"
}

echo "=== baseline cold build (expect 100) ==="
build baseline; report baseline 100

echo
echo "=== edit genrule srcs input (seed.txt): 100 -> 200, expect 200 ==="
echo "VALUE 200" > "$W/seed.txt"; build edit_seed; report edit_seed 200

echo
echo "=== edit regular header (core.h) BONUS 0 -> 7, expect 207 ==="
printf '#pragma once\n#define BONUS 7\nint core_value();\n' > "$W/core.h"
build edit_hdr; report edit_hdr 207

echo
echo "=== edit a .cc source (core.cc): +1, expect 208 ==="
printf '#include "core.h"\n#include "gen_value.h"\nint core_value() { return GEN_VALUE + BONUS + 1; }\n' > "$W/core.cc"
build edit_cc; report edit_cc 208

echo
echo "=== edit genrule srcs AGAIN: 200 -> 300, expect 308 ==="
echo "VALUE 300" > "$W/seed.txt"; build edit_seed2; report edit_seed2 308

echo
echo "=== edit the BUILD file (genrule cmd adds +0 via comment) ==="
echo
echo "=== FINAL CONTROL: same sources, fresh out-dir, fresh HOME cache ==="
rm -rf "$W/rush-out" "$W/.cache"
HOME=$ROOT/fakehome build control
report control 308
