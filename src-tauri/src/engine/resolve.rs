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
use std::process::{Child, ChildStdin, Command, Stdio};
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
        let target = match first.strip_prefix("READY ") {
            Some(t) => t.to_string(),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                let msg = first
                    .strip_prefix("FAIL ")
                    .map(String::from)
                    .unwrap_or_else(|| {
                        let tail = err_tail.lock().unwrap().join("\n");
                        if tail.is_empty() {
                            "Resolve bridge exited without connecting".into()
                        } else {
                            format!("Resolve bridge failed:\n{tail}")
                        }
                    });
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

/// A Python 3 to run the bridge with. `VCOMPARE_PYTHON` overrides.
fn python_command() -> Result<Command, String> {
    if let Some(p) = std::env::var_os("VCOMPARE_PYTHON") {
        return Ok(Command::new(p));
    }
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &["python", "py", "python3"]
    } else {
        &["python3", "python"]
    };
    for c in candidates {
        let ok = Command::new(c)
            .args(["-c", "import sys; sys.exit(0 if sys.version_info[0] == 3 else 1)"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Ok(Command::new(c));
        }
    }
    Err("Python 3 not found — the Resolve bridge needs it (Resolve's scripting API is Python)".into())
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
}
