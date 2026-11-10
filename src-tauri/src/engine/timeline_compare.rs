//! Timeline mode, per sample: one grabbed SDI frame against the same frame
//! of the file.
//!
//! One ffmpeg per sample. Input 0 is the file, seeked to the frame; input 1
//! is the grabbed frame as rawvideo on stdin (packed `rgb48le`, full range,
//! already interpreted from the SDI codes by scopetap). Both go to planar
//! 4:4:4 Y'CbCr at the file's bit depth (at least 10) and meet in the `psnr`
//! filter, whose per-plane MSE gives the RMS code difference, the same
//! `diff_y/u/v` the diff mode reports.
//!
//! The live side's conversion is explicit, never swscale's default (BT.601):
//!
//! ```text
//! [1:v]scale=in_range=full:out_range=<file range>:out_color_matrix=<file matrix>:flags=lanczos,format=<T>
//! ```
//!
//! with matrix and range from the file's ffprobe tags (untagged: BT.709 /
//! limited, or BT.2020 when the primaries say so). No transfer or gamut
//! conversion is applied: the monitor is assumed to have been showing the
//! deliverable's colour space. When the sizes differ, the larger side is
//! scaled down in the same `scale`, so each side is resampled once.

use std::io::{Read, Write};
use std::process::Stdio;
#[cfg(test)]
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use regex::Regex;
use serde_json::Value;

use crate::engine::pixfmt::parse_pix_fmt;
use crate::engine::probe::is_concat_manifest;
use crate::engine::process::Job;
use crate::engine::rawstats::{fps_mode_args, seek_seconds};
use crate::engine::timecode::{find_video_stream, parse_rational};

/// The file side of the comparison, from ffprobe.
#[derive(Debug, Clone)]
pub struct FileSide {
    pub path: String,
    pub fps: (u32, u32),
    pub width: u32,
    pub height: u32,
    pub pix_fmt: String,
    pub bit_depth: u32,
    pub is_rgb: bool,
    /// swscale matrix name: "bt709", "bt2020", "bt601", "smpte240m", "fcc".
    pub matrix: &'static str,
    /// True = full ("pc") range.
    pub full_range: bool,
    /// Whether matrix / range came from tags (false = defaulted).
    pub matrix_tagged: bool,
    pub range_tagged: bool,
    /// Raw tags, for the report: color_primaries / color_transfer.
    pub primaries: Option<String>,
    pub transfer: Option<String>,
}

fn tag<'a>(stream: &'a Value, key: &str) -> Option<&'a str> {
    stream
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "unknown" && *s != "unspecified" && *s != "reserved")
}

/// swscale matrix for an ffprobe `color_space`, falling back on the
/// primaries, then BT.709. Returns (matrix, tagged).
pub fn matrix_for(color_space: Option<&str>, primaries: Option<&str>) -> (&'static str, bool) {
    match color_space {
        Some("bt709") => ("bt709", true),
        Some("bt2020nc") | Some("bt2020c") => ("bt2020", true),
        Some("smpte170m") | Some("bt470bg") => ("bt601", true),
        Some("smpte240m") => ("smpte240m", true),
        Some("fcc") => ("fcc", true),
        _ => match primaries {
            Some(p) if p.starts_with("bt2020") => ("bt2020", false),
            _ => ("bt709", false),
        },
    }
}

impl FileSide {
    pub fn from_probe(path: &str, probe: &Value) -> Result<Self, String> {
        let v = find_video_stream(probe).ok_or("No video stream found")?;
        let fr = v.get("r_frame_rate").and_then(|x| x.as_str()).unwrap_or("24/1");
        let fps = parse_rational(fr).ok_or_else(|| format!("Invalid r_frame_rate: {fr}"))?;
        let pix_fmt = v.get("pix_fmt").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let parsed = parse_pix_fmt(&pix_fmt);
        let primaries = tag(v, "color_primaries").map(String::from);
        let (matrix, matrix_tagged) = matrix_for(tag(v, "color_space"), primaries.as_deref());
        let range = tag(v, "color_range");
        let (full_range, range_tagged) = if parsed.is_rgb {
            (true, true)
        } else if pix_fmt.starts_with("yuvj") {
            (true, true)
        } else {
            match range {
                Some("pc") => (true, true),
                Some("tv") => (false, true),
                _ => (false, false),
            }
        };
        Ok(Self {
            path: path.to_string(),
            fps,
            width: v.get("width").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            height: v.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            bit_depth: parsed.bit_depth,
            is_rgb: parsed.is_rgb,
            pix_fmt,
            matrix,
            full_range,
            matrix_tagged,
            range_tagged,
            primaries,
            transfer: tag(v, "color_transfer").map(String::from),
        })
    }

    /// Planar 4:4:4 at the file's depth, never below 10 (the SDI side is
    /// 10-bit). Returns (pix_fmt, bit depth).
    pub fn target(&self) -> (String, u32) {
        let d = match self.bit_depth {
            0..=10 => 10,
            11..=12 => 12,
            _ => 16,
        };
        (format!("yuv444p{d}le"), d)
    }

    /// "422" / "420" for a chroma-subsampled Y'CbCr file, `None` for 4:4:4 / RGB.
    pub fn subsampling(&self) -> Option<&'static str> {
        if self.is_rgb {
            return None;
        }
        match parse_pix_fmt(&self.pix_fmt).subsampling {
            "444" => None,
            s => Some(s),
        }
    }

    fn range_name(&self) -> &'static str {
        if self.full_range { "full" } else { "limited" }
    }

    /// "ProRes-ish yuv422p10le, bt709 limited" for diagnostics.
    pub fn describe(&self) -> String {
        format!(
            "{}x{} {}, matrix {}{}, {} range{}",
            self.width,
            self.height,
            self.pix_fmt,
            self.matrix,
            if self.matrix_tagged { "" } else { " (untagged, assumed)" },
            self.range_name(),
            if self.range_tagged { "" } else { " (untagged, assumed)" },
        )
    }
}

/// Size both sides are compared at: the smaller of the two (by pixels).
pub fn compare_size(file: (u32, u32), live: (u32, u32)) -> (u32, u32) {
    if (file.0 as u64 * file.1 as u64) <= (live.0 as u64 * live.1 as u64) {
        file
    } else {
        live
    }
}

/// The `-filter_complex` graph for one sample. `matrix_override` replaces
/// the file's matrix on the live side only (tests use it to prove a wrong
/// matrix is caught).
pub fn build_filter(
    file: &FileSide,
    live_w: u32,
    live_h: u32,
    matrix_override: Option<&str>,
    stats_path: &str,
) -> String {
    let (target, _) = file.target();
    let (cw, ch) = compare_size((file.width, file.height), (live_w, live_h));
    let size = |w: u32, h: u32| {
        if (w, h) != (cw, ch) { format!("w={cw}:h={ch}:") } else { String::new() }
    };
    let range = if file.full_range { "full" } else { "limited" };
    let matrix = matrix_override.unwrap_or(file.matrix);

    // File side: YUV stays in its own matrix and range (only chroma and bit
    // depth change); an RGB file goes to Y'CbCr with the same matrix the
    // live side uses, full range on both.
    let file_scale = if file.is_rgb {
        format!("{}in_range=full:out_range=full:out_color_matrix={matrix}", size(file.width, file.height))
    } else {
        format!("{}in_range={range}:out_range={range}", size(file.width, file.height))
    };
    let live_scale = format!(
        "{}in_range=full:out_range={range}:out_color_matrix={matrix}",
        size(live_w, live_h)
    );
    // A subsampled file has lost chroma resolution the SDI picture still
    // has. Put the live side through the same subsampling first, so the
    // chroma difference measures what changed rather than what 4:2:2 always
    // loses on hard colour edges (lossless 4:2:2: 15-21 codes RMS without
    // this, 0 with it; ProRes HQ: ~1.2 on every plane).
    let live_tail = match file.subsampling() {
        Some(sub) => format!("format={},scale=flags=lanczos,format={target}", target.replace("444", sub)),
        None => format!("format={target}"),
    };
    // `shortest=1`: one frame in on the live side, one result out (otherwise
    // framesync repeats the live frame against the file's next frames).
    format!(
        "[0:v]setpts=PTS-STARTPTS,scale={file_scale}:flags=lanczos,format={target}[file];\
         [1:v]setpts=PTS-STARTPTS,scale={live_scale}:flags=lanczos,{live_tail}[live];\
         [live][file]psnr=shortest=1:stats_file={}",
        escape_filter_path(stats_path)
    )
}

/// Quote a path for use as a filter option value.
fn escape_filter_path(p: &str) -> String {
    let mut s = String::with_capacity(p.len() + 2);
    s.push('\'');
    for c in p.chars() {
        if c == '\'' {
            s.push_str("'\\''");
        } else {
            s.push(c);
        }
    }
    s.push('\'');
    s
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Diff {
    /// RMS code difference per plane, in codes of the target bit depth.
    pub y: f64,
    pub u: f64,
    pub v: f64,
}

/// Parse the first line of a psnr stats file.
pub fn parse_psnr_line(content: &str) -> Option<Diff> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(\w+):([\d.]+|inf|nan)").unwrap());
    let line = content.lines().find(|l| l.contains("mse_y"))?;
    let mut y = None;
    let mut u = None;
    let mut v = None;
    for c in re.captures_iter(line) {
        let val: f64 = c[2].parse().unwrap_or(0.0);
        match &c[1] {
            "mse_y" => y = Some(val),
            "mse_u" => u = Some(val),
            "mse_v" => v = Some(val),
            _ => {}
        }
    }
    let rms = |m: f64| if m > 0.0 { (m.sqrt() * 10000.0).round() / 10000.0 } else { 0.0 };
    Some(Diff { y: rms(y?), u: rms(u?), v: rms(v?) })
}

static STATS_SEQ: AtomicU64 = AtomicU64::new(0);

/// Compare one grabbed frame (`live`, packed rgb48le `live_w`×`live_h`)
/// against `file_frame` of the file.
pub fn compare_one(
    file: &FileSide,
    file_frame: i64,
    live: Arc<Vec<u8>>,
    live_w: u32,
    live_h: u32,
    matrix_override: Option<&str>,
    job: &Arc<Job>,
) -> Result<Diff, String> {
    let need = live_w as usize * live_h as usize * 6;
    if live.len() != need {
        return Err(format!("live frame is {} bytes, expected {need}", live.len()));
    }
    let stats = std::env::temp_dir().join(format!(
        "vcompare_tl_{}_{}.txt",
        std::process::id(),
        STATS_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let stats_s = stats.to_string_lossy().to_string();
    struct Rm(std::path::PathBuf);
    impl Drop for Rm {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _rm = Rm(stats.clone());

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-noautorotate".into(),
    ];
    if file_frame > 0 {
        args.extend(["-ss".into(), format!("{:.6}", seek_seconds(file_frame as u64, file.fps))]);
    }
    if is_concat_manifest(&file.path) {
        args.extend(["-f".into(), "concat".into(), "-safe".into(), "0".into()]);
    }
    args.extend(["-i".into(), file.path.clone()]);
    args.extend([
        "-f".into(), "rawvideo".into(),
        "-pix_fmt".into(), "rgb48le".into(),
        "-video_size".into(), format!("{live_w}x{live_h}"),
        "-framerate".into(), format!("{}/{}", file.fps.0, file.fps.1),
        "-i".into(), "pipe:0".into(),
    ]);
    args.extend([
        "-filter_complex".into(),
        build_filter(file, live_w, live_h, matrix_override, &stats_s),
        "-an".into(),
        "-sn".into(),
    ]);
    args.extend(fps_mode_args());
    args.extend(["-frames:v".into(), "1".into(), "-f".into(), "null".into(), "-".into()]);

    let mut child = crate::engine::sysenv::command("ffmpeg")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("ffmpeg stdin")?;
    let mut stderr = child.stderr.take().ok_or("ffmpeg stderr")?;
    let id = job.attach(child).ok_or("Analysis cancelled")?;

    let writer = std::thread::spawn(move || {
        // ffmpeg may stop reading early on an error; that shows in its exit.
        let _ = stdin.write_all(&live);
    });
    let mut err = String::new();
    let _ = stderr.read_to_string(&mut err);
    let _ = writer.join();
    let status = match job.detach(id) {
        Some(mut c) => c.wait().map_err(|e| format!("Failed to wait for ffmpeg: {e}"))?,
        None => return Err("Analysis cancelled".into()),
    };
    if !status.success() {
        return Err(format!("ffmpeg failed: {}", err.trim()));
    }
    let content = std::fs::read_to_string(&stats).unwrap_or_default();
    parse_psnr_line(&content).ok_or_else(|| {
        if err.trim().is_empty() {
            format!("no frame {file_frame} in the file")
        } else {
            format!("no frame {file_frame} in the file: {}", err.trim())
        }
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::engine::probe::ffprobe_json;
    use crate::engine::process::standalone_job;
    use std::path::{Path, PathBuf};

    fn ff(args: &[&str]) {
        let out = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(args)
            .output()
            .expect("ffmpeg");
        assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// Frame `n` of `path` as packed rgb48le at `w`×`h`.
    pub fn rgb48_frame(path: &Path, n: u64, fps: (u32, u32), w: u32, h: u32) -> Vec<u8> {
        let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
        if n > 0 {
            args.extend(["-ss".into(), format!("{:.6}", seek_seconds(n, fps))]);
        }
        args.extend(["-i".into(), path.to_string_lossy().to_string()]);
        args.extend(fps_mode_args());
        args.extend([
            "-frames:v".into(), "1".into(),
            "-vf".into(), format!("scale={w}:{h}:flags=lanczos,format=rgb48le"),
            "-f".into(), "rawvideo".into(), "pipe:1".into(),
        ]);
        let out = Command::new("ffmpeg").args(&args).output().unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), (w * h * 6) as usize);
        out.stdout
    }

    /// The "monitor truth" (lossless RGB, noisy so every frame differs from
    /// its neighbours everywhere) and deliverables encoded from it.
    pub struct Fixture {
        pub dir: PathBuf,
        pub truth: PathBuf,
        pub d709: PathBuf,
        pub d2020: PathBuf,
    }

    impl Fixture {
        pub fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("vcompare_tlc_{tag}_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let truth = dir.join("truth.nut");
            let d709 = dir.join("d709.mov");
            let d2020 = dir.join("d2020.mov");
            let t = truth.to_string_lossy().to_string();
            ff(&[
                "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=24:duration=3,noise=alls=30:allf=t",
                "-vf", "format=rgb48le", "-c:v", "rawvideo", "-f", "nut", &t,
            ]);
            let encode = |m: &str, space: &str, prim: &str, out: &Path| {
                ff(&[
                    "-i", &t,
                    "-vf", &format!("scale=out_color_matrix={m}:out_range=limited:flags=lanczos,format=yuv422p10le"),
                    "-c:v", "prores_ks", "-profile:v", "3",
                    "-color_primaries", prim, "-color_trc", "bt709", "-colorspace", space,
                    "-color_range", "tv", "-timecode", "01:00:00:00",
                    &out.to_string_lossy(),
                ]);
            };
            encode("bt709", "bt709", "bt709", &d709);
            encode("bt2020", "bt2020nc", "bt2020", &d2020);
            Fixture { dir, truth, d709, d2020 }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn side(p: &Path) -> FileSide {
        let s = p.to_string_lossy().to_string();
        FileSide::from_probe(&s, &ffprobe_json(&s).unwrap()).unwrap()
    }

    #[test]
    fn matrix_and_range_from_tags() {
        assert_eq!(matrix_for(Some("bt709"), None), ("bt709", true));
        assert_eq!(matrix_for(Some("bt2020nc"), None), ("bt2020", true));
        assert_eq!(matrix_for(Some("smpte170m"), None), ("bt601", true));
        assert_eq!(matrix_for(None, Some("bt2020")), ("bt2020", false));
        assert_eq!(matrix_for(None, None), ("bt709", false));
    }

    #[test]
    fn filter_graph_shapes() {
        let mut f = FileSide {
            path: "x.mov".into(),
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
            primaries: None,
            transfer: None,
        };
        // UHD SDI vs HD file: the live side is scaled down, the file is not.
        let g = build_filter(&f, 3840, 2160, None, "/tmp/s.txt");
        assert!(g.contains("[0:v]setpts=PTS-STARTPTS,scale=in_range=limited:out_range=limited:flags=lanczos,format=yuv444p10le[file]"), "{g}");
        assert!(g.contains("[1:v]setpts=PTS-STARTPTS,scale=w=1920:h=1080:in_range=full:out_range=limited:out_color_matrix=bt709:flags=lanczos,format=yuv422p10le,scale=flags=lanczos,format=yuv444p10le[live]"), "{g}");
        assert!(g.ends_with("psnr=shortest=1:stats_file='/tmp/s.txt'"), "{g}");
        // Same size, 2020 full-range 12-bit file.
        f.matrix = "bt2020";
        f.full_range = true;
        f.bit_depth = 12;
        f.pix_fmt = "yuv444p12le".into();
        let g = build_filter(&f, 1920, 1080, None, "/tmp/s.txt");
        assert!(g.contains("scale=in_range=full:out_range=full:out_color_matrix=bt2020:flags=lanczos,format=yuv444p12le[live]"), "{g}");
        // UHD file, HD SDI: the file is scaled down.
        f.width = 3840;
        f.height = 2160;
        let g = build_filter(&f, 1920, 1080, Some("bt601"), "/tmp/s.txt");
        assert!(g.contains("[0:v]setpts=PTS-STARTPTS,scale=w=1920:h=1080:in_range=full"), "{g}");
        assert!(g.contains("out_color_matrix=bt601"), "{g}");
        // RGB file: both sides to Y'CbCr with the same matrix, full range.
        f.is_rgb = true;
        let g = build_filter(&f, 3840, 2160, None, "/tmp/s.txt");
        assert!(g.contains("[0:v]setpts=PTS-STARTPTS,scale=in_range=full:out_range=full:out_color_matrix=bt2020"), "{g}");
        assert_eq!(escape_filter_path("/a b/it's.txt"), "'/a b/it'\\''s.txt'");
    }

    #[test]
    fn psnr_line() {
        let d = parse_psnr_line("n:1 mse_avg:4.00 mse_y:4.00 mse_u:1.00 mse_v:0.00 psnr_avg:inf\n").unwrap();
        assert_eq!(d, Diff { y: 2.0, u: 1.0, v: 0.0 });
        assert!(parse_psnr_line("").is_none());
    }

    /// Plan step 4 gate (synthetic half): the same picture compares at codec
    /// noise level for 709 and 2020 files; a one-frame slip, a wrong matrix
    /// and a frame past the end are all caught.
    #[test]
    fn synthetic_compare() {
        let fx = Fixture::new("cmp");
        let job = standalone_job();
        let f709 = side(&fx.d709);
        let f2020 = side(&fx.d2020);
        assert_eq!((f709.matrix, f709.matrix_tagged, f709.full_range), ("bt709", true, false));
        assert_eq!(f2020.matrix, "bt2020");
        assert_eq!(f709.target(), ("yuv444p10le".to_string(), 10));

        let live = |n: u64| Arc::new(rgb48_frame(&fx.truth, n, (24, 1), 320, 180));
        let cmp = |f: &FileSide, file_frame: i64, l: Arc<Vec<u8>>, m: Option<&str>| {
            compare_one(f, file_frame, l, 320, 180, m, &job).unwrap()
        };

        let right = cmp(&f709, 10, live(10), None);
        let slip = cmp(&f709, 10, live(11), None);
        let wrong_m = cmp(&f709, 10, live(10), Some("bt601"));
        let right0 = cmp(&f709, 0, live(0), None);
        let r2020 = cmp(&f2020, 10, live(10), None);
        let w2020 = cmp(&f2020, 10, live(10), Some("bt709"));
        eprintln!("right {right:?}\nslip {slip:?}\nwrong matrix {wrong_m:?}\nframe0 {right0:?}\n2020 {r2020:?}\n2020 as 709 {w2020:?}");

        // A slipped frame is far worse than codec noise (noise changes every frame).
        assert!(slip.y > right.y * 4.0, "slip {slip:?} vs right {right:?}");
        // A wrong matrix is caught on luma.
        assert!(wrong_m.y > right.y * 3.0, "wrong matrix {wrong_m:?} vs {right:?}");
        assert!(w2020.y > r2020.y * 1.8, "2020 as 709 {w2020:?} vs {r2020:?}");
        // The 2020 file compares as well as the 709 one when read with its own tags.
        assert!((r2020.y - right.y).abs() < right.y * 0.25, "{r2020:?} vs {right:?}");
        assert!((right0.y - right.y).abs() < right.y * 0.25, "frame 0 seek path");

        // The live frame bigger than the file (UHD SDI over an HD deliverable).
        let big = Arc::new(rgb48_frame(&fx.truth, 10, (24, 1), 640, 360));
        let scaled = compare_one(&f709, 10, big, 640, 360, None, &job).unwrap();
        assert!(scaled.y < slip.y / 2.0, "scaled {scaled:?} vs slip {slip:?}");

        // Past the end of the file (3 s = 72 frames).
        let e = compare_one(&f709, 500, live(10), 320, 180, None, &job).unwrap_err();
        assert!(e.contains("no frame 500"), "{e}");
        // A short live buffer is refused before ffmpeg runs.
        assert!(compare_one(&f709, 0, Arc::new(vec![0; 10]), 320, 180, None, &job).is_err());
    }

    /// A smooth picture through ProRes HQ: the baseline is a couple of codes,
    /// chroma included (the live side goes through 4:2:2 like the file).
    #[test]
    fn smooth_picture_baseline() {
        let dir = std::env::temp_dir().join(format!("vcompare_tlc_smooth_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let truth = dir.join("t.nut");
        let file = dir.join("f.mov");
        ff(&["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=24:duration=1", "-vf", "format=rgb48le",
             "-c:v", "rawvideo", "-f", "nut", &truth.to_string_lossy()]);
        ff(&["-i", &truth.to_string_lossy(), "-vf",
             "scale=out_color_matrix=bt709:out_range=limited:flags=lanczos,format=yuv422p10le",
             "-c:v", "prores_ks", "-profile:v", "3", "-colorspace", "bt709", "-color_range", "tv",
             &file.to_string_lossy()]);
        let f = side(&file);
        let job = standalone_job();
        let d = compare_one(&f, 5, Arc::new(rgb48_frame(&truth, 5, (24, 1), 640, 360)), 640, 360, None, &job).unwrap();
        eprintln!("smooth baseline {d:?}");
        assert!(d.y < 3.0 && d.u < 3.0 && d.v < 3.0, "{d:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
