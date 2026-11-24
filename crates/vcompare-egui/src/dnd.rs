//! Files dragged onto the window: what's hovering, where the pointer is,
//! and which box a drop lands in.
//!
//! winit (0.30) tells us *which* files are being dragged but not *where*:
//! during a drag the source owns the pointer, so the window gets no motion
//! events (X11 sends only XdndPosition, which winit swallows; macOS the
//! same). Two things make per-box drop targets work anyway:
//!
//! - On X11 we ask the server where the pointer is (QueryPointer on our
//!   window, its own connection) on each frame while files hover.
//! - Elsewhere, a drop waits briefly for the first pointer position after
//!   the drag ends (the pointer is still where the files were let go) and
//!   falls back to the default box if none comes.
//!
//! egui repaints nonstop while files hover, and the drag source waits for
//! our reply to every XdndPosition, which winit only sends between frames:
//! with vsync that made the drag stutter. So the hover is taken out of
//! egui's input (`take`), and we repaint at `HOVER_FRAME` instead.

use eframe::egui::{self, Pos2, RawInput};
use std::time::{Duration, Instant};

/// Repaint interval while files hover (the box highlight follows the pointer).
pub const HOVER_FRAME: Duration = Duration::from_millis(40);
/// How long a drop waits for a pointer position before using the default box.
const DROP_WAIT: Duration = Duration::from_millis(400);

#[derive(Default)]
pub struct Dnd {
    /// Paths being dragged over the window, in the source's order.
    pub hovered: Vec<String>,
    /// Where the pointer is during the hover (points), when known.
    pub pointer: Option<Pos2>,
    /// Dropped this frame (from `take`), not yet handled.
    dropped: Vec<String>,
    /// A drop waiting for a pointer position: the files, when, and the
    /// (stale) position egui had at that moment.
    pending: Option<(Vec<String>, Instant, Option<Pos2>)>,
    #[cfg(target_os = "linux")]
    x11: x11::Pointer,
}

/// A drop to act on: the files, and where they were let go (if known).
pub struct Drop {
    pub paths: Vec<String>,
    pub at: Option<Pos2>,
}

fn path_of(f: &egui::HoveredFile) -> Option<String> {
    f.path.as_ref().map(|p| p.to_string_lossy().to_string())
}

impl Dnd {
    /// From `raw_input_hook`, before egui sees the input.
    pub fn take(&mut self, raw: &mut RawInput) {
        self.hovered = raw.hovered_files.iter().filter_map(path_of).collect();
        raw.hovered_files.clear();
        let dropped: Vec<String> =
            raw.dropped_files.drain(..).map(|f| f.path().to_string_lossy().to_string()).collect();
        if !dropped.is_empty() {
            self.dropped = dropped;
        }
    }

    pub fn hovering(&self) -> bool {
        !self.hovered.is_empty()
    }

    /// Once per frame. Updates the hover pointer, and returns a drop when
    /// one is ready to act on.
    pub fn update(&mut self, ctx: &egui::Context, frame: &eframe::Frame) -> Option<Drop> {
        if self.hovering() {
            self.pointer = self.query(ctx, frame);
            ctx.request_repaint_after(HOVER_FRAME);
        }
        if !self.dropped.is_empty() {
            let paths = std::mem::take(&mut self.dropped);
            // The live position (X11), else the last one seen while hovering.
            let at = self.query(ctx, frame).or(self.pointer.take());
            if at.is_some() {
                return Some(Drop { paths, at });
            }
            self.pending = Some((paths, Instant::now(), ctx.input(|i| i.pointer.latest_pos())));
        }
        let (_, since, stale) = self.pending.as_ref()?;
        let now = ctx.input(|i| i.pointer.latest_pos());
        let fresh = now.filter(|p| Some(*p) != *stale);
        if fresh.is_none() && since.elapsed() < DROP_WAIT {
            ctx.request_repaint_after(Duration::from_millis(30));
            return None;
        }
        let (paths, _, _) = self.pending.take()?;
        Some(Drop { paths, at: fresh })
    }

    #[cfg(target_os = "linux")]
    fn query(&mut self, ctx: &egui::Context, frame: &eframe::Frame) -> Option<Pos2> {
        self.x11.query(frame).map(|(x, y)| Pos2::new(x as f32, y as f32) / ctx.pixels_per_point())
    }

    #[cfg(not(target_os = "linux"))]
    fn query(&mut self, _ctx: &egui::Context, _frame: &eframe::Frame) -> Option<Pos2> {
        None
    }
}

#[cfg(target_os = "linux")]
mod x11 {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt;
    use x11rb::rust_connection::RustConnection;

    /// Our own X connection, opened on first use; `None` on Wayland or if
    /// it can't be had (then drops fall back to the post-drop position).
    #[derive(Default)]
    pub struct Pointer {
        conn: Option<RustConnection>,
        tried: bool,
    }

    impl Pointer {
        /// The pointer in the window's pixels, if it's over the window.
        pub fn query(&mut self, frame: &eframe::Frame) -> Option<(i32, i32)> {
            let window = match frame.window_handle().ok()?.as_raw() {
                RawWindowHandle::Xlib(h) => h.window as u32,
                RawWindowHandle::Xcb(h) => h.window.get(),
                _ => return None,
            };
            if !self.tried {
                self.tried = true;
                self.conn = RustConnection::connect(None).ok().map(|(c, _)| c);
            }
            let conn = self.conn.as_ref()?;
            let reply = conn.query_pointer(window).ok()?.reply().ok()?;
            let geo = conn.get_geometry(window).ok()?.reply().ok()?;
            let (x, y) = (reply.win_x as i32, reply.win_y as i32);
            let inside = reply.same_screen && x >= 0 && y >= 0 && x < geo.width as i32 && y < geo.height as i32;
            let _ = conn.flush();
            inside.then_some((x, y))
        }
    }
}
