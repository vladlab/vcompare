//! JSON-RPC 2.0 multiplexer over stdio pipes to the Python sidecar.
//!
//! Architecture:
//!   - A reader task continuously reads stdout lines from the Python process
//!   - Responses (have "id") are routed to the waiting caller via oneshot channels
//!   - Notifications (no "id") are broadcast as Tauri events to the frontend
//!   - A writer serializes requests to stdin with an atomic request ID counter
//!
//! Process tree management:
//!   The sidecar is spawned in its own process group (setpgid). On cleanup,
//!   killpg() sends signals to the entire group — killing Python, mpv, and
//!   any ffmpeg children in one shot.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

/// A pending RPC request waiting for its response.
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "RPC error {}: {}", self.code, self.message)
    }
}

/// JSON-RPC notification from Python → frontend.
/// Emitted as a Tauri event with name "rpc:{method}".
#[derive(Clone, Serialize)]
struct RpcNotification {
    method: String,
    params: Value,
}

pub struct Sidecar {
    child: Mutex<Option<Child>>,
    stdin: Arc<Mutex<Option<std::process::ChildStdin>>>,
    pending: PendingMap,
    next_id: AtomicU64,
}

impl Sidecar {
    /// Spawn the sidecar process and start the reader task.
    ///
    /// Accepts a pre-configured `Command` — caller decides whether
    /// it's `python3 script.py` (dev) or a compiled binary (prod).
    ///
    /// On Unix, the child is placed in its own process group so that
    /// `kill()` can terminate the entire tree (Python + mpv + ffmpeg).
    pub fn spawn(app: &AppHandle, mut cmd: Command) -> Result<Arc<Self>, String> {
        // Put the sidecar in its own process group. This means killpg()
        // will reach Python AND any grandchildren (mpv, ffmpeg) that
        // haven't explicitly changed their own group.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: setpgid is async-signal-safe (POSIX). Called after
            // fork() but before exec() — no other threads in the child.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setpgid(0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // logs go to terminal
            .spawn()
            .map_err(|e| format!("Failed to spawn sidecar: {e}"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or("Failed to capture sidecar stdout")?;
        let stdin = child
            .stdin
            .take()
            .ok_or("Failed to capture sidecar stdin")?;

        let sidecar = Arc::new(Self {
            child: Mutex::new(Some(child)),
            stdin: Arc::new(Mutex::new(Some(stdin))),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
        });

        // Spawn the stdout reader on a background thread.
        // We use a std thread (not tokio) because BufReader::lines() is blocking
        // and we don't want to hold a tokio worker.
        let pending = Arc::clone(&sidecar.pending);
        let app_handle = app.clone();

        std::thread::Builder::new()
            .name("sidecar-reader".into())
            .spawn(move || {
                Self::reader_loop(stdout, pending, app_handle);
            })
            .map_err(|e| format!("Failed to spawn reader thread: {e}"))?;

        Ok(sidecar)
    }

    /// Send a JSON-RPC request and wait for the response.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        // Register the pending response channel BEFORE writing
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            pending.insert(id, tx);
        }

        // Write to stdin
        {
            let mut stdin_guard = self.stdin.lock().unwrap();
            if let Some(ref mut stdin) = *stdin_guard {
                let line = serde_json::to_string(&request).unwrap();
                if let Err(e) = writeln!(stdin, "{}", line) {
                    // Remove pending entry on write failure
                    self.pending.lock().unwrap().remove(&id);
                    return Err(RpcError {
                        code: -32000,
                        message: format!("Failed to write to sidecar: {e}"),
                    });
                }
                let _ = stdin.flush();
            } else {
                self.pending.lock().unwrap().remove(&id);
                return Err(RpcError {
                    code: -32000,
                    message: "Sidecar stdin closed".into(),
                });
            }
        }

        // Wait for response
        rx.await.unwrap_or(Err(RpcError {
            code: -32000,
            message: "Sidecar response channel dropped".into(),
        }))
    }

    /// Send a fire-and-forget notification to Python (no id, no response).
    pub fn notify(&self, method: &str, params: Value) {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        let mut stdin_guard = self.stdin.lock().unwrap();
        if let Some(ref mut stdin) = *stdin_guard {
            let line = serde_json::to_string(&msg).unwrap();
            let _ = writeln!(stdin, "{}", line);
            let _ = stdin.flush();
        }
    }

    /// Background reader: parse stdout lines, route responses & notifications.
    fn reader_loop(
        stdout: std::process::ChildStdout,
        pending: PendingMap,
        app: AppHandle,
    ) {
        let reader = BufReader::new(stdout);

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    log::error!("Sidecar stdout read error: {e}");
                    break;
                }
            };

            if line.trim().is_empty() {
                continue;
            }

            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("Sidecar sent non-JSON: {e} — line: {line}");
                    continue;
                }
            };

            if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                // Response to a request — route to waiting caller
                let mut pending_map = pending.lock().unwrap();
                if let Some(tx) = pending_map.remove(&id) {
                    if let Some(error) = msg.get("error") {
                        let rpc_err = RpcError {
                            code: error.get("code").and_then(|c| c.as_i64()).unwrap_or(-1),
                            message: error
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("Unknown error")
                                .to_string(),
                        };
                        let _ = tx.send(Err(rpc_err));
                    } else {
                        let result = msg.get("result").cloned().unwrap_or(Value::Null);
                        let _ = tx.send(Ok(result));
                    }
                } else {
                    log::warn!("Response for unknown id {id}");
                }
            } else if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                // Notification — broadcast as Tauri event "rpc:{method}"
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let event_name = format!("rpc:{}", method.replace('.', "-"));
                log::info!("Emitting event: {event_name}");
                match app.emit(&event_name, &params) {
                    Ok(_) => {},
                    Err(e) => log::error!("Failed to emit {event_name}: {e}"),
                }
            } else {
                log::warn!("Unrecognized message (no id, no method): {line}");
            }
        }

        log::info!("Sidecar reader loop ended (process exited or pipe closed)");
    }

    /// Kill the sidecar process and all its children.
    ///
    /// Strategy:
    ///   1. Send "cleanup" RPC so Python can gracefully kill mpv/ffmpeg
    ///   2. Close stdin (signals EOF → Python's main loop exits)
    ///   3. SIGTERM the entire process group (catches anything Python missed)
    ///   4. SIGKILL as last resort
    pub fn kill(&self) {
        // 1. Try sending a cleanup RPC (kills mpv, ffmpeg children)
        {
            let mut stdin_guard = self.stdin.lock().unwrap();
            if let Some(ref mut stdin) = *stdin_guard {
                let msg = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 999999,
                    "method": "cleanup",
                    "params": {},
                });
                let line = serde_json::to_string(&msg).unwrap();
                let _ = writeln!(stdin, "{}", line);
                let _ = stdin.flush();
            }
        }

        // 2. Brief pause to let Python handle cleanup
        std::thread::sleep(std::time::Duration::from_millis(100));

        // 3. Close stdin (signals EOF to Python)
        {
            let mut stdin = self.stdin.lock().unwrap();
            *stdin = None;
        }

        // 4. Signal the entire process group, then force-kill if needed
        let mut child = self.child.lock().unwrap();
        if let Some(ref mut c) = *child {
            #[cfg(unix)]
            {
                let pid = c.id() as i32;

                // SIGTERM the entire process group (Python + mpv + ffmpeg).
                // Because we spawned with setpgid(0,0), the child IS the
                // group leader and its PID equals the PGID.
                unsafe {
                    libc::killpg(pid, libc::SIGTERM);
                }

                // Wait briefly for graceful exit
                std::thread::sleep(std::time::Duration::from_millis(300));
                match c.try_wait() {
                    Ok(Some(_)) => {}
                    _ => {
                        // Still alive — SIGKILL the entire group
                        log::warn!("Sidecar didn't exit after SIGTERM, sending SIGKILL to pgid {pid}");
                        unsafe {
                            libc::killpg(pid, libc::SIGKILL);
                        }
                        let _ = c.wait();
                    }
                }
            }

            #[cfg(not(unix))]
            {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        *child = None;
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.kill();
    }
}
