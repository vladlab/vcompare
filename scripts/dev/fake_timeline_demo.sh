#!/usr/bin/env bash
#
# fake_timeline_demo.sh — try vcompare's timeline mode with no Resolve and no
# SDI loop. Builds, in DIR (default /tmp/vcompare_timeline_demo):
#
#   truth.nut       the "monitor": what Resolve would put out on SDI
#   deliverable.mov ProRes 422 HQ of the same reel, with two planted faults:
#                   clip 3 slipped by one frame, clip 5 rendered with the
#                   wrong data levels
#   timeline.json   a fake Resolve timeline: 6 clips on V1, a title on V2
#
# then prints the environment to start vcompare with: the bridge talks to
# src-tauri/testdata/fake_resolve instead of Resolve, and the grabber is
# scripts/dev/fake_scopetap_grab.py serving truth.nut.
#
# Usage:
#   scripts/dev/fake_timeline_demo.sh [DIR]
#   eval "$(scripts/dev/fake_timeline_demo.sh --env [DIR])"; vcompare
#
# Pick timeline mode, choose DIR/deliverable.mov, Connect to Resolve, tick
# V1 (and V2), Check timeline. Clips 3 and 5 should be flagged.

set -euo pipefail
ENV_ONLY=0
if [ "${1:-}" = "--env" ]; then ENV_ONLY=1; shift; fi
DIR=${1:-/tmp/vcompare_timeline_demo}
HERE=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$DIR"

ff() { ffmpeg -hide_banner -loglevel error -y "$@"; }

if [ ! -f "$DIR/deliverable.mov" ]; then
    [ $ENV_ONLY = 1 ] || echo "building demo media in $DIR ..." >&2
    # 6 clips x 2 s at 24p = 288 frames, 960x540.
    ff -f lavfi -i "testsrc2=size=960x540:rate=24:duration=12,noise=alls=12:allf=t" \
       -vf format=rgb48le -c:v rawvideo -f nut "$DIR/truth.nut"
    # Clip k covers frames 48k .. 48k+47. Clip 3 (k=2) shows frame n+1;
    # clip 5 (k=4) has its levels squeezed.
    ff -i "$DIR/truth.nut" -filter_complex "
        [0:v]split=4[a][b][c][d];
        [a]trim=start_frame=0:end_frame=96,setpts=PTS-STARTPTS[a1];
        [b]trim=start_frame=97:end_frame=145,setpts=PTS-STARTPTS[b1];
        [c]trim=start_frame=144:end_frame=192,setpts=PTS-STARTPTS[c1];
        [d]trim=start_frame=192:end_frame=288,setpts=PTS-STARTPTS[d0];
        [d0]split[d1][d2];
        [d1]trim=start_frame=0:end_frame=48,setpts=PTS-STARTPTS,lutrgb=r=val*0.88+7000:g=val*0.88+7000:b=val*0.88+7000[e1];
        [d2]trim=start_frame=48:end_frame=96,setpts=PTS-STARTPTS[f1];
        [a1][b1][c1][e1][f1]concat=n=5:v=1[v];
        [v]scale=out_color_matrix=bt709:out_range=limited:flags=lanczos,format=yuv422p10le[o]" \
       -map "[o]" -c:v prores_ks -profile:v 3 \
       -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv \
       -timecode 01:00:00:00 "$DIR/deliverable.mov"
    cat > "$DIR/timeline.json" <<'EOF'
{"project": "Demo", "name": "Reel 1 (fake)", "start_frame": 86400, "start_tc": "01:00:00:00",
 "fps": "24", "drop_frame": false,
 "tracks": [
   {"name": "V1", "enabled": true, "items": [
     {"name": "A001_C002", "start": 86400, "duration": 48},
     {"name": "A001_C005", "start": 86448, "duration": 48},
     {"name": "A002_C001 (slipped)", "start": 86496, "duration": 48},
     {"name": "A002_C007", "start": 86544, "duration": 48},
     {"name": "A003_C003 (levels)", "start": 86592, "duration": 48},
     {"name": "A003_C011", "start": 86640, "duration": 48}]},
   {"name": "Titles", "enabled": true, "items": [
     {"name": "Main title", "start": 86424, "duration": 36}]}
 ]}
EOF
fi

ENV="export RESOLVE_SCRIPT_API='$HERE/src-tauri/testdata/fake_resolve'
export RESOLVE_SCRIPT_LIB='$HERE/src-tauri/testdata/fake_resolve/Modules/DaVinciResolveScript.py'
export VCOMPARE_FAKE_TIMELINE='$DIR/timeline.json'
export VCOMPARE_FAKE_PLAYHEAD='$DIR/playhead'
export VCOMPARE_GRABBER='$HERE/scripts/dev/fake_scopetap_grab.py'
export FAKE_GRAB_SOURCE='$DIR/truth.nut'
export FAKE_GRAB_START_TC=01:00:00:00
export FAKE_GRAB_PLAYHEAD='$DIR/playhead'"

if [ $ENV_ONLY = 1 ]; then
    echo "$ENV"
else
    echo "Demo media in $DIR. Start vcompare with:"
    echo
    echo "$ENV"
    echo
    echo "Then: Timeline mode, $DIR/deliverable.mov, Connect to Resolve, Check timeline."
fi
