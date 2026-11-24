#!/usr/bin/env bash
#
# package-macos.sh — Build the native (egui) vcompare as a macOS app.
#
# Usage (on a Mac, with Rust installed):
#   ./scripts/package-macos.sh              # for this Mac's CPU
#   ./scripts/package-macos.sh --universal  # Apple silicon + Intel in one
#
# Output:
#   target/dist/vcompare.app
#   target/dist/vcompare-egui-<version>-macos-<arch>.zip
#
# The app is signed ad hoc, not notarised. On another Mac, the first launch
# of a downloaded copy is blocked by Gatekeeper: right-click > Open once, or
#   xattr -dr com.apple.quarantine /Applications/vcompare.app
#
# ffmpeg / ffprobe / mpv are not included: brew install ffmpeg mpv. vcompare
# looks in /opt/homebrew/bin, /usr/local/bin and /opt/local/bin as well as
# PATH (apps opened from the Finder don't get the shell's PATH), and File >
# Tools… can point it anywhere else.

set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
APP=target/dist/vcompare.app

if [ "${1:-}" = "--universal" ]; then
    ARCH=universal
    rustup target add aarch64-apple-darwin x86_64-apple-darwin
    for t in aarch64-apple-darwin x86_64-apple-darwin; do
        echo "► Building for $t..."
        MACOSX_DEPLOYMENT_TARGET=11.0 cargo build --release -p vcompare-egui --target "$t"
    done
    mkdir -p target/universal
    lipo -create -output target/universal/vcompare-egui \
        target/aarch64-apple-darwin/release/vcompare-egui \
        target/x86_64-apple-darwin/release/vcompare-egui
    BIN=target/universal/vcompare-egui
else
    ARCH=$(uname -m)
    echo "► Building..."
    MACOSX_DEPLOYMENT_TARGET=11.0 cargo build --release -p vcompare-egui
    BIN=target/release/vcompare-egui
fi

echo "► Making $APP..."
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 755 "$BIN" "$APP/Contents/MacOS/vcompare"

# Icon: an .iconset of PNG sizes, then iconutil.
SET=$(mktemp -d)/vcompare.iconset
mkdir -p "$SET"
for s in 16 32 128 256 512; do
    sips -z $s $s packaging/icons/512x512.png --out "$SET/icon_${s}x${s}.png" >/dev/null
    d=$((s * 2))
    if [ $d -le 512 ]; then
        sips -z $d $d packaging/icons/512x512.png --out "$SET/icon_${s}x${s}@2x.png" >/dev/null
    fi
done
iconutil -c icns "$SET" -o "$APP/Contents/Resources/vcompare.icns"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>vcompare</string>
    <key>CFBundleDisplayName</key><string>vcompare</string>
    <key>CFBundleIdentifier</key><string>com.vcompare.egui</string>
    <key>CFBundleExecutable</key><string>vcompare</string>
    <key>CFBundleIconFile</key><string>vcompare</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>LSApplicationCategoryType</key><string>public.app-category.video</string>
</dict>
</plist>
EOF

codesign --force --deep --sign - "$APP"

ZIP="target/dist/vcompare-egui-$VERSION-macos-$ARCH.zip"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"
echo ""
echo "  $APP"
echo "  $ZIP ($(du -h "$ZIP" | cut -f1))"
