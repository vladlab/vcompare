<!--
  TimelineBar.svelte — Dual-bar visualization showing how reference and
  distorted clips relate in timecode space. Shows overlap region and
  non-coverage areas.

  Props:
    timeline: TimelineInfo from the analysis summary
    tcFormat: { fps_num, fps_den, tc_type } for TC string formatting
    onSeek?: (frame: number) => void — called when user clicks a TC position
-->
<script lang="ts">
  interface TimelineInfo {
    ref_start: number;
    ref_end: number;
    ref_filename: string;
    dist_start: number;
    dist_end: number;
    dist_filename: string;
    overlap_start: number;
    overlap_end: number;
    span_start: number;
    span_end: number;
  }

  interface TcFormat {
    fps_num: number;
    fps_den: number;
    tc_type: string;
  }

  let {
    timeline,
    tcFormat,
    onSeek,
  }: {
    timeline: TimelineInfo;
    tcFormat: TcFormat | null;
    onSeek?: (frame: number) => void;
  } = $props();

  let spanWidth = $derived(timeline.span_end - timeline.span_start);

  function pct(frame: number): number {
    if (spanWidth <= 0) return 0;
    return ((frame - timeline.span_start) / spanWidth) * 100;
  }

  function barStyle(start: number, end: number): string {
    const left = pct(start);
    const width = pct(end) - pct(start);
    return `left: ${left}%; width: ${Math.max(width, 0.3)}%`;
  }

  // ── Timecode formatting (matches engine's frame_to_tc) ──
  function frameToTc(frame: number): string {
    if (!tcFormat) return `F${frame}`;
    const { fps_num, fps_den, tc_type } = tcFormat;

    if (tc_type === "DF") {
      return formatDropFrame(frame, fps_num, fps_den);
    }

    const fps = Math.round(fps_num / fps_den);
    const f = ((frame % fps) + fps) % fps;
    let remaining = (frame - f) / fps;
    const s = ((remaining % 60) + 60) % 60;
    remaining = (remaining - s) / 60;
    const m = ((remaining % 60) + 60) % 60;
    const h = (remaining - m) / 60;

    return `${pad(h)}:${pad(m)}:${pad(s)}:${pad(f, fps >= 100 ? 3 : 2)}`;
  }

  function formatDropFrame(frame: number, fpsNum: number, fpsDen: number): string {
    const nominalFps = Math.round(fpsNum / fpsDen);
    const dropPerMin = nominalFps === 30 ? 2 : 4;
    const framesPerSec = nominalFps;
    const framesPerMin = framesPerSec * 60 - dropPerMin;
    const framesPer10Min = framesPerMin * 10 + dropPerMin;

    let f = frame;
    const tenMins = Math.floor(f / framesPer10Min);
    let rem = f - tenMins * framesPer10Min;

    let mins;
    if (rem < dropPerMin) {
      mins = 0;
    } else {
      mins = Math.floor((rem - dropPerMin) / framesPerMin) + 1;
    }

    if (mins > 0) {
      rem = rem - dropPerMin - (mins - 1) * framesPerMin;
    }

    const totalMins = tenMins * 10 + mins;
    const h = Math.floor(totalMins / 60);
    const m = totalMins % 60;
    const s = Math.floor(rem / framesPerSec);
    const ff = rem % framesPerSec;

    return `${pad(h)}:${pad(m)}:${pad(s)};${pad(ff)}`;
  }

  function pad(n: number, width: number = 2): string {
    return Math.abs(Math.floor(n)).toString().padStart(width, "0");
  }

  // ── TC tick marks ──
  // Generate evenly spaced TC markers across the span
  let tcTicks = $derived.by(() => {
    if (!tcFormat || spanWidth <= 0) return [];

    const fps = tcFormat.fps_num / tcFormat.fps_den;
    // Target ~6-8 ticks. Find a nice interval in seconds.
    const spanSec = spanWidth / fps;
    const intervals = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1800, 3600];
    const targetCount = 7;
    let intervalSec = 10;
    for (const iv of intervals) {
      if (spanSec / iv <= targetCount + 2) {
        intervalSec = iv;
        break;
      }
    }

    const intervalFrames = Math.round(intervalSec * fps);
    const firstTick =
      Math.ceil(timeline.span_start / intervalFrames) * intervalFrames;

    const ticks: { frame: number; tc: string; pct: number }[] = [];
    for (let f = firstTick; f <= timeline.span_end; f += intervalFrames) {
      ticks.push({
        frame: f,
        tc: frameToTc(f),
        pct: pct(f),
      });
    }
    return ticks;
  });

  let hasOffset = $derived(
    timeline.ref_start !== timeline.dist_start ||
    timeline.ref_end !== timeline.dist_end
  );

  function handleClick(e: MouseEvent) {
    if (!onSeek) return;
    const el = e.currentTarget as HTMLElement;
    const rect = el.getBoundingClientRect();
    const x = (e.clientX - rect.left) / rect.width;
    const frame = Math.round(timeline.span_start + x * spanWidth);
    onSeek(frame);
  }
</script>

{#if spanWidth > 0}
<div class="timeline-bar" class:clickable={!!onSeek}>
  <!-- TC tick marks -->
  <div class="tc-axis">
    {#each tcTicks as tick}
      <span class="tc-tick" style="left: {tick.pct}%">
        <span class="tc-line"></span>
        <span class="tc-label">{tick.tc}</span>
      </span>
    {/each}
  </div>

  <!-- Clip bars -->
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="bars" onclick={handleClick}>
    <!-- Reference bar -->
    <div class="bar-row">
      <span class="bar-label ref-label">REF</span>
      <div class="bar-track">
        <div class="bar ref-bar" style={barStyle(timeline.ref_start, timeline.ref_end)}>
        </div>
      </div>
    </div>

    <!-- Distorted bar -->
    <div class="bar-row">
      <span class="bar-label dist-label">DIST</span>
      <div class="bar-track">
        <div class="bar dist-bar" style={barStyle(timeline.dist_start, timeline.dist_end)}>
        </div>
      </div>
    </div>

    <!-- Overlap bracket -->
    {#if hasOffset}
      <div class="overlap-bracket" style={barStyle(timeline.overlap_start, timeline.overlap_end)}>
        <span class="overlap-label">overlap</span>
      </div>
    {/if}
  </div>

  <!-- Filenames -->
  <div class="filenames">
    <span class="fn ref-fn" title={timeline.ref_filename}>
      {timeline.ref_filename}
    </span>
    <span class="fn dist-fn" title={timeline.dist_filename}>
      {timeline.dist_filename}
    </span>
  </div>
</div>
{/if}

<style>
  .timeline-bar {
    --ref-color: #4a9eff;
    --dist-color: #ff6b8a;
    --overlap-color: #ffffffcc;
    --track-bg: #1a1a1a;
    --border-color: #333;
    --text-dim: #888;
    --text-bright: #ccc;

    padding: 8px 12px 6px;
    background: #111;
    border: 1px solid var(--border-color);
    border-radius: 4px;
    font-family: "JetBrains Mono", "SF Mono", "Fira Code", monospace;
    font-size: 10px;
    user-select: none;
  }

  .timeline-bar.clickable {
    cursor: crosshair;
  }

  /* TC axis */
  .tc-axis {
    position: relative;
    height: 18px;
    margin-left: 36px; /* match bar-label width */
  }

  .tc-tick {
    position: absolute;
    transform: translateX(-50%);
    display: flex;
    flex-direction: column;
    align-items: center;
  }

  .tc-line {
    display: block;
    width: 1px;
    height: 4px;
    background: #444;
  }

  .tc-label {
    color: var(--text-dim);
    white-space: nowrap;
    font-size: 9px;
    letter-spacing: 0.02em;
  }

  /* Bars */
  .bars {
    position: relative;
    margin-left: 36px;
  }

  .bar-row {
    display: flex;
    align-items: center;
    height: 14px;
    margin-bottom: 2px;
  }

  .bar-label {
    position: absolute;
    left: -36px;
    width: 32px;
    text-align: right;
    font-size: 9px;
    font-weight: 600;
    letter-spacing: 0.05em;
  }

  .ref-label { color: var(--ref-color); }
  .dist-label { color: var(--dist-color); }

  .bar-track {
    flex: 1;
    position: relative;
    height: 10px;
    background: var(--track-bg);
    border-radius: 2px;
    overflow: hidden;
  }

  .bar {
    position: absolute;
    top: 0;
    height: 100%;
    border-radius: 2px;
    min-width: 2px;
  }

  .ref-bar {
    background: var(--ref-color);
    opacity: 0.75;
  }

  .dist-bar {
    background: var(--dist-color);
    opacity: 0.75;
  }

  /* Overlap bracket */
  .overlap-bracket {
    position: absolute;
    bottom: -4px;
    height: 32px; /* spans both bars + gap */
    top: 0px;
    border: 1px dashed var(--overlap-color);
    border-radius: 2px;
    pointer-events: none;
    opacity: 0.4;
  }

  .overlap-label {
    position: absolute;
    bottom: -12px;
    left: 50%;
    transform: translateX(-50%);
    color: var(--text-dim);
    font-size: 8px;
    letter-spacing: 0.05em;
    white-space: nowrap;
  }

  /* Filenames */
  .filenames {
    display: flex;
    justify-content: space-between;
    margin-top: 8px;
    margin-left: 36px;
    gap: 12px;
  }

  .fn {
    font-size: 9px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 45%;
  }

  .ref-fn { color: var(--ref-color); opacity: 0.6; }
  .dist-fn { color: var(--dist-color); opacity: 0.6; }
</style>
