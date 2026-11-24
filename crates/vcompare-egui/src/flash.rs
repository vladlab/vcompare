//! The Flash tab: detector controls and the event list with in-row
//! thumbnail strips (port of the flash parts of DetectorTab.svelte and
//! EventList.svelte).

use crate::theme;
use crate::thumbs::{Span, StripState, ThumbLoader};
use eframe::egui::{self, pos2, vec2, Align, Color32, Rect, RichText, Sense, Ui, UiBuilder};
use std::collections::HashSet;
use vcompare_service::engine::detect::{DetectResult, Event, FlashParams};

/// Frames of a run shown as tiles at most (the rest is "+N").
const RUN_TILES: u32 = 5;
const TILE_H: f32 = 54.0;
const TILE_W: f32 = 96.0;
const ROLE_H: f32 = 13.0;

pub enum FlashAction {
    /// A row was clicked: move the cursor there.
    Select(i64),
    /// Accept / reject / clear the event starting at this frame.
    Review(i64, Option<&'static str>),
    /// A control is moving: hold thumbnails back.
    Adjusting,
    /// Parameters changed: run detection again.
    Changed,
}

/// The tiles of an event's strip: the frame before, the run (capped) and
/// the frame after when the run is shown whole.
pub struct StripLayout {
    pub span: Span,
    pub roles: Vec<&'static str>,
    pub more: u32,
}

pub fn strip_for(e: &Event) -> StripLayout {
    let shown = e.length.min(RUN_TILES);
    let whole = shown == e.length;
    let mut roles = vec!["prev"];
    roles.extend(std::iter::repeat_n("run", shown as usize));
    if whole {
        roles.push("next");
    }
    StripLayout { span: Span { start: e.frame - 1, count: roles.len() as u32 }, roles, more: e.length - shown }
}

pub struct FlashTab {
    pub params: FlashParams,
    pub hide_rejected: bool,
    pub result: Option<DetectResult>,
    pub error: Option<String>,
    /// Frame of the picked event (row highlight, y / x target).
    pub selected: Option<i64>,
    pub seen: HashSet<i64>,
    /// Slider value while dragging (% of peak); applied on release.
    drag_pct: Option<f64>,
    scroll_to: Option<usize>,
    viewport_h: f32,
}

impl Default for FlashTab {
    fn default() -> Self {
        Self {
            params: FlashParams::default(),
            hide_rejected: false,
            result: None,
            error: None,
            selected: None,
            seen: HashSet::new(),
            drag_pct: None,
            scroll_to: None,
            viewport_h: 400.0,
        }
    }
}

impl FlashTab {
    /// Forget results (another analysis was opened); keep the settings.
    pub fn reset(&mut self) {
        self.result = None;
        self.error = None;
        self.selected = None;
        self.seen.clear();
    }

    /// The events the list shows (and the chart marks).
    pub fn visible(&self) -> Vec<&Event> {
        self.result
            .as_ref()
            .map(|r| r.events.iter().filter(|e| !(self.hide_rejected && e.review.as_deref() == Some("reject"))).collect())
            .unwrap_or_default()
    }

    /// Next (`dir` > 0) or previous event relative to `cursor`.
    pub fn neighbour(&mut self, cursor: i64, dir: i32) -> Option<i64> {
        let ev = self.visible();
        let f = if dir > 0 {
            ev.iter().find(|e| e.frame > cursor).map(|e| e.frame)
        } else {
            ev.iter().rev().find(|e| e.frame < cursor).map(|e| e.frame)
        }?;
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

    pub fn set_review(&mut self, frame: i64, review: Option<&'static str>) {
        if let Some(r) = self.result.as_mut() {
            if let Some(e) = r.events.iter_mut().find(|e| e.frame == frame) {
                e.review = review.map(String::from);
            }
        }
    }

    pub fn show(&mut self, ui: &mut Ui, thumbs: &mut ThumbLoader, files: usize) -> Vec<FlashAction> {
        let mut acts = Vec::new();
        self.toolbar(ui, &mut acts);
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
            ui.label(RichText::new("No flash frames found.").color(theme::TEXT_MUTED));
            return acts;
        }

        let max_tiles = self.params.max_run.min(RUN_TILES) + 2;
        let row_h = files as f32 * (TILE_H + ROLE_H + 2.0) + 12.0;
        let mut sa = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("flash_events");
        if let Some(i) = self.scroll_to.take() {
            sa = sa.vertical_scroll_offset((i as f32 * row_h - self.viewport_h / 2.0 + row_h / 2.0).max(0.0));
        }
        let mut wanted: Vec<Span> = Vec::new();
        let out = sa.show_rows(ui, row_h, events.len(), |ui, range| {
            // Load order: picked event, visible rows, a screenful below.
            if let Some(i) = self.selected.and_then(|f| events.iter().position(|e| e.frame == f)) {
                wanted.push(strip_for(&events[i]).span);
            }
            let below = range.end..(range.end + range.len()).min(events.len());
            for i in range.clone().chain(below) {
                wanted.push(strip_for(&events[i]).span);
            }
            let spans: Vec<Span> = range.clone().map(|i| strip_for(&events[i]).span).collect();
            thumbs.forget_errors(&spans);
            for i in range {
                let e = &events[i];
                if let Some(a) = self.row(ui, e, row_h, threshold, max_tiles, thumbs) {
                    acts.push(a);
                }
            }
        });
        self.viewport_h = out.inner_rect.height();
        thumbs.want(&wanted);
        acts
    }

    fn toolbar(&mut self, ui: &mut Ui, acts: &mut Vec<FlashAction>) {
        ui.horizontal(|ui| {
            let mut auto = self.params.threshold.is_none();
            if ui
                .checkbox(&mut auto, "auto")
                .on_hover_text("Derive the threshold from the footage's own frame-to-frame noise")
                .changed()
            {
                acts.push(FlashAction::Adjusting);
                self.params.threshold = if auto { None } else { Some(self.result.as_ref().map(|r| r.threshold).unwrap_or(0.04)) };
                acts.push(FlashAction::Changed);
            }
            let current = self.params.threshold.or(self.result.as_ref().map(|r| r.threshold)).unwrap_or(0.04);
            let mut pct = self.drag_pct.unwrap_or((current * 1000.0).round() / 10.0);
            let r = ui
                .add_enabled(
                    !auto,
                    egui::Slider::new(&mut pct, 0.5..=40.0).step_by(0.5).suffix("%").fixed_decimals(1),
                )
                .on_hover_text("Minimum frame-to-frame change, as % of signal peak. Applies when you let go.");
            if r.dragged() {
                self.drag_pct = Some(pct);
                acts.push(FlashAction::Adjusting);
            }
            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                self.drag_pct = None;
                self.params.threshold = Some(pct.max(0.1) / 100.0);
                acts.push(FlashAction::Adjusting);
                acts.push(FlashAction::Changed);
            }

            ui.label("≤");
            let mut mr = self.params.max_run;
            egui::ComboBox::from_id_salt("flash_max_run").width(44.0).selected_text(mr.to_string()).show_ui(ui, |ui| {
                for n in 1..=5 {
                    ui.selectable_value(&mut mr, n, n.to_string());
                }
            });
            ui.label("fr");
            if mr != self.params.max_run {
                self.params.max_run = mr;
                acts.push(FlashAction::Adjusting);
                acts.push(FlashAction::Changed);
            }
            if ui
                .checkbox(&mut self.params.exclude_at_cut, "hide at-cut")
                .on_hover_text("Hide short inserts that sit on a cut (fast montage, etc.)")
                .changed()
            {
                acts.push(FlashAction::Adjusting);
                acts.push(FlashAction::Changed);
            }
            ui.checkbox(&mut self.hide_rejected, "hide rejected").on_hover_text("Hide events you've rejected");

            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if let Some(r) = &self.result {
                    let (mut acc, mut rej) = (0, 0);
                    for e in &r.events {
                        match e.review.as_deref() {
                            Some("accept") => acc += 1,
                            Some("reject") => rej += 1,
                            _ => {}
                        }
                    }
                    let mut parts: Vec<(String, usize)> = Vec::new();
                    let mut at_cut = 0;
                    for e in self.visible() {
                        match parts.iter_mut().find(|p| p.0 == e.subkind) {
                            Some(p) => p.1 += 1,
                            None => parts.push((e.subkind.clone(), 1)),
                        }
                        at_cut += e.at_cut as usize;
                    }
                    let n: usize = parts.iter().map(|p| p.1).sum();
                    let mut text = format!("{n} event{}", if n == 1 { "" } else { "s" });
                    for (k, c) in &parts {
                        text += &format!(" · {c} {k}");
                    }
                    if at_cut > 0 {
                        text += &format!(" · {at_cut} at cut");
                    }
                    if acc + rej > 0 {
                        text += &format!("   {acc} accepted · {rej} rejected");
                    }
                    ui.label(RichText::new(text).color(theme::TEXT_DIM).size(12.0));
                }
            });
        });
    }

    fn row(&mut self, ui: &mut Ui, e: &Event, row_h: f32, threshold: f64, max_tiles: u32, thumbs: &mut ThumbLoader) -> Option<FlashAction> {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
        let selected = self.selected == Some(e.frame);
        let bg = if selected {
            theme::BG_ACTIVE
        } else if resp.hovered() {
            theme::BG_HOVER
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect.shrink2(vec2(0.0, 1.0)), 3.0, bg);
        let mut act = None;
        if resp.clicked() {
            act = Some(FlashAction::Select(e.frame));
        }

        let inner = rect.shrink2(vec2(8.0, 6.0));
        let mut child = ui.new_child(UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(Align::Center)));
        let ui = &mut child;

        // Dot
        let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        let color = theme::event_color(e).gamma_multiply(if theme::event_faded(e) { 0.45 } else { 1.0 });
        ui.painter().circle_filled(dot.center(), 4.0, color);

        // Timecode + description
        fixed(ui, vec2(190.0, inner.height()), egui::Layout::top_down(Align::Min), |ui| {
            let seen = self.seen.contains(&e.frame) && e.review.is_none();
            ui.label(RichText::new(format!("{}{}", e.tc, if seen { " ·" } else { "" })).monospace().size(12.5));
            let len = if e.length == 1 { "1 fr".to_string() } else { format!("{} fr", e.length) };
            let desc = format!("{} · {len}{}", e.subkind, if e.at_cut { " · at cut" } else { "" });
            ui.label(RichText::new(desc).color(theme::TEXT_DIM).size(12.0));
        });

        // Strip
        let strip = strip_for(e);
        let strip_w = max_tiles as f32 * (TILE_W + 4.0) + 28.0;
        fixed(ui, vec2(strip_w, inner.height()), egui::Layout::top_down(Align::Min), |ui| {
            let state = thumbs.get(strip.span);
            let rows: Vec<Option<Vec<crate::thumbs::Tile>>> = match state {
                Some(StripState::Ready(s)) => s.rows.iter().map(|(_, t)| Some(t.clone())).collect(),
                _ => vec![None],
            };
            let failed = matches!(state, Some(StripState::Error));
            for tiles in rows {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (k, role) in strip.roles.iter().enumerate() {
                        let tile = tiles.as_ref().and_then(|t| t.get(k).cloned().flatten());
                        let label = match *role {
                            "run" if e.length > 1 => format!("flash {}", k),
                            "run" => "flash".into(),
                            r => r.into(),
                        };
                        ui.allocate_ui_with_layout(vec2(TILE_W, TILE_H + ROLE_H), egui::Layout::top_down(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            let (r, _) = ui.allocate_exact_size(vec2(TILE_W, TILE_H), Sense::hover());
                            match tile {
                                Some(t) => {
                                    let sz = t.size_vec2();
                                    let w = (TILE_H * sz.x / sz.y.max(1.0)).min(TILE_W);
                                    let img = Rect::from_center_size(r.center(), vec2(w, TILE_H));
                                    ui.painter().image(t.id(), img, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                                    if *role == "run" {
                                        ui.painter().rect_stroke(img, 0.0, egui::Stroke::new(1.5, color), egui::StrokeKind::Inside);
                                    }
                                }
                                None => {
                                    ui.painter().rect_filled(r, 2.0, theme::BG_OVERLAY);
                                    if failed {
                                        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, "—", egui::FontId::proportional(12.0), theme::TEXT_MUTED);
                                    }
                                }
                            }
                            ui.label(RichText::new(label).size(9.5).color(theme::TEXT_MUTED));
                        });
                    }
                    if strip.more > 0 {
                        ui.label(RichText::new(format!("+{}", strip.more)).monospace().size(11.0).color(theme::TEXT_DIM));
                    }
                });
            }
        });

        // Score + verdict, right-aligned
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            let review = e.review.as_deref();
            let rej = verdict_button(ui, false, review == Some("reject"));
            let acc = verdict_button(ui, true, review == Some("accept"));
            if acc.on_hover_text("Accept (y)").clicked() {
                act = Some(FlashAction::Review(e.frame, if review == Some("accept") { None } else { Some("accept") }));
            }
            if rej.on_hover_text("Reject (x)").clicked() {
                act = Some(FlashAction::Review(e.frame, if review == Some("reject") { None } else { Some("reject") }));
            }
            ui.add_space(10.0);
            let pct = e.score * threshold * 100.0;
            let score = format!("{pct:.*}%", if pct < 10.0 { 1 } else { 0 });
            ui.label(RichText::new(score).monospace().size(12.5)).on_hover_text(
                "How big the flash is: the smaller of the change going in and coming out, as % of peak — the same units as the threshold slider. Above the slider = listed.",
            );
        });
        act
    }
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

    fn ev(frame: i64, length: u32) -> Event {
        Event { frame, tc: String::new(), length, kind: "flash".into(), subkind: "white".into(), at_cut: false, score: 1.0, review: None, label: None }
    }

    #[test]
    fn strips_match_the_web_ui() {
        let s = strip_for(&ev(100, 1));
        assert_eq!((s.span.start, s.span.count, s.more), (99, 3, 0));
        assert_eq!(s.roles, vec!["prev", "run", "next"]);
        let s = strip_for(&ev(100, 7));
        assert_eq!((s.span.count, s.more), (6, 2));
        assert_eq!(s.roles.last(), Some(&"run"));
    }

    #[test]
    fn neighbours_skip_hidden_rejects() {
        let mut t = FlashTab::default();
        let mut evs = vec![ev(10, 1), ev(20, 1), ev(30, 1)];
        evs[1].review = Some("reject".into());
        t.result = Some(DetectResult { events: evs, threshold: 0.04, source: "levels".into(), detector: vcompare_service::engine::detect::Detector::Flash, note: None });
        assert_eq!(t.neighbour(10, 1), Some(20));
        t.hide_rejected = true;
        assert_eq!(t.neighbour(10, 1), Some(30));
        assert_eq!(t.neighbour(30, -1), Some(10));
        assert_eq!(t.neighbour(30, 1), None);
        assert!(t.seen.contains(&10));
    }
}
