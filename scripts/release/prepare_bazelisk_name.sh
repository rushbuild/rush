#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
    echo "usage: $0 <vX.Y.Z-rc.N> <os> <arch>" >&2
    exit 2
fi

requested_version=$1
bazel_os=$2
bazel_arch=$3

if [[ ! $requested_version =~ ^v[0-9]+\.[0-9]+\.[0-9]+-rc\.[1-9][0-9]*$ ]]; then
    echo "version must match vX.Y.Z-rc.N with N greater than zero" >&2
    exit 2
fi

if [[ $bazel_os != linux || $bazel_arch != x86_64 ]]; then
    echo "unsupported release target: $bazel_os/$bazel_arch" >&2
    exit 2
fi

version=${requested_version#v}
printf 'version=%s\n' "$version"
printf 'binary_name=bazel-%s-%s-%s\n' "$version" "$bazel_os" "$bazel_arch"
