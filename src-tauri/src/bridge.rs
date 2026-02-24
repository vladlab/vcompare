//! Tauri commands — the frontend calls these via `invoke()`.
//!
//! All analysis logic now runs natively via the engine module.
//! No Python sidecar, no JSON-RPC — just direct function calls.

use crate::engine;
use crate::engine::analysis::ProcessHandle;
use crate::engine::cache;
use crate::engine::mpv::MpvController;
use crate::engine::pixfmt::choose_diff_pixfmt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, State};

// Re-export for lib.rs
pub use crate::engine::state::EngineState;

// ─── Typed param structs for Tauri IPC ──────────────────────────────────────

#[derive(Deserialize, Serialize)]
pub struct AnalyzeParams {
    pub reference: String,
    pub distorted: String,
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
    pub log_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
}

#[derive(Deserialize, Serialize)]
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

// ─── Shared state types ─────────────────────────────────────────────────────

/// Manages the mpv controller and its poll thread.
pub struct MpvState {
    pub controller: Mutex<Option<Arc<MpvController>>>,
}

impl MpvState {
    pub fn new() -> Self {
        Self {
            controller: Mutex::new(None),
        }
    }
}

/// Handle to a running ffmpeg process for cancellation.
pub struct FfmpegHandle(pub ProcessHandle);

impl FfmpegHandle {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }
}

// ─── Recent analyses ────────────────────────────────────────────────────────

const MAX_RECENTS: usize = 20;

#[derive(Clone, Serialize, Deserialize)]
pub struct RecentEntry {
    pub ref_path: String,
    pub dist_path: String,
    pub cache_path: String,
    pub mode: String,
    pub frame_count: u64,
    pub timestamp: u64,
}

pub struct RecentsState {
    path: PathBuf,
    entries: Mutex<Vec<RecentEntry>>,
}

impl RecentsState {
    pub fn new(data_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&data_dir);
        let path = data_dir.join("recents.json");
        let entries = Self::load_file(&path);
        Self {
            path,
            entries: Mutex::new(entries),
        }
    }

    fn load_file(path: &PathBuf) -> Vec<RecentEntry> {
        match std::fs::read_to_string(path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn save(&self, entries: &[RecentEntry]) {
        if let Ok(json) = serde_json::to_string_pretty(entries) {
            let _ = std::fs::write(&self.path, json);
        }
    }
}

// ─── Commands ───────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn ping() -> Result<Value, String> {
    Ok(json!({"status": "ok", "engine": "native"}))
}

#[tauri::command]
pub async fn detect_capabilities() -> Result<engine::types::Capabilities, String> {
    tokio::task::spawn_blocking(engine::probe::detect_capabilities)
        .await
        .map_err(|e| format!("Task join error: {e}"))
}

#[tauri::command]
pub async fn probe_file(path: String) -> Result<engine::types::ProbeResult, String> {
    tokio::task::spawn_blocking(move || engine::probe::probe_file(&path))
        .await
        .map_err(|e| format!("Task join error: {e}"))?
}

#[tauri::command]
pub async fn check_cache(
    reference: String,
    distorted: String,
) -> Result<Value, String> {
    Ok(cache::check_cache(&reference, &distorted, None))
}

#[tauri::command]
pub async fn load_saved_analysis(
    path: String,
    engine_state: State<'_, EngineState>,
) -> Result<Value, String> {
    if !std::path::Path::new(&path).is_file() {
        return Err(format!("File not found: {path}"));
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read: {e}"))?;
    let data: Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse: {e}"))?;

    let mode = data.get("mode").and_then(|v| v.as_str()).unwrap_or("vmaf");

    if mode == "diff" {
        engine_state.apply_diff_result(&data);
    } else {
        let loaded = cache::load_vmaf_analysis(&path)?;
        engine_state.apply_vmaf_result(
            loaded.frames,
            &loaded.ref_path,
            &loaded.dist_path,
            loaded.ref_start_frame,
            loaded.ref_file_start_tc,
            &loaded.sync_mode,
            loaded.tc_format,
            None, // timeline not stored in vmaf cache
        );
    }

    engine_state.set_save_path(Some(path.clone()));
    eprintln!("[vcompare] Loaded saved {mode} analysis: {path}");

    engine_state.build_summary()
}

#[tauri::command]
pub async fn analyze(
    app: AppHandle,
    params: AnalyzeParams,
    engine_state: State<'_, EngineState>,
    ffmpeg_handle: State<'_, FfmpegHandle>,
) -> Result<Value, String> {
    let mode = params.mode.clone().unwrap_or_else(|| "diff".into());
    let ref_path = params.reference.clone();
    let dist_path = params.distorted.clone();
    let sync_mode = params.sync.clone().unwrap_or_else(|| "start".into());
    let offset = params.offset.unwrap_or(0);
    let save_path = params.log_path.clone();
    let force = params.force.unwrap_or(false);

    // Check cache first
    if let Some(ref sp) = save_path {
        if !force && std::path::Path::new(sp).is_file() {
            if mode == "diff" {
                if let Ok(cached) = cache::load_diff_cache(sp) {
                    let cached_sync = cached.get("sync_mode").and_then(|v| v.as_str());
                    if cached_sync == Some(sync_mode.as_str()) {
                        eprintln!("[vcompare] Loaded cached diff analysis: {sp}");
                        engine_state.apply_diff_result(&cached);
                        engine_state.set_save_path(save_path);
                        return engine_state.build_summary();
                    }
                }
            } else if let Ok(loaded) = cache::load_vmaf_analysis(sp) {
                if loaded.sync_mode == sync_mode {
                    eprintln!("[vcompare] Loaded cached vmaf analysis: {sp}");
                    engine_state.apply_vmaf_result(
                        loaded.frames,
                        &loaded.ref_path,
                        &loaded.dist_path,
                        loaded.ref_start_frame,
                        loaded.ref_file_start_tc,
                        &loaded.sync_mode,
                        loaded.tc_format,
                        None, // timeline not in vmaf cache
                    );
                    engine_state.set_save_path(save_path);
                    return engine_state.build_summary();
                }
            }
        }
    }

    let process_handle = ffmpeg_handle.0.clone();

    if mode == "vmaf" {
        _run_vmaf_analysis(
            app, &ref_path, &dist_path, &sync_mode, offset, &params,
            save_path, &engine_state, process_handle,
        )
        .await
    } else {
        _run_diff_analysis(
            app, &ref_path, &dist_path, &sync_mode, offset,
            save_path, &engine_state, process_handle,
        )
        .await
    }
}

async fn _run_diff_analysis(
    app: AppHandle,
    ref_path: &str,
    dist_path: &str,
    sync_mode: &str,
    offset: i64,
    save_path: Option<String>,
    engine_state: &State<'_, EngineState>,
    process_handle: ProcessHandle,
) -> Result<Value, String> {
    let ref_path = ref_path.to_string();
    let dist_path = dist_path.to_string();
    let sync_mode = sync_mode.to_string();
    let app_clone = app.clone();

    let result = tokio::task::spawn_blocking(move || {
        let mut diagnostics = vec!["mode: pixel difference (psnr filter)".into()];

        let (config, tc_fmt, timeline) = engine::analysis::prepare_comparison(
            &ref_path, &dist_path, &sync_mode,
            false, offset, &mut diagnostics,
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

        let _ = app_clone.emit("rpc:diagnostics", json!({"lines": diagnostics}));

        let app_progress = app_clone.clone();
        let progress: engine::analysis::ProgressFn = Box::new(move |current, total| {
            let _ = app_progress.emit("rpc:progress", json!({"current": current, "total": total}));
        });

        let frames = engine::analysis::run_diff_analysis(
            &ref_path, &dist_path, &config, &tc_fmt,
            &target_fmt, bit_depth, Some(progress), process_handle,
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
            "tc_format": {
                "fps_num": tc_fmt.fps_num,
                "fps_den": tc_fmt.fps_den,
                "tc_type": tc_fmt.tc_type,
            },
            "timeline": serde_json::to_value(&timeline).unwrap_or_default(),
        });

        Ok::<(Value, Option<String>), String>((result, save_path))
    })
    .await
    .map_err(|e| format!("Task join error: {e}"))??;

    let (result_val, save_path) = result;

    engine_state.apply_diff_result(&result_val);

    if let Some(ref sp) = save_path {
        cache::save_diff_analysis(&result_val, sp)?;
        eprintln!("[vcompare] Saved diff analysis to {sp}");
    }

    engine_state.set_save_path(save_path);
    engine_state.build_summary()
}

async fn _run_vmaf_analysis(
    app: AppHandle,
    ref_path: &str,
    dist_path: &str,
    sync_mode: &str,
    offset: i64,
    params: &AnalyzeParams,
    save_path: Option<String>,
    engine_state: &State<'_, EngineState>,
    process_handle: ProcessHandle,
) -> Result<Value, String> {
    let ref_path = ref_path.to_string();
    let dist_path = dist_path.to_string();
    let sync_mode = sync_mode.to_string();
    let model = params.model.clone().unwrap_or_else(|| "vmaf_v0.6.1".into());
    let hwaccel_pref = params.hwaccel.clone().unwrap_or_else(|| "auto".into());
    let subsample = params.subsample.unwrap_or(1);
    let want_psnr = params.psnr.unwrap_or(false);
    let app_clone = app.clone();

    let result = tokio::task::spawn_blocking(move || {
        let hwaccel = match hwaccel_pref.as_str() {
            "auto" => engine::probe::detect_hwaccel(),
            "none" => None,
            other => Some(other.to_string()),
        };

        let vmaf_cuda = if hwaccel.as_deref() == Some("cuda") {
            engine::probe::detect_vmaf_cuda()
        } else {
            false
        };

        let mut diagnostics = vec!["mode: vmaf".into()];

        let (mut config, tc_fmt, timeline) = engine::analysis::prepare_comparison(
            &ref_path, &dist_path, &sync_mode,
            vmaf_cuda, offset, &mut diagnostics,
        )?;

        let _ = app_clone.emit("rpc:diagnostics", json!({"lines": diagnostics.clone()}));

        let app_progress = app_clone.clone();
        let progress: engine::analysis::ProgressFn = Box::new(move |current, total| {
            let _ = app_progress.emit("rpc:progress", json!({"current": current, "total": total}));
        });

        let frames = engine::analysis::run_vmaf_analysis(
            &ref_path, &dist_path, &mut config, &tc_fmt,
            &model, hwaccel.as_deref(), subsample, want_psnr,
            Some(progress), &mut diagnostics, process_handle,
        )?;

        eprintln!("[vcompare] Parsed {} vmaf frames", frames.len());

        Ok::<_, String>((
            frames, ref_path, dist_path,
            config.ref_start_frame, config.ref_file_start_tc,
            sync_mode, tc_fmt, save_path, timeline,
        ))
    })
    .await
    .map_err(|e| format!("Task join error: {e}"))??;

    let (frames, ref_path, dist_path, ref_start_frame, ref_file_start_tc, sync_mode, tc_fmt, save_path, timeline) = result;

    if let Some(ref sp) = save_path {
        cache::save_vmaf_analysis(
            &frames, &ref_path, &dist_path,
            ref_start_frame, ref_file_start_tc,
            &sync_mode, Some(&tc_fmt), sp,
        )?;
        eprintln!("[vcompare] Saved vmaf analysis to {sp}");
    }

    engine_state.apply_vmaf_result(
        frames, &ref_path, &dist_path,
        ref_start_frame, ref_file_start_tc,
        &sync_mode, Some(tc_fmt), Some(timeline),
    );
    engine_state.set_save_path(save_path);
    engine_state.build_summary()
}

#[tauri::command]
pub async fn get_frames(
    params: GetFramesParams,
    engine_state: State<'_, EngineState>,
) -> Result<Value, String> {
    let params_value = serde_json::to_value(params)
        .map_err(|e| format!("Failed to serialize: {e}"))?;
    engine_state.get_frames(&params_value)
}

#[tauri::command]
pub async fn get_metric_series(
    metric: String,
    engine_state: State<'_, EngineState>,
) -> Result<Value, String> {
    engine_state.get_metric_series(&metric)
}

// ─── mpv commands ───────────────────────────────────────────────────────────

#[tauri::command]
pub async fn mpv_launch(
    app: AppHandle,
    engine_state: State<'_, EngineState>,
    mpv_state: State<'_, MpvState>,
) -> Result<Value, String> {
    let ctx = engine_state.mpv_context()?;

    {
        let existing = mpv_state.controller.lock().unwrap();
        if let Some(ref mpv) = *existing {
            if mpv.is_running() {
                return Ok(json!({"status": "already_running"}));
            }
        }
    }

    let mpv = Arc::new(MpvController::new(
        ctx.ref_path,
        ctx.dist_path,
        ctx.tc_format,
        ctx.ref_file_start_tc,
        ctx.timeline,
    ));

    if let Some(err) = mpv.launch() {
        return Err(err);
    }

    *mpv_state.controller.lock().unwrap() = Some(mpv.clone());

    // Spawn position poll thread.
    // The thread holds its own Arc<MpvController> and exits when is_running() is false
    // (either mpv was closed by user, or mpv_kill was called which kills the process).
    let poll_mpv = mpv.clone();
    let app_poll = app.clone();

    std::thread::spawn(move || {
        let mut last_frame: Option<i64> = None;
        loop {
            if !poll_mpv.is_running() {
                eprintln!("[vcompare] mpv process exited");
                let _ = app_poll.emit("rpc:mpv-exited", json!({"reason": "closed"}));
                break;
            }
            if let Some(frame) = poll_mpv.get_frame() {
                if last_frame != Some(frame) {
                    last_frame = Some(frame);
                    let _ = app_poll.emit("rpc:mpv-position", json!({"frame": frame}));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });

    Ok(json!({"status": "launched"}))
}

#[tauri::command]
pub async fn mpv_seek(
    frame: i64,
    mpv_state: State<'_, MpvState>,
) -> Result<Value, String> {
    let ctrl = mpv_state.controller.lock().unwrap();
    let mpv = ctrl.as_ref().ok_or("mpv not running")?;
    Ok(json!({"ok": mpv.seek_to_frame(frame)}))
}

#[tauri::command]
pub async fn mpv_step(
    direction: String,
    mpv_state: State<'_, MpvState>,
) -> Result<Value, String> {
    let ctrl = mpv_state.controller.lock().unwrap();
    let mpv = ctrl.as_ref().ok_or("mpv not running")?;
    let ok = if direction == "backward" {
        mpv.step_backward()
    } else {
        mpv.step_forward()
    };
    Ok(json!({"ok": ok}))
}

#[tauri::command]
pub async fn mpv_toggle_pause(
    mpv_state: State<'_, MpvState>,
) -> Result<Value, String> {
    let ctrl = mpv_state.controller.lock().unwrap();
    let mpv = ctrl.as_ref().ok_or("mpv not running")?;
    Ok(json!({"ok": mpv.toggle_pause()}))
}

#[tauri::command]
pub async fn mpv_kill(
    mpv_state: State<'_, MpvState>,
) -> Result<Value, String> {
    let mut ctrl = mpv_state.controller.lock().unwrap();
    if let Some(mpv) = ctrl.take() {
        mpv.kill(); // kills the process → poll thread's is_running() returns false
    }
    Ok(json!({"status": "killed"}))
}

// ─── Export ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn export_csv(
    path: Option<String>,
    engine_state: State<'_, EngineState>,
) -> Result<Value, String> {
    engine_state.export_csv(path.as_deref())
}

// ─── Recent analyses ────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_recents(
    recents: State<'_, RecentsState>,
) -> Result<Vec<RecentEntry>, String> {
    let entries = recents.entries.lock().unwrap();
    Ok(entries.clone())
}

#[tauri::command]
pub async fn add_recent(
    recents: State<'_, RecentsState>,
    entry: RecentEntry,
) -> Result<(), String> {
    let mut entries = recents.entries.lock().unwrap();
    entries.retain(|e| e.cache_path != entry.cache_path);
    entries.insert(0, entry);
    entries.truncate(MAX_RECENTS);
    recents.save(&entries);
    Ok(())
}

#[tauri::command]
pub async fn clear_recents(recents: State<'_, RecentsState>) -> Result<(), String> {
    let mut entries = recents.entries.lock().unwrap();
    entries.clear();
    recents.save(&entries);
    Ok(())
}
