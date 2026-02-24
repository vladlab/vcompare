/**
 * rpc.ts — Typed wrappers around Tauri invoke() and event listeners.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// ─── Types ──────────────────────────────────────────────────────────────────

export interface FileProbe {
  path: string;
  filename: string;
  width: number;
  height: number;
  pix_fmt: string;
  codec: string;
  fps: number;
  fps_num: number;
  fps_den: number;
  nb_frames: number | null;
  duration: number;
  timecode: string | null;
}

export interface MetricStats {
  mean: number;
  min: number;
  max: number;
  stddev?: number;
}

export interface TcFormat {
  fps_num: number;
  fps_den: number;
  tc_type: "NDF" | "DF";
  fps: number;
}

export interface DiffDistribution {
  zero: number;
  below_1: number;
  below_5: number;
  below_10: number;
  above_10: number;
}

export interface TimelineInfo {
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

export interface AnalysisSummary {
  mode: "diff" | "vmaf";
  frame_count: number;
  metrics_available: string[];
  tc_format: TcFormat | null;
  ref_start_frame: number;
  sync_mode: string;
  bit_depth?: number;
  // Per-metric stats (dynamic keys)
  diff_y_stats?: MetricStats;
  diff_u_stats?: MetricStats;
  diff_v_stats?: MetricStats;
  vmaf_stats?: MetricStats;
  psnr_stats?: MetricStats;
  diff_distribution?: DiffDistribution;
  timeline?: TimelineInfo;
}

/** Generic frame data — fields depend on analysis mode */
export interface FrameData {
  frame: number;
  tc: string;
  // diff mode
  diff_y?: number | null;
  diff_u?: number | null;
  diff_v?: number | null;
  // vmaf mode
  vmaf?: number | null;
  psnr?: number | null;
}

export interface FrameResult {
  total_matching: number;
  offset: number;
  count: number;
  frames: FrameData[];
}

export interface MetricSeries {
  frames: number[];
  values: number[];
}

export interface Capabilities {
  hwaccel: string | null;
  vmaf_cuda: boolean;
  has_mpv: boolean;
}

export interface ProgressEvent {
  current: number;
  total: number;
}

export interface DiagnosticsEvent {
  lines: string[];
}

export interface MpvPositionEvent {
  frame: number;
}

// ─── Analysis parameters ────────────────────────────────────────────────────

export interface AnalyzeParams {
  reference: string;
  distorted: string;
  mode?: "diff" | "vmaf";
  model?: string;
  hwaccel?: string;
  sync?: string;
  offset?: number;
  subsample?: number;
  psnr?: boolean;
  log_path?: string;
  force?: boolean;
}

export interface GetFramesParams {
  offset?: number;
  limit?: number;
  sort?: string;
  ascending?: boolean;
  filter_metric?: string;
  filter_below?: number;
  filter_above?: number;
}

export interface CacheCheck {
  exists: boolean;
  path: string;
  frame_count: number | null;
  mode?: string;
  sync_mode?: string;
}

// ─── Commands (frontend → Rust → Python) ────────────────────────────────────

export async function ping(): Promise<{ status: string; pid: number }> {
  return invoke("ping");
}

export async function detectCapabilities(): Promise<Capabilities> {
  return invoke("detect_capabilities");
}

export async function probeFile(path: string): Promise<FileProbe> {
  return invoke("probe_file", { path });
}

export async function checkCache(reference: string, distorted: string): Promise<CacheCheck> {
  return invoke("check_cache", { reference, distorted });
}

export async function loadSavedAnalysis(path: string): Promise<AnalysisSummary> {
  return invoke("load_saved_analysis", { path });
}

export async function analyze(params: AnalyzeParams): Promise<AnalysisSummary> {
  return invoke("analyze", { params });
}

export async function getFrames(params: GetFramesParams): Promise<FrameResult> {
  return invoke("get_frames", { params });
}

export async function getMetricSeries(metric: string): Promise<MetricSeries> {
  return invoke("get_metric_series", { metric });
}

export async function mpvLaunch(): Promise<{ status: string }> {
  return invoke("mpv_launch");
}

export async function mpvSeek(frame: number): Promise<{ ok: boolean }> {
  return invoke("mpv_seek", { frame });
}

export async function mpvStep(direction: "forward" | "backward"): Promise<{ ok: boolean }> {
  return invoke("mpv_step", { direction });
}

export async function mpvTogglePause(): Promise<{ ok: boolean }> {
  return invoke("mpv_toggle_pause");
}

export async function mpvKill(): Promise<{ status: string }> {
  return invoke("mpv_kill");
}

// ─── Difference view (second mpv instance) ────────────────────────────────

export async function diffviewLaunch(): Promise<{ status: string }> {
  return invoke("diffview_launch");
}

export async function diffviewSeek(frame: number): Promise<{ ok: boolean }> {
  return invoke("diffview_seek", { frame });
}

export async function diffviewStep(direction: "forward" | "backward"): Promise<{ ok: boolean }> {
  return invoke("diffview_step", { direction });
}

export async function diffviewPause(): Promise<{ ok: boolean }> {
  return invoke("diffview_pause");
}

export async function diffviewKill(): Promise<{ status: string }> {
  return invoke("diffview_kill");
}

export async function exportCsv(path?: string): Promise<{ path: string; rows: number }> {
  return invoke("export_csv", { path: path ?? null });
}

// ─── Event listeners (Python → Rust → Frontend) ────────────────────────────

export function onProgress(callback: (e: ProgressEvent) => void): Promise<UnlistenFn> {
  return listen<ProgressEvent>("rpc:progress", (event) => callback(event.payload));
}

export function onDiagnostics(callback: (e: DiagnosticsEvent) => void): Promise<UnlistenFn> {
  return listen<DiagnosticsEvent>("rpc:diagnostics", (event) => callback(event.payload));
}

export function onMpvPosition(callback: (e: MpvPositionEvent) => void): Promise<UnlistenFn> {
  return listen<MpvPositionEvent>("rpc:mpv-position", (event) => callback(event.payload));
}

export function onDiffviewPosition(callback: (e: MpvPositionEvent) => void): Promise<UnlistenFn> {
  return listen<MpvPositionEvent>("rpc:diffview-position", (event) => callback(event.payload));
}

export { type UnlistenFn };
