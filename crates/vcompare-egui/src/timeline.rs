//! Timeline mode (file vs Resolve's SDI output) and the comparison
//! timeline bar. Ports of TimelineSetup.svelte, TimelineReport.svelte and
//! TimelineBar.svelte.

use crate::theme;
use eframe::egui::{self, pos2, vec2, Color32, Rect, RichText, Sense, Stroke, Ui};
use serde_json::Value;
use std::collections::BTreeSet;
use vcompare_service::engine::detect::Event;
use vcompare_service::engine::types::TimelineInfo;

pub enum SetupAction {
    Connect,
    Start { tracks: Vec<u32>, offset: i64, dry_run: bool },
}

/// The timeline mode's setup on the start screen: connect to Resolve,
/// tick the video tracks to sample, start.
#[derive(Default)]
pub struct Setup {
    /// `timeline_tracks` reply.
    pub reply: Option<Value>,
    pub connecting: bool,
    pub error: Option<String>,
    ticked: BTreeSet<u32>,
    offset: i64,
    dry_run: bool,
}

fn tracks(reply: &Value) -> Vec<&Value> {
    reply["tracks"].as_array().map(|a| a.iter().collect()).unwrap_or_default()
}

impl Setup {
    /// A `timeline_tracks` reply arrived. Keep the operator's ticks when the
    /// tracks still exist, otherwise start from the enabled tracks with clips.
    pub fn connected(&mut self, r: Result<Value, String>) {
        self.connecting = false;
        match r {
            Ok(v) => {
                let idx: Vec<u32> = tracks(&v).iter().filter_map(|t| t["index"].as_u64().map(|i| i as u32)).collect();
                let keep: BTreeSet<u32> = self.ticked.iter().copied().filter(|i| idx.contains(i)).collect();
                self.ticked = if !keep.is_empty() {
                    keep
                } else {
                    tracks(&v)
                        .iter()
                        .filter(|t| t["enabled"].as_bool() != Some(false) && t["items"].as_u64().unwrap_or(0) > 0)
                        .filter_map(|t| t["index"].as_u64().map(|i| i as u32))
                        .collect()
                };
                self.error = None;
                self.reply = Some(v);
            }
            Err(e) => {
                self.reply = None;
                self.error = Some(e);
            }
        }
    }

    fn items(&self) -> u64 {
        self.reply
            .as_ref()
            .map(|r| {
                tracks(r)
                    .iter()
                    .filter(|t| t["index"].as_u64().is_some_and(|i| self.ticked.contains(&(i as u32))))
                    .map(|t| t["items"].as_u64().unwrap_or(0))
                    .sum()
            })
            .unwrap_or(0)
    }

    fn grabber<'a>(&'a self, caps_grabber: Option<&'a str>) -> Option<&'a str> {
        self.reply.as_ref().and_then(|r| r["grabber"].as_str()).or(caps_grabber)
    }

    /// Why Start can't run yet, if it can't.
    fn blocker(&self, file_picked: bool, grabber: Option<&str>) -> Option<&'static str> {
        if !file_picked {
            Some("Choose the deliverable file")
        } else if grabber.is_none() {
            Some("scopetap_grab isn't installed (or set VCOMPARE_GRABBER)")
        } else if self.reply.is_none() {
            Some("Connect to Resolve")
        } else if self.ticked.is_empty() {
            Some("Tick at least one track")
        } else if self.items() == 0 {
            Some("The ticked tracks have no clips")
        } else {
            None
        }
    }

    pub fn show(&mut self, ui: &mut Ui, file_picked: bool, caps_grabber: Option<&str>) -> Option<SetupAction> {
        let mut act = None;
        ui.horizontal(|ui| {
            let label = if self.connecting {
                "Connecting…"
            } else if self.reply.is_some() {
                "Refresh from Resolve"
            } else {
                "Connect to Resolve"
            };
            if ui.add_enabled(!self.connecting, egui::Button::new(label)).clicked() {
                self.connecting = true;
                act = Some(SetupAction::Connect);
            }
            if let Some(r) = &self.reply {
                let tl = &r["timeline"];
                let text = format!(
                    "{} · {} fps{} · starts {}",
                    r["target"].as_str().unwrap_or("?"),
                    tl["fps"].as_str().unwrap_or("?"),
                    if tl["drop_frame"].as_bool() == Some(true) { " DF" } else { "" },
                    tl["start_tc"].as_str().unwrap_or("?")
                );
                ui.add(egui::Label::new(RichText::new(text).monospace().size(11.5).color(theme::TEXT_DIM)).truncate());
            }
        });
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(theme::THRESHOLD).size(12.0));
        }
        if let Some(r) = self.reply.clone() {
            egui::Frame::NONE.stroke(Stroke::new(1.0, Color32::from_gray(0x2a))).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                    let ts = tracks(&r);
                    if ts.is_empty() {
                        ui.label(RichText::new("This timeline has no video tracks.").color(theme::TEXT_DIM));
                    }
                    for t in ts.iter().rev() {
                        let Some(i) = t["index"].as_u64().map(|i| i as u32) else { continue };
                        let items = t["items"].as_u64().unwrap_or(0);
                        let off = t["enabled"].as_bool() == Some(false);
                        ui.horizontal(|ui| {
                            let mut on = self.ticked.contains(&i);
                            if ui.checkbox(&mut on, RichText::new(format!("V{i}")).monospace()).changed() {
                                if on {
                                    self.ticked.insert(i);
                                } else {
                                    self.ticked.remove(&i);
                                }
                            }
                            let name = RichText::new(t["name"].as_str().unwrap_or(""));
                            ui.label(if off { name.color(theme::TEXT_MUTED) } else { name });
                            ui.label(
                                RichText::new(format!(
                                    "{items} clip{}{}",
                                    if items == 1 { "" } else { "s" },
                                    if off { " · track disabled" } else { "" }
                                ))
                                .monospace()
                                .size(11.5)
                                .color(theme::TEXT_DIM),
                            );
                        });
                    }
                });
            });
            let n = self.items();
            ui.label(
                RichText::new(format!("{n} clip{}: up to {} samples (first, middle and last frame of each)", if n == 1 { "" } else { "s" }, n * 3))
                    .size(12.0)
                    .color(theme::TEXT_DIM),
            );
        }
        ui.horizontal(|ui| {
            ui.label("Offset").on_hover_text("Positive: the picture is that many frames later in the file than its timecode says");
            ui.add(egui::DragValue::new(&mut self.offset).speed(0.2));
            ui.label("frames");
            ui.add_space(12.0);
            ui.checkbox(&mut self.dry_run, "Dry run (timing only)")
                .on_hover_text("Seek and grab every sample but compare nothing and save nothing: measures how long a real run would take");
        });
        let grabber = self.grabber(caps_grabber).map(String::from);
        match &grabber {
            Some(g) => ui.label(RichText::new(format!("grabber: {g}")).monospace().size(11.5).color(theme::TEXT_DIM)),
            None => ui.label(
                RichText::new("scopetap_grab not found: install scopetap (or set VCOMPARE_GRABBER) and run scopetap_stream --scopes on the SDI loop")
                    .monospace()
                    .size(11.5)
                    .color(theme::THRESHOLD),
            ),
        };
        ui.add_space(6.0);
        let blocker = self.blocker(file_picked, grabber.as_deref());
        ui.horizontal(|ui| {
            let label = if self.dry_run { "Dry run" } else { "Check timeline" };
            let b = ui.add_enabled(blocker.is_none(), egui::Button::new(RichText::new(label).strong()).min_size(vec2(140.0, 30.0)));
            if b.clicked() {
                act = Some(SetupAction::Start { tracks: self.ticked.iter().copied().collect(), offset: self.offset, dry_run: self.dry_run });
            }
            if let Some(why) = blocker {
                ui.label(RichText::new(why).size(12.0).color(theme::TEXT_DIM));
            }
        });
        ui.add_space(6.0);
        egui::CollapsingHeader::new(RichText::new("Before you run: Resolve state that shows up as differences").size(12.0).color(theme::TEXT_DIM))
            .id_salt("tl_checklist")
            .show(ui, |ui| {
                for l in [
                    "Bypass, wipes / split screen, highlight or reference stills on the output",
                    "Timeline proxy or performance mode (Playback › Timeline Proxy Resolution)",
                    "A stale render cache",
                    "LUTs or calibration applied only to the monitor output (Project Settings › Color Management › Lookup Tables)",
                    "Data levels on the SDI output different from the render",
                ] {
                    ui.label(RichText::new(format!("•  {l}")).size(12.0).color(theme::TEXT_DIM));
                }
            });
        act
    }
}

/// What a timeline run looked at and how it went. `full`: all the
/// diagnostics open (dry runs, the Report tab); otherwise just warnings.
pub fn report(ui: &mut Ui, r: &Value, full: bool) {
    let row = |ui: &mut Ui, k: &str, v: String, dim: Option<String>| {
        ui.horizontal(|ui| {
            ui.add_sized(vec2(64.0, 14.0), egui::Label::new(RichText::new(k).size(11.5).color(theme::TEXT_MUTED)));
            ui.label(RichText::new(v).monospace().size(11.5));
            if let Some(d) = dim {
                ui.label(RichText::new(d).monospace().size(11.5).color(theme::TEXT_DIM));
            }
        });
    };
    let res = &r["resolve"];
    let tracks: Vec<String> = res["tracks"].as_array().map(|a| a.iter().filter_map(|t| t.as_u64()).map(|t| format!("V{t}")).collect()).unwrap_or_default();
    let off = r["offset"].as_i64().filter(|o| *o != 0).map(|o| format!(" · offset {o:+}")).unwrap_or_default();
    row(ui, "Resolve", res["target"].as_str().unwrap_or("?").to_string(), Some(format!("· {}{off}", tracks.join(", "))));
    let s = &r["signal"];
    if s.is_object() {
        row(
            ui,
            "SDI",
            format!(
                "{}×{} {}, levels {}{}",
                s["width"],
                s["height"],
                s["name"].as_str().unwrap_or("?"),
                s["levels"].as_str().unwrap_or("?"),
                if s["levels_auto"].as_bool() == Some(true) { " (auto)" } else { "" }
            ),
            None,
        );
    } else {
        row(ui, "SDI", "no frame grabbed".into(), None);
    }
    let f = &r["file_signal"];
    if f.is_object() {
        let q = |b: bool| if b { "" } else { "?" };
        row(
            ui,
            "File",
            format!(
                "{}×{} {}, {}{} {}{}{}",
                f["width"],
                f["height"],
                f["pix_fmt"].as_str().unwrap_or("?"),
                f["matrix"].as_str().unwrap_or("?"),
                q(f["matrix_tagged"].as_bool() == Some(true)),
                if f["full_range"].as_bool() == Some(true) { "full" } else { "limited" },
                q(f["range_tagged"].as_bool() == Some(true)),
                f["transfer"].as_str().map(|t| format!(", {t}")).unwrap_or_default()
            ),
            None,
        );
    }
    const ORDER: [(&str, &str); 8] = [
        ("ok", "compared"),
        ("grabbed", "grabbed"),
        ("not_in_file", "outside the file"),
        ("grab_timeout", "never settled"),
        ("grab_error", "grab failed"),
        ("seek_error", "seek refused"),
        ("compare_error", "compare failed"),
        ("wrong_tc", "wrong timecode"),
    ];
    if let Some(c) = r["counts"].as_object() {
        let mut job = egui::text::LayoutJob::default();
        let mut first = true;
        let mut keys: Vec<&String> = c.keys().collect();
        keys.sort_by_key(|k| ORDER.iter().position(|o| o.0 == k.as_str()).unwrap_or(99));
        for k in keys {
            let label = ORDER.iter().find(|o| o.0 == k.as_str()).map(|o| o.1).unwrap_or(k.as_str());
            let warn = !["ok", "grabbed", "not_in_file"].contains(&k.as_str());
            if !first {
                job.append(" · ", 0.0, egui::TextFormat::simple(egui::FontId::monospace(11.5), theme::TEXT_DIM));
            }
            first = false;
            job.append(
                &format!("{} {label}", c[k]),
                0.0,
                egui::TextFormat::simple(egui::FontId::monospace(11.5), if warn { theme::WARNING } else { theme::TEXT }),
            );
        }
        ui.horizontal(|ui| {
            ui.add_sized(vec2(64.0, 14.0), egui::Label::new(RichText::new("Samples").size(11.5).color(theme::TEXT_MUTED)));
            ui.label(job);
        });
    }
    let t = &r["timing"];
    if let Some(el) = t["elapsed_s"].as_f64() {
        let g = &t["grab_wait_ms"];
        row(
            ui,
            "Time",
            format!(
                "{} · {} ms/sample · grab wait median {} / p95 {} / max {} ms",
                fmt_secs(el),
                t["per_sample_ms"],
                g["median"],
                g["p95"],
                g["max"]
            ),
            None,
        );
    }
    let diag: Vec<&str> = r["diagnostics"].as_array().map(|a| a.iter().filter_map(|l| l.as_str()).collect()).unwrap_or_default();
    for w in diag.iter().filter_map(|l| l.strip_prefix("warning:")) {
        ui.label(RichText::new(format!("Warning: {}", w.trim())).size(12.0).color(theme::WARNING));
    }
    if !diag.is_empty() {
        egui::CollapsingHeader::new(RichText::new("diagnostics").size(11.5).color(theme::TEXT_DIM))
            .id_salt(("tl_diag", full))
            .default_open(full)
            .show(ui, |ui| {
                egui::Frame::NONE.fill(theme::BG_BASE).inner_margin(8.0).corner_radius(4.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().max_height(if full { 600.0 } else { 160.0 }).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(diag.join("\n")).monospace().size(11.0).color(theme::TEXT_DIM)).selectable(true));
                    });
                });
            });
    }
}

fn fmt_secs(s: f64) -> String {
    if s < 90.0 {
        return format!("{s:.1} s");
    }
    let m = (s / 60.0).floor() as u64;
    if m < 90 {
        format!("{m} min {} s", (s % 60.0).round() as u64)
    } else {
        format!("{} h {} min", m / 60, m % 60)
    }
}

pub const BAR_H: f32 = 64.0;

/// How the two clips of a comparison sit in timecode: REF and DIST bars
/// over the full span, the overlap, event ticks and the cursor. Click to
/// move the cursor. `tc` turns a frame into timecode for the axis labels.
pub fn bar(ui: &mut Ui, tl: &TimelineInfo, fps: f64, events: &[Event], cursor: i64, tc: &dyn Fn(i64) -> String) -> Option<i64> {
    let span = (tl.span_end - tl.span_start) as f64;
    if span <= 0.0 {
        return None;
    }
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), BAR_H), Sense::click_and_drag());
    let p = ui.painter_at(rect);
    let label_w = 40.0;
    let track = Rect::from_min_max(pos2(rect.left() + label_w, rect.top() + 16.0), pos2(rect.right() - 6.0, rect.bottom() - 14.0));
    let x = |f: i64| track.left() + ((f - tl.span_start) as f64 / span) as f32 * track.width();
    let ref_c = Color32::from_rgb(0x4a, 0x9e, 0xff);
    let dist_c = Color32::from_rgb(0xff, 0x6b, 0x8a);

    // Axis: ~7 timecode ticks on round second intervals.
    let span_s = span / fps.max(1.0);
    let iv = [1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0].into_iter().find(|iv| span_s / iv <= 9.0).unwrap_or(3600.0);
    let step = (iv * fps).round().max(1.0) as i64;
    let mut f = (tl.span_start as f64 / step as f64).ceil() as i64 * step;
    while f <= tl.span_end {
        let xf = x(f);
        p.line_segment([pos2(xf, rect.top() + 12.0), pos2(xf, track.bottom())], Stroke::new(1.0, Color32::from_white_alpha(14)));
        p.text(pos2(xf, rect.top() + 1.0), egui::Align2::CENTER_TOP, tc(f), egui::FontId::monospace(10.0), theme::TEXT_MUTED);
        f += step;
    }
    let row_h = (track.height() - 4.0) / 2.0;
    for (i, (label, a, z, c, name)) in [("REF", tl.ref_start, tl.ref_end, ref_c, &tl.ref_filename), ("DIST", tl.dist_start, tl.dist_end, dist_c, &tl.dist_filename)].into_iter().enumerate() {
        let y = track.top() + i as f32 * (row_h + 4.0);
        p.text(pos2(rect.left() + 4.0, y + row_h / 2.0), egui::Align2::LEFT_CENTER, label, egui::FontId::monospace(10.0), c);
        p.rect_filled(Rect::from_min_max(pos2(track.left(), y), pos2(track.right(), y + row_h)), 2.0, Color32::from_gray(0x1a));
        let r = Rect::from_min_max(pos2(x(a), y), pos2(x(z).max(x(a) + 2.0), y + row_h));
        p.rect_filled(r, 2.0, c.gamma_multiply(0.55));
        p.text(r.left_center() + vec2(4.0, 0.0), egui::Align2::LEFT_CENTER, name, egui::FontId::proportional(10.5), Color32::from_white_alpha(200));
    }
    if tl.ref_start != tl.dist_start || tl.ref_end != tl.dist_end {
        let a = x(tl.overlap_start);
        let z = x(tl.overlap_end);
        let y = track.bottom() + 2.0;
        p.line_segment([pos2(a, y), pos2(z, y)], Stroke::new(1.0, Color32::from_white_alpha(180)));
        p.text(pos2((a + z) / 2.0, y + 1.0), egui::Align2::CENTER_TOP, "overlap", egui::FontId::proportional(9.5), theme::TEXT_DIM);
    }
    for e in events {
        let a = x(e.frame);
        let z = x(e.frame + e.length as i64).max(a + 2.0);
        let c = theme::event_color(e).gamma_multiply(if theme::event_faded(e) { 0.4 } else { 0.9 });
        p.rect_filled(Rect::from_min_max(pos2(a, track.top()), pos2(z, track.bottom())), 0.0, c);
    }
    if cursor >= tl.span_start && cursor <= tl.span_end {
        let xc = x(cursor);
        p.line_segment([pos2(xc, track.top() - 2.0), pos2(xc, track.bottom() + 2.0)], Stroke::new(1.5, Color32::WHITE));
    }
    if resp.is_pointer_button_down_on() {
        if let Some(pp) = resp.interact_pointer_pos() {
            let t = ((pp.x - track.left()) / track.width()).clamp(0.0, 1.0) as f64;
            let f = (tl.span_start as f64 + t * span).round() as i64;
            if f != cursor {
                return Some(f);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ticks_keep_or_default_and_blockers() {
        let mut s = Setup::default();
        assert_eq!(s.blocker(false, None), Some("Choose the deliverable file"));
        assert_eq!(s.blocker(true, None), Some("scopetap_grab isn't installed (or set VCOMPARE_GRABBER)"));
        assert_eq!(s.blocker(true, Some("/g")), Some("Connect to Resolve"));
        let reply = json!({"target": "P / T", "timeline": {}, "grabber": "/g", "tracks": [
            {"index": 1, "name": "V1", "items": 6, "enabled": true},
            {"index": 2, "name": "titles", "items": 1, "enabled": false},
            {"index": 3, "name": "empty", "items": 0, "enabled": true},
        ]});
        s.connected(Ok(reply.clone()));
        assert_eq!(s.ticked.iter().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(s.items(), 6);
        assert_eq!(s.blocker(true, s.grabber(None)), None);
        // The operator's ticks survive a refresh while the tracks exist.
        s.ticked = [2, 3].into();
        s.connected(Ok(reply));
        assert_eq!(s.ticked.iter().copied().collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(s.blocker(true, Some("/g")), None);
        s.ticked = [3].into();
        assert_eq!(s.blocker(true, Some("/g")), Some("The ticked tracks have no clips"));
        s.connected(Err("no Resolve".into()));
        assert!(s.reply.is_none() && s.error.is_some());
    }

    #[test]
    fn seconds() {
        assert_eq!(fmt_secs(12.34), "12.3 s");
        assert_eq!(fmt_secs(125.0), "2 min 5 s");
        assert_eq!(fmt_secs(5600.0), "1 h 33 min");
    }
}
