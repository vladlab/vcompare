#!/usr/bin/env python3
"""
vcompare JSON-RPC server — stdio bridge for Tauri sidecar.

Protocol: JSON-RPC 2.0 over newline-delimited stdin/stdout.
All logging goes to stderr. stdout is EXCLUSIVELY for protocol messages.

Notifications (no id) are sent for streaming events:
  - progress: {current, total}
  - diagnostics: {lines: [...]}
  - mpv.position: {frame}
  - analysis.error: {message}
"""

from __future__ import annotations

import json
import sys
import threading
import traceback
from typing import Any, Callable

# ── Ensure stdout is line-buffered for JSON-RPC ──
# (Python 3 defaults to block-buffered when stdout is a pipe)
sys.stdout.reconfigure(line_buffering=True)


def log(msg: str) -> None:
    """Log to stderr (never stdout — that's the RPC channel)."""
    print(f"[vcompare-rpc] {msg}", file=sys.stderr, flush=True)


class JsonRpcServer:
    """
    Single-connection JSON-RPC 2.0 server over stdin/stdout.

    Methods are registered with @server.method("name") or server.register().
    Long-running methods (like analyze) run in a thread so the server
    can still accept cancellation requests.

    Notifications (server→client) are sent with server.notify().
    Thread-safe: notify() can be called from any thread (analysis workers,
    mpv poll timers, etc.)
    """

    def __init__(self):
        self._methods: dict[str, Callable] = {}
        self._write_lock = threading.Lock()
        self._running = True
        self._pending_tasks: dict[int, threading.Thread] = {}

    def register(self, name: str, handler: Callable) -> None:
        self._methods[name] = handler

    def method(self, name: str):
        """Decorator: @server.method("analyze")"""
        def decorator(fn):
            self.register(name, fn)
            return fn
        return decorator

    def notify(self, method: str, params: dict[str, Any]) -> None:
        """
        Send a JSON-RPC notification (no id → no response expected).
        Thread-safe — called from analysis workers, mpv poll, etc.
        """
        self._write({"jsonrpc": "2.0", "method": method, "params": params})

    def respond(self, req_id: int | str, result: Any) -> None:
        """Send a successful response."""
        self._write({"jsonrpc": "2.0", "id": req_id, "result": result})

    def respond_error(self, req_id: int | str | None, code: int, message: str) -> None:
        """Send an error response."""
        msg: dict[str, Any] = {
            "jsonrpc": "2.0",
            "error": {"code": code, "message": message},
        }
        if req_id is not None:
            msg["id"] = req_id
        self._write(msg)

    def _write(self, msg: dict) -> None:
        """Write a JSON message to stdout. Thread-safe."""
        line = json.dumps(msg, separators=(",", ":"))
        with self._write_lock:
            try:
                sys.stdout.write(line + "\n")
                sys.stdout.flush()
            except (BrokenPipeError, OSError):
                # Parent process died — exit cleanly
                self._running = False

    def run(self) -> None:
        """
        Main loop: read stdin line-by-line, dispatch methods.
        Blocks until stdin closes (Tauri kills the sidecar).
        """
        log("Server started, waiting for requests...")

        for line in sys.stdin:
            if not self._running:
                break

            line = line.strip()
            if not line:
                continue

            try:
                req = json.loads(line)
            except json.JSONDecodeError as e:
                self.respond_error(None, -32700, f"Parse error: {e}")
                continue

            method_name = req.get("method")
            req_id = req.get("id")  # None for notifications from client
            params = req.get("params", {})

            if method_name not in self._methods:
                if req_id is not None:
                    self.respond_error(req_id, -32601, f"Method not found: {method_name}")
                continue

            handler = self._methods[method_name]

            # Check if handler is marked as async (long-running)
            if getattr(handler, "_rpc_async", False):
                # Run in thread so we can still accept cancel/status requests
                t = threading.Thread(
                    target=self._run_async_handler,
                    args=(handler, params, req_id),
                    daemon=True,
                )
                if req_id is not None:
                    self._pending_tasks[req_id] = t
                t.start()
            else:
                # Run inline (fast methods: mpv.seek, get_frames, etc.)
                try:
                    result = handler(params)
                    if req_id is not None:
                        self.respond(req_id, result)
                except Exception as e:
                    log(f"Error in {method_name}: {e}")
                    if req_id is not None:
                        self.respond_error(req_id, -32000, str(e))

        log("stdin closed, shutting down")
        self._running = False

    def _run_async_handler(self, handler, params, req_id):
        try:
            result = handler(params)
            if req_id is not None:
                self.respond(req_id, result)
        except Exception as e:
            log(f"Async handler error: {traceback.format_exc()}")
            if req_id is not None:
                self.respond_error(req_id, -32000, str(e))
        finally:
            if req_id is not None:
                self._pending_tasks.pop(req_id, None)


def async_method(fn):
    """Decorator: marks a handler to run in a background thread."""
    fn._rpc_async = True
    return fn
