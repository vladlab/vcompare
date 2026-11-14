//! Event detection — finds flash frames, repeated frames and image matches
//! in per-frame analysis data.
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

use crate::engine::timecode::TimecodeFormat;

pub mod flash;
pub mod matcher;
pub mod repeat;
pub mod timeline;

pub use flash::*;
pub use matcher::{MatchParams, Reference};
pub use repeat::{detect_repeats, RepeatParams};
pub use timeline::*;

/// Which detector produced a set of events. Each has its own tab, its own
/// parameters and its own review verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Detector {
    /// Flash frames (levels / diff), and flagged samples in timeline mode.
    Flash,
    /// Repeated frames (levels).
    Repeat,
    /// Frames that look like a reference picture (levels, needs signatures).
    Match,
}

impl Detector {
    pub fn as_str(self) -> &'static str {
        match self {
            Detector::Flash => "flash",
            Detector::Repeat => "repeat",
            Detector::Match => "match",
        }
    }
}

/// One detection request: which detector, with its parameters.
///
/// JSON: `{ "detector": "flash", "threshold": null, "max_run": 3, … }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "detector", rename_all = "snake_case")]
pub enum DetectRequest {
    Flash(FlashParams),
    Repeat(RepeatParams),
    Match(MatchParams),
}

impl DetectRequest {
    pub fn detector(&self) -> Detector {
        match self {
            DetectRequest::Flash(_) => Detector::Flash,
            DetectRequest::Repeat(_) => Detector::Repeat,
            DetectRequest::Match(_) => Detector::Match,
        }
    }
}

impl Default for DetectRequest {
    fn default() -> Self {
        DetectRequest::Flash(FlashParams::default())
    }
}

/// A detected multi-frame event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    /// First frame of the run (display frame number, same space as `frames[].frame`).
    pub frame: i64,
    /// Timecode of the first frame.
    pub tc: String,
    /// Run length in frames (flash: 1..=max_run; repeat: repeated frames;
    /// match: matching frames).
    pub length: u32,
    /// Event family: "flash"; "mismatch" / "error" (timeline); "repeat";
    /// "match".
    pub kind: String,
    /// Flash: "black" | "white" | "content". Repeat: "short" | "hold" |
    /// "black". Match: the reference's id. Timeline: clip position or error.
    pub subkind: String,
    /// True when the frames on either side of the run differ from each
    /// other (the run sits on a cut) rather than agreeing (flash-and-return).
    pub at_cut: bool,
    /// Detection strength relative to the threshold (>= 1.0). Higher = more
    /// obvious.
    pub score: f64,
    /// The user's verdict, if any: "accept" or "reject". Kept per analysis
    /// (see `state::EngineState::set_event_review`), keyed by first frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    /// Timeline mode: "<clip> (V<track>, <pos>)", for the list and markers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Flash detector parameters, all in normalized 0..1 units of the signal peak.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlashParams {
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

impl Default for FlashParams {
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

/// Flash parameters under their original name (timeline mode and the tests).
pub type DetectParams = FlashParams;

/// What the detector returns to the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectResult {
    pub events: Vec<Event>,
    /// The threshold actually used (auto-derived or as given).
    pub threshold: f64,
    /// Which per-frame series the detector ran on ("levels", "diff_y",
    /// "timeline", "signatures").
    pub source: String,
    /// Which detector this is.
    #[serde(default = "default_detector")]
    pub detector: Detector,
    /// One line for the tab's toolbar (e.g. a repeat cadence), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn default_detector() -> Detector {
    Detector::Flash
}


// ─── Resolve marker EDL ─────────────────────────────────────────────────────

/// Render events as a DaVinci Resolve timeline-marker EDL (what Resolve
/// itself writes from Timeline › Export › Timeline Markers to EDL, and reads
/// back with Timeline › Import › Timeline Markers from EDL):
///
/// ```text
/// TITLE: <title>
/// FCM: NON-DROP FRAME
///
/// 001  001      V     C        01:00:05:00 01:00:05:01 01:00:05:00 01:00:05:01
///  |C:ResolveColorRed |M:flash: content 1 fr x13.5 |D:1
/// ```
///
/// Timecodes are the events' own (the file's embedded TC), so this lines up
/// when the file was exported from the timeline it's going back into.
/// Marker colour: content → Red, white → Yellow, black → Purple; events
/// sitting on a cut get Cyan so they're easy to tell apart. Repeats are
/// Green, image matches Pink.
pub fn events_to_resolve_edl(events: &[Event], tc_fmt: &TimecodeFormat, title: &str) -> String {
    use crate::engine::timecode::TcType;
    let mut out = String::new();
    out.push_str(&format!("TITLE: {}
", title.trim()));
    out.push_str(match tc_fmt.tc_type {
        TcType::DropFrame => "FCM: DROP FRAME

",
        _ => "FCM: NON-DROP FRAME

",
    });
    // Rejected events are not markers.
    let events: Vec<&Event> = events
        .iter()
        .filter(|e| e.review.as_deref() != Some("reject"))
        .collect();
    // Resolve numbers edits with at least three digits.
    let width = events.len().to_string().len().max(3);
    for (i, e) in events.iter().enumerate() {
        let n = i + 1;
        let tc_in = tc_fmt.frame_to_tc(e.frame);
        let tc_out = tc_fmt.frame_to_tc(e.frame + e.length as i64);
        let color = match e.kind.as_str() {
            "error" => "ResolveColorYellow",
            "mismatch" => "ResolveColorRed",
            "repeat" => "ResolveColorGreen",
            "match" => "ResolveColorPink",
            _ if e.at_cut => "ResolveColorCyan",
            _ => match e.subkind.as_str() {
                "white" => "ResolveColorYellow",
                "black" => "ResolveColorPurple",
                _ => "ResolveColorRed",
            },
        };
        let name = match e.kind.as_str() {
            "mismatch" => format!(
                "mismatch: {} x{:.1}",
                e.label.as_deref().unwrap_or(&e.subkind),
                e.score
            ),
            "error" => format!(
                "not checked ({}): {}",
                e.subkind.replace('_', " "),
                e.label.as_deref().unwrap_or("")
            ),
            "repeat" => format!("repeat: {} {} fr", e.subkind, e.length),
            "match" => format!(
                "match: {} {} fr x{:.2}",
                e.label.as_deref().unwrap_or(&e.subkind),
                e.length,
                e.score
            ),
            _ => format!(
                "flash: {} {} fr x{:.1}{}",
                e.subkind,
                e.length,
                e.score,
                if e.at_cut { " (at cut)" } else { "" }
            ),
        };
        out.push_str(&format!(
            "{n:0width$}  001      V     C        {tc_in} {tc_out} {tc_in} {tc_out}
"
        ));
        out.push_str(&format!(" |C:{color} |M:{name} |D:{}

", e.length));
    }
    out
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Pick the threshold: explicit, or robust auto from the change series.
///
/// Auto = median + k · (1.4826 · MAD), floored at `min_threshold`. The
/// median/MAD are robust to the handful of cuts and flashes we are looking
/// for, so they measure the "ordinary" frame-to-frame change of the footage.
pub(crate) fn resolve_threshold(d: &[f64], params: &DetectParams) -> f64 {
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

pub(crate) fn num(f: &Value, key: &str) -> Option<f64> {
    f.get(key).and_then(|v| v.as_f64())
}

pub(crate) fn frame_num(f: &Value) -> i64 {
    f.get("frame").and_then(|v| v.as_i64()).unwrap_or(0)
}

pub(crate) fn frame_tc(f: &Value) -> String {
    f.get("tc").and_then(|v| v.as_str()).unwrap_or("").to_string()
}

pub(crate) fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

pub(crate) fn round4(v: f64) -> f64 {
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
    fn resolve_edl_format() {
        use crate::engine::timecode::{TcType, TimecodeFormat};
        let tc = TimecodeFormat { fps_num: 24, fps_den: 1, tc_type: TcType::NonDrop };
        let ev = |frame: i64, length: u32, subkind: &str, at_cut: bool| Event {
            frame, tc: tc.frame_to_tc(frame), length, kind: "flash".into(),
            subkind: subkind.into(), at_cut, score: 13.452, review: None, label: None,
        };
        let edl = events_to_resolve_edl(
            &[ev(86400 + 120, 1, "white", false), ev(86400 + 300, 2, "content", true)],
            &tc, "clip",
        );
        let lines: Vec<&str> = edl.lines().collect();
        assert_eq!(lines[0], "TITLE: clip");
        assert_eq!(lines[1], "FCM: NON-DROP FRAME");
        assert_eq!(lines[2], "");
        assert_eq!(lines[3], "001  001      V     C        01:00:05:00 01:00:05:01 01:00:05:00 01:00:05:01");
        assert_eq!(lines[4], " |C:ResolveColorYellow |M:flash: white 1 fr x13.5 |D:1");
        assert_eq!(lines[5], "");
        assert_eq!(lines[6], "002  001      V     C        01:00:12:12 01:00:12:14 01:00:12:12 01:00:12:14");
        assert_eq!(lines[7], " |C:ResolveColorCyan |M:flash: content 2 fr x13.5 (at cut) |D:2");

        // Rejected events are left out and the numbering closes up.
        let mut rejected = ev(100, 1, "black", false);
        rejected.review = Some("reject".into());
        let edl = events_to_resolve_edl(&[rejected, ev(200, 1, "white", false)], &tc, "x");
        assert!(!edl.contains("00:00:04:04"), "rejected event must not be a marker");
        assert!(edl.contains("\n001  001      V     C        00:00:08:08"));

        let df = TimecodeFormat { fps_num: 30000, fps_den: 1001, tc_type: TcType::DropFrame };
        assert!(events_to_resolve_edl(&[], &df, "x").contains("FCM: DROP FRAME"));
        // 1000+ events widen the edit number
        let many: Vec<Event> = (0..1000).map(|i| ev(i * 10, 1, "black", false)).collect();
        assert!(events_to_resolve_edl(&many, &tc, "x").contains("\n1000  001      V"));
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
    #[test]
    fn timeline_detector() {
        let row = |tc: &str, frame: i64, status: &str, y: Option<f64>, pos: &str| {
            let mut v = serde_json::json!({"frame": frame, "tc": tc, "status": status,
                                           "clip": "c", "track": 1, "pos": pos});
            if let Some(y) = y {
                v["diff_y"] = serde_json::json!(y);
            }
            v
        };
        // Ten clean samples around 2 codes (10-bit), one at 60, one timeout,
        // one outside the file.
        let mut frames: Vec<Value> = (0..10)
            .map(|i| row(&format!("01:00:00:{i:02}"), i, "ok", Some(2.0 + (i % 3) as f64 * 0.3), "mid"))
            .collect();
        frames.push(row("01:00:01:00", 24, "ok", Some(60.0), "first"));
        frames.push(row("01:00:01:01", 25, "grab_timeout", None, "mid"));
        frames.push(row("01:00:01:02", 26, "not_in_file", None, "mid"));
        let r = detect_timeline(&frames, 10, &DetectParams::default()).unwrap();
        let got: Vec<(&str, &str, &str, bool)> =
            r.events.iter().map(|e| (e.tc.as_str(), e.kind.as_str(), e.subkind.as_str(), e.at_cut)).collect();
        assert_eq!(
            got,
            vec![("01:00:01:00", "mismatch", "first", true), ("01:00:01:01", "error", "grab_timeout", false)]
        );
        assert_eq!(r.events[0].label.as_deref(), Some("c (V1, first)"));
        assert!(r.threshold >= TIMELINE_FLOOR && r.threshold < 60.0 / 1023.0);
        // A fault on every sample: the median is high, but the ceiling still flags it all.
        let all_bad: Vec<Value> = (0..10)
            .map(|i| row(&format!("01:00:00:{i:02}"), i, "ok", Some(80.0 + i as f64), "mid"))
            .collect();
        let r = detect_timeline(&all_bad, 10, &DetectParams::default()).unwrap();
        assert_eq!(r.events.len(), 10, "threshold {}", r.threshold);
        assert_eq!(r.threshold, TIMELINE_CEILING);
        // Two thirds of the run off by a frame (24 codes) over a clean 0:
        // the clean end sets the baseline, so every bad sample is flagged.
        let mut majority: Vec<Value> = (0..3).map(|i| row(&format!("01:00:00:{i:02}"), i, "ok", Some(0.0), "mid")).collect();
        majority.extend((3..9).map(|i| row(&format!("01:00:00:{i:02}"), i, "ok", Some(24.3 + i as f64), "mid")));
        let r = detect_timeline(&majority, 10, &DetectParams::default()).unwrap();
        assert_eq!(r.events.len(), 6, "threshold {}", r.threshold);
        // A clean reel with grain varying shot to shot (1..10 codes): nothing flagged.
        let mixed: Vec<Value> = (0..30)
            .map(|i| row(&format!("01:00:{:02}:00", i), i, "ok", Some(1.0 + (i * 7 % 30) as f64 * 0.3), "mid"))
            .collect();
        let r = detect_timeline(&mixed, 10, &DetectParams::default()).unwrap();
        assert!(r.events.is_empty(), "threshold {} flagged {:?}", r.threshold, r.events.iter().map(|e| &e.tc).collect::<Vec<_>>());
        // Excluding cut-adjacent samples drops first/last mismatches only.
        let p = DetectParams { exclude_at_cut: true, ..Default::default() };
        let r = detect_timeline(&frames, 10, &p).unwrap();
        assert_eq!(r.events.len(), 1);
        // Markers.
        let edl = events_to_resolve_edl(
            &detect_timeline(&frames, 10, &DetectParams::default()).unwrap().events,
            &crate::engine::timecode::TimecodeFormat { fps_num: 24, fps_den: 1, tc_type: crate::engine::timecode::TcType::NonDrop },
            "t",
        );
        assert!(edl.contains("|C:ResolveColorRed |M:mismatch: c (V1, first) x"), "{edl}");
        assert!(edl.contains("|C:ResolveColorYellow |M:not checked (grab timeout): c (V1, mid)"), "{edl}");
    }
}

