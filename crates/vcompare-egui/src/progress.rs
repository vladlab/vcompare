//! The analysing / opening view: progress bar, frames, fps and ETA over a
//! sliding window, the engine's diagnostics, Cancel (Esc).

use crate::pick::{group, mode_label};
use crate::theme;
use eframe::egui::{self, vec2, Align, Layout, RichText, Ui};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const RATE_WINDOW: Duration = Duration::from_secs(3);

pub enum JobKind {
    Analyze { mode: String },
    /// Reading a saved analysis back in (path).
    Open(String),
}

pub struct Job {
    pub kind: JobKind,
    pub current: u64,
    pub total: u64,
    samples: VecDeque<(Instant, u64)>,
    pub fps: Option<f64>,
    pub eta: Option<f64>,
    pub diagnostics: Vec<String>,
    pub cancelling: bool,
}

impl Job {
    pub fn new(kind: JobKind) -> Self {
        Self { kind, current: 0, total: 0, samples: VecDeque::new(), fps: None, eta: None, diagnostics: vec![], cancelling: false }
    }

    pub fn progress(&mut self, current: u64, total: u64) {
        self.progress_at(Instant::now(), current, total);
    }

    fn progress_at(&mut self, now: Instant, current: u64, total: u64) {
        self.current = current;
        self.total = total;
        self.samples.push_back((now, current));
        while self.samples.len() > 2 && now.duration_since(self.samples[0].0) > RATE_WINDOW {
            self.samples.pop_front();
        }
        let (t0, f0) = self.samples[0];
        let dt = now.duration_since(t0).as_secs_f64();
        if self.samples.len() >= 2 && dt > 0.5 {
            let fps = current.saturating_sub(f0) as f64 / dt;
            self.fps = Some(fps);
            self.eta = (fps > 0.0 && total > current).then(|| (total - current) as f64 / fps);
        }
    }

    /// Returns true when Cancel was clicked.
    pub fn show(&self, ui: &mut Ui) -> bool {
        let mut cancel = false;
        let width = 640.0f32.min(ui.available_width() - 40.0);
        ui.add_space(60.0);
        ui.vertical_centered(|ui| {
            ui.set_max_width(width);
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                let opening = matches!(self.kind, JobKind::Open(_));
                let title = match &self.kind {
                    JobKind::Open(_) => "Opening saved analysis…".to_string(),
                    JobKind::Analyze { mode } => format!("Analyzing… ({})", match mode.as_str() {
                        "diff" => "pixel diff",
                        "levels" => "levels",
                        m => mode_label(m),
                    }),
                };
                ui.label(RichText::new(title).size(18.0).strong());
                ui.add_space(10.0);
                let frac = if opening || self.total == 0 { None } else { Some((self.current as f32 / self.total as f32).min(1.0)) };
                let mut bar = egui::ProgressBar::new(frac.unwrap_or(0.0)).desired_height(8.0).fill(theme::ACCENT);
                if frac.is_none() {
                    bar = bar.animate(true);
                }
                ui.add(bar);
                ui.add_space(6.0);
                let line = match &self.kind {
                    JobKind::Open(p) => {
                        let name = std::path::Path::new(p).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                        format!("Reading {name} — nothing is being re-analysed")
                    }
                    _ if self.total > 0 => {
                        let pct = (self.current as f64 / self.total as f64 * 100.0).round() as u64;
                        let mut s = format!("{} / {} frames ({pct}%)", group(self.current), group(self.total));
                        if let Some(f) = self.fps {
                            s += &format!(" · {f:.1} fps");
                            if let Some(e) = self.eta {
                                s += &format!(" · ~{} left", fmt_duration(e));
                            }
                        }
                        s
                    }
                    _ => "Starting FFmpeg…".to_string(),
                };
                ui.label(RichText::new(line).monospace().size(12.5).color(theme::TEXT_DIM));
                if !opening && !self.diagnostics.is_empty() {
                    ui.add_space(10.0);
                    egui::Frame::NONE.fill(theme::BG_BASE).inner_margin(10.0).corner_radius(4.0).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::ScrollArea::vertical().max_height(260.0).stick_to_bottom(true).show(ui, |ui| {
                            ui.label(RichText::new(self.diagnostics.join("\n")).monospace().size(11.5).color(theme::TEXT_DIM));
                        });
                    });
                }
                if !opening {
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        let label = if self.cancelling { "Cancelling…" } else { "Cancel" };
                        let b = ui.add_enabled(!self.cancelling, egui::Button::new(label).min_size(vec2(90.0, 26.0)));
                        if b.on_hover_text("Stop the analysis (Esc). Nothing is saved.").clicked() {
                            cancel = true;
                        }
                        ui.label(RichText::new("Esc").color(theme::TEXT_MUTED));
                    });
                }
            });
        });
        cancel
    }
}

pub fn fmt_duration(sec: f64) -> String {
    let s = sec.round() as u64;
    let (h, m, ss) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{ss:02}s")
    } else {
        format!("{ss}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_and_eta_over_a_window() {
        let mut j = Job::new(JobKind::Analyze { mode: "levels".into() });
        let t = Instant::now();
        j.progress_at(t, 0, 1000);
        assert_eq!(j.fps, None);
        j.progress_at(t + Duration::from_millis(250), 25, 1000);
        assert_eq!(j.fps, None, "too short a window");
        j.progress_at(t + Duration::from_secs(1), 100, 1000);
        assert!((j.fps.unwrap() - 100.0).abs() < 1e-6);
        assert!((j.eta.unwrap() - 9.0).abs() < 1e-6);
        // Old samples fall out of the 3 s window: the rate follows a slowdown.
        j.progress_at(t + Duration::from_secs(5), 140, 1000);
        j.progress_at(t + Duration::from_secs(6), 150, 1000);
        assert!(j.fps.unwrap() < 20.0, "{:?}", j.fps);
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(42.4), "42s");
        assert_eq!(fmt_duration(125.0), "2m05s");
        assert_eq!(fmt_duration(3725.0), "1h02m");
    }
}
