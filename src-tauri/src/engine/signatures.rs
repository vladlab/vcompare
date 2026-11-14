//! Per-frame picture signatures, for matching frames against a reference
//! picture (Media Offline cards, slates, …).
//!
//! A signature is three tiny box-averaged planes, 8-bit, normalized to the
//! source's peak code value (so it is independent of bit depth and frame
//! size): luma on a 32×18 grid (the same grid `rawstats` uses for its
//! thumbnail differences), U and V on 16×9. 864 bytes per frame.
//!
//! Levels analyses write them to a sidecar next to the analysis JSON
//! (`X.levels.json` → `X.levels.sig`):
//!
//! ```text
//! "VCSG" | version u32 | y_w y_h c_w c_h u16 | first_frame i64 | count u64
//! count × SIG_LEN bytes, in frame order (little-endian throughout)
//! ```

use std::io::Read;

pub const SIG_Y_W: usize = 32;
pub const SIG_Y_H: usize = 18;
pub const SIG_C_W: usize = 16;
pub const SIG_C_H: usize = 9;
pub const SIG_Y_LEN: usize = SIG_Y_W * SIG_Y_H;
pub const SIG_C_LEN: usize = SIG_C_W * SIG_C_H;
/// Bytes per frame: Y, then U, then V.
pub const SIG_LEN: usize = SIG_Y_LEN + 2 * SIG_C_LEN;

const MAGIC: &[u8; 4] = b"VCSG";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 4 + 4 + 2 * 4 + 8 + 8;

/// Signatures of a run of consecutive display frames.
#[derive(Debug, Clone, PartialEq)]
pub struct Signatures {
    /// Display frame number of the first signature.
    pub first_frame: i64,
    /// `count × SIG_LEN` bytes.
    pub data: Vec<u8>,
}

impl Signatures {
    pub fn count(&self) -> usize {
        self.data.len() / SIG_LEN
    }

    /// Signature of display frame `frame`, if it is in range.
    pub fn get(&self, frame: i64) -> Option<&[u8]> {
        let i = frame - self.first_frame;
        if i < 0 || i as usize >= self.count() {
            return None;
        }
        let i = i as usize;
        Some(&self.data[i * SIG_LEN..(i + 1) * SIG_LEN])
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.data.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        for d in [SIG_Y_W, SIG_Y_H, SIG_C_W, SIG_C_H] {
            out.extend_from_slice(&(d as u16).to_le_bytes());
        }
        out.extend_from_slice(&self.first_frame.to_le_bytes());
        out.extend_from_slice(&(self.count() as u64).to_le_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    pub fn from_bytes(b: &[u8]) -> Result<Signatures, String> {
        if b.len() < HEADER_LEN || &b[0..4] != MAGIC {
            return Err("not a vcompare signature file".into());
        }
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u16_at = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) as usize;
        if u32_at(4) != VERSION {
            return Err(format!("signature file version {} not supported", u32_at(4)));
        }
        if [u16_at(8), u16_at(10), u16_at(12), u16_at(14)] != [SIG_Y_W, SIG_Y_H, SIG_C_W, SIG_C_H] {
            return Err("signature file uses a different grid".into());
        }
        let first_frame = i64::from_le_bytes(b[16..24].try_into().unwrap());
        let count = u64::from_le_bytes(b[24..32].try_into().unwrap()) as usize;
        let data = &b[HEADER_LEN..];
        if data.len() != count * SIG_LEN {
            return Err("signature file is truncated".into());
        }
        Ok(Signatures { first_frame, data: data.to_vec() })
    }

    pub fn write(&self, path: &str) -> Result<(), String> {
        crate::engine::cache::write_atomic(path, &self.to_bytes())
    }

    pub fn read(path: &str) -> Result<Signatures, String> {
        let mut f = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
        let mut b = Vec::new();
        f.read_to_end(&mut b).map_err(|e| format!("{path}: {e}"))?;
        Signatures::from_bytes(&b)
    }
}

/// `X.levels.json` → `X.levels.sig`.
pub fn sidecar_path(analysis_path: &str) -> String {
    match analysis_path.strip_suffix(".json") {
        Some(stem) => format!("{stem}.sig"),
        None => format!("{analysis_path}.sig"),
    }
}

/// Bytes a signature sidecar takes for `frames` frames.
#[cfg(test)]
pub fn sidecar_bytes(frames: u64) -> u64 {
    HEADER_LEN as u64 + frames * SIG_LEN as u64
}

/// Box-average a plane onto a `gw × gh` grid, normalized to `peak` (0..1).
pub fn grid_average(plane: &[u8], w: usize, h: usize, two_byte: bool, peak: f64, gw: usize, gh: usize) -> Vec<f32> {
    let cells = gw * gh;
    let mut sums = vec![0u64; cells];
    let mut counts = vec![0u64; cells];
    // Cell boundaries along x, so each cell's row segment is a contiguous
    // slice (which keeps the inner sum a simple vectorizable loop).
    let xb: Vec<usize> = (0..=gw).map(|tx| tx * w / gw).collect();
    let bps = if two_byte { 2 } else { 1 };
    for y in 0..h {
        let ty = y * gh / h;
        let row = &plane[y * w * bps..(y + 1) * w * bps];
        for tx in 0..gw {
            let (x0, x1) = (xb[tx], xb[tx + 1]);
            if x1 <= x0 {
                continue;
            }
            let seg = &row[x0 * bps..x1 * bps];
            let s: u64 = if two_byte {
                seg.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as u64).sum()
            } else {
                seg.iter().map(|&v| v as u64).sum()
            };
            let cell = ty * gw + tx;
            sums[cell] += s;
            counts[cell] += (x1 - x0) as u64;
        }
    }
    sums.iter()
        .zip(&counts)
        .map(|(&s, &c)| if c == 0 { 0.0 } else { (s as f64 / c as f64 / peak) as f32 })
        .collect()
}

/// Build a signature from a 32×18 luma thumbnail (0..1) and the two chroma
/// planes of the frame.
#[allow(clippy::too_many_arguments)]
pub fn build(luma_thumb: &[f32], u: &[u8], v: &[u8], cw: usize, ch: usize, two_byte: bool, peak: f64) -> Vec<u8> {
    let q = |x: f32| (x * 255.0).round().clamp(0.0, 255.0) as u8;
    let mut sig = Vec::with_capacity(SIG_LEN);
    sig.extend(luma_thumb.iter().map(|&x| q(x)));
    sig.extend(grid_average(u, cw, ch, two_byte, peak, SIG_C_W, SIG_C_H).into_iter().map(q));
    sig.extend(grid_average(v, cw, ch, two_byte, peak, SIG_C_W, SIG_C_H).into_iter().map(q));
    debug_assert_eq!(sig.len(), SIG_LEN);
    sig
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_round_trip() {
        let s = Signatures { first_frame: 86400, data: (0..3 * SIG_LEN).map(|i| (i % 251) as u8).collect() };
        let b = s.to_bytes();
        assert_eq!(b.len() as u64, sidecar_bytes(3));
        let back = Signatures::from_bytes(&b).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.get(86401).unwrap()[0], (SIG_LEN % 251) as u8);
        assert!(back.get(86403).is_none());
        assert!(back.get(86399).is_none());
        assert!(Signatures::from_bytes(&b[..b.len() - 1]).is_err());
        assert!(Signatures::from_bytes(b"nope").is_err());
    }

    #[test]
    fn sidecar_path_follows_analysis() {
        assert_eq!(sidecar_path("/a/X.levels.json"), "/a/X.levels.sig");
    }

    #[test]
    fn grid_average_two_byte() {
        // 4×2 plane, 10-bit, left half 0, right half 1023 → 2×1 grid
        let mut p = Vec::new();
        for _ in 0..2 {
            for x in 0..4u16 {
                p.extend_from_slice(&(if x < 2 { 0u16 } else { 1023 }).to_le_bytes());
            }
        }
        let g = grid_average(&p, 4, 2, true, 1023.0, 2, 1);
        assert_eq!(g, vec![0.0, 1.0]);
    }
}
