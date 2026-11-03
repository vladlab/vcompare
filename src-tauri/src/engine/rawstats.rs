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
//! - `y_low`, `y_high`: 10th / 90th percentile luma (signalstats YLOW/YHIGH)
//! - `y_oor_lo`, `y_oor_hi`, `c_oor`: sample counts outside the legal
//!   (limited/"video") range — luma below black / above white, chroma
//!   (U and V together) outside 16–240 scaled to the bit depth. Only for
//!   limited-range YUV sources; absent for full-range and RGB sources.
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
    /// Legal (limited-range) bounds to count out-of-range samples against.
    /// `None` for full-range and RGB sources, where "legal" doesn't apply.
    pub legal: Option<LegalRange>,
}

/// Limited-range code-value bounds for one bit depth (inclusive).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LegalRange {
    pub y_lo: u32,
    pub y_hi: u32,
    pub c_lo: u32,
    pub c_hi: u32,
}

impl LegalRange {
    /// 16–235 luma, 16–240 chroma, scaled by 2^(depth-8) (BT.601/709/2020
    /// limited range).
    pub fn limited(bit_depth: u32) -> LegalRange {
        let k = 1u32 << bit_depth.saturating_sub(8);
        LegalRange { y_lo: 16 * k, y_hi: 235 * k, c_lo: 16 * k, c_hi: 240 * k }
    }
}

impl RawFormat {
    /// Pick the planar YUV format closest to the source. Keeps the source
    /// bit depth and subsampling; RGB sources become 4:4:4 at their depth
    /// (ffmpeg converts; `signalstats` only handled YUV too).
    ///
    /// Full-range `yuvj*` sources are emitted as themselves: asking ffmpeg
    /// for the plain `yuv*` equivalent would rescale them to limited range
    /// and change the very values being measured.
    pub fn for_source(src_pix_fmt: &str, width: u32, height: u32) -> RawFormat {
        let src = src_pix_fmt.trim().to_ascii_lowercase();
        let p = parse_pix_fmt(&src);
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
        let full_range_fmt = src.starts_with("yuvj");
        let pix_fmt = if full_range_fmt {
            // yuvj420p / yuvj422p / yuvj444p are all 8-bit planar
            format!("yuvj{}p", p.subsampling)
        } else if bit_depth == 8 {
            format!("yuv{}p", p.subsampling)
        } else {
            format!("yuv{}p{}le", p.subsampling, bit_depth)
        };
        let legal = if p.is_rgb || full_range_fmt {
            None
        } else {
            Some(LegalRange::limited(bit_depth))
        };
        RawFormat {
            pix_fmt,
            width: width as usize,
            height: height as usize,
            bit_depth,
            sub_x,
            sub_y,
            legal,
        }
    }

    /// Mark the source as full range (ffprobe `color_range=pc`), which turns
    /// off legal-range counting.
    pub fn with_full_range(mut self, full: bool) -> RawFormat {
        if full {
            self.legal = None;
        }
        self
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
    /// 10th / 90th percentile luma code values.
    pub y_low: u32,
    pub y_high: u32,
    /// Out-of-legal-range sample counts (luma below, luma above, chroma
    /// outside); `None` when the source isn't limited-range YUV.
    pub oor: Option<OutOfRange>,
    /// Mean |Δ| vs previous frame per plane; 0 for the first frame.
    pub y_dif: f64,
    pub u_dif: f64,
    pub v_dif: f64,
    /// Thumbnail difference vs frame i-1 … i-4 (normalized 0..1); 0 when
    /// that frame doesn't exist.
    pub tdif: [f64; TDIF_DEPTH],
}

/// Sample counts outside the legal range for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct OutOfRange {
    pub y_lo: u64,
    pub y_hi: u64,
    /// U and V samples together
    pub c: u64,
}

impl FrameStats {
    /// Same decoded picture, judged from everything that depends only on the
    /// frame itself (not on its neighbours). Used to check chunk seams.
    pub fn same_picture(&self, o: &FrameStats) -> bool {
        (self.y_min, self.y_max, self.u_min, self.u_max, self.v_min, self.v_max)
            == (o.y_min, o.y_max, o.u_min, o.u_max, o.v_min, o.v_max)
            && (self.y_low, self.y_high, self.oor) == (o.y_low, o.y_high, o.oor)
            && self.y_avg == o.y_avg
            && self.u_avg == o.u_avg
            && self.v_avg == o.v_avg
    }

    /// Convert to the JSON shape the rest of the engine consumes.
    pub fn to_json(&self, display_frame: i64, tc_fmt: &TimecodeFormat) -> Value {
        let mut v = self.to_json_base(display_frame, tc_fmt);
        let obj = v.as_object_mut().expect("object");
        obj.insert("y_low".into(), self.y_low.into());
        obj.insert("y_high".into(), self.y_high.into());
        if let Some(o) = self.oor {
            obj.insert("y_oor_lo".into(), o.y_lo.into());
            obj.insert("y_oor_hi".into(), o.y_hi.into());
            obj.insert("c_oor".into(), o.c.into());
        }
        v
    }

    fn to_json_base(&self, display_frame: i64, tc_fmt: &TimecodeFormat) -> Value {
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

/// Everything derived from one plane's histogram.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PlaneSummary {
    min: u32,
    max: u32,
    sum: u64,
    /// 10th / 90th percentile (first code value whose cumulative count
    /// reaches round(n·0.1) / round(n·0.9), as signalstats computes it)
    low: u32,
    high: u32,
    /// samples < lo and > hi of the given bounds
    below: u64,
    above: u64,
}

/// min / max / sum and out-of-bounds counts in one pass, for planes that
/// don't need a histogram (chroma). The comparisons are branch-free so the
/// loops vectorize. `bounds = None` skips the counting.
fn plane_scan(bytes: &[u8], two_byte: bool, bounds: Option<(u32, u32)>) -> PlaneSummary {
    // With no bounds, use an impossible range so the counts stay 0 without
    // a second loop variant.
    let (lo, hi) = bounds.unwrap_or((0, u32::MAX));
    let (mut mn, mut mx, mut sum, mut below, mut above) = (u32::MAX, 0u32, 0u64, 0u64, 0u64);

    if two_byte {
        let (prefix, mid, suffix) = unsafe { bytes.align_to::<u16>() };
        let scalar = |v: u32, mn: &mut u32, mx: &mut u32, sum: &mut u64, b: &mut u64, a: &mut u64| {
            *mn = (*mn).min(v);
            *mx = (*mx).max(v);
            *sum += v as u64;
            *b += (v < lo) as u64;
            *a += (v > hi) as u64;
        };
        if !prefix.is_empty() || !suffix.is_empty() {
            // Unaligned buffer (never happens for Vec-backed frames): byte-wise.
            for c in bytes.chunks_exact(2) {
                let v = u16::from_le_bytes([c[0], c[1]]) as u32;
                scalar(v, &mut mn, &mut mx, &mut sum, &mut below, &mut above);
            }
        } else {
            let lo16 = lo.min(u16::MAX as u32 + 1);
            let hi16 = hi.min(u16::MAX as u32);
            for chunk in mid.chunks(1 << 15) {
                let (mut cmn, mut cmx) = (u16::MAX, 0u16);
                let (mut s, mut b, mut a) = (0u32, 0u32, 0u32);
                for &v in chunk {
                    let v = u16::from_le(v);
                    cmn = cmn.min(v);
                    cmx = cmx.max(v);
                    s += v as u32;
                    b += ((v as u32) < lo16) as u32;
                    a += ((v as u32) > hi16) as u32;
                }
                mn = mn.min(cmn as u32);
                mx = mx.max(cmx as u32);
                sum += s as u64;
                below += b as u64;
                above += a as u64;
            }
        }
    } else {
        for chunk in bytes.chunks(1 << 16) {
            let (mut cmn, mut cmx) = (u8::MAX, 0u8);
            let (mut s, mut b, mut a) = (0u32, 0u32, 0u32);
            for &v in chunk {
                cmn = cmn.min(v);
                cmx = cmx.max(v);
                s += v as u32;
                b += ((v as u32) < lo) as u32;
                a += ((v as u32) > hi) as u32;
            }
            mn = mn.min(cmn as u32);
            mx = mx.max(cmx as u32);
            sum += s as u64;
            below += b as u64;
            above += a as u64;
        }
    }
    if bytes.is_empty() {
        mn = 0;
    }
    PlaneSummary { min: mn, max: mx, sum, low: 0, high: 0, below, above }
}

/// Histogram of a plane with `bins` bins (values above the last bin are
/// clamped into it). Four interleaved sub-histograms avoid the
/// store-to-load stall of incrementing the same counter back to back on
/// flat regions, then get summed.
fn plane_histogram(bytes: &[u8], two_byte: bool, bins: usize) -> Vec<u32> {
    let top = bins - 1;
    let mut h = vec![0u32; bins * 4];
    let (h0, rest) = h.split_at_mut(bins);
    let (h1, rest) = rest.split_at_mut(bins);
    let (h2, h3) = rest.split_at_mut(bins);

    if two_byte {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            let a = u16::from_le_bytes([c[0], c[1]]) as usize;
            let b = u16::from_le_bytes([c[2], c[3]]) as usize;
            let d = u16::from_le_bytes([c[4], c[5]]) as usize;
            let e = u16::from_le_bytes([c[6], c[7]]) as usize;
            h0[a.min(top)] += 1;
            h1[b.min(top)] += 1;
            h2[d.min(top)] += 1;
            h3[e.min(top)] += 1;
        }
        for c in chunks.remainder().chunks_exact(2) {
            h0[(u16::from_le_bytes([c[0], c[1]]) as usize).min(top)] += 1;
        }
    } else {
        let mut chunks = bytes.chunks_exact(4);
        for c in &mut chunks {
            h0[(c[0] as usize).min(top)] += 1;
            h1[(c[1] as usize).min(top)] += 1;
            h2[(c[2] as usize).min(top)] += 1;
            h3[(c[3] as usize).min(top)] += 1;
        }
        for &v in chunks.remainder() {
            h0[(v as usize).min(top)] += 1;
        }
    }

    for i in 0..bins {
        h0[i] += h1[i] + h2[i] + h3[i];
    }
    h.truncate(bins);
    h
}

fn summarize(hist: &[u32], n: u64, bounds: Option<(u32, u32)>) -> PlaneSummary {
    let low_target = ((n as f64) * 0.10).round() as u64;
    let high_target = ((n as f64) * 0.90).round() as u64;
    let (mut min, mut max) = (None, 0u32);
    let (mut low, mut high) = (None, None);
    let (mut sum, mut acc, mut below, mut above) = (0u64, 0u64, 0u64, 0u64);

    for (v, &c) in hist.iter().enumerate() {
        if c == 0 {
            // still need the percentile test when the target is 0
            if low.is_none() && acc >= low_target { low = Some(v as u32); }
            if high.is_none() && acc >= high_target { high = Some(v as u32); }
            continue;
        }
        let v32 = v as u32;
        let c = c as u64;
        if min.is_none() { min = Some(v32); }
        max = v32;
        sum += v as u64 * c;
        acc += c;
        if low.is_none() && acc >= low_target { low = Some(v32); }
        if high.is_none() && acc >= high_target { high = Some(v32); }
        if let Some((lo, hi)) = bounds {
            if v32 < lo { below += c; }
            if v32 > hi { above += c; }
        }
    }
    PlaneSummary {
        min: min.unwrap_or(0),
        max,
        sum,
        low: low.unwrap_or(0),
        high: high.unwrap_or(max),
        below,
        above,
    }
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

/// Test-only fault injection: panic in the worker computing this frame.
#[cfg(test)]
static PANIC_AT_FRAME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

fn compute_frame(fmt: &RawFormat, frame: &[u8], prev: Option<&[u8]>) -> (FrameStats, Vec<f32>) {
    let two = fmt.bytes_per_sample() == 2;
    let bins = 1usize << fmt.bit_depth;
    let mut planes: [Option<PlaneSummary>; 3] = [None; 3];
    let mut difs = [0f64; 3];

    for p in 0..3 {
        let r = fmt.plane_range(p);
        let (w, h) = fmt.plane_dims(p);
        let n = (w * h) as u64;
        let bounds = fmt.legal.map(|l| if p == 0 { (l.y_lo, l.y_hi) } else { (l.c_lo, l.c_hi) });
        planes[p] = Some(if p == 0 {
            // Luma needs percentiles, so it gets a histogram (and min/max/
            // sum/counts fall out of it). Chroma only needs the scan.
            let hist = plane_histogram(&frame[r.clone()], two, bins);
            summarize(&hist, n, bounds)
        } else {
            plane_scan(&frame[r.clone()], two, bounds)
        });
        if let Some(prev) = prev {
            difs[p] = plane_absdiff_sum(&frame[r.clone()], &prev[r], two) as f64 / n as f64;
        }
    }
    let [py, pu, pv] = planes.map(|p| p.expect("all planes computed"));
    let avg = |p: &PlaneSummary, plane: usize| {
        let (w, h) = fmt.plane_dims(plane);
        p.sum as f64 / (w * h) as f64
    };

    let (yw, yh) = fmt.plane_dims(0);
    let thumb = luma_thumbnail(&frame[fmt.plane_range(0)], yw, yh, two, fmt.peak());

    let stats = FrameStats {
        y_min: py.min,
        y_max: py.max,
        y_avg: avg(&py, 0),
        u_min: pu.min,
        u_max: pu.max,
        u_avg: avg(&pu, 1),
        v_min: pv.min,
        v_max: pv.max,
        v_avg: avg(&pv, 2),
        y_low: py.low,
        y_high: py.high,
        oor: fmt.legal.map(|_| OutOfRange {
            y_lo: py.below,
            y_hi: py.above,
            c: pu.below + pu.above + pv.below + pv.above,
        }),
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

/// Where a raw stats run spent its time, for telling decode-bound runs from
/// stats-bound ones.
#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    pub frames: u64,
    pub elapsed_s: f64,
    /// Reader blocked on the pipe: ffmpeg decoding (or reading the file).
    pub wait_ffmpeg_s: f64,
    /// Reader blocked for a free buffer: stats workers behind.
    pub wait_buffer_s: f64,
    pub workers: usize,
    /// Fraction of total worker capacity spent computing (0..1).
    pub worker_busy: f64,
}

impl RunReport {
    pub fn fps(&self) -> f64 {
        if self.elapsed_s > 0.0 { self.frames as f64 / self.elapsed_s } else { 0.0 }
    }

    /// What limited this run, in a few words.
    pub fn bottleneck(&self) -> &'static str {
        let e = self.elapsed_s.max(1e-9);
        if self.wait_buffer_s > 0.4 * e {
            "stats workers"
        } else if self.wait_ffmpeg_s > 0.5 * e {
            "ffmpeg (decode, file read or raw output)"
        } else {
            "balanced"
        }
    }

    /// One line for logs and the UI.
    pub fn summary(&self) -> String {
        format!(
            "{} frames in {:.1}s = {:.1} fps — limited by {} (waited on ffmpeg {:.0}%, on stats {:.0}%; {} stats workers {:.0}% busy)",
            self.frames,
            self.elapsed_s,
            self.fps(),
            self.bottleneck(),
            100.0 * self.wait_ffmpeg_s / self.elapsed_s.max(1e-9),
            100.0 * self.wait_buffer_s / self.elapsed_s.max(1e-9),
            self.workers,
            100.0 * self.worker_busy,
        )
    }
}

impl RunReport {
    /// Merge the reports of chunks that ran side by side over `elapsed_s`.
    pub fn combine(reports: &[RunReport], elapsed_s: f64) -> RunReport {
        let n = reports.len().max(1) as f64;
        let frac = |f: &dyn Fn(&RunReport) -> f64| {
            reports.iter().map(|r| f(r) / r.elapsed_s.max(1e-9)).sum::<f64>() / n
        };
        RunReport {
            frames: reports.iter().map(|r| r.frames).sum(),
            elapsed_s,
            wait_ffmpeg_s: frac(&|r| r.wait_ffmpeg_s) * elapsed_s,
            wait_buffer_s: frac(&|r| r.wait_buffer_s) * elapsed_s,
            workers: reports.iter().map(|r| r.workers).sum(),
            worker_busy: reports.iter().map(|r| r.worker_busy).sum::<f64>() / n,
        }
    }
}

// ─── Chunked decoding ───────────────────────────────────────────────────────
//
// One ffmpeg writing raw frames to a pipe tops out at ~60 fps for UHD 10-bit
// 4:2:2 (≈2 GB/s through a single process's output path), far below what a
// many-core machine decodes. Splitting the file into chunks decoded by
// separate ffmpeg processes in parallel lifts that ceiling. Only sources
// where a seek lands exactly on a frame are chunked: intra-only codecs at a
// constant frame rate. The frame count may be the container's or an estimate
// from the duration (MXF and MKV often report none); with an estimate the
// last chunk simply decodes to the end of the file.

/// Codecs whose every frame is a keyframe, so a seek is exact and cheap.
pub const INTRA_ONLY_CODECS: &[&str] = &[
    "prores", "dnxhd", "ffv1", "jpeg2000", "mjpeg", "cfhd", "v210", "v410",
    "r210", "rawvideo", "utvideo", "huffyuv", "ffvhuff", "magicyuv",
    "dvvideo", "hqx", "hq_hqa",
];

/// Chunks shorter than this aren't worth a separate ffmpeg.
pub const MIN_CHUNK_FRAMES: u64 = 240;
/// Extra frames decoded before each chunk (and dropped) so frame and
/// thumbnail differences are continuous across chunk seams.
pub const CHUNK_WARMUP: u64 = TDIF_DEPTH as u64;
/// Upper bound for the chunk count, from the UI or auto.
pub const MAX_CHUNKS: u32 = 16;

/// Default chunk count for a machine: about one ffmpeg per 10 hardware
/// threads, at most 8 (64 threads → 6).
pub fn auto_chunks(cores: usize) -> u32 {
    ((cores / 10) as u32).clamp(1, 8)
}

/// What a source allows, for deciding the chunk count.
#[derive(Debug, Clone)]
pub struct ChunkSource<'a> {
    pub codec_name: &'a str,
    pub total_frames: Option<u64>,
    pub cfr: bool,
    pub concat: bool,
    pub hwaccel: bool,
}

/// Decide how many chunks to use and say why, for the diagnostics line.
/// `requested`: `None` = auto, `Some(n)` = what the user picked.
pub fn chunk_count(requested: Option<u32>, src: &ChunkSource, cores: usize) -> (u32, String) {
    let auto = requested.is_none();
    let wanted = requested.unwrap_or_else(|| auto_chunks(cores)).clamp(1, MAX_CHUNKS);
    if wanted == 1 {
        let why = if auto { "auto: not enough cores to split" } else { "set to 1" };
        return (1, format!("single process ({why})"));
    }
    let single = |why: String| (1, format!("single process — {why}"));
    let codec = src.codec_name.to_ascii_lowercase();
    if !INTRA_ONLY_CODECS.contains(&codec.as_str()) {
        return single(format!("{} isn't intra-only, so chunks can't start on an exact frame",
                              if codec.is_empty() { "unknown codec" } else { &codec }));
    }
    if src.concat {
        return single("concat / IMF manifests aren't split".into());
    }
    if src.hwaccel {
        return single("hardware decode".into());
    }
    if !src.cfr {
        return single("variable frame rate".into());
    }
    let Some(total) = src.total_frames else {
        return single("unknown length (no frame count or duration)".into());
    };
    let fit = (total / MIN_CHUNK_FRAMES).max(1) as u32;
    let n = wanted.min(fit);
    let how = if auto { "auto" } else { "set" };
    if n == 1 {
        return single(format!("file too short to split ({total} frames)"));
    }
    if n < wanted {
        (n, format!("{n} parallel decodes ({how} {wanted}; file too short for more)"))
    } else {
        (n, format!("{n} parallel decodes ({how})"))
    }
}

/// One chunk of a chunked pass, in frame indices from the start of the file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chunk {
    /// First frame decoded (includes warm-up frames).
    pub decode_start: u64,
    /// First frame kept.
    pub keep_from: u64,
    /// One past the last frame kept (and decoded).
    pub keep_to: u64,
}

impl Chunk {
    pub fn decode_count(&self) -> u64 {
        self.keep_to - self.decode_start
    }
    pub fn warmup(&self) -> u64 {
        self.keep_from - self.decode_start
    }
}

/// Split `total` frames into `n` contiguous chunks of near-equal length.
pub fn plan_chunks(total: u64, n: u32) -> Vec<Chunk> {
    let n = (n.max(1) as u64).min(total.max(1));
    (0..n)
        .map(|i| {
            let keep_from = total * i / n;
            let keep_to = total * (i + 1) / n;
            Chunk {
                decode_start: keep_from.saturating_sub(CHUNK_WARMUP),
                keep_from,
                keep_to,
            }
        })
        .collect()
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

/// How to run one raw stats pass (one ffmpeg process).
#[derive(Debug, Clone, Default)]
pub struct RawRunOptions {
    /// Passed to ffmpeg's `-hwaccel` (callers decide; see `choose_hwaccel`).
    pub hwaccel: Option<String>,
    /// First frame to decode, counted from the start of the file. 0 = no
    /// seek. Anything else seeks with `-ss` before `-i`, which ffmpeg makes
    /// frame-accurate by decoding from the preceding keyframe and dropping
    /// frames before the target; only intra-only CFR sources are chunked, so
    /// that is exact and cheap.
    pub start_frame: u64,
    /// Frame rate (num, den) — needed to turn `start_frame` into a time.
    pub fps: (u32, u32),
    /// `-frames:v`
    pub frame_limit: Option<u64>,
    /// Denominator for progress callbacks; may be an estimate, never used to
    /// stop decoding.
    pub progress_total: Option<u64>,
    /// Stats worker threads (default: all cores).
    pub workers: Option<usize>,
    /// ffmpeg decoder threads (default: ffmpeg's own choice).
    pub decoder_threads: Option<usize>,
    /// Bytes of decoded frames allowed in flight (default 1 GiB).
    pub buffer_bytes: Option<usize>,
}

/// Seek target for `start_frame`: half a frame before its nominal time, so
/// timestamp rounding can't drop the frame itself or let the one before in.
pub fn seek_seconds(start_frame: u64, fps: (u32, u32)) -> f64 {
    let (num, den) = (fps.0.max(1) as f64, fps.1.max(1) as f64);
    (start_frame as f64 - 0.5).max(0.0) * den / num
}

/// Decode `file_path` with ffmpeg and compute per-frame statistics.
///
/// The ffmpeg child is attached to `process_handle` so the app can cancel it
/// or kill it on exit (see `engine::process` for the conventions).
pub fn run_raw_stats(
    file_path: &str,
    fmt: &RawFormat,
    opts: &RawRunOptions,
    progress: Option<ProgressFn>,
    process_handle: crate::engine::analysis::ProcessHandle,
) -> Result<(Vec<FrameStats>, RunReport), RawStatsError> {
    let hwaccel = opts.hwaccel.as_deref();
    let frame_limit = opts.frame_limit;
    let progress_total = opts.progress_total;
    // A zero-sized frame would make the reader "read" empty frames forever
    // without touching the pipe (ffprobe reports 0×0 when it can't work out
    // the stream's parameters).
    if fmt.width == 0 || fmt.height == 0 || fmt.frame_bytes() == 0 {
        return Err(RawStatsError::new(
            format!(
                "Can't analyse this file: ffprobe reported a {}x{} video stream",
                fmt.width, fmt.height
            ),
            0,
        ));
    }

    // ── ffmpeg command ──
    // -loglevel error: stderr is only kept for error reports (progress comes
    // from the collector), so keep the banner and stream dump out of them.
    let mut args: Vec<String> = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
    ];
    if let Some(hw) = hwaccel {
        if !is_concat_manifest(file_path) {
            args.extend(["-hwaccel".into(), hw.to_string()]);
        }
    }
    if let Some(t) = opts.decoder_threads {
        args.extend(["-threads".into(), t.max(1).to_string()]);
    }
    if opts.start_frame > 0 {
        args.extend([
            "-ss".into(),
            format!("{:.6}", seek_seconds(opts.start_frame, opts.fps)),
        ]);
    }
    // Keep the stored orientation: the frame geometry comes from ffprobe's
    // (unrotated) width/height, so auto-rotating would transpose the planes
    // relative to what we think the row width is.
    args.push("-noautorotate".into());
    if is_concat_manifest(file_path) {
        args.extend(["-f".into(), "concat".into(), "-safe".into(), "0".into()]);
    }
    args.extend(["-i".into(), file_path.into()]);
    args.extend(["-map".into(), "0:v:0".into(), "-an".into(), "-sn".into()]);
    args.extend(fps_mode_args());
    if let Some(limit) = frame_limit {
        args.extend(["-frames:v".into(), limit.to_string()]);
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
    // Larger pipe buffer: a UHD 10-bit frame is 33 MB, i.e. ~500 handoffs
    // between ffmpeg and us through the default 64 KB pipe. 1 MB (the usual
    // unprivileged maximum) measured ~7% faster for UHD raw output. Linux
    // only; elsewhere the default stays.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let fd = stdout.as_raw_fd();
        for size in [1 << 20, 256 << 10] {
            if unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, size as libc::c_int) } > 0 {
                break;
            }
        }
    }

    let Some(child_id) = process_handle.attach(child) else {
        return Err(RawStatsError {
            message: "Analysis cancelled".into(),
            frames_read: 0,
            killed: true,
        });
    };

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
    let n_workers = opts
        .workers
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
        .max(1);
    // Frames in flight: enough to keep every worker busy plus a little
    // read-ahead, bounded by the memory budget. Each job also pins its
    // predecessor, so the pool needs one extra.
    let budget = opts.buffer_bytes.unwrap_or(IN_FLIGHT_BYTES);
    let max_buffers = (budget / frame_bytes.max(1)).clamp(3, (2 * n_workers + 2).max(3));

    let (job_tx, job_rx) = mpsc::sync_channel::<Job>(max_buffers);
    let job_rx = Arc::new(Mutex::new(job_rx));
    let (res_tx, res_rx) = mpsc::channel::<WorkerOut>();
    let (pool_tx, pool_rx) = mpsc::channel::<Vec<u8>>();

    let busy_ns = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut workers = Vec::with_capacity(n_workers);
    for _ in 0..n_workers {
        let busy_ns = busy_ns.clone();
        let rx = job_rx.clone();
        let tx = res_tx.clone();
        let fmt = fmt.clone();
        workers.push(std::thread::spawn(move || loop {
            let job = match rx.lock().unwrap().recv() {
                Ok(j) => j,
                Err(_) => break,
            };
            let prev = job.prev.as_ref().map(|p| p.data.as_slice());
            #[cfg(test)]
            if job.index == PANIC_AT_FRAME.load(std::sync::atomic::Ordering::Relaxed) {
                panic!("injected worker failure at frame {}", job.index);
            }
            let t0 = Instant::now();
            let (stats, thumb) = compute_frame(&fmt, &job.frame.data, prev);
            busy_ns.fetch_add(t0.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
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

                if let (Some(total), Some(cb)) = (progress_total, progress.as_ref()) {
                    if last_progress.elapsed().as_millis() >= 250 {
                        last_progress = Instant::now();
                        // An estimated total can be exceeded; never report >100%.
                        cb(next, total.max(next));
                    }
                }
            }
        }
        if let (Some(_), Some(cb)) = (progress_total, progress.as_ref()) {
            cb(next, next);
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
    let report = RunReport {
        frames: index,
        elapsed_s: t_start.elapsed().as_secs_f64(),
        wait_ffmpeg_s: t_read,
        wait_buffer_s: t_pool,
        workers: n_workers,
        worker_busy: busy_ns.load(std::sync::atomic::Ordering::Relaxed) as f64
            / 1e9
            / (t_start.elapsed().as_secs_f64() * n_workers as f64).max(1e-9),
    };
    if profile {
        eprintln!(
            "[vcompare] rawstats reader: {index} frames, {:.2}s total — waiting for buffer {:.2}s, reading pipe {:.2}s, sending jobs {:.2}s (workers={n_workers}, buffers={max_buffers})",
            t_start.elapsed().as_secs_f64(), t_pool, t_read, t_send
        );
    }
    drop(prev);
    drop(job_tx);

    // If the reader stopped before EOF (read error, worker pool gone),
    // ffmpeg may still be blocked writing to the pipe: kill it and close our
    // end, or the wait() below would block forever. The child stays in the
    // handle so this isn't mistaken for an external kill.
    if read_err.is_some() {
        process_handle.kill_attached(child_id);
    }
    drop(reader);

    let frames_read = index;
    let collected = collector.join();
    let mut worker_failed = false;
    for w in workers {
        worker_failed |= w.join().is_err();
    }
    // A failed worker loses its frame; the collector then stops at that
    // index and would return a silently truncated list.
    let mut out = match collected {
        Ok(out) if !worker_failed && out.len() as u64 == frames_read => out,
        Ok(out) => {
            process_handle.kill_attached(child_id);
            if read_err.is_none() {
                read_err = Some(format!(
                    "stats worker failed: computed {} of {} frames",
                    out.len(),
                    frames_read
                ));
            }
            Vec::new()
        }
        Err(_) => {
            process_handle.kill_attached(child_id);
            if read_err.is_none() {
                read_err = Some("stats collector thread panicked".into());
            }
            Vec::new()
        }
    };

    // ── ffmpeg exit status ──
    let mut child_opt = process_handle.detach(child_id);
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
    eprintln!("[vcompare] native stats: {}", report.summary());
    Ok((out, report))
}

/// Emit exactly the decoded frames: no duplication/dropping to a fixed
/// rate. (`-fps_mode` arrived in ffmpeg 5.1; older builds only know `-vsync`.)
pub fn fps_mode_args() -> [String; 2] {
    if ffmpeg_has_fps_mode() {
        ["-fps_mode".into(), "passthrough".into()]
    } else {
        ["-vsync".into(), "passthrough".into()]
    }
}

/// Whether this ffmpeg understands `-fps_mode` (5.1+). Checked once.
fn ffmpeg_has_fps_mode() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-h", "long"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("-fps_mode"))
            // If we can't tell, assume a current ffmpeg.
            .unwrap_or(true)
    })
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
        assert_eq!(f.legal, None);
        // Full-range sources pass through and don't count legal range
        let f = fmt("yuvj420p", 64, 36);
        assert_eq!(f.pix_fmt, "yuvj420p");
        assert_eq!(f.legal, None);
        let f = fmt("yuv422p10le", 64, 36);
        assert_eq!(f.legal, Some(LegalRange { y_lo: 64, y_hi: 940, c_lo: 64, c_hi: 960 }));
        assert_eq!(f.clone().with_full_range(true).legal, None);
        assert_eq!(fmt("yuv420p", 2, 2).legal.unwrap().y_hi, 235);
    }

    #[test]
    fn percentiles_and_out_of_range() {
        // 10-bit 4:4:4, 100 luma samples 0..99 → shifted into a known range
        let f = fmt("yuv444p10le", 10, 10);
        let mut a = vec![0u8; f.frame_bytes()];
        let ry = f.plane_range(0);
        for (i, c) in a[ry.clone()].chunks_exact_mut(2).enumerate() {
            // values 30, 40, ..., 1020 step 10 → 10 below 64? (30,40,50,60 = 4)
            let v = 30 + 10 * i as u16;
            c.copy_from_slice(&v.min(1023).to_le_bytes());
        }
        // Chroma: all legal except 3 U samples at 1000 and 2 V samples at 10
        fill_u16(&mut a, f.plane_range(1), 512);
        fill_u16(&mut a, f.plane_range(2), 512);
        let ru = f.plane_range(1);
        for k in 0..3 {
            a[ru.start + 2 * k..ru.start + 2 * k + 2].copy_from_slice(&1000u16.to_le_bytes());
        }
        let rv = f.plane_range(2);
        for k in 0..2 {
            a[rv.start + 2 * k..rv.start + 2 * k + 2].copy_from_slice(&10u16.to_le_bytes());
        }

        let (s, _) = compute_frame(&f, &a, None);
        assert_eq!((s.y_min, s.y_max), (30, 1020));
        // n=100: 10th percentile = 10th smallest (40+..) = 120, 90th = 920
        assert_eq!(s.y_low, 120);
        assert_eq!(s.y_high, 920);
        let o = s.oor.unwrap();
        assert_eq!(o.y_lo, 4); // 30, 40, 50, 60
        assert_eq!(o.y_hi, 8); // 950 ..= 1020
        assert_eq!(o.c, 5);
        assert!((s.y_avg - (30.0 + 1020.0) / 2.0).abs() < 1e-9);

        let full = f.clone().with_full_range(true);
        assert_eq!(compute_frame(&full, &a, None).0.oor, None);
    }

    #[test]
    fn scan_and_histogram_agree() {
        // The chroma scan and the luma histogram must give identical
        // min/max/sum/counts on the same data.
        for (two, data) in [
            (true, (0..2000u32).flat_map(|i| (((i * 37) % 1024) as u16).to_le_bytes()).collect::<Vec<u8>>()),
            (false, (0..2001u32).map(|i| ((i * 37) % 256) as u8).collect::<Vec<u8>>()),
        ] {
            let bins = if two { 1024 } else { 256 };
            let n = if two { data.len() / 2 } else { data.len() } as u64;
            let b = Some(if two { (64, 960) } else { (16, 240) });
            let h = summarize(&plane_histogram(&data, two, bins), n, b);
            let sc = plane_scan(&data, two, b);
            assert_eq!((h.min, h.max, h.sum, h.below, h.above), (sc.min, sc.max, sc.sum, sc.below, sc.above));
            let none = plane_scan(&data, two, None);
            assert_eq!((none.below, none.above), (0, 0));
        }
    }

    #[test]
    fn histogram_clamps_out_of_depth_values() {
        // A 10-bit buffer holding a stray 16-bit value must not index past the end.
        let f = fmt("yuv420p10le", 4, 2);
        let mut a = vec![0u8; f.frame_bytes()];
        let ry = f.plane_range(0);
        a[ry.start..ry.start + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        let (s, _) = compute_frame(&f, &a, None);
        assert_eq!(s.y_max, 1023);
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
    fn chunk_plan_is_contiguous_with_warmup() {
        let c = plan_chunks(1000, 3);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], Chunk { decode_start: 0, keep_from: 0, keep_to: 333 });
        assert_eq!(c[1], Chunk { decode_start: 329, keep_from: 333, keep_to: 666 });
        assert_eq!(c[2], Chunk { decode_start: 662, keep_from: 666, keep_to: 1000 });
        for w in c.windows(2) {
            assert_eq!(w[0].keep_to, w[1].keep_from, "no gap or overlap in kept frames");
        }
        assert_eq!(c.iter().map(|k| k.keep_to - k.keep_from).sum::<u64>(), 1000);
        assert_eq!(c[1].warmup(), CHUNK_WARMUP);
        assert_eq!(plan_chunks(5, 8).len(), 5);
    }

    #[test]
    fn chunk_count_rules() {
        let prores = ChunkSource { codec_name: "prores", total_frames: Some(24 * 3600), cfr: true, concat: false, hwaccel: false };
        assert_eq!(chunk_count(None, &prores, 64).0, 6);
        assert_eq!(chunk_count(None, &prores, 8).0, 1);
        assert_eq!(chunk_count(Some(3), &prores, 8).0, 3, "user choice wins over cores");
        assert_eq!(chunk_count(Some(1), &prores, 64).0, 1);
        assert_eq!(chunk_count(Some(99), &prores, 64).0, MAX_CHUNKS);
        let short = ChunkSource { total_frames: Some(600), ..prores.clone() };
        assert_eq!(chunk_count(Some(8), &short, 64).0, 2, "≥240 frames per chunk");
        for (src, why) in [
            (ChunkSource { codec_name: "hevc", ..prores.clone() }, "intra-only"),
            (ChunkSource { cfr: false, ..prores.clone() }, "variable"),
            (ChunkSource { total_frames: None, ..prores.clone() }, "unknown length"),
            (ChunkSource { concat: true, ..prores.clone() }, "concat"),
        ] {
            let (n, reason) = chunk_count(Some(6), &src, 64);
            assert_eq!(n, 1);
            assert!(reason.contains(why), "{reason}");
        }
        assert!((seek_seconds(48, (24, 1)) - 47.5 / 24.0).abs() < 1e-12);
        assert_eq!(seek_seconds(0, (24, 1)), 0.0);
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
    fn zero_sized_stream_is_rejected_before_spawning() {
        let f = fmt("yuv420p", 0, 0);
        let t = std::time::Instant::now();
        let r = run_raw_stats(
            "/nonexistent.mov", &f, &RawRunOptions::default(), None,
            crate::engine::process::standalone_job(),
        );
        let e = r.unwrap_err();
        assert!(e.message.contains("0x0"), "{}", e.message);
        assert!(t.elapsed().as_secs() < 1);
    }

    /// A worker dying mid-run must produce a prompt error — not a hang
    /// waiting on an ffmpeg blocked on a full pipe, and not a silently
    /// truncated Ok. Needs ffmpeg on PATH; skipped otherwise.
    #[test]
    fn worker_failure_fails_cleanly_and_promptly() {
        let dir = std::env::temp_dir().join(format!("vcompare_rawstats_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("t.mkv");
        let made = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi",
                   "-i", "testsrc2=s=640x360:r=24", "-frames:v", "400",
                   "-c:v", "ffv1", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            eprintln!("ffmpeg not available; skipping");
            return;
        }
        let f = fmt("yuv420p", 640, 360);
        let clip_s = clip.to_string_lossy().to_string();

        // Sanity: the clean run returns every frame.
        let (ok, report) = run_raw_stats(&clip_s, &f, &RawRunOptions::default(), None,
                               crate::engine::process::standalone_job()).unwrap();
        assert_eq!(ok.len(), 400);
        assert_eq!(report.frames, 400);
        assert!(report.fps() > 0.0 && report.worker_busy > 0.0 && report.worker_busy <= 1.0);

        PANIC_AT_FRAME.store(5, std::sync::atomic::Ordering::Relaxed);
        let t = std::time::Instant::now();
        let r = run_raw_stats(&clip_s, &f, &RawRunOptions::default(), None,
                              crate::engine::process::standalone_job());
        PANIC_AT_FRAME.store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&dir);

        let e = r.expect_err("a worker failure must not return Ok");
        assert!(!e.killed);
        assert!(t.elapsed().as_secs() < 10, "took {:?}", t.elapsed());
        eprintln!("worker failure reported as: {}", e.message);
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
