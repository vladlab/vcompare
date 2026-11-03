//! Analysis engine — diff and VMAF analysis via ffmpeg.
//!
//! Ports the core analysis pipeline from vcompare.py and vcompare_engine.py.
//! Spawns ffmpeg as a subprocess, streams stderr for progress, parses results.

use regex::Regex;
use serde_json::Value;
use std::collections::VecDeque;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::engine::pixfmt::{choose_common_pixfmt, parse_pix_fmt};
use crate::engine::probe::{ffprobe_json, get_video_info, is_concat_manifest};
use crate::engine::timecode::{detect_timecode_format, TimecodeFormat};
use crate::engine::types::{ComparisonConfig, TimelineInfo};

/// Callback for progress updates: (current_frame, total_frames).
pub type ProgressFn = Box<dyn Fn(u64, u64) + Send>;

/// The running ffmpeg process and cancel flag, shared with the app so it can
/// cancel or kill on quit. See `engine::process`.
pub type ProcessHandle = Arc<crate::engine::process::Job>;

/// Removes a scratch directory (and anything ffmpeg left in it) when
/// dropped, so killed or failed runs don't leave partial stats / VMAF JSON
/// logs behind in the temp folder.
struct TempDirGuard(std::path::PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ─── Prepare comparison ─────────────────────────────────────────────────────

/// Probe both files and compute trim/scale/format parameters.
///
/// Returns config, timecode format, and timeline info for the UI.
pub fn prepare_comparison(
    ref_path: &str,
    dist_path: &str,
    sync_mode: &str,
    vmaf_cuda: bool,
    offset_frames: i64,
    diagnostics: &mut Vec<String>,
) -> Result<(ComparisonConfig, TimecodeFormat, TimelineInfo), String> {
    let ref_probe = ffprobe_json(ref_path)?;
    let dist_probe = ffprobe_json(dist_path)?;

    let tc_fmt = detect_timecode_format(&ref_probe)?;
    let ref_info = get_video_info(&ref_probe, &tc_fmt)?;
    let dist_info = get_video_info(&dist_probe, &tc_fmt)?;

    let mut config = ComparisonConfig::default();
    config.ref_file_start_tc = ref_info.start_tc_frames;
    config.use_vmaf_cuda = vmaf_cuda;
    config.ref_src_pixfmt = ref_info.pix_fmt.clone();
    config.dist_src_pixfmt = dist_info.pix_fmt.clone();

    diagnostics.push(format!(
        "ref: {}x{} {} {}/{}",
        ref_info.width, ref_info.height, ref_info.pix_fmt, ref_info.fps_num, ref_info.fps_den
    ));
    diagnostics.push(format!(
        "dist: {}x{} {} {}/{}",
        dist_info.width, dist_info.height, dist_info.pix_fmt, dist_info.fps_num, dist_info.fps_den
    ));

    // Resolution scaling — always compare at the smaller resolution
    if ref_info.width != dist_info.width || ref_info.height != dist_info.height {
        let ref_pixels = ref_info.width as u64 * ref_info.height as u64;
        let dist_pixels = dist_info.width as u64 * dist_info.height as u64;

        if ref_pixels > dist_pixels {
            // Ref is larger → scale ref down to dist's size
            config.scale_w = dist_info.width;
            config.scale_h = dist_info.height;
            config.scale_ref = true;
            diagnostics.push(format!(
                "resolution: MISMATCH → scaling ref down to {}x{}",
                dist_info.width, dist_info.height
            ));
        } else {
            // Dist is larger → scale dist down to ref's size
            config.scale_w = ref_info.width;
            config.scale_h = ref_info.height;
            config.scale_ref = false;
            diagnostics.push(format!(
                "resolution: MISMATCH → scaling dist down to {}x{}",
                ref_info.width, ref_info.height
            ));
        }
    } else {
        diagnostics.push(format!(
            "resolution: matched ({}x{})",
            ref_info.width, ref_info.height
        ));
    }

    // Pixel format negotiation (for vmaf — diff overrides this)
    let neg = choose_common_pixfmt(&ref_info.pix_fmt, &dist_info.pix_fmt);
    config.ref_pixfmt = neg.ref_convert;
    config.dist_pixfmt = neg.dist_convert;

    if config.ref_pixfmt.is_some() || config.dist_pixfmt.is_some() {
        diagnostics.push(format!(
            "pixfmt: {} vs {} → target {}",
            ref_info.pix_fmt, dist_info.pix_fmt, neg.target
        ));
    } else {
        diagnostics.push(format!(
            "pixfmt: matched ({}, no conversion)",
            ref_info.pix_fmt
        ));
    }

    diagnostics.push(format!("sync: {sync_mode}"));

    // ── Timeline computation ──
    // Capture each clip's TC range in display space BEFORE trim/offset
    let ref_total = ref_info.nb_frames.unwrap_or(0) as i64;
    let dist_total = dist_info.nb_frames.unwrap_or(0) as i64;

    let ref_tc_start = ref_info.start_tc_frames;
    let ref_tc_end = if ref_total > 0 { ref_tc_start + ref_total } else { ref_tc_start };

    // In TC mode, dist keeps its own TC. In start mode, dist is shown
    // aligned to ref's start (same visual position).
    let (dist_display_start, dist_display_end) = if sync_mode == "tc" {
        let ds = dist_info.start_tc_frames;
        let de = if dist_total > 0 { ds + dist_total } else { ds };
        (ds, de)
    } else {
        // Start mode: both aligned at ref's start TC for display
        let ds = ref_tc_start;
        let de = if dist_total > 0 { ds + dist_total } else { ds };
        (ds, de)
    };

    // ── TC sync ──
    if sync_mode == "tc" {
        let ref_start = ref_info.start_tc_frames;
        let dist_start = dist_info.start_tc_frames;

        if ref_start != dist_start {
            let overlap_start = ref_start.max(dist_start);
            config.ref_trim_frames = overlap_start - ref_start;
            config.dist_trim_frames = overlap_start - dist_start;
            config.ref_start_frame = overlap_start;

            diagnostics.push(format!(
                "  tc offset: ref={}, dist={}",
                ref_info.start_tc.as_deref().unwrap_or("00:00:00:00"),
                dist_info.start_tc.as_deref().unwrap_or("00:00:00:00"),
            ));
            if config.ref_trim_frames > 0 {
                diagnostics.push(format!("  seek ref: {} frames", config.ref_trim_frames));
            }
            if config.dist_trim_frames > 0 {
                diagnostics.push(format!("  seek dist: {} frames", config.dist_trim_frames));
            }
        } else {
            config.ref_start_frame = ref_start;
            diagnostics.push(format!(
                "  tc aligned at {}",
                ref_info.start_tc.as_deref().unwrap_or("00:00:00:00")
            ));
        }
    } else {
        config.ref_start_frame = ref_info.start_tc_frames;
    }

    // Manual frame offset
    if offset_frames != 0 {
        if offset_frames > 0 {
            config.dist_trim_frames += offset_frames;
        } else {
            config.ref_trim_frames += -offset_frames;
            config.ref_start_frame += -offset_frames;
        }
        diagnostics.push(format!("  manual offset: {offset_frames:+} frames"));
    }

    // Convert trim frames to -ss seconds
    if config.ref_trim_frames > 0 {
        config.ref_seek_sec =
            config.ref_trim_frames as f64 * ref_info.fps_den as f64 / ref_info.fps_num as f64;
    }
    if config.dist_trim_frames > 0 {
        config.dist_seek_sec =
            config.dist_trim_frames as f64 * dist_info.fps_den as f64 / dist_info.fps_num as f64;
    }

    // Estimate total frames for progress (from the shorter overlapping region)
    if ref_total > 0 && dist_total > 0 {
        let ref_avail = ref_total - config.ref_trim_frames;
        let dist_avail = dist_total - config.dist_trim_frames;
        config.total_frames = Some(ref_avail.min(dist_avail).max(0) as u64);
    } else if ref_total > 0 {
        config.total_frames = Some((ref_total - config.ref_trim_frames).max(0) as u64);
    } else if dist_total > 0 {
        config.total_frames = Some((dist_total - config.dist_trim_frames).max(0) as u64);
    }

    // ── Build timeline info ──
    let overlap_start = config.ref_start_frame;
    let overlap_end = overlap_start + config.total_frames.unwrap_or(0) as i64;

    let span_start = ref_tc_start.min(dist_display_start);
    let span_end = ref_tc_end.max(dist_display_end);

    let ref_filename = std::path::Path::new(ref_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let dist_filename = std::path::Path::new(dist_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let timeline = TimelineInfo {
        ref_start: ref_tc_start,
        ref_end: ref_tc_end,
        ref_filename,
        dist_start: dist_display_start,
        dist_end: dist_display_end,
        dist_filename,
        overlap_start,
        overlap_end,
        span_start,
        span_end,
    };

    diagnostics.push(format!(
        "timeline: ref=[{}..{}] dist=[{}..{}] overlap=[{}..{}]",
        tc_fmt.frame_to_tc(ref_tc_start),
        tc_fmt.frame_to_tc(ref_tc_end),
        tc_fmt.frame_to_tc(dist_display_start),
        tc_fmt.frame_to_tc(dist_display_end),
        tc_fmt.frame_to_tc(overlap_start),
        tc_fmt.frame_to_tc(overlap_end),
    ));

    Ok((config, tc_fmt, timeline))
}

// ─── Diff analysis ──────────────────────────────────────────────────────────

/// Run pixel-difference analysis using FFmpeg's psnr filter.
/// When `with_ssim` is true, also runs the ssim filter in the same pass.
///
/// Returns a JSON object with mode, frames, metadata — ready for state storage.
pub fn run_diff_analysis(
    ref_path: &str,
    dist_path: &str,
    config: &ComparisonConfig,
    tc_fmt: &TimecodeFormat,
    target_fmt: &str,
    bit_depth: u32,
    with_ssim: bool,
    progress: Option<ProgressFn>,
    process_handle: ProcessHandle,
) -> Result<Vec<Value>, String> {
    // Stats output file
    let stats_dir = std::env::temp_dir().join(format!("vcompare_diff_{}", std::process::id()));
    std::fs::create_dir_all(&stats_dir)
        .map_err(|e| format!("Failed to create temp dir: {e}"))?;
    let _cleanup = TempDirGuard(stats_dir.clone());
    let psnr_stats_path = stats_dir.join("psnr_stats.txt");
    let psnr_stats_str = psnr_stats_path.to_string_lossy().to_string();
    let ssim_stats_path = stats_dir.join("ssim_stats.txt");
    let ssim_stats_str = ssim_stats_path.to_string_lossy().to_string();

    // Build filter chain
    let mut ref_chain = format!("format={target_fmt}");
    let mut dist_chain = format!("format={target_fmt}");
    if config.scale_w > 0 {
        let scale = format!(",scale={}:{}:flags=lanczos", config.scale_w, config.scale_h);
        if config.scale_ref {
            ref_chain.push_str(&scale);
        } else {
            dist_chain.push_str(&scale);
        }
    }

    let filter_complex = if with_ssim {
        // Chain psnr → ssim in single pass: split dist, feed both filters
        format!(
            "[0:v]{ref_chain}[ref];[1:v]{dist_chain}[dist];\
             [dist]split[dist1][dist2];\
             [ref][dist1]psnr=stats_file={psnr_stats_str}[psnr_out];\
             [psnr_out][dist2]ssim=stats_file={ssim_stats_str}"
        )
    } else {
        format!(
            "[0:v]{ref_chain}[ref];[1:v]{dist_chain}[dist];[ref][dist]psnr=stats_file={psnr_stats_str}"
        )
    };

    // Build command
    let mut args = vec!["ffmpeg".to_string(), "-y".to_string()];
    args.extend(input_args_for(ref_path, config.ref_seek_sec));
    args.extend(input_args_for(dist_path, config.dist_seek_sec));
    args.extend([
        "-filter_complex".into(),
        filter_complex.clone(),
        "-shortest".into(),
    ]);

    // Hard-limit frame count to the overlap region
    if let Some(total) = config.total_frames {
        args.extend(["-frames:v".into(), total.to_string()]);
    }

    args.extend([
        "-f".into(),
        "null".into(),
        "-".into(),
    ]);

    eprintln!("[vcompare] cmd: {}", args.join(" "));

    // Run ffmpeg
    run_ffmpeg_with_progress(&args[1..], config.total_frames, progress, process_handle)?;

    // Parse psnr stats
    let peak = (1u64 << bit_depth) - 1;
    let mut frames = parse_psnr_stats(&psnr_stats_str, tc_fmt, config.ref_start_frame, peak)?;

    // Merge SSIM data if requested
    if with_ssim {
        let ssim_data = parse_ssim_stats(&ssim_stats_str)?;
        // Merge by index (both should have same frame count)
        for (i, frame) in frames.iter_mut().enumerate() {
            if let Some(ssim_vals) = ssim_data.get(i) {
                if let Some(obj) = frame.as_object_mut() {
                    obj.insert("ssim".into(), serde_json::json!(ssim_vals.all));
                    obj.insert("ssim_y".into(), serde_json::json!(ssim_vals.y));
                    obj.insert("ssim_u".into(), serde_json::json!(ssim_vals.u));
                    obj.insert("ssim_v".into(), serde_json::json!(ssim_vals.v));
                }
            }
        }
        eprintln!("[vcompare] Merged SSIM data: {} frames", ssim_data.len());
    }

    // Cleanup temp
    let _ = std::fs::remove_file(&psnr_stats_path);
    if with_ssim {
        let _ = std::fs::remove_file(&ssim_stats_path);
    }
    let _ = std::fs::remove_dir(&stats_dir);

    Ok(frames)
}

// ─── VMAF analysis ──────────────────────────────────────────────────────────

/// Run VMAF analysis using FFmpeg's libvmaf filter.
///
/// Returns parsed metric frames as JSON values.
pub fn run_vmaf_analysis(
    ref_path: &str,
    dist_path: &str,
    config: &mut ComparisonConfig,
    tc_fmt: &TimecodeFormat,
    model: &str,
    hwaccel: Option<&str>,
    subsample: u32,
    want_psnr: bool,
    progress: Option<ProgressFn>,
    diagnostics: &mut Vec<String>,
    process_handle: ProcessHandle,
) -> Result<Vec<Value>, String> {
    // Temp log file
    let log_dir = std::env::temp_dir().join(format!("vcompare_{}", std::process::id()));
    std::fs::create_dir_all(&log_dir)
        .map_err(|e| format!("Failed to create temp dir: {e}"))?;
    let _cleanup = TempDirGuard(log_dir.clone());
    let log_path = log_dir.join("vmaf_log.json");
    let log_path_str = log_path.to_string_lossy().to_string();

    let extra_features: Vec<&str> = if want_psnr { vec!["psnr"] } else { vec![] };

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);

    // Feature string
    let feature_str = if !extra_features.is_empty() {
        format!(
            ":feature={}",
            extra_features
                .iter()
                .map(|f| format!("name={f}"))
                .collect::<Vec<_>>()
                .join("|")
        )
    } else {
        String::new()
    };

    // Decide GPU vs CPU path
    let use_cuda_vmaf =
        config.use_vmaf_cuda && hwaccel == Some("cuda") && extra_features.is_empty();

    let mut args = vec!["-y".to_string()];

    if use_cuda_vmaf {
        // ── GPU path: libvmaf_cuda ──
        let mut vmaf_opts =
            format!("log_fmt=json:log_path={log_path_str}:model=version={model}");
        if !feature_str.is_empty() {
            vmaf_opts.push_str(&feature_str);
        }
        if subsample > 1 {
            vmaf_opts.push_str(&format!(":n_subsample={subsample}"));
        }
        config.log_subsample = 1;

        let (ref_scale, dist_scale) = if config.scale_w > 0 {
            let scaled = format!(
                "w={}:h={}:format=yuv420p",
                config.scale_w, config.scale_h
            );
            let plain = "format=yuv420p".to_string();
            if config.scale_ref {
                (scaled, plain)
            } else {
                (plain, scaled)
            }
        } else {
            ("format=yuv420p".to_string(), "format=yuv420p".to_string())
        };

        let all_filters = format!(
            "[0:v]hwupload_cuda,scale_cuda={ref_scale}[ref];\
             [1:v]hwupload_cuda,scale_cuda={dist_scale}[dist];\
             [ref][dist]libvmaf_cuda={vmaf_opts}"
        );

        args.extend([
            "-init_hw_device".into(),
            "cuda=cu".into(),
            "-filter_hw_device".into(),
            "cu".into(),
        ]);
        args.extend(input_args_for(ref_path, config.ref_seek_sec));
        args.extend(input_args_for(dist_path, config.dist_seek_sec));
        args.extend(["-filter_complex".into(), all_filters, "-shortest".into()]);
        if let Some(total) = config.total_frames {
            args.extend(["-frames:v".into(), total.to_string()]);
        }
        args.extend(["-f".into(), "null".into(), "-".into()]);
    } else {
        // ── CPU path: libvmaf ──
        let mut vmaf_opts = format!(
            "log_fmt=json:log_path={log_path_str}:model=version={model}:n_threads={threads}"
        );
        if !feature_str.is_empty() {
            vmaf_opts.push_str(&feature_str);
        }
        if subsample > 1 {
            vmaf_opts.push_str(&format!(":n_subsample={subsample}"));
        }
        config.log_subsample = 1;

        let mut ref_chain: Vec<String> = Vec::new();
        let mut dist_chain: Vec<String> = Vec::new();

        if let Some(ref fmt) = config.ref_pixfmt {
            ref_chain.push(format!("format={fmt}"));
        }
        if let Some(ref fmt) = config.dist_pixfmt {
            dist_chain.push(format!("format={fmt}"));
        }
        if config.scale_w > 0 {
            let scale = format!(
                "scale={}:{}:flags=lanczos",
                config.scale_w, config.scale_h
            );
            if config.scale_ref {
                ref_chain.push(scale);
            } else {
                dist_chain.push(scale);
            }
        }

        let mut filters_before: Vec<String> = Vec::new();
        let mut ref_label = "0:v".to_string();
        let mut dist_label = "1:v".to_string();

        if !ref_chain.is_empty() {
            filters_before.push(format!("[0:v]{}[ref]", ref_chain.join(",")));
            ref_label = "ref".into();
        }
        if !dist_chain.is_empty() {
            filters_before.push(format!("[1:v]{}[dist]", dist_chain.join(",")));
            dist_label = "dist".into();
        }

        let vmaf_filter = format!("[{ref_label}][{dist_label}]libvmaf={vmaf_opts}");
        let all_filters = if filters_before.is_empty() {
            vmaf_filter
        } else {
            filters_before.push(vmaf_filter);
            filters_before.join(";")
        };

        // Input args with optional hwaccel
        for (path, seek_sec) in [(ref_path, config.ref_seek_sec), (dist_path, config.dist_seek_sec)] {
            if let Some(hw) = hwaccel {
                if !is_concat_manifest(path) {
                    args.extend(["-hwaccel".into(), hw.to_string()]);
                }
            }
            args.extend(input_args_for(path, seek_sec));
        }

        args.extend(["-filter_complex".into(), all_filters, "-shortest".into()]);
        if let Some(total) = config.total_frames {
            args.extend(["-frames:v".into(), total.to_string()]);
        }
        args.extend(["-f".into(), "null".into(), "-".into()]);
    }

    let fc_display = &args[args.iter().position(|a| a == "-filter_complex").map(|i| i + 1).unwrap_or(0)];
    let fc_short = if fc_display.len() > 80 {
        format!("{}…", &fc_display[..77])
    } else {
        fc_display.clone()
    };
    diagnostics.push(format!("filter: {fc_short}"));

    eprintln!("[vcompare] cmd: ffmpeg {}", args.join(" "));

    // Run ffmpeg
    run_ffmpeg_with_progress(&args, config.total_frames, progress, process_handle)?;

    // Parse VMAF JSON log
    let frames = parse_vmaf_log(
        &log_path_str,
        tc_fmt,
        config.ref_start_frame,
        config.log_subsample,
    )?;

    // Cleanup temp
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_dir(&log_dir);

    Ok(frames)
}

// ─── Levels analysis (single file) ─────────────────────────────────────────

/// Run a single-file levels analysis with the native stats engine.
///
/// ffmpeg decodes to a rawvideo pipe in the source's own depth/subsampling
/// and `rawstats` computes per-frame statistics on a worker pool (see
/// `engine/rawstats.rs`). Returns frame JSON objects in display frame space.
///
/// `requested_chunks`: `None` = auto, `Some(n)` = the user's choice of
/// parallel ffmpeg decodes. Sources that can't be split exactly (see
/// `rawstats::chunk_count`) run as one process regardless.
///
/// `hwaccel_available` is what `probe::detect_hwaccel()` found; it is only
/// used for codecs with hardware decoders. A listed hwaccel doesn't mean a
/// device exists (an NVIDIA-enabled ffmpeg on a machine without the driver
/// fails before the first frame), so a hw attempt that dies having read no
/// frames is retried in software.
///
/// `publish` is called with the diagnostics lines whenever they change so
/// the progress screen can show them. Returns the frames and a one-line
/// performance summary (fps and what limited it).
pub fn run_levels_analysis(
    file_path: &str,
    info: &SingleFileInfo,
    hwaccel_available: Option<&str>,
    requested_chunks: Option<u32>,
    progress: Option<ProgressFn>,
    process_handle: ProcessHandle,
    diagnostics: &mut Vec<String>,
    publish: &dyn Fn(&[String]),
) -> Result<(Vec<Value>, String), String> {
    use crate::engine::rawstats::{chunk_count, choose_hwaccel, ChunkSource, RawFormat};

    let fmt = RawFormat::for_source(&info.pix_fmt, info.width, info.height)
        .with_full_range(info.full_range);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let hw = choose_hwaccel(&info.codec_name, file_path, hwaccel_available);
    let (n_chunks, chunk_note) = chunk_count(
        requested_chunks,
        &ChunkSource {
            codec_name: &info.codec_name,
            total_frames: info.total_frames.or(info.estimated_frames),
            cfr: info.cfr,
            concat: is_concat_manifest(file_path),
            hwaccel: hw.is_some(),
        },
        cores,
    );

    diagnostics.push(format!("stats: native ({cores} threads)"));
    diagnostics.push(format!(
        "decode: {}{} — {chunk_note}",
        if info.codec_name.is_empty() { "?" } else { &info.codec_name },
        hw.as_deref().map(|h| format!(" [{h}]")).unwrap_or_default()
    ));
    diagnostics.push(format!("raw: {} {}x{}", fmt.pix_fmt, fmt.width, fmt.height));
    diagnostics.push(match fmt.legal {
        Some(l) => format!(
            "legal range: Y {}–{}, C {}–{} (limited)",
            l.y_lo, l.y_hi, l.c_lo, l.c_hi
        ),
        None => "legal range: n/a (full-range or RGB source)".into(),
    });
    publish(diagnostics);

    // The progress callback may be needed more than once (hw attempt +
    // software retry, chunks, chunk fallback); share it behind a mutex.
    let shared_progress: Option<Arc<Mutex<ProgressFn>>> = progress.map(|p| Arc::new(Mutex::new(p)));

    let mut ran_chunks = 1;
    let (stats, report) = if n_chunks > 1 {
        match run_chunked(file_path, &fmt, info, n_chunks, cores, &shared_progress, &process_handle) {
            Ok(r) => {
                ran_chunks = n_chunks;
                r
            }
            Err(ChunkedError::Fatal(e)) => return Err(e),
            Err(ChunkedError::Inexact(why)) => {
                eprintln!("[vcompare] chunked decode inexact, rerunning as one process: {why}");
                diagnostics.push(format!("decode: chunks didn't line up ({why}) — rerunning as one process"));
                publish(diagnostics);
                run_single(file_path, &fmt, info, None, cores, &shared_progress, &process_handle,
                           diagnostics, publish)?
            }
        }
    } else {
        run_single(file_path, &fmt, info, hw.as_deref(), cores, &shared_progress, &process_handle,
                   diagnostics, publish)?
    };

    let tc = &info.tc_fmt;
    let frames = stats
        .iter()
        .enumerate()
        .map(|(i, s)| s.to_json(info.start_frame + i as i64, tc))
        .collect();
    let summary = if ran_chunks > 1 {
        format!("{ran_chunks} parallel decodes: {}", report.summary())
    } else {
        report.summary()
    };
    Ok((frames, summary))
}

/// Do `warmup` (a chunk's first frames) repeat the end of `stitched`?
fn seam_matches(
    stitched: &[crate::engine::rawstats::FrameStats],
    warmup: &[crate::engine::rawstats::FrameStats],
) -> bool {
    warmup.len() <= stitched.len()
        && stitched[stitched.len() - warmup.len()..]
            .iter()
            .zip(warmup)
            .all(|(a, b)| a.same_picture(b))
}

fn progress_wrapper(shared: &Option<Arc<Mutex<ProgressFn>>>) -> Option<ProgressFn> {
    shared.as_ref().map(|p| {
        let p = p.clone();
        Box::new(move |c: u64, t: u64| (p.lock().unwrap())(c, t)) as ProgressFn
    })
}

/// One ffmpeg for the whole file, with the software retry for a hw decode
/// that fails before its first frame.
#[allow(clippy::too_many_arguments)]
fn run_single(
    file_path: &str,
    fmt: &crate::engine::rawstats::RawFormat,
    info: &SingleFileInfo,
    hw: Option<&str>,
    cores: usize,
    shared_progress: &Option<Arc<Mutex<ProgressFn>>>,
    process_handle: &ProcessHandle,
    diagnostics: &mut Vec<String>,
    publish: &dyn Fn(&[String]),
) -> Result<(Vec<crate::engine::rawstats::FrameStats>, crate::engine::rawstats::RunReport), String> {
    use crate::engine::rawstats::{run_raw_stats, RawRunOptions};
    let opts = RawRunOptions {
        hwaccel: hw.map(String::from),
        fps: info.fps,
        frame_limit: info.total_frames,
        progress_total: info.estimated_frames,
        workers: Some(cores),
        ..Default::default()
    };
    match run_raw_stats(file_path, fmt, &opts, progress_wrapper(shared_progress), process_handle.clone()) {
        Ok(r) => Ok(r),
        Err(e) if hw.is_some() && !e.killed && e.frames_read == 0 => {
            eprintln!("[vcompare] hw decode failed, retrying in software: {}", e.message);
            diagnostics.push(format!("decode: {} unavailable — software decode", hw.unwrap_or("")));
            publish(diagnostics);
            let sw = RawRunOptions { hwaccel: None, ..opts };
            run_raw_stats(file_path, fmt, &sw, progress_wrapper(shared_progress), process_handle.clone())
                .map_err(String::from)
        }
        Err(e) => Err(e.into()),
    }
}

enum ChunkedError {
    /// Report to the user (includes cancellation).
    Fatal(String),
    /// Seeking wasn't exact for this file; rerun as one process.
    Inexact(String),
}

/// Decode `n` chunks in parallel ffmpeg processes and stitch the results.
fn run_chunked(
    file_path: &str,
    fmt: &crate::engine::rawstats::RawFormat,
    info: &SingleFileInfo,
    n: u32,
    cores: usize,
    shared_progress: &Option<Arc<Mutex<ProgressFn>>>,
    process_handle: &ProcessHandle,
) -> Result<(Vec<crate::engine::rawstats::FrameStats>, crate::engine::rawstats::RunReport), ChunkedError> {
    use crate::engine::rawstats::{plan_chunks, run_raw_stats, RawRunOptions, RunReport};
    use std::sync::atomic::{AtomicU64, Ordering};

    // With only an estimated length, the last chunk runs to the end of the
    // file instead of stopping at the estimate.
    let exact = info.total_frames.is_some();
    let total = info.total_frames.or(info.estimated_frames).unwrap_or(0);
    let chunks = plan_chunks(total, n);
    let last = chunks.len() - 1;
    let per = (cores / chunks.len()).max(1);
    // Overall frames done, summed over chunks (warm-up frames included —
    // a handful per chunk).
    let done = Arc::new(AtomicU64::new(0));
    let t0 = Instant::now();

    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .iter()
            .enumerate()
            .map(|(i, chunk)| {
                let opts = RawRunOptions {
                    hwaccel: None,
                    start_frame: chunk.decode_start,
                    fps: info.fps,
                    frame_limit: if i == last && !exact { None } else { Some(chunk.decode_count()) },
                    progress_total: Some(chunk.decode_count()),
                    workers: Some(per),
                    decoder_threads: Some(per.max(2)),
                    buffer_bytes: Some((1usize << 30) / chunks.len()),
                };
                // Each chunk reports its own count; turn that into increments
                // of the shared total and report against the whole file.
                let progress: Option<ProgressFn> = shared_progress.as_ref().map(|p| {
                    let (p, done) = (p.clone(), done.clone());
                    let last = AtomicU64::new(0);
                    Box::new(move |c: u64, _t: u64| {
                        let prev = last.swap(c, Ordering::Relaxed);
                        if c > prev {
                            let sum = done.fetch_add(c - prev, Ordering::Relaxed) + (c - prev);
                            (p.lock().unwrap())(sum.min(total), total.max(sum));
                        }
                    }) as ProgressFn
                });
                let job = process_handle.clone();
                scope.spawn(move || {
                    let r = run_raw_stats(file_path, fmt, &opts, progress, job.clone());
                    if r.is_err() {
                        // No point finishing the other chunks.
                        job.kill_all_attached();
                    }
                    r
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join()).collect()
    });

    if process_handle.is_cancelled() {
        return Err(ChunkedError::Fatal("Analysis cancelled".into()));
    }

    let mut stitched = Vec::with_capacity(total as usize);
    let mut reports: Vec<RunReport> = Vec::new();
    for (i, (chunk, r)) in chunks.iter().zip(results).enumerate() {
        let (stats, report) = match r {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(ChunkedError::Fatal(format!("chunk {} of {}: {}", i + 1, chunks.len(), e.message))),
            Err(_) => return Err(ChunkedError::Fatal(format!("chunk {} of {} panicked", i + 1, chunks.len()))),
        };
        let got = stats.len() as u64;
        let want = chunk.decode_count();
        // Middle chunks must line up exactly or there'd be a gap/overlap at a
        // seam. The last one may come up short (like a single-process run
        // whose container overstated its length) or, when the length was
        // only estimated, long.
        let last_ok = got > chunk.warmup() && (!exact || got <= want);
        if (i < last && got != want) || (i == last && !last_ok) {
            return Err(ChunkedError::Inexact(format!(
                "chunk {} returned {got} frames, expected {want}",
                i + 1
            )));
        }
        // The warm-up frames are the previous chunk's last kept frames, decoded
        // again. If the seek landed anywhere else they won't match, and the
        // stitched result would be shifted at this seam.
        let w = chunk.warmup() as usize;
        if i > 0 && !seam_matches(&stitched, &stats[..w]) {
            return Err(ChunkedError::Inexact(format!(
                "chunk {} didn't start on the expected frame",
                i + 1
            )));
        }
        stitched.extend(stats.into_iter().skip(w));
        reports.push(report);
    }

    if let Some(p) = shared_progress {
        let n = stitched.len() as u64;
        (p.lock().unwrap())(n, n);
    }
    let mut report = RunReport::combine(&reports, t0.elapsed().as_secs_f64());
    report.frames = stitched.len() as u64; // not counting warm-up frames
    eprintln!("[vcompare] chunked ({}): {}", chunks.len(), report.summary());
    Ok((stitched, report))
}

/// What a single-file (levels) analysis needs to know about its input.
#[derive(Debug, Clone)]
pub struct SingleFileInfo {
    pub tc_fmt: TimecodeFormat,
    /// First frame in TC space (display frame of the file's first frame).
    pub start_frame: i64,
    /// Frame count from the container, if it reports one.
    pub total_frames: Option<u64>,
    /// `total_frames`, or failing that duration × frame rate. For progress
    /// display only — never used to stop decoding.
    pub estimated_frames: Option<u64>,
    /// Bit depth of the source pixel format.
    pub bit_depth: u32,
    pub pix_fmt: String,
    pub width: u32,
    pub height: u32,
    /// ffprobe `codec_name` of the video stream, e.g. "prores", "hevc".
    pub codec_name: String,
    /// ffprobe `color_range` is "pc" (full range). Unknown counts as limited,
    /// which is what an untagged YUV file is assumed to be.
    pub full_range: bool,
    /// Stream frame rate (r_frame_rate).
    pub fps: (u32, u32),
    /// avg_frame_rate agrees with r_frame_rate (within 0.1%): constant rate,
    /// so frame index ↔ time is exact.
    pub cfr: bool,
}

/// Prepare a single file for levels analysis.
///
/// Probes the file and returns everything the levels pass needs: timecode
/// format, start frame, frame count, and the source pixel format / geometry
/// / codec (which decide the raw pipe format and whether to hw-decode).
pub fn prepare_single_file(
    file_path: &str,
    diagnostics: &mut Vec<String>,
) -> Result<SingleFileInfo, String> {
    let probe = ffprobe_json(file_path)?;
    let tc_fmt = detect_timecode_format(&probe)?;
    let info = get_video_info(&probe, &tc_fmt)?;
    let stream_str = |key: &str| {
        crate::engine::timecode::find_video_stream(&probe)
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let codec_name = stream_str("codec_name");
    let full_range = stream_str("color_range") == "pc";
    let cfr = match crate::engine::timecode::parse_rational(&stream_str("avg_frame_rate")) {
        Some((an, ad)) if an > 0 && ad > 0 && info.fps_den > 0 => {
            let avg = an as f64 / ad as f64;
            let nominal = info.fps_num as f64 / info.fps_den as f64;
            ((avg - nominal) / nominal).abs() < 1e-3
        }
        _ => false,
    };

    diagnostics.push(format!(
        "file: {}x{} {} {} {}/{}",
        info.width, info.height, codec_name, info.pix_fmt, info.fps_num, info.fps_den
    ));
    if let Some(ref tc) = info.start_tc {
        diagnostics.push(format!("timecode: {tc}"));
    }

    let total = info.nb_frames.map(|n| n as u64);
    // Many containers (MKV, MXF, TS) don't report a frame count; estimate
    // one from the duration so the progress bar still moves.
    let estimated = total.or_else(|| {
        let dur = probe
            .get("format")
            .and_then(|f| f.get("duration"))
            .and_then(|d| d.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| d.as_f64()))?;
        let est = (dur * info.fps_num as f64 / info.fps_den.max(1) as f64).round();
        (est >= 1.0).then_some(est as u64)
    });
    match (total, estimated) {
        (Some(n), _) => diagnostics.push(format!("frames: {n}")),
        (None, Some(n)) => diagnostics.push(format!("frames: ~{n} (from duration)")),
        _ => {}
    }

    let bit_depth = parse_pix_fmt(&info.pix_fmt).bit_depth;
    diagnostics.push(format!("bit depth: {bit_depth}"));

    Ok(SingleFileInfo {
        tc_fmt,
        start_frame: info.start_tc_frames,
        total_frames: total,
        estimated_frames: estimated,
        bit_depth,
        pix_fmt: info.pix_fmt,
        width: info.width,
        height: info.height,
        codec_name,
        full_range,
        fps: (info.fps_num, info.fps_den),
        cfr,
    })
}

// ─── FFmpeg process management ──────────────────────────────────────────────

/// Build -ss/-f/-i args for one input file.
fn input_args_for(path: &str, seek_sec: f64) -> Vec<String> {
    let mut args = Vec::new();
    if seek_sec > 0.0 {
        args.push("-ss".into());
        args.push(format!("{seek_sec:.6}"));
    }
    if is_concat_manifest(path) {
        args.push("-f".into());
        args.push("concat".into());
        args.push("-safe".into());
        args.push("0".into());
    }
    args.push("-i".into());
    args.push(path.into());
    args
}

/// Spawn ffmpeg, stream stderr for progress, wait for completion.
///
/// The process handle allows external cancellation (e.g. app quit).
/// FFmpeg writes progress updates with `\r` (no `\n`), so we read bytes
/// and split on both `\r` and `\n`.
fn run_ffmpeg_with_progress(
    args: &[String],
    total_frames: Option<u64>,
    progress: Option<ProgressFn>,
    process_handle: ProcessHandle,
) -> Result<(), String> {
    let mut child = Command::new("ffmpeg")
        .args(args)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg: {e}"))?;

    // Take stderr before storing child — the Child retains pid for kill/wait
    let stderr = child.stderr.take().ok_or("Failed to get ffmpeg stderr")?;

    // Store child in handle for external cancellation; a job cancelled
    // before we got here gets its ffmpeg killed straight away.
    let Some(child_id) = process_handle.attach(child) else {
        return Err("Analysis cancelled".into());
    };

    let frame_re = Regex::new(r"frame=\s*(\d+)").unwrap();
    let mut last_reported: i64 = -1;
    let mut last_progress_time = Instant::now();
    let mut stderr_tail: VecDeque<String> = VecDeque::with_capacity(20);

    // Read stderr byte-by-byte, splitting on both \r and \n.
    // FFmpeg uses \r for progress counter updates (same-line overwrite).
    let mut reader = BufReader::new(stderr);
    let mut line_buf = Vec::with_capacity(256);

    loop {
        let mut byte = [0u8; 1];
        match std::io::Read::read(&mut reader, &mut byte) {
            Ok(0) => break, // EOF
            Ok(_) => {
                if byte[0] == b'\r' || byte[0] == b'\n' {
                    if !line_buf.is_empty() {
                        let line = String::from_utf8_lossy(&line_buf).to_string();
                        line_buf.clear();

                        if stderr_tail.len() >= 20 {
                            stderr_tail.pop_front();
                        }
                        stderr_tail.push_back(line.clone());

                        if let Some(caps) = frame_re.captures(&line) {
                            if let Some(total) = total_frames {
                                if let Ok(current) = caps[1].parse::<u64>() {
                                    if current as i64 != last_reported {
                                        last_reported = current as i64;
                                        let now = Instant::now();
                                        if now.duration_since(last_progress_time).as_millis() >= 250
                                        {
                                            last_progress_time = now;
                                            if let Some(ref cb) = progress {
                                                cb(current, total);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    line_buf.push(byte[0]);
                }
            }
            Err(e) => {
                eprintln!("[vcompare] stderr read error: {e}");
                break;
            }
        }
    }

    // Wait for process to finish
    let mut child_opt = process_handle.detach(child_id);
    let status = match child_opt.as_mut() {
        Some(c) => c.wait().map_err(|e| format!("Failed to wait for ffmpeg: {e}"))?,
        None => return Err("ffmpeg process was killed externally".into()),
    };

    // Final progress
    if last_reported > 0 {
        if let (Some(total), Some(ref cb)) = (total_frames, &progress) {
            cb(total, total);
        }
    }

    if !status.success() {
        let tail: String = stderr_tail.into_iter().collect::<Vec<_>>().join("\n");
        return Err(format!(
            "FFmpeg exited with code {}\n\n{tail}",
            status.code().unwrap_or(-1)
        ));
    }

    Ok(())
}

// ─── Result parsing ─────────────────────────────────────────────────────────

/// Parse psnr filter stats_file output into frame JSON values.
///
/// Format per line: `n:1 mse_avg:0.00 mse_y:0.00 mse_u:0.00 mse_v:0.00 ...`
fn parse_psnr_stats(
    stats_path: &str,
    tc_fmt: &TimecodeFormat,
    ref_start_frame: i64,
    _peak: u64,
) -> Result<Vec<Value>, String> {
    let content = std::fs::read_to_string(stats_path)
        .map_err(|e| format!("Failed to read psnr stats: {e}"))?;

    let kv_re = Regex::new(r"(\w+):([\d.inf]+)").unwrap();
    let mut frames = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let pairs: std::collections::HashMap<&str, &str> = kv_re
            .captures_iter(line)
            .map(|c| (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str()))
            .collect();

        let n = match pairs.get("n").and_then(|s| s.parse::<i64>().ok()) {
            Some(n) => n,
            None => continue,
        };

        let fn_ = n - 1; // psnr filter is 1-indexed
        let display_frame = fn_ + ref_start_frame;

        let mse_y = parse_float_or_zero(pairs.get("mse_y").copied());
        let mse_u = parse_float_or_zero(pairs.get("mse_u").copied());
        let mse_v = parse_float_or_zero(pairs.get("mse_v").copied());

        let rms_y = if mse_y > 0.0 { mse_y.sqrt() } else { 0.0 };
        let rms_u = if mse_u > 0.0 { mse_u.sqrt() } else { 0.0 };
        let rms_v = if mse_v > 0.0 { mse_v.sqrt() } else { 0.0 };

        frames.push(serde_json::json!({
            "frame": display_frame,
            "tc": tc_fmt.frame_to_tc(display_frame),
            "diff_y": round4(rms_y),
            "diff_u": round4(rms_u),
            "diff_v": round4(rms_v),
        }));
    }

    Ok(frames)
}

/// Parse a VMAF JSON log into frame values.
fn parse_vmaf_log(
    log_path: &str,
    tc_fmt: &TimecodeFormat,
    ref_start_frame: i64,
    subsample: u32,
) -> Result<Vec<Value>, String> {
    let content = std::fs::read_to_string(log_path)
        .map_err(|e| format!("Failed to read VMAF log: {e}"))?;

    let data: Value =
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse VMAF JSON: {e}"))?;

    let all_frames = data
        .get("frames")
        .and_then(|v| v.as_array())
        .ok_or("No 'frames' array in VMAF log")?;

    let mut frames = Vec::new();

    for entry in all_frames {
        let fn_ = entry
            .get("frameNum")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        let metrics = entry.get("metrics").and_then(|v| v.as_object());

        // PSNR key varies by libvmaf version
        let psnr = metrics
            .and_then(|m| {
                m.get("psnr_y")
                    .and_then(|v| v.as_f64())
                    .or_else(|| m.get("psnr_y (mean)").and_then(|v| v.as_f64()))
                    .or_else(|| {
                        m.iter()
                            .find(|(k, v)| k.to_lowercase().contains("psnr") && v.is_f64())
                            .and_then(|(_, v)| v.as_f64())
                    })
            });

        let vmaf = metrics.and_then(|m| m.get("vmaf").and_then(|v| v.as_f64()));

        let real_frame = fn_ * subsample as i64;
        let display_frame = real_frame + ref_start_frame;

        frames.push(serde_json::json!({
            "frame": display_frame,
            "tc": tc_fmt.frame_to_tc(display_frame),
            "vmaf": vmaf.map(|v| round3(v)),
            "psnr": psnr.map(|v| round3(v)),
        }));
    }

    Ok(frames)
}

// ─── SSIM stats parsing ────────────────────────────────────────────────────

/// Parsed SSIM values for a single frame.
struct SsimFrame {
    y: f64,
    u: f64,
    v: f64,
    all: f64,
}

/// Parse ssim filter stats_file output into per-frame SSIM values.
///
/// Format per line: `n:1 Y:0.999900 U:0.999800 V:0.999700 All:0.999800 (39.99)`
fn parse_ssim_stats(stats_path: &str) -> Result<Vec<SsimFrame>, String> {
    let content = std::fs::read_to_string(stats_path)
        .map_err(|e| format!("Failed to read ssim stats: {e}"))?;

    let kv_re = Regex::new(r"(\w+):([\d.inf]+)").unwrap();
    let mut frames = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let pairs: std::collections::HashMap<&str, &str> = kv_re
            .captures_iter(line)
            .map(|c| (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str()))
            .collect();

        // Skip if no frame number
        if pairs.get("n").and_then(|s| s.parse::<i64>().ok()).is_none() {
            continue;
        }

        let y = parse_float_or_zero(pairs.get("Y").copied());
        let u = parse_float_or_zero(pairs.get("U").copied());
        let v = parse_float_or_zero(pairs.get("V").copied());
        let all = parse_float_or_zero(pairs.get("All").copied());

        frames.push(SsimFrame {
            y: round6(y),
            u: round6(u),
            v: round6(v),
            all: round6(all),
        });
    }

    Ok(frames)
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn parse_float_or_zero(s: Option<&str>) -> f64 {
    s.and_then(|s| {
        if s == "inf" || s == "-inf" {
            None
        } else {
            s.parse::<f64>().ok()
        }
    })
    .unwrap_or(0.0)
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

fn round6(v: f64) -> f64 {
    (v * 1000000.0).round() / 1000000.0
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Chunked decoding must give exactly the single-process result: every
    /// field of every frame, including the frame / thumbnail differences
    /// that straddle chunk seams. Covers 23.976 MOV, a MOV with a non-zero
    /// start time, MXF and MKV without container frame counts. Needs ffmpeg
    /// and ffprobe; skipped otherwise.
    #[test]
    fn chunked_levels_match_single_process() {
        let dir = std::env::temp_dir().join(format!("vcompare_chunk_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = "testsrc2=s=320x180:r=24000/1001,noise=alls=6:allf=t+u";
        let clips: Vec<(&str, Vec<&str>)> = vec![
            ("p.mov", vec!["-f", "lavfi", "-i", src, "-frames:v", "1500", "-c:v", "prores_ks", "-profile:v", "3", "-pix_fmt", "yuv422p10le"]),
            ("p_off.mov", vec!["-f", "lavfi", "-i", src, "-frames:v", "1500", "-c:v", "prores_ks", "-profile:v", "3", "-pix_fmt", "yuv422p10le", "-output_ts_offset", "7.3"]),
            ("d.mxf", vec!["-f", "lavfi", "-i", "testsrc2=s=640x360:r=25,noise=alls=6:allf=t+u", "-frames:v", "1000", "-c:v", "dnxhd", "-profile:v", "dnxhr_hq", "-pix_fmt", "yuv422p"]),
            ("f.mkv", vec!["-f", "lavfi", "-i", src, "-frames:v", "1500", "-c:v", "ffv1", "-pix_fmt", "yuv420p"]),
        ];
        for (name, args) in clips {
            let path = dir.join(name);
            let ok = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-y"])
                .args(&args)
                .arg(&path)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                eprintln!("ffmpeg unavailable or can't make {name}; skipping");
                continue;
            }
            let p = path.to_string_lossy().to_string();
            let mut d = Vec::new();
            let info = prepare_single_file(&p, &mut d).unwrap();
            let run = |n: u32| {
                let mut d = Vec::new();
                run_levels_analysis(&p, &info, None, Some(n), None,
                                    crate::engine::process::standalone_job(), &mut d, &|_| {})
                    .unwrap()
            };
            let (single, s1) = run(1);
            let (chunked, s4) = run(4);
            assert!(!s1.contains("parallel"), "{name}: {s1}");
            assert!(s4.starts_with("4 parallel decodes"), "{name} didn't chunk: {s4}");
            assert_eq!(single.len(), chunked.len(), "{name}: frame count");
            for (i, (a, b)) in single.iter().zip(&chunked).enumerate() {
                assert_eq!(a, b, "{name}: frame {i} differs");
            }
            eprintln!("{name}: {} frames identical (single vs 4 chunks)", single.len());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The seam check must reject a chunk that starts one frame off.
    #[test]
    fn seam_check_catches_off_by_one() {
        use crate::engine::rawstats::{bench_compute, RawFormat};
        let f = RawFormat::for_source("yuv420p", 64, 36);
        // Distinct frames: frame k is a flat field at level 20 + 10k.
        let frames: Vec<_> = (0..12u8)
            .map(|k| bench_compute(&f, &vec![20 + 10 * k; f.frame_bytes()], None).0)
            .collect();
        let stitched = &frames[..8]; // previous chunk kept 0..8
        assert!(seam_matches(stitched, &frames[4..8]), "correct seek: warm-up = frames 4..8");
        assert!(!seam_matches(stitched, &frames[5..9]), "one frame late");
        assert!(!seam_matches(stitched, &frames[3..7]), "one frame early");
    }
}
