//! Reference pictures for image match, kept in the app data directory
//! (`references/<id>.json`) so a card set up once is offered for every
//! analysis.
//!
//! Two ways in: from an image file (ffmpeg converts it into the analysed
//! file's frame size and range, then the same signature code as the
//! analysis runs over it) or from a frame of the current analysis (its
//! signature is copied out of the sidecar, so it is exact).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};

use crate::engine::detect::Reference;
use crate::engine::signatures::{self, SIG_LEN, SIG_Y_H, SIG_Y_W};

/// A stored reference picture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredReference {
    pub id: String,
    pub name: String,
    /// Where it came from: an image path, or "<analysed file> @ <timecode>".
    pub source: String,
    /// SIG_LEN bytes.
    pub sig: Vec<u8>,
    /// Small JPEG for the list, `data:image/jpeg;base64,…`.
    #[serde(default)]
    pub thumb: Option<String>,
    /// Every plane flat: matched as a pure colour.
    #[serde(default)]
    pub flat: bool,
}

impl StoredReference {
    pub fn to_reference(&self) -> Reference {
        Reference { id: self.id.clone(), name: self.name.clone(), sig: self.sig.clone() }
    }
}

/// The reference store.
pub struct ReferenceStore {
    dir: PathBuf,
}

impl ReferenceStore {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { dir: data_dir.join("references") }
    }

    pub fn list(&self) -> Vec<StoredReference> {
        let mut out: Vec<StoredReference> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|s| serde_json::from_str::<StoredReference>(&s).ok())
            .filter(|r| r.sig.len() == SIG_LEN)
            .collect();
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        out
    }

    pub fn get(&self, ids: &[String]) -> Vec<Reference> {
        self.list()
            .into_iter()
            .filter(|r| ids.contains(&r.id))
            .map(|r| r.to_reference())
            .collect()
    }

    pub fn save(&self, r: &StoredReference) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let content = serde_json::to_string_pretty(r).map_err(|e| e.to_string())?;
        crate::engine::cache::write_atomic(&self.path(&r.id)?.to_string_lossy(), content.as_bytes())
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<(), String> {
        let mut r = self
            .list()
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| format!("No reference {id}"))?;
        r.name = name.trim().to_string();
        self.save(&r)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        std::fs::remove_file(self.path(id)?).map_err(|e| format!("Failed to delete reference: {e}"))
    }

    fn path(&self, id: &str) -> Result<PathBuf, String> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(format!("bad reference id {id:?}"));
        }
        Ok(self.dir.join(format!("{id}.json")))
    }
}

/// A new, unique-enough id: time plus a counter.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("r{t:x}-{}", N.fetch_add(1, Ordering::Relaxed))
}

/// Signature of an image file as it would appear in a `width × height`
/// frame: scaled to fit, centred on black, converted to YUV with the
/// Rec.709 matrix in limited range (unless `full_range`).
pub fn signature_from_image(path: &str, width: u32, height: u32, full_range: bool) -> Result<Vec<u8>, String> {
    let (w, h) = (width.max(2) as usize, height.max(2) as usize);
    let range = if full_range { "pc" } else { "tv" };
    let vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease:flags=area:out_color_matrix=bt709:out_range={range},\
         format=yuv444p,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black"
    );
    let raw = run_ffmpeg(&[
        "-i", path, "-frames:v", "1", "-vf", &vf, "-f", "rawvideo", "-pix_fmt", "yuv444p", "pipe:1",
    ])?;
    let plane = w * h;
    if raw.len() < 3 * plane {
        return Err(format!("Could not read an image from {}", Path::new(path).display()));
    }
    let luma = signatures::grid_average(&raw[..plane], w, h, false, 255.0, SIG_Y_W, SIG_Y_H);
    Ok(signatures::build(&luma, &raw[plane..2 * plane], &raw[2 * plane..3 * plane], w, h, false, 255.0))
}

/// A 108 px high JPEG of an image file, for the reference list.
pub fn thumbnail_of_image(path: &str) -> Option<String> {
    let jpeg = run_ffmpeg(&[
        "-i", path, "-frames:v", "1", "-vf", "scale=-2:108:flags=area", "-f", "image2pipe", "-c:v", "mjpeg",
        "-q:v", "4", "pipe:1",
    ])
    .ok()?;
    (!jpeg.is_empty()).then(|| format!("data:image/jpeg;base64,{}", crate::engine::thumbs::base64(&jpeg)))
}

fn run_ffmpeg(args: &[&str]) -> Result<Vec<u8>, String> {
    let mut child = crate::engine::sysenv::command("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg: {e}"))?;
    let mut out = Vec::new();
    child.stdout.take().ok_or("ffmpeg stdout")?.read_to_end(&mut out).map_err(|e| e.to_string())?;
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("ffmpeg failed: {}", err.trim()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::detect::matcher::similarity;

    fn ffmpeg_ok() -> bool {
        crate::engine::sysenv::command("ffmpeg").arg("-version").output().is_ok()
    }

    /// Make a test card PNG with ffmpeg's own sources.
    fn make_png(dir: &Path, name: &str, src: &str) -> String {
        let p = dir.join(name).to_string_lossy().to_string();
        let ok = crate::engine::sysenv::command("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", src, "-frames:v", "1", "-y", &p])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "could not make {name}");
        p
    }

    #[test]
    fn image_signatures() {
        if !ffmpeg_ok() {
            eprintln!("ffmpeg unavailable; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("vcompare_refs_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A 16:9 card and the same card at 4:3 (pillarboxed when fitted
        // into a 16:9 frame), plus an unrelated picture.
        let card = make_png(&dir, "card.png", "testsrc2=size=640x360");
        let narrow = make_png(&dir, "narrow.png", "testsrc2=size=480x360");
        let other = make_png(&dir, "other.png", "mandelbrot=size=640x360");

        let a = signature_from_image(&card, 1920, 1080, false).unwrap();
        assert_eq!(a.len(), SIG_LEN);
        let a_full = signature_from_image(&card, 1920, 1080, true).unwrap();
        assert!(similarity(&a, &a_full) > 0.95, "range change: {}", similarity(&a, &a_full));
        let b = signature_from_image(&card, 3840, 2160, false).unwrap();
        assert!(similarity(&a, &b) > 0.98, "frame size change: {}", similarity(&a, &b));
        let c = signature_from_image(&other, 1920, 1080, false).unwrap();
        assert!(similarity(&a, &c) < 0.6, "unrelated: {}", similarity(&a, &c));
        // pillarboxed: matches itself, not the full-width card
        let n = signature_from_image(&narrow, 1920, 1080, false).unwrap();
        assert_eq!(&n[0..1], &[16]); // left pillar is legal black
        assert!(similarity(&n, &n) > 0.999);

        assert!(thumbnail_of_image(&card).unwrap().starts_with("data:image/jpeg;base64,"));
        assert!(signature_from_image(&dir.join("nope.png").to_string_lossy(), 1920, 1080, false).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_round_trip() {
        let dir = std::env::temp_dir().join(format!("vcompare_store_{}", std::process::id()));
        let store = ReferenceStore::new(dir.clone());
        assert!(store.list().is_empty());
        let r = StoredReference {
            id: new_id(),
            name: "Media Offline".into(),
            source: "x.png".into(),
            sig: vec![7; SIG_LEN],
            thumb: None,
            flat: true,
        };
        store.save(&r).unwrap();
        store.rename(&r.id, "Resolve offline").unwrap();
        let l = store.list();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].name, "Resolve offline");
        assert_eq!(store.get(std::slice::from_ref(&r.id)).len(), 1);
        assert!(store.delete("../etc").is_err());
        store.delete(&r.id).unwrap();
        assert!(store.list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End to end: a clip with an offline-style card spliced in, analysed
    /// by the levels engine, matched against a PNG of the card (made
    /// separately, RGB, another size) — found as one event, nothing else.
    #[test]
    fn finds_spliced_card_in_analysed_clip() {
        if !ffmpeg_ok() {
            eprintln!("ffmpeg unavailable; skipping");
            return;
        }
        use crate::engine::detect::matcher::detect_matches;
        use crate::engine::detect::MatchParams;
        let dir = std::env::temp_dir().join(format!("vcompare_card_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let card = "color=c=0x8a1010:s=640x360,drawbox=x=200:y=140:w=240:h=80:color=white:t=fill,\
                    drawbox=x=0:y=300:w=640:h=8:color=0x404040:t=fill";
        let png = make_png(&dir, "card.png", &card.replace("640x360", "1280x720").replace("x=200:y=140:w=240:h=80", "x=400:y=280:w=480:h=160").replace("y=300:w=640:h=8", "y=600:w=1280:h=16"));
        let clip = dir.join("reel.mov").to_string_lossy().to_string();
        let graph = format!(
            "testsrc2=s=640x360:r=24:d=2[a];{card},trim=duration=1,fps=24[b];mandelbrot=s=640x360:r=24,trim=duration=2[c];[a][b][c]concat=n=3:v=1:a=0,format=yuv422p10le"
        );
        let ok = crate::engine::sysenv::command("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-y", "-filter_complex", &graph, "-c:v", "prores_ks", "-profile:v", "3", &clip])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "could not make the clip");

        let mut d = Vec::new();
        let info = crate::engine::analysis::prepare_single_file(&clip, &mut d).unwrap();
        let (frames, _, sigs) = crate::engine::analysis::run_levels_analysis(
            &clip, &info, None, Some(1), true, None, crate::engine::process::standalone_job(), &mut d, &|_| {},
        )
        .unwrap();
        let sigs = sigs.unwrap();
        let reference = Reference {
            id: "card".into(),
            name: "card".into(),
            sig: signature_from_image(&png, info.width, info.height, info.full_range).unwrap(),
        };
        let r = detect_matches(&frames, &sigs, &[reference], &MatchParams::default()).unwrap();
        let got: Vec<(i64, u32)> = r.events.iter().map(|e| (e.frame - info.start_frame, e.length)).collect();
        eprintln!("matches: {got:?} (scores {:?})", r.events.iter().map(|e| e.score).collect::<Vec<_>>());
        assert_eq!(got, vec![(48, 24)]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
