#!/usr/bin/env bash
#
# package-linux.sh — Build the native (egui) vcompare and pack it as a
# tarball that installs on Rocky / RHEL 9 and anything newer.
#
# Usage:
#   ./scripts/package-linux.sh            # build, check, pack
#   MAX_GLIBC=2.39 ./scripts/package-linux.sh   # only for newer targets
#
# Output:
#   target/dist/vcompare-egui-<version>-linux-<arch>.tar.gz
#
# The binary needs nothing but glibc at load time (X11 / Wayland / GL /
# Vulkan libraries are opened at run time), so the only thing that limits
# where it runs is the glibc it was linked against: Rocky 9 has 2.34. Build
# on Rocky 9 itself (it needs only rustc and cargo, no -devel packages), or
# have cargo-zigbuild installed, which this script then uses to target
# glibc 2.34 from any distro. Either way the script refuses to pack a
# binary that needs a newer glibc than MAX_GLIBC (default 2.34).

set -euo pipefail
cd "$(dirname "$0")/.."

APP=vcompare-egui
MAX_GLIBC=${MAX_GLIBC:-2.34}
VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(uname -m)

if cargo zigbuild --help >/dev/null 2>&1; then
    TARGET="$ARCH-unknown-linux-gnu"
    echo "► Building with cargo-zigbuild for glibc $MAX_GLIBC..."
    cargo zigbuild --release -p vcompare-egui --target "$TARGET.$MAX_GLIBC"
    BIN="target/$TARGET/release/vcompare-egui"
else
    echo "► Building..."
    cargo build --release -p vcompare-egui
    BIN="target/release/vcompare-egui"
fi

# The newest GLIBC_x.y symbol version the binary asks for.
need=$(LC_ALL=C grep -aoE 'GLIBC_[0-9]+\.[0-9]+(\.[0-9]+)?' "$BIN" | sed 's/GLIBC_//' | sort -uV | tail -1)
newest=$(printf '%s\n%s\n' "$need" "$MAX_GLIBC" | sort -V | tail -1)
if [ "$newest" != "$MAX_GLIBC" ]; then
    echo "✗ $BIN needs glibc $need; Rocky 9 has $MAX_GLIBC." >&2
    echo "  Build on Rocky 9, install cargo-zigbuild, or set MAX_GLIBC if you only target newer systems." >&2
    exit 1
fi
echo "  needs glibc $need (limit $MAX_GLIBC): ok"

NAME="$APP-$VERSION-linux-$ARCH"
STAGE="target/dist/$NAME"
rm -rf "$STAGE"
mkdir -p "$STAGE/icons"
install -m 755 "$BIN" "$STAGE/$APP"
strip "$STAGE/$APP" 2>/dev/null || true
install -m 644 packaging/icons/{32x32,128x128,256x256,512x512}.png packaging/icons/icon.svg "$STAGE/icons/"
install -m 755 packaging/linux/install.sh "$STAGE/install.sh"
install -m 644 packaging/linux/README.txt "$STAGE/README.txt"

tar -C target/dist -czf "target/dist/$NAME.tar.gz" "$NAME"
rm -rf "$STAGE"
echo ""
echo "  target/dist/$NAME.tar.gz ($(du -h "target/dist/$NAME.tar.gz" | cut -f1))"
