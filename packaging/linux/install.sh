#!/usr/bin/env bash
#
# install.sh — Install vcompare (native) from this folder, with a menu entry.
#
#   ./install.sh                      for you only, in ~/.local (no root)
#   sudo ./install.sh --prefix /usr/local     for everyone on this machine
#   ./install.sh --uninstall          (with the same --prefix, if you used one)
#
# Installs:
#   <prefix>/bin/vcompare-egui
#   <prefix>/share/applications/vcompare-egui.desktop
#   <prefix>/share/icons/hicolor/<size>/apps/vcompare-egui.png (and .svg)
#
# ffmpeg and ffprobe are needed (mpv too, for the p / d players); they are
# not included. Install them with the package manager, or point vcompare at
# a build of your choice in File > Tools….

set -euo pipefail
cd "$(dirname "$0")"

APP=vcompare-egui
PREFIX="$HOME/.local"
UNINSTALL=0
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX=${2:?--prefix needs a folder}; shift 2 ;;
        --prefix=*) PREFIX=${1#--prefix=}; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h|--help) sed -n '3,16p' "$0"; exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 2 ;;
    esac
done

BIN_DIR="$PREFIX/bin"
APPS_DIR="$PREFIX/share/applications"
ICON_ROOT="$PREFIX/share/icons/hicolor"
DESKTOP="$APPS_DIR/$APP.desktop"

refresh() {
    command -v update-desktop-database >/dev/null && update-desktop-database "$APPS_DIR" 2>/dev/null || true
    command -v gtk-update-icon-cache  >/dev/null && gtk-update-icon-cache -q -t "$ICON_ROOT" 2>/dev/null || true
}

if [ "$UNINSTALL" = 1 ]; then
    rm -f "$BIN_DIR/$APP" "$DESKTOP" "$ICON_ROOT/scalable/apps/$APP.svg"
    for s in 32 128 256 512; do rm -f "$ICON_ROOT/${s}x${s}/apps/$APP.png"; done
    refresh
    echo "$APP removed from $PREFIX."
    exit 0
fi

mkdir -p "$BIN_DIR" "$APPS_DIR"
install -m 755 "$APP" "$BIN_DIR/$APP"
for s in 32 128 256 512; do
    mkdir -p "$ICON_ROOT/${s}x${s}/apps"
    install -m 644 "icons/${s}x${s}.png" "$ICON_ROOT/${s}x${s}/apps/$APP.png"
done
mkdir -p "$ICON_ROOT/scalable/apps"
install -m 644 icons/icon.svg "$ICON_ROOT/scalable/apps/$APP.svg"

cat > "$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Name=vcompare (native)
GenericName=Video Comparison and QC
Comment=Compare two video files, measure levels, find flash frames
Exec=$BIN_DIR/$APP %F
Icon=$APP
Terminal=false
Categories=AudioVideo;Video;Graphics;
MimeType=video/quicktime;video/mp4;video/x-matroska;application/mxf;
StartupWMClass=$APP
EOF
chmod 644 "$DESKTOP"
refresh

echo "Installed $BIN_DIR/$APP"
echo "It should now be in the application menu as “vcompare (native)”."
for t in ffmpeg ffprobe mpv; do
    command -v "$t" >/dev/null || echo "Note: no $t on PATH. Install it, or set its path in File > Tools…."
done
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) echo "Note: $BIN_DIR is not on your PATH (only matters for starting it from a terminal)." ;; esac
