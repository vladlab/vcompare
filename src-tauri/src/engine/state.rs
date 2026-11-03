//! Engine state — holds the current analysis results in memory.
//!
//! Mirrors Python's `_state` dict but with proper typing and thread safety.
//! All mutable access goes through the Mutex-guarded inner state.

use serde_json::Value;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::engine::events::{self, DetectParams, DetectResult, Event};
use crate::engine::timecode::TimecodeFormat;
use crate::engine::types::TimelineInfo;

/// Context needed by the mpv controller — extracted from analysis results.
#[derive(Debug, Clone)]
pub struct MpvContext {
    pub ref_path: String,
    pub dist_path: String,
    pub tc_format: TimecodeFormat,
    pub ref_file_start_tc: i64,
    pub timeline: Option<TimelineInfo>,
    /// If dist needs scaling to match ref. 0 = no scaling needed.
    pub scale_w: u32,
    pub scale_h: u32,
    /// true = scale ref, false = scale dist
    pub scale_ref: bool,
}

/// Mutable inner state, protected by Mutex.
#[derive(Debug)]
struct Inner {
    /// Frame data as JSON objects — works for both diff and vmaf modes.
    frames: Vec<Value>,
    /// Which metrics are available
    metrics_available: Vec<String>,
    /// "diff" or "vmaf"
    analysis_mode: Option<String>,
    /// Metadata about the current analysis
    meta: AnalysisMeta,
    /// Path where the current analysis was saved
    save_path: Option<String>,
    /// Context for mpv (set after analysis completes)
    mpv_context: Option<MpvContext>,
    /// Events from the last `detect_events` run on these frames.
    events: Vec<Event>,
}

/// Metadata about the current analysis.
#[derive(Debug, Clone, Default)]
struct AnalysisMeta {
    ref_path: String,
    dist_path: String,
    ref_start_frame: i64,
    ref_file_start_tc: i64,
    sync_mode: String,
    bit_depth: Option<u32>,
    tc_format: Option<TimecodeFormat>,
    timeline: Option<TimelineInfo>,
    /// If dist was scaled to match ref, these hold target dimensions. 0 = no scaling.
    scale_w: u32,
    scale_h: u32,
    /// true = ref was scaled, false = dist was scaled
    scale_ref: bool,
}

/// Every per-frame metric a levels analysis can carry, in display order.
const LEVELS_METRICS: &[&str] = &[
    "y_min", "y_max", "u_min", "u_max", "v_min", "v_max",
    "y_avg", "u_avg", "v_avg",
    "y_dif", "u_dif", "v_dif",
    // signalstats-era caches only
    "sat_avg",
];

/// Thread-safe engine state, managed as Tauri state.
pub struct EngineState {
    inner: Mutex<Inner>,
}

impl EngineState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                frames: Vec::new(),
                metrics_available: Vec::new(),
                analysis_mode: None,
                meta: AnalysisMeta::default(),
                save_path: None,
                mpv_context: None,
                events: Vec::new(),
            }),
        }
    }

    // ─── Mutation (called by analysis) ──────────────────────────────────

    /// Store diff analysis results.
    pub fn apply_diff_result(&self, result: &Value) {
        let mut state = self.inner.lock().unwrap();
        state.events.clear();
        state.frames = result
            .get("frames")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut metrics = vec![
            "diff_y".into(),
            "diff_u".into(),
            "diff_v".into(),
        ];

        // Check if SSIM data is present in frames
        let has_ssim = state.frames.iter().any(|f| {
            f.get("ssim").and_then(|v| v.as_f64()).is_some()
        });
        if has_ssim {
            metrics.push("ssim".into());
            metrics.push("ssim_y".into());
            metrics.push("ssim_u".into());
            metrics.push("ssim_v".into());
        }

        state.metrics_available = metrics;
        state.analysis_mode = Some("diff".into());
        state.meta = AnalysisMeta {
            ref_path: str_field(result, "ref_path"),
            dist_path: str_field(result, "dist_path"),
            ref_start_frame: result
                .get("ref_start_frame")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            ref_file_start_tc: result
                .get("ref_file_start_tc")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            sync_mode: str_field_or(result, "sync_mode", "start"),
            bit_depth: result.get("bit_depth").and_then(|v| v.as_u64()).map(|v| v as u32),
            tc_format: parse_tc_format(result.get("tc_format")),
            timeline: result
                .get("timeline")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            scale_w: result.get("scale_w").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            scale_h: result.get("scale_h").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            scale_ref: result.get("scale_ref").and_then(|v| v.as_bool()).unwrap_or(false),
        };
        state.mpv_context = build_mpv_context(&state.meta);
    }

    /// Store VMAF analysis results.
    pub fn apply_vmaf_result(
        &self,
        frames: Vec<Value>,
        ref_path: &str,
        dist_path: &str,
        ref_start_frame: i64,
        ref_file_start_tc: i64,
        sync_mode: &str,
        tc_format: Option<TimecodeFormat>,
        timeline: Option<TimelineInfo>,
        scale_w: u32,
        scale_h: u32,
        scale_ref: bool,
    ) {
        let mut state = self.inner.lock().unwrap();
        state.events.clear();

        let has_vmaf = frames
            .iter()
            .any(|f| f.get("vmaf").and_then(|v| v.as_f64()).is_some());
        let has_psnr = frames
            .iter()
            .any(|f| f.get("psnr").and_then(|v| v.as_f64()).is_some());

        let mut metrics = Vec::new();
        if has_vmaf {
            metrics.push("vmaf".into());
        }
        if has_psnr {
            metrics.push("psnr".into());
        }

        state.frames = frames;
        state.metrics_available = metrics;
        state.analysis_mode = Some("vmaf".into());
        state.meta = AnalysisMeta {
            ref_path: ref_path.into(),
            dist_path: dist_path.into(),
            ref_start_frame,
            ref_file_start_tc,
            sync_mode: sync_mode.into(),
            bit_depth: None,
            tc_format: tc_format.clone(),
            timeline,
            scale_w,
            scale_h,
            scale_ref,
        };
        state.mpv_context = build_mpv_context(&state.meta);
    }

    /// Store levels analysis results (single-file YUV min/max).
    pub fn apply_levels_result(&self, result: &Value) {
        let mut state = self.inner.lock().unwrap();
        state.events.clear();
        state.frames = result
            .get("frames")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        // Levels caches come in several generations (signalstats min/max
        // only; signalstats + temporal/sat; native engine with u/v averages
        // and later histogram stats), so advertise exactly what is present.
        // `tdif_*` are detector inputs, not chart metrics.
        state.metrics_available = LEVELS_METRICS
            .iter()
            .filter(|m| {
                state
                    .frames
                    .iter()
                    .any(|f| f.get(**m).and_then(|v| v.as_f64()).is_some())
            })
            .map(|m| m.to_string())
            .collect();
        state.analysis_mode = Some("levels".into());
        state.meta = AnalysisMeta {
            ref_path: str_field(result, "ref_path"),
            dist_path: String::new(),
            ref_start_frame: result
                .get("ref_start_frame")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            ref_file_start_tc: result
                .get("ref_file_start_tc")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            sync_mode: "start".into(),
            bit_depth: result.get("bit_depth").and_then(|v| v.as_u64()).map(|v| v as u32),
            tc_format: parse_tc_format(result.get("tc_format")),
            timeline: None,
            scale_w: 0,
            scale_h: 0,
            scale_ref: false,
        };
        // Single-file mpv view (no comparison) — dist_path stays empty.
        state.mpv_context = build_mpv_context(&state.meta);
    }

    pub fn set_save_path(&self, path: Option<String>) {
        self.inner.lock().unwrap().save_path = path;
    }

    /// Clear all analysis state (e.g. for "New Analysis").
    #[allow(dead_code)]
    pub fn clear(&self) {
        let mut state = self.inner.lock().unwrap();
        state.frames.clear();
        state.metrics_available.clear();
        state.analysis_mode = None;
        state.meta = AnalysisMeta::default();
        state.save_path = None;
        state.mpv_context = None;
        state.events.clear();
    }

    // ─── Queries (called by commands) ───────────────────────────────────

    /// Build the analysis summary for the frontend.
    pub fn build_summary(&self) -> Result<Value, String> {
        let state = self.inner.lock().unwrap();
        let frames = &state.frames;
        let metrics = &state.metrics_available;
        let meta = &state.meta;
        let mode = state
            .analysis_mode
            .as_deref()
            .ok_or("No analysis loaded")?;

        eprintln!(
            "[vcompare] build_summary: mode={mode}, frames={}, metrics={:?}",
            frames.len(), metrics
        );

        let mut summary = serde_json::json!({
            "mode": mode,
            "frame_count": frames.len(),
            "metrics_available": metrics,
            "tc_format": meta.tc_format.as_ref().map(|tc| serde_json::json!({
                "fps_num": tc.fps_num,
                "fps_den": tc.fps_den,
                "tc_type": tc.tc_type,
            })),
            "ref_start_frame": meta.ref_start_frame,
            "sync_mode": meta.sync_mode,
            "bit_depth": meta.bit_depth,
        });

        // Per-metric stats
        for m in metrics {
            let vals: Vec<f64> = frames
                .iter()
                .filter_map(|f| f.get(m.as_str()).and_then(|v| v.as_f64()))
                .collect();

            if vals.is_empty() {
                continue;
            }

            let sum: f64 = vals.iter().sum();
            let mean = sum / vals.len() as f64;
            let mn = vals.iter().cloned().fold(f64::INFINITY, f64::min);
            let mx = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

            let stddev = if vals.len() >= 2 {
                let variance =
                    vals.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (vals.len() - 1) as f64;
                variance.sqrt()
            } else {
                0.0
            };

            let stats_key = format!("{m}_stats");
            summary[&stats_key] = serde_json::json!({
                "mean": round4(mean),
                "min": round4(mn),
                "max": round4(mx),
                "stddev": round4(stddev),
            });

            // Diff distribution for diff_y
            if mode == "diff" && m == "diff_y" {
                summary["diff_distribution"] = serde_json::json!({
                    "zero": vals.iter().filter(|&&v| v == 0.0).count(),
                    "below_1": vals.iter().filter(|&&v| v > 0.0 && v <= 1.0).count(),
                    "below_5": vals.iter().filter(|&&v| v > 1.0 && v <= 5.0).count(),
                    "below_10": vals.iter().filter(|&&v| v > 5.0 && v <= 10.0).count(),
                    "above_10": vals.iter().filter(|&&v| v > 10.0).count(),
                });
            }
        }

        eprintln!("[vcompare] summary: {}", serde_json::to_string(&summary).unwrap_or_default());

        // Add timeline info if available
        if let Some(ref tl) = meta.timeline {
            summary["timeline"] = serde_json::to_value(tl).unwrap_or_default();
        }

        Ok(summary)
    }

    /// Get frames with filtering, sorting, and pagination.
    pub fn get_frames(&self, params: &Value) -> Result<Value, String> {
        let state = self.inner.lock().unwrap();
        if state.frames.is_empty() {
            return Err("No analysis loaded".into());
        }

        eprintln!(
            "[vcompare] get_frames: {} frames in state, first={}",
            state.frames.len(),
            state.frames.first().map(|f| f.to_string()).unwrap_or_default()
        );

        let mut frames = state.frames.clone();

        // Filter
        let metric = params.get("filter_metric").and_then(|v| v.as_str());
        let below = params.get("filter_below").and_then(|v| v.as_f64());
        let above = params.get("filter_above").and_then(|v| v.as_f64());

        if let Some(m) = metric {
            if let Some(b) = below {
                frames.retain(|f| {
                    f.get(m)
                        .and_then(|v| v.as_f64())
                        .map(|v| v < b)
                        .unwrap_or(false)
                });
            }
            if let Some(a) = above {
                frames.retain(|f| {
                    f.get(m)
                        .and_then(|v| v.as_f64())
                        .map(|v| v > a)
                        .unwrap_or(false)
                });
            }
        }

        let total_matching = frames.len();

        // Sort
        let sort_key = params.get("sort").and_then(|v| v.as_str());
        let ascending = params
            .get("ascending")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        if let Some(key) = sort_key {
            if key == "frame" {
                frames.sort_by(|a, b| {
                    let av = a.get("frame").and_then(|v| v.as_i64()).unwrap_or(0);
                    let bv = b.get("frame").and_then(|v| v.as_i64()).unwrap_or(0);
                    if ascending { av.cmp(&bv) } else { bv.cmp(&av) }
                });
            } else if state.metrics_available.iter().any(|m| m == key) {
                frames.sort_by(|a, b| {
                    let av = a.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let bv = b.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    if ascending {
                        av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal)
                    } else {
                        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
                    }
                });
            }
        }

        // Paginate
        let offset = params
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let paged = if limit > 0 {
            frames[offset..frames.len().min(offset + limit)].to_vec()
        } else if offset > 0 {
            frames[offset..].to_vec()
        } else {
            frames
        };

        Ok(serde_json::json!({
            "total_matching": total_matching,
            "offset": offset,
            "count": paged.len(),
            "frames": paged,
        }))
    }

    /// Get a single metric as flat arrays (optimized for chart rendering).
    pub fn get_metric_series(&self, metric: &str) -> Result<Value, String> {
        let state = self.inner.lock().unwrap();
        if state.frames.is_empty() {
            return Err("No analysis loaded".into());
        }

        let mut frame_nums = Vec::new();
        let mut values = Vec::new();

        for f in &state.frames {
            if let Some(val) = f.get(metric).and_then(|v| v.as_f64()) {
                if let Some(frame) = f.get("frame").and_then(|v| v.as_i64()) {
                    frame_nums.push(frame);
                    values.push(round4(val));
                }
            }
        }

        eprintln!(
            "[vcompare] get_metric_series({metric}): {}/{} frames have data",
            frame_nums.len(), state.frames.len()
        );

        Ok(serde_json::json!({
            "frames": frame_nums,
            "values": values,
        }))
    }

    /// Export analysis to CSV.
    pub fn export_csv(&self, path: Option<&str>) -> Result<Value, String> {
        let state = self.inner.lock().unwrap();
        if state.frames.is_empty() {
            return Err("No analysis loaded".into());
        }

        let output_path = match path {
            Some(p) => p.to_string(),
            None => {
                let ref_stem = PathBuf::from(&state.meta.ref_path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "ref".into());
                let dist_stem = PathBuf::from(&state.meta.dist_path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "dist".into());
                let ref_dir = PathBuf::from(&state.meta.ref_path)
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .to_path_buf();
                ref_dir
                    .join(format!("{ref_stem}_vs_{dist_stem}.csv"))
                    .to_string_lossy()
                    .to_string()
            }
        };

        let metrics = &state.metrics_available;
        let mut csv = String::new();

        // Header
        csv.push_str("frame,timecode");
        for m in metrics {
            csv.push(',');
            csv.push_str(m);
        }
        csv.push('\n');

        // Rows
        for f in &state.frames {
            let frame = f.get("frame").and_then(|v| v.as_i64()).unwrap_or(0);
            let tc = f.get("tc").and_then(|v| v.as_str()).unwrap_or("");
            csv.push_str(&frame.to_string());
            csv.push(',');
            csv.push_str(tc);
            for m in metrics {
                csv.push(',');
                if let Some(val) = f.get(m.as_str()).and_then(|v| v.as_f64()) {
                    csv.push_str(&format!("{val:.4}"));
                }
            }
            csv.push('\n');
        }

        std::fs::write(&output_path, &csv)
            .map_err(|e| format!("Failed to write CSV: {e}"))?;

        Ok(serde_json::json!({
            "path": output_path,
            "rows": state.frames.len(),
        }))
    }

    // ─── Events ─────────────────────────────────────────────────────────

    /// Run flash detection over the loaded frames and remember the result.
    pub fn detect_events(&self, params: &DetectParams) -> Result<DetectResult, String> {
        let mut state = self.inner.lock().unwrap();
        let mode = state
            .analysis_mode
            .clone()
            .ok_or("No analysis loaded")?;
        let bit_depth = state.meta.bit_depth.unwrap_or(8);
        let result = events::detect_flashes(&mode, &state.frames, bit_depth, params)?;
        eprintln!(
            "[vcompare] detect_events({mode}): {} events, threshold={}",
            result.events.len(),
            result.threshold
        );
        state.events = result.events.clone();
        Ok(result)
    }

    /// Export the current events to CSV.
    pub fn export_events_csv(&self, path: Option<&str>) -> Result<Value, String> {
        let state = self.inner.lock().unwrap();
        if state.frames.is_empty() {
            return Err("No analysis loaded".into());
        }

        let output_path = match path {
            Some(p) => p.to_string(),
            None => {
                let stem = PathBuf::from(&state.meta.ref_path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "ref".into());
                let dir = PathBuf::from(&state.meta.ref_path)
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .to_path_buf();
                dir.join(format!("{stem}_flash_frames.csv"))
                    .to_string_lossy()
                    .to_string()
            }
        };

        let mut csv = String::from("frame,timecode,length,kind,subkind,at_cut,score\n");
        for e in &state.events {
            csv.push_str(&format!(
                "{},{},{},{},{},{},{:.3}\n",
                e.frame, e.tc, e.length, e.kind, e.subkind, e.at_cut, e.score
            ));
        }

        std::fs::write(&output_path, &csv)
            .map_err(|e| format!("Failed to write CSV: {e}"))?;

        Ok(serde_json::json!({
            "path": output_path,
            "rows": state.events.len(),
        }))
    }

    /// Get the mpv context (paths + TC info) from the current analysis.
    pub fn mpv_context(&self) -> Result<MpvContext, String> {
        self.inner
            .lock()
            .unwrap()
            .mpv_context
            .clone()
            .ok_or_else(|| "No analysis loaded".into())
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn str_field_or(v: &Value, key: &str, default: &str) -> String {
    v.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .to_string()
}

fn parse_tc_format(v: Option<&Value>) -> Option<TimecodeFormat> {
    let v = v?.as_object()?;
    Some(TimecodeFormat {
        fps_num: v.get("fps_num")?.as_u64()? as u32,
        fps_den: v.get("fps_den")?.as_u64()? as u32,
        tc_type: serde_json::from_value(v.get("tc_type")?.clone()).ok()?,
    })
}

/// `dist_path` may be empty (levels mode): the controller then opens the
/// reference file alone.
fn build_mpv_context(meta: &AnalysisMeta) -> Option<MpvContext> {
    let tc = meta.tc_format.clone()?;
    if meta.ref_path.is_empty() {
        return None;
    }
    Some(MpvContext {
        ref_path: meta.ref_path.clone(),
        dist_path: meta.dist_path.clone(),
        tc_format: tc,
        ref_file_start_tc: meta.ref_file_start_tc,
        timeline: meta.timeline.clone(),
        scale_w: meta.scale_w,
        scale_h: meta.scale_h,
        scale_ref: meta.scale_ref,
    })
}
