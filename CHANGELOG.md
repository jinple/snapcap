# Changelog

Each release's section below becomes its GitHub release description.

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
- Installer (`SnapCap-Setup-0.1.0.exe`): per-user install, Start Menu shortcut, optional desktop shortcut and start-with-Windows.
