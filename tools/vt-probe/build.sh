#!/bin/sh
# Builds tools/vt-probe into build/vt-probe using the system Swift toolchain.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
mkdir -p "$root/build"
xcrun swiftc -O "$here/main.swift" -o "$root/build/vt-probe"
echo "built $root/build/vt-probe"
