//! Colours and styling, matching the web UI's palette (src/styles/global.css).

use eframe::egui::{self, Color32};
use vcompare_service::engine::detect::Event;

pub const BG_BASE: Color32 = Color32::from_rgb(0x0d, 0x0d, 0x0d);
pub const BG_SURFACE: Color32 = Color32::from_rgb(0x16, 0x16, 0x16);
pub const BG_RAISED: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x1e);
pub const BG_OVERLAY: Color32 = Color32::from_rgb(0x26, 0x26, 0x26);
pub const BG_HOVER: Color32 = Color32::from_rgb(0x2e, 0x2e, 0x2e);
pub const BG_ACTIVE: Color32 = Color32::from_rgb(0x36, 0x36, 0x36);
pub const TEXT: Color32 = Color32::from_rgb(0xe8, 0xe8, 0xe8);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x99, 0x99, 0x99);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x66, 0x66, 0x66);
pub const ACCENT: Color32 = Color32::from_rgb(0x5a, 0x8f, 0xd4);
pub const WARNING: Color32 = Color32::from_rgb(0xe8, 0xb8, 0x30);
pub const THRESHOLD: Color32 = Color32::from_rgb(0xe5, 0x45, 0x45);
pub const SUCCESS: Color32 = Color32::from_rgb(0x4e, 0xc9, 0x66);
pub const ERROR: Color32 = THRESHOLD;

pub fn apply(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.global_style_mut(|s| {
        let v = &mut s.visuals;
        v.panel_fill = BG_SURFACE;
        v.window_fill = BG_RAISED;
        v.extreme_bg_color = BG_BASE;
        v.faint_bg_color = Color32::from_rgb(0x19, 0x19, 0x19);
        v.selection.bg_fill = Color32::from_rgb(0x3a, 0x5f, 0x8a);
        v.widgets.noninteractive.bg_stroke.color = Color32::from_rgb(0x2a, 0x2a, 0x2a);
        v.widgets.inactive.weak_bg_fill = BG_OVERLAY;
        v.widgets.hovered.weak_bg_fill = BG_HOVER;
        v.widgets.active.weak_bg_fill = BG_ACTIVE;
        v.override_text_color = Some(TEXT);
        s.spacing.item_spacing = egui::vec2(8.0, 4.0);
    });
}

/// Line colour and label of a metric (as in MetricChart.svelte).
pub fn metric_style(metric: &str) -> (Color32, &'static str) {
    let hex = |h: u32| Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8);
    match metric {
        "vmaf" => (hex(0xe8b830), "VMAF"),
        "psnr" => (hex(0x4ec9b0), "PSNR (dB)"),
        "diff_y" => (hex(0xe8b830), "RMS Diff (Y)"),
        "diff_u" => (hex(0x6bc4e8), "RMS Diff (U)"),
        "diff_v" => (hex(0xe86b9e), "RMS Diff (V)"),
        "ssim" => (hex(0x4ec9b0), "SSIM (All)"),
        "ssim_y" => (hex(0x7be870), "SSIM (Y)"),
        "ssim_u" => (hex(0x70b8e8), "SSIM (U)"),
        "ssim_v" => (hex(0xd870e8), "SSIM (V)"),
        "y_min" => (hex(0xb89830), "Y Min"),
        "y_max" => (hex(0xf0d060), "Y Max"),
        "u_min" => (hex(0x4090c0), "U Min (Cb)"),
        "u_max" => (hex(0x70c0f0), "U Max (Cb)"),
        "v_min" => (hex(0xc04070), "V Min (Cr)"),
        "v_max" => (hex(0xf070a0), "V Max (Cr)"),
        "y_avg" => (hex(0xd8b848), "Y Avg"),
        "u_avg" => (hex(0x58a8d8), "U Avg (Cb)"),
        "v_avg" => (hex(0xd85888), "V Avg (Cr)"),
        "y_low" => (hex(0xa08830), "Y Low (10th pct)"),
        "y_high" => (hex(0xf8e080), "Y High (90th pct)"),
        "y_oor_lo" => (hex(0x6b8ce8), "Luma samples below legal"),
        "y_oor_hi" => (hex(0xe54545), "Luma samples above legal"),
        "c_oor" => (hex(0xd870e8), "Chroma samples outside legal"),
        "y_dif" => (hex(0xe8e8e8), "Y Dif (vs prev)"),
        "u_dif" => (hex(0x70c0f0), "U Dif (vs prev)"),
        "v_dif" => (hex(0xf070a0), "V Dif (vs prev)"),
        "sat_avg" => (hex(0xc070e8), "Sat Avg"),
        _ => (hex(0xe8b830), "?"),
    }
}

pub fn metric_label(metric: &str) -> String {
    match metric_style(metric).1 {
        "?" => metric.to_string(),
        l => l.to_string(),
    }
}

/// Table column label and decimals (as in FrameTable.svelte).
pub fn column_style(metric: &str) -> (&str, usize) {
    match metric {
        "diff_y" => ("Diff Y", 3),
        "diff_u" => ("Diff U", 3),
        "diff_v" => ("Diff V", 3),
        "vmaf" => ("VMAF", 2),
        "psnr" => ("PSNR", 2),
        "ssim" => ("SSIM", 5),
        "ssim_y" => ("SSIM Y", 5),
        "ssim_u" => ("SSIM U", 5),
        "ssim_v" => ("SSIM V", 5),
        "y_min" => ("Y Min", 0),
        "y_max" => ("Y Max", 0),
        "u_min" => ("U Min", 0),
        "u_max" => ("U Max", 0),
        "v_min" => ("V Min", 0),
        "v_max" => ("V Max", 0),
        "y_avg" => ("Y Avg", 1),
        "u_avg" => ("U Avg", 1),
        "v_avg" => ("V Avg", 1),
        "y_low" => ("Y Low", 0),
        "y_high" => ("Y High", 0),
        "y_oor_lo" => ("Y < legal", 0),
        "y_oor_hi" => ("Y > legal", 0),
        "c_oor" => ("C illegal", 0),
        "y_dif" => ("Y Dif", 2),
        "u_dif" => ("U Dif", 2),
        "v_dif" => ("V Dif", 2),
        "sat_avg" => ("Sat Avg", 1),
        m => (m, 3),
    }
}

/// Event marker colour (src/lib/eventColors.ts).
pub fn event_color(e: &Event) -> Color32 {
    let hex = |h: u32| Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8);
    let black = hex(0x9a9a9a);
    let content = hex(0xe8b830);
    match e.kind.as_str() {
        "flash" => match e.subkind.as_str() {
            "black" => black,
            "white" => hex(0xffffff),
            _ => content,
        },
        "repeat" => match e.subkind.as_str() {
            "hold" => hex(0x2e8a5e),
            "black" => black,
            _ => hex(0x4cc38a),
        },
        "mismatch" => hex(0xe5534b),
        "error" => hex(0xd29922),
        "match" => hex(0xf070c0),
        _ => content,
    }
}

/// Faded markers: flashes sitting on a cut.
pub fn event_faded(e: &Event) -> bool {
    e.kind == "flash" && e.at_cut
}

#[cfg(test)]
mod tests {
    use eframe::egui::{epaint::text::{FontDefinitions, Fonts, TextOptions}, FontId};

    /// Every non-ASCII character in this crate's sources must be in the
    /// built-in proportional font, or it shows as a box. Arrows are only
    /// in the monospace font: use them in monospace text only (the Keys
    /// panel).
    #[test]
    fn every_glyph_we_use_exists() {
        let mut fonts = Fonts::new(TextOptions::default(), FontDefinitions::default());
        let mut missing = std::collections::BTreeMap::new();
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            for line in text.lines() {
                // Comments may use anything.
                let code = line.split("//").next().unwrap_or("");
                for c in code.chars().filter(|c| !c.is_ascii()) {
                    let f = if "←↑→↓".contains(c) { FontId::monospace(12.0) } else { FontId::proportional(12.0) };
                    if !fonts.has_glyph(&f, c) {
                        missing.entry(c).or_insert_with(Vec::new).push(path.file_name().unwrap().to_string_lossy().to_string());
                    }
                }
            }
        }
        for v in missing.values_mut() {
            v.sort();
            v.dedup();
        }
        assert!(missing.is_empty(), "glyphs missing from the built-in fonts: {missing:?}");
    }
}
