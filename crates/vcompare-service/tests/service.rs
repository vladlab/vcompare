//! The service end to end, the way a UI drives it: a levels analysis of a
//! generated clip with events collected from the sink, then queries,
//! thumbnails, cache reuse and reload. Needs ffmpeg; skips without it.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use vcompare_service::{AnalyzeParams, GetFramesParams, RecentEntry, Service, UiEvent, CANCELLED};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("vcompare_service_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 48 frames at 24 fps, 10-bit 4:2:2, flat grey ramping 64 + 8·N code values.
fn make_clip(dir: &Path) -> Option<PathBuf> {
    let clip = dir.join("ramp.mkv");
    let ok = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi",
               "-i", "color=c=black:s=128x72:r=24,format=yuv422p10le,geq=lum='64+8*N':cb=512:cr=512",
               "-frames:v", "48", "-c:v", "ffv1"])
        .arg(&clip)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then_some(clip)
}

struct Collect(Mutex<Vec<UiEvent>>);

impl Collect {
    fn names(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().iter().map(|e| e.name()).collect()
    }
}

#[test]
fn levels_through_the_service() {
    let dir = tmp("levels");
    let Some(clip) = make_clip(&dir) else {
        eprintln!("no ffmpeg; skipping");
        return;
    };
    let events = Arc::new(Collect(Mutex::new(Vec::new())));
    let sink = {
        let events = events.clone();
        move |e: UiEvent| events.0.lock().unwrap().push(e)
    };
    let svc = Service::new(dir.join("data"), Arc::new(sink));
    let clip_s = clip.to_string_lossy().to_string();

    let probe = svc.probe_file(&clip_s).unwrap();
    assert!(serde_json::to_value(&probe).unwrap().is_object());

    let cache = svc.check_levels_cache(&clip_s);
    assert_eq!(cache["exists"], false);
    let save = cache["path"].as_str().unwrap().to_string();

    let params = AnalyzeParams {
        reference: clip_s.clone(),
        mode: Some("levels".into()),
        log_path: Some(save.clone()),
        ..Default::default()
    };
    let summary = svc.analyze(&params).unwrap();
    assert_eq!(summary["mode"], "levels");
    assert!(summary.get("perf").is_some(), "a fresh run reports perf");

    // Progress and diagnostics reached the sink, with the web payloads.
    let names = events.names();
    assert!(names.contains(&"rpc:progress"), "{names:?}");
    assert!(names.contains(&"rpc:diagnostics"), "{names:?}");
    let last = events
        .0
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|e| matches!(e, UiEvent::Progress { .. }))
        .unwrap()
        .payload();
    assert_eq!(last["total"].as_u64(), Some(48), "{last}");

    // Queries.
    let frames = svc.get_frames(&GetFramesParams { limit: Some(100), ..Default::default() }).unwrap();
    let rows = frames["frames"].as_array().expect("frames array");
    assert_eq!(rows.len(), 48);
    let series = svc.get_metric_series("y_avg").unwrap();
    assert!(series.is_object());

    // Thumbnails: JPEG bytes, missing before the file's start, at most 3
    // extractions at once (12 callers).
    let strip = svc.preview_frames(-1, 3, 36).unwrap();
    assert_eq!(strip.len(), 1);
    let f = &strip[0].frames;
    assert!(f[0].jpeg.is_none());
    for t in &f[1..] {
        let j = t.jpeg.as_ref().expect("jpeg");
        assert_eq!(&j[..2], &[0xFF, 0xD8]);
    }
    let svc = Arc::new(svc);
    let handles: Vec<_> = (0..12)
        .map(|k| {
            let s = svc.clone();
            std::thread::spawn(move || s.preview_frames(k * 3, 2, 24).map(|r| r[0].frames.len()))
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap().unwrap(), 2);
    }

    // Detection runs on the loaded analysis.
    let det = svc.detect_events(&Default::default()).unwrap();
    assert!(serde_json::to_value(&det).unwrap().is_object());

    // Same params again. Existing behaviour, kept as is: `analyze` re-runs
    // levels even when its cache exists (`cache::load_diff_cache` only
    // accepts diff caches); the UI reopens levels caches through
    // `load_saved_analysis` instead.
    let again = svc.analyze(&params).unwrap();
    assert!(again.get("perf").is_some());

    // Reload as Recent Analyses does.
    let reloaded = svc.load_saved_analysis(&save).unwrap();
    assert_eq!(reloaded["mode"], "levels");
    assert_eq!(svc.get_frames(&GetFramesParams { limit: Some(100), ..Default::default() }).unwrap()["frames"]
        .as_array()
        .unwrap()
        .len(), 48);

    svc.add_recent(RecentEntry {
        ref_path: clip_s.clone(),
        dist_path: String::new(),
        cache_path: save.clone(),
        mode: "levels".into(),
        frame_count: 48,
        timestamp: 1,
    });
    assert_eq!(svc.recents().len(), 1);

    // No player is running: a clear error, not a panic.
    assert_eq!(svc.mpv_seek(3).unwrap_err(), "mpv not running");
    assert_eq!(svc.diffview_step("forward").unwrap_err(), "diffview not running");
    assert!(svc.diffview_launch().unwrap_err().contains("two files"));

    svc.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cancel_reports_cancelled() {
    let dir = tmp("cancel");
    let Some(clip) = make_clip(&dir) else {
        eprintln!("no ffmpeg; skipping");
        return;
    };
    // Cancel from the first progress callback.
    let svc: Arc<Mutex<Option<Arc<Service>>>> = Arc::new(Mutex::new(None));
    let sink = {
        let svc = svc.clone();
        move |e: UiEvent| {
            if let UiEvent::Progress { .. } = e {
                if let Some(s) = svc.lock().unwrap().as_ref() {
                    s.cancel_analysis();
                }
            }
        }
    };
    let s = Arc::new(Service::new(dir.join("data"), Arc::new(sink)));
    *svc.lock().unwrap() = Some(s.clone());
    let save = dir.join("out.vcompare.json");
    let r = s.analyze(&AnalyzeParams {
        reference: clip.to_string_lossy().to_string(),
        mode: Some("levels".into()),
        log_path: Some(save.to_string_lossy().to_string()),
        ..Default::default()
    });
    // A 48-frame clip may finish before the kill lands; when it doesn't,
    // it must say cancelled and save nothing.
    if let Err(e) = r {
        assert_eq!(e, CANCELLED);
        assert!(!save.exists());
    }
    *svc.lock().unwrap() = None;
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_file_is_an_error() {
    let dir = tmp("missing");
    let svc = Service::new(dir.join("data"), Arc::new(vcompare_service::NullSink));
    assert!(svc.load_saved_analysis("/nonexistent/x.vcompare.json").unwrap_err().contains("File not found"));
    assert!(svc.probe_file("/nonexistent/x.mov").is_err());
    let v: Value = svc.check_levels_cache("/nonexistent/x.mov");
    assert_eq!(v["exists"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn typed_accessors_match_the_json_ones() {
    let dir = tmp("typed");
    let Some(clip) = make_clip(&dir) else {
        eprintln!("no ffmpeg; skipping");
        return;
    };
    let svc = Service::new(dir.join("data"), Arc::new(vcompare_service::NullSink));
    let summary = svc
        .analyze(&AnalyzeParams {
            reference: clip.to_string_lossy().to_string(),
            mode: Some("levels".into()),
            log_path: Some(dir.join("t.levels.json").to_string_lossy().to_string()),
            ..Default::default()
        })
        .unwrap();
    let metrics: Vec<String> = summary["metrics_available"].as_array().unwrap().iter().map(|m| m.as_str().unwrap().to_string()).collect();

    for m in &metrics {
        let j = svc.get_metric_series(m).unwrap();
        let (f, v) = svc.metric_series(m).unwrap();
        let jf: Vec<i64> = j["frames"].as_array().unwrap().iter().map(|x| x.as_i64().unwrap()).collect();
        let jv: Vec<f64> = j["values"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
        assert_eq!((f, v), (jf, jv), "{m}");
    }

    let cols = svc.frame_columns(&metrics).unwrap();
    let rows = svc.get_frames(&GetFramesParams { sort: Some("frame".into()), ..Default::default() }).unwrap();
    let rows = rows["frames"].as_array().unwrap();
    assert_eq!(cols.frames.len(), rows.len());
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(cols.frames[i], r["frame"].as_i64().unwrap());
        assert_eq!(cols.tcs[i], r["tc"].as_str().unwrap());
        for (m, col) in &cols.columns {
            match r.get(m).and_then(|x| x.as_f64()) {
                Some(x) => assert_eq!(col[i], x, "{m} @ {i}"),
                None => assert!(col[i].is_nan()),
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Image match through the service: a reference from a PNG of the card
/// (at another size, as a still from Resolve would be) finds the card
/// spliced into the clip; reviews survive reopening; events export.
#[test]
fn image_reference_finds_the_card_and_events_export() {
    use vcompare_service::engine::detect::{DetectRequest, Detector, MatchParams, RepeatParams};
    let dir = tmp("refs");
    let clip = dir.join("card.mov");
    let png = dir.join("card.png");
    let font = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", "DejaVu Sans:bold"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .filter(|f| !f.is_empty());
    let Some(font) = font else {
        eprintln!("no font for drawtext; skipping");
        return;
    };
    let text = |size: u32| format!("drawtext=fontfile={font}:text='MEDIA OFFLINE':fontsize={size}:fontcolor=white:x=(w-tw)/2:y=(h-th)/2");
    let ok = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=320x180:r=24", "-frames:v", "120", "-vf"])
        .arg(format!(
            "drawbox=x=0:y=0:w=iw:h=ih:t=fill:c=0xb02020:enable='between(n,40,59)',{}:enable='between(n,40,59)',format=yuv422p10le",
            text(24)
        ))
        .args(["-c:v", "prores_ks", "-profile:v", "3"])
        .arg(&clip)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        && Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "color=c=0xb02020:s=1280x720", "-frames:v", "1", "-vf"])
            .arg(text(96))
            .arg(&png)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    if !ok {
        eprintln!("no ffmpeg; skipping");
        return;
    }
    let svc = Service::new(dir.join("data"), Arc::new(vcompare_service::NullSink));
    let save = dir.join("card.levels.json").to_string_lossy().to_string();
    let summary = svc
        .analyze(&AnalyzeParams {
            reference: clip.to_string_lossy().to_string(),
            mode: Some("levels".into()),
            log_path: Some(save.clone()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(summary["has_signatures"], true);

    let r = svc.add_reference_from_image(&png.to_string_lossy(), Some("Offline".into())).unwrap();
    assert_eq!(svc.list_references().len(), 1);
    let req = DetectRequest::Match(MatchParams { references: vec![r.id.clone()], ..Default::default() });
    let found = svc.detect_events(&req).unwrap();
    assert_eq!(found.events.len(), 1, "{:?}", found.events);
    let e = &found.events[0];
    let first = summary["ref_start_frame"].as_i64().unwrap();
    assert!((e.frame - first - 40).abs() <= 1 && (19..=21).contains(&e.length), "{e:?}");
    assert_eq!(e.label.as_deref(), Some("Offline"));

    // The card is also a hold for the repeat detector.
    let holds = svc.detect_events(&DetectRequest::Repeat(RepeatParams { include_long: true, ..Default::default() })).unwrap();
    assert!(holds.events.iter().any(|h| h.subkind == "hold" && (h.frame - e.frame).abs() <= 1), "{:?}", holds.events);

    // Review, reopen: the verdict is still there.
    svc.set_event_review(Detector::Match, e.frame, Some("accept")).unwrap();
    svc.load_saved_analysis(&save).unwrap();
    let again = svc.detect_events(&req).unwrap();
    assert_eq!(again.events[0].review.as_deref(), Some("accept"));

    // Exports.
    let edl = dir.join("m.edl").to_string_lossy().to_string();
    let csv = dir.join("m.csv").to_string_lossy().to_string();
    assert_eq!(svc.export_events_edl(Some(Detector::Match), Some(&edl)).unwrap()["rows"], 1);
    assert_eq!(svc.export_events_csv(Some(Detector::Match), Some(&csv)).unwrap()["rows"], 1);
    let edl_text = std::fs::read_to_string(&edl).unwrap();
    assert!(edl_text.contains("TITLE"), "{edl_text}");
    assert!(std::fs::read_to_string(&csv).unwrap().lines().count() >= 2);

    svc.rename_reference(&r.id, "Card").unwrap();
    assert_eq!(svc.list_references()[0].name, "Card");
    svc.delete_reference(&r.id).unwrap();
    assert!(svc.list_references().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
