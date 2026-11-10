//! Timeline mode, the run: plan samples from Resolve, then for each one
//! seek → grab the SDI frame → compare against the file, with the compares
//! on a small worker pool so the next seek overlaps the previous compare.
//!
//! No Tauri here: the command layer and the CLI both call [`run`] with
//! callbacks for progress and diagnostics.
//!
//! ```text
//! ResolveSession ──CLIPS──▶ plan ──▶ for each sample:
//!      ▲                               SEEK tc ─▶ Resolve
//!      │                               GRAB tc ─▶ scopetap_grab ─▶ frame ─┐
//!      │                                                                  ▼
//!      └──── (next sample)                           bounded queue ─▶ N workers (ffmpeg psnr)
//! ```
//!
//! Memory: at most `workers` frames waiting plus `workers` being compared.
//! A UHD rgb48le frame is 49.8 MB, so the default 4 workers is ~400 MB peak.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use crate::engine::grab::{self, GrabError, GrabMeta, Grabber};
use crate::engine::probe::{ffprobe_json, get_video_info};
use crate::engine::process::Job;
use crate::engine::resolve::{ResolveSession, TimelineMeta};
use crate::engine::timecode::{detect_timecode_format, TimecodeFormat};
use crate::engine::timeline::{plan_samples, FileRange, Plan, Sample, TimelineClock};
use crate::engine::timeline_compare::{compare_one, Diff, FileSide};

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RunParams {
    pub file: String,
    /// Video track indices (1-based) to sample.
    pub tracks: Vec<u32>,
    #[serde(default)]
    pub offset: i64,
    /// Seek + grab only (no compare): measures how fast this setup is.
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub grab_timeout_ms: Option<u32>,
    #[serde(default)]
    pub settle_ms: Option<u32>,
    #[serde(default)]
    pub workers: Option<usize>,
    /// Testing only: force the live side's matrix.
    #[serde(default)]
    pub matrix_override: Option<String>,
}

/// Progress for the UI: samples done / total and what's being looked at.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub clip: String,
    pub tc: String,
    /// Seconds left at the current pace, once there is a pace.
    pub eta_s: Option<f64>,
}

pub struct Callbacks<'a> {
    pub progress: &'a (dyn Fn(&Progress) + Sync),
    pub diagnostics: &'a (dyn Fn(&[String]) + Sync),
}

/// One row of the result, before it becomes JSON.
#[derive(Debug, Clone)]
struct Row {
    sample: Sample,
    status: &'static str,
    diff: Option<Diff>,
    meta: Option<GrabMeta>,
    error: Option<String>,
}

impl Row {
    fn new(sample: Sample, status: &'static str) -> Self {
        Self { sample, status, diff: None, meta: None, error: None }
    }

    fn to_json(&self) -> Value {
        let s = &self.sample;
        let mut v = json!({
            "frame": s.frame,
            "tc": s.tc,
            "track": s.track,
            "clip": s.clip,
            "pos": s.pos.as_str(),
            "status": self.status,
        });
        if let Some(ff) = s.file_frame {
            v["file_frame"] = json!(ff);
        }
        if let Some(d) = self.diff {
            v["diff_y"] = json!(d.y);
            v["diff_u"] = json!(d.u);
            v["diff_v"] = json!(d.v);
        }
        if let Some(m) = &self.meta {
            v["waited_ms"] = json!(m.waited_ms);
            v["refines"] = json!(m.refines);
            if m.clamped > 0 {
                v["clamped"] = json!(m.clamped);
            }
        }
        if let Some(e) = &self.error {
            v["error"] = json!(e);
        }
        v
    }
}

fn same_tc(a: &str, b: &str) -> bool {
    a.replace(';', ":") == b.replace(';', ":")
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

/// Warnings when the SDI signal's tags and the file's disagree. The run goes
/// on regardless: the operator may know better.
pub fn signal_warnings(sig: &GrabMeta, file: &FileSide) -> Vec<String> {
    let mut w = Vec::new();
    let file_2020 = file.matrix == "bt2020";
    match sig.colorspace {
        2020 if !file_2020 => w.push(format!(
            "SDI is tagged Rec.2020 but the file's matrix is {}: if the monitor was showing 2020 and the deliverable is 709, everything will differ",
            file.matrix
        )),
        709 if file_2020 => w.push("SDI is tagged Rec.709 but the file is Rec.2020".into()),
        _ => {}
    }
    let file_pq = file.transfer.as_deref() == Some("smpte2084");
    let file_hlg = file.transfer.as_deref() == Some("arib-std-b67");
    match sig.eotf {
        2 if !file_pq => w.push(format!(
            "SDI is PQ but the file's transfer is {}",
            file.transfer.as_deref().unwrap_or("untagged")
        )),
        3 if !file_hlg => w.push(format!(
            "SDI is HLG but the file's transfer is {}",
            file.transfer.as_deref().unwrap_or("untagged")
        )),
        0 if file_pq || file_hlg => w.push(format!(
            "SDI is SDR but the file is {}",
            if file_pq { "PQ" } else { "HLG" }
        )),
        _ => {}
    }
    w
}

/// Display frame of the file's first frame, and how we got it.
fn file_start(file_tc: Option<&str>, clock: &TimelineClock, diag: &mut Vec<String>) -> i64 {
    match file_tc {
        Some(tc) => {
            let file_df = tc.contains(';');
            let tl_df = clock.tc_fmt.tc_type == crate::engine::timecode::TcType::DropFrame;
            if file_df != tl_df {
                diag.push(format!(
                    "warning: file timecode {tc} is {} but the timeline counts {}",
                    if file_df { "drop-frame" } else { "non-drop" },
                    if tl_df { "drop-frame" } else { "non-drop" }
                ));
            }
            crate::engine::timecode::tc_string_to_frames(tc, &clock.tc_fmt)
        }
        None => {
            diag.push("file has no timecode: assuming it starts at the timeline's start".into());
            clock.start_tc_frames
        }
    }
}

fn fps_matches(a: &TimecodeFormat, b: &TimecodeFormat) -> bool {
    a.fps_num as u64 * b.fps_den as u64 == b.fps_num as u64 * a.fps_den as u64
}

/// Everything decided before the first seek.
pub struct Setup {
    pub file: FileSide,
    pub clock: TimelineClock,
    pub range: FileRange,
    pub plan: Plan,
    pub timeline: TimelineMeta,
    pub target: String,
    pub resolve_target: String,
}

/// Connect, read the timeline, plan. Separate from the loop so the CLI can
/// show the plan, and so tests can check it.
pub fn prepare(
    p: &RunParams,
    session: &mut ResolveSession,
    diag: &mut Vec<String>,
) -> Result<Setup, String> {
    let probe = ffprobe_json(&p.file)?;
    let file = FileSide::from_probe(&p.file, &probe)?;
    let file_tc_fmt = detect_timecode_format(&probe)?;
    let info = get_video_info(&probe, &file_tc_fmt)?;

    if p.tracks.is_empty() {
        return Err("No tracks ticked".into());
    }
    let clips = session.clips(&p.tracks)?;
    let clock = TimelineClock::from_meta(&clips.timeline)?;
    if !fps_matches(&clock.tc_fmt, &file_tc_fmt) {
        return Err(format!(
            "The file is {}/{} fps but the timeline is {}/{} fps",
            file_tc_fmt.fps_num, file_tc_fmt.fps_den, clock.tc_fmt.fps_num, clock.tc_fmt.fps_den
        ));
    }
    let range = FileRange {
        start_tc_frames: file_start(info.start_tc.as_deref(), &clock, diag),
        frames: info.nb_frames,
        offset: p.offset,
    };
    let plan = plan_samples(&clips.items, &clock, &range);
    if plan.samples.is_empty() {
        return Err(format!("No clips on track(s) {:?}", p.tracks));
    }
    let (target, depth) = file.target();
    diag.push(format!(
        "resolve: {} / {}, {} fps{}, starts {}",
        clips.timeline.project.as_deref().unwrap_or("?"),
        clips.timeline.name.as_deref().unwrap_or("?"),
        clips.timeline.fps.as_deref().unwrap_or("?"),
        if clips.timeline.drop_frame { " DF" } else { "" },
        clips.timeline.start_tc.as_deref().unwrap_or("?"),
    ));
    diag.push(format!(
        "tracks {:?}: {} items → {} samples ({} in the file){}{}",
        p.tracks,
        clips.items.len(),
        plan.samples.len(),
        plan.in_file(),
        if plan.skipped_disabled > 0 { format!(", {} disabled items skipped", plan.skipped_disabled) } else { String::new() },
        if plan.skipped_bad > 0 { format!(", {} items without a usable range", plan.skipped_bad) } else { String::new() },
    ));
    diag.push(format!("file: {}", file.describe()));
    diag.push(format!(
        "file range: {} .. {}",
        clock.tc_fmt.frame_to_tc(range.origin()),
        range
            .frames
            .map(|n| clock.tc_fmt.frame_to_tc(range.origin() + n as i64))
            .unwrap_or_else(|| "?".into())
    ));
    if p.offset != 0 {
        diag.push(format!("manual offset: {:+} frames", p.offset));
    }
    diag.push(format!("compare: {target} ({depth}-bit codes)"));
    Ok(Setup {
        file,
        clock,
        range,
        plan,
        timeline: clips.timeline,
        target,
        resolve_target: session.target.clone(),
    })
}

/// The whole run. Returns the analysis JSON (`mode: "timeline"`), ready for
/// the engine state and the cache.
pub fn run(p: &RunParams, job: &Arc<Job>, cb: &Callbacks) -> Result<Value, String> {
    let t0 = Instant::now();
    let mut diag: Vec<String> = vec![if p.dry_run {
        "mode: timeline dry run (seek + grab, no compare)".into()
    } else {
        "mode: timeline (file vs. Resolve's SDI output)".into()
    }];
    let bin = grab::find_binary().ok_or(
        "scopetap_grab not found (install scopetap, or set VCOMPARE_GRABBER)",
    )?;
    let mut session = ResolveSession::connect()?;
    let setup = prepare(p, &mut session, &mut diag)?;
    (cb.diagnostics)(&diag);

    let mut grabber = Grabber::start(&bin, setup.clock.tc_fmt.nominal_fps(), p.settle_ms, job)?;
    diag.push(format!("grabber: {} ({}x{})", bin.display(), grabber.width, grabber.height));
    (cb.diagnostics)(&diag);

    let timeout = p.grab_timeout_ms.unwrap_or(5000);
    let n_workers = p.workers.unwrap_or(4).clamp(1, 16);

    // Rows in plan order; workers fill in diffs by index.
    let mut rows: Vec<Row> = setup
        .plan
        .samples
        .iter()
        .map(|s| Row::new(s.clone(), if s.file_frame.is_some() || p.dry_run { "pending" } else { "not_in_file" }))
        .collect();
    let todo: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].status == "pending").collect();

    // Worker pool: bounded queue of grabbed frames.
    type Work = (usize, i64, Arc<Vec<u8>>, u32, u32);
    let (work_tx, work_rx) = mpsc::sync_channel::<Work>(n_workers);
    let work_rx = Arc::new(std::sync::Mutex::new(work_rx));
    let (res_tx, res_rx) = mpsc::channel::<(usize, Result<Diff, String>)>();
    let file = Arc::new(setup.file.clone());
    let matrix_override = p.matrix_override.clone();
    let mut workers = Vec::new();
    if !p.dry_run {
        for _ in 0..n_workers {
            let rx = work_rx.clone();
            let tx = res_tx.clone();
            let file = file.clone();
            let job = job.clone();
            let m = matrix_override.clone();
            workers.push(std::thread::spawn(move || loop {
                let next = rx.lock().unwrap().recv();
                let Ok((i, ff, data, w, h)) = next else { break };
                let r = compare_one(&file, ff, data, w, h, m.as_deref(), &job);
                let _ = tx.send((i, r));
            }));
        }
    }
    drop(res_tx);

    let mut signal: Option<GrabMeta> = None;
    let mut signal_changes = 0usize;
    let mut fatal: Option<String> = None;
    // Resolve refuses every seek while the Project Manager (or a modal
    // dialog) is open: stop early with a hint rather than refuse the reel.
    let mut any_seek_ok = false;
    let mut refused_in_a_row = 0usize;
    let loop_t0 = Instant::now();
    let total = todo.len();
    for (k, &i) in todo.iter().enumerate() {
        if job.is_cancelled() {
            break;
        }
        let (tc, clip, ff) = {
            let s = &rows[i].sample;
            (s.tc.clone(), s.clip.clone(), s.file_frame.unwrap_or(0))
        };
        let eta = (k > 0).then(|| loop_t0.elapsed().as_secs_f64() / k as f64 * (total - k) as f64);
        (cb.progress)(&Progress { done: k, total, clip: clip.clone(), tc: tc.clone(), eta_s: eta });

        if let Err(e) = session.seek(&tc) {
            if e.contains("bridge closed") {
                fatal = Some(format!("Lost Resolve at {tc}: {e}"));
                break;
            }
            rows[i].status = "seek_error";
            rows[i].error = Some(e.clone());
            refused_in_a_row += 1;
            if !any_seek_ok && refused_in_a_row >= 3 {
                fatal = Some(format!(
                    "Resolve refused the first {refused_in_a_row} seeks ({e}). \
                     Close the Project Manager and any dialogs, and have the timeline open on the Edit or Color page."
                ));
                break;
            }
            continue;
        }
        any_seek_ok = true;
        refused_in_a_row = 0;
        match grabber.grab(&tc, timeout) {
            Ok((meta, data)) => {
                if !same_tc(&meta.tc, &tc) {
                    rows[i].status = "wrong_tc";
                    rows[i].error = Some(format!("asked for {tc}, got {}", meta.tc));
                    rows[i].meta = Some(meta);
                    continue;
                }
                match &signal {
                    None => {
                        for w in signal_warnings(&meta, &setup.file) {
                            diag.push(format!("warning: {w}"));
                        }
                        diag.push(format!(
                            "SDI: {}x{} {}, levels {}{}",
                            meta.width,
                            meta.height,
                            meta.signal_name(),
                            meta.levels,
                            if meta.levels_auto { " (auto)" } else { "" }
                        ));
                        (cb.diagnostics)(&diag);
                        signal = Some(meta.clone());
                    }
                    Some(s) => {
                        if s.colorspace != meta.colorspace || s.eotf != meta.eotf || s.levels != meta.levels
                            || s.width != meta.width || s.height != meta.height
                        {
                            signal_changes += 1;
                        }
                    }
                }
                let (w, h) = (meta.width, meta.height);
                rows[i].meta = Some(meta);
                if p.dry_run {
                    rows[i].status = "grabbed";
                } else if work_tx.send((i, ff, Arc::new(data), w, h)).is_err() {
                    fatal = Some("compare workers stopped".into());
                    break;
                }
            }
            Err(GrabError::Timeout { have }) => {
                rows[i].status = "grab_timeout";
                rows[i].error = Some(match have {
                    Some(h) => format!("never settled on {tc} (SDI showed {h})"),
                    None => format!("never settled on {tc}"),
                });
            }
            Err(GrabError::Refused { code, detail }) => {
                rows[i].status = "grab_error";
                rows[i].error = Some(if detail.is_empty() { code } else { format!("{code}: {detail}") });
            }
            Err(e @ GrabError::Closed(_)) => {
                fatal = Some(format!("scopetap_grab stopped at {tc}: {e}"));
                break;
            }
        }
    }
    drop(work_tx);
    for (i, r) in res_rx.iter() {
        match r {
            Ok(d) => {
                rows[i].diff = Some(d);
                rows[i].status = "ok";
            }
            Err(e) => {
                rows[i].status = "compare_error";
                rows[i].error = Some(e);
            }
        }
    }
    for w in workers {
        let _ = w.join();
    }
    grabber.close();
    session.close();

    if job.is_cancelled() {
        return Err("Analysis cancelled".into());
    }
    if let Some(f) = fatal {
        return Err(f);
    }
    (cb.progress)(&Progress { done: total, total, clip: String::new(), tc: String::new(), eta_s: Some(0.0) });

    // ── Summary ──
    let mut waits: Vec<u64> = rows.iter().filter_map(|r| r.meta.as_ref().map(|m| m.waited_ms)).collect();
    waits.sort_unstable();
    let mut counts = serde_json::Map::new();
    for r in &rows {
        let c = counts.entry(r.status.to_string()).or_insert(json!(0));
        *c = json!(c.as_u64().unwrap_or(0) + 1);
    }
    let loop_s = loop_t0.elapsed().as_secs_f64();
    diag.push(format!(
        "{} samples in {:.1} s ({:.0} ms each); grab wait median {} ms, p95 {} ms, max {} ms",
        total,
        loop_s,
        if total > 0 { loop_s * 1000.0 / total as f64 } else { 0.0 },
        percentile(&waits, 0.5),
        percentile(&waits, 0.95),
        waits.last().copied().unwrap_or(0),
    ));
    if signal_changes > 0 {
        diag.push(format!("warning: the SDI signal changed format {signal_changes} time(s) during the run"));
    }
    let (_, depth) = setup.file.target();
    if !p.dry_run {
        let mut ys: Vec<f64> = rows.iter().filter_map(|r| r.diff.map(|d| d.y)).collect();
        ys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if !ys.is_empty() {
            let median = ys[ys.len() / 2];
            let peak = ((1u64 << depth) - 1) as f64;
            diag.push(format!("diff_y median {median:.2} codes ({:.2}% of peak)", median / peak * 100.0));
            if median / peak > crate::engine::events::TIMELINE_MEDIAN_WARN {
                diag.push(format!(
                    "warning: the file differs from the SDI picture everywhere (median {median:.1} codes): \
                     check levels, matrix, output LUTs and Resolve's monitoring state"
                ));
            }
        }
    }
    (cb.diagnostics)(&diag);

    let tc = &setup.clock.tc_fmt;
    Ok(json!({
        "mode": "timeline",
        "dry_run": p.dry_run,
        "frames": rows.iter().map(Row::to_json).collect::<Vec<_>>(),
        "ref_path": p.file,
        "dist_path": "",
        "ref_start_frame": setup.plan.samples.first().map(|s| s.frame).unwrap_or(0),
        "ref_file_start_tc": setup.range.origin(),
        "file_frames": setup.range.frames,
        "offset": p.offset,
        "bit_depth": depth,
        "compare_format": setup.target,
        "tc_format": { "fps_num": tc.fps_num, "fps_den": tc.fps_den, "tc_type": tc.tc_type },
        "resolve": {
            "target": setup.resolve_target,
            "project": setup.timeline.project,
            "timeline": setup.timeline.name,
            "start_tc": setup.timeline.start_tc,
            "fps": setup.timeline.fps,
            "drop_frame": setup.timeline.drop_frame,
            "tracks": p.tracks,
        },
        "signal": signal.as_ref().map(|s| json!({
            "width": s.width, "height": s.height, "levels": s.levels, "levels_auto": s.levels_auto,
            "colorspace": s.colorspace, "eotf": s.eotf, "name": s.signal_name(),
        })),
        "file_signal": {
            "matrix": setup.file.matrix, "matrix_tagged": setup.file.matrix_tagged,
            "full_range": setup.file.full_range, "range_tagged": setup.file.range_tagged,
            "primaries": setup.file.primaries, "transfer": setup.file.transfer,
            "pix_fmt": setup.file.pix_fmt, "width": setup.file.width, "height": setup.file.height,
        },
        "counts": counts,
        "plan": {
            "samples": setup.plan.samples.len(),
            "in_file": setup.plan.in_file(),
            "merged": setup.plan.merged,
            "skipped_disabled": setup.plan.skipped_disabled,
            "skipped_bad": setup.plan.skipped_bad,
        },
        "timing": {
            "elapsed_s": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            "per_sample_ms": if total > 0 { (loop_s * 1000.0 / total as f64).round() } else { 0.0 },
            "grab_wait_ms": {
                "median": percentile(&waits, 0.5),
                "p95": percentile(&waits, 0.95),
                "max": waits.last().copied().unwrap_or(0),
            },
        },
        "diagnostics": diag,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::process::standalone_job;
    use crate::engine::testenv;
    use crate::engine::timeline_compare::tests::Fixture;
    use std::process::Command;
    use std::sync::Mutex;

    fn ff(args: &[&str]) {
        let out = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    fn rows_by_tc(v: &Value) -> std::collections::HashMap<String, Value> {
        v["frames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["tc"].as_str().unwrap().to_string(), r.clone()))
            .collect()
    }

    /// Plan step 5 gate, without hardware: fake Resolve + fake grabber
    /// serving the "monitor truth", against a deliverable where one clip is
    /// slipped by a frame and one has the wrong levels. Those two are flagged;
    /// the rest compare at codec-noise level; a sample outside the file is
    /// reported, not compared.
    #[test]
    fn end_to_end_with_fakes() {
        let _env = testenv::lock();
        let fx = Fixture::new("run");
        let dir = &fx.dir;
        // The truth is 72 frames at 24p (3 s). Deliverable: frames 0-23 as is,
        // 24-47 slipped by one (shows truth n+1), 48-71 with video levels
        // squeezed (a "wrong data levels" render).
        let t = fx.truth.to_string_lossy().to_string();
        let bad = dir.join("bad.mov");
        ff(&[
            "-i", &t,
            "-filter_complex",
            "[0:v]split=3[a][b][c];\
             [a]trim=start_frame=0:end_frame=24,setpts=PTS-STARTPTS[a1];\
             [b]trim=start_frame=25:end_frame=49,setpts=PTS-STARTPTS[b1];\
             [c]trim=start_frame=48:end_frame=72,setpts=PTS-STARTPTS,lutrgb=r=val*0.86+9000:g=val*0.86+9000:b=val*0.86+9000[c1];\
             [a1][b1][c1]concat=n=3:v=1[v];\
             [v]scale=out_color_matrix=bt709:out_range=limited:flags=lanczos,format=yuv422p10le[o]",
            "-map", "[o]", "-c:v", "prores_ks", "-profile:v", "3",
            "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709",
            "-color_range", "tv", "-timecode", "01:00:00:00",
            &bad.to_string_lossy(),
        ]);

        // Timeline: three 24-frame clips on V1 from 01:00:00:00, plus a V2
        // title after the end of the file.
        let tl = dir.join("timeline.json");
        std::fs::write(
            &tl,
            r#"{"project":"P","name":"Reel","start_frame":86400,"start_tc":"01:00:00:00","fps":"24",
                "tracks":[{"name":"V1","items":[
                    {"name":"good","start":86400,"duration":24},
                    {"name":"slipped","start":86424,"duration":24},
                    {"name":"levels","start":86448,"duration":24}]},
                  {"name":"V2","items":[{"name":"tail","start":86472,"duration":10}]}]}"#,
        )
        .unwrap();
        let playhead = dir.join("playhead");
        testenv::use_fake_resolve(&tl, Some(&playhead));
        let grabber = testenv::crate_dir().join("../scripts/dev/fake_scopetap_grab.py");
        std::env::set_var("VCOMPARE_GRABBER", &grabber);
        std::env::set_var("FAKE_GRAB_SOURCE", &fx.truth);
        std::env::set_var("FAKE_GRAB_START_TC", "01:00:00:00");
        std::env::set_var("FAKE_GRAB_PLAYHEAD", &playhead);
        std::env::set_var("FAKE_GRAB_SIZE", "640x360"); // SDI bigger than the file

        let progress: Mutex<Vec<Progress>> = Mutex::new(Vec::new());
        let diags: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let cb = Callbacks {
            progress: &|p| progress.lock().unwrap().push(p.clone()),
            diagnostics: &|d| *diags.lock().unwrap() = d.to_vec(),
        };
        let params = RunParams {
            file: bad.to_string_lossy().to_string(),
            tracks: vec![1, 2],
            offset: 0,
            dry_run: false,
            grab_timeout_ms: Some(3000),
            settle_ms: None,
            workers: Some(2),
            matrix_override: None,
        };
        let job = standalone_job();
        let v = run(&params, &job, &cb).unwrap();
        let rows = rows_by_tc(&v);
        for d in diags.lock().unwrap().iter() {
            eprintln!("  {d}");
        }
        let y = |tc: &str| rows[tc]["diff_y"].as_f64().unwrap_or(f64::NAN);
        for tc in ["01:00:00:00", "01:00:00:11", "01:00:00:23", "01:00:01:00", "01:00:01:11",
                   "01:00:01:23", "01:00:02:00", "01:00:02:11", "01:00:02:23"] {
            eprintln!("{tc} {} {} y={:.2}", rows[tc]["clip"], rows[tc]["status"], y(tc));
        }
        // Good clip: codec noise. Slipped and levels: far above it.
        let good = y("01:00:00:11");
        assert!(good < 20.0, "good clip {good}");
        for tc in ["01:00:01:00", "01:00:01:11", "01:00:01:23"] {
            assert!(y(tc) > good * 4.0, "slipped {tc} {}", y(tc));
        }
        for tc in ["01:00:02:00", "01:00:02:11", "01:00:02:23"] {
            assert!(y(tc) > good * 2.5, "levels {tc} {}", y(tc));
        }
        // V2 clip after the file: planned, reported, not compared.
        assert_eq!(rows["01:00:03:00"]["status"], "not_in_file");
        assert_eq!(rows["01:00:03:00"]["track"], 2);
        assert_eq!(v["counts"]["ok"], 9);
        assert_eq!(v["counts"]["not_in_file"], 3);
        assert_eq!(v["signal"]["width"], 640);
        // The seeks really went through Resolve: the playhead ends on the last compared sample.
        assert_eq!(std::fs::read_to_string(&playhead).unwrap().trim(), "01:00:02:23");
        {
            let p = progress.lock().unwrap();
            assert_eq!(p.last().map(|p| (p.done, p.total)), Some((9, 9)));
        }

        // Detection over the result flags exactly the six bad samples.
        let state = crate::engine::state::EngineState::new();
        state.apply_timeline_result(&v);
        let det = state.detect_events(&Default::default()).unwrap();
        let mut flagged: Vec<&str> = det.events.iter().map(|e| e.tc.as_str()).collect();
        flagged.sort();
        assert_eq!(
            flagged,
            vec!["01:00:01:00", "01:00:01:11", "01:00:01:23", "01:00:02:00", "01:00:02:11", "01:00:02:23"],
            "threshold {}",
            det.threshold
        );

        // Dry run: same samples, grab only.
        let dry = RunParams { dry_run: true, ..params.clone() };
        let v = run(&dry, &job, &cb).unwrap();
        // Dry runs grab every sample, in the file or not; the fake's source
        // ends with the file, so the three V2 samples past it can't be served.
        assert_eq!(v["counts"]["grabbed"], 9);
        assert_eq!(v["counts"]["grab_error"], 3);
        assert!(v["frames"].as_array().unwrap().iter().all(|r| r.get("diff_y").is_none()));

        // A grab that never settles is reported per sample, the run goes on.
        std::env::set_var("FAKE_GRAB_PLAYHEAD", dir.join("nowhere"));
        let v = run(&RunParams { grab_timeout_ms: Some(200), ..params.clone() }, &job, &cb).unwrap();
        assert_eq!(v["counts"]["grab_timeout"], 9);

        for k in ["VCOMPARE_GRABBER", "FAKE_GRAB_SOURCE", "FAKE_GRAB_START_TC", "FAKE_GRAB_PLAYHEAD", "FAKE_GRAB_SIZE"] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn warnings_for_mismatched_signal() {
        let file = FileSide {
            path: String::new(),
            fps: (24, 1),
            width: 1920,
            height: 1080,
            pix_fmt: "yuv422p10le".into(),
            bit_depth: 10,
            is_rgb: false,
            matrix: "bt709",
            full_range: false,
            matrix_tagged: true,
            range_tagged: true,
            primaries: Some("bt709".into()),
            transfer: Some("bt709".into()),
        };
        let mut sig = GrabMeta { colorspace: 709, eotf: 0, ..Default::default() };
        assert!(signal_warnings(&sig, &file).is_empty());
        sig.colorspace = 2020;
        sig.eotf = 2;
        let w = signal_warnings(&sig, &file);
        assert_eq!(w.len(), 2, "{w:?}");
    }
    /// The same kind of run through the *real* scopetap_grab, built with
    /// scopetap's no-GPU rig (a host-memory CUDA stand-in and `fake_stream`
    /// as the producer). `scripts/dev/scopetap_rig_sdi.py` plays Resolve's SDI
    /// output: it publishes the frame the fake Resolve is parked on. Opt-in:
    ///
    /// ```text
    /// # in scopetap (see tests/grab_rig/fake_stream.cpp):
    /// g++ -std=c++17 -I. tests/grab_rig/fake_stream.cpp -o $RIG/fake_stream -lrt
    /// g++ -std=c++17 -Itests/grab_rig tools/scopetap_grab.cpp -o $RIG/scopetap_grab -lrt
    /// # here:
    /// VCOMPARE_SCOPETAP_RIG=$RIG cargo test -- --ignored real_grabber_rig
    /// ```
    ///
    /// Uses the real /scopetap-stream ring name: don't run it next to a live stream.
    #[test]
    #[ignore]
    fn real_grabber_rig() {
        let Some(rig) = std::env::var_os("VCOMPARE_SCOPETAP_RIG").map(std::path::PathBuf::from) else {
            panic!("set VCOMPARE_SCOPETAP_RIG to the directory with scopetap_grab and fake_stream");
        };
        let _env = testenv::lock();
        let dir = std::env::temp_dir().join(format!("vcompare_rig_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (w, h, n) = (64usize, 16usize, 72usize);
        // Must match VALUE in scripts/dev/scopetap_rig_sdi.py.
        let value = |k: usize| 0.2 + 0.03 * (k % 20) as f64;
        // Deliverable: clip 1 right, clip 2 slipped by a frame, clip 3 with squeezed levels.
        let mut raw = Vec::with_capacity(w * h * 6 * n);
        for k in 0..n {
            let v = match k {
                0..=23 => value(k),
                24..=47 => value(k + 1),
                _ => value(k) * 0.86 + 0.137,
            };
            for y in 0..h {
                let row = if y == 0 { 1.0 } else if y == h - 1 { 0.0 } else { v };
                let code = ((row * 65535.0f64).round() as u16).to_le_bytes();
                for _ in 0..w * 3 {
                    raw.extend_from_slice(&code);
                }
            }
        }
        let rawp = dir.join("frames.rgb48");
        std::fs::write(&rawp, &raw).unwrap();
        let file = dir.join("deliverable.mov");
        ff(&[
            "-f", "rawvideo", "-pix_fmt", "rgb48le", "-video_size", &format!("{w}x{h}"), "-framerate", "24",
            "-i", &rawp.to_string_lossy(),
            "-vf", "scale=out_color_matrix=bt709:out_range=limited:flags=lanczos,format=yuv422p10le",
            "-c:v", "prores_ks", "-profile:v", "3", "-colorspace", "bt709", "-color_primaries", "bt709",
            "-color_trc", "bt709", "-timecode", "01:00:00:00", &file.to_string_lossy(),
        ]);
        let tl = dir.join("timeline.json");
        std::fs::write(
            &tl,
            r#"{"project":"P","name":"Rig","start_frame":86400,"start_tc":"01:00:00:00","fps":"24",
                "tracks":[{"name":"V1","items":[
                    {"name":"good","start":86400,"duration":24},
                    {"name":"slipped","start":86424,"duration":24},
                    {"name":"levels","start":86448,"duration":24}]}]}"#,
        )
        .unwrap();
        let playhead = dir.join("playhead");
        testenv::use_fake_resolve(&tl, Some(&playhead));

        let sdi_script = testenv::crate_dir().join("../scripts/dev/scopetap_rig_sdi.py");
        let mut sdi = Command::new("python3")
            .arg(&sdi_script)
            .arg(rig.join("fake_stream"))
            .args(["/scopetap-stream", &w.to_string(), &h.to_string()])
            .arg(&playhead)
            .args(["01:00:00:00", "24"])
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        std::env::set_var("VCOMPARE_GRABBER", rig.join("scopetap_grab"));

        let diags: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let cb = Callbacks { progress: &|_| {}, diagnostics: &|d| *diags.lock().unwrap() = d.to_vec() };
        let params = RunParams {
            file: file.to_string_lossy().to_string(),
            tracks: vec![1],
            offset: 0,
            dry_run: false,
            grab_timeout_ms: Some(3000),
            settle_ms: Some(60),
            workers: Some(2),
            matrix_override: None,
        };
        let result = run(&params, &standalone_job(), &cb);
        let _ = sdi.kill();
        let _ = sdi.wait();
        std::env::remove_var("VCOMPARE_GRABBER");
        for d in diags.lock().unwrap().iter() {
            eprintln!("  {d}");
        }
        let v = result.unwrap();
        let rows = rows_by_tc(&v);
        let mut tcs: Vec<&String> = rows.keys().collect();
        tcs.sort();
        for tc in &tcs {
            let r = &rows[*tc];
            eprintln!("{tc} {} {} y={} {}", r["clip"], r["status"], r["diff_y"], r["error"]);
        }
        assert_eq!(v["counts"]["ok"], 9, "{}", v["counts"]);
        let y = |tc: &str| rows[tc]["diff_y"].as_f64().unwrap();
        for tc in ["01:00:00:00", "01:00:00:11", "01:00:00:23"] {
            assert!(y(tc) < 3.0, "good {tc} {}", y(tc));
        }
        for tc in ["01:00:01:00", "01:00:01:11", "01:00:01:23", "01:00:02:00", "01:00:02:11", "01:00:02:23"] {
            assert!(y(tc) > 15.0, "bad {tc} {}", y(tc));
        }
        let state = crate::engine::state::EngineState::new();
        state.apply_timeline_result(&v);
        let det = state.detect_events(&Default::default()).unwrap();
        assert_eq!(det.events.len(), 6, "{:?}", det.events.iter().map(|e| &e.tc).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
