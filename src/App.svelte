<!--
  vcompare — main application shell.

  Analysis modes:
    - diff (default): fast pixel difference (psnr/ssim filters)
    - vmaf: perceptual quality (libvmaf)
    - levels: single-file signal levels (signalstats)
    - timeline: a deliverable against Resolve's SDI output, sampled at the
      first / middle / last frame of every clip (needs scopetap_grab)

  Results: the chart runs full width; below it a tab strip holds the frame
  table and one tab per detector (flash frames in diff / levels, repeats
  and image match in levels; in timeline mode the flash tab lists the
  flagged samples and a Report tab holds the run report). Each event row
  carries a filmstrip, loaded lazily (lib/thumbLoader.svelte.ts).
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
    type DetectResult,
    type DetectorKind,
    type ImageReference,
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
    type EventReview,
    listReferences,
    addReferenceFromImage,
    addReferenceFromFrame,
    renameReference,
    deleteReference,
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
    onTimelineProgress,
    type TimelineProgressEvent,
  } from "./lib/rpc";
  import MetricChart from "./components/MetricChart.svelte";
  import FrameTable from "./components/FrameTable.svelte";
  import TimelineBar from "./components/TimelineBar.svelte";
  import TabStrip from "./components/TabStrip.svelte";
  import DetectorTab from "./components/DetectorTab.svelte";
  import { tabsFor, DEFAULT_PARAMS, requestFor, type TabId, type TabItem, type DetectorParams } from "./lib/detectors";
  import { ThumbLoader, SETTLE_MS, QUICK_MS } from "./lib/thumbLoader.svelte";
  import TimelineSetup from "./components/TimelineSetup.svelte";
  import TimelineReport from "./components/TimelineReport.svelte";

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
  let analysisMode: "diff" | "vmaf" | "levels" | "timeline" = $state("diff");
  /** Single-file modes: one file, no "distorted" slot. */
  let singleFile = $derived(analysisMode === "levels" || analysisMode === "timeline");
  let syncMode: string = $state("start");
  let hwaccelMode: string = $state("auto");
  let wantPsnr: boolean = $state(false);
  let wantSsim: boolean = $state(false);
  let subsample: number = $state(1);
  // Levels: parallel ffmpeg decodes (0 = auto). Lower it for slow disks or
  // network shares, where parallel reads just compete.
  let parallelDecodes: number = $state(0);
  // Levels: keep per-frame picture signatures for image match (a .sig
  // sidecar, 864 bytes a frame). Remembered across runs.
  let keepSignatures: boolean = $state(true);
  try { keepSignatures = localStorage.getItem("vcompare.keepSignatures") !== "false"; } catch { /* ignore */ }
  $effect(() => {
    const v = keepSignatures;
    try { localStorage.setItem("vcompare.keepSignatures", String(v)); } catch { /* ignore */ }
  });
  let signatureSize = $derived.by(() => {
    if (!refProbe) return "";
    const frames = refProbe.nb_frames ?? Math.round(refProbe.duration * refProbe.fps);
    if (!frames) return "";
    const mb = (frames * 864) / 1e6;
    return mb >= 1000 ? `~${(mb / 1000).toFixed(1)} GB` : `~${Math.max(1, Math.round(mb))} MB`;
  });
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
  let timelineProgress: TimelineProgressEvent | null = $state(null);

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
      resolveDeclined = true;
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

  // ── Detectors ──
  //
  // One entry per detector: its parameters, last result / error, the event
  // picked last (it stays put while the cursor shuttles around it) and the
  // events visited this session.
  const DETECTORS: DetectorKind[] = ["flash", "repeat", "match"];
  let detectParams: DetectorParams = $state(structuredClone(DEFAULT_PARAMS));
  let detectResults: Record<DetectorKind, DetectResult | null> = $state({ flash: null, repeat: null, match: null });
  let detectErrors: Record<DetectorKind, string | null> = $state({ flash: null, repeat: null, match: null });
  let selectedEvent: Record<DetectorKind, number | null> = $state({ flash: null, repeat: null, match: null });
  let seenEvents: Record<DetectorKind, Set<number>> = $state({ flash: new Set(), repeat: new Set(), match: new Set() });
  let hideRejected: Record<DetectorKind, boolean> = $state({ flash: false, repeat: false, match: false });
  const detectTimers: Partial<Record<DetectorKind, ReturnType<typeof setTimeout>>> = {};
  /** Parameters each detector last ran with (JSON), so the re-run effect
   *  only fires on a real change. */
  const lastRunParams: Partial<Record<DetectorKind, string>> = {};

  // Image-match references (stored in the app data dir) and which are ticked
  let references: ImageReference[] = $state([]);
  const REF_SELECTION_KEY = "vcompare.matchReferences";
  try {
    const saved = JSON.parse(localStorage.getItem(REF_SELECTION_KEY) ?? "null");
    if (Array.isArray(saved)) detectParams.match.references = saved;
  } catch { /* storage unavailable */ }

  // Tabs under the chart
  let activeTab: TabId = $state("frames");
  /** The detector whose events the chart shows and ↑↓ / y x act on: the
   *  active tab's, or the last detector tab visited. */
  let markerDetector: DetectorKind = $state("flash");
  const tabMemory: Partial<Record<string, TabId>> = {};

  const thumbs = new ThumbLoader();
  thumbs.onError = (msg) => showStatus(`Preview: ${msg}`);

  function selectEvent(frame: number, kind: DetectorKind = markerDetector) {
    selectedEvent[kind] = frame;
    seenEvents[kind] = new Set(seenEvents[kind]).add(frame);
    handleCursorChange(frame);
  }

  async function handleReview(frame: number, review: EventReview | null, kind: DetectorKind = markerDetector) {
    try {
      const r = await setEventReview(kind, frame, review);
      const res = detectResults[kind];
      if (res) {
        detectResults[kind] = {
          ...res,
          events: res.events.map((e) => (e.frame === frame ? { ...e, review } : e)),
        };
      }
      if (!r.saved) showStatus("Verdict kept for this session only (analysis has no save path)");
    } catch (e) {
      showStatus(`Review: ${e}`);
    }
  }

  /** y / x on the picked event. */
  function reviewSelected(review: EventReview) {
    const kind = markerDetector;
    const frame = selectedEvent[kind];
    const ev = detectResults[kind]?.events.find((e) => e.frame === frame);
    if (frame == null || !ev) return;
    handleReview(frame, ev.review === review ? null : review, kind);
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
    unlisteners.push(await onTimelineProgress((e) => (timelineProgress = e)));
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
          resetDetectors();
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

  // ── Detection ──

  let isTimeline = $derived(summary?.mode === "timeline");
  let tabDefs = $derived(tabsFor(summary?.mode));
  let tabItems: TabItem[] = $derived(
    summary
      ? tabDefs.map((t) => ({
          id: t.id,
          label: t.label(summary!.mode),
          count: t.detector ? (detectResults[t.detector]?.events.length ?? null) : null,
          unavailable: t.unavailable?.(summary!) ?? null,
        }))
      : []
  );
  /** Detectors with a usable tab in this analysis. */
  let liveDetectors = $derived(
    summary
      ? tabDefs.filter((t) => t.detector && !t.unavailable?.(summary!)).map((t) => t.detector!)
      : []
  );
  let detectAvailable = $derived(liveDetectors.includes("flash"));
  let markerEvents = $derived(detectResults[markerDetector]?.events ?? []);
  let previewFiles = $derived(summary?.mode === "diff" || summary?.mode === "vmaf" ? 2 : 1);

  function resetDetectors() {
    for (const k of DETECTORS) {
      detectResults[k] = null;
      detectErrors[k] = null;
      selectedEvent[k] = null;
      seenEvents[k] = new Set();
      delete lastRunParams[k];
    }
    thumbs.clear();
  }

  async function runDetection(kind: DetectorKind) {
    lastRunParams[kind] = JSON.stringify(detectParams[kind]);
    if (!liveDetectors.includes(kind)) {
      detectResults[kind] = null;
      detectErrors[kind] = null;
      return;
    }
    // Image match with nothing ticked: nothing to look for.
    if (kind === "match" && (detectParams.match.references ?? []).length === 0) {
      detectResults[kind] = { events: [], threshold: detectParams.match.min_similarity ?? 0.9, source: "signatures", detector: "match" };
      detectErrors[kind] = null;
      return;
    }
    try {
      detectResults[kind] = await detectEvents(requestFor(kind, $state.snapshot(detectParams[kind])));
      detectErrors[kind] = null;
    } catch (e) {
      detectResults[kind] = null;
      detectErrors[kind] = String(e);
    }
  }

  async function runAllDetections() {
    await Promise.all(liveDetectors.map(runDetection));
  }

  // Re-run a detector whenever its parameters change (the engine scan is
  // cheap, but slider drags fire fast, so coalesce). Thumbnails hold back
  // until things settle.
  for (const kind of DETECTORS) {
    $effect(() => {
      const json = JSON.stringify(detectParams[kind]);
      if (state !== "ready" || json === lastRunParams[kind]) return;
      if (detectTimers[kind]) clearTimeout(detectTimers[kind]);
      thumbs.hold(SETTLE_MS);
      detectTimers[kind] = setTimeout(() => runDetection(kind), 60);
    });
  }

  // Remember which references are ticked, across analyses and restarts.
  $effect(() => {
    const ids = $state.snapshot(detectParams.match.references ?? []);
    try { localStorage.setItem(REF_SELECTION_KEY, JSON.stringify(ids)); } catch { /* ignore */ }
  });

  function selectTab(id: TabId) {
    activeTab = id;
    const def = tabDefs.find((t) => t.id === id);
    if (def?.detector) markerDetector = def.detector;
    if (summary) tabMemory[summary.mode] = id;
    thumbs.hold(QUICK_MS);
  }

  /** Jump to the next (dir=1) or previous (dir=-1) event from the cursor. */
  function jumpToEvent(dir: 1 | -1) {
    const kind = markerDetector;
    const events = detectResults[kind]?.events ?? [];
    if (events.length === 0) return;
    // Step from the picked event if there is one, so shuttling around it
    // with mpv doesn't change where n/N go next.
    const from = selectedEvent[kind] ?? cursorFrame;
    let target;
    if (dir === 1) {
      target = events.find((e) => e.frame > from) ?? events[0];
    } else {
      target = [...events].reverse().find((e) => e.frame < from) ?? events[events.length - 1];
    }
    selectEvent(target.frame, kind);
  }

  // ── Image-match references ──

  async function refreshReferences() {
    try {
      references = await listReferences();
      // Drop ticks for references that no longer exist.
      const ids = new Set(references.map((r) => r.id));
      const ticked = detectParams.match.references ?? [];
      if (ticked.some((id) => !ids.has(id))) {
        detectParams.match = { ...detectParams.match, references: ticked.filter((id) => ids.has(id)) };
      }
    } catch (e) {
      showStatus(`References: ${e}`);
    }
  }

  function tickReference(id: string) {
    const cur = detectParams.match.references ?? [];
    if (!cur.includes(id)) detectParams.match = { ...detectParams.match, references: [...cur, id] };
  }

  async function handleAddRefImage() {
    const path = await open({
      title: "Reference picture (e.g. a still of the offline card)",
      defaultPath: lastDir,
      filters: [
        { name: "Images", extensions: ["png", "jpg", "jpeg", "tif", "tiff", "dpx", "exr", "bmp", "webp"] },
        { name: "All Files", extensions: ["*"] },
      ],
    });
    if (!path) return;
    try {
      showStatus("Adding reference…");
      const r = await addReferenceFromImage(path as string);
      await refreshReferences();
      tickReference(r.id);
      showStatus(`Reference “${r.name}” added${r.flat ? " (flat colour: matched by colour)" : ""}`);
    } catch (e) {
      showStatus(`Reference: ${e}`);
    }
  }

  async function handleAddRefFrame() {
    if (cursorFrame < 0) {
      showStatus("Put the cursor on the frame to use first");
      return;
    }
    try {
      const r = await addReferenceFromFrame(cursorFrame);
      await refreshReferences();
      tickReference(r.id);
      showStatus(`Reference “${r.name}” added`);
    } catch (e) {
      showStatus(`Reference: ${e}`);
    }
  }

  async function handleRenameRef(id: string, name: string) {
    if (!name.trim()) return;
    try {
      await renameReference(id, name);
      await refreshReferences();
      runDetection("match");
    } catch (e) {
      showStatus(`Reference: ${e}`);
    }
  }

  async function handleDeleteRef(id: string) {
    const r = references.find((x) => x.id === id);
    if (!r || !confirm(`Delete the reference “${r.name}”? It is removed for every analysis.`)) return;
    try {
      await deleteReference(id);
      await refreshReferences();
    } catch (e) {
      showStatus(`Reference: ${e}`);
    }
  }

  // ── Chart / tabs split ──
  const SPLIT_KEY = "vcompare.chartFraction";
  let chartFraction: number = $state(0.45);
  try {
    const v = parseFloat(localStorage.getItem(SPLIT_KEY) ?? "");
    if (v > 0.1 && v < 0.9) chartFraction = v;
  } catch { /* storage unavailable */ }
  let resultsMainEl: HTMLDivElement | undefined = $state();

  function startSplitDrag(e: PointerEvent) {
    const el = resultsMainEl;
    if (!el) return;
    e.preventDefault();
    const handle = e.currentTarget as HTMLElement;
    handle.setPointerCapture(e.pointerId);
    const chartTop = (el.querySelector(".chart-area") as HTMLElement | null)?.getBoundingClientRect().top ?? el.getBoundingClientRect().top;
    const bottom = el.getBoundingClientRect().bottom;
    const MIN = 160;
    const move = (ev: PointerEvent) => {
      const avail = bottom - chartTop;
      const h = Math.min(Math.max(ev.clientY - chartTop, MIN), avail - MIN);
      chartFraction = h / el.clientHeight;
    };
    const up = () => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", up);
      try { localStorage.setItem(SPLIT_KEY, String(chartFraction)); } catch { /* ignore */ }
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", up);
  }

  const EXPORT_NAMES: Record<DetectorKind, { what: string; stem: string }> = {
    flash: { what: "flash frames", stem: "flash" },
    repeat: { what: "repeated frames", stem: "repeat" },
    match: { what: "image matches", stem: "match" },
  };

  async function handleExportEdl(kind: DetectorKind) {
    const stem = refPath.split("/").pop()?.replace(/\.[^.]+$/, "") ?? "vcompare";
    const n = EXPORT_NAMES[kind];
    const result = await save({
      title: isTimeline ? "Export flagged samples as Resolve marker EDL" : `Export ${n.what} as Resolve marker EDL`,
      defaultPath: isTimeline ? `${stem}_timeline_markers.edl` : `${stem}_${n.stem}_markers.edl`,
      filters: [{ name: "EDL", extensions: ["edl"] }],
    });
    if (result) {
      const r = await exportEventsEdl(kind, result as string);
      showStatus(`${r.rows} markers written — in Resolve: Timeline › Import › Timeline Markers from EDL`, 10000);
    }
  }

  async function handleExportEvents(kind: DetectorKind) {
    const stem = refPath.split("/").pop()?.replace(/\.[^.]+$/, "") ?? "vcompare";
    const n = EXPORT_NAMES[kind];
    const result = await save({
      title: isTimeline ? "Export flagged samples CSV" : `Export ${n.what} CSV`,
      defaultPath: isTimeline ? `${stem}_timeline_flags.csv` : `${stem}_${n.stem}_frames.csv`,
      filters: [{ name: "CSV", extensions: ["csv"] }],
    });
    if (result) {
      await exportEventsCsv(kind, result as string);
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
    } else if (analysisMode === "timeline") {
      cacheInfo = null;
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
      title: analysisMode === "levels" ? "Select video" : analysisMode === "timeline" ? "Select the deliverable" : "Select reference video",
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
    if (singleFile) {
      await setReference(paths[0]);
      if (paths.length > 1) showStatus(`This mode takes one file — using ${fileName(paths[0])}`);
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
    if (!refPath || (!distPath && !singleFile)) return;
    state = "probing";
    try {
      refProbe = await probeFile(refPath);
      if (distPath && !singleFile) {
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
      if (analysisMode === "timeline") {
        cacheInfo = null;
      } else if (analysisMode === "levels" && refPath) {
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
    timelineProgress = null;
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
    resetDetectors();
    activeTab = tabMemory[result.mode] ?? "frames";
    if (!tabsFor(result.mode).some((t) => t.id === activeTab)) activeTab = "frames";
    markerDetector = (tabsFor(result.mode).find((t) => t.id === activeTab)?.detector) ?? "flash";

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
    } else if (result.mode === "levels" || result.mode === "timeline") {
      // No threshold line by default (timeline: the event panel flags samples)
      threshold = null;
    }

    state = "ready";
    if (result.perf) showStatus(result.perf, 20000);

    // Every detector with a tab, so the tabs show their counts at once.
    if (result.mode === "levels") await refreshReferences();
    await runAllDetections();
    thumbs.hold(QUICK_MS);

    if (result.mode === "timeline") {
      // Where the run was saved (none for a dry run), for recents.
      cacheInfo = result.save_path
        ? { exists: true, path: result.save_path, frame_count: result.frame_count, mode: "timeline" }
        : null;
      // Reviewing a flagged sample means seeing it on the reference monitor:
      // follow the cursor in Resolve unless the operator turned that off.
      if (!result.timeline_run?.dry_run && !resolveOn && !resolveBusy && !resolveDeclined) {
        toggleResolveSync();
      }
    }

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

  /** Set once the operator turns Resolve sync off, so a later run doesn't
   *  switch it back on. */
  let resolveDeclined: boolean = $state(false);

  async function startTimeline(opts: { tracks: number[]; offset: number; dry_run: boolean }) {
    const token = beginJob();
    state = "analyzing";
    progressCurrent = 0;
    progressTotal = 0;
    timelineProgress = null;
    resetProgressRate();
    diagnosticLines = [];
    try {
      const result = await analyze({
        reference: refPath,
        mode: "timeline",
        tracks: opts.tracks,
        offset: opts.offset,
        dry_run: opts.dry_run,
      });
      await finishAnalysis(result, token);
    } catch (e) {
      if (token !== jobToken) return;
      state = "error";
      errorMessage = `Timeline check failed: ${e}`;
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
        distorted: !singleFile ? distPath : undefined,
        mode: analysisMode,
        hwaccel: hwaccelMode,
        sync: syncMode,
        psnr: wantPsnr,
        ssim: wantSsim,
        subsample,
        chunks: analysisMode === "levels" && parallelDecodes > 0 ? parallelDecodes : undefined,
        signatures: analysisMode === "levels" ? keepSignatures : undefined,
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
    if (entry.mode === "levels" || entry.mode === "diff" || entry.mode === "vmaf" || entry.mode === "timeline") {
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
      case "1":
      case "2":
      case "3":
      case "4":
      case "5": {
        const t = tabItems[parseInt(e.key) - 1];
        if (t) selectTab(t.id);
        break;
      }
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
        {summary.frame_count.toLocaleString()} {summary.mode === "timeline" ? "samples" : "frames"}
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
          <h2>{analysisMode === "levels" ? "Analyze Video" : analysisMode === "timeline" ? "Check a Deliverable Against Resolve" : "Compare Videos"}</h2>
          <p class="text-secondary">
            {analysisMode === "levels"
              ? "Select or drop a video file to analyze signal levels."
              : analysisMode === "timeline"
                ? "Compares the file with what Resolve puts out on SDI, at the first, middle and last frame of every clip on the tracks you tick."
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
              <option value="timeline">Timeline (vs. Resolve SDI)</option>
            </select>
          </label>
        </div>

        <div
          class="file-row drop-zone"
          class:drop-armed={dragActive}
          class:drop-hover={dragActive && (dragZone === "ref" || singleFile || dragCount >= 2)}
          data-drop-zone="ref"
        >
          <label>{analysisMode === "levels" ? "File" : analysisMode === "timeline" ? "Deliverable" : "Reference"}</label>
          <button onclick={pickReference} class="file-btn" title={refPath || ""}>
            {refPath ? refPath.split("/").pop() : "Choose file…"}
          </button>
          {#if refProbe}
            <span class="file-meta mono">{fmtProbe(refProbe)}</span>
          {/if}
        </div>

        {#if !singleFile}
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

        {#if analysisMode === "timeline"}
          <TimelineSetup filePicked={refProbe != null} grabber={caps?.grabber} onStart={startTimeline} />
        {:else if analysisMode === "levels" && refProbe}
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
            <label class="checkbox-label" title="Save a tiny picture signature of every frame next to the analysis (a .sig file), so the Image match tab can find frames that look like a reference picture — Media Offline cards, slates, missing renders.">
              <input type="checkbox" bind:checked={keepSignatures} />
              Keep frame signatures{signatureSize ? ` (${signatureSize})` : ""}
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
        {:else if !singleFile && refProbe && distProbe}
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
        <h2>
          {#if analysisMode === "timeline"}
            Checking the timeline…
          {:else}
            Analyzing… ({analysisMode === "diff" ? "pixel diff" : analysisMode === "levels" ? "levels" : "VMAF"})
          {/if}
        </h2>
        <div class="progress-bar-container">
          <div class="progress-bar" style="width: {progressPct}%"></div>
        </div>
        <div class="progress-label mono">
          {#if analysisMode === "timeline" && timelineProgress}
            sample {timelineProgress.done.toLocaleString()} / {timelineProgress.total.toLocaleString()}
            {#if timelineProgress.tc}· {timelineProgress.tc} · {timelineProgress.clip}{/if}
            {#if timelineProgress.eta_s != null && timelineProgress.done < timelineProgress.total}· ~{fmtDuration(timelineProgress.eta_s)} left{/if}
          {:else if analysisMode === "timeline"}
            Connecting to Resolve and scopetap_grab…
          {:else if progressTotal > 0}
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

    {:else if state === "ready" && isTimeline && !activeSeries && summary?.timeline_run}
      <!-- Timeline dry run (or a run where nothing could be compared) -->
      <div class="progress-panel">
        <h2>{summary.timeline_run.dry_run ? "Dry run finished" : "Nothing was compared"}</h2>
        <TimelineReport report={summary.timeline_run} full />
        <div class="progress-actions">
          <button class="primary" onclick={() => (state = "idle")}>Back</button>
        </div>
      </div>

    {:else if state === "ready" && activeSeries}
      <!-- Results view: chart full width, tabs below -->
      <div class="results-layout">
        <div class="results-main" bind:this={resultsMainEl}>
          {#if isTimeline && summary?.timeline_run}
            <TimelineReport report={summary.timeline_run} />
          {/if}
          {#if summary?.timeline}
            <TimelineBar
              timeline={summary.timeline}
              tcFormat={summary.tc_format}
              events={markerEvents}
              onSeek={handleCursorChange}
            />
          {/if}
          <div class="chart-area" style="flex-basis: {chartFraction * 100}%">
            <MetricChart
              bind:this={chartComponent}
              series={displaySeries}
              metric={activeMetric}
              {threshold}
              {cursorFrame}
              onCursorChange={handleCursorChange}
              timeline={summary?.timeline}
              events={markerEvents}
              sparse={isTimeline}
            />
          </div>
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <div class="split-handle" title="Drag to resize" onpointerdown={startSplitDrag}></div>
          <TabStrip tabs={tabItems} active={activeTab} onSelect={selectTab} />
          <div class="tab-body">
            {#if activeTab === "frames"}
              <FrameTable
                {cursorFrame}
                metrics={summary?.metrics_available ?? []}
                mode={summary?.mode ?? "diff"}
                onFrameSelect={handleCursorChange}
              />
            {:else if activeTab === "report" && summary?.timeline_run}
              <div class="report-tab">
                <TimelineReport report={summary.timeline_run} full />
              </div>
            {:else}
              {@const def = tabDefs.find((t) => t.id === activeTab)}
              {@const why = def && summary ? def.unavailable?.(summary) : null}
              {#if def?.detector && why}
                <p class="tab-unavailable">{why}</p>
              {:else if def?.detector}
                {@const kind = def.detector}
                {#key kind}
                  <DetectorTab
                    {kind}
                    result={detectResults[kind]}
                    error={detectErrors[kind]}
                    bind:params={detectParams[kind]}
                    bind:hideRejected={hideRejected[kind]}
                    loader={thumbs}
                    files={previewFiles}
                    {cursorFrame}
                    selectedFrame={selectedEvent[kind]}
                    seen={seenEvents[kind]}
                    onSelect={(f) => selectEvent(f, kind)}
                    onReview={(f, r) => handleReview(f, r, kind)}
                    onExport={() => handleExportEvents(kind)}
                    onExportEdl={() => handleExportEdl(kind)}
                    onAdjust={() => thumbs.hold(SETTLE_MS)}
                    timeline={isTimeline}
                    {references}
                    onAddImage={handleAddRefImage}
                    onAddFrame={handleAddRefFrame}
                    onRenameRef={handleRenameRef}
                    onDeleteRef={handleDeleteRef}
                  />
                {/key}
              {/if}
            {/if}
          </div>
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
        <kbd>1</kbd>–<kbd>{tabItems.length}</kbd> tabs
        {#if liveDetectors.length > 0}
          <kbd>↑</kbd><kbd>↓</kbd> {isTimeline ? "flagged" : { flash: "flash", repeat: "repeat", match: "match" }[markerDetector]}
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
                {entry.mode} · {entry.frame_count.toLocaleString()} {entry.mode === "timeline" ? "samples" : "frames"} · {new Date(entry.timestamp * 1000).toLocaleDateString()}
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
    flex-direction: column;
    overflow: hidden;
  }
  .results-main {
    flex: 1;
    display: flex;
    flex-direction: column;
    overflow: hidden;
    min-width: 0;
  }
  .chart-area {
    flex-grow: 0;
    flex-shrink: 0;
    min-height: 160px;
  }
  .split-handle {
    height: 5px;
    flex-shrink: 0;
    cursor: row-resize;
    background: var(--border-subtle);
    border-top: 2px solid var(--bg-base);
    border-bottom: 2px solid var(--bg-base);
    box-sizing: border-box;
  }
  .split-handle:hover { background: var(--text-muted); }
  .tab-body {
    flex: 1;
    min-height: 160px;
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }
  .tab-body > :global(*) { flex: 1; min-height: 0; }
  .report-tab { overflow-y: auto; padding: var(--gap-md); }
  .tab-unavailable {
    padding: var(--gap-md);
    color: var(--text-secondary);
    font-size: 12px;
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
