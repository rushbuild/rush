#!/usr/bin/env bash
# Materialize TF's pinned linenoise and pybind11 sources for Rush.
#
# Both repos have no system package to adapt to on typical hosts:
# linenoise is a two-file C library TF builds from source, and pybind11
# is header-only at a pinned version. Without the sources rush cannot
# declare their primary targets honestly, and the stub-label audit
# reports @linenoise//:linenoise and @pybind11//:pybind11 as gaps.
#
# Pins come from tensorflow/workspace2.bzl (TF 2.15).
set -euo pipefail

CACHE_ROOT="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"

fetch() (
    local name="$1" url="$2" sha="$3" strip="$4" witness="$5"
    local dest="${CACHE_ROOT}/${name}"
    local stamp="${dest}/.rush-bootstrap-sha256"
    if [[ -f "$stamp" && "$(cat "$stamp")" == "$sha" &&
          -f "${dest}/${witness}" ]]; then
        echo "${name}: already materialized (${sha:0:12})"
        return 0
    fi
    local tmp
    mkdir -p "$CACHE_ROOT"
    tmp="$(mktemp -d "${CACHE_ROOT}/.rush-bootstrap-${name}.XXXXXX")"
    trap 'rm -rf "$tmp"' EXIT
    echo "${name}: fetching ${url}"
    curl -fsSL "$url" -o "${tmp}/a.tar.gz"
    local got
    got="$(sha256sum "${tmp}/a.tar.gz" | awk '{print $1}')"
    if [[ "$got" != "$sha" ]]; then
        echo "${name}: sha256 mismatch: got ${got}, want ${sha}" >&2
        return 1
    fi
    tar -xzf "${tmp}/a.tar.gz" -C "$tmp"
    if [[ ! -f "${tmp}/${strip}/${witness}" ]]; then
        echo "${name}: archive lacks required file ${strip}/${witness}" >&2
        return 1
    fi
    echo "$sha" > "${tmp}/${strip}/.rush-bootstrap-sha256"

    if [[ -e "$dest" || -L "$dest" ]]; then
        local previous="${tmp}/previous"
        mv -T "$dest" "$previous"
        if mv -T "${tmp}/${strip}" "$dest"; then
            :
        else
            local publish_status=$?
            if ! mv -T "$previous" "$dest"; then
                trap - EXIT
                echo "${name}: publish and rollback failed; previous clone remains at ${previous}" >&2
            fi
            return "$publish_status"
        fi
    else
        mv -T "${tmp}/${strip}" "$dest"
    fi
    echo "${name}: materialized at ${dest}"
)

fetch linenoise \
    "https://github.com/antirez/linenoise/archive/4ce393a66b10903a0ef52edf9775ed526a17395f.tar.gz" \
    "b35a74dbc9cd2fef9e4d56222761d61daf7e551510e6cd1a86f0789b548d074e" \
    "linenoise-4ce393a66b10903a0ef52edf9775ed526a17395f" \
    "linenoise.c"

fetch pybind11 \
    "https://github.com/pybind/pybind11/archive/v2.10.4.tar.gz" \
    "832e2f309c57da9c1e6d4542dedd34b24e4192ecb4d62f6f4866a737454c9970" \
    "pybind11-2.10.4" \
    "include/pybind11/pybind11.h"
