//! Things the service tells the UI while it works.

use serde_json::{json, Value};
use vcompare_engine::resolve::ResolveStatus;
use vcompare_engine::timeline_run;

/// Why an mpv window went away.
#[derive(Debug, Clone)]
pub struct PlayerExit {
    /// `None` when the user closed it; the reason when it failed.
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    /// Frames (or samples) done of total for the running job.
    Progress { current: u64, total: u64 },
    /// The running job's diagnostics so far (the whole list, not a delta).
    Diagnostics(Vec<String>),
    TimelineProgress(timeline_run::Progress),
    MpvPosition(i64),
    MpvExited(PlayerExit),
    DiffviewPosition(i64),
    DiffviewExited(PlayerExit),
    ResolveStatus(ResolveStatus),
}

impl UiEvent {
    /// Event name used by the web frontend (`rpc:*`).
    pub fn name(&self) -> &'static str {
        match self {
            UiEvent::Progress { .. } => "rpc:progress",
            UiEvent::Diagnostics(_) => "rpc:diagnostics",
            UiEvent::TimelineProgress(_) => "rpc:timeline-progress",
            UiEvent::MpvPosition(_) => "rpc:mpv-position",
            UiEvent::MpvExited(_) => "rpc:mpv-exited",
            UiEvent::DiffviewPosition(_) => "rpc:diffview-position",
            UiEvent::DiffviewExited(_) => "rpc:diffview-exited",
            UiEvent::ResolveStatus(_) => "rpc:resolve-status",
        }
    }

    /// JSON payload for the web frontend, as the Tauri app always sent it.
    pub fn payload(&self) -> Value {
        fn exit(e: &PlayerExit) -> Value {
            json!({
                "reason": if e.error.is_some() { "error" } else { "closed" },
                "error": e.error,
            })
        }
        match self {
            UiEvent::Progress { current, total } => json!({"current": current, "total": total}),
            UiEvent::Diagnostics(lines) => json!({"lines": lines}),
            UiEvent::TimelineProgress(p) => serde_json::to_value(p).unwrap_or(Value::Null),
            UiEvent::MpvPosition(f) | UiEvent::DiffviewPosition(f) => json!({"frame": f}),
            UiEvent::MpvExited(e) | UiEvent::DiffviewExited(e) => exit(e),
            UiEvent::ResolveStatus(s) => serde_json::to_value(s).unwrap_or(Value::Null),
        }
    }
}

/// Where the service sends `UiEvent`s. Called from worker threads.
pub trait EventSink: Send + Sync + 'static {
    fn send(&self, event: UiEvent);
}

/// A sink that drops everything (tests, headless use).
pub struct NullSink;

impl EventSink for NullSink {
    fn send(&self, _event: UiEvent) {}
}

impl<F: Fn(UiEvent) + Send + Sync + 'static> EventSink for F {
    fn send(&self, event: UiEvent) {
        self(event)
    }
}
