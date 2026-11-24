//! The start screen: choose the mode and the file(s), see what they are,
//! set the options, then analyse (or open the cached analysis).
//! Port of the idle / probing view of App.svelte.

use crate::theme;
use eframe::egui::{self, vec2, Align, Layout, RichText, Ui};
use serde_json::Value;
use std::path::{Path, PathBuf};
use vcompare_service::engine::types::{Capabilities, ProbeResult};
use vcompare_service::AnalyzeParams;

pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mxf", "mov", "mp4", "mkv", "avi", "m4v", "mpg", "mpeg", "ts", "m2ts", "webm", "ogv", "mj2", "xml", "txt",
];
const PARALLEL_OPTIONS: &[u32] = &[1, 2, 3, 4, 6, 8, 12, 16];

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
}

pub struct Pick {
    pub mode: String,
    pub ref_path: String,
    pub dist_path: String,
    pub ref_probe: Option<ProbeResult>,
    pub dist_probe: Option<ProbeResult>,
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
}

impl Pick {
    pub fn new(mode: String, keep_signatures: bool, last_dir: Option<PathBuf>) -> Self {
        let mode = if ["diff", "vmaf", "levels"].contains(&mode.as_str()) { mode } else { "diff".into() };
        Self {
            mode,
            ref_path: String::new(),
            dist_path: String::new(),
            ref_probe: None,
            dist_probe: None,
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
        }
    }

    /// Back to no files (New analysis); options are kept.
    pub fn clear_files(&mut self) {
        self.ref_path.clear();
        self.dist_path.clear();
        self.ref_probe = None;
        self.dist_probe = None;
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
            }
            Slot::Distorted => {
                self.dist_path = path.clone();
                self.dist_probe = None;
            }
        }
        self.cache = None;
        self.probing += 1;
        PickAction::Probe(slot, path)
    }

    /// Dropped files: one mode file → the file; compare: two files fill
    /// reference then distorted, one file goes to the empty slot
    /// (reference first). Returns what to do, and a note if files were
    /// ignored.
    pub fn drop_files(&mut self, paths: &[String]) -> (Vec<PickAction>, Option<String>) {
        let name = |p: &str| Path::new(p).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if paths.is_empty() {
            return (vec![], None);
        }
        if self.single_file() {
            let note = (paths.len() > 1).then(|| format!("This mode takes one file — using {}", name(&paths[0])));
            return (vec![self.set(Slot::Reference, paths[0].clone())], note);
        }
        if paths.len() >= 2 {
            let a = self.set(Slot::Reference, paths[0].clone());
            let b = self.set(Slot::Distorted, paths[1].clone());
            let note = (paths.len() > 2).then(|| "Compare uses two files — extra files ignored".to_string());
            return (vec![a, b], note);
        }
        let slot = if self.ref_path.is_empty() || !self.dist_path.is_empty() { Slot::Reference } else { Slot::Distorted };
        (vec![self.set(slot, paths[0].clone())], None)
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

    pub fn show(&mut self, ui: &mut Ui, caps: Option<&Capabilities>, status: Option<&str>) -> Vec<PickAction> {
        let mut acts = Vec::new();
        let width = 760.0f32.min(ui.available_width() - 40.0);
        ui.add_space(28.0);
        ui.vertical_centered(|ui| {
            ui.set_max_width(width);
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                let (title, blurb) = match self.mode.as_str() {
                    "levels" => ("Analyze Video", "Select or drop a video file to analyze signal levels."),
                    _ => ("Compare Videos", "Select or drop a reference and distorted file — or drop both at once."),
                };
                ui.label(RichText::new(title).size(20.0).strong());
                ui.label(RichText::new(blurb).color(theme::TEXT_DIM));
                ui.add_space(10.0);
                if let Some(s) = status {
                    ui.label(RichText::new(s).color(theme::TEXT_DIM));
                    ui.add_space(6.0);
                }

                ui.horizontal(|ui| {
                    ui.label("Mode");
                    let before = self.mode.clone();
                    egui::ComboBox::from_id_salt("mode").width(220.0).selected_text(mode_label(&self.mode)).show_ui(ui, |ui| {
                        for m in ["diff", "vmaf", "levels"] {
                            ui.selectable_value(&mut self.mode, m.to_string(), mode_label(m));
                        }
                        ui.add_enabled(false, egui::Button::selectable(false, mode_label("timeline")))
                            .on_disabled_hover_text("Not in the native UI yet; use the Tauri app for timeline checks.");
                    });
                    if self.mode != before {
                        acts.push(PickAction::ModeChanged);
                        acts.push(PickAction::CheckCache);
                    }
                });
                ui.add_space(8.0);

                let ref_label = if self.mode == "levels" { "File" } else { "Reference" };
                self.file_row(ui, ref_label, Slot::Reference, &mut acts);
                if !self.single_file() {
                    self.file_row(ui, "Distorted", Slot::Distorted, &mut acts);
                }
                ui.add_space(8.0);

                if !self.ready() {
                    if self.probing > 0 {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("Reading the file…").color(theme::TEXT_DIM));
                        });
                    }
                    return;
                }

                if self.mode == "levels" {
                    self.levels_options(ui);
                } else {
                    self.compare_options(ui, caps);
                }
                ui.add_space(10.0);

                let cached = self.cache.as_ref().filter(|c| c["exists"].as_bool() == Some(true));
                let label = if cached.is_some() { "Re-analyze" } else { "Analyze" };
                if ui.add(egui::Button::new(RichText::new(label).strong()).min_size(vec2(140.0, 30.0))).clicked() {
                    acts.push(PickAction::Analyze(self.params()));
                }
                if let Some(c) = cached {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let file = c["path"].as_str().and_then(|p| Path::new(p).file_name()).map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                        let frames = c["frame_count"].as_u64().map(group).unwrap_or_default();
                        let what = if self.mode == "levels" {
                            "levels".to_string()
                        } else {
                            format!(
                                "{}{}, {}",
                                c["mode"].as_str().unwrap_or("vmaf"),
                                if c["with_ssim"].as_bool() == Some(true) { "+ssim" } else { "" },
                                c["sync_mode"].as_str().unwrap_or("start")
                            )
                        };
                        ui.label(RichText::new(format!("Cached ({what}): {frames} frames ({file})")).monospace().size(12.0).color(theme::TEXT_DIM));
                        if ui.button("Load cached").clicked() {
                            if let Some(p) = c["path"].as_str() {
                                acts.push(PickAction::LoadCached(p.to_string()));
                            }
                        }
                    });
                }
            });
        });
        acts
    }

    fn file_row(&mut self, ui: &mut Ui, label: &str, slot: Slot, acts: &mut Vec<PickAction>) {
        let (path, probe) = match slot {
            Slot::Reference => (&self.ref_path, &self.ref_probe),
            Slot::Distorted => (&self.dist_path, &self.dist_probe),
        };
        ui.horizontal(|ui| {
            ui.add_sized(vec2(72.0, 22.0), egui::Label::new(RichText::new(label).color(theme::TEXT_DIM)));
            let name = if path.is_empty() {
                "Choose file…".to_string()
            } else {
                Path::new(path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| path.clone())
            };
            let b = ui.add(egui::Button::new(name).min_size(vec2(220.0, 24.0)));
            let b = if path.is_empty() { b } else { b.on_hover_text(path.as_str()) };
            if b.clicked() {
                acts.push(PickAction::Choose(slot));
            }
            if let Some(p) = probe {
                ui.label(RichText::new(fmt_probe(p)).monospace().size(12.0).color(theme::TEXT_DIM));
            }
        });
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
        if let (Some(r), Some(d)) = (&self.ref_probe, &self.dist_probe) {
            if (r.fps - d.fps).abs() > 0.01 {
                ui.label(
                    RichText::new(format!(
                        "⚠ Framerate mismatch: ref {}/{} ({:.3}fps) vs dist {}/{} ({:.3}fps) — results may be unreliable",
                        r.fps_num, r.fps_den, r.fps, d.fps_num, d.fps_den, d.fps
                    ))
                    .color(theme::WARNING),
                );
                ui.add_space(6.0);
            }
        }
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

pub fn fmt_probe(p: &ProbeResult) -> String {
    let fps = if p.fps_den == 1001 {
        format!("{:.2}", p.fps_num as f64 / p.fps_den as f64)
    } else {
        let f = p.fps_num as f64 / p.fps_den.max(1) as f64;
        if f.fract() == 0.0 { format!("{f:.0}") } else { format!("{f}") }
    };
    let tc = p.timecode.as_deref().map(|t| format!(" TC {t}")).unwrap_or_default();
    format!("{}×{} {} {} {fps}fps{tc}", p.width, p.height, p.codec, p.pix_fmt)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(mode: &str) -> Pick {
        Pick::new(mode.into(), true, None)
    }

    #[test]
    fn drops_fill_slots_like_the_web_ui() {
        let mut p = pick("diff");
        let (a, note) = p.drop_files(&["/x/a.mov".into(), "/x/b.mov".into(), "/x/c.mov".into()]);
        assert_eq!((p.ref_path.as_str(), p.dist_path.as_str()), ("/x/a.mov", "/x/b.mov"));
        assert_eq!(a.len(), 2);
        assert!(note.is_some());

        let mut p = pick("diff");
        p.drop_files(&["/x/a.mov".into()]);
        p.drop_files(&["/x/b.mov".into()]);
        assert_eq!((p.ref_path.as_str(), p.dist_path.as_str()), ("/x/a.mov", "/x/b.mov"));
        // Both full: a single drop replaces the reference.
        p.drop_files(&["/x/c.mov".into()]);
        assert_eq!(p.ref_path, "/x/c.mov");

        let mut p = pick("levels");
        let (_, note) = p.drop_files(&["/x/a.mov".into(), "/x/b.mov".into()]);
        assert_eq!((p.ref_path.as_str(), p.dist_path.as_str()), ("/x/a.mov", ""));
        assert!(note.unwrap().contains("a.mov"));
        assert_eq!(p.last_dir.as_deref(), Some(Path::new("/x")));
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
        assert_eq!(fmt_probe(&p), "3840×2160 prores yuv422p10le 23.98fps TC 01:00:00:00");
    }
}
