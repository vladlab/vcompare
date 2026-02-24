//! Core data types for the analysis engine.
//!
//! These mirror the Python dataclasses in vcompare.py but with proper typing.
//! Types that cross the Tauri IPC boundary derive Serialize; types used for
//! internal engine state may only derive Deserialize where needed.

use serde::{Deserialize, Serialize};

use crate::engine::timecode::TimecodeFormat;

/// Key properties extracted from a video file via ffprobe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub nb_frames: Option<u64>,
    pub pix_fmt: String,
    /// Embedded timecode string, e.g. "01:00:00:00"
    pub start_tc: Option<String>,
    /// Start TC converted to an absolute frame number
    pub start_tc_frames: i64,
}

/// Result of probing a file — sent to the frontend.
///
/// Superset of VideoInfo with additional fields the UI needs.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    pub path: String,
    pub filename: String,
    pub width: u32,
    pub height: u32,
    pub pix_fmt: String,
    pub codec: String,
    pub fps: f64,
    pub fps_num: u32,
    pub fps_den: u32,
    pub nb_frames: Option<u64>,
    pub duration: f64,
    pub timecode: Option<String>,
}

/// Computed parameters for the ffmpeg comparison pass.
///
/// Built by `prepare_comparison()` from probing both files.
#[derive(Debug, Clone)]
pub struct ComparisonConfig {
    pub ref_trim_frames: i64,
    pub dist_trim_frames: i64,
    /// `-ss` value for reference (computed from trim_frames + fps)
    pub ref_seek_sec: f64,
    /// `-ss` value for distorted
    pub dist_seek_sec: f64,
    /// 0 = no scaling needed
    pub scale_w: u32,
    pub scale_h: u32,
    /// Format filter for ref (None = no conversion)
    pub ref_pixfmt: Option<String>,
    /// Format filter for dist (None = no conversion)
    pub dist_pixfmt: Option<String>,
    /// Actual source pixel format from probe
    pub ref_src_pixfmt: String,
    pub dist_src_pixfmt: String,
    /// Use libvmaf_cuda (full GPU pipeline)
    pub use_vmaf_cuda: bool,
    /// Multiply VMAF log frameNum by this to get real frames
    pub log_subsample: u32,
    /// Absolute frame for TC display
    pub ref_start_frame: i64,
    /// Ref file's own start TC (for mpv seeking)
    pub ref_file_start_tc: i64,
    /// For progress reporting
    pub total_frames: Option<u64>,
}

impl Default for ComparisonConfig {
    fn default() -> Self {
        Self {
            ref_trim_frames: 0,
            dist_trim_frames: 0,
            ref_seek_sec: 0.0,
            dist_seek_sec: 0.0,
            scale_w: 0,
            scale_h: 0,
            ref_pixfmt: None,
            dist_pixfmt: None,
            ref_src_pixfmt: "yuv420p".into(),
            dist_src_pixfmt: "yuv420p".into(),
            use_vmaf_cuda: false,
            log_subsample: 1,
            ref_start_frame: 0,
            ref_file_start_tc: 0,
            total_frames: None,
        }
    }
}

/// A single frame's metric values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct MetricFrame {
    pub frame: i64,
    pub timecode: String,
    pub psnr: Option<f64>,
    pub vmaf: Option<f64>,
}

/// Diff-mode frame data (pixel difference analysis).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct DiffFrame {
    pub frame: i64,
    pub timecode: String,
    pub diff_y: f64,
    pub diff_u: f64,
    pub diff_v: f64,
}

/// Aggregate statistics for a single metric.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct MetricStats {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub stddev: Option<f64>,
    pub median: Option<f64>,
    pub percentile_5: Option<f64>,
    pub percentile_95: Option<f64>,
}

/// Summary of a completed analysis — sent to the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct AnalysisSummary {
    pub mode: String,
    pub frame_count: u64,
    pub metrics_available: Vec<String>,
    pub tc_format: Option<TimecodeFormat>,
    pub ref_start_frame: i64,
    pub sync_mode: String,
    // Per-metric stats (present when the metric is available)
    pub diff_y_stats: Option<MetricStats>,
    pub diff_u_stats: Option<MetricStats>,
    pub diff_v_stats: Option<MetricStats>,
    pub vmaf_stats: Option<MetricStats>,
    pub psnr_stats: Option<MetricStats>,
    pub diff_distribution: Option<DiffDistribution>,
}

/// Distribution summary for diff mode — how many frames at each severity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct DiffDistribution {
    pub zero: u64,
    pub under_1: u64,
    pub under_5: u64,
    pub under_10: u64,
    pub above_10: u64,
}

/// Capabilities of the current system — ffmpeg features, hardware, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub hwaccel: Option<String>,
    pub vmaf_cuda: bool,
    pub has_mpv: bool,
}

/// Parameters for checking if a cached analysis exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct CacheCheck {
    pub exists: bool,
    pub path: String,
    pub frame_count: Option<u64>,
    pub mode: Option<String>,
}

/// Timeline layout info — shows how two clips relate in TC space.
///
/// All frame numbers are in display TC space (absolute TC frames).
/// The frontend uses this to render the dual-bar timeline visualization
/// and to know where "no data" regions are in the chart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineInfo {
    /// Reference clip range
    pub ref_start: i64,
    pub ref_end: i64,
    pub ref_filename: String,
    /// Distorted clip range (in display TC space — aligned per sync mode)
    pub dist_start: i64,
    pub dist_end: i64,
    pub dist_filename: String,
    /// Region where both clips overlap (analysis data exists here)
    pub overlap_start: i64,
    pub overlap_end: i64,
    /// Full span (union of both clips)
    pub span_start: i64,
    pub span_end: i64,
}
