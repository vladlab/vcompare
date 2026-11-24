//! Pixel format parsing and negotiation for ffmpeg filter chains.
//!
//! Ported from vcompare.py's `_parse_pix_fmt` and `choose_common_pixfmt`.
//! Determines the cheapest common pixel format when ref and distorted files
//! have different formats (subsampling, bit depth, RGB vs YUV).

/// Parsed representation of an ffmpeg pixel format.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPixFmt {
    /// Chroma subsampling: "420", "422", or "444"
    pub subsampling: &'static str,
    /// Bit depth per component: 8, 10, 12, or 16
    pub bit_depth: u32,
    /// Whether the source is an RGB family format
    pub is_rgb: bool,
}

/// Subsampling rank: higher = more chroma data preserved.
fn subsamp_rank(s: &str) -> u32 {
    match s {
        "420" => 0,
        "422" => 1,
        "444" => 2,
        _ => 0,
    }
}

/// Parse an ffmpeg pix_fmt string into its components.
///
/// Handles YUV planar (yuv420p, yuv422p10le, etc.), NV12/NV21,
/// and RGB family formats (rgb24, gbrp10le, bgra, etc.).
pub fn parse_pix_fmt(fmt: &str) -> ParsedPixFmt {
    let f = fmt.to_lowercase();
    let f = f.trim();

    // RGB family → treat as 444 equivalent
    if f.starts_with("rgb")
        || f.starts_with("bgr")
        || f.starts_with("gbr")
        || f.starts_with("gbrp")
        || f.starts_with("rgba")
        || f.starts_with("bgra")
    {
        let bits = if f.contains("48") || f.contains("16") {
            16
        } else if f.contains("10") {
            10
        } else if f.contains("12") {
            12
        } else {
            8
        };
        return ParsedPixFmt {
            subsampling: "444",
            bit_depth: bits,
            is_rgb: true,
        };
    }

    // Detect bit depth from suffix
    let bits = if f.contains("p16") || f.contains("16le") || f.contains("16be") {
        16
    } else if f.contains("p12") || f.contains("12le") || f.contains("12be") {
        12
    } else if f.contains("p10") || f.contains("10le") || f.contains("10be") {
        10
    } else {
        8
    };

    // Detect subsampling from name
    let sub = if f.contains("444") {
        "444"
    } else if f.contains("422") {
        "422"
    } else if f.contains("nv12") || f.contains("nv21") || f.contains("420") {
        "420"
    } else {
        // Fallback: most planar formats without a number are 420
        "420"
    };

    ParsedPixFmt {
        subsampling: sub,
        bit_depth: bits,
        is_rgb: false,
    }
}

/// Result of pixel format negotiation.
#[derive(Debug, Clone, PartialEq)]
pub struct PixFmtNegotiation {
    /// The target format both streams will be converted to
    pub target: String,
    /// Format filter value for reference (None = no conversion needed)
    pub ref_convert: Option<String>,
    /// Format filter value for distorted (None = no conversion needed)
    pub dist_convert: Option<String>,
}

/// Choose a common pixel format for libvmaf/PSNR comparison.
///
/// Strategy: use the LOWER subsampling and LOWER bit depth between the two.
/// For comparison analysis, both streams just need to match — downconverting
/// is fine since VMAF/PSNR measure relative differences.
pub fn choose_common_pixfmt(ref_fmt: &str, dist_fmt: &str) -> PixFmtNegotiation {
    // If identical, no conversion needed
    if ref_fmt == dist_fmt {
        return PixFmtNegotiation {
            target: ref_fmt.to_string(),
            ref_convert: None,
            dist_convert: None,
        };
    }

    let ref_parsed = parse_pix_fmt(ref_fmt);
    let dist_parsed = parse_pix_fmt(dist_fmt);

    // Pick the lower of each dimension (cheaper conversion, same comparison)
    let target_sub = if subsamp_rank(ref_parsed.subsampling) <= subsamp_rank(dist_parsed.subsampling)
    {
        ref_parsed.subsampling
    } else {
        dist_parsed.subsampling
    };
    let target_bits = ref_parsed.bit_depth.min(dist_parsed.bit_depth);

    // Construct target format name (always YUV planar for libvmaf)
    let target = if target_bits == 8 {
        format!("yuv{target_sub}p")
    } else {
        format!("yuv{target_sub}p{target_bits}le")
    };

    // Determine which streams need conversion
    let ref_native = if ref_parsed.bit_depth == 8 {
        format!("yuv{}p", ref_parsed.subsampling)
    } else {
        format!("yuv{}p{}le", ref_parsed.subsampling, ref_parsed.bit_depth)
    };
    let ref_convert = if ref_parsed.is_rgb || ref_native != target {
        Some(target.clone())
    } else {
        None
    };

    let dist_native = if dist_parsed.bit_depth == 8 {
        format!("yuv{}p", dist_parsed.subsampling)
    } else {
        format!(
            "yuv{}p{}le",
            dist_parsed.subsampling, dist_parsed.bit_depth
        )
    };
    let dist_convert = if dist_parsed.is_rgb || dist_native != target {
        Some(target.clone())
    } else {
        None
    };

    PixFmtNegotiation {
        target,
        ref_convert,
        dist_convert,
    }
}

/// Choose a common pixel format for diff (pixel difference) analysis.
///
/// **Opposite strategy from VMAF**: use the HIGHER subsampling (always 444) and
/// HIGHER bit depth. For diff analysis, we want to preserve all information from
/// both streams so the diff only reflects actual content changes, not conversion artifacts.
///
/// Upconversion is cheap and deterministic:
///   - Chroma 420→444 or 422→444: interpolation (sub-pixel, deterministic)
///   - Bit depth 8→10 or 10→12: left-shift (lossless)
///   - RGB→YUV: matrix conversion (~0.5 LSB rounding at 12-bit, negligible)
pub fn choose_diff_pixfmt(ref_fmt: &str, dist_fmt: &str) -> String {
    let ref_parsed = parse_pix_fmt(ref_fmt);
    let dist_parsed = parse_pix_fmt(dist_fmt);

    // If identical and already YUV, use as-is
    if ref_fmt == dist_fmt && !ref_parsed.is_rgb {
        return ref_fmt.to_string();
    }

    // For diff: always go to highest subsampling and highest bit depth.
    // If either stream is RGB or they differ in subsampling, force 444
    // to avoid any chroma information loss.
    let target_sub = if ref_parsed.is_rgb
        || dist_parsed.is_rgb
        || ref_parsed.subsampling != dist_parsed.subsampling
    {
        "444"
    } else {
        ref_parsed.subsampling
    };

    let target_bits = ref_parsed.bit_depth.max(dist_parsed.bit_depth);

    if target_bits == 8 {
        format!("yuv{target_sub}p")
    } else {
        format!("yuv{target_sub}p{target_bits}le")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_yuv420p() {
        let p = parse_pix_fmt("yuv420p");
        assert_eq!(p.subsampling, "420");
        assert_eq!(p.bit_depth, 8);
        assert!(!p.is_rgb);
    }

    #[test]
    fn parse_yuv422p10le() {
        let p = parse_pix_fmt("yuv422p10le");
        assert_eq!(p.subsampling, "422");
        assert_eq!(p.bit_depth, 10);
        assert!(!p.is_rgb);
    }

    #[test]
    fn parse_yuv444p16le() {
        let p = parse_pix_fmt("yuv444p16le");
        assert_eq!(p.subsampling, "444");
        assert_eq!(p.bit_depth, 16);
        assert!(!p.is_rgb);
    }

    #[test]
    fn parse_nv12() {
        let p = parse_pix_fmt("nv12");
        assert_eq!(p.subsampling, "420");
        assert_eq!(p.bit_depth, 8);
        assert!(!p.is_rgb);
    }

    #[test]
    fn parse_gbrp10le() {
        let p = parse_pix_fmt("gbrp10le");
        assert_eq!(p.subsampling, "444");
        assert_eq!(p.bit_depth, 10);
        assert!(p.is_rgb);
    }

    #[test]
    fn parse_rgb24() {
        let p = parse_pix_fmt("rgb24");
        assert_eq!(p.subsampling, "444");
        assert_eq!(p.bit_depth, 8);
        assert!(p.is_rgb);
    }

    #[test]
    fn parse_rgb48le() {
        let p = parse_pix_fmt("rgb48le");
        assert_eq!(p.subsampling, "444");
        assert_eq!(p.bit_depth, 16);
        assert!(p.is_rgb);
    }

    #[test]
    fn negotiate_identical() {
        let r = choose_common_pixfmt("yuv420p", "yuv420p");
        assert_eq!(r.target, "yuv420p");
        assert!(r.ref_convert.is_none());
        assert!(r.dist_convert.is_none());
    }

    #[test]
    fn negotiate_10bit_vs_8bit() {
        // 10-bit ref, 8-bit dist → target 8-bit (downconvert ref)
        let r = choose_common_pixfmt("yuv422p10le", "yuv422p");
        assert_eq!(r.target, "yuv422p");
        assert_eq!(r.ref_convert.as_deref(), Some("yuv422p"));
        assert!(r.dist_convert.is_none());
    }

    #[test]
    fn negotiate_422_vs_420() {
        // 422 ref, 420 dist → target 420 (downconvert ref)
        let r = choose_common_pixfmt("yuv422p", "yuv420p");
        assert_eq!(r.target, "yuv420p");
        assert_eq!(r.ref_convert.as_deref(), Some("yuv420p"));
        assert!(r.dist_convert.is_none());
    }

    #[test]
    fn negotiate_rgb_vs_yuv() {
        // RGB ref (treated as 444/8) vs yuv420p dist → target yuv420p
        let r = choose_common_pixfmt("rgb24", "yuv420p");
        assert_eq!(r.target, "yuv420p");
        assert_eq!(r.ref_convert.as_deref(), Some("yuv420p"));
        assert!(r.dist_convert.is_none());
    }

    #[test]
    fn negotiate_mixed_everything() {
        // gbrp10le (rgb, 444, 10bit) vs yuv420p (yuv, 420, 8bit) → yuv420p
        let r = choose_common_pixfmt("gbrp10le", "yuv420p");
        assert_eq!(r.target, "yuv420p");
        assert_eq!(r.ref_convert.as_deref(), Some("yuv420p"));
        assert!(r.dist_convert.is_none());
    }
}
