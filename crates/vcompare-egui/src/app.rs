//! The window. Screens: Pick (choose files, options) → Working (analysing
//! or opening, with progress) → Results (chart, tabs), or Error.
//!
//! Anything that can block (probing, analysing, opening, loading a
//! series, detection, thumbnails) runs on a worker thread and comes back
//! as a `Msg`. Jobs carry a token; a cancelled or superseded job's late
//! answer is dropped.

use crate::chart::{Chart, ChartProps};
use crate::data::{Loaded, Series};
use crate::dnd::{Dnd, Drop};
use crate::detector::{self as det, Action as DetAction, DetectorTab, RefView};
use crate::pick::{group, is_analysis, Pick, PickAction, Slot, VIDEO_EXTENSIONS};
use crate::progress::{Job, JobKind};
use crate::settings::Settings;
use crate::table::FrameTable;
use crate::theme;
use crate::thumbs::{ThumbLoader, QUICK, SETTLE};
use crate::timeline;
use crate::tools::{ToolsAction, ToolsWindow};
use eframe::egui::{self, Key, RichText, TextureHandle, Ui};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vcompare_service::engine::detect::{DetectResult, Detector};
use vcompare_service::engine::references::StoredReference;
use vcompare_service::engine::resolve::ResolveStatus;
use vcompare_service::engine::types::{Capabilities, ProbeResult};
use vcompare_service::engine::tools::Tool;
use vcompare_service::{AnalyzeParams, RecentEntry, Service, ToolReport, UiEvent};

enum Msg {
    ToolReport(ToolReport),
    Probed(Slot, String, Result<ProbeResult, String>),
    /// Cache check result for (mode, reference, distorted).
    Cache((String, String, String), Value),
    Finished {
        token: u64,
        result: Result<(Loaded, Series), String>,
        /// What failed, for the error screen ("Analysis failed: …").
        what: &'static str,
        perf: Option<String>,
    },
    Series(Result<Series, String>),
    /// A smoothed copy of the shown series (generation, series).
    Smoothed(u64, Series),
    /// A timeline run with nothing to chart (dry run, or nothing compared).
    TimelineOnly { token: u64, summary: Value },
    /// `timeline_tracks` (timeline mode's Connect to Resolve).
    Tracks(Result<Value, String>),
    /// mpv (false) or the difference view (true) launched, or not.
    PlayerLaunched(bool, Result<&'static str, String>),
    ResolveConnected(Result<ResolveStatus, String>),
    Detected(Detector, u64, Result<DetectResult, String>),
    Exported(Result<String, String>),
    /// An events export finished: (rows written, path) or error; EDL or CSV.
    EventsExported(Result<(u64, String), String>, bool),
    References(Vec<StoredReference>, HashMap<String, egui::ColorImage>),
    /// A reference was added (tick it), renamed or deleted.
    RefAdded(Result<StoredReference, String>),
    RefChanged(Result<(), String>),
    /// A file dialog closed (None: cancelled, or no dialog available).
    Picked(Dialog, Option<PathBuf>),
    Service(UiEvent),
}

/// What a file dialog was opened for.
#[derive(Clone, Copy)]
enum Dialog {
    Media(Slot),
    Analysis,
    ExportCsv,
    RefImage,
    ExportEvents(Detector, bool),
    Tool(Tool),
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Frames,
    Det(Detector),
    /// Timeline mode: the run report.
    Report,
}

enum Screen {
    Pick,
    Working(Job),
    Results,
    /// A timeline run with nothing to chart: its report (dry run, or
    /// every sample failed).
    TimelineReport(Value),
    Error(String),
}

pub struct VcApp {
    svc: Arc<Service>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    data_dir: PathBuf,
    screen: Screen,
    pick: Pick,
    caps: Option<Capabilities>,
    job_token: u64,
    loaded: Option<Loaded>,
    series: Option<Series>,
    table: Option<FrameTable>,
    chart: Chart,
    /// One per detector, indexed by `det::index`.
    dets: [DetectorTab; 3],
    detect_gen: [u64; 3],
    references: Vec<StoredReference>,
    ref_thumbs: HashMap<String, TextureHandle>,
    /// Reference waiting for the delete confirmation: (id, name).
    confirm_delete: Option<(String, String)>,
    thumbs: ThumbLoader,
    tab: Tab,
    cursor: i64,
    /// Chart threshold line (`t` / `T`); diff and vmaf start with one.
    threshold: Option<f64>,
    status: Option<(String, Instant, Duration, bool)>,
    ui_times: VecDeque<f32>,
    show_perf: bool,
    show_keys: bool,
    tools: ToolsWindow,
    dnd: Dnd,
    recents: Option<Vec<RecentEntry>>,
    // ── Players (mpv side-by-side / single, difference view) ──
    mpv: bool,
    diffview: bool,
    /// mpv's frame; followed by the cursor when `auto_seek` (bidirectional).
    auto_seek: bool,
    /// The cursor last sent to players / Resolve.
    synced_cursor: i64,
    /// The cursor moved because mpv did (auto-seek): don't seek mpv back.
    cursor_from_player: bool,
    /// Debounced player seek: (frame, when).
    pending_seek: Option<(i64, Instant)>,
    // ── DaVinci Resolve playhead sync ──
    resolve_on: bool,
    resolve_busy: bool,
    resolve_status: ResolveStatus,
    /// Set once the operator turns sync off, so a later timeline run
    /// doesn't switch it back on.
    resolve_declined: bool,
    // ── Smoothing (f) ──
    smooth: usize,
    smoothed: Option<Series>,
    smooth_gen: u64,
    /// A file dialog is open (they run on a worker thread so the window
    /// keeps painting); others wait.
    dialog_open: bool,
    quit: bool,
}

impl VcApp {
    pub fn new(cc: &eframe::CreationContext<'_>, open: Vec<String>) -> Self {
        theme::apply(&cc.egui_ctx);
        let (tx, rx) = channel();
        let sink = {
            let tx = tx.clone();
            let ctx = cc.egui_ctx.clone();
            move |e: UiEvent| {
                let _ = tx.send(Msg::Service(e));
                ctx.request_repaint();
            }
        };
        let data_dir = data_dir();
        let settings = Settings::load(&data_dir);
        let svc = Arc::new(Service::new(data_dir.clone(), Arc::new(sink)));
        svc.set_tool_paths(settings.tools.clone());
        let ticked = settings.match_references.clone();
        let thumbs = ThumbLoader::new(svc.clone(), cc.egui_ctx.clone());
        let mut app = Self {
            svc,
            tx,
            rx,
            data_dir,
            screen: Screen::Pick,
            pick: Pick::new(settings.mode, settings.keep_signatures, settings.last_dir),
            caps: None,
            job_token: 0,
            loaded: None,
            series: None,
            table: None,
            chart: Chart::default(),
            dets: det::ALL.map(DetectorTab::new),
            detect_gen: [0; 3],
            references: vec![],
            ref_thumbs: HashMap::new(),
            confirm_delete: None,
            thumbs,
            tab: Tab::Frames,
            cursor: -1,
            threshold: None,
            status: None,
            ui_times: VecDeque::new(),
            show_perf: false,
            show_keys: false,
            tools: ToolsWindow::new(settings.tools.clone()),
            dnd: Dnd::default(),
            recents: None,
            mpv: false,
            diffview: false,
            auto_seek: false,
            synced_cursor: -1,
            cursor_from_player: false,
            pending_seek: None,
            resolve_on: false,
            resolve_busy: false,
            resolve_status: ResolveStatus::Disconnected,
            resolve_declined: false,
            smooth: 0,
            smoothed: None,
            smooth_gen: 0,
            dialog_open: false,
            quit: false,
        };
        app.dets[det::index(Detector::Match)].matching.references = ticked;
        app.tools.checking = true;
        app.spawn(&cc.egui_ctx, |svc| Msg::ToolReport(svc.tool_report()));
        if !open.is_empty() {
            app.open_paths(&cc.egui_ctx, open);
        }
        app
    }

    fn spawn<F: FnOnce(&Service) -> Msg + Send + 'static>(&self, ctx: &egui::Context, f: F) {
        let (svc, tx, ctx) = (self.svc.clone(), self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(f(&svc));
            ctx.request_repaint();
        });
    }

    fn save_settings(&self) {
        Settings {
            mode: self.pick.mode.clone(),
            last_dir: self.pick.last_dir.clone(),
            keep_signatures: self.pick.keep_signatures,
            match_references: self.dets[det::index(Detector::Match)].matching.references.clone(),
            tools: vcompare_service::engine::tools::configured(),
        }
        .save(&self.data_dir);
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        let secs = if error { 12 } else { 6 };
        self.status = Some((text.into(), Instant::now(), Duration::from_secs(secs), error));
    }

    // ── Jobs ──

    fn begin(&mut self, kind: JobKind) -> u64 {
        self.job_token += 1;
        self.pick.recents = None; // this job joins the list
        self.screen = Screen::Working(Job::new(kind));
        self.job_token
    }

    fn analyze(&mut self, ctx: &egui::Context, params: AnalyzeParams) {
        self.save_settings();
        let mode = params.mode.clone().unwrap_or_default();
        let token = self.begin(JobKind::Analyze { mode });
        let timeline = params.mode.as_deref() == Some("timeline");
        self.spawn(ctx, move |svc| {
            let r = svc.analyze(&params);
            let perf = r.as_ref().ok().and_then(|s| s["perf"].as_str().map(String::from));
            if let Ok(summary) = &r {
                let nothing = summary["metrics_available"].as_array().is_none_or(|a| a.is_empty()) || summary["frame_count"].as_u64() == Some(0);
                if timeline && nothing {
                    return Msg::TimelineOnly { token, summary: summary.clone() };
                }
            }
            // Timeline runs say where they saved (none for a dry run).
            let path = params.log_path.clone().or_else(|| r.as_ref().ok().and_then(|s| s["save_path"].as_str().map(String::from))).unwrap_or_default();
            let result = r.and_then(|summary| Loaded::from_summary(svc, summary, &path));
            let what = if timeline { "Timeline check failed" } else { "Analysis failed" };
            Msg::Finished { token, result, what, perf }
        });
    }

    /// Open a saved analysis (from the dialog, a drop, a recent, the
    /// command line or "Load cached").
    fn open_saved(&mut self, ctx: &egui::Context, path: String, what: &'static str) {
        let token = self.begin(JobKind::Open(path.clone()));
        self.spawn(ctx, move |svc| Msg::Finished { token, result: Loaded::open(svc, &path), what, perf: None });
    }

    /// Paths from the command line: an analysis opens; media goes to the
    /// start screen's boxes.
    fn open_paths(&mut self, ctx: &egui::Context, paths: Vec<String>) {
        self.handle_drop(ctx, Drop { paths, at: None });
    }

    /// Files dropped on the window (or given on the command line). On the
    /// start screen, a single file goes to the box it was dropped on.
    fn handle_drop(&mut self, ctx: &egui::Context, drop: Drop) {
        if let Some(j) = drop.paths.iter().find(|p| is_analysis(p)) {
            if !matches!(self.screen, Screen::Working(_)) {
                self.open_saved(ctx, j.clone(), "Failed to open the analysis");
            }
            return;
        }
        if matches!(self.screen, Screen::Working(_)) {
            return;
        }
        let target = match self.screen {
            Screen::Pick => drop.at.and_then(|p| self.pick.slot_at(p)),
            _ => None,
        };
        if !matches!(self.screen, Screen::Pick) {
            self.new_analysis();
        }
        let (acts, note) = self.pick.drop_files(&drop.paths, target);
        if let Some(n) = note {
            self.say(n, false);
        }
        self.pick_actions(ctx, acts);
    }

    /// While files are dragged over a screen other than the start screen:
    /// say what a drop would do.
    fn drop_overlay(&self, ctx: &egui::Context) {
        if !self.dnd.hovering() || matches!(self.screen, Screen::Pick) {
            return;
        }
        let analysis = self.dnd.hovered.iter().any(|p| is_analysis(p));
        let text = match (&self.screen, analysis) {
            (Screen::Working(_), _) => "Busy — drop files when the analysis has finished",
            (_, true) => "Drop to open this saved analysis",
            _ => "Drop to start a new analysis with these files",
        };
        let rect = ctx.content_rect();
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop_overlay")));
        painter.rect_filled(rect, 0.0, theme::BG_BASE.gamma_multiply(0.85));
        painter.rect_stroke(rect.shrink(10.0), 10.0, egui::Stroke::new(2.0, theme::ACCENT), egui::StrokeKind::Inside);
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(20.0), theme::TEXT);
    }

    fn cancel(&mut self, ctx: &egui::Context) {
        let Screen::Working(job) = &mut self.screen else { return };
        if job.cancelling || matches!(job.kind, JobKind::Open(_)) {
            return;
        }
        job.cancelling = true;
        self.job_token += 1; // drop whatever the cancelled job returns
        self.svc.cancel_analysis();
        self.screen = Screen::Pick;
        self.say("Analysis cancelled — nothing was saved", false);
        self.check_cache(ctx);
    }

    fn finished(&mut self, ctx: &egui::Context, loaded: Loaded, series: Series, perf: Option<String>) {
        self.cursor = loaded.rows.frames.first().copied().unwrap_or(-1);
        self.table = Some(FrameTable::new(&loaded.rows));
        self.chart.reset();
        for d in &mut self.dets {
            d.reset();
        }
        self.thumbs.clear();
        self.thumbs.hold(QUICK);
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("vcompare — {}", loaded.title())));
        if !loaded.path.is_empty() && !loaded.ref_path.is_empty() {
            self.svc.add_recent(RecentEntry {
                ref_path: loaded.ref_path.clone(),
                dist_path: loaded.dist_path.clone(),
                cache_path: loaded.path.clone(),
                mode: loaded.mode.clone(),
                frame_count: loaded.rows.frames.len() as u64,
                timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            });
        }
        match perf {
            Some(p) => self.status = Some((p, Instant::now(), Duration::from_secs(20), false)),
            None => self.say(format!("{} · {} frames", loaded.title(), group(loaded.rows.frames.len() as u64)), false),
        }
        // As the web UI: diff and vmaf open with a threshold line on the
        // first metric; levels with none.
        self.threshold = match loaded.mode.as_str() {
            "diff" | "vmaf" => auto_threshold(&loaded.summary, &series.metric),
            _ => None,
        };
        let mode = loaded.mode.clone();
        let dry = loaded.summary["timeline_run"]["dry_run"].as_bool() == Some(true);
        self.dets[det::index(Detector::Flash)].timeline = mode == "timeline";
        self.loaded = Some(loaded);
        self.series = Some(series);
        self.smoothed = None;
        self.smooth_gen += 1;
        self.screen = Screen::Results;
        if self.smooth > 0 {
            self.resmooth(ctx);
        }
        // Reviewing a flagged sample means seeing it on the reference
        // monitor: follow the cursor in Resolve unless the operator turned
        // that off.
        if mode == "timeline" && !dry && !self.resolve_on && !self.resolve_busy && !self.resolve_declined {
            self.toggle_resolve(ctx);
        }
        if !self.tabs().contains(&self.tab) {
            self.tab = Tab::Frames;
        }
        if mode == "levels" {
            self.load_references(ctx);
        }
        // Every detector with a tab, so the tabs show their counts at once.
        for k in det::ALL {
            self.detect(ctx, k);
        }
    }

    /// The tabs under the chart for the loaded analysis.
    fn tabs(&self) -> Vec<Tab> {
        let mode = self.loaded.as_ref().map(|l| l.mode.as_str()).unwrap_or("");
        let mut t = vec![Tab::Frames];
        t.extend(det::ALL.iter().filter(|k| det::in_mode(**k, mode)).map(|k| Tab::Det(*k)));
        if mode == "timeline" {
            t.push(Tab::Report);
        }
        t
    }

    /// Why a detector's tab can't be used, if it can't.
    fn unavailable(&self, k: Detector) -> Option<&'static str> {
        let l = self.loaded.as_ref()?;
        (k == Detector::Match && l.summary["has_signatures"].as_bool() != Some(true))
            .then_some("This analysis has no frame signatures — re-run it with “keep frame signatures” on")
    }

    /// The detector whose events the chart marks and the event keys act on:
    /// the active tab's (none on Frames).
    fn active_detector(&self) -> Option<Detector> {
        match self.tab {
            Tab::Det(k) if self.unavailable(k).is_none() => Some(k),
            _ => None,
        }
    }

    fn select_tab(&mut self, t: Tab) {
        if self.tab != t {
            self.tab = t;
            self.thumbs.hold(QUICK);
        }
    }

    fn load_references(&mut self, ctx: &egui::Context) {
        self.spawn(ctx, |svc| {
            let list = svc.list_references();
            let mut imgs = HashMap::new();
            for r in &list {
                if let Some(img) = r.thumb.as_deref().and_then(decode_data_uri) {
                    imgs.insert(r.id.clone(), img);
                }
            }
            Msg::References(list, imgs)
        });
    }

    fn new_analysis(&mut self) {
        if matches!(self.screen, Screen::Working(_)) {
            return;
        }
        self.pick.clear_files();
        self.pick.recents = None;
        self.screen = Screen::Pick;
        self.recents = None;
        self.kill_players();
    }

    // ── Players ──

    fn kill_players(&mut self) {
        if self.mpv {
            self.svc.mpv_kill();
        }
        if self.diffview {
            self.svc.diffview_kill();
        }
        self.mpv = false;
        self.diffview = false;
        self.auto_seek = false;
        self.pending_seek = None;
    }

    fn launch_player(&mut self, ctx: &egui::Context, diff: bool) {
        if (diff && self.diffview) || (!diff && self.mpv) {
            return;
        }
        self.spawn(ctx, move |svc| Msg::PlayerLaunched(diff, if diff { svc.diffview_launch() } else { svc.mpv_launch() }));
    }

    fn kill_player(&mut self, diff: bool) {
        if diff {
            self.svc.diffview_kill();
            self.diffview = false;
        } else {
            self.svc.mpv_kill();
            self.mpv = false;
            self.auto_seek = false;
        }
    }

    /// Something every running player does: seek, step, pause.
    fn players(&self, ctx: &egui::Context, f: impl Fn(&Service, bool) + Send + Sync + 'static) {
        let (mpv, diff) = (self.mpv, self.diffview);
        if !mpv && !diff {
            return;
        }
        let (svc, ctx) = (self.svc.clone(), ctx.clone());
        std::thread::spawn(move || {
            if mpv {
                f(&svc, false);
            }
            if diff {
                f(&svc, true);
            }
            ctx.request_repaint();
        });
    }

    fn seek_players(&self, ctx: &egui::Context, frame: i64) {
        self.players(ctx, move |svc, diff| {
            let _ = if diff { svc.diffview_seek(frame) } else { svc.mpv_seek(frame) };
        });
    }

    // ── Resolve ──

    fn toggle_resolve(&mut self, ctx: &egui::Context) {
        if self.resolve_busy {
            return;
        }
        if self.resolve_on {
            self.resolve_on = false;
            self.resolve_declined = true;
            self.resolve_status = ResolveStatus::Disconnected;
            self.spawn(ctx, |svc| {
                svc.resolve_disconnect();
                Msg::ResolveConnected(Ok(ResolveStatus::Disconnected))
            });
            return;
        }
        self.resolve_busy = true;
        self.resolve_status = ResolveStatus::Connecting;
        self.spawn(ctx, |svc| Msg::ResolveConnected(svc.resolve_connect()));
    }

    fn resolve_text(&self) -> Option<(String, egui::Color32)> {
        match &self.resolve_status {
            ResolveStatus::Connecting => Some(("connecting…".into(), theme::TEXT_DIM)),
            ResolveStatus::Connected { target } => Some((target.clone(), theme::TEXT_DIM)),
            ResolveStatus::SeekFailed { message } => Some((format!("seek failed: {message}"), theme::WARNING)),
            ResolveStatus::Failed { message } => Some((format!("failed: {message}"), theme::THRESHOLD)),
            ResolveStatus::Disconnected => None,
        }
    }

    /// After a frame: send a cursor the user moved to the players
    /// (debounced 50 ms) and to Resolve (the engine sends the newest only).
    fn sync_cursor(&mut self, ctx: &egui::Context) {
        if self.cursor != self.synced_cursor {
            if !self.cursor_from_player && self.cursor >= 0 && matches!(self.screen, Screen::Results) {
                if self.mpv || self.diffview {
                    self.pending_seek = Some((self.cursor, Instant::now() + Duration::from_millis(50)));
                }
                if self.resolve_on {
                    let f = self.cursor;
                    let svc = self.svc.clone();
                    std::thread::spawn(move || {
                        let _ = svc.resolve_seek(f);
                    });
                }
            }
            self.synced_cursor = self.cursor;
            self.cursor_from_player = false;
        }
        if let Some((f, at)) = self.pending_seek {
            let now = Instant::now();
            if now >= at {
                self.pending_seek = None;
                self.seek_players(ctx, f);
            } else {
                ctx.request_repaint_after(at - now);
            }
        }
    }

    // ── Smoothing ──

    const SMOOTH: [usize; 5] = [0, 5, 11, 21, 51];

    fn resmooth(&mut self, ctx: &egui::Context) {
        self.smooth_gen += 1;
        self.smoothed = None;
        let w = Self::SMOOTH[self.smooth];
        let Some(s) = self.series.as_ref() else { return };
        if w <= 1 {
            return;
        }
        let g = self.smooth_gen;
        let (frames, values, metric) = (s.frames.clone(), s.values.clone(), s.metric.clone());
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let raw = Series::from_arrays(metric, frames, values);
            let _ = tx.send(Msg::Smoothed(g, raw.smoothed(w)));
            ctx.request_repaint();
        });
    }

    // ── Pick screen plumbing ──

    fn pick_actions(&mut self, ctx: &egui::Context, acts: Vec<PickAction>) {
        for a in acts {
            match a {
                PickAction::Choose(slot) => {
                    let title = match (slot, self.pick.mode.as_str()) {
                        (Slot::Distorted, _) => "Select distorted video",
                        (_, "levels") => "Select video",
                        _ => "Select reference video",
                    };
                    let mut d = rfd::FileDialog::new()
                        .set_title(title)
                        .add_filter("Video Files", VIDEO_EXTENSIONS)
                        .add_filter("All Files", &["*"]);
                    if let Some(dir) = &self.pick.last_dir {
                        d = d.set_directory(dir);
                    }
                    self.dialog(ctx, Dialog::Media(slot), move || d.pick_file());
                }
                PickAction::Probe(slot, path) => {
                    self.spawn(ctx, move |svc| {
                        let r = svc.probe_file(&path);
                        Msg::Probed(slot, path, r)
                    });
                }
                PickAction::CheckCache => self.check_cache(ctx),
                PickAction::Analyze(p) => self.analyze(ctx, p),
                PickAction::LoadCached(p) => self.open_saved(ctx, p, "Failed to load cached analysis"),
                PickAction::ModeChanged => self.save_settings(),
                PickAction::TimelineConnect => self.spawn(ctx, |svc| Msg::Tracks(svc.timeline_tracks())),
                PickAction::OpenTools => self.open_tools(ctx),
                PickAction::OpenRecent(p) => self.open_saved(ctx, p, "Failed to load recent analysis"),
                PickAction::AllRecents => self.show_recents(),
            }
        }
    }

    fn cache_key(&self) -> (String, String, String) {
        (self.pick.mode.clone(), self.pick.ref_path.clone(), self.pick.dist_path.clone())
    }

    fn check_cache(&mut self, ctx: &egui::Context) {
        self.pick.cache = None;
        let key = self.cache_key();
        let (mode, r, d) = key.clone();
        if r.is_empty() || mode == "timeline" || (mode != "levels" && d.is_empty()) {
            return;
        }
        self.spawn(ctx, move |svc| {
            let v = if mode == "levels" { svc.check_levels_cache(&r) } else { svc.check_cache(&r, &d, Some(&mode)) };
            Msg::Cache(key, v)
        });
    }

    // ── Results helpers ──

    fn set_metric(&mut self, ctx: &egui::Context, metric: String) {
        if self.series.as_ref().is_some_and(|s| s.metric == metric) {
            return;
        }
        self.spawn(ctx, move |svc| Msg::Series(Series::load(svc, &metric)));
    }

    fn detect(&mut self, ctx: &egui::Context, k: Detector) {
        let i = det::index(k);
        self.detect_gen[i] += 1;
        let g = self.detect_gen[i];
        let in_mode = self.loaded.as_ref().is_some_and(|l| det::in_mode(k, &l.mode));
        if !in_mode || self.unavailable(k).is_some() {
            self.dets[i].result = None;
            self.dets[i].error = None;
            return;
        }
        match self.dets[i].request() {
            Some(req) => self.spawn(ctx, move |svc| Msg::Detected(k, g, svc.detect_events(&req))),
            None => {
                self.dets[i].result = Some(self.dets[i].empty_result());
                self.dets[i].error = None;
            }
        }
    }

    fn export_csv(&mut self, ctx: &egui::Context) {
        if self.loaded.is_none() {
            return;
        }
        let mut d = rfd::FileDialog::new().set_title("Export CSV").set_file_name("vcompare_export.csv").add_filter("CSV", &["csv"]);
        if let Some(dir) = self.loaded.as_ref().and_then(|l| Path::new(&l.ref_path).parent().map(Path::to_path_buf)) {
            d = d.set_directory(dir);
        }
        self.dialog(ctx, Dialog::ExportCsv, move || d.save_file());
    }

    /// Run a file dialog on a worker thread; the answer comes back as
    /// `Msg::Picked`.
    fn dialog(&mut self, ctx: &egui::Context, what: Dialog, run: impl FnOnce() -> Option<PathBuf> + Send + 'static) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Picked(what, run()));
            ctx.request_repaint();
        });
    }

    fn receive(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::ToolReport(r) => {
                    self.tools.checking = false;
                    self.caps = Some(r.capabilities.clone());
                    self.tools.report = Some(r);
                }
                Msg::Probed(slot, path, r) => {
                    self.pick.probing = self.pick.probing.saturating_sub(1);
                    let current = match slot {
                        Slot::Reference => &self.pick.ref_path,
                        Slot::Distorted => &self.pick.dist_path,
                    };
                    if *current != path {
                        continue; // replaced since
                    }
                    match r {
                        Ok(p) => match slot {
                            Slot::Reference => self.pick.ref_probe = Some(p),
                            Slot::Distorted => self.pick.dist_probe = Some(p),
                        },
                        Err(e) => self.pick.probe_failed(slot, e),
                    }
                    self.check_cache(ctx);
                }
                Msg::Cache(key, v) => {
                    if key == self.cache_key() {
                        self.pick.cache = Some(v);
                    }
                }
                Msg::Finished { token, result, what, perf } => {
                    if token != self.job_token {
                        continue;
                    }
                    match result {
                        Ok((l, s)) => self.finished(ctx, l, s, perf),
                        Err(e) => self.screen = Screen::Error(format!("{what}: {e}")),
                    }
                }
                Msg::Series(Ok(s)) => {
                    self.series = Some(s);
                    self.resmooth(ctx);
                }
                Msg::Smoothed(g, s) => {
                    if g == self.smooth_gen {
                        self.smoothed = Some(s);
                    }
                }
                Msg::TimelineOnly { token, summary } => {
                    if token == self.job_token {
                        self.screen = Screen::TimelineReport(summary);
                    }
                }
                Msg::Tracks(r) => self.pick.timeline.connected(r),
                Msg::PlayerLaunched(diff, r) => match r {
                    Ok(_) => {
                        if diff {
                            self.diffview = true;
                        } else {
                            self.mpv = true;
                        }
                        // Start where the cursor is.
                        if self.cursor >= 0 {
                            let f = self.cursor;
                            self.spawn(ctx, move |svc| {
                                let _ = if diff { svc.diffview_seek(f) } else { svc.mpv_seek(f) };
                                Msg::Service(UiEvent::Diagnostics(vec![]))
                            });
                        }
                    }
                    Err(e) => self.say(if diff { format!("difference view: {e}") } else { format!("mpv: {e}") }, true),
                },
                Msg::ResolveConnected(r) => {
                    self.resolve_busy = false;
                    match r {
                        Ok(st) => {
                            self.resolve_on = matches!(st, ResolveStatus::Connected { .. });
                            self.resolve_status = st;
                            if self.resolve_on && self.cursor >= 0 {
                                let (f, svc) = (self.cursor, self.svc.clone());
                                std::thread::spawn(move || {
                                    let _ = svc.resolve_seek(f);
                                });
                            }
                        }
                        Err(e) => {
                            self.resolve_on = false;
                            self.resolve_status = ResolveStatus::Failed { message: e.clone() };
                            self.status = Some((format!("Resolve: {e}"), Instant::now(), Duration::from_secs(12), true));
                        }
                    }
                }
                Msg::Series(Err(e)) => self.say(e, true),
                Msg::Detected(k, g, r) if g == self.detect_gen[det::index(k)] => {
                    let d = &mut self.dets[det::index(k)];
                    match r {
                        Ok(r) => {
                            d.error = None;
                            d.result = Some(r);
                        }
                        Err(e) => {
                            d.result = None;
                            d.error = Some(e);
                        }
                    }
                }
                Msg::Detected(..) => {}
                Msg::EventsExported(Ok((rows, p)), edl) => {
                    let msg = if edl {
                        format!("{rows} markers written — in Resolve: Timeline › Import › Timeline Markers from EDL")
                    } else {
                        format!("{rows} events written to {p}")
                    };
                    self.status = Some((msg, Instant::now(), Duration::from_secs(10), false));
                }
                Msg::EventsExported(Err(e), _) => self.say(format!("Export failed: {e}"), true),
                Msg::References(list, imgs) => {
                    self.ref_thumbs = imgs
                        .into_iter()
                        .map(|(id, img)| {
                            let t = ctx.load_texture(format!("ref-{id}"), img, egui::TextureOptions::LINEAR);
                            (id, t)
                        })
                        .collect();
                    // Drop ticks for references that no longer exist.
                    let ids: Vec<String> = list.iter().map(|r| r.id.clone()).collect();
                    let m = &mut self.dets[det::index(Detector::Match)];
                    let before = m.matching.references.len();
                    m.matching.references.retain(|id| ids.contains(id));
                    let dropped = m.matching.references.len() != before;
                    self.references = list;
                    if dropped {
                        self.save_settings();
                        self.detect(ctx, Detector::Match);
                    }
                }
                Msg::RefAdded(Ok(r)) => {
                    let m = &mut self.dets[det::index(Detector::Match)];
                    if !m.matching.references.contains(&r.id) {
                        m.matching.references.push(r.id.clone());
                    }
                    self.say(format!("Reference “{}” added{}", r.name, if r.flat { " (flat colour: matched by colour)" } else { "" }), false);
                    self.save_settings();
                    self.load_references(ctx);
                    self.detect(ctx, Detector::Match);
                }
                Msg::RefAdded(Err(e)) | Msg::RefChanged(Err(e)) => self.say(format!("Reference: {e}"), true),
                Msg::RefChanged(Ok(())) => {
                    self.load_references(ctx);
                    self.detect(ctx, Detector::Match);
                }
                Msg::Picked(what, path) => {
                    self.dialog_open = false;
                    let Some(p) = path.map(|p| p.to_string_lossy().to_string()) else { continue };
                    match what {
                        Dialog::Media(slot) => {
                            if matches!(self.screen, Screen::Pick) {
                                let a = self.pick.set(slot, p);
                                self.pick_actions(ctx, vec![a]);
                            }
                        }
                        Dialog::Analysis => {
                            if !matches!(self.screen, Screen::Working(_)) {
                                self.open_saved(ctx, p, "Failed to open the analysis");
                            }
                        }
                        Dialog::ExportCsv => self.spawn(ctx, move |svc| Msg::Exported(svc.export_csv(Some(&p)).map(|_| p))),
                        Dialog::RefImage => {
                            if let Some(d) = Path::new(&p).parent() {
                                self.pick.last_dir = Some(d.to_path_buf());
                            }
                            self.say("Adding reference…", false);
                            self.spawn(ctx, move |svc| Msg::RefAdded(svc.add_reference_from_image(&p, None)));
                        }
                        Dialog::Tool(t) => self.tools.set(t, p),
                        Dialog::ExportEvents(k, edl) => self.spawn(ctx, move |svc| {
                            let r = if edl { svc.export_events_edl(Some(k), Some(&p)) } else { svc.export_events_csv(Some(k), Some(&p)) };
                            Msg::EventsExported(r.map(|v| (v["rows"].as_u64().unwrap_or(0), p)), edl)
                        }),
                    }
                }
                Msg::Exported(Ok(p)) => self.say(format!("Exported {p}"), false),
                Msg::Exported(Err(e)) => self.say(format!("Export failed: {e}"), true),
                Msg::Service(e) => match e {
                    UiEvent::Progress { current, total } => {
                        if let Screen::Working(job) = &mut self.screen {
                            job.progress(current, total);
                        }
                    }
                    UiEvent::Diagnostics(lines) => {
                        if let Screen::Working(job) = &mut self.screen {
                            if !lines.is_empty() {
                                job.diagnostics = lines;
                            }
                        }
                    }
                    UiEvent::TimelineProgress(p) => {
                        if let Screen::Working(job) = &mut self.screen {
                            job.timeline = Some(p);
                        }
                    }
                    UiEvent::MpvPosition(f) => {
                        if self.auto_seek && self.mpv && matches!(self.screen, Screen::Results) && f != self.cursor {
                            self.cursor = f;
                            self.cursor_from_player = true;
                        }
                    }
                    UiEvent::DiffviewPosition(_) => {}
                    UiEvent::MpvExited(x) => {
                        self.mpv = false;
                        self.auto_seek = false;
                        if let Some(e) = x.error {
                            self.status = Some((format!("mpv: {e}"), Instant::now(), Duration::from_secs(15), true));
                        }
                    }
                    UiEvent::DiffviewExited(x) => {
                        self.diffview = false;
                        if let Some(e) = x.error {
                            self.status = Some((format!("difference view — {e}"), Instant::now(), Duration::from_secs(15), true));
                        }
                    }
                    UiEvent::ResolveStatus(st) => {
                        if let ResolveStatus::Failed { message } = &st {
                            self.resolve_on = false;
                            self.status = Some((format!("Resolve: {message}"), Instant::now(), Duration::from_secs(12), true));
                        }
                        self.resolve_status = st;
                    }
                },
            }
        }
    }

    // ── Keys ──

    fn keys(&mut self, ctx: &egui::Context) {
        let presses: Vec<(Key, egui::Modifiers)> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } => Some((*key, *modifiers)),
                    egui::Event::Key { key, pressed: true, modifiers, .. }
                        if matches!(key, Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp | Key::ArrowDown | Key::Period | Key::Comma) =>
                    {
                        Some((*key, *modifiers))
                    }
                    _ => None,
                })
                .collect()
        });
        let typing = ctx.egui_wants_keyboard_input();
        for (key, m) in presses {
            // Menu accelerators work everywhere.
            if m.command {
                match (key, m.shift) {
                    (Key::N, false) => self.new_analysis(),
                    (Key::O, false) => self.pick_analysis(ctx),
                    (Key::O, true) => self.show_recents(),
                    (Key::E, true) => self.export_csv(ctx),
                    (Key::Q, false) => self.quit = true,
                    _ => {}
                }
                continue;
            }
            if key == Key::F1 {
                self.show_keys = !self.show_keys;
                continue;
            }
            if key == Key::Escape {
                if self.show_keys {
                    self.show_keys = false;
                } else if self.confirm_delete.is_some() {
                    self.confirm_delete = None;
                } else if self.recents.is_some() {
                    self.recents = None;
                } else {
                    self.cancel(ctx);
                }
                continue;
            }
            if typing || m.alt || !matches!(self.screen, Screen::Results) {
                continue;
            }
            let (Some(loaded), Some(series)) = (self.loaded.as_ref(), self.series.as_ref()) else { continue };
            let tl = loaded.timeline.as_ref();
            match key {
                Key::Equals | Key::Plus => self.chart.zoom_keys(series, tl, self.cursor, 0.5),
                Key::Minus => self.chart.zoom_keys(series, tl, self.cursor, 2.0),
                Key::Num0 => self.chart.reset(),
                // With a player open, . and , step the players (and auto-seek
                // brings the cursor along); otherwise they step the cursor.
                Key::Period | Key::Comma if self.mpv || self.diffview => {
                    let fwd = key == Key::Period;
                    self.players(ctx, move |svc, diff| {
                        let d = if fwd { "forward" } else { "backward" };
                        let _ = if diff { svc.diffview_step(d) } else { svc.mpv_step(d) };
                    });
                }
                Key::Period | Key::ArrowRight => self.step(1),
                Key::Comma | Key::ArrowLeft => self.step(-1),
                Key::P if m.shift => self.kill_player(false),
                Key::P => self.launch_player(ctx, false),
                Key::D if m.shift => self.kill_player(true),
                Key::D => self.launch_player(ctx, true),
                Key::S => {
                    if self.cursor >= 0 {
                        self.seek_players(ctx, self.cursor);
                    }
                }
                Key::A => self.auto_seek = !self.auto_seek,
                Key::R => self.toggle_resolve(ctx),
                Key::Space => self.players(ctx, |svc, diff| {
                    let _ = if diff { svc.diffview_pause() } else { svc.mpv_toggle_pause() };
                }),
                Key::F => {
                    self.smooth = (self.smooth + 1) % Self::SMOOTH.len();
                    self.resmooth(ctx);
                    let w = Self::SMOOTH[self.smooth];
                    self.say(if w == 0 { "Smoothing off".to_string() } else { format!("Smoothing: running median of {w} frames") }, false);
                }
                Key::M => {
                    let ms = &loaded.metrics;
                    if let Some(i) = ms.iter().position(|x| *x == series.metric) {
                        let n = ms.len();
                        let next = ms[if m.shift { (i + n - 1) % n } else { (i + 1) % n }].clone();
                        self.set_metric(ctx, next);
                    }
                }
                Key::Num1 | Key::Num2 | Key::Num3 | Key::Num4 | Key::Num5 => {
                    let n = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5].iter().position(|k| *k == key).unwrap();
                    if let Some(t) = self.tabs().get(n).copied() {
                        self.select_tab(t);
                    }
                }
                Key::N => self.next_event(if m.shift { -1 } else { 1 }),
                Key::ArrowDown | Key::ArrowUp => {
                    let d = if key == Key::ArrowDown { 1 } else { -1 };
                    match self.tab {
                        Tab::Det(_) => self.next_event(d),
                        Tab::Frames => self.step(d as i64),
                        Tab::Report => {}
                    }
                }
                Key::T if m.shift => self.threshold = None,
                Key::T => self.threshold = auto_threshold(&loaded.summary, &series.metric),
                Key::Y => self.review_selected("accept"),
                Key::X => self.review_selected("reject"),
                Key::E => self.export_csv(ctx),
                Key::F12 => self.show_perf = !self.show_perf,
                _ => {}
            }
        }
    }

    fn step(&mut self, d: i64) {
        let (Some(l), Some(t)) = (&self.loaded, &self.table) else { return };
        let next = match self.tab {
            Tab::Frames => t.step(&l.rows, self.cursor, d),
            Tab::Det(_) | Tab::Report => l.rows.index_of(self.cursor).and_then(|i| {
                let j = (i as i64 + d).clamp(0, l.rows.frames.len() as i64 - 1) as usize;
                l.rows.frames.get(j).copied()
            }),
        };
        if let Some(f) = next {
            self.cursor = f;
        }
    }

    /// n / N / ↓ / ↑ on a detector tab (nothing on Frames).
    fn next_event(&mut self, dir: i32) {
        let Some(k) = self.active_detector() else { return };
        if let Some(f) = self.dets[det::index(k)].neighbour(self.cursor, dir) {
            self.cursor = f;
        }
    }

    /// y / x on the active tab's picked event.
    fn review_selected(&mut self, verdict: &'static str) {
        let Some(k) = self.active_detector() else { return };
        let d = &self.dets[det::index(k)];
        let Some(f) = d.selected else { return };
        let Some(cur) = d.review_of(f) else { return };
        let v = if cur.as_deref() == Some(verdict) { None } else { Some(verdict) };
        self.review(k, f, v);
    }

    fn review(&mut self, k: Detector, frame: i64, v: Option<&'static str>) {
        match self.svc.set_event_review(k, frame, v) {
            Ok(r) => {
                self.dets[det::index(k)].set_review(frame, v);
                if r["saved"].as_bool() == Some(false) {
                    self.say("Verdict kept for this session only (analysis has no save path)", false);
                }
            }
            Err(e) => self.say(format!("Review: {e}"), true),
        }
    }

    fn export_events(&mut self, ctx: &egui::Context, k: Detector, edl: bool) {
        let stem = self
            .loaded
            .as_ref()
            .and_then(|l| Path::new(&l.ref_path).file_stem().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_else(|| "vcompare".into());
        let (what, short) = match k {
            Detector::Flash => ("flash frames", "flash"),
            Detector::Repeat => ("repeated frames", "repeat"),
            Detector::Match => ("image matches", "match"),
        };
        let (title, name, filter) = if edl {
            (format!("Export {what} as Resolve marker EDL"), format!("{stem}_{short}_markers.edl"), "edl")
        } else {
            (format!("Export {what} CSV"), format!("{stem}_{short}_frames.csv"), "csv")
        };
        let mut d = rfd::FileDialog::new().set_title(title).set_file_name(name).add_filter(filter.to_uppercase(), &[filter]);
        if let Some(dir) = self.loaded.as_ref().and_then(|l| Path::new(&l.ref_path).parent().map(Path::to_path_buf)) {
            d = d.set_directory(dir);
        }
        self.dialog(ctx, Dialog::ExportEvents(k, edl), move || d.save_file());
    }

    fn add_reference_image(&mut self, ctx: &egui::Context) {
        let mut d = rfd::FileDialog::new()
            .set_title("Reference picture (e.g. a still of the offline card)")
            .add_filter("Images", &["png", "jpg", "jpeg", "tif", "tiff", "dpx", "exr", "bmp", "webp"])
            .add_filter("All Files", &["*"]);
        if let Some(dir) = &self.pick.last_dir {
            d = d.set_directory(dir);
        }
        self.dialog(ctx, Dialog::RefImage, move || d.pick_file());
    }

    fn confirm_delete_window(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.confirm_delete.clone() else { return };
        let mut answer = None;
        let m = egui::Modal::new(egui::Id::unique("confirm_delete_ref")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(RichText::new(format!("Delete the reference “{name}”?")).strong());
            ui.label(RichText::new("It is removed for every analysis.").color(theme::TEXT_DIM));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Delete").color(theme::THRESHOLD)).clicked() {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
        if m.should_close() && answer.is_none() {
            answer = Some(false);
        }
        match answer {
            Some(true) => {
                self.confirm_delete = None;
                self.spawn(ctx, move |svc| Msg::RefChanged(svc.delete_reference(&id)));
            }
            Some(false) => self.confirm_delete = None,
            None => {}
        }
    }

    fn pick_analysis(&mut self, ctx: &egui::Context) {
        if matches!(self.screen, Screen::Working(_)) {
            return;
        }
        let mut d = rfd::FileDialog::new().add_filter("vcompare analysis", &["json"]).set_title("Open analysis");
        let dir = self
            .loaded
            .as_ref()
            .and_then(|l| Path::new(&l.path).parent().map(Path::to_path_buf))
            .or_else(|| self.pick.last_dir.clone());
        if let Some(dir) = dir {
            d = d.set_directory(dir);
        }
        self.dialog(ctx, Dialog::Analysis, move || d.pick_file());
    }

    fn open_tools(&mut self, ctx: &egui::Context) {
        self.tools.open = true;
        self.check_tools(ctx);
    }

    fn check_tools(&mut self, ctx: &egui::Context) {
        if !self.tools.checking {
            self.tools.checking = true;
            self.spawn(ctx, |svc| Msg::ToolReport(svc.tool_report()));
        }
    }

    /// What's missing for the chosen mode, from the last tool check.
    fn tool_warning(&self) -> Option<String> {
        let r = self.tools.report.as_ref()?;
        let broken = |t: Tool| r.tools.iter().find(|s| s.tool == t).is_some_and(|s| s.version.is_err());
        let missing: Vec<&str> = [Tool::Ffmpeg, Tool::Ffprobe].into_iter().filter(|&t| broken(t)).map(Tool::name).collect();
        if !missing.is_empty() {
            return Some(format!("{} not found or won't run, so nothing can be analysed.", missing.join(" and ")));
        }
        let c = &r.capabilities;
        if self.pick.mode == "vmaf" && !c.libvmaf && !c.vmaf_cuda {
            return Some("This ffmpeg has no libvmaf filter, so VMAF won't run. Pixel Diff and Levels are fine.".into());
        }
        None
    }

    fn tools_window(&mut self, ctx: &egui::Context) {
        if !self.tools.open {
            return;
        }
        for a in self.tools.show(ctx) {
            match a {
                ToolsAction::Browse(t) => {
                    let mut d = rfd::FileDialog::new().set_title(format!("Choose {}", t.name()));
                    let current = vcompare_service::engine::tools::locate(t);
                    if let Some(dir) = current.as_deref().and_then(Path::parent) {
                        d = d.set_directory(dir);
                    }
                    self.dialog(ctx, Dialog::Tool(t), move || d.pick_file());
                }
                ToolsAction::Apply(paths) => {
                    self.svc.set_tool_paths(paths.clone());
                    self.tools.applied(paths);
                    self.save_settings();
                    self.check_tools(ctx);
                }
                ToolsAction::Recheck => self.check_tools(ctx),
            }
        }
    }

    fn show_recents(&mut self) {
        if !matches!(self.screen, Screen::Working(_)) {
            self.recents = Some(self.svc.recents());
        }
    }

    // ── Chrome ──

    fn menu(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let working = matches!(self.screen, Screen::Working(_));
        let results = matches!(self.screen, Screen::Results);
        let sc = |k: &str| egui::RichText::new(if cfg!(target_os = "macos") { k.replace("Ctrl+", "Cmd+") } else { k.to_string() });
        ui.menu_button("File", |ui| {
            if ui.add_enabled(!working, egui::Button::new("New Analysis").shortcut_text(sc("Ctrl+N"))).clicked() {
                self.new_analysis();
            }
            if ui.add_enabled(!working, egui::Button::new("Open Analysis…").shortcut_text(sc("Ctrl+O"))).clicked() {
                self.pick_analysis(&ctx);
            }
            if ui.add_enabled(!working, egui::Button::new("Recent Analyses…").shortcut_text(sc("Ctrl+Shift+O"))).clicked() {
                self.show_recents();
            }
            ui.separator();
            if ui.add_enabled(results, egui::Button::new("Export CSV…").shortcut_text(sc("Ctrl+Shift+E"))).clicked() {
                self.export_csv(&ctx);
            }
            ui.separator();
            if ui.button("Tools…").on_hover_text("Which ffmpeg, ffprobe and mpv to use").clicked() {
                self.open_tools(&ctx);
            }
            ui.separator();
            if ui.add(egui::Button::new("Quit").shortcut_text(sc("Ctrl+Q"))).clicked() {
                self.quit = true;
            }
        });
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            self.menu(ui);
            let mut info = None;
            if let (Screen::Results, Some(l)) = (&self.screen, &self.loaded) {
                ui.separator();
                let current = self.series.as_ref().map(|s| s.metric.clone()).unwrap_or_default();
                let mut pick = current.clone();
                egui::ComboBox::from_id_salt("metric")
                    .selected_text(theme::metric_label(&current))
                    .width(170.0)
                    .show_ui(ui, |ui| {
                        for m in &l.metrics {
                            ui.selectable_value(&mut pick, m.clone(), theme::metric_label(m));
                        }
                    })
                    .response
                    .on_hover_text("Metric (m / M)");
                if pick != current {
                    let (svc, tx, c2) = (self.svc.clone(), self.tx.clone(), ctx.clone());
                    std::thread::spawn(move || {
                        let _ = tx.send(Msg::Series(Series::load(&svc, &pick)));
                        c2.request_repaint();
                    });
                }
                if let Some(s) = &self.series {
                    let tl = l.timeline.as_ref();
                    if ui.small_button("−").on_hover_text("Zoom out (-)").clicked() {
                        self.chart.zoom_keys(s, tl, self.cursor, 2.0);
                    }
                    if ui.small_button("+").on_hover_text("Zoom in (=)").clicked() {
                        self.chart.zoom_keys(s, tl, self.cursor, 0.5);
                    }
                    if ui.small_button("fit").on_hover_text("Show all (0, or double-click the chart)").clicked() {
                        self.chart.reset();
                    }
                }
                ui.separator();
                info = Some((l.title(), summary_line(l, self.series.as_ref().map(|s| s.metric.as_str())), l.path.clone()));
            }
            // Right side first, so the summary gets what's left and
            // truncates instead of pushing anything off the window.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Keys").on_hover_text("Keyboard and mouse shortcuts (F1)").clicked() {
                    self.show_keys = !self.show_keys;
                }
                if matches!(self.screen, Screen::Results) {
                    let label = if self.resolve_on { "Resolve sync on" } else { "Resolve sync off" };
                    let mut b = egui::Button::new(RichText::new(label).size(12.0)).selected(self.resolve_on);
                    if matches!(self.resolve_status, ResolveStatus::SeekFailed { .. }) {
                        b = b.stroke(egui::Stroke::new(1.0, theme::WARNING));
                    }
                    if ui
                        .add_enabled(!self.resolve_busy, b)
                        .on_hover_text(
                            "Follow the cursor in DaVinci Resolve's open timeline (r). Needs Resolve running with external scripting enabled: \
                             Preferences › System › General › External scripting using: Local.",
                        )
                        .clicked()
                    {
                        self.toggle_resolve(&ctx);
                    }
                    if let Some((t, c)) = self.resolve_text() {
                        ui.add(egui::Label::new(RichText::new(t).monospace().size(11.0).color(c)).truncate());
                    }
                    if self.diffview {
                        ui.label(RichText::new("diff view").size(11.0).color(theme::ACCENT));
                    }
                    if self.mpv {
                        ui.label(RichText::new(format!("mpv · sync={}", if self.auto_seek { "bidi" } else { "fwd" })).size(11.0).color(theme::ACCENT));
                    }
                }
                if let Some((title, line, path)) = info {
                    ui.add(egui::Label::new(RichText::new(line).monospace().size(11.5).color(theme::TEXT_DIM)).truncate());
                    ui.add(egui::Label::new(RichText::new(title).strong()).truncate()).on_hover_text(path);
                }
            });
        });
    }

    fn keys_window(&mut self, ctx: &egui::Context) {
        let rows: &[(&str, &str)] = &[
            ("= / -", "zoom in / out on the playhead"),
            ("0", "show all"),
            (". ,   → ←", "step a frame"),
            ("n N   ↓ ↑", "next / previous event (on a detector tab)"),
            ("y  x", "accept / reject the picked event"),
            ("m  M", "next / previous metric"),
            ("t  T", "threshold line from the metric (mean ± σ) / clear it"),
            ("1 – 4", "tabs under the chart"),
            ("f", "smoothing: off / 5 / 11 / 21 / 51 frames (running median)"),
            ("e", "export CSV"),
            ("p  P", "open / close mpv (side by side; the file alone for levels)"),
            ("d  D", "open / close the difference view"),
            ("s", "seek the players to the cursor"),
            ("a", "auto-seek: the cursor follows mpv too (bidirectional)"),
            ("space", "pause / play the players"),
            (". ,", "with a player open: step the players"),
            ("r", "DaVinci Resolve playhead sync on / off"),
            ("Esc", "cancel the running analysis (nothing is saved)"),
            ("Ctrl+N", "new analysis"),
            ("Ctrl+O", "open an analysis (or drop a file on the window)"),
            ("Ctrl+Shift+O", "recent analyses"),
            ("F1", "this panel"),
            ("F12", "frame timings in the status line"),
        ];
        let mouse: &[(&str, &str)] = &[
            ("wheel", "zoom on the mouse"),
            ("drag", "scrub"),
            ("double-click", "show all"),
            ("minimap drag", "on the window: pan; elsewhere: zoom to that range"),
        ];
        egui::Window::new("Keys").open(&mut self.show_keys).collapsible(false).resizable(false).show(ctx, |ui| {
            egui::Grid::new("keys_grid").num_columns(2).spacing([18.0, 6.0]).show(ui, |ui| {
                for (k, what) in rows {
                    ui.label(RichText::new(*k).monospace());
                    ui.label(*what);
                    ui.end_row();
                }
                ui.label(RichText::new("Chart").strong());
                ui.end_row();
                for (k, what) in mouse {
                    ui.label(RichText::new(*k).monospace());
                    ui.label(*what);
                    ui.end_row();
                }
            });
        });
    }

    fn recents_window(&mut self, ctx: &egui::Context) {
        let Some(entries) = self.recents.clone() else { return };
        let mut open = None;
        let mut clear = false;
        let modal = egui::Modal::new(egui::Id::unique("recents")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Recent Analyses").size(16.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        self.recents = None;
                    }
                    if !entries.is_empty() && ui.button("Clear").on_hover_text("Forget the list (the analyses stay on disk)").clicked() {
                        clear = true;
                    }
                });
            });
            ui.separator();
            if entries.is_empty() {
                ui.label(RichText::new("No recent analyses").color(theme::TEXT_MUTED));
                return;
            }
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                for e in &entries {
                    let name = |p: &str| Path::new(p).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    let files = if e.dist_path.is_empty() { name(&e.ref_path) } else { format!("{} vs {}", name(&e.ref_path), name(&e.dist_path)) };
                    let meta = format!("{} · {} frames · {}", e.mode, group(e.frame_count), fmt_date(e.timestamp));
                    let missing = !Path::new(&e.cache_path).is_file();
                    let r = ui
                        .add_enabled(
                            !missing,
                            egui::Button::new(
                                egui::text::LayoutJob::simple_singleline(files, egui::FontId::monospace(12.5), theme::TEXT),
                            )
                            .right_text(RichText::new(meta).size(11.5).color(theme::TEXT_DIM))
                            .min_size(egui::vec2(ui.available_width(), 30.0)),
                        )
                        .on_hover_text(&e.cache_path)
                        .on_disabled_hover_text(format!("{} is gone", e.cache_path));
                    if r.clicked() {
                        open = Some(e.cache_path.clone());
                    }
                }
            });
        });
        if modal.should_close() {
            self.recents = None;
        }
        if clear {
            self.svc.clear_recents();
            self.pick.recents = None;
            self.recents = Some(vec![]);
        }
        if let Some(p) = open {
            self.recents = None;
            self.open_saved(ctx, p, "Failed to load recent analysis");
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if let (Screen::Results, Some(l), Some(s)) = (&self.screen, &self.loaded, &self.series) {
                if let Some(i) = l.rows.index_of(self.cursor) {
                    ui.label(RichText::new(&l.rows.tcs[i]).monospace());
                    ui.label(RichText::new(format!("frame {}", self.cursor)).monospace().color(theme::TEXT_DIM));
                    if let Some(v) = s.value_at(self.cursor) {
                        ui.label(RichText::new(format!("{} {v:.3}", theme::metric_label(&s.metric))).monospace().color(theme::TEXT_DIM));
                    }
                    if self.smooth > 0 {
                        ui.label(RichText::new(format!("smooth {}", Self::SMOOTH[self.smooth])).monospace().color(theme::ACCENT));
                    }
                    ui.separator();
                }
            }
            if let Some(e) = self.thumbs.error.take() {
                self.say(format!("Thumbnails: {e}"), true);
            }
            // The pick screen shows its messages inline.
            let inline = matches!(self.screen, Screen::Pick);
            if let (false, Some((t, at, dur, err))) = (inline, &self.status) {
                if at.elapsed() < *dur {
                    ui.add(egui::Label::new(RichText::new(t).color(if *err { theme::THRESHOLD } else { theme::TEXT_DIM })).truncate());
                    ui.ctx().request_repaint_after(*dur - at.elapsed());
                }
            }
            if self.show_perf {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let avg = if self.ui_times.is_empty() { 0.0 } else { self.ui_times.iter().sum::<f32>() / self.ui_times.len() as f32 };
                    let max = self.ui_times.iter().cloned().fold(0.0, f32::max);
                    ui.label(
                        RichText::new(format!("ui {avg:.1} ms (max {max:.1}) · chart {:.2} ms", self.chart.last_paint.as_secs_f32() * 1000.0))
                            .monospace()
                            .size(11.0)
                            .color(theme::TEXT_MUTED),
                    )
                    .on_hover_text("CPU time per frame for the last 60 frames, and for the chart alone (F12 hides)");
                });
            }
        });
    }

    fn results(&mut self, ui: &mut Ui) {
        let (Some(_), Some(_)) = (&self.loaded, &self.series) else { return };
        let h = ui.available_height();
        let mut new_cursor = None;
        let mut det_acts = Vec::new();
        let active = self.active_detector();
        {
            let loaded = self.loaded.as_ref().unwrap();
            let series = self.smoothed.as_ref().filter(|s| Some(&s.metric) == self.series.as_ref().map(|r| &r.metric)).unwrap_or(self.series.as_ref().unwrap());
            let mut events: Vec<_> = active.map(|k| self.dets[det::index(k)].visible().into_iter().cloned().collect()).unwrap_or_default();
            events.sort_by_key(|e| e.frame);
            // Timeline mode: what the run looked at, above the chart.
            if loaded.mode == "timeline" && loaded.summary["timeline_run"].is_object() {
                egui::Panel::top("timeline_report")
                    .frame(egui::Frame::NONE.fill(theme::BG_SURFACE).inner_margin(egui::Margin::symmetric(12, 6)))
                    .show(ui, |ui| timeline::report(ui, &loaded.summary["timeline_run"], false));
            }
            // Compare modes: how the two clips sit in timecode.
            if let Some(tl) = loaded.timeline.as_ref() {
                let fps = loaded.summary["tc_format"]["fps_num"].as_f64().unwrap_or(24.0) / loaded.summary["tc_format"]["fps_den"].as_f64().unwrap_or(1.0).max(1.0);
                let svc = self.svc.clone();
                let tc = move |f: i64| svc.engine().frame_to_tc(f).unwrap_or_else(|_| format!("F{f}"));
                egui::Panel::top("timeline_bar").frame(egui::Frame::NONE.fill(theme::BG_SURFACE).inner_margin(egui::Margin::symmetric(8, 4))).show(ui, |ui| {
                    if let Some(f) = timeline::bar(ui, tl, fps, &events, self.cursor, &tc) {
                        new_cursor = Some(f);
                    }
                });
            }
            egui::Panel::top("chart")
                .resizable(true)
                .default_size((h * 0.45).max(260.0))
                .min_size(180.0)
                .frame(egui::Frame::NONE.fill(theme::BG_SURFACE).inner_margin(4.0))
                .show(ui, |ui| {
                    let props = ChartProps {
                        cursor: self.cursor,
                        events: &events,
                        threshold: self.threshold,
                        timeline: loaded.timeline.as_ref(),
                        sparse: loaded.mode == "timeline",
                        peak: loaded.summary["bit_depth"].as_u64().filter(|b| (8..=16).contains(b)).map(|b| ((1u64 << b) - 1) as f64),
                    };
                    new_cursor = self.chart.show(ui, series, &props);
                });
        }
        let tabs = self.tabs();
        egui::CentralPanel::default().show(ui, |ui| {
            let mut pick_tab = None;
            ui.horizontal(|ui| {
                for (n, t) in tabs.iter().enumerate() {
                    let (label, tip) = match t {
                        Tab::Frames => ("Frames".to_string(), None),
                        Tab::Report => ("Report".to_string(), None),
                        Tab::Det(k) => {
                            let d = &self.dets[det::index(*k)];
                            let count = d.result.as_ref().map(|_| format!(" ({})", d.visible().len())).unwrap_or_default();
                            (format!("{}{count}", det::tab_label(*k, d.timeline)), self.unavailable(*k))
                        }
                    };
                    let mut text = RichText::new(label);
                    if tip.is_some() {
                        text = text.color(theme::TEXT_MUTED);
                    }
                    let r = ui.selectable_label(self.tab == *t, text).on_hover_text(format!("{}{}", n + 1, tip.map(|t| format!(" — {t}")).unwrap_or_default()));
                    if r.clicked() {
                        pick_tab = Some(*t);
                    }
                }
            });
            if let Some(t) = pick_tab {
                self.select_tab(t);
            }
            ui.separator();
            let loaded = self.loaded.as_ref().unwrap();
            match self.tab {
                Tab::Frames => {
                    if let Some(t) = self.table.as_mut() {
                        if let Some(f) = t.show(ui, &loaded.rows, self.cursor) {
                            new_cursor = Some(f);
                        }
                    }
                }
                Tab::Report => {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        timeline::report(ui, &loaded.summary["timeline_run"], true);
                    });
                }
                Tab::Det(k) => {
                    if let Some(why) = self.unavailable(k) {
                        ui.add_space(12.0);
                        ui.label(RichText::new(why).color(theme::TEXT_DIM));
                    } else {
                        let refs = RefView { list: &self.references, thumbs: &self.ref_thumbs };
                        det_acts = self.dets[det::index(k)].show(ui, &mut self.thumbs, loaded.files, self.cursor, &refs).into_iter().map(|a| (k, a)).collect();
                    }
                }
            }
        });

        let ctx = ui.ctx().clone();
        let mut redetect = vec![];
        for (k, a) in det_acts {
            match a {
                DetAction::Select { event, frame } => {
                    self.dets[det::index(k)].pick(event);
                    new_cursor = Some(frame);
                }
                DetAction::Review(f, v) => self.review(k, f, v),
                DetAction::Adjusting => self.thumbs.hold(SETTLE),
                DetAction::Changed => {
                    if !redetect.contains(&k) {
                        redetect.push(k);
                    }
                    if k == Detector::Match {
                        self.save_settings();
                    }
                }
                DetAction::ExportEdl => self.export_events(&ctx, k, true),
                DetAction::ExportCsv => self.export_events(&ctx, k, false),
                DetAction::AddImage => self.add_reference_image(&ctx),
                DetAction::AddFrame => {
                    let f = self.cursor;
                    if f < 0 {
                        self.say("Put the cursor on the frame to use first", false);
                    } else {
                        self.spawn(&ctx, move |svc| Msg::RefAdded(svc.add_reference_from_frame(f, None)));
                    }
                }
                DetAction::Rename(id, name) => self.spawn(&ctx, move |svc| Msg::RefChanged(svc.rename_reference(&id, &name))),
                DetAction::Delete(id) => {
                    let name = self.references.iter().find(|r| r.id == id).map(|r| r.name.clone()).unwrap_or_default();
                    self.confirm_delete = Some((id, name));
                }
            }
        }
        for k in redetect {
            self.detect(&ctx, k);
        }
        if let Some(f) = new_cursor {
            self.cursor = f;
        }
    }
}

impl eframe::App for VcApp {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.dnd.take(raw);
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let t0 = Instant::now();
        let ctx = ui.ctx().clone();
        self.receive(&ctx);
        self.thumbs.poll();
        self.keys(&ctx);
        if let Some(drop) = self.dnd.update(&ctx, frame) {
            self.handle_drop(&ctx, drop);
        }
        if self.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        self.keys_window(&ctx);
        self.tools_window(&ctx);
        self.recents_window(&ctx);
        self.confirm_delete_window(&ctx);
        egui::Panel::top("top").frame(egui::Frame::side_top_panel(ui.style()).fill(theme::BG_RAISED)).show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").frame(egui::Frame::side_top_panel(ui.style()).fill(theme::BG_RAISED)).show(ui, |ui| {
            self.status_bar(ui);
        });

        match &mut self.screen {
            Screen::Results => self.results(ui),
            Screen::Working(job) => {
                let mut cancel = false;
                egui::CentralPanel::default().show(ui, |ui| cancel = job.show(ui));
                if cancel {
                    self.cancel(&ctx);
                }
            }
            Screen::TimelineReport(summary) => {
                let r = summary["timeline_run"].clone();
                let mut back = false;
                egui::CentralPanel::default().show(ui, |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        ui.add_space(30.0);
                        ui.vertical_centered(|ui| {
                            ui.set_max_width(860.0f32.min(ui.available_width() - 40.0));
                            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                let title = if r["dry_run"].as_bool() == Some(true) { "Dry run finished" } else { "Nothing was compared" };
                                ui.label(RichText::new(title).size(18.0).strong());
                                ui.add_space(8.0);
                                timeline::report(ui, &r, true);
                                ui.add_space(10.0);
                                back = ui.button("Back").clicked();
                            });
                        });
                    });
                });
                if back {
                    self.screen = Screen::Pick;
                }
            }
            Screen::Error(msg) => {
                let msg = msg.clone();
                let mut back = false;
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.set_max_width(760.0f32.min(ui.available_width() - 40.0));
                        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Error").size(18.0).strong().color(theme::THRESHOLD));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    back = ui.button("Back").clicked();
                                    if ui.button("Copy").on_hover_text("Copy the message").clicked() {
                                        ui.ctx().copy_text(msg.clone());
                                    }
                                });
                            });
                            ui.add_space(8.0);
                            // The first line says what failed; ffmpeg's output can follow.
                            let (head, rest) = msg.split_once('\n').unwrap_or((msg.as_str(), ""));
                            ui.label(RichText::new(head).strong());
                            if !rest.trim().is_empty() {
                                ui.add_space(6.0);
                                egui::Frame::NONE.fill(theme::BG_BASE).inner_margin(10.0).corner_radius(4.0).show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    egui::ScrollArea::both().max_height(ui.available_height() - 20.0).show(ui, |ui| {
                                        ui.add(egui::Label::new(RichText::new(rest.trim()).monospace().size(11.5).color(theme::TEXT_DIM)).selectable(true).extend());
                                    });
                                });
                            }
                        });
                    });
                });
                if back {
                    self.screen = Screen::Pick;
                }
            }
            Screen::Pick => {
                let status = self.status.as_ref().filter(|s| s.1.elapsed() < s.2).map(|s| s.0.clone());
                let mut acts = Vec::new();
                let warning = self.tool_warning();
                if self.pick.recents.is_none() {
                    let list = self.svc.recents().into_iter().map(|e| {
                        let exists = Path::new(&e.cache_path).is_file();
                        (e, exists)
                    });
                    self.pick.recents = Some(list.collect());
                }
                let drag = self.dnd.hovering().then_some((self.dnd.hovered.as_slice(), self.dnd.pointer));
                egui::CentralPanel::default().show(ui, |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        acts = self.pick.show(ui, self.caps.as_ref(), status.as_deref(), warning.as_deref(), drag);
                    });
                });
                self.pick_actions(&ctx, acts);
            }
        }
        self.drop_overlay(&ctx);
        self.sync_cursor(&ctx);
        self.timing(t0);
    }

    // Kill ffmpeg, the players and the Resolve helper on the way out.
    #[cfg(feature = "glow")]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.svc.shutdown();
    }

    #[cfg(not(feature = "glow"))]
    fn on_exit(&mut self) {
        self.svc.shutdown();
    }
}

impl VcApp {
    fn timing(&mut self, t0: Instant) {
        self.ui_times.push_back(t0.elapsed().as_secs_f32() * 1000.0);
        while self.ui_times.len() > 60 {
            self.ui_times.pop_front();
        }
    }
}

/// "levels · 10-bit · 130,000 frames · 24/1fps · Y Avg μ=252 range=64–440"
/// (the web UI's header line).
fn summary_line(l: &Loaded, metric: Option<&str>) -> String {
    let s = &l.summary;
    let mut out = l.mode.clone();
    if let Some(b) = s["bit_depth"].as_u64() {
        out += &format!(" · {b}-bit");
    }
    out += &format!(" · {} frames", group(l.rows.frames.len() as u64));
    if let (Some(n), Some(d)) = (s["tc_format"]["fps_num"].as_u64(), s["tc_format"]["fps_den"].as_u64()) {
        out += &format!(" · {n}/{d}fps");
    }
    if let Some(m) = metric {
        let st = &s[format!("{m}_stats")];
        if let (Some(mean), Some(min), Some(max)) = (st["mean"].as_f64(), st["min"].as_f64(), st["max"].as_f64()) {
            let prec = if m.starts_with("ssim") { 4 } else { 0 };
            out += &format!(" · {} μ={mean:.prec$}", theme::column_style(m).0);
            if l.mode == "levels" {
                out += &format!(" range={min:.0}–{max:.0}");
            } else if m.starts_with("diff") {
                out += &format!(" max={max:.2}");
            } else {
                out += &format!(" min={min:.*}", if m.starts_with("ssim") { 4 } else { 2 });
            }
        }
    }
    if l.mode == "diff" {
        let d = &s["diff_distribution"];
        if let (Some(z), Some(a)) = (d["zero"].as_u64(), d["above_10"].as_u64()) {
            out += &format!(" · {} identical · {} major", group(z), group(a));
        }
    }
    out
}

/// Threshold line for a metric from its stats (App.svelte `t`): mean + σ
/// where higher is worse (differences, out-of-range counts), mean − σ
/// where lower is worse (ssim, vmaf, psnr, levels). Rounded to 0.01.
fn auto_threshold(summary: &Value, metric: &str) -> Option<f64> {
    const HIGHER_IS_WORSE: &[&str] = &["y_dif", "u_dif", "v_dif", "y_oor_lo", "y_oor_hi", "c_oor"];
    let st = &summary[format!("{metric}_stats")];
    let mean = st["mean"].as_f64()?;
    let sd = st["stddev"].as_f64().unwrap_or(0.0);
    let v = if metric.starts_with("diff") || HIGHER_IS_WORSE.contains(&metric) { mean + sd } else { mean - sd };
    Some((v * 100.0).round() / 100.0)
}

/// `data:image/jpeg;base64,…` → image (reference thumbnails).
fn decode_data_uri(uri: &str) -> Option<egui::ColorImage> {
    use base64::Engine;
    let b64 = uri.split_once(";base64,")?.1;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()))
}

/// Unix seconds → "2026-11-24" (UTC).
pub(crate) fn fmt_date(ts: u64) -> String {
    // Civil-from-days (Howard Hinnant), no date crate needed.
    let z = (ts / 86400) as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02}")
}

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        if let Some(h) = &home {
            return h.join("Library/Application Support/com.vcompare.app");
        }
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    // Same place as the Tauri app (its identifier), so recents and
    // references are shared while both exist.
    base.join("com.vcompare.app")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_follow_the_metric_direction() {
        let s = serde_json::json!({
            "diff_y_stats": {"mean": 2.0, "stddev": 0.5},
            "vmaf_stats": {"mean": 95.0, "stddev": 1.234},
            "y_oor_hi_stats": {"mean": 10.0},
        });
        assert_eq!(auto_threshold(&s, "diff_y"), Some(2.5));
        assert_eq!(auto_threshold(&s, "vmaf"), Some(93.77));
        assert_eq!(auto_threshold(&s, "y_oor_hi"), Some(10.0));
        assert_eq!(auto_threshold(&s, "psnr"), None);
    }

    #[test]
    fn dates() {
        assert_eq!(fmt_date(0), "1970-01-01");
        assert_eq!(fmt_date(1_795_000_000), "2026-11-18");
        assert_eq!(fmt_date(951_782_400), "2000-02-29");
    }
}
