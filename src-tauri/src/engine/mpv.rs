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
use std::process::{Child, Stdio};
use std::sync::Mutex;
use std::time::Duration;

/// The first few error / fatal lines of an mpv log file, as "module: text".
/// Video-output lines are left out: mpv probes several outputs at start-up
/// and logs each one that doesn't apply as an error.
fn log_errors(log: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in log.lines() {
        // "[   0.031][e][file] Cannot open file …"
        let mut parts = line.splitn(4, ']');
        let (Some(_time), Some(level), Some(module), Some(text)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if level != "[e" && level != "[f" {
            continue;
        }
        let module = module.trim_start_matches('[');
        if module.starts_with("vo/") {
            continue;
        }
        let msg = format!("{module}: {}", text.trim());
        if !out.contains(&msg) {
            out.push(msg);
        }
        if out.len() == 3 {
            break;
        }
    }
    out
}

use crate::engine::timecode::TimecodeFormat;
use crate::engine::types::TimelineInfo;

/// How to combine the two video streams in mpv.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ViewMode {
    /// Side-by-side: `[ref][dist]hstack[vo]`
    SideBySide,
    /// Pixel difference: `[ref][dist]blend=all_mode=difference[vo]`
    Difference,
    /// Just the reference file, no second input (levels mode).
    Single,
}

impl ViewMode {
    fn suffix(self) -> &'static str {
        match self {
            ViewMode::SideBySide => "sbs",
            ViewMode::Difference => "diff",
            ViewMode::Single => "single",
        }
    }
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
    /// mpv's own log (`--log-file`), read back if it quits with an error
    log_path: PathBuf,
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
        let suffix = view_mode.suffix();
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
            log_path: std::env::temp_dir().join(format!("vcompare_mpv_{pid}_{suffix}.log")),
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

        // mpv itself only says "cannot open" in a log nobody sees, then
        // quits: check the files first and say which one is missing.
        for p in [&self.ref_path, &self.dist_path] {
            if !p.is_empty() && !Path::new(p).exists() {
                return Some(format!(
                    "file not found: {p} — moved, renamed, or its volume isn't mounted?"
                ));
            }
        }

        // Clean up any stale socket / EDL / log
        let _ = std::fs::remove_file(&self.sock_path);
        let _ = std::fs::remove_file(&self.log_path);
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

        let mut cmd = crate::engine::sysenv::command("mpv");

        let title = if self.view_mode == ViewMode::Single {
            eprintln!("[vcompare] mpv: single file {ref_name}");
            cmd.arg(&self.ref_path);
            format!("--title=vcompare: {ref_name}")
        } else {
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
                ViewMode::Single => unreachable!(),
            };
            cmd.arg(&primary)
                .arg(format!("--external-file={}", external))
                .arg(format!("--lavfi-complex={lavfi}"));
            format!("--title=vcompare [{mode_label}]: {ref_name} | {dist_name}")
        };

        let cmd_result = cmd
            .arg(format!("--input-ipc-server={sock_str}"))
            .arg("--pause")
            .arg("--hr-seek=yes")
            .arg("--keep-open=yes")
            .arg("--force-window=yes")
            .arg("--autofit=25%x25%")
            .arg("--osd-level=2")
            .arg("--osd-fractions")
            .arg(title)
            .arg("--no-terminal")
            .arg(format!("--log-file={}", self.log_path.to_string_lossy()))
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
            if !self.is_running() {
                return Some(
                    self.exit_report()
                        .unwrap_or_else(|| "mpv quit as soon as it started".into()),
                );
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
            ViewMode::Single => String::new(),
        }
    }

    /// Write a temporary mpv EDL file that starts playback at the given offset.
    ///
    /// EDL format:
    /// ```text
    /// # mpv EDL v0
    /// /path/to/file.mov,56.48
    /// ```
    fn write_edl(&self, file_path: &str, start_sec: f64) -> PathBuf {
        let suffix = self.view_mode.suffix();
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

    /// Why mpv went away, if it wasn't asked to: `None` after `kill()` or a
    /// normal quit (window closed); otherwise the first errors from its log
    /// (a file it could not open, a filter graph it rejected, …).
    pub fn exit_report(&self) -> Option<String> {
        let code = {
            let mut proc = self.process.lock().unwrap();
            let status = proc.as_mut()?.try_wait().ok().flatten()?;
            if status.success() {
                return None;
            }
            status.code()
        };
        let errors = std::fs::read_to_string(&self.log_path)
            .map(|log| log_errors(&log))
            .unwrap_or_default();
        Some(if errors.is_empty() {
            match code {
                Some(c) => format!("mpv quit with an error (exit code {c}); its log has no error lines: {}", self.log_path.display()),
                None => "mpv was killed by a signal (crashed?)".into(),
            }
        } else {
            format!("mpv quit: {}", errors.join(" · "))
        })
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

#[cfg(test)]
mod exit_report_tests {
    use super::*;

    #[test]
    fn log_errors_picks_the_cause() {
        let log = "[   0.012][v][cplayer] mpv 0.37.0\n\
[   0.160][e][file] Cannot open file '/x/b.mov': No such file or directory\n\
[   0.160][e][stream] Failed to open /x/b.mov.\n\
[   0.160][e][cplayer] Can not open external file /x/b.mov.\n\
[   0.282][e][vo/gpu/drm] Can't open TTY for VT control\n\
[   0.300][e][ffmpeg] later noise\n";
        assert_eq!(
            log_errors(log),
            vec![
                "file: Cannot open file '/x/b.mov': No such file or directory",
                "stream: Failed to open /x/b.mov.",
                "cplayer: Can not open external file /x/b.mov.",
            ]
        );
        assert!(log_errors("[   0.2][e][vo/gpu/drm] probing\n[  0.3][w][x] warn\n").is_empty());
    }

    #[test]
    fn missing_file_is_reported_before_launch() {
        let tc = TimecodeFormat { fps_num: 24, fps_den: 1, tc_type: crate::engine::timecode::TcType::NonDrop };
        let m = MpvController::new("/nonexistent/a.mov".into(), String::new(), tc, 0, None, ViewMode::Single, 0, 0, false);
        let err = m.launch().unwrap();
        assert!(err.contains("file not found: /nonexistent/a.mov"), "{err}");
    }

    /// A file that exists but isn't video: mpv starts, fails to load it and
    /// quits. That used to be silent. Needs mpv and a display (xvfb-run).
    #[test]
    fn unplayable_file_is_explained() {
        let have_mpv = crate::engine::sysenv::command("mpv").arg("--version").output().is_ok();
        if !have_mpv || std::env::var_os("DISPLAY").is_none() {
            eprintln!("mpv or a display unavailable; skipping");
            return;
        }
        let path = std::env::temp_dir().join(format!("vcompare_notvideo_{}.mov", std::process::id()));
        std::fs::write(&path, b"this is not a movie").unwrap();
        let tc = TimecodeFormat { fps_num: 24, fps_den: 1, tc_type: crate::engine::timecode::TcType::NonDrop };
        let m = MpvController::new(path.to_string_lossy().into(), String::new(), tc, 0, None, ViewMode::Single, 0, 0, false);
        let report = match m.launch() {
            Some(err) => err, // quit before its socket appeared
            None => {
                let t = std::time::Instant::now();
                while m.is_running() && t.elapsed() < Duration::from_secs(10) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                assert!(!m.is_running(), "mpv should have quit");
                m.exit_report().expect("a report")
            }
        };
        eprintln!("report: {report}");
        assert!(report.starts_with("mpv quit"), "{report}");
        m.kill();
        let _ = std::fs::remove_file(&path);
    }
}
