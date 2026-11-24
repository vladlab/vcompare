//! The window. Screens: Pick (choose files, options) → Working (analysing
//! or opening, with progress) → Results (chart, tabs), or Error.
//!
//! Anything that can block (probing, analysing, opening, loading a
//! series, detection, thumbnails) runs on a worker thread and comes back
//! as a `Msg`. Jobs carry a token; a cancelled or superseded job's late
//! answer is dropped.

use crate::chart::{Chart, ChartProps};
use crate::data::{Loaded, Series};
use crate::flash::{FlashAction, FlashTab};
use crate::pick::{group, Pick, PickAction, Slot, VIDEO_EXTENSIONS};
use crate::progress::{Job, JobKind};
use crate::settings::Settings;
use crate::table::FrameTable;
use crate::theme;
use crate::thumbs::{ThumbLoader, QUICK, SETTLE};
use eframe::egui::{self, Key, RichText, Ui};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vcompare_service::engine::detect::{DetectRequest, DetectResult, Detector};
use vcompare_service::engine::types::{Capabilities, ProbeResult};
use vcompare_service::{AnalyzeParams, RecentEntry, Service, UiEvent};

enum Msg {
    Caps(Capabilities),
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
    Detected(u64, Result<DetectResult, String>),
    Exported(Result<String, String>),
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
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Frames,
    Flash,
}

enum Screen {
    Pick,
    Working(Job),
    Results,
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
    flash: FlashTab,
    thumbs: ThumbLoader,
    tab: Tab,
    cursor: i64,
    /// Chart threshold line (`t` / `T`); diff and vmaf start with one.
    threshold: Option<f64>,
    status: Option<(String, Instant, Duration, bool)>,
    detect_gen: u64,
    ui_times: VecDeque<f32>,
    show_perf: bool,
    show_keys: bool,
    recents: Option<Vec<RecentEntry>>,
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
            flash: FlashTab::default(),
            thumbs,
            tab: Tab::Frames,
            cursor: -1,
            threshold: None,
            status: None,
            detect_gen: 0,
            ui_times: VecDeque::new(),
            show_perf: false,
            show_keys: false,
            recents: None,
            dialog_open: false,
            quit: false,
        };
        app.spawn(&cc.egui_ctx, |svc| Msg::Caps(svc.detect_capabilities()));
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
        Settings { mode: self.pick.mode.clone(), last_dir: self.pick.last_dir.clone(), keep_signatures: self.pick.keep_signatures }
            .save(&self.data_dir);
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        let secs = if error { 12 } else { 6 };
        self.status = Some((text.into(), Instant::now(), Duration::from_secs(secs), error));
    }

    // ── Jobs ──

    fn begin(&mut self, kind: JobKind) -> u64 {
        self.job_token += 1;
        self.screen = Screen::Working(Job::new(kind));
        self.job_token
    }

    fn analyze(&mut self, ctx: &egui::Context, params: AnalyzeParams) {
        self.save_settings();
        let mode = params.mode.clone().unwrap_or_default();
        let token = self.begin(JobKind::Analyze { mode });
        self.spawn(ctx, move |svc| {
            let r = svc.analyze(&params);
            let perf = r.as_ref().ok().and_then(|s| s["perf"].as_str().map(String::from));
            let path = params.log_path.clone().unwrap_or_default();
            let result = r.and_then(|summary| Loaded::from_summary(svc, summary, &path));
            Msg::Finished { token, result, what: "Analysis failed", perf }
        });
    }

    /// Open a saved analysis (from the dialog, a drop, a recent, the
    /// command line or "Load cached").
    fn open_saved(&mut self, ctx: &egui::Context, path: String, what: &'static str) {
        let token = self.begin(JobKind::Open(path.clone()));
        self.spawn(ctx, move |svc| Msg::Finished { token, result: Loaded::open(svc, &path), what, perf: None });
    }

    /// Paths from the command line or a drop: an analysis opens; media
    /// goes to the pick screen's slots.
    fn open_paths(&mut self, ctx: &egui::Context, paths: Vec<String>) {
        if let Some(j) = paths.iter().find(|p| p.to_lowercase().ends_with(".json")) {
            self.open_saved(ctx, j.clone(), "Failed to open the analysis");
            return;
        }
        if matches!(self.screen, Screen::Working(_)) {
            return;
        }
        self.screen = Screen::Pick;
        let (acts, note) = self.pick.drop_files(&paths);
        if let Some(n) = note {
            self.say(n, false);
        }
        self.pick_actions(ctx, acts);
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
        self.flash.reset();
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
        self.loaded = Some(loaded);
        self.series = Some(series);
        self.screen = Screen::Results;
        self.detect(ctx);
    }

    fn new_analysis(&mut self) {
        if matches!(self.screen, Screen::Working(_)) {
            return;
        }
        self.pick.clear_files();
        self.screen = Screen::Pick;
        self.recents = None;
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
        if r.is_empty() || (mode != "levels" && d.is_empty()) {
            return;
        }
        self.spawn(ctx, move |svc| {
            let v = if mode == "levels" { svc.check_levels_cache(&r) } else { svc.check_cache(&r, &d) };
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

    fn detect(&mut self, ctx: &egui::Context) {
        self.detect_gen += 1;
        let g = self.detect_gen;
        let req = DetectRequest::Flash(self.flash.params.clone());
        self.spawn(ctx, move |svc| Msg::Detected(g, svc.detect_events(&req)));
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
                Msg::Caps(c) => self.caps = Some(c),
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
                        Err(e) => {
                            let name = Path::new(&path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or(path);
                            self.say(format!("Can't read {name}: {e}"), true);
                        }
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
                Msg::Series(Ok(s)) => self.series = Some(s),
                Msg::Series(Err(e)) => self.say(e, true),
                Msg::Detected(g, r) if g == self.detect_gen => match r {
                    Ok(r) => {
                        self.flash.error = None;
                        self.flash.result = Some(r);
                    }
                    Err(e) => self.flash.error = Some(e),
                },
                Msg::Detected(..) => {}
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
                    }
                }
                Msg::Exported(Ok(p)) => self.say(format!("Exported {p}"), false),
                Msg::Exported(Err(e)) => self.say(format!("Export failed: {e}"), true),
                Msg::Service(e) => {
                    if let Screen::Working(job) = &mut self.screen {
                        match e {
                            UiEvent::Progress { current, total } => job.progress(current, total),
                            UiEvent::Diagnostics(lines) => job.diagnostics = lines,
                            _ => {}
                        }
                    }
                }
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
                Key::Period | Key::ArrowRight => self.step(1),
                Key::Comma | Key::ArrowLeft => self.step(-1),
                Key::M => {
                    let ms = &loaded.metrics;
                    if let Some(i) = ms.iter().position(|x| *x == series.metric) {
                        let n = ms.len();
                        let next = ms[if m.shift { (i + n - 1) % n } else { (i + 1) % n }].clone();
                        self.set_metric(ctx, next);
                    }
                }
                Key::Num1 => self.tab = Tab::Frames,
                Key::Num2 => {
                    self.tab = Tab::Flash;
                    self.thumbs.hold(QUICK);
                }
                Key::N => self.next_event(if m.shift { -1 } else { 1 }),
                Key::ArrowDown | Key::ArrowUp => {
                    let d = if key == Key::ArrowDown { 1 } else { -1 };
                    match self.tab {
                        Tab::Flash => self.next_event(d),
                        Tab::Frames => self.step(d as i64),
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
            Tab::Flash => l.rows.index_of(self.cursor).and_then(|i| {
                let j = (i as i64 + d).clamp(0, l.rows.frames.len() as i64 - 1) as usize;
                l.rows.frames.get(j).copied()
            }),
        };
        if let Some(f) = next {
            self.cursor = f;
        }
    }

    fn next_event(&mut self, dir: i32) {
        self.tab = Tab::Flash;
        if let Some(f) = self.flash.neighbour(self.cursor, dir) {
            self.cursor = f;
        }
    }

    fn review_selected(&mut self, verdict: &'static str) {
        let Some(f) = self.flash.selected else { return };
        let cur = self.flash.result.as_ref().and_then(|r| r.events.iter().find(|e| e.frame == f)).and_then(|e| e.review.clone());
        let v = if cur.as_deref() == Some(verdict) { None } else { Some(verdict) };
        self.review(f, v);
    }

    fn review(&mut self, frame: i64, v: Option<&'static str>) {
        match self.svc.set_event_review(Detector::Flash, frame, v) {
            Ok(_) => self.flash.set_review(frame, v),
            Err(e) => self.say(e, true),
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
        let sc = |k: &str| egui::RichText::new(if cfg!(target_os = "macos") { k.replace("Ctrl+", "⌘") } else { k.to_string() });
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
            ("n N   ↓ ↑", "next / previous event"),
            ("y  x", "accept / reject the picked event"),
            ("m  M", "next / previous metric"),
            ("t  T", "threshold line from the metric (mean ± σ) / clear it"),
            ("1  2", "Frames / Flash tab"),
            ("e", "export CSV"),
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
        let mut flash_acts = Vec::new();
        {
            let loaded = self.loaded.as_ref().unwrap();
            let series = self.series.as_ref().unwrap();
            let events: Vec<_> = if self.tab == Tab::Flash { self.flash.visible().into_iter().cloned().collect() } else { vec![] };
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
                    };
                    new_cursor = self.chart.show(ui, series, &props);
                });
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                let n = self.flash.visible().len();
                if ui.selectable_label(self.tab == Tab::Frames, "Frames").clicked() {
                    self.tab = Tab::Frames;
                }
                let flash_label = if self.flash.result.is_some() { format!("Flash ({n})") } else { "Flash".into() };
                if ui.selectable_label(self.tab == Tab::Flash, flash_label).clicked() && self.tab != Tab::Flash {
                    self.tab = Tab::Flash;
                    self.thumbs.hold(QUICK);
                }
            });
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
                Tab::Flash => {
                    flash_acts = self.flash.show(ui, &mut self.thumbs, loaded.files);
                }
            }
        });

        let mut redetect = false;
        for a in flash_acts {
            match a {
                FlashAction::Select(f) => {
                    self.flash.pick(f);
                    new_cursor = Some(f);
                }
                FlashAction::Review(f, v) => self.review(f, v),
                FlashAction::Adjusting => self.thumbs.hold(SETTLE),
                FlashAction::Changed => redetect = true,
            }
        }
        if redetect {
            self.detect(ui.ctx());
        }
        if let Some(f) = new_cursor {
            self.cursor = f;
        }
    }
}

impl eframe::App for VcApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let t0 = Instant::now();
        let ctx = ui.ctx().clone();
        self.receive(&ctx);
        self.thumbs.poll();
        self.keys(&ctx);
        let dropped: Vec<String> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_string_lossy().to_string()).collect());
        if !dropped.is_empty() {
            self.open_paths(&ctx, dropped);
        }
        if self.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        self.keys_window(&ctx);
        self.recents_window(&ctx);
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
                egui::CentralPanel::default().show(ui, |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        acts = self.pick.show(ui, self.caps.as_ref(), status.as_deref());
                    });
                });
                self.pick_actions(&ctx, acts);
            }
        }
        self.timing(t0);
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

/// Unix seconds → "2026-11-24" (UTC).
fn fmt_date(ts: u64) -> String {
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
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
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
