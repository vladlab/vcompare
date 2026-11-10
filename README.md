# vcompare

Video comparison and QC tool — Tauri desktop app with a native Rust analysis
engine driving ffmpeg.

## Modes

| Mode | Inputs | ffmpeg filters | Metrics |
|------|--------|----------------|---------|
| `diff` | reference + distorted | `psnr` (+ optional `ssim`) | `diff_y/u/v` (RMS pixel difference), `ssim*` |
| `vmaf` | reference + distorted | `libvmaf` / `libvmaf_cuda` | `vmaf`, `psnr` |
| `levels` | single file | native stats (ffmpeg decode → raw pipe) | `y/u/v_min/max`, `y/u/v_avg`, `y/u/v_dif` |
| `timeline` | single file + Resolve's SDI output | `psnr` per sample | `diff_y/u/v` at the first / middle / last frame of every clip |

Both comparison modes sync the two files by start-of-file or by embedded
timecode, handle resolution and pixel-format mismatches (compare at the
smaller resolution, upconvert to the higher bit depth), and accept IMF /
concat manifests as inputs.

`levels` doesn't use an ffmpeg filter: ffmpeg only decodes, writing planar
YUV at the source's bit depth and subsampling to a pipe, and the statistics
are computed in Rust on a worker pool (`engine/rawstats.rs`). ffmpeg's
`signalstats` filter is single-threaded and costs more than decoding, so this
makes the pass decode-bound. Sources with hardware decoders (H.264, HEVC,
AV1, …) use `-hwaccel` when ffmpeg offers one, falling back to software if
no device is available. ProRes / DNxHR always decode in software.

A single ffmpeg can only push raw UHD 10-bit frames out at ~60 fps, well
below what a many-core machine decodes, so intra-frame constant-rate files
(ProRes, DNxHR, FFV1, …) are split into chunks decoded by several ffmpeg
processes in parallel and stitched back together; the result is identical
to a single-process run (each seam is verified, with a fallback to one
process if a seek ever lands off-frame). **Parallel decodes** in the levels
options picks how many — Auto is about one per 10 hardware threads; lower it
for slow disks or network shares, where parallel reads just compete.

Files can be chosen with the file dialog or dropped onto the window: onto
the Reference / Distorted rows individually, or two at once to fill both.
A running analysis can be cancelled (button or `Esc`); a cancelled job
saves nothing and leaves no temp files.

Results are cached as `.vcompare.json` next to the reference file and can be
reopened from the Recent Analyses list, exported to CSV, and scrubbed in mpv
(side-by-side or difference view).

### Flash-frame detection

`levels` and `diff` analyses feed an event detector that finds flash frames:
runs of 1–3 frames that differ sharply from *both* neighbours while the
neighbours agree with each other (which is what separates a flash from a cut).
Flashes are sub-classified as black, white, or content. For native levels
analyses, whether the neighbours agree is judged from small luma thumbnails
of the frames on either side; older caches fall back to average luma and
chroma. Detection runs over
the cached per-frame stats, so sensitivity can be tuned live without
re-running ffmpeg. Events are shown as markers on the timeline and chart,
listed in a panel, and navigable with `↑` / `↓` (which also seeks mpv). The
event you picked stays highlighted while you shuttle around it, with a
filmstrip of the frame before, the flash frame(s) and the frame after (both
files in a comparison). Each event can be accepted or rejected (`y` / `x`);
verdicts are saved in `<analysis>.reviews.json` next to the analysis and come
back when it's reopened. Events export as CSV (with the verdict) or as a
DaVinci Resolve marker EDL (Timeline › Import › Timeline Markers from EDL;
colour by kind, cyan for at-cut; rejected events are left out).

### DaVinci Resolve sync

With **Resolve sync** on (`r`), the cursor drives the playhead of the timeline
open in Resolve, via Resolve's scripting API. Needs Resolve running with
*Preferences › System › General › External scripting using: Local*, and a
Python 3 on the PATH (set `VCOMPARE_PYTHON` to pick one). Timecodes are the
file's own, so this lines up when the file was exported from that timeline.

### Timeline mode (file vs. Resolve's SDI output)

Checks a deliverable against what Resolve puts out on SDI, which is what
was on the reference monitor during the grade. For every clip on the video
tracks you tick, vcompare parks Resolve's playhead on the clip's first,
middle and last frame, takes that frame off the SDI loop, and compares it
with the same timecode in the file. Nothing is rendered and nothing is kept
on disk per frame: each grabbed frame is compared and dropped.

Needs, on the grading machine:

- Resolve Studio with *External scripting using: Local* and a Python 3 (as
  for Resolve sync).
- [scopetap](https://github.com/vladlab/scopetap): `scopetap_stream --scopes`
  running on the DeckLink input that loops Resolve's SDI output back in, and
  `scopetap_grab` on the PATH (or `VCOMPARE_GRABBER` pointing at it). Without
  it the mode is offered but the Check button says what's missing.

How it compares: both sides go to planar 4:4:4 at the file's bit depth (at
least 10), the SDI side converted with the file's own matrix and range from
its tags (untagged: BT.709 limited, or BT.2020 when the primaries say so),
and first put through the file's chroma subsampling so 4:2:2 losses on hard
colour edges don't drown everything else. ProRes 422 HQ of a clean picture
compares at about 1–2 codes RMS on every plane. No transfer or gamut
conversion is applied: the monitor is assumed to show the deliverable's
colour space, and the run warns when the SDI tags and the file's disagree.

Flagging: a sample is flagged when its RMS luma difference stands out from
the clean end of the run (lower quartile + 6σ of the cleaner half, so a fault
on most clips can't become the baseline), with the threshold kept between
0.4 % and 3 % of peak so a fault on every sample (wrong levels, a missing
output LUT) is still flagged, plus a warning when the whole run differs. Samples that
couldn't be checked (the SDI never settled on the timecode, a seek was
refused, outside the file) are listed too. Selecting one seeks Resolve (sync
turns on by itself) and mpv to that frame; flagged samples export as Resolve
timeline markers. Runs are saved as `<file>.timeline.json` and reopen from
Recent Analyses without Resolve.

Resolve's monitoring state is the operator's job, and shows up as
differences: bypass, wipes / split screen, highlight, reference stills,
timeline proxy or performance mode, a stale render cache, LUTs applied only
to the monitor output, SDI data levels different from the render. Temporal
effects (temporal NR, optical-flow retimes) can also differ between a parked
frame and a rendered one.

**Dry run** grabs every sample without comparing or saving, to measure how
long a real run will take on a given machine and reel.

**Without the hardware:** `scripts/dev/fake_timeline_demo.sh` builds a reel
with two planted faults and prints the environment that points vcompare at a
fake Resolve (`src-tauri/testdata/fake_resolve`) and a fake grabber
(`scripts/dev/fake_scopetap_grab.py`).

## Architecture

```
┌─────────────────────────────────────────────────┐
│  Svelte 5 frontend (webview)                    │
│  uPlot charts · virtual-scroll table · timeline │
└──────────────┬──────────────────────────────────┘
               │ invoke() / listen()   (Tauri IPC)
┌──────────────┴──────────────────────────────────┐
│  Rust core (Tauri v2)                           │
│  bridge.rs   Tauri commands                     │
│  engine/     ffmpeg pipeline, parsing, state,   │
│              event detection, mpv IPC, timecode │
└──────────────┬──────────────────────────────────┘
               │ subprocess
         ffmpeg / ffprobe / mpv
```

Progress and diagnostics are streamed to the frontend as Tauri events
(`rpc:progress`, `rpc:diagnostics`, `rpc:mpv-position`, …).

## Project structure

```
src-tauri/
├── src/
│   ├── lib.rs                 App setup, menu, managed state
│   ├── bridge.rs              #[tauri::command] handlers
│   └── engine/
│       ├── analysis.rs        ffmpeg command construction + stats parsing
│       ├── rawstats.rs        Native per-frame stats over a rawvideo pipe
│       ├── events.rs          Flash-frame / spike detection over series
│       ├── state.rs           In-memory analysis results, queries, CSV export
│       ├── cache.rs           .vcompare.json load/save
│       ├── probe.rs           ffprobe, capability detection
│       ├── pixfmt.rs          Pixel-format negotiation
│       ├── timecode.rs        TC ⇄ frame arithmetic (incl. drop-frame)
│       ├── mpv.rs             mpv IPC controller
│       ├── resolve.rs         Resolve bridge: cursor sync + run sessions
│       ├── resolve_bridge.py  Python helper (Resolve's API is Python)
│       ├── grab.rs            scopetap_grab client (SDI frames by timecode)
│       ├── timeline.rs        Timeline mode: sample planner
│       ├── timeline_compare.rs  …one SDI frame vs. the file (ffmpeg psnr)
│       ├── timeline_run.rs    …the run: seek, grab, compare pool, report
│       └── types.rs           Shared structs
├── testdata/fake_resolve/     Fake DaVinciResolveScript for tests and the demo
└── tauri.conf.json
scripts/dev/                   fake_scopetap_grab.py, fake_timeline_demo.sh
src/
├── App.svelte                 State machine: pick → analyze → view
├── lib/rpc.ts                 Typed invoke() wrappers + event listeners
└── components/
    ├── MetricChart.svelte     uPlot detail + overview minimap
    ├── TimelineBar.svelte     Ref/dist overlap + event markers
    ├── FrameTable.svelte      Virtual-scrolled frame table
    ├── EventPanel.svelte      Flash-frame detector controls + event list
    ├── TimelineSetup.svelte   Timeline mode: Resolve tracks, start
    └── TimelineReport.svelte  Timeline mode: what a run looked at
```

## Prerequisites

- **Rust** (via rustup) and the Tauri CLI: `cargo install tauri-cli --version "^2.0"`
- **Node.js** ≥ 18
- **ffmpeg / ffprobe** built with `libvmaf` (and optionally `libvmaf_cuda`)
- **mpv** for side-by-side / difference preview (optional)
- WebKitGTK and friends — Fedora:
  ```bash
  sudo dnf install webkit2gtk4.1-devel openssl-devel curl-devel \
    librsvg2-devel gtk3-devel libsoup3-devel javascriptcoregtk4.1-devel
  ```

## Development

```bash
npm install
cargo tauri dev        # Vite dev server + Rust binary, hot-reloading frontend
npm run dev            # frontend only (no engine) — useful for layout work
cargo test --manifest-path src-tauri/Cargo.toml   # engine unit tests
```

Set `RUST_LOG=debug` for verbose logging. On NVIDIA / Wayland the app sets
the usual WebKitGTK workaround env vars automatically.

## Building and installing

```bash
./scripts/build.sh      # → src-tauri/target/release/bundle/{appimage,deb}/
./scripts/install.sh    # AppImage → ~/.local/bin/vcompare + app-menu entry and icon
./scripts/install.sh --uninstall
```

The install is per-user (no root). ffmpeg, ffprobe and mpv aren't bundled;
install them from your distribution.

## Keyboard

| Key | Action |
|-----|--------|
| `m` | cycle active metric |
| `t` / `T` | auto-threshold from active metric / clear threshold |
| `↑` / `↓` (or `n` / `N`) | previous / next flash event |
| `y` / `x` | accept / reject the picked event |
| `r` | toggle DaVinci Resolve playhead sync |
| `Esc` | cancel the running analysis (nothing is saved) |
| `p` / `P` | launch / close mpv side-by-side |
| `d` / `D` | launch / close mpv difference view |
