<!--
  vcompare — main application shell.

  Analysis modes:
    - diff (default): fast pixel difference (psnr/ssim filters)
    - vmaf: perceptual quality (libvmaf)
    - levels: single-file signal levels (signalstats)

  Flash-frame detection runs over diff and levels results (EventPanel).
-->

<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { open, save } from "@tauri-apps/plugin-dialog";
  import { getCurrentWebview } from "@tauri-apps/api/webview";
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
    type DetectParams,
    type DetectResult,
    ping,
    detectCapabilities,
    probeFile,
    checkCache,
    checkLevelsCache,
    loadSavedAnalysis,
    analyze,
    cancelAnalysis,
    getMetricSeries,
    getFrames,
    exportCsv,
    detectEvents,
    exportEventsCsv,
    exportEventsEdl,
    setEventReview,
    previewFrames,
    type EventReview,
    type PreviewRow,
    getRecents,
    addRecent,
    type RecentEntry,
    mpvLaunch,
    mpvSeek,
    mpvStep,
    mpvTogglePause,
    mpvKill,
    diffviewLaunch,
    diffviewSeek,
    diffviewStep,
    diffviewPause,
    diffviewKill,
    resolveConnect,
    resolveSeek,
    resolveDisconnect,
    onResolveStatus,
    type ResolveStatus,
    onProgress,
    onDiagnostics,
    onMpvPosition,
    onDiffviewPosition,
  } from "./lib/rpc";
  import MetricChart from "./components/MetricChart.svelte";
  import FrameTable from "./components/FrameTable.svelte";
  import TimelineBar from "./components/TimelineBar.svelte";
  import EventPanel from "./components/EventPanel.svelte";

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

  // Recent analyses
  let showRecents: boolean = $state(false);
  let recentEntries: RecentEntry[] = $state([]);

  // Analysis options
  let analysisMode: "diff" | "vmaf" | "levels" = $state("diff");
  let syncMode: string = $state("start");
  let hwaccelMode: string = $state("auto");
  let wantPsnr: boolean = $state(false);
  let wantSsim: boolean = $state(false);
  let subsample: number = $state(1);
  // Levels: parallel ffmpeg decodes (0 = auto). Lower it for slow disks or
  // network shares, where parallel reads just compete.
  let parallelDecodes: number = $state(0);
  const PARALLEL_OPTIONS = [1, 2, 3, 4, 6, 8, 12, 16];
  // Mirrors rawstats::auto_chunks (the engine still falls back to one
  // process for sources that can't be split exactly).
  const autoParallel = Math.min(8, Math.max(1, Math.floor((navigator.hardwareConcurrency || 4) / 10)));

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

  // DaVinci Resolve playhead sync
  let resolveOn: boolean = $state(false);
  let resolveStatus: ResolveStatus = $state({ state: "disconnected" });
  let resolveBusy: boolean = $state(false);

  async function toggleResolveSync() {
    if (resolveBusy) return;
    if (resolveOn) {
      resolveOn = false;
      resolveDisconnect().catch(() => {});
      resolveStatus = { state: "disconnected" };
      return;
    }
    resolveBusy = true;
    resolveStatus = { state: "connecting" };
    try {
      resolveStatus = await resolveConnect();
      resolveOn = resolveStatus.state === "connected";
      if (resolveOn && cursorFrame >= 0) resolveSeek(cursorFrame).catch(() => {});
    } catch (e) {
      resolveOn = false;
      resolveStatus = { state: "failed", message: String(e) };
      showStatus(`Resolve: ${e}`, 12000);
    } finally {
      resolveBusy = false;
    }
  }

  function resolveStatusText(): string {
    const s = resolveStatus;
    switch (s.state) {
      case "connecting": return "connecting…";
      case "connected": return s.target;
      case "seek_failed": return `⚠ ${s.message}`;
      case "failed": return `✕ ${s.message}`;
      default: return "";
    }
  }

  // Difference view state
  let diffviewRunning: boolean = $state(false);
  let diffviewFrame: number = $state(-1);

  // Flash-frame detection
  let detectParams: DetectParams = $state({ threshold: null, max_run: 3, exclude_at_cut: false });
  let detectResult: DetectResult | null = $state(null);
  let detectError: string | null = $state(null);
  let detectTimer: ReturnType<typeof setTimeout> | null = null;
  // The event picked last (panel click, strip click, n/N). Stays put while
  // the cursor shuttles around it, so you don't lose your place.
  let selectedEventFrame: number | null = $state(null);
  // Events already visited this session.
  let seenEvents: Set<number> = $state(new Set());

  function selectEvent(frame: number) {
    selectedEventFrame = frame;
    seenEvents = new Set(seenEvents).add(frame);
    handleCursorChange(frame);
  }

  // ── Event preview (frame before, the run, frame after) ──
  const PREVIEW_HEIGHT = 108; // px, shown at half size on HiDPI
  let eventPreview: PreviewRow[] | null = $state(null);
  const previewCache = new Map<string, PreviewRow[]>();
  let previewRequest = 0;

  $effect(() => {
    const frame = selectedEventFrame;
    const ev = detectResult?.events.find((e) => e.frame === frame);
    if (frame == null || !ev) {
      eventPreview = null;
      return;
    }
    const key = `${frame}:${ev.length}`;
    const cached = previewCache.get(key);
    if (cached) {
      eventPreview = cached;
      return;
    }
    eventPreview = null;
    const req = ++previewRequest;
    previewFrames(frame - 1, ev.length + 2, PREVIEW_HEIGHT)
      .then((r) => {
        previewCache.set(key, r.rows);
        if (req === previewRequest) eventPreview = r.rows;
      })
      .catch((e) => {
        if (req === previewRequest) showStatus(`Preview: ${e}`);
      });
  });

  async function handleReview(frame: number, review: EventReview | null) {
    try {
      const r = await setEventReview(frame, review);
      if (detectResult) {
        detectResult = {
          ...detectResult,
          events: detectResult.events.map((e) => (e.frame === frame ? { ...e, review } : e)),
        };
      }
      if (!r.saved) showStatus("Verdict kept for this session only (analysis has no save path)");
    } catch (e) {
      showStatus(`Review: ${e}`);
    }
  }

  /** y / x on the picked event. */
  function reviewSelected(review: EventReview) {
    const frame = selectedEventFrame;
    const ev = detectResult?.events.find((e) => e.frame === frame);
    if (frame == null || !ev) return;
    handleReview(frame, ev.review === review ? null : review);
  }

  // Debounced seek — sends to both mpv instances if running
  let seekTimer: ReturnType<typeof setTimeout> | null = null;
  let seekGeneration = 0;

  function debouncedMpvSeek(frame: number) {
    if (!mpvRunning && !diffviewRunning) return;
    seekGeneration++;
    const gen = seekGeneration;
    if (seekTimer) clearTimeout(seekTimer);
    seekTimer = setTimeout(() => {
      if (gen === seekGeneration) {
        if (mpvRunning) mpvSeek(frame).catch(() => {});
        if (diffviewRunning) diffviewSeek(frame).catch(() => {});
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
      await onDiffviewPosition((e) => {
        diffviewFrame = e.frame;
      })
    );
    unlisteners.push(
      await listen("rpc:diffview-exited", () => {
        diffviewRunning = false;
        diffviewFrame = -1;
      })
    );

    unlisteners.push(
      await getCurrentWebview().onDragDropEvent((event) => {
        const p = event.payload;
        // Only the file picker accepts drops.
        if (state !== "idle") {
          dragActive = false;
          return;
        }
        if (p.type === "enter") {
          dragActive = true;
          dragCount = p.paths.length;
          dragZone = zoneAt(p.position);
        } else if (p.type === "over") {
          dragZone = zoneAt(p.position);
        } else if (p.type === "drop") {
          const zone = zoneAt(p.position);
          dragActive = false;
          dragZone = null;
          handleDrop(p.paths, zone).catch((e) => showStatus(`Drop failed: ${e}`));
        } else {
          dragActive = false;
          dragZone = null;
        }
      })
    );

    unlisteners.push(
      await onResolveStatus((s) => {
        resolveStatus = s;
        if (s.state === "failed") {
          resolveOn = false;
          showStatus(`Resolve: ${s.message}`, 12000);
        }
      })
    );

    unlisteners.push(
      await listen("menu-event", (event) => {
        const id = event.payload as string;
        if (id === "new-analysis") {
          // Kill mpv instances if running
          mpvKill().catch(() => {});
          diffviewKill().catch(() => {});
          // Reset to idle
          state = "idle";
          refPath = "";
          distPath = "";
          refProbe = null;
          distProbe = null;
          cacheInfo = null;
          summary = null;
          metricSeries = {};
          detectResult = null;
          detectError = null;
          selectedEventFrame = null;
          seenEvents = new Set();
          previewCache.clear();
          progressCurrent = 0;
          progressTotal = 0;
          diagnosticLines = [];
          cursorFrame = -1;
          mpvRunning = false;
          mpvAutoSeek = false;
          mpvFrame = -1;
          diffviewRunning = false;
          diffviewFrame = -1;
          showRecents = false;
        } else if (id === "open-recent") {
          getRecents().then((entries) => {
            recentEntries = entries;
            showRecents = true;
          }).catch(() => {});
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

  // ── Flash-frame detection ──

  let detectAvailable = $derived(
    summary?.mode === "levels" || summary?.mode === "diff"
  );

  async function runDetection() {
    if (!detectAvailable) {
      detectResult = null;
      detectError = null;
      return;
    }
    try {
      detectResult = await detectEvents({ ...detectParams });
      detectError = null;
    } catch (e) {
      detectResult = null;
      detectError = String(e);
    }
  }

  // Re-run whenever the parameters change (the engine scan is cheap, but
  // slider drags fire fast, so coalesce).
  $effect(() => {
    void detectParams;
    if (state !== "ready" || !detectAvailable) return;
    if (detectTimer) clearTimeout(detectTimer);
    detectTimer = setTimeout(runDetection, 60);
  });

  /** Frame range of the analysed data, for the event overview strip. */
  let analysedRange = $derived.by((): [number, number] | null => {
    if (summary?.timeline) {
      return [summary.timeline.overlap_start, summary.timeline.overlap_end];
    }
    const first = Object.values(metricSeries)[0];
    if (first && first.frames.length > 0) {
      return [first.frames[0], first.frames[first.frames.length - 1] + 1];
    }
    return null;
  });

  /** Jump to the next (dir=1) or previous (dir=-1) event from the cursor. */
  function jumpToEvent(dir: 1 | -1) {
    const events = detectResult?.events ?? [];
    if (events.length === 0) return;
    // Step from the picked event if there is one, so shuttling around it
    // with mpv doesn't change where n/N go next.
    const from = selectedEventFrame ?? cursorFrame;
    let target;
    if (dir === 1) {
      target = events.find((e) => e.frame > from) ?? events[0];
    } else {
      target = [...events].reverse().find((e) => e.frame < from) ?? events[events.length - 1];
    }
    selectEvent(target.frame);
  }

  async function handleExportEdl() {
    const stem = refPath.split("/").pop()?.replace(/\.[^.]+$/, "") ?? "vcompare";
    const result = await save({
      title: "Export flash frames as Resolve marker EDL",
      defaultPath: `${stem}_flash_markers.edl`,
      filters: [{ name: "EDL", extensions: ["edl"] }],
    });
    if (result) {
      const r = await exportEventsEdl(result as string);
      showStatus(`${r.rows} markers written — in Resolve: Timeline › Import › Timeline Markers from EDL`, 10000);
    }
  }

  async function handleExportEvents() {
    const stem = refPath.split("/").pop()?.replace(/\.[^.]+$/, "") ?? "vcompare";
    const result = await save({
      title: "Export flash frames CSV",
      defaultPath: `${stem}_flash_frames.csv`,
      filters: [{ name: "CSV", extensions: ["csv"] }],
    });
    if (result) {
      await exportEventsCsv(result as string);
    }
  }

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

  function dirOf(path: string): string | undefined {
    const i = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
    return i > 0 ? path.substring(0, i) : undefined;
  }

  /** Set (and probe) the reference / single file — from the dialog or a drop. */
  async function setReference(path: string) {
    refPath = path;
    lastDir = dirOf(path) ?? lastDir;
    try {
      refProbe = await probeFile(refPath);
    } catch (e) {
      refProbe = null;
      showStatus(`Can't read ${fileName(path)}: ${e}`);
    }
    if (analysisMode === "levels") {
      await updateCacheInfo();
    } else if (distPath) {
      await probeSelected();
    }
  }

  async function setDistorted(path: string) {
    distPath = path;
    lastDir = dirOf(path) ?? lastDir;
    try {
      distProbe = await probeFile(distPath);
    } catch (e) {
      distProbe = null;
      showStatus(`Can't read ${fileName(path)}: ${e}`);
    }
    if (refPath) await probeSelected();
  }

  function fileName(path: string): string {
    return path.split(/[\\/]/).pop() ?? path;
  }

  async function pickReference() {
    const result = await open({
      title: analysisMode === "levels" ? "Select video" : "Select reference video",
      filters: VIDEO_FILTERS,
      defaultPath: lastDir,
    });
    if (result) await setReference(result as string);
  }

  async function pickDistorted() {
    const result = await open({
      title: "Select distorted video",
      filters: VIDEO_FILTERS,
      defaultPath: lastDir,
    });
    if (result) await setDistorted(result as string);
  }

  // ── Drag and drop (OS file drops arrive via the webview, with paths) ──

  type DropZone = "ref" | "dist";
  let dragActive: boolean = $state(false);
  let dragZone: DropZone | null = $state(null);
  let dragCount: number = $state(0);

  /** Which drop zone (if any) is under a webview position (physical px). */
  function zoneAt(pos: { x: number; y: number }): DropZone | null {
    const dpr = window.devicePixelRatio || 1;
    const el = document.elementFromPoint(pos.x / dpr, pos.y / dpr);
    const zone = el?.closest("[data-drop-zone]")?.getAttribute("data-drop-zone");
    return zone === "ref" || zone === "dist" ? zone : null;
  }

  /**
   * Levels: the first file is the one to analyse.
   * Compare: two files fill reference then distorted (in the order given);
   * one file goes to the zone it was dropped on, or else to whichever slot
   * is empty (reference first).
   */
  async function handleDrop(paths: string[], zone: DropZone | null) {
    if (paths.length === 0) return;
    if (analysisMode === "levels") {
      await setReference(paths[0]);
      if (paths.length > 1) showStatus(`Levels analyses one file — using ${fileName(paths[0])}`);
      return;
    }
    if (paths.length >= 2) {
      // Set both before probing so the pair is probed once.
      distPath = paths[1];
      await setReference(paths[0]);
      if (!distProbe) await setDistorted(paths[1]);
      if (paths.length > 2) showStatus("Compare uses two files — extra files ignored");
      return;
    }
    const target: DropZone = zone ?? (!refPath ? "ref" : !distPath ? "dist" : "ref");
    if (target === "ref") await setReference(paths[0]);
    else await setDistorted(paths[0]);
  }

  async function probeSelected() {
    if (!refPath || (!distPath && analysisMode !== "levels")) return;
    state = "probing";
    try {
      refProbe = await probeFile(refPath);
      if (distPath && analysisMode !== "levels") {
        distProbe = await probeFile(distPath);
      }
      await updateCacheInfo();
      state = "idle";
    } catch (e) {
      state = "error";
      errorMessage = `Probe failed: ${e}`;
    }
  }

  async function updateCacheInfo() {
    try {
      if (analysisMode === "levels" && refPath) {
        cacheInfo = await checkLevelsCache(refPath);
      } else if (refPath && distPath) {
        cacheInfo = await checkCache(refPath, distPath);
      } else {
        cacheInfo = null;
      }
    } catch {
      cacheInfo = null;
    }
  }

  // ── Analysis ──

  // Each job gets a token; a cancelled (or superseded) job's late result or
  // error is ignored instead of replacing whatever the user is doing now.
  let jobToken = 0;
  let cancelling: boolean = $state(false);

  function beginJob(): number {
    cancelling = false;
    return ++jobToken;
  }

  async function handleCancel() {
    if (state !== "analyzing" || cancelling) return;
    cancelling = true;
    jobToken++; // drop whatever the cancelled job eventually returns
    try {
      await cancelAnalysis();
    } catch (e) {
      console.error("cancel:", e);
    }
    progressCurrent = 0;
    progressTotal = 0;
    resetProgressRate();
    diagnosticLines = [];
    cancelling = false;
    state = "idle";
    await updateCacheInfo();
    showStatus("Analysis cancelled — nothing was saved");
  }

  async function finishAnalysis(result: AnalysisSummary, token: number) {
    if (token !== jobToken) return;
    summary = result;
    metricSeries = {};
    selectedEventFrame = null;
    seenEvents = new Set();
    previewCache.clear();

    // Fetch series for each available metric
    for (const m of result.metrics_available) {
      try {
        metricSeries[m] = await getMetricSeries(m);
      } catch (e) {
        console.error(`Failed to fetch series for ${m}:`, e);
      }
      if (token !== jobToken) return;
    }

    // Set active metric to first available
    if (result.metrics_available.length > 0) {
      activeMetric = result.metrics_available[0];
    }

    // Auto-set threshold
    if (result.mode === "diff" && result.diff_y_stats) {
      const s = result.diff_y_stats;
      threshold = Math.round((s.mean + (s.stddev ?? 0)) * 100) / 100;
    } else if (result.mode === "vmaf" && result.vmaf_stats) {
      const s = result.vmaf_stats;
      threshold = Math.round((s.mean - (s.stddev ?? 0)) * 100) / 100;
    } else if (result.mode === "levels") {
      // No threshold for levels by default
      threshold = null;
    }

    state = "ready";
    if (result.perf) showStatus(result.perf, 20000);

    // Flash-frame detection (levels / diff only)
    await runDetection();

    // Record in recents
    if (cacheInfo?.path && refPath) {
      addRecent({
        ref_path: refPath,
        dist_path: distPath || "",
        cache_path: cacheInfo.path,
        mode: result.mode,
        frame_count: result.frame_count,
        timestamp: Math.floor(Date.now() / 1000),
      }).catch(() => {});
    }
  }

  async function startAnalysis() {
    const token = beginJob();
    state = "analyzing";
    progressCurrent = 0;
    progressTotal = 0;
    resetProgressRate();
    diagnosticLines = [];

    try {
      const result = await analyze({
        reference: refPath,
        distorted: analysisMode !== "levels" ? distPath : undefined,
        mode: analysisMode,
        hwaccel: hwaccelMode,
        sync: syncMode,
        psnr: wantPsnr,
        ssim: wantSsim,
        subsample,
        chunks: analysisMode === "levels" && parallelDecodes > 0 ? parallelDecodes : undefined,
        log_path: cacheInfo?.path,
        force: true,
      });
      await finishAnalysis(result, token);
    } catch (e) {
      if (token !== jobToken) return; // cancelled: handleCancel already reset the UI
      state = "error";
      errorMessage = `Analysis failed: ${e}`;
    }
  }

  async function loadCached() {
    if (!cacheInfo?.path) return;
    const token = beginJob();
    state = "analyzing";
    try {
      const result = await loadSavedAnalysis(cacheInfo.path);
      await finishAnalysis(result, token);
    } catch (e) {
      if (token !== jobToken) return;
      state = "error";
      errorMessage = `Failed to load cached analysis: ${e}`;
    }
  }

  async function loadRecentEntry(entry: RecentEntry) {
    showRecents = false;
    refPath = entry.ref_path;
    distPath = entry.dist_path;
    if (entry.mode === "levels" || entry.mode === "diff" || entry.mode === "vmaf") {
      analysisMode = entry.mode;
    }
    // Set cacheInfo so finishAnalysis can record it back to recents
    cacheInfo = { exists: true, path: entry.cache_path, frame_count: entry.frame_count, mode: entry.mode };
    const token = beginJob();
    state = "analyzing";
    try {
      const result = await loadSavedAnalysis(entry.cache_path);
      await finishAnalysis(result, token);
    } catch (e) {
      if (token !== jobToken) return;
      state = "error";
      errorMessage = `Failed to load recent analysis: ${e}`;
    }
  }

  // ── Event handlers ──

  // Throughput: frames/s over a sliding window of recent progress events
  // (events arrive every ~250 ms), plus an ETA from that rate.
  const RATE_WINDOW_MS = 3000;
  let rateSamples: { t: number; frame: number }[] = [];
  let progressFps: number | null = $state(null);
  let progressEta: number | null = $state(null);

  function resetProgressRate() {
    rateSamples = [];
    progressFps = null;
    progressEta = null;
  }

  function handleProgress(e: ProgressEvent) {
    progressCurrent = e.current;
    progressTotal = e.total;

    const now = performance.now();
    rateSamples.push({ t: now, frame: e.current });
    while (rateSamples.length > 2 && now - rateSamples[0].t > RATE_WINDOW_MS) {
      rateSamples.shift();
    }
    const first = rateSamples[0];
    const dt = (now - first.t) / 1000;
    if (rateSamples.length >= 2 && dt > 0.5) {
      progressFps = (e.current - first.frame) / dt;
      progressEta = progressFps > 0 && e.total > e.current
        ? (e.total - e.current) / progressFps
        : null;
    }
  }

  function fmtDuration(sec: number): string {
    const s = Math.round(sec);
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const ss = s % 60;
    return h > 0 ? `${h}h${String(m).padStart(2, "0")}m` : m > 0 ? `${m}m${String(ss).padStart(2, "0")}s` : `${ss}s`;
  }

  function handleDiagnostics(e: DiagnosticsEvent) {
    diagnosticLines = e.lines;
  }

  function handleCursorChange(frame: number) {
    cursorFrame = frame;
    if (mpvRunning || diffviewRunning) {
      debouncedMpvSeek(frame);
    }
    // No debounce needed: the engine sends only the newest frame once the
    // previous seek has been answered.
    if (resolveOn) resolveSeek(frame).catch(() => {});
  }

  // ── Keyboard shortcuts ──

  /** Levels metrics where a bigger value is the problem (for the `t` key). */
  const HIGHER_IS_WORSE = new Set(["y_dif", "u_dif", "v_dif", "y_oor_lo", "y_oor_hi", "c_oor"]);

  /** True when the keystroke belongs to a text field, not to our shortcuts. */
  function isTextEntry(target: EventTarget | null): boolean {
    const el = target as HTMLElement | null;
    if (!el) return false;
    if (el.isContentEditable || el.tagName === "TEXTAREA" || el.tagName === "SELECT") return true;
    if (el.tagName === "INPUT") {
      const type = (el as HTMLInputElement).type;
      // Sliders, checkboxes and buttons keep focus after a click but don't
      // take typed text — shortcuts should still work with those focused.
      return !["range", "checkbox", "radio", "button", "submit"].includes(type);
    }
    return false;
  }

  // Transient status line (e.g. why mpv didn't launch)
  let statusMessage: string | null = $state(null);
  let statusTimer: ReturnType<typeof setTimeout> | null = null;
  function showStatus(msg: string, ms = 6000) {
    statusMessage = msg;
    if (statusTimer) clearTimeout(statusTimer);
    statusTimer = setTimeout(() => (statusMessage = null), ms);
  }

  function handleKeydown(e: KeyboardEvent) {
    if (state === "analyzing" && e.key === "Escape") {
      e.preventDefault();
      handleCancel();
      return;
    }
    if (state !== "ready") return;
    if (isTextEntry(e.target)) return;

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
        // diff_* metrics: higher = worse (mean + σ)
        // ssim/vmaf/psnr: lower = worse (mean - σ)
        if (summary) {
          const statsKey = `${activeMetric}_stats` as keyof AnalysisSummary;
          const stats = summary[statsKey] as any;
          if (stats) {
            const higherIsWorse =
              activeMetric.startsWith("diff") || HIGHER_IS_WORSE.has(activeMetric);
            if (higherIsWorse) {
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
      case "n":
      case "ArrowDown":
        e.preventDefault();
        jumpToEvent(1);
        break;
      case "N":
      case "ArrowUp":
        e.preventDefault();
        jumpToEvent(-1);
        break;
      case "p":
        if (!mpvRunning) {
          mpvLaunch()
            .then(() => (mpvRunning = true))
            .catch((err) => showStatus(`mpv: ${err}`));
        }
        break;
      case "P":
        mpvKill().then(() => (mpvRunning = false));
        break;
      case "d":
        if (!diffviewRunning) {
          diffviewLaunch()
            .then(() => (diffviewRunning = true))
            .catch((err) => showStatus(`difference view: ${err}`));
        }
        break;
      case "D":
        diffviewKill().then(() => (diffviewRunning = false));
        break;
      case "s":
        if (cursorFrame >= 0) {
          if (mpvRunning) mpvSeek(cursorFrame);
          if (diffviewRunning) diffviewSeek(cursorFrame);
        }
        break;
      case "a":
        mpvAutoSeek = !mpvAutoSeek;
        break;
      case "r":
        toggleResolveSync();
        break;
      case "y":
        reviewSelected("accept");
        break;
      case "x":
        reviewSelected("reject");
        break;
      case ".":
        if (mpvRunning) mpvStep("forward");
        if (diffviewRunning) diffviewStep("forward");
        break;
      case ",":
        if (mpvRunning) mpvStep("backward");
        if (diffviewRunning) diffviewStep("backward");
        break;
      case " ":
        if (mpvRunning || diffviewRunning) {
          e.preventDefault();
          if (mpvRunning) mpvTogglePause();
          if (diffviewRunning) diffviewPause();
        }
        break;
      case "e":
        handleExport();
        break;
      case "f": {
        const idx = SMOOTH_OPTIONS.indexOf(smoothWindow);
        smoothWindow = SMOOTH_OPTIONS[(idx + 1) % SMOOTH_OPTIONS.length];
        break;
      }
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

  // ── Temporal smoothing (running median) ──

  let smoothWindow: number = $state(0); // 0 = off, otherwise odd window size
  const SMOOTH_OPTIONS = [0, 5, 11, 21, 51]; // off, light, medium, heavy, very heavy

  function runningMedian(values: number[], window: number): number[] {
    if (window <= 1) return values;
    const half = (window - 1) / 2;
    const result = new Array(values.length);
    for (let i = 0; i < values.length; i++) {
      const lo = Math.max(0, i - half);
      const hi = Math.min(values.length - 1, i + half);
      const slice = values.slice(lo, hi + 1);
      slice.sort((a, b) => a - b);
      result[i] = slice[Math.floor(slice.length / 2)];
    }
    return result;
  }

  let displaySeries = $derived.by(() => {
    if (!activeSeries) return null;
    if (smoothWindow <= 1) return activeSeries;
    return {
      frames: activeSeries.frames,
      values: runningMedian(activeSeries.values, smoothWindow),
    };
  });

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
    const tc = p.timecode ? ` TC ${p.timecode}` : "";
    return `${p.width}×${p.height} ${p.codec} ${p.pix_fmt} ${fps}fps${tc}`;
  }

  function fmtMetricLabel(m: string): string {
    const labels: Record<string, string> = {
      diff_y: "Diff Y",
      diff_u: "Diff U",
      diff_v: "Diff V",
      ssim: "SSIM",
      ssim_y: "SSIM Y",
      ssim_u: "SSIM U",
      ssim_v: "SSIM V",
      vmaf: "VMAF",
      psnr: "PSNR",
      y_min: "Y Min",
      y_max: "Y Max",
      u_min: "U Min",
      u_max: "U Max",
      v_min: "V Min",
      v_max: "V Max",
      y_avg: "Y Avg",
      u_avg: "U Avg",
      v_avg: "V Avg",
      y_low: "Y Low",
      y_high: "Y High",
      y_oor_lo: "Y < legal",
      y_oor_hi: "Y > legal",
      c_oor: "C illegal",
      y_dif: "Y Dif",
      u_dif: "U Dif",
      v_dif: "V Dif",
      sat_avg: "Sat Avg",
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
          μ={activeStats().mean.toFixed(activeMetric.startsWith("ssim") ? 4 : 0)}
          {#if summary.mode === "levels"}
            range={activeStats().min.toFixed(0)}–{activeStats().max.toFixed(0)}
          {:else}
            {activeMetric.startsWith("diff") ? "max" : "min"}={activeMetric.startsWith("diff") ? activeStats().max.toFixed(2) : activeStats().min.toFixed(activeMetric.startsWith("ssim") ? 4 : 2)}
          {/if}
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
    {#if state === "ready"}
      <span class="header-spacer"></span>
      <button
        class="resolve-btn"
        class:on={resolveOn}
        class:warn={resolveStatus.state === "seek_failed"}
        onclick={toggleResolveSync}
        disabled={resolveBusy}
        title="Follow the cursor in DaVinci Resolve's open timeline (r). Needs Resolve running with external scripting enabled: Preferences › System › General › External scripting using: Local."
      >
        Resolve sync {resolveOn ? "on" : "off"}
      </button>
      {#if resolveStatusText()}
        <span class="header-meta mono resolve-status">{resolveStatusText()}</span>
      {/if}
    {/if}
  </header>

  <!-- ── Main content ── -->
  <main class="main">

    {#if state === "idle" || state === "probing"}
      <!-- File picker panel -->
      <div class="picker-panel">
        <div class="picker-section">
          <h2>{analysisMode === "levels" ? "Analyze Video" : "Compare Videos"}</h2>
          <p class="text-secondary">
            {analysisMode === "levels"
              ? "Select or drop a video file to analyze signal levels."
              : "Select or drop a reference and distorted file — or drop both at once."}
          </p>
        </div>

        {#if statusMessage}
          <div class="picker-status">{statusMessage}</div>
        {/if}

        <div class="options-row" style="margin-bottom: 12px;">
          <label>
            Mode
            <select bind:value={analysisMode} onchange={() => updateCacheInfo()}>
              <option value="diff">Pixel Diff</option>
              <option value="vmaf">VMAF</option>
              <option value="levels">Levels</option>
            </select>
          </label>
        </div>

        <div
          class="file-row drop-zone"
          class:drop-armed={dragActive}
          class:drop-hover={dragActive && (dragZone === "ref" || analysisMode === "levels" || dragCount >= 2)}
          data-drop-zone="ref"
        >
          <label>{analysisMode === "levels" ? "File" : "Reference"}</label>
          <button onclick={pickReference} class="file-btn" title={refPath || ""}>
            {refPath ? refPath.split("/").pop() : "Choose file…"}
          </button>
          {#if refProbe}
            <span class="file-meta mono">{fmtProbe(refProbe)}</span>
          {/if}
        </div>

        {#if analysisMode !== "levels"}
          <div
            class="file-row drop-zone"
            class:drop-armed={dragActive}
            class:drop-hover={dragActive && (dragZone === "dist" || dragCount >= 2)}
            data-drop-zone="dist"
          >
            <label>Distorted</label>
            <button onclick={pickDistorted} class="file-btn" title={distPath || ""}>
              {distPath ? distPath.split("/").pop() : "Choose file…"}
            </button>
            {#if distProbe}
              <span class="file-meta mono">{fmtProbe(distProbe)}</span>
            {/if}
          </div>
        {/if}

        {#if analysisMode === "levels" && refProbe}
          <div class="options-row">
            <label title="How many ffmpeg processes decode the file side by side. One ffmpeg tops out around 60 fps for UHD 10-bit; more lift that on fast local storage. Lower it for slow disks or network shares. Only intra-frame, constant-rate files (ProRes, DNxHR, …) can be split.">
              Parallel decodes
              <select bind:value={parallelDecodes}>
                <option value={0}>Auto ({autoParallel})</option>
                {#each PARALLEL_OPTIONS as n}
                  <option value={n}>{n}{n === 1 ? " (no split)" : ""}</option>
                {/each}
              </select>
            </label>
          </div>
          <button class="primary analyze-btn" onclick={startAnalysis}>
            {cacheInfo?.exists ? "Re-analyze" : "Analyze"}
          </button>

          {#if cacheInfo?.exists}
            <div class="cache-row">
              <span class="cache-info mono">
                Cached (levels): {cacheInfo.frame_count?.toLocaleString()} frames
                ({cacheInfo.path.split("/").pop()})
              </span>
              <button class="primary" onclick={loadCached}>
                Load cached
              </button>
            </div>
          {/if}
        {:else if analysisMode !== "levels" && refProbe && distProbe}
          {#if Math.abs(refProbe.fps - distProbe.fps) > 0.01}
            <div class="fps-warning">
              ⚠ Framerate mismatch: ref {refProbe.fps_num}/{refProbe.fps_den} ({refProbe.fps.toFixed(3)}fps) vs dist {distProbe.fps_num}/{distProbe.fps_den} ({distProbe.fps.toFixed(3)}fps) — results may be unreliable
            </div>
          {/if}
          <div class="options-row">
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
            {:else}
              <label class="checkbox-label">
                <input type="checkbox" bind:checked={wantSsim} />
                SSIM
              </label>
            {/if}
          </div>

          <button class="primary analyze-btn" onclick={startAnalysis}>
            {cacheInfo?.exists ? "Re-analyze" : "Analyze"}
          </button>

          {#if cacheInfo?.exists}
            <div class="cache-row">
              <span class="cache-info mono">
                Cached ({cacheInfo.mode ?? "vmaf"}{cacheInfo.with_ssim ? "+ssim" : ""}, {cacheInfo.sync_mode ?? "start"}): {cacheInfo.frame_count?.toLocaleString()} frames
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
        <h2>Analyzing… ({analysisMode === "diff" ? "pixel diff" : analysisMode === "levels" ? "levels" : "VMAF"})</h2>
        <div class="progress-bar-container">
          <div class="progress-bar" style="width: {progressPct}%"></div>
        </div>
        <div class="progress-label mono">
          {#if progressTotal > 0}
            {progressCurrent.toLocaleString()} / {progressTotal.toLocaleString()} frames
            ({progressPct}%)
            {#if progressFps != null}
              · {progressFps.toFixed(1)} fps
              {#if progressEta != null}· ~{fmtDuration(progressEta)} left{/if}
            {/if}
          {:else}
            Starting FFmpeg…
          {/if}
        </div>
        {#if diagnosticLines.length > 0}
          <pre class="diagnostics mono">{diagnosticLines.join("\n")}</pre>
        {/if}
        <div class="progress-actions">
          <button class="cancel-btn" onclick={handleCancel} disabled={cancelling} title="Stop the analysis (Esc). Nothing is saved.">
            {cancelling ? "Cancelling…" : "Cancel"}
          </button>
          <span class="text-muted">Esc</span>
        </div>
      </div>

    {:else if state === "ready" && activeSeries}
      <!-- Results view -->
      <div class="results-layout">
        <div class="results-main">
          {#if summary?.timeline}
            <TimelineBar
              timeline={summary.timeline}
              tcFormat={summary.tc_format}
              events={detectResult?.events ?? []}
              onSeek={handleCursorChange}
            />
          {/if}
          <div class="chart-area">
            <MetricChart
              bind:this={chartComponent}
              series={displaySeries}
              metric={activeMetric}
              {threshold}
              {cursorFrame}
              onCursorChange={handleCursorChange}
              timeline={summary?.timeline}
              events={detectResult?.events ?? []}
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
        {#if detectAvailable}
          <div class="events-area">
            <EventPanel
              result={detectResult}
              error={detectError}
              bind:params={detectParams}
              {cursorFrame}
              selectedFrame={selectedEventFrame}
              seen={seenEvents}
              frameRange={analysedRange}
              onSelect={selectEvent}
              onReview={handleReview}
              preview={eventPreview}
              onExport={handleExportEvents}
              onExportEdl={handleExportEdl}
            />
          </div>
        {/if}
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
        {#if detectAvailable}
          <kbd>↑</kbd><kbd>↓</kbd> flash
          <kbd>y</kbd><kbd>x</kbd> accept/reject
        {/if}
        <kbd>f</kbd> smooth{smoothWindow > 0 ? ` (${smoothWindow})` : ""}
        <kbd>e</kbd> export
        <kbd>+</kbd><kbd>-</kbd> zoom
        <kbd>0</kbd> reset
        <kbd>p</kbd> mpv
        <kbd>d</kbd> diff
        <kbd>s</kbd> seek
        <kbd>a</kbd> sync {mpvAutoSeek ? "↔ bidi" : "→ fwd"}
        <kbd>r</kbd> resolve
        <kbd>.</kbd><kbd>,</kbd> step
        <kbd>space</kbd> pause
      </span>
      {#if statusMessage}
        <span class="footer-status">{statusMessage}</span>
      {/if}
    </footer>
  {/if}
</div>

<!-- ── Recents overlay ── -->
{#if showRecents}
  <div class="recents-overlay" onclick={() => (showRecents = false)}>
    <div class="recents-panel" onclick={(e) => e.stopPropagation()}>
      <div class="recents-header">
        <h3>Recent Analyses</h3>
        <button class="recents-close" onclick={() => (showRecents = false)}>✕</button>
      </div>
      {#if recentEntries.length === 0}
        <p class="recents-empty">No recent analyses</p>
      {:else}
        <div class="recents-list">
          {#each recentEntries as entry}
            <button
              class="recents-entry"
              onclick={() => loadRecentEntry(entry)}
            >
              <span class="recents-files mono">
                {entry.ref_path.split("/").pop()}{entry.dist_path ? ` vs ${entry.dist_path.split("/").pop()}` : ""}
              </span>
              <span class="recents-meta">
                {entry.mode} · {entry.frame_count.toLocaleString()} frames · {new Date(entry.timestamp * 1000).toLocaleDateString()}
              </span>
            </button>
          {/each}
        </div>
      {/if}
    </div>
  </div>
{/if}

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
  .header-spacer { flex: 1; }
  .resolve-btn {
    font-size: 11px;
    padding: 2px 8px;
  }
  .resolve-btn.on {
    background: var(--accent-dim);
    border-color: var(--accent);
    color: #fff;
  }
  .resolve-btn.warn {
    border-color: var(--warning);
  }
  .resolve-status {
    max-width: 360px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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
    max-width: 300px;
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
  /* Drag and drop */
  .drop-zone {
    border: 1px dashed transparent;
    border-radius: var(--radius-md);
    padding: 6px 8px;
    margin: -7px -9px;
    transition: border-color 80ms, background 80ms;
  }
  .drop-zone.drop-armed {
    border-color: var(--border-strong);
  }
  .drop-zone.drop-hover {
    border-color: var(--accent);
    background: color-mix(in srgb, var(--accent) 12%, transparent);
  }
  .picker-status {
    font-size: 12px;
    color: var(--warning);
  }

  /* Cancel */
  .progress-actions {
    display: flex;
    align-items: center;
    gap: var(--gap-sm);
    margin-top: var(--gap-md);
  }
  .cancel-btn {
    min-width: 96px;
  }

  .results-layout {
    flex: 1;
    display: flex;
    flex-direction: row;
    overflow: hidden;
  }
  .results-main {
    flex: 1;
    display: flex;
    flex-direction: column;
    overflow: hidden;
    min-width: 0;
  }
  .events-area {
    flex: 0 0 360px;
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
  .footer-status {
    margin-left: auto;
    font-size: 11px;
    color: var(--warning);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .footer-shortcuts {
    font-size: 11px;
    color: var(--text-muted);
    display: flex;
    gap: var(--gap-sm);
    align-items: center;
    flex-wrap: wrap;
  }
  .recents-overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 100;
  }
  .recents-panel {
    background: var(--bg-surface, #1e1e1e);
    border: 1px solid var(--border, rgba(255,255,255,0.1));
    border-radius: 8px;
    width: min(600px, 90vw);
    max-height: 70vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .recents-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border, rgba(255,255,255,0.1));
  }
  .recents-header h3 {
    margin: 0;
    font-size: 14px;
  }
  .recents-close {
    background: none;
    border: none;
    color: var(--text-muted);
    font-size: 16px;
    cursor: pointer;
    padding: 4px 8px;
  }
  .recents-close:hover {
    color: var(--text);
  }
  .recents-empty {
    padding: 24px;
    text-align: center;
    color: var(--text-muted);
  }
  .recents-list {
    overflow-y: auto;
    padding: 4px;
  }
  .recents-entry {
    display: flex;
    flex-direction: column;
    gap: 2px;
    width: 100%;
    padding: 10px 12px;
    text-align: left;
    background: none;
    border: none;
    border-radius: 4px;
    color: var(--text);
    cursor: pointer;
  }
  .recents-entry:hover {
    background: rgba(255, 255, 255, 0.06);
  }
  .recents-files {
    font-size: 12px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .recents-meta {
    font-size: 11px;
    color: var(--text-muted);
  }
</style>
