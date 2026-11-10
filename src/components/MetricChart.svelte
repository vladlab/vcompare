<!--
  MetricChart — uPlot-based interactive metric chart.

  Features:
    - Keyboard/wheel zoom centers on playhead (keys) or mouse (wheel)
    - Playhead auto-follow: view shifts to keep playhead visible when zoomed
    - Overview minimap with playhead indicator
    - Horizontal scrollbar on detail chart for panning
    - Scrub by click+drag on detail chart
-->

<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import uPlot from "uplot";
  import "uplot/dist/uPlot.min.css";
  import type { MetricSeries, TimelineInfo, FlashEvent } from "../lib/rpc";

  // ── Props ──

  interface Props {
    series: MetricSeries;
    metric: string;
    threshold: number | null;
    cursorFrame: number;
    onCursorChange: (frame: number) => void;
    timeline?: TimelineInfo | null;
    /** Detected events to mark on the chart (flash frames). */
    events?: FlashEvent[];
    /** Data is a set of samples (timeline mode): draw the points. */
    sparse?: boolean;
  }

  let {
    series,
    metric,
    threshold,
    cursorFrame,
    onCursorChange,
    timeline = null,
    events = [],
    sparse = false,
  }: Props = $props();

  // ── Refs ──

  let detailEl: HTMLDivElement;
  let overviewEl: HTMLDivElement;
  let scrollbarEl: HTMLInputElement;
  let detailPlot: uPlot | null = null;
  let overviewPlot: uPlot | null = null;

  // ── Colors and config per metric ──

  const METRIC_CONFIG: Record<string, { color: string; label: string }> = {
    vmaf:   { color: "#e8b830", label: "VMAF" },
    psnr:   { color: "#4ec9b0", label: "PSNR (dB)" },
    diff_y: { color: "#e8b830", label: "RMS Diff (Y)" },
    diff_u: { color: "#6bc4e8", label: "RMS Diff (U)" },
    diff_v: { color: "#e86b9e", label: "RMS Diff (V)" },
    ssim:   { color: "#4ec9b0", label: "SSIM (All)" },
    ssim_y: { color: "#7be870", label: "SSIM (Y)" },
    ssim_u: { color: "#70b8e8", label: "SSIM (U)" },
    ssim_v: { color: "#d870e8", label: "SSIM (V)" },
    y_min:  { color: "#b89830", label: "Y Min" },
    y_max:  { color: "#f0d060", label: "Y Max" },
    u_min:  { color: "#4090c0", label: "U Min (Cb)" },
    u_max:  { color: "#70c0f0", label: "U Max (Cb)" },
    v_min:  { color: "#c04070", label: "V Min (Cr)" },
    v_max:  { color: "#f070a0", label: "V Max (Cr)" },
    y_avg:  { color: "#d8b848", label: "Y Avg" },
    u_avg:  { color: "#58a8d8", label: "U Avg (Cb)" },
    v_avg:  { color: "#d85888", label: "V Avg (Cr)" },
    y_low:  { color: "#a08830", label: "Y Low (10th pct)" },
    y_high: { color: "#f8e080", label: "Y High (90th pct)" },
    y_oor_lo: { color: "#6b8ce8", label: "Luma samples below legal" },
    y_oor_hi: { color: "#e54545", label: "Luma samples above legal" },
    c_oor:  { color: "#d870e8", label: "Chroma samples outside legal" },
    y_dif:  { color: "#e8e8e8", label: "Y Dif (vs prev)" },
    u_dif:  { color: "#70c0f0", label: "U Dif (vs prev)" },
    v_dif:  { color: "#f070a0", label: "V Dif (vs prev)" },
    sat_avg:{ color: "#c070e8", label: "Sat Avg" },
  };

  // Event marker colours by subkind
  const EVENT_COLORS: Record<string, string> = {
    black: "#9a9a9a",
    white: "#ffffff",
    content: "#e8b830",
    // timeline mode (by kind)
    mismatch: "#e5534b",
    error: "#d29922",
  };
  const EVENT_AT_CUT_ALPHA = 0.45;

  function getColor(): string {
    return METRIC_CONFIG[metric]?.color ?? "#e8b830";
  }
  function getLabel(): string {
    return METRIC_CONFIG[metric]?.label ?? metric;
  }

  const THRESHOLD_COLOR = "#e54545";
  const GRID_COLOR = "rgba(255,255,255,0.06)";
  const AXIS_COLOR = "#666";
  const PLAYHEAD_COLOR = "rgba(255,255,255,0.8)";
  const PLAYHEAD_DOT_COLOR = "#ffffff";
  const OVERVIEW_PLAYHEAD_COLOR = "rgba(255,255,255,0.5)";

  // ── Chart data ──

  function buildData(s: MetricSeries): [number[], (number | null)[]] {
    if (!timeline) {
      return [s.frames, s.values];
    }

    // Filter to only include frames within the overlap region,
    // then pad with boundary points at span edges so uPlot extends the axis.
    const frames: number[] = [];
    const values: (number | null)[] = [];

    // Leading boundary (before data)
    if (timeline.span_start < timeline.overlap_start) {
      frames.push(timeline.span_start);
      values.push(null);
    }

    // Only include frames within the overlap region
    for (let i = 0; i < s.frames.length; i++) {
      if (s.frames[i] >= timeline.overlap_start && s.frames[i] <= timeline.overlap_end) {
        frames.push(s.frames[i]);
        values.push(s.values[i]);
      }
    }

    // Trailing boundary (after data)
    if (timeline.span_end > timeline.overlap_end) {
      frames.push(timeline.span_end);
      values.push(null);
    }

    return [frames, values];
  }

  // ── No-data zone plugin ──
  // Draws gray hatched regions where only one clip has media.

  const NO_DATA_COLOR = "rgba(255,255,255,0.06)";
  const NO_DATA_HATCH_COLOR = "rgba(255,255,255,0.15)";
  const NO_DATA_BORDER_COLOR = "rgba(255,255,255,0.25)";
  const HATCH_SPACING = 10;

  function noDataZonePlugin(): uPlot.Plugin {
    return {
      hooks: {
        draw: [
          (u: uPlot) => {
            if (!timeline) return;
            const ctx = u.ctx;
            const { left, top, width, height } = u.bbox;
            const dpr = devicePixelRatio;

            // Helper: draw hatched rect in canvas coords
            function drawHatchedRect(x1: number, x2: number) {
              if (x2 <= x1) return;
              const rx1 = Math.max(x1, left);
              const rx2 = Math.min(x2, left + width);
              if (rx2 <= rx1) return;

              ctx.save();

              // Clip to this rect
              ctx.beginPath();
              ctx.rect(rx1, top, rx2 - rx1, height);
              ctx.clip();

              // Solid background
              ctx.fillStyle = NO_DATA_COLOR;
              ctx.fillRect(rx1, top, rx2 - rx1, height);

              // Diagonal hatch lines
              ctx.strokeStyle = NO_DATA_HATCH_COLOR;
              ctx.lineWidth = 1 * dpr;
              ctx.beginPath();
              const spacing = HATCH_SPACING * dpr;
              const h = height;
              const w = rx2 - rx1;
              for (let offset = -h; offset < w + h; offset += spacing) {
                ctx.moveTo(rx1 + offset, top + h);
                ctx.lineTo(rx1 + offset + h, top);
              }
              ctx.stroke();

              // "no data" label centered in zone
              const zoneW = (rx2 - rx1) / dpr;
              if (zoneW > 60) {
                ctx.fillStyle = "rgba(255,255,255,0.3)";
                ctx.font = `${11 * dpr}px "JetBrains Mono", monospace`;
                ctx.textAlign = "center";
                ctx.textBaseline = "middle";
                ctx.fillText("no data", (rx1 + rx2) / 2, top + height / 2);
              }

              ctx.restore();

              // Border line at the data boundary edge
              ctx.save();
              ctx.strokeStyle = NO_DATA_BORDER_COLOR;
              ctx.lineWidth = 1.5 * dpr;
              ctx.setLineDash([4 * dpr, 4 * dpr]);

              // Draw border at the edge closest to data
              const borderX = (rx1 > left) ? rx1 : rx2;
              ctx.beginPath();
              ctx.moveTo(borderX, top);
              ctx.lineTo(borderX, top + height);
              ctx.stroke();
              ctx.restore();
            }

            // Left no-data zone: span_start to overlap_start
            if (timeline.overlap_start > timeline.span_start) {
              const x1 = u.valToPos(timeline.span_start, "x", true);
              const x2 = u.valToPos(timeline.overlap_start, "x", true);
              drawHatchedRect(x1, x2);
            }

            // Right no-data zone: overlap_end to span_end
            if (timeline.overlap_end < timeline.span_end) {
              const x1 = u.valToPos(timeline.overlap_end, "x", true);
              const x2 = u.valToPos(timeline.span_end, "x", true);
              drawHatchedRect(x1, x2);
            }
          },
        ],
      },
    };
  }

  // ── Y-axis range logic ──

  function yRange(): [number, number] {
    if (metric === "vmaf") return [0, 100];
    if (series && series.values.length > 0) {
      const min = Math.min(...series.values);
      const max = Math.max(...series.values);
      if (metric.startsWith("diff")) {
        const pad = max * 0.05 || 0.5;
        return [0, max + pad];
      }
      if (metric.startsWith("ssim")) {
        const pad = (max - min) * 0.1 || 0.001;
        return [Math.max(0, min - pad), Math.min(1, max + pad)];
      }
      if (metric.startsWith("y_") || metric.startsWith("u_") || metric.startsWith("v_")) {
        // Levels: signal values, auto-detect range from data
        // Use full 0–peak range, where peak is inferred (255 or 1023)
        const peak = max > 255 ? 1023 : 255;
        return [0, peak];
      }
      const pad = (max - min) * 0.05 || 1;
      return [min - pad, max + pad];
    }
    return [0, 100];
  }

  // ── Data extents ──
  // When timeline is present, the chart spans the full timeline (both clips).
  // Otherwise it just spans the data points.

  function dataMin(): number {
    if (timeline) return timeline.span_start;
    return series?.frames[0] ?? 0;
  }
  function dataMax(): number {
    if (timeline) return timeline.span_end;
    return series?.frames[series.frames.length - 1] ?? 1;
  }
  function dataRange(): number { return dataMax() - dataMin(); }

  // ── Current visible X range ──

  let currentXMin: number | null = null;
  let currentXMax: number | null = null;

  function visibleMin(): number { return currentXMin ?? dataMin(); }
  function visibleMax(): number { return currentXMax ?? dataMax(); }
  function visibleRange(): number { return visibleMax() - visibleMin(); }
  function isZoomed(): boolean { return visibleRange() < dataRange() * 0.99; }

  // ── Zoom helpers ──

  function setXRange(min: number, max: number) {
    const dMin = dataMin();
    const dMax = dataMax();
    // Clamp
    if (min < dMin) { max += dMin - min; min = dMin; }
    if (max > dMax) { min -= max - dMax; max = dMax; }
    min = Math.max(dMin, min);
    max = Math.min(dMax, max);
    if (max - min < 10) return; // don't zoom in too far
    detailPlot?.setScale("x", { min, max });
  }

  function zoomAroundPoint(anchor: number, factor: number) {
    const min = visibleMin();
    const max = visibleMax();
    const range = max - min;
    const newRange = range * factor;
    const ratio = (anchor - min) / range;
    setXRange(anchor - newRange * ratio, anchor + newRange * (1 - ratio));
  }

  // ── Public zoom methods ──

  export function zoomIn() {
    if (!detailPlot || !series) return;
    // Center on playhead if visible, otherwise center of view
    const anchor = (cursorFrame >= visibleMin() && cursorFrame <= visibleMax())
      ? cursorFrame
      : (visibleMin() + visibleMax()) / 2;
    zoomAroundPoint(anchor, 0.5);
  }

  export function zoomOut() {
    if (!detailPlot || !series) return;
    const anchor = (cursorFrame >= visibleMin() && cursorFrame <= visibleMax())
      ? cursorFrame
      : (visibleMin() + visibleMax()) / 2;
    zoomAroundPoint(anchor, 2);
  }

  export function zoomReset() {
    if (!detailPlot || !series) return;
    currentXMin = null;
    currentXMax = null;
    detailPlot.setScale("x", { min: dataMin(), max: dataMax() });
  }

  // ── Playhead auto-follow ──

  function ensurePlayheadVisible() {
    if (!isZoomed() || cursorFrame < 0 || !detailPlot) return;

    const min = visibleMin();
    const max = visibleMax();
    const range = visibleRange();

    if (cursorFrame < min || cursorFrame > max) {
      // Shift view to center playhead, maintaining zoom level
      const newMin = cursorFrame - range / 2;
      const newMax = cursorFrame + range / 2;
      setXRange(newMin, newMax);
    }
  }

  // ── Scrub handling ──

  function nearestFrameAtX(u: uPlot, clientX: number): number | null {
    if (!series || series.frames.length === 0) return null;
    const rect = u.over.getBoundingClientRect();
    const cx = clientX - rect.left;
    const xVal = u.posToVal(cx, "x");

    let lo = 0, hi = series.frames.length - 1;
    let bestIdx = 0, bestDist = Infinity;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      const dist = Math.abs(series.frames[mid] - xVal);
      if (dist < bestDist) { bestDist = dist; bestIdx = mid; }
      if (series.frames[mid] < xVal) lo = mid + 1;
      else hi = mid - 1;
    }
    return series.frames[bestIdx];
  }

  function scrubPlugin(): uPlot.Plugin {
    let scrubbing = false;
    let moveHandler: ((e: MouseEvent) => void) | null = null;
    let upHandler: (() => void) | null = null;

    return {
      hooks: {
        ready: [
          (u: uPlot) => {
            const plot = u.over;
            if (moveHandler) window.removeEventListener("mousemove", moveHandler);
            if (upHandler) window.removeEventListener("mouseup", upHandler);

            plot.addEventListener("mousedown", (e: MouseEvent) => {
              if (e.button !== 0) return;
              scrubbing = true;
              const frame = nearestFrameAtX(u, e.clientX);
              if (frame !== null) onCursorChange(frame);
            });

            moveHandler = (e: MouseEvent) => {
              if (!scrubbing) return;
              const frame = nearestFrameAtX(u, e.clientX);
              if (frame !== null) onCursorChange(frame);
            };
            upHandler = () => { scrubbing = false; };

            window.addEventListener("mousemove", moveHandler);
            window.addEventListener("mouseup", upHandler);
          },
        ],
      },
    };
  }

  // ── Playhead plugin (detail chart) ──

  function playheadPlugin(): uPlot.Plugin {
    return {
      hooks: {
        draw: [
          (u: uPlot) => {
            if (cursorFrame < 0 || !series) return;
            const x = u.valToPos(cursorFrame, "x", true);
            if (x < u.bbox.left || x > u.bbox.left + u.bbox.width) return;

            const ctx = u.ctx;
            ctx.save();
            ctx.strokeStyle = PLAYHEAD_COLOR;
            ctx.lineWidth = 1.5;
            ctx.setLineDash([]);
            ctx.beginPath();
            ctx.moveTo(x, u.bbox.top);
            ctx.lineTo(x, u.bbox.top + u.bbox.height);
            ctx.stroke();

            const idx = series.frames.indexOf(cursorFrame);
            if (idx >= 0 && idx < series.values.length) {
              const y = u.valToPos(series.values[idx], "y", true);
              ctx.fillStyle = PLAYHEAD_DOT_COLOR;
              ctx.beginPath();
              ctx.arc(x, y, 4, 0, Math.PI * 2);
              ctx.fill();
            }
            ctx.restore();
          },
        ],
      },
    };
  }

  // ── Event marker plugin (both charts) ──
  //
  // Draws a translucent band across the plot for every event's run of
  // frames plus a solid tick at the top edge, so single-frame events stay
  // visible when the x-scale is zoomed far out (a 1-frame band may be
  // narrower than a pixel).

  function eventMarkerPlugin(compact: boolean): uPlot.Plugin {
    return {
      hooks: {
        draw: [
          (u: uPlot) => {
            if (!events || events.length === 0) return;
            const ctx = u.ctx;
            const dpr = devicePixelRatio;
            const { left, top, width, height } = u.bbox;
            const right = left + width;
            const tickH = (compact ? 4 : 7) * dpr;

            ctx.save();
            for (const ev of events) {
              const x1 = u.valToPos(ev.frame, "x", true);
              const x2 = u.valToPos(ev.frame + ev.length, "x", true);
              if (x2 < left || x1 > right) continue;

              const flash = ev.kind === "flash";
              const color = (flash ? EVENT_COLORS[ev.subkind] : EVENT_COLORS[ev.kind]) ?? EVENT_COLORS.content;
              ctx.globalAlpha = flash && ev.at_cut ? EVENT_AT_CUT_ALPHA : 1;

              // Band over the run (min 1.5px so it never vanishes)
              const bx1 = Math.max(x1, left);
              const bx2 = Math.min(Math.max(x2, x1 + 1.5 * dpr), right);
              ctx.fillStyle = color;
              ctx.globalAlpha *= 0.18;
              ctx.fillRect(bx1, top, bx2 - bx1, height);

              // Tick at the top edge
              ctx.globalAlpha = flash && ev.at_cut ? EVENT_AT_CUT_ALPHA : 1;
              ctx.fillStyle = color;
              ctx.fillRect(bx1, top, Math.max(bx2 - bx1, 2 * dpr), tickH);
            }
            ctx.restore();
          },
        ],
      },
    };
  }

  // ── Overview playhead plugin ──

  function overviewPlayheadPlugin(): uPlot.Plugin {
    return {
      hooks: {
        draw: [
          (u: uPlot) => {
            if (cursorFrame < 0 || !series) return;
            const x = u.valToPos(cursorFrame, "x", true);
            if (x < u.bbox.left || x > u.bbox.left + u.bbox.width) return;

            const ctx = u.ctx;
            ctx.save();
            ctx.strokeStyle = OVERVIEW_PLAYHEAD_COLOR;
            ctx.lineWidth = 1.5;
            ctx.setLineDash([]);
            ctx.beginPath();
            ctx.moveTo(x, u.bbox.top);
            ctx.lineTo(x, u.bbox.top + u.bbox.height);
            ctx.stroke();
            ctx.restore();
          },
        ],
      },
    };
  }

  // ── Wheel zoom plugin ──

  function wheelZoomPlugin(): uPlot.Plugin {
    return {
      hooks: {
        ready: [
          (u: uPlot) => {
            const plot = u.over;
            plot.addEventListener("wheel", (e: WheelEvent) => {
              e.preventDefault();
              const rect = plot.getBoundingClientRect();
              const cx = e.clientX - rect.left;
              const mouseFrame = u.posToVal(cx, "x");
              const factor = e.deltaY < 0 ? 0.75 : 1.333;
              zoomAroundPoint(mouseFrame, factor);
            }, { passive: false });

            plot.addEventListener("dblclick", () => { zoomReset(); });
          },
        ],
      },
    };
  }

  // ── Scrollbar sync ──

  function syncScrollbar() {
    if (!scrollbarEl || !series) return;
    const range = visibleRange();
    const total = dataRange();
    if (range >= total) {
      scrollbarEl.style.display = "none";
      return;
    }
    scrollbarEl.style.display = "block";
    // Map visible range position to scrollbar (0–1000)
    const pos = ((visibleMin() - dataMin()) / (total - range)) * 1000;
    scrollbarEl.value = String(Math.round(pos));
  }

  function handleScrollbarInput() {
    if (!scrollbarEl || !detailPlot || !series) return;
    const range = visibleRange();
    const total = dataRange();
    const pos = parseFloat(scrollbarEl.value) / 1000;
    const newMin = dataMin() + pos * (total - range);
    setXRange(newMin, newMin + range);
  }

  // ── Reactive effects ──

  // Redraw + auto-follow on cursorFrame change
  let prevCursorFrame = -1;
  $effect(() => {
    if (cursorFrame !== prevCursorFrame) {
      prevCursorFrame = cursorFrame;
      ensurePlayheadVisible();
      detailPlot?.redraw(false);
      overviewPlot?.redraw(false);
    }
  });

  // ── uPlot config ──

  function detailOpts(width: number, height: number): uPlot.Options {
    const color = getColor();

    return {
      width,
      height,
      cursor: {
        drag: { x: false, y: false, setScale: false },
        points: { show: false },
      },
      hooks: {
        setScale: [
          (u: uPlot, key: string) => {
            if (key === "x") {
              currentXMin = u.scales.x.min ?? null;
              currentXMax = u.scales.x.max ?? null;

              // Sync overview selection brush
              if (overviewPlot && currentXMin != null && currentXMax != null) {
                const left = overviewPlot.valToPos(currentXMin, "x");
                const right = overviewPlot.valToPos(currentXMax, "x");
                overviewPlot.setSelect(
                  {
                    left,
                    width: right - left,
                    top: 0,
                    height: overviewPlot.bbox.height / devicePixelRatio,
                  },
                  false
                );
              }

              // Sync scrollbar
              syncScrollbar();
            }
          },
        ],
      },
      scales: {
        x: { time: false },
        y: {
          auto: false,
          range: () => yRange(),
        },
      },
      axes: [
        {
          stroke: AXIS_COLOR,
          grid: { stroke: GRID_COLOR, width: 1 },
          ticks: { stroke: GRID_COLOR, width: 1 },
          font: "11px 'JetBrains Mono', monospace",
          gap: 5,
          size: 40,
          label: "Frame",
          labelFont: "11px Inter, sans-serif",
          labelGap: 4,
        },
        {
          stroke: AXIS_COLOR,
          grid: { stroke: GRID_COLOR, width: 1 },
          ticks: { stroke: GRID_COLOR, width: 1 },
          font: "11px 'JetBrains Mono', monospace",
          gap: 5,
          size: 50,
          label: getLabel(),
          labelFont: "11px Inter, sans-serif",
          labelGap: 4,
        },
      ],
      series: [
        {},
        {
          stroke: color,
          width: 1,
          fill: color + "18",
          points: { show: sparse, size: 5, fill: color },
          label: getLabel(),
        },
      ],
      plugins: [
        // Threshold line
        {
          hooks: {
            draw: [
              (u: uPlot) => {
                if (threshold == null) return;
                const ctx = u.ctx;
                const y = u.valToPos(threshold, "y", true);
                if (y < u.bbox.top || y > u.bbox.top + u.bbox.height) return;

                ctx.save();
                ctx.strokeStyle = THRESHOLD_COLOR;
                ctx.lineWidth = 1;
                ctx.setLineDash([4, 4]);
                ctx.beginPath();
                ctx.moveTo(u.bbox.left, y);
                ctx.lineTo(u.bbox.left + u.bbox.width, y);
                ctx.stroke();

                ctx.fillStyle = THRESHOLD_COLOR;
                ctx.font = "10px 'JetBrains Mono', monospace";
                ctx.textAlign = "left";
                const isLevels = metric.startsWith("y_") || metric.startsWith("u_") || metric.startsWith("v_");
                const precision = isLevels ? 0 : metric.startsWith("ssim") ? 4 : 1;
                ctx.fillText(`${threshold.toFixed(precision)}`, u.bbox.left + 4, y - 4);
                ctx.restore();
              },
            ],
          },
        },
        wheelZoomPlugin(),
        scrubPlugin(),
        eventMarkerPlugin(false),
        playheadPlugin(),
        noDataZonePlugin(),
      ],
    };
  }

  function overviewOpts(width: number, height: number): uPlot.Options {
    const color = getColor();

    return {
      width,
      height,
      cursor: {
        drag: { x: true, y: false, setScale: false },
      },
      hooks: {
        setSelect: [
          (u: uPlot) => {
            const sel = u.select;
            if (sel.width > 2 && detailPlot) {
              const min = u.posToVal(sel.left, "x");
              const max = u.posToVal(sel.left + sel.width, "x");
              detailPlot.setScale("x", { min, max });
            }
          },
        ],
      },
      select: { show: true, left: 0, top: 0, width: 0, height: 0 },
      legend: { show: false },
      scales: {
        x: { time: false },
        y: { auto: true },
      },
      axes: [
        { show: false },
        { show: false },
      ],
      series: [
        {},
        {
          stroke: color + "99",
          width: 1,
          fill: color + "22",
          points: { show: false },
        },
      ],
      plugins: [
        eventMarkerPlugin(true),
        overviewPlayheadPlugin(),
        noDataZonePlugin(),
      ],
    };
  }

  // ── Lifecycle ──

  function createCharts() {
    if (!series || !detailEl || !overviewEl) return;

    detailPlot?.destroy();
    overviewPlot?.destroy();

    const rect = detailEl.parentElement!.getBoundingClientRect();
    const w = Math.floor(rect.width);
    const overviewH = 60;
    const detailH = Math.floor(rect.height) - overviewH - 24; // 24 for scrollbar

    if (w < 50 || detailH < 50) return;

    const data = buildData(series) as uPlot.AlignedData;

    detailPlot = new uPlot(
      detailOpts(w, Math.max(detailH, 100)),
      data,
      detailEl
    );

    overviewPlot = new uPlot(
      overviewOpts(w, overviewH),
      data,
      overviewEl
    );

    // Restore previous zoom state
    if (currentXMin != null && currentXMax != null) {
      detailPlot.setScale("x", { min: currentXMin, max: currentXMax });
    }

    syncScrollbar();
  }

  let resizeObserver: ResizeObserver;

  onMount(() => {
    createCharts();
    resizeObserver = new ResizeObserver(() => { createCharts(); });
    if (detailEl?.parentElement) {
      resizeObserver.observe(detailEl.parentElement);
    }
  });

  onDestroy(() => {
    detailPlot?.destroy();
    overviewPlot?.destroy();
    resizeObserver?.disconnect();
  });

  $effect(() => {
    void series;
    void metric;
    void threshold;
    requestAnimationFrame(createCharts);
  });

  // Events change without the series changing — a redraw is enough.
  $effect(() => {
    void events;
    detailPlot?.redraw(false);
    overviewPlot?.redraw(false);
  });
</script>

<div class="chart-wrapper">
  <div class="detail-chart" bind:this={detailEl}></div>
  <input
    type="range"
    class="chart-scrollbar"
    bind:this={scrollbarEl}
    min="0"
    max="1000"
    value="0"
    oninput={handleScrollbarInput}
  />
  <div class="overview-chart" bind:this={overviewEl}></div>
</div>

<style>
  .chart-wrapper {
    width: 100%;
    height: 100%;
    display: flex;
    flex-direction: column;
    background: var(--bg-surface);
    padding: var(--gap-xs);
    gap: 2px;
  }
  .detail-chart {
    flex: 1;
    min-height: 0;
    cursor: crosshair;
  }
  .chart-scrollbar {
    width: 100%;
    height: 14px;
    margin: 0;
    padding: 0;
    -webkit-appearance: none;
    appearance: none;
    background: transparent;
    cursor: pointer;
    display: none; /* shown only when zoomed */
  }
  .chart-scrollbar::-webkit-slider-runnable-track {
    height: 6px;
    background: var(--bg-overlay, rgba(255,255,255,0.06));
    border-radius: 3px;
  }
  .chart-scrollbar::-webkit-slider-thumb {
    -webkit-appearance: none;
    appearance: none;
    width: 40px;
    height: 10px;
    background: rgba(255,255,255,0.2);
    border-radius: 5px;
    margin-top: -2px;
    cursor: grab;
  }
  .chart-scrollbar::-webkit-slider-thumb:hover {
    background: rgba(255,255,255,0.35);
  }
  .chart-scrollbar::-moz-range-track {
    height: 6px;
    background: var(--bg-overlay, rgba(255,255,255,0.06));
    border-radius: 3px;
    border: none;
  }
  .chart-scrollbar::-moz-range-thumb {
    width: 40px;
    height: 10px;
    background: rgba(255,255,255,0.2);
    border-radius: 5px;
    border: none;
    cursor: grab;
  }
  .chart-scrollbar::-moz-range-thumb:hover {
    background: rgba(255,255,255,0.35);
  }
  .overview-chart {
    flex: 0 0 60px;
  }
  .chart-wrapper :global(.u-wrap) {
    background: transparent !important;
  }
  .chart-wrapper :global(.u-select) {
    background: rgba(90, 143, 212, 0.35) !important;
    border-left: 1px solid rgba(90, 143, 212, 0.7) !important;
    border-right: 1px solid rgba(90, 143, 212, 0.7) !important;
  }
  .overview-chart :global(.u-select) {
    background: rgba(90, 143, 212, 0.25) !important;
    border-left: 1px solid rgba(90, 143, 212, 0.6) !important;
    border-right: 1px solid rgba(90, 143, 212, 0.6) !important;
  }
</style>
