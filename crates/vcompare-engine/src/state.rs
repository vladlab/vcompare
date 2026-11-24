//! Engine state — holds the current analysis results in memory.
//!
//! Mirrors Python's `_state` dict but with proper typing and thread safety.
//! All mutable access goes through the Mutex-guarded inner state.

use serde_json::Value;
use std::path::PathBuf;
use std::sync::Mutex;

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::detect::{self as events, DetectRequest, DetectResult, Detector, Event, Reference};
use crate::signatures::{self, Signatures};
use crate::timecode::TimecodeFormat;
use crate::types::TimelineInfo;

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
    /// Events from the last `detect_events` run of each detector.
    events: BTreeMap<Detector, Vec<Event>>,
    /// User verdicts ("accept" / "reject") keyed by `"<detector>:<first
    /// frame>"`. Persisted in a small sidecar next to the analysis JSON.
    reviews: BTreeMap<String, String>,
    /// Picture signatures of a levels analysis (image match). Set straight
    /// after a run, or read lazily from the `.sig` sidecar.
    signatures: Option<Arc<Signatures>>,
    /// Timeline mode: the run's report (Resolve target, SDI signal, counts,
    /// timing, diagnostics), passed through to the summary as `timeline_run`.
    run_report: Option<Value>,
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
    "y_low", "y_high",
    "y_oor_lo", "y_oor_hi", "c_oor",
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
                events: BTreeMap::new(),
                reviews: Default::default(),
                signatures: None,
                run_report: None,
            }),
        }
    }

    // ─── Mutation (called by analysis) ──────────────────────────────────

    /// Store diff analysis results.
    pub fn apply_diff_result(&self, result: &Value) {
        let mut state = self.inner.lock().unwrap();
        state.events.clear();
        state.signatures = None;
        state.run_report = None;
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
        state.signatures = None;
        state.run_report = None;

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
        state.signatures = None;
        state.run_report = None;
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

    /// Store a timeline-mode result (file vs. Resolve's SDI output, sampled).
    pub fn apply_timeline_result(&self, result: &Value) {
        let mut state = self.inner.lock().unwrap();
        state.events.clear();
        state.signatures = None;
        state.frames = result
            .get("frames")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        state.metrics_available = ["diff_y", "diff_u", "diff_v"]
            .iter()
            .filter(|m| state.frames.iter().any(|f| f.get(**m).and_then(|v| v.as_f64()).is_some()))
            .map(|m| m.to_string())
            .collect();
        state.analysis_mode = Some("timeline".into());
        state.meta = AnalysisMeta {
            ref_path: str_field(result, "ref_path"),
            dist_path: String::new(),
            ref_start_frame: result.get("ref_start_frame").and_then(|v| v.as_i64()).unwrap_or(0),
            ref_file_start_tc: result.get("ref_file_start_tc").and_then(|v| v.as_i64()).unwrap_or(0),
            sync_mode: "tc".into(),
            bit_depth: result.get("bit_depth").and_then(|v| v.as_u64()).map(|v| v as u32),
            tc_format: parse_tc_format(result.get("tc_format")),
            timeline: None,
            scale_w: 0,
            scale_h: 0,
            scale_ref: false,
        };
        let mut report = serde_json::Map::new();
        for k in [
            "dry_run", "resolve", "signal", "file_signal", "counts", "plan", "timing",
            "diagnostics", "offset", "compare_format", "file_frames",
        ] {
            if let Some(v) = result.get(k) {
                report.insert(k.into(), v.clone());
            }
        }
        state.run_report = Some(Value::Object(report));
        // Single-file mpv view of the file; Resolve shows the other side.
        state.mpv_context = build_mpv_context(&state.meta);
    }

    /// Set where this analysis lives on disk, and load the review verdicts
    /// saved next to it (if any).
    pub fn set_save_path(&self, path: Option<String>) {
        let mut state = self.inner.lock().unwrap();
        state.reviews = path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(reviews_path(p)).ok())
            .and_then(|s| serde_json::from_str::<ReviewsFile>(&s).ok())
            .map(|f| migrate_review_keys(f.reviews))
            .unwrap_or_default();
        if !state.reviews.is_empty() {
            eprintln!("[vcompare] loaded {} event reviews", state.reviews.len());
        }
        let st = &mut *state;
        for (d, evs) in st.events.iter_mut() {
            for e in evs.iter_mut() {
                e.review = st.reviews.get(&review_key(*d, e.frame)).cloned();
            }
        }
        st.save_path = path;
    }

    /// Record (or clear, with `None`) the verdict on the event starting at
    /// `frame`, and save the verdicts next to the analysis.
    pub fn set_event_review(&self, detector: Detector, frame: i64, review: Option<&str>) -> Result<Value, String> {
        let mut state = self.inner.lock().unwrap();
        let key = review_key(detector, frame);
        match review {
            Some(r) if r == "accept" || r == "reject" => {
                state.reviews.insert(key, r.to_string());
            }
            Some(other) => return Err(format!("unknown review {other:?}")),
            None => {
                state.reviews.remove(&key);
            }
        }
        if let Some(evs) = state.events.get_mut(&detector) {
            for e in evs.iter_mut() {
                if e.frame == frame {
                    e.review = review.map(String::from);
                }
            }
        }
        if let Some(ref sp) = state.save_path {
            let path = reviews_path(sp);
            if state.reviews.is_empty() {
                let _ = std::fs::remove_file(&path);
            } else {
                let file = ReviewsFile {
                    version: 2,
                    ref_path: state.meta.ref_path.clone(),
                    reviews: state.reviews.clone(),
                };
                let content = serde_json::to_string_pretty(&file)
                    .map_err(|e| format!("Failed to serialize reviews: {e}"))?;
                crate::cache::write_atomic(&path, content.as_bytes())?;
            }
        }
        Ok(serde_json::json!({
            "detector": detector,
            "frame": frame,
            "review": review,
            "saved": state.save_path.is_some(),
        }))
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
        state.signatures = None;
        state.run_report = None;
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

        if let Some(ref r) = state.run_report {
            summary["timeline_run"] = r.clone();
            summary["ref_path"] = serde_json::json!(meta.ref_path);
        }

        if mode == "levels" {
            let on_disk = state
                .save_path
                .as_deref()
                .map(|p| std::path::Path::new(&signatures::sidecar_path(p)).is_file())
                .unwrap_or(false);
            summary["has_signatures"] = serde_json::json!(state.signatures.is_some() || on_disk);
        }

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
                    .unwrap_or_else(|| {
                        if state.analysis_mode.as_deref() == Some("timeline") { "timeline" } else { "dist" }.into()
                    });
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

        if state.analysis_mode.as_deref() == Some("timeline") {
            csv.push_str("frame,timecode,track,clip,pos,status,diff_y,diff_u,diff_v,waited_ms,error\n");
            for f in &state.frames {
                let txt = |k: &str| f.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let num = |k: &str| f.get(k).and_then(|v| v.as_f64()).map(|v| format!("{v:.4}")).unwrap_or_default();
                csv.push_str(&format!(
                    "{},{},{},{},{},{},{},{},{},{},{}\n",
                    f.get("frame").and_then(|v| v.as_i64()).unwrap_or(0),
                    txt("tc"),
                    f.get("track").and_then(|v| v.as_u64()).unwrap_or(0),
                    csv_field(&txt("clip")),
                    txt("pos"),
                    txt("status"),
                    num("diff_y"),
                    num("diff_u"),
                    num("diff_v"),
                    f.get("waited_ms").and_then(|v| v.as_u64()).map(|v| v.to_string()).unwrap_or_default(),
                    csv_field(&txt("error")),
                ));
            }
            std::fs::write(&output_path, &csv).map_err(|e| format!("Failed to write CSV: {e}"))?;
            return Ok(serde_json::json!({ "path": output_path, "rows": state.frames.len() }));
        }

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

    /// Run one detector over the loaded frames and remember its events.
    /// `refs`: the reference pictures a match request names (the caller
    /// loads them; unused by the other detectors).
    pub fn detect_events(&self, request: &DetectRequest, refs: &[Reference]) -> Result<DetectResult, String> {
        let mut state = self.inner.lock().unwrap();
        let mode = state
            .analysis_mode
            .clone()
            .ok_or("No analysis loaded")?;
        let bit_depth = state.meta.bit_depth.unwrap_or(8);
        let mut result = match request {
            DetectRequest::Flash(p) => events::detect_flashes(&mode, &state.frames, bit_depth, p)?,
            DetectRequest::Repeat(p) => {
                if mode != "levels" {
                    return Err(format!("Repeated-frame detection is not available for {mode} mode"));
                }
                events::detect_repeats(&state.frames, bit_depth, p)?
            }
            DetectRequest::Match(p) => {
                if mode != "levels" {
                    return Err(format!("Image match is not available for {mode} mode"));
                }
                let sigs = Self::signatures_locked(&mut state)?;
                events::matcher::detect_matches(&state.frames, &sigs, refs, p)?
            }
        };
        let detector = request.detector();
        eprintln!(
            "[vcompare] detect_events({mode}, {}): {} events, threshold={}",
            detector.as_str(),
            result.events.len(),
            result.threshold
        );
        for e in result.events.iter_mut() {
            e.review = state.reviews.get(&review_key(detector, e.frame)).cloned();
        }
        state.events.insert(detector, result.events.clone());
        Ok(result)
    }

    /// Keep the signatures a levels run just computed.
    pub fn set_signatures(&self, sigs: Option<Signatures>) {
        self.inner.lock().unwrap().signatures = sigs.map(Arc::new);
    }

    /// The signatures of the loaded levels analysis, reading the sidecar
    /// on first use.
    pub fn signatures(&self) -> Result<Arc<Signatures>, String> {
        let mut state = self.inner.lock().unwrap();
        Self::signatures_locked(&mut state)
    }

    fn signatures_locked(state: &mut Inner) -> Result<Arc<Signatures>, String> {
        if let Some(ref s) = state.signatures {
            return Ok(s.clone());
        }
        let path = state
            .save_path
            .as_deref()
            .map(signatures::sidecar_path)
            .ok_or("This analysis has no frame signatures — re-run it with \"keep frame signatures\" on")?;
        if !std::path::Path::new(&path).is_file() {
            return Err("This analysis has no frame signatures — re-run it with \"keep frame signatures\" on".into());
        }
        let sigs = Signatures::read(&path)?;
        let first = state.frames.first().and_then(|f| f.get("frame")).and_then(|v| v.as_i64());
        if first != Some(sigs.first_frame) || sigs.count() != state.frames.len() {
            return Err("The frame signatures are out of date — re-run the analysis".into());
        }
        let sigs = Arc::new(sigs);
        state.signatures = Some(sigs.clone());
        Ok(sigs)
    }

    /// Events of `detector`, or of every detector (sorted by frame) for `None`.
    fn events_for(state: &Inner, detector: Option<Detector>) -> Vec<Event> {
        match detector {
            Some(d) => state.events.get(&d).cloned().unwrap_or_default(),
            None => {
                let mut all: Vec<Event> = state.events.values().flatten().cloned().collect();
                all.sort_by_key(|e| e.frame);
                all
            }
        }
    }

    /// Export the current events to CSV.
    pub fn export_events_csv(&self, detector: Option<Detector>, path: Option<&str>) -> Result<Value, String> {
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
                let what = export_stem(state.analysis_mode.as_deref(), detector, "flags", "frames");
                dir.join(format!("{stem}_{what}.csv"))
                    .to_string_lossy()
                    .to_string()
            }
        };

        let events = Self::events_for(&state, detector);
        let mut csv = String::from("frame,timecode,length,kind,subkind,at_cut,score,review,label\n");
        for e in &events {
            csv.push_str(&format!(
                "{},{},{},{},{},{},{:.3},{},{}\n",
                e.frame, e.tc, e.length, e.kind, e.subkind, e.at_cut, e.score,
                e.review.as_deref().unwrap_or(""),
                csv_field(e.label.as_deref().unwrap_or(""))
            ));
        }

        std::fs::write(&output_path, &csv)
            .map_err(|e| format!("Failed to write CSV: {e}"))?;

        Ok(serde_json::json!({
            "path": output_path,
            "rows": events.len(),
        }))
    }

    /// Export the current events as a DaVinci Resolve marker EDL.
    pub fn export_events_edl(&self, detector: Option<Detector>, path: Option<&str>) -> Result<Value, String> {
        let state = self.inner.lock().unwrap();
        if state.frames.is_empty() {
            return Err("No analysis loaded".into());
        }
        let tc_fmt = state
            .meta
            .tc_format
            .clone()
            .ok_or("This analysis has no timecode format")?;

        let stem = PathBuf::from(&state.meta.ref_path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "ref".into());
        let output_path = match path {
            Some(p) => p.to_string(),
            None => {
                let dir = PathBuf::from(&state.meta.ref_path)
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .to_path_buf();
                let what = export_stem(state.analysis_mode.as_deref(), detector, "markers", "markers");
                dir.join(format!("{stem}_{what}.edl"))
                    .to_string_lossy()
                    .to_string()
            }
        };

        let events = Self::events_for(&state, detector);
        let edl = events::events_to_resolve_edl(&events, &tc_fmt, &stem);
        std::fs::write(&output_path, edl.as_bytes())
            .map_err(|e| format!("Failed to write EDL: {e}"))?;

        Ok(serde_json::json!({
            "path": output_path,
            "rows": events.len(),
        }))
    }

    /// What the preview needs: each file with the display frame of its
    /// first frame, plus the frame rate. Levels: the one file; comparisons:
    /// reference then distorted.
    pub fn preview_sources(&self) -> Result<(Vec<(String, String, i64)>, (u32, u32)), String> {
        let state = self.inner.lock().unwrap();
        let tc = state.meta.tc_format.as_ref().ok_or("No timecode format")?;
        let meta = &state.meta;
        if meta.ref_path.is_empty() {
            return Err("No analysis loaded".into());
        }
        let mut v = vec![("REF".to_string(), meta.ref_path.clone(), meta.ref_file_start_tc)];
        if !meta.dist_path.is_empty() {
            // In TC sync the distorted file keeps its own start TC; in start
            // sync it's shown aligned to the reference's.
            let dist_origin = meta
                .timeline
                .as_ref()
                .map(|t| t.dist_start)
                .unwrap_or(meta.ref_file_start_tc);
            v.push(("DIST".to_string(), meta.dist_path.clone(), dist_origin));
        } else {
            v[0].0 = "FILE".to_string();
        }
        Ok((v, (tc.fps_num, tc.fps_den)))
    }

    /// Timecode string for a display frame, in the analysis's own format.
    pub fn frame_to_tc(&self, frame: i64) -> Result<String, String> {
        let state = self.inner.lock().unwrap();
        let tc = state.meta.tc_format.as_ref().ok_or("No timecode format")?;
        Ok(tc.frame_to_tc(frame))
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

/// Quote a CSV field when it needs it.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

/// Sidecar holding the user's accept/reject verdicts for one analysis.
#[derive(serde::Serialize, serde::Deserialize)]
struct ReviewsFile {
    version: u32,
    ref_path: String,
    reviews: BTreeMap<String, String>,
}

/// Default export file name part: `timeline_flags`, `flash_frames`,
/// `repeat_frames`, `match_frames`, `all_events` (CSV) / `…_markers` (EDL).
fn export_stem(mode: Option<&str>, detector: Option<Detector>, timeline_suffix: &str, suffix: &str) -> String {
    match (mode, detector) {
        (Some("timeline"), _) => format!("timeline_{timeline_suffix}"),
        (_, None) => if suffix == "frames" { "all_events".into() } else { "all_markers".into() },
        (_, Some(d)) => format!("{}_{suffix}", d.as_str()),
    }
}

/// Version 1 sidecars keyed verdicts by bare frame number: those were flash
/// verdicts (or timeline flags, which the flash detector reports too).
fn migrate_review_keys(reviews: BTreeMap<String, String>) -> BTreeMap<String, String> {
    reviews
        .into_iter()
        .map(|(k, v)| if k.contains(':') { (k, v) } else { (format!("flash:{k}"), v) })
        .collect()
}

fn review_key(d: Detector, frame: i64) -> String {
    format!("{}:{frame}", d.as_str())
}

/// `X.levels.json` → `X.levels.reviews.json` (same for `.vcompare.json`).
fn reviews_path(analysis_path: &str) -> String {
    match analysis_path.strip_suffix(".json") {
        Some(stem) => format!("{stem}.reviews.json"),
        None => format!("{analysis_path}.reviews.json"),
    }
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

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn version_1_review_files_still_load() {
        let v1 = r#"{"version":1,"ref_path":"/a.mov","reviews":{"86410":"accept","86500":"reject"}}"#;
        let f: ReviewsFile = serde_json::from_str(v1).unwrap();
        let m = migrate_review_keys(f.reviews);
        assert_eq!(m.get("flash:86410").map(String::as_str), Some("accept"));
        assert_eq!(m.get("flash:86500").map(String::as_str), Some("reject"));
        let v2 = migrate_review_keys([("repeat:12".to_string(), "accept".to_string())].into());
        assert!(v2.contains_key("repeat:12"));
    }

    #[test]
    fn verdicts_are_per_detector() {
        let dir = std::env::temp_dir().join(format!("vcompare_reviews_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let analysis = dir.join("x.levels.json").to_string_lossy().to_string();
        let st = EngineState::new();
        st.apply_levels_result(&serde_json::json!({"frames": [], "bit_depth": 8}));
        st.set_save_path(Some(analysis.clone()));
        st.set_event_review(Detector::Flash, 100, Some("accept")).unwrap();
        st.set_event_review(Detector::Repeat, 100, Some("reject")).unwrap();
        let st2 = EngineState::new();
        st2.set_save_path(Some(analysis));
        let r = &st2.inner.lock().unwrap().reviews;
        assert_eq!(r.get("flash:100").map(String::as_str), Some("accept"));
        assert_eq!(r.get("repeat:100").map(String::as_str), Some("reject"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
