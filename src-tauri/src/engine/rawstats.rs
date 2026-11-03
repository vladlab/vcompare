//! Native per-frame statistics over a raw video pipe.
//!
//! ffmpeg's `signalstats` filter is single-threaded and costs more per UHD
//! frame than decoding does, so it caps a levels pass at ~10 fps however
//! many cores the machine has. This module replaces it: ffmpeg only decodes
//! (which it threads well) and writes planar YUV to stdout; we read frames
//! into a bounded buffer pool and compute the statistics on a worker pool,
//! one frame per worker, so throughput becomes decode-bound.
//!
//! What is computed per frame (all in native code values of the pixel
//! format, matching what `signalstats` reported):
//!
//! - `y/u/v_min`, `y/u/v_max`, `y/u/v_avg`
//! - `y/u/v_dif`: mean absolute difference against the previous frame
//! - `tdif_1..=4`: mean absolute difference of a 32×18 luma thumbnail
//!   against the thumbnail 1..4 frames back, normalized 0..1. These are
//!   cheap, noise-robust "how different is the picture" measures that the
//!   flash detector uses to compare the frames on either side of a run.
//!
//! Pipeline:
//!
//! ```text
//!   ffmpeg ─stdout─▶ reader (this thread) ─jobs─▶ N workers ─results─▶ collector
//!                        ▲                                                │
//!                        └──────── buffer pool (bounded) ◀────────────────┘
//! ```
//!
//! The collector reorders results by frame index (workers finish out of
//! order), computes the thumbnail differences against the last four frames,
//! and emits progress.

use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::engine::analysis::ProgressFn;
use crate::engine::pixfmt::parse_pix_fmt;
use crate::engine::probe::is_concat_manifest;
use crate::engine::timecode::TimecodeFormat;

/// Thumbnail grid used for the temporal signature.
pub const THUMB_W: usize = 32;
pub const THUMB_H: usize = 18;
const THUMB_CELLS: usize = THUMB_W * THUMB_H;
/// How many frames back the thumbnail difference is computed.
pub const TDIF_DEPTH: usize = 4;

/// Memory budget for frames in flight (decoded, not yet processed).
const IN_FLIGHT_BYTES: usize = 1 << 30; // 1 GiB

// ─── Raw format description ─────────────────────────────────────────────────

/// The planar YUV layout ffmpeg is asked to emit.
#[derive(Debug, Clone, PartialEq)]
pub struct RawFormat {
    /// ffmpeg pix_fmt name passed to `-pix_fmt`
    pub pix_fmt: String,
    pub width: usize,
    pub height: usize,
    pub bit_depth: u32,
    /// 1 or 2: chroma width divisor
    pub sub_x: usize,
    /// 1 or 2: chroma height divisor
    pub sub_y: usize,
}

impl RawFormat {
    /// Pick the planar YUV format closest to the source. Keeps the source
    /// bit depth and subsampling; RGB sources become 4:4:4 at their depth
    /// (ffmpeg converts; `signalstats` only handled YUV too).
    pub fn for_source(src_pix_fmt: &str, width: u32, height: u32) -> RawFormat {
        let p = parse_pix_fmt(src_pix_fmt);
        let (sub_x, sub_y) = match p.subsampling {
            "444" => (1, 1),
            "422" => (2, 1),
            _ => (2, 2),
        };
        let bit_depth = match p.bit_depth {
            8 => 8,
            10 => 10,
            12 => 12,
            _ => 16,
        };
        let pix_fmt = if bit_depth == 8 {
            format!("yuv{}p", p.subsampling)
        } else {
            format!("yuv{}p{}le", p.subsampling, bit_depth)
        };
        RawFormat {
            pix_fmt,
            width: width as usize,
            height: height as usize,
            bit_depth,
            sub_x,
            sub_y,
        }
    }

    pub fn bytes_per_sample(&self) -> usize {
        if self.bit_depth > 8 { 2 } else { 1 }
    }

    /// (width, height) in samples of plane 0 (Y), 1 (U) or 2 (V).
    pub fn plane_dims(&self, plane: usize) -> (usize, usize) {
        if plane == 0 {
            (self.width, self.height)
        } else {
            (
                (self.width + self.sub_x - 1) / self.sub_x,
                (self.height + self.sub_y - 1) / self.sub_y,
            )
        }
    }

    pub fn plane_bytes(&self, plane: usize) -> usize {
        let (w, h) = self.plane_dims(plane);
        w * h * self.bytes_per_sample()
    }

    pub fn frame_bytes(&self) -> usize {
        self.plane_bytes(0) + self.plane_bytes(1) + self.plane_bytes(2)
    }

    /// Byte range of a plane inside a frame buffer.
    pub fn plane_range(&self, plane: usize) -> std::ops::Range<usize> {
        let mut start = 0;
        for p in 0..plane {
            start += self.plane_bytes(p);
        }
        start..start + self.plane_bytes(plane)
    }

    pub fn peak(&self) -> f64 {
        ((1u64 << self.bit_depth) - 1) as f64
    }
}

// ─── Per-frame results ──────────────────────────────────────────────────────

/// Statistics for one frame, in native code values.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameStats {
    pub y_min: u32,
    pub y_max: u32,
    pub y_avg: f64,
    pub u_min: u32,
    pub u_max: u32,
    pub u_avg: f64,
    pub v_min: u32,
    pub v_max: u32,
    pub v_avg: f64,
    /// Mean |Δ| vs previous frame per plane; 0 for the first frame.
    pub y_dif: f64,
    pub u_dif: f64,
    pub v_dif: f64,
    /// Thumbnail difference vs frame i-1 … i-4 (normalized 0..1); 0 when
    /// that frame doesn't exist.
    pub tdif: [f64; TDIF_DEPTH],
}

impl FrameStats {
    /// Convert to the JSON shape the rest of the engine consumes.
    pub fn to_json(&self, display_frame: i64, tc_fmt: &TimecodeFormat) -> Value {
        serde_json::json!({
            "frame": display_frame,
            "tc": tc_fmt.frame_to_tc(display_frame),
            "y_min": self.y_min,
            "y_max": self.y_max,
            "u_min": self.u_min,
            "u_max": self.u_max,
            "v_min": self.v_min,
            "v_max": self.v_max,
            "y_avg": round4(self.y_avg),
            "u_avg": round4(self.u_avg),
            "v_avg": round4(self.v_avg),
            "y_dif": round4(self.y_dif),
            "u_dif": round4(self.u_dif),
            "v_dif": round4(self.v_dif),
            "tdif_1": round4(self.tdif[0]),
            "tdif_2": round4(self.tdif[1]),
            "tdif_3": round4(self.tdif[2]),
            "tdif_4": round4(self.tdif[3]),
        })
    }
}

// ─── Plane maths ────────────────────────────────────────────────────────────

/// (min, max, sum) over a plane.
fn plane_minmaxsum(bytes: &[u8], two_byte: bool) -> (u32, u32, u64) {
    if two_byte {
        let (prefix, mid, suffix) = unsafe { bytes.align_to::<u16>() };
        // Plane buffers come from Vec<u8> allocations, which are at least
        // 2-byte aligned on every platform we run on; handle the general
        // case anyway so this can never read out of bounds.
        if !prefix.is_empty() || !suffix.is_empty() {
            return minmaxsum_iter(bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as u32));
        }
        minmaxsum_u16(mid)
    } else {
        minmaxsum_u8(bytes)
    }
}

fn minmaxsum_u8(data: &[u8]) -> (u32, u32, u64) {
    let mut mn = u8::MAX;
    let mut mx = u8::MIN;
    let mut sum: u64 = 0;
    // Chunked so the inner loops vectorize and the u32 accumulator can't
    // overflow (255 * 65536 < u32::MAX).
    for chunk in data.chunks(1 << 16) {
        let mut s: u32 = 0;
        for &v in chunk {
            mn = mn.min(v);
            mx = mx.max(v);
            s += v as u32;
        }
        sum += s as u64;
    }
    (mn as u32, mx as u32, sum)
}

fn minmaxsum_u16(data: &[u16]) -> (u32, u32, u64) {
    let mut mn = u16::MAX;
    let mut mx = u16::MIN;
    let mut sum: u64 = 0;
    for chunk in data.chunks(1 << 15) {
        let mut s: u32 = 0;
        for &v in chunk {
            let v = u16::from_le(v);
            mn = mn.min(v);
            mx = mx.max(v);
            s += v as u32;
        }
        sum += s as u64;
    }
    (mn as u32, mx as u32, sum)
}

fn minmaxsum_iter(it: impl Iterator<Item = u32>) -> (u32, u32, u64) {
    let mut mn = u32::MAX;
    let mut mx = 0u32;
    let mut sum = 0u64;
    for v in it {
        mn = mn.min(v);
        mx = mx.max(v);
        sum += v as u64;
    }
    (mn, mx, sum)
}

/// Sum of |a - b| over a plane.
fn plane_absdiff_sum(a: &[u8], b: &[u8], two_byte: bool) -> u64 {
    if two_byte {
        let (pa, ma, sa) = unsafe { a.align_to::<u16>() };
        let (pb, mb, sb) = unsafe { b.align_to::<u16>() };
        if !pa.is_empty() || !sa.is_empty() || !pb.is_empty() || !sb.is_empty() {
            return a
                .chunks_exact(2)
                .zip(b.chunks_exact(2))
                .map(|(x, y)| {
                    let x = u16::from_le_bytes([x[0], x[1]]) as i32;
                    let y = u16::from_le_bytes([y[0], y[1]]) as i32;
                    (x - y).unsigned_abs() as u64
                })
                .sum();
        }
        let mut sum = 0u64;
        for (ca, cb) in ma.chunks(1 << 15).zip(mb.chunks(1 << 15)) {
            let mut s: u32 = 0;
            for (&x, &y) in ca.iter().zip(cb) {
                let x = u16::from_le(x);
                let y = u16::from_le(y);
                s += x.abs_diff(y) as u32;
            }
            sum += s as u64;
        }
        sum
    } else {
        let mut sum = 0u64;
        for (ca, cb) in a.chunks(1 << 16).zip(b.chunks(1 << 16)) {
            let mut s: u32 = 0;
            for (&x, &y) in ca.iter().zip(cb) {
                s += x.abs_diff(y) as u32;
            }
            sum += s as u64;
        }
        sum
    }
}

/// Box-average a luma plane onto the THUMB_W × THUMB_H grid, normalized 0..1.
fn luma_thumbnail(plane: &[u8], w: usize, h: usize, two_byte: bool, peak: f64) -> Vec<f32> {
    let mut sums = vec![0u64; THUMB_CELLS];
    let mut counts = vec![0u64; THUMB_CELLS];

    // Cell boundaries along x, so each cell's row segment is a contiguous
    // slice (which keeps the inner sum a simple vectorizable loop).
    let mut xb = [0usize; THUMB_W + 1];
    for (tx, b) in xb.iter_mut().enumerate() {
        *b = tx * w / THUMB_W;
    }

    let bps = if two_byte { 2 } else { 1 };
    for y in 0..h {
        let ty = y * THUMB_H / h;
        let row = &plane[y * w * bps..(y + 1) * w * bps];
        for tx in 0..THUMB_W {
            let (x0, x1) = (xb[tx], xb[tx + 1]);
            if x1 <= x0 {
                continue;
            }
            let seg = &row[x0 * bps..x1 * bps];
            let s: u64 = if two_byte {
                seg.chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]) as u64)
                    .sum()
            } else {
                seg.iter().map(|&v| v as u64).sum()
            };
            let cell = ty * THUMB_W + tx;
            sums[cell] += s;
            counts[cell] += (x1 - x0) as u64;
        }
    }

    sums.iter()
        .zip(&counts)
        .map(|(&s, &c)| if c == 0 { 0.0 } else { (s as f64 / c as f64 / peak) as f32 })
        .collect()
}

fn thumb_distance(a: &[f32], b: &[f32]) -> f64 {
    let s: f32 = a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum();
    (s / THUMB_CELLS as f32) as f64
}

// ─── Buffer pool ────────────────────────────────────────────────────────────

/// A frame buffer that returns itself to the pool when dropped.
struct FrameBuf {
    data: Vec<u8>,
    pool: Sender<Vec<u8>>,
}

impl Drop for FrameBuf {
    fn drop(&mut self) {
        let data = std::mem::take(&mut self.data);
        let _ = self.pool.send(data);
    }
}

struct Job {
    index: u64,
    frame: Arc<FrameBuf>,
    prev: Option<Arc<FrameBuf>>,
}

struct WorkerOut {
    index: u64,
    stats: FrameStats, // tdif left zeroed; collector fills it in
    thumb: Vec<f32>,
}

fn compute_frame(fmt: &RawFormat, frame: &[u8], prev: Option<&[u8]>) -> (FrameStats, Vec<f32>) {
    let two = fmt.bytes_per_sample() == 2;
    let mut planes = [(0u32, 0u32, 0f64); 3];
    let mut difs = [0f64; 3];

    for p in 0..3 {
        let r = fmt.plane_range(p);
        let (w, h) = fmt.plane_dims(p);
        let n = (w * h) as f64;
        let (mn, mx, sum) = plane_minmaxsum(&frame[r.clone()], two);
        planes[p] = (mn, mx, sum as f64 / n);
        if let Some(prev) = prev {
            difs[p] = plane_absdiff_sum(&frame[r.clone()], &prev[r], two) as f64 / n;
        }
    }

    let (yw, yh) = fmt.plane_dims(0);
    let thumb = luma_thumbnail(&frame[fmt.plane_range(0)], yw, yh, two, fmt.peak());

    let stats = FrameStats {
        y_min: planes[0].0,
        y_max: planes[0].1,
        y_avg: planes[0].2,
        u_min: planes[1].0,
        u_max: planes[1].1,
        u_avg: planes[1].2,
        v_min: planes[2].0,
        v_max: planes[2].1,
        v_avg: planes[2].2,
        y_dif: difs[0],
        u_dif: difs[1],
        v_dif: difs[2],
        tdif: [0.0; TDIF_DEPTH],
    };
    (stats, thumb)
}

// ─── Driver ─────────────────────────────────────────────────────────────────

/// Why a raw stats pass failed, with enough context for the caller to decide
/// whether a retry makes sense.
#[derive(Debug, Clone, PartialEq)]
pub struct RawStatsError {
    pub message: String,
    /// Complete frames read from ffmpeg before the failure.
    pub frames_read: u64,
    /// The ffmpeg child was taken out of the process handle by someone else
    /// (app quit / cancel) — never retry this.
    pub killed: bool,
}

impl RawStatsError {
    fn new(message: impl Into<String>, frames_read: u64) -> Self {
        Self { message: message.into(), frames_read, killed: false }
    }
}

impl std::fmt::Display for RawStatsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<RawStatsError> for String {
    fn from(e: RawStatsError) -> String {
        e.message
    }
}

/// Codecs for which passing `-hwaccel` to ffmpeg is worthwhile. Intra-only
/// mastering codecs (ProRes, DNxHR, FFV1, …) have no hardware decoders.
pub const HW_DECODE_CODECS: &[&str] =
    &["h264", "hevc", "av1", "vp9", "mpeg2video", "mpeg4", "vc1"];

/// Decide whether to hw-decode this input. `available` is what
/// `probe::detect_hwaccel()` found in this ffmpeg build — which says nothing
/// about whether a device actually exists, so callers must be ready to fall
/// back (see `analysis::run_levels_analysis`).
pub fn choose_hwaccel(codec_name: &str, file_path: &str, available: Option<&str>) -> Option<String> {
    let hw = available?;
    if is_concat_manifest(file_path) {
        return None;
    }
    if HW_DECODE_CODECS.contains(&codec_name.to_ascii_lowercase().as_str()) {
        Some(hw.to_string())
    } else {
        None
    }
}

/// Decode `file_path` with ffmpeg and compute per-frame statistics.
///
/// `hwaccel` is passed through to ffmpeg's `-hwaccel` for the input (only
/// useful for codecs the GPU can decode; callers decide). `total_frames`
/// drives `-frames:v` and progress. The ffmpeg child is stored in
/// `process_handle` so the app can kill it on exit.
pub fn run_raw_stats(
    file_path: &str,
    fmt: &RawFormat,
    hwaccel: Option<&str>,
    total_frames: Option<u64>,
    progress: Option<ProgressFn>,
    process_handle: crate::engine::analysis::ProcessHandle,
) -> Result<Vec<FrameStats>, RawStatsError> {
    // ── ffmpeg command ──
    let mut args: Vec<String> = vec!["-nostdin".into(), "-hide_banner".into()];
    if let Some(hw) = hwaccel {
        if !is_concat_manifest(file_path) {
            args.extend(["-hwaccel".into(), hw.to_string()]);
        }
    }
    if is_concat_manifest(file_path) {
        args.extend(["-f".into(), "concat".into(), "-safe".into(), "0".into()]);
    }
    args.extend(["-i".into(), file_path.into()]);
    args.extend(["-map".into(), "0:v:0".into(), "-an".into(), "-sn".into()]);
    // Emit exactly the decoded frames: no duplication/dropping to a fixed rate.
    args.extend(["-fps_mode".into(), "passthrough".into()]);
    if let Some(total) = total_frames {
        args.extend(["-frames:v".into(), total.to_string()]);
    }
    args.extend([
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        fmt.pix_fmt.clone(),
        "pipe:1".into(),
    ]);

    eprintln!("[vcompare] cmd: ffmpeg {}", args.join(" "));

    let mut child = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RawStatsError::new(format!("Failed to spawn ffmpeg: {e}"), 0))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RawStatsError::new("Failed to get ffmpeg stdout", 0))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| RawStatsError::new("Failed to get ffmpeg stderr", 0))?;
    *process_handle.lock().unwrap() = Some(child);

    // ── stderr: keep a tail for error reporting ──
    let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let tail_writer = stderr_tail.clone();
    let stderr_thread = std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            // ffmpeg progress lines end in \r, everything else in \n
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    for piece in buf.split(|&b| b == b'\r' || b == b'\n') {
                        if piece.is_empty() {
                            continue;
                        }
                        let line = String::from_utf8_lossy(piece).to_string();
                        let mut t = tail_writer.lock().unwrap();
                        if t.len() >= 20 {
                            t.pop_front();
                        }
                        t.push_back(line);
                    }
                }
            }
        }
    });

    // ── worker pool ──
    let frame_bytes = fmt.frame_bytes();
    let n_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1);
    // Frames in flight: enough to keep every worker busy plus a little
    // read-ahead, bounded by the memory budget. Each job also pins its
    // predecessor, so the pool needs one extra.
    let max_buffers = (IN_FLIGHT_BYTES / frame_bytes.max(1)).clamp(3, 2 * n_workers + 2);

    let (job_tx, job_rx) = mpsc::sync_channel::<Job>(max_buffers);
    let job_rx = Arc::new(Mutex::new(job_rx));
    let (res_tx, res_rx) = mpsc::channel::<WorkerOut>();
    let (pool_tx, pool_rx) = mpsc::channel::<Vec<u8>>();

    let mut workers = Vec::with_capacity(n_workers);
    for _ in 0..n_workers {
        let rx = job_rx.clone();
        let tx = res_tx.clone();
        let fmt = fmt.clone();
        workers.push(std::thread::spawn(move || loop {
            let job = match rx.lock().unwrap().recv() {
                Ok(j) => j,
                Err(_) => break,
            };
            let prev = job.prev.as_ref().map(|p| p.data.as_slice());
            let (stats, thumb) = compute_frame(&fmt, &job.frame.data, prev);
            if tx.send(WorkerOut { index: job.index, stats, thumb }).is_err() {
                break;
            }
            // job (and its buffers) dropped here → buffers return to pool
        }));
    }
    drop(res_tx);

    // ── collector: reorder, thumbnail difs, progress ──
    let collector = std::thread::spawn(move || -> Vec<FrameStats> {
        let mut out: Vec<FrameStats> = Vec::new();
        let mut pending: BTreeMap<u64, WorkerOut> = BTreeMap::new();
        let mut recent_thumbs: VecDeque<Vec<f32>> = VecDeque::with_capacity(TDIF_DEPTH + 1);
        let mut next: u64 = 0;
        let mut last_progress = Instant::now();

        for r in res_rx {
            pending.insert(r.index, r);
            while let Some(mut r) = pending.remove(&next) {
                for (k, back) in recent_thumbs.iter().rev().enumerate() {
                    r.stats.tdif[k] = thumb_distance(&r.thumb, back);
                }
                recent_thumbs.push_back(r.thumb);
                if recent_thumbs.len() > TDIF_DEPTH {
                    recent_thumbs.pop_front();
                }
                out.push(r.stats);
                next += 1;

                if let (Some(total), Some(cb)) = (total_frames, progress.as_ref()) {
                    if last_progress.elapsed().as_millis() >= 250 {
                        last_progress = Instant::now();
                        cb(next, total);
                    }
                }
            }
        }
        if let (Some(total), Some(cb)) = (total_frames, progress.as_ref()) {
            cb(total.min(next), total);
        }
        out
    });

    // ── reader (this thread) ──
    let mut reader = BufReader::with_capacity(1 << 20, stdout);
    let mut allocated = 0usize;
    let mut prev: Option<Arc<FrameBuf>> = None;
    let mut index: u64 = 0;
    let mut read_err: Option<String> = None;
    let profile = std::env::var_os("VCOMPARE_RAWSTATS_PROFILE").is_some();
    let (mut t_pool, mut t_read, mut t_send) = (0f64, 0f64, 0f64);
    let t_start = Instant::now();

    loop {
        let t0 = Instant::now();
        let mut data = if allocated < max_buffers {
            allocated += 1;
            vec![0u8; frame_bytes]
        } else {
            match pool_rx.recv() {
                Ok(d) => d,
                Err(_) => break,
            }
        };
        if data.len() != frame_bytes {
            data.resize(frame_bytes, 0);
        }
        let t1 = Instant::now();

        match read_frame(&mut reader, &mut data) {
            Ok(true) => {}
            Ok(false) => break, // clean EOF
            Err(e) => {
                read_err = Some(e);
                break;
            }
        }
        let t2 = Instant::now();

        let frame = Arc::new(FrameBuf { data, pool: pool_tx.clone() });
        let job = Job { index, frame: frame.clone(), prev: prev.take() };
        if job_tx.send(job).is_err() {
            read_err = Some("worker pool stopped".into());
            break;
        }
        prev = Some(frame);
        index += 1;
        let t3 = Instant::now();
        t_pool += (t1 - t0).as_secs_f64();
        t_read += (t2 - t1).as_secs_f64();
        t_send += (t3 - t2).as_secs_f64();
    }
    if profile {
        eprintln!(
            "[vcompare] rawstats reader: {index} frames, {:.2}s total — waiting for buffer {:.2}s, reading pipe {:.2}s, sending jobs {:.2}s (workers={n_workers}, buffers={max_buffers})",
            t_start.elapsed().as_secs_f64(), t_pool, t_read, t_send
        );
    }
    drop(prev);
    drop(job_tx);

    let frames_read = index;
    let mut out = collector
        .join()
        .map_err(|_| RawStatsError::new("collector thread panicked", frames_read))?;
    for w in workers {
        let _ = w.join();
    }

    // ── ffmpeg exit status ──
    let mut child_opt = process_handle.lock().unwrap().take();
    let status = match child_opt.as_mut() {
        Some(c) => c.wait().map_err(|e| {
            RawStatsError::new(format!("Failed to wait for ffmpeg: {e}"), frames_read)
        })?,
        None => {
            return Err(RawStatsError {
                message: "ffmpeg process was killed externally".into(),
                frames_read,
                killed: true,
            })
        }
    };
    let _ = stderr_thread.join();

    if let Some(e) = read_err {
        return Err(RawStatsError::new(e, frames_read));
    }
    if !status.success() {
        let tail: Vec<String> = stderr_tail.lock().unwrap().iter().cloned().collect();
        return Err(RawStatsError::new(
            format!(
                "FFmpeg exited with code {}\n\n{}",
                status.code().unwrap_or(-1),
                tail.join("\n")
            ),
            frames_read,
        ));
    }

    out.shrink_to_fit();
    Ok(out)
}

/// Fill `buf` with exactly one frame. Ok(false) on clean EOF before any
/// bytes; Err on a truncated frame.
fn read_frame<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<bool, String> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(format!(
                    "ffmpeg output ended mid-frame ({filled} of {} bytes)",
                    buf.len()
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("read error: {e}")),
        }
    }
    Ok(true)
}

/// Exposed for benchmarking only.
#[doc(hidden)]
#[allow(dead_code)]
pub fn bench_compute(fmt: &RawFormat, frame: &[u8], prev: Option<&[u8]>) -> (FrameStats, Vec<f32>) {
    compute_frame(fmt, frame, prev)
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(pix: &str, w: u32, h: u32) -> RawFormat {
        RawFormat::for_source(pix, w, h)
    }

    #[test]
    fn format_geometry() {
        let f = fmt("yuv422p10le", 1920, 1080);
        assert_eq!(f.pix_fmt, "yuv422p10le");
        assert_eq!(f.plane_dims(1), (960, 1080));
        assert_eq!(f.frame_bytes(), 1920 * 1080 * 2 * 2);
        let f = fmt("yuv420p", 1919, 1079);
        assert_eq!(f.plane_dims(1), (960, 540));
        assert_eq!(f.plane_range(2).start, 1919 * 1079 + 960 * 540);
        let f = fmt("gbrp12le", 100, 100);
        assert_eq!(f.pix_fmt, "yuv444p12le");
        assert_eq!(f.bit_depth, 12);
        let f = fmt("rgb48le", 10, 10);
        assert_eq!(f.pix_fmt, "yuv444p16le");
    }

    fn fill_u16(buf: &mut [u8], range: std::ops::Range<usize>, val: u16) {
        for c in buf[range].chunks_exact_mut(2) {
            c.copy_from_slice(&val.to_le_bytes());
        }
    }

    #[test]
    fn stats_on_flat_10bit_frame() {
        let f = fmt("yuv422p10le", 64, 36);
        let mut a = vec![0u8; f.frame_bytes()];
        fill_u16(&mut a, f.plane_range(0), 400);
        fill_u16(&mut a, f.plane_range(1), 512);
        fill_u16(&mut a, f.plane_range(2), 520);
        // One hot pixel in Y
        let r = f.plane_range(0);
        a[r.start..r.start + 2].copy_from_slice(&940u16.to_le_bytes());

        let (s, thumb) = compute_frame(&f, &a, None);
        assert_eq!((s.y_min, s.y_max), (400, 940));
        assert!((s.y_avg - (400.0 * 2303.0 + 940.0) / 2304.0).abs() < 1e-9);
        assert_eq!((s.u_min, s.u_max, s.v_min, s.v_max), (512, 512, 520, 520));
        assert_eq!(s.y_dif, 0.0);
        assert_eq!(thumb.len(), THUMB_CELLS);
        // Thumbnail cell (0,0) holds the hot pixel; others are flat 400/1023
        assert!(thumb[0] > thumb[1]);
        assert!((thumb[1] - 400.0 / 1023.0).abs() < 1e-5);

        // Difference against a frame that is 10 codes brighter everywhere
        let mut b = a.clone();
        fill_u16(&mut b, f.plane_range(0), 410);
        let (s2, _) = compute_frame(&f, &b, Some(&a));
        assert!((s2.y_dif - (10.0 * 2303.0 + 530.0) / 2304.0).abs() < 1e-9);
        assert_eq!(s2.u_dif, 0.0);
    }

    #[test]
    fn stats_on_8bit_frame() {
        let f = fmt("yuv420p", 32, 18);
        let mut a = vec![128u8; f.frame_bytes()];
        let r = f.plane_range(0);
        a[r.start + 5] = 16;
        a[r.start + 6] = 235;
        let (s, _) = compute_frame(&f, &a, None);
        assert_eq!((s.y_min, s.y_max), (16, 235));
        assert_eq!(s.u_min, 128);
        let b = vec![129u8; f.frame_bytes()];
        let (s2, _) = compute_frame(&f, &b, Some(&a));
        assert_eq!(s2.u_dif, 1.0);
        assert!((s2.y_dif - (1.0 * 574.0 + 113.0 + 106.0) / 576.0).abs() < 1e-9);
    }

    #[test]
    fn thumbnail_distance_sees_structure_not_noise() {
        // Same mean luma, different layout: left-bright vs right-bright.
        let f = fmt("yuv444p", 64, 36);
        let mut a = vec![128u8; f.frame_bytes()];
        let mut b = vec![128u8; f.frame_bytes()];
        for y in 0..36 {
            for x in 0..64 {
                a[y * 64 + x] = if x < 32 { 200 } else { 56 };
                b[y * 64 + x] = if x < 32 { 56 } else { 200 };
            }
        }
        let (sa, ta) = compute_frame(&f, &a, None);
        let (sb, tb) = compute_frame(&f, &b, None);
        assert_eq!(sa.y_avg, sb.y_avg);
        assert!((thumb_distance(&ta, &tb) - 144.0 / 255.0).abs() < 1e-5);
    }

    #[test]
    fn hwaccel_policy() {
        assert_eq!(choose_hwaccel("hevc", "/a.mov", Some("cuda")), Some("cuda".into()));
        assert_eq!(choose_hwaccel("H264", "/a.mp4", Some("videotoolbox")), Some("videotoolbox".into()));
        assert_eq!(choose_hwaccel("prores", "/a.mov", Some("cuda")), None);
        assert_eq!(choose_hwaccel("dnxhd", "/a.mxf", Some("cuda")), None);
        assert_eq!(choose_hwaccel("hevc", "/a.mov", None), None);
    }

    #[test]
    fn read_frame_handles_eof_and_truncation() {
        let data = vec![1u8; 10];
        let mut cur = std::io::Cursor::new(data);
        let mut buf = [0u8; 4];
        assert_eq!(read_frame(&mut cur, &mut buf), Ok(true));
        assert_eq!(read_frame(&mut cur, &mut buf), Ok(true));
        assert!(read_frame(&mut cur, &mut buf).is_err()); // 2 bytes left
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        assert_eq!(read_frame(&mut empty, &mut buf), Ok(false));
    }
}
