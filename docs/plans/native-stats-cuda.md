# Plan: CUDA-backed native stats engine for levels (and later diff)

Branch: `native-stats` (off `flash-frames`). Status: CPU reference
implementation committed in `src-tauri/src/engine/rawstats.rs`, not yet wired
into the app. This document is the implementation plan for the CUDA backend
and the wiring. It is written for whoever picks the work up next; read it top
to bottom before touching code.

## 1. Why

`levels` mode runs ffmpeg's `signalstats` filter. Measured on UHD ProRes 422 HQ
10-bit (120 frames, ffmpeg 6.1, 2-core box):

| Pipeline | ms/frame | fps |
|---|---|---|
| decode only, 1 thread | 135 | 7.4 |
| decode only, 2 threads | 72 | 13.9 |
| decode + `signalstats`, 2 threads | 154 | 6.5 |
| decode + `signalstats`, `-filter_threads 2` | 159 | 6.3 |
| decode → rawvideo pipe → discard | 89 | 11.3 |
| decode → rawvideo pipe → `rawstats.rs` (CPU, 2 workers) | 98 | 10.2 |

`signalstats` is effectively single-threaded (only its hue/sat pass is
slice-threaded) and costs more per UHD frame than decoding. It caps the whole
pass at ~10 fps regardless of core count, so a one-hour UHD program takes
hours on a 32-core workstation. Decoding threads well; piping raw frames out
of ffmpeg is cheap (~17 ms/frame at UHD 10-bit).

The CPU `rawstats.rs` already removes the filter bottleneck: `compute_frame`
costs 9 ms/frame at UHD on one core (4.8 ms without the frame difference), so
on 32 cores it is decode-bound. The CUDA backend exists because:

1. Vladimir's workstations have NVIDIA GPUs and he writes CUDA already (OFX
   plugins), so kernels in CUDA C are the natural form.
2. At ~1–2 ms/frame GPU time the stats become free, which leaves CPU cores
   for decoding — the next bottleneck — and makes the later additions (per
   plane histograms, out-of-range counts, GPU RMS diff / SSIM for `diff`
   mode) cheap.
3. The same resident-frame machinery is the foundation for a GPU `diff` mode
   that replaces the (also serial) `psnr`/`ssim` filters.

## 2. Non-negotiables

- **Output contract is fixed.** `run_raw_stats(...) -> Vec<FrameStats>` with
  the fields in `rawstats.rs` (`y/u/v_min|max|avg`, `y/u/v_dif`, `tdif[0..4]`),
  in native code values of the source pixel format. CPU and CUDA backends must
  produce the same numbers: integer fields exactly, float fields within 1e-3
  code values (sum ordering). Tests enforce this.
- **CPU backend stays** as the fallback when no CUDA device/driver is present
  or the `cuda` cargo feature is off. Backend selection is runtime, never a
  compile-time-only choice, and the app must build without the CUDA toolkit.
- **Decode stays in ffmpeg** (CLI, rawvideo pipe). Do not pull libavcodec into
  the process in this phase. ProRes/DNxHR (most inputs) have no NVDEC path
  anyway; for H.264/HEVC/AV1 pass `-hwaccel cuda` (ffmpeg downloads to host
  memory automatically when no `-hwaccel_output_format` is given) — see §6.
- **Keep the validation approach**: compare against `signalstats` on the
  synthetic flash clip, and CPU vs CUDA on synthetic frames. The example
  programs in §8 show how.
- Package installs in docs or scripts: never add `-y` / auto-confirm flags.

## 3. Architecture

```
ffmpeg ─stdout (rawvideo, planar YUV)─▶ reader thread
                                          │  pinned host buffers (ring)
                                          ▼
                                   cudaMemcpyAsync H2D (stream s[i%N])
                                          ▼
                     kernels: plane_reduce (min/max/sum)   ─┐
                              plane_absdiff(prev, cur)      ├─ per frame, ordered
                              luma_thumbnail 32×18          │
                              (phase 2: histogram, OOR cnt) ┘
                                          ▼
                       small results struct D2H (batched every K frames)
                                          ▼
                            collector: tdif from thumbnails, progress
```

Frames are processed **in order** on the GPU (the frame difference needs the
previous frame resident), but H2D copies for frame *i+1*, *i+2* overlap with
compute on *i* via 2–3 streams + events. Keep two device frame buffers (cur,
prev) and swap pointers; thumbnails live in a device ring of 5 so `tdif` can
also be computed on-device if convenient, or copied back as 576 floats/frame
and differenced on the CPU collector exactly as the CPU backend does (simpler;
do this first).

### Rust side

- Crate: `cudarc` (driver API + NVRTC optional). Use the **driver API with
  precompiled PTX**, not NVRTC at runtime: user machines need only the driver.
  Build the `.cu` to PTX in `build.rs` with `nvcc -ptx` when the `cuda`
  feature is on; check the generated `.ptx` into `src-tauri/cuda/` as well so
  a machine without nvcc can still build the feature (build.rs prefers a
  fresh nvcc build, falls back to the committed PTX with a warning).
  Target a conservative arch for the PTX (`-arch=sm_75`), JIT handles the
  rest.
- Module layout:
  - `engine/rawstats.rs` — keep: `RawFormat`, `FrameStats`, pipe reader,
    buffer pool, collector, CPU `compute_frame`. Refactor the driver so the
    worker-pool section becomes one implementation of a small trait:
    ```rust
    trait StatsBackend {
        /// Called in frame order. Returns stats (tdif zeroed) + thumbnail.
        fn process(&mut self, index: u64, frame: &[u8]) -> Result<(FrameStats, Vec<f32>), String>;
    }
    ```
    CPU backend wraps the existing thread pool (it is already out-of-order
    safe because the collector reorders). CUDA backend is sequential by
    nature; the reader hands it frames directly from the pool.
  - `engine/cuda_stats.rs` (behind `#[cfg(feature = "cuda")]`) — device
    init, PTX load, pinned buffers, streams, kernel launches, D2H.
  - `src-tauri/cuda/stats.cu` — kernels.
- Backend selection: `Backend::detect()` → CUDA if feature on, a device
  exists, PTX loads; else CPU. Expose the chosen backend in the diagnostics
  lines so it shows in the analysis panel ("stats: cuda (RTX 6000 Ada)" /
  "stats: cpu (31 workers)").

### Kernels (phase 1)

Inputs are planar, 8-bit (`u8`) or 16-bit little-endian (`u16`), three planes
with the geometry from `RawFormat::plane_dims`. Template on sample type.

1. `plane_reduce`: per-plane min, max, sum. Block-level reduce in shared
   memory, then one `atomicMin/atomicMax/atomicAdd(unsigned long long)` per
   block into a 3×3 result array. Grid-stride loop, 256 threads, vectorized
   loads (`uint4` / `ushort8`-style) are worth it: the frame is 33 MB.
2. `plane_absdiff`: sum |cur − prev| per plane, same reduction pattern.
   Skip for frame 0 (fill 0).
3. `luma_thumbnail`: 32×18 cells, box average. One block per cell is
   simplest (576 blocks, each strides its cell's rows); write normalized
   `float` to a 576-entry array. Must match `rawstats::luma_thumbnail` cell
   boundaries exactly: `x0 = tx*w/32`, `x1 = (tx+1)*w/32`, `ty = y*18/h` (the
   CPU code derives `ty` per row, so cell row ranges are
   `y in [y0, y1)` where `y0 = ceil(ty*h/18)`… just reuse the same per-row
   `ty = y*18/h` mapping to stay bit-identical in which pixels go where).
4. Result struct D2H: `{min[3], max[3], sum[3], dif_sum[3], thumb[576]}` per
   frame; batch K=8 frames per copy or just copy per frame (it's ~2.5 KB,
   latency-bound either way — measure, don't guess).

Phase 2 kernels (do after phase 1 is validated and wired):

5. `plane_histogram` (256 bins for 8-bit, 1024 for ≥10-bit via `>>` shift):
   gives YLOW/YHIGH (10th/90th percentile like signalstats) and lets the UI
   show a per-frame waveform-ish distribution later.
6. `out_of_range_count`: count luma samples below/above the legal range for
   the depth (16/235 scaled) and chroma outside 16/240 scaled — this is the
   actual broadcast QC question `levels` mode exists to answer.

Both are reductions over data already resident, so they are ~free.

## 4. Steps (each is a commit; each ends green)

1. **Backend trait refactor (CPU only).** Introduce `StatsBackend`, move the
   thread-pool into `CpuBackend`, keep `run_raw_stats` signature. Run the
   existing tests + the §8 validation; numbers unchanged.
2. **CUDA scaffolding.** `cuda` feature, `cudarc` dep, `build.rs` PTX step,
   `cuda_stats.rs` with device detection and a trivial kernel round-trip
   test (ignored unless a device exists).
3. **Kernels 1–3 + CudaBackend.** Unit test: generate the same synthetic
   frames as `rawstats::tests`, run both backends, assert equality per §2.
   Validation: §8 against `signalstats` on the flash clip (expect max
   deviation 0.0005).
4. **Wire into levels mode.**
   - `analysis::prepare_single_file` already returns `(tc_fmt, start_frame,
     total_frames, bit_depth)`; extend it to also return `pix_fmt`, `width`,
     `height`, `codec_name` (from the ffprobe JSON — `probe::get_video_info`
     has width/height/pix_fmt; add codec via `find_video_stream`).
   - `bridge::_run_levels_analysis`: build `RawFormat::for_source`, pick
     hwaccel per §6, call `run_raw_stats`, map with `FrameStats::to_json`.
     Delete `analysis::run_levels_analysis` and the signalstats parser only
     after the new path is in (keep `parse_signalstats_text` if it is still
     used by a test; otherwise remove it and the `regex` use with it).
   - `state::apply_levels_result`: metrics list becomes
     `y/u/v_min, y/u/v_max, y_avg, u_avg, v_avg, y_dif, u_dif, v_dif`
     (drop `sat_avg`; `tdif_*` stay in the frame JSON but are not advertised
     as chart metrics).
   - Frontend: `FrameTable.COLUMN_CONFIG`, `MetricChart.METRIC_CONFIG`,
     `App.fmtMetricLabel`: replace `sat_avg` with `u_avg`/`v_avg`.
   - Cache compatibility: caches from the `signalstats` era have `sat_avg`
     and no `tdif_*`; the detector must keep working on them (see step 5).
5. **Detector uses thumbnail differences.** In `events::detect_flashes_levels`
   the "neighbours agree" test is currently a mean-luma + sat proxy. With
   `tdif_k` available: for a run `[a..=b]` of length `L`, the frames on
   either side are `L+1` apart, so `at_cut = tdif_{L+1}(b+1) > tol` (tol ≈
   0.06 normalized — tune on real footage; keep `neighbour_tol` as the
   param). Fall back to the old proxy when `tdif_*` is absent (old caches).
   Add tests for both paths. `max_run` is 3 so `L+1 ≤ 4` = `TDIF_DEPTH`;
   if `max_run` is raised above 3 the detector must clamp and fall back.
6. **Progress/diagnostics.** Progress comes from the collector (frames
   completed / total). Add the backend line to diagnostics. Make sure Quit
   still kills ffmpeg (`FfmpegHandle` holds the child — unchanged).
7. **Phase 2 kernels** (histogram, out-of-range) + `y_low`/`y_high`,
   `y_oor_lo`/`y_oor_hi` fields, exposed as metrics. Optional, separate PR.

Later, separate plans: chunked parallel decode for intra-only codecs (K
ffmpeg processes on time ranges, one-frame overlap for `dif`); GPU `diff`
mode (two pipes → resident ref/dist → RMS per plane, later SSIM) replacing
`psnr`/`ssim` filters.

## 5. Memory and backpressure

- Reader → GPU: use **pinned** host buffers (`cudarc` `alloc_pinned` / the
  driver `cuMemHostAlloc`) so H2D is DMA and async. Ring of 3–4 host buffers
  is enough; the pipe read fills buffer *i+2* while *i* copies and *i−1*
  computes. Budget: UHD 10-bit 4:2:2 = 33 MB/frame; 8K = 132 MB. Keep the
  existing 1 GiB in-flight cap logic.
- Device: 2 frame buffers (cur/prev), ~70 MB at UHD. Thumbnail ring + result
  structs are negligible.
- Reads must still handle a truncated final frame as an error (`read_frame`
  already does) and clean EOF.

## 6. ffmpeg invocation

Existing command (keep): `ffmpeg -nostdin -hide_banner [-hwaccel cuda] [-f
concat -safe 0] -i X -map 0:v:0 -an -sn -fps_mode passthrough [-frames:v N]
-f rawvideo -pix_fmt <RawFormat.pix_fmt> pipe:1`.

- Pass `-hwaccel cuda` only when `probe::detect_hwaccel()` says cuda **and**
  the codec is NVDEC-capable: `h264, hevc, av1, vp9, mpeg2video, mpeg4,
  vc1`. For ProRes/DNxHR/ffv1/… leave it off (ffmpeg would warn and fall
  back; no benefit). Never with concat manifests (existing rule).
- `-fps_mode passthrough` is deliberate: emit exactly the decoded frames.
- Pixel format: `RawFormat::for_source` keeps the source depth and
  subsampling; RGB sources become 4:4:4 YUV at the same depth (ffmpeg
  converts; the old filter only handled YUV either way).
- The pipe size bump (`F_SETPIPE_SZ`, Linux only) is in place; it made no
  measurable difference on the 2-core test box because that box is
  decode-bound. Keep it; it is harmless.

## 7. Gotchas already hit

- `signalstats` `YBITDEPTH` is the *measured* significant bits of a frame,
  not the format depth. Bit depth must come from the probed `pix_fmt`
  (`pixfmt::parse_pix_fmt`). Already fixed on `flash-frames`.
- `align_to::<u16>` on plane slices: plane offsets are always even for the
  formats we emit, but the code keeps a byte-wise fallback. Keep it.
- The worker pool uses `Arc<Mutex<Receiver<Job>>>` and holds the lock across
  `recv()`; that is fine (one waiter at a time) and avoids a crossbeam dep.
  Don't "fix" it.
- `cargo check` of the Tauri crate needs GTK/WebKit dev libs; on a box
  without them, compile and test the engine via a scratch crate that
  includes the engine sources by path (see §8). The engine module is
  tauri-free and must stay so.
- `svelte-check` reports ~35 pre-existing false positives because
  `App.svelte` has a variable named `state` (collides with the `$state`
  rune). `npx vite build` is the real frontend check. Renaming `state` →
  `appState` would clear them if someone wants to.

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
# + cudarc when testing the cuda feature
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

Reference comparison (an example program in the scratch crate; the one used
so far parsed `stats.txt` with `analysis::parse_signalstats_text`, ran
`run_raw_stats` on `clip.mkv`, and printed the max abs deviation over
`y/u/v_min|max`, `y_avg`, `y/u/v_dif` — expect 0.0005). Expected events from
`events::detect_flashes_levels` on the result: white@40 (len 1, not at cut),
black@70 (len 2), content@90 (len 1, `at_cut`).

UHD benchmark clip: `ffmpeg -f lavfi -i "testsrc2=s=3840x2160:r=24,noise=alls=12:allf=t+u,format=yuv422p10le" -frames:v 120 -c:v prores_ks -profile:v 3 uhd.mov`.
Report fps for: decode only, decode+signalstats, decode→pipe→CPU backend,
decode→pipe→CUDA backend, on the real workstation (3975WX + GPU). Expect the
CUDA path to sit at the decode/pipe ceiling; if it doesn't, profile the H2D
overlap first.

`VCOMPARE_RAWSTATS_PROFILE=1` makes the reader print where it spent time
(waiting for a buffer / reading the pipe / sending jobs).

## 9. Definition of done

- `cargo tauri dev` on the workstation: levels mode on a UHD ProRes file
  shows "stats: cuda (…)" in diagnostics and completes at or near the
  decode-only fps of that machine.
- Same file through the CPU backend (force via `VCOMPARE_STATS_BACKEND=cpu`)
  gives identical integer fields and float fields within 1e-3.
- Flash panel works on the new output; `at_cut` uses `tdif`.
- Old `.vcompare.json` levels caches still load and the panel still works
  on them (proxy path).
- README "Modes" table updated (`levels` no longer says `signalstats`).
