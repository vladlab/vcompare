#!/usr/bin/env python3
"""Stand-in for Resolve's SDI output, for running vcompare's timeline mode
against the *real* scopetap_grab without a GPU or a DeckLink.

scopetap's no-GPU rig (scopetap/tests/grab_rig) builds scopetap_grab against
a host-memory CUDA stand-in, plus `fake_stream`, which owns a ScopeTap ring
and publishes a frame when told `PUB <tc_frames> <value>` (every pixel =
value, top row 1.0, bottom row 0.0). This script watches the fake Resolve's
playhead file and publishes the frame "Resolve is parked on" whenever it
moves, the way scopetap_stream publishes what arrives on the SDI loop.

    scopetap_rig_sdi.py FAKE_STREAM SHM W H PLAYHEAD START_TC TC_BASE

Picture of display frame f: flat grey `value(f - start)` (see VALUE below);
the vcompare rig test builds the deliverable from the same function.
Runs until stdin closes or it is killed.
"""
import os
import subprocess
import sys
import time


def value(k):
    """Grey level of timeline frame k (0 = the start), 0..1."""
    return 0.2 + 0.03 * (k % 20)


def tc_frames(tc, base):
    h, m, s, f = (int(x) for x in tc.replace(";", ":").split(":"))
    return ((h * 60 + m) * 60 + s) * base + f


def main():
    fake, shm, w, h, playhead, start_tc, base = sys.argv[1:8]
    base = int(base)
    start = tc_frames(start_tc, base)
    p = subprocess.Popen([fake, shm, w, h], stdin=subprocess.PIPE, text=True, bufsize=1,
                         stderr=subprocess.DEVNULL)

    def pub(frames):
        p.stdin.write("PUB %d %.6f\n" % (frames, value(frames - start)))
        p.stdin.flush()

    # Something on the wire from the start, so the grabber can say READY.
    pub(start)
    last = None
    try:
        while p.poll() is None:
            try:
                with open(playhead) as f:
                    cur = f.read().strip()
            except OSError:
                cur = None
            if cur and cur != last:
                last = cur
                # Like the SDI output: the old frame for a moment, then the new one.
                time.sleep(0.02)
                pub(tc_frames(cur, base))
            time.sleep(0.005)
    finally:
        try:
            p.stdin.write("STOP\n")
            p.stdin.flush()
        except OSError:
            pass
        p.wait(timeout=5)


if __name__ == "__main__":
    main()
