//! Timeline mode: flagged samples of a file-vs-SDI run.

use serde_json::Value;

use super::*;

/// Auto threshold floor for timeline samples, normalized to peak (~4 codes
/// at 10-bit): below this nothing is flagged however clean the run is.
pub const TIMELINE_FLOOR: f64 = 0.004;
/// Auto threshold ceiling (~31 codes at 10-bit): anything above it is
/// flagged even if most of the run is that bad. Without it a fault that
/// hits every sample (wrong levels, a missing output LUT) would *be* the
/// median and flag nothing.
pub const TIMELINE_CEILING: f64 = 0.03;
/// A run whose median is above this gets a warning: the file differs from
/// the SDI picture everywhere.
pub const TIMELINE_MEDIAN_WARN: f64 = 0.015;

/// Flag timeline samples whose `diff_y` stands out, and report samples that
/// couldn't be measured.
///
/// Auto threshold, from the clean end of the run: the lower quartile `q` of
/// the normalized `diff_y` plus `auto_k` · 1.4826 · MAD of the cleaner half
/// (values up to the median) around `q`, clamped to [`TIMELINE_FLOOR`,
/// `TIMELINE_CEILING`]. Faults only ever add difference, so the cleanest
/// samples say what codec noise looks like; centring on the median instead
/// would let a fault on most of the run become the baseline (a reel where
/// two thirds of the clips slipped by a frame flagged only a third of them).
/// An explicit `params.threshold` (normalized) is used as is.
///
/// Events: `kind` "mismatch" (subkind = position in the clip: first / mid /
/// last; `at_cut` for first and last) or "error" (subkind = the row status:
/// grab_timeout, grab_error, seek_error, compare_error, wrong_tc).
pub fn detect_timeline(frames: &[Value], bit_depth: u32, params: &DetectParams) -> Result<DetectResult, String> {
    let peak = ((1u64 << bit_depth.clamp(8, 16)) - 1) as f64;
    let measured: Vec<(usize, f64)> = frames
        .iter()
        .enumerate()
        .filter_map(|(i, f)| num(f, "diff_y").map(|y| (i, y / peak)))
        .collect();
    let threshold = match params.threshold {
        Some(t) => t.max(1e-6),
        None if measured.len() < 3 => TIMELINE_FLOOR,
        None => {
            let mut v: Vec<f64> = measured.iter().map(|m| m.1).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let q = v[(v.len() - 1) / 4];
            let cleaner = &v[..=v.len() / 2];
            let mut dev: Vec<f64> = cleaner.iter().map(|x| (x - q).abs()).collect();
            dev.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let sigma = 1.4826 * dev[dev.len() / 2];
            (q + params.auto_k * sigma).clamp(TIMELINE_FLOOR, TIMELINE_CEILING)
        }
    };
    let label = |f: &Value| {
        format!(
            "{} (V{}, {})",
            f.get("clip").and_then(|v| v.as_str()).unwrap_or("?"),
            f.get("track").and_then(|v| v.as_u64()).unwrap_or(0),
            f.get("pos").and_then(|v| v.as_str()).unwrap_or("?"),
        )
    };
    let mut events = Vec::new();
    for f in frames {
        let status = f.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let pos = f.get("pos").and_then(|v| v.as_str()).unwrap_or("mid");
        let ev = |kind: &str, subkind: &str, score: f64| Event {
            frame: frame_num(f),
            tc: frame_tc(f),
            length: 1,
            kind: kind.into(),
            subkind: subkind.into(),
            at_cut: pos != "mid",
            score: round3(score),
            review: None,
            label: Some(label(f)),
        };
        match status {
            "ok" => {
                if let Some(y) = num(f, "diff_y") {
                    let n = y / peak;
                    if n > threshold {
                        let e = ev("mismatch", pos, n / threshold);
                        if !(e.at_cut && params.exclude_at_cut) {
                            events.push(e);
                        }
                    }
                }
            }
            "grab_timeout" | "grab_error" | "seek_error" | "compare_error" | "wrong_tc" => {
                events.push(ev("error", status, 0.0));
            }
            _ => {}
        }
    }
    Ok(DetectResult { events, threshold: round4(threshold), source: "timeline".into(), detector: Detector::Flash, note: None })
}

