#!/usr/bin/env python3
"""
vcompare_engine — JSON-RPC method handlers wrapping the analysis engine.

Supports two analysis modes:
  - "diff" (default): fast pixel difference via psnr filter
  - "vmaf": perceptual quality via libvmaf (+ optional PSNR)
"""

from __future__ import annotations

import atexit
import json
import math
import os
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from rpc_server import JsonRpcServer, async_method, log

# ── Import from vcompare.py ──

try:
    from vcompare import (
        AnalysisResult,
        ComparisonConfig,
        MetricFrame,
        MpvController,
        TimecodeFormat,
        TCType,
        VideoInfo,
        choose_common_pixfmt,
        detect_hwaccel,
        detect_timecode_format,
        detect_vmaf_cuda,
        ffprobe_json,
        get_video_info,
        load_analysis,
        parse_vmaf_log,
        prepare_comparison,
        run_comparison,
        save_analysis,
    )
    # Also try to import _is_concat_manifest (private but we need it)
    try:
        from vcompare import _is_concat_manifest
    except ImportError:
        def _is_concat_manifest(path):
            try:
                with open(path) as f:
                    first = f.readline().strip()
                return first == "ffconcat version 1.0"
            except Exception:
                return False

    # Import pixel format parsing for diff-specific negotiation
    try:
        from vcompare import _parse_pix_fmt, _SUBSAMP_RANK
    except ImportError:
        _SUBSAMP_RANK = {"420": 0, "422": 1, "444": 2}
        def _parse_pix_fmt(fmt):
            f = fmt.lower().strip()
            is_rgb = False
            if f.startswith(("rgb", "bgr", "gbr", "gbrp", "rgba", "bgra")):
                is_rgb = True
                if "48" in f or "16" in f: return ("444", 16, True)
                elif "10" in f: return ("444", 10, True)
                elif "12" in f: return ("444", 12, True)
                return ("444", 8, True)
            bits = 8
            for b in (16, 12, 10):
                if f"p{b}" in f or f"{b}le" in f or f"{b}be" in f:
                    bits = b
                    break
            if "444" in f: sub = "444"
            elif "422" in f: sub = "422"
            elif "nv12" in f or "nv21" in f or "420" in f: sub = "420"
            else: sub = "420"
            return (sub, bits, is_rgb)

    log("Imported vcompare engine successfully")
except ImportError as e:
    log(f"WARNING: Could not import vcompare: {e}")
    log("Running in stub mode — analysis will not work")
    AnalysisResult = None


# ─── Server + State ──────────────────────────────────────────────────────────

server = JsonRpcServer()

_state = {
    # Unified frame storage (works for both diff and vmaf modes)
    "frames": [],            # list of dicts: {frame, tc, diff_y, diff_u, diff_v, ...}
    "metrics_available": [],  # ["diff_y", "diff_u", "diff_v"] or ["vmaf", "psnr"]
    "analysis_mode": None,   # "diff" or "vmaf"
    "meta": {},              # tc_format, paths, sync_mode, bit_depth, etc.

    # Legacy: AnalysisResult for vmaf mode (needed for mpv controller)
    "analysis": None,

    # mpv
    "mpv": None,
    "mpv_poll_active": False,

    # Temp files
    "log_dir": None,
    "log_path": None,
    "stats_path": None,
    "ffmpeg_procs": [],
    "save_path": None,
}


# ─── Capability / Probe Methods ──────────────────────────────────────────────

@server.method("ping")
def handle_ping(params):
    return {"status": "ok", "pid": os.getpid()}


@server.method("detect_capabilities")
def handle_detect_capabilities(params):
    hwaccel = None
    cuda_vmaf = False
    try:
        hwaccel = detect_hwaccel()
    except Exception:
        pass
    try:
        cuda_vmaf = detect_vmaf_cuda()
    except Exception:
        pass
    return {
        "hwaccel": hwaccel,
        "vmaf_cuda": cuda_vmaf,
    }


@server.method("probe_file")
def handle_probe_file(params):
    path = params.get("path") or params.get("p")
    if not path:
        raise ValueError("Missing 'path' parameter")
    if not os.path.isfile(path):
        raise FileNotFoundError(f"File not found: {path}")

    probe = ffprobe_json(path)
    video = None
    for s in probe.get("streams", []):
        if s.get("codec_type") == "video":
            video = s
            break
    if not video:
        raise RuntimeError(f"No video stream in {path}")

    r_fr = video.get("r_frame_rate", "24/1")
    num, den = (int(x) for x in r_fr.split("/"))
    nb_frames = int(video.get("nb_frames", 0)) or None
    duration = float(probe.get("format", {}).get("duration", 0))

    tc = None
    for source in (video.get("tags", {}), probe.get("format", {}).get("tags", {})):
        tc = source.get("timecode")
        if tc:
            break

    return {
        "path": path,
        "filename": Path(path).name,
        "width": int(video.get("width", 0)),
        "height": int(video.get("height", 0)),
        "pix_fmt": video.get("pix_fmt", "unknown"),
        "codec": video.get("codec_name", "unknown"),
        "fps_num": num,
        "fps_den": den,
        "fps": round(num / den, 3),
        "nb_frames": nb_frames,
        "duration": round(duration, 3),
        "timecode": tc,
    }


# ─── Unified Analysis Handler ────────────────────────────────────────────────

@server.method("analyze")
@async_method
def handle_analyze(params):
    """
    Run analysis. Routes to diff or vmaf mode.

    Params:
        reference: str
        distorted: str
        mode: str — "diff" (default) or "vmaf"
        sync: str — "start"|"tc"
        offset: int — manual frame offset
        subsample: int — subsample factor (vmaf only)
        hwaccel: str — "auto"|"cuda"|"vaapi"|"none" (vmaf only)
        psnr: bool — also compute PSNR (vmaf only)
        model: str — VMAF model (vmaf only)
        log_path: str|null — path to save/load analysis JSON
    """
    mode = params.get("mode", "diff")

    if mode == "vmaf":
        return _run_vmaf_analysis(params)
    else:
        return _run_diff_analysis(params)


# ─── Diff-specific pixel format negotiation ──────────────────────────────────

def choose_diff_pixfmt(ref_fmt: str, dist_fmt: str) -> str:
    """Choose a common pixel format for pixel-difference comparison.

    Strategy: use the HIGHER subsampling (always 444) and HIGHER bit depth.
    For diff analysis, we want to preserve all information from both streams
    so the diff only reflects actual content changes, not conversion artifacts.

    Upconversion is cheap and deterministic:
      - Chroma 420→444 or 422→444: interpolation (sub-pixel, deterministic)
      - Bit depth 8→10 or 10→12: left-shift (lossless)
      - RGB→YUV: matrix conversion (~0.5 LSB rounding at 12-bit, negligible)

    Downconversion would destroy information asymmetrically, creating false
    positives in the diff.

    Returns:
        target_fmt: str — YUV planar format name (e.g. "yuv444p12le")
    """
    ref_sub, ref_bits, ref_rgb = _parse_pix_fmt(ref_fmt)
    dist_sub, dist_bits, dist_rgb = _parse_pix_fmt(dist_fmt)

    # If identical and already YUV, use as-is
    if ref_fmt == dist_fmt and not ref_rgb:
        return ref_fmt

    # For diff: always go to highest subsampling and highest bit depth.
    # If either stream is RGB or they differ in subsampling, force 444
    # to avoid any chroma information loss.
    if ref_rgb or dist_rgb or ref_sub != dist_sub:
        target_sub = "444"
    else:
        # Both YUV, same subsampling — keep it
        target_sub = ref_sub

    target_bits = max(ref_bits, dist_bits)

    if target_bits == 8:
        return f"yuv{target_sub}p"
    else:
        return f"yuv{target_sub}p{target_bits}le"


# ─── Diff Analysis ───────────────────────────────────────────────────────────

def _run_diff_analysis(params):
    """Fast pixel difference analysis using FFmpeg's native psnr filter.

    Uses the built-in psnr filter with stats_file output — SIMD-optimized,
    one compact line per frame. From MSE we derive RMS pixel difference
    in code values (0–1023 for 10-bit).
    """
    ref_path = params["reference"]
    dist_path = params["distorted"]
    sync_mode = params.get("sync", "start")
    offset = params.get("offset", 0)
    save_path = params.get("log_path")
    force = params.get("force", False)

    # Check cache
    if save_path and not force and os.path.isfile(save_path):
        try:
            cached = _load_diff_cache(save_path)
            if cached and cached.get("sync_mode") == sync_mode:
                log(f"Loaded cached diff analysis: {save_path}")
                _apply_diff_result(cached)
                _state["analysis"] = _build_analysis_result_for_mpv(cached)
                _state["save_path"] = save_path
                return _build_summary()
        except Exception as e:
            log(f"Cache load failed: {e}, re-analyzing")

    diagnostics: list[str] = []
    diagnostics.append("mode: pixel difference (psnr filter)")

    config, tc_fmt = prepare_comparison(
        ref_path, dist_path, sync_mode,
        vmaf_cuda=False,
        offset_frames=offset,
        diagnostics=diagnostics,
    )

    # Diff-specific format negotiation: upconvert to highest quality
    # to avoid false positives from format conversion
    target_fmt = choose_diff_pixfmt(
        config.ref_src_pixfmt, config.dist_src_pixfmt
    )
    if config.ref_src_pixfmt != config.dist_src_pixfmt:
        diagnostics.append(
            f"pixfmt: {config.ref_src_pixfmt} vs {config.dist_src_pixfmt} → {target_fmt}"
        )
    else:
        diagnostics.append(f"pixfmt: {config.ref_src_pixfmt} → {target_fmt}")

    # Detect bit depth for interpreting results
    bit_depth = 8
    if "10" in target_fmt:
        bit_depth = 10
    elif "12" in target_fmt:
        bit_depth = 12
    elif "16" in target_fmt:
        bit_depth = 16
    peak = (2 ** bit_depth) - 1
    diagnostics.append(f"bit depth: {bit_depth} (peak {peak})")

    # Stats output file
    stats_dir = tempfile.mkdtemp(prefix="vcompare_diff_")
    stats_path = os.path.join(stats_dir, "psnr_stats.txt")
    _state["log_dir"] = stats_dir
    _state["stats_path"] = stats_path

    server.notify("diagnostics", {"lines": diagnostics})

    # Build FFmpeg command
    def _input_args(path, seek_sec):
        args = []
        if seek_sec > 0:
            args.extend(["-ss", f"{seek_sec:.6f}"])
        if _is_concat_manifest(path):
            args.extend(["-f", "concat", "-safe", "0"])
        args.extend(["-i", path])
        return args

    input_args = []
    input_args.extend(_input_args(ref_path, config.ref_seek_sec))
    input_args.extend(_input_args(dist_path, config.dist_seek_sec))

    # Filter chain: explicit format on both → psnr with stats_file
    ref_chain = f"format={target_fmt}"
    dist_chain = f"format={target_fmt}"

    if config.scale_w > 0:
        dist_chain += f",scale={config.scale_w}:{config.scale_h}:flags=lanczos"

    filter_complex = (
        f"[0:v]{ref_chain}[ref];"
        f"[1:v]{dist_chain}[dist];"
        f"[ref][dist]psnr=stats_file={stats_path}"
    )

    cmd = [
        "ffmpeg", "-y",
        *input_args,
        "-filter_complex", filter_complex,
        "-f", "null", "-",
    ]

    print(f"[vcompare] cmd: {' '.join(cmd)}", file=sys.stderr)

    def on_progress(current, total):
        server.notify("progress", {"current": current, "total": total})

    process = subprocess.Popen(
        cmd,
        stderr=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        text=True,
        bufsize=1,
    )
    _state["ffmpeg_procs"].append(process)

    frame_re = re.compile(r"frame=\s*(\d+)")
    last_reported = -1
    last_progress_time = 0.0
    stderr_tail: deque[str] = deque(maxlen=20)

    try:
        for line in process.stderr:
            stderr_tail.append(line)
            m = frame_re.search(line)
            if m and config.total_frames:
                current = int(m.group(1))
                if current != last_reported:
                    last_reported = current
                    now = time.monotonic()
                    if now - last_progress_time >= 0.25:
                        last_progress_time = now
                        on_progress(current, config.total_frames)

        process.wait()
        if last_reported > 0 and config.total_frames:
            on_progress(config.total_frames, config.total_frames)

    except Exception:
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
        if process.stderr and not process.stderr.closed:
            process.stderr.close()
        if process in _state["ffmpeg_procs"]:
            _state["ffmpeg_procs"].remove(process)

    if process.returncode != 0:
        tail = "".join(stderr_tail).strip()
        raise RuntimeError(f"FFmpeg exited with code {process.returncode}\n\n{tail}")

    # Parse stats
    frames = _parse_psnr_stats(stats_path, tc_fmt, config.ref_start_frame, peak)
    log(f"Parsed {len(frames)} diff frames")

    result = {
        "mode": "diff",
        "frames": frames,
        "ref_path": ref_path,
        "dist_path": dist_path,
        "ref_start_frame": config.ref_start_frame,
        "ref_file_start_tc": config.ref_file_start_tc,
        "sync_mode": sync_mode,
        "bit_depth": bit_depth,
        "tc_format": {
            "fps_num": tc_fmt.fps_num,
            "fps_den": tc_fmt.fps_den,
            "tc_type": tc_fmt.tc_type.value,
        } if tc_fmt else None,
    }

    _apply_diff_result(result)
    _state["analysis"] = _build_analysis_result_for_mpv(result)

    if save_path:
        with open(save_path, "w") as f:
            json.dump(result, f)
        log(f"Saved diff analysis to {save_path}")
        _state["save_path"] = save_path

    return _build_summary()


def _parse_psnr_stats(
    stats_path: str,
    tc_fmt: TimecodeFormat,
    ref_start_frame: int,
    peak: int,
) -> list[dict]:
    """Parse psnr filter stats_file output into frame dicts.

    Format per line:
        n:1 mse_avg:0.00 mse_y:0.00 mse_u:0.00 mse_v:0.00 psnr_avg:inf ...

    We extract mse_y/u/v and convert to RMS diff in code values.
    """
    frames = []
    # Regex to extract key:value pairs
    kv_re = re.compile(r"(\w+):([\d.inf]+)")

    with open(stats_path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue

            pairs = dict(kv_re.findall(line))
            if "n" not in pairs:
                continue

            fn = int(pairs["n"]) - 1  # psnr filter is 1-indexed
            display_frame = fn + ref_start_frame

            # Extract MSE values — convert to RMS in code values
            mse_y = float(pairs.get("mse_y", "0"))
            mse_u = float(pairs.get("mse_u", "0"))
            mse_v = float(pairs.get("mse_v", "0"))

            # RMS diff in code values (e.g. 0–1023 for 10-bit)
            rms_y = math.sqrt(mse_y) if mse_y > 0 else 0.0
            rms_u = math.sqrt(mse_u) if mse_u > 0 else 0.0
            rms_v = math.sqrt(mse_v) if mse_v > 0 else 0.0

            frames.append({
                "frame": display_frame,
                "tc": tc_fmt.frame_to_tc(display_frame) if tc_fmt else f"F{display_frame}",
                "diff_y": round(rms_y, 4),
                "diff_u": round(rms_u, 4),
                "diff_v": round(rms_v, 4),
            })

    return frames


def _build_analysis_result_for_mpv(data: dict) -> AnalysisResult:
    """Build a minimal AnalysisResult from a diff cache dict (for mpv)."""
    tc_data = data.get("tc_format")
    tc_fmt = None
    if tc_data:
        tc_fmt = TimecodeFormat(
            fps_num=tc_data["fps_num"],
            fps_den=tc_data["fps_den"],
            tc_type=TCType(tc_data["tc_type"]),
        )
    return AnalysisResult(
        frames=[],
        tc_format=tc_fmt,
        ref_path=data.get("ref_path", ""),
        dist_path=data.get("dist_path", ""),
        ref_start_frame=data.get("ref_start_frame", 0),
        ref_file_start_tc=data.get("ref_file_start_tc", 0),
        sync_mode=data.get("sync_mode", "start"),
    )


def _apply_diff_result(result: dict):
    """Store a diff result in the unified state."""
    _state["frames"] = result["frames"]
    _state["metrics_available"] = ["diff_y", "diff_u", "diff_v"]
    _state["analysis_mode"] = "diff"
    _state["meta"] = {
        "ref_path": result.get("ref_path", ""),
        "dist_path": result.get("dist_path", ""),
        "ref_start_frame": result.get("ref_start_frame", 0),
        "ref_file_start_tc": result.get("ref_file_start_tc", 0),
        "sync_mode": result.get("sync_mode", "start"),
        "bit_depth": result.get("bit_depth", 8),
        "tc_format": result.get("tc_format"),
    }


def _load_diff_cache(path: str) -> dict | None:
    """Try to load a cached diff analysis."""
    with open(path) as f:
        data = json.load(f)
    if data.get("mode") != "diff":
        return None
    return data


# ─── VMAF Analysis ───────────────────────────────────────────────────────────

def _run_vmaf_analysis(params):
    """Perceptual quality analysis via libvmaf. Legacy path."""
    ref_path = params["reference"]
    dist_path = params["distorted"]
    model = params.get("model", "vmaf_v0.6.1")
    hwaccel_pref = params.get("hwaccel", "auto")
    sync_mode = params.get("sync", "start")
    offset = params.get("offset", 0)
    subsample = params.get("subsample", 1)
    want_psnr = params.get("psnr", False)
    save_path = params.get("log_path")
    force = params.get("force", False)

    # Resolve hwaccel
    if hwaccel_pref == "auto":
        hwaccel = detect_hwaccel()
    elif hwaccel_pref == "none":
        hwaccel = None
    else:
        hwaccel = hwaccel_pref

    vmaf_cuda = detect_vmaf_cuda() if hwaccel == "cuda" else False
    extra_features = ["psnr"] if want_psnr else []

    # Check cache
    if save_path and not force and os.path.isfile(save_path):
        try:
            result = load_analysis(save_path)
            if result.sync_mode == sync_mode:
                log(f"Loaded cached vmaf analysis: {save_path}")
                _apply_vmaf_result(result)
                _state["analysis"] = result
                _state["save_path"] = save_path
                return _build_summary()
        except Exception as e:
            log(f"Cache load failed: {e}, re-analyzing")

    # Set up temp log
    log_dir = tempfile.mkdtemp(prefix="vcompare_")
    log_path = os.path.join(log_dir, "vmaf_log.json")
    _state["log_dir"] = log_dir
    _state["log_path"] = log_path

    diagnostics: list[str] = []
    diagnostics.append("mode: vmaf")
    config, tc_fmt = prepare_comparison(
        ref_path, dist_path, sync_mode,
        vmaf_cuda=vmaf_cuda,
        offset_frames=offset,
        diagnostics=diagnostics,
    )

    server.notify("diagnostics", {"lines": diagnostics})

    def on_progress(current, total):
        server.notify("progress", {"current": current, "total": total})

    run_comparison(
        ref_path, dist_path, log_path,
        vmaf_model=model,
        threads=0,
        subsample=subsample,
        hwaccel=hwaccel,
        config=config,
        progress_callback=on_progress,
        diagnostics=diagnostics,
        process_holder=_state["ffmpeg_procs"],
        extra_features=extra_features,
    )

    frames = parse_vmaf_log(
        log_path, tc_fmt,
        ref_start_frame=config.ref_start_frame,
        subsample=config.log_subsample,
    )

    result = AnalysisResult(
        frames=frames,
        tc_format=tc_fmt,
        ref_path=ref_path,
        dist_path=dist_path,
        ref_start_frame=config.ref_start_frame,
        ref_file_start_tc=config.ref_file_start_tc,
        sync_mode=sync_mode,
    )

    if save_path:
        save_analysis(result, save_path)
        log(f"Saved vmaf analysis to {save_path}")

    _state["analysis"] = result
    _state["save_path"] = save_path

    _apply_vmaf_result(result)

    return _build_summary()


def _apply_vmaf_result(result: AnalysisResult):
    """Convert AnalysisResult into unified frame storage."""
    has_vmaf = any(f.vmaf is not None for f in result.frames)
    has_psnr = any(f.psnr is not None for f in result.frames)

    metrics = []
    if has_vmaf:
        metrics.append("vmaf")
    if has_psnr:
        metrics.append("psnr")

    _state["frames"] = [
        {
            "frame": f.frame,
            "tc": f.timecode,
            "vmaf": round(f.vmaf, 3) if f.vmaf is not None else None,
            "psnr": round(f.psnr, 3) if f.psnr is not None else None,
        }
        for f in result.frames
    ]
    _state["metrics_available"] = metrics
    _state["analysis_mode"] = "vmaf"

    tc = result.tc_format
    _state["meta"] = {
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
    }


# ─── Summary ─────────────────────────────────────────────────────────────────

def _build_summary() -> dict:
    """Build the analysis summary from unified state."""
    frames = _state["frames"]
    metrics = _state["metrics_available"]
    meta = _state["meta"]
    mode = _state["analysis_mode"]

    summary = {
        "mode": mode,
        "frame_count": len(frames),
        "metrics_available": metrics,
        "tc_format": meta.get("tc_format"),
        "ref_start_frame": meta.get("ref_start_frame", 0),
        "sync_mode": meta.get("sync_mode", "start"),
        "bit_depth": meta.get("bit_depth"),
    }

    # Per-metric stats
    for m in metrics:
        vals = [f[m] for f in frames if f.get(m) is not None]
        if vals:
            mean = sum(vals) / len(vals)
            mn = min(vals)
            mx = max(vals)
            # Stddev
            if len(vals) >= 2:
                variance = sum((x - mean) ** 2 for x in vals) / (len(vals) - 1)
                stddev = variance ** 0.5
            else:
                stddev = 0.0

            summary[f"{m}_stats"] = {
                "mean": round(mean, 4),
                "min": round(mn, 4),
                "max": round(mx, 4),
                "stddev": round(stddev, 4),
            }

            # For diff: count frames above various thresholds
            if mode == "diff" and m == "diff_y":
                summary["diff_distribution"] = {
                    "zero": sum(1 for v in vals if v == 0),
                    "below_1": sum(1 for v in vals if 0 < v <= 1),
                    "below_5": sum(1 for v in vals if 1 < v <= 5),
                    "below_10": sum(1 for v in vals if 5 < v <= 10),
                    "above_10": sum(1 for v in vals if v > 10),
                }

    return summary


# ─── Cache Check / Load ──────────────────────────────────────────────────────

@server.method("check_cache")
def handle_check_cache(params):
    ref = params["reference"]
    dist = params["distorted"]
    path = params.get("log_path") or _default_log_path(ref, dist)

    if os.path.isfile(path):
        try:
            with open(path) as f:
                data = json.load(f)
            mode = data.get("mode", "vmaf")
            if mode == "diff":
                fc = len(data.get("frames", []))
            else:
                fc = len(data.get("frames", []))
            return {
                "exists": True,
                "path": path,
                "frame_count": fc,
                "mode": mode,
                "sync_mode": data.get("sync_mode"),
            }
        except Exception:
            return {"exists": False, "path": path, "frame_count": None}

    return {"exists": False, "path": path, "frame_count": None}


@server.method("load_saved_analysis")
def handle_load_saved_analysis(params):
    path = params["path"]
    if not os.path.isfile(path):
        raise FileNotFoundError(f"File not found: {path}")

    with open(path) as f:
        data = json.load(f)

    mode = data.get("mode", "vmaf")

    if mode == "diff":
        _apply_diff_result(data)
        _state["analysis"] = _build_analysis_result_for_mpv(data)
    else:
        result = load_analysis(path)
        _apply_vmaf_result(result)
        _state["analysis"] = result

    _state["save_path"] = path
    log(f"Loaded saved {mode} analysis: {path}")
    return _build_summary()


def _default_log_path(ref: str, dist: str) -> str:
    ref_stem = Path(ref).stem
    dist_stem = Path(dist).stem
    ref_dir = Path(ref).parent
    return str(ref_dir / f"{ref_stem}_vs_{dist_stem}.vcompare.json")


# ─── Frame Data ──────────────────────────────────────────────────────────────

@server.method("get_frames")
def handle_get_frames(params):
    """
    Fetch frame data with optional filtering, sorting, and pagination.
    Works with any analysis mode — returns whatever metrics are available.

    Params:
        offset: int (default 0)
        limit: int (default 0 = all)
        sort: str|null — any metric name or "frame"
        ascending: bool (default true)
        filter_metric: str|null
        filter_below: float|null — frames below this value (vmaf/psnr)
        filter_above: float|null — frames above this value (diff)
    """
    frames = _state.get("frames", [])
    if not frames:
        raise RuntimeError("No analysis loaded")

    frames = list(frames)  # copy

    # Filter
    metric = params.get("filter_metric")
    below = params.get("filter_below")
    above = params.get("filter_above")
    if metric:
        if below is not None:
            frames = [f for f in frames if f.get(metric) is not None and f[metric] < below]
        if above is not None:
            frames = [f for f in frames if f.get(metric) is not None and f[metric] > above]

    total_matching = len(frames)

    # Sort
    sort_key = params.get("sort")
    ascending = params.get("ascending", True)
    if sort_key == "frame":
        frames.sort(key=lambda f: f["frame"], reverse=not ascending)
    elif sort_key and sort_key in _state.get("metrics_available", []):
        frames.sort(
            key=lambda f: f.get(sort_key) if f.get(sort_key) is not None else 0,
            reverse=not ascending,
        )

    # Paginate
    offset = params.get("offset", 0)
    limit = params.get("limit", 0)
    if limit > 0:
        frames = frames[offset:offset + limit]
    elif offset > 0:
        frames = frames[offset:]

    return {
        "total_matching": total_matching,
        "offset": offset,
        "count": len(frames),
        "frames": frames,
    }


@server.method("get_metric_series")
def handle_get_metric_series(params):
    """
    Fetch a single metric as flat arrays (optimized for chart rendering).

    Params:
        metric: str — any metric name ("diff_y", "diff_u", "diff_v", "vmaf", "psnr")

    Returns:
        {frames: [int,...], values: [float,...]}
    """
    frames = _state.get("frames", [])
    if not frames:
        raise RuntimeError("No analysis loaded")

    metric = params.get("metric", "diff_y")
    frame_nums = []
    values = []

    for f in frames:
        val = f.get(metric)
        if val is not None:
            frame_nums.append(f["frame"])
            values.append(round(val, 4))

    return {"frames": frame_nums, "values": values}


# ─── mpv Control ─────────────────────────────────────────────────────────────

def _get_mpv() -> MpvController:
    mpv = _state.get("mpv")
    if not mpv:
        raise RuntimeError("mpv not running")
    if not mpv.is_running:
        _state["mpv_poll_active"] = False
        _state["mpv"] = None
        server.notify("mpv.exited", {"reason": "closed"})
        raise RuntimeError("mpv not running (closed externally)")
    return mpv


@server.method("mpv.launch")
def handle_mpv_launch(params):
    result = _state.get("analysis")
    if not result:
        raise RuntimeError("No analysis loaded")

    if _state.get("mpv") and _state["mpv"].is_running:
        return {"status": "already_running"}

    mpv = MpvController(
        ref_path=result.ref_path,
        dist_path=result.dist_path,
        tc_format=result.tc_format,
        ref_file_start_tc=result.ref_file_start_tc,
    )
    err = mpv.launch()
    if err:
        raise RuntimeError(err)

    _state["mpv"] = mpv
    _state["mpv_poll_active"] = True
    t = threading.Thread(target=_mpv_poll_loop, daemon=True)
    t.start()

    return {"status": "launched"}


@server.method("mpv.seek")
def handle_mpv_seek(params):
    _state["mpv_seek_gen"] = _state.get("mpv_seek_gen", 0) + 1
    return {"ok": _get_mpv().seek_to_frame(params["frame"])}


@server.method("mpv.step")
def handle_mpv_step(params):
    mpv = _get_mpv()
    if params.get("direction") == "backward":
        return {"ok": mpv.step_backward()}
    return {"ok": mpv.step_forward()}


@server.method("mpv.toggle_pause")
def handle_mpv_toggle_pause(params):
    return {"ok": _get_mpv().toggle_pause()}


@server.method("mpv.get_position")
def handle_mpv_get_position(params):
    return {"frame": _get_mpv().get_frame()}


@server.method("mpv.kill")
def handle_mpv_kill(params):
    mpv = _state.get("mpv")
    _state["mpv_poll_active"] = False
    if mpv:
        mpv.kill()
        _state["mpv"] = None
    return {"status": "killed"}


def _mpv_poll_loop():
    last_frame = None
    poll_count = 0
    while _state.get("mpv_poll_active"):
        mpv = _state.get("mpv")
        if not mpv:
            break
        if not mpv.is_running:
            log("mpv process exited (closed externally)")
            _state["mpv_poll_active"] = False
            _state["mpv"] = None
            server.notify("mpv.exited", {"reason": "closed"})
            try:
                mpv.kill()
            except Exception:
                pass
            break
        frame = mpv.get_frame()
        poll_count += 1
        if frame is not None and frame != last_frame:
            last_frame = frame
            server.notify("mpv.position", {"frame": frame})
        time.sleep(0.1)


# ─── CSV Export ──────────────────────────────────────────────────────────────

@server.method("export_csv")
def handle_export_csv(params):
    frames = _state.get("frames", [])
    if not frames:
        raise RuntimeError("No analysis loaded")

    metrics = _state.get("metrics_available", [])
    meta = _state.get("meta", {})

    path = params.get("path")
    if not path:
        ref_stem = Path(meta.get("ref_path", "ref")).stem
        dist_stem = Path(meta.get("dist_path", "dist")).stem
        ref_dir = Path(meta.get("ref_path", ".")).parent
        path = str(ref_dir / f"{ref_stem}_vs_{dist_stem}.csv")

    with open(path, "w") as out:
        # Header
        cols = ["frame", "timecode"] + metrics
        out.write(",".join(cols) + "\n")

        for f in frames:
            row = [str(f["frame"]), f.get("tc", "")]
            for m in metrics:
                val = f.get(m)
                row.append(f"{val:.4f}" if val is not None else "")
            out.write(",".join(row) + "\n")

    return {"path": path, "rows": len(frames)}


# ─── Cleanup ─────────────────────────────────────────────────────────────────

@server.method("cleanup")
def handle_cleanup(params):
    """Kill running processes and remove temp files."""
    for proc in _state.get("ffmpeg_procs", []):
        try:
            proc.kill()
        except OSError:
            pass

    mpv = _state.get("mpv")
    if mpv:
        try:
            mpv.kill()
        except Exception:
            pass

    log_path = _state.get("log_path")
    stats_path = _state.get("stats_path")
    if log_path:
        try:
            os.unlink(log_path)
        except FileNotFoundError:
            pass
    if stats_path:
        try:
            os.unlink(stats_path)
        except FileNotFoundError:
            pass

    return {"status": "cleaned"}


# ─── Main ────────────────────────────────────────────────────────────────────

def _handle_sigterm(sig, frame):
    handle_cleanup({})
    sys.exit(0)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, _handle_sigterm)

    # Safety net: ensure mpv/ffmpeg children are killed even if Python
    # exits abnormally (e.g. parent sends SIGKILL and we only get to
    # run atexit handlers during interpreter shutdown).
    atexit.register(lambda: handle_cleanup({}))

    log("vcompare engine ready (pid={})".format(os.getpid()))
    server.run()
