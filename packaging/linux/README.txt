vcompare (native)
=================

Compares two video files frame by frame (pixel difference, SSIM, VMAF),
measures signal levels, finds flash frames, repeats and look-alike frames,
and checks a deliverable against what DaVinci Resolve plays out on SDI.

Install
-------
    ./install.sh                              just for you, in ~/.local
    sudo ./install.sh --prefix /usr/local     for everyone on the machine

Then start "vcompare (native)" from the application menu, or run
vcompare-egui (optionally with files to open: two videos to compare, one
for levels, or a saved analysis .json).

Uninstall with ./install.sh --uninstall (plus the same --prefix).

Needs
-----
* ffmpeg and ffprobe. VMAF mode needs an ffmpeg built with libvmaf (or
  libvmaf_cuda); File > Tools… shows what the one in use can do, and lets
  you point vcompare at another build.
* mpv, for the side-by-side (p) and difference (d) players. Optional.
* Timeline mode: DaVinci Resolve with external scripting set to Local, and
  scopetap's scopetap_grab on PATH (or VCOMPARE_GRABBER) to read the SDI.

Keys: F1 in the app.

If the window is blank or slow (remote desktops, older GPUs), try the
other renderer:  VCOMPARE_RENDERER=glow vcompare-egui   (or =wgpu).
