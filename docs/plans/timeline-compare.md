# Plan: timeline mode (file vs. Resolve's SDI output)

Branch: `timeline-compare` (off `native-stats` at `81009e3`). **Status:
steps 1–7 built (2026-11-10), every gate that can pass without hardware
has.** The gates still open need the office machine: a real Resolve
timeline, the SDI loop, and `scopetap_grab` (scopetap `docs/GRAB_PLAN.md`,
not written yet). Until then the mode runs against the fakes:
`scripts/dev/fake_timeline_demo.sh`.

Additions beyond the plan, found while implementing:
- The live side goes through the file's own chroma subsampling before both
  go to 4:4:4. Without it a lossless 4:2:2 file compares at 15–21 codes RMS
  on chroma (hard colour edges), which would hide real chroma faults; with
  it, 0 lossless and ~1.2 for ProRes HQ on every plane.
- The auto threshold is centred on the clean end of the run (lower quartile
  + 6σ of the cleaner half), not the median: through the real grabber, a
  reel with two thirds of its clips slipped by a frame (24 codes over a clean
  0) flagged only a third of them with a median centre. It is also clamped to
  an absolute ceiling (3 % of peak) and the run warns when the median exceeds
  1.5 %, so a fault on every sample (wrong levels, a missing output LUT) is
  still flagged.
- Samples that couldn't be measured are events too (`kind: "error"`), so a
  timeout is reviewed like a mismatch, and markers say "not checked".
- A dry run is a checkbox in the setup (not a hidden command); it is the
  speed test of step 3.
- Resolve sync switches itself on when a run's results open (unless the
  operator turned it off this session), since reviewing a flagged sample
  means seeing it on the reference monitor.
- `psnr` needs `shortest=1`, or framesync repeats the single live frame
  against the file's next frames.

Start a session with: *"Read docs/plans/timeline-compare.md, then begin at
the first step whose gate isn't ticked."* Each step is a commit and ends
green. Tick gates here and keep §9 honest.

The other half of this work is in the scopetap repo: `docs/GRAB_PLAN.md`
(branch `grab-tool`). That document owns the grabber's wire protocol; this
one does not restate it.

Design principle, unchanged from `native-stats.md`: **ffmpeg decodes and
converts, Rust orchestrates.** No GPU code in vcompare. The grabber is an
optional external helper, like mpv.

## 1. Why

Weeks of grading are spent looking at Resolve's output on SDI. The check
worth having at delivery is whether the file matches what was on the
monitor. Comparing two renders can't answer that; comparing the file to the
live SDI picture can.

A full frame-by-frame pass would be a second render, so this mode samples:
the first, middle and last frame of every clip on the tracks the operator
ticks. That catches wrong or offline media, a missing grade, a sizing
shift, a levels or tag mismatch, and an off-by-one at a cut. It does not
catch a single bad frame mid-shot; the flash detector covers that from the
file alone.

## 2. Decisions (2026-11-10)

- **Pixels come from scopetap.** `scopetap_stream --scopes` already
  publishes the looped-back SDI frames; a new `scopetap_grab --serve`
  returns the frame at a requested timecode as raw `rgb48le` on a pipe.
- **vcompare drives the playhead**, through the existing Resolve bridge.
- **Track choice is a checklist** in vcompare (enabled tracks ticked by
  default). The track-header selector state is not readable from scripting.
- **Nothing is cached per frame.** Each grabbed frame is compared and
  dropped. A UHD `rgb48le` frame is 49.8 MB; 6000 samples would be ~300 GB.
- **The operator owns Resolve's monitoring state** (bypass, wipes,
  highlight, proxy / performance modes, render cache). The mode only drives
  timecode. It reports what it saw; it does not police the setup.
- **HDR is in scope.** The SDI path and scopetap handle it.
- **Speed is unknown** and gets measured before any UI is built (scopetap
  Phase 2, then step 3 here).

## 3. What exists on this branch

| Thing | Where | Role here |
|---|---|---|
| Resolve bridge: Python helper, `SEEK` / `PING`, coalescing sender, `VCOMPARE_PYTHON` | `engine/resolve.rs`, `engine/resolve_bridge.py` | gains `TRACKS` and `CLIPS`; `SEEK` is used as is |
| Diff chain: `choose_diff_pixfmt` (upconvert to 4:4:4, higher depth), scale the larger side down with Lanczos, `psnr` filter, `parse_psnr_stats` | `bridge.rs`, `engine/pixfmt.rs`, `engine/analysis.rs` | the per-sample compare reuses all of it |
| Timecode arithmetic incl. drop-frame | `engine/timecode.rs` | timeline frame ⇄ TC ⇄ file frame |
| Job / cancel bookkeeping for child processes | `engine/process.rs` | the grabber, bridge and per-sample ffmpeg attach to a `Job` |
| Event list with accept / reject verdicts, `.reviews.json`, CSV and marker-EDL export | `engine/events.rs`, `engine/state.rs`, `EventPanel.svelte` | flagged samples are events |
| Frame previews as JPEG over a pipe | `engine/thumbs.rs` | file-side thumbnail for a flagged sample |
| mpv single-file view, Resolve sync on the cursor (`r`) | `engine/mpv.rs`, `App.svelte` | reviewing a flagged sample |
| `.vcompare.json` cache, recents | `engine/cache.rs` | results persist as a fourth mode, `timeline` |

## 4. How a run works

```
resolve_bridge.py ──TRACKS/CLIPS──▶ sample list (timeline TC, sorted, deduped)
        ▲                                   │
        │ SEEK tc                           ▼ for each sample
   vcompare (Rust) ───GRAB tc──▶ scopetap_grab ──rgb48le──▶ ffmpeg stdin ─┐
        │                                                                 ├─ psnr ─▶ stats
        └──────────── file, -ss to the same TC ──────────▶ ffmpeg input ──┘
```

1. **Tracks.** `TRACKS` lists video tracks: index, name, enabled, item
   count. The UI shows them as checkboxes.
2. **Clips.** `CLIPS 1,3` returns the items on those tracks: track, name,
   start frame, end frame, plus the timeline's start frame, start timecode,
   frame rate and drop-frame flag.
3. **Samples.** Per item: first frame, middle frame, last frame. Convert to
   timecode, dedupe across tracks (two tracks cutting on the same frame give
   one sample), sort ascending. Drop samples outside the file's timecode
   range and count them as `not_in_file`.
4. **Per sample:**
   1. `SEEK <tc>` on the bridge (direct, not through the coalescing sender;
      a run must not skip seeks).
   2. `GRAB <tc>` on the grabber. It returns once the SDI picture at that
      timecode has gone quiet, with the frame and its metadata.
   3. Hand the bytes to a worker and move straight on to the next seek. The
      worker runs one ffmpeg: input 0 is the file at `-ss` for that
      timecode, `-frames:v 1`; input 1 is rawvideo on stdin. Filter graph as
      in `run_diff_analysis`, one frame, `psnr` stats to a temp file, parsed
      with `parse_psnr_stats`.
   4. Record the sample. Free the frame.
5. **Finish.** Compute the run's threshold, flag outliers, save the cache.

Workers are bounded (start at 4) and so is the queue of frames waiting for
one, so memory stays at a few frames however long the timeline is. If the
queue is full the seek loop waits.

### File mapping

Timecode sync only: file frame = sample TC − file start TC, using the
existing `tc` path in `prepare_comparison`. A file with no timecode is
treated as starting at the timeline's start timecode, with the existing
manual offset to correct it.

### The live side's colour conversion

The grabber's frame is full-range RGB, 0..65535, already interpreted from
the SDI codes. The file is usually limited-range Y'CbCr. `format=` alone
would let swscale pick its default matrix (BT.601), which is wrong for
everything we deliver. The live chain is therefore explicit:

```
[1:v]scale=in_range=full:out_range=<file range>:out_color_matrix=<file matrix>:flags=lanczos,format=<target>
```

with `<file matrix>` and `<file range>` from ffprobe's `color_space` and
`color_range` (defaulting to bt709 / tv when untagged, bt2020nc when the
file's primaries say 2020). If the comparison needs scaling, it goes in the
same `scale` so there is one resample per side.

No transfer-function or gamut conversion is applied. The assumption is that
the monitor was showing the deliverable's colour space. The grabber reports
the SDI tags (`colorspace`, `eotf`) and the file has its own; **if they
disagree the run says so up front** and carries on.

### Threshold

Lossy codecs and 4:2:2 mean nothing is ever zero. Fixed thresholds would
need tuning per codec, so the default is relative to the run: flag a sample
when its `diff_y` exceeds `median + k · MAD` of the run (k adjustable live,
like the flash detector's sensitivity), with an absolute floor so a perfect
run flags nothing. Detection reruns over the cached samples without
touching Resolve.

## 5. Results

One row per sample:

| Field | Notes |
|---|---|
| `frame`, `timecode` | timeline timecode |
| `track`, `clip`, `pos` | `first` / `mid` / `last`; clip name from Resolve |
| `diff_y`, `diff_u`, `diff_v` | RMS code difference, same meaning as diff mode |
| `status` | `ok`, `flagged`, `grab_timeout`, `not_in_file`, `decode_error` |
| `waited_ms`, `refines`, `clamped` | from the grabber, for diagnostics |

Run header: project / timeline name, ticked tracks, grabber's `levels`,
`colorspace`, `eotf`, the file's tags, any mismatch warning, sample counts
by status, elapsed time.

Cache: `.vcompare.json` next to the file, `mode: "timeline"`. Opens from
Recent Analyses without Resolve running.

## 6. UI

- A fourth mode, **Timeline**, offered only when `scopetap_grab` is on the
  PATH and the Resolve bridge connects. Otherwise the button is disabled
  with the reason in its tooltip.
- Pick the file; connect; tick tracks; Start. Progress shows samples done /
  total, the current clip, and a running ETA from measured `waited_ms`.
- Results reuse the chart (x = timeline timecode, one point per sample),
  the table (one row per sample), and the event panel for flagged samples
  with the existing accept / reject verdicts and exports.
- **Reviewing a flagged sample:** selecting it seeks Resolve (existing
  `resolve_seek`) so the timeline side is on the reference monitor, and
  seeks mpv to the same frame of the file. A file-side thumbnail comes from
  `thumbs.rs`. No live-side image is stored in v1.
- `Esc` cancels: kill the workers, stop seeking, leave Resolve parked where
  it is, save nothing.

## 7. Steps

### 1. Bridge: `TRACKS` and `CLIPS`

Extend `resolve_bridge.py` and the Rust side. The helper answers with one
line of JSON after `OK ` so item names with spaces survive. Extend the
stub-Resolve test in `resolve.rs` to cover both commands.

Things to verify against the real API and write in §9:
`TimelineItem.GetEnd()` exclusive or inclusive; how compound and nested
clips, generators, titles and transitions appear in `GetItemListInTrack`;
the settings keys for frame rate and drop-frame.

**Gate**
- [x] Unit tests pass with the stub (`resolve::tests::session_tracks_clips_seek`).
- [ ] Against a real timeline, the item list for a ticked track matches the
      Edit page cut for cut (spot-check 20 cuts incl. the first and last).

### 2. Sample planner

`engine/timeline.rs`: items → samples (first / mid / last, dedupe, sort,
file-range filter). Pure function, table-driven tests incl. one-frame
clips (one sample), two-frame clips (two), drop-frame timelines, and a
timeline that starts at 00:59:58:00 with a file that starts at 01:00:00:00.

**Gate**
- [x] Tests pass (`timeline::tests`, 8).

### 3. Grabber client and a dry run

`engine/grab.rs`: spawn `scopetap_grab --serve`, parse `READY` / `OK` /
`ERR`, read exactly `nbytes`. Attach the child to the `Job`.

A dry run (checkbox in the setup; `analyze` with `dry_run: true`) does
seek + grab for every sample and discards the pixels, reporting `waited_ms` min / median /
p95 / max and total time. This is the speed test on real projects before
any compare code exists.

**Gate**
- [ ] Dry run completes on a real reel with zero wrong-timecode grabs.
- [ ] Numbers recorded in §9. Decide here whether seeking is good enough or
      the playback pass (§8) is needed first.

### 4. Per-sample compare

`engine/timeline_compare.rs`: the worker described in §4. Build the two
chains from the existing diff negotiation plus the explicit live-side
`scale`. Unit-test the chain builder for 709 / 2020, tv / pc, scaled and
unscaled.

**Gate**
- [x] Synthetic test, no Resolve needed (`timeline_compare::tests::synthetic_compare`): generate colour bars with ffmpeg,
      encode ProRes 422 HQ, feed the *same* bars as rgb48le on stdin as the
      live side. `diff_y/u/v` are at codec-noise level. Repeat with a
      bt2020nc file. Repeat with the matrix deliberately wrong and confirm
      the diff jumps (proves the test can fail).
- [ ] Real test: render a short timeline, run the compare against the SDI
      loop. Record the baseline diff per codec in §9.
- [ ] Break things on purpose and confirm each is flagged (done with the
      fakes for a one-frame slip and wrong data levels,
      `timeline_run::tests::end_to_end_with_fakes`; still to do on real
      renders, plus bypassed grade and offline clip): a clip with its
      grade bypassed in the render, a clip slipped by one frame, a clip
      taken offline, a render with the wrong data levels.

### 5. Run orchestration

`analyze` gains `mode: "timeline"`: connect, plan, loop, bounded workers,
progress events, cancel, threshold, summary, cache save / load, recents.

**Gate**
- [ ] A full reel runs end to end, twice, with the same flags both times
      (with the fakes: yes; real reel: to do).
- [ ] Cancel mid-run leaves no ffmpeg, grabber or Python process behind
      (grabber and ffmpeg are attached to the job; check on hardware).
- [ ] RSS stays flat across the run.

### 6. Frontend

Mode button, track checklist, progress, results views, review flow (§6).

**Gate**
- [ ] The whole flow by keyboard and mouse on the office machine (done on
      a virtual display against the fakes: setup, run, review, dry run,
      reopen from recents).
- [x] Reopen a saved timeline analysis with Resolve closed.

### 7. Docs

README: the mode, its requirements (Linux, `scopetap_stream --scopes`
running on the SDI loop, `scopetap_grab` on the PATH, Resolve scripting
on), and an operator checklist of Resolve state that will show up as
differences: bypass, wipes / split screen, highlight, timeline proxy or
performance mode, a stale render cache, viewer-only LUTs on the SDI output.

## 8. Deferred

- **Playback pass.** Play the timeline and take the sample frames as they
  go by (the grabber's `WATCH`, also deferred there), then seek for any
  that were dropped. Real-time cost regardless of cut count.
- **Live-side evidence.** Save a small JPEG pair (or the full frame, capped)
  for flagged samples so a failure can be shown without Resolve open.
- **More samples per clip**, or every Nth frame of long clips.
- **Native per-sample metrics** (max absolute difference, worst region) once
  the native `diff` plan in `native-stats.md` §4 lands; the per-sample
  ffmpeg then only decodes and converts.
- **Float live frames** (`gbrpf32le`) if the grabber's `clamped` counts show
  out-of-range signal that matters.

## 9. Known false positives, and facts learned

Expected differences between a parked frame and a rendered one, to be
confirmed or struck off as they are tested:

- temporal noise reduction and anything else that looks at neighbouring
  frames;
- optical-flow retimes and speed warp;
- effects that are not deterministic per frame;
- 12-bit SDI is captured as 10-bit, so a 12-bit file is compared at 10.

Facts (fill in as gates pass):

- `GetEnd()` semantics: not verified. The planner uses `GetDuration()` when
  present and only falls back to `GetEnd() - GetStart()` (exclusive end).
- Item types seen in `GetItemListInTrack`: not verified. Disabled items
  (`GetClipEnabled() == False`) are skipped and counted.
- Dry-run timing (reel, samples, median / p95 `waited_ms`, total): fakes
  only (76 ms/sample on a 2-core VM, meaningless for hardware).
- Baseline diff by codec (synthetic, swscale encode, 10-bit codes RMS):
  ProRes 422 HQ smooth picture 1.2 / 1.2 / 1.1 (Y/U/V); lossless 4:2:2 0;
  heavy per-frame noise (worst case) ~13 / 11 / 11. A one-frame slip on
  noisy content ~87, BT.601 read as 709 ~68, 2020 read as 709 ~31.
- Against the real `scopetap_grab` (scopetap `grab-tool` at `4e45e83`, built
  with its no-GPU rig, `fake_stream` driven by `scripts/dev/scopetap_rig_sdi.py`
  from the fake Resolve's playhead): protocol, READY, `;`/`:` timecodes and
  pixels all line up; a clean clip compares at 0.0 codes. Opt-in test:
  `VCOMPARE_SCOPETAP_RIG=<dir> cargo test -- --ignored real_grabber_rig`.
- Seek cost on Color 02 (from scopetap's bench): `SetCurrentTimecode` blocks
  ~317 ms, whole sample ~500 ms median, so ~1.5 s per clip: a 253-clip reel
  is ~6–7 min. With the Project Manager open Resolve refuses every seek.
- File ranges: prores_ks in MOV doesn't keep `color_range`, so ProRes files
  usually show as "limited (untagged, assumed)". Correct for video-range
  renders; a full-range ProRes render would need a tag or an override.
