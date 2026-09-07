#!/usr/bin/env bash
set -euo pipefail
jackstay_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$jackstay_root"
case "$(uname -s)" in
  Darwin) cargo build --workspace --locked --features backend-macos ;;
  Linux) cargo build --workspace --locked ;;
  *) echo "SDL reference viewer supports macOS and Linux" >&2; exit 1 ;;
esac
cmake -S tools/capture-viewer-sdl -B build/viewer
cmake --build build/viewer
# Set SDL_VIDEODRIVER=dummy for an offline C-ABI/render smoke in CI.
# Leave it unset to see the generated frames in a normal desktop window.
./build/viewer/capture-viewer-sdl --frames 30
