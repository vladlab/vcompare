//! A detector's tab: its controls and its event list with in-row
//! thumbnail strips. One type for all three detectors (flash, repeat,
//! image match); port of DetectorTab.svelte, EventList.svelte and
//! src/lib/detectors.ts.

use crate::theme;
use crate::thumbs::{Span, StripState, ThumbLoader};
use eframe::egui::{self, pos2, vec2, Align, Color32, Rect, RichText, Sense, TextureHandle, Ui, UiBuilder};
use std::collections::{HashMap, HashSet};
use vcompare_service::engine::detect::{DetectRequest, DetectResult, Detector, Event, FlashParams, MatchParams, RepeatParams};
use vcompare_service::engine::references::StoredReference;

const TILE_H: f32 = 54.0;
const TILE_W: f32 = 96.0;
const ROLE_H: f32 = 13.0;

pub const ALL: [Detector; 3] = [Detector::Flash, Detector::Repeat, Detector::Match];

pub fn index(k: Detector) -> usize {
    match k {
        Detector::Flash => 0,
        Detector::Repeat => 1,
        Detector::Match => 2,
    }
}

pub fn tab_label(k: Detector, timeline: bool) -> &'static str {
    match k {
        Detector::Flash if timeline => "Mismatches",
        Detector::Flash => "Flash frames",
        Detector::Repeat => "Repeats",
        Detector::Match => "Image match",
    }
}

/// Modes where the detector has a tab (detectors.ts TABS).
pub fn in_mode(k: Detector, mode: &str) -> bool {
    match k {
        Detector::Flash => mode == "diff" || mode == "levels" || mode == "timeline",
        Detector::Repeat | Detector::Match => mode == "levels",
    }
}

/// Event frames a strip shows before "+n".
fn run_tiles(k: Detector) -> u32 {
    match k {
        Detector::Flash => 5,
        Detector::Repeat => 3,
        Detector::Match => 2,
    }
}

pub enum Action {
    /// Move the cursor to this frame and pick the event starting at `event`.
    Select { event: i64, frame: i64 },
    /// Accept / reject / clear the event starting at this frame.
    Review(i64, Option<&'static str>),
    /// A control is moving: hold thumbnails back.
    Adjusting,
    /// Parameters changed: run detection again.
    Changed,
    ExportEdl,
    ExportCsv,
    // Image-match references
    AddImage,
    AddFrame,
    Rename(String, String),
    Delete(String),
}

/// The tiles of an event's strip: the frame before, the run (capped) and
/// the frame after when the run is shown whole.
pub struct StripLayout {
    pub span: Span,
    pub roles: Vec<&'static str>,
    pub more: u32,
}

pub fn strip_for(k: Detector, e: &Event) -> StripLayout {
    let shown = e.length.min(run_tiles(k));
    let whole = shown == e.length;
    let mut roles = vec!["prev"];
    roles.extend(std::iter::repeat_n("run", shown as usize));
    if whole {
        roles.push("next");
    }
    StripLayout { span: Span { start: e.frame - 1, count: roles.len() as u32 }, roles, more: e.length - shown }
}

/// The image-match references as the tab shows them.
pub struct RefView<'a> {
    pub list: &'a [StoredReference],
    pub thumbs: &'a HashMap<String, TextureHandle>,
}

pub struct DetectorTab {
    pub kind: Detector,
    /// Timeline mode: the flash detector lists mismatching samples.
    pub timeline: bool,
    pub flash: FlashParams,
    pub repeat: RepeatParams,
    pub matching: MatchParams,
    pub hide_rejected: bool,
    pub result: Option<DetectResult>,
    pub error: Option<String>,
    /// First frame of the picked event (row highlight, y / x, n / N origin).
    pub selected: Option<i64>,
    pub seen: HashSet<i64>,
    /// Slider value while dragging; applied on release.
    drag: Option<f64>,
    scroll_to: Option<usize>,
    viewport_h: f32,
    /// Reference being renamed: (id, text, focus taken yet).
    editing: Option<(String, String, bool)>,
}

impl DetectorTab {
    pub fn new(kind: Detector) -> Self {
        Self {
            kind,
            timeline: false,
            flash: FlashParams::default(),
            repeat: RepeatParams::default(),
            matching: MatchParams::default(),
            hide_rejected: false,
            result: None,
            error: None,
            selected: None,
            seen: HashSet::new(),
            drag: None,
            scroll_to: None,
            viewport_h: 400.0,
            editing: None,
        }
    }

    /// Forget results (another analysis was opened); keep the settings.
    pub fn reset(&mut self) {
        self.result = None;
        self.error = None;
        self.selected = None;
        self.seen.clear();
    }

    /// What to ask the engine, or `None` when there's nothing to look for
    /// (image match with no reference ticked): then `empty_result` stands in.
    pub fn request(&self) -> Option<DetectRequest> {
        match self.kind {
            Detector::Flash => Some(DetectRequest::Flash(self.flash.clone())),
            Detector::Repeat => Some(DetectRequest::Repeat(self.repeat.clone())),
            Detector::Match if self.matching.references.is_empty() => None,
            Detector::Match => Some(DetectRequest::Match(self.matching.clone())),
        }
    }

    pub fn empty_result(&self) -> DetectResult {
        DetectResult { events: vec![], threshold: self.matching.min_similarity, source: "signatures".into(), detector: self.kind, note: None }
    }

    /// The events the list shows and the chart marks.
    pub fn visible(&self) -> Vec<&Event> {
        self.result
            .as_ref()
            .map(|r| r.events.iter().filter(|e| !(self.hide_rejected && e.review.as_deref() == Some("reject"))).collect())
            .unwrap_or_default()
    }

    /// Next (`dir` > 0) or previous event, from the picked one if any (so
    /// shuttling around it doesn't change where n / N go), else from the
    /// cursor; wraps around (App.svelte `jumpToEvent`).
    pub fn neighbour(&mut self, cursor: i64, dir: i32) -> Option<i64> {
        let ev = self.visible();
        if ev.is_empty() {
            return None;
        }
        let from = self.selected.unwrap_or(cursor);
        let f = if dir > 0 {
            ev.iter().find(|e| e.frame > from).unwrap_or(&ev[0]).frame
        } else {
            ev.iter().rev().find(|e| e.frame < from).unwrap_or(&ev[ev.len() - 1]).frame
        };
        self.pick(f);
        Some(f)
    }

    pub fn pick(&mut self, frame: i64) {
        self.selected = Some(frame);
        self.seen.insert(frame);
        if let Some(i) = self.visible().iter().position(|e| e.frame == frame) {
            self.scroll_to = Some(i);
        }
    }

    pub fn review_of(&self, frame: i64) -> Option<Option<String>> {
        self.result.as_ref()?.events.iter().find(|e| e.frame == frame).map(|e| e.review.clone())
    }

    pub fn set_review(&mut self, frame: i64, review: Option<&'static str>) {
        if let Some(r) = self.result.as_mut() {
            if let Some(e) = r.events.iter_mut().find(|e| e.frame == frame) {
                e.review = review.map(String::from);
            }
        }
    }

    fn max_tiles(&self) -> u32 {
        match self.kind {
            Detector::Flash => self.flash.max_run.min(run_tiles(Detector::Flash)) + 2,
            k => run_tiles(k) + 2,
        }
    }

    fn label(&self, e: &Event) -> String {
        let len = if e.length == 1 { "1 fr".to_string() } else { format!("{} fr", e.length) };
        match e.kind.as_str() {
            "repeat" => match e.subkind.as_str() {
                "hold" => format!("held {len}"),
                "black" => format!("black, repeated {len}"),
                _ => format!("repeated {len}"),
            },
            "match" => format!("{} · {len}", e.label.as_deref().unwrap_or(&e.subkind)),
            "error" => format!("{} · not checked: {}", e.label.as_deref().unwrap_or(""), e.subkind.replace('_', " ")),
            "mismatch" => e.label.clone().unwrap_or_else(|| e.subkind.clone()),
            _ => format!("{} · {len}{}", e.subkind, if e.at_cut { " · at cut" } else { "" }),
        }
    }

    fn score(&self, e: &Event, threshold: f64) -> String {
        match e.kind.as_str() {
            "error" => "—".into(),
            "repeat" if e.score >= 1.0 && e.score.fract() == 0.0 => String::new(),
            "repeat" => format!("×{:.1}", e.score),
            "match" => format!("×{:.2}", e.score),
            // Flashes: the change itself in the slider's units (score is
            // change ÷ threshold), so it reads as "set the slider above this".
            _ if threshold > 0.0 => {
                let pct = e.score * threshold * 100.0;
                format!("{pct:.*}%", if pct >= 10.0 { 0 } else if self.timeline { 2 } else { 1 })
            }
            _ => format!("×{:.1}", e.score),
        }
    }

    fn role_label(&self, role: &str, e: &Event, k: usize) -> String {
        match (role, e.kind.as_str()) {
            ("prev", "repeat") => "shown".into(),
            ("prev", _) => "prev".into(),
            ("next", _) => "next".into(),
            _ if self.timeline => "sample".into(),
            (_, "repeat") => "repeat".into(),
            (_, "match") => "match".into(),
            _ if e.length > 1 => format!("flash {k}"),
            _ => "flash".into(),
        }
    }

    fn empty_text(&self, refs: &RefView) -> &'static str {
        match self.kind {
            Detector::Match if self.matching.references.is_empty() => {
                if refs.list.is_empty() {
                    "No reference pictures yet. Add one from an image (a still of the offline card) or from the frame under the cursor."
                } else {
                    "Tick a reference above to look for it."
                }
            }
            Detector::Match => "No frames match the ticked references.",
            Detector::Repeat => "No repeated frames.",
            Detector::Flash if self.timeline => "No mismatches: every sample matches the SDI picture.",
            Detector::Flash => "No flash frames found.",
        }
    }

    pub fn show(&mut self, ui: &mut Ui, thumbs: &mut ThumbLoader, files: usize, cursor: i64, refs: &RefView) -> Vec<Action> {
        let mut acts = Vec::new();
        ui.horizontal_wrapped(|ui| {
            match self.kind {
                Detector::Flash => self.flash_controls(ui, &mut acts),
                Detector::Repeat => self.repeat_controls(ui, &mut acts),
                Detector::Match => self.match_controls(ui, &mut acts),
            }
            ui.checkbox(&mut self.hide_rejected, "hide rejected").on_hover_text("Hide events you've rejected");
        });
        ui.horizontal(|ui| {
            self.summary(ui);
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let any = !self.visible().is_empty();
                if ui.add_enabled(any, egui::Button::new("CSV").small()).on_hover_text("Export as CSV").clicked() {
                    acts.push(Action::ExportCsv);
                }
                if ui
                    .add_enabled(any, egui::Button::new("EDL").small())
                    .on_hover_text("Export as a DaVinci Resolve marker EDL (Timeline › Import › Timeline Markers from EDL). Rejected events are left out.")
                    .clicked()
                {
                    acts.push(Action::ExportEdl);
                }
            });
        });
        if self.kind == Detector::Match {
            self.references(ui, refs, &mut acts);
        }
        if let Some(n) = self.result.as_ref().and_then(|r| r.note.clone()) {
            ui.label(RichText::new(n).size(12.0).color(theme::TEXT_DIM));
        }
        ui.separator();

        if let Some(e) = &self.error {
            ui.colored_label(theme::THRESHOLD, e);
            return acts;
        }
        let Some(result) = &self.result else {
            ui.label(RichText::new("Detecting…").color(theme::TEXT_MUTED));
            return acts;
        };
        let threshold = result.threshold;
        let events: Vec<Event> = self.visible().into_iter().cloned().collect();
        if events.is_empty() {
            ui.label(RichText::new(self.empty_text(refs)).color(theme::TEXT_MUTED));
            return acts;
        }

        let max_tiles = self.max_tiles();
        let row_h = files as f32 * (TILE_H + ROLE_H + 2.0) + 12.0;
        let mut sa = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt(("events", index(self.kind)));
        if let Some(i) = self.scroll_to.take() {
            sa = sa.vertical_scroll_offset((i as f32 * row_h - self.viewport_h / 2.0 + row_h / 2.0).max(0.0));
        }
        let kind = self.kind;
        let mut wanted: Vec<Span> = Vec::new();
        let out = sa.show_rows(ui, row_h, events.len(), |ui, range| {
            // Load order: picked event, visible rows, a screenful below.
            if let Some(i) = self.selected.and_then(|f| events.iter().position(|e| e.frame == f)) {
                wanted.push(strip_for(kind, &events[i]).span);
            }
            let below = range.end..(range.end + range.len()).min(events.len());
            for i in range.clone().chain(below) {
                wanted.push(strip_for(kind, &events[i]).span);
            }
            let spans: Vec<Span> = range.clone().map(|i| strip_for(kind, &events[i]).span).collect();
            thumbs.forget_errors(&spans);
            for i in range {
                if let Some(a) = self.row(ui, &events[i], row_h, threshold, max_tiles, files, cursor, thumbs, refs) {
                    acts.push(a);
                }
            }
        });
        self.viewport_h = out.inner_rect.height();
        thumbs.want(&wanted);
        acts
    }

    fn summary(&self, ui: &mut Ui) {
        let Some(r) = &self.result else { return };
        let ev = self.visible();
        let mut parts: Vec<(String, usize)> = Vec::new();
        let mut at_cut = 0;
        for e in &ev {
            let k = if self.kind == Detector::Match {
                e.label.clone().unwrap_or_else(|| e.subkind.clone())
            } else if self.timeline {
                e.kind.clone()
            } else {
                e.subkind.clone()
            };
            match parts.iter_mut().find(|p| p.0 == k) {
                Some(p) => p.1 += 1,
                None => parts.push((k, 1)),
            }
            at_cut += (self.kind == Detector::Flash && !self.timeline && e.at_cut) as usize;
        }
        let (mut acc, mut rej) = (0, 0);
        for e in &r.events {
            match e.review.as_deref() {
                Some("accept") => acc += 1,
                Some("reject") => rej += 1,
                _ => {}
            }
        }
        let mut job = egui::text::LayoutJob::default();
        let f = egui::FontId::proportional(12.0);
        let add = |job: &mut egui::text::LayoutJob, t: &str, c: Color32| job.append(t, 0.0, egui::TextFormat::simple(f.clone(), c));
        add(&mut job, &format!("{} event{}", ev.len(), if ev.len() == 1 { "" } else { "s" }), theme::WARNING);
        let mut rest = String::new();
        for (k, n) in &parts {
            rest += &format!(" · {n} {k}");
        }
        if at_cut > 0 {
            rest += &format!(" · {at_cut} at cut");
        }
        add(&mut job, &rest, theme::TEXT_DIM);
        if acc + rej > 0 {
            add(&mut job, " · ", theme::TEXT_DIM);
            add(&mut job, &format!("{acc} accepted"), theme::SUCCESS);
            add(&mut job, " · ", theme::TEXT_DIM);
            add(&mut job, &format!("{rej} rejected"), theme::THRESHOLD);
        }
        if self.kind == Detector::Flash {
            add(&mut job, &format!(" · thr {:.*}%", if self.timeline { 2 } else { 1 }, r.threshold * 100.0), theme::TEXT_DIM);
        }
        ui.label(job);
    }

    /// A slider that applies its value when let go (or on a click / key).
    fn slider(&mut self, ui: &mut Ui, current: f64, range: std::ops::RangeInclusive<f64>, step: f64, decimals: usize, suffix: &str, enabled: bool, tip: &str, acts: &mut Vec<Action>) -> Option<f64> {
        let mut v = self.drag.unwrap_or(current);
        let r = ui
            .add_enabled(enabled, egui::Slider::new(&mut v, range).step_by(step).suffix(suffix).fixed_decimals(decimals))
            .on_hover_text(tip);
        if r.dragged() {
            self.drag = Some(v);
            acts.push(Action::Adjusting);
            return None;
        }
        if r.drag_stopped() || (r.changed() && !r.dragged()) {
            self.drag = None;
            acts.push(Action::Adjusting);
            acts.push(Action::Changed);
            return Some(v);
        }
        None
    }

    fn combo<T: PartialEq + Copy>(ui: &mut Ui, id: &str, value: &mut T, options: &[(T, String)], acts: &mut Vec<Action>) {
        let before = *value;
        let text = options.iter().find(|o| o.0 == *value).map(|o| o.1.clone()).unwrap_or_default();
        egui::ComboBox::from_id_salt(id).width(52.0).selected_text(text).show_ui(ui, |ui| {
            for (v, l) in options {
                ui.selectable_value(value, *v, l);
            }
        });
        if *value != before {
            acts.push(Action::Adjusting);
            acts.push(Action::Changed);
        }
    }

    fn flash_controls(&mut self, ui: &mut Ui, acts: &mut Vec<Action>) {
        let tl = self.timeline;
        let mut auto = self.flash.threshold.is_none();
        let auto_tip = if tl {
            "Derive the threshold from the cleanest samples of this run (lower quartile + 6σ, kept between 0.4% and 3% of peak, so a fault on every sample is still flagged)"
        } else {
            "Derive the threshold from the footage's own frame-to-frame noise"
        };
        if ui.checkbox(&mut auto, "auto").on_hover_text(auto_tip).changed() {
            self.flash.threshold = if auto { None } else { Some(self.result.as_ref().map(|r| r.threshold).unwrap_or(0.04)) };
            acts.push(Action::Adjusting);
            acts.push(Action::Changed);
        }
        let current = self.flash.threshold.or(self.result.as_ref().map(|r| r.threshold)).unwrap_or(0.04) * 100.0;
        let (range, step, dec, tip) = if tl {
            (0.1..=10.0, 0.05, 2, "RMS luma difference that flags a sample, as % of peak. Applies when you let go.")
        } else {
            (0.5..=40.0, 0.5, 1, "Minimum frame-to-frame change, as % of signal peak. Applies when you let go.")
        };
        let shown = (current / step).round() * step;
        if let Some(v) = self.slider(ui, shown, range, step, dec, "%", !auto, tip, acts) {
            self.flash.threshold = Some(v.max(0.1) / 100.0);
        }
        if !tl {
            ui.label("≤");
            let opts: Vec<(u32, String)> = (1..=5).map(|n| (n, n.to_string())).collect();
            Self::combo(ui, "flash_max_run", &mut self.flash.max_run, &opts, acts);
            ui.label("fr").on_hover_text("Longest run of frames that still counts as a flash");
        }
        let (label, tip) = if tl {
            ("hide cut frames", "Hide samples on a clip's first or last frame (where an edit that moved by a frame shows)")
        } else {
            ("hide at-cut", "Hide short inserts that sit on a cut (fast montage, etc.)")
        };
        if ui.checkbox(&mut self.flash.exclude_at_cut, label).on_hover_text(tip).changed() {
            acts.push(Action::Adjusting);
            acts.push(Action::Changed);
        }
    }

    fn repeat_controls(&mut self, ui: &mut Ui, acts: &mut Vec<Action>) {
        let mut exact = self.repeat.tolerance == 0.0;
        if ui.checkbox(&mut exact, "exact").on_hover_text("Only frames identical to the one before (right for intra codecs: ProRes, DNx, J2K)").changed() {
            self.repeat.tolerance = if exact { 0.0 } else { 0.001 };
            acts.push(Action::Adjusting);
            acts.push(Action::Changed);
        }
        let pct = ((self.repeat.tolerance * 10000.0).round() / 100.0).max(0.01);
        if let Some(v) = self.slider(
            ui,
            pct,
            0.01..=0.5,
            0.01,
            2,
            "%",
            !exact,
            "Largest mean difference from the previous frame that still counts as a repeat, as % of peak (long-GOP encodes repeat to near zero, not zero). Applies when you let go.",
            acts,
        ) {
            self.repeat.tolerance = v / 100.0;
        }
        ui.label("≥").on_hover_text("Fewest repeated frames that make an event");
        let o: Vec<(u32, String)> = [1, 2, 3].iter().map(|&n| (n, n.to_string())).collect();
        Self::combo(ui, "repeat_min_run", &mut self.repeat.min_run, &o, acts);
        ui.label("≤").on_hover_text("Longest run listed as a repeat; longer runs are holds");
        let o: Vec<(u32, String)> = [1, 2, 3, 5, 8, 12].iter().map(|&n| (n, n.to_string())).collect();
        Self::combo(ui, "repeat_max_run", &mut self.repeat.max_run, &o, acts);
        ui.label("fr");
        if ui.checkbox(&mut self.repeat.include_long, "holds").on_hover_text("Also list longer runs: freezes, holds, stills, titles").changed() {
            acts.push(Action::Adjusting);
            acts.push(Action::Changed);
        }
        ui.label("join if still ≤").on_hover_text(
            "Repeats with only near-still frames between them are joined into one hold, so a card that shimmers, fades or barely animates \
             is one event instead of a scatter of short repeats. This is how different those in-between frames may be (% of peak). \
             Raise it if a static card is still listed in pieces; lower it (or turn it off) if separate faults in a very quiet shot get joined.",
        );
        let stills = [0.0, 0.0005, 0.001, 0.002, 0.005, 0.01];
        let label = |v: f64| if v == 0.0 { "off".to_string() } else { format!("{}%", trim_zeros(v * 100.0)) };
        let before = self.repeat.still_tolerance;
        egui::ComboBox::from_id_salt("repeat_still").width(64.0).selected_text(label(self.repeat.still_tolerance)).show_ui(ui, |ui| {
            for v in stills {
                ui.selectable_value(&mut self.repeat.still_tolerance, v, label(v));
            }
        });
        if self.repeat.still_tolerance != before {
            acts.push(Action::Adjusting);
            acts.push(Action::Changed);
        }
        if ui.checkbox(&mut self.repeat.skip_black, "skip black").on_hover_text("Leave out repeated black frames").changed() {
            acts.push(Action::Adjusting);
            acts.push(Action::Changed);
        }
    }

    fn match_controls(&mut self, ui: &mut Ui, acts: &mut Vec<Action>) {
        let pct = (self.matching.min_similarity * 100.0).round();
        if let Some(v) = self.slider(
            ui,
            pct,
            50.0..=100.0,
            1.0,
            0,
            "%",
            true,
            "Minimum similarity to a reference (correlation of the picture's layout, so a card that went through a LUT or a range change still matches). Applies when you let go.",
            acts,
        ) {
            self.matching.min_similarity = v / 100.0;
        }
        ui.label("≥").on_hover_text("Fewest consecutive matching frames");
        let o: Vec<(u32, String)> = [1, 2, 3, 6, 12].iter().map(|&n| (n, n.to_string())).collect();
        Self::combo(ui, "match_min_run", &mut self.matching.min_run, &o, acts);
        ui.label("fr");
    }

    fn references(&mut self, ui: &mut Ui, refs: &RefView, acts: &mut Vec<Action>) {
        ui.horizontal_wrapped(|ui| {
            for r in refs.list {
                let mut on = self.matching.references.contains(&r.id);
                let tip = format!("{}{}", r.source, if r.flat { " — flat colour: matched by colour, not layout" } else { "" });
                egui::Frame::NONE
                    .fill(if on { theme::BG_ACTIVE } else { theme::BG_OVERLAY })
                    .corner_radius(4.0)
                    .inner_margin(egui::Margin::symmetric(6, 3))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut on, "").changed() {
                                if on {
                                    self.matching.references.push(r.id.clone());
                                } else {
                                    self.matching.references.retain(|x| *x != r.id);
                                }
                                acts.push(Action::Adjusting);
                                acts.push(Action::Changed);
                            }
                            if let Some(t) = refs.thumbs.get(&r.id) {
                                let s = t.size_vec2();
                                ui.add(egui::Image::new((t.id(), vec2(36.0 * s.x / s.y.max(1.0), 36.0 * 9.0 / 16.0).min(vec2(40.0, 24.0)))));
                            }
                            let editing = self.editing.as_ref().is_some_and(|e| e.0 == r.id);
                            if editing {
                                let (_, buf, focused) = self.editing.as_mut().unwrap();
                                let resp = ui.add(egui::TextEdit::singleline(buf).desired_width(140.0));
                                if !*focused {
                                    // Once, with everything selected, so typing replaces the name.
                                    resp.request_focus();
                                    let mut st = egui::text_edit::TextEditState::load(ui.ctx(), resp.id).unwrap_or_default();
                                    let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(buf.chars().count()));
                                    st.cursor.set_char_range(Some(all));
                                    st.store(ui.ctx(), resp.id);
                                    *focused = true;
                                } else if resp.lost_focus() {
                                    // Enter renames; Esc or clicking away cancels.
                                    let (id, name, _) = self.editing.take().unwrap();
                                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) && !name.trim().is_empty() && name != r.name {
                                        acts.push(Action::Rename(id, name));
                                    }
                                }
                            } else if ui
                                .add(egui::Label::new(&r.name).sense(Sense::click()))
                                .on_hover_text(format!("{tip}\nDouble-click to rename"))
                                .double_clicked()
                            {
                                self.editing = Some((r.id.clone(), r.name.clone(), false));
                            }
                            if r.flat {
                                ui.label(RichText::new("flat").size(10.5).color(theme::TEXT_DIM)).on_hover_text("Flat colour: matched by colour");
                            }
                            if ui.small_button("×").on_hover_text("Delete this reference").clicked() {
                                acts.push(Action::Delete(r.id.clone()));
                            }
                        });
                    });
            }
            if ui
                .small_button("+ image…")
                .on_hover_text("Add a reference from an image file (PNG, JPEG, TIFF, DPX, EXR…). It is fitted into this file's frame the way it would be placed on the timeline.")
                .clicked()
            {
                acts.push(Action::AddImage);
            }
            if ui
                .small_button("+ this frame")
                .on_hover_text("Use the frame under the cursor as a reference — exact, and the reliable route for a card that went through an output transform")
                .clicked()
            {
                acts.push(Action::AddFrame);
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn row(&mut self, ui: &mut Ui, e: &Event, row_h: f32, threshold: f64, max_tiles: u32, files: usize, cursor: i64, thumbs: &mut ThumbLoader, refs: &RefView) -> Option<Action> {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
        let selected = self.selected == Some(e.frame);
        let bg = if selected {
            theme::BG_ACTIVE
        } else if resp.hovered() {
            theme::BG_HOVER
        } else {
            Color32::TRANSPARENT
        };
        let row = rect.shrink2(vec2(0.0, 1.0));
        ui.painter().rect_filled(row, 3.0, bg);
        let mark = match e.review.as_deref() {
            Some("accept") => Some(theme::SUCCESS),
            Some("reject") => Some(theme::THRESHOLD),
            _ => None,
        };
        if let Some(c) = mark {
            ui.painter().rect_filled(Rect::from_min_size(row.min, vec2(3.0, row.height())), 1.0, c);
        }
        let mut act = resp.clicked().then_some(Action::Select { event: e.frame, frame: e.frame });

        let inner = rect.shrink2(vec2(8.0, 6.0));
        let mut child = ui.new_child(UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(Align::Center)));
        let ui = &mut child;
        if e.review.as_deref() == Some("reject") {
            ui.set_opacity(0.55);
        }

        let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        let color = theme::event_color(e).gamma_multiply(if theme::event_faded(e) { 0.45 } else { 1.0 });
        ui.painter().circle_filled(dot.center(), 4.0, color);

        fixed(ui, vec2(190.0, inner.height()), egui::Layout::top_down(Align::Min), |ui| {
            let seen = self.seen.contains(&e.frame) && e.review.is_none();
            ui.label(RichText::new(format!("{}{}", e.tc, if seen { " ·" } else { "" })).monospace().size(12.5));
            ui.add(egui::Label::new(RichText::new(self.label(e)).color(theme::TEXT_DIM).size(12.0)).truncate());
        });

        // Match: the reference first.
        if self.kind == Detector::Match {
            fixed(ui, vec2(TILE_W + 6.0, inner.height()), egui::Layout::top_down(Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                let (r, _) = ui.allocate_exact_size(vec2(TILE_W, TILE_H), Sense::hover());
                match refs.thumbs.get(&e.subkind) {
                    Some(t) => paint_tile(ui, r, t, Some(theme::event_color(e))),
                    None => {
                        ui.painter().rect_filled(r, 2.0, theme::BG_OVERLAY);
                    }
                }
                ui.label(RichText::new("ref").size(9.5).color(theme::TEXT_MUTED));
            });
        }

        let strip = strip_for(self.kind, e);
        let strip_w = max_tiles as f32 * (TILE_W + 4.0) + 60.0;
        fixed(ui, vec2(strip_w, inner.height()), egui::Layout::top_down(Align::Min), |ui| {
            let state = thumbs.get(strip.span);
            let failed = matches!(state, Some(StripState::Error));
            let rows: Vec<(String, Option<Vec<crate::thumbs::Tile>>)> = match state {
                Some(StripState::Ready(s)) => s.rows.iter().map(|(l, t)| (l.clone(), Some(t.clone()))).collect(),
                _ => (0..files).map(|r| (if r == 0 { "REF" } else { "DIST" }.to_string(), None)).collect(),
            };
            for (label, tiles) in rows {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if files > 1 {
                        fixed(ui, vec2(34.0, TILE_H), egui::Layout::left_to_right(Align::Center), |ui| {
                            ui.label(RichText::new(label.to_uppercase()).size(9.5).color(theme::TEXT_MUTED));
                        });
                    }
                    for (k, role) in strip.roles.iter().enumerate() {
                        let tile = tiles.as_ref().and_then(|t| t.get(k).cloned().flatten());
                        let frame = strip.span.start + k as i64;
                        fixed(ui, vec2(TILE_W, TILE_H + ROLE_H), egui::Layout::top_down(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            let (r, tr) = ui.allocate_exact_size(vec2(TILE_W, TILE_H), Sense::click());
                            match &tile {
                                Some(t) => {
                                    paint_tile(ui, r, t, (*role == "run").then_some(color));
                                    if frame == cursor {
                                        ui.painter().rect_stroke(r, 0.0, egui::Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Outside);
                                    }
                                    if tr.on_hover_text("Click to go to this frame").clicked() {
                                        act = Some(Action::Select { event: e.frame, frame });
                                    }
                                }
                                None => {
                                    ui.painter().rect_filled(r, 2.0, theme::BG_OVERLAY);
                                    if failed {
                                        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, "—", egui::FontId::proportional(12.0), theme::TEXT_MUTED);
                                    }
                                }
                            }
                            ui.label(RichText::new(self.role_label(role, e, k)).size(9.5).color(theme::TEXT_MUTED));
                        });
                    }
                    if strip.more > 0 {
                        ui.label(RichText::new(format!("+{}", strip.more)).monospace().size(11.0).color(theme::TEXT_DIM));
                    }
                });
            }
        });

        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            let review = e.review.as_deref();
            let rej = verdict_button(ui, false, review == Some("reject"));
            let acc = verdict_button(ui, true, review == Some("accept"));
            if acc.on_hover_text("Accept (y)").clicked() {
                act = Some(Action::Review(e.frame, if review == Some("accept") { None } else { Some("accept") }));
            }
            if rej.on_hover_text("Reject (x)").clicked() {
                act = Some(Action::Review(e.frame, if review == Some("reject") { None } else { Some("reject") }));
            }
            ui.add_space(10.0);
            let s = self.score(e, threshold);
            if !s.is_empty() {
                let tip = match self.kind {
                    Detector::Flash => "How big the flash is: the smaller of the change going in and coming out, as % of peak — the same units as the threshold slider. Above the slider = listed.",
                    Detector::Match => "similarity relative to the minimum",
                    Detector::Repeat => "",
                };
                let l = ui.label(RichText::new(s).monospace().size(12.5));
                if !tip.is_empty() {
                    l.on_hover_text(tip);
                }
            }
        });
        act
    }
}

fn paint_tile(ui: &Ui, r: Rect, t: &TextureHandle, outline: Option<Color32>) {
    let sz = t.size_vec2();
    let w = (TILE_H * sz.x / sz.y.max(1.0)).min(TILE_W);
    let img = Rect::from_center_size(r.center(), vec2(w, TILE_H));
    ui.painter().image(t.id(), img, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    if let Some(c) = outline {
        ui.painter().rect_stroke(img, 0.0, egui::Stroke::new(1.5, c), egui::StrokeKind::Inside);
    }
}

fn trim_zeros(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A cell of exactly `size`, whatever its contents measure (keeps the
/// columns of the event list aligned).
fn fixed(ui: &mut Ui, size: egui::Vec2, layout: egui::Layout, add: impl FnOnce(&mut Ui)) {
    let (r, _) = ui.allocate_exact_size(size, Sense::hover());
    let mut child = ui.new_child(UiBuilder::new().max_rect(r).layout(layout));
    child.set_clip_rect(r.intersect(ui.clip_rect()));
    add(&mut child);
}

/// ✓ / ✗ button, painted (the default fonts have no check marks).
fn verdict_button(ui: &mut Ui, accept: bool, on: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(26.0, 22.0), Sense::click());
    let color = match (on, accept) {
        (true, true) => theme::SUCCESS,
        (true, false) => theme::THRESHOLD,
        _ if resp.hovered() => theme::TEXT,
        _ => theme::TEXT_DIM,
    };
    let bg = if on { color.gamma_multiply(0.18) } else if resp.hovered() { theme::BG_HOVER } else { theme::BG_OVERLAY };
    ui.painter().rect_filled(r, 4.0, bg);
    let c = r.center();
    let s = egui::Stroke::new(1.8, color);
    if accept {
        ui.painter().line(vec![c + vec2(-5.0, 0.5), c + vec2(-1.5, 4.0), c + vec2(5.5, -4.0)], s);
    } else {
        ui.painter().line_segment([c + vec2(-4.5, -4.5), c + vec2(4.5, 4.5)], s);
        ui.painter().line_segment([c + vec2(-4.5, 4.5), c + vec2(4.5, -4.5)], s);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, frame: i64, length: u32) -> Event {
        Event { frame, tc: String::new(), length, kind: kind.into(), subkind: "white".into(), at_cut: false, score: 1.0, review: None, label: None }
    }

    #[test]
    fn strips_match_the_web_ui() {
        let s = strip_for(Detector::Flash, &ev("flash", 100, 1));
        assert_eq!((s.span.start, s.span.count, s.more), (99, 3, 0));
        assert_eq!(s.roles, vec!["prev", "run", "next"]);
        let s = strip_for(Detector::Flash, &ev("flash", 100, 7));
        assert_eq!((s.span.count, s.more), (6, 2));
        // Repeats show 3, matches 2, before "+n".
        let s = strip_for(Detector::Repeat, &ev("repeat", 10, 48));
        assert_eq!((s.span.count, s.more), (4, 45));
        let s = strip_for(Detector::Match, &ev("match", 10, 2));
        assert_eq!(s.roles, vec!["prev", "run", "run", "next"]);
    }

    #[test]
    fn neighbours_wrap_and_start_from_the_pick() {
        let mut t = DetectorTab::new(Detector::Flash);
        let mut evs = vec![ev("flash", 10, 1), ev("flash", 20, 1), ev("flash", 30, 1)];
        evs[1].review = Some("reject".into());
        t.result = Some(DetectResult { events: evs, threshold: 0.04, source: "levels".into(), detector: Detector::Flash, note: None });
        assert_eq!(t.neighbour(0, 1), Some(10));
        // From the pick (10), not the cursor.
        assert_eq!(t.neighbour(25, 1), Some(20));
        t.hide_rejected = true;
        assert_eq!(t.neighbour(0, 1), Some(30));
        assert_eq!(t.neighbour(0, 1), Some(10), "wraps");
        assert_eq!(t.neighbour(0, -1), Some(30), "wraps back");
        assert!(t.seen.contains(&10) && t.seen.contains(&30));
    }

    #[test]
    fn match_with_nothing_ticked_asks_nothing() {
        let mut t = DetectorTab::new(Detector::Match);
        assert!(t.request().is_none());
        assert_eq!(t.empty_result().threshold, 0.9);
        t.matching.references.push("abc".into());
        assert!(matches!(t.request(), Some(DetectRequest::Match(_))));
    }

    #[test]
    fn labels_and_scores() {
        let t = DetectorTab::new(Detector::Repeat);
        let mut e = ev("repeat", 1, 48);
        e.subkind = "hold".into();
        assert_eq!(t.label(&e), "held 48 fr");
        assert_eq!(t.score(&e, 0.0), "");
        let f = DetectorTab::new(Detector::Flash);
        let mut t = DetectorTab::new(Detector::Flash);
        t.timeline = true;
        let mut m = ev("mismatch", 1, 1);
        m.label = Some("clip3 (V1, first)".into());
        m.score = 2.5;
        assert_eq!(t.label(&m), "clip3 (V1, first)");
        assert_eq!(t.score(&m, 0.01), "2.50%");
        let mut er = ev("error", 1, 1);
        er.subkind = "grab_timeout".into();
        er.label = Some("clip4 (V1, last)".into());
        assert_eq!(t.label(&er), "clip4 (V1, last) · not checked: grab timeout");
        assert_eq!(t.score(&er, 0.01), "—");
        assert_eq!(t.role_label("run", &m, 1), "sample");
        let mut e = ev("flash", 1, 1);
        e.score = 15.0;
        e.at_cut = true;
        assert_eq!(f.label(&e), "white · 1 fr · at cut");
        assert_eq!(f.score(&e, 0.04), "60%");
        assert_eq!(trim_zeros(0.05), "0.05");
        assert_eq!(trim_zeros(0.1), "0.1");
    }
}
