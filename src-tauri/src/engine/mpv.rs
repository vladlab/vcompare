//! mpv controller — manages an mpv instance with JSON IPC.
//!
//! Uses Unix domain sockets for frame-accurate seeking and position polling.
//! Supports TC-aligned side-by-side playback: when clips have different TC
//! start points, the file that needs to skip ahead is wrapped in an mpv EDL
//! (Edit Decision List) which provides a native start-time offset while
//! preserving mpv's fast keyframe seeking.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use crate::engine::timecode::TimecodeFormat;
use crate::engine::types::TimelineInfo;

/// How to combine the two video streams in mpv.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ViewMode {
    /// Side-by-side: `[ref][dist]hstack[vo]`
    SideBySide,
    /// Pixel difference: `[ref][dist]blend=all_mode=difference[vo]`
    Difference,
}

/// mpv process controller with IPC.
pub struct MpvController {
    ref_path: String,
    dist_path: String,
    tc_format: TimecodeFormat,
    /// First frame of the ref file in TC space (used for non-timeline seek)
    ref_file_start_tc: i64,
    /// Timeline info for TC-aligned playback
    timeline: Option<TimelineInfo>,
    /// How to combine the two streams
    view_mode: ViewMode,
    /// If dist needs scaling to match ref (0 = no scaling)
    scale_w: u32,
    scale_h: u32,
    /// true = scale ref, false = scale dist
    scale_ref: bool,
    process: Mutex<Option<Child>>,
    sock_path: PathBuf,
    /// Temp EDL file path (cleaned up on kill)
    edl_path: Mutex<Option<PathBuf>>,
}

impl MpvController {
    pub fn new(
        ref_path: String,
        dist_path: String,
        tc_format: TimecodeFormat,
        ref_file_start_tc: i64,
        timeline: Option<TimelineInfo>,
        view_mode: ViewMode,
        scale_w: u32,
        scale_h: u32,
        scale_ref: bool,
    ) -> Self {
        let pid = std::process::id();
        let suffix = match view_mode {
            ViewMode::SideBySide => "sbs",
            ViewMode::Difference => "diff",
        };
        let sock_path = std::env::temp_dir().join(format!("vcompare_mpv_{pid}_{suffix}.sock"));
        Self {
            ref_path,
            dist_path,
            tc_format,
            ref_file_start_tc,
            timeline,
            view_mode,
            scale_w,
            scale_h,
            scale_ref,
            process: Mutex::new(None),
            sock_path,
            edl_path: Mutex::new(None),
        }
    }

    /// Check if the mpv process is still running.
    pub fn is_running(&self) -> bool {
        let mut proc = self.process.lock().unwrap();
        match proc.as_mut() {
            Some(p) => p.try_wait().ok().flatten().is_none(),
            None => false,
        }
    }

    /// Whether clips have a TC offset requiring alignment.
    fn has_offset(&self) -> bool {
        match &self.timeline {
            Some(tl) => tl.ref_start != tl.dist_start || tl.ref_end != tl.dist_end,
            None => false,
        }
    }

    /// Launch mpv with side-by-side view. Returns error string or None.
    pub fn launch(&self) -> Option<String> {
        if self.is_running() {
            return Some("mpv already running".into());
        }

        // Clean up any stale socket / EDL
        let _ = std::fs::remove_file(&self.sock_path);
        {
            let mut edl = self.edl_path.lock().unwrap();
            if let Some(ref p) = *edl {
                let _ = std::fs::remove_file(p);
            }
            *edl = None;
        }

        let ref_name = Path::new(&self.ref_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let dist_name = Path::new(&self.dist_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let sock_str = self.sock_path.to_string_lossy().to_string();

        // Determine alignment: which file is primary (at time 0) and which
        // needs an EDL wrapper to skip to the overlap start.
        let (primary, external, lavfi) = self.build_inputs();

        eprintln!(
            "[vcompare] mpv: primary={}, external={}, lavfi={}",
            Path::new(&primary).file_name().unwrap_or_default().to_string_lossy(),
            Path::new(&external).file_name().unwrap_or_default().to_string_lossy(),
            lavfi
        );

        let mode_label = match self.view_mode {
            ViewMode::SideBySide => "SBS",
            ViewMode::Difference => "DIFF",
        };

        let cmd_result = Command::new("mpv")
            .arg(&primary)
            .arg(format!("--external-file={}", external))
            .arg(format!("--lavfi-complex={lavfi}"))
            .arg(format!("--input-ipc-server={sock_str}"))
            .arg("--pause")
            .arg("--hr-seek=yes")
            .arg("--keep-open=yes")
            .arg("--force-window=yes")
            .arg("--autofit=25%x25%")
            .arg("--osd-level=2")
            .arg("--osd-fractions")
            .arg(format!("--title=vcompare [{mode_label}]: {ref_name} | {dist_name}"))
            .arg("--no-terminal")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        match cmd_result {
            Ok(child) => {
                *self.process.lock().unwrap() = Some(child);
            }
            Err(_) => return Some("mpv not found in PATH".into()),
        }

        // Wait for IPC socket to appear (up to 5 seconds)
        for _ in 0..50 {
            if self.sock_path.exists() {
                return None; // Success
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        Some("mpv started but IPC socket not ready (timeout)".into())
    }

    /// Determine primary / external inputs and lavfi string.
    ///
    /// When clips have a TC offset, the file that needs to skip ahead
    /// (to reach the overlap start) is wrapped in an mpv EDL file.
    /// The other file is the primary input (starts at time 0 = overlap start).
    ///
    /// Returns (primary_path, external_path, lavfi_string).
    fn build_inputs(&self) -> (String, String, String) {
        let tl = match &self.timeline {
            Some(tl) if self.has_offset() => tl.clone(),
            _ => {
                return (
                    self.ref_path.clone(),
                    self.dist_path.clone(),
                    self.make_lavfi("[vid1]", "[vid2]"),
                );
            }
        };

        let fps = self.tc_format.fps_float();
        let ref_skip_sec = (tl.overlap_start - tl.ref_start).max(0) as f64 / fps;
        let dist_skip_sec = (tl.overlap_start - tl.dist_start).max(0) as f64 / fps;

        if ref_skip_sec > dist_skip_sec {
            // Ref needs to skip ahead → wrap ref in EDL, dist is primary
            let edl_path = self.write_edl(&self.ref_path, ref_skip_sec);
            let edl_str = edl_path.to_string_lossy().to_string();
            *self.edl_path.lock().unwrap() = Some(edl_path);

            // vid1=dist(primary), vid2=ref(EDL)
            // ref on left, dist on right → ref=[vid2], dist=[vid1]
            (
                self.dist_path.clone(),
                edl_str,
                self.make_lavfi("[vid2]", "[vid1]"),
            )
        } else if dist_skip_sec > 0.01 {
            // Dist needs to skip ahead → wrap dist in EDL, ref is primary
            let edl_path = self.write_edl(&self.dist_path, dist_skip_sec);
            let edl_str = edl_path.to_string_lossy().to_string();
            *self.edl_path.lock().unwrap() = Some(edl_path);

            (
                self.ref_path.clone(),
                edl_str,
                self.make_lavfi("[vid1]", "[vid2]"),
            )
        } else {
            // No skip needed — both start at overlap
            (
                self.ref_path.clone(),
                self.dist_path.clone(),
                self.make_lavfi("[vid1]", "[vid2]"),
            )
        }
    }

    /// Build the lavfi filter string for the given input labels.
    /// `ref_label` and `dist_label` are the mpv stream labels (e.g. "[vid1]", "[vid2]").
    fn make_lavfi(&self, ref_label: &str, dist_label: &str) -> String {
        // If scaling needed, prepend a scale filter to the correct stream
        let scale_filter = if self.scale_w > 0 {
            Some(format!("scale={}:{}:flags=lanczos", self.scale_w, self.scale_h))
        } else {
            None
        };

        let (ref_expr, ref_out, dist_expr, dist_out) = match &scale_filter {
            Some(scale) if self.scale_ref => (
                format!("{ref_label}{scale}[_scaled];"),
                "[_scaled]",
                String::new(),
                dist_label,
            ),
            Some(scale) => (
                String::new(),
                ref_label,
                format!("{dist_label}{scale}[_scaled];"),
                "[_scaled]",
            ),
            None => (String::new(), ref_label, String::new(), dist_label),
        };

        match self.view_mode {
            ViewMode::SideBySide => format!("{ref_expr}{dist_expr}{ref_out}{dist_out}hstack[vo]"),
            ViewMode::Difference => format!(
                "{ref_expr}{dist_expr}{ref_out}format=gbrp[_r];{dist_out}format=gbrp[_d];[_r][_d]blend=all_mode=difference[vo]"
            ),
        }
    }

    /// Write a temporary mpv EDL file that starts playback at the given offset.
    ///
    /// EDL format:
    /// ```
    /// # mpv EDL v0
    /// /path/to/file.mov,56.48
    /// ```
    fn write_edl(&self, file_path: &str, start_sec: f64) -> PathBuf {
        let suffix = match self.view_mode {
            ViewMode::SideBySide => "sbs",
            ViewMode::Difference => "diff",
        };
        let edl_path = std::env::temp_dir().join(format!(
            "vcompare_mpv_{}_{}.edl",
            std::process::id(), suffix
        ));
        let content = format!("# mpv EDL v0\n{},{:.6}\n", file_path, start_sec);
        eprintln!("[vcompare] mpv EDL: {}", content.trim());
        let _ = std::fs::write(&edl_path, &content);
        edl_path
    }

    /// Send a JSON IPC command to mpv and return the response.
    fn send_command(&self, args: &[&str]) -> Option<Value> {
        if !self.is_running() {
            return None;
        }

        let msg = serde_json::json!({"command": args});
        let msg_str = format!("{}\n", serde_json::to_string(&msg).ok()?);

        let mut stream = UnixStream::connect(&self.sock_path).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .ok()?;
        stream.write_all(msg_str.as_bytes()).ok()?;

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response).ok()?;

        serde_json::from_str(&response).ok()
    }

    /// Send a command with mixed argument types.
    fn send_command_val(&self, args: Vec<Value>) -> Option<Value> {
        if !self.is_running() {
            return None;
        }

        let msg = serde_json::json!({"command": args});
        let msg_str = format!("{}\n", serde_json::to_string(&msg).ok()?);

        let mut stream = UnixStream::connect(&self.sock_path).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .ok()?;
        stream.write_all(msg_str.as_bytes()).ok()?;

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response).ok()?;

        serde_json::from_str(&response).ok()
    }

    /// The TC frame that corresponds to mpv time=0.
    ///
    /// With offset: overlap_start (both inputs are aligned here via EDL).
    /// Without offset: ref_file_start_tc.
    fn time_origin(&self) -> i64 {
        match &self.timeline {
            Some(tl) if self.has_offset() => tl.overlap_start,
            _ => self.ref_file_start_tc,
        }
    }

    /// Seek mpv to a display frame number (absolute TC frame).
    pub fn seek_to_frame(&self, frame_num: i64) -> bool {
        if !self.is_running() {
            return false;
        }
        let file_frame = frame_num - self.time_origin();
        let seconds = (file_frame as f64 / self.tc_format.fps_float()).max(0.0);
        let result = self.send_command_val(vec![
            "set_property".into(),
            "time-pos".into(),
            serde_json::json!(seconds),
        ]);
        result.is_some()
    }

    /// Get mpv's current position as a display frame number.
    pub fn get_frame(&self) -> Option<i64> {
        if !self.is_running() {
            return None;
        }
        let result = self.send_command(&["get_property", "time-pos"])?;
        let seconds = result.get("data")?.as_f64()?;
        let file_frame = (seconds * self.tc_format.fps_float()).round() as i64;
        Some(file_frame + self.time_origin())
    }

    /// Step one frame forward.
    pub fn step_forward(&self) -> bool {
        self.send_command(&["frame-step"]).is_some()
    }

    /// Step one frame backward.
    pub fn step_backward(&self) -> bool {
        self.send_command(&["frame-back-step"]).is_some()
    }

    /// Toggle play/pause.
    pub fn toggle_pause(&self) -> bool {
        self.send_command(&["cycle", "pause"]).is_some()
    }

    /// Terminate mpv and clean up.
    pub fn kill(&self) {
        let _ = self.send_command(&["quit"]);

        let mut proc = self.process.lock().unwrap();
        if let Some(ref mut child) = *proc {
            match child.try_wait() {
                Ok(Some(_)) => {}
                _ => {
                    std::thread::sleep(Duration::from_secs(1));
                    match child.try_wait() {
                        Ok(Some(_)) => {}
                        _ => {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                }
            }
        }
        *proc = None;

        let _ = std::fs::remove_file(&self.sock_path);
        if let Some(ref p) = *self.edl_path.lock().unwrap() {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl Drop for MpvController {
    fn drop(&mut self) {
        self.kill();
    }
}
