# vcompare-tauri

Video quality comparison tool — Tauri desktop app with Python analysis backend.

## Architecture

```
┌─────────────────────────────────────────────────┐
│  Svelte Frontend (webview)                      │
│  uPlot charts · virtual-scroll table · controls │
└──────────────┬──────────────────────────────────┘
               │ invoke() / listen()
               │ Tauri IPC
┌──────────────┴──────────────────────────────────┐
│  Rust core (Tauri v2)                           │
│  Sidecar lifecycle · JSON-RPC mux · file dialog │
└──────────────┬──────────────────────────────────┘
               │ stdin/stdout JSON-RPC 2.0
               │ (newline-delimited JSON)
┌──────────────┴──────────────────────────────────┐
│  Python sidecar (vcompare_engine.py)            │
│  FFmpeg/VMAF pipeline · mpv IPC · timecode math │
└─────────────────────────────────────────────────┘
```

Communication: **JSON-RPC 2.0 over stdio**. Requests have IDs and get
routed to the caller. Notifications (progress, mpv position) are broadcast
as Tauri events to the frontend.

## Project Structure

```
vcompare-tauri/
├── src-tauri/                    # Rust / Tauri backend
│   ├── src/
│   │   ├── main.rs              # Entry point
│   │   ├── lib.rs               # App setup, sidecar spawn
│   │   ├── sidecar.rs           # JSON-RPC stdio multiplexer
│   │   └── bridge.rs            # Tauri commands (typed passthrough)
│   ├── Cargo.toml
│   └── tauri.conf.json
├── src/                          # Svelte frontend
│   ├── main.ts
│   ├── App.svelte               # State machine: pick → analyze → view
│   ├── lib/
│   │   └── rpc.ts               # Typed Tauri invoke wrappers + events
│   ├── components/
│   │   ├── MetricChart.svelte   # uPlot detail + overview minimap
│   │   └── FrameTable.svelte    # Virtual-scrolled data table
│   └── styles/
│       └── global.css           # Dark theme
├── sidecar/                      # Python analysis engine
│   ├── vcompare_engine.py       # RPC method handlers (entry point)
│   ├── rpc_server.py            # JSON-RPC 2.0 stdio server
│   └── vcompare.py              # ← copy your existing file here
├── package.json
├── vite.config.ts
└── README.md
```

## Prerequisites

- **Rust** (via rustup): `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- **Node.js** ≥ 18: for Svelte/Vite frontend
- **Python** ≥ 3.10: analysis engine
- **ffmpeg/ffprobe** built with libvmaf (and optionally libvmaf_cuda)
- **mpv** for side-by-side preview (optional)
- **System libs** (Fedora):
  ```bash
  sudo dnf install webkit2gtk4.1-devel \
    openssl-devel curl-devel \
    librsvg2-devel gtk3-devel \
    libsoup3-devel javascriptcoregtk4.1-devel
  ```

## Setup

```bash
# 1. Clone/copy the project
cd vcompare-tauri

# 2. Copy your existing vcompare.py into the sidecar directory
cp /path/to/vcompare.py sidecar/vcompare.py

# 3. Install frontend dependencies
npm install

# 4. Install Tauri CLI
cargo install tauri-cli --version "^2.0"

# 5. Run in dev mode (hot-reload frontend + Rust rebuild)
cargo tauri dev
```

## Development Workflow

### Frontend only (fast iteration on UI)
```bash
npm run dev
# Opens at http://localhost:5173
# Won't connect to sidecar — use for layout/styling
```

### Full stack
```bash
cargo tauri dev
# Spawns: Vite dev server + Rust binary + Python sidecar
# Frontend hot-reloads. Rust rebuilds on save. Python restarts on app relaunch.
```

### Test Python sidecar standalone
```bash
# Pipe JSON-RPC messages manually to test the engine
echo '{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}' | python3 sidecar/vcompare_engine.py
```

## Key Decisions

| Decision | Choice | Why |
|----------|--------|-----|
| IPC transport | stdio JSON-RPC | Zero-config, no ports, sidecar lifecycle tied to app |
| Chart library | uPlot | Canvas-based, handles 500k+ points natively, 35KB |
| Frontend framework | Svelte 5 | Lightest option for Tauri, smallest bundle |
| Python packaging | PATH | Simplest for personal tool; PyInstaller later if needed |
| mpv management | Python | Existing code works, Unix socket IPC stays in one place |

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `VCOMPARE_PYTHON` | `python3` | Python interpreter path |
| `VCOMPARE_SIDECAR` | auto-detected | Path to `vcompare_engine.py` |
| `RUST_LOG` | — | Set to `debug` for verbose Rust logging |

## Building for Distribution

```bash
cargo tauri build
# Output: src-tauri/target/release/bundle/
#   - .deb (Debian/Ubuntu)
#   - .AppImage (universal Linux)
#   - .rpm (Fedora) — may need tauri-plugin-bundler-rpm
```

For distribution, bundle the sidecar as a standalone binary:
```bash
pip install pyinstaller
pyinstaller --onefile sidecar/vcompare_engine.py
# Then update tauri.conf.json externalBin to point to the binary
```

## What's Not Wired Yet (TODO)

The scaffold provides all the plumbing. What remains is connecting
specific UI interactions to the existing RPC methods:

- [ ] Threshold input field (currently only set via keyboard shortcut)
- [ ] Filter controls panel (metric + threshold below)
- [ ] "Jump to worst/best" buttons
- [ ] Timecode display + copy-to-clipboard
- [ ] mpv status indicator (paused/playing, current TC)
- [ ] IMF CPL support in file picker (detect .xml, resolve dialog)
- [ ] Manual frame offset input
- [ ] Saved analysis auto-detection (check for existing .vcompare.json)
- [ ] Window title update with file names
- [ ] Drag-and-drop file import
