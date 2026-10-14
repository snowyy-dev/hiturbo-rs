#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "Build the macOS app on a Mac." >&2
    exit 1
fi
if [[ $# -gt 1 ]]; then
    echo "Usage: $0 [aarch64-apple-darwin|x86_64-apple-darwin]" >&2
    exit 1
fi
if ! command -v cmake >/dev/null; then
    echo "CMake is required to build bundled SDL2. Install it with: brew install cmake" >&2
    exit 1
fi

hiturbo_project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$hiturbo_project_dir"
hiturbo_target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
case "$hiturbo_target" in
    aarch64-apple-darwin|x86_64-apple-darwin) ;;
    *) echo "Unsupported macOS target: $hiturbo_target" >&2; exit 1 ;;
esac

# Match the bundle's minimum system version for Rust and bundled SDL.
export MACOSX_DEPLOYMENT_TARGET=11.0
cargo build --release --locked --target "$hiturbo_target" --target-dir "$hiturbo_project_dir/target"
hiturbo_bundle="$hiturbo_project_dir/target/macos/$hiturbo_target/Hiturbo.app"
mkdir -p "$hiturbo_bundle/Contents/MacOS" "$hiturbo_bundle/Contents/Resources"
cp "target/$hiturbo_target/release/hiturbo-rs" "$hiturbo_bundle/Contents/MacOS/hiturbo-rs"
cp macos/Info.plist "$hiturbo_bundle/Contents/Info.plist"
cp LICENSE "$hiturbo_bundle/Contents/Resources/LICENSE"
plutil -lint "$hiturbo_bundle/Contents/Info.plist"

# Ad-hoc signing supports local use. Supply an Apple Developer identity when
# preparing a release for distribution; notarization is a separate release step.
codesign --force --sign "${HITURBO_CODESIGN_IDENTITY:--}" "$hiturbo_bundle"
codesign --verify --strict "$hiturbo_bundle"
echo "Built $hiturbo_bundle"
echo "Open it in Finder, or run: open \"$hiturbo_bundle\""
