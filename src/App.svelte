<!--
  vcompare — main application shell.

  Analysis modes:
    - diff (default): fast pixel difference (blend + signalstats)
    - vmaf: perceptual quality (libvmaf)
-->

<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { open, save } from "@tauri-apps/plugin-dialog";
  import {
    type AnalysisSummary,
    type FileProbe,
    type MetricSeries,
    type FrameResult,
    type Capabilities,
    type CacheCheck,
    type ProgressEvent,
    type DiagnosticsEvent,
    type UnlistenFn,
    ping,
    detectCapabilities,
    probeFile,
    checkCache,
    loadSavedAnalysis,
    analyze,
    getMetricSeries,
    getFrames,
    exportCsv,
    mpvLaunch,
    mpvSeek,
    mpvStep,
    mpvTogglePause,
    mpvKill,
    onProgress,
    onDiagnostics,
    onMpvPosition,
  } from "./lib/rpc";
  import MetricChart from "./components/MetricChart.svelte";
  import FrameTable from "./components/FrameTable.svelte";
  import TimelineBar from "./components/TimelineBar.svelte";

  // ── App state ──

  type AppState = "idle" | "probing" | "analyzing" | "ready" | "error";

  let state: AppState = $state("idle");
  let errorMessage: string = $state("");

  // File selection
  let refPath: string = $state("");
  let distPath: string = $state("");
  let refProbe: FileProbe | null = $state(null);
  let distProbe: FileProbe | null = $state(null);
  let lastDir: string | undefined = $state(undefined);

  // Capabilities
  let caps: Capabilities | null = $state(null);

  // Cached analysis detection
  let cacheInfo: CacheCheck | null = $state(null);

  // Analysis options
  let analysisMode: "diff" | "vmaf" = $state("diff");
  let syncMode: string = $state("start");
  let hwaccelMode: string = $state("auto");
  let wantPsnr: boolean = $state(false);
  let subsample: number = $state(1);

  // Analysis results
  let summary: AnalysisSummary | null = $state(null);
  let metricSeries: Record<string, MetricSeries> = $state({});

  // Progress
  let progressCurrent: number = $state(0);
  let progressTotal: number = $state(0);
  let diagnosticLines: string[] = $state([]);

  // View state
  let activeMetric: string = $state("diff_y");
  let cursorFrame: number = $state(-1);
  let threshold: number | null = $state(null);

  // mpv state
  let mpvRunning: boolean = $state(false);
  let mpvAutoSeek: boolean = $state(false);
  let chartComponent: MetricChart | undefined = $state();
  let mpvFrame: number = $state(-1);

  // Debounced seek
  let seekTimer: ReturnType<typeof setTimeout> | null = null;
  let seekGeneration = 0;

  function debouncedMpvSeek(frame: number) {
    if (!mpvRunning) return;
    seekGeneration++;
    const gen = seekGeneration;
    if (seekTimer) clearTimeout(seekTimer);
    seekTimer = setTimeout(() => {
      if (gen === seekGeneration) {
        mpvSeek(frame).catch(() => {});
      }
    }, 50);
  }

  // Event listener cleanup
  let unlisteners: UnlistenFn[] = [];

  // ── Lifecycle ──

  onMount(async () => {
    unlisteners.push(await onProgress(handleProgress));
    unlisteners.push(await onDiagnostics(handleDiagnostics));
    unlisteners.push(
      await onMpvPosition((e) => {
        mpvFrame = e.frame;
      })
    );

    const { listen } = await import("@tauri-apps/api/event");
    unlisteners.push(
      await listen("rpc:mpv-exited", () => {
        mpvRunning = false;
        mpvAutoSeek = false;
        mpvFrame = -1;
      })
    );

    unlisteners.push(
      await listen("menu-event", (event) => {
        const id = event.payload as string;
        if (id === "new-analysis") {
          // Kill mpv if running
          mpvKill().catch(() => {});
          // Reset to idle
          state = "idle";
          refPath = "";
          distPath = "";
          refProbe = null;
          distProbe = null;
          cacheInfo = null;
          summary = null;
          metricSeries = {};
          progressCurrent = 0;
          progressTotal = 0;
          diagnosticLines = [];
          cursorFrame = -1;
          mpvRunning = false;
          mpvAutoSeek = false;
          mpvFrame = -1;
        }
      })
    );

    try {
      await ping();
      caps = await detectCapabilities();
    } catch (e) {
      state = "error";
      errorMessage = `Engine initialization failed: ${e}`;
    }
  });

  onDestroy(() => {
    unlisteners.forEach((fn) => fn());
  });

  // ── Bidirectional mpv sync ──
  $effect(() => {
    if (mpvAutoSeek && mpvFrame >= 0) {
      cursorFrame = mpvFrame;
    }
  });

  // ── File picking ──

  const VIDEO_FILTERS = [
    {
      name: "Video Files",
      extensions: [
        "mxf", "mov", "mp4", "mkv", "avi", "m4v", "mpg", "mpeg",
        "ts", "m2ts", "webm", "ogv", "mj2", "xml", "txt",
      ],
    },
    { name: "All Files", extensions: ["*"] },
  ];

  async function pickReference() {
    const result = await open({
      title: "Select reference video",
      filters: VIDEO_FILTERS,
      defaultPath: lastDir,
    });
    if (result) {
      refPath = result as string;
      lastDir = refPath.substring(0, refPath.lastIndexOf("/")) || undefined;
      try {
        refProbe = await probeFile(refPath);
      } catch (e) {
        refProbe = null;
      }
      if (distPath) await probeSelected();
    }
  }

  async function pickDistorted() {
    const result = await open({
      title: "Select distorted video",
      filters: VIDEO_FILTERS,
      defaultPath: lastDir,
    });
    if (result) {
      distPath = result as string;
      lastDir = distPath.substring(0, distPath.lastIndexOf("/")) || undefined;
      try {
        distProbe = await probeFile(distPath);
      } catch (e) {
        distProbe = null;
      }
      if (refPath) await probeSelected();
    }
  }

  async function probeSelected() {
    if (!refPath || !distPath) return;
    state = "probing";
    try {
      [refProbe, distProbe] = await Promise.all([
        probeFile(refPath),
        probeFile(distPath),
      ]);
      try {
        cacheInfo = await checkCache(refPath, distPath);
      } catch {
        cacheInfo = null;
      }
      state = "idle";
    } catch (e) {
      state = "error";
      errorMessage = `Probe failed: ${e}`;
    }
  }

  // ── Analysis ──

  async function finishAnalysis(result: AnalysisSummary) {
    summary = result;
    metricSeries = {};

    // Fetch series for each available metric
    for (const m of result.metrics_available) {
      try {
        metricSeries[m] = await getMetricSeries(m);
      } catch (e) {
        console.error(`Failed to fetch series for ${m}:`, e);
      }
    }

    // Set active metric to first available
    if (result.metrics_available.length > 0) {
      activeMetric = result.metrics_available[0];
    }

    // Auto-set threshold
    if (result.mode === "diff" && result.diff_y_stats) {
      const s = result.diff_y_stats;
      // For diff: threshold = mean + 1σ (flag things above)
      threshold = Math.round((s.mean + (s.stddev ?? 0)) * 100) / 100;
    } else if (result.mode === "vmaf" && result.vmaf_stats) {
      const s = result.vmaf_stats;
      // For vmaf: threshold = mean - 1σ (flag things below)
      threshold = Math.round((s.mean - (s.stddev ?? 0)) * 100) / 100;
    }

    state = "ready";
  }

  async function startAnalysis() {
    state = "analyzing";
    progressCurrent = 0;
    progressTotal = 0;
    diagnosticLines = [];

    try {
      const result = await analyze({
        reference: refPath,
        distorted: distPath,
        mode: analysisMode,
        hwaccel: hwaccelMode,
        sync: syncMode,
        psnr: wantPsnr,
        subsample,
        log_path: cacheInfo?.path,
        force: true,
      });
      await finishAnalysis(result);
    } catch (e) {
      state = "error";
      errorMessage = `Analysis failed: ${e}`;
    }
  }

  async function loadCached() {
    if (!cacheInfo?.path) return;
    state = "analyzing";
    try {
      const result = await loadSavedAnalysis(cacheInfo.path);
      await finishAnalysis(result);
    } catch (e) {
      state = "error";
      errorMessage = `Failed to load cached analysis: ${e}`;
    }
  }

  // ── Event handlers ──

  function handleProgress(e: ProgressEvent) {
    progressCurrent = e.current;
    progressTotal = e.total;
  }

  function handleDiagnostics(e: DiagnosticsEvent) {
    diagnosticLines = e.lines;
  }

  function handleCursorChange(frame: number) {
    cursorFrame = frame;
    if (mpvRunning) {
      debouncedMpvSeek(frame);
    }
  }

  // ── Keyboard shortcuts ──

  function handleKeydown(e: KeyboardEvent) {
    if (state !== "ready") return;
    if ((e.target as HTMLElement)?.tagName === "INPUT") return;

    switch (e.key) {
      case "m":
        // Cycle through available metrics
        if (summary) {
          const avail = summary.metrics_available;
          const idx = avail.indexOf(activeMetric);
          activeMetric = avail[(idx + 1) % avail.length];
        }
        break;
      case "t":
        // Recalculate threshold from active metric stats
        if (summary) {
          const statsKey = `${activeMetric}_stats` as keyof AnalysisSummary;
          const stats = summary[statsKey] as any;
          if (stats) {
            if (summary.mode === "diff") {
              threshold = Math.round((stats.mean + (stats.stddev ?? 0)) * 100) / 100;
            } else {
              threshold = Math.round((stats.mean - (stats.stddev ?? 0)) * 100) / 100;
            }
          }
        }
        break;
      case "T":
        threshold = null;
        break;
      case "p":
        if (!mpvRunning) {
          mpvLaunch().then(() => (mpvRunning = true)).catch(() => {});
        }
        break;
      case "P":
        mpvKill().then(() => (mpvRunning = false));
        break;
      case "s":
        if (mpvRunning && cursorFrame >= 0) mpvSeek(cursorFrame);
        break;
      case "a":
        mpvAutoSeek = !mpvAutoSeek;
        break;
      case ".":
        if (mpvRunning) mpvStep("forward");
        break;
      case ",":
        if (mpvRunning) mpvStep("backward");
        break;
      case " ":
        if (mpvRunning) {
          e.preventDefault();
          mpvTogglePause();
        }
        break;
      case "e":
        handleExport();
        break;
      case "=":
      case "+":
        chartComponent?.zoomIn();
        break;
      case "-":
        chartComponent?.zoomOut();
        break;
      case "0":
        chartComponent?.zoomReset();
        break;
    }
  }

  async function handleExport() {
    const result = await save({
      title: "Export CSV",
      defaultPath: "vcompare_export.csv",
      filters: [{ name: "CSV", extensions: ["csv"] }],
    });
    if (result) {
      await exportCsv(result as string);
    }
  }

  // ── Derived values ──

  let progressPct = $derived(
    progressTotal > 0 ? Math.round((progressCurrent / progressTotal) * 100) : 0
  );

  let activeSeries = $derived(metricSeries[activeMetric] ?? null);

  let activeStats = $derived(() => {
    if (!summary) return null;
    const key = `${activeMetric}_stats` as keyof AnalysisSummary;
    return (summary[key] as any) ?? null;
  });

  function fmtProbe(p: FileProbe | null): string {
    if (!p) return "—";
    const fps = p.fps_den === 1001
      ? `${(p.fps_num / p.fps_den).toFixed(2)}`
      : `${p.fps_num / p.fps_den}`;
    return `${p.width}×${p.height} ${p.codec} ${p.pix_fmt} ${fps}fps`;
  }

  function fmtMetricLabel(m: string): string {
    const labels: Record<string, string> = {
      diff_y: "Diff Y",
      diff_u: "Diff U",
      diff_v: "Diff V",
      vmaf: "VMAF",
      psnr: "PSNR",
    };
    return labels[m] ?? m;
  }
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="app">
  <!-- ── Header bar ── -->
  <header class="header">
    <span class="header-title">vcompare</span>
    {#if state === "ready" && summary}
      <span class="header-badge">{summary.mode}</span>
      <span class="header-meta mono">
        {summary.frame_count.toLocaleString()} frames
        {#if summary.tc_format}
          · {summary.tc_format.fps_num}/{summary.tc_format.fps_den}fps
        {/if}
        {#if activeStats()}
          · {fmtMetricLabel(activeMetric)}
          μ={activeStats().mean.toFixed(2)}
          {summary.mode === "diff" ? "max" : "min"}={summary.mode === "diff" ? activeStats().max.toFixed(2) : activeStats().min.toFixed(2)}
        {/if}
      </span>
      {#if summary.mode === "diff" && summary.diff_distribution}
        <span class="header-meta mono">
          · {summary.diff_distribution.zero} identical
          · {summary.diff_distribution.above_10} major
        </span>
      {/if}
    {/if}
    {#if mpvRunning}
      <span class="header-badge">mpv</span>
      <span class="header-meta mono">
        sync={mpvAutoSeek ? "bidi" : "fwd"}
      </span>
    {/if}
  </header>

  <!-- ── Main content ── -->
  <main class="main">

    {#if state === "idle" || state === "probing"}
      <!-- File picker panel -->
      <div class="picker-panel">
        <div class="picker-section">
          <h2>Compare Videos</h2>
          <p class="text-secondary">
            Select a reference and distorted file to compare.
          </p>
        </div>

        <div class="file-row">
          <label>Reference</label>
          <button onclick={pickReference} class="file-btn">
            {refPath ? refPath.split("/").pop() : "Choose file…"}
          </button>
          {#if refProbe}
            <span class="file-meta mono">{fmtProbe(refProbe)}</span>
          {/if}
        </div>

        <div class="file-row">
          <label>Distorted</label>
          <button onclick={pickDistorted} class="file-btn">
            {distPath ? distPath.split("/").pop() : "Choose file…"}
          </button>
          {#if distProbe}
            <span class="file-meta mono">{fmtProbe(distProbe)}</span>
          {/if}
        </div>

        {#if refProbe && distProbe}
          {#if Math.abs(refProbe.fps - distProbe.fps) > 0.01}
            <div class="fps-warning">
              ⚠ Framerate mismatch: ref {refProbe.fps_num}/{refProbe.fps_den} ({refProbe.fps.toFixed(3)}fps) vs dist {distProbe.fps_num}/{distProbe.fps_den} ({distProbe.fps.toFixed(3)}fps) — results may be unreliable
            </div>
          {/if}
          <div class="options-row">
            <label>
              Mode
              <select bind:value={analysisMode}>
                <option value="diff">Pixel Diff</option>
                <option value="vmaf">VMAF</option>
              </select>
            </label>
            <label>
              Sync
              <select bind:value={syncMode}>
                <option value="start">Start-aligned</option>
                <option value="tc">Timecode</option>
              </select>
            </label>
            {#if analysisMode === "vmaf"}
              <label>
                HW Accel
                <select bind:value={hwaccelMode}>
                  <option value="auto">Auto{caps?.hwaccel ? ` (${caps.hwaccel})` : ""}</option>
                  <option value="cuda">CUDA</option>
                  <option value="vaapi">VAAPI</option>
                  <option value="none">None</option>
                </select>
              </label>
              <label class="checkbox-label">
                <input type="checkbox" bind:checked={wantPsnr} />
                PSNR
              </label>
              <label>
                Subsample
                <select bind:value={subsample}>
                  <option value={1}>1× (every frame)</option>
                  <option value={2}>2×</option>
                  <option value={4}>4×</option>
                  <option value={8}>8×</option>
                </select>
              </label>
            {/if}
          </div>

          <button class="primary analyze-btn" onclick={startAnalysis}>
            {cacheInfo?.exists ? "Re-analyze" : "Analyze"}
          </button>

          {#if cacheInfo?.exists}
            <div class="cache-row">
              <span class="cache-info mono">
                Cached ({cacheInfo.mode ?? "vmaf"}): {cacheInfo.frame_count?.toLocaleString()} frames
                ({cacheInfo.path.split("/").pop()})
              </span>
              <button class="primary" onclick={loadCached}>
                Load cached
              </button>
            </div>
          {/if}
        {/if}
      </div>

    {:else if state === "analyzing"}
      <!-- Progress view -->
      <div class="progress-panel">
        <h2>Analyzing… ({analysisMode === "diff" ? "pixel diff" : "VMAF"})</h2>
        <div class="progress-bar-container">
          <div class="progress-bar" style="width: {progressPct}%"></div>
        </div>
        <div class="progress-label mono">
          {#if progressTotal > 0}
            {progressCurrent.toLocaleString()} / {progressTotal.toLocaleString()} frames
            ({progressPct}%)
          {:else}
            Starting FFmpeg…
          {/if}
        </div>
        {#if diagnosticLines.length > 0}
          <pre class="diagnostics mono">{diagnosticLines.join("\n")}</pre>
        {/if}
      </div>

    {:else if state === "ready" && activeSeries}
      <!-- Results view -->
      <div class="results-layout">
        {#if summary?.timeline}
          <TimelineBar
            timeline={summary.timeline}
            tcFormat={summary.tc_format}
          />
        {/if}
        <div class="chart-area">
          <MetricChart
            bind:this={chartComponent}
            series={activeSeries}
            metric={activeMetric}
            {threshold}
            {cursorFrame}
            onCursorChange={handleCursorChange}
            timeline={summary?.timeline}
          />
        </div>
        <div class="table-area">
          <FrameTable
            {cursorFrame}
            metrics={summary?.metrics_available ?? []}
            mode={summary?.mode ?? "diff"}
            onFrameSelect={handleCursorChange}
          />
        </div>
      </div>

    {:else if state === "error"}
      <div class="error-panel">
        <h2 class="text-error">Error</h2>
        <pre class="error-message mono">{errorMessage}</pre>
        <button onclick={() => (state = "idle")}>Back</button>
      </div>
    {/if}
  </main>

  <!-- ── Footer ── -->
  {#if state === "ready"}
    <footer class="footer">
      <span class="footer-shortcuts">
        <kbd>m</kbd> metric
        <kbd>t</kbd> threshold
        <kbd>e</kbd> export
        <kbd>+</kbd><kbd>-</kbd> zoom
        <kbd>0</kbd> reset
        <kbd>p</kbd> mpv
        <kbd>s</kbd> seek
        <kbd>a</kbd> sync {mpvAutoSeek ? "↔ bidi" : "→ fwd"}
        <kbd>.</kbd><kbd>,</kbd> step
        <kbd>space</kbd> pause
      </span>
    </footer>
  {/if}
</div>

<style>
  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
    overflow: hidden;
  }

  .header {
    display: flex;
    align-items: center;
    gap: var(--gap-md);
    padding: 6px var(--gap-lg);
    background: var(--bg-surface);
    border-bottom: 1px solid var(--border-subtle);
    min-height: 34px;
    flex-shrink: 0;
  }
  .header-title {
    font-weight: 600;
    font-size: 14px;
    letter-spacing: 0.02em;
  }
  .header-meta {
    color: var(--text-secondary);
    font-size: 11px;
  }
  .header-badge {
    font-size: 10px;
    font-weight: 600;
    padding: 1px 6px;
    border-radius: 3px;
    background: var(--accent-dim);
    color: #fff;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .main {
    flex: 1;
    overflow: hidden;
    display: flex;
    align-items: stretch;
  }

  /* File picker */
  .picker-panel {
    max-width: 560px;
    margin: auto;
    padding: var(--gap-xl);
    display: flex;
    flex-direction: column;
    gap: var(--gap-lg);
  }
  .picker-section h2 {
    font-size: 18px;
    font-weight: 600;
    margin-bottom: var(--gap-xs);
  }
  .file-row {
    display: flex;
    align-items: center;
    gap: var(--gap-md);
  }
  .file-row label {
    width: 80px;
    font-weight: 500;
    color: var(--text-secondary);
    text-align: right;
    flex-shrink: 0;
  }
  .file-btn {
    min-width: 200px;
    text-align: left;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .file-meta {
    color: var(--text-muted);
    font-size: 11px;
    white-space: nowrap;
  }
  .fps-warning {
    color: #e8a020;
    font-size: 12px;
    padding: 6px 10px;
    background: rgba(232, 160, 32, 0.08);
    border: 1px solid rgba(232, 160, 32, 0.25);
    border-radius: 4px;
  }
  .options-row {
    display: flex;
    gap: var(--gap-lg);
    align-items: center;
    flex-wrap: wrap;
  }
  .options-row label {
    display: flex;
    align-items: center;
    gap: var(--gap-xs);
    color: var(--text-secondary);
    font-size: 12px;
  }
  .options-row select {
    font-family: var(--font-ui);
    font-size: 12px;
    padding: 3px 6px;
    background: var(--bg-raised);
    border: 1px solid var(--border-default);
    border-radius: var(--radius-sm);
    color: var(--text-primary);
  }
  .checkbox-label {
    cursor: pointer;
  }
  .analyze-btn {
    align-self: flex-end;
    padding: 8px 24px;
    font-size: 14px;
  }
  .cache-row {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: var(--gap-md);
  }
  .cache-info {
    color: var(--text-secondary);
    font-size: 11px;
  }

  /* Progress */
  .progress-panel {
    max-width: 560px;
    margin: auto;
    padding: var(--gap-xl);
    display: flex;
    flex-direction: column;
    gap: var(--gap-md);
  }
  .progress-panel h2 {
    font-size: 16px;
    font-weight: 600;
  }
  .progress-bar-container {
    width: 100%;
    height: 6px;
    background: var(--bg-overlay);
    border-radius: 3px;
    overflow: hidden;
  }
  .progress-bar {
    height: 100%;
    background: var(--vmaf-color);
    transition: width 0.2s ease;
    border-radius: 3px;
  }
  .progress-label {
    color: var(--text-secondary);
    font-size: 12px;
  }
  .diagnostics {
    font-size: 11px;
    color: var(--text-muted);
    background: var(--bg-raised);
    padding: var(--gap-sm);
    border-radius: var(--radius-sm);
    overflow: auto;
    max-height: 200px;
    white-space: pre-wrap;
  }

  /* Results */
  .results-layout {
    flex: 1;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .chart-area {
    flex: 0 0 45%;
    min-height: 200px;
    border-bottom: 1px solid var(--border-subtle);
  }
  .table-area {
    flex: 1;
    overflow: hidden;
  }

  /* Error */
  .error-panel {
    max-width: 560px;
    margin: auto;
    padding: var(--gap-xl);
    display: flex;
    flex-direction: column;
    gap: var(--gap-md);
  }
  .error-message {
    font-size: 12px;
    color: var(--threshold-color);
    background: var(--bg-raised);
    padding: var(--gap-sm);
    border-radius: var(--radius-sm);
    overflow: auto;
    max-height: 300px;
    white-space: pre-wrap;
  }

  /* Footer */
  .footer {
    display: flex;
    align-items: center;
    padding: 4px var(--gap-lg);
    background: var(--bg-surface);
    border-top: 1px solid var(--border-subtle);
    min-height: 28px;
    flex-shrink: 0;
  }
  .footer-shortcuts {
    font-size: 11px;
    color: var(--text-muted);
    display: flex;
    gap: var(--gap-sm);
    align-items: center;
    flex-wrap: wrap;
  }
</style>
