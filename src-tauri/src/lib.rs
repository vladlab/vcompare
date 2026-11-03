mod bridge;
mod engine;

use bridge::{DiffViewState, EngineState, FfmpegHandle, MpvState, RecentsState, ResolveState};
use std::env;
use tauri::{Emitter, Manager};

/// Build the native menu bar.
fn build_menu(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};

    let new_analysis = MenuItem::with_id(
        app,
        "new-analysis",
        "New Analysis",
        true,
        Some("CmdOrCtrl+N"),
    )?;
    let open_recent = MenuItem::with_id(
        app,
        "open-recent",
        "Recent Analyses…",
        true,
        Some("CmdOrCtrl+Shift+O"),
    )?;
    let export = MenuItem::with_id(
        app,
        "export-csv",
        "Export CSV…",
        true,
        Some("CmdOrCtrl+Shift+E"),
    )?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = PredefinedMenuItem::quit(app, Some("Quit"))?;

    let file_menu = Submenu::with_items(
        app,
        "File",
        true,
        &[&new_analysis, &open_recent, &export, &sep, &quit],
    )?;

    let menu = Menu::with_items(app, &[&file_menu])?;
    app.set_menu(menu)?;

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::init();

    // ── WebKitGTK workarounds ──────────────────────────────────────────────
    if env::var("WEBKIT_DISABLE_DMABUF_RENDERER").is_err() {
        let has_nvidia = std::fs::read_to_string("/proc/driver/nvidia/version").is_ok()
            || std::path::Path::new("/dev/nvidiactl").exists();
        if has_nvidia {
            env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
            log::info!("NVIDIA detected — set WEBKIT_DISABLE_DMABUF_RENDERER=1");
        }
    }

    if env::var("WEBKIT_DISABLE_COMPOSITING_MODE").is_err() {
        if env::var("WAYLAND_DISPLAY").is_ok() {
            env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
            log::info!("Wayland detected — set WEBKIT_DISABLE_COMPOSITING_MODE=1");
        }
    }

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            // ── Menu ──
            if let Err(e) = build_menu(app) {
                log::error!("Failed to build menu: {e}");
            }

            app.on_menu_event(|app_handle, event| {
                let id = event.id().as_ref();
                match id {
                    "new-analysis" | "open-recent" | "export-csv" => {
                        if let Err(e) = app_handle.emit("menu-event", id) {
                            log::error!("Failed to emit menu event {id}: {e}");
                        }
                    }
                    _ => {}
                }
            });

            // ── Engine state ──
            app.manage(EngineState::new());
            app.manage(MpvState::new());
            app.manage(DiffViewState::new());
            app.manage(FfmpegHandle::new());
            app.manage(ResolveState::new());

            // ── Recent analyses store ──
            let data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            app.manage(RecentsState::new(data_dir));

            log::info!("vcompare engine ready (native, pid={})", std::process::id());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bridge::ping,
            bridge::detect_capabilities,
            bridge::probe_file,
            bridge::check_cache,
            bridge::check_levels_cache,
            bridge::load_saved_analysis,
            bridge::analyze,
            bridge::cancel_analysis,
            bridge::get_frames,
            bridge::get_metric_series,
            bridge::detect_events,
            bridge::export_events_csv,
            bridge::export_events_edl,
            bridge::set_event_review,
            bridge::preview_frames,
            bridge::mpv_launch,
            bridge::mpv_seek,
            bridge::mpv_step,
            bridge::mpv_toggle_pause,
            bridge::mpv_kill,
            bridge::diffview_launch,
            bridge::diffview_seek,
            bridge::diffview_step,
            bridge::diffview_pause,
            bridge::diffview_kill,
            bridge::resolve_connect,
            bridge::resolve_seek,
            bridge::resolve_disconnect,
            bridge::export_csv,
            bridge::get_recents,
            bridge::add_recent,
            bridge::clear_recents,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            log::info!("App exiting — cleaning up");

            // Kill any running ffmpeg process (same path as Cancel: the
            // runners then neither retry nor save).
            if let Some(handle) = app_handle.try_state::<FfmpegHandle>() {
                handle.0.cancel();
            }

            // Kill mpv if running
            if let Some(state) = app_handle.try_state::<MpvState>() {
                if let Ok(mut ctrl) = state.controller.lock() {
                    if let Some(mpv) = ctrl.take() {
                        mpv.kill();
                    }
                }
            }

            // Close the Resolve bridge if open
            if let Some(state) = app_handle.try_state::<ResolveState>() {
                if let Some(s) = state.sync.lock().unwrap().take() {
                    s.disconnect();
                }
            }

            // Kill diffview if running
            if let Some(state) = app_handle.try_state::<DiffViewState>() {
                if let Ok(mut ctrl) = state.controller.lock() {
                    if let Some(mpv) = ctrl.take() {
                        mpv.kill();
                    }
                }
            }
        }
    });
}
