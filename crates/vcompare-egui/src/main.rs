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
mod dnd;
mod pick;
mod progress;
mod pyramid;
mod settings;
mod table;
mod theme;
mod thumbs;
mod timeline;
mod tools;

fn renderer() -> eframe::Renderer {
    let want = std::env::var("VCOMPARE_RENDERER").unwrap_or_default();
    // macOS: wgpu runs on Metal, glow on Apple's deprecated OpenGL, so
    // wgpu is the default there; elsewhere glow is, until the spike decides.
    #[cfg(feature = "wgpu")]
    if want.eq_ignore_ascii_case("wgpu") || (cfg!(target_os = "macos") && !want.eq_ignore_ascii_case("glow")) {
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

/// The window icon (X11 shows it in the taskbar / alt-tab; Wayland and
/// macOS take theirs from the .desktop file / app bundle).
fn icon() -> eframe::egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../../../packaging/icons/256x256.png")).unwrap_or_default()
}

/// X11 unless asked for Wayland or there's no X server to talk to.
#[cfg(target_os = "linux")]
fn prefer_x11() -> bool {
    let wayland = std::env::var("VCOMPARE_WAYLAND").is_ok_and(|v| v == "1");
    let x = std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty());
    eprintln!("[vcompare] backend: {}", if x && !wayland { "x11" } else { "default (wayland if available)" });
    x && !wayland
}

fn main() -> eframe::Result {
    let open: Vec<String> = std::env::args().skip(1).filter(|a| !a.is_empty()).collect();
    let renderer = renderer();
    eprintln!("[vcompare] renderer: {renderer:?}");
    // VCOMPARE_VSYNC=0: present without waiting for vblank (less latency,
    // may tear). For comparing; the default stays on.
    let vsync = std::env::var("VCOMPARE_VSYNC").map(|v| v != "0").unwrap_or(true);
    let mut options = eframe::NativeOptions {
        renderer,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("vcompare")
            .with_app_id("vcompare-egui")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 500.0])
            .with_drag_and_drop(true)
            .with_icon(icon()),
        ..Default::default()
    };
    #[cfg(target_os = "linux")]
    if prefer_x11() {
        // winit 0.30 has no file drag and drop on Wayland, so use X11
        // (XWayland on a Wayland desktop). VCOMPARE_WAYLAND=1 for native.
        options.event_loop_builder = Some(Box::new(|b| {
            use winit::platform::x11::EventLoopBuilderExtX11;
            b.with_x11();
        }));
    }
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
