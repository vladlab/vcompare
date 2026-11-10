//! Client for scopetap's `scopetap_grab --serve`: Resolve's SDI output, one
//! frame at a time, by timecode.
//!
//! The grabber is an optional helper (like mpv): it lives in the scopetap
//! repo, reads scopetap_stream's GPU ring, and hands back plain packed
//! `rgb48le` bytes on a pipe, so vcompare needs no CUDA. Protocol (proto=1)
//! is owned by scopetap's docs/GRAB_PLAN.md §4:
//!
//! ```text
//! → (start)                     ← READY proto=1 w=3840 h=2160 fmt=rgb48le | FAIL <msg>
//! → GRAB 01:00:05:12 [ms]       ← OK <nbytes> w= h= fmt= tc= seq= levels= auto= colorspace=
//!                                     eotf= clamped= waited_ms= refines=   + <nbytes> bytes
//!                               ← ERR <code> [key=value ...]
//! → QUIT
//! ```
//!
//! Which binary: `$VCOMPARE_GRABBER`, else `scopetap_grab` on the PATH.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use crate::engine::process::{ChildId, Job};

pub const BINARY: &str = "scopetap_grab";

/// What the grabber said about one frame (all fields from the `OK` line).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct GrabMeta {
    pub width: u32,
    pub height: u32,
    pub fmt: String,
    pub tc: String,
    pub seq: u64,
    /// "video" | "full" | "full_expand": how the SDI codes were read.
    pub levels: String,
    pub levels_auto: bool,
    /// 709 | 2020 | 601 | 0 (unknown), from the VPID.
    pub colorspace: i32,
    /// -1 none, 0 SDR, 2 PQ, 3 HLG.
    pub eotf: i32,
    pub clamped: u64,
    pub waited_ms: u64,
    pub refines: u32,
}

impl GrabMeta {
    fn from_kv(kv: &HashMap<String, String>) -> Self {
        let num = |k: &str| kv.get(k).and_then(|v| v.parse::<i64>().ok());
        Self {
            width: num("w").unwrap_or(0) as u32,
            height: num("h").unwrap_or(0) as u32,
            fmt: kv.get("fmt").cloned().unwrap_or_default(),
            tc: kv.get("tc").cloned().unwrap_or_default(),
            seq: num("seq").unwrap_or(0) as u64,
            levels: kv.get("levels").cloned().unwrap_or_default(),
            levels_auto: num("auto") == Some(1),
            colorspace: num("colorspace").unwrap_or(0) as i32,
            eotf: num("eotf").unwrap_or(-1) as i32,
            clamped: num("clamped").unwrap_or(0) as u64,
            waited_ms: num("waited_ms").unwrap_or(0) as u64,
            refines: num("refines").unwrap_or(0) as u32,
        }
    }

    /// "Rec.709 SDR", "Rec.2020 PQ" ... for diagnostics.
    pub fn signal_name(&self) -> String {
        let cs = match self.colorspace {
            709 => "Rec.709",
            2020 => "Rec.2020",
            601 => "Rec.601",
            _ => "unknown colorspace",
        };
        let eotf = match self.eotf {
            0 => "SDR",
            1 => "HDR gamma",
            2 => "PQ",
            3 => "HLG",
            _ => "no EOTF tag",
        };
        format!("{cs} {eotf}")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GrabError {
    /// The target timecode never settled on the wire. `have` is the last
    /// timecode the grabber saw.
    Timeout { have: Option<String> },
    /// Any other `ERR <code>` answer.
    Refused { code: String, detail: String },
    /// The grabber went away (crashed, killed by a cancel, bad output).
    Closed(String),
}

impl std::fmt::Display for GrabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrabError::Timeout { have: Some(h) } => write!(f, "timed out (SDI showed {h})"),
            GrabError::Timeout { have: None } => write!(f, "timed out"),
            GrabError::Refused { code, detail } if detail.is_empty() => write!(f, "{code}"),
            GrabError::Refused { code, detail } => write!(f, "{code}: {detail}"),
            GrabError::Closed(m) => write!(f, "grabber closed: {m}"),
        }
    }
}

/// Parse `key=value` words; words without `=` are ignored.
pub fn parse_kv(words: &str) -> HashMap<String, String> {
    words
        .split_whitespace()
        .filter_map(|w| w.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Parsed answer line to a GRAB.
#[derive(Debug, PartialEq)]
pub enum Answer {
    Ok { nbytes: usize, kv: HashMap<String, String> },
    Err { code: String, kv: HashMap<String, String>, rest: String },
}

pub fn parse_answer(line: &str) -> Result<Answer, String> {
    let line = line.trim_end();
    if let Some(rest) = line.strip_prefix("OK ") {
        let (n, kv) = rest.split_once(' ').unwrap_or((rest, ""));
        let nbytes = n
            .parse::<usize>()
            .map_err(|_| format!("bad byte count in {line:?}"))?;
        Ok(Answer::Ok { nbytes, kv: parse_kv(kv) })
    } else if let Some(rest) = line.strip_prefix("ERR ") {
        let (code, more) = rest.split_once(' ').unwrap_or((rest, ""));
        let detail: Vec<&str> = more.split_whitespace().filter(|w| !w.contains('=')).collect();
        Ok(Answer::Err {
            code: code.to_string(),
            kv: parse_kv(more),
            rest: detail.join(" "),
        })
    } else {
        Err(format!("unexpected answer {line:?}"))
    }
}

/// Where the grabber binary is, if anywhere.
pub fn find_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VCOMPARE_GRABBER") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(BINARY))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// A running `scopetap_grab --serve`. Its process is attached to the job, so
/// cancelling the analysis kills it (and any read in progress fails).
pub struct Grabber {
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    job: Arc<Job>,
    child: Option<ChildId>,
    /// From READY (the signal's size when the grabber attached).
    pub width: u32,
    pub height: u32,
}

impl Grabber {
    pub fn start(bin: &Path, tc_base: u32, settle_ms: Option<u32>, job: &Arc<Job>) -> Result<Self, String> {
        let mut cmd = Command::new(bin);
        cmd.args(["--serve", "--tc-base", &tc_base.to_string()]);
        if let Some(ms) = settle_ms {
            cmd.args(["--settle-ms", &ms.to_string()]);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("Failed to start {}: {e}", bin.display()))?;
        let stdin = child.stdin.take().ok_or("grabber stdin")?;
        let stdout = child.stdout.take().ok_or("grabber stdout")?;
        let id = job.attach(child).ok_or("Analysis cancelled")?;

        // READY can take a moment (attaching the ring); never hang on it.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let r = reader.read_line(&mut line).map(|_| line);
            let _ = tx.send((reader, r));
        });
        let (reader, first) = match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(v) => v,
            Err(_) => {
                job.kill_attached(id);
                return Err("scopetap_grab didn't answer within 20 s".into());
            }
        };
        let first = first.map_err(|e| format!("scopetap_grab: {e}"))?;
        let first = first.trim();
        let Some(rest) = first.strip_prefix("READY") else {
            job.kill_attached(id);
            return Err(match first.strip_prefix("FAIL ") {
                Some(m) => format!("scopetap_grab: {m}"),
                None if first.is_empty() => "scopetap_grab exited without answering".into(),
                None => format!("scopetap_grab: unexpected {first:?}"),
            });
        };
        let kv = parse_kv(rest);
        if kv.get("proto").map(String::as_str) != Some("1") {
            job.kill_attached(id);
            return Err(format!("scopetap_grab speaks protocol {:?}; vcompare needs 1", kv.get("proto")));
        }
        let fmt = kv.get("fmt").cloned().unwrap_or_default();
        if fmt != "rgb48le" {
            job.kill_attached(id);
            return Err(format!("scopetap_grab offers {fmt:?}; vcompare reads rgb48le"));
        }
        Ok(Self {
            stdin,
            reader,
            job: job.clone(),
            child: Some(id),
            width: kv.get("w").and_then(|v| v.parse().ok()).unwrap_or(0),
            height: kv.get("h").and_then(|v| v.parse().ok()).unwrap_or(0),
        })
    }

    /// The settled SDI frame at `tc`, as packed rgb48le, top row first.
    pub fn grab(&mut self, tc: &str, timeout_ms: u32) -> Result<(GrabMeta, Vec<u8>), GrabError> {
        writeln!(self.stdin, "GRAB {tc} {timeout_ms}")
            .and_then(|_| self.stdin.flush())
            .map_err(|e| GrabError::Closed(e.to_string()))?;
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(n) if n > 0 => {}
            Ok(_) => return Err(GrabError::Closed("end of output".into())),
            Err(e) => return Err(GrabError::Closed(e.to_string())),
        }
        match parse_answer(&line).map_err(GrabError::Closed)? {
            Answer::Ok { nbytes, kv } => {
                let meta = GrabMeta::from_kv(&kv);
                let mut data = vec![0u8; nbytes];
                self.reader
                    .read_exact(&mut data)
                    .map_err(|e| GrabError::Closed(format!("short frame: {e}")))?;
                let want = meta.width as usize * meta.height as usize * 6;
                if meta.fmt != "rgb48le" || nbytes != want {
                    return Err(GrabError::Closed(format!(
                        "frame is {nbytes} bytes of {:?}, expected {want} bytes of rgb48le for {}x{}",
                        meta.fmt, meta.width, meta.height
                    )));
                }
                Ok((meta, data))
            }
            Answer::Err { code, kv, rest } => {
                if code == "timeout" {
                    Err(GrabError::Timeout { have: kv.get("have").cloned() })
                } else {
                    let mut detail = rest;
                    for (k, v) in kv {
                        detail.push_str(&format!(" {k}={v}"));
                    }
                    Err(GrabError::Refused { code, detail: detail.trim().to_string() })
                }
            }
        }
    }

    pub fn close(&mut self) {
        let _ = writeln!(self.stdin, "QUIT");
        let _ = self.stdin.flush();
        if let Some(id) = self.child.take() {
            if let Some(mut c) = self.job.detach(id) {
                for _ in 0..20 {
                    if c.try_wait().ok().flatten().is_some() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

impl Drop for Grabber {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(
            parse_answer("OK 12 w=2 h=1 fmt=rgb48le tc=01:00:00:00\n").unwrap(),
            Answer::Ok {
                nbytes: 12,
                kv: parse_kv("w=2 h=1 fmt=rgb48le tc=01:00:00:00"),
            }
        );
        match parse_answer("ERR timeout have=01:00:00:03").unwrap() {
            Answer::Err { code, kv, .. } => {
                assert_eq!(code, "timeout");
                assert_eq!(kv.get("have").map(String::as_str), Some("01:00:00:03"));
            }
            other => panic!("{other:?}"),
        }
        match parse_answer("ERR cuda out of memory").unwrap() {
            Answer::Err { code, rest, .. } => {
                assert_eq!(code, "cuda");
                assert_eq!(rest, "out of memory");
            }
            other => panic!("{other:?}"),
        }
        assert!(parse_answer("OK twelve").is_err());
        assert!(parse_answer("hello").is_err());
    }

    #[test]
    fn meta_fields() {
        let m = GrabMeta::from_kv(&parse_kv(
            "w=3840 h=2160 fmt=rgb48le tc=01:00:00:00 seq=7 levels=video auto=1 \
             colorspace=2020 eotf=2 clamped=12 waited_ms=180 refines=1",
        ));
        assert_eq!((m.width, m.height, m.seq, m.clamped, m.refines), (3840, 2160, 7, 12, 1));
        assert!(m.levels_auto);
        assert_eq!(m.signal_name(), "Rec.2020 PQ");
    }
}
