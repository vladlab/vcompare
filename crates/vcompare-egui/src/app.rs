//! The window: top bar, chart, tabs (Frames / Flash), status line.
//!
//! Anything that can block (opening, loading a series, detection,
//! thumbnails) runs on a worker thread and comes back as a `Msg`.

use crate::chart::{Chart, ChartProps};
use crate::data::{Loaded, Series};
use crate::flash::{FlashAction, FlashTab};
use crate::table::FrameTable;
use crate::theme;
use crate::thumbs::{ThumbLoader, QUICK, SETTLE};
use eframe::egui::{self, Key, RichText, Ui};
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vcompare_service::engine::detect::{DetectRequest, DetectResult, Detector};
use vcompare_service::{Service, UiEvent};

enum Msg {
    Opened(Result<(Loaded, Series), String>),
    Series(Result<Series, String>),
    Detected(u64, Result<DetectResult, String>),
    /// Progress, mpv position etc. Nothing runs that sends them yet
    /// (analysis and players come in §3–§5 of the plan).
    #[allow(dead_code)]
    Service(UiEvent),
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Frames,
    Flash,
}

pub struct VcApp {
    svc: Arc<Service>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    loaded: Option<Loaded>,
    series: Option<Series>,
    table: Option<FrameTable>,
    chart: Chart,
    flash: FlashTab,
    thumbs: ThumbLoader,
    tab: Tab,
    cursor: i64,
    busy: Option<String>,
    status: Option<(String, Instant, bool)>,
    detect_gen: u64,
    ui_times: VecDeque<f32>,
    show_perf: bool,
}

impl VcApp {
    pub fn new(cc: &eframe::CreationContext<'_>, open: Option<String>) -> Self {
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
        let svc = Arc::new(Service::new(data_dir, Arc::new(sink)));
        let thumbs = ThumbLoader::new(svc.clone(), cc.egui_ctx.clone());
        let mut app = Self {
            svc,
            tx,
            rx,
            loaded: None,
            series: None,
            table: None,
            chart: Chart::default(),
            flash: FlashTab::default(),
            thumbs,
            tab: Tab::Frames,
            cursor: -1,
            busy: None,
            status: None,
            detect_gen: 0,
            ui_times: VecDeque::new(),
            show_perf: true,
        };
        if let Some(p) = open {
            app.open(&cc.egui_ctx, p);
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

    fn open(&mut self, ctx: &egui::Context, path: String) {
        self.busy = Some(format!("Opening {path}…"));
        self.spawn(ctx, move |svc| Msg::Opened(Loaded::open(svc, &path)));
    }

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

    fn say(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some((text.into(), Instant::now(), error));
    }

    fn receive(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Opened(Ok((loaded, series))) => {
                    self.busy = None;
                    self.cursor = loaded.rows.frames.first().copied().unwrap_or(-1);
                    self.table = Some(FrameTable::new(&loaded.rows));
                    self.chart.reset();
                    self.flash.reset();
                    self.thumbs.clear();
                    self.thumbs.hold(QUICK);
                    let msg = format!("Opened {} ({} frames)", loaded.file_name(), loaded.rows.frames.len());
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("vcompare — {}", loaded.file_name())));
                    self.loaded = Some(loaded);
                    self.series = Some(series);
                    self.say(msg, false);
                    self.detect(ctx);
                }
                Msg::Opened(Err(e)) => {
                    self.busy = None;
                    self.say(e, true);
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
                Msg::Service(_) => {}
            }
        }
    }

    fn keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let presses: Vec<(Key, egui::Modifiers)> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key, pressed: true, modifiers, .. } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        for (key, m) in presses {
            if m.ctrl || m.command || m.alt {
                if key == Key::O {
                    self.pick_file(ctx);
                }
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
                Key::Y => self.review_selected("accept"),
                Key::X => self.review_selected("reject"),
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

    fn pick_file(&mut self, ctx: &egui::Context) {
        let mut d = rfd::FileDialog::new().add_filter("vcompare analysis", &["json"]).set_title("Open analysis");
        if let Some(dir) = self.loaded.as_ref().and_then(|l| std::path::Path::new(&l.path).parent().map(|p| p.to_path_buf())) {
            d = d.set_directory(dir);
        }
        if let Some(p) = d.pick_file() {
            self.open(ctx, p.to_string_lossy().to_string());
        }
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            if ui.button("Open…").on_hover_text("Open a saved analysis (Ctrl+O)").clicked() {
                self.pick_file(&ctx);
            }
            if let Some(l) = &self.loaded {
                ui.separator();
                ui.label(RichText::new(l.file_name()).strong());
                let depth = l.summary["bit_depth"].as_u64().map(|b| format!(" · {b}-bit")).unwrap_or_default();
                ui.label(RichText::new(format!("{}{depth} · {} frames", l.mode, l.rows.frames.len())).color(theme::TEXT_DIM));
                ui.separator();
                let current = self.series.as_ref().map(|s| s.metric.clone()).unwrap_or_default();
                let mut pick = current.clone();
                egui::ComboBox::from_id_salt("metric")
                    .selected_text(theme::metric_label(&current))
                    .width(190.0)
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
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new("?").color(theme::TEXT_MUTED)).on_hover_text(
                    "Keys\n\
                     = / -  zoom in / out on the playhead,  0  show all\n\
                     . ,  → ←  step a frame\n\
                     n N  ↓ ↑  next / previous event\n\
                     y x  accept / reject the picked event\n\
                     m M  next / previous metric\n\
                     1 2  Frames / Flash tab\n\
                     Ctrl+O  open,  F12  timings\n\n\
                     Chart: wheel zooms on the mouse, drag scrubs, double-click shows all.\n\
                     Minimap: drag the window to pan, drag elsewhere to zoom to a range.",
                );
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if let Some(b) = &self.busy {
                ui.spinner();
                ui.label(b);
            } else if let (Some(l), Some(s)) = (&self.loaded, &self.series) {
                if let Some(i) = l.rows.index_of(self.cursor) {
                    ui.label(RichText::new(&l.rows.tcs[i]).monospace());
                    ui.label(RichText::new(format!("frame {}", self.cursor)).monospace().color(theme::TEXT_DIM));
                    if let Some(v) = s.value_at(self.cursor) {
                        ui.label(RichText::new(format!("{} {v:.3}", theme::metric_label(&s.metric))).monospace().color(theme::TEXT_DIM));
                    }
                }
            }
            if let Some((t, at, err)) = &self.status {
                if at.elapsed() < Duration::from_secs(if *err { 12 } else { 5 }) {
                    ui.separator();
                    ui.label(RichText::new(t).color(if *err { theme::THRESHOLD } else { theme::TEXT_DIM }));
                }
            }
            if let Some(e) = self.thumbs.error.take() {
                self.status = Some((format!("Thumbnails: {e}"), Instant::now(), true));
            }
            if self.show_perf {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let avg = if self.ui_times.is_empty() { 0.0 } else { self.ui_times.iter().sum::<f32>() / self.ui_times.len() as f32 };
                    let max = self.ui_times.iter().cloned().fold(0.0, f32::max);
                    ui.label(
                        RichText::new(format!(
                            "ui {avg:.1} ms (max {max:.1}) · chart {:.2} ms",
                            self.chart.last_paint.as_secs_f32() * 1000.0
                        ))
                        .monospace()
                        .size(11.0)
                        .color(theme::TEXT_MUTED),
                    )
                    .on_hover_text("CPU time per frame for the last 60 frames, and for the chart alone (F12 hides)");
                });
            }
        });
    }
}

impl eframe::App for VcApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let t0 = Instant::now();
        let ctx = ui.ctx().clone();
        self.receive(&ctx);
        self.thumbs.poll();
        self.keys(&ctx);
        let dropped: Option<String> = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_string_lossy().to_string()));
        if let Some(p) = dropped {
            self.open(&ctx, p);
        }

        egui::Panel::top("top").frame(egui::Frame::side_top_panel(ui.style()).fill(theme::BG_RAISED)).show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").frame(egui::Frame::side_top_panel(ui.style()).fill(theme::BG_RAISED)).show(ui, |ui| {
            self.status_bar(ui);
        });

        if self.loaded.is_none() || self.series.is_none() {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new(if self.busy.is_some() { "Loading…" } else { "Open a saved analysis (.levels.json / .vcompare.json), or drop one here." })
                            .color(theme::TEXT_MUTED),
                    );
                });
            });
            self.timing(t0);
            return;
        }

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
                        threshold: None,
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
            self.detect(&ctx);
        }
        if let Some(f) = new_cursor {
            self.cursor = f;
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

fn data_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    // Same place as the Tauri app (its identifier), so recents and
    // references are shared while both exist.
    base.join("com.vcompare.app")
}
