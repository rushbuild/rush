#!/usr/bin/env bash
set -euo pipefail

script=$(dirname "$0")/prepare_bazelisk_name.sh

expected=$(cat <<'EOF'
version=0.1.0-rc.1
binary_name=bazel-0.1.0-rc.1-linux-x86_64
EOF
)
actual=$($script v0.1.0-rc.1 linux x86_64)
test "$actual" = "$expected"

expect_failure() {
    if "$script" "$@" >/dev/null 2>&1; then
        echo "unexpected success: $*" >&2
        exit 1
    fi
}

expect_failure v0.1.0 linux x86_64
expect_failure 0.1.0-rc.1 linux x86_64
expect_failure v0.1.0-rc.0 linux x86_64
expect_failure v0.1.0-beta.1 linux x86_64
expect_failure v0.1.0-rc.1 darwin x86_64
expect_failure v0.1.0-rc.1 linux arm64
expect_failure v0.1.0-rc.1 linux

echo "release candidate identity tests passed"
