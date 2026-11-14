//! Repeated frames: a frame that is (nearly) the same picture as the frame
//! before it.
//!
//! Levels analyses only. Uses the per-frame difference to the previous
//! frame the analysis already records (`y_dif`, `u_dif`, `v_dif`, mean |Δ|
//! in code values), combined the same way as the flash detector. Nothing new
//! is decoded.
//!
//! "Exact" (`tolerance == 0`) means the recorded mean |Δ| is zero. The
//! analysis rounds it to 1e-4 codes, so at UHD a handful of pixels off by a
//! code still reads as zero; that is below anything visible, and an intra
//! codec (ProRes, DNx, J2K) decodes a frame repeated in its source
//! bit-identical anyway. Long-GOP deliverables repeat to *near* zero, which
//! is what the tolerance is for.
//!
//! A run is a stretch of consecutive repeating frames. The event starts at
//! the first repeated frame (the second showing of the picture) and
//! `length` counts the repeated frames, so a length-1 event is one picture
//! shown twice.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::*;

/// Repeat detector parameters. Tolerance is normalized 0..1 of peak.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepeatParams {
    /// Max mean |Δ| vs the previous frame (normalized). 0 = exact.
    #[serde(default)]
    pub tolerance: f64,
    /// Fewest repeated frames that make an event.
    #[serde(default = "default_min_run")]
    pub min_run: u32,
    /// Longest run reported as a short repeat.
    #[serde(default = "default_max_run")]
    pub max_run: u32,
    /// Also report runs longer than `max_run` (holds, freezes, stills).
    #[serde(default)]
    pub include_long: bool,
    /// Leave out runs whose picture is black.
    #[serde(default = "default_true")]
    pub skip_black: bool,
}

fn default_min_run() -> u32 {
    1
}
fn default_max_run() -> u32 {
    5
}
fn default_true() -> bool {
    true
}

impl Default for RepeatParams {
    fn default() -> Self {
        Self {
            tolerance: 0.0,
            min_run: default_min_run(),
            max_run: default_max_run(),
            include_long: false,
            skip_black: true,
        }
    }
}

/// Brightest pixel below this (normalized) = a black picture. Same line as
/// the flash detector's black classification.
const BLACK_YMAX: f64 = 0.12;

/// Find repeated frames in a levels analysis.
pub fn detect_repeats(frames: &[Value], bit_depth: u32, params: &RepeatParams) -> Result<DetectResult, String> {
    let peak = ((1u64 << bit_depth.clamp(8, 16)) - 1) as f64;
    let n = frames.len();
    if !frames.iter().any(|f| num(f, "y_dif").is_some()) {
        return Err(
            "This levels analysis predates temporal stats — re-run the analysis to find repeated frames".into(),
        );
    }
    let tol = params.tolerance.max(0.0);
    let d: Vec<f64> = frames
        .iter()
        .map(|f| {
            let y = num(f, "y_dif").unwrap_or(f64::MAX) / peak;
            let u = num(f, "u_dif").unwrap_or(0.0) / peak;
            let v = num(f, "v_dif").unwrap_or(0.0) / peak;
            y.max(0.75 * u.max(v))
        })
        .collect();
    let repeats = |i: usize| i > 0 && d[i] <= tol;
    let min_run = params.min_run.max(1) as usize;
    let max_run = params.max_run.max(min_run as u32) as usize;

    let mut events = Vec::new();
    let mut short_starts = Vec::new();
    let mut i = 1;
    while i < n {
        if !repeats(i) {
            i += 1;
            continue;
        }
        let a = i;
        while i < n && repeats(i) {
            i += 1;
        }
        let len = i - a; // repeated frames a..i
        if len < min_run {
            continue;
        }
        // The picture shown: frame a-1 and its repeats.
        let black = frames[a - 1..i]
            .iter()
            .map(|f| num(f, "y_max").unwrap_or(peak) / peak)
            .fold(f64::MIN, f64::max)
            < BLACK_YMAX;
        let subkind = if black {
            if params.skip_black {
                continue;
            }
            "black"
        } else if len > max_run {
            if !params.include_long {
                continue;
            }
            "hold"
        } else {
            "short"
        };
        if subkind == "short" {
            short_starts.push(a);
        }
        let max_d = d[a..i].iter().cloned().fold(0.0, f64::max);
        let score = if tol > 0.0 {
            (tol / max_d.max(tol / 99.0)).max(1.0)
        } else {
            len as f64
        };
        events.push(Event {
            frame: frame_num(&frames[a]),
            tc: frame_tc(&frames[a]),
            length: len as u32,
            kind: "repeat".into(),
            subkind: subkind.into(),
            at_cut: false,
            score: round3(score),
            review: None,
            label: None,
        });
    }

    Ok(DetectResult {
        events,
        threshold: round4(tol),
        source: "levels".into(),
        detector: Detector::Repeat,
        note: cadence(&short_starts).map(|p| format!("repeats every {p} frames — a cadence, not isolated faults")),
    })
}

/// If most gaps between consecutive short repeats share one period, return
/// it. Needs at least 4 repeats (3 gaps) and 80 % of gaps within ±1 of the
/// commonest gap.
fn cadence(starts: &[usize]) -> Option<usize> {
    if starts.len() < 4 {
        return None;
    }
    let gaps: Vec<usize> = starts.windows(2).map(|w| w[1] - w[0]).collect();
    let mut counts = std::collections::BTreeMap::<usize, usize>::new();
    for &g in &gaps {
        *counts.entry(g).or_default() += 1;
    }
    let (&period, _) = counts.iter().max_by_key(|(g, c)| (**c, usize::MAX - **g))?;
    let near = gaps.iter().filter(|&&g| g.abs_diff(period) <= 1).count();
    (near * 5 >= gaps.len() * 4 && period > 1).then_some(period)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 8-bit frames from a list of "picture ids": equal ids in a row are
    /// repeats (y_dif 0), a change gives y_dif 20. Id 0 is black.
    fn frames(ids: &[u32]) -> Vec<Value> {
        ids.iter()
            .enumerate()
            .map(|(i, &id)| {
                let dif = if i == 0 { 0.0 } else if ids[i - 1] == id { 0.0 } else { 20.0 };
                let ymax = if id == 0 { 20.0 } else { 200.0 };
                json!({
                    "frame": i as i64 + 100, "tc": format!("f{i}"),
                    "y_max": ymax, "y_dif": dif, "u_dif": 0.0, "v_dif": 0.0,
                })
            })
            .collect()
    }

    fn run(ids: &[u32], p: &RepeatParams) -> Vec<(i64, u32, String)> {
        detect_repeats(&frames(ids), 8, p)
            .unwrap()
            .events
            .into_iter()
            .map(|e| (e.frame, e.length, e.subkind))
            .collect()
    }

    #[test]
    fn single_repeat() {
        let r = run(&[1, 2, 3, 3, 4, 5], &RepeatParams::default());
        assert_eq!(r, vec![(103, 1, "short".into())]);
    }

    #[test]
    fn run_of_three() {
        let r = run(&[1, 2, 2, 2, 2, 3], &RepeatParams::default());
        assert_eq!(r, vec![(102, 3, "short".into())]);
    }

    #[test]
    fn long_hold_hidden_unless_asked() {
        let ids = [1, 2, 2, 2, 2, 2, 2, 2, 3];
        assert!(run(&ids, &RepeatParams::default()).is_empty());
        let p = RepeatParams { include_long: true, ..Default::default() };
        assert_eq!(run(&ids, &p), vec![(102, 6, "hold".into())]);
    }

    #[test]
    fn black_skipped_by_default() {
        let ids = [1, 0, 0, 0, 2];
        assert!(run(&ids, &RepeatParams::default()).is_empty());
        let p = RepeatParams { skip_black: false, ..Default::default() };
        assert_eq!(run(&ids, &p), vec![(102, 2, "black".into())]);
    }

    #[test]
    fn first_frame_is_never_a_repeat() {
        // frame 0 has y_dif 0 by construction (no previous frame)
        assert!(run(&[1, 2, 3], &RepeatParams::default()).is_empty());
    }

    #[test]
    fn min_run_filters_singles() {
        let p = RepeatParams { min_run: 2, ..Default::default() };
        assert_eq!(run(&[1, 1, 2, 3, 3, 3, 4], &p), vec![(104, 2, "short".into())]);
    }

    #[test]
    fn tolerance_catches_near_repeats() {
        let mut f = frames(&[1, 2, 3, 4]);
        f[2]["y_dif"] = json!(0.3); // near-repeat, 0.12 % of peak
        let exact = detect_repeats(&f, 8, &RepeatParams::default()).unwrap();
        assert!(exact.events.is_empty());
        let loose = detect_repeats(&f, 8, &RepeatParams { tolerance: 0.002, ..Default::default() }).unwrap();
        assert_eq!(loose.events.len(), 1);
        assert_eq!(loose.events[0].frame, 102);
        assert!(loose.events[0].score >= 1.0);
    }

    #[test]
    fn periodic_repeats_report_cadence() {
        // a repeat every 5 frames (24 → 30 style)
        let mut ids = Vec::new();
        let mut pic = 1;
        for i in 0..40 {
            if i % 5 == 4 {
                ids.push(pic);
            } else {
                pic += 1;
                ids.push(pic);
            }
        }
        let r = detect_repeats(&frames(&ids), 8, &RepeatParams::default()).unwrap();
        assert_eq!(r.events.len(), 8);
        assert!(r.note.as_deref().unwrap_or("").contains("every 5 frames"), "{:?}", r.note);
        // isolated repeats: no cadence
        let r = detect_repeats(&frames(&[1, 2, 2, 3, 4, 5, 6, 6, 7, 8, 9, 10, 11, 12, 12, 13]), 8, &RepeatParams::default()).unwrap();
        assert!(r.note.is_none());
    }

    #[test]
    fn old_cache_errors() {
        let f = vec![json!({"frame": 0, "y_max": 200.0})];
        assert!(detect_repeats(&f, 8, &RepeatParams::default()).is_err());
    }

    /// End to end: a ProRes clip with one frame doubled (as a bad retime or
    /// a conform slip leaves it), through the levels engine.
    #[test]
    fn finds_doubled_frame_in_analysed_clip() {
        let dir = std::env::temp_dir().join(format!("vcompare_repeat_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("dup.mov").to_string_lossy().to_string();
        let ok = crate::engine::sysenv::command("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=640x360:r=24:d=3",
                   "-vf", "loop=loop=1:size=1:start=30,setpts=N/24/TB", "-frames:v", "72",
                   "-c:v", "prores_ks", "-profile:v", "3", "-pix_fmt", "yuv422p10le", &clip])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("ffmpeg unavailable; skipping");
            return;
        }
        let mut d = Vec::new();
        let info = crate::engine::analysis::prepare_single_file(&clip, &mut d).unwrap();
        let (frames, _, _) = crate::engine::analysis::run_levels_analysis(
            &clip, &info, None, Some(1), false, None, crate::engine::process::standalone_job(), &mut d, &|_| {},
        )
        .unwrap();
        let r = detect_repeats(&frames, info.bit_depth, &RepeatParams::default()).unwrap();
        let got: Vec<(i64, u32)> = r.events.iter().map(|e| (e.frame - info.start_frame, e.length)).collect();
        // ffmpeg's loop filter repeats the frame *before* `start` here:
        // framemd5 shows frames 29 and 30 identical, so the repeat is 30.
        assert_eq!(got, vec![(30, 1)]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
