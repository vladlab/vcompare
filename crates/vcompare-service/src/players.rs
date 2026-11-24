//! The two mpv windows: the player (side-by-side, or single for levels)
//! and the difference view. Each has a poll thread that reports its
//! position and its exit.

use crate::events::{EventSink, PlayerExit, UiEvent};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vcompare_engine::mpv::{MpvController, ViewMode};
use vcompare_engine::state::EngineState;

#[derive(Clone, Copy, PartialEq)]
pub enum PlayerKind {
    Mpv,
    Diffview,
}

impl PlayerKind {
    fn label(self) -> &'static str {
        match self {
            PlayerKind::Mpv => "mpv",
            PlayerKind::Diffview => "diffview",
        }
    }

    fn position(self, frame: i64) -> UiEvent {
        match self {
            PlayerKind::Mpv => UiEvent::MpvPosition(frame),
            PlayerKind::Diffview => UiEvent::DiffviewPosition(frame),
        }
    }

    fn exited(self, error: Option<String>) -> UiEvent {
        match self {
            PlayerKind::Mpv => UiEvent::MpvExited(PlayerExit { error }),
            PlayerKind::Diffview => UiEvent::DiffviewExited(PlayerExit { error }),
        }
    }
}

pub struct Player {
    kind: PlayerKind,
    controller: Mutex<Option<Arc<MpvController>>>,
}

impl Player {
    pub fn new(kind: PlayerKind) -> Self {
        Self { kind, controller: Mutex::new(None) }
    }

    pub fn launch(&self, engine: &EngineState, sink: &Arc<dyn EventSink>) -> Result<&'static str, String> {
        let ctx = engine.mpv_context()?;
        let view_mode = match self.kind {
            PlayerKind::Diffview if ctx.dist_path.is_empty() => {
                return Err("Difference view needs two files (not available for a levels analysis)".into());
            }
            PlayerKind::Diffview => ViewMode::Difference,
            // Levels analyses have no second file: open the reference on its own.
            PlayerKind::Mpv if ctx.dist_path.is_empty() => ViewMode::Single,
            PlayerKind::Mpv => ViewMode::SideBySide,
        };

        if let Some(ref mpv) = *self.controller.lock().unwrap() {
            if mpv.is_running() {
                return Ok("already_running");
            }
        }

        let mpv = Arc::new(MpvController::new(
            ctx.ref_path,
            ctx.dist_path,
            ctx.tc_format,
            ctx.ref_file_start_tc,
            ctx.timeline,
            view_mode,
            ctx.scale_w,
            ctx.scale_h,
            ctx.scale_ref,
        ));
        if let Some(err) = mpv.launch() {
            return Err(err);
        }
        *self.controller.lock().unwrap() = Some(mpv.clone());

        // The thread holds its own Arc and exits once mpv is gone (closed by
        // the user, or killed by `kill`).
        let kind = self.kind;
        let sink = sink.clone();
        std::thread::spawn(move || {
            let mut last_frame: Option<i64> = None;
            loop {
                if !mpv.is_running() {
                    let error = mpv.exit_report();
                    eprintln!(
                        "[vcompare] {} process exited{}",
                        kind.label(),
                        error.as_deref().map(|e| format!(": {e}")).unwrap_or_default()
                    );
                    sink.send(kind.exited(error));
                    break;
                }
                if let Some(frame) = mpv.get_frame() {
                    if last_frame != Some(frame) {
                        last_frame = Some(frame);
                        sink.send(kind.position(frame));
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Ok("launched")
    }

    /// Run `f` on the live controller, or fail with "<label> not running".
    pub fn with<T>(&self, f: impl FnOnce(&MpvController) -> T) -> Result<T, String> {
        let g = self.controller.lock().unwrap();
        let mpv = g.as_ref().ok_or_else(|| format!("{} not running", self.kind.label()))?;
        Ok(f(mpv))
    }

    /// Kill the window if open (its poll thread then reports the exit).
    pub fn kill(&self) {
        if let Some(mpv) = self.controller.lock().unwrap().take() {
            mpv.kill();
        }
    }
}
