//! DaVinci Resolve playhead sync.
//!
//! Resolve's scripting API is a Python module (`DaVinciResolveScript`) that
//! loads Resolve's `fusionscript` library, so the app drives it through a
//! small Python helper (`resolve_bridge.py`, embedded in the binary and
//! written to the temp dir on first use). The helper connects once, then
//! takes `SEEK <timecode>` lines on stdin and answers `OK` / `ERR …`.
//!
//! Seeks are coalesced: if the cursor moves while a seek is in flight, only
//! the newest timecode is sent once the helper answers, so scrubbing never
//! queues up a backlog in Resolve.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};

const BRIDGE_SOURCE: &str = include_str!("resolve_bridge.py");

/// Where the helper got to.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ResolveStatus {
    /// Starting Python / connecting to Resolve.
    Connecting,
    /// Connected; `target` is "project / timeline".
    Connected { target: String },
    /// A seek was refused (Resolve still connected).
    SeekFailed { message: String },
    /// Couldn't connect, or the helper died.
    Failed { message: String },
    Disconnected,
}

struct Pending {
    /// Newest timecode not yet sent.
    next: Option<String>,
    /// A command is out and we're waiting on its answer.
    in_flight: bool,
    closing: bool,
}

/// One connection to Resolve. Dropping it closes the helper.
pub struct ResolveSync {
    target: String,
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Arc<(Mutex<Pending>, Condvar)>,
    status: Arc<Mutex<ResolveStatus>>,
}

impl ResolveSync {
    /// Spawn the helper and connect. Blocks until Resolve answers (a second
    /// or two) or fails. `on_status` is called for every status change,
    /// including the first.
    pub fn connect(on_status: Box<dyn Fn(ResolveStatus) + Send + Sync>) -> Result<Arc<Self>, String> {
        let on_status = Arc::new(on_status);
        on_status(ResolveStatus::Connecting);

        let Spawned { child, stdin, mut reader, target } = match spawn_bridge() {
            Ok(s) => s,
            Err(msg) => {
                on_status(ResolveStatus::Failed { message: msg.clone() });
                return Err(msg);
            }
        };

        let status = Arc::new(Mutex::new(ResolveStatus::Connected { target: target.clone() }));
        on_status(ResolveStatus::Connected { target: target.clone() });

        let sync = Arc::new(Self {
            target: target.clone(),
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            pending: Arc::new((
                Mutex::new(Pending { next: None, in_flight: false, closing: false }),
                Condvar::new(),
            )),
            status,
        });

        // Sender thread: waits for a pending timecode, writes it, waits for
        // the answer, repeats. One command in flight at a time.
        let s = sync.clone();
        let cb = on_status.clone();
        std::thread::spawn(move || {
            let (lock, cv) = &*s.pending;
            loop {
                let tc = {
                    let mut p = lock.lock().unwrap();
                    while p.next.is_none() && !p.closing {
                        p = cv.wait(p).unwrap();
                    }
                    if p.closing {
                        break;
                    }
                    p.in_flight = true;
                    p.next.take().unwrap()
                };

                let written = {
                    let mut g = s.stdin.lock().unwrap();
                    match g.as_mut() {
                        Some(w) => writeln!(w, "SEEK {tc}").and_then(|_| w.flush()).is_ok(),
                        None => false,
                    }
                };
                let mut line = String::new();
                let answered = written && matches!(reader.read_line(&mut line), Ok(n) if n > 0);
                lock.lock().unwrap().in_flight = false;

                if !answered {
                    let msg = "Resolve bridge closed".to_string();
                    *s.status.lock().unwrap() = ResolveStatus::Failed { message: msg.clone() };
                    cb(ResolveStatus::Failed { message: msg });
                    break;
                }
                let line = line.trim();
                let new_status = if let Some(err) = line.strip_prefix("ERR ") {
                    Some(ResolveStatus::SeekFailed { message: err.to_string() })
                } else {
                    // Back to healthy after a refused seek. (Don't hold the
                    // status lock while deciding — the update takes it too.)
                    let was_failed = matches!(*s.status.lock().unwrap(), ResolveStatus::SeekFailed { .. });
                    was_failed.then(|| ResolveStatus::Connected { target: s.target.clone() })
                };
                if let Some(st) = new_status {
                    *s.status.lock().unwrap() = st.clone();
                    cb(st);
                }
            }
        });

        Ok(sync)
    }

    /// Move Resolve's playhead. Returns immediately; the newest timecode
    /// wins if several arrive while one is in flight.
    pub fn seek(&self, timecode: &str) {
        let (lock, cv) = &*self.pending;
        let mut p = lock.lock().unwrap();
        p.next = Some(timecode.to_string());
        cv.notify_one();
    }

    pub fn status(&self) -> ResolveStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn is_running(&self) -> bool {
        let mut g = self.child.lock().unwrap();
        match g.as_mut() {
            Some(c) => c.try_wait().ok().flatten().is_none(),
            None => false,
        }
    }

    /// Close the helper.
    pub fn disconnect(&self) {
        {
            let (lock, cv) = &*self.pending;
            let mut p = lock.lock().unwrap();
            p.closing = true;
            p.next = None;
            cv.notify_all();
        }
        if let Some(mut w) = self.stdin.lock().unwrap().take() {
            let _ = writeln!(w, "QUIT");
            let _ = w.flush();
        }
        if let Some(mut c) = self.child.lock().unwrap().take() {
            // Give it a moment to exit on QUIT, then make sure.
            for _ in 0..20 {
                if c.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
        *self.status.lock().unwrap() = ResolveStatus::Disconnected;
    }
}

impl Drop for ResolveSync {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// A started helper that answered READY.
struct Spawned {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    /// "project / timeline"
    target: String,
}

/// Write the helper script out, start it and wait for its READY line.
fn spawn_bridge() -> Result<Spawned, String> {
    let script = std::env::temp_dir().join("vcompare_resolve_bridge.py");
    std::fs::write(&script, BRIDGE_SOURCE)
        .map_err(|e| format!("Failed to write bridge script: {e}"))?;

    let mut child = python_command()?
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Python for the Resolve bridge: {e}"))?;

    let stdin = child.stdin.take().ok_or("bridge stdin")?;
    let stdout = child.stdout.take().ok_or("bridge stdout")?;
    let stderr = child.stderr.take().ok_or("bridge stderr")?;

    // Drain stderr so the helper can't block on it; keep a tail for errors.
    let err_tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let tail = err_tail.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut t = tail.lock().unwrap();
                if t.len() >= 10 {
                    t.remove(0);
                }
                t.push(line);
            }
        });
    }

    let mut reader = BufReader::new(stdout);
    let mut first = String::new();
    reader
        .read_line(&mut first)
        .map_err(|e| format!("Resolve bridge: {e}"))?;
    let first = first.trim().to_string();
    match first.strip_prefix("READY ") {
        Some(t) => Ok(Spawned { child, stdin, reader, target: t.to_string() }),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Err(first
                .strip_prefix("FAIL ")
                .map(String::from)
                .unwrap_or_else(|| {
                    // The reader thread may still be collecting the traceback.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    let tail = err_tail.lock().unwrap().join("\n");
                    if tail.is_empty() {
                        "Resolve bridge exited without connecting".into()
                    } else {
                        format!("Resolve bridge failed:\n{tail}")
                    }
                }))
        }
    }
}

// ─── Synchronous session (timeline runs) ────────────────────────────────────

/// Timeline facts the bridge reports with every TRACKS / CLIPS answer.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TimelineMeta {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub start_frame: Option<i64>,
    #[serde(default)]
    pub end_frame: Option<i64>,
    #[serde(default)]
    pub start_tc: Option<String>,
    /// Resolve's setting string, e.g. "23.976".
    #[serde(default)]
    pub fps: Option<String>,
    #[serde(default)]
    pub drop_frame: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrackInfo {
    pub index: u32,
    pub name: String,
    /// `None` on builds without `GetIsTrackEnabled`.
    #[serde(default)]
    pub enabled: Option<bool>,
    pub items: u32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TracksReply {
    pub timeline: TimelineMeta,
    pub tracks: Vec<TrackInfo>,
}

/// One item on a video track, as Resolve reports it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClipItem {
    pub track: u32,
    pub name: String,
    pub start: Option<i64>,
    pub duration: Option<i64>,
    pub end: Option<i64>,
    /// `None` when the build can't tell.
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClipsReply {
    pub timeline: TimelineMeta,
    pub items: Vec<ClipItem>,
}

/// A private helper for one timeline run: every command waits for its
/// answer, nothing is coalesced (a run must not skip seeks). Independent of
/// the cursor-sync connection, which may be open at the same time.
pub struct ResolveSession {
    child: Option<Child>,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    pub target: String,
}

impl ResolveSession {
    pub fn connect() -> Result<Self, String> {
        let Spawned { child, stdin, reader, target } = spawn_bridge()?;
        Ok(Self { child: Some(child), stdin, reader, target })
    }

    /// Send one line, return the answer after `OK` (possibly empty).
    fn request(&mut self, line: &str) -> Result<String, String> {
        writeln!(self.stdin, "{line}")
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("Resolve bridge closed: {e}"))?;
        let mut answer = String::new();
        match self.reader.read_line(&mut answer) {
            Ok(n) if n > 0 => {}
            _ => return Err("Resolve bridge closed".into()),
        }
        let answer = answer.trim_end();
        if let Some(rest) = answer.strip_prefix("OK") {
            Ok(rest.trim_start().to_string())
        } else if let Some(err) = answer.strip_prefix("ERR ") {
            Err(err.to_string())
        } else {
            Err(format!("Resolve bridge: unexpected answer {answer:?}"))
        }
    }

    pub fn tracks(&mut self) -> Result<TracksReply, String> {
        let body = self.request("TRACKS")?;
        serde_json::from_str(&body).map_err(|e| format!("Resolve bridge: bad TRACKS answer: {e}"))
    }

    pub fn clips(&mut self, tracks: &[u32]) -> Result<ClipsReply, String> {
        let list: Vec<String> = tracks.iter().map(|t| t.to_string()).collect();
        let body = self.request(&format!("CLIPS {}", list.join(",")))?;
        serde_json::from_str(&body).map_err(|e| format!("Resolve bridge: bad CLIPS answer: {e}"))
    }

    /// Move the playhead and wait until Resolve has accepted it.
    pub fn seek(&mut self, timecode: &str) -> Result<(), String> {
        self.request(&format!("SEEK {timecode}")).map(|_| ())
    }

    pub fn close(&mut self) {
        let _ = writeln!(self.stdin, "QUIT");
        let _ = self.stdin.flush();
        if let Some(mut c) = self.child.take() {
            for _ in 0..20 {
                if c.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for ResolveSession {
    fn drop(&mut self) {
        self.close();
    }
}

/// A Python 3 to run the bridge with. `VCOMPARE_PYTHON` overrides.
fn python_command() -> Result<Command, String> {
    if let Some(p) = std::env::var_os("VCOMPARE_PYTHON") {
        return Ok(crate::engine::sysenv::command(p));
    }
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &["python", "py", "python3"]
    } else {
        &["python3", "python"]
    };
    // Why each candidate didn't do, so "not found" never hides "found but broken".
    let mut why: Vec<String> = Vec::new();
    for c in candidates {
        match crate::engine::sysenv::command(c)
            .args(["-c", "import sys; sys.exit(0 if sys.version_info[0] == 3 else 1)"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
        {
            Ok(o) if o.status.success() => return Ok(crate::engine::sysenv::command(c)),
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let last = err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("").trim().to_string();
                why.push(format!("{c}: exit {}{}", o.status.code().unwrap_or(-1),
                                 if last.is_empty() { String::new() } else { format!(" ({last})") }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => why.push(format!("{c}: not on PATH")),
            Err(e) => why.push(format!("{c}: {e}")),
        }
    }
    Err(format!(
        "Python 3 not usable — the Resolve bridge needs it (Resolve's scripting API is Python). \
         Set VCOMPARE_PYTHON to pick one. Tried {}",
        why.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the bridge protocol against a fake "Resolve" by pointing the
    /// helper at a stub module directory. Exercises connect, seek
    /// coalescing, ERR handling and disconnect without Resolve installed.
    #[test]
    fn bridge_protocol_with_stub_resolve() {
        if python_command().is_err() {
            eprintln!("no python3; skipping");
            return;
        }
        let _env = crate::engine::testenv::lock();
        let dir = std::env::temp_dir().join(format!("vcompare_resolve_stub_{}", std::process::id()));
        let modules = dir.join("Modules");
        std::fs::create_dir_all(&modules).unwrap();
        // Stub DaVinciResolveScript: records seeks to a file; refuses one TC.
        let log = dir.join("seeks.log");
        std::fs::write(
            modules.join("DaVinciResolveScript.py"),
            format!(
                r#"
class _TL:
    def GetName(self): return "TL"
    def SetCurrentTimecode(self, tc):
        open({log:?}, "a").write(tc + "\n")
        return tc != "00:00:00:00"
class _P:
    def GetName(self): return "Proj"
    def GetCurrentTimeline(self): return _TL()
class _PM:
    def GetCurrentProject(self): return _P()
class _R:
    def GetProjectManager(self): return _PM()
def scriptapp(name): return _R()
"#,
                log = log.to_string_lossy()
            ),
        )
        .unwrap();
        let fake_lib = dir.join("fusionscript.so");
        std::fs::write(&fake_lib, b"").unwrap();
        std::env::set_var("RESOLVE_SCRIPT_API", &dir);
        std::env::set_var("RESOLVE_SCRIPT_LIB", &fake_lib);

        let seen: Arc<Mutex<Vec<ResolveStatus>>> = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let sync = ResolveSync::connect(Box::new(move |st| s2.lock().unwrap().push(st))).unwrap();
        assert!(matches!(sync.status(), ResolveStatus::Connected { ref target } if target == "Proj / TL"));

        let wait_for = |pred: &dyn Fn() -> bool, what: &str| {
            let t = std::time::Instant::now();
            while !pred() {
                assert!(t.elapsed().as_secs() < 10, "timed out waiting for {what}");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let logged = || std::fs::read_to_string(&log).unwrap_or_default();

        sync.seek("01:00:00:01");
        // Burst while the first is in flight: only the last should go out.
        for i in 2..50 {
            sync.seek(&format!("01:00:00:{i:02}"));
        }
        wait_for(&|| logged().contains("01:00:00:49"), "burst to drain");
        sync.seek("00:00:00:00"); // the stub refuses this one
        wait_for(&|| matches!(sync.status(), ResolveStatus::SeekFailed { .. }), "SeekFailed");
        sync.seek("01:00:00:10");
        wait_for(&|| matches!(sync.status(), ResolveStatus::Connected { .. }), "recovery");

        sync.disconnect();
        assert!(!sync.is_running());
        let seeks = std::fs::read_to_string(&log).unwrap();
        let seeks: Vec<&str> = seeks.lines().collect();
        // The newest of the burst must always go out; earlier ones may be
        // superseded before the sender wakes, which is the point.
        assert!(seeks.contains(&"01:00:00:49"), "newest of the burst must be sent: {seeks:?}");
        assert!(seeks.len() < 10, "burst must be coalesced, got {} seeks", seeks.len());
        assert_eq!(seeks.last(), Some(&"01:00:00:10"));
        let statuses = seen.lock().unwrap();
        assert!(matches!(statuses[0], ResolveStatus::Connecting));
        assert!(matches!(statuses[1], ResolveStatus::Connected { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// TRACKS / CLIPS / synchronous SEEK against the shared fake Resolve in
    /// `testdata/fake_resolve` (also used by the timeline run tests).
    #[test]
    fn session_tracks_clips_seek() {
        if python_command().is_err() {
            eprintln!("no python3; skipping");
            return;
        }
        let _env = crate::engine::testenv::lock();
        let dir = std::env::temp_dir().join(format!("vcompare_session_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tl = dir.join("timeline.json");
        std::fs::write(
            &tl,
            r#"{"project":"P","name":"Reel 1","start_frame":86400,"start_tc":"01:00:00:00",
                "fps":"23.976","drop_frame":false,
                "tracks":[{"name":"V1","enabled":true,"items":[
                            {"name":"A 001","start":86400,"duration":10},
                            {"name":"B","start":86410,"duration":1,"enabled":false}]},
                          {"name":"Gfx","enabled":false,"items":[]}]}"#,
        )
        .unwrap();
        let playhead = dir.join("playhead");
        crate::engine::testenv::use_fake_resolve(&tl, Some(&playhead));

        let mut s = ResolveSession::connect().unwrap();
        assert_eq!(s.target, "P / Reel 1");
        let t = s.tracks().unwrap();
        assert_eq!(t.tracks.len(), 2);
        assert_eq!(t.tracks[0].items, 2);
        assert_eq!(t.tracks[1].enabled, Some(false));
        assert_eq!(t.timeline.fps.as_deref(), Some("23.976"));
        assert_eq!(t.timeline.start_frame, Some(86400));
        let c = s.clips(&[1, 2]).unwrap();
        assert_eq!(c.items.len(), 2);
        assert_eq!(c.items[0].name, "A 001");
        assert_eq!(c.items[0].end, Some(86410));
        assert_eq!(c.items[1].enabled, Some(false));
        s.seek("01:00:00:05").unwrap();
        assert_eq!(std::fs::read_to_string(&playhead).unwrap().trim(), "01:00:00:05");
        assert!(s.seek("bogus").is_err(), "a refused seek is an error");
        assert!(s.clips(&[]).unwrap().items.is_empty());
        s.close();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
