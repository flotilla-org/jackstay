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
# Stage only the dylib so stale archives from older builds cannot be selected.
cp "${CARGO_TARGET_DIR:-target}/debug/libjackstay.dylib" "$swift_smoke_dir/"
if ! otool -D "$swift_smoke_dir/libjackstay.dylib" | grep -Fx '@rpath/libjackstay.dylib' >/dev/null; then
  echo "Jackstay dylib must have install name @rpath/libjackstay.dylib" >&2
  exit 1
fi
swiftc scripts/swift/writer.swift \
  -I crates/jackstay/include -L "$swift_smoke_dir" -ljackstay \
  -Xlinker -rpath -Xlinker @executable_path \
  -module-cache-path "$swift_smoke_dir/module-cache" \
  -o "$swift_smoke_dir/writer"
if ! otool -L "$swift_smoke_dir/writer" | grep -F '@rpath/libjackstay.dylib (' >/dev/null; then
  echo "Swift smoke must link @rpath/libjackstay.dylib" >&2
  exit 1
fi
if ! otool -l "$swift_smoke_dir/writer" | grep -F 'path @executable_path (offset ' >/dev/null; then
  echo "Swift smoke must have an @executable_path rpath" >&2
  exit 1
fi
env -u DYLD_LIBRARY_PATH -u DYLD_FALLBACK_LIBRARY_PATH \
  "${CARGO_TARGET_DIR:-target}/debug/examples/swift_writer_export" "$swift_smoke_dir/writer"
echo "Swift dylib: loaded from beside the executable through @executable_path"

# A missing packaged library must fail at dynamic loading, before Swift main.
mv "$swift_smoke_dir/libjackstay.dylib" "$swift_smoke_dir/libjackstay.dylib.hidden"
if { env -u DYLD_LIBRARY_PATH -u DYLD_FALLBACK_LIBRARY_PATH \
  "$swift_smoke_dir/writer" </dev/null; } > "$swift_smoke_dir/missing-dylib.log" 2>&1; then
  echo "Swift smoke ran without its packaged Jackstay dylib" >&2
  exit 1
fi
if ! grep -Fq 'Library not loaded: @rpath/libjackstay.dylib' "$swift_smoke_dir/missing-dylib.log"; then
  cat "$swift_smoke_dir/missing-dylib.log" >&2
  echo "Swift missing-dylib check failed for an unexpected reason" >&2
  exit 1
fi
mv "$swift_smoke_dir/libjackstay.dylib.hidden" "$swift_smoke_dir/libjackstay.dylib"
echo "Swift dylib mutation: missing library rejected with a dyld diagnostic"

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
