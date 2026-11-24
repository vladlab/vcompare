//! Tauri commands — the web frontend calls these via `invoke()`.
//!
//! Each one is a thin wrapper over `vcompare_service::Service`, run on a
//! blocking thread. Command names, argument names and the JSON they return
//! are the frontend's contract (`src/lib/rpc.ts`); keep them stable.

use serde_json::{json, Value};
use std::sync::Arc;
use tauri::State;
use vcompare_service::engine;
use vcompare_service::engine::detect::{DetectRequest, DetectResult, Detector};
use vcompare_service::engine::references::StoredReference;
use vcompare_service::{AnalyzeParams, GetFramesParams, RecentEntry, Service};

pub type Svc<'a> = State<'a, Arc<Service>>;

/// Run `f` on a blocking thread with the service.
async fn blocking<T, F>(svc: &Svc<'_>, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&Service) -> T + Send + 'static,
{
    let s = Arc::clone(svc);
    tauri::async_runtime::spawn_blocking(move || f(&s))
        .await
        .map_err(|e| format!("Task join error: {e}"))
}

// ─── Probing and caches ─────────────────────────────────────────────────────

#[tauri::command]
pub async fn ping() -> Result<Value, String> {
    Ok(json!({"status": "ok", "engine": "native"}))
}

#[tauri::command]
pub async fn detect_capabilities(svc: Svc<'_>) -> Result<engine::types::Capabilities, String> {
    blocking(&svc, |s| s.detect_capabilities()).await
}

#[tauri::command]
pub async fn probe_file(path: String, svc: Svc<'_>) -> Result<engine::types::ProbeResult, String> {
    blocking(&svc, move |s| s.probe_file(&path)).await?
}

#[tauri::command]
pub async fn check_cache(reference: String, distorted: String, svc: Svc<'_>) -> Result<Value, String> {
    Ok(svc.check_cache(&reference, &distorted))
}

#[tauri::command]
pub async fn check_levels_cache(reference: String, svc: Svc<'_>) -> Result<Value, String> {
    Ok(svc.check_levels_cache(&reference))
}

#[tauri::command]
pub async fn load_saved_analysis(path: String, svc: Svc<'_>) -> Result<Value, String> {
    blocking(&svc, move |s| s.load_saved_analysis(&path)).await?
}

// ─── Analysis ───────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn analyze(params: AnalyzeParams, svc: Svc<'_>) -> Result<Value, String> {
    blocking(&svc, move |s| s.analyze(&params)).await?
}

/// Cancel the running analysis: kills its ffmpeg (or the next one it starts)
/// and stops it from saving anything. Recents are only recorded by the
/// frontend after a successful job, so nothing else needs undoing.
#[tauri::command]
pub async fn cancel_analysis(svc: Svc<'_>) -> Result<Value, String> {
    let was_running = svc.cancel_analysis();
    Ok(json!({"cancelled": true, "was_running": was_running}))
}

#[tauri::command]
pub async fn timeline_tracks(svc: Svc<'_>) -> Result<Value, String> {
    blocking(&svc, |s| s.timeline_tracks()).await?
}

#[tauri::command]
pub async fn get_frames(params: GetFramesParams, svc: Svc<'_>) -> Result<Value, String> {
    svc.get_frames(&params)
}

#[tauri::command]
pub async fn get_metric_series(metric: String, svc: Svc<'_>) -> Result<Value, String> {
    svc.get_metric_series(&metric)
}

// ─── Events ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn detect_events(request: Option<DetectRequest>, svc: Svc<'_>) -> Result<DetectResult, String> {
    svc.detect_events(&request.unwrap_or_default())
}

/// `{ rows: [{ label, frames: [{ frame, tc, src }] }] }`, `src` a data: URI
/// or null before the start of the file.
#[tauri::command]
pub async fn preview_frames(frame: i64, count: u32, height: u32, svc: Svc<'_>) -> Result<Value, String> {
    let rows = blocking(&svc, move |s| s.preview_frames(frame, count, height)).await??;
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let frames: Vec<Value> = r
                .frames
                .into_iter()
                .map(|f| {
                    let src = f.jpeg.as_deref().map(engine::thumbs::data_uri);
                    json!({ "frame": f.frame, "tc": f.tc, "src": src })
                })
                .collect();
            json!({ "label": r.label, "frames": frames })
        })
        .collect();
    Ok(json!({ "rows": rows }))
}

#[tauri::command]
pub async fn set_event_review(
    detector: Option<Detector>,
    frame: i64,
    review: Option<String>,
    svc: Svc<'_>,
) -> Result<Value, String> {
    svc.set_event_review(detector.unwrap_or(Detector::Flash), frame, review.as_deref())
}

#[tauri::command]
pub async fn export_events_edl(detector: Option<Detector>, path: Option<String>, svc: Svc<'_>) -> Result<Value, String> {
    svc.export_events_edl(detector, path.as_deref())
}

#[tauri::command]
pub async fn export_events_csv(detector: Option<Detector>, path: Option<String>, svc: Svc<'_>) -> Result<Value, String> {
    svc.export_events_csv(detector, path.as_deref())
}

// ─── Image-match references ─────────────────────────────────────────────────

#[tauri::command]
pub async fn list_references(svc: Svc<'_>) -> Result<Vec<StoredReference>, String> {
    Ok(svc.list_references())
}

#[tauri::command]
pub async fn add_reference_from_image(path: String, name: Option<String>, svc: Svc<'_>) -> Result<StoredReference, String> {
    blocking(&svc, move |s| s.add_reference_from_image(&path, name)).await?
}

#[tauri::command]
pub async fn add_reference_from_frame(frame: i64, name: Option<String>, svc: Svc<'_>) -> Result<StoredReference, String> {
    blocking(&svc, move |s| s.add_reference_from_frame(frame, name)).await?
}

#[tauri::command]
pub async fn rename_reference(id: String, name: String, svc: Svc<'_>) -> Result<(), String> {
    svc.rename_reference(&id, &name)
}

#[tauri::command]
pub async fn delete_reference(id: String, svc: Svc<'_>) -> Result<(), String> {
    svc.delete_reference(&id)
}

// ─── DaVinci Resolve sync ───────────────────────────────────────────────────

#[tauri::command]
pub async fn resolve_connect(svc: Svc<'_>) -> Result<engine::resolve::ResolveStatus, String> {
    blocking(&svc, |s| s.resolve_connect()).await?
}

#[tauri::command]
pub async fn resolve_seek(frame: i64, svc: Svc<'_>) -> Result<Value, String> {
    let timecode = svc.resolve_seek(frame)?;
    Ok(json!({"ok": true, "timecode": timecode}))
}

#[tauri::command]
pub async fn resolve_disconnect(svc: Svc<'_>) -> Result<Value, String> {
    blocking(&svc, |s| s.resolve_disconnect()).await?;
    Ok(json!({"status": "disconnected"}))
}

// ─── mpv and difference view ────────────────────────────────────────────────

#[tauri::command]
pub async fn mpv_launch(svc: Svc<'_>) -> Result<Value, String> {
    let status = blocking(&svc, |s| s.mpv_launch()).await??;
    Ok(json!({"status": status}))
}

#[tauri::command]
pub async fn mpv_seek(frame: i64, svc: Svc<'_>) -> Result<Value, String> {
    Ok(json!({"ok": svc.mpv_seek(frame)?}))
}

#[tauri::command]
pub async fn mpv_step(direction: String, svc: Svc<'_>) -> Result<Value, String> {
    Ok(json!({"ok": svc.mpv_step(&direction)?}))
}

#[tauri::command]
pub async fn mpv_toggle_pause(svc: Svc<'_>) -> Result<Value, String> {
    Ok(json!({"ok": svc.mpv_toggle_pause()?}))
}

#[tauri::command]
pub async fn mpv_kill(svc: Svc<'_>) -> Result<Value, String> {
    svc.mpv_kill();
    Ok(json!({"status": "killed"}))
}

#[tauri::command]
pub async fn diffview_launch(svc: Svc<'_>) -> Result<Value, String> {
    let status = blocking(&svc, |s| s.diffview_launch()).await??;
    Ok(json!({"status": status}))
}

#[tauri::command]
pub async fn diffview_seek(frame: i64, svc: Svc<'_>) -> Result<Value, String> {
    Ok(json!({"ok": svc.diffview_seek(frame)?}))
}

#[tauri::command]
pub async fn diffview_step(direction: String, svc: Svc<'_>) -> Result<Value, String> {
    Ok(json!({"ok": svc.diffview_step(&direction)?}))
}

#[tauri::command]
pub async fn diffview_pause(svc: Svc<'_>) -> Result<Value, String> {
    Ok(json!({"ok": svc.diffview_pause()?}))
}

#[tauri::command]
pub async fn diffview_kill(svc: Svc<'_>) -> Result<Value, String> {
    svc.diffview_kill();
    Ok(json!({"status": "killed"}))
}

// ─── Export ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn export_csv(path: Option<String>, svc: Svc<'_>) -> Result<Value, String> {
    svc.export_csv(path.as_deref())
}

// ─── Recent analyses ────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_recents(svc: Svc<'_>) -> Result<Vec<RecentEntry>, String> {
    Ok(svc.recents())
}

#[tauri::command]
pub async fn add_recent(entry: RecentEntry, svc: Svc<'_>) -> Result<(), String> {
    svc.add_recent(entry);
    Ok(())
}

#[tauri::command]
pub async fn clear_recents(svc: Svc<'_>) -> Result<(), String> {
    svc.clear_recents();
    Ok(())
}
