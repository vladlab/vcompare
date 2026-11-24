//! The metric chart: a detail view over an overview minimap, painted
//! directly (no plotting crate) from the series' min/max pyramid.
//!
//! Behaviour follows MetricChart.svelte: wheel zoom around the mouse, key
//! zoom around the playhead, playhead auto-follow, click-drag scrub,
//! double-click reset, scrollbar pan when zoomed, minimap drag to pan or
//! to select a range, event bands with top ticks, threshold line, hatched
//! no-data zones outside the clips' overlap, points for sparse data.

use crate::data::Series;
use crate::theme;
use eframe::egui::{self, pos2, vec2, Color32, Mesh, Pos2, Rect, Sense, Shape, Stroke, Ui};
use std::time::{Duration, Instant};
use vcompare_service::engine::detect::Event;
use vcompare_service::engine::types::TimelineInfo;

const OVERVIEW_H: f32 = 60.0;
const SCROLLBAR_H: f32 = 12.0;
const LEFT_AXIS_W: f32 = 52.0;
const BOTTOM_AXIS_H: f32 = 20.0;
const MIN_RANGE: f64 = 10.0;
const GRID: Color32 = Color32::from_rgba_premultiplied(15, 15, 15, 15);
const AXIS_TEXT: Color32 = Color32::from_rgb(0x66, 0x66, 0x66);
const SELECT_FILL: Color32 = Color32::from_rgba_premultiplied(22, 36, 53, 64);
const SELECT_EDGE: Color32 = Color32::from_rgba_premultiplied(54, 86, 127, 153);

pub struct ChartProps<'a> {
    pub cursor: i64,
    pub events: &'a [Event],
    pub threshold: Option<f64>,
    pub timeline: Option<&'a TimelineInfo>,
    pub sparse: bool,
}

#[derive(Clone, Copy)]
enum MiniDrag {
    Pan { grab: f64 },
    Select { start: f64 },
}

#[derive(Default)]
pub struct Chart {
    /// Visible x range; `None` = everything.
    view: Option<(f64, f64)>,
    prev_cursor: i64,
    mini_drag: Option<MiniDrag>,
    bar_drag: Option<(f32, f64)>,
    /// Time spent in the last `show`.
    pub last_paint: Duration,
}

fn bounds(s: &Series, tl: Option<&TimelineInfo>) -> (f64, f64) {
    if let Some(t) = tl {
        return (t.span_start as f64, t.span_end as f64);
    }
    let a = s.frames.first().copied().unwrap_or(0) as f64;
    let b = s.frames.last().copied().unwrap_or(1) as f64;
    (a, b.max(a + 1.0))
}

impl Chart {
    pub fn reset(&mut self) {
        self.view = None;
    }

    fn visible(&self, b: (f64, f64)) -> (f64, f64) {
        self.view.unwrap_or(b)
    }

    fn is_zoomed(&self, b: (f64, f64)) -> bool {
        let (v0, v1) = self.visible(b);
        v1 - v0 < (b.1 - b.0) * 0.99
    }

    fn set_range(&mut self, b: (f64, f64), mut min: f64, mut max: f64) {
        if min < b.0 {
            max += b.0 - min;
            min = b.0;
        }
        if max > b.1 {
            min -= max - b.1;
            max = b.1;
        }
        let (min, max) = (min.max(b.0), max.min(b.1));
        if max - min < MIN_RANGE {
            return;
        }
        self.view = if max - min >= (b.1 - b.0) * 0.999 { None } else { Some((min, max)) };
    }

    fn zoom_around(&mut self, b: (f64, f64), anchor: f64, factor: f64) {
        let (v0, v1) = self.visible(b);
        let r = v1 - v0;
        let nr = (r * factor).max(MIN_RANGE);
        let t = ((anchor - v0) / r).clamp(0.0, 1.0);
        self.set_range(b, anchor - nr * t, anchor + nr * (1.0 - t));
    }

    /// Key zoom: around the playhead if it is on screen, else the centre.
    pub fn zoom_keys(&mut self, s: &Series, tl: Option<&TimelineInfo>, cursor: i64, factor: f64) {
        let b = bounds(s, tl);
        let (v0, v1) = self.visible(b);
        let c = cursor as f64;
        let anchor = if cursor >= 0 && c >= v0 && c <= v1 { c } else { (v0 + v1) / 2.0 };
        self.zoom_around(b, anchor, factor);
    }

    fn follow(&mut self, b: (f64, f64), cursor: i64) {
        if cursor < 0 || !self.is_zoomed(b) {
            return;
        }
        let (v0, v1) = self.visible(b);
        let c = cursor as f64;
        if c < v0 || c > v1 {
            let r = v1 - v0;
            self.set_range(b, c - r / 2.0, c + r / 2.0);
        }
    }

    /// Draw the chart in the space left in `ui`; returns a new cursor frame
    /// when the user scrubbed or clicked.
    pub fn show(&mut self, ui: &mut Ui, s: &Series, p: &ChartProps) -> Option<i64> {
        let t0 = Instant::now();
        let b = bounds(s, p.timeline);
        if p.cursor != self.prev_cursor {
            self.prev_cursor = p.cursor;
            self.follow(b, p.cursor);
        }

        let full = ui.available_rect_before_wrap();
        ui.allocate_rect(full, Sense::hover());
        let overview = Rect::from_min_max(pos2(full.left() + LEFT_AXIS_W, full.bottom() - OVERVIEW_H), full.right_bottom());
        let bar = Rect::from_min_max(
            pos2(overview.left(), overview.top() - SCROLLBAR_H - 2.0),
            pos2(overview.right(), overview.top() - 2.0),
        );
        let detail_outer = Rect::from_min_max(full.left_top(), pos2(full.right(), bar.top() - 2.0));
        let plot = Rect::from_min_max(
            pos2(detail_outer.left() + LEFT_AXIS_W, detail_outer.top() + 4.0),
            pos2(detail_outer.right() - 4.0, detail_outer.bottom() - BOTTOM_AXIS_H),
        );
        if plot.width() < 50.0 || plot.height() < 30.0 {
            return None;
        }
        let overview = Rect::from_min_max(pos2(plot.left(), overview.top()), pos2(plot.right(), overview.bottom()));
        let bar = Rect::from_min_max(pos2(plot.left(), bar.top()), pos2(plot.right(), bar.bottom()));

        let mut new_cursor = None;
        let id = ui.scope_id().with("metric_chart");

        // ── Detail interaction ──
        let resp = ui.interact(plot, id.with("detail"), Sense::click_and_drag());
        let (mut v0, mut v1) = self.visible(b);
        let to_frame = |x: f32, v0: f64, v1: f64| v0 + (x - plot.left()) as f64 / plot.width() as f64 * (v1 - v0);
        if resp.hovered() {
            let (wheel, ptr) = ui.input(|i| {
                let mut f = 1.0f64;
                for e in &i.events {
                    if let egui::Event::MouseWheel { unit, delta, .. } = e {
                        let lines = match unit {
                            egui::MouseWheelUnit::Point => delta.y as f64 / 50.0,
                            egui::MouseWheelUnit::Line => delta.y as f64,
                            egui::MouseWheelUnit::Page => delta.y as f64 * 4.0,
                        };
                        f *= 0.75f64.powf(lines);
                    }
                }
                (f, i.pointer.hover_pos())
            });
            if (wheel - 1.0).abs() > 1e-6 {
                if let Some(pp) = ptr {
                    self.zoom_around(b, to_frame(pp.x, v0, v1), wheel);
                    (v0, v1) = self.visible(b);
                }
            }
        }
        if resp.double_clicked() {
            self.reset();
            (v0, v1) = self.visible(b);
        } else if resp.is_pointer_button_down_on() || resp.clicked() {
            if let Some(pp) = resp.interact_pointer_pos() {
                if let Some(f) = s.nearest(to_frame(pp.x, v0, v1)) {
                    if f != p.cursor {
                        new_cursor = Some(f);
                    }
                }
            }
        }

        // ── Minimap interaction ──
        let mresp = ui.interact(overview, id.with("overview"), Sense::click_and_drag());
        let mframe = |x: f32| b.0 + ((x - overview.left()) / overview.width()).clamp(0.0, 1.0) as f64 * (b.1 - b.0);
        let mut selecting: Option<(f64, f64)> = None;
        // Anchor drags where the button went down, not where egui decided
        // it was a drag (a few px later), and follow the latest pointer.
        let (press, latest) = ui.input(|i| (i.pointer.press_origin(), i.pointer.latest_pos()));
        if mresp.drag_started() {
            if let Some(pp) = press.or(mresp.interact_pointer_pos()) {
                let f = mframe(pp.x);
                self.mini_drag = Some(if self.is_zoomed(b) && f >= v0 && f <= v1 {
                    MiniDrag::Pan { grab: f - v0 }
                } else {
                    MiniDrag::Select { start: f }
                });
            }
        }
        if let (Some(d), Some(pp)) = (self.mini_drag, latest.filter(|_| mresp.dragged() || mresp.drag_stopped())) {
            let f = mframe(pp.x);
            match d {
                MiniDrag::Pan { grab } => {
                    let r = v1 - v0;
                    self.set_range(b, f - grab, f - grab + r);
                    (v0, v1) = self.visible(b);
                }
                MiniDrag::Select { start } => selecting = Some((start.min(f), start.max(f))),
            }
        }
        if mresp.drag_stopped() {
            if let (Some(MiniDrag::Select { .. }), Some((a, z))) = (self.mini_drag, selecting) {
                let px = (z - a) / (b.1 - b.0) * overview.width() as f64;
                if px > 2.0 {
                    self.set_range(b, a, z);
                    (v0, v1) = self.visible(b);
                }
                selecting = None;
            }
            self.mini_drag = None;
        } else if mresp.clicked() {
            if let Some(f) = mresp.interact_pointer_pos().and_then(|pp| s.nearest(mframe(pp.x))) {
                new_cursor = Some(f);
            }
        }

        // ── Scrollbar ──
        let zoomed = self.is_zoomed(b);
        let total = b.1 - b.0;
        let thumb = {
            let w = ((v1 - v0) / total * bar.width() as f64).max(24.0) as f32;
            let x = bar.left() + ((v0 - b.0) / (total - (v1 - v0)).max(1e-9)) as f32 * (bar.width() - w);
            Rect::from_min_size(pos2(x, bar.top()), vec2(w, bar.height()))
        };
        if zoomed {
            let bresp = ui.interact(bar, id.with("bar"), Sense::click_and_drag());
            if bresp.drag_started() {
                if let Some(pp) = press.or(bresp.interact_pointer_pos()) {
                    let grab = if thumb.contains(pp) { pp.x } else { thumb.center().x };
                    self.bar_drag = Some((grab, v0));
                }
            }
            if let (Some((x0, start)), Some(pp)) = (self.bar_drag, latest.filter(|_| bresp.dragged() || bresp.drag_stopped())) {
                let df = (pp.x - x0) as f64 / (bar.width() - thumb.width()).max(1.0) as f64 * (total - (v1 - v0));
                let r = v1 - v0;
                self.set_range(b, start + df, start + df + r);
                (v0, v1) = self.visible(b);
            }
            if bresp.drag_stopped() {
                self.bar_drag = None;
            }
        }

        // ── Paint ──
        let painter = ui.painter();
        let ppp = ui.ctx().pixels_per_point();
        let (color, _) = theme::metric_style(&s.metric);
        let label = theme::metric_label(&s.metric);
        let (y0, y1) = y_range(&s.metric, s);

        painter.rect_filled(full, 0.0, theme::BG_SURFACE);
        // Grid + axes
        let xt = ticks(v0, v1, (plot.width() / 90.0).max(2.0) as usize);
        let yt = ticks(y0, y1, (plot.height() / 40.0).max(2.0) as usize);
        let font = egui::FontId::monospace(10.5);
        for &t in &xt {
            let x = plot.left() + ((t - v0) / (v1 - v0)) as f32 * plot.width();
            painter.line_segment([pos2(x, plot.top()), pos2(x, plot.bottom())], Stroke::new(1.0, GRID));
            painter.text(pos2(x, plot.bottom() + 3.0), egui::Align2::CENTER_TOP, fmt_tick(t, xt_step(&xt)), font.clone(), AXIS_TEXT);
        }
        for &t in &yt {
            let y = plot.bottom() - ((t - y0) / (y1 - y0)) as f32 * plot.height();
            painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, GRID));
            painter.text(pos2(plot.left() - 5.0, y), egui::Align2::RIGHT_CENTER, fmt_tick(t, xt_step(&yt)), font.clone(), AXIS_TEXT);
        }

        let dp = painter.with_clip_rect(plot);
        if let Some(tl) = p.timeline {
            no_data_zones(&dp, plot, tl, v0, v1, true);
        }
        draw_events(&dp, plot, p.events, v0, v1, 7.0);
        draw_series(&dp, plot, s, (v0, v1), (y0, y1), color, ppp, p.sparse);
        if let Some(th) = p.threshold {
            let y = plot.bottom() - ((th - y0) / (y1 - y0)) as f32 * plot.height();
            if y >= plot.top() && y <= plot.bottom() {
                dp.extend(Shape::dashed_line(&[pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, theme::THRESHOLD), 4.0, 4.0));
                let prec = if is_levels(&s.metric) { 0 } else if s.metric.starts_with("ssim") { 4 } else { 1 };
                dp.text(pos2(plot.left() + 4.0, y - 3.0), egui::Align2::LEFT_BOTTOM, format!("{th:.prec$}"), font.clone(), theme::THRESHOLD);
            }
        }
        if p.cursor >= 0 {
            let x = plot.left() + ((p.cursor as f64 - v0) / (v1 - v0)) as f32 * plot.width();
            if x >= plot.left() && x <= plot.right() {
                dp.line_segment([pos2(x, plot.top()), pos2(x, plot.bottom())], Stroke::new(1.5, Color32::from_white_alpha(204)));
                if let Some(v) = s.value_at(p.cursor) {
                    let y = plot.bottom() - ((v - y0) / (y1 - y0)) as f32 * plot.height();
                    dp.circle_filled(pos2(x, y), 4.0, Color32::WHITE);
                }
            }
        }
        // Metric label, on a backing so grid and events don't cross it.
        let g = dp.layout_no_wrap(label.clone(), egui::FontId::proportional(11.5), theme::TEXT_DIM);
        let lp = pos2(plot.left() + 8.0, plot.top() + 12.0);
        dp.rect_filled(Rect::from_min_size(lp, g.size()).expand2(vec2(5.0, 2.0)), 3.0, theme::BG_SURFACE.gamma_multiply(0.85));
        dp.galley(lp, g, theme::TEXT_DIM);
        // Hover readout
        if let Some(hp) = resp.hover_pos() {
            if let Some(f) = s.nearest(to_frame(hp.x, v0, v1)) {
                let v = s.value_at(f).unwrap_or(f64::NAN);
                let prec = if is_levels(&s.metric) { 1 } else if s.metric.starts_with("ssim") { 5 } else { 3 };
                painter.text(
                    pos2(plot.right() - 6.0, plot.top() + 2.0),
                    egui::Align2::RIGHT_TOP,
                    format!("frame {f}   {v:.prec$}"),
                    font.clone(),
                    theme::TEXT_DIM,
                );
            }
        }

        // Scrollbar
        if zoomed {
            painter.rect_filled(bar.shrink2(vec2(0.0, 3.0)), 3.0, theme::BG_OVERLAY);
            let hot = self.bar_drag.is_some() || ui.rect_contains_pointer(thumb);
            painter.rect_filled(thumb.shrink2(vec2(0.0, 1.0)), 5.0, Color32::from_white_alpha(if hot { 90 } else { 51 }));
        }

        // Overview
        let op = painter.with_clip_rect(overview);
        op.rect_filled(overview, 0.0, theme::BG_RAISED);
        if let Some(tl) = p.timeline {
            no_data_zones(&op, overview, tl, b.0, b.1, false);
        }
        draw_events(&op, overview, p.events, b.0, b.1, 4.0);
        let pad = ((s.max - s.min) * 0.05).max(1e-9);
        draw_series(&op, overview, s, b, (s.min - pad, s.max + pad), color.gamma_multiply(0.6), ppp, false);
        if p.cursor >= 0 {
            let x = overview.left() + ((p.cursor as f64 - b.0) / (b.1 - b.0)) as f32 * overview.width();
            op.line_segment([pos2(x, overview.top()), pos2(x, overview.bottom())], Stroke::new(1.5, Color32::from_white_alpha(128)));
        }
        let sel = selecting.or(if zoomed { Some((v0, v1)) } else { None });
        if let Some((a, z)) = sel {
            let xa = overview.left() + ((a - b.0) / (b.1 - b.0)) as f32 * overview.width();
            let xz = overview.left() + ((z - b.0) / (b.1 - b.0)) as f32 * overview.width();
            let r = Rect::from_min_max(pos2(xa, overview.top()), pos2(xz.max(xa + 1.0), overview.bottom()));
            op.rect_filled(r, 0.0, SELECT_FILL);
            op.line_segment([r.left_top(), r.left_bottom()], Stroke::new(1.0, SELECT_EDGE));
            op.line_segment([r.right_top(), r.right_bottom()], Stroke::new(1.0, SELECT_EDGE));
        }

        if resp.hovered() || mresp.hovered() {
            ui.ctx().set_cursor_icon(if resp.hovered() { egui::CursorIcon::Crosshair } else { egui::CursorIcon::Default });
        }
        self.last_paint = t0.elapsed();
        new_cursor
    }
}

fn is_levels(m: &str) -> bool {
    m.starts_with("y_") || m.starts_with("u_") || m.starts_with("v_")
}

/// Y range per metric (MetricChart.svelte `yRange`).
fn y_range(metric: &str, s: &Series) -> (f64, f64) {
    if metric == "vmaf" {
        return (0.0, 100.0);
    }
    if s.values.is_empty() {
        return (0.0, 100.0);
    }
    let (min, max) = (s.min, s.max);
    if metric.starts_with("diff") {
        let pad = if max * 0.05 != 0.0 { max * 0.05 } else { 0.5 };
        return (0.0, max + pad);
    }
    if metric.starts_with("ssim") {
        let pad = if (max - min) * 0.1 != 0.0 { (max - min) * 0.1 } else { 0.001 };
        return ((min - pad).max(0.0), (max + pad).min(1.0));
    }
    if is_levels(metric) {
        return (0.0, if max > 255.0 { 1023.0 } else { 255.0 });
    }
    let pad = if (max - min) * 0.05 != 0.0 { (max - min) * 0.05 } else { 1.0 };
    (min - pad, max + pad)
}

/// Line + fill for the visible part of `s`. Far zoomed out, each pixel
/// column draws the min..max of its frames (from the pyramid); zoomed in
/// past ~2 points per pixel, it draws the actual points.
fn draw_series(p: &egui::Painter, r: Rect, s: &Series, (x0, x1): (f64, f64), (y0, y1): (f64, f64), color: Color32, ppp: f32, points: bool) {
    if s.frames.is_empty() || x1 <= x0 || y1 <= y0 {
        return;
    }
    let fill = color.gamma_multiply(0.094);
    let px = |f: f64| r.left() + ((f - x0) / (x1 - x0)) as f32 * r.width();
    let py = |v: f64| r.bottom() - ((v - y0) / (y1 - y0)) as f32 * r.height();
    let i0 = s.lower_bound(x0).saturating_sub(1);
    let i1 = (s.lower_bound(x1) + 1).min(s.frames.len());
    let cols = (r.width() * ppp).round().max(1.0) as usize;
    let mut mesh = Mesh::default();

    if i1 - i0 > cols * 2 {
        let cw = 1.0 / ppp;
        let mut prev_last: Option<f64> = None;
        for c in 0..cols {
            let fa = x0 + (x1 - x0) * c as f64 / cols as f64;
            let fb = x0 + (x1 - x0) * (c + 1) as f64 / cols as f64;
            let (ia, ib) = (s.lower_bound(fa), s.lower_bound(fb));
            let Some(a) = s.pyramid.query(ia, ib) else {
                prev_last = None;
                continue;
            };
            // Join to the previous column like a line would.
            let (mut lo, mut hi) = (a.min, a.max);
            if let Some(v) = prev_last {
                lo = lo.min(v);
                hi = hi.max(v);
            }
            prev_last = Some(s.values[ib - 1]);
            let x = r.left() + c as f32 * cw;
            let (ylo, yhi) = (py(lo), py(hi));
            mesh.add_colored_rect(Rect::from_min_max(pos2(x, py(a.max)), pos2(x + cw, r.bottom())), fill);
            mesh.add_colored_rect(Rect::from_min_max(pos2(x, yhi.min(ylo - cw)), pos2(x + cw, ylo)), color);
        }
        p.add(Shape::mesh(mesh));
        return;
    }

    let pts: Vec<Pos2> = (i0..i1).filter(|&i| s.values[i].is_finite()).map(|i| pos2(px(s.frames[i] as f64), py(s.values[i]))).collect();
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(a, fill);
        mesh.colored_vertex(b, fill);
        mesh.colored_vertex(pos2(b.x, r.bottom()), fill);
        mesh.colored_vertex(pos2(a.x, r.bottom()), fill);
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    }
    p.add(Shape::mesh(mesh));
    p.add(Shape::line(pts.clone(), Stroke::new(1.0, color)));
    if points {
        for q in pts {
            p.circle_filled(q, 2.5, color);
        }
    }
}

/// Translucent band over each event's frames plus a tick at the top, so
/// one-frame events stay visible when zoomed far out.
fn draw_events(p: &egui::Painter, r: Rect, events: &[Event], x0: f64, x1: f64, tick_h: f32) {
    if events.is_empty() {
        return;
    }
    let px = |f: f64| r.left() + ((f - x0) / (x1 - x0)) as f32 * r.width();
    let start = events.partition_point(|e| ((e.frame + e.length as i64) as f64) < x0);
    let mut mesh = Mesh::default();
    for e in &events[start..] {
        if e.frame as f64 > x1 {
            break;
        }
        let a = px(e.frame as f64);
        let z = px((e.frame + e.length as i64) as f64).max(a + 1.5);
        let base = theme::event_color(e).gamma_multiply(if theme::event_faded(e) { 0.45 } else { 1.0 });
        mesh.add_colored_rect(Rect::from_min_max(pos2(a, r.top()), pos2(z, r.bottom())), base.gamma_multiply(0.18));
        mesh.add_colored_rect(Rect::from_min_max(pos2(a, r.top()), pos2(z.max(a + 2.0), r.top() + tick_h)), base);
    }
    p.add(Shape::mesh(mesh));
}

/// Hatched zones where only one clip has media.
fn no_data_zones(p: &egui::Painter, r: Rect, tl: &TimelineInfo, x0: f64, x1: f64, label: bool) {
    let px = |f: i64| r.left() + ((f as f64 - x0) / (x1 - x0)) as f32 * r.width();
    let zones = [
        (tl.span_start < tl.overlap_start).then(|| (px(tl.span_start), px(tl.overlap_start), false)),
        (tl.overlap_end < tl.span_end).then(|| (px(tl.overlap_end), px(tl.span_end), true)),
    ];
    for (a, z, data_left) in zones.into_iter().flatten() {
        let (a, z) = (a.max(r.left()), z.min(r.right()));
        if z <= a {
            continue;
        }
        let zr = Rect::from_min_max(pos2(a, r.top()), pos2(z, r.bottom()));
        let zp = p.with_clip_rect(zr.intersect(p.clip_rect()));
        zp.rect_filled(zr, 0.0, Color32::from_white_alpha(15));
        let h = r.height();
        let mut o = -h;
        while o < (z - a) + h {
            zp.line_segment([pos2(a + o, r.bottom()), pos2(a + o + h, r.top())], Stroke::new(1.0, Color32::from_white_alpha(38)));
            o += 10.0;
        }
        if label && z - a > 60.0 {
            zp.text(zr.center(), egui::Align2::CENTER_CENTER, "no data", egui::FontId::monospace(11.0), Color32::from_white_alpha(77));
        }
        let bx = if data_left { a } else { z };
        p.extend(Shape::dashed_line(&[pos2(bx, r.top()), pos2(bx, r.bottom())], Stroke::new(1.5, Color32::from_white_alpha(64)), 4.0, 4.0));
    }
}

/// Round tick values covering [a, b], about `n` of them.
fn ticks(a: f64, b: f64, n: usize) -> Vec<f64> {
    if !(b > a) || n == 0 {
        return vec![];
    }
    let raw = (b - a) / n as f64;
    let mag = 10f64.powf(raw.log10().floor());
    // Smallest round step not much below the raw one (so about n ticks).
    let step = [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw * 0.75).unwrap_or(10.0 * mag);
    let mut t = (a / step).ceil() * step;
    let mut out = vec![];
    while t <= b + step * 1e-9 {
        out.push(t);
        t += step;
    }
    out
}

fn xt_step(t: &[f64]) -> f64 {
    if t.len() >= 2 { t[1] - t[0] } else { 1.0 }
}

fn fmt_tick(v: f64, step: f64) -> String {
    let dec = if step >= 1.0 { 0 } else { (-step.log10()).ceil() as usize };
    format!("{v:.dec$}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_are_round_and_cover() {
        assert_eq!(ticks(0.0, 1023.0, 5), vec![0.0, 200.0, 400.0, 600.0, 800.0, 1000.0]);
        let t = ticks(12345.0, 12391.0, 4);
        assert!(t.iter().all(|v| v % 10.0 == 0.0) && t[0] >= 12345.0 && *t.last().unwrap() <= 12391.0);
        assert!(ticks(5.0, 5.0, 3).is_empty());
    }

    #[test]
    fn zoom_clamps_and_keeps_min_range() {
        let mut c = Chart::default();
        let b = (0.0, 1000.0);
        c.zoom_around(b, 500.0, 0.5);
        assert_eq!(c.view, Some((250.0, 750.0)));
        for _ in 0..20 {
            c.zoom_around(b, 500.0, 0.5);
        }
        let (a, z) = c.view.unwrap();
        assert!(z - a >= MIN_RANGE);
        c.set_range(b, -100.0, 100.0);
        assert_eq!(c.view, Some((0.0, 200.0)));
        c.zoom_around(b, 0.0, 100.0);
        assert_eq!(c.view, None);
    }

    #[test]
    fn follow_recentres_only_when_off_screen() {
        let mut c = Chart::default();
        let b = (0.0, 1000.0);
        c.set_range(b, 100.0, 200.0);
        c.follow(b, 150);
        assert_eq!(c.view, Some((100.0, 200.0)));
        c.follow(b, 700);
        assert_eq!(c.view, Some((650.0, 750.0)));
    }
}
