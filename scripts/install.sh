#!/usr/bin/env bash
#
# install.sh — Install the built vcompare AppImage for the current user, with
# a desktop entry so it appears in the application menu.
#
# Usage:
#   ./scripts/build.sh && ./scripts/install.sh      # build, then install
#   ./scripts/install.sh --uninstall                # remove it again
#
# Installs to (no root needed):
#   ~/.local/bin/vcompare                            the AppImage
#   ~/.local/share/applications/vcompare.desktop     menu entry
#   ~/.local/share/icons/hicolor/<size>/apps/vcompare.png
#
# Make sure ~/.local/bin is on your PATH if you also want to launch it from a
# terminal. ffmpeg, ffprobe and mpv are not bundled: install them with the
# system package manager.

set -euo pipefail
cd "$(dirname "$0")/.."

BIN_DIR="$HOME/.local/bin"
APPS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
ICON_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"
DESKTOP="$APPS_DIR/vcompare.desktop"

refresh() {
    command -v update-desktop-database >/dev/null && update-desktop-database "$APPS_DIR" 2>/dev/null || true
    command -v gtk-update-icon-cache  >/dev/null && gtk-update-icon-cache -q -t "$ICON_ROOT" 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$BIN_DIR/vcompare" "$DESKTOP"
    for s in 32 128 256; do rm -f "$ICON_ROOT/${s}x${s}/apps/vcompare.png"; done
    refresh
    echo "vcompare removed."
    exit 0
fi

APPIMAGE=$(find src-tauri/target/release/bundle/appimage -name '*.AppImage' 2>/dev/null | sort | tail -1 || true)
if [ -z "$APPIMAGE" ]; then
    echo "No AppImage found — run ./scripts/build.sh first." >&2
    exit 1
fi

mkdir -p "$BIN_DIR" "$APPS_DIR"
install -m 755 "$APPIMAGE" "$BIN_DIR/vcompare"

for s in 32 128 256; do
    mkdir -p "$ICON_ROOT/${s}x${s}/apps"
    install -m 644 "src-tauri/icons/${s}x${s}.png" "$ICON_ROOT/${s}x${s}/apps/vcompare.png"
done

cat > "$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Name=vcompare
GenericName=Video Comparison and QC
Comment=Compare two video files, measure levels, find flash frames
Exec=$BIN_DIR/vcompare %F
Icon=vcompare
Terminal=false
Categories=AudioVideo;Video;Graphics;
MimeType=video/quicktime;video/mp4;video/x-matroska;application/mxf;
StartupWMClass=vcompare
EOF
chmod 644 "$DESKTOP"
refresh

echo "Installed:"
echo "  $BIN_DIR/vcompare   ($(du -h "$BIN_DIR/vcompare" | cut -f1), from $(basename "$APPIMAGE"))"
echo "  $DESKTOP"
echo "It should now appear in your application menu as “vcompare”."
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) echo "Note: $BIN_DIR is not on your PATH (only matters for launching from a terminal)." ;; esac
