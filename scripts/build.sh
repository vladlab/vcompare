#!/usr/bin/env bash
#
# build.sh — Build the vcompare AppImage / deb.
#
# Prerequisites:
#   - npm install
#   - Rust + Tauri CLI  (cargo install tauri-cli --version "^2.0")
#
# Usage:
#   ./scripts/build.sh
#
# Output:
#   src-tauri/target/release/bundle/appimage/vcompare_*.AppImage
#   src-tauri/target/release/bundle/deb/vcompare_*.deb

set -euo pipefail
cd "$(dirname "$0")/.."

echo "► Building Tauri application..."
cargo tauri build

echo ""
APPIMAGE=$(find src-tauri/target/release/bundle/appimage -name '*.AppImage' 2>/dev/null | head -1)
DEB=$(find src-tauri/target/release/bundle/deb -name '*.deb' 2>/dev/null | head -1)

if [ -n "$APPIMAGE" ]; then
    echo "  AppImage: $APPIMAGE ($(du -h "$APPIMAGE" | cut -f1))"
fi
if [ -n "$DEB" ]; then
    echo "  Deb:      $DEB ($(du -h "$DEB" | cut -f1))"
fi
