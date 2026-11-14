# Plan: full-width chart, tabs below, and more detectors

Branch: `detector-tabs` (off `timeline-compare` at `7a03e1e`). **Status:
plan only, nothing built.**

Five parts. 1–3 are one piece of work (the UI rework and the engine
scaffolding it needs). 4 and 5 are new detectors that drop into it; 5 needs
a re-analysis of old files, 4 does not.

1. Layout: chart full width, tab strip below
2. Flash tab: thumbnails in every row, loaded lazily
3. Detector scaffolding in the engine
4. Repeated-frame detector
5. Image-match detector (Media Offline cards etc.)

## 0. What exists (read before changing)

- `src/App.svelte` `.results-layout` is a row: `.results-main` (timeline
  report, `TimelineBar`, chart at 45 % height, `FrameTable`) and a fixed
  360 px `.events-area` holding `EventPanel`.
- `EventPanel.svelte` holds the detector controls, a preview strip for the
  *selected* event only, an overview strip, and the event list (not
  virtualised). It also serves timeline mode (`timeline` prop: events are
  mismatched / unchecked samples).
- Previews: `App.svelte` calls `previewFrames(frame - 1, length + 2, 108)`
  for the selected event and caches by `frame:length`. Engine side
  (`bridge::preview_frames` → `thumbs::extract_frames`) is one ffmpeg seek
  per file per call, so two in diff mode.
- Detection: `events.rs` (`detect_flashes_levels`, `detect_flashes_diff`,
  `detect_timeline`), called from `state::detect_events`, re-run 60 ms after
  any `detectParams` change. `Event.kind` exists ("flash", and "mismatch" /
  "error" in timeline mode).
- Reviews (accept / reject) are stored in `{analysis}.reviews.json`, keyed by
  the event's first frame only.
- `rawstats.rs` computes a 32×18 luma thumbnail per frame, uses it for
  `tdif_1..4`, then drops it. Chunked runs are stitched in
  `analysis.rs` after the fact.

## 1. Layout

```
┌──────────────────────────────────────────────────────────┐
│ TimelineBar (when there is a timeline)                   │
│ MetricChart — full width                                 │
├──────────────── drag handle ─────────────────────────────┤
│ [Frames] [Flash frames 12] [Repeats 3] [Matches 0] …     │
│ active tab, full width                                   │
└──────────────────────────────────────────────────────────┘
```

- `.results-layout` becomes a column; `.events-area` goes away.
- Chart keeps its 45 % default. A drag handle between chart and tabs sets
  the split; store the fraction in `localStorage` (try/catch), clamp so
  neither side goes under 160 px.
- New `src/components/TabStrip.svelte` and a registry in
  `src/lib/tabs.ts`:

  ```ts
  interface TabDef {
    id: string;                       // "frames" | "flash" | "repeat" | "match" | "report"
    label: string;
    modes: Mode[];                    // where the tab exists
    detector?: DetectorKind;          // tabs that list events
    available?: (s: Summary) => string | null;   // null = ok, else why not
  }
  ```

  A tab that exists for the mode but cannot run (old cache without the data
  it needs) stays visible, greyed, with the reason as its tooltip and as the
  tab's body. Adding a detector later is one registry entry plus one engine
  detector; the tab body is the shared `EventList` (below) unless the
  detector needs its own controls.

- Tabs per mode:

  | mode | tabs |
  |---|---|
  | levels | Frames, Flash frames, Repeats, Matches |
  | diff | Frames, Flash frames |
  | vmaf | Frames |
  | timeline | Frames, Mismatches, Report |

  Timeline mode's `TimelineReport` moves from above the chart to the Report
  tab; its events (today's `EventPanel` in `timeline` flavour) become the
  Mismatches tab. A one-line run summary stays above the chart so a warning
  (median over 1.5 %) is not hidden behind a tab.

- Count badge on each detector tab. Detectors run for every available tab
  when an analysis opens (they are linear scans over memory), so badges are
  filled without visiting the tab.
- Active tab is remembered per mode for the session. Opening an analysis
  with flash events lands on Frames as today.
- Chart and `TimelineBar` markers show the events of the active detector
  tab; on Frames (or Report) they keep showing the last detector tab's.
  `↑ ↓ y x` act on that same detector. `1`…`4` switch tabs.

### Split `EventPanel`

- `EventToolbar.svelte`: one row of controls at the top of the tab
  (threshold slider / auto, max run, hide at-cut, hide rejected, export CSV,
  export EDL, the "N events · thr x %" summary). Per-detector controls are a
  snippet passed in.
- `EventList.svelte`: the virtualised list (fixed row height, same windowing
  as `FrameTable`), used by every detector tab.
- The overview strip is dropped: the chart and `TimelineBar` already carry
  the markers and are now directly above at full width.
- The selected-event preview strip is dropped (replaced by §2).

Gate: every mode opens with the right tabs; chart width equals the window
width; the split persists across a restart; keyboard review still works with
the Frames tab active.

## 2. Thumbnails in every row

Row, 64 px high (54 px image + padding), levels mode:

```
● 01:02:03:04  [prev][flash][next]   white flash, 1 frame      ×3.2   ✓ ✗
```

- The strip shows the frame before, every frame of the run, the frame
  after: 3 tiles for a single-frame flash, up to 5 at `max_run` 3. The
  column is sized for `max_run + 2` so the text columns line up.
- Diff mode shows two strips side by side, `REF` then `DIST`.
- Clicking a tile moves the cursor to that frame, as in today's strip.
- Tiles are requested at 108 px and shown at 54 px, as now.

### Loader (`src/lib/thumbLoader.ts`)

- **Settle delay.** Any change to the event set that comes from a parameter
  change (threshold drag, auto toggle, max run, hide at-cut) cancels the
  queue and starts a 2.5 s timer; loading begins when it fires. Constant
  `THUMB_SETTLE_MS = 2500`. The first detection after an analysis opens, and
  switching tabs, use a short delay (300 ms) instead.
- **Only what is on screen.** The queue is: the selected event, then the
  visible rows top to bottom, then one screenful below. Scrolling re-builds
  the queue after 150 ms of stillness.
- **Bounded.** At most 3 requests in flight. The engine enforces the same
  cap with a semaphore in `bridge::preview_frames`, so a second window or a
  bug in the page cannot fork a storm of ffmpegs.
- **Cached.** Keyed by `frame:count` (independent of threshold, so events
  that survive a threshold change keep their tiles). LRU, 1500 entries
  (~50 MB of data URIs). Cleared when another analysis opens.
- **Cancelling** is a generation counter: a request already in ffmpeg is
  left to finish and its result is cached, since it is still correct for
  that frame; nothing new is started for a stale generation.
- Rows show placeholder tiles of the final size until their strip lands, so
  the list never reflows.
- A failed strip shows "—" tiles and is not retried until the row leaves and
  re-enters the viewport; the first failure of a generation goes to the
  status line once.

Engine: `preview_frames` is unchanged apart from the semaphore. If the gate
below fails on seek-heavy sources, add `preview_frames_batch(requests[])`
that groups events closer than ~2 s into one ffmpeg run with a `select`
filter; do not build it up front.

Gate: on a real 90-minute ProRes file and a real IMF (J2K, concat manifest),
with ~200 events: dragging the threshold never spawns ffmpeg; 2.5 s after
release the visible rows fill within a few seconds; scrolling to the end and
back re-uses the cache. Record ms per strip for both in this file.

## 3. Detector scaffolding (engine)

- `engine/events.rs` → `engine/detect/`:
  - `mod.rs` — `Event`, `DetectResult`, `Detector` enum, shared helpers
    (`num`, `frame_num`, `frame_tc`, `resolve_threshold`, rounding), EDL/CSV
    export.
  - `flash.rs` — today's levels and diff detectors, moved unchanged with
    their tests.
  - `timeline.rs` — `detect_timeline`, moved unchanged.
  - `repeat.rs`, `matcher.rs` — §4, §5.
- Parameters become a tagged enum, one variant per detector:

  ```rust
  #[serde(tag = "detector", rename_all = "snake_case")]
  pub enum DetectRequest {
      Flash(FlashParams),      // today's DetectParams
      Repeat(RepeatParams),
      Match(MatchParams),
  }
  ```

  `detect_events(request)` replaces `detect_events(params)`. Timeline mode
  keeps answering to `Flash` (its events are still "mismatch" / "error").
- `EngineState.events` becomes a map `detector → Vec<Event>`;
  `export_events_csv` / `export_events_edl` take the detector (and an `all`
  option that merges, sorted by frame). EDL marker colour and name come from
  the kind: flash as today, repeat and match get their own.
- `Event` gains nothing new for flash. `length` stays "frames in the run".
  `subkind` is per detector (§4, §5). `score` stays "strength, ≥ 1 is a
  hit"; detectors document what it is.
- **Reviews** are keyed by `kind:frame` instead of `frame`. On load, a
  bare-number key is read as `flash:<n>` (or `mismatch:` / `error:` cannot
  be told apart from the key alone: for a `.timeline.json` analysis a bare
  key is applied to whichever event starts at that frame). Files are
  rewritten in the new form on the next verdict.
- `rpc.ts`: `DetectorKind`, `DetectRequest`, `detectEvents(request)`;
  `FlashEvent` renamed `DetectedEvent` (keep an alias until the components
  are moved over).

Gate: `cargo test` passes with the flash tests untouched apart from paths;
an existing `.reviews.json` keeps its verdicts; CSV and EDL from the flash
tab are byte-identical to before.

## 4. Repeated frames

Levels mode only (diff mode has no per-file frame differences; if wanted
there later, the psnr pass would have to carry `rawstats` for one file).
Needs nothing new in the analysis: a repeat is a frame whose difference from
the previous frame is (near) zero.

```rust
pub struct RepeatParams {
    /// Max mean |Δ| vs the previous frame, 0..1 of peak, on the same
    /// combined y/u/v signal the flash detector uses. 0 = exact.
    pub tolerance: f64,          // default 0.0
    /// Fewest repeated frames that make an event. Default 1.
    pub min_run: u32,
    /// Longest run reported. Default 5.
    pub max_run: u32,
    /// Report runs longer than `max_run` too (holds, freezes, stills).
    pub include_long: bool,      // default false
    /// Drop runs whose frames are black (y_max at or under black + a hair).
    pub skip_black: bool,        // default true
}
```

- Scan: frame `i` repeats when `d[i] <= tolerance` (and, when `tdif_1` is
  present, `tdif_1 <= max(tolerance, 1/255)`, which guards against a
  full-res difference that averages to zero by luck). A run is consecutive
  repeating frames; the event starts at the first repeated frame, `length`
  = number of repeated frames (so "1" is one frame shown twice).
- `subkind`: `"short"` (≤ `max_run`), `"hold"` (longer, only with
  `include_long`), `"black"` (only with `skip_black` off).
- `score`: for exact repeats, the run length; with a tolerance,
  `tolerance / max(d)` over the run, floored at 1.
- Exact zero is the right default for intra codecs (ProRes, J2K, DNx): a
  frame repeated in the source decodes bit-identical. For long-GOP
  deliverables a repeated frame is near zero, not zero, so the toolbar
  offers the tolerance as a slider (0–0.5 % of peak) with an "exact"
  checkbox, like the flash threshold's "auto".
- Toolbar summary also reports cadence when the events are periodic:
  if more than 80 % of the gaps between consecutive short repeats share one
  value N (±1), show "every N frames" (frame-rate conversion or pulldown
  rather than isolated faults). It is a line of text, not a filter.
- Row strip: frame before, the repeated frames (capped at 4 tiles, then
  "+n"), frame after.
- Synthetic titles, freeze frames and stills are exact repeats too. With
  `include_long` off they are simply not listed; with it on they are listed
  as holds.

Tests (synthetic frames, as in the flash tests): single repeat; run of 3;
run of 6 hidden by default and listed as a hold with `include_long`; black
run skipped; periodic repeats reported with the right period; tolerance 0
ignores a near-repeat that a small tolerance catches; old cache without
`y_dif` returns the "re-run the analysis" error.

Gate: on a real file with a known duplicated frame (make one: a ProRes
export with one frame doubled in Resolve), the event is found at the right
timecode and nothing else is listed outside titles and black.

## 5. Image match

Find frames that look like a supplied picture: Resolve's "Media Offline",
Avid's offline slide, a slate, a missing-render card.

### 5a. Keep a per-frame signature

- `compute_frame` already makes the 32×18 luma thumbnail. Add U and V at
  16×9 from the chroma planes (same box average; cost is negligible next to
  the full-plane stats). Flat coloured cards such as the red offline frame
  are told apart mostly by chroma.
- Signature = 576 + 144 + 144 = **864 bytes per frame**, 8-bit (normalized
  value × 255, rounded). `tdif` keeps using the float luma thumbnail.
- Signatures travel with `FrameStats` out of the run (a `sig: Box<[u8]>`
  beside the stats) so chunk stitching and warm-up trimming in
  `analysis.rs` handle them exactly like stats, then are written once to a
  sidecar next to the analysis: `{stem}.levels.sig`.

  ```
  magic "VCSG" | version u32 | yw,yh,cw,ch u16 | first_frame i64 | count u64
  then count × 864 bytes, frame order
  ```

  Written with `cache::write_atomic`. 2 h at 24 fps ≈ 150 MB; 45 min ≈ 56 MB.
- A "keep frame signatures" checkbox in the levels setup, default on, with
  the estimated size beside it. Off → no sidecar and the Matches tab says
  why.
- Loading an analysis memory-maps or reads the sidecar lazily, on first use
  of the Matches tab. A sidecar whose `count` / `first_frame` disagree with
  the analysis is ignored (tab reports "signatures out of date, re-run").

### 5b. References

- A reference is a named signature. Two ways to make one:
  1. **From an image file** (PNG, JPEG, TIFF, DPX, EXR; whatever ffmpeg
     reads). ffmpeg converts it to the analysed file's raw format and frame
     size (`scale` to fit, pad to the frame, explicit
     `out_color_matrix`/`out_range` matching the file), piped as one raw
     frame through the same thumbnail code. No image crate is added.
  2. **From the current frame** ("use this frame as reference"): copies the
     signature straight out of the sidecar. Exact by construction, and the
     practical route when the card has been through an output transform.
- References are kept in the app config dir as small JSON files (name,
  signature, source path, a 108 px JPEG for the row), so the usual cards are
  set up once and offered for every analysis. The Matches toolbar lists them
  with checkboxes; the detector runs every checked reference.
- Ship none. Resolve's and Avid's graphics are theirs; the user adds their
  own from a frame or a still.

### 5c. Matching

```rust
pub struct MatchParams {
    pub references: Vec<String>,   // ids of the checked references
    /// Minimum similarity, 0..1. Default 0.90.
    pub min_similarity: f64,
    /// Fewest consecutive matching frames. Default 1.
    pub min_run: u32,
}
```

- Similarity is zero-mean normalized correlation over the luma cells,
  combined with the chroma planes' (weights 0.6 Y, 0.2 U, 0.2 V), mapped to
  0..1. Correlation rather than absolute difference because the card in a
  deliverable has been through a transform the still has not (SDR still
  against a PQ file, legal/full, a display LUT): gain, offset and most of a
  tone curve leave the correlation high.
- A reference with almost no variance (a flat colour) cannot be correlated;
  for those, fall back to mean absolute difference over all three planes
  with a tight tolerance, and say so in the reference's tooltip.
- Events are runs of consecutive matching frames, `kind: "match"`,
  `subkind` = reference name, `score = similarity / min_similarity`,
  `label` = reference name. One event per run however long: an offline clip
  is one finding.
- Cost: 864 bytes × frames × references of byte arithmetic, tens of
  milliseconds for a feature; re-run on every slider move like the others.
- Row strip: reference thumbnail, frame before, first matching frame, last
  matching frame, frame after.

Tests: a reference matches itself at 1.0; survives a gain/offset change and
a 2.4 → PQ-like curve above `min_similarity`; an unrelated frame scores
under 0.5; flat-colour fallback; pillarboxed reference in a wider frame;
sidecar round trip; mismatched sidecar is rejected.

Gate: on a real export with a deliberately offline clip, both routes (a
screenshot of the card as PNG, and "use this frame") find the clip as one
event and nothing else at the default similarity. If the PNG route is
unreliable through an HDR output transform, that is acceptable so long as
the frame route works; note it here.

Open, to settle with numbers at this gate: whether 16×9 luma would do (a
quarter of the size), and the default `min_similarity`.

## 6. Order of work

1. §3 engine move and `DetectRequest` (no behaviour change). Gate §3.
2. §1 layout, `TabStrip`, split of `EventPanel`, timeline tabs. Gate §1.
3. §2 in-row thumbnails and loader. Gate §2.
4. §4 repeat detector and tab. Gate §4.
5. §5a signatures and sidecar, then §5b/§5c. Gate §5.
6. README: screenshots, "Modes" table, a "Detectors" section (what each
   finds, which modes, what needs a re-analysis).

Steps 1–3 are worth merging on their own; 4 and 5 are independent of each
other.

## 7. Not in this plan

- A decode-light "detectors only" pass: the stats are ~10 % of a levels
  run (`native-stats.md` §1), so a separate mode would save little. A
  `-lowres` J2K decode is the untested idea to try if IMF scans feel slow.
- Repeats or matches in diff and timeline modes.
- Detecting dropped frames (motion discontinuity without a repeat), black
  runs, freeze-at-end: natural next tabs, same scaffolding.
