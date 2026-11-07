#!/usr/bin/env bash
# hermeticity_probe.sh — roadmap P1 hermeticity negative for rush.
#
# A faithful Bazel replacement sandboxes each action so it can read ONLY its
# declared inputs. This probe builds a genrule that reads an UNDECLARED file:
#   - sandboxed Bazel FAILS (the undeclared file isn't in the action sandbox)
#   - a non-hermetic builder SUCCEEDS by reading the on-disk file directly
# Reports HERMETIC / NON-HERMETIC. As of 2026-06-20 rush is NON-HERMETIC
# (no action sandboxing) — this script documents + re-checks that gap.
#
# Env: RUSH=/data/rush/target/release/rush
set -u
RUSH="${RUSH:-/data/rush/target/release/rush}"
WS="$(mktemp -d)"; cd "$WS" || exit 2
cat > WORKSPACE <<'EOF'
workspace(name = "herm_probe")
EOF
echo "DECLARED-content" > declared.txt
echo "UNDECLARED-SECRET-$$" > undeclared.txt
SECRET="UNDECLARED-SECRET-$$"
cat > BUILD <<'EOF'
# genrule reads undeclared.txt, which is NOT in srcs. Sandboxed Bazel fails this.
genrule(
    name = "leak",
    srcs = ["declared.txt"],
    outs = ["out.txt"],
    cmd = "cat $(location declared.txt) undeclared.txt > $@",
)
EOF
sed -i "s/UNDECLARED-SECRET-\$\$/$SECRET/" undeclared.txt 2>/dev/null
echo "=== hermeticity probe: genrule //:leak reads undeclared.txt ==="
"$RUSH" build //:leak > "$WS/build.log" 2>&1; BEX=$?
OUT="$(find rush-out -name out.txt -exec cat {} \; 2>/dev/null)"
echo "build exit=$BEX, out.txt=[$OUT]"
if echo "$OUT" | grep -q "$SECRET"; then
  echo "RESULT: NON-HERMETIC — rush read the undeclared input (no sandbox); Bazel would FAIL this. (known gap, roadmap P1)"
  RC=1
elif [ "$BEX" -ne 0 ]; then
  echo "RESULT: HERMETIC — rush rejected the undeclared-input action."
  RC=0
else
  echo "RESULT: INCONCLUSIVE — build succeeded but secret not observed."
  RC=2
fi
rm -rf "$WS"
exit $RC
