//! File > Tools…: which ffmpeg, ffprobe and mpv vcompare runs, and what
//! that ffmpeg can do (libvmaf, libvmaf_cuda, hardware decode). The paths
//! are kept in the app's settings; empty means "the usual lookup"
//! (`VCOMPARE_FFMPEG` & co., then `PATH`).

use eframe::egui::{self, RichText, Ui};
use std::path::PathBuf;
use vcompare_service::engine::tools::{Source, Tool, ToolPaths};
use vcompare_service::ToolReport;

use crate::theme;

pub enum ToolsAction {
    /// Pick a file for this tool (opens the dialog).
    Browse(Tool),
    /// Use these paths from now on (and save them).
    Apply(ToolPaths),
    /// Run the checks again (something changed outside the app).
    Recheck,
}

#[derive(Default)]
pub struct ToolsWindow {
    pub open: bool,
    /// What's typed, per `Tool::ALL`.
    draft: [String; 3],
    /// What's in force.
    applied: ToolPaths,
    pub report: Option<ToolReport>,
    pub checking: bool,
}

fn idx(t: Tool) -> usize {
    Tool::ALL.iter().position(|&x| x == t).unwrap_or(0)
}

fn to_text(p: Option<&std::path::Path>) -> String {
    p.map(|p| p.to_string_lossy().to_string()).unwrap_or_default()
}

impl ToolsWindow {
    pub fn new(applied: ToolPaths) -> Self {
        let mut w = Self { applied, ..Default::default() };
        w.reset_draft();
        w
    }

    fn reset_draft(&mut self) {
        for t in Tool::ALL {
            self.draft[idx(t)] = to_text(self.applied.get(t));
        }
    }

    fn draft_paths(&self) -> ToolPaths {
        let get = |t: Tool| {
            let s = self.draft[idx(t)].trim();
            (!s.is_empty()).then(|| PathBuf::from(s))
        };
        ToolPaths { ffmpeg: get(Tool::Ffmpeg), ffprobe: get(Tool::Ffprobe), mpv: get(Tool::Mpv) }
    }

    /// A file came back from Browse.
    pub fn set(&mut self, tool: Tool, path: String) {
        self.draft[idx(tool)] = path;
    }

    pub fn applied(&mut self, paths: ToolPaths) {
        self.applied = paths;
        self.reset_draft();
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Vec<ToolsAction> {
        let mut acts = vec![];
        let mut open = self.open;
        egui::Window::new("Tools").open(&mut open).collapsible(false).resizable(false).default_width(640.0).show(ctx, |ui| {
            self.body(ui, &mut acts);
        });
        self.open = open;
        if !self.open {
            // Closing drops unapplied edits.
            self.reset_draft();
        }
        acts
    }

    fn body(&mut self, ui: &mut Ui, acts: &mut Vec<ToolsAction>) {
        ui.label(
            RichText::new(
                "Leave a path empty to use the environment variable if set, else the program on PATH. \
                 A set ffmpeg brings the ffprobe next to it.",
            )
            .color(theme::TEXT_DIM),
        );
        ui.add_space(8.0);
        egui::Grid::new("tools_grid").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            for t in Tool::ALL {
                ui.label(RichText::new(t.name()).strong());
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.draft[idx(t)])
                            .desired_width(400.0)
                            .hint_text(format!("{} or {} on PATH", t.env_var(), t.name())),
                    );
                    if ui.button("Browse…").clicked() {
                        acts.push(ToolsAction::Browse(t));
                    }
                    if ui.add_enabled(!self.draft[idx(t)].is_empty(), egui::Button::new("Clear")).clicked() {
                        self.draft[idx(t)].clear();
                    }
                });
                ui.end_row();
                ui.label("");
                self.status_line(ui, t);
                ui.end_row();
            }
        });
        ui.add_space(8.0);
        ui.separator();
        self.features(ui);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let changed = self.draft_paths() != self.applied;
            if ui.add_enabled(changed && !self.checking, egui::Button::new("Apply")).clicked() {
                acts.push(ToolsAction::Apply(self.draft_paths()));
            }
            if ui.add_enabled(changed, egui::Button::new("Revert")).clicked() {
                self.reset_draft();
            }
            if ui.add_enabled(!self.checking, egui::Button::new("Check again")).clicked() {
                acts.push(ToolsAction::Recheck);
            }
            if self.checking {
                ui.spinner();
                ui.label(RichText::new("checking…").color(theme::TEXT_DIM));
            } else if changed {
                ui.label(RichText::new("not applied yet").color(theme::WARNING));
            }
        });
    }

    fn status_line(&self, ui: &mut Ui, t: Tool) {
        let Some(st) = self.report.as_ref().and_then(|r| r.tools.iter().find(|s| s.tool == t)) else {
            ui.label(RichText::new("…").color(theme::TEXT_DIM));
            return;
        };
        let from = match st.source {
            Source::Settings => "set here".to_string(),
            Source::Env => format!("from {}", t.env_var()),
            Source::NextToFfmpeg => "next to ffmpeg".to_string(),
            // A bare name resolved outside PATH (Homebrew from Finder, …).
            Source::Path if st.program.components().count() > 1 => "not on PATH, found here".to_string(),
            Source::Path => "on PATH".to_string(),
        };
        let small = |s: String| RichText::new(s).size(12.0);
        match (&st.path, &st.version) {
            (Some(p), Ok(v)) => {
                ui.label(small(format!("{v}  —  {}  ({from})", p.display())).color(theme::SUCCESS));
            }
            (Some(p), Err(e)) => {
                ui.label(small(format!("{} doesn't run: {e}  ({from})", p.display())).color(theme::ERROR));
            }
            (None, _) => {
                let what = if st.program.components().count() > 1 {
                    format!("{} doesn't exist", st.program.display())
                } else {
                    format!("no {} on PATH", t.name())
                };
                let c = if t == Tool::Mpv { theme::WARNING } else { theme::ERROR };
                let note = if t == Tool::Mpv { " — only needed for the p / d players" } else { "" };
                ui.label(small(format!("{what}  ({from}){note}")).color(c));
            }
        }
    }

    fn features(&self, ui: &mut Ui) {
        let Some(c) = self.report.as_ref().map(|r| &r.capabilities) else { return };
        let ffmpeg_ok = self
            .report
            .as_ref()
            .and_then(|r| r.tools.iter().find(|s| s.tool == Tool::Ffmpeg))
            .is_some_and(|s| s.version.is_ok());
        if !ffmpeg_ok {
            return;
        }
        let yes_no = |ui: &mut Ui, label: &str, ok: bool, yes: &str, no: &str, no_colour: egui::Color32| {
            ui.label(label);
            ui.label(RichText::new(if ok { yes } else { no }).color(if ok { theme::SUCCESS } else { no_colour }));
            ui.end_row();
        };
        egui::Grid::new("tools_features").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            let vmaf_none = !c.libvmaf && !c.vmaf_cuda;
            yes_no(
                ui,
                "libvmaf",
                c.libvmaf,
                "yes",
                if c.vmaf_cuda { "no (CUDA only: VMAF needs HW Accel CUDA and no PSNR)" } else { "no — VMAF mode won't run" },
                if vmaf_none { theme::ERROR } else { theme::WARNING },
            );
            yes_no(ui, "libvmaf_cuda", c.vmaf_cuda, "yes", if c.libvmaf { "no (VMAF runs on the CPU)" } else { "no" }, theme::TEXT_DIM);
            ui.label("hardware decode").on_hover_text("What this ffmpeg was built with; Auto in VMAF mode uses it");
            ui.label(match &c.hwaccel {
                Some(h) => RichText::new(h).color(theme::SUCCESS),
                None => RichText::new("none found").color(theme::TEXT_DIM),
            });
            ui.end_row();
            ui.label("scopetap_grab");
            ui.label(match &c.grabber {
                Some(g) => RichText::new(g).color(theme::SUCCESS),
                None => RichText::new("not found — only needed for Timeline mode").color(theme::TEXT_DIM),
            });
            ui.end_row();
        });
    }
}
