//! Flash frames: a run of 1..=`max_run` frames that differs sharply from the
//! frame before it *and* the frame after it. See `detect/mod.rs` for the
//! two flavours (return / at cut) and the inputs each mode supplies.

use serde_json::Value;

use super::*;

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
        "timeline" => detect_timeline(frames, bit_depth, params),
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
                        review: None,
                        label: None,
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
        detector: Detector::Flash,
        note: None,
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
                    review: None,
                    label: None,
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
        detector: Detector::Flash,
        note: None,
    })
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
