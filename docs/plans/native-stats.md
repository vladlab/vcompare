# Plan: native stats engine for levels (raw pipe, pure Rust)

Branch: `native-stats` (off `flash-frames`). **Status: steps 1–7 done**
(Nov 2026). Levels mode runs on the native engine; the "Later, separate
plans" items in §4 are not started.

Additions beyond the plan, found while implementing:
- hw decode that fails before the first frame (ffmpeg built with cuda/vaapi
  but no device) retries in software; `RawStatsError` carries
  `frames_read` / `killed` to make that decision.
- App exit now `take()`s the ffmpeg child out of the handle (it used to
  kill in place, which the runners couldn't tell from a failure).
- Full-range `yuvj*` sources are piped as themselves; asking for `yuv*p`
  made ffmpeg rescale them to limited range.
- Legal-range counts only for limited-range YUV (absent for full-range /
  RGB). Luma is histogrammed; chroma uses a vectorized scan (~5 ms/UHD
  frame total over the previous engine).
- Progress estimate from duration when the container has no frame count;
  `-noautorotate`; `-vsync passthrough` on ffmpeg < 5.1.
- Failure paths: zero-size streams rejected up front; ffmpeg is killed
  before `wait()` if the reader stops early; a lost worker result is an
  error, not a truncated `Ok`.

Design principle: **ffmpeg decodes, Rust counts.** ffmpeg stays the only
media dependency, used through its most stable interface (`-f rawvideo`
with a documented planar pixel format). Everything we add is `std`-only
Rust so the tool builds and behaves the same on Linux, macOS and Windows.
No GPU code — see §1 for why it isn't needed.

## 1. Why

`levels` mode runs ffmpeg's `signalstats` filter. Measured on UHD ProRes 422
HQ 10-bit (120 frames, ffmpeg 6.1, 2-core box):

| Pipeline | ms/frame | fps |
|---|---|---|
| decode only, 1 thread | 135 | 7.4 |
| decode only, 2 threads | 72 | 13.9 |
| decode + `signalstats`, 2 threads | 154 | 6.5 |
| decode + `signalstats`, `-filter_threads 2` | 159 | 6.3 |
| decode → rawvideo pipe → discard | 89 | 11.3 |
| decode → rawvideo pipe → `rawstats.rs` (CPU, 2 workers) | 98 | 10.2 |

`signalstats` is effectively single-threaded (only its hue/sat pass is
slice-threaded) and costs more per UHD frame than decoding, so it caps the
pass at ~10 fps regardless of core count. Decoding threads well; piping raw
frames out of ffmpeg costs ~17 ms/frame at UHD 10-bit.

`rawstats.rs` removes the filter bottleneck: `compute_frame` is 9 ms/frame at
UHD on one core (4.8 ms without the frame difference) and frames are
independent, so on a many-core workstation the stats use a few cores and the
pass is decode-bound. That is why there is no GPU backend: at that cost the
statistics are already free relative to decoding, and a GPU path would only
add a platform-specific build dependency.

The text output of `signalstats` (`metadata=print`) is also the less stable
interface of the two: it is a filter's human-readable dump, and we already
had to work around one quirk of it (§7, `YBITDEPTH`).

## 2. What exists

`src-tauri/src/engine/rawstats.rs`:

- `RawFormat::for_source(pix_fmt, w, h)` — picks the planar YUV format
  ffmpeg is asked to emit (keeps source depth and subsampling; RGB sources
  become 4:4:4 at their depth) and knows plane geometry / byte ranges.
- `FrameStats` — per frame, in native code values: `y/u/v_min|max|avg`,
  `y/u/v_dif` (mean |Δ| vs previous frame), `tdif[0..4]` (mean |Δ| of a
  32×18 luma thumbnail vs frames 1..4 back, normalized 0..1).
  `FrameStats::to_json(display_frame, tc_fmt)` produces the frame object the
  rest of the engine consumes.
- `run_raw_stats(file, &fmt, hwaccel, total_frames, progress, process_handle)
  -> Result<Vec<FrameStats>, String>` — spawns ffmpeg, reads frames into a
  bounded buffer pool, computes on a worker pool (one frame per worker,
  previous frame pinned for `dif`), reorders in a collector that also fills
  in `tdif` and emits progress, reports ffmpeg failures with a stderr tail.
  `VCOMPARE_RAWSTATS_PROFILE=1` prints where the reader spent its time.
- Unit tests for geometry, 8/10-bit stats, thumbnail distance, and frame
  reading (EOF / truncation).

Validated against `signalstats` on the synthetic flash clip (§8): max
deviation 0.0005 over all min/max/avg/dif fields (its print rounding).

The Linux-only `F_SETPIPE_SZ` call made no measurable difference on the
test box (decode-bound). It is harmless but is the one non-portable line;
remove it, or keep it behind its `cfg` — either is fine. If removed, drop
the `libc` use from this file (the crate still needs `libc` elsewhere).

## 3. Non-negotiables

- **Output contract is fixed**: the `FrameStats` fields above, native code
  values, `tdif` normalized. Tests enforce the geometry and maths.
- **Decode stays in the ffmpeg CLI** over a rawvideo pipe. Do not link
  libavcodec. Hardware decode only through ffmpeg's own `-hwaccel` (§6).
- **`std` only** in the engine. No GPU, no SIMD crates (the plain loops
  vectorize under `--release`; measure before adding anything).
- The engine module (`src-tauri/src/engine/`) stays tauri-free so it can be
  built and tested without GTK/WebKit (§8).
- Package installs in docs or scripts: never add `-y` / auto-confirm flags.

## 4. Steps (each is a commit; each ends green)

1. **Probe plumbing.** `analysis::prepare_single_file` returns
   `(tc_fmt, start_frame, total_frames, bit_depth)`; extend it to also return
   `pix_fmt`, `width`, `height` and `codec_name` (a small struct is cleaner
   than a 7-tuple). `probe::get_video_info` already has width/height/pix_fmt;
   codec comes from the ffprobe JSON (`find_video_stream` → `codec_name`).

2. **Wire into levels mode.** In `bridge::_run_levels_analysis`: build
   `RawFormat::for_source`, choose hwaccel per §6, call `run_raw_stats`, map
   with `FrameStats::to_json`. Diagnostics lines: `stats: native (N workers)`,
   `decode: <codec> [hwaccel]`, `raw: <pix_fmt> <w>x<h>`. Then delete
   `analysis::run_levels_analysis`, `parse_signalstats_metadata` and
   `parse_signalstats_text` (and the `regex` import if nothing else uses it
   in that file). `cache::save_diff_analysis` is reused as-is; add
   `"stats_engine": "native"` to the result JSON so caches are
   distinguishable.

3. **Metrics list and frontend.** `state::apply_levels_result` advertises
   `y/u/v_min, y/u/v_max, y_avg, u_avg, v_avg, y_dif, u_dif, v_dif`
   (`sat_avg` is gone; `tdif_*` stay in the frame JSON but are not chart
   metrics). Update `FrameTable.COLUMN_CONFIG`, `MetricChart.METRIC_CONFIG`
   and `App.fmtMetricLabel`: replace `sat_avg` with `u_avg`/`v_avg`
   (precision 1, width 70px, colours in the existing U/V families).
   Keep the old names loadable: caches from the `signalstats` era carry
   `sat_avg` and no `tdif_*`; `apply_levels_result` already only advertises
   what is present — keep that behaviour and extend the presence check to
   `u_avg`.

4. **Detector uses thumbnail differences.** In
   `events::detect_flashes_levels` the "neighbours agree" test is a
   mean-luma + saturation proxy. With `tdif_k` present: for a run `[a..=b]`
   of length `L`, the frames on either side are `L+1` apart, so
   `at_cut = tdif_{L+1}(b+1) > neighbour_tol` (start at 0.06; tune on real
   footage). When `tdif_*` is absent (old caches) fall back to the proxy,
   using `u_avg`/`v_avg` distance in place of `sat_avg` where available.
   `max_run` is 3 so `L+1 ≤ 4 = TDIF_DEPTH`; if `max_run` is set higher,
   clamp to the proxy for longer runs. Tests for both paths; the existing
   synthetic tests must keep passing (give the test helper a `tdif` field).

5. **Progress and cancellation.** Progress already comes from the
   collector. Confirm Quit still kills ffmpeg (`FfmpegHandle` holds the
   child; `run_raw_stats` stores it the same way). Make sure a killed
   ffmpeg produces the "killed externally" error rather than a hang: the
   reader hits EOF on the pipe, the collector drains, then `wait()` runs.

6. **README.** "Modes" table: `levels` → "native stats (ffmpeg decode →
   raw pipe)". Mention the new metrics.

7. **Phase 2 stats (separate PR, optional).** Cheap additions inside
   `compute_frame`, all pure Rust over data already in the buffer:
   - per-plane 1024-bin histogram → `y_low`/`y_high` (10th/90th
     percentiles, what `signalstats` called YLOW/YHIGH);
   - out-of-legal-range sample counts (luma below 16/235-scaled, chroma
     outside 16/240-scaled for the depth) → `y_oor_lo`, `y_oor_hi`,
     `c_oor` — the actual broadcast-QC question levels mode exists for.
   Budget: a histogram pass over a UHD plane is ~3–5 ms single-threaded;
   still far below decode cost.

Later, separate plans:
- **Chunked parallel decode** for intra-only codecs: K ffmpeg processes on
  frame ranges (`-ss` before `-i` + `-frames:v`), each piped to the same
  stats machinery, one-frame overlap so `dif`/`tdif` are continuous across
  chunk boundaries. This is the lever once a single ffmpeg's decode threading
  flattens (~8–16 threads). Also applies to `diff`/`vmaf`.
- **Native `diff` mode**: two pipes → RMS per plane (what the `psnr` filter
  produces) and later SSIM, replacing the serial `psnr`/`ssim` filters with
  the same worker-pool pattern.

## 5. Memory and backpressure

- UHD 10-bit 4:2:2 = 33 MB/frame; 8K = 132 MB. In-flight frames are capped
  by `IN_FLIGHT_BYTES` (1 GiB) and `2 × workers + 2`, whichever is smaller,
  min 3. Each job pins its predecessor, hence the `+2`.
- Buffers are recycled through a channel; nothing is allocated per frame
  after warm-up.
- `read_frame` treats a truncated final frame as an error and a clean EOF
  as the end. Keep that.

## 6. ffmpeg invocation

`ffmpeg -nostdin -hide_banner [-hwaccel X] [-f concat -safe 0] -i FILE
-map 0:v:0 -an -sn -fps_mode passthrough [-frames:v N] -f rawvideo
-pix_fmt <RawFormat.pix_fmt> pipe:1`

- `-hwaccel` only when `probe::detect_hwaccel()` returns something **and**
  the codec is one hardware decoders handle: `h264, hevc, av1, vp9,
  mpeg2video, mpeg4, vc1`. For ProRes/DNxHR/FFV1 etc. leave it off (ffmpeg
  would just fall back with a warning). Never with concat manifests
  (existing rule). Without `-hwaccel_output_format`, ffmpeg downloads
  decoded frames to host memory itself, which is what the pipe needs — this
  works the same for cuda, vaapi, videotoolbox and d3d11va.
- `-fps_mode passthrough` is deliberate: emit exactly the decoded frames,
  no duplication or dropping to a nominal rate.
- Windows: `pipe:1` and `Stdio::piped()` work the same; the only Linux-only
  line is the optional pipe-size fcntl (§2).

## 7. Gotchas already hit

- `signalstats` `YBITDEPTH` is the *measured* significant bits of a frame,
  not the format depth. Bit depth must come from the probed `pix_fmt`
  (`pixfmt::parse_pix_fmt`). Fixed on `flash-frames`; the native engine
  takes depth from `RawFormat`.
- `align_to::<u16>` on plane slices: plane offsets are always even for the
  formats we emit, but the code keeps a byte-wise fallback. Keep it.
- The worker pool uses `Arc<Mutex<Receiver<Job>>>` and holds the lock across
  `recv()`; fine for one waiter at a time and avoids a crossbeam dep.
- `cargo check` of the Tauri crate needs GTK/WebKit dev libs; on a box
  without them, build and test the engine through the scratch crate in §8.
- `svelte-check` reports ~35 pre-existing false positives because
  `App.svelte` has a variable named `state` (collides with the `$state`
  rune). `npx vite build` is the real frontend check. Renaming `state` →
  `appState` would clear them.

## 8. Validation and benchmarks

Scratch crate for engine-only builds (no Tauri):

```toml
# Cargo.toml
[package] name = "enginetest" version = "0.0.0" edition = "2021"
[lib] path = "src/lib.rs"
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
regex = "1"
libc = "0.2"
```
```rust
// src/lib.rs
#![allow(dead_code)]
#[path = "/path/to/vcompare/src-tauri/src/engine/mod.rs"]
pub mod engine;
```

Test clip with known flash frames (24 fps, 10-bit 4:2:2, 120 frames: a
1-frame white at 40, 2-frame black at 70, 1-frame blue on a cut at 90):

```bash
seg() { ffmpeg -f lavfi -i "color=c=$1:s=320x180:r=24,noise=alls=$3:allf=t+u" \
        -frames:v $2 -c:v ffv1 -pix_fmt yuv422p10le "$4.mkv"; }
seg 0x606060 40 10 a; seg white 1 0 fw; seg 0x606060 29 10 b; seg black 2 0 fk
seg 0x606060 18 10 c; seg 0x2040c0 1 0 fb; seg 0xa0a0a0 29 10 d
printf "file '%s'\n" a.mkv fw.mkv b.mkv fk.mkv c.mkv fb.mkv d.mkv > list.txt
ffmpeg -f concat -safe 0 -i list.txt -c copy -timecode 01:00:00:00 clip.mkv
ffmpeg -i clip.mkv -vf "signalstats,metadata=mode=print:file=stats.txt" -f null -
```

Reference comparison: an example program in the scratch crate that parses
`stats.txt` (keep a copy of `parse_signalstats_text` in the example if it has
been removed from the engine), runs `run_raw_stats` on `clip.mkv`, and prints
the max abs deviation over `y/u/v_min|max`, `y_avg`, `y/u/v_dif` — expect
0.0005. Expected events from `events::detect_flashes_levels` on the result:
white@40 (len 1, not at cut), black@70 (len 2), content@90 (len 1, `at_cut`).

UHD benchmark clip: `ffmpeg -f lavfi -i "testsrc2=s=3840x2160:r=24,noise=alls=12:allf=t+u,format=yuv422p10le" -frames:v 120 -c:v prores_ks -profile:v 3 uhd.mov`.
Report fps on the workstation for: decode only, decode+signalstats,
decode→pipe→native. Expect native at or near decode-only; if it isn't, run
with `VCOMPARE_RAWSTATS_PROFILE=1` and look at where the reader waits.

## 9. Definition of done

- `cargo tauri dev` on the workstation: levels mode on a UHD ProRes file
  shows `stats: native (…)` in diagnostics and completes at or near that
  machine's decode-only fps.
- Flash panel works on the new output; `at_cut` uses `tdif`.
- Old `.vcompare.json` levels caches still load and the panel still works
  on them (proxy path).
- README "Modes" table updated.
- Builds with no new dependencies beyond what `Cargo.toml` already lists.
