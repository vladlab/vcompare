//! Analysis cache — save, load, and check .vcompare.json files.

use serde_json::Value;
use std::path::Path;

use crate::engine::timecode::{TcType, TimecodeFormat};

/// Default cache path for an analysis: `{ref_dir}/{ref_stem}_vs_{dist_stem}.vcompare.json`
pub fn default_log_path(ref_path: &str, dist_path: &str) -> String {
    let ref_p = Path::new(ref_path);
    let dist_p = Path::new(dist_path);

    let ref_stem = ref_p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "ref".into());
    let dist_stem = dist_p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "dist".into());
    let ref_dir = ref_p.parent().unwrap_or_else(|| Path::new("."));

    ref_dir
        .join(format!("{ref_stem}_vs_{dist_stem}.vcompare.json"))
        .to_string_lossy()
        .to_string()
}

/// Default cache path for levels analysis: `{dir}/{stem}.levels.json`
pub fn default_levels_path(file_path: &str) -> String {
    let p = Path::new(file_path);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let dir = p.parent().unwrap_or_else(|| Path::new("."));

    dir.join(format!("{stem}.levels.json"))
        .to_string_lossy()
        .to_string()
}

/// Default path for a timeline-mode analysis: `{dir}/{stem}.timeline.json`
pub fn default_timeline_path(file_path: &str) -> String {
    let p = Path::new(file_path);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let dir = p.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{stem}.timeline.json")).to_string_lossy().to_string()
}

/// Check if a cached analysis exists and return info about it.
pub fn check_cache(ref_path: &str, dist_path: &str, log_path: Option<&str>) -> Value {
    let path = log_path
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_log_path(ref_path, dist_path));

    if Path::new(&path).is_file() {
        match std::fs::read_to_string(&path).and_then(|s| {
            serde_json::from_str::<Value>(&s).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        }) {
            Ok(data) => {
                let mode = data
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("vmaf")
                    .to_string();
                let fc = data
                    .get("frames")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let sync_mode = data.get("sync_mode").and_then(|v| v.as_str());
                let with_ssim = data.get("with_ssim").and_then(|v| v.as_bool()).unwrap_or(false);

                serde_json::json!({
                    "exists": true,
                    "path": path,
                    "frame_count": fc,
                    "mode": mode,
                    "sync_mode": sync_mode,
                    "with_ssim": with_ssim,
                })
            }
            Err(_) => serde_json::json!({
                "exists": false,
                "path": path,
                "frame_count": null,
            }),
        }
    } else {
        serde_json::json!({
            "exists": false,
            "path": path,
            "frame_count": null,
        })
    }
}

/// Load a cached diff analysis (mode == "diff").
pub fn load_diff_cache(path: &str) -> Result<Value, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read cache: {e}"))?;
    let data: Value =
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse cache: {e}"))?;

    if data.get("mode").and_then(|v| v.as_str()) != Some("diff") {
        return Err("Not a diff cache file".into());
    }
    Ok(data)
}

/// Load a cached VMAF analysis — returns frames as JSON values.
///
/// Mirrors vcompare.py's `load_analysis()`.
pub fn load_vmaf_analysis(path: &str) -> Result<LoadedVmafAnalysis, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read analysis: {e}"))?;
    let data: Value =
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse analysis: {e}"))?;

    let tc_data = data.get("tc_format");
    let tc_fmt = tc_data.and_then(|v| {
        let obj = v.as_object()?;
        Some(TimecodeFormat {
            fps_num: obj.get("fps_num")?.as_u64()? as u32,
            fps_den: obj.get("fps_den")?.as_u64()? as u32,
            tc_type: match obj.get("tc_type")?.as_str()? {
                "DF" => TcType::DropFrame,
                _ => TcType::NonDrop,
            },
        })
    });

    let raw_frames = data
        .get("frames")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // Convert to the format the engine state expects (with "tc" key, rounding)
    let frames: Vec<Value> = raw_frames
        .into_iter()
        .map(|fd| {
            let frame = fd.get("frame").and_then(|v| v.as_i64()).unwrap_or(0);
            let vmaf = fd.get("vmaf").and_then(|v| v.as_f64());
            let psnr = fd.get("psnr").and_then(|v| v.as_f64());
            let tc = fd
                .get("timecode")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    tc_fmt
                        .as_ref()
                        .map(|tc| tc.frame_to_tc(frame))
                        .unwrap_or_else(|| format!("F{frame}"))
                });

            serde_json::json!({
                "frame": frame,
                "tc": tc,
                "vmaf": vmaf.map(|v| (v * 1000.0).round() / 1000.0),
                "psnr": psnr.map(|v| (v * 1000.0).round() / 1000.0),
            })
        })
        .collect();

    Ok(LoadedVmafAnalysis {
        frames,
        ref_path: data
            .get("ref_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        dist_path: data
            .get("dist_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        ref_start_frame: data
            .get("ref_start_frame")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        ref_file_start_tc: data
            .get("ref_file_start_tc")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        sync_mode: data
            .get("sync_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("start")
            .to_string(),
        tc_format: tc_fmt,
    })
}

/// Parsed VMAF analysis loaded from cache.
pub struct LoadedVmafAnalysis {
    pub frames: Vec<Value>,
    pub ref_path: String,
    pub dist_path: String,
    pub ref_start_frame: i64,
    pub ref_file_start_tc: i64,
    pub sync_mode: String,
    pub tc_format: Option<TimecodeFormat>,
}

/// Write a file so that readers only ever see the old or the complete new
/// contents: write `<path>.partial`, then rename over `path`. A crash or
/// quit mid-write leaves at most a `.partial` file, never a truncated
/// analysis that the picker would offer as "cached".
pub fn write_atomic(path: &str, content: &[u8]) -> Result<(), String> {
    let tmp = format!("{path}.partial");
    if let Err(e) = std::fs::write(&tmp, content) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("Failed to write {path}: {e}"));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Failed to write {path}: {e}")
    })
}

/// Save a diff analysis result to JSON.
pub fn save_diff_analysis(result: &Value, path: &str) -> Result<(), String> {
    let content = serde_json::to_string(result)
        .map_err(|e| format!("Failed to serialize analysis: {e}"))?;
    write_atomic(path, content.as_bytes())
}

/// Save a VMAF analysis result to JSON.
///
/// Mirrors vcompare.py's `save_analysis()`.
pub fn save_vmaf_analysis(
    frames: &[Value],
    ref_path: &str,
    dist_path: &str,
    ref_start_frame: i64,
    ref_file_start_tc: i64,
    sync_mode: &str,
    tc_format: Option<&TimecodeFormat>,
    path: &str,
) -> Result<(), String> {
    let data = serde_json::json!({
        "version": 2,
        "ref_path": ref_path,
        "dist_path": dist_path,
        "ref_start_frame": ref_start_frame,
        "ref_file_start_tc": ref_file_start_tc,
        "sync_mode": sync_mode,
        "tc_format": tc_format.map(|tc| serde_json::json!({
            "fps_num": tc.fps_num,
            "fps_den": tc.fps_den,
            "tc_type": tc.tc_type,
        })),
        "frames": frames.iter().map(|f| {
            serde_json::json!({
                "frame": f.get("frame"),
                "timecode": f.get("tc"),
                "psnr": f.get("psnr"),
                "vmaf": f.get("vmaf"),
            })
        }).collect::<Vec<_>>(),
    });

    let content = serde_json::to_string(&data)
        .map_err(|e| format!("Failed to serialize: {e}"))?;
    write_atomic(path, content.as_bytes())
}
