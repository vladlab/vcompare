#!/usr/bin/env bash
#
# build.sh — Build the vcompare AppImage.
#
# Steps:
#   1. Compile Python sidecar → single binary via PyInstaller
#   2. Place binary at src-tauri/binaries/ with target triple suffix
#   3. Build Tauri (frontend + Rust + bundle)
#
# Prerequisites:
#   - pip install pyinstaller  (in same Python env as your vcompare deps)
#   - npm install              (already done if you ran cargo tauri dev)
#   - Rust + Tauri CLI         (cargo install tauri-cli)
#
# Usage:
#   ./scripts/build.sh
#
# Output:
#   src-tauri/target/release/bundle/appimage/vcompare_*.AppImage

set -euo pipefail
cd "$(dirname "$0")/.."

TRIPLE=$(rustc -vV | grep '^host:' | awk '{print $2}')
BINDIR="src-tauri/binaries"
SIDECAR_DIR="sidecar"

echo "═══════════════════════════════════════════"
echo "  vcompare build"
echo "  target: ${TRIPLE}"
echo "═══════════════════════════════════════════"

# ── Step 1: Compile Python sidecar ──────────────────────────────────────────

echo ""
echo "► Compiling Python sidecar with PyInstaller..."

# Ensure pyinstaller is available
if ! command -v pyinstaller &>/dev/null; then
    echo "ERROR: pyinstaller not found. Install with: pip install pyinstaller"
    exit 1
fi

# Check that vcompare.py exists in sidecar/
if [ ! -f "$SIDECAR_DIR/vcompare.py" ]; then
    echo "ERROR: sidecar/vcompare.py not found."
    echo "Copy your vcompare.py into the sidecar/ directory first."
    exit 1
fi

# Run PyInstaller
# --onefile: single binary
# --name: output binary name
# --distpath: where to put the result
# --workpath/--specpath: keep build artifacts out of the source tree
pyinstaller \
    --onefile \
    --name "vcompare_engine" \
    --distpath "$SIDECAR_DIR/dist" \
    --workpath "/tmp/vcompare_pyinstaller_build" \
    --specpath "/tmp/vcompare_pyinstaller_build" \
    --clean \
    --noconfirm \
    --log-level WARN \
    "$SIDECAR_DIR/vcompare_engine.py"

COMPILED_BIN="$SIDECAR_DIR/dist/vcompare_engine"

if [ ! -f "$COMPILED_BIN" ]; then
    echo "ERROR: PyInstaller did not produce $COMPILED_BIN"
    exit 1
fi

# Check it runs
echo -n "  Verifying sidecar binary... "
PING='{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}'
RESPONSE=$(echo "$PING" | timeout 10 "$COMPILED_BIN" 2>/dev/null | head -1)
if echo "$RESPONSE" | grep -q '"status":"ok"'; then
    echo "OK"
else
    echo "WARNING: ping check failed (may still work)"
    echo "  Response: $RESPONSE"
fi

# ── Step 2: Place binary for Tauri bundler ──────────────────────────────────

echo ""
echo "► Installing sidecar binary for Tauri..."

mkdir -p "$BINDIR"
cp "$COMPILED_BIN" "$BINDIR/vcompare_engine-${TRIPLE}"
chmod +x "$BINDIR/vcompare_engine-${TRIPLE}"
echo "  → $BINDIR/vcompare_engine-${TRIPLE}"

SIZE=$(du -h "$BINDIR/vcompare_engine-${TRIPLE}" | cut -f1)
echo "  Size: $SIZE"

# ── Step 3: Build Tauri ─────────────────────────────────────────────────────

echo ""
echo "► Building Tauri application..."

cargo tauri build

# ── Done ────────────────────────────────────────────────────────────────────

echo ""
echo "═══════════════════════════════════════════"
echo "  Build complete!"
echo ""

# Find the output
APPIMAGE=$(find src-tauri/target/release/bundle/appimage -name '*.AppImage' 2>/dev/null | head -1)
DEB=$(find src-tauri/target/release/bundle/deb -name '*.deb' 2>/dev/null | head -1)

if [ -n "$APPIMAGE" ]; then
    SIZE=$(du -h "$APPIMAGE" | cut -f1)
    echo "  AppImage: $APPIMAGE ($SIZE)"
fi
if [ -n "$DEB" ]; then
    SIZE=$(du -h "$DEB" | cut -f1)
    echo "  Deb:      $DEB ($SIZE)"
fi
echo "═══════════════════════════════════════════"
