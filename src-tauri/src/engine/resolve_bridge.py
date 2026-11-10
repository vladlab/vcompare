"""vcompare → DaVinci Resolve playhead bridge.

Spawned by the app. Connects to the running Resolve through its scripting
API (DaVinciResolveScript, which loads fusionscript.so / .dll), then reads
one command per line on stdin and answers one line per command on stdout:

    SEEK 01:00:05:12   → OK                   playhead moved
                       → ERR <message>       Resolve refused / not on a timeline
    PING               → OK <project> / <timeline>
    TRACKS             → OK <json>           video tracks + timeline info
    CLIPS 1,3          → OK <json>           items on those video tracks
    QUIT               → (exits)

TRACKS answers {"timeline": {...}, "tracks": [{"index", "name", "enabled",
"items"}]}. CLIPS answers {"timeline": {...}, "items": [{"track", "name",
"start", "duration", "end", "enabled"}]}. Timeline info: {"project", "name",
"start_frame", "end_frame", "start_tc", "fps", "drop_frame"}; "fps" is
Resolve's own setting string ("23.976", "29.97", "25" ...). Frames are
timeline frame numbers as Resolve reports them (GetStart / GetDuration).

Startup prints exactly one line first: "READY <project> / <timeline>" or
"FAIL <message>". Resolve must be running with external scripting enabled
(Preferences › System › General › External scripting using: Local).
"""
import json
import os
import sys


def _setup_paths():
    """Point Python at Resolve's scripting module and library, per platform,
    unless the user already set RESOLVE_SCRIPT_API / RESOLVE_SCRIPT_LIB."""
    if sys.platform.startswith("win"):
        api = os.path.join(
            os.environ.get("PROGRAMDATA", r"C:\ProgramData"),
            "Blackmagic Design", "DaVinci Resolve", "Support", "Developer", "Scripting",
        )
        lib = r"C:\Program Files\Blackmagic Design\DaVinci Resolve\fusionscript.dll"
    elif sys.platform == "darwin":
        api = "/Library/Application Support/Blackmagic Design/DaVinci Resolve/Developer/Scripting"
        lib = "/Applications/DaVinci Resolve/DaVinci Resolve.app/Contents/Libraries/Fusion/fusionscript.so"
    else:
        api = "/opt/resolve/Developer/Scripting"
        lib = "/opt/resolve/libs/Fusion/fusionscript.so"
        if not os.path.exists(lib):
            # Some installs live under /home/resolve
            alt = "/home/resolve/libs/Fusion/fusionscript.so"
            if os.path.exists(alt):
                lib = alt
                api = "/home/resolve/Developer/Scripting"
    api = os.environ.get("RESOLVE_SCRIPT_API", api)
    lib = os.environ.get("RESOLVE_SCRIPT_LIB", lib)
    os.environ["RESOLVE_SCRIPT_API"] = api
    os.environ["RESOLVE_SCRIPT_LIB"] = lib
    modules = os.path.join(api, "Modules")
    if modules not in sys.path:
        sys.path.insert(0, modules)
    return api, lib


def _connect():
    api, lib = _setup_paths()
    if not os.path.exists(lib):
        return None, "DaVinci Resolve not found (no %s)" % lib
    try:
        import DaVinciResolveScript as dvr  # noqa: E402
    except ImportError as e:
        return None, "Resolve scripting module not importable from %s: %s" % (api, e)
    try:
        resolve = dvr.scriptapp("Resolve")
    except Exception as e:  # noqa: BLE001
        return None, "Resolve scripting failed: %s" % e
    if resolve is None:
        return None, ("Resolve isn't running, or external scripting is off "
                      "(Preferences > System > General > External scripting using: Local)")
    return resolve, None


def _timeline(resolve):
    pm = resolve.GetProjectManager()
    project = pm.GetCurrentProject() if pm else None
    if project is None:
        return None, None, "no project open in Resolve"
    timeline = project.GetCurrentTimeline()
    if timeline is None:
        return project, None, "no timeline open in Resolve"
    return project, timeline, None


def _describe(resolve):
    project, timeline, err = _timeline(resolve)
    if err:
        return err
    try:
        return "%s / %s" % (project.GetName(), timeline.GetName())
    except Exception:  # noqa: BLE001
        return "connected"


def _get(obj, name, *args):
    """Call an API method that may be missing on older Resolve builds."""
    fn = getattr(obj, name, None)
    if fn is None:
        return None
    try:
        return fn(*args)
    except Exception:  # noqa: BLE001
        return None


def _timeline_info(project, timeline):
    def setting(key):
        v = _get(timeline, "GetSetting", key)
        if v in (None, ""):
            v = _get(project, "GetSetting", key)
        return v
    return {
        "project": _get(project, "GetName"),
        "name": _get(timeline, "GetName"),
        "start_frame": _get(timeline, "GetStartFrame"),
        "end_frame": _get(timeline, "GetEndFrame"),
        "start_tc": _get(timeline, "GetStartTimecode"),
        "fps": setting("timelineFrameRate"),
        "drop_frame": str(setting("timelineDropFrameTimecode") or "0") in ("1", "True", "true"),
    }


def _int(v):
    try:
        return int(round(float(v)))
    except (TypeError, ValueError):
        return None


def _tracks(project, timeline):
    out = []
    n = _int(_get(timeline, "GetTrackCount", "video")) or 0
    for i in range(1, n + 1):
        items = _get(timeline, "GetItemListInTrack", "video", i) or []
        out.append({
            "index": i,
            "name": _get(timeline, "GetTrackName", "video", i) or ("V%d" % i),
            # None when the build has no GetIsTrackEnabled (before 18.5)
            "enabled": _get(timeline, "GetIsTrackEnabled", "video", i),
            "items": len(items),
        })
    return {"timeline": _timeline_info(project, timeline), "tracks": out}


def _clips(project, timeline, arg):
    try:
        wanted = [int(x) for x in arg.replace(" ", "").split(",") if x]
    except ValueError:
        raise ValueError("bad track list %r" % arg)
    items = []
    for t in wanted:
        for it in _get(timeline, "GetItemListInTrack", "video", t) or []:
            items.append({
                "track": t,
                "name": _get(it, "GetName") or "",
                "start": _int(_get(it, "GetStart")),
                "duration": _int(_get(it, "GetDuration")),
                "end": _int(_get(it, "GetEnd")),
                # None when the build can't tell
                "enabled": _get(it, "GetClipEnabled"),
            })
    return {"timeline": _timeline_info(project, timeline), "items": items}


def main():
    out = sys.stdout
    resolve, err = _connect()
    if err:
        out.write("FAIL %s\n" % err)
        out.flush()
        return 1
    out.write("READY %s\n" % _describe(resolve))
    out.flush()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        cmd, _, arg = line.partition(" ")
        cmd = cmd.upper()
        if cmd == "QUIT":
            break
        if cmd == "PING":
            out.write("OK %s\n" % _describe(resolve))
            out.flush()
            continue
        if cmd not in ("SEEK", "TRACKS", "CLIPS"):
            out.write("ERR unknown command %s\n" % cmd)
            out.flush()
            continue

        _project, timeline, terr = _timeline(resolve)
        if terr:
            # Resolve may have been restarted: try to reconnect once.
            resolve2, cerr = _connect()
            if resolve2 is not None:
                resolve = resolve2
                _project, timeline, terr = _timeline(resolve)
            if terr:
                out.write("ERR %s\n" % (terr if not cerr else cerr))
                out.flush()
                continue
        if cmd in ("TRACKS", "CLIPS"):
            try:
                info = _tracks(_project, timeline) if cmd == "TRACKS" else _clips(_project, timeline, arg)
                out.write("OK %s\n" % json.dumps(info, separators=(",", ":")))
            except Exception as e:  # noqa: BLE001
                out.write("ERR %s\n" % str(e).replace("\n", " "))
            out.flush()
            continue
        try:
            ok = timeline.SetCurrentTimecode(arg)
        except Exception as e:  # noqa: BLE001
            ok = False
            terr = str(e)
        if ok:
            out.write("OK\n")
        else:
            out.write("ERR Resolve refused %s%s (is the timeline open on the Edit or Color page?)\n"
                      % (arg, (": " + terr) if terr else ""))
        out.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
