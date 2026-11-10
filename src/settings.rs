//! User settings: font sizes, "overlay in recordings", keybinds. Stored as a tiny
//! key=value file in %APPDATA%\SnapCap\settings.ini.

use std::path::PathBuf;

use windows::Win32::UI::Input::KeyboardAndMouse::*;

/// Order of `Settings::keys` (index + 1 is the global hotkey id).
pub const ACTION_NAMES: [&str; 4] = ["Toggle Rec", "Toggle Overlay", "Settings", "Quit"];

/// Any size can be typed in, within these bounds (the sliders cover the common part).
pub const MIN_PCT: u32 = 10;
pub const MAX_PCT: u32 = 500;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    pub vk: u32,
    /// `MOD_ALT | MOD_CONTROL | MOD_SHIFT` bits.
    pub mods: u32,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    /// Size of the FPS number, in percent.
    pub counter_pct: u32,
    /// Size of the app name / timer / hints, in percent.
    pub label_pct: u32,
    /// Burn the overlay into recordings (off by default).
    pub overlay_in_rec: bool,
    /// Inject the in-game overlay hook into exclusive-fullscreen Direct3D 11 games.
    /// Off by default: it loads a DLL into the game process.
    pub hook_enabled: bool,
    /// Frame-rate cap for the focused app in FPS (0 = off). Needs the in-game hook.
    pub fps_limit: u32,
    pub keys: [Key; 4],
}

pub const MAX_FPS_LIMIT: u32 = 240;

pub const DEFAULT_KEYS: [Key; 4] = [
    Key { vk: VK_F9.0 as u32, mods: 0 },
    Key { vk: VK_F10.0 as u32, mods: 0 },
    Key { vk: VK_F8.0 as u32, mods: 0 },
    Key { vk: VK_F11.0 as u32, mods: 0 },
];

impl Default for Settings {
    fn default() -> Self {
        Self { counter_pct: 100, label_pct: 100, overlay_in_rec: false, hook_enabled: false, fps_limit: 0, keys: DEFAULT_KEYS }
    }
}

fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("SnapCap").join("settings.ini"))
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Self::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return s };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "counter_pct" => s.counter_pct = v.parse().unwrap_or(100).clamp(MIN_PCT, MAX_PCT),
                "label_pct" => s.label_pct = v.parse().unwrap_or(100).clamp(MIN_PCT, MAX_PCT),
                "overlay_in_recording" => s.overlay_in_rec = v == "1",
                "ingame_hook" => s.hook_enabled = v == "1",
                "fps_limit" => s.fps_limit = v.parse().unwrap_or(0).min(MAX_FPS_LIMIT),
                _ => {
                    if let Some(i) = k.strip_prefix("key").and_then(|n| n.parse::<usize>().ok()) {
                        if let (true, Some((vk, mods))) = (i < 4, v.split_once(',')) {
                            if let (Ok(vk), Ok(mods)) = (vk.parse(), mods.parse()) {
                                if vk != 0 {
                                    s.keys[i] = Key { vk, mods };
                                }
                            }
                        }
                    }
                }
            }
        }
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut t = format!(
            "counter_pct={}\nlabel_pct={}\noverlay_in_recording={}\ningame_hook={}\nfps_limit={}\n",
            self.counter_pct, self.label_pct, self.overlay_in_rec as u8, self.hook_enabled as u8, self.fps_limit
        );
        for (i, k) in self.keys.iter().enumerate() {
            t += &format!("key{i}={},{}\n", k.vk, k.mods);
        }
        let _ = std::fs::write(p, t);
    }

    /// Returns a user-facing problem with these keybinds, if any.
    pub fn validate_keys(keys: &[Key; 4]) -> Option<String> {
        for (i, k) in keys.iter().enumerate() {
            if k.vk == 0 {
                return Some(format!("\"{}\" has no key assigned.", ACTION_NAMES[i]));
            }
            // plain letters/digits/space would hijack normal typing everywhere
            let plain = (0x30..=0x5A).contains(&k.vk) || k.vk == VK_SPACE.0 as u32;
            if plain && k.mods == 0 {
                return Some(format!(
                    "\"{}\": add Ctrl, Alt or Shift to letter/number keys (or use a function key).",
                    ACTION_NAMES[i]
                ));
            }
            for (j, o) in keys.iter().enumerate().skip(i + 1) {
                if o == k {
                    return Some(format!(
                        "\"{}\" and \"{}\" can't share the same key.",
                        ACTION_NAMES[i], ACTION_NAMES[j]
                    ));
                }
            }
        }
        None
    }
}

fn is_extended(vk: u32) -> bool {
    matches!(
        vk,
        0x21..=0x28 /* PgUp..Down */ | 0x2D | 0x2E | 0x6F /* Insert, Delete, Numpad / */ | 0x90
    )
}

/// Human-readable name, e.g. "F9" or "Ctrl+Shift+R".
pub fn key_name(k: Key) -> String {
    let mut s = String::new();
    if k.mods & MOD_CONTROL.0 != 0 {
        s += "Ctrl+";
    }
    if k.mods & MOD_ALT.0 != 0 {
        s += "Alt+";
    }
    if k.mods & MOD_SHIFT.0 != 0 {
        s += "Shift+";
    }
    if (0x70..=0x87).contains(&k.vk) {
        s += &format!("F{}", k.vk - 0x6F);
        return s;
    }
    let sc = unsafe { MapVirtualKeyW(k.vk, MAPVK_VK_TO_VSC) };
    let lparam = ((sc << 16) | if is_extended(k.vk) { 1 << 24 } else { 0 }) as i32;
    let mut buf = [0u16; 64];
    let n = unsafe { GetKeyNameTextW(lparam, &mut buf) } as usize;
    if n > 0 {
        s += &String::from_utf16_lossy(&buf[..n]);
    } else {
        s += &format!("0x{:X}", k.vk);
    }
    s
}
