#!/usr/bin/env bash
# Materialize TensorFlow's pinned BoringSSL source for Rush.
#
# tsl/platform/cloud (oauth_client, google_auth_provider) calls EVP_*, RSA_*
# and BIO_* from @boringssl//:crypto. TF pins the source and BoringSSL ships
# its own BUILD, so rush compiles it like Bazel does rather than linking the
# host's OpenSSL, whose ABI and API differ from the pinned snapshot.
#
# Without this the repo is absent, rush stubs @boringssl//:crypto, and the
# build dies hundreds of archives later on `undefined reference to
# EVP_PKEY_free` with nothing naming BoringSSL.
set -euo pipefail

# From tensorflow/workspace2.bzl (TF 2.15).
BORINGSSL_COMMIT="${BORINGSSL_COMMIT:-c00d7ca810e93780bd0c8ee4eea28f4f2ea4bcdc}"
BORINGSSL_SHA256="${BORINGSSL_SHA256:-9dc53f851107eaf87b391136d13b815df97ec8f76dadb487b58b2fc45e624d2c}"
BORINGSSL_URL="https://github.com/google/boringssl/archive/${BORINGSSL_COMMIT}.tar.gz"
BORINGSSL_MIRROR_URL="https://storage.googleapis.com/mirror.tensorflow.org/github.com/google/boringssl/archive/${BORINGSSL_COMMIT}.tar.gz"
CACHE_ROOT="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
DEST="${CACHE_ROOT}/boringssl"
STAMP="${DEST}/.rush-bootstrap-commit"

MODE=""
FORCE=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) MODE="dry-run"; shift ;;
        --apply) MODE="apply"; shift ;;
        --force) FORCE=1; shift ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        *) echo "error: unknown arg: $1" >&2; exit 2 ;;
    esac
done
[[ -n "$MODE" ]] || { echo "error: must specify --dry-run or --apply" >&2; exit 2; }

cat <<EOF
=== bootstrap_boringssl plan ===
commit      : ${BORINGSSL_COMMIT}
sha256      : ${BORINGSSL_SHA256}
destination : ${DEST}
EOF

if [[ -f "$STAMP" ]] && [[ "$(cat "$STAMP")" == "$BORINGSSL_COMMIT" ]] && [[ $FORCE -eq 0 ]]; then
    echo "boringssl: already materialized at ${BORINGSSL_COMMIT}"
    exit 0
fi
[[ "$MODE" == "apply" ]] || { echo "(dry-run; re-run with --apply)"; exit 0; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fetched=""
for url in "$BORINGSSL_MIRROR_URL" "$BORINGSSL_URL"; do
    echo "boringssl: fetching ${url}"
    if curl -sSL --fail -o "$WORK/boringssl.tar.gz" "$url"; then fetched="$url"; break; fi
    echo "boringssl: fetch failed, trying next mirror" >&2
done
[[ -n "$fetched" ]] || { echo "ERROR: could not fetch BoringSSL" >&2; exit 1; }

actual="$(sha256sum "$WORK/boringssl.tar.gz" | awk '{print $1}')"
[[ "$actual" == "$BORINGSSL_SHA256" ]] || {
    echo "ERROR: sha256 mismatch for ${fetched}" >&2
    echo "  expected ${BORINGSSL_SHA256}" >&2
    echo "  actual   ${actual}" >&2
    exit 1
}
echo "boringssl: sha256 matches TensorFlow's pin"

tar xzf "$WORK/boringssl.tar.gz" -C "$WORK"
SRC="$WORK/boringssl-${BORINGSSL_COMMIT}"
[[ -f "$SRC/BUILD" ]] || { echo "ERROR: ${SRC}/BUILD missing" >&2; exit 1; }

mkdir -p "$(dirname "$DEST")"
rm -rf "$DEST"
mv "$SRC" "$DEST"
printf '%s\n' "$BORINGSSL_COMMIT" > "$STAMP"
echo "boringssl: materialized ${BORINGSSL_COMMIT} in ${DEST}"
