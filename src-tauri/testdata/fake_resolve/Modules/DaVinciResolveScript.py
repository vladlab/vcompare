"""Fake DaVinciResolveScript for tests and dry runs without Resolve.

Laid out like Resolve's own install, so the bridge finds it with:

    RESOLVE_SCRIPT_API=<.../testdata/fake_resolve>   (this file is in Modules/)
    RESOLVE_SCRIPT_LIB=<any existing file, e.g. this one>

and describe the timeline in JSON:

    VCOMPARE_FAKE_TIMELINE=/path/timeline.json
    {
      "project": "Proj", "name": "TL",
      "start_frame": 86400, "start_tc": "01:00:00:00",
      "fps": "24", "drop_frame": false,
      "tracks": [
        {"name": "V1", "enabled": true,
         "items": [{"name": "A", "start": 86400, "duration": 48}, ...]}
      ]
    }

Every accepted SetCurrentTimecode is written (overwritten) to
VCOMPARE_FAKE_PLAYHEAD if set, and appended to VCOMPARE_FAKE_SEEKLOG if set.
The fake scopetap_grab (scripts/dev/fake_scopetap_grab.py) reads the playhead
file to decide what the "SDI output" is showing.
"""
import json
import os


def _load():
    with open(os.environ["VCOMPARE_FAKE_TIMELINE"]) as f:
        return json.load(f)


class _Item:
    def __init__(self, d):
        self._d = d

    def GetName(self):
        return self._d.get("name", "")

    def GetStart(self):
        return self._d["start"]

    def GetDuration(self):
        return self._d["duration"]

    def GetEnd(self):
        # Resolve's GetEnd is start + duration (exclusive); kept that way here.
        return self._d["start"] + self._d["duration"]

    def GetClipEnabled(self):
        return self._d.get("enabled", True)


class _Timeline:
    def __init__(self, d):
        self._d = d

    def GetName(self):
        return self._d.get("name", "TL")

    def GetStartFrame(self):
        return self._d.get("start_frame", 0)

    def GetEndFrame(self):
        ends = [i["start"] + i["duration"] for t in self._d["tracks"] for i in t["items"]]
        return max(ends) if ends else self.GetStartFrame()

    def GetStartTimecode(self):
        return self._d.get("start_tc", "00:00:00:00")

    def GetSetting(self, key):
        if key == "timelineFrameRate":
            return self._d.get("fps", "24")
        if key == "timelineDropFrameTimecode":
            return "1" if self._d.get("drop_frame") else "0"
        return ""

    def GetTrackCount(self, kind):
        return len(self._d["tracks"]) if kind == "video" else 0

    def GetTrackName(self, kind, i):
        return self._d["tracks"][i - 1].get("name", "V%d" % i)

    def GetIsTrackEnabled(self, kind, i):
        return self._d["tracks"][i - 1].get("enabled", True)

    def GetItemListInTrack(self, kind, i):
        if kind != "video" or not 1 <= i <= len(self._d["tracks"]):
            return []
        return [_Item(x) for x in self._d["tracks"][i - 1]["items"]]

    def SetCurrentTimecode(self, tc):
        if len(tc.replace(";", ":").split(":")) != 4:
            return False
        p = os.environ.get("VCOMPARE_FAKE_PLAYHEAD")
        if p:
            tmp = p + ".tmp"
            with open(tmp, "w") as f:
                f.write(tc + "\n")
            os.replace(tmp, p)
        log = os.environ.get("VCOMPARE_FAKE_SEEKLOG")
        if log:
            with open(log, "a") as f:
                f.write(tc + "\n")
        return True


class _Project:
    def __init__(self, d):
        self._d = d

    def GetName(self):
        return self._d.get("project", "Proj")

    def GetCurrentTimeline(self):
        return _Timeline(self._d)

    def GetSetting(self, key):
        return ""


class _PM:
    def GetCurrentProject(self):
        return _Project(_load())


class _Resolve:
    def GetProjectManager(self):
        return _PM()


def scriptapp(name):
    return _Resolve()
