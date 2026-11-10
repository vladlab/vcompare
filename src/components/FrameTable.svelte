<!--
  FrameTable — virtual-scrolled data table for frame metrics.

  Dynamic columns based on analysis mode:
    - diff mode: Frame, TC, Mean Diff, Peak Diff
    - vmaf mode: Frame, TC, VMAF, [PSNR]
    - timeline mode: Frame, TC, Clip, Pos, Status, Diff Y/U/V

  Supports sort, filter, virtual scroll, click-to-select, keyboard nav.
-->

<script lang="ts">
  import { onMount } from "svelte";
  import { getFrames, type FrameData, type GetFramesParams } from "../lib/rpc";

  // ── Props ──

  interface Props {
    cursorFrame: number;
    metrics: string[];   // e.g. ["diff_y", "diff_u", "diff_v"] or ["vmaf", "psnr"]
    mode: "diff" | "vmaf" | "levels" | "timeline";
    onFrameSelect: (frame: number) => void;
  }

  let { cursorFrame, metrics, mode, onFrameSelect }: Props = $props();

  // ── Column config ──

  const COLUMN_CONFIG: Record<string, { label: string; width: string; precision: number }> = {
    diff_y:    { label: "Diff Y",   width: "90px", precision: 3 },
    diff_u:    { label: "Diff U",   width: "90px", precision: 3 },
    diff_v:    { label: "Diff V",   width: "90px", precision: 3 },
    vmaf:      { label: "VMAF",     width: "90px", precision: 2 },
    psnr:      { label: "PSNR",     width: "90px", precision: 2 },
    ssim:      { label: "SSIM",     width: "90px", precision: 5 },
    ssim_y:    { label: "SSIM Y",   width: "90px", precision: 5 },
    ssim_u:    { label: "SSIM U",   width: "90px", precision: 5 },
    ssim_v:    { label: "SSIM V",   width: "90px", precision: 5 },
    y_min:     { label: "Y Min",    width: "70px", precision: 0 },
    y_max:     { label: "Y Max",    width: "70px", precision: 0 },
    u_min:     { label: "U Min",    width: "70px", precision: 0 },
    u_max:     { label: "U Max",    width: "70px", precision: 0 },
    v_min:     { label: "V Min",    width: "70px", precision: 0 },
    v_max:     { label: "V Max",    width: "70px", precision: 0 },
    y_avg:     { label: "Y Avg",    width: "70px", precision: 1 },
    u_avg:     { label: "U Avg",    width: "70px", precision: 1 },
    v_avg:     { label: "V Avg",    width: "70px", precision: 1 },
    y_low:     { label: "Y Low",    width: "70px", precision: 0 },
    y_high:    { label: "Y High",   width: "70px", precision: 0 },
    y_oor_lo:  { label: "Y < legal", width: "80px", precision: 0 },
    y_oor_hi:  { label: "Y > legal", width: "80px", precision: 0 },
    c_oor:     { label: "C illegal", width: "80px", precision: 0 },
    y_dif:     { label: "Y Dif",    width: "70px", precision: 2 },
    u_dif:     { label: "U Dif",    width: "70px", precision: 2 },
    v_dif:     { label: "V Dif",    width: "70px", precision: 2 },
    sat_avg:   { label: "Sat Avg",  width: "70px", precision: 1 },
  };

  // ── State ──

  const ROW_HEIGHT = 26;
  const BUFFER = 50;

  let frames: FrameData[] = $state([]);
  let totalCount: number = $state(0);
  let sortColumn: string = $state("frame");
  let sortAsc: boolean = $state(true);
  let filterMetric: string | null = $state(null);
  let filterBelow: number | null = $state(null);
  let filterAbove: number | null = $state(null);

  let scrollTop: number = $state(0);
  let viewportHeight: number = $state(600);
  let containerEl: HTMLDivElement;
  let loading: boolean = $state(false);

  // ── Fetch ──

  async function fetchData() {
    loading = true;
    try {
      const params: GetFramesParams = {
        sort: sortColumn,
        ascending: sortAsc,
        offset: 0,
        limit: 0,
      };
      if (filterMetric) {
        if (filterBelow != null) params.filter_below = filterBelow;
        if (filterAbove != null) params.filter_above = filterAbove;
        params.filter_metric = filterMetric;
      }
      const result = await getFrames(params);
      frames = result.frames;
      totalCount = result.total_matching;
    } catch (e) {
      console.error("Failed to fetch frames:", e);
    }
    loading = false;
  }

  onMount(fetchData);

  $effect(() => {
    void sortColumn;
    void sortAsc;
    void filterMetric;
    void filterBelow;
    void filterAbove;
    fetchData();
  });

  // ── Virtual scroll ──

  let visibleStart = $derived(
    Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - BUFFER)
  );
  let visibleEnd = $derived(
    Math.min(
      frames.length,
      Math.ceil((scrollTop + viewportHeight) / ROW_HEIGHT) + BUFFER
    )
  );
  let visibleFrames = $derived(frames.slice(visibleStart, visibleEnd));
  let totalHeight = $derived(frames.length * ROW_HEIGHT);
  let offsetY = $derived(visibleStart * ROW_HEIGHT);

  // ── Auto-scroll to selected frame ──

  let prevCursorFrame = -1;
  $effect(() => {
    if (cursorFrame !== prevCursorFrame && cursorFrame >= 0 && containerEl) {
      prevCursorFrame = cursorFrame;
      const idx = frames.findIndex((f) => f.frame === cursorFrame);
      if (idx >= 0) {
        const rowTop = idx * ROW_HEIGHT;
        const rowBottom = rowTop + ROW_HEIGHT;
        if (rowTop < containerEl.scrollTop || rowBottom > containerEl.scrollTop + viewportHeight) {
          containerEl.scrollTop = Math.max(0, rowTop - viewportHeight / 2 + ROW_HEIGHT / 2);
        }
      }
    }
  });

  // ── Interaction ──

  function handleSort(col: string) {
    if (sortColumn === col) {
      sortAsc = !sortAsc;
    } else {
      sortColumn = col;
      sortAsc = col === "frame";
    }
  }

  function handleScroll(e: Event) {
    scrollTop = (e.target as HTMLDivElement).scrollTop;
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      // With the table focused, arrows step frames here; elsewhere the app
      // uses them to step flash events.
      e.stopPropagation();
      const currentIdx = frames.findIndex((f) => f.frame === cursorFrame);
      let nextIdx =
        e.key === "ArrowDown"
          ? Math.min(currentIdx + 1, frames.length - 1)
          : Math.max(currentIdx - 1, 0);
      if (nextIdx >= 0 && nextIdx < frames.length) {
        onFrameSelect(frames[nextIdx].frame);
        const rowTop = nextIdx * ROW_HEIGHT;
        if (containerEl) {
          if (rowTop < containerEl.scrollTop) {
            containerEl.scrollTop = rowTop;
          } else if (rowTop + ROW_HEIGHT > containerEl.scrollTop + viewportHeight) {
            containerEl.scrollTop = rowTop + ROW_HEIGHT - viewportHeight;
          }
        }
      }
    }
  }

  function sortIcon(col: string): string {
    if (sortColumn !== col) return "";
    return sortAsc ? " ▲" : " ▼";
  }

  function getVal(frame: FrameData, metric: string): number | null | undefined {
    return (frame as any)[metric];
  }

  function fmtVal(val: number | null | undefined, metric: string): string {
    if (val == null) return "—";
    const prec = COLUMN_CONFIG[metric]?.precision ?? 2;
    return val.toFixed(prec);
  }

  /** Is this value "flagged" — above threshold for diff, below for vmaf */
  function isFlagged(val: number | null | undefined, metric: string): boolean {
    if (val == null) return false;
    if (mode === "diff" && filterAbove != null) return val > filterAbove;
    if (mode === "vmaf" && filterBelow != null) return val < filterBelow;
    return false;
  }

  // ── Public API ──

  export function setFilter(m: string | null, below: number | null, above: number | null = null) {
    filterMetric = m;
    filterBelow = below;
    filterAbove = above;
  }
</script>

<div class="table-container" role="grid" tabindex="0" onkeydown={handleKeydown}>
  <!-- Header -->
  <div class="table-header">
    <div class="th col-frame" onclick={() => handleSort("frame")}>
      Frame{sortIcon("frame")}
    </div>
    <div class="th col-tc">Timecode</div>
    {#if mode === "timeline"}
      <div class="th col-clip">Clip</div>
      <div class="th col-pos">Pos</div>
      <div class="th col-status">Status</div>
    {/if}
    {#each metrics as m}
      {@const cfg = COLUMN_CONFIG[m]}
      {#if cfg}
        <div
          class="th"
          style="width: {cfg.width}; flex-shrink: 0;"
          onclick={() => handleSort(m)}
        >
          {cfg.label}{sortIcon(m)}
        </div>
      {/if}
    {/each}
  </div>

  <!-- Scrollable body -->
  <div
    class="table-body"
    bind:this={containerEl}
    bind:clientHeight={viewportHeight}
    onscroll={handleScroll}
  >
    <div class="scroll-phantom" style="height: {totalHeight}px;">
      <div class="visible-rows" style="transform: translateY({offsetY}px);">
        {#each visibleFrames as frame (frame.frame)}
          {@const isSelected = frame.frame === cursorFrame}
          <div
            class="tr"
            class:selected={isSelected}
            onclick={() => onFrameSelect(frame.frame)}
            style="height: {ROW_HEIGHT}px;"
          >
            <div class="td col-frame mono">{frame.frame}</div>
            <div class="td col-tc mono">{frame.tc}</div>
            {#if mode === "timeline"}
              <div class="td col-clip" title={frame.clip ?? ""}>
                <span class="mono trk">V{frame.track}</span> {frame.clip ?? ""}
              </div>
              <div class="td col-pos mono">{frame.pos ?? ""}</div>
              <div class="td col-status mono" class:bad={frame.status !== "ok" && frame.status !== "not_in_file"} title={frame.error ?? ""}>
                {(frame.status ?? "").replace(/_/g, " ")}
              </div>
            {/if}
            {#each metrics as m}
              {@const val = getVal(frame, m)}
              {@const cfg = COLUMN_CONFIG[m]}
              {#if cfg}
                <div class="td mono" style="width: {cfg.width}; flex-shrink: 0;">
                  <span
                    class="metric-val"
                    class:flagged={isFlagged(val, m)}
                    class:zero={val === 0}
                  >
                    {fmtVal(val, m)}
                  </span>
                </div>
              {/if}
            {/each}
          </div>
        {/each}
      </div>
    </div>
  </div>

  <!-- Footer -->
  <div class="table-footer mono">
    {totalCount.toLocaleString()} {mode === "timeline" ? "samples" : "frames"}
    {#if filterMetric && (filterBelow != null || filterAbove != null)}
      · filtered
      {#if filterAbove != null}
        above {filterAbove.toFixed(1)}
      {:else if filterBelow != null}
        below {filterBelow.toFixed(1)}
      {/if}
    {/if}
    {#if loading}
      · loading…
    {/if}
  </div>
</div>

<style>
  .table-container {
    display: flex;
    flex-direction: column;
    height: 100%;
    background: var(--bg-base);
    outline: none;
  }

  .table-header {
    display: flex;
    background: var(--bg-surface);
    border-bottom: 1px solid var(--border-subtle);
    flex-shrink: 0;
  }
  .th {
    padding: 6px 8px;
    font-size: 11px;
    font-weight: 600;
    color: var(--text-secondary);
    cursor: pointer;
    user-select: none;
    white-space: nowrap;
  }
  .th:hover {
    color: var(--text-primary);
    background: var(--bg-hover);
  }

  .col-frame { width: 90px; flex-shrink: 0; }
  .col-tc    { width: 130px; flex-shrink: 0; }

  .table-body {
    flex: 1;
    overflow-y: auto;
    overflow-x: hidden;
  }
  .scroll-phantom { position: relative; }
  .visible-rows {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
  }

  .tr {
    display: flex;
    align-items: center;
    border-bottom: 1px solid var(--border-subtle);
    cursor: pointer;
    transition: background 0.05s;
  }
  .tr:hover { background: var(--bg-hover); }
  .tr.selected {
    background: var(--bg-active);
    border-left: 2px solid var(--accent);
  }

  .td {
    padding: 4px 8px;
    font-size: 12px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .metric-val {
    color: var(--vmaf-color);
  }
  .metric-val.flagged {
    color: var(--threshold-color);
    font-weight: 600;
  }
  .metric-val.zero {
    color: var(--text-muted);
  }

  .table-footer {
    padding: 4px 8px;
    font-size: 11px;
    color: var(--text-muted);
    background: var(--bg-surface);
    border-top: 1px solid var(--border-subtle);
    flex-shrink: 0;
  }
  .col-clip {
    width: 220px;
    flex-shrink: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .col-clip .trk {
    color: var(--text-muted);
  }
  .col-pos {
    width: 52px;
    flex-shrink: 0;
  }
  .col-status {
    width: 100px;
    flex-shrink: 0;
    color: var(--text-secondary);
  }
  .col-status.bad {
    color: var(--warning, #d29922);
  }
</style>
