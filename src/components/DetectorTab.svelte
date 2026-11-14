<!--
  DetectorTab — one detector's tab: a toolbar of its controls and the
  event list.

  Detection runs in the engine over data already in memory; App re-runs it
  when `params` change. `onAdjust` is called while a control is being
  moved (before the change is applied) so App can hold thumbnail loading
  back until things settle.

  Props:
    kind, result, error
    params        — bindable, this detector's parameters
    hideRejected  — bindable
    loader, files, cursorFrame, selectedFrame, seen
    onSelect, onReview, onExport, onExportEdl, onAdjust
    timeline      — flash tab in timeline mode (flagged samples)
    references    — match: the stored references
    onAddImage, onAddFrame, onRenameRef, onDeleteRef — match: manage them
-->
<script lang="ts">
  import type {
    DetectResult,
    DetectorKind,
    EventReview,
    FlashParams,
    ImageReference,
    MatchParams,
    RepeatParams,
  } from "../lib/rpc";
  import { maxTiles, DEFAULT_PARAMS } from "../lib/detectors";
  import type { ThumbLoader } from "../lib/thumbLoader.svelte";
  import EventList from "./EventList.svelte";

  interface Props {
    kind: DetectorKind;
    result: DetectResult | null;
    error: string | null;
    params: FlashParams | RepeatParams | MatchParams;
    hideRejected: boolean;
    loader: ThumbLoader;
    files: number;
    cursorFrame: number;
    selectedFrame: number | null;
    seen: Set<number>;
    onSelect: (frame: number) => void;
    onReview: (frame: number, review: EventReview | null) => void;
    onExport: () => void;
    onExportEdl: () => void;
    onAdjust: () => void;
    timeline?: boolean;
    references?: ImageReference[];
    onAddImage?: () => void;
    onAddFrame?: () => void;
    onRenameRef?: (id: string, name: string) => void;
    onDeleteRef?: (id: string) => void;
  }

  let {
    kind,
    result,
    error,
    params = $bindable(),
    hideRejected = $bindable(),
    loader,
    files,
    cursorFrame,
    selectedFrame,
    seen,
    onSelect,
    onReview,
    onExport,
    onExportEdl,
    onAdjust,
    timeline = false,
    references = [],
    onAddImage,
    onAddFrame,
    onRenameRef,
    onDeleteRef,
  }: Props = $props();

  // Typed views of the bindable params (one detector per tab instance)
  let fp = $derived(params as FlashParams);
  let rp = $derived(params as RepeatParams);
  let mp = $derived(params as MatchParams);

  function set(patch: Record<string, unknown>) {
    params = { ...params, ...patch };
  }

  // ── Sliders: while dragging, the slider owns its value and only the
  // readout follows; the change applies on release. Re-rendering the
  // slider from params on every move made the thumb jump. ──
  let dragPct: number | null = $state(null);

  let flashPct = $derived(
    Math.round((fp.threshold ?? result?.threshold ?? 0.04) * 1000) / 10
  );
  let repeatPct = $derived(Math.round((rp.tolerance ?? 0) * 10000) / 100);
  let matchPct = $derived(Math.round((mp.min_similarity ?? 0.9) * 100));

  function sliderInput(e: Event) {
    dragPct = parseFloat((e.target as HTMLInputElement).value);
    onAdjust();
  }
  function sliderDone(e: Event, apply: (v: number) => void) {
    const v = parseFloat((e.target as HTMLInputElement).value);
    dragPct = null;
    apply(v);
    (e.target as HTMLElement).blur();
  }

  // ── Derived ──
  let allEvents = $derived(result?.events ?? []);
  let events = $derived(hideRejected ? allEvents.filter((e) => e.review !== "reject") : allEvents);
  let reviewCounts = $derived.by(() => {
    let accept = 0, reject = 0;
    for (const e of allEvents) {
      if (e.review === "accept") accept++;
      else if (e.review === "reject") reject++;
    }
    return { accept, reject };
  });
  let breakdown = $derived.by(() => {
    const c: Record<string, number> = {};
    for (const e of events) {
      const k = kind === "match" ? (e.label ?? e.subkind) : timeline ? e.kind : e.subkind;
      c[k] = (c[k] ?? 0) + 1;
      if (kind === "flash" && !timeline && e.at_cut) c["at cut"] = (c["at cut"] ?? 0) + 1;
    }
    return Object.entries(c);
  });

  let tiles = $derived(
    maxTiles(kind, {
      ...DEFAULT_PARAMS,
      flash: kind === "flash" ? fp : DEFAULT_PARAMS.flash,
    })
  );
  let refThumbs = $derived(Object.fromEntries(references.map((r) => [r.id, r.thumb])));
  let selectedRefs = $derived(new Set(mp.references ?? []));

  function toggleRef(id: string, on: boolean) {
    const cur = new Set(mp.references ?? []);
    if (on) cur.add(id);
    else cur.delete(id);
    onAdjust();
    set({ references: [...cur] });
  }

  function emptyText(): string {
    if (kind === "match" && (mp.references ?? []).length === 0)
      return references.length === 0
        ? "No reference pictures yet. Add one from an image (a still of the offline card) or from the frame under the cursor."
        : "Tick a reference above to look for it.";
    if (!result) return "";
    if (timeline) return "No mismatches: every sample matches the SDI picture.";
    if (kind === "repeat") return "No repeated frames.";
    if (kind === "match") return "No frames match the ticked references.";
    return "No flash frames found.";
  }

  let editing: string | null = $state(null);
</script>

<div class="detector-tab">
  {#if error}
    <p class="tab-error">{error}</p>
  {:else}
    <div class="toolbar">
      {#if kind === "flash"}
        <label class="checkbox-label" title={timeline ? "Derive the threshold from the cleanest samples of this run (lower quartile + 6σ, kept between 0.4% and 3% of peak, so a fault on every sample is still flagged)" : "Derive the threshold from the footage's own frame-to-frame noise"}>
          <input
            type="checkbox"
            checked={fp.threshold == null}
            onchange={(e) => {
              onAdjust();
              set({ threshold: (e.target as HTMLInputElement).checked ? null : (result?.threshold ?? 0.04) });
            }}
          />
          auto
        </label>
        <label class="slider-label" title={timeline ? "RMS luma difference that flags a sample, as % of peak. Applies when you let go." : "Minimum frame-to-frame change, as % of signal peak. Applies when you let go."}>
          <span class="mono readout">{(dragPct ?? flashPct).toFixed(timeline ? 2 : 1)}%</span>
          <input
            type="range"
            min={timeline ? 0.1 : 0.5}
            max={timeline ? 10 : 40}
            step={timeline ? 0.05 : 0.5}
            value={dragPct ?? flashPct}
            disabled={fp.threshold == null}
            oninput={sliderInput}
            onchange={(e) => sliderDone(e, (v) => set({ threshold: Math.max(0.1, v) / 100 }))}
          />
        </label>
        {#if !timeline}
          <label title="Longest run of frames that still counts as a flash">
            ≤
            <select value={fp.max_run ?? 3} onchange={(e) => { onAdjust(); set({ max_run: parseInt((e.target as HTMLSelectElement).value) }); }}>
              {#each [1, 2, 3, 4, 5] as n}<option value={n}>{n}</option>{/each}
            </select>
            fr
          </label>
        {/if}
        <label class="checkbox-label" title={timeline ? "Hide samples on a clip's first or last frame (where an edit that moved by a frame shows)" : "Hide short inserts that sit on a cut (fast montage, etc.)"}>
          <input
            type="checkbox"
            checked={fp.exclude_at_cut ?? false}
            onchange={(e) => { onAdjust(); set({ exclude_at_cut: (e.target as HTMLInputElement).checked }); }}
          />
          {timeline ? "hide cut frames" : "hide at-cut"}
        </label>
      {:else if kind === "repeat"}
        <label class="checkbox-label" title="Only frames identical to the one before (right for intra codecs: ProRes, DNx, J2K)">
          <input
            type="checkbox"
            checked={(rp.tolerance ?? 0) === 0}
            onchange={(e) => { onAdjust(); set({ tolerance: (e.target as HTMLInputElement).checked ? 0 : 0.001 }); }}
          />
          exact
        </label>
        <label class="slider-label" title="Largest mean difference from the previous frame that still counts as a repeat, as % of peak (long-GOP encodes repeat to near zero, not zero). Applies when you let go.">
          <span class="mono readout">{(dragPct ?? repeatPct).toFixed(2)}%</span>
          <input
            type="range"
            min={0.01}
            max={0.5}
            step={0.01}
            value={dragPct ?? Math.max(repeatPct, 0.01)}
            disabled={(rp.tolerance ?? 0) === 0}
            oninput={sliderInput}
            onchange={(e) => sliderDone(e, (v) => set({ tolerance: v / 100 }))}
          />
        </label>
        <label title="Fewest repeated frames that make an event">
          ≥
          <select value={rp.min_run ?? 1} onchange={(e) => { onAdjust(); set({ min_run: parseInt((e.target as HTMLSelectElement).value) }); }}>
            {#each [1, 2, 3] as n}<option value={n}>{n}</option>{/each}
          </select>
        </label>
        <label title="Longest run listed as a repeat; longer runs are holds">
          ≤
          <select value={rp.max_run ?? 5} onchange={(e) => { onAdjust(); set({ max_run: parseInt((e.target as HTMLSelectElement).value) }); }}>
            {#each [1, 2, 3, 5, 8, 12] as n}<option value={n}>{n}</option>{/each}
          </select>
          fr
        </label>
        <label class="checkbox-label" title="Also list longer runs: freezes, holds, stills, titles">
          <input type="checkbox" checked={rp.include_long ?? false} onchange={(e) => { onAdjust(); set({ include_long: (e.target as HTMLInputElement).checked }); }} />
          holds
        </label>
        <label title="Repeats with only near-still frames between them are joined into one hold, so a card that shimmers, fades or barely animates is one event instead of a scatter of short repeats. This is how different those in-between frames may be (% of peak). Raise it if a static card is still listed in pieces; lower it (or turn it off) if separate faults in a very quiet shot get joined.">
          join if still ≤
          <select value={rp.still_tolerance ?? 0.001} onchange={(e) => { onAdjust(); set({ still_tolerance: parseFloat((e.target as HTMLSelectElement).value) }); }}>
            <option value={0}>off</option>
            {#each [0.0005, 0.001, 0.002, 0.005, 0.01] as v}<option value={v}>{(v * 100).toFixed(2).replace(/0$/, "")}%</option>{/each}
          </select>
        </label>
        <label class="checkbox-label" title="Leave out repeated black frames">
          <input type="checkbox" checked={rp.skip_black ?? true} onchange={(e) => { onAdjust(); set({ skip_black: (e.target as HTMLInputElement).checked }); }} />
          skip black
        </label>
      {:else if kind === "match"}
        <label class="slider-label" title="Minimum similarity to a reference (correlation of the picture's layout, so a card that went through a LUT or a range change still matches). Applies when you let go.">
          <span class="mono readout">{Math.round(dragPct ?? matchPct)}%</span>
          <input
            type="range"
            min={50}
            max={100}
            step={1}
            value={dragPct ?? matchPct}
            oninput={sliderInput}
            onchange={(e) => sliderDone(e, (v) => set({ min_similarity: v / 100 }))}
          />
        </label>
        <label title="Fewest consecutive matching frames">
          ≥
          <select value={mp.min_run ?? 1} onchange={(e) => { onAdjust(); set({ min_run: parseInt((e.target as HTMLSelectElement).value) }); }}>
            {#each [1, 2, 3, 6, 12] as n}<option value={n}>{n}</option>{/each}
          </select>
          fr
        </label>
      {/if}

      <label class="checkbox-label" title="Hide events you've rejected">
        <input type="checkbox" bind:checked={hideRejected} />
        hide rejected
      </label>

      <span class="spacer"></span>
      {#if result}
        <span class="summary">
          <span class="count mono">{events.length}</span>
          {#each breakdown as [k, n]}{" · "}{n} {k}{/each}
          {#if reviewCounts.accept + reviewCounts.reject > 0}
            {" · "}<span class="ok">✓{reviewCounts.accept}</span> <span class="no">✗{reviewCounts.reject}</span>
          {/if}
          {#if kind === "flash"}{" · "}thr {(result.threshold * 100).toFixed(timeline ? 2 : 1)}%{/if}
        </span>
      {/if}
      <button class="small" onclick={onExportEdl} disabled={events.length === 0} title="Export as a DaVinci Resolve marker EDL (Timeline › Import › Timeline Markers from EDL). Rejected events are left out.">EDL</button>
      <button class="small" onclick={onExport} disabled={events.length === 0} title="Export as CSV">CSV</button>
    </div>

    {#if kind === "match"}
      <div class="refs">
        {#each references as r (r.id)}
          <div class="ref-chip" class:on={selectedRefs.has(r.id)} title="{r.source}{r.flat ? ' — flat colour: matched by colour, not layout' : ''}">
            <input type="checkbox" checked={selectedRefs.has(r.id)} onchange={(e) => toggleRef(r.id, (e.target as HTMLInputElement).checked)} />
            {#if r.thumb}<img src={r.thumb} alt="" />{/if}
            {#if editing === r.id}
              <!-- svelte-ignore a11y_autofocus -->
              <input
                class="ref-name-edit"
                value={r.name}
                autofocus
                onkeydown={(e) => {
                  e.stopPropagation();
                  if (e.key === "Enter") { onRenameRef?.(r.id, (e.target as HTMLInputElement).value); editing = null; }
                  if (e.key === "Escape") editing = null;
                }}
                onblur={() => (editing = null)}
              />
            {:else}
              <button class="ref-name" ondblclick={() => (editing = r.id)} title="Double-click to rename">{r.name}</button>
            {/if}
            {#if r.flat}<span class="flat" title="Flat colour: matched by colour">■</span>{/if}
            <button class="ref-del" title="Delete this reference" onclick={() => onDeleteRef?.(r.id)}>×</button>
          </div>
        {/each}
        <button class="small" onclick={onAddImage} title="Add a reference from an image file (PNG, JPEG, TIFF, DPX, EXR…). It is fitted into this file's frame the way it would be placed on the timeline.">+ image…</button>
        <button class="small" onclick={onAddFrame} title="Use the frame under the cursor as a reference — exact, and the reliable route for a card that went through an output transform">+ this frame</button>
      </div>
    {/if}

    {#if result?.note}
      <p class="note">{result.note}</p>
    {/if}

    <EventList
      {kind}
      {events}
      {loader}
      {tiles}
      {files}
      {cursorFrame}
      {selectedFrame}
      {seen}
      {onSelect}
      {onReview}
      {timeline}
      threshold={result?.threshold ?? null}
      {refThumbs}
      empty={emptyText()}
    />
  {/if}
</div>

<style>
  .detector-tab {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
  }
  .tab-error {
    padding: var(--gap-md);
    color: var(--text-secondary);
    font-size: 12px;
    line-height: 1.5;
  }
  .toolbar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--gap-sm) var(--gap-lg, 16px);
    padding: var(--gap-sm) var(--gap-md);
    border-bottom: 1px solid var(--border-subtle);
    font-size: 12px;
    color: var(--text-secondary);
    flex-shrink: 0;
  }
  .toolbar label { display: flex; align-items: center; gap: var(--gap-xs); }
  .slider-label { width: 260px; }
  .slider-label input[type="range"] {
    flex: 1;
    padding: 0;
    border: none;
    background: transparent;
    accent-color: var(--warning);
  }
  .slider-label input[type="range"]:disabled { opacity: 0.4; }
  .readout { width: 50px; text-align: right; color: var(--text-primary); }
  .checkbox-label input { margin: 0; }
  .spacer { flex: 1; }
  .summary { color: var(--text-muted); font-size: 11px; }
  .summary .ok { color: var(--success); }
  .summary .no { color: var(--threshold-color); }
  .count { color: var(--warning); font-weight: 600; }
  button.small { padding: 2px 8px; font-size: 11px; }

  .refs {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--gap-sm);
    padding: var(--gap-sm) var(--gap-md);
    border-bottom: 1px solid var(--border-subtle);
    flex-shrink: 0;
  }
  .ref-chip {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 2px 4px;
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-sm);
    font-size: 12px;
    color: var(--text-secondary);
  }
  .ref-chip.on { border-color: #f070c0; color: var(--text-primary); }
  .ref-chip img { height: 24px; border-radius: 2px; }
  .ref-chip input[type="checkbox"] { margin: 0; }
  .ref-name { background: none; border: none; padding: 0 2px; color: inherit; font-size: 12px; cursor: text; }
  .ref-name-edit { font-size: 12px; padding: 1px 4px; width: 160px; }
  .ref-del { background: none; border: none; padding: 0 4px; color: var(--text-muted); font-size: 14px; line-height: 1; }
  .ref-del:hover { color: var(--threshold-color); }
  .flat { color: var(--text-muted); font-size: 10px; }

  .note {
    margin: 0;
    padding: var(--gap-xs) var(--gap-md);
    font-size: 11px;
    color: var(--warning);
    border-bottom: 1px solid var(--border-subtle);
    flex-shrink: 0;
  }
</style>
