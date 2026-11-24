//! vcompare, native UI (egui).
//!
//! `vcompare-egui [analysis.json | media file(s)]`: an analysis opens;
//! media fills the start screen (two files: reference, distorted).
//!
//! VCOMPARE_RENDERER=glow|wgpu picks the renderer (default glow);
//! VCOMPARE_VSYNC=0 turns vsync off.

mod app;
mod chart;
mod data;
mod detector;
mod pick;
mod progress;
mod pyramid;
mod settings;
mod table;
mod theme;
mod thumbs;
mod timeline;

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
    let open: Vec<String> = std::env::args().skip(1).collect();
    let renderer = renderer();
    eprintln!("[vcompare] renderer: {renderer:?}");
    // VCOMPARE_VSYNC=0: present without waiting for vblank (less latency,
    // may tear). For comparing; the default stays on.
    let vsync = std::env::var("VCOMPARE_VSYNC").map(|v| v != "0").unwrap_or(true);
    let mut options = eframe::NativeOptions {
        renderer,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("vcompare")
            .with_app_id("vcompare")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 500.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    #[cfg(feature = "glow")]
    {
        options.glow_options.vsync = vsync;
    }
    #[cfg(feature = "wgpu")]
    {
        // One frame in flight instead of wgpu's default two.
        options.wgpu_options.surface.desired_maximum_frame_latency = Some(1);
        if !vsync {
            options.wgpu_options.surface.present_mode = eframe::wgpu::PresentMode::AutoNoVsync;
        }
    }
    eprintln!("[vcompare] vsync: {vsync}");
    eframe::run_native("vcompare", options, Box::new(move |cc| Ok(Box::new(app::VcApp::new(cc, open)))))
}
