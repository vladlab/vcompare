//! Timeline mode: which frames to sample from a Resolve timeline.
//!
//! Pure functions, no I/O. The run (`timeline_run.rs`) asks Resolve for the
//! items on the ticked video tracks, turns them into samples here, and then
//! seeks / grabs / compares each one.
//!
//! Per item: the first, middle and last frame. Samples are keyed by frame:
//! when two items (on different tracks, or one-frame clips) land on the same
//! frame, the item on the highest track wins (it is the one on top of the
//! picture), and within an item `first` beats `mid` beats `last`.
//!
//! Frame spaces:
//! - *timeline frame*: what Resolve's `GetStart()` reports (starts at the
//!   timeline's `GetStartFrame()`, typically 86400 for 01:00:00:00 at 24p).
//! - *display frame*: absolute timecode frame count in the timeline's
//!   timecode format, the same space every other vcompare mode uses for
//!   `frames[].frame`. `display = start_tc_frames + (timeline - start_frame)`.
//! - *file frame*: 0-based frame index into the file:
//!   `display - file_start_tc_frames + offset`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::engine::resolve::{ClipItem, TimelineMeta};
use crate::engine::timecode::{tc_string_to_frames, TcType, TimecodeFormat};

/// Resolve's frame-rate setting string → a timecode format.
///
/// "23.976" → 24000/1001, "29.97" + drop → 30000/1001 DF, "25" → 25/1.
/// Any rate within 0.01 of n·1000/1001 is taken as that NTSC rate.
pub fn parse_resolve_fps(s: &str, drop_frame: bool) -> Result<TimecodeFormat, String> {
    let s = s.trim();
    let f: f64 = s
        .parse()
        .map_err(|_| format!("Resolve reported an unreadable frame rate {s:?}"))?;
    if !(1.0..=1000.0).contains(&f) {
        return Err(format!("Resolve reported an unusable frame rate {s:?}"));
    }
    let nominal = f.round().max(1.0);
    let ntsc = nominal * 1000.0 / 1001.0;
    let (num, den) = if (f - nominal).abs() < 1e-3 {
        (nominal as u32, 1)
    } else if (f - ntsc).abs() < 0.01 {
        (nominal as u32 * 1000, 1001)
    } else {
        return Err(format!("Resolve reported a frame rate vcompare can't use: {s:?}"));
    };
    let nom = nominal as u32;
    let tc_type = if drop_frame && den == 1001 && (nom == 30 || nom == 60) {
        TcType::DropFrame
    } else {
        TcType::NonDrop
    };
    Ok(TimecodeFormat { fps_num: num, fps_den: den, tc_type })
}

/// Maps Resolve timeline frames to display frames.
#[derive(Debug, Clone)]
pub struct TimelineClock {
    pub tc_fmt: TimecodeFormat,
    pub start_frame: i64,
    pub start_tc_frames: i64,
}

impl TimelineClock {
    pub fn from_meta(meta: &TimelineMeta) -> Result<Self, String> {
        let fps = meta
            .fps
            .as_deref()
            .ok_or("Resolve didn't report the timeline's frame rate")?;
        let tc_fmt = parse_resolve_fps(fps, meta.drop_frame)?;
        let start_frame = meta
            .start_frame
            .ok_or("Resolve didn't report the timeline's start frame")?;
        let start_tc = meta
            .start_tc
            .as_deref()
            .ok_or("Resolve didn't report the timeline's start timecode")?;
        let start_tc_frames = tc_string_to_frames(start_tc, &tc_fmt);
        Ok(Self { tc_fmt, start_frame, start_tc_frames })
    }

    pub fn display(&self, timeline_frame: i64) -> i64 {
        self.start_tc_frames + (timeline_frame - self.start_frame)
    }
}

/// Where the file sits in display space.
#[derive(Debug, Clone)]
pub struct FileRange {
    /// The file's start timecode as a display frame (0 if untagged).
    pub start_tc_frames: i64,
    /// Frame count, if known. Unknown → no end check.
    pub frames: Option<u64>,
    /// Manual offset, same sense as the diff modes: positive = the picture
    /// is `offset` frames later in the file.
    pub offset: i64,
}

impl FileRange {
    /// File frame for a display frame, or `None` when it's outside the file.
    pub fn file_frame(&self, display: i64) -> Option<i64> {
        let f = display - self.start_tc_frames + self.offset;
        if f < 0 {
            return None;
        }
        if let Some(n) = self.frames {
            if f >= n as i64 {
                return None;
            }
        }
        Some(f)
    }

    /// The display frame of file frame 0 (for previews / mpv).
    pub fn origin(&self) -> i64 {
        self.start_tc_frames - self.offset
    }
}

/// Position of a sample within its clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pos {
    First,
    Mid,
    Last,
}

impl Pos {
    pub fn as_str(&self) -> &'static str {
        match self {
            Pos::First => "first",
            Pos::Mid => "mid",
            Pos::Last => "last",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Display frame.
    pub frame: i64,
    pub tc: String,
    pub track: u32,
    pub clip: String,
    pub pos: Pos,
    /// 0-based frame in the file; `None` = outside the file.
    pub file_frame: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Plan {
    /// Sorted by frame, one per frame.
    pub samples: Vec<Sample>,
    /// Items skipped because Resolve marks them disabled.
    pub skipped_disabled: usize,
    /// Items without a usable start / duration.
    pub skipped_bad: usize,
    /// Samples that collapsed onto an earlier one (same frame).
    pub merged: usize,
}

impl Plan {
    pub fn in_file(&self) -> usize {
        self.samples.iter().filter(|s| s.file_frame.is_some()).count()
    }
}

/// Item → (first, last) timeline frames, inclusive.
///
/// Uses `duration` when Resolve gave one (unambiguous), else `end - start`
/// (`GetEnd` is exclusive: start + duration).
fn item_span(it: &ClipItem) -> Option<(i64, i64)> {
    let start = it.start?;
    let dur = match it.duration {
        Some(d) => d,
        None => it.end? - start,
    };
    if dur < 1 {
        return None;
    }
    Some((start, start + dur - 1))
}

pub fn plan_samples(items: &[ClipItem], clock: &TimelineClock, file: &FileRange) -> Plan {
    let mut plan = Plan::default();
    // Highest track first, so it claims shared frames.
    let mut order: Vec<&ClipItem> = items.iter().collect();
    order.sort_by(|a, b| b.track.cmp(&a.track).then(a.start.cmp(&b.start)));

    let mut by_frame: BTreeMap<i64, Sample> = BTreeMap::new();
    for it in order {
        if it.enabled == Some(false) {
            plan.skipped_disabled += 1;
            continue;
        }
        let Some((first, last)) = item_span(it) else {
            plan.skipped_bad += 1;
            continue;
        };
        let mid = first + (last - first) / 2;
        for (pos, tf) in [(Pos::First, first), (Pos::Mid, mid), (Pos::Last, last)] {
            let frame = clock.display(tf);
            if by_frame.contains_key(&frame) {
                plan.merged += 1;
                continue;
            }
            by_frame.insert(
                frame,
                Sample {
                    frame,
                    tc: clock.tc_fmt.frame_to_tc(frame),
                    track: it.track,
                    clip: it.name.clone(),
                    pos,
                    file_frame: file.file_frame(frame),
                },
            );
        }
    }
    plan.samples = by_frame.into_values().collect();
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(track: u32, name: &str, start: i64, duration: i64) -> ClipItem {
        ClipItem {
            track,
            name: name.into(),
            start: Some(start),
            duration: Some(duration),
            end: Some(start + duration),
            enabled: None,
        }
    }

    fn clock24() -> TimelineClock {
        TimelineClock {
            tc_fmt: parse_resolve_fps("24", false).unwrap(),
            start_frame: 86400,
            start_tc_frames: 86400,
        }
    }

    fn whole_file(start: i64) -> FileRange {
        FileRange { start_tc_frames: start, frames: None, offset: 0 }
    }

    fn summary(p: &Plan) -> Vec<(String, u32, &str, &str, Option<i64>)> {
        p.samples
            .iter()
            .map(|s| (s.tc.clone(), s.track, s.clip.as_str(), s.pos.as_str(), s.file_frame))
            .collect()
    }

    #[test]
    fn fps_strings() {
        let cases = [
            ("23.976", false, 24000, 1001, TcType::NonDrop),
            ("24", false, 24, 1, TcType::NonDrop),
            ("25", true, 25, 1, TcType::NonDrop), // drop only means something at 29.97/59.94
            ("29.97", true, 30000, 1001, TcType::DropFrame),
            ("29.97", false, 30000, 1001, TcType::NonDrop),
            ("59.94", true, 60000, 1001, TcType::DropFrame),
            ("47.952", false, 48000, 1001, TcType::NonDrop),
            ("50", false, 50, 1, TcType::NonDrop),
        ];
        for (s, df, num, den, t) in cases {
            let f = parse_resolve_fps(s, df).unwrap();
            assert_eq!((f.fps_num, f.fps_den, f.tc_type), (num, den, t), "{s} df={df}");
        }
        assert!(parse_resolve_fps("", false).is_err());
        assert!(parse_resolve_fps("24.5", false).is_err());
    }

    #[test]
    fn first_mid_last() {
        let items = [item(1, "A", 86400, 10), item(1, "B", 86410, 5)];
        let p = plan_samples(&items, &clock24(), &whole_file(86400));
        assert_eq!(
            summary(&p),
            vec![
                ("01:00:00:00".into(), 1, "A", "first", Some(0)),
                ("01:00:00:04".into(), 1, "A", "mid", Some(4)),
                ("01:00:00:09".into(), 1, "A", "last", Some(9)),
                ("01:00:00:10".into(), 1, "B", "first", Some(10)),
                ("01:00:00:12".into(), 1, "B", "mid", Some(12)),
                ("01:00:00:14".into(), 1, "B", "last", Some(14)),
            ]
        );
        assert_eq!(p.merged, 0);
    }

    #[test]
    fn one_and_two_frame_clips() {
        let p = plan_samples(&[item(1, "one", 86400, 1)], &clock24(), &whole_file(86400));
        assert_eq!(summary(&p), vec![("01:00:00:00".into(), 1, "one", "first", Some(0))]);
        assert_eq!(p.merged, 2);

        let p = plan_samples(&[item(1, "two", 86400, 2)], &clock24(), &whole_file(86400));
        assert_eq!(
            summary(&p),
            vec![
                ("01:00:00:00".into(), 1, "two", "first", Some(0)),
                ("01:00:00:01".into(), 1, "two", "last", Some(1)),
            ]
        );
    }

    #[test]
    fn higher_track_wins_shared_frames() {
        // V2 sits exactly over V1's last frames: the V2 sample is the visible one.
        let items = [item(1, "under", 86400, 10), item(2, "over", 86409, 3)];
        let p = plan_samples(&items, &clock24(), &whole_file(86400));
        let at9: Vec<_> = summary(&p).into_iter().filter(|s| s.0 == "01:00:00:09").collect();
        assert_eq!(at9, vec![("01:00:00:09".into(), 2, "over", "first", Some(9))]);
        assert_eq!(p.merged, 1);
        // Sorted by frame regardless of track.
        assert!(p.samples.windows(2).all(|w| w[0].frame < w[1].frame));
    }

    #[test]
    fn disabled_and_unusable_items_are_skipped() {
        let mut off = item(1, "off", 86400, 10);
        off.enabled = Some(false);
        let mut nodur = item(1, "nodur", 86410, 5);
        nodur.duration = None;
        nodur.end = None;
        let mut endonly = item(1, "endonly", 86420, 4);
        endonly.duration = None; // falls back to end - start (exclusive end)
        let p = plan_samples(&[off, nodur, endonly], &clock24(), &whole_file(86400));
        assert_eq!(p.skipped_disabled, 1);
        assert_eq!(p.skipped_bad, 1);
        assert_eq!(
            summary(&p).iter().map(|s| s.0.as_str()).collect::<Vec<_>>(),
            vec!["01:00:00:20", "01:00:00:21", "01:00:00:23"]
        );
    }

    #[test]
    fn file_starting_after_the_timeline() {
        // Timeline starts at 00:59:58:00 (2 s of slate), file at 01:00:00:00.
        let clock = TimelineClock {
            tc_fmt: parse_resolve_fps("24", false).unwrap(),
            start_frame: 86400,
            start_tc_frames: 86400 - 48,
        };
        let file = FileRange { start_tc_frames: 86400, frames: Some(24), offset: 0 };
        let items = [item(1, "slate", 86400, 48), item(1, "shot", 86448, 30)];
        let p = plan_samples(&items, &clock, &file);
        assert_eq!(
            summary(&p),
            vec![
                ("00:59:58:00".into(), 1, "slate", "first", None),
                ("00:59:58:23".into(), 1, "slate", "mid", None),
                ("00:59:59:23".into(), 1, "slate", "last", None),
                ("01:00:00:00".into(), 1, "shot", "first", Some(0)),
                ("01:00:00:14".into(), 1, "shot", "mid", Some(14)),
                // The file is only 24 frames long.
                ("01:00:01:05".into(), 1, "shot", "last", None),
            ]
        );
        assert_eq!(p.in_file(), 2);
    }

    #[test]
    fn manual_offset() {
        let file = FileRange { start_tc_frames: 86400, frames: None, offset: 3 };
        assert_eq!(file.file_frame(86400), Some(3));
        assert_eq!(file.origin(), 86397);
        let file = FileRange { start_tc_frames: 86400, frames: None, offset: -3 };
        assert_eq!(file.file_frame(86401), None);
        assert_eq!(file.file_frame(86403), Some(0));
    }

    #[test]
    fn drop_frame_timeline() {
        let tc_fmt = parse_resolve_fps("29.97", true).unwrap();
        let start = tc_string_to_frames("00:59:59;28", &tc_fmt);
        let clock = TimelineClock { tc_fmt, start_frame: 107892, start_tc_frames: start };
        // 4 frames from 00:59:59;28 cross the 01:00 minute: ;28 ;29 then 01:00:00;00 ;01
        // (minute 60 is a multiple of ten, so nothing is dropped there).
        let p = plan_samples(&[item(1, "x", 107892, 4)], &clock, &whole_file(start));
        assert_eq!(
            summary(&p).iter().map(|s| s.0.as_str()).collect::<Vec<_>>(),
            vec!["00:59:59;28", "00:59:59;29", "01:00:00;01"]
        );
        // And across a dropping minute: 00:00:59;29 → 00:01:00;02.
        let start = tc_string_to_frames("00:00:59;29", &clock.tc_fmt);
        let clock = TimelineClock { start_tc_frames: start, start_frame: 0, ..clock };
        let p = plan_samples(&[item(1, "y", 0, 2)], &clock, &whole_file(0));
        assert_eq!(
            summary(&p).iter().map(|s| s.0.as_str()).collect::<Vec<_>>(),
            vec!["00:00:59;29", "00:01:00;02"]
        );
    }
}
