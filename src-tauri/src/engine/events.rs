//! Event detection — finds flash frames in per-frame analysis data.
//!
//! An *event* is something that spans one or more frames, as opposed to the
//! per-frame scalar metrics the rest of the engine deals in. Detection is a
//! cheap linear scan over data that is already in memory, so it can be
//! re-run with different parameters without touching ffmpeg.
//!
//! A flash frame (post-production sense) is a run of 1..=`max_run` frames
//! that differs sharply from the frame before it *and* the frame after it.
//! Two flavours are reported:
//!
//! - **return** (`at_cut == false`): the frames on either side of the run
//!   agree with each other — the picture flashes and comes back. This is
//!   the classic stray frame inside a shot.
//! - **at cut** (`at_cut == true`): the frames on either side differ — a
//!   very short insert sitting on a cut. Usually also a mistake, but a
//!   fast montage will trigger it too, so the UI can filter these out.
//!
//! Two inputs are supported:
//!
//! - `levels` analyses, using the per-frame temporal differences
//!   (`y_dif`, `u_dif`, `v_dif`) to find runs. Whether the frames either
//!   side of a run agree is judged from the native engine's thumbnail
//!   differences (`tdif_1..4`) when present; older signalstats-era caches
//!   fall back to comparing average luma and saturation / chroma.
//! - `diff` analyses, using the `diff_y` series: a flash present in the
//!   distorted file but not the reference is an isolated spike in the
//!   difference, well above its neighbours.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A detected multi-frame event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    /// First frame of the run (display frame number, same space as `frames[].frame`).
    pub frame: i64,
    /// Timecode of the first frame.
    pub tc: String,
    /// Run length in frames (1..=max_run).
    pub length: u32,
    /// Event family. Currently always "flash".
    pub kind: String,
    /// "black" | "white" | "content"
    pub subkind: String,
    /// True when the frames on either side of the run differ from each
    /// other (the run sits on a cut) rather than agreeing (flash-and-return).
    pub at_cut: bool,
    /// Detection strength relative to the threshold (>= 1.0). Higher = more
    /// obvious.
    pub score: f64,
}

/// Detector parameters, all in normalized 0..1 units of the signal peak.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectParams {
    /// Minimum frame-to-frame change to count as "sharp". `None` = derive
    /// from the data (robust mean + `auto_k` sigma, floored at `min_threshold`).
    #[serde(default)]
    pub threshold: Option<f64>,
    /// Longest run that still counts as a flash. Default 3.
    #[serde(default = "default_max_run")]
    pub max_run: u32,
    /// Floor for the auto threshold.
    #[serde(default = "default_min_threshold")]
    pub min_threshold: f64,
    /// Sigma multiplier for the auto threshold.
    #[serde(default = "default_auto_k")]
    pub auto_k: f64,
    /// How close the two neighbours must be to count as "agreeing", i.e. a
    /// flash-and-return rather than at-cut. Compared against the thumbnail
    /// difference between them when available (mean |Δ| over a 32×18 luma
    /// thumbnail, 0..1), otherwise against their average-luma difference.
    #[serde(default = "default_neighbour_tol")]
    pub neighbour_tol: f64,
    /// Drop at-cut events from the result.
    #[serde(default)]
    pub exclude_at_cut: bool,
}

fn default_max_run() -> u32 {
    3
}
fn default_min_threshold() -> f64 {
    0.04
}
fn default_auto_k() -> f64 {
    6.0
}
fn default_neighbour_tol() -> f64 {
    0.06
}

impl Default for DetectParams {
    fn default() -> Self {
        Self {
            threshold: None,
            max_run: default_max_run(),
            min_threshold: default_min_threshold(),
            auto_k: default_auto_k(),
            neighbour_tol: default_neighbour_tol(),
            exclude_at_cut: false,
        }
    }
}

/// What the detector returns to the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectResult {
    pub events: Vec<Event>,
    /// The threshold actually used (auto-derived or as given).
    pub threshold: f64,
    /// Which per-frame series the detector ran on ("levels" or "diff_y").
    pub source: String,
}

// ─── Public entry points ────────────────────────────────────────────────────

/// Run the detector appropriate for an analysis mode.
///
/// Returns `Err` when the loaded data doesn't carry what the detector needs
/// (e.g. an old levels cache without `y_dif`).
pub fn detect_flashes(
    mode: &str,
    frames: &[Value],
    bit_depth: u32,
    params: &DetectParams,
) -> Result<DetectResult, String> {
    match mode {
        "levels" => detect_flashes_levels(frames, bit_depth, params),
        "diff" => detect_flashes_diff(frames, bit_depth, params),
        other => Err(format!("Flash detection is not available for {other} mode")),
    }
}

/// Flash detection over a `levels` analysis.
pub fn detect_flashes_levels(
    frames: &[Value],
    bit_depth: u32,
    params: &DetectParams,
) -> Result<DetectResult, String> {
    let peak = ((1u64 << bit_depth) - 1) as f64;
    let n = frames.len();

    let has_temporal = frames.iter().any(|f| num(f, "y_dif").is_some());
    if !has_temporal {
        return Err(
            "This levels analysis predates temporal stats — re-run the analysis to enable flash detection"
                .into(),
        );
    }

    // Per-frame "how different from the previous frame" signal.
    // Chroma gets a little less weight: on 4:2:0 it's a smaller plane and
    // chroma noise in grainy footage is proportionally larger.
    let d: Vec<f64> = frames
        .iter()
        .map(|f| {
            let y = num(f, "y_dif").unwrap_or(0.0) / peak;
            let u = num(f, "u_dif").unwrap_or(0.0) / peak;
            let v = num(f, "v_dif").unwrap_or(0.0) / peak;
            y.max(0.75 * u.max(v))
        })
        .collect();

    let threshold = resolve_threshold(&d, params);

    let y_avg: Vec<f64> = frames.iter().map(|f| num(f, "y_avg").unwrap_or(0.0) / peak).collect();
    // Chroma "colour" proxy for the fallback neighbour test: signalstats-era
    // caches have sat_avg; native ones have u_avg / v_avg.
    let chroma: Vec<(f64, f64)> = frames
        .iter()
        .map(|f| match (num(f, "u_avg"), num(f, "v_avg")) {
            (Some(u), Some(v)) => (u / peak, v / peak),
            _ => (num(f, "sat_avg").unwrap_or(0.0) / peak, 0.0),
        })
        .collect();
    let y_min: Vec<f64> = frames.iter().map(|f| num(f, "y_min").unwrap_or(0.0) / peak).collect();
    let y_max: Vec<f64> = frames.iter().map(|f| num(f, "y_max").unwrap_or(0.0) / peak).collect();

    let mut events = Vec::new();
    let max_run = params.max_run.max(1) as usize;

    // d[i] is the change going INTO frame i. A run [a..=b] is bounded by
    // d[a] (enter) and d[b+1] (leave). Frame 0 has no "enter" and the last
    // frame has no "leave", so runs touching either edge are not flashes.
    let mut a = 1usize;
    while a < n {
        if d[a] <= threshold {
            a += 1;
            continue;
        }
        let mut matched: Option<usize> = None;
        for len in 1..=max_run {
            let b = a + len - 1;
            if b + 1 >= n {
                break;
            }
            // Inside the run the picture should hold still-ish; if another
            // big change occurs before we leave, this isn't one flash.
            if len > 1 && d[b] > threshold {
                break;
            }
            if d[b + 1] > threshold {
                matched = Some(len);
                break;
            }
        }

        match matched {
            Some(len) => {
                let b = a + len - 1;
                let enter = d[a];
                let leave = d[b + 1];
                let score = enter.min(leave) / threshold;

                // Do the neighbours agree? Frames a-1 and b+1 are len+1
                // apart; the native engine records the thumbnail difference
                // up to TDIF_DEPTH frames back, which compares the actual
                // pictures. Otherwise compare average luma and chroma.
                let at_cut = match tdif(&frames[b + 1], len + 1) {
                    Some(t) => t > params.neighbour_tol,
                    None => {
                        let dy = (y_avg[a - 1] - y_avg[b + 1]).abs();
                        let (c0, c1) = (chroma[a - 1], chroma[b + 1]);
                        let dc = (c0.0 - c1.0).abs().max((c0.1 - c1.1).abs());
                        dy > params.neighbour_tol || dc > params.neighbour_tol * 1.5
                    }
                };

                let run_ymax = y_max[a..=b].iter().cloned().fold(f64::MIN, f64::max);
                let run_ymin = y_min[a..=b].iter().cloned().fold(f64::MAX, f64::min);
                let subkind = classify_run(run_ymin, run_ymax);

                if !(at_cut && params.exclude_at_cut) {
                    events.push(Event {
                        frame: frame_num(&frames[a]),
                        tc: frame_tc(&frames[a]),
                        length: len as u32,
                        kind: "flash".into(),
                        subkind,
                        at_cut,
                        score: round3(score),
                    });
                }
                // Resume at the frame after the run. d[b+1] was the "leave"
                // edge of this run — it must not be reused as the "enter"
                // edge of the next one, so skip past it.
                a = b + 2;
            }
            None => {
                // A sharp change with no matching exit within max_run: a cut,
                // or the start of something longer. Move on.
                a += 1;
            }
        }
    }

    Ok(DetectResult {
        events,
        threshold: round4(threshold),
        source: "levels".into(),
    })
}

/// Flash detection over a `diff` analysis (`diff_y` series).
///
/// A flash in the distorted file only shows up as a run of `diff_y` values
/// standing well above both neighbours. Because an encode may have a
/// non-zero baseline difference everywhere, the test is on the *height
/// above the neighbours*, not the absolute value.
pub fn detect_flashes_diff(
    frames: &[Value],
    bit_depth: u32,
    params: &DetectParams,
) -> Result<DetectResult, String> {
    let peak = ((1u64 << bit_depth) - 1) as f64;
    let n = frames.len();

    let v: Vec<f64> = frames.iter().map(|f| num(f, "diff_y").unwrap_or(0.0) / peak).collect();
    if v.is_empty() {
        return Err("No diff_y data loaded".into());
    }

    // Derive the threshold from the first difference of the series so the
    // auto estimate reflects frame-to-frame noise, not the baseline level.
    let d: Vec<f64> = std::iter::once(0.0)
        .chain(v.windows(2).map(|w| (w[1] - w[0]).abs()))
        .collect();
    let threshold = resolve_threshold(&d, params);

    let mut events = Vec::new();
    let max_run = params.max_run.max(1) as usize;

    let mut a = 1usize;
    while a < n {
        let mut matched: Option<usize> = None;
        for len in 1..=max_run {
            let b = a + len - 1;
            if b + 1 >= n {
                break;
            }
            let run_min = v[a..=b].iter().cloned().fold(f64::MAX, f64::min);
            let neighbours = v[a - 1].max(v[b + 1]);
            if run_min - neighbours > threshold {
                matched = Some(len);
                break;
            }
            // If the run's low point is already not above the left
            // neighbour there's no point extending it.
            if run_min - v[a - 1] <= threshold {
                break;
            }
        }

        match matched {
            Some(len) => {
                let b = a + len - 1;
                let run_min = v[a..=b].iter().cloned().fold(f64::MAX, f64::min);
                let neighbours = v[a - 1].max(v[b + 1]);
                let score = (run_min - neighbours) / threshold;
                events.push(Event {
                    frame: frame_num(&frames[a]),
                    tc: frame_tc(&frames[a]),
                    length: len as u32,
                    kind: "flash".into(),
                    // We only know the files differ here; the picture
                    // content is not available in diff mode.
                    subkind: "content".into(),
                    at_cut: false,
                    score: round3(score),
                });
                a = b + 1;
            }
            None => a += 1,
        }
    }

    Ok(DetectResult {
        events,
        threshold: round4(threshold),
        source: "diff_y".into(),
    })
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Pick the threshold: explicit, or robust auto from the change series.
///
/// Auto = median + k · (1.4826 · MAD), floored at `min_threshold`. The
/// median/MAD are robust to the handful of cuts and flashes we are looking
/// for, so they measure the "ordinary" frame-to-frame change of the footage.
fn resolve_threshold(d: &[f64], params: &DetectParams) -> f64 {
    if let Some(t) = params.threshold {
        return t.max(1e-6);
    }
    if d.len() < 3 {
        return params.min_threshold;
    }
    let mut sorted: Vec<f64> = d[1..].to_vec(); // d[0] is always 0
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];
    let mut dev: Vec<f64> = sorted.iter().map(|x| (x - median).abs()).collect();
    dev.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mad = dev[dev.len() / 2];
    let sigma = 1.4826 * mad;
    (median + params.auto_k * sigma).max(params.min_threshold)
}

/// Black / white / content from the run's normalized luma extremes.
///
/// Black: even the brightest pixel is near black (a 16/235 legal black in
/// 8-bit is 0.063). White: even the darkest pixel is near white.
fn classify_run(run_ymin: f64, run_ymax: f64) -> String {
    if run_ymax < 0.12 {
        "black".into()
    } else if run_ymin > 0.85 {
        "white".into()
    } else {
        "content".into()
    }
}

/// Thumbnail difference of this frame against the one `back` frames
/// earlier, if the analysis recorded it.
fn tdif(f: &Value, back: usize) -> Option<f64> {
    match back {
        1 => num(f, "tdif_1"),
        2 => num(f, "tdif_2"),
        3 => num(f, "tdif_3"),
        4 => num(f, "tdif_4"),
        _ => None,
    }
}

fn num(f: &Value, key: &str) -> Option<f64> {
    f.get(key).and_then(|v| v.as_f64())
}

fn frame_num(f: &Value) -> i64 {
    f.get("frame").and_then(|v| v.as_i64()).unwrap_or(0)
}

fn frame_tc(f: &Value) -> String {
    f.get("tc").and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build 8-bit levels frames from a luma-average sequence, computing
    /// y_dif as |Δ y_avg| plus a little noise so the auto threshold has
    /// something to measure. Chroma is held flat.
    fn levels_frames(y_avgs: &[f64], noise: f64) -> Vec<Value> {
        let mut out = Vec::new();
        for (i, &ya) in y_avgs.iter().enumerate() {
            let prev = if i == 0 { ya } else { y_avgs[i - 1] };
            let dif = (ya - prev).abs() + noise * ((i % 3) as f64);
            // Flat-ish frame: min/max tight around the average, except
            // true black/white frames which pin their extremes.
            let (ymin, ymax) = if ya <= 16.0 {
                (16.0, 20.0)
            } else if ya >= 235.0 {
                (230.0, 235.0)
            } else {
                (ya - 40.0, ya + 40.0)
            };
            out.push(json!({
                "frame": i as i64 + 1000,
                "tc": format!("00:00:{:02}:{:02}", i / 24, i % 24),
                "y_min": ymin, "y_max": ymax,
                "u_min": 120.0, "u_max": 136.0, "v_min": 120.0, "v_max": 136.0,
                "y_avg": ya, "y_dif": dif, "u_dif": 0.0, "v_dif": 0.0,
                "sat_avg": 10.0,
            }));
        }
        out
    }

    /// Same frames, plus native-engine thumbnail differences. For the flat
    /// synthetic frames above the thumbnail difference is just |Δ y_avg|.
    fn levels_frames_tdif(y_avgs: &[f64], noise: f64) -> Vec<Value> {
        let mut out = levels_frames(y_avgs, noise);
        for i in 0..out.len() {
            let obj = out[i].as_object_mut().unwrap();
            obj.remove("sat_avg");
            obj.insert("u_avg".into(), json!(128.0));
            obj.insert("v_avg".into(), json!(128.0));
            for k in 1..=4usize {
                let t = if i >= k { (y_avgs[i] - y_avgs[i - k]).abs() / 255.0 } else { 0.0 };
                obj.insert(format!("tdif_{k}"), json!(t));
            }
        }
        out
    }

    fn shot(level: f64, len: usize) -> Vec<f64> {
        vec![level; len]
    }

    #[test]
    fn native_frames_match_proxy_on_simple_cases() {
        // The main scenarios give the same answer through either neighbour test.
        let cases: Vec<Vec<f64>> = vec![
            { let mut s = shot(100.0, 30); s[15] = 235.0; s.extend(shot(100.0, 10)); s },
            { let mut s = shot(120.0, 20); s.extend([16.0, 16.0]); s.extend(shot(120.0, 20)); s },
            { let mut s = shot(80.0, 25); s.push(235.0); s.extend(shot(160.0, 25)); s },
            { let mut s = shot(80.0, 25); s.extend(shot(160.0, 25)); s },
        ];
        for seq in cases {
            let p = DetectParams::default();
            let a = detect_flashes_levels(&levels_frames(&seq, 0.5), 8, &p).unwrap().events;
            let b = detect_flashes_levels(&levels_frames_tdif(&seq, 0.5), 8, &p).unwrap().events;
            assert_eq!(a, b);
        }
    }

    #[test]
    fn thumbnail_test_catches_cut_between_equal_brightness_shots() {
        // Shot A and shot B have the same average luma, so the proxy thinks
        // the picture came back; the thumbnails know it didn't.
        let mut seq = shot(100.0, 20);
        seq.push(235.0);
        seq.extend(shot(100.0, 20));
        let mut frames = levels_frames_tdif(&seq, 0.5);
        // Frame 21 (first of shot B) vs frame 19 (last of shot A): different picture.
        frames[21]["tdif_2"] = json!(0.30);
        let r = detect_flashes_levels(&frames, 8, &DetectParams::default()).unwrap();
        assert_eq!(r.events.len(), 1);
        assert!(r.events[0].at_cut, "thumbnail difference should mark this at-cut");

        let proxy = detect_flashes_levels(&levels_frames(&seq, 0.5), 8, &DetectParams::default()).unwrap();
        assert!(!proxy.events[0].at_cut, "proxy cannot see this cut");
    }

    #[test]
    fn long_runs_fall_back_to_chroma_proxy() {
        // A 4-frame run needs tdif_5, which isn't recorded: use the proxy.
        // Neighbours have equal luma but different chroma → at cut.
        let mut seq = shot(100.0, 20);
        seq.extend(shot(230.0, 4));
        seq.extend(shot(100.0, 20));
        let mut frames = levels_frames_tdif(&seq, 0.5);
        for f in frames.iter_mut().skip(24) {
            f["u_avg"] = json!(170.0);
        }
        let p = DetectParams { max_run: 4, ..Default::default() };
        let r = detect_flashes_levels(&frames, 8, &p).unwrap();
        assert_eq!(r.events.len(), 1, "{:?}", r.events);
        assert_eq!(r.events[0].length, 4);
        assert!(r.events[0].at_cut);
    }

    #[test]
    fn single_white_flash_inside_shot() {
        let mut seq = shot(100.0, 30);
        seq[15] = 235.0;
        seq.extend(shot(100.0, 10));
        let frames = levels_frames(&seq, 0.5);
        let r = detect_flashes_levels(&frames, 8, &DetectParams::default()).unwrap();
        assert_eq!(r.events.len(), 1, "{:?}", r.events);
        let e = &r.events[0];
        assert_eq!(e.frame, 1015);
        assert_eq!(e.length, 1);
        assert_eq!(e.subkind, "white");
        assert!(!e.at_cut);
        assert!(e.score > 1.0);
    }

    #[test]
    fn two_frame_black_flash() {
        let mut seq = shot(120.0, 20);
        seq.extend([16.0, 16.0]);
        seq.extend(shot(120.0, 20));
        let frames = levels_frames(&seq, 0.5);
        let r = detect_flashes_levels(&frames, 8, &DetectParams::default()).unwrap();
        assert_eq!(r.events.len(), 1, "{:?}", r.events);
        assert_eq!(r.events[0].frame, 1020);
        assert_eq!(r.events[0].length, 2);
        assert_eq!(r.events[0].subkind, "black");
        assert!(!r.events[0].at_cut);
    }

    #[test]
    fn hard_cut_is_not_a_flash() {
        let mut seq = shot(80.0, 25);
        seq.extend(shot(160.0, 25));
        let frames = levels_frames(&seq, 0.5);
        let r = detect_flashes_levels(&frames, 8, &DetectParams::default()).unwrap();
        assert!(r.events.is_empty(), "{:?}", r.events);
    }

    #[test]
    fn flash_at_cut_is_flagged_at_cut() {
        let mut seq = shot(80.0, 25);
        seq.push(235.0);
        seq.extend(shot(160.0, 25));
        let frames = levels_frames(&seq, 0.5);
        let r = detect_flashes_levels(&frames, 8, &DetectParams::default()).unwrap();
        assert_eq!(r.events.len(), 1, "{:?}", r.events);
        assert!(r.events[0].at_cut);
        assert_eq!(r.events[0].subkind, "white");

        let p = DetectParams { exclude_at_cut: true, ..Default::default() };
        let r = detect_flashes_levels(&frames, 8, &p).unwrap();
        assert!(r.events.is_empty());
    }

    #[test]
    fn insert_longer_than_max_run_is_ignored() {
        let mut seq = shot(100.0, 20);
        seq.extend(shot(220.0, 4));
        seq.extend(shot(100.0, 20));
        let frames = levels_frames(&seq, 0.5);
        let r = detect_flashes_levels(&frames, 8, &DetectParams::default()).unwrap();
        assert!(r.events.is_empty(), "{:?}", r.events);

        let p = DetectParams { max_run: 4, ..Default::default() };
        let r = detect_flashes_levels(&frames, 8, &p).unwrap();
        assert_eq!(r.events.len(), 1);
        assert_eq!(r.events[0].length, 4);
    }

    #[test]
    fn grain_noise_below_threshold_is_quiet() {
        // Noisy but steady shot: y_dif wobbles up to ~3 codes.
        let seq = shot(100.0, 200);
        let frames = levels_frames(&seq, 1.5);
        let r = detect_flashes_levels(&frames, 8, &DetectParams::default()).unwrap();
        assert!(r.events.is_empty(), "{:?}", r.events);
    }

    #[test]
    fn flashes_on_consecutive_runs_do_not_share_edges() {
        // flash, 1 normal frame, flash — the shared normal frame's edges
        // must not be double-counted.
        let mut seq = shot(100.0, 10);
        seq.extend([235.0, 100.0, 16.0]);
        seq.extend(shot(100.0, 10));
        let frames = levels_frames(&seq, 0.5);
        let r = detect_flashes_levels(&frames, 8, &DetectParams::default()).unwrap();
        assert_eq!(r.events.len(), 2, "{:?}", r.events);
        assert_eq!(r.events[0].frame, 1010);
        assert_eq!(r.events[1].frame, 1012);
    }

    #[test]
    fn old_cache_without_temporal_stats_errors() {
        let frames = vec![json!({"frame": 0, "tc": "00:00:00:00", "y_min": 16.0, "y_max": 235.0})];
        assert!(detect_flashes_levels(&frames, 8, &DetectParams::default()).is_err());
    }

    #[test]
    fn ten_bit_normalization() {
        let mut seq: Vec<f64> = vec![400.0; 30];
        seq[12] = 940.0;
        let mut frames = levels_frames(&seq, 2.0);
        // levels_frames pins extremes for 8-bit; fix up the white frame for 10-bit
        frames[12]["y_min"] = json!(920.0);
        frames[12]["y_max"] = json!(940.0);
        let r = detect_flashes_levels(&frames, 10, &DetectParams::default()).unwrap();
        assert_eq!(r.events.len(), 1, "{:?}", r.events);
        assert_eq!(r.events[0].subkind, "white");
    }

    fn diff_frames(vals: &[f64]) -> Vec<Value> {
        vals.iter()
            .enumerate()
            .map(|(i, &v)| json!({"frame": i as i64, "tc": "", "diff_y": v}))
            .collect()
    }

    #[test]
    fn diff_isolated_spike_over_baseline() {
        // Baseline diff of ~2 codes everywhere (a re-encode), one frame at 90.
        let mut vals = vec![2.0; 60];
        vals[30] = 90.0;
        vals[31] = 2.3;
        let r = detect_flashes_diff(&diff_frames(&vals), 8, &DetectParams::default()).unwrap();
        assert_eq!(r.events.len(), 1, "{:?}", r.events);
        assert_eq!(r.events[0].frame, 30);
        assert_eq!(r.events[0].length, 1);
    }

    #[test]
    fn diff_two_frame_spike_and_step_change() {
        let mut vals = vec![1.0; 40];
        vals[10] = 60.0;
        vals[11] = 55.0;
        // A step change (difference starts and stays) is not a flash.
        for v in vals.iter_mut().skip(25) {
            *v = 40.0;
        }
        let r = detect_flashes_diff(&diff_frames(&vals), 8, &DetectParams::default()).unwrap();
        assert_eq!(r.events.len(), 1, "{:?}", r.events);
        assert_eq!(r.events[0].frame, 10);
        assert_eq!(r.events[0].length, 2);
    }

    #[test]
    fn explicit_threshold_is_respected() {
        let mut seq = shot(100.0, 30);
        seq[15] = 115.0; // ~0.06 normalized — subtle
        let frames = levels_frames(&seq, 0.2);
        let loose = DetectParams { threshold: Some(0.03), ..Default::default() };
        let tight = DetectParams { threshold: Some(0.10), ..Default::default() };
        assert_eq!(detect_flashes_levels(&frames, 8, &loose).unwrap().events.len(), 1);
        assert_eq!(detect_flashes_levels(&frames, 8, &tight).unwrap().events.len(), 0);
    }
}
