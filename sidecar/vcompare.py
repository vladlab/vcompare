#!/usr/bin/env python3
"""
vcompare — Video quality comparison TUI

Compares two video files using FFmpeg's libvmaf (with optional PSNR),
then displays an interactive graph and data table of per-frame metrics
with accurate timecodes auto-detected from the source.

Usage:
    python vcompare.py <reference> <distorted> [options]

Requirements:
    - Python 3.10+
    - ffmpeg/ffprobe built with libvmaf
    - pip: textual
"""

from __future__ import annotations

import argparse
import atexit
import json
import math
import os
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time
from collections import deque
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path

from rich.segment import Segment
from rich.style import Style
from rich.text import Text

from textual import on, work
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Vertical
from textual.geometry import Size
from textual.message import Message
from textual.reactive import reactive
from textual.screen import ModalScreen
from textual.strip import Strip
from textual.widget import Widget
from textual.widgets import (
    DataTable,
    Footer,
    Header,
    Input,
    Label,
    ProgressBar,
    Static,
)

# Optional IMF support — imf_concat.py must be alongside this script
try:
    import importlib.util as _ilu
    _imf_spec = _ilu.spec_from_file_location(
        "imf_concat",
        os.path.join(os.path.dirname(os.path.abspath(__file__)), "imf_concat.py"),
    )
    if _imf_spec and _imf_spec.loader:
        imf_concat = _ilu.module_from_spec(_imf_spec)
        sys.modules["imf_concat"] = imf_concat  # register before exec for dataclasses
        _imf_spec.loader.exec_module(imf_concat)
        HAS_IMF = True
    else:
        HAS_IMF = False
except Exception:
    HAS_IMF = False


# ─── Timecode helpers ────────────────────────────────────────────────────────

class TCType(Enum):
    NON_DROP = "NDF"
    DROP_FRAME = "DF"


@dataclass
class TimecodeFormat:
    fps_num: int
    fps_den: int
    tc_type: TCType

    @property
    def fps_float(self) -> float:
        return self.fps_num / self.fps_den

    @property
    def nominal_fps(self) -> int:
        return math.ceil(self.fps_float)

    @property
    def separator(self) -> str:
        return ";" if self.tc_type == TCType.DROP_FRAME else ":"

    def frame_to_tc(self, frame_num: int) -> str:
        if self.tc_type == TCType.DROP_FRAME:
            return self._frame_to_df(frame_num)
        return self._frame_to_ndf(frame_num)

    def _frame_to_ndf(self, fn: int) -> str:
        nom = self.nominal_fps
        ff = fn % nom
        s = (fn // nom) % 60
        m = (fn // (nom * 60)) % 60
        h = fn // (nom * 3600)
        return f"{h:02d}:{m:02d}:{s:02d}:{ff:02d}"

    def _frame_to_df(self, fn: int) -> str:
        nom = self.nominal_fps
        drop = 2 if nom == 30 else 4

        frames_per_10min = nom * 60 * 10 - drop * 9
        d = fn // frames_per_10min
        m = fn % frames_per_10min

        if m < drop:
            adj_frames = fn + drop * 9 * d
        else:
            adj_frames = fn + drop * 9 * d + drop * ((m - drop) // (nom * 60 - drop))

        ff = adj_frames % nom
        s = (adj_frames // nom) % 60
        mi = (adj_frames // (nom * 60)) % 60
        h = adj_frames // (nom * 3600)
        return f"{h:02d}:{mi:02d}:{s:02d};{ff:02d}"


def detect_timecode_format(probe_info: dict) -> TimecodeFormat:
    video = None
    for s in probe_info.get("streams", []):
        if s.get("codec_type") == "video":
            video = s
            break
    if not video:
        raise RuntimeError("No video stream found")

    r_fr = video.get("r_frame_rate", "24/1")
    num, den = (int(x) for x in r_fr.split("/"))
    fps = num / den

    is_drop = False
    if den == 1001 and num in (30000, 60000):
        is_drop = True
    elif abs(fps - 29.97) < 0.01 or abs(fps - 59.94) < 0.01:
        is_drop = True

    tc_type = TCType.DROP_FRAME if is_drop else TCType.NON_DROP
    return TimecodeFormat(fps_num=num, fps_den=den, tc_type=tc_type)


@dataclass
class VideoInfo:
    """Key properties extracted from a video file via ffprobe."""
    width: int
    height: int
    fps_num: int
    fps_den: int
    nb_frames: int | None
    pix_fmt: str
    start_tc: str | None  # embedded timecode string, e.g. "01:00:00:00"
    start_tc_frames: int  # start TC converted to an absolute frame number


def get_video_info(probe: dict, tc_fmt: TimecodeFormat) -> VideoInfo:
    """Extract resolution, frame count, and start timecode from ffprobe output."""
    video = None
    for s in probe.get("streams", []):
        if s.get("codec_type") == "video":
            video = s
            break
    if not video:
        raise RuntimeError("No video stream found")

    width = int(video.get("width", 0))
    height = int(video.get("height", 0))
    r_fr = video.get("r_frame_rate", "24/1")
    fps_num, fps_den = (int(x) for x in r_fr.split("/"))
    nb_frames = int(video.get("nb_frames", 0)) or None
    pix_fmt = video.get("pix_fmt", "yuv420p")

    # Find start timecode — check stream tags, then format tags
    start_tc = None
    for source in (video.get("tags", {}), probe.get("format", {}).get("tags", {})):
        tc = source.get("timecode")
        if tc:
            start_tc = tc
            break

    start_tc_frames = 0
    if start_tc:
        start_tc_frames = tc_string_to_frames(start_tc, tc_fmt)

    return VideoInfo(
        width=width,
        height=height,
        fps_num=fps_num,
        fps_den=fps_den,
        nb_frames=nb_frames,
        pix_fmt=pix_fmt,
        start_tc=start_tc,
        start_tc_frames=start_tc_frames,
    )


def tc_string_to_frames(tc: str, tc_fmt: TimecodeFormat) -> int:
    """Parse a timecode string (HH:MM:SS:FF or HH:MM:SS;FF) to frame number."""
    # Normalize separators
    tc = tc.replace(";", ":").strip()
    parts = tc.split(":")
    if len(parts) != 4:
        return 0
    try:
        h, m, s, ff = int(parts[0]), int(parts[1]), int(parts[2]), int(parts[3])
    except ValueError:
        return 0

    nom = tc_fmt.nominal_fps

    if tc_fmt.tc_type == TCType.DROP_FRAME:
        # Reverse SMPTE drop-frame: convert HH:MM:SS;FF → frame count
        drop = 2 if nom == 30 else 4
        total_minutes = h * 60 + m
        frames = (
            h * 3600 * nom
            + m * 60 * nom
            + s * nom
            + ff
            - drop * (total_minutes - total_minutes // 10)
        )
        return frames
    else:
        return h * 3600 * nom + m * 60 * nom + s * nom + ff


# ─── FFmpeg / ffprobe wrappers ───────────────────────────────────────────────

def detect_hwaccel() -> str | None:
    """Auto-detect the best available hardware decoder for the current platform."""
    plat = sys.platform

    # Platform-preferred order
    if plat == "darwin":
        candidates = ["videotoolbox"]
    elif plat == "linux":
        candidates = ["cuda", "vaapi"]
    else:
        candidates = ["cuda", "vaapi"]

    # Probe ffmpeg for supported hwaccels
    try:
        result = subprocess.run(
            ["ffmpeg", "-hide_banner", "-hwaccels"],
            capture_output=True, text=True, timeout=5,
        )
        supported = result.stdout.lower()
    except (OSError, subprocess.SubprocessError):
        return None

    for hw in candidates:
        if hw in supported:
            return hw

    return None


def detect_vmaf_cuda() -> bool:
    """Check if ffmpeg was built with the libvmaf_cuda filter."""
    try:
        result = subprocess.run(
            ["ffmpeg", "-hide_banner", "-filters"],
            capture_output=True, text=True, timeout=5,
        )
        for line in result.stdout.splitlines():
            if "libvmaf_cuda" in line:
                return True
    except (OSError, subprocess.SubprocessError):
        pass
    return False


def ffprobe_json(path: str) -> dict:
    """Run ffprobe and return parsed JSON. Handles concat manifests."""
    cmd = [
        "ffprobe", "-v", "quiet",
        "-print_format", "json",
        "-show_format", "-show_streams",
    ]
    if _is_concat_manifest(path):
        cmd.extend(["-f", "concat", "-safe", "0"])
    cmd.append(str(path))
    result = subprocess.run(cmd, capture_output=True, text=True, check=True)
    return json.loads(result.stdout)


def _is_concat_manifest(path: str) -> bool:
    """Check if a file is an ffmpeg concat demuxer manifest."""
    if not os.path.isfile(path):
        return False
    try:
        with open(path, "r") as f:
            first_line = f.readline(64).strip()
        return first_line.startswith("ffconcat version")
    except (OSError, UnicodeDecodeError):
        return False


def _is_imf_cpl(path: str) -> bool:
    """Check if a file is an IMF Composition Playlist XML."""
    if not os.path.isfile(path):
        return False
    if not path.lower().endswith(".xml"):
        return False
    try:
        with open(path, "r", encoding="utf-8") as f:
            head = f.read(2048)
        return "CompositionPlaylist" in head
    except (OSError, UnicodeDecodeError):
        return False


@dataclass
class ImfResolveResult:
    """Result of resolving an IMF CPL to a playable concat manifest."""
    concat_path: str  # path to generated ffconcat manifest
    cpl_title: str
    total_resources: int
    resolved_count: int
    unresolved_count: int
    segment_count: int
    total_frames: int
    fps: float
    cpl: object = None  # imf_concat.CompositionPlaylist


def resolve_imf_cpl(
    cpl_path: str,
    extra_package_dirs: list[str] | None = None,
) -> ImfResolveResult:
    """Parse an IMF CPL and generate an ffmpeg concat manifest.

    Args:
        cpl_path: Path to the CPL XML file.
        extra_package_dirs: Additional IMF package directories to search
            for referenced MXF files (for supplemental packages).

    Returns:
        ImfResolveResult with the path to the generated concat manifest.

    Raises:
        RuntimeError: If imf_concat module is not available.
    """
    if not HAS_IMF:
        raise RuntimeError(
            "IMF support requires imf_concat.py alongside vcompare.py"
        )

    cpl_dir = os.path.dirname(os.path.abspath(cpl_path))

    # Find and parse primary ASSETMAP
    assetmap_path = imf_concat.find_assetmap(cpl_dir)
    if not assetmap_path:
        raise RuntimeError(f"ASSETMAP.xml not found near {cpl_path}")

    assetmap_dir = os.path.dirname(os.path.abspath(assetmap_path))
    uuid_map = imf_concat.parse_assetmap(assetmap_path)
    search_dirs = [assetmap_dir]

    # Parse additional package ASSETMAPs
    for pkg_dir in (extra_package_dirs or []):
        pkg_dir = os.path.abspath(pkg_dir)
        if not os.path.isdir(pkg_dir):
            continue
        pkg_am = imf_concat.find_assetmap(pkg_dir)
        if pkg_am:
            pkg_map = imf_concat.parse_assetmap(pkg_am)
            for uid, path in pkg_map.items():
                if uid not in uuid_map:
                    uuid_map[uid] = path
            pkg_am_dir = os.path.dirname(os.path.abspath(pkg_am))
            if pkg_am_dir not in search_dirs:
                search_dirs.append(pkg_am_dir)
        if pkg_dir not in search_dirs:
            search_dirs.append(pkg_dir)

    # Parse CPL
    cpl = imf_concat.parse_cpl(cpl_path)

    # Resolve paths
    unresolved = imf_concat.resolve_paths(cpl, uuid_map, search_dirs)

    # Collect video resources
    resources = imf_concat.get_video_resources(cpl)
    resolved = [r for r in resources if r.file_path]

    if not resolved:
        raise RuntimeError("No video resources could be resolved from CPL")

    # Generate concat manifest to a temp file
    manifest = imf_concat.generate_concat_manifest(resolved)
    tmp_dir = tempfile.mkdtemp(prefix="vcompare_imf_")
    concat_path = os.path.join(tmp_dir, "imf_video.txt")
    with open(concat_path, "w") as f:
        f.write(manifest)

    total_frames = sum(
        r.source_duration * r.repeat_count for r in resources
    )

    return ImfResolveResult(
        concat_path=concat_path,
        cpl_title=cpl.title,
        total_resources=len(resources),
        resolved_count=len(resolved),
        unresolved_count=unresolved,
        segment_count=len(cpl.segments),
        total_frames=total_frames,
        fps=cpl.edit_rate.fps,
        cpl=cpl,
    )


@dataclass
class MetricFrame:
    frame: int
    timecode: str
    psnr: float | None
    vmaf: float | None


@dataclass
class AnalysisResult:
    frames: list[MetricFrame] = field(default_factory=list)
    tc_format: TimecodeFormat | None = None
    ref_path: str = ""
    dist_path: str = ""
    ref_start_frame: int = 0       # frame offset for TC display (overlap start)
    ref_file_start_tc: int = 0     # ref file's own start TC in frames (for mpv seeking)
    sync_mode: str = "start"       # "start" or "tc"


def save_analysis(result: AnalysisResult, path: str) -> None:
    """Save analysis result to a JSON file."""
    tc = result.tc_format
    data = {
        "version": 2,
        "ref_path": result.ref_path,
        "dist_path": result.dist_path,
        "ref_start_frame": result.ref_start_frame,
        "ref_file_start_tc": result.ref_file_start_tc,
        "sync_mode": result.sync_mode,
        "tc_format": {
            "fps_num": tc.fps_num,
            "fps_den": tc.fps_den,
            "tc_type": tc.tc_type.value,
        } if tc else None,
        "frames": [
            {
                "frame": f.frame,
                "timecode": f.timecode,
                "psnr": f.psnr,
                "vmaf": f.vmaf,
            }
            for f in result.frames
        ],
    }
    with open(path, "w") as f:
        json.dump(data, f)


def load_analysis(path: str) -> AnalysisResult:
    """Load analysis result from a saved JSON file."""
    with open(path) as f:
        data = json.load(f)

    tc_data = data.get("tc_format")
    tc_fmt = None
    if tc_data:
        tc_fmt = TimecodeFormat(
            fps_num=tc_data["fps_num"],
            fps_den=tc_data["fps_den"],
            tc_type=TCType(tc_data["tc_type"]),
        )

    frames = [
        MetricFrame(
            frame=fd["frame"],
            timecode=fd["timecode"],
            psnr=fd.get("psnr"),
            vmaf=fd.get("vmaf"),
        )
        for fd in data.get("frames", [])
    ]

    return AnalysisResult(
        frames=frames,
        tc_format=tc_fmt,
        ref_path=data.get("ref_path", ""),
        dist_path=data.get("dist_path", ""),
        ref_start_frame=data.get("ref_start_frame", 0),
        ref_file_start_tc=data.get("ref_file_start_tc", 0),
        sync_mode=data.get("sync_mode", "start"),
    )


# ─── Pixel format negotiation ────────────────────────────────────────────────

# Subsampling rank: higher = more chroma data preserved
_SUBSAMP_RANK = {"420": 0, "422": 1, "444": 2}

def _parse_pix_fmt(fmt: str) -> tuple[str, int, bool]:
    """Parse an ffmpeg pix_fmt into (subsampling, bit_depth, is_rgb).

    Returns:
        ("420"|"422"|"444", 8|10|12|16, is_rgb)
    """
    f = fmt.lower().strip()

    # RGB family → treat as 444 equivalent
    is_rgb = False
    if f.startswith(("rgb", "bgr", "gbr", "gbrp", "rgba", "bgra")):
        is_rgb = True
        if "48" in f or "16" in f:
            return ("444", 16, True)
        elif "10" in f:
            return ("444", 10, True)
        elif "12" in f:
            return ("444", 12, True)
        return ("444", 8, True)

    # Detect bit depth from suffix
    bits = 8
    for b in (16, 12, 10):
        if f"p{b}" in f or f"{b}le" in f or f"{b}be" in f:
            bits = b
            break

    # Detect subsampling from name
    if "444" in f:
        sub = "444"
    elif "422" in f:
        sub = "422"
    elif "nv12" in f or "nv21" in f or "420" in f:
        sub = "420"
    else:
        # Fallback: most planar formats without a number are 420
        sub = "420"

    return (sub, bits, is_rgb)


def choose_common_pixfmt(ref_fmt: str, dist_fmt: str) -> tuple[str, str | None, str | None]:
    """Choose a common pixel format for libvmaf comparison.

    Strategy: use the LOWER subsampling and LOWER bit depth between the two.
    For comparison analysis, both streams just need to match — downconverting
    is fine since VMAF/PSNR measure relative differences. This avoids
    unnecessary upconversion work on large frames.

    Returns:
        (target_fmt, ref_convert_fmt_or_None, dist_convert_fmt_or_None)
        None means no conversion needed for that stream.
    """
    ref_sub, ref_bits, ref_rgb = _parse_pix_fmt(ref_fmt)
    dist_sub, dist_bits, dist_rgb = _parse_pix_fmt(dist_fmt)

    # If identical, no conversion needed
    if ref_fmt == dist_fmt:
        return (ref_fmt, None, None)

    # Pick the lower of each dimension (cheaper conversion, same comparison)
    target_sub = ref_sub if _SUBSAMP_RANK.get(ref_sub, 0) <= _SUBSAMP_RANK.get(dist_sub, 0) else dist_sub
    target_bits = min(ref_bits, dist_bits)

    # Construct target format name (always YUV planar for libvmaf)
    if target_bits == 8:
        target_fmt = f"yuv{target_sub}p"
    else:
        target_fmt = f"yuv{target_sub}p{target_bits}le"

    # Determine which streams need conversion
    ref_target = None
    dist_target = None

    ref_native = f"yuv{ref_sub}p" if ref_bits == 8 else f"yuv{ref_sub}p{ref_bits}le"
    if ref_rgb or ref_native != target_fmt:
        ref_target = target_fmt

    dist_native = f"yuv{dist_sub}p" if dist_bits == 8 else f"yuv{dist_sub}p{dist_bits}le"
    if dist_rgb or dist_native != target_fmt:
        dist_target = target_fmt

    return (target_fmt, ref_target, dist_target)


@dataclass
class ComparisonConfig:
    """Computed parameters for the ffmpeg comparison pass."""
    ref_trim_frames: int = 0
    dist_trim_frames: int = 0
    ref_seek_sec: float = 0.0        # -ss value for ref (computed from trim_frames + fps)
    dist_seek_sec: float = 0.0       # -ss value for dist
    scale_w: int = 0  # 0 = no scaling needed
    scale_h: int = 0
    ref_pixfmt: str | None = None    # format filter for ref (None = no conversion)
    dist_pixfmt: str | None = None   # format filter for dist (None = no conversion)
    ref_src_pixfmt: str = "yuv420p"  # actual source pixel format from probe
    dist_src_pixfmt: str = "yuv420p" # actual source pixel format from probe
    use_vmaf_cuda: bool = False      # use libvmaf_cuda (full GPU pipeline)
    log_subsample: int = 1           # multiply VMAF log frameNum by this to get real frames
    ref_start_frame: int = 0         # absolute frame for TC display
    ref_file_start_tc: int = 0       # ref file's own start TC (for mpv seeking)
    total_frames: int | None = None  # for progress


def prepare_comparison(
    ref: str,
    dist: str,
    sync_mode: str = "start",
    vmaf_cuda: bool = False,
    offset_frames: int = 0,
    diagnostics: list[str] | None = None,
) -> tuple[ComparisonConfig, TimecodeFormat]:
    """Probe both files and compute trim/scale parameters."""
    if diagnostics is None:
        diagnostics = []

    ref_probe = ffprobe_json(ref)
    dist_probe = ffprobe_json(dist)

    tc_fmt = detect_timecode_format(ref_probe)
    ref_info = get_video_info(ref_probe, tc_fmt)

    # For distorted file, use ref's TC format for parsing
    dist_info = get_video_info(dist_probe, tc_fmt)

    config = ComparisonConfig()
    config.ref_file_start_tc = ref_info.start_tc_frames
    config.use_vmaf_cuda = vmaf_cuda
    config.ref_src_pixfmt = ref_info.pix_fmt
    config.dist_src_pixfmt = dist_info.pix_fmt

    diagnostics.append(
        f"ref: {ref_info.width}x{ref_info.height} {ref_info.pix_fmt} "
        f"{ref_info.fps_num}/{ref_info.fps_den}"
    )
    diagnostics.append(
        f"dist: {dist_info.width}x{dist_info.height} {dist_info.pix_fmt} "
        f"{dist_info.fps_num}/{dist_info.fps_den}"
    )

    # Resolution scaling — scale distorted to match reference
    if ref_info.width != dist_info.width or ref_info.height != dist_info.height:
        config.scale_w = ref_info.width
        config.scale_h = ref_info.height
        diagnostics.append(
            f"resolution: MISMATCH → scaling dist to {ref_info.width}x{ref_info.height}"
        )
    else:
        diagnostics.append(f"resolution: matched ({ref_info.width}x{ref_info.height})")

    # Pixel format negotiation
    target_fmt, ref_conv, dist_conv = choose_common_pixfmt(
        ref_info.pix_fmt, dist_info.pix_fmt
    )
    config.ref_pixfmt = ref_conv
    config.dist_pixfmt = dist_conv

    if ref_conv or dist_conv:
        parts = [f"pixfmt: {ref_info.pix_fmt} vs {dist_info.pix_fmt} → target {target_fmt}"]
        if ref_conv:
            parts.append(f"  ref convert: {ref_info.pix_fmt} → {ref_conv}")
        if dist_conv:
            parts.append(f"  dist convert: {dist_info.pix_fmt} → {dist_conv}")
        diagnostics.extend(parts)
    else:
        diagnostics.append(f"pixfmt: matched ({ref_info.pix_fmt}, no conversion)")

    diagnostics.append(f"sync: {sync_mode}")

    # TC sync
    if sync_mode == "tc":
        ref_start = ref_info.start_tc_frames
        dist_start = dist_info.start_tc_frames

        if ref_start != dist_start:
            overlap_start = max(ref_start, dist_start)
            config.ref_trim_frames = overlap_start - ref_start
            config.dist_trim_frames = overlap_start - dist_start
            config.ref_start_frame = overlap_start

            diagnostics.append(
                f"  tc offset: ref={ref_info.start_tc or '00:00:00:00'}, "
                f"dist={dist_info.start_tc or '00:00:00:00'}"
            )
            if config.ref_trim_frames > 0:
                diagnostics.append(f"  seek ref: {config.ref_trim_frames} frames")
            if config.dist_trim_frames > 0:
                diagnostics.append(f"  seek dist: {config.dist_trim_frames} frames")
        else:
            config.ref_start_frame = ref_start
            diagnostics.append(f"  tc aligned at {ref_info.start_tc or '00:00:00:00'}")
    else:
        # sync=start — use ref's embedded TC for display
        config.ref_start_frame = ref_info.start_tc_frames

    # Apply manual frame offset: positive = skip dist, negative = skip ref
    if offset_frames != 0:
        if offset_frames > 0:
            config.dist_trim_frames += offset_frames
        else:
            config.ref_trim_frames += -offset_frames
            config.ref_start_frame += -offset_frames
        diagnostics.append(f"  manual offset: {offset_frames:+d} frames")

    # Convert trim frames to -ss seconds using rational fps for precision
    if config.ref_trim_frames > 0:
        config.ref_seek_sec = config.ref_trim_frames * ref_info.fps_den / ref_info.fps_num
    if config.dist_trim_frames > 0:
        config.dist_seek_sec = config.dist_trim_frames * dist_info.fps_den / dist_info.fps_num

    # Estimate total frames for progress (from the shorter overlapping region)
    ref_total = ref_info.nb_frames or 0
    dist_total = dist_info.nb_frames or 0
    if ref_total and dist_total:
        ref_avail = ref_total - config.ref_trim_frames
        dist_avail = dist_total - config.dist_trim_frames
        config.total_frames = min(ref_avail, dist_avail)
    elif ref_total:
        config.total_frames = ref_total - config.ref_trim_frames
    elif dist_total:
        config.total_frames = dist_total - config.dist_trim_frames

    return config, tc_fmt


def run_comparison(
    ref: str,
    dist: str,
    log_path: str,
    vmaf_model: str = "vmaf_v0.6.1",
    threads: int = 0,
    subsample: int = 1,
    hwaccel: str | None = None,
    config: ComparisonConfig | None = None,
    progress_callback=None,
    diagnostics: list[str] | None = None,
    diagnostics_callback=None,
    process_holder: list | None = None,
    extra_features: list[str] | None = None,
) -> None:
    if config is None:
        config = ComparisonConfig()
    if diagnostics is None:
        diagnostics = []

    if threads <= 0:
        try:
            threads = os.cpu_count() or 8
        except Exception:
            threads = 8

    # Build optional feature string (psnr)
    feature_str = ""
    if extra_features:
        feature_str = ":feature=" + "|".join(f"name={f}" for f in extra_features)

    # ── Decide: libvmaf_cuda (GPU) vs libvmaf (CPU) ──
    #
    # GPU path: hwupload_cuda + scale_cuda handles any codec, pixel format,
    # input type (concat demuxer), and TC alignment (via -ss input seeking).
    # However, PSNR has no CUDA kernel — it'd force
    # per-frame GPU→CPU round-trips, slower than pure CPU. So when extra
    # features are requested, use the CPU path.
    use_cuda_vmaf = (
        config.use_vmaf_cuda
        and hwaccel == "cuda"
        and not extra_features
    )

    # Helper: build -ss/-f/-i args for one input
    def _input_args_for(path: str, seek_sec: float) -> list[str]:
        args: list[str] = []
        if seek_sec > 0:
            args.extend(["-ss", f"{seek_sec:.6f}"])
        if _is_concat_manifest(path):
            args.extend(["-f", "concat", "-safe", "0"])
        args.extend(["-i", path])
        return args

    if use_cuda_vmaf:
        # ── GPU path: libvmaf_cuda ──
        # CPU decode → hwupload_cuda → scale_cuda=format=yuv420p → libvmaf_cuda
        # Works with any codec (ProRes, DNxHR, H.264, etc.) and any pixel format.
        vmaf_opts = (
            f"log_fmt=json:log_path={log_path}"
            f":model=version={vmaf_model}"
        )
        if feature_str:
            vmaf_opts += feature_str
        if subsample > 1:
            vmaf_opts += f":n_subsample={subsample}"
        config.log_subsample = 1  # frameNum is already real in VMAF log

        # scale_cuda handles both format conversion and resolution in one pass.
        # format=yuv420p is essentially free when input is already yuv420p.
        ref_scale = "format=yuv420p"
        dist_scale = "format=yuv420p"
        if config.scale_w > 0:
            dist_scale = f"w={config.scale_w}:h={config.scale_h}:{dist_scale}"

        all_filters = (
            f"[0:v]hwupload_cuda,scale_cuda={ref_scale}[ref];"
            f"[1:v]hwupload_cuda,scale_cuda={dist_scale}[dist];"
            f"[ref][dist]libvmaf_cuda={vmaf_opts}"
        )

        input_args = ["-init_hw_device", "cuda=cu", "-filter_hw_device", "cu"]
        input_args.extend(_input_args_for(ref, config.ref_seek_sec))
        input_args.extend(_input_args_for(dist, config.dist_seek_sec))

    else:
        # ── CPU path: libvmaf ──

        vmaf_opts = (
            f"log_fmt=json:log_path={log_path}"
            f":model=version={vmaf_model}"
            f":n_threads={threads}"
        )
        if feature_str:
            vmaf_opts += feature_str
        if subsample > 1:
            vmaf_opts += f":n_subsample={subsample}"
        config.log_subsample = 1  # n_subsample: frameNum in log is already real

        # Build per-stream filter chains: format → scale (trim handled by -ss)
        ref_chain: list[str] = []
        dist_chain: list[str] = []

        if config.ref_pixfmt:
            ref_chain.append(f"format={config.ref_pixfmt}")
        if config.dist_pixfmt:
            dist_chain.append(f"format={config.dist_pixfmt}")

        if config.scale_w > 0:
            dist_chain.append(f"scale={config.scale_w}:{config.scale_h}:flags=lanczos")

        filters_before: list[str] = []
        ref_label = "0:v"
        dist_label = "1:v"

        if ref_chain:
            filters_before.append(f"[0:v]{','.join(ref_chain)}[ref]")
            ref_label = "ref"
        if dist_chain:
            filters_before.append(f"[1:v]{','.join(dist_chain)}[dist]")
            dist_label = "dist"

        vmaf_filter = f"[{ref_label}][{dist_label}]libvmaf={vmaf_opts}"
        all_filters = ";".join(filters_before + [vmaf_filter]) if filters_before else vmaf_filter

        input_args = []
        for path, seek_sec in ((ref, config.ref_seek_sec), (dist, config.dist_seek_sec)):
            if hwaccel and not _is_concat_manifest(path):
                input_args.extend(["-hwaccel", hwaccel])
            input_args.extend(_input_args_for(path, seek_sec))

    cmd = [
        "ffmpeg", "-y",
        *input_args,
        "-filter_complex", all_filters,
        "-f", "null", "-",
    ]

    fc_display = all_filters if len(all_filters) < 80 else all_filters[:77] + "…"
    diagnostics.append(f"filter: {fc_display}")

    # Print the ffmpeg command for debugging
    print(f"[vcompare] cmd: {' '.join(cmd)}", file=sys.stderr)

    if diagnostics_callback:
        diagnostics_callback(diagnostics)

    process = subprocess.Popen(
        cmd,
        stderr=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        text=True,
        bufsize=1,
    )

    # Allow external cancellation (e.g. app quit during analysis)
    if process_holder is not None:
        process_holder.append(process)

    frame_re = re.compile(r"frame=\s*(\d+)")
    last_reported = -1
    last_progress_time = 0.0
    stderr_tail: deque[str] = deque(maxlen=20)  # only keep last 20 lines for error reporting

    try:
        for line in process.stderr:
            stderr_tail.append(line)
            m = frame_re.search(line)
            if m and progress_callback and config.total_frames:
                current = int(m.group(1))
                if current != last_reported:
                    last_reported = current
                    # Throttle UI updates to ~4Hz
                    now = time.monotonic()
                    if now - last_progress_time >= 0.25:
                        last_progress_time = now
                        progress_callback(current, config.total_frames)

        process.wait()

        # Final progress update so bar reaches 100%
        if last_reported > 0 and progress_callback and config.total_frames:
            progress_callback(config.total_frames, config.total_frames)

    except Exception:
        # Kill ffmpeg on any error reading stderr or in callbacks
        try:
            process.kill()
        except OSError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
        raise

    finally:
        # Ensure stderr pipe is closed and process is reaped
        if process.stderr and not process.stderr.closed:
            process.stderr.close()
        if process.poll() is None:
            # Still running — force kill
            try:
                process.kill()
            except OSError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
        # Remove from holder
        if process_holder is not None and process in process_holder:
            process_holder.remove(process)

    if process.returncode != 0:
        tail = "".join(stderr_tail).strip()
        raise RuntimeError(
            f"FFmpeg exited with code {process.returncode}\n\n{tail}"
        )


def parse_vmaf_log(
    log_path: str,
    tc_fmt: TimecodeFormat,
    ref_start_frame: int = 0,
    subsample: int = 1,
) -> list[MetricFrame]:
    with open(log_path) as f:
        data = json.load(f)

    # Log detected keys for debugging
    all_frames = data.get("frames", [])
    if all_frames:
        keys = list(all_frames[0].get("metrics", {}).keys())
        print(f"[vcompare] Metric keys in log: {keys}", file=sys.stderr)

    frames = []
    for entry in all_frames:
        fn = entry["frameNum"]
        metrics = entry.get("metrics", {})

        # PSNR key varies by libvmaf version
        psnr = metrics.get("psnr_y")
        if psnr is None:
            psnr = metrics.get("psnr_y (mean)")
        if psnr is None:
            for k, v in metrics.items():
                if "psnr" in k.lower() and isinstance(v, (int, float)):
                    psnr = v
                    break

        # Map frame number: with select filter, VMAF log has sequential
        # numbers (0,1,2,...) that map to real frames (0, N, 2N, ...).
        # With n_subsample (GPU path), frameNum is already the real number.
        real_frame = fn * subsample
        display_frame = real_frame + ref_start_frame

        frames.append(MetricFrame(
            frame=display_frame,
            timecode=tc_fmt.frame_to_tc(display_frame),
            psnr=psnr,
            vmaf=metrics.get("vmaf"),
        ))
    return frames


# ─── mpv IPC controller ──────────────────────────────────────────────────────

class MpvController:
    """Manages an mpv instance with JSON IPC for frame-accurate seeking."""

    def __init__(self, ref_path: str, dist_path: str, tc_format: TimecodeFormat,
                 ref_file_start_tc: int = 0):
        self.ref_path = ref_path
        self.dist_path = dist_path
        self.tc_format = tc_format
        self.ref_file_start_tc = ref_file_start_tc
        self._process: subprocess.Popen | None = None
        self._sock_path = os.path.join(
            tempfile.gettempdir(), f"vcompare_mpv_{os.getpid()}.sock"
        )

    @property
    def is_running(self) -> bool:
        return self._process is not None and self._process.poll() is None

    def launch(self) -> str | None:
        """Launch mpv with side-by-side view. Returns error string or None."""
        if self.is_running:
            return "mpv already running"

        try:
            os.unlink(self._sock_path)
        except FileNotFoundError:
            pass

        # Simple side-by-side — offset alignment is handled by analysis only.
        # lavfi trim breaks mpv seeking, so we don't apply offsets here.
        lavfi = "[vid1][vid2]hstack[vo]"

        cmd = [
            "mpv",
            self.ref_path,
            f"--external-file={self.dist_path}",
            f"--lavfi-complex={lavfi}",
            f"--input-ipc-server={self._sock_path}",
            "--pause",
            "--hr-seek=yes",
            "--keep-open=yes",
            "--force-window=yes",
            "--osd-level=2",
            "--osd-fractions",
            f"--title=vcompare: {Path(self.ref_path).name} | {Path(self.dist_path).name}",
            "--no-terminal",
        ]

        try:
            self._process = subprocess.Popen(
                cmd,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except FileNotFoundError:
            return "mpv not found in PATH"

        for _ in range(50):
            if os.path.exists(self._sock_path):
                return None
            time.sleep(0.1)

        return "mpv started but IPC socket not ready (timeout)"

    def _send_command(self, *args) -> dict | None:
        """Send a JSON IPC command to mpv and return the response."""
        if not self.is_running:
            return None

        msg = json.dumps({"command": list(args)}) + "\n"
        sock = None

        try:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.settimeout(2.0)
            sock.connect(self._sock_path)
            sock.sendall(msg.encode())

            # Read response
            data = b""
            while True:
                chunk = sock.recv(4096)
                if not chunk:
                    break
                data += chunk
                if b"\n" in data:
                    break

            if data:
                return json.loads(data.split(b"\n")[0])
            return None

        except (socket.error, json.JSONDecodeError, OSError):
            return None
        finally:
            if sock:
                try:
                    sock.close()
                except OSError:
                    pass

    def seek_to_frame(self, frame_num: int) -> bool:
        """Seek mpv to a display frame number (absolute TC frame)."""
        if not self.is_running:
            return False
        # Convert absolute TC frame → file-relative seconds
        file_frame = frame_num - self.ref_file_start_tc
        seconds = max(0.0, file_frame / self.tc_format.fps_float)
        result = self._send_command("set_property", "time-pos", seconds)
        return result is not None

    def get_frame(self) -> int | None:
        """Get mpv's current position as a display frame number (absolute TC frame)."""
        if not self.is_running:
            return None
        result = self._send_command("get_property", "time-pos")
        if result and "data" in result:
            try:
                seconds = float(result["data"])
                file_frame = round(seconds * self.tc_format.fps_float)
                return file_frame + self.ref_file_start_tc
            except (ValueError, TypeError):
                return None
        return None

    def step_forward(self) -> bool:
        """Step one frame forward."""
        result = self._send_command("frame-step")
        return result is not None

    def step_backward(self) -> bool:
        """Step one frame backward."""
        result = self._send_command("frame-back-step")
        return result is not None

    def is_paused(self) -> bool | None:
        """Check if mpv is paused. Returns None if unknown."""
        result = self._send_command("get_property", "pause")
        if result and "data" in result:
            return bool(result["data"])
        return None

    def toggle_pause(self) -> bool:
        """Toggle play/pause."""
        result = self._send_command("cycle", "pause")
        return result is not None

    def kill(self) -> None:
        """Terminate mpv and clean up."""
        if self._process:
            self._send_command("quit")
            try:
                self._process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self._process.kill()
                try:
                    self._process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    pass  # OS will reap on parent exit
            self._process = None

        try:
            os.unlink(self._sock_path)
        except FileNotFoundError:
            pass


# ─── Braille graph widget ───────────────────────────────────────────────────
#
# Each terminal cell = 2×4 braille dot matrix, giving 2x horizontal and 4x
# vertical sub-cell resolution. Renders natively in Rich/Textual — no
# external deps, handles resize automatically via render_line().

BRAILLE_BASE = 0x2800
# Dot bit positions within the 2×4 grid
# Col 0 (left):  bits 0,1,2,6 (rows top→bottom)
# Col 1 (right): bits 3,4,5,7 (rows top→bottom)
BRAILLE_DOTS = [
    [0x01, 0x02, 0x04, 0x40],  # left column
    [0x08, 0x10, 0x20, 0x80],  # right column
]

# Styles
STYLE_AXIS = Style(color="grey50")
STYLE_CURSOR = Style(color="white", bold=True)
STYLE_THRESH = Style(color="red")
STYLE_VMAF = Style(color="yellow")
STYLE_PSNR = Style(color="cyan")
STYLE_BELOW = Style(color="red")

METRIC_STYLE = {
    "vmaf": STYLE_VMAF,
    "psnr": STYLE_PSNR,
}


class MetricGraph(Widget):
    """Braille-character graph with cursor, threshold, and zoom."""

    DEFAULT_CSS = """
    MetricGraph {
        height: 100%;
        width: 100%;
    }
    """

    cursor_frame: reactive[int] = reactive(-1)
    zoom_start: reactive[int] = reactive(0)
    zoom_end: reactive[int] = reactive(-1)  # -1 = show all

    def __init__(
        self,
        values: list[float],
        metric_name: str = "VMAF",
        threshold: float | None = None,
        **kwargs,
    ):
        super().__init__(**kwargs)
        self._values: list[float] = values
        self._metric_name: str = metric_name
        self._threshold: float | None = threshold
        self._zoom_window: tuple[int, int] | None = None  # (start, end) for minimap highlight

    def update_data(
        self,
        values: list[float],
        metric_name: str | None = None,
        threshold: float | None = None,
        zoom_window: tuple[int, int] | None = None,
    ) -> None:
        self._values = values
        if metric_name is not None:
            self._metric_name = metric_name
        self._threshold = threshold
        self._zoom_window = zoom_window
        self.refresh()

    def _visible_range(self) -> tuple[int, int]:
        n = len(self._values)
        if n == 0:
            return 0, 0
        start = max(0, self.zoom_start)
        end = self.zoom_end if self.zoom_end > 0 else n
        return start, min(end, n)

    @staticmethod
    def _y_range(values: list[float]) -> tuple[float, float]:
        if not values:
            return 0.0, 100.0
        lo, hi = min(values), max(values)
        if lo == hi:
            lo -= 1.0
            hi += 1.0
        pad = (hi - lo) * 0.05
        return lo - pad, hi + pad

    # ── rendering ────────────────────────────────────────────────────────

    def render_line(self, y: int) -> Strip:
        w, h = self.size.width, self.size.height

        if w < 10 or h < 4 or not self._values:
            if y == 0:
                msg = "No data" if not self._values else ""
                return Strip([Segment(msg.ljust(w))])
            return Strip.blank(w)

        axis_w = 8
        graph_w = w - axis_w - 1
        graph_h = h - 2  # row 0 = title, last row = x-axis

        if graph_w < 2 or graph_h < 1:
            return Strip.blank(w)

        start, end = self._visible_range()
        vis = self._values[start:end]
        n_vis = len(vis)
        if n_vis == 0:
            return Strip.blank(w)

        y_min, y_max = self._y_range(vis)
        sub_rows = graph_h * 4  # total vertical sub-pixels

        # ── Row 0: title ──
        if y == 0:
            title = f" {self._metric_name}"
            if self._threshold is not None:
                title += f"  threshold={self._threshold:.2f}"
            text = Text(title[:w].ljust(w), style="bold")
            return Strip(text.render(self.app.console))

        # ── Last row: x-axis with frame numbers ──
        if y == h - 1:
            buf = [" "] * w
            # Place frame labels at left, center, right of graph area
            for frac in (0.0, 0.5, 1.0):
                fi = start + int(frac * max(n_vis - 1, 0))
                label = str(fi)
                px = axis_w + int(frac * max(graph_w - 1, 0))
                # Center the label on px
                lx = px - len(label) // 2
                for i, ch in enumerate(label):
                    pos = lx + i
                    if 0 <= pos < w:
                        buf[pos] = ch
            return Strip([Segment("".join(buf), STYLE_AXIS)])

        # ── Graph body ──
        row = y - 1  # 0-indexed graph row (0 = top)
        if row < 0 or row >= graph_h:
            return Strip.blank(w)

        row_sub_top = row * 4

        # Y-axis label — show at top, middle, bottom
        axis_label = ""
        if row == 0:
            axis_label = f"{y_max:>7.1f}"
        elif row == graph_h // 2:
            axis_label = f"{(y_max + y_min) / 2:>7.1f}"
        elif row == graph_h - 1:
            axis_label = f"{y_min:>7.1f}"
        axis_str = axis_label.ljust(axis_w)[:axis_w]

        # Helper: value → sub-pixel y
        def val_to_suby(v: float) -> int:
            if y_max == y_min:
                return sub_rows // 2
            return round((1.0 - (v - y_min) / (y_max - y_min)) * (sub_rows - 1))

        thresh_suby = val_to_suby(self._threshold) if self._threshold is not None else None

        # Cursor column in graph space
        cursor_col = None
        if 0 <= self.cursor_frame - start < n_vis:
            cursor_col = round(
                (self.cursor_frame - start) / max(n_vis - 1, 1) * max(graph_w - 1, 0)
            )

        metric_key = self._metric_name.lower().split()[0]  # handle "Overview (VMAF)"
        if "psnr" in self._metric_name.lower():
            metric_key = "psnr"
        elif "vmaf" in self._metric_name.lower():
            metric_key = "vmaf"
        line_style = METRIC_STYLE.get(metric_key, STYLE_VMAF)

        # Build cells
        segments: list[Segment] = [Segment(axis_str, STYLE_AXIS)]

        # Precompute: for each column, the sub-pixel y range to draw.
        # We interpolate between adjacent data points to ensure a
        # continuous line with no gaps at steep transitions.

        # Map each column to an interpolated value
        col_values: list[tuple[float, float]] = []  # (min_val, max_val) per col
        for col in range(graph_w):
            # Fractional position into the data
            frac = col / max(graph_w - 1, 1) * max(n_vis - 1, 0)
            idx = int(frac)
            t = frac - idx
            idx = min(idx, n_vis - 1)
            idx_next = min(idx + 1, n_vis - 1)

            # Interpolated value at this column
            v = vis[idx] * (1 - t) + vis[idx_next] * t
            col_values.append((v, v))

        # Also include the bucket min/max for columns that span multiple
        # data points (when zoomed out), and connect to neighbors
        for col in range(graph_w):
            d_start = int(col / graph_w * n_vis)
            d_end = max(int((col + 1) / graph_w * n_vis), d_start + 1)
            bucket = vis[d_start:min(d_end, n_vis)]
            if bucket:
                bmin, bmax = min(bucket), max(bucket)
                old_min, old_max = col_values[col]
                col_values[col] = (min(old_min, bmin), max(old_max, bmax))

        # Connect adjacent columns: extend each column's range to touch
        # its neighbors, ensuring no vertical gaps
        col_suby: list[tuple[int, int]] = []  # (top_suby, bot_suby) per col
        for col in range(graph_w):
            vmin, vmax = col_values[col]

            # Also include the midpoint between this and adjacent columns
            if col > 0:
                prev_min, prev_max = col_values[col - 1]
                vmin = min(vmin, (prev_min + vmin) / 2, (prev_max + vmin) / 2)
                vmax = max(vmax, (prev_max + vmax) / 2, (prev_min + vmax) / 2)
            if col < graph_w - 1:
                next_min, next_max = col_values[col + 1]
                vmin = min(vmin, (next_min + vmin) / 2, (next_max + vmin) / 2)
                vmax = max(vmax, (next_max + vmax) / 2, (next_min + vmax) / 2)

            col_suby.append((val_to_suby(vmax), val_to_suby(vmin)))

        # Batch: compute braille char + style for each column
        prev_style = None
        run_chars: list[str] = []

        # Zoom window indicator for minimap: columns inside the window get a bg
        zw_col_start = -1
        zw_col_end = -1
        if self._zoom_window and n_vis > 0:
            zw_s, zw_e = self._zoom_window
            zw_col_start = round(max(0, zw_s - start) / max(n_vis - 1, 1) * max(graph_w - 1, 0))
            zw_col_end = round(max(0, zw_e - start - 1) / max(n_vis - 1, 1) * max(graph_w - 1, 0))

        for col in range(graph_w):
            suby_top, suby_bot = col_suby[col]
            vmin = col_values[col][0]

            braille = 0
            # Data line — left column dots
            for dot in range(4):
                if suby_top <= row_sub_top + dot <= suby_bot:
                    braille |= BRAILLE_DOTS[0][dot]

            # Threshold — right column dots
            if thresh_suby is not None:
                for dot in range(4):
                    if row_sub_top + dot == thresh_suby:
                        braille |= BRAILLE_DOTS[1][dot]

            # Style — zoom window highlight dims outside region
            is_cursor = (col == cursor_col)
            below = self._threshold is not None and vmin < self._threshold
            in_zoom = zw_col_start <= col <= zw_col_end

            if is_cursor:
                st = STYLE_CURSOR
            elif below:
                st = STYLE_BELOW
            elif in_zoom:
                st = line_style + Style(bgcolor="grey19")
            elif zw_col_start >= 0:
                # Zoom window active but this column is outside — dim it
                st = Style(color="grey35")
            else:
                st = line_style

            ch = chr(BRAILLE_BASE + braille) if braille else " "

            # Merge adjacent same-style runs
            if st == prev_style:
                run_chars.append(ch)
            else:
                if run_chars:
                    segments.append(Segment("".join(run_chars), prev_style))
                run_chars = [ch]
                prev_style = st

        if run_chars:
            segments.append(Segment("".join(run_chars), prev_style))

        return Strip(segments, w)

    def get_content_width(self, container: Size, viewport: Size) -> int:
        return container.width

    def get_content_height(self, container: Size, viewport: Size, width: int) -> int:
        return container.height

    class FrameClicked(Message):
        """Posted when user clicks on the graph."""
        def __init__(self, frame_index: int) -> None:
            super().__init__()
            self.frame_index = frame_index

    def on_click(self, event) -> None:
        """Map click x-position to a frame index and post message."""
        w, h = self.size.width, self.size.height
        axis_w = 8
        graph_w = w - axis_w - 1

        if graph_w < 2 or not self._values:
            return

        # Click x relative to graph area
        gx = event.x - axis_w
        if gx < 0 or gx >= graph_w:
            return

        start, end = self._visible_range()
        n_vis = end - start
        if n_vis <= 0:
            return

        frame_index = start + int(gx / max(graph_w - 1, 1) * max(n_vis - 1, 0))
        frame_index = max(start, min(frame_index, end - 1))

        self.cursor_frame = frame_index
        self.post_message(self.FrameClicked(frame_index))

    def watch_cursor_frame(self) -> None:
        self.refresh()

    def watch_zoom_start(self) -> None:
        self.refresh()

    def watch_zoom_end(self) -> None:
        self.refresh()


# ─── Textual TUI ─────────────────────────────────────────────────────────────

METRICS = ["vmaf", "psnr"]


class StatusBar(Static):
    pass


class ImfResolveScreen(ModalScreen[list[str] | None]):
    """Modal screen for resolving IMF supplemental package paths."""

    BINDINGS = [
        Binding("escape", "cancel", "Cancel"),
    ]

    CSS = """
    ImfResolveScreen {
        align: center middle;
    }

    #imf-dialog {
        width: 80;
        max-height: 24;
        border: heavy $accent;
        background: $surface;
        padding: 1 2;
    }

    #imf-title {
        text-style: bold;
        margin-bottom: 1;
    }

    #imf-info {
        margin-bottom: 1;
    }

    #imf-warn {
        color: $warning;
        margin-bottom: 1;
    }

    #imf-prompt {
        margin-bottom: 0;
    }

    #imf-input {
        margin-bottom: 1;
    }

    #imf-hint {
        color: $text-muted;
    }
    """

    def __init__(
        self,
        cpl_title: str,
        segment_count: int,
        total_resources: int,
        resolved_count: int,
        unresolved_count: int,
    ):
        super().__init__()
        self.cpl_title = cpl_title
        self.segment_count = segment_count
        self.total_resources = total_resources
        self.resolved_count = resolved_count
        self.unresolved_count = unresolved_count

    def compose(self) -> ComposeResult:
        with Vertical(id="imf-dialog"):
            yield Label(f"IMF: {self.cpl_title}", id="imf-title")
            yield Label(
                f"{self.segment_count} segment(s), "
                f"{self.total_resources} video resource(s), "
                f"{self.resolved_count} resolved",
                id="imf-info",
            )
            yield Label(
                f"⚠ {self.unresolved_count} resource(s) unresolved — "
                f"supplemental package paths needed",
                id="imf-warn",
            )
            yield Label(
                "Enter additional IMF package folder paths (one per line or comma-separated):",
                id="imf-prompt",
            )
            yield Input(
                placeholder="/path/to/original_package, /path/to/another",
                id="imf-input",
            )
            yield Label(
                "Press Enter to resolve · Escape to skip (unresolved assets will be missing)",
                id="imf-hint",
            )

    def on_mount(self) -> None:
        self.query_one("#imf-input", Input).focus()

    @on(Input.Submitted)
    def on_submit(self, event: Input.Submitted) -> None:
        raw = event.value.strip()
        if not raw:
            self.dismiss([])
            return
        # Split on commas and newlines, strip whitespace
        paths = []
        for part in re.split(r"[,\n]+", raw):
            p = part.strip()
            if p:
                paths.append(p)
        self.dismiss(paths)

    def action_cancel(self) -> None:
        self.dismiss(None)


class VCompareTUI(App):
    """Video quality comparison TUI."""

    CSS = """
    Screen {
        layout: vertical;
    }

    #loading-container {
        align: center middle;
        height: 100%;
        width: 100%;
    }

    #loading-label {
        text-align: center;
        margin-bottom: 1;
    }

    #progress {
        width: 60;
        margin: 0 4;
    }

    #diagnostics-label {
        margin-top: 1;
        color: $text-muted;
        text-align: left;
        width: 80;
        max-height: 14;
    }

    #main-container {
        height: 1fr;
        width: 1fr;
    }

    #graph-container {
        height: 14;
        min-height: 8;
        border: solid $primary-background;
    }

    #overview-container {
        height: 8;
        min-height: 6;
        border: solid $primary-background;
    }

    #table-container {
        height: 1fr;
        min-height: 8;
    }

    DataTable {
        height: 1fr;
    }

    DataTable > .datatable--cursor {
        background: $accent;
    }

    #info-bar {
        dock: bottom;
        height: 1;
        background: $accent;
        color: $text;
        padding: 0 1;
    }
    """

    BINDINGS = [
        Binding("m", "cycle_metric", "Metric"),
        Binding("t", "set_threshold", "Threshold"),
        Binding("T", "clear_threshold", "Clear thresh"),
        Binding("w", "jump_worst", "Worst"),
        Binding("b", "jump_best", "Best"),
        Binding("y", "copy_timecode", "Copy TC"),
        Binding("0", "zoom_in", "Zoom +"),
        Binding("9", "zoom_out", "Zoom −"),
        Binding("8", "zoom_reset", "Zoom reset"),
        Binding("f", "filter_below", "Filter"),
        Binding("F", "filter_clear", "Unfilter"),
        Binding("e", "export_csv", "Export"),
        Binding("slash", "sort_toggle", "Sort"),
        Binding("p", "mpv_launch", "mpv"),
        Binding("s", "mpv_seek", "Seek mpv"),
        Binding("a", "mpv_auto_toggle", "Auto-seek"),
        Binding("period", "mpv_step_fwd", "Step →"),
        Binding("comma", "mpv_step_back", "Step ←"),
        Binding("space", "mpv_pause", "Play/Pause"),
        Binding("P", "mpv_kill", "Kill mpv"),
        Binding("q", "quit", "Quit"),
    ]

    metric_index: reactive[int] = reactive(0)
    threshold: reactive[float | None] = reactive(None)
    mpv_auto_seek: reactive[bool] = reactive(False)
    analysis: AnalysisResult | None = None
    sorted_ascending: bool = True
    _display_frames: list[MetricFrame] | None = None

    def __init__(
        self,
        ref_path: str,
        dist_path: str,
        vmaf_model: str = "vmaf_v0.6.1",
        threads: int = 0,
        subsample: int = 1,
        hwaccel: str | None = None,
        sync_mode: str = "start",
        vmaf_cuda: bool = False,
        preloaded: AnalysisResult | None = None,
        save_path: str | None = None,
        extra_features: list[str] | None = None,
        offset_frames: int = 0,
    ):
        super().__init__()
        self.ref_path = ref_path
        self.dist_path = dist_path
        self.vmaf_model = vmaf_model
        self.ffmpeg_threads = threads
        self.subsample = subsample
        self.hwaccel = hwaccel
        self.sync_mode = sync_mode
        self.vmaf_cuda = vmaf_cuda
        self.extra_features = extra_features or []
        self.offset_frames = offset_frames
        self._preloaded = preloaded
        self._save_path = save_path
        self._log_dir = tempfile.mkdtemp(prefix="vcompare_")
        self._log_path = os.path.join(self._log_dir, "vmaf_log.json")
        self._mpv: MpvController | None = None
        self._mpv_poll_timer = None
        self._mpv_seek_until = 0.0  # suppress poll cursor updates until this time
        self._mpv_last_frame: int | None = None  # track mpv position for scrub detection
        # IMF state: original CPL paths before resolution
        self._ref_cpl = ref_path if _is_imf_cpl(ref_path) else None
        self._dist_cpl = dist_path if _is_imf_cpl(dist_path) else None
        self._imf_temps: list[str] = []  # temp concat manifests to clean up
        self._ffmpeg_processes: list[subprocess.Popen] = []  # for cleanup on quit

    @property
    def current_metric(self) -> str:
        return METRICS[self.metric_index % len(METRICS)]

    @property
    def display_frames(self) -> list[MetricFrame]:
        if self._display_frames is not None:
            return self._display_frames
        return self.analysis.frames if self.analysis else []

    def _frame_to_idx(self, frame_num: int) -> int:
        """Convert absolute TC frame number → closest 0-based array index."""
        if not self.analysis or not self.analysis.frames:
            return 0
        frames = self.analysis.frames
        # Binary search for closest frame
        lo, hi = 0, len(frames) - 1
        while lo < hi:
            mid = (lo + hi) // 2
            if frames[mid].frame < frame_num:
                lo = mid + 1
            else:
                hi = mid
        # lo is now the insertion point; check neighbors for closest
        best = lo
        if lo > 0 and abs(frames[lo - 1].frame - frame_num) < abs(frames[lo].frame - frame_num):
            best = lo - 1
        return best

    def _idx_to_frame(self, idx: int) -> int:
        """Convert 0-based array index → absolute TC frame number."""
        if not self.analysis or not self.analysis.frames:
            return 0
        idx = max(0, min(idx, len(self.analysis.frames) - 1))
        return self.analysis.frames[idx].frame

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        with Vertical(id="loading-container"):
            yield Label("Analyzing videos with FFmpeg…", id="loading-label")
            yield ProgressBar(id="progress", total=100, show_eta=True)
            yield Label("", id="diagnostics-label")
        with Vertical(id="main-container"):
            with Vertical(id="graph-container"):
                yield MetricGraph([], metric_name="VMAF", id="graph")
            with Vertical(id="overview-container"):
                yield MetricGraph([], metric_name="Overview", id="overview")
            with Vertical(id="table-container"):
                yield DataTable(id="table")
        yield StatusBar(id="info-bar")
        yield Footer()

    def on_mount(self) -> None:
        self.title = "vcompare"
        self.sub_title = f"{Path(self.ref_path).name} vs {Path(self.dist_path).name}"
        self.query_one("#main-container").display = False
        self.query_one("#info-bar").display = False

        if self._preloaded:
            # Skip analysis, go straight to display
            self.query_one("#loading-label", Label).update("Loading saved analysis…")
            self._on_analysis_complete(self._preloaded)
            self._preloaded = None
        elif self._ref_cpl or self._dist_cpl:
            # IMF input detected — resolve before analysis
            self._resolve_imf_inputs()
        else:
            self.run_analysis()

    def _resolve_imf_inputs(self, extra_dirs: list[str] | None = None) -> None:
        """Resolve IMF CPL inputs to concat manifests."""
        pending_prompt = False

        for attr, cpl_path in [("ref_path", self._ref_cpl), ("dist_path", self._dist_cpl)]:
            if cpl_path is None:
                continue
            try:
                result = resolve_imf_cpl(cpl_path, extra_dirs)
                self._imf_temps.append(result.concat_path)

                label = self.query_one("#loading-label", Label)
                label.update(
                    f"IMF: {result.cpl_title}\n"
                    f"  {result.segment_count} segments, "
                    f"{result.resolved_count}/{result.total_resources} resources resolved"
                )

                if result.unresolved_count > 0 and extra_dirs is None:
                    # First attempt has unresolved — prompt user
                    self.push_screen(
                        ImfResolveScreen(
                            cpl_title=result.cpl_title,
                            segment_count=result.segment_count,
                            total_resources=result.total_resources,
                            resolved_count=result.resolved_count,
                            unresolved_count=result.unresolved_count,
                        ),
                        callback=self._on_imf_paths_received,
                    )
                    pending_prompt = True
                    break  # handle one at a time
                elif result.unresolved_count > 0:
                    self.notify(
                        f"[yellow]IMF: {result.unresolved_count} resource(s) still unresolved[/]",
                        timeout=4,
                    )

                # Substitute the concat manifest as the input
                setattr(self, attr, result.concat_path)

            except Exception as e:
                self.notify(f"[red]IMF error: {e}[/]", timeout=6)
                return

        if not pending_prompt:
            # All resolved (or user already gave paths) — start analysis
            self.sub_title = f"{Path(self.ref_path).name} vs {Path(self.dist_path).name}"
            self.run_analysis()

    def _on_imf_paths_received(self, paths: list[str] | None) -> None:
        """Callback from ImfResolveScreen."""
        if paths is None:
            # User cancelled — proceed with what we have (may have gaps)
            pass
        extra_dirs = paths if paths else []
        # Re-resolve with the additional paths
        self._resolve_imf_inputs(extra_dirs=extra_dirs)

    @work(thread=True)
    def run_analysis(self) -> None:
        try:
            diagnostics: list[str] = []

            # Probe both files, compute trim/scale/TC offset
            config, tc_fmt = prepare_comparison(
                self.ref_path, self.dist_path, self.sync_mode,
                vmaf_cuda=self.vmaf_cuda,
                offset_frames=self.offset_frames,
                diagnostics=diagnostics,
            )

            # Build compute diagnostics before showing anything
            threads = self.ffmpeg_threads
            if threads <= 0:
                try:
                    threads = os.cpu_count() or 8
                except Exception:
                    threads = 8
            diagnostics.append(f"hwaccel decode: {self.hwaccel or 'none (software)'}")

            # Determine compute path (must match run_comparison logic)
            use_gpu = (config.use_vmaf_cuda and self.hwaccel == "cuda"
                       and not self.extra_features)

            if use_gpu:
                diagnostics.append(f"vmaf threads: N/A (GPU path)")
                pipeline = "CPU decode → hwupload_cuda → scale_cuda=yuv420p"
                if config.scale_w > 0:
                    pipeline += f" ({config.scale_w}x{config.scale_h})"
                pipeline += " → libvmaf_cuda"
                if config.ref_seek_sec > 0 or config.dist_seek_sec > 0:
                    pipeline += " (with -ss seek)"
                diagnostics.append(f"vmaf compute: {pipeline}")
            else:
                diagnostics.append(f"vmaf threads: {threads}")
                if config.use_vmaf_cuda and self.extra_features:
                    diagnostics.append("vmaf compute: libvmaf (CPU) — PSNR has no CUDA kernel")
                else:
                    diagnostics.append("vmaf compute: libvmaf (CPU)")

            if self.subsample > 1:
                method = "n_subsample (skips VMAF math, decode still runs)"
                diagnostics.append(f"subsample: {self.subsample}× via {method}")

            _display_names = {"psnr": "PSNR"}
            metrics_list = ["VMAF"] + [_display_names.get(f, f) for f in self.extra_features]
            diagnostics.append(f"metrics: {', '.join(metrics_list)}")

            # Single display call with everything
            self.call_from_thread(self._show_diagnostics, list(diagnostics))

            def on_progress(current, total):
                self.call_from_thread(self._update_progress, current, total)

            def on_diagnostics(diag):
                self.call_from_thread(self._show_diagnostics, diag)

            run_comparison(
                self.ref_path,
                self.dist_path,
                self._log_path,
                vmaf_model=self.vmaf_model,
                threads=self.ffmpeg_threads,
                subsample=self.subsample,
                hwaccel=self.hwaccel,
                config=config,
                progress_callback=on_progress,
                diagnostics=diagnostics,
                diagnostics_callback=on_diagnostics,
                process_holder=self._ffmpeg_processes,
                extra_features=self.extra_features,
            )

            frames = parse_vmaf_log(
                self._log_path, tc_fmt,
                ref_start_frame=config.ref_start_frame,
                subsample=config.log_subsample,
            )
            result = AnalysisResult(
                frames=frames,
                tc_format=tc_fmt,
                ref_path=self.ref_path,
                dist_path=self.dist_path,
                ref_start_frame=config.ref_start_frame,
                ref_file_start_tc=config.ref_file_start_tc,
                sync_mode=self.sync_mode,
            )
            self.call_from_thread(self._on_analysis_complete, result)

        except Exception as e:
            self.call_from_thread(self._on_analysis_error, str(e))

    def _show_diagnostics(self, diagnostics: list[str]) -> None:
        """Display diagnostic lines on the loading screen."""
        try:
            label = self.query_one("#diagnostics-label", Label)
            text = "\n".join(f"  [dim]•[/] {line}" for line in diagnostics)
            label.update(text)
        except Exception:
            pass  # loading screen may have been replaced already

    def _update_progress(self, current: int, total: int) -> None:
        bar = self.query_one("#progress", ProgressBar)
        pct = (current / total) * 100 if total else 0
        bar.update(progress=pct)
        label = self.query_one("#loading-label", Label)
        label.update(f"Analyzing… frame {current:,} / {total:,}")

    def _on_analysis_error(self, error: str) -> None:
        self.query_one("#loading-label", Label).update(
            f"[bold red]Error:[/] {error}\n\nPress q to quit."
        )
        self.query_one("#progress").display = False

    def _on_analysis_complete(self, result: AnalysisResult) -> None:
        self.analysis = result
        self._display_frames = None

        # Auto-save analysis
        if self._save_path:
            try:
                save_analysis(result, self._save_path)
                self.notify(f"Saved: {self._save_path}", timeout=3)
            except Exception as e:
                self.notify(f"[red]Save error:[/] {e}", timeout=4)

        self.query_one("#loading-container").display = False
        self.query_one("#main-container").display = True
        self.query_one("#info-bar").display = True

        table = self.query_one("#table", DataTable)
        table.cursor_type = "row"
        table.add_columns("Frame", "Timecode", "VMAF", "PSNR (dB)")

        self._populate_table()
        self._refresh_graphs()
        self._update_info_bar()

    # ── Data helpers ─────────────────────────────────────────────────────

    def _get_metric_value(self, f: MetricFrame) -> float | None:
        m = self.current_metric
        if m == "vmaf":
            return f.vmaf
        elif m == "psnr":
            return f.psnr
        return None

    def _get_all_values(self, frames: list[MetricFrame] | None = None) -> list[float]:
        frames = frames or self.display_frames
        return [v for f in frames if (v := self._get_metric_value(f)) is not None]

    # ── Table ────────────────────────────────────────────────────────────

    def _populate_table(self) -> None:
        table = self.query_one("#table", DataTable)
        table.clear()
        rows = []
        for f in self.display_frames:
            vmaf_str = f"{f.vmaf:.2f}" if f.vmaf is not None else "—"
            psnr_str = f"{f.psnr:.2f}" if f.psnr is not None else "—"
            rows.append((str(f.frame), f.timecode, vmaf_str, psnr_str))
        table.add_rows(rows)

    # ── Graphs ───────────────────────────────────────────────────────────

    def _refresh_graphs(self, cursor_frame: int | None = None) -> None:
        if not self.analysis:
            return

        all_values = self._get_all_values(self.analysis.frames)
        name = self.current_metric.upper()

        # Overview — always full range, shows zoom window indicator
        overview = self.query_one("#overview", MetricGraph)
        graph = self.query_one("#graph", MetricGraph)
        zoom_window = None
        if graph.zoom_end > 0:
            zoom_window = (graph.zoom_start, graph.zoom_end)
        overview.update_data(all_values, f"Overview ({name})", self.threshold,
                            zoom_window=zoom_window)
        if cursor_frame is not None:
            overview.cursor_frame = cursor_frame

        # Detail — respects zoom, auto-follows cursor

        if cursor_frame is not None and graph.zoom_end > 0:
            # Zoomed in — recenter if cursor is outside visible range
            zs, ze = graph._visible_range()
            if cursor_frame < zs or cursor_frame >= ze:
                span = ze - zs
                n = len(all_values)
                new_start = max(0, cursor_frame - span // 2)
                new_end = min(n, new_start + span)
                # Clamp start if we hit the end
                if new_end >= n:
                    new_start = max(0, n - span)
                graph.zoom_start = new_start
                graph.zoom_end = new_end

        graph.update_data(all_values, name, self.threshold)
        if cursor_frame is not None:
            graph.cursor_frame = cursor_frame

    # ── Info bar ─────────────────────────────────────────────────────────

    def _update_info_bar(self) -> None:
        if not self.analysis:
            return
        info = self.query_one("#info-bar", StatusBar)
        tc = self.analysis.tc_format
        tc_type = tc.tc_type.value if tc else "?"
        fps = f"{tc.fps_float:.3f}" if tc else "?"
        n = len(self.analysis.frames)
        showing = len(self.display_frames)

        metric = self.current_metric.upper()
        values = self._get_all_values()

        if values:
            avg = sum(values) / len(values)
            stats = f"avg={avg:.2f} min={min(values):.2f} max={max(values):.2f}"
        else:
            stats = "no data"

        parts = [
            f" {n:,} frames",
            f"{fps} fps ({tc_type})",
            f"sync={self.analysis.sync_mode}",
            f"{metric}: {stats}",
        ]
        if self.threshold is not None:
            parts.append(f"thresh={self.threshold:.2f}")
        if showing != n:
            parts.append(f"[{showing}/{n} shown]")

        graph = self.query_one("#graph", MetricGraph)
        if graph.zoom_end > 0:
            zs, ze = graph._visible_range()
            parts.append(f"zoom [{zs}:{ze}]")

        if self._mpv and self._mpv.is_running:
            seek_mode = "auto" if self.mpv_auto_seek else "manual"
            parts.append(f"mpv:{seek_mode}")

        info.update(" │ ".join(parts) + " ")

    # ── Watchers / Events ────────────────────────────────────────────────

    def watch_metric_index(self) -> None:
        if self.analysis:
            self._refresh_graphs()
            self._update_info_bar()

    def watch_threshold(self) -> None:
        if self.analysis:
            self._refresh_graphs()
            self._update_info_bar()

    @on(DataTable.RowHighlighted)
    def on_row_highlighted(self, event: DataTable.RowHighlighted) -> None:
        if not self.analysis or event.cursor_row is None:
            return
        row_idx = event.cursor_row
        if row_idx < 0 or row_idx >= len(self.display_frames):
            return
        try:
            frame_num = self.display_frames[row_idx].frame
            graph_idx = self._frame_to_idx(frame_num)  # 0-based array index
            self._refresh_graphs(cursor_frame=graph_idx)

            # Auto-seek mpv if enabled (skip during cooldown from poll,
            # and skip while mpv is playing — that causes jitter)
            if (
                self.mpv_auto_seek
                and time.monotonic() >= self._mpv_seek_until
                and self._mpv
                and self._mpv.is_running
                and self._mpv.is_paused() is not False
            ):
                self._mpv_seek_until = time.monotonic() + 0.75
                self._mpv.seek_to_frame(frame_num)

        except (ValueError, TypeError):
            pass

    @on(MetricGraph.FrameClicked)
    def on_graph_clicked(self, event: MetricGraph.FrameClicked) -> None:
        """Navigate table to the frame clicked on the graph."""
        if not self.display_frames:
            return
        target_idx = event.frame_index  # 0-based array index in analysis.frames

        # Find the closest row in display_frames (may be filtered subset)
        best_row = 0
        best_dist = abs(self._frame_to_idx(self.display_frames[0].frame) - target_idx)
        for i, f in enumerate(self.display_frames):
            d = abs(self._frame_to_idx(f.frame) - target_idx)
            if d < best_dist:
                best_dist = d
                best_row = i

        # Set cooldown BEFORE moving cursor to prevent poll from reverting
        self._mpv_seek_until = time.monotonic() + 1.0

        table = self.query_one("#table", DataTable)
        table.move_cursor(row=best_row)
        self._refresh_graphs(cursor_frame=target_idx)

        # Seek mpv directly (don't rely on on_row_highlighted chain)
        if (
            self.mpv_auto_seek
            and self._mpv
            and self._mpv.is_running
        ):
            frame_num = self.display_frames[best_row].frame
            self._mpv.seek_to_frame(frame_num)

    # ── Actions ──────────────────────────────────────────────────────────

    def action_cycle_metric(self) -> None:
        self.metric_index = (self.metric_index + 1) % len(METRICS)

    def action_set_threshold(self) -> None:
        if not self.analysis:
            return
        values = self._get_all_values(self.analysis.frames)
        if not values:
            return
        avg = sum(values) / len(values)
        std = (sum((v - avg) ** 2 for v in values) / len(values)) ** 0.5
        self.threshold = round(avg - std, 2)
        self.notify(f"Threshold: {self.threshold:.2f} (mean − 1σ)", timeout=2)

    def action_clear_threshold(self) -> None:
        self.threshold = None
        self.notify("Threshold cleared", timeout=2)

    def action_jump_worst(self) -> None:
        if not self.display_frames:
            return
        worst_i, worst_v = None, float("inf")
        for i, f in enumerate(self.display_frames):
            v = self._get_metric_value(f)
            if v is not None and v < worst_v:
                worst_v, worst_i = v, i
        if worst_i is not None:
            table = self.query_one("#table", DataTable)
            table.move_cursor(row=worst_i)
            tc = self.display_frames[worst_i].timecode
            self.notify(f"Worst: {tc} ({worst_v:.2f})", timeout=2)

    def action_jump_best(self) -> None:
        if not self.display_frames:
            return
        best_i, best_v = None, float("-inf")
        for i, f in enumerate(self.display_frames):
            v = self._get_metric_value(f)
            if v is not None and v > best_v:
                best_v, best_i = v, i
        if best_i is not None:
            table = self.query_one("#table", DataTable)
            table.move_cursor(row=best_i)
            tc = self.display_frames[best_i].timecode
            self.notify(f"Best: {tc} ({best_v:.2f})", timeout=2)

    def action_copy_timecode(self) -> None:
        if not self.display_frames:
            return
        table = self.query_one("#table", DataTable)
        row_idx = table.cursor_row
        if row_idx is not None and row_idx < len(self.display_frames):
            tc = self.display_frames[row_idx].timecode
            import base64
            encoded = base64.b64encode(tc.encode()).decode()
            sys.stdout.write(f"\033]52;c;{encoded}\a")
            sys.stdout.flush()
            self.notify(f"Copied: {tc}", timeout=2)

    def action_zoom_in(self) -> None:
        if not self.analysis:
            return
        n = len(self.analysis.frames)
        graph = self.query_one("#graph", MetricGraph)
        cs, ce = graph._visible_range()
        span = ce - cs
        new_span = max(span // 2, 50)

        center = graph.cursor_frame if graph.cursor_frame >= 0 else (cs + ce) // 2
        new_start = max(0, center - new_span // 2)
        new_end = min(n, new_start + new_span)
        graph.zoom_start = new_start
        graph.zoom_end = new_end
        self._refresh_graphs()
        self._update_info_bar()

    def action_zoom_out(self) -> None:
        if not self.analysis:
            return
        n = len(self.analysis.frames)
        graph = self.query_one("#graph", MetricGraph)
        cs, ce = graph._visible_range()
        span = ce - cs
        new_span = min(span * 2, n)

        center = (cs + ce) // 2
        new_start = max(0, center - new_span // 2)
        new_end = min(n, new_start + new_span)

        if new_start == 0 and new_end >= n:
            graph.zoom_start = 0
            graph.zoom_end = -1
        else:
            graph.zoom_start = new_start
            graph.zoom_end = new_end
        self._refresh_graphs()
        self._update_info_bar()

    def action_zoom_reset(self) -> None:
        graph = self.query_one("#graph", MetricGraph)
        graph.zoom_start = 0
        graph.zoom_end = -1
        self._refresh_graphs()
        self._update_info_bar()

    def action_filter_below(self) -> None:
        if not self.analysis or self.threshold is None:
            self.notify("Set a threshold first (t)", timeout=2)
            return
        thresh = self.threshold
        self._display_frames = [
            f for f in self.analysis.frames
            if (v := self._get_metric_value(f)) is not None and v < thresh
        ]
        self._populate_table()
        self._update_info_bar()
        self.notify(
            f"Showing {len(self._display_frames)} frames below {thresh:.2f}",
            timeout=2,
        )

    def action_filter_clear(self) -> None:
        self._display_frames = None
        self._populate_table()
        self._update_info_bar()
        self.notify("Filter cleared", timeout=2)

    def action_export_csv(self) -> None:
        if not self.analysis:
            return
        import csv
        out_path = f"vcompare_export_{self.current_metric}.csv"
        with open(out_path, "w", newline="") as f:
            writer = csv.writer(f)
            writer.writerow(["frame", "timecode", "vmaf", "psnr"])
            for fr in self.display_frames:
                writer.writerow([
                    fr.frame, fr.timecode,
                    f"{fr.vmaf:.4f}" if fr.vmaf is not None else "",
                    f"{fr.psnr:.4f}" if fr.psnr is not None else "",
                ])
        self.notify(f"Exported: {out_path}", timeout=3)

    def action_sort_toggle(self) -> None:
        if not self.analysis:
            return
        self.sorted_ascending = not self.sorted_ascending
        frames = self.display_frames
        frames.sort(
            key=lambda f: self._get_metric_value(f) or 0,
            reverse=not self.sorted_ascending,
        )
        if self._display_frames is not None:
            self._display_frames = frames
        else:
            self.analysis.frames = frames
        self._populate_table()
        arrow = "↑" if self.sorted_ascending else "↓"
        self.notify(f"Sorted {self.current_metric.upper()} {arrow}", timeout=2)

    # ── mpv actions ─────────────────────────────────────────────────────

    def action_mpv_launch(self) -> None:
        """Launch mpv with side-by-side view."""
        if not self.analysis or not self.analysis.tc_format:
            self.notify("Analysis not complete", timeout=2)
            return

        if self._mpv and self._mpv.is_running:
            self.notify("mpv already running (P to kill)", timeout=2)
            return

        self._mpv = MpvController(
            self.ref_path, self.dist_path, self.analysis.tc_format,
            ref_file_start_tc=self.analysis.ref_file_start_tc,
        )
        err = self._mpv.launch()
        if err:
            self.notify(f"[red]mpv error:[/] {err}", timeout=4)
            self._mpv = None
        else:
            self.mpv_auto_seek = True
            msg = "mpv launched — auto-seek on (a to toggle)"
            if self.offset_frames != 0:
                msg += f" [yellow](note: --offset {self.offset_frames:+d} not reflected in mpv)[/]"
            self.notify(msg, timeout=4 if self.offset_frames else 2)
            # Start polling mpv position
            self._mpv_poll_timer = self.set_interval(0.25, self._poll_mpv_position)
            self._update_info_bar()

    def _poll_mpv_position(self) -> None:
        """Poll mpv's current position and sync the table.

        When mpv is playing, always syncs. When paused, only syncs if
        mpv's position changed (i.e., user scrubbed in the mpv window).
        """
        if not self._mpv or not self._mpv.is_running:
            # mpv died externally — clean up
            if self._mpv_poll_timer:
                self._mpv_poll_timer.stop()
                self._mpv_poll_timer = None
            self._mpv = None
            self._update_info_bar()
            return

        frame = self._mpv.get_frame()  # absolute TC frame
        if frame is None or not self.display_frames:
            return

        # When paused, only sync if position actually changed (user scrubbed in mpv)
        paused = self._mpv.is_paused()
        if paused is not False:
            if frame == self._mpv_last_frame:
                return
        self._mpv_last_frame = frame

        # Skip if within cooldown window (TUI recently seeked mpv)
        if time.monotonic() < self._mpv_seek_until:
            return

        # Find closest row in display_frames (absolute frame comparison)
        best_row = 0
        best_dist = abs(self.display_frames[0].frame - frame)
        for i, f in enumerate(self.display_frames):
            d = abs(f.frame - frame)
            if d < best_dist:
                best_dist = d
                best_row = i

        table = self.query_one("#table", DataTable)
        if table.cursor_row != best_row:
            self._mpv_seek_until = time.monotonic() + 0.5
            table.move_cursor(row=best_row)
            self._refresh_graphs(cursor_frame=self._frame_to_idx(frame))

    def action_mpv_auto_toggle(self) -> None:
        """Toggle automatic mpv seeking on row change."""
        self.mpv_auto_seek = not self.mpv_auto_seek
        mode = "AUTO" if self.mpv_auto_seek else "MANUAL (s)"
        self.notify(f"mpv seek: {mode}", timeout=2)
        self._update_info_bar()

    def action_mpv_seek(self) -> None:
        """Seek mpv to the currently highlighted frame."""
        if not self._mpv or not self._mpv.is_running:
            self.notify("mpv not running (p to launch)", timeout=2)
            return
        if not self.display_frames:
            return

        table = self.query_one("#table", DataTable)
        row_idx = table.cursor_row
        if row_idx is not None and row_idx < len(self.display_frames):
            frame = self.display_frames[row_idx]
            self._mpv_seek_until = time.monotonic() + 0.75
            if self._mpv.seek_to_frame(frame.frame):
                self.notify(f"mpv → {frame.timecode}", timeout=1)
            else:
                self.notify("[red]Seek failed[/]", timeout=2)

    def action_mpv_step_fwd(self) -> None:
        """Step mpv one frame forward."""
        if self._mpv and self._mpv.is_running:
            self._mpv_seek_until = time.monotonic() + 0.5
            self._mpv.step_forward()

    def action_mpv_step_back(self) -> None:
        """Step mpv one frame backward."""
        if self._mpv and self._mpv.is_running:
            self._mpv_seek_until = time.monotonic() + 0.5
            self._mpv.step_backward()

    def action_mpv_pause(self) -> None:
        """Toggle mpv play/pause."""
        if self._mpv and self._mpv.is_running:
            self._mpv.toggle_pause()

    def action_mpv_kill(self) -> None:
        """Kill mpv."""
        if self._mpv_poll_timer:
            self._mpv_poll_timer.stop()
            self._mpv_poll_timer = None
        if self._mpv:
            self._mpv.kill()
            self._mpv = None
            self.notify("mpv closed", timeout=2)
            self._update_info_bar()
        else:
            self.notify("mpv not running", timeout=2)

    def action_quit(self) -> None:
        # Stop mpv polling
        if self._mpv_poll_timer:
            self._mpv_poll_timer.stop()
        # Kill mpv
        if self._mpv:
            self._mpv.kill()
        # Kill any running ffmpeg processes
        for proc in list(self._ffmpeg_processes):
            try:
                if proc.poll() is None:
                    proc.kill()
                    proc.wait(timeout=5)
            except (OSError, subprocess.TimeoutExpired):
                pass
        self._ffmpeg_processes.clear()
        # Clean up temp files
        try:
            os.unlink(self._log_path)
            os.rmdir(self._log_dir)
        except OSError:
            pass
        # Clean up IMF temp concat manifests
        for tmp in self._imf_temps:
            try:
                os.unlink(tmp)
                os.rmdir(os.path.dirname(tmp))
            except OSError:
                pass
        self.exit()


# ─── CLI ─────────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(
        description="Compare two video files using VMAF and optional PSNR metrics.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Keybindings:
  m           Cycle through VMAF / PSNR
  t           Set threshold (mean − 1σ)
  T           Clear threshold
  w / b       Jump to worst / best frame
  y           Copy timecode to clipboard (OSC 52)
  0 / 9       Zoom in / out on detail graph
  8           Reset zoom
  f           Filter table to frames below threshold
  F           Clear filter
  e           Export current view to CSV
  /           Toggle sort by current metric

  mpv preview:
  p           Launch mpv (side-by-side ref | distorted, auto-seek on)
  s           Seek mpv to current frame
  a           Toggle auto-seek (seek on every row change)
  . / ,       Step mpv forward / backward one frame
  space       Toggle mpv play / pause
  P           Kill mpv

  q           Quit

Analysis logs:
  Results are auto-saved as <ref>_vs_<dist>.vcompare.json next to the
  script. On re-run, the saved log is loaded instantly — no re-analysis
  needed. Use --log to override the path.

Sync modes (--sync):
  start       Align file starts (frame 0 = frame 0). Default.
  tc          Align by embedded timecodes. Trims the earlier file so
              only the overlapping TC range is compared.
        """,
    )
    parser.add_argument("reference", help="Reference (source) video file")
    parser.add_argument("distorted", help="Distorted (encoded) video file")
    parser.add_argument(
        "--model", default="vmaf_v0.6.1",
        help="VMAF model version (default: vmaf_v0.6.1)",
    )
    parser.add_argument(
        "--threads", type=int, default=0,
        help="Number of threads for CPU VMAF (0 = all cores, ignored on GPU path)",
    )
    parser.add_argument(
        "--subsample", type=int, default=1,
        help="Only compute every Nth frame (e.g. 4 = 4x faster, less granular)",
    )
    parser.add_argument(
        "--psnr", action="store_true",
        help="Also compute PSNR (off by default, forces CPU path)",
    )
    parser.add_argument(
        "--hwaccel", default="auto",
        choices=["auto", "cuda", "vaapi", "videotoolbox", "none"],
        help="Hardware decoder: auto (default), cuda, vaapi, videotoolbox, none",
    )
    parser.add_argument(
        "--sync", default="start", choices=["start", "tc"],
        help="Sync mode: 'start' = align file starts (default), "
             "'tc' = align by embedded timecodes",
    )
    parser.add_argument(
        "--offset", type=int, default=0, metavar="N",
        help="Manual frame offset: +N = skip N frames of distorted, "
             "-N = skip N frames of reference (applied after TC sync)",
    )
    parser.add_argument(
        "--log", default=None, metavar="PATH",
        help="Analysis log path. If file exists, load it (skip analysis). "
             "If not, run analysis and save here. "
             "Default: auto-named next to reference file.",
    )

    args = parser.parse_args()

    for path in (args.reference, args.distorted):
        if not os.path.isfile(path):
            print(f"Error: file not found: {path}", file=sys.stderr)
            sys.exit(1)

    # Determine log path
    if args.log:
        log_path = args.log
    else:
        # Auto-name: <reference_stem>_vs_<distorted_stem>.vcompare.json
        ref_stem = Path(args.reference).stem
        dist_stem = Path(args.distorted).stem
        log_dir = Path(__file__).resolve().parent
        log_path = str(log_dir / f"{ref_stem}_vs_{dist_stem}.vcompare.json")

    # Check if we can load existing analysis
    preloaded = None
    if os.path.isfile(log_path):
        try:
            preloaded = load_analysis(log_path)

            # Invalidate cache if settings changed
            cache_ok = True
            if preloaded.sync_mode != args.sync:
                print(
                    f"[vcompare] Cache sync mode ({preloaded.sync_mode}) != "
                    f"requested ({args.sync}), re-analyzing.",
                    file=sys.stderr,
                )
                cache_ok = False

            if cache_ok:
                print(
                    f"[vcompare] Loaded saved analysis: {log_path} "
                    f"({len(preloaded.frames):,} frames)",
                    file=sys.stderr,
                )
            else:
                preloaded = None

        except Exception as e:
            print(
                f"[vcompare] Warning: could not load {log_path}: {e}",
                file=sys.stderr,
            )
            print("[vcompare] Will re-analyze.", file=sys.stderr)

    # Only need ffmpeg/ffprobe if we're actually analyzing
    if not preloaded:
        for tool in ("ffmpeg", "ffprobe"):
            try:
                subprocess.run([tool, "-version"], capture_output=True, check=True)
            except FileNotFoundError:
                print(f"Error: {tool} not found in PATH", file=sys.stderr)
                sys.exit(1)

    if args.hwaccel == "none":
        hwaccel = None
    elif args.hwaccel == "auto":
        hwaccel = detect_hwaccel()
        if hwaccel:
            print(f"[vcompare] Auto-detected hwaccel: {hwaccel}", file=sys.stderr)
        else:
            print("[vcompare] No hardware decoder found, using software decode", file=sys.stderr)
    else:
        hwaccel = args.hwaccel

    # Auto-detect libvmaf_cuda when using CUDA hwaccel
    vmaf_cuda = False
    if hwaccel == "cuda" and not preloaded:
        vmaf_cuda = detect_vmaf_cuda()
        if vmaf_cuda:
            print("[vcompare] libvmaf_cuda filter available (GPU VMAF)", file=sys.stderr)

    # Build optional metric features
    extra_features = []
    if args.psnr:
        extra_features.append("psnr")

    app = VCompareTUI(
        ref_path=args.reference,
        dist_path=args.distorted,
        vmaf_model=args.model,
        threads=args.threads,
        subsample=args.subsample,
        hwaccel=hwaccel,
        sync_mode=args.sync,
        vmaf_cuda=vmaf_cuda,
        preloaded=preloaded,
        save_path=log_path if not preloaded else None,
        extra_features=extra_features,
        offset_frames=args.offset,
    )

    # Safety net: kill child processes on abnormal exit (Ctrl+C, crash)
    def _cleanup_children():
        for proc in list(app._ffmpeg_processes):
            try:
                if proc.poll() is None:
                    proc.kill()
                    proc.wait(timeout=2)
            except Exception:
                pass
        if app._mpv:
            try:
                app._mpv.kill()
            except Exception:
                pass

    atexit.register(_cleanup_children)

    def _signal_handler(signum, frame):
        _cleanup_children()
        sys.exit(128 + signum)

    signal.signal(signal.SIGTERM, _signal_handler)
    # SIGINT is handled by Textual's event loop, but register as fallback
    prev_sigint = signal.getsignal(signal.SIGINT)
    def _sigint_handler(signum, frame):
        _cleanup_children()
        if callable(prev_sigint) and prev_sigint not in (signal.SIG_DFL, signal.SIG_IGN):
            prev_sigint(signum, frame)
        else:
            sys.exit(128 + signum)
    signal.signal(signal.SIGINT, _sigint_handler)

    app.run()


if __name__ == "__main__":
    main()
