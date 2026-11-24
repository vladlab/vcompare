//! vcompare's application layer, independent of any UI toolkit.
//!
//! `Service` owns everything a running app holds: the loaded analysis, the
//! ffmpeg job slot, the mpv windows, the Resolve connection, the image-match
//! references and the recents list. Its methods are synchronous and may
//! block (probing, analysing, extracting thumbnails): call them off the UI
//! thread. Progress and other unsolicited news go to the `EventSink` given
//! at construction.
//!
//! The Tauri app wraps each method in a `#[tauri::command]`; a native UI
//! calls them from worker threads.

mod events;
mod players;
mod recents;
mod slots;

pub use events::{EventSink, NullSink, PlayerExit, UiEvent};
pub use recents::RecentEntry;
pub use vcompare_engine as engine;

use engine::analysis::ProcessHandle;
use engine::cache;
use engine::detect::{DetectRequest, DetectResult, Detector};
use engine::pixfmt::choose_diff_pixfmt;
use engine::process::ProcessSlot;
use engine::references::{ReferenceStore, StoredReference};
use engine::state::EngineState;
use players::{Player, PlayerKind};
use recents::Recents;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use slots::Slots;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Error returned by `analyze` when the job was cancelled.
pub const CANCELLED: &str = "Analysis cancelled";

/// At most this many thumbnail extractions run at once.
const PREVIEW_SLOTS: usize = 3;

// ─── Parameters ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AnalyzeParams {
    pub reference: String,
    #[serde(default)]
    pub distorted: Option<String>,
    /// "diff" (default), "vmaf", "levels" or "timeline".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hwaccel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subsample: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub psnr: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssim: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
    /// Levels: parallel ffmpeg decodes. None or 0 = auto.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks: Option<u32>,
    /// Levels: keep per-frame picture signatures (image match) in a `.sig`
    /// sidecar. Default on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signatures: Option<bool>,
    /// Timeline: video tracks (1-based) to sample.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracks: Option<Vec<u32>>,
    /// Timeline: seek + grab only, no compare, nothing saved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grab_timeout_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settle_ms: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GetFramesParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ascending: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter_metric: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter_below: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter_above: Option<f64>,
}

/// One thumbnail of a preview strip.
#[derive(Debug, Clone, Serialize)]
pub struct PreviewFrame {
    pub frame: i64,
    pub tc: String,
    /// JPEG bytes; `None` before the start of the file.
    #[serde(skip)]
    pub jpeg: Option<Vec<u8>>,
}

/// Thumbnails of one file (reference / distorted).
#[derive(Debug, Clone, Serialize)]
pub struct PreviewRow {
    pub label: String,
    pub frames: Vec<PreviewFrame>,
}

// ─── Service ────────────────────────────────────────────────────────────────

pub struct Service {
    engine: EngineState,
    ffmpeg: Arc<ProcessSlot>,
    mpv: Player,
    diffview: Player,
    resolve: Mutex<Option<Arc<engine::resolve::ResolveSync>>>,
    references: ReferenceStore,
    recents: Recents,
    previews: Slots,
    sink: Arc<dyn EventSink>,
}

impl Service {
    /// `data_dir` holds app-wide files (recents, image-match references).
    pub fn new(data_dir: PathBuf, sink: Arc<dyn EventSink>) -> Self {
        Self {
            engine: EngineState::new(),
            ffmpeg: Arc::new(ProcessSlot::new()),
            mpv: Player::new(PlayerKind::Mpv),
            diffview: Player::new(PlayerKind::Diffview),
            resolve: Mutex::new(None),
            references: ReferenceStore::new(data_dir.clone()),
            recents: Recents::new(&data_dir),
            previews: Slots::new(PREVIEW_SLOTS),
            sink,
        }
    }

    /// The loaded analysis (for queries the service doesn't wrap).
    pub fn engine(&self) -> &EngineState {
        &self.engine
    }

    fn emit(&self, e: UiEvent) {
        self.sink.send(e);
    }

    /// Stop everything external: the running ffmpeg (as Cancel does, so the
    /// runner neither retries nor saves), the mpv windows, the Resolve helper.
    pub fn shutdown(&self) {
        self.ffmpeg.cancel();
        self.mpv.kill();
        if let Some(s) = self.resolve.lock().unwrap().take() {
            s.disconnect();
        }
        self.diffview.kill();
    }

    // ── External tools ──

    /// Use these ffmpeg / ffprobe / mpv paths from now on (unset entries
    /// fall back to `VCOMPARE_*` env vars, then `PATH`; see `engine::tools`).
    pub fn set_tool_paths(&self, paths: engine::tools::ToolPaths) {
        engine::tools::configure(paths);
    }

    /// The file each tool resolves to right now, `None` if not found.
    pub fn tool_locations(&self) -> Vec<(engine::tools::Tool, Option<PathBuf>)> {
        engine::tools::Tool::ALL.iter().map(|&t| (t, engine::tools::locate(t))).collect()
    }

    // ── Probing and caches ──

    pub fn detect_capabilities(&self) -> engine::types::Capabilities {
        engine::probe::detect_capabilities()
    }

    pub fn probe_file(&self, path: &str) -> Result<engine::types::ProbeResult, String> {
        engine::probe::probe_file(path)
    }

    pub fn check_cache(&self, reference: &str, distorted: &str) -> Value {
        cache::check_cache(reference, distorted, None)
    }

    pub fn check_levels_cache(&self, reference: &str) -> Value {
        let path = cache::default_levels_path(reference);
        let missing = json!({"exists": false, "path": path, "frame_count": null});
        if !Path::new(&path).is_file() {
            return missing;
        }
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        match parsed {
            Some(data) => {
                let mode = data.get("mode").and_then(|v| v.as_str()).unwrap_or("levels");
                let fc = data.get("frames").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                json!({
                    "exists": mode == "levels",
                    "path": path,
                    "frame_count": fc,
                    "mode": "levels",
                })
            }
            None => missing,
        }
    }

    pub fn load_saved_analysis(&self, path: &str) -> Result<Value, String> {
        if !Path::new(path).is_file() {
            return Err(format!("File not found: {path}"));
        }
        let content = std::fs::read_to_string(path).map_err(|e| format!("Failed to read: {e}"))?;
        let data: Value = serde_json::from_str(&content).map_err(|e| format!("Failed to parse: {e}"))?;
        let mode = data.get("mode").and_then(|v| v.as_str()).unwrap_or("vmaf");

        match mode {
            "levels" => self.engine.apply_levels_result(&data),
            "timeline" => self.engine.apply_timeline_result(&data),
            "diff" => self.engine.apply_diff_result(&data),
            _ => self.apply_vmaf_cache(cache::load_vmaf_analysis(path)?),
        }

        self.engine.set_save_path(Some(path.to_string()));
        eprintln!("[vcompare] Loaded saved {mode} analysis: {path}");
        self.engine.build_summary()
    }

    fn apply_vmaf_cache(&self, loaded: cache::LoadedVmafAnalysis) {
        self.engine.apply_vmaf_result(
            loaded.frames,
            &loaded.ref_path,
            &loaded.dist_path,
            loaded.ref_start_frame,
            loaded.ref_file_start_tc,
            &loaded.sync_mode,
            loaded.tc_format,
            None, // timeline not stored in vmaf cache
            0, 0, false, // scale not stored in vmaf cache
        );
    }

    // ── Analysis ──

    /// Run (or load from its cache) an analysis. Blocks until done;
    /// progress goes to the sink. Returns the summary.
    pub fn analyze(&self, params: &AnalyzeParams) -> Result<Value, String> {
        let mode = params.mode.clone().unwrap_or_else(|| "diff".into());
        let sync_mode = params.sync.clone().unwrap_or_else(|| "start".into());
        let save_path = params.log_path.clone();
        let force = params.force.unwrap_or(false);

        // Timeline runs always run: Resolve and the render may have changed
        // since the last one (saved runs reopen from Recent Analyses).
        if mode != "timeline" && !force {
            if let Some(summary) = self.try_cached(&mode, &sync_mode, save_path.as_deref()) {
                return summary;
            }
        }

        // A new job: supersedes (and kills) anything still winding down from
        // a cancelled one.
        let job = self.ffmpeg.begin();
        let result = match mode.as_str() {
            "timeline" => self.run_timeline(params, job.clone()),
            "levels" => self.run_levels(params, job.clone()),
            "vmaf" => self.run_vmaf(params, &sync_mode, job.clone()),
            _ => self.run_diff(params, &sync_mode, job.clone()),
        };

        // Whatever the runner reported, a cancelled job is reported as such
        // (its ffmpeg was killed, so its own error is just fallout).
        if job.is_cancelled() {
            return Err(CANCELLED.into());
        }
        result
    }

    fn try_cached(&self, mode: &str, sync_mode: &str, save_path: Option<&str>) -> Option<Result<Value, String>> {
        let sp = save_path?;
        if !Path::new(sp).is_file() {
            return None;
        }
        if mode == "levels" || mode == "diff" {
            let cached = cache::load_diff_cache(sp).ok()?;
            let ok = if mode == "levels" {
                cached.get("mode").and_then(|v| v.as_str()) == Some("levels")
            } else {
                cached.get("sync_mode").and_then(|v| v.as_str()) == Some(sync_mode)
            };
            if !ok {
                return None;
            }
            eprintln!("[vcompare] Loaded cached {mode} analysis: {sp}");
            if mode == "levels" {
                self.engine.apply_levels_result(&cached);
            } else {
                self.engine.apply_diff_result(&cached);
            }
        } else {
            let loaded = cache::load_vmaf_analysis(sp).ok()?;
            if loaded.sync_mode != sync_mode {
                return None;
            }
            eprintln!("[vcompare] Loaded cached vmaf analysis: {sp}");
            self.apply_vmaf_cache(loaded);
        }
        self.engine.set_save_path(Some(sp.to_string()));
        Some(self.engine.build_summary())
    }

    fn progress_fn(&self) -> engine::analysis::ProgressFn {
        let sink = self.sink.clone();
        Box::new(move |current, total| sink.send(UiEvent::Progress { current, total }))
    }

    fn run_diff(&self, params: &AnalyzeParams, sync_mode: &str, job: ProcessHandle) -> Result<Value, String> {
        let ref_path = params.reference.as_str();
        let dist_path = params.distorted.as_deref().unwrap_or_default();
        let offset = params.offset.unwrap_or(0);
        let with_ssim = params.ssim.unwrap_or(false);
        let save_path = params.log_path.clone();
        let cancel_check = job.clone();

        let mut diagnostics: Vec<String> = if with_ssim {
            vec!["mode: pixel difference + SSIM (psnr+ssim filters)".into()]
        } else {
            vec!["mode: pixel difference (psnr filter)".into()]
        };

        let (config, tc_fmt, timeline) = engine::analysis::prepare_comparison(
            ref_path, dist_path, sync_mode, false, offset, &mut diagnostics,
        )?;

        // Diff-specific format negotiation: upconvert to highest quality
        let target_fmt = choose_diff_pixfmt(&config.ref_src_pixfmt, &config.dist_src_pixfmt);
        if config.ref_src_pixfmt != config.dist_src_pixfmt {
            diagnostics.push(format!(
                "pixfmt: {} vs {} → {target_fmt}",
                config.ref_src_pixfmt, config.dist_src_pixfmt
            ));
        } else {
            diagnostics.push(format!("pixfmt: {} → {target_fmt}", config.ref_src_pixfmt));
        }

        let bit_depth = if target_fmt.contains("10") {
            10
        } else if target_fmt.contains("12") {
            12
        } else if target_fmt.contains("16") {
            16
        } else {
            8
        };
        let peak = (1u64 << bit_depth) - 1;
        diagnostics.push(format!("bit depth: {bit_depth} (peak {peak})"));
        self.emit(UiEvent::Diagnostics(diagnostics));

        let frames = engine::analysis::run_diff_analysis(
            ref_path, dist_path, &config, &tc_fmt,
            &target_fmt, bit_depth, with_ssim, Some(self.progress_fn()), job,
        )?;
        eprintln!("[vcompare] Parsed {} diff frames", frames.len());

        let result = json!({
            "mode": "diff",
            "frames": frames,
            "ref_path": ref_path,
            "dist_path": dist_path,
            "ref_start_frame": config.ref_start_frame,
            "ref_file_start_tc": config.ref_file_start_tc,
            "sync_mode": sync_mode,
            "bit_depth": bit_depth,
            "with_ssim": with_ssim,
            "scale_w": config.scale_w,
            "scale_h": config.scale_h,
            "scale_ref": config.scale_ref,
            "tc_format": {
                "fps_num": tc_fmt.fps_num,
                "fps_den": tc_fmt.fps_den,
                "tc_type": tc_fmt.tc_type,
            },
            "timeline": serde_json::to_value(&timeline).unwrap_or_default(),
        });

        if cancel_check.is_cancelled() {
            return Err(CANCELLED.into());
        }
        self.engine.apply_diff_result(&result);
        if let Some(ref sp) = save_path {
            cache::save_diff_analysis(&result, sp)?;
            eprintln!("[vcompare] Saved diff analysis to {sp}");
        }
        self.engine.set_save_path(save_path);
        self.engine.build_summary()
    }

    fn run_timeline(&self, params: &AnalyzeParams, job: ProcessHandle) -> Result<Value, String> {
        let run_params = engine::timeline_run::RunParams {
            file: params.reference.clone(),
            tracks: params.tracks.clone().unwrap_or_default(),
            offset: params.offset.unwrap_or(0),
            dry_run: params.dry_run.unwrap_or(false),
            grab_timeout_ms: params.grab_timeout_ms,
            settle_ms: params.settle_ms,
            workers: None,
            matrix_override: None,
        };
        let save_path = if run_params.dry_run {
            None
        } else {
            Some(
                params
                    .log_path
                    .clone()
                    .unwrap_or_else(|| cache::default_timeline_path(&run_params.file)),
            )
        };
        let cancel_check = job.clone();
        let progress = |p: &engine::timeline_run::Progress| {
            self.emit(UiEvent::Progress { current: p.done as u64, total: p.total as u64 });
            self.emit(UiEvent::TimelineProgress(p.clone()));
        };
        let diagnostics = |lines: &[String]| self.emit(UiEvent::Diagnostics(lines.to_vec()));
        let cb = engine::timeline_run::Callbacks { progress: &progress, diagnostics: &diagnostics };
        let result = engine::timeline_run::run(&run_params, &job, &cb)?;

        if cancel_check.is_cancelled() {
            return Err(CANCELLED.into());
        }
        self.engine.apply_timeline_result(&result);
        if let Some(ref sp) = save_path {
            cache::save_diff_analysis(&result, sp)?;
            eprintln!("[vcompare] Saved timeline analysis to {sp}");
        }
        self.engine.set_save_path(save_path.clone());
        let mut summary = self.engine.build_summary()?;
        summary["save_path"] = json!(save_path);
        Ok(summary)
    }

    fn run_levels(&self, params: &AnalyzeParams, job: ProcessHandle) -> Result<Value, String> {
        let file_path = params.reference.as_str();
        let chunks = params.chunks.filter(|&n| n > 0);
        let want_sigs = params.signatures.unwrap_or(true);
        let save_path = params.log_path.clone();
        let cancel_check = job.clone();

        let mut diagnostics = vec!["mode: levels".into()];
        let info = engine::analysis::prepare_single_file(file_path, &mut diagnostics)?;
        let hwaccel = engine::probe::detect_hwaccel();
        let publish = |lines: &[String]| self.emit(UiEvent::Diagnostics(lines.to_vec()));
        publish(&diagnostics);

        let (frames, perf, sigs) = engine::analysis::run_levels_analysis(
            file_path,
            &info,
            hwaccel.as_deref(),
            chunks,
            want_sigs,
            Some(self.progress_fn()),
            job,
            &mut diagnostics,
            &publish,
        )?;
        eprintln!("[vcompare] {} levels frames ({}-bit)", frames.len(), info.bit_depth);

        let tc_fmt = &info.tc_fmt;
        let result = json!({
            "mode": "levels",
            "stats_engine": "native",
            "frames": frames,
            "ref_path": file_path,
            "ref_start_frame": info.start_frame,
            "ref_file_start_tc": info.start_frame,
            "bit_depth": info.bit_depth,
            "tc_format": {
                "fps_num": tc_fmt.fps_num,
                "fps_den": tc_fmt.fps_den,
                "tc_type": tc_fmt.tc_type,
            },
        });

        if cancel_check.is_cancelled() {
            return Err(CANCELLED.into());
        }
        self.engine.apply_levels_result(&result);

        if let Some(ref sp) = save_path {
            cache::save_diff_analysis(&result, sp)?;
            eprintln!("[vcompare] Saved levels analysis to {sp}");
            let sig_path = engine::signatures::sidecar_path(sp);
            match sigs {
                Some(ref s) => match s.write(&sig_path) {
                    Ok(()) => eprintln!("[vcompare] Saved {} frame signatures to {sig_path}", s.count()),
                    Err(e) => eprintln!("[vcompare] Could not save frame signatures: {e}"),
                },
                // An old sidecar would no longer match this analysis.
                None => {
                    let _ = std::fs::remove_file(&sig_path);
                }
            }
        }
        self.engine.set_signatures(sigs);
        self.engine.set_save_path(save_path);
        // Not part of the stored analysis: only for this run's status line.
        let mut summary = self.engine.build_summary()?;
        summary["perf"] = json!(perf);
        Ok(summary)
    }

    fn run_vmaf(&self, params: &AnalyzeParams, sync_mode: &str, job: ProcessHandle) -> Result<Value, String> {
        let ref_path = params.reference.as_str();
        let dist_path = params.distorted.as_deref().unwrap_or_default();
        let offset = params.offset.unwrap_or(0);
        let model = params.model.clone().unwrap_or_else(|| "vmaf_v0.6.1".into());
        let hwaccel_pref = params.hwaccel.clone().unwrap_or_else(|| "auto".into());
        let subsample = params.subsample.unwrap_or(1);
        let want_psnr = params.psnr.unwrap_or(false);
        let save_path = params.log_path.clone();
        let cancel_check = job.clone();

        let hwaccel = match hwaccel_pref.as_str() {
            "auto" => engine::probe::detect_hwaccel(),
            "none" => None,
            other => Some(other.to_string()),
        };
        let vmaf_cuda = hwaccel.as_deref() == Some("cuda") && engine::probe::detect_vmaf_cuda();

        let mut diagnostics = vec!["mode: vmaf".into()];
        let (mut config, tc_fmt, timeline) = engine::analysis::prepare_comparison(
            ref_path, dist_path, sync_mode, vmaf_cuda, offset, &mut diagnostics,
        )?;
        self.emit(UiEvent::Diagnostics(diagnostics.clone()));

        let frames = engine::analysis::run_vmaf_analysis(
            ref_path, dist_path, &mut config, &tc_fmt,
            &model, hwaccel.as_deref(), subsample, want_psnr,
            Some(self.progress_fn()), &mut diagnostics, job,
        )?;
        eprintln!("[vcompare] Parsed {} vmaf frames", frames.len());

        if cancel_check.is_cancelled() {
            return Err(CANCELLED.into());
        }
        if let Some(ref sp) = save_path {
            cache::save_vmaf_analysis(
                &frames, ref_path, dist_path,
                config.ref_start_frame, config.ref_file_start_tc,
                sync_mode, Some(&tc_fmt), sp,
            )?;
            eprintln!("[vcompare] Saved vmaf analysis to {sp}");
        }
        self.engine.apply_vmaf_result(
            frames, ref_path, dist_path,
            config.ref_start_frame, config.ref_file_start_tc,
            sync_mode, Some(tc_fmt), Some(timeline),
            config.scale_w, config.scale_h, config.scale_ref,
        );
        self.engine.set_save_path(save_path);
        self.engine.build_summary()
    }

    /// Cancel the running analysis: kills its ffmpeg (or the next one it
    /// starts) and stops it from saving anything. Returns whether ffmpeg
    /// was running.
    pub fn cancel_analysis(&self) -> bool {
        let was_running = self.ffmpeg.cancel();
        eprintln!("[vcompare] analysis cancelled (ffmpeg was running: {was_running})");
        was_running
    }

    /// Video tracks of the timeline open in Resolve, for the track
    /// checklist, plus whether the grabber is installed. Uses a short-lived
    /// helper of its own (independent of cursor sync).
    pub fn timeline_tracks(&self) -> Result<Value, String> {
        let mut s = engine::resolve::ResolveSession::connect()?;
        let t = s.tracks()?;
        let target = s.target.clone();
        s.close();
        Ok(json!({
            "target": target,
            "timeline": t.timeline,
            "tracks": t.tracks,
            "grabber": engine::grab::find_binary().map(|p| p.to_string_lossy().to_string()),
        }))
    }

    // ── Queries on the loaded analysis ──

    pub fn get_frames(&self, params: &GetFramesParams) -> Result<Value, String> {
        let v = serde_json::to_value(params).map_err(|e| format!("Failed to serialize: {e}"))?;
        self.engine.get_frames(&v)
    }

    pub fn get_metric_series(&self, metric: &str) -> Result<Value, String> {
        self.engine.get_metric_series(metric)
    }

    /// `get_metric_series` as typed arrays (frames, values).
    pub fn metric_series(&self, metric: &str) -> Result<(Vec<i64>, Vec<f64>), String> {
        self.engine.metric_series(metric)
    }

    /// Every frame as columns, in frame order (native table).
    pub fn frame_columns(&self, metrics: &[String]) -> Result<engine::state::FrameColumns, String> {
        self.engine.frame_columns(metrics)
    }

    /// Run event detection over the loaded analysis. Pure in-memory scan;
    /// cheap enough to call on every slider change.
    pub fn detect_events(&self, request: &DetectRequest) -> Result<DetectResult, String> {
        let refs = match request {
            DetectRequest::Match(p) => self.references.get(&p.references),
            _ => Vec::new(),
        };
        self.engine.detect_events(request, &refs)
    }

    /// Thumbnails of `count` consecutive display frames from `frame`, one
    /// row per file of the analysis. At most `PREVIEW_SLOTS` run at once;
    /// callers beyond that wait.
    pub fn preview_frames(&self, frame: i64, count: u32, height: u32) -> Result<Vec<PreviewRow>, String> {
        let _slot = self.previews.acquire();
        let (sources, fps) = self.engine.preview_sources()?;
        let tcs: Vec<String> = (0..count as i64)
            .map(|k| self.engine.frame_to_tc(frame + k))
            .collect::<Result<_, _>>()?;
        let mut rows = Vec::new();
        for (label, path, origin) in sources {
            let file_frame = frame - origin;
            let jpegs = if file_frame + count as i64 <= 0 {
                Vec::new()
            } else {
                engine::thumbs::extract_frames(&path, fps, file_frame, count, height)?
            };
            // Frames before the file's start come back as missing.
            let skipped = (-file_frame).max(0) as usize;
            let mut jpegs = jpegs.into_iter();
            let frames = (0..count as usize)
                .map(|k| PreviewFrame {
                    frame: frame + k as i64,
                    tc: tcs[k].clone(),
                    jpeg: if k >= skipped { jpegs.next() } else { None },
                })
                .collect();
            rows.push(PreviewRow { label, frames });
        }
        Ok(rows)
    }

    /// Accept / reject (or clear, with `None`) the event at `frame`.
    pub fn set_event_review(&self, detector: Detector, frame: i64, review: Option<&str>) -> Result<Value, String> {
        self.engine.set_event_review(detector, frame, review)
    }

    pub fn export_events_edl(&self, detector: Option<Detector>, path: Option<&str>) -> Result<Value, String> {
        self.engine.export_events_edl(detector, path)
    }

    pub fn export_events_csv(&self, detector: Option<Detector>, path: Option<&str>) -> Result<Value, String> {
        self.engine.export_events_csv(detector, path)
    }

    pub fn export_csv(&self, path: Option<&str>) -> Result<Value, String> {
        self.engine.export_csv(path)
    }

    // ── Image-match references ──

    pub fn list_references(&self) -> Vec<StoredReference> {
        self.references.list()
    }

    /// Add a reference from an image file, converted into the loaded
    /// analysis's frame size and range.
    pub fn add_reference_from_image(&self, path: &str, name: Option<String>) -> Result<StoredReference, String> {
        let (sources, _) = self.engine.preview_sources()?;
        let file = sources.into_iter().next().map(|s| s.1).ok_or("No analysis loaded")?;
        let mut d = Vec::new();
        let info = engine::analysis::prepare_single_file(&file, &mut d)?;
        let sig = engine::references::signature_from_image(path, info.width, info.height, info.full_range)?;
        let flat = engine::detect::matcher::Prepared::new(&sig).is_flat();
        let stem = Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "reference".into());
        let r = StoredReference {
            id: engine::references::new_id(),
            name: name.filter(|n| !n.trim().is_empty()).unwrap_or(stem),
            source: path.to_string(),
            thumb: engine::references::thumbnail_of_image(path),
            sig,
            flat,
        };
        self.references.save(&r)?;
        Ok(r)
    }

    /// Add a reference from a frame of the loaded (levels) analysis: its
    /// signature is copied from the analysis's signatures, so it is exact.
    pub fn add_reference_from_frame(&self, frame: i64, name: Option<String>) -> Result<StoredReference, String> {
        let sigs = self.engine.signatures()?;
        let sig = sigs.get(frame).ok_or("That frame has no signature")?.to_vec();
        let tc = self.engine.frame_to_tc(frame)?;
        let (sources, fps) = self.engine.preview_sources()?;
        let (_, path, origin) = sources.into_iter().next().ok_or("No analysis loaded")?;
        let thumb = engine::thumbs::extract_frames(&path, fps, frame - origin, 1, 108)
            .ok()
            .and_then(|v| v.into_iter().next())
            .map(|jpeg| engine::thumbs::data_uri(&jpeg));
        let file_name = Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let r = StoredReference {
            id: engine::references::new_id(),
            name: name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| format!("{file_name} {tc}")),
            source: format!("{file_name} @ {tc}"),
            flat: engine::detect::matcher::Prepared::new(&sig).is_flat(),
            sig,
            thumb,
        };
        self.references.save(&r)?;
        Ok(r)
    }

    pub fn rename_reference(&self, id: &str, name: &str) -> Result<(), String> {
        self.references.rename(id, name)
    }

    pub fn delete_reference(&self, id: &str) -> Result<(), String> {
        self.references.delete(id)
    }

    // ── DaVinci Resolve sync ──

    /// Connect to the running Resolve. Status changes go to the sink as
    /// `UiEvent::ResolveStatus`.
    pub fn resolve_connect(&self) -> Result<engine::resolve::ResolveStatus, String> {
        if let Some(ref s) = *self.resolve.lock().unwrap() {
            if s.is_running() {
                return Ok(s.status());
            }
        }
        let sink = self.sink.clone();
        let sync = engine::resolve::ResolveSync::connect(Box::new(move |st| {
            sink.send(UiEvent::ResolveStatus(st));
        }))?;
        let status = sync.status();
        *self.resolve.lock().unwrap() = Some(sync);
        Ok(status)
    }

    /// Move Resolve's playhead to a display frame of the current analysis
    /// (converted to timecode here so drop-frame is handled in one place).
    /// Newest wins while one seek is in flight. Returns the timecode.
    pub fn resolve_seek(&self, frame: i64) -> Result<String, String> {
        let timecode = self.engine.frame_to_tc(frame)?;
        let g = self.resolve.lock().unwrap();
        let s = g.as_ref().ok_or("Resolve sync is not connected")?;
        s.seek(&timecode);
        Ok(timecode)
    }

    pub fn resolve_disconnect(&self) {
        let taken = self.resolve.lock().unwrap().take();
        if let Some(s) = taken {
            s.disconnect();
        }
    }

    // ── mpv (side-by-side / single) and difference view ──

    /// Launch mpv on the loaded analysis. Returns `"launched"` or
    /// `"already_running"`.
    pub fn mpv_launch(&self) -> Result<&'static str, String> {
        self.mpv.launch(&self.engine, &self.sink)
    }

    pub fn mpv_seek(&self, frame: i64) -> Result<bool, String> {
        self.mpv.with(|m| m.seek_to_frame(frame))
    }

    /// `"backward"` steps back; anything else steps forward.
    pub fn mpv_step(&self, direction: &str) -> Result<bool, String> {
        self.mpv.with(|m| if direction == "backward" { m.step_backward() } else { m.step_forward() })
    }

    pub fn mpv_toggle_pause(&self) -> Result<bool, String> {
        self.mpv.with(|m| m.toggle_pause())
    }

    pub fn mpv_kill(&self) {
        self.mpv.kill();
    }

    pub fn diffview_launch(&self) -> Result<&'static str, String> {
        self.diffview.launch(&self.engine, &self.sink)
    }

    pub fn diffview_seek(&self, frame: i64) -> Result<bool, String> {
        self.diffview.with(|m| m.seek_to_frame(frame))
    }

    pub fn diffview_step(&self, direction: &str) -> Result<bool, String> {
        self.diffview.with(|m| match direction {
            "forward" => Ok(m.step_forward()),
            "backward" => Ok(m.step_backward()),
            _ => Err("invalid direction".to_string()),
        })?
    }

    pub fn diffview_pause(&self) -> Result<bool, String> {
        self.diffview.with(|m| m.toggle_pause())
    }

    pub fn diffview_kill(&self) {
        self.diffview.kill();
    }

    // ── Recent analyses ──

    pub fn recents(&self) -> Vec<RecentEntry> {
        self.recents.list()
    }

    pub fn add_recent(&self, entry: RecentEntry) {
        self.recents.add(entry)
    }

    pub fn clear_recents(&self) {
        self.recents.clear()
    }
}
