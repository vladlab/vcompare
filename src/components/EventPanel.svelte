<!--
  EventPanel — flash-frame detector controls and event list.

  Shown alongside the results in `levels` and `diff` modes. Detection runs
  in the engine over data already in memory, so every control change
  re-runs it immediately (debounced in App.svelte).

  Props:
    result: DetectResult | null   — events + threshold actually used
    error:  string | null         — why detection isn't available
    params: DetectParams          — bindable; App re-runs detection on change
    cursorFrame: number
    frameRange: [start, end]      — for the overview strip
    selectedFrame                 — the event picked last (stays highlighted
                                    while the cursor shuttles around it)
    seen                          — first frames of events already visited
    onSelect(frame)               — jump the cursor (and mpv) to a frame
    onExport()                    — export events CSV
    onExportEdl()                 — export events as a Resolve marker EDL
-->
<script lang="ts">
  import type { DetectParams, DetectResult, FlashEvent } from "../lib/rpc";

  interface Props {
    result: DetectResult | null;
    error: string | null;
    params: DetectParams;
    cursorFrame: number;
    selectedFrame: number | null;
    seen: Set<number>;
    frameRange: [number, number] | null;
    onSelect: (frame: number) => void;
    onExport: () => void;
    onExportEdl: () => void;
  }

  let {
    result,
    error,
    params = $bindable(),
    cursorFrame,
    selectedFrame,
    seen,
    frameRange,
    onSelect,
    onExport,
    onExportEdl,
  }: Props = $props();

  // ── Threshold control ──
  //
  // The engine threshold is normalized 0..1 of signal peak. The slider works
  // in percent of peak, which reads naturally ("a 4% luma jump").

  let auto = $derived(params.threshold == null);

  function thresholdPct(): number {
    const t = params.threshold ?? result?.threshold ?? 0.04;
    return Math.round(t * 1000) / 10;
  }

  function setAuto(on: boolean) {
    if (on) {
      params = { ...params, threshold: null };
    } else {
      // Start manual editing from whatever auto picked
      params = { ...params, threshold: result?.threshold ?? 0.04 };
    }
  }

  function setThresholdPct(pct: number) {
    params = { ...params, threshold: Math.max(0.1, pct) / 100 };
  }

  function setMaxRun(n: number) {
    params = { ...params, max_run: n };
  }

  function setExcludeAtCut(on: boolean) {
    params = { ...params, exclude_at_cut: on };
  }

  // ── Derived ──

  let events = $derived(result?.events ?? []);

  let counts = $derived.by(() => {
    const c: Record<"black" | "white" | "content" | "at_cut", number> = { black: 0, white: 0, content: 0, at_cut: 0 };
    for (const e of events) {
      c[e.subkind] = (c[e.subkind] ?? 0) + 1;
      if (e.at_cut) c.at_cut++;
    }
    return c;
  });

  /** Index of the event whose run contains the cursor, or -1. */
  let activeIndex = $derived(
    events.findIndex((e) => cursorFrame >= e.frame && cursorFrame < e.frame + e.length)
  );
  /** Index of the last-picked event (kept while shuttling around it). */
  let selectedIndex = $derived(
    selectedFrame == null ? -1 : events.findIndex((e) => e.frame === selectedFrame)
  );

  function stripPct(frame: number): number {
    if (!frameRange) return 0;
    const [s, e] = frameRange;
    if (e <= s) return 0;
    return ((frame - s) / (e - s)) * 100;
  }

  function label(e: FlashEvent): string {
    const len = e.length === 1 ? "1 fr" : `${e.length} fr`;
    return `${e.subkind} · ${len}${e.at_cut ? " · at cut" : ""}`;
  }

  // Keep the picked row in view when navigating with the keyboard
  let listEl: HTMLDivElement | undefined = $state();
  $effect(() => {
    if (selectedIndex < 0 || !listEl) return;
    const row = listEl.children[selectedIndex] as HTMLElement | undefined;
    row?.scrollIntoView({ block: "nearest" });
  });
</script>

<div class="event-panel">
  <div class="panel-header">
    <span class="panel-title">Flash frames</span>
    {#if result}
      <span class="count mono">{events.length}</span>
    {/if}
    <span class="spacer"></span>
    <button class="small" onclick={onExportEdl} disabled={events.length === 0} title="Export events as a DaVinci Resolve marker EDL (Timeline › Import › Timeline Markers from EDL)">
      EDL
    </button>
    <button class="small" onclick={onExport} disabled={events.length === 0} title="Export events as CSV">
      CSV
    </button>
  </div>

  {#if error}
    <p class="panel-error">{error}</p>
  {:else}
    <!-- Controls -->
    <div class="controls">
      <label class="checkbox-label" title="Derive the threshold from the footage's own frame-to-frame noise">
        <input type="checkbox" checked={auto} onchange={(e) => setAuto((e.target as HTMLInputElement).checked)} />
        auto
      </label>
      <label class="slider-label" title="Minimum frame-to-frame change, as % of signal peak">
        <span class="mono">{thresholdPct().toFixed(1)}%</span>
        <input
          type="range"
          min="0.5"
          max="40"
          step="0.5"
          value={thresholdPct()}
          disabled={auto}
          oninput={(e) => setThresholdPct(parseFloat((e.target as HTMLInputElement).value))}
        />
      </label>
      <label title="Longest run of frames that still counts as a flash">
        ≤
        <select value={params.max_run ?? 3} onchange={(e) => setMaxRun(parseInt((e.target as HTMLSelectElement).value))}>
          <option value={1}>1</option>
          <option value={2}>2</option>
          <option value={3}>3</option>
          <option value={4}>4</option>
          <option value={5}>5</option>
        </select>
        fr
      </label>
      <label class="checkbox-label" title="Hide short inserts that sit on a cut (fast montage, etc.)">
        <input
          type="checkbox"
          checked={params.exclude_at_cut ?? false}
          onchange={(e) => setExcludeAtCut((e.target as HTMLInputElement).checked)}
        />
        hide at-cut
      </label>
    </div>

    <!-- Overview strip -->
    {#if frameRange}
      <div class="strip" title="Event positions across the analysed range">
        {#each events as e, i}
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <span
            class="strip-tick {e.subkind}"
            class:at-cut={e.at_cut}
            class:active={i === activeIndex}
            class:selected={i === selectedIndex}
            style="left: {stripPct(e.frame)}%"
            onclick={() => onSelect(e.frame)}
          ></span>
        {/each}
      </div>
    {/if}

    <!-- Summary line -->
    <div class="summary mono">
      {#if result}
        {counts.content} content · {counts.white} white · {counts.black} black
        {#if counts.at_cut > 0}· {counts.at_cut} at cut{/if}
        · thr {(result.threshold * 100).toFixed(1)}%
      {:else}
        detecting…
      {/if}
    </div>

    <!-- Event list -->
    <div class="list" bind:this={listEl}>
      {#each events as e, i (e.frame)}
        <button
          class="row"
          class:active={i === activeIndex}
          class:selected={i === selectedIndex}
          class:seen={seen.has(e.frame)}
          onclick={(ev) => { onSelect(e.frame); (ev.currentTarget as HTMLElement).blur(); }}
        >
          <span class="dot {e.subkind}" class:at-cut={e.at_cut}></span>
          <span class="tc mono">{e.tc || e.frame}</span>
          {#if seen.has(e.frame)}<span class="seen-mark" title="visited">✓</span>{/if}
          <span class="desc">{label(e)}</span>
          <span class="score mono" title="strength relative to threshold">×{e.score.toFixed(1)}</span>
        </button>
      {:else}
        {#if result}
          <p class="empty">No flash frames found.</p>
        {/if}
      {/each}
    </div>
  {/if}
</div>

<style>
  .event-panel {
    display: flex;
    flex-direction: column;
    height: 100%;
    background: var(--bg-surface);
    border-left: 1px solid var(--border-subtle);
    min-width: 0;
  }

  .panel-header {
    display: flex;
    align-items: center;
    gap: var(--gap-sm);
    padding: 6px var(--gap-md);
    border-bottom: 1px solid var(--border-subtle);
    flex-shrink: 0;
  }
  .panel-title {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--text-secondary);
  }
  .count {
    color: var(--warning);
    font-weight: 600;
  }
  .spacer { flex: 1; }

  button.small {
    padding: 2px 8px;
    font-size: 11px;
  }

  .panel-error {
    padding: var(--gap-md);
    color: var(--text-secondary);
    font-size: 12px;
    line-height: 1.5;
  }

  .controls {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--gap-sm) var(--gap-md);
    padding: var(--gap-sm) var(--gap-md);
    border-bottom: 1px solid var(--border-subtle);
    font-size: 12px;
    color: var(--text-secondary);
    flex-shrink: 0;
  }
  .controls label {
    display: flex;
    align-items: center;
    gap: var(--gap-xs);
  }
  .slider-label {
    flex: 1;
    min-width: 120px;
  }
  .slider-label input[type="range"] {
    flex: 1;
    padding: 0;
    border: none;
    background: transparent;
    accent-color: var(--warning);
  }
  .slider-label input[type="range"]:disabled {
    opacity: 0.4;
  }
  .slider-label .mono {
    width: 46px;
    text-align: right;
    color: var(--text-primary);
  }
  .checkbox-label input {
    margin: 0;
  }

  /* Overview strip */
  .strip {
    position: relative;
    height: 10px;
    margin: var(--gap-sm) var(--gap-md) 0;
    background: var(--bg-base);
    border: 1px solid var(--border-subtle);
    border-radius: 2px;
    flex-shrink: 0;
  }
  .strip-tick {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 2px;
    margin-left: -1px;
    cursor: pointer;
  }
  .strip-tick.selected {
    width: 4px;
    margin-left: -2px;
    outline: 1px solid var(--text-secondary);
  }
  .strip-tick.active {
    width: 4px;
    margin-left: -2px;
    outline: 1px solid var(--text-primary);
  }

  .summary {
    padding: var(--gap-xs) var(--gap-md);
    color: var(--text-muted);
    font-size: 11px;
    flex-shrink: 0;
  }

  /* List */
  .list {
    flex: 1;
    overflow-y: auto;
    padding: 0 var(--gap-xs) var(--gap-xs);
  }
  .row {
    display: flex;
    align-items: center;
    gap: var(--gap-sm);
    width: 100%;
    padding: 4px var(--gap-sm);
    background: transparent;
    border: none;
    border-radius: var(--radius-sm);
    color: var(--text-primary);
    text-align: left;
    cursor: pointer;
  }
  .row:hover { background: var(--bg-hover); }
  /* selected = the event you picked; it stays grey while you shuttle
     around it, and turns accent again whenever the cursor is on it */
  .row.selected { background: var(--bg-active); }
  .row.active { background: var(--accent-dim); }
  .row.seen .tc { color: var(--text-secondary); }
  .seen-mark {
    color: var(--text-muted);
    font-size: 11px;
    flex-shrink: 0;
  }
  .tc { flex-shrink: 0; }
  .desc {
    flex: 1;
    color: var(--text-secondary);
    font-size: 11px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .score {
    color: var(--text-muted);
    font-size: 11px;
    flex-shrink: 0;
  }
  .empty {
    padding: var(--gap-md);
    color: var(--text-muted);
    font-size: 12px;
  }

  /* Subkind colours — kept in sync with MetricChart EVENT_COLORS */
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .content, .dot.content { background: #e8b830; }
  .white,   .dot.white   { background: #ffffff; }
  .black,   .dot.black   { background: #9a9a9a; }
  .at-cut { opacity: 0.45; }
</style>
