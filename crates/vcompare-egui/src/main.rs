//! vcompare, native UI (egui). Spike: opens a saved analysis.
//!
//! `vcompare-egui [analysis.json]`
//!
//! VCOMPARE_RENDERER=glow|wgpu picks the renderer (default glow).

mod app;
mod chart;
mod data;
mod flash;
mod pyramid;
mod table;
mod theme;
mod thumbs;

fn renderer() -> eframe::Renderer {
    let want = std::env::var("VCOMPARE_RENDERER").unwrap_or_default();
    #[cfg(feature = "wgpu")]
    if want.eq_ignore_ascii_case("wgpu") {
        return eframe::Renderer::Wgpu;
    }
    #[cfg(feature = "glow")]
    {
        let _ = want;
        eframe::Renderer::Glow
    }
    #[cfg(not(feature = "glow"))]
    {
        let _ = want;
        eframe::Renderer::Wgpu
    }
}

fn main() -> eframe::Result {
    let open = std::env::args().nth(1);
    let renderer = renderer();
    eprintln!("[vcompare] renderer: {renderer:?}");
    let options = eframe::NativeOptions {
        renderer,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("vcompare")
            .with_app_id("vcompare")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 500.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("vcompare", options, Box::new(move |cc| Ok(Box::new(app::VcApp::new(cc, open)))))
}
