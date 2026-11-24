# Plan: port the UI from Tauri/Svelte to egui

Branch: `egui-port` (off `detector-tabs` at `1572726`). **Status: §1 built
and checked by Vladimir (2026-11-24); §2 spike built and accepted (go);
§3 built, see its status note.** §1: Gates passed in a sandbox (Ubuntu 24.04, no GPU, no
Resolve): 131 tests from the workspace root (126 engine, 5 service, three
of them end to end through `Service` on a generated clip); every
`invoke()` in `rpc.ts` matched to a command with the same argument names
and every `listen()` to a `UiEvent` name; the release Tauri build starts
under Xvfb. Not yet checked: the web UI clicked through on a real machine
(levels, diff, a saved analysis, mpv).

Differences from the plan, found while building:
- **Test fix.** `rawstats`' worker-failure test injected its panic through
  a process-wide static, so `chunked_levels_match_single_process`
  sometimes died with it when the two ran in parallel. The injection is
  now a test-only field of `RawRunOptions`.
- **Tool lookup.** Production code already started every child through
  `sysenv::command`, so `tools::command(Tool)` sits on top of it. Test
  code still calls `Command::new("ffmpeg")` directly, which is fine.
- **Mpv and diffview share code.** The two copies of the launch / poll /
  kill code are now one `Player` in `vcompare-service`.
- **Levels cache reuse.** Unchanged and pinned by a test: `analyze` re-runs
  levels even when its cache exists, because `cache::load_diff_cache` only
  accepts diff caches. The UI reopens levels caches through
  `load_saved_analysis`, so nothing visible depends on it.

## Why

Tauri 2 needs WebKitGTK 4.1 (libsoup 3). EL9 ships only webkit2gtk-4.0;
Red Hat closed the RHEL 9 request WONTFIX and nobody maintains it in EPEL 9.
So vcompare can't be built natively on Color 01 (Rocky 9). A Fedora-built
AppImage won't run there either, because its glibc is newer than EL9's 2.34.
This is also the tool most likely to go to other people, so the target is
**one native binary with no web runtime**: build it with `cargo build` on any
Linux, macOS or Windows and run it.

Assumption about the handoff (not confirmed): mostly operators running it on
their own machines, plus possibly a developer maintaining it later. So the
priorities are a trivial install, clear errors when ffmpeg or mpv is missing,
and a codebase that is all Rust.

Vladimir wants to give egui a fair shake. That's why §2 is a spike with a
go/no-go decision before the bulk of the port.

## 0. What exists (read before changing)

- **Engine (`src-tauri/src/engine/`, ~14k lines) has no Tauri dependency.**
  It is synchronous: analysis functions take `ProgressFn` callbacks and a
  `ProcessHandle` for cancellation (`run_diff_analysis`,
  `run_levels_analysis`, `run_vmaf_analysis`, `timeline_run::run` with
  `Callbacks`). All of its tests live there.
- **`bridge.rs` (1293 lines) is more than glue.** It holds orchestration:
  `AnalyzeParams` handling, cache/save paths, recents, reference store,
  mpv/diffview lifecycles with position-polling threads, and Resolve
  connect/seek. It talks to the UI through `#[tauri::command]` return values
  and `app.emit(...)` for `rpc:progress`, `rpc:diagnostics`,
  `rpc:timeline-progress`, `rpc:mpv-position`, `rpc:mpv-exited`,
  `rpc:diffview-position`, `rpc:diffview-exited`, `rpc:resolve-status`.
  It uses `tokio::spawn_blocking` only to get off the IPC thread.
- **`lib.rs`** has the File menu (New Analysis Ctrl+N, Recent Analyses
  Ctrl+Shift+O, Export CSV Ctrl+Shift+E, Quit), managed state, WebKit
  env-var workarounds, and cleanup on exit (kills mpv/ffmpeg).
- **Frontend (~6.1k lines).** Svelte only calls `@tauri-apps` from `App.svelte`
  and `lib/rpc.ts`. These are the pieces to port:
  - `App.svelte` (2173): state machine idle → probing → analyzing → ready;
    file pickers and probe cards; mode select (diff / vmaf / levels /
    timeline); sync mode, offset and SSIM options; cache-exists prompt;
    progress with fps/ETA; diagnostics; summary header; tab strip; status
    line; Recents modal; the keyboard map (below).
  - `MetricChart.svelte` (928, uPlot): detail chart plus overview minimap;
    wheel zoom on the mouse and key zoom on the playhead; playhead
    auto-follow; scrollbar pan; click-drag scrub; hatched no-data zones
    outside the overlap; threshold line; event markers (hidden on the
    Frames tab); sparse point mode for timeline samples; y-range per metric
    (levels infer 255/1023 peak).
  - `FrameTable.svelte` (416): virtual-scrolled frame table.
  - `TabStrip`, `DetectorTab` (438), `EventList` (381): per-detector
    controls, event rows with 54 px in-row thumbnail strips, accept/reject
    review, CSV/EDL export.
  - `lib/thumbLoader.svelte.ts` (140): thumbnails load after a 2.5 s settle
    once the threshold stops moving, none while dragging, at most 3 strips
    in flight.
  - `TimelineBar` (384), `TimelineSetup` (253), `TimelineReport` (154):
    timeline mode.
  - `lib/detectors.ts` (115): the detector/tab registry.
- **Keyboard map (must survive the port):** `m` metric · `t`/`T` auto/clear
  threshold · `n`/`↓`, `N`/`↑` next/prev event · `p`/`P` mpv · `d`/`D`
  diffview · `s` seek players to cursor · `a` mpv auto-seek · `r` Resolve
  sync · `y`/`x` accept/reject · `.`/`,` step · space pause · `e` export ·
  `f` cycle smoothing · `=`/`+`/`-`/`0` zoom · `1`–`5` tabs · Esc cancels
  analysis. Shortcuts are ignored while a text field has focus.
- **Thumbnails:** `thumbs::extract_frames` returns JPEG data-URI strings
  (count 1–16, height 16–1080).
- **ffmpeg, ffprobe and mpv** are found by bare name on `PATH`
  (`analysis.rs`, `probe.rs`, `mpv.rs`).

## 1. Split the workspace (no behaviour change)

Lay the repo out as a Cargo workspace:

```
crates/vcompare-engine/   ← src-tauri/src/engine/ moved as is (+ resolve_bridge.py)
crates/vcompare-service/  ← the orchestration now in bridge.rs, Tauri-free
crates/vcompare-egui/     ← new UI (from §2)
src-tauri/                ← kept building against vcompare-service until §6
```

- **`vcompare-service`.** Make `Service` own what `app.manage(...)` holds
  today: engine state, mpv, diffview, ffmpeg handle, Resolve, references,
  recents (`data_dir` comes from the `directories` crate, not
  `app.path()`). Each Tauri command becomes a plain method with the same
  inputs and outputs. Long work runs on a `std::thread`. Every `emit` goes
  through an `EventSink`:
  ```rust
  pub enum UiEvent { Progress{..}, Diagnostics(Vec<String>), TimelineProgress(..),
                     MpvPosition(..), MpvExited(..), DiffviewPosition(..),
                     DiffviewExited(..), ResolveStatus(..) }
  pub trait EventSink: Send + Sync + 'static { fn send(&self, e: UiEvent); }
  ```
- **Tauri adapter.** `src-tauri/src/bridge.rs` becomes a thin layer:
  commands call `Service` and the sink calls `app.emit` with the same
  event names and JSON. The Svelte app keeps working throughout, so there
  is always something usable while the port is in progress.
- **Tool paths.** Add `ToolPaths { ffmpeg, ffprobe, mpv }` in the engine,
  set from config, then env (`VCOMPARE_FFMPEG`, …), then `PATH`. Replace
  the bare `Command::new("ffmpeg")` calls with it. This is needed for the
  handoff (static builds in odd places, macOS Homebrew paths).
- **Gate:** all engine tests pass from the workspace root; the Tauri app
  runs the same as before (levels, diff, a saved analysis, mpv).

## 2. Spike: the hard 20 % (decision point)

Build the minimum `vcompare-egui` (eframe) that **opens an existing
`.vcompare.json`** (no analysis yet) and shows these three things. They're
the parts most likely to decide whether egui is good enough:

1. **Chart, as a custom widget on `egui::Painter`, not `egui_plot`.**
   egui_plot tessellates every point each frame, which won't stay snappy
   at 130k+ points (a 90-minute feature). Build:
   - A **min/max pyramid per series**: level 0 is per-frame, each level up
     halves the resolution, built once on load and on smoothing change.
     Drawing picks the level where one bucket ≤ one pixel column and draws
     a min–max envelope plus a mean line. Cost per frame then depends on
     the chart's width, not the file's length.
   - The detail view and a minimap from the same pyramid. Wheel zoom on
     the mouse, keys on the playhead, drag on the minimap to pan,
     click-drag to scrub, auto-follow, threshold line, event markers,
     hatched no-data zones, sparse point mode, y-range rules ported from
     `MetricChart.svelte`.
2. **Frame table** with `egui_extras::TableBuilder` and `body.rows(h, n, …)`
   (only visible rows are built), synced to the cursor.
3. **One detector tab (flash) with in-row 54 px thumbnail strips.**
   - Change `thumbs::extract_frames` to return JPEG **bytes**; keep a
     `data:` wrapper in the Tauri adapter only.
   - Decode with the `image` crate on the worker thread and upload as
     `TextureHandle`s, with an LRU cache keyed by `(file, frame, height)`.
   - Port the thumbLoader rules (2.5 s settle, none while dragging, ≤ 3 in
     flight).

**Renderer.** Start with eframe's default renderer for the version in use
and test both `glow` and `wgpu` on Color 01 (Xorg + NVIDIA, both under
Reemo) and on a Mac if one is available. Keep whichever has no tearing or
latency issues under Reemo.

**File dialog.** Use `rfd`, trying the xdg-portal backend first and the
`gtk3` feature if the portal is flaky on EL9 GNOME. It has to browse
`/Volumes` autofs mounts properly.

**Measure** on a real 90-minute ProRes levels cache and a diff cache,
release build:
- frame time while idle, while wheel-zooming quickly, and while scrubbing
  (target < 8 ms);
- memory;
- time from launch to a drawn chart.

Compare side by side with the Svelte build on Color 02.

**Decision point: Vladimir uses the spike and says go or no-go.** Things to
judge: whether text rendering and the look are acceptable (egui is
immediate-mode and looks like egui; theming gets it close to the current
dark palette but not identical), whether keyboard and scroll feel right,
and how it does under Reemo.

**If no-go:** keep §1 (it's useful anyway) and put the Svelte UI on top of
`vcompare-service` behind a local axum HTTP/WebSocket server instead. That
needs no WebKit and keeps all of the existing UI.

### Spike status (2026-11-24): built, waiting on Vladimir's go / no-go

`crates/vcompare-egui` opens a saved analysis and has the three pieces
above. Also built while doing it:
- **Headless analysis example.**
  `cargo run --release -p vcompare-service --example analyze -- levels <file>`
  makes test caches without the web UI, and doubles as a CLI later.
- **Typed engine accessors.** `EngineState::metric_series` and
  `frame_columns` (with a test that they match the JSON forms), so the
  native UI doesn't build the whole table as JSON.

**Test material:** a synthetic 130,000-frame clip (90 min at 24 fps,
96×54 10-bit 4:2:2 FFV1) with planted 1-frame white flashes, 2-frame
black flashes and 1-frame content inserts, analysed with `levels`.

**Measured in the sandbox:** Ubuntu 24.04, 2 cores, Xvfb, Mesa
llvmpipe (software GL), release build. Real GPUs will only be faster.
- **Chart paint:** 0.8–2 ms CPU per frame, the same zoomed out over the
  whole 130k frames as zoomed in. A full 3840-column redraw is 0.43 ms of
  pyramid queries.
- **Whole UI:** 2–4 ms per frame on average while wheel-zooming,
  scrubbing and switching tabs; worst 6–8 ms after load.
- **Open:** 3.7 s from launch to chart drawn and flash detection done.
  Almost all of it is the engine parsing the 42 MB cache, the same in
  both UIs.
- **Memory:** 1.05 GB resident. Of that, ~920 MB is the engine holding
  130k frames as JSON objects, which the Tauri app pays too; ~120 MB is
  the empty UI (fonts, GL); the chart, table and pyramid add little.
  Possible follow-up for both UIs: store frames typed instead of as
  `serde_json::Value`.

**Checked by driving it under Xvfb with xdotool and screenshots**, with
both renderers (`glow`; `wgpu` via lavapipe):
- wheel zoom on the mouse; `=` `-` `0` on the playhead;
- click-drag scrub; double-click shows all;
- minimap drag-to-range and drag-to-pan; scrollbar;
- playhead auto-follow;
- `n`/`N` and `↑`/`↓` events; `y`/`x` reviews, saved to the analysis's
  reviews file and shown again on reopen;
- the Frames table following the cursor and sorting by header click;
- thumbnail strips loading after the settle delay.

**Not in the spike:**
- smoothing (`f`) and the metric threshold line (`t`/`T`);
- the repeat / match / timeline tabs and the summary header;
- mpv, Resolve and analysis runs (that's §3–§5);
- timeline-mode details (TimelineBar, report).

**Findings:**
- **Fonts.** egui's default fonts have no ✓ ✗ ▲ glyphs. Those are
  painted shapes now; bundle Inter / JetBrains Mono in §6 if the text
  should look like the web UI.
- **Column widths.** egui lays cells out to their content, so fixed
  columns need an explicitly sized child `Ui` (`flash::fixed`).
- **Build.** The binary links only glibc; X11/Wayland, GL/EGL, Vulkan
  and xkbcommon are loaded at runtime. No `-devel` packages and no
  pkg-config/cmake in the dependency tree, so building on Rocky 9 should
  need only Rust and gcc. Not tried on Rocky yet.

**Still to judge on real machines (Vladimir):**
- glow vs wgpu on Color 01 under Reemo: tearing, latency;
- the look next to the Svelte build on Color 02;
- scroll and keyboard feel.

### Spike feedback (Vladimir, 2026-11-24): runs; small fixes done

- **Frames table:** scrolled (animated) to the cursor, too slow across a
  feature. Now jumps (`animate_scrolling(false)`).
- **Minimap range select lagged the mouse.** The drag was anchored where
  egui recognised it (a few px after the press) and followed
  `interact_pointer_pos`. It's now anchored at `press_origin` and follows
  `latest_pos`, and the scrollbar does the same. For the rest of the
  pipeline: wgpu keeps 1 frame in flight; `VCOMPARE_VSYNC=0` turns vsync
  off for comparison (may tear).
- **Top bar:** `+` / `fit` fell off at the default size. Controls now come
  first, the file name truncates, and it fits down to the 800 pt minimum
  width (checked at 1× and 1.5× scale).
- **The "?" corner:** it was only a hover tooltip. Now a **Keys** button
  (or F1) opens a shortcuts panel; Esc closes it.

## 3. Pick and analyse

- **Screens** as an `enum Screen { Pick, Analyzing, Ready }` in the eframe
  `App`: pickers, probe cards, mode select, sync/offset/SSIM/VMAF options,
  fps-mismatch warning, cache-exists prompt (open saved vs re-run),
  "loading saved" (it must not say ffmpeg is running).
- **Progress:** worker threads send `UiEvent`s over a `crossbeam_channel`;
  the sink calls `ctx.request_repaint()`. The UI drains the channel at the
  top of `update`. Show fps and ETA, the diagnostics panel, and Esc to
  cancel.
- **Menu bar** (File: New, Recent…, Export CSV…, Quit) with the same
  accelerators. Recents as an `egui::Modal`.
- **Summary header** (stats for levels, diff distribution) and status-line
  messages that time out.
- **Gate:** levels, diff (± SSIM), vmaf and IMF / concat inputs run
  end-to-end and produce caches that load in both UIs.

### §3 status (2026-11-24): built

Vladimir said go after the spike. Built in `crates/vcompare-egui`:
- **Pick screen** (`pick.rs`):
  - mode: diff / vmaf / levels; timeline is listed but disabled until §5;
  - file buttons, with probing on a worker and a probe line per file;
  - drops follow the web UI's slot rules, and media can come from the
    command line (`vcompare-egui ref.mov dist.mov`);
  - fps-mismatch warning;
  - options: sync, SSIM; for VMAF hwaccel, PSNR and subsample; for levels
    parallel decodes and keep-signatures with its size estimate;
  - cache row with Load cached / Re-analyze.
  - Mode, last folder and keep-signatures persist in
    `com.vcompare.app/egui-settings.json`.
- **Working screen** (`progress.rs`):
  - progress bar, frames, fps and ETA over a 3 s window;
  - the engine's diagnostics;
  - Cancel / Esc. Its token drops the late result; nothing is saved.
  - Opening a saved analysis shows an indeterminate bar and says nothing
    is being re-analysed.
- **Error screen:** the first line says what failed; the rest (ffmpeg's
  output) scrolls; Copy and Back.
- **File menu:**
  - New, Open, Recent…, Export CSV…, Quit (Ctrl+N / O / Shift+O /
    Shift+E / Q).
  - Recents are a modal; a missing analysis shows greyed out; Clear.
  - Recents are recorded after every finished job, in the same file as
    the Tauri app's.
- **File dialogs** run on a worker thread so the window keeps painting.
- **Results** got:
  - the web UI's summary line (mode, depth, frames, fps, metric μ and
    range, diff identical / major);
  - the threshold line: diff and vmaf open with mean ± σ; `t` sets it
    from the metric, `T` clears it;
  - `e` exports CSV; the perf line shows in the status bar for 20 s.
- **Tests:** drop slot rules, analyse params per mode, probe formatting,
  rate / ETA window, threshold direction, dates.

**Gate, run in the sandbox under Xvfb with xdotool:**
- **diff + SSIM:** a 720-frame ProRes pair; then Load cached, which opens
  with the 14.9 threshold line.
- **levels on a concat list:** `ffconcat` of two ProRes parts; the
  planted flash is found.
- **levels cancel:** Esc 5 s into the 130k-frame clip; ffmpeg gone; the
  existing cache's mtime and size unchanged.
- **VMAF:** only the failure path. This ffmpeg has no libvmaf, and
  running a downloaded static ffmpeg wasn't allowed here. The error
  screen ends with "No such filter: 'libvmaf'" and the cache is left as
  it was.
- **Recents, File menu, New Analysis:** work.
- **Not tested here:**
  - **VMAF success** and **IMF CPL** inputs: check on a real machine;
  - **file dialogs:** there is no portal in the sandbox; rfd returns at
    once and the UI keeps working.

**Existing behaviour kept, worth deciding later (both UIs):**
- **Shared cache file.** Diff and VMAF of the same pair share one file
  name (`<ref>_vs_<dist>.vcompare.json`). So VMAF mode offers the diff
  cache as "Load cached", and Re-analyze in one mode overwrites the
  other's.

## 4. Detector tabs complete

- **Tab strip** from the detector registry (port `detectors.ts` as a
  Rust table): Frames, Flash, Repeat (short runs by default, long-hold
  toggle), Match, and Timeline report in the same strip.
- **Per-detector controls:** re-run detection when parameters change,
  debounced 60 ms like now (detection runs over cached stats and is cheap).
- **Reviews:** `y`/`x` and buttons, persisted as `{analysis}.reviews.json`
  with detector-keyed ids (`flash:123`), unchanged.
- **References:** list; add from an image file or from the current frame;
  rename; delete.
- **Exports:** CSV/EDL for events, CSV for frames.
- **Gate:** the detector-tabs engine tests still pass; on a real file the
  same events and the same review state show in both UIs.

## 5. Players, Resolve, timeline mode

- **mpv and diffview:** launch, kill, seek, step, pause, auto-seek, and
  position events driving the cursor. Show the exit reason (the latest
  commit's "say why it quit") in the status line.
- **Resolve:** connect, disconnect, `r` sync, status indicator.
- **Timeline mode:** TimelineSetup (track checkboxes, grabber capability),
  TimelineBar, TimelineReport (full and compact), sparse chart points.
- **Full keyboard map from §0.** Check `ctx.wants_keyboard_input()` before
  handling a shortcut.
- **Exit cleanup:** kill children in `App::on_exit` and in a `Drop` on
  `Service`.

## 6. Handoff and removal

- **Settings window:** tool paths (§1) with a "found / version / has
  libvmaf / has libvmaf_cuda" readout from `detect_capabilities`. Missing
  tools get a plain sentence saying what won't work, not a failure in the
  middle of an analysis.
- **Packaging:**
  - Linux: build on Rocky 9 (oldest glibc we care about, so the binary
    also runs on Fedora and Ubuntu); release tarball with binary +
    `.desktop` + icon. Optionally an AppImage.
  - macOS: `.app` via `cargo-bundle`.
  - Windows: should just build; untested unless someone needs it.
- **Rocky 9 build deps:** list them in the README after confirming them in
  the spike. Expect only `gcc`, `libxkbcommon-devel` and the X11/GL
  headers (winit loads X11 at runtime), plus `gtk3-devel` if `rfd` uses
  gtk3.
- **README:** rewrite Prerequisites / Development / Building for the
  workspace; screenshots.
- **Remove `src-tauri/`, `src/`, `package.json` and node files** only after
  Vladimir has used the egui build on a real job and says so. Until then
  both build.

## Risks / open questions

- **Look:** egui won't be pixel-identical to the Svelte UI. Fonts in
  particular differ; consider bundling Inter / JetBrains Mono for the
  timecode.
- **Reemo:** an X11 capture of a GL/Vulkan window. Check in §2 before
  anything else.
- **The chart widget is a real chunk of work** (~1–1.5k lines). It's the
  thing that makes or breaks the port, which is why it comes first.
- **Duplicate work:** `detector-tabs` work still pending on the office
  machine (§2 timing, real-deliverable gates) is engine-side and carries
  over. WebKitGTK look-checks become moot.
