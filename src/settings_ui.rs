//! The little settings window (opened with F8 by default): font sizes, whether the
//! overlay is burned into recordings, and keybinds. Plain Win32 controls.
//!
//! Re-entrancy note: this window's messages can arrive while the main window proc
//! already holds `APP` borrowed (e.g. creating the window from a hotkey handler),
//! so every access goes through `with_app`, which just skips if the borrow is taken.

use std::cell::Cell;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::settings::*;

const TBM_GETPOS: u32 = 0x0400; // WM_USER
const SL_COUNTER: i32 = 101;
const SL_LABEL: i32 = 102;
const LBL_COUNTER: i32 = 103;
const LBL_LABEL: i32 = 104;
const CHK_REC: i32 = 105;
const CHK_HOOK: i32 = 106;
const SL_LIMIT: i32 = 107;
const LBL_LIMIT: i32 = 108;
const HK_BASE: i32 = 110; // 110..=113
const BTN_SAVE: i32 = 120;
const BTN_CLOSE: i32 = 121;
const BTN_RESET: i32 = 122;

thread_local! {
    static SETTINGS_HWND: Cell<isize> = const { Cell::new(0) };
    static FONT: Cell<isize> = const { Cell::new(0) };
}

pub fn is_open() -> bool {
    SETTINGS_HWND.get() != 0
}

fn with_app<R>(f: impl FnOnce(&mut crate::App) -> R) -> Option<R> {
    crate::APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub unsafe fn open(s: &Settings) {
    unsafe {
        let existing = SETTINGS_HWND.get();
        if existing != 0 {
            let _ = SetForegroundWindow(HWND(existing as _));
            return;
        }
        let hinst = GetModuleHandleW(None).unwrap();
        let class = w!("SnapCapSettings");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: hinst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as isize as _),
            ..Default::default()
        });

        let k = GetDpiForSystem() as f32 / 96.0;
        let (cw, ch) = ((420.0 * k) as i32, (516.0 * k) as i32);
        let style = WS_CAPTION | WS_SYSMENU;
        let mut r = RECT { left: 0, top: 0, right: cw, bottom: ch };
        let _ = AdjustWindowRectEx(&mut r, style, false, WS_EX_TOPMOST);
        let (ww, wh) = (r.right - r.left, r.bottom - r.top);
        let x = (GetSystemMetrics(SM_CXSCREEN) - ww) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - wh) / 2;

        let init = Box::into_raw(Box::new(s.clone()));
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST,
            class,
            w!("SnapCap Settings"),
            style,
            x,
            y,
            ww,
            wh,
            None,
            None,
            Some(hinst.into()),
            Some(init as *const _),
        );
        match hwnd {
            Ok(h) => {
                SETTINGS_HWND.set(h.0 as isize);
                let _ = ShowWindow(h, SW_SHOW);
                let _ = SetForegroundWindow(h);
            }
            Err(_) => drop(Box::from_raw(init)),
        }
    }
}

unsafe fn child(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    style: u32,
    (x, y, w_, h): (i32, i32, i32, i32),
    id: i32,
    k: f32,
) -> HWND {
    unsafe {
        let t = wide(text);
        let hinst = GetModuleHandleW(None).unwrap();
        let hw = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(t.as_ptr()),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
            (x as f32 * k) as i32,
            (y as f32 * k) as i32,
            (w_ as f32 * k) as i32,
            (h as f32 * k) as i32,
            Some(parent),
            Some(HMENU(id as isize as _)),
            Some(hinst.into()),
            None,
        )
        .unwrap_or_default();
        SendMessageW(hw, WM_SETFONT, Some(WPARAM(FONT.get() as usize)), Some(LPARAM(1)));
        hw
    }
}

fn mods_to_hotkeyf(m: u32) -> u8 {
    (if m & MOD_SHIFT.0 != 0 { 1 } else { 0 })
        | (if m & MOD_CONTROL.0 != 0 { 2 } else { 0 })
        | (if m & MOD_ALT.0 != 0 { 4 } else { 0 })
}

fn hotkeyf_to_mods(f: u32) -> u32 {
    (if f & 1 != 0 { MOD_SHIFT.0 } else { 0 })
        | (if f & 2 != 0 { MOD_CONTROL.0 } else { 0 })
        | (if f & 4 != 0 { MOD_ALT.0 } else { 0 })
}

unsafe fn get_item(hwnd: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(Some(hwnd), id).unwrap_or_default() }
}

unsafe fn set_text(hwnd: HWND, id: i32, t: &str) {
    unsafe {
        let t = wide(t);
        let _ = SetWindowTextW(get_item(hwnd, id), PCWSTR(t.as_ptr()));
    }
}

unsafe fn read_slider(hwnd: HWND, id: i32) -> u32 {
    unsafe { SendMessageW(get_item(hwnd, id), TBM_GETPOS, None, None).0 as u32 }
}

unsafe fn read_keys(hwnd: HWND) -> [Key; 4] {
    unsafe {
        let mut keys = DEFAULT_KEYS;
        for (i, k) in keys.iter_mut().enumerate() {
            let v = SendMessageW(get_item(hwnd, HK_BASE + i as i32), HKM_GETHOTKEY, None, None).0 as u32;
            *k = Key { vk: v & 0xFF, mods: hotkeyf_to_mods((v >> 8) & 0xFF) };
        }
        keys
    }
}

unsafe fn fill_keys(hwnd: HWND, keys: &[Key; 4]) {
    unsafe {
        for (i, k) in keys.iter().enumerate() {
            let v = (k.vk as usize & 0xFF) | ((mods_to_hotkeyf(k.mods) as usize) << 8);
            SendMessageW(get_item(hwnd, HK_BASE + i as i32), HKM_SETHOTKEY, Some(WPARAM(v)), None);
        }
    }
}

unsafe fn create_controls(hwnd: HWND, s: &Settings) {
    unsafe {
        let _ = InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES | ICC_HOTKEY_CLASS,
        });
        let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96);
        let k = dpi as f32 / 96.0;
        let font = CreateFontW(
            -((9.0 * dpi as f32 / 72.0).round() as i32),
            0, 0, 0, 400, 0, 0, 0,
            FONT_CHARSET(1), // DEFAULT_CHARSET
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            FONT_QUALITY(5), // CLEARTYPE_QUALITY
            0,
            w!("Segoe UI"),
        );
        FONT.set(font.0 as isize);

        let (ws_tab, st_right) = (WS_TABSTOP.0, 2u32);
        let stat = w!("STATIC");
        let btn = w!("BUTTON");

        child(hwnd, stat, "Font sizes", 0, (16, 10, 380, 20), 0, k);
        child(hwnd, stat, "FPS counter", 0, (16, 40, 140, 20), 0, k);
        let sl = child(hwnd, w!("msctls_trackbar32"), "", ws_tab | 1 /*TBS_AUTOTICKS*/, (160, 34, 180, 30), SL_COUNTER, k);
        SendMessageW(sl, TBM_SETRANGE, Some(WPARAM(1)), Some(LPARAM(((MAX_PCT << 16) | MIN_PCT) as isize)));
        SendMessageW(sl, TBM_SETTICFREQ, Some(WPARAM(25)), None);
        SendMessageW(sl, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(s.counter_pct as isize)));
        child(hwnd, stat, &format!("{}%", s.counter_pct), st_right, (346, 40, 56, 20), LBL_COUNTER, k);

        child(hwnd, stat, "App name / timer / hints", 0, (16, 76, 142, 20), 0, k);
        let sl = child(hwnd, w!("msctls_trackbar32"), "", ws_tab | 1, (160, 70, 180, 30), SL_LABEL, k);
        SendMessageW(sl, TBM_SETRANGE, Some(WPARAM(1)), Some(LPARAM(((MAX_PCT << 16) | MIN_PCT) as isize)));
        SendMessageW(sl, TBM_SETTICFREQ, Some(WPARAM(25)), None);
        SendMessageW(sl, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(s.label_pct as isize)));
        child(hwnd, stat, &format!("{}%", s.label_pct), st_right, (346, 76, 56, 20), LBL_LABEL, k);

        child(hwnd, stat, "Frame limit (focused app)", 0, (16, 112, 142, 20), 0, k);
        let sl = child(hwnd, w!("msctls_trackbar32"), "", ws_tab | 1, (160, 106, 180, 30), SL_LIMIT, k);
        SendMessageW(sl, TBM_SETRANGE, Some(WPARAM(1)), Some(LPARAM(((MAX_FPS_LIMIT << 16) | 0) as isize)));
        SendMessageW(sl, TBM_SETTICFREQ, Some(WPARAM(30)), None);
        SendMessageW(sl, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(s.fps_limit as isize)));
        child(hwnd, stat, &limit_text(s.fps_limit), st_right, (346, 112, 56, 20), LBL_LIMIT, k);
        child(hwnd, stat, "Caps the game's frame rate. Loads the in-game hook (Direct3D 11 games only, never anti-cheat).", 0, (34, 138, 372, 34), 0, k);

        let chk = child(
            hwnd,
            btn,
            "Show overlay in recordings",
            ws_tab | 3, /*BS_AUTOCHECKBOX*/
            (16, 176, 380, 22),
            CHK_REC,
            k,
        );
        SendMessageW(chk, BM_SETCHECK, Some(WPARAM(s.overlay_in_rec as usize)), None);
        child(hwnd, stat, "(off: not in the video. On also works in exclusive fullscreen)", 0, (34, 198, 360, 18), 0, k);

        let hk = child(
            hwnd,
            btn,
            "In-game overlay for exclusive fullscreen (experimental)",
            ws_tab | 3,
            (16, 228, 390, 22),
            CHK_HOOK,
            k,
        );
        SendMessageW(hk, BM_SETCHECK, Some(WPARAM(s.hook_enabled as usize)), None);
        child(hwnd, stat, "Loads a DLL into Direct3D 11 games to draw the overlay inside them. Never used with anti-cheat. Applies the next time you play.", 0, (34, 250, 372, 34), 0, k);

        child(hwnd, stat, "Keybinds  (click a box, then press the keys)", 0, (16, 294, 380, 20), 0, k);
        for (i, name) in ACTION_NAMES.iter().enumerate() {
            let y = 320 + 36 * i as i32;
            child(hwnd, stat, name, 0, (16, y + 4, 140, 20), 0, k);
            child(hwnd, w!("msctls_hotkey32"), "", ws_tab, (160, y, 180, 26), HK_BASE + i as i32, k);
        }
        fill_keys(hwnd, &s.keys);

        child(hwnd, btn, "Reset defaults", ws_tab, (16, 476, 110, 28), BTN_RESET, k);
        child(hwnd, btn, "Save", ws_tab | 1 /*BS_DEFPUSHBUTTON*/, (236, 476, 80, 28), BTN_SAVE, k);
        child(hwnd, btn, "Close", ws_tab, (322, 476, 80, 28), BTN_CLOSE, k);
    }
}

fn limit_text(l: u32) -> String {
    if l == 0 { "Off".into() } else { format!("{l} FPS") }
}

unsafe fn apply_visuals(hwnd: HWND) {
    unsafe {
        let (c, l) = (read_slider(hwnd, SL_COUNTER), read_slider(hwnd, SL_LABEL));
        let limit = read_slider(hwnd, SL_LIMIT).min(MAX_FPS_LIMIT);
        set_text(hwnd, LBL_LIMIT, &limit_text(limit));
        let on = SendMessageW(get_item(hwnd, CHK_REC), BM_GETCHECK, None, None).0 == 1;
        let hook = SendMessageW(get_item(hwnd, CHK_HOOK), BM_GETCHECK, None, None).0 == 1;
        set_text(hwnd, LBL_COUNTER, &format!("{c}%"));
        set_text(hwnd, LBL_LABEL, &format!("{l}%"));
        with_app(|app| {
            app.settings.counter_pct = c;
            app.settings.label_pct = l;
            app.settings.overlay_in_rec = on;
            app.settings.hook_enabled = hook;
            app.settings.fps_limit = limit;
            app.apply_visuals();
        });
    }
}

unsafe fn show_error(hwnd: HWND, msg: &str) {
    unsafe {
        let m = wide(msg);
        MessageBoxW(Some(hwnd), PCWSTR(m.as_ptr()), w!("SnapCap"), MB_OK | MB_ICONWARNING);
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let cs = lp.0 as *const CREATESTRUCTW;
                let s = Box::from_raw((*cs).lpCreateParams as *mut Settings);
                create_controls(hwnd, &s);
                LRESULT(0)
            }
            WM_HSCROLL => {
                apply_visuals(hwnd);
                LRESULT(0)
            }
            WM_COMMAND => {
                let (id, code) = ((wp.0 & 0xFFFF) as i32, ((wp.0 >> 16) & 0xFFFF) as u32);
                match id {
                    CHK_REC | CHK_HOOK if code == 0 => apply_visuals(hwnd),
                    BTN_RESET if code == 0 => {
                        let d = Settings::default();
                        SendMessageW(get_item(hwnd, SL_COUNTER), TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(d.counter_pct as isize)));
                        SendMessageW(get_item(hwnd, SL_LABEL), TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(d.label_pct as isize)));
                        SendMessageW(get_item(hwnd, CHK_REC), BM_SETCHECK, Some(WPARAM(d.overlay_in_rec as usize)), None);
                        SendMessageW(get_item(hwnd, CHK_HOOK), BM_SETCHECK, Some(WPARAM(d.hook_enabled as usize)), None);
                        SendMessageW(get_item(hwnd, SL_LIMIT), TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(d.fps_limit as isize)));
                        fill_keys(hwnd, &d.keys);
                        apply_visuals(hwnd);
                    }
                    BTN_SAVE if code == 0 => {
                        let keys = read_keys(hwnd);
                        match with_app(|app| app.set_keys(keys)) {
                            Some(Ok(())) => {
                                with_app(|app| app.settings.save());
                                let _ = DestroyWindow(hwnd);
                            }
                            Some(Err(e)) => show_error(hwnd, &e),
                            None => {}
                        }
                    }
                    BTN_CLOSE if code == 0 => {
                        let _ = SendMessageW(hwnd, WM_CLOSE, None, None);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                // font sizes / overlay option are live, so keep them; keybinds only change via Save
                with_app(|app| app.settings.save());
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                SETTINGS_HWND.set(0);
                let f = FONT.replace(0);
                if f != 0 {
                    let _ = DeleteObject(HGDIOBJ(f as _));
                }
                // global hotkeys were released while the window was open so they could be re-bound
                with_app(|app| app.register_keys());
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
