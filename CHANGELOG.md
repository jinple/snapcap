# Changelog

Each release's section below becomes its GitHub release description.

## [0.2.0] - 2026-11-12

Much lighter recording, and support for Unreal Engine games.

- **GPU recording.** Frames are now converted on the GPU (D3D11 video processor, including the burned-in overlay) and handed straight to the hardware encoder, instead of being read back and converted on the CPU. CPU use while recording at 1080p60 dropped from about 130% to about 23% of a core, so games lose far fewer frames. New setting **Encode recordings on the GPU** (on by default); turn it off to use the previous CPU path. If the GPU setup isn't supported, SnapCap falls back to the CPU path by itself.
- **10-bit back buffers.** The in-game hook now handles `R10G10B10A2` swap chains (Unreal Engine's default), for both the in-game overlay and clean recording frames.
- **Unreal games that also load `d3d12.dll`** (for example through DLSS/Streamline) are no longer mistaken for Direct3D 12 games when they render with Direct3D 11.
- **Restart in DX11.** For an Unreal game running Direct3D 12 (which the hook can't use), SnapCap offers to close it and relaunch it with `-dx11`: a button in the control window, and an **F7** prompt in the overlay while the game is focused. It always asks first.
- Less overhead while idle and recording: the screen-duplication monitor pauses when the hook reports the game's frame rate, the overlay is re-rendered about 15 times a second instead of per frame, the UI timer runs at about 15 Hz, and the overlay window's topmost re-assert is throttled.
- Fewer in-game GPU stalls: the hook copies a clean frame for the recorder only when asked (once per recorded frame) instead of on every present.
- The hook writes a short timing line to `%TEMP%\snapcap_hook.log` every few seconds (presents per second, its own cost, overlay and recording state) to help diagnose performance.
- Recordings now use the frames' real capture times (variable frame rate on the GPU path).

## [0.1.1] - 2026-11-10

- Font sizes can be set to any value: type 10%-500% in the new box beside each slider (previously 50%-250%).

## [0.1.0] - 2026-11-10

First release.

- Records the focused or fullscreen window to H.264/AAC MP4 (hardware encoded, 60 fps by default).
- Overlay with a live FPS counter, app name, REC timer and pulsing LIVE dot (white idle, red recording).
- Per-application audio: only the recorded app's audio, kept in sync with the video.
- Settings window (F8): font sizes, overlay-in-recordings, rebindable hotkeys.
- Overlay hidden from recordings by default; optional software burn-in that also works in exclusive fullscreen.
- Picks the fullscreen window on top as the target, falling back to the focused window.
- Experimental in-game overlay for exclusive-fullscreen Direct3D 11 games (opt-in; skips anti-cheat, 32-bit and non-D3D11 games).
- Frame limiter: cap the focused game's frame rate (settings, 0 = off to 240 FPS). Paces `Present` through the in-game hook, so it works for Direct3D 11 games only; the cap is released automatically if SnapCap exits.
- No console window: SnapCap runs as a proper GUI app with its own icon. A small control window (status, last saved file or error, Start/Stop, Settings, Open recordings folder, Quit) gives it a taskbar presence; the taskbar title and a red badge on the icon show when it is recording. `--help` still prints when started from a terminal.
- Installer (`SnapCap-Setup-0.1.0.exe`): per-user install, Start Menu shortcut, optional desktop shortcut and start-with-Windows.
