//! Image match: frames that look like a reference picture (Resolve's Media
//! Offline, Avid's offline slide, a slate, a missing-render card).
//!
//! Works on the per-frame signatures of a levels analysis
//! (`engine::signatures`): 32×18 luma and 16×9 chroma grids. Each plane is
//! compared by zero-mean normalized correlation, so the gain, offset and
//! most of a tone curve that a card picks up on its way into a deliverable
//! (SDR still vs PQ file, legal vs full range, a display LUT) leave the
//! score high. A plane with almost no variance in the reference (a flat
//! background, neutral chroma) has nothing to correlate; it is compared by
//! mean absolute difference instead, with a tight tolerance. A reference
//! that is flat in every plane is therefore a pure colour match.
//!
//! Score per frame = 0.6·Y + 0.2·U + 0.2·V, each plane 0..1 (negative
//! correlation counts as 0). Runs of consecutive frames at or above
//! `min_similarity` become one event each.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::*;
use crate::signatures::{Signatures, SIG_C_LEN, SIG_LEN, SIG_Y_LEN};

/// A reference picture, as the detector needs it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    pub id: String,
    pub name: String,
    /// SIG_LEN bytes.
    pub sig: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchParams {
    /// Ids of the references to look for.
    #[serde(default)]
    pub references: Vec<String>,
    /// Minimum similarity, 0..1.
    #[serde(default = "default_min_similarity")]
    pub min_similarity: f64,
    /// Fewest consecutive matching frames for an event.
    #[serde(default = "default_min_run")]
    pub min_run: u32,
}

fn default_min_similarity() -> f64 {
    0.9
}
fn default_min_run() -> u32 {
    1
}

impl Default for MatchParams {
    fn default() -> Self {
        Self { references: Vec::new(), min_similarity: default_min_similarity(), min_run: default_min_run() }
    }
}

/// Standard deviation (in 8-bit signature codes) under which a reference
/// plane counts as flat.
const FLAT_STD: f64 = 2.5;
/// Mean |Δ| (8-bit signature codes) at which a flat plane scores 0.
const FLAT_TOL: f64 = 20.0;
const WEIGHTS: [f64; 3] = [0.6, 0.2, 0.2];

fn planes(sig: &[u8]) -> [&[u8]; 3] {
    [
        &sig[..SIG_Y_LEN],
        &sig[SIG_Y_LEN..SIG_Y_LEN + SIG_C_LEN],
        &sig[SIG_Y_LEN + SIG_C_LEN..SIG_LEN],
    ]
}

/// A reference plane, prepared once: mean-removed values and their norm,
/// or the raw values when the plane is flat.
struct PreparedPlane {
    flat: bool,
    centred: Vec<f64>,
    norm: f64,
    raw: Vec<f64>,
}

fn prepare(p: &[u8]) -> PreparedPlane {
    let n = p.len() as f64;
    let raw: Vec<f64> = p.iter().map(|&x| x as f64).collect();
    let mean = raw.iter().sum::<f64>() / n;
    let centred: Vec<f64> = raw.iter().map(|x| x - mean).collect();
    let ss: f64 = centred.iter().map(|x| x * x).sum();
    PreparedPlane { flat: (ss / n).sqrt() < FLAT_STD, centred, norm: ss.sqrt(), raw }
}

fn plane_score(r: &PreparedPlane, f: &[u8]) -> f64 {
    let n = f.len() as f64;
    if r.flat {
        let mad = r.raw.iter().zip(f).map(|(a, &b)| (a - b as f64).abs()).sum::<f64>() / n;
        return (1.0 - mad / FLAT_TOL).clamp(0.0, 1.0);
    }
    let mean = f.iter().map(|&x| x as f64).sum::<f64>() / n;
    let mut dot = 0.0;
    let mut ss = 0.0;
    for (a, &b) in r.centred.iter().zip(f) {
        let c = b as f64 - mean;
        dot += a * c;
        ss += c * c;
    }
    if ss <= 0.0 || r.norm <= 0.0 {
        return 0.0;
    }
    (dot / (r.norm * ss.sqrt())).max(0.0)
}

/// A reference prepared for scoring many frames.
pub struct Prepared {
    planes: [PreparedPlane; 3],
}

impl Prepared {
    pub fn new(sig: &[u8]) -> Prepared {
        let [y, u, v] = planes(sig);
        Prepared { planes: [prepare(y), prepare(u), prepare(v)] }
    }

    /// Is every plane flat (a pure colour)?
    pub fn is_flat(&self) -> bool {
        self.planes.iter().all(|p| p.flat)
    }

    pub fn score(&self, frame_sig: &[u8]) -> f64 {
        let fp = planes(frame_sig);
        (0..3).map(|k| WEIGHTS[k] * plane_score(&self.planes[k], fp[k])).sum()
    }
}

/// Similarity of two signatures, 0..1 (reference first: its flat planes
/// decide how each plane is compared).
#[cfg(test)]
pub fn similarity(reference: &[u8], frame: &[u8]) -> f64 {
    Prepared::new(reference).score(frame)
}

/// Find runs of frames matching any of `refs`.
///
/// `frames` supplies frame numbers and timecodes; each must have a
/// signature in `sigs`.
pub fn detect_matches(
    frames: &[Value],
    sigs: &Signatures,
    refs: &[Reference],
    params: &MatchParams,
) -> Result<DetectResult, String> {
    let min_sim = params.min_similarity.clamp(0.01, 1.0);
    let min_run = params.min_run.max(1) as usize;
    let mut events = Vec::new();
    for r in refs {
        if r.sig.len() != SIG_LEN {
            return Err(format!("Reference \"{}\" has an unusable signature", r.name));
        }
        let prep = Prepared::new(&r.sig);
        let scores: Vec<Option<f64>> = frames
            .iter()
            .map(|f| sigs.get(frame_num(f)).map(|s| prep.score(s)))
            .collect();
        let hit = |i: usize| scores[i].is_some_and(|s| s >= min_sim);
        let mut i = 0;
        while i < frames.len() {
            if !hit(i) {
                i += 1;
                continue;
            }
            let a = i;
            while i < frames.len() && hit(i) {
                i += 1;
            }
            if i - a < min_run {
                continue;
            }
            let best = scores[a..i].iter().flatten().cloned().fold(0.0, f64::max);
            events.push(Event {
                frame: frame_num(&frames[a]),
                tc: frame_tc(&frames[a]),
                length: (i - a) as u32,
                kind: "match".into(),
                subkind: r.id.clone(),
                at_cut: false,
                score: round3(best / min_sim),
                review: None,
                label: Some(r.name.clone()),
            });
        }
    }
    events.sort_by_key(|e| e.frame);
    Ok(DetectResult {
        events,
        threshold: round4(min_sim),
        source: "signatures".into(),
        detector: Detector::Match,
        note: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A card: dark red field with a light box in the middle (luma), red
    /// chroma with the box neutral.
    fn card() -> Vec<u8> {
        let mut s = vec![0u8; SIG_LEN];
        for y in 0..18 {
            for x in 0..32 {
                let inside = (6..12).contains(&y) && (10..22).contains(&x);
                s[y * 32 + x] = if inside { 200 } else { 50 };
            }
        }
        for y in 0..9 {
            for x in 0..16 {
                let inside = (3..6).contains(&y) && (5..11).contains(&x);
                s[SIG_Y_LEN + y * 16 + x] = if inside { 128 } else { 110 }; // U
                s[SIG_Y_LEN + SIG_C_LEN + y * 16 + x] = if inside { 128 } else { 190 }; // V
            }
        }
        s
    }

    /// A natural-ish frame: gradients.
    fn picture(seed: u32) -> Vec<u8> {
        (0..SIG_LEN)
            .map(|i| {
                let x = (i as u32).wrapping_mul(2654435761).wrapping_add(seed.wrapping_mul(40503));
                (60 + (x >> 24) % 120) as u8
            })
            .collect()
    }

    fn map(s: &[u8], f: impl Fn(f64) -> f64) -> Vec<u8> {
        s.iter().map(|&v| f(v as f64).round().clamp(0.0, 255.0) as u8).collect()
    }

    #[test]
    fn matches_itself() {
        let c = card();
        assert!((similarity(&c, &c) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn survives_gain_offset_and_curve() {
        let c = card();
        let graded = map(&c, |v| 16.0 + v * 0.85);
        assert!(similarity(&c, &graded) > 0.95);
        // a strong curve (2.4 gamma one way, PQ-ish lift the other)
        let curved = map(&c, |v| 255.0 * (v / 255.0).powf(0.6));
        assert!(similarity(&c, &curved) > 0.9, "{}", similarity(&c, &curved));
    }

    #[test]
    fn unrelated_pictures_score_low() {
        let c = card();
        for seed in 0..20 {
            let s = similarity(&c, &picture(seed));
            assert!(s < 0.5, "seed {seed}: {s}");
        }
    }

    #[test]
    fn flat_colour_reference() {
        let red: Vec<u8> = [vec![60u8; SIG_Y_LEN], vec![100u8; SIG_C_LEN], vec![200u8; SIG_C_LEN]].concat();
        let p = Prepared::new(&red);
        assert!(p.is_flat());
        assert!(p.score(&red) > 0.99);
        let blue: Vec<u8> = [vec![40u8; SIG_Y_LEN], vec![200u8; SIG_C_LEN], vec![110u8; SIG_C_LEN]].concat();
        assert!(p.score(&blue) < 0.3);
        let grey: Vec<u8> = [vec![60u8; SIG_Y_LEN], vec![128u8; SIG_C_LEN], vec![128u8; SIG_C_LEN]].concat();
        assert!(p.score(&grey) < 0.9);
    }

    #[test]
    fn runs_become_events() {
        let c = card();
        let mut data = Vec::new();
        let seq = [0u32, 1, 99, 99, 99, 2, 3, 99, 4];
        for &k in &seq {
            data.extend(if k == 99 { c.clone() } else { picture(k) });
        }
        let sigs = Signatures { first_frame: 500, data };
        let frames: Vec<Value> =
            (0..seq.len()).map(|i| json!({"frame": 500 + i as i64, "tc": format!("t{i}")})).collect();
        let refs = vec![Reference { id: "offline".into(), name: "Media Offline".into(), sig: c }];
        let r = detect_matches(&frames, &sigs, &refs, &MatchParams::default()).unwrap();
        let got: Vec<(i64, u32)> = r.events.iter().map(|e| (e.frame, e.length)).collect();
        assert_eq!(got, vec![(502, 3), (507, 1)]);
        assert_eq!(r.events[0].label.as_deref(), Some("Media Offline"));
        let p = MatchParams { min_run: 2, ..Default::default() };
        assert_eq!(detect_matches(&frames, &sigs, &refs, &p).unwrap().events.len(), 1);
    }
}
