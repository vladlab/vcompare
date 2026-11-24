//! The start screen: pick the mode, drop or browse the file(s) into their
//! boxes, see what they are, set the options, then analyse (or open the
//! saved result). Recent analyses underneath.

use crate::theme;
use crate::timeline::{Setup, SetupAction};
use eframe::egui::{self, vec2, Align, Layout, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui};
use serde_json::Value;
use std::path::{Path, PathBuf};
use vcompare_service::engine::types::{Capabilities, ProbeResult};
use vcompare_service::{AnalyzeParams, RecentEntry};

pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mxf", "mov", "mp4", "mkv", "avi", "m4v", "mpg", "mpeg", "ts", "m2ts", "webm", "ogv", "mj2", "xml", "txt",
];
const PARALLEL_OPTIONS: &[u32] = &[1, 2, 3, 4, 6, 8, 12, 16];
const CARD_HEIGHT: f32 = 150.0;
/// Recent analyses listed on the start screen (the rest via "All…").
const RECENT_ROWS: usize = 6;

/// What a file box shows while files are dragged over the window.
#[derive(Clone, Copy)]
enum Hover<'a> {
    None,
    /// This box would get this file.
    Gets(&'a str),
    /// Files are hovering, but not for this box.
    Elsewhere,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Reference,
    Distorted,
}

/// What the screen asks the app to do.
pub enum PickAction {
    /// Pick a file for a slot (opens the dialog).
    Choose(Slot),
    /// Probe this path for this slot.
    Probe(Slot, String),
    /// Find out whether a cached analysis exists for the current files.
    CheckCache,
    Analyze(AnalyzeParams),
    LoadCached(String),
    ModeChanged,
    /// Timeline mode: ask Resolve for its tracks.
    TimelineConnect,
    /// Open File > Tools….
    OpenTools,
    /// Open a recent analysis.
    OpenRecent(String),
    /// Show the full Recent Analyses list.
    AllRecents,
}

pub struct Pick {
    pub mode: String,
    pub ref_path: String,
    pub dist_path: String,
    pub ref_probe: Option<ProbeResult>,
    pub dist_probe: Option<ProbeResult>,
    ref_error: Option<String>,
    dist_error: Option<String>,
    /// The boxes as last drawn, to find which one a drop landed in.
    card_rects: [Option<Rect>; 2],
    /// Recent analyses and whether each file still exists; `None` = reload.
    pub recents: Option<Vec<(RecentEntry, bool)>>,
    /// `check_cache` / `check_levels_cache` result.
    pub cache: Option<Value>,
    pub probing: usize,
    sync: String,
    hwaccel: String,
    psnr: bool,
    ssim: bool,
    subsample: u32,
    /// Levels: parallel ffmpeg decodes, 0 = auto.
    parallel: u32,
    pub keep_signatures: bool,
    pub last_dir: Option<PathBuf>,
    /// Timeline mode's setup (Resolve tracks, offset, dry run).
    pub timeline: Setup,
}

impl Pick {
    pub fn new(mode: String, keep_signatures: bool, last_dir: Option<PathBuf>) -> Self {
        let mode = if ["diff", "vmaf", "levels", "timeline"].contains(&mode.as_str()) { mode } else { "diff".into() };
        Self {
            mode,
            ref_path: String::new(),
            dist_path: String::new(),
            ref_probe: None,
            dist_probe: None,
            ref_error: None,
            dist_error: None,
            card_rects: [None, None],
            recents: None,
            cache: None,
            probing: 0,
            sync: "start".into(),
            hwaccel: "auto".into(),
            psnr: false,
            ssim: false,
            subsample: 1,
            parallel: 0,
            keep_signatures,
            last_dir,
            timeline: Setup::default(),
        }
    }

    /// Back to no files (New analysis); options are kept.
    pub fn clear_files(&mut self) {
        self.ref_path.clear();
        self.dist_path.clear();
        self.ref_probe = None;
        self.dist_probe = None;
        self.ref_error = None;
        self.dist_error = None;
        self.cache = None;
    }

    pub fn single_file(&self) -> bool {
        self.mode == "levels" || self.mode == "timeline"
    }

    /// Set a slot's path (from the dialog, a drop or a recent) and ask for
    /// it to be probed.
    pub fn set(&mut self, slot: Slot, path: String) -> PickAction {
        if let Some(d) = Path::new(&path).parent() {
            self.last_dir = Some(d.to_path_buf());
        }
        match slot {
            Slot::Reference => {
                self.ref_path = path.clone();
                self.ref_probe = None;
                self.ref_error = None;
            }
            Slot::Distorted => {
                self.dist_path = path.clone();
                self.dist_probe = None;
                self.dist_error = None;
            }
        }
        self.cache = None;
        self.probing += 1;
        PickAction::Probe(slot, path)
    }

    /// Where dropped files go. Levels / timeline: the first file. Compare:
    /// two files fill reference then distorted; one file goes to `target`
    /// (the box it was dropped on), else the empty box (reference first),
    /// else the reference. Returns the assignments and a note if files were
    /// left out. Saved analyses (.json) are the app's business, not ours.
    pub fn plan_drop(&self, paths: &[String], target: Option<Slot>) -> (Vec<(Slot, String)>, Option<String>) {
        let media: Vec<&String> = paths.iter().filter(|p| !is_analysis(p)).collect();
        let Some(first) = media.first() else { return (vec![], None) };
        if self.single_file() {
            let note = (media.len() > 1).then(|| format!("This mode takes one file — using {}", file_name(first)));
            return (vec![(Slot::Reference, first.to_string())], note);
        }
        if media.len() >= 2 {
            let note = (media.len() > 2).then(|| "A comparison takes two files — the others were left out".to_string());
            return (vec![(Slot::Reference, first.to_string()), (Slot::Distorted, media[1].to_string())], note);
        }
        let slot = target.unwrap_or(if self.ref_path.is_empty() || !self.dist_path.is_empty() {
            Slot::Reference
        } else {
            Slot::Distorted
        });
        (vec![(slot, first.to_string())], None)
    }

    /// Put dropped files in their boxes (see `plan_drop`).
    pub fn drop_files(&mut self, paths: &[String], target: Option<Slot>) -> (Vec<PickAction>, Option<String>) {
        let (plan, note) = self.plan_drop(paths, target);
        (plan.into_iter().map(|(s, p)| self.set(s, p)).collect(), note)
    }

    /// Whether the files needed for this mode are chosen and probed.
    fn ready(&self) -> bool {
        self.ref_probe.is_some() && (self.single_file() || self.dist_probe.is_some())
    }

    fn params(&self) -> AnalyzeParams {
        let levels = self.mode == "levels";
        AnalyzeParams {
            reference: self.ref_path.clone(),
            distorted: (!self.single_file()).then(|| self.dist_path.clone()),
            mode: Some(self.mode.clone()),
            hwaccel: Some(self.hwaccel.clone()),
            sync: Some(self.sync.clone()),
            psnr: Some(self.psnr),
            ssim: Some(self.ssim),
            subsample: Some(self.subsample),
            chunks: (levels && self.parallel > 0).then_some(self.parallel),
            signatures: levels.then_some(self.keep_signatures),
            log_path: self.cache.as_ref().and_then(|c| c["path"].as_str()).map(String::from),
            force: Some(true),
            ..Default::default()
        }
    }


    /// `tool_warning`: something missing for this mode (see File > Tools…).
    /// `drag`: files being dragged over the window, and where the pointer
    /// is if known.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        caps: Option<&Capabilities>,
        status: Option<&str>,
        tool_warning: Option<&str>,
        drag: Option<(&[String], Option<Pos2>)>,
    ) -> Vec<PickAction> {
        let mut acts = Vec::new();
        let width = 820.0f32.min(ui.available_width() - 48.0);
        ui.add_space(28.0);
        ui.vertical_centered(|ui| {
            ui.set_max_width(width);
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.label(RichText::new("New analysis").size(22.0).strong());
                ui.add_space(12.0);
                self.mode_picker(ui, &mut acts);
                ui.add_space(4.0);
                ui.label(RichText::new(mode_blurb(&self.mode)).color(theme::TEXT_DIM));
                if let Some(w) = tool_warning {
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(w).color(theme::WARNING));
                        if ui.small_button("Tools…").clicked() {
                            acts.push(PickAction::OpenTools);
                        }
                    });
                }
                ui.add_space(16.0);

                self.cards(ui, drag, &mut acts);
                if let Some(s) = status {
                    ui.add_space(6.0);
                    ui.label(RichText::new(s).color(theme::TEXT_DIM));
                }
                self.pair_notes(ui, &mut acts);
                ui.add_space(14.0);

                if self.mode == "timeline" {
                    section(ui, "Resolve");
                    let grabber = caps.and_then(|c| c.grabber.as_deref());
                    match self.timeline.show(ui, self.ref_probe.is_some(), grabber) {
                        Some(SetupAction::Connect) => acts.push(PickAction::TimelineConnect),
                        Some(SetupAction::Start { tracks, offset, dry_run }) => acts.push(PickAction::Analyze(AnalyzeParams {
                            reference: self.ref_path.clone(),
                            mode: Some("timeline".into()),
                            tracks: Some(tracks),
                            offset: Some(offset),
                            dry_run: Some(dry_run),
                            ..Default::default()
                        })),
                        None => {}
                    }
                } else {
                    section(ui, "Options");
                    if self.mode == "levels" {
                        self.levels_options(ui);
                    } else {
                        self.compare_options(ui, caps);
                    }
                    ui.add_space(16.0);
                    self.go(ui, &mut acts);
                }

                self.recent_list(ui, &mut acts);
            });
        });
        acts
    }

    fn mode_picker(&mut self, ui: &mut Ui, acts: &mut Vec<PickAction>) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for m in ["diff", "vmaf", "levels", "timeline"] {
                let on = self.mode == m;
                let text = RichText::new(mode_label(m)).size(14.0).color(if on { theme::TEXT } else { theme::TEXT_DIM });
                let b = egui::Button::new(text).selected(on).min_size(vec2(0.0, 30.0));
                if ui.add(b).clicked() && !on {
                    self.mode = m.to_string();
                    acts.push(PickAction::ModeChanged);
                    acts.push(PickAction::CheckCache);
                }
            }
        });
    }

    /// The drop boxes: one for levels / timeline, two side by side for a
    /// comparison.
    fn cards(&mut self, ui: &mut Ui, drag: Option<(&[String], Option<Pos2>)>, acts: &mut Vec<PickAction>) {
        let slots: &[Slot] = if self.single_file() { &[Slot::Reference] } else { &[Slot::Reference, Slot::Distorted] };
        let gap = 12.0;
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), CARD_HEIGHT), Sense::hover());
        let w = (row.width() - gap * (slots.len() - 1) as f32) / slots.len() as f32;
        // Where the hovering files would go, given the box under the pointer.
        let incoming: Vec<(Slot, String)> = match drag {
            Some((paths, at)) => {
                let target = at.and_then(|p| self.slot_at_rects(p, row, w, gap, slots));
                self.plan_drop(paths, target).0
            }
            None => vec![],
        };
        let json = drag.is_some_and(|(p, _)| p.iter().any(|p| is_analysis(p)));
        self.card_rects = [None, None];
        for (i, &slot) in slots.iter().enumerate() {
            let rect = Rect::from_min_size(row.min + vec2(i as f32 * (w + gap), 0.0), vec2(w, CARD_HEIGHT));
            self.card_rects[i] = Some(rect);
            let gets = incoming.iter().find(|(s, _)| *s == slot).map(|(_, p)| p.as_str());
            let hover = match (drag.is_some(), gets) {
                (false, _) => Hover::None,
                (true, _) if json => Hover::Elsewhere,
                (true, Some(p)) => Hover::Gets(p),
                (true, None) => Hover::Elsewhere,
            };
            self.card(ui, rect, slot, hover, acts);
        }
        if json {
            ui.add_space(6.0);
            ui.label(RichText::new("Drop to open this saved analysis").color(theme::ACCENT).strong());
        }
    }

    fn slot_at_rects(&self, p: Pos2, row: Rect, w: f32, gap: f32, slots: &[Slot]) -> Option<Slot> {
        slots.iter().enumerate().find_map(|(i, &s)| {
            let r = Rect::from_min_size(row.min + vec2(i as f32 * (w + gap), 0.0), vec2(w, CARD_HEIGHT));
            r.expand(gap / 2.0).contains(p).then_some(s)
        })
    }

    /// Which box is at `p` (from the last frame's layout), for a drop.
    pub fn slot_at(&self, p: Pos2) -> Option<Slot> {
        let slots = [Slot::Reference, Slot::Distorted];
        self.card_rects.iter().zip(slots).find_map(|(r, s)| r.filter(|r| r.expand(6.0).contains(p)).map(|_| s))
    }

    fn slot_label(&self, slot: Slot) -> &'static str {
        match (slot, self.mode.as_str()) {
            (Slot::Distorted, _) => "Distorted",
            (_, "levels") => "File",
            (_, "timeline") => "Deliverable",
            _ => "Reference",
        }
    }

    fn card(&mut self, ui: &mut Ui, rect: Rect, slot: Slot, hover: Hover, acts: &mut Vec<PickAction>) {
        let id = ui.scope_id().with(("card", slot as u8));
        let resp = ui.interact(rect, id, Sense::click());
        let (path, probe, error) = match slot {
            Slot::Reference => (self.ref_path.clone(), self.ref_probe.clone(), self.ref_error.clone()),
            Slot::Distorted => (self.dist_path.clone(), self.dist_probe.clone(), self.dist_error.clone()),
        };
        let empty = path.is_empty();
        let painter = ui.painter_at(rect);
        let r = 8.0;

        // Background and border say what the box will do.
        let (fill, border, dashed) = match hover {
            Hover::Gets(_) => (theme::ACCENT.gamma_multiply(0.16), Stroke::new(2.0, theme::ACCENT), false),
            Hover::Elsewhere => (theme::BG_SURFACE, Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.5)), true),
            Hover::None if empty && resp.hovered() => (theme::BG_HOVER, Stroke::new(1.0, theme::TEXT_DIM), true),
            Hover::None if empty => (theme::BG_SURFACE, Stroke::new(1.0, theme::TEXT_MUTED), true),
            Hover::None if error.is_some() => (theme::BG_SURFACE, Stroke::new(1.0, theme::ERROR), false),
            Hover::None => (theme::BG_SURFACE, Stroke::new(1.0, theme::BG_ACTIVE), false),
        };
        painter.rect_filled(rect, r, fill);
        if dashed {
            dashed_rect(&painter, rect.shrink(0.5), border);
        } else {
            painter.rect_stroke(rect, r, border, StrokeKind::Inside);
        }
        if empty && matches!(hover, Hover::None) {
            resp.clone().on_hover_cursor(egui::CursorIcon::PointingHand);
        }

        let inner = rect.shrink2(vec2(16.0, 12.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
        let ui = &mut child;
        let mut remove = false;
        let label = self.slot_label(slot);
        ui.horizontal(|ui| {
            ui.label(RichText::new(label.to_uppercase()).size(11.0).strong().color(theme::TEXT_DIM));
            if !empty && matches!(hover, Hover::None) {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Remove").on_hover_text("Empty this box").clicked() {
                        remove = true;
                    }
                    if ui.small_button("Change…").clicked() {
                        acts.push(PickAction::Choose(slot));
                    }
                });
            }
        });

        match hover {
            Hover::Gets(p) => {
                ui.add_space(14.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(if empty { "Drop to use" } else { "Drop to replace with" }).color(theme::ACCENT));
                    ui.add(egui::Label::new(RichText::new(file_name(p)).size(16.0).strong().color(theme::TEXT)).truncate());
                });
            }
            Hover::Elsewhere => {}
            _ if empty => {
                ui.add_space(4.0);
                let icon = Rect::from_center_size(egui::pos2(inner.center().x, ui.cursor().top() + 18.0), vec2(30.0, 30.0));
                drop_icon(ui.painter(), icon, if resp.hovered() { theme::TEXT } else { theme::TEXT_DIM });
                ui.add_space(42.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Drop a file here").size(15.0).color(theme::TEXT));
                    ui.label(RichText::new("or click to browse").color(theme::TEXT_DIM));
                });
            }
            _ => {
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(file_name(&path)).size(16.0).strong().color(theme::TEXT)).truncate())
                    .on_hover_text(&path);
                let dir = Path::new(&path).parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
                ui.add(egui::Label::new(RichText::new(dir).size(11.5).color(theme::TEXT_MUTED)).truncate());
                ui.add_space(6.0);
                if let Some(e) = &error {
                    ui.add(egui::Label::new(RichText::new(format!("Can't read this file: {e}")).color(theme::ERROR)).wrap());
                } else if let Some(p) = &probe {
                    let mono = |s: String| RichText::new(s).monospace().size(12.0).color(theme::TEXT_DIM);
                    ui.add(egui::Label::new(mono(probe_line1(p))).truncate());
                    ui.add(egui::Label::new(mono(probe_line2(p))).truncate());
                } else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Reading the file…").color(theme::TEXT_DIM));
                    });
                }
            }
        }
        if empty && matches!(hover, Hover::None) && resp.clicked() {
            acts.push(PickAction::Choose(slot));
        }
        if remove {
            self.clear_slot(slot);
            acts.push(PickAction::CheckCache);
        }
    }

    /// Under the boxes: swap, and what differs between the two files.
    fn pair_notes(&mut self, ui: &mut Ui, acts: &mut Vec<PickAction>) {
        if self.single_file() || (self.ref_path.is_empty() && self.dist_path.is_empty()) {
            return;
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.small_button("Swap").on_hover_text("Swap reference and distorted").clicked() {
                self.swap();
                acts.push(PickAction::CheckCache);
            }
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                if let (Some(r), Some(d)) = (&self.ref_probe, &self.dist_probe) {
                    for (text, warn) in pair_differences(r, d, &self.sync) {
                        ui.label(RichText::new(text).size(12.5).color(if warn { theme::WARNING } else { theme::TEXT_DIM }));
                    }
                }
            });
        });
    }

    /// The Analyze button, or the saved result for these files.
    fn go(&mut self, ui: &mut Ui, acts: &mut Vec<PickAction>) {
        let ready = self.ready();
        let cached = self.cache.as_ref().filter(|c| c["exists"].as_bool() == Some(true)).cloned();
        ui.horizontal(|ui| {
            if let Some(c) = &cached {
                let open = egui::Button::new(RichText::new("Open saved result").size(15.0).strong().color(theme::TEXT))
                    .fill(theme::ACCENT.gamma_multiply(0.55))
                    .min_size(vec2(180.0, 36.0));
                if ui.add(open).clicked() {
                    if let Some(p) = c["path"].as_str() {
                        acts.push(PickAction::LoadCached(p.to_string()));
                    }
                }
                if ui.add_enabled(ready, egui::Button::new(RichText::new("Analyze again").size(14.0)).min_size(vec2(140.0, 36.0))).clicked() {
                    acts.push(PickAction::Analyze(self.params()));
                }
            } else {
                let b = egui::Button::new(RichText::new("Analyze").size(15.0).strong().color(if ready { theme::TEXT } else { theme::TEXT_MUTED }))
                    .fill(if ready { theme::ACCENT.gamma_multiply(0.55) } else { theme::BG_RAISED })
                    .min_size(vec2(180.0, 36.0));
                if ui.add_enabled(ready, b).clicked() {
                    acts.push(PickAction::Analyze(self.params()));
                }
            }
            let hint = if let Some(c) = &cached {
                let frames = c["frame_count"].as_u64().map(group).unwrap_or_default();
                let what = if self.mode == "levels" {
                    "levels".to_string()
                } else {
                    format!(
                        "{}{}, {} sync",
                        c["mode"].as_str().unwrap_or("vmaf"),
                        if c["with_ssim"].as_bool() == Some(true) { " + SSIM" } else { "" },
                        if c["sync_mode"].as_str() == Some("tc") { "timecode" } else { "start" }
                    )
                };
                format!("Saved next to the file: {what}, {frames} frames")
            } else if self.probing > 0 {
                "Reading the files…".to_string()
            } else if !ready {
                let (r, d) = (self.ref_path.is_empty(), self.dist_path.is_empty());
                match (self.single_file(), r, d) {
                    (true, true, _) => "Choose the file first".into(),
                    (false, true, true) => "Choose both files first".into(),
                    (false, true, false) => "Choose the reference first".into(),
                    (false, false, true) => "Choose the distorted file first".into(),
                    _ => String::new(),
                }
            } else {
                String::new()
            };
            ui.label(RichText::new(hint).color(theme::TEXT_DIM));
        });
    }

    fn recent_list(&mut self, ui: &mut Ui, acts: &mut Vec<PickAction>) {
        let Some(recents) = &self.recents else { return };
        if recents.is_empty() {
            return;
        }
        ui.add_space(26.0);
        ui.horizontal(|ui| {
            section_label(ui, "Recent");
            if recents.len() > RECENT_ROWS && ui.small_button("All…").clicked() {
                acts.push(PickAction::AllRecents);
            }
        });
        ui.add_space(4.0);
        for (e, exists) in recents.iter().take(RECENT_ROWS) {
            let files = if e.dist_path.is_empty() {
                file_name(&e.ref_path)
            } else {
                format!("{}  vs  {}", file_name(&e.ref_path), file_name(&e.dist_path))
            };
            let meta = format!("{} · {} frames · {}", mode_label(&e.mode), group(e.frame_count), crate::app::fmt_date(e.timestamp));
            let b = egui::Button::new(RichText::new(files).size(13.0))
                .right_text(RichText::new(meta).size(11.5).color(theme::TEXT_DIM))
                .frame_when_inactive(false)
                .min_size(vec2(ui.available_width(), 26.0));
            let r = ui.add_enabled(*exists, b).on_hover_text(&e.cache_path).on_disabled_hover_text(format!("{} is gone", e.cache_path));
            if r.clicked() {
                acts.push(PickAction::OpenRecent(e.cache_path.clone()));
            }
        }
    }

    fn clear_slot(&mut self, slot: Slot) {
        match slot {
            Slot::Reference => {
                self.ref_path.clear();
                self.ref_probe = None;
                self.ref_error = None;
            }
            Slot::Distorted => {
                self.dist_path.clear();
                self.dist_probe = None;
                self.dist_error = None;
            }
        }
        self.cache = None;
    }

    fn swap(&mut self) {
        std::mem::swap(&mut self.ref_path, &mut self.dist_path);
        std::mem::swap(&mut self.ref_probe, &mut self.dist_probe);
        std::mem::swap(&mut self.ref_error, &mut self.dist_error);
        self.cache = None;
    }

    /// A slot's file couldn't be read.
    pub fn probe_failed(&mut self, slot: Slot, error: String) {
        match slot {
            Slot::Reference => self.ref_error = Some(error),
            Slot::Distorted => self.dist_error = Some(error),
        }
    }

    fn levels_options(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
            let auto = (cores / 10).clamp(1, 8);
            ui.label("Parallel decodes").on_hover_text(
                "How many ffmpeg processes decode the file side by side. One ffmpeg tops out around 60 fps for UHD 10-bit; \
                 more lift that on fast local storage. Lower it for slow disks or network shares. \
                 Only intra-frame, constant-rate files (ProRes, DNxHR, …) can be split.",
            );
            let text = |n: u32| match n {
                0 => format!("Auto ({auto})"),
                1 => "1 (no split)".to_string(),
                n => n.to_string(),
            };
            egui::ComboBox::from_id_salt("parallel").selected_text(text(self.parallel)).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.parallel, 0, text(0));
                for &n in PARALLEL_OPTIONS {
                    ui.selectable_value(&mut self.parallel, n, text(n));
                }
            });
            ui.add_space(12.0);
            let size = self.ref_probe.as_ref().and_then(|p| {
                let frames = p.nb_frames.unwrap_or_else(|| (p.duration * p.fps).round() as u64);
                (frames > 0).then(|| {
                    let mb = frames as f64 * 864.0 / 1e6;
                    if mb >= 1000.0 { format!(" (~{:.1} GB)", mb / 1000.0) } else { format!(" (~{} MB)", mb.round().max(1.0)) }
                })
            });
            ui.checkbox(&mut self.keep_signatures, format!("Keep frame signatures{}", size.unwrap_or_default())).on_hover_text(
                "Save a tiny picture signature of every frame next to the analysis (a .sig file), so the Image match tab \
                 can find frames that look like a reference picture — Media Offline cards, slates, missing renders.",
            );
        });
    }

    fn compare_options(&mut self, ui: &mut Ui, caps: Option<&Capabilities>) {
        ui.horizontal(|ui| {
            ui.label("Sync");
            let sync_label = |s: &str| if s == "tc" { "Timecode" } else { "Start-aligned" };
            egui::ComboBox::from_id_salt("sync").selected_text(sync_label(&self.sync)).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.sync, "start".into(), sync_label("start"));
                ui.selectable_value(&mut self.sync, "tc".into(), sync_label("tc"));
            });
            ui.add_space(12.0);
            if self.mode == "vmaf" {
                ui.label("HW Accel");
                let auto = format!("Auto{}", caps.and_then(|c| c.hwaccel.as_deref()).map(|h| format!(" ({h})")).unwrap_or_default());
                let hw_label = |h: &str| match h {
                    "cuda" => "CUDA".to_string(),
                    "vaapi" => "VAAPI".to_string(),
                    "none" => "None".to_string(),
                    _ => auto.clone(),
                };
                egui::ComboBox::from_id_salt("hwaccel").selected_text(hw_label(&self.hwaccel)).show_ui(ui, |ui| {
                    for h in ["auto", "cuda", "vaapi", "none"] {
                        ui.selectable_value(&mut self.hwaccel, h.to_string(), hw_label(h));
                    }
                });
                ui.checkbox(&mut self.psnr, "PSNR");
                ui.label("Subsample");
                let sub_label = |n: u32| if n == 1 { "1× (every frame)".to_string() } else { format!("{n}×") };
                egui::ComboBox::from_id_salt("subsample").selected_text(sub_label(self.subsample)).show_ui(ui, |ui| {
                    for n in [1, 2, 4, 8] {
                        ui.selectable_value(&mut self.subsample, n, sub_label(n));
                    }
                });
            } else {
                ui.checkbox(&mut self.ssim, "SSIM");
            }
        });
    }
}

pub fn mode_label(m: &str) -> &'static str {
    match m {
        "vmaf" => "VMAF",
        "levels" => "Levels",
        "timeline" => "Timeline (vs. Resolve SDI)",
        _ => "Pixel Diff",
    }
}

/// 130000 → "130,000".
pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn mode_blurb(m: &str) -> &'static str {
    match m {
        "vmaf" => "VMAF (and optionally PSNR) of a distorted file against its reference, frame by frame. Needs an ffmpeg with libvmaf.",
        "levels" => "Signal levels of one file: per-plane min, max and average for every frame, plus flash frames, repeats and image match.",
        "timeline" => "Compares a deliverable with what Resolve puts out on SDI, at the first, middle and last frame of every clip on the tracks you tick.",
        _ => "Pixel difference (and optionally SSIM) between a reference and a distorted file, frame by frame.",
    }
}

fn file_name(p: &str) -> String {
    Path::new(p).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string())
}

/// A saved analysis rather than media (opened, not put in a box).
pub fn is_analysis(p: &str) -> bool {
    p.to_lowercase().ends_with(".json")
}

fn section_label(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).size(11.0).strong().color(theme::TEXT_DIM));
}

fn section(ui: &mut Ui, text: &str) {
    section_label(ui, text);
    ui.add_space(4.0);
}

/// "3840×2160 · prores · yuv422p10le"
fn probe_line1(p: &ProbeResult) -> String {
    format!("{}×{} · {} · {}", p.width, p.height, p.codec, p.pix_fmt)
}

/// "23.98 fps · TC 01:00:00:00 · 42:13 · 60,756 frames"
fn probe_line2(p: &ProbeResult) -> String {
    let mut parts = vec![format!("{} fps", fmt_fps(p))];
    if let Some(tc) = &p.timecode {
        parts.push(format!("TC {tc}"));
    }
    if p.duration > 0.0 {
        parts.push(crate::progress::fmt_duration(p.duration));
    }
    let frames = p.nb_frames.unwrap_or_else(|| (p.duration * p.fps).round() as u64);
    if frames > 0 {
        parts.push(format!("{} frames", group(frames)));
    }
    parts.join(" · ")
}

fn fmt_fps(p: &ProbeResult) -> String {
    if p.fps_den == 1001 {
        format!("{:.2}", p.fps_num as f64 / p.fps_den as f64)
    } else {
        let f = p.fps_num as f64 / p.fps_den.max(1) as f64;
        if f.fract() == 0.0 { format!("{f:.0}") } else { format!("{f}") }
    }
}

/// What differs between reference and distorted that changes the result,
/// as (text, is a warning).
fn pair_differences(r: &ProbeResult, d: &ProbeResult, sync: &str) -> Vec<(String, bool)> {
    let mut out = vec![];
    if (r.fps - d.fps).abs() > 0.01 {
        out.push((format!("Frame rates differ ({} vs {} fps): results may be unreliable", fmt_fps(r), fmt_fps(d)), true));
    }
    if (r.width, r.height) != (d.width, d.height) {
        let (w, h) = if r.width * r.height <= d.width * d.height { (r.width, r.height) } else { (d.width, d.height) };
        out.push((format!("Sizes differ: compared at {w}×{h}"), false));
    }
    if r.pix_fmt != d.pix_fmt {
        out.push((format!("Pixel formats differ ({} vs {})", r.pix_fmt, d.pix_fmt), false));
    }
    if sync == "tc" && (r.timecode.is_none() || d.timecode.is_none()) {
        out.push(("Timecode sync, but a file has no timecode".to_string(), true));
    }
    out
}

/// A rounded-looking dashed outline (straight dashes along each side).
fn dashed_rect(p: &egui::Painter, r: Rect, stroke: Stroke) {
    let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
    for w in pts.windows(2) {
        p.extend(egui::Shape::dashed_line(&[w[0], w[1]], stroke, 6.0, 4.0));
    }
}

/// An arrow down into a tray.
fn drop_icon(p: &egui::Painter, r: Rect, color: egui::Color32) {
    let s = Stroke::new(2.0, color);
    let c = r.center();
    let (w, h) = (r.width(), r.height());
    p.line_segment([egui::pos2(c.x, r.top()), egui::pos2(c.x, c.y + h * 0.12)], s);
    p.line_segment([egui::pos2(c.x - w * 0.2, c.y - h * 0.08), egui::pos2(c.x, c.y + h * 0.12)], s);
    p.line_segment([egui::pos2(c.x + w * 0.2, c.y - h * 0.08), egui::pos2(c.x, c.y + h * 0.12)], s);
    let y = r.bottom() - h * 0.05;
    p.line_segment([egui::pos2(r.left(), c.y + h * 0.05), egui::pos2(r.left(), y)], s);
    p.line_segment([egui::pos2(r.left(), y), egui::pos2(r.right(), y)], s);
    p.line_segment([egui::pos2(r.right(), y), egui::pos2(r.right(), c.y + h * 0.05)], s);
}


#[cfg(test)]
mod tests {
    use super::*;

    fn pick(mode: &str) -> Pick {
        Pick::new(mode.into(), true, None)
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn drops_fill_slots() {
        let mut p = pick("diff");
        let (a, note) = p.drop_files(&s(&["/x/a.mov", "/x/b.mov", "/x/c.mov"]), None);
        assert_eq!((p.ref_path.as_str(), p.dist_path.as_str()), ("/x/a.mov", "/x/b.mov"));
        assert_eq!(a.len(), 2);
        assert!(note.is_some());

        let mut p = pick("diff");
        p.drop_files(&s(&["/x/a.mov"]), None);
        p.drop_files(&s(&["/x/b.mov"]), None);
        assert_eq!((p.ref_path.as_str(), p.dist_path.as_str()), ("/x/a.mov", "/x/b.mov"));
        // Both full, dropped nowhere in particular: replaces the reference.
        p.drop_files(&s(&["/x/c.mov"]), None);
        assert_eq!(p.ref_path, "/x/c.mov");

        let mut p = pick("levels");
        let (_, note) = p.drop_files(&s(&["/x/a.mov", "/x/b.mov"]), Some(Slot::Distorted));
        assert_eq!((p.ref_path.as_str(), p.dist_path.as_str()), ("/x/a.mov", ""));
        assert!(note.unwrap().contains("a.mov"));
        assert_eq!(p.last_dir.as_deref(), Some(Path::new("/x")));
    }

    #[test]
    fn a_single_drop_goes_to_the_box_it_landed_on() {
        let mut p = pick("diff");
        // Empty boxes: dropped on Distorted, goes there.
        p.drop_files(&s(&["/x/b.mov"]), Some(Slot::Distorted));
        assert_eq!((p.ref_path.as_str(), p.dist_path.as_str()), ("", "/x/b.mov"));
        p.drop_files(&s(&["/x/a.mov"]), None);
        assert_eq!(p.ref_path, "/x/a.mov");
        // Both full: replaces the one it landed on.
        p.drop_files(&s(&["/x/c.mov"]), Some(Slot::Distorted));
        assert_eq!((p.ref_path.as_str(), p.dist_path.as_str()), ("/x/a.mov", "/x/c.mov"));
        // Two files ignore the target: reference, then distorted.
        p.drop_files(&s(&["/x/d.mov", "/x/e.mov"]), Some(Slot::Distorted));
        assert_eq!((p.ref_path.as_str(), p.dist_path.as_str()), ("/x/d.mov", "/x/e.mov"));
    }

    #[test]
    fn saved_analyses_are_not_media() {
        let p = pick("diff");
        let (plan, _) = p.plan_drop(&s(&["/x/a.diff.json"]), None);
        assert!(plan.is_empty());
        let (plan, _) = p.plan_drop(&s(&["/x/a.mov", "/x/A.LEVELS.JSON"]), None);
        assert_eq!(plan, vec![(Slot::Reference, "/x/a.mov".to_string())]);
    }

    #[test]
    fn swap_and_remove() {
        let mut p = pick("diff");
        p.drop_files(&s(&["/x/a.mov", "/x/b.mov"]), None);
        p.probe_failed(Slot::Reference, "bad".into());
        p.swap();
        assert_eq!((p.ref_path.as_str(), p.dist_path.as_str()), ("/x/b.mov", "/x/a.mov"));
        assert_eq!((p.ref_error.as_deref(), p.dist_error.as_deref()), (None, Some("bad")));
        p.clear_slot(Slot::Distorted);
        assert_eq!((p.dist_path.as_str(), p.dist_error.as_deref()), ("", None));
    }

    #[test]
    fn params_follow_the_mode() {
        let mut p = pick("levels");
        p.ref_path = "/x/a.mov".into();
        p.dist_path = "/x/b.mov".into();
        p.parallel = 4;
        p.cache = Some(serde_json::json!({"exists": true, "path": "/x/a.levels.json"}));
        let a = p.params();
        assert_eq!(a.distorted, None);
        assert_eq!(a.chunks, Some(4));
        assert_eq!(a.signatures, Some(true));
        assert_eq!(a.log_path.as_deref(), Some("/x/a.levels.json"));
        assert_eq!(a.force, Some(true));
        p.mode = "diff".into();
        let a = p.params();
        assert_eq!(a.distorted.as_deref(), Some("/x/b.mov"));
        assert_eq!((a.chunks, a.signatures), (None, None));
    }

    #[test]
    fn formats() {
        assert_eq!(group(130000), "130,000");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        let p = ProbeResult {
            path: String::new(),
            filename: String::new(),
            width: 3840,
            height: 2160,
            pix_fmt: "yuv422p10le".into(),
            codec: "prores".into(),
            fps: 23.976,
            fps_num: 24000,
            fps_den: 1001,
            nb_frames: None,
            duration: 1.0,
            timecode: Some("01:00:00:00".into()),
        };
        assert_eq!(probe_line1(&p), "3840×2160 · prores · yuv422p10le");
        assert_eq!(probe_line2(&p), "23.98 fps · TC 01:00:00:00 · 1s · 24 frames");
        let mut d = p.clone();
        d.width = 1920;
        d.height = 1080;
        d.fps = 25.0;
        d.fps_num = 25;
        d.fps_den = 1;
        d.timecode = None;
        let notes: Vec<String> = pair_differences(&p, &d, "tc").into_iter().map(|n| n.0).collect();
        assert_eq!(
            notes,
            [
                "Frame rates differ (23.98 vs 25 fps): results may be unreliable",
                "Sizes differ: compared at 1920×1080",
                "Timecode sync, but a file has no timecode"
            ]
        );
    }
}
