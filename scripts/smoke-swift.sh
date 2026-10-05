#!/usr/bin/env bash
set -euo pipefail
jackstay_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$jackstay_root"
if [[ "$(uname -s)" != Darwin ]]; then
  echo "The Swift smoke requires macOS and the Xcode command line tools" >&2
  exit 1
fi
cargo build --locked -p jackstay --lib --example swift_writer_export
swift_smoke_dir="$(mktemp -d)"
trap 'rm -rf "$swift_smoke_dir"' EXIT
# -ljackstay must resolve to the archive, even when Cargo also builds a dylib.
cp "${CARGO_TARGET_DIR:-target}/debug/libjackstay.a" "$swift_smoke_dir/"
swiftc scripts/swift/writer.swift \
  -I crates/jackstay/include -L "$swift_smoke_dir" -ljackstay \
  -module-cache-path "$swift_smoke_dir/module-cache" \
  -o "$swift_smoke_dir/writer"
if otool -L "$swift_smoke_dir/writer" | grep -q 'libjackstay.*dylib'; then
  echo "Swift smoke linked a Jackstay dylib instead of the static archive" >&2
  exit 1
fi
"${CARGO_TARGET_DIR:-target}/debug/examples/swift_writer_export" "$swift_smoke_dir/writer"

# Mutation check: a broken transitive include must fail even with a warm cache
# from the successful import above. Mutate only a disposable header copy.
cp -R crates/jackstay/include "$swift_smoke_dir/broken-include"
sed 's/"jackstay_input.h"/"jackstay_missing_header.h"/' \
  crates/jackstay/include/jackstay_bootstrap.h \
  > "$swift_smoke_dir/broken-include/jackstay_bootstrap.h"
if swiftc -typecheck scripts/swift/writer.swift \
  -I "$swift_smoke_dir/broken-include" \
  -module-cache-path "$swift_smoke_dir/module-cache" \
  > "$swift_smoke_dir/mutation.log" 2>&1; then
  echo "Swift imported a module with a broken public header include" >&2
  exit 1
fi
if ! grep -q "'jackstay_missing_header.h' file not found" "$swift_smoke_dir/mutation.log"; then
  cat "$swift_smoke_dir/mutation.log" >&2
  echo "Swift mutation failed for an unexpected reason" >&2
  exit 1
fi
echo "Swift header mutation: broken include rejected"
