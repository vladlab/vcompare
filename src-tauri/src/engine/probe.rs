//! FFprobe wrappers, capability detection, and file inspection.
//!
//! All functions in this module call external tools (ffprobe, ffmpeg) via
//! `std::process::Command`. They're synchronous and fast (<100ms typical),
//! so callers should use `spawn_blocking()` when calling from async context.

use serde_json::Value;
use std::path::Path;

use crate::engine::timecode::{find_video_stream, parse_rational, tc_string_to_frames};
use crate::engine::timecode::TimecodeFormat;
use crate::engine::types::{Capabilities, ProbeResult, VideoInfo};

// ─── FFprobe ────────────────────────────────────────────────────────────────

/// Run ffprobe and return parsed JSON. Handles concat manifests.
pub fn ffprobe_json(path: &str) -> Result<Value, String> {
    let mut cmd = crate::engine::sysenv::command("ffprobe");
    cmd.args(["-v", "quiet", "-print_format", "json", "-show_format", "-show_streams"]);

    if is_concat_manifest(path) {
        cmd.args(["-f", "concat", "-safe", "0"]);
    }

    cmd.arg(path);

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to run ffprobe: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ffprobe exited with code {}: {stderr}",
            output.status.code().unwrap_or(-1)
        ));
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse ffprobe JSON: {e}"))
}

/// Check if a file is an ffmpeg concat demuxer manifest.
///
/// Reads only the first 64 bytes — safe to call on multi-GB video files.
pub fn is_concat_manifest(path: &str) -> bool {
    use std::io::Read;

    let p = Path::new(path);
    if !p.is_file() {
        return false;
    }
    let mut buf = [0u8; 64];
    let n = match std::fs::File::open(p).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    head.trim_start().starts_with("ffconcat version")
}

/// Check if a file is an IMF Composition Playlist XML.
///
/// Reads only the first 2048 bytes.
#[allow(dead_code)]
pub fn is_imf_cpl(path: &str) -> bool {
    use std::io::Read;

    let p = Path::new(path);
    if !p.is_file() {
        return false;
    }
    if !path.to_lowercase().ends_with(".xml") {
        return false;
    }
    let mut buf = [0u8; 2048];
    let n = match std::fs::File::open(p).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    head.contains("CompositionPlaylist")
}

// ─── Video info extraction ──────────────────────────────────────────────────

/// Extract detailed video info from ffprobe JSON output.
///
/// Finds the video stream, extracts resolution, frame rate, pixel format,
/// and embedded timecode. The timecode is parsed using the provided format.
pub fn get_video_info(probe: &Value, tc_fmt: &TimecodeFormat) -> Result<VideoInfo, String> {
    let video = find_video_stream(probe).ok_or("No video stream found")?;

    let width = video
        .get("width")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let height = video
        .get("height")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    let r_fr = video
        .get("r_frame_rate")
        .and_then(|v| v.as_str())
        .unwrap_or("24/1");
    let (fps_num, fps_den) =
        parse_rational(r_fr).ok_or_else(|| format!("Invalid r_frame_rate: {r_fr}"))?;

    let nb_frames = video
        .get("nb_frames")
        .and_then(|v| {
            // ffprobe returns nb_frames as a string in most cases,
            // but some codecs/versions may return it as an integer.
            v.as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .or_else(|| v.as_u64())
        })
        .filter(|&n| n > 0);

    let pix_fmt = video
        .get("pix_fmt")
        .and_then(|v| v.as_str())
        .unwrap_or("yuv420p")
        .to_string();

    // Find start timecode — check stream tags, then format tags
    let start_tc = find_timecode(video, probe);
    let start_tc_frames = start_tc
        .as_ref()
        .map(|tc| tc_string_to_frames(tc, tc_fmt))
        .unwrap_or(0);

    Ok(VideoInfo {
        width,
        height,
        fps_num,
        fps_den,
        nb_frames,
        pix_fmt,
        start_tc,
        start_tc_frames,
    })
}

/// Build a `ProbeResult` from a file path — the full probe response for the frontend.
pub fn probe_file(path: &str) -> Result<ProbeResult, String> {
    if !Path::new(path).is_file() {
        return Err(format!("File not found: {path}"));
    }

    let probe = ffprobe_json(path)?;
    let video = find_video_stream(&probe).ok_or("No video stream found")?;

    let r_fr = video
        .get("r_frame_rate")
        .and_then(|v| v.as_str())
        .unwrap_or("24/1");
    let (num, den) =
        parse_rational(r_fr).ok_or_else(|| format!("Invalid r_frame_rate: {r_fr}"))?;

    let nb_frames = video
        .get("nb_frames")
        .and_then(|v| {
            v.as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .or_else(|| v.as_u64())
        })
        .filter(|&n| n > 0);

    let duration = probe
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);

    let timecode = find_timecode(video, &probe);

    let filename = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let fps = (num as f64 / den as f64 * 1000.0).round() / 1000.0;

    Ok(ProbeResult {
        path: path.to_string(),
        filename,
        width: video.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        height: video.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        pix_fmt: video
            .get("pix_fmt")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        codec: video
            .get("codec_name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        fps,
        fps_num: num,
        fps_den: den,
        nb_frames,
        duration: (duration * 1000.0).round() / 1000.0,
        timecode,
    })
}

// ─── Capability detection ───────────────────────────────────────────────────

/// Auto-detect the best available hardware decoder for the current platform.
pub fn detect_hwaccel() -> Option<String> {
    let candidates = if cfg!(target_os = "macos") {
        vec!["videotoolbox"]
    } else if cfg!(target_os = "windows") {
        vec!["cuda", "d3d11va"]
    } else {
        // Linux and others
        vec!["cuda", "vaapi"]
    };

    let output = crate::engine::sysenv::command("ffmpeg")
        .args(["-hide_banner", "-hwaccels"])
        .output()
        .ok()?;

    let supported = String::from_utf8_lossy(&output.stdout).to_lowercase();

    for hw in candidates {
        if supported.contains(hw) {
            return Some(hw.to_string());
        }
    }

    None
}

/// Check if ffmpeg was built with the libvmaf_cuda filter.
pub fn detect_vmaf_cuda() -> bool {
    let output = match crate::engine::sysenv::command("ffmpeg")
        .args(["-hide_banner", "-filters"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().any(|line| line.contains("libvmaf_cuda"))
}

/// Detect all capabilities in one call — used by the frontend at startup.
pub fn detect_capabilities() -> Capabilities {
    let hwaccel = detect_hwaccel();
    let vmaf_cuda = detect_vmaf_cuda();
    let has_mpv = detect_mpv();

    Capabilities {
        hwaccel,
        vmaf_cuda,
        has_mpv,
        grabber: crate::engine::grab::find_binary().map(|p| p.to_string_lossy().to_string()),
    }
}

/// Check if mpv is available in PATH.
fn detect_mpv() -> bool {
    crate::engine::sysenv::command("mpv")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Find the timecode string from stream tags or format tags.
fn find_timecode(video: &Value, probe: &Value) -> Option<String> {
    // Check stream tags first
    if let Some(tc) = video
        .get("tags")
        .and_then(|t| t.get("timecode"))
        .and_then(|v| v.as_str())
    {
        return Some(tc.to_string());
    }

    // Then format tags
    if let Some(tc) = probe
        .get("format")
        .and_then(|f| f.get("tags"))
        .and_then(|t| t.get("timecode"))
        .and_then(|v| v.as_str())
    {
        return Some(tc.to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_manifest_detection() {
        // Can't test with real files in unit tests, but test the path handling
        assert!(!is_concat_manifest("/nonexistent/file.txt"));
        assert!(!is_concat_manifest(""));
    }

    #[test]
    fn imf_cpl_detection() {
        assert!(!is_imf_cpl("/nonexistent/file.xml"));
        assert!(!is_imf_cpl("/some/file.txt")); // wrong extension
    }

    #[test]
    fn find_timecode_stream_tags() {
        let video = serde_json::json!({
            "tags": {"timecode": "01:00:00:00"}
        });
        let probe = serde_json::json!({"format": {}});
        assert_eq!(find_timecode(&video, &probe), Some("01:00:00:00".into()));
    }

    #[test]
    fn find_timecode_format_tags() {
        let video = serde_json::json!({"tags": {}});
        let probe = serde_json::json!({
            "format": {"tags": {"timecode": "02:00:00:00"}}
        });
        assert_eq!(find_timecode(&video, &probe), Some("02:00:00:00".into()));
    }

    #[test]
    fn find_timecode_none() {
        let video = serde_json::json!({});
        let probe = serde_json::json!({"format": {}});
        assert_eq!(find_timecode(&video, &probe), None);
    }

    #[test]
    fn get_video_info_basic() {
        let probe = serde_json::json!({
            "streams": [{
                "codec_type": "video",
                "width": 1920,
                "height": 1080,
                "r_frame_rate": "24/1",
                "nb_frames": "1000",
                "pix_fmt": "yuv422p10le",
                "tags": {"timecode": "01:00:00:00"}
            }],
            "format": {"duration": "41.666"}
        });

        let tc_fmt = TimecodeFormat {
            fps_num: 24,
            fps_den: 1,
            tc_type: crate::engine::timecode::TcType::NonDrop,
        };

        let info = get_video_info(&probe, &tc_fmt).unwrap();
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert_eq!(info.fps_num, 24);
        assert_eq!(info.fps_den, 1);
        assert_eq!(info.nb_frames, Some(1000));
        assert_eq!(info.pix_fmt, "yuv422p10le");
        assert_eq!(info.start_tc.as_deref(), Some("01:00:00:00"));
        assert_eq!(info.start_tc_frames, 86400); // 1hr at 24fps
    }
}
