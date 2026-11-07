#!/usr/bin/env bash
# run_conformance.sh — roadmap P2 micro-conformance suite for rush.
#
# Each case is a TINY generated workspace exercising ONE Bazel semantic, with an
# expected outcome (success | fail). A faithful Bazel replacement must ACCEPT
# what Bazel accepts and REJECT what Bazel rejects (analysis/loading errors).
# "463/463 passing" only proves rush builds valid graphs; this proves it also
# ENFORCES the rules — visibility, package boundaries, undefined deps, etc. Any
# mismatch is a DIVERGENCE (usually rush being too permissive, like the
# no-sandbox hermeticity gap).
#
# Usage: run_conformance.sh    (RUSH=/data/rush/target/release/rush)
set -u
RUSH="${RUSH:-/data/rush/target/release/rush}"
ROOT="$(mktemp -d)"; CONFORM=0; DIVERGE=0
trap 'rm -rf "$ROOT"' EXIT

# run_case <name> <expected:success|fail> <target> <setup-fn> [rush-build-args...]
run_case() {
    local name="$1" exp="$2" tgt="$3" setup="$4"
    shift 4
    local ws="$ROOT/$name"; mkdir -p "$ws"; ( cd "$ws"; echo 'workspace(name="conf")' > WORKSPACE; "$setup" )
    # timeout guards against a semantic that makes rush HANG (e.g. a dep cycle)
    ( cd "$ws"; rm -rf rush-out 2>/dev/null; timeout 90 "$RUSH" build "$@" "$tgt" > "$ws/.log" 2>&1 ); local rc=$?
    [ $rc -eq 124 ] && echo "    (note: $name TIMED OUT — rush hung where Bazel fails fast)"
    local got; [ $rc -eq 0 ] && got=success || got=fail
    if [ "$got" = "$exp" ]; then
        echo "  CONFORM  $name (expected=$exp, rush=$got)"; CONFORM=$((CONFORM+1))
    else
        local why="rush too STRICT"; [ "$exp" = fail ] && why="rush too PERMISSIVE — accepted what Bazel rejects"
        echo "  DIVERGE  $name (expected=$exp, rush=$got) — $why"; DIVERGE=$((DIVERGE+1))
    fi
}

# --- C1: positive control — a valid cc_binary must build ---
c_control() {
    mkdir -p p; echo 'int main(){return 0;}' > p/m.cc
    printf 'cc_binary(name="hi", srcs=["m.cc"])\n' > p/BUILD
}
run_case control success //p:hi c_control

# --- C2: visibility — a //visibility:private lib referenced cross-package must FAIL ---
c_vis_private() {
    mkdir -p lib app
    echo 'int secret(){return 7;}' > lib/s.cc
    printf 'cc_library(name="secret", srcs=["s.cc"], visibility=["//visibility:private"])\n' > lib/BUILD
    echo 'int secret(); int main(){return secret();}' > app/m.cc
    printf 'cc_binary(name="app", srcs=["m.cc"], deps=["//lib:secret"])\n' > app/BUILD
}
run_case visibility_private_violation fail //app:app c_vis_private

# --- C3: visibility public control — same shape but public, must BUILD ---
c_vis_public() {
    mkdir -p lib app
    echo 'int ok(){return 0;}' > lib/s.cc
    printf 'cc_library(name="pub", srcs=["s.cc"], visibility=["//visibility:public"])\n' > lib/BUILD
    echo 'int ok(); int main(){return ok();}' > app/m.cc
    printf 'cc_binary(name="app", srcs=["m.cc"], deps=["//lib:pub"])\n' > app/BUILD
}
run_case visibility_public_ok success //app:app c_vis_public

# --- C4: package boundary — a target globbing a file owned by a SUB-package must FAIL ---
c_subpkg() {
    mkdir -p a/sub
    echo 'int f(){return 0;}' > a/sub/f.cc
    printf 'filegroup(name="sub")\n' > a/sub/BUILD          # makes //a/sub a package
    echo 'int g(){return 0;}' > a/g.cc
    # srcs reaches across into the sub-package's file → Bazel: "label crosses package boundary"
    printf 'cc_library(name="x", srcs=["g.cc", "sub/f.cc"])\n' > a/BUILD
}
run_case subpackage_boundary_violation fail //a:x c_subpkg

# --- C5: undefined dep — deps on a nonexistent label must FAIL ---
c_undef_dep() {
    mkdir -p p; echo 'int main(){return 0;}' > p/m.cc
    printf 'cc_binary(name="u", srcs=["m.cc"], deps=["//nope:nothing"])\n' > p/BUILD
}
run_case undefined_dep fail //p:u c_undef_dep

# --- C6: missing source — srcs names a file that does not exist must FAIL ---
c_missing_src() {
    mkdir -p p
    printf 'cc_binary(name="m", srcs=["does_not_exist.cc"])\n' > p/BUILD
}
run_case missing_source fail //p:m c_missing_src

# --- C7: duplicate target name in one BUILD must FAIL ---
c_dup_name() {
    mkdir -p p; echo 'int main(){return 0;}' > p/m.cc
    printf 'cc_binary(name="x", srcs=["m.cc"])\ncc_library(name="x", srcs=["m.cc"])\n' > p/BUILD
}
run_case duplicate_target_name fail //p:x c_dup_name

# --- C8: dependency cycle a<->b must FAIL (not hang) ---
c_cycle() {
    mkdir -p p
    echo 'int a(){return 0;}' > p/a.cc; echo 'int b(){return 0;}' > p/b.cc
    printf 'cc_library(name="a", srcs=["a.cc"], deps=[":b"])\ncc_library(name="b", srcs=["b.cc"], deps=[":a"])\n' > p/BUILD
}
run_case dependency_cycle fail //p:a c_cycle

# --- C9: two genrules producing the SAME output must FAIL (conflicting actions) ---
c_out_conflict() {
    mkdir -p p
    printf 'genrule(name="g1", outs=["shared.txt"], cmd="echo one > $@")\ngenrule(name="g2", outs=["shared.txt"], cmd="echo two > $@")\n' > p/BUILD
}
run_case output_conflict fail //p:g1 c_out_conflict

# --- C10: genrule $(location) make-var must expand + build (success control) ---
c_genrule_loc() {
    mkdir -p p; echo 'hello' > p/in.txt
    printf 'genrule(name="g", srcs=["in.txt"], outs=["out.txt"], cmd="cat $(location in.txt) > $@")\n' > p/BUILD
}
run_case genrule_location success //p:g c_genrule_loc

# --- C11: select() with //conditions:default must build (success) ---
c_select_default() {
    mkdir -p p; echo 'int main(){return 0;}' > p/m.cc
    printf 'cc_binary(name="b", srcs=["m.cc"], copts=select({"//conditions:default":["-DOK=1"]}))\n' > p/BUILD
}
run_case select_default success //p:b c_select_default

# --- C12: select() whose only condition can't match + NO default must FAIL ---
c_select_nomatch() {
    mkdir -p p; echo 'int main(){return 0;}' > p/m.cc
    printf 'config_setting(name="never", values={"define":"NEVER_SET=1"})\ncc_binary(name="b", srcs=["m.cc"], copts=select({":never":["-DX"]}))\n' > p/BUILD
}
run_case select_no_match fail //p:b c_select_nomatch --analyze-only

# --- C13: a header listed in srcs (private header) is allowed (success) ---
c_hdr_in_srcs() {
    mkdir -p p
    printf '#pragma once\nint v();\n' > p/h.h
    printf '#include "h.h"\nint v(){return 0;}\nint main(){return v();}\n' > p/m.cc
    printf 'cc_binary(name="b", srcs=["m.cc", "h.h"])\n' > p/BUILD
}
run_case header_in_srcs success //p:b c_hdr_in_srcs

# --- C14: alwayslink=1 library is accepted + builds (success) ---
c_alwayslink() {
    mkdir -p p
    echo 'int f(){return 0;}' > p/l.cc; echo 'int main(){return 0;}' > p/m.cc
    printf 'cc_library(name="lib", srcs=["l.cc"], alwayslink=1)\ncc_binary(name="b", srcs=["m.cc"], deps=[":lib"])\n' > p/BUILD
}
run_case alwayslink success //p:b c_alwayslink

# --- C15: a basic cc_test target builds (success) ---
c_cc_test() {
    mkdir -p p; printf 'int main(){return 0;}\n' > p/t.cc
    printf 'cc_test(name="t", srcs=["t.cc"])\n' > p/BUILD
}
run_case cc_test_basic success //p:t c_cc_test

# --- C16: a generated header (genrule output) used by a cc_binary in srcs must
#     build — Bazel puts the genfiles dir on the compile include path (success) ---
c_genhdr() {
    mkdir -p p
    printf '#include "gen.h"\nint main(){return 0;}\n' > p/m.cc
    printf 'genrule(name="genh", outs=["gen.h"], cmd="echo // > $@")\ncc_binary(name="useh", srcs=["m.cc", "gen.h"])\n' > p/BUILD
}
run_case generated_header_include success //p:useh c_genhdr

echo "=== conformance: $CONFORM conform / $DIVERGE diverge (of $((CONFORM+DIVERGE))) [default mode] ==="
exit $([ "$DIVERGE" -eq 0 ] && echo 0 || echo 1)
