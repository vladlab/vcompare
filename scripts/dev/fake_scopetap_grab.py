#!/usr/bin/env python3
"""Stand-in for scopetap's `scopetap_grab --serve`, for tests and for trying
vcompare's timeline mode without the SDI loop.

Speaks the protocol in scopetap's docs/GRAB_PLAN.md §4 (proto=1):

    startup  → READY proto=1 w=<W> h=<H> fmt=rgb48le     | FAIL <message>
    GRAB tc [timeout_ms] → OK <nbytes> key=value ... + <nbytes> raw bytes
                         | ERR <code> key=value ...
    INFO     → OK key=value ...
    QUIT     → exit 0

The "SDI output" is a video file (the monitor's truth). A GRAB for timecode T
returns that file's frame at T, decoded to packed rgb48le, top row first.
If FAKE_GRAB_PLAYHEAD is set, the frame is only returned once that file (the
fake Resolve's playhead, see src-tauri/testdata/fake_resolve) says T, like a
real SDI output that only shows the parked frame; otherwise the GRAB times
out with `ERR timeout have=<playhead>`.

Configured from the environment, because vcompare starts it the same way it
starts the real one (`--serve --tc-base N`):

    FAKE_GRAB_SOURCE     video file to serve (required)
    FAKE_GRAB_START_TC   its start timecode (default 00:00:00:00)
    FAKE_GRAB_SIZE       WxH to scale to, e.g. 3840x2160 (default: source size)
    FAKE_GRAB_PLAYHEAD   playhead file written by the fake Resolve (optional)
    FAKE_GRAB_LEVELS     reported levels (default video)
    FAKE_GRAB_COLORSPACE reported VPID colorspace (default 709)
    FAKE_GRAB_EOTF       reported EOTF (default 0)
    FAKE_GRAB_DELAY_MS   extra wait per GRAB, to simulate settling (default 0)
"""
import json
import os
import subprocess
import sys
import time


def tc_frames(tc, base):
    h, m, s, f = (int(x) for x in tc.replace(";", ":").split(":"))
    # Matches scopetap: a label built from the components (no drop-frame maths).
    return ((h * 60 + m) * 60 + s) * base + f


def probe(path):
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries",
         "stream=width,height,r_frame_rate", "-of", "json", path],
        check=True, capture_output=True, text=True).stdout
    st = json.loads(out)["streams"][0]
    num, den = (int(x) for x in st["r_frame_rate"].split("/"))
    return st["width"], st["height"], num, den


def decode(path, index, num, den, w, h):
    ss = max(index - 0.5, 0) * den / num
    vf = "scale=%d:%d:flags=lanczos,format=rgb48le" % (w, h)
    cmd = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error",
           "-ss", "%.6f" % ss, "-i", path, "-map", "0:v:0",
           "-fps_mode", "passthrough", "-frames:v", "1",
           "-vf", vf, "-f", "rawvideo", "-pix_fmt", "rgb48le", "pipe:1"]
    r = subprocess.run(cmd, capture_output=True)
    if r.returncode != 0 or len(r.stdout) != w * h * 6:
        raise RuntimeError("decode failed: %s" % r.stderr.decode(errors="replace").strip()[-300:])
    return r.stdout


def main():
    args = sys.argv[1:]
    base = 24
    if "--tc-base" in args:
        base = int(args[args.index("--tc-base") + 1])
    out = sys.stdout.buffer

    def line(s):
        out.write((s + "\n").encode())
        out.flush()

    src = os.environ.get("FAKE_GRAB_SOURCE")
    if not src or not os.path.exists(src):
        line("FAIL no producer (FAKE_GRAB_SOURCE not set or missing)")
        return 1
    sw, sh, num, den = probe(src)
    size = os.environ.get("FAKE_GRAB_SIZE")
    w, h = (int(x) for x in size.split("x")) if size else (sw, sh)
    start = tc_frames(os.environ.get("FAKE_GRAB_START_TC", "00:00:00:00"), base)
    playhead = os.environ.get("FAKE_GRAB_PLAYHEAD")
    delay = int(os.environ.get("FAKE_GRAB_DELAY_MS", "0")) / 1000.0
    tags = "levels=%s auto=1 colorspace=%s eotf=%s" % (
        os.environ.get("FAKE_GRAB_LEVELS", "video"),
        os.environ.get("FAKE_GRAB_COLORSPACE", "709"),
        os.environ.get("FAKE_GRAB_EOTF", "0"))
    seq = 0

    line("READY proto=1 w=%d h=%d fmt=rgb48le" % (w, h))
    for raw in sys.stdin:
        parts = raw.split()
        if not parts:
            continue
        cmd = parts[0].upper()
        if cmd == "QUIT":
            return 0
        if cmd == "INFO":
            line("OK w=%d h=%d fmt=rgb48le seq=%d %s" % (w, h, seq, tags))
            continue
        if cmd != "GRAB" or len(parts) < 2:
            line("ERR bad-command")
            continue
        tc = parts[1]
        timeout = (int(parts[2]) if len(parts) > 2 else 5000) / 1000.0
        try:
            want = tc_frames(tc, base)
        except ValueError:
            line("ERR bad-tc")
            continue
        t0 = time.monotonic()
        have = None
        while True:
            if playhead:
                try:
                    with open(playhead) as f:
                        have = f.read().strip()
                except OSError:
                    have = None
            if not playhead or (have and tc_frames(have, base) == want):
                break
            if time.monotonic() - t0 > timeout:
                break
            time.sleep(0.005)
        if playhead and not (have and tc_frames(have, base) == want):
            line("ERR timeout have=%s" % (have or "none"))
            continue
        if delay:
            time.sleep(delay)
        try:
            data = decode(src, want - start, num, den, w, h)
        except Exception as e:  # noqa: BLE001
            line("ERR cuda %s" % str(e).replace("\n", " "))
            continue
        seq += 1
        waited = int((time.monotonic() - t0) * 1000)
        line("OK %d w=%d h=%d fmt=rgb48le tc=%s seq=%d %s clamped=0 waited_ms=%d refines=0"
             % (len(data), w, h, tc, seq, tags, waited))
        out.write(data)
        out.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
