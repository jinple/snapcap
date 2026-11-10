# SnapCap

A lightweight Windows recorder for gameplay (and anything else). It records the focused or
fullscreen window to MP4 and puts a clear **FPS counter** over it, with the app name, a
**REC timer** and a pulsing **LIVE dot** while recording.

- **FPS counter:** white with a black outline when idle, red when recording.
- **Per-app audio:** records only the audio of the app you're recording (plus its child processes).
- **Hardware encoding:** H.264 + AAC in an MP4, written to `Videos\SnapCap`.
- **Settings (F8):** font sizes, whether the overlay is burned into recordings, rebindable hotkeys.
- **Frame limit:** cap the focused game's frame rate from settings (Direct3D 11 games; released
  automatically if SnapCap quits). It uses the same in-game hook as the overlay below.
- **Exclusive fullscreen:** burn-in always works. For the *live* overlay over exclusive fullscreen,
  enable the experimental in-game overlay (Direct3D 11 games only; never use it with anti-cheat).

## Hotkeys (rebindable in settings)

| Key | Action |
| --- | --- |
| F9  | Start / stop recording |
| F10 | Show / hide the overlay |
| F8  | Settings |
| F11 | Quit |

## Install

Download `SnapCap-Setup-<version>.exe` from the
[latest release](https://github.com/jinple/snapcap/releases/latest) and run it.
No admin rights are needed. Settings are stored in `%APPDATA%\SnapCap` and kept on uninstall.

## Build from source

Requires Windows 10 2004+ and a recent Rust toolchain.

```powershell
cargo build --release   # builds snapcap.exe and snapcap_hook.dll into target\release
```

`snapcap_hook.dll` must stay next to `snapcap.exe`.

## Releasing (maintainers)

```powershell
scripts\release.ps1 -Version 0.2.0   # bumps versions, builds the installer, tags, pushes, publishes
scripts\release.ps1 -Version 0.2.0 -NoPublish   # just build dist\SnapCap-Setup-0.2.0.exe
```

Needs [Inno Setup 6](https://jrsoftware.org/isinfo.php) and the GitHub CLI (`gh`). Add the release
notes to `CHANGELOG.md` first; the script uses that section as the release description.
