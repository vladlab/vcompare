//! SMPTE timecode math — non-drop-frame and drop-frame.
//!
//! Ported from vcompare.py's TimecodeFormat class. The drop-frame algorithm
//! follows SMPTE ST 12-1 exactly.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Timecode counting mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TcType {
    /// Non-drop-frame (integer fps: 24, 25, 30, 50, 60)
    #[serde(rename = "NDF")]
    NonDrop,
    /// Drop-frame (29.97, 59.94 — skips frame numbers to stay in sync with wall clock)
    #[serde(rename = "DF")]
    DropFrame,
}

/// Timecode format tied to a specific frame rate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimecodeFormat {
    pub fps_num: u32,
    pub fps_den: u32,
    pub tc_type: TcType,
}

impl TimecodeFormat {
    pub fn fps_float(&self) -> f64 {
        self.fps_num as f64 / self.fps_den as f64
    }

    /// Nominal (rounded-up) fps: 29.97 → 30, 59.94 → 60, 24.0 → 24
    pub fn nominal_fps(&self) -> u32 {
        let fps = self.fps_float();
        fps.ceil() as u32
    }

    /// Timecode field separator: ";" for drop-frame, ":" for NDF
    #[allow(dead_code)]
    pub fn separator(&self) -> char {
        match self.tc_type {
            TcType::DropFrame => ';',
            TcType::NonDrop => ':',
        }
    }

    /// Convert an absolute frame number to a timecode string.
    pub fn frame_to_tc(&self, frame_num: i64) -> String {
        match self.tc_type {
            TcType::DropFrame => self.frame_to_df(frame_num),
            TcType::NonDrop => self.frame_to_ndf(frame_num),
        }
    }

    fn frame_to_ndf(&self, fn_: i64) -> String {
        let nom = self.nominal_fps() as i64;
        let ff = fn_ % nom;
        let s = (fn_ / nom) % 60;
        let m = (fn_ / (nom * 60)) % 60;
        let h = fn_ / (nom * 3600);
        format!("{h:02}:{m:02}:{s:02}:{ff:02}")
    }

    fn frame_to_df(&self, fn_: i64) -> String {
        let nom = self.nominal_fps() as i64;
        let drop: i64 = if nom == 30 { 2 } else { 4 };

        let frames_per_10min = nom * 60 * 10 - drop * 9;
        let d = fn_ / frames_per_10min;
        let m = fn_ % frames_per_10min;

        let adj_frames = if m < drop {
            fn_ + drop * 9 * d
        } else {
            fn_ + drop * 9 * d + drop * ((m - drop) / (nom * 60 - drop))
        };

        let ff = adj_frames % nom;
        let s = (adj_frames / nom) % 60;
        let mi = (adj_frames / (nom * 60)) % 60;
        let h = adj_frames / (nom * 3600);
        format!("{h:02}:{mi:02}:{s:02};{ff:02}")
    }
}

/// Detect the timecode format from ffprobe JSON output.
///
/// Inspects the video stream's `r_frame_rate` to determine fps and whether
/// drop-frame counting should be used (29.97df, 59.94df).
pub fn detect_timecode_format(probe: &Value) -> Result<TimecodeFormat, String> {
    let video = find_video_stream(probe).ok_or("No video stream found")?;

    let r_fr = video
        .get("r_frame_rate")
        .and_then(|v| v.as_str())
        .unwrap_or("24/1");

    let (num, den) = parse_rational(r_fr).ok_or_else(|| {
        format!("Invalid r_frame_rate: {r_fr}")
    })?;

    let fps = num as f64 / den as f64;

    let is_drop = (den == 1001 && (num == 30000 || num == 60000))
        || (fps - 29.97).abs() < 0.01
        || (fps - 59.94).abs() < 0.01;

    let tc_type = if is_drop {
        TcType::DropFrame
    } else {
        TcType::NonDrop
    };

    Ok(TimecodeFormat {
        fps_num: num,
        fps_den: den,
        tc_type,
    })
}

/// Parse a timecode string (HH:MM:SS:FF or HH:MM:SS;FF) to an absolute frame number.
pub fn tc_string_to_frames(tc: &str, tc_fmt: &TimecodeFormat) -> i64 {
    let normalized = tc.replace(';', ":").trim().to_string();
    let parts: Vec<&str> = normalized.split(':').collect();
    if parts.len() != 4 {
        return 0;
    }

    let (h, m, s, ff) = match (
        parts[0].parse::<i64>(),
        parts[1].parse::<i64>(),
        parts[2].parse::<i64>(),
        parts[3].parse::<i64>(),
    ) {
        (Ok(h), Ok(m), Ok(s), Ok(ff)) => (h, m, s, ff),
        _ => return 0,
    };

    let nom = tc_fmt.nominal_fps() as i64;

    if tc_fmt.tc_type == TcType::DropFrame {
        let drop: i64 = if nom == 30 { 2 } else { 4 };
        let total_minutes = h * 60 + m;
        h * 3600 * nom + m * 60 * nom + s * nom + ff
            - drop * (total_minutes - total_minutes / 10)
    } else {
        h * 3600 * nom + m * 60 * nom + s * nom + ff
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Find the first video stream in ffprobe JSON output.
pub(crate) fn find_video_stream(probe: &Value) -> Option<&Value> {
    probe
        .get("streams")?
        .as_array()?
        .iter()
        .find(|s| s.get("codec_type").and_then(|v| v.as_str()) == Some("video"))
}

/// Parse a rational number string like "30000/1001" into (num, den).
pub(crate) fn parse_rational(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.split('/');
    let num = parts.next()?.parse::<u32>().ok()?;
    let den = parts.next()?.parse::<u32>().ok()?;
    Some((num, den))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ndf_24() -> TimecodeFormat {
        TimecodeFormat { fps_num: 24, fps_den: 1, tc_type: TcType::NonDrop }
    }

    fn ndf_25() -> TimecodeFormat {
        TimecodeFormat { fps_num: 25, fps_den: 1, tc_type: TcType::NonDrop }
    }

    fn df_2997() -> TimecodeFormat {
        TimecodeFormat { fps_num: 30000, fps_den: 1001, tc_type: TcType::DropFrame }
    }

    fn ndf_30() -> TimecodeFormat {
        TimecodeFormat { fps_num: 30, fps_den: 1, tc_type: TcType::NonDrop }
    }

    // ── frame_to_tc ─────────────────────────────────────────────────────

    #[test]
    fn ndf_24_frame_0() {
        assert_eq!(ndf_24().frame_to_tc(0), "00:00:00:00");
    }

    #[test]
    fn ndf_24_one_second() {
        assert_eq!(ndf_24().frame_to_tc(24), "00:00:01:00");
    }

    #[test]
    fn ndf_24_one_hour() {
        assert_eq!(ndf_24().frame_to_tc(86400), "01:00:00:00");
    }

    #[test]
    fn ndf_25_one_minute() {
        assert_eq!(ndf_25().frame_to_tc(1500), "00:01:00:00");
    }

    #[test]
    fn ndf_30_typical_tc() {
        // 01:00:00:00 at 30fps = 108000 frames
        assert_eq!(ndf_30().frame_to_tc(108000), "01:00:00:00");
        assert_eq!(ndf_30().frame_to_tc(108001), "01:00:00:01");
    }

    #[test]
    fn df_2997_frame_0() {
        assert_eq!(df_2997().frame_to_tc(0), "00:00:00;00");
    }

    #[test]
    fn df_2997_one_minute() {
        // At 29.97df, minute 0 has 1800 frames (no drop at 10th-min boundary).
        // Frame 1800 = 00:01:00;02 (;00 and ;01 are dropped at minute 1).
        assert_eq!(df_2997().frame_to_tc(1800), "00:01:00;02");
    }

    #[test]
    fn df_2997_ten_minutes() {
        // 10-minute boundary — no drop at xx:x0:00
        // 10 minutes = 17982 frames at 29.97df
        assert_eq!(df_2997().frame_to_tc(17982), "00:10:00;00");
    }

    #[test]
    fn df_2997_one_hour() {
        // 1 hour = 107892 frames at 29.97df
        assert_eq!(df_2997().frame_to_tc(107892), "01:00:00;00");
    }

    // ── tc_string_to_frames ─────────────────────────────────────────────

    #[test]
    fn tc_parse_ndf_24() {
        let tc = ndf_24();
        assert_eq!(tc_string_to_frames("00:00:00:00", &tc), 0);
        assert_eq!(tc_string_to_frames("00:00:01:00", &tc), 24);
        assert_eq!(tc_string_to_frames("01:00:00:00", &tc), 86400);
    }

    #[test]
    fn tc_parse_df_2997() {
        let tc = df_2997();
        assert_eq!(tc_string_to_frames("00:00:00;00", &tc), 0);
        assert_eq!(tc_string_to_frames("00:01:00;02", &tc), 1800);
        assert_eq!(tc_string_to_frames("00:10:00;00", &tc), 17982);
        assert_eq!(tc_string_to_frames("01:00:00;00", &tc), 107892);
    }

    #[test]
    fn tc_parse_invalid() {
        let tc = ndf_24();
        assert_eq!(tc_string_to_frames("", &tc), 0);
        assert_eq!(tc_string_to_frames("garbage", &tc), 0);
        assert_eq!(tc_string_to_frames("00:00:00", &tc), 0);
    }

    // ── Round-trip: frame → TC string → frame ───────────────────────────

    #[test]
    fn roundtrip_ndf_24() {
        let tc = ndf_24();
        for frame in [0, 1, 23, 24, 1500, 86399, 86400, 172800] {
            let s = tc.frame_to_tc(frame);
            let back = tc_string_to_frames(&s, &tc);
            assert_eq!(back, frame, "Round-trip failed for frame {frame} (tc={s})");
        }
    }

    #[test]
    fn roundtrip_ndf_25() {
        let tc = ndf_25();
        for frame in [0, 1, 24, 25, 1500, 90000] {
            let s = tc.frame_to_tc(frame);
            let back = tc_string_to_frames(&s, &tc);
            assert_eq!(back, frame, "Round-trip failed for frame {frame} (tc={s})");
        }
    }

    #[test]
    fn roundtrip_df_2997() {
        let tc = df_2997();
        // Test a range that covers multiple minute boundaries including
        // non-drop (x0:00) and drop (x1:00, x2:00, ..., x9:00) minutes.
        for frame in 0..20000 {
            let s = tc.frame_to_tc(frame);
            let back = tc_string_to_frames(&s, &tc);
            assert_eq!(back, frame, "Round-trip failed for frame {frame} (tc={s})");
        }
    }

    #[test]
    fn roundtrip_df_2997_hour_boundary() {
        let tc = df_2997();
        // Test around the 1-hour mark
        for frame in 107880..107910 {
            let s = tc.frame_to_tc(frame);
            let back = tc_string_to_frames(&s, &tc);
            assert_eq!(back, frame, "Round-trip failed for frame {frame} (tc={s})");
        }
    }

    // ── detect_timecode_format ──────────────────────────────────────────

    #[test]
    fn detect_2997_drop() {
        let probe: Value = serde_json::json!({
            "streams": [{"codec_type": "video", "r_frame_rate": "30000/1001"}]
        });
        let tc = detect_timecode_format(&probe).unwrap();
        assert_eq!(tc.fps_num, 30000);
        assert_eq!(tc.fps_den, 1001);
        assert_eq!(tc.tc_type, TcType::DropFrame);
    }

    #[test]
    fn detect_24_ndf() {
        let probe: Value = serde_json::json!({
            "streams": [{"codec_type": "video", "r_frame_rate": "24/1"}]
        });
        let tc = detect_timecode_format(&probe).unwrap();
        assert_eq!(tc.fps_num, 24);
        assert_eq!(tc.fps_den, 1);
        assert_eq!(tc.tc_type, TcType::NonDrop);
    }

    #[test]
    fn detect_5994_drop() {
        let probe: Value = serde_json::json!({
            "streams": [{"codec_type": "video", "r_frame_rate": "60000/1001"}]
        });
        let tc = detect_timecode_format(&probe).unwrap();
        assert_eq!(tc.tc_type, TcType::DropFrame);
    }
}
