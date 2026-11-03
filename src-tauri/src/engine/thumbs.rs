//! Small frame previews around an event, via ffmpeg.
//!
//! One ffmpeg run seeks to the first frame and emits `count` consecutive
//! frames as an MJPEG stream on stdout (`-f image2pipe`), which we split on
//! JPEG start-of-image markers. Limited-range sources are expanded to full
//! range so the JPEGs look right in a browser.

use std::io::Read;
use std::process::{Command, Stdio};

use crate::engine::probe::is_concat_manifest;
use crate::engine::rawstats::{fps_mode_args, seek_seconds};

/// Extract `count` frames starting at `file_frame` (0-based from the start
/// of the file), scaled to `height` px, as `data:image/jpeg;base64,…` URIs.
/// Fewer than `count` come back at the end of the file.
pub fn extract_frames(
    file_path: &str,
    fps: (u32, u32),
    file_frame: i64,
    count: u32,
    height: u32,
) -> Result<Vec<String>, String> {
    let file_frame = file_frame.max(0) as u64;
    let count = count.clamp(1, 16);
    let height = height.clamp(16, 1080);

    let mut args: Vec<String> = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-noautorotate".into(),
    ];
    if file_frame > 0 {
        args.extend(["-ss".into(), format!("{:.6}", seek_seconds(file_frame, fps))]);
    }
    if is_concat_manifest(file_path) {
        args.extend(["-f".into(), "concat".into(), "-safe".into(), "0".into()]);
    }
    args.extend(["-i".into(), file_path.into()]);
    args.extend(["-map".into(), "0:v:0".into(), "-an".into(), "-sn".into()]);
    // Without passthrough ffmpeg duplicates the first frame after a seek to
    // fill the half-frame gap to the seek point, shifting the whole strip.
    args.extend(fps_mode_args());
    args.extend([
        "-frames:v".into(), count.to_string(),
        "-vf".into(), format!("scale=-2:{height}:flags=bicubic:out_range=pc,format=yuvj422p"),
        "-f".into(), "image2pipe".into(),
        "-c:v".into(), "mjpeg".into(),
        "-q:v".into(), "4".into(),
        "pipe:1".into(),
    ]);

    let mut child = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg: {e}"))?;
    let mut out = Vec::new();
    child
        .stdout
        .take()
        .ok_or("ffmpeg stdout")?
        .read_to_end(&mut out)
        .map_err(|e| format!("read: {e}"))?;
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    if !status.success() && out.is_empty() {
        return Err(format!("ffmpeg failed: {}", err.trim()));
    }

    Ok(split_mjpeg(&out)
        .into_iter()
        .map(|jpeg| format!("data:image/jpeg;base64,{}", base64(jpeg)))
        .collect())
}

/// Split a concatenated MJPEG stream into frames at SOI markers. Inside
/// entropy-coded data `0xFF` is always followed by `0x00` or an `RSTn`
/// marker, and ffmpeg's MJPEG carries no embedded thumbnails, so `FF D8 FF`
/// only ever starts a frame.
fn split_mjpeg(data: &[u8]) -> Vec<&[u8]> {
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0xFF && data[i + 1] == 0xD8 && data[i + 2] == 0xFF {
            starts.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| &data[s..starts.get(k + 1).copied().unwrap_or(data.len())])
        .collect()
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xFF, 0xD8, 0xFF]), "/9j/");
    }

    #[test]
    fn mjpeg_split() {
        let a = [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 0xFF, 0xD9];
        let b = [0xFF, 0xD8, 0xFF, 0xDB, 3, 0xFF, 0x00, 0xFF, 0xD9];
        let mut data = a.to_vec();
        data.extend_from_slice(&b);
        let parts = split_mjpeg(&data);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], &a[..]);
        assert_eq!(parts[1], &b[..]);
        assert!(split_mjpeg(&[]).is_empty());
    }

    /// Frames come back in order and the seek lands on the right frame:
    /// a clip whose frame k is a flat grey of level 40 + 10k. Needs ffmpeg.
    #[test]
    fn extract_lands_on_the_requested_frame() {
        let dir = std::env::temp_dir().join(format!("vcompare_thumbs_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("ramp.mkv");
        let ok = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi",
                   "-i", "color=c=black:s=64x36:r=24,geq=lum='40+10*N':cb=128:cr=128",
                   "-frames:v", "20", "-c:v", "ffv1", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("no ffmpeg; skipping");
            return;
        }
        // Also as H.264 in MP4 (B-frames, edit list) — the container where a
        // naive seek duplicates the first frame.
        let mp4 = dir.join("ramp.mp4");
        let _ = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&clip)
            .args(["-c:v", "libx264", "-crf", "10", "-pix_fmt", "yuv420p"])
            .arg(&mp4)
            .status();
        for src in [&clip, &mp4] {
        if !src.exists() { continue; }
        let uris = extract_frames(&src.to_string_lossy(), (24, 1), 7, 3, 36).unwrap();
        assert_eq!(uris.len(), 3);
        // Decode each JPEG's mean luma back through ffmpeg and check the ramp.
        for (k, uri) in uris.iter().enumerate() {
            let b64 = uri.strip_prefix("data:image/jpeg;base64,").unwrap();
            let bytes = decode_b64(b64);
            let p = dir.join(format!("f{k}.jpg"));
            std::fs::write(&p, bytes).unwrap();
            let out = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-i"])
                .arg(&p)
                .args(["-vf", "scale=in_range=pc:out_range=tv,signalstats,metadata=mode=print:file=-", "-f", "null", "-"])
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            let yavg: f64 = text
                .lines()
                .find_map(|l| l.strip_prefix("lavfi.signalstats.YAVG="))
                .unwrap()
                .parse()
                .unwrap();
            let expected = 40.0 + 10.0 * (7 + k) as f64;
            assert!((yavg - expected).abs() < 4.0, "{}: frame {k}: YAVG {yavg}, expected ~{expected}", src.display());
        }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn decode_b64(s: &str) -> Vec<u8> {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let val = |c: u8| T.iter().position(|&t| t == c).unwrap() as u32;
        let mut out = Vec::new();
        for chunk in s.as_bytes().chunks(4) {
            let n = chunk.iter().filter(|&&c| c != b'=').count();
            let mut acc = 0u32;
            for (i, &c) in chunk.iter().enumerate() {
                acc |= if c == b'=' { 0 } else { val(c) } << (18 - 6 * i);
            }
            out.push((acc >> 16) as u8);
            if n > 2 { out.push((acc >> 8) as u8); }
            if n > 3 { out.push(acc as u8); }
        }
        out
    }
}
