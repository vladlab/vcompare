#!/usr/bin/env bash
#
# setup-dev.sh — Create a shim sidecar binary for development.
#
# Tauri validates externalBin paths at compile time, so even in dev mode
# the binary must exist. This creates a tiny shell script shim that
# forwards to python3. (In dev mode, lib.rs ignores it and spawns
# python3 directly, but the file has to be there for Tauri's build
# script to pass.)
#
# Run this once after cloning:
#   ./scripts/setup-dev.sh

set -euo pipefail
cd "$(dirname "$0")/.."

TRIPLE=$(rustc -vV | grep '^host:' | awk '{print $2}')
BINDIR="src-tauri/binaries"

mkdir -p "$BINDIR"

SHIM="$BINDIR/vcompare_engine-${TRIPLE}"

cat > "$SHIM" << 'EOF'
#!/usr/bin/env bash
# Dev shim — forwards to python3. Only used if cargo tauri dev somehow
# invokes the binary path instead of the python3 codepath in lib.rs.
DIR="$(cd "$(dirname "$0")/../.." && pwd)"
exec python3 "$DIR/sidecar/vcompare_engine.py" "$@"
EOF

chmod +x "$SHIM"
echo "Created dev shim: $SHIM"
echo "You can now run: cargo tauri dev"
