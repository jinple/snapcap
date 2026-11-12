//! SnapCap's control window. With no console any more, this is the app's visible presence:
//! a taskbar button with SnapCap's own icon, a title and red badge that show when it's
//! recording, a status line, the last message (saved file / errors), and a few buttons.
//!
//! Re-entrancy: messages can arrive while `APP` is already borrowed (e.g. a title change
//! made from `tick`), so every access goes through `with_app`, which skips if it's taken.

use std::cell::Cell;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Shell::{ITaskbarList3, ShellExecuteW, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR, w};

const ST_STATUS: i32 = 201;
const ST_NOTE: i32 = 202;
const ST_HINTS: i32 = 203;
const BTN_REC: i32 = 210;
const BTN_SETTINGS: i32 = 211;
const BTN_FOLDER: i32 = 212;
const BTN_QUIT: i32 = 213;
const BTN_DX11: i32 = 214;

thread_local! {
    static FONT: Cell<isize> = const { Cell::new(0) };
    /// Value of the registered "TaskbarButtonCreated" message (0 until registered).
    static TASKBAR_MSG: Cell<u32> = const { Cell::new(0) };
}

fn with_app<R>(f: impl FnOnce(&mut crate::App) -> R) -> Option<R> {
    crate::APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// What the window should currently say.
pub struct Status<'a> {
    pub recording: bool,
    pub elapsed: u64,
    pub name: &'a str,
    pub fps: u32,
    pub note: &'a str,
    pub hints: &'a str,
}

pub struct MainWindow {
    pub hwnd: HWND,
    taskbar: Option<ITaskbarList3>,
    taskbar_ready: bool,
    rec_icon: HICON,
    // what's currently displayed, so we only touch Windows when something changed
    title: String,
    status: String,
    note: String,
    hints: String,
    rec_btn: String,
    badge: Option<bool>,
    dx11_shown: bool,
}

/// Gives a window SnapCap's icon (the one embedded in the exe), at the sizes Windows asks for.
pub unsafe fn set_window_icons(hwnd: HWND) {
    unsafe {
        let Ok(hinst) = GetModuleHandleW(None) else { return };
        for (kind, cx, cy) in [(1usize, SM_CXICON, SM_CYICON), (0usize, SM_CXSMICON, SM_CYSMICON)] {
            if let Ok(icon) = LoadImageW(
                Some(hinst.into()),
                PCWSTR(1 as *const u16), // resource id 1 = the app icon (see build.rs)
                IMAGE_ICON,
                GetSystemMetrics(cx),
                GetSystemMetrics(cy),
                LR_DEFAULTCOLOR,
            ) {
                SendMessageW(hwnd, WM_SETICON, Some(WPARAM(kind)), Some(LPARAM(icon.0 as isize)));
            }
        }
    }
}

/// A 32x32 red record dot with a white ring: the badge shown on the taskbar button while recording.
unsafe fn make_rec_icon() -> HICON {
    unsafe {
        use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};
        let mut pm = Pixmap::new(32, 32).unwrap();
        let circle = PathBuilder::from_circle(16.0, 16.0, 12.5).unwrap();
        let mut red = Paint::default();
        red.set_color(Color::from_rgba8(255, 38, 38, 255));
        red.anti_alias = true;
        pm.fill_path(&circle, &red, FillRule::Winding, Transform::identity(), None);
        let mut white = Paint::default();
        white.set_color(Color::WHITE);
        white.anti_alias = true;
        pm.stroke_path(&circle, &white, &Stroke { width: 3.0, ..Stroke::default() }, Transform::identity(), None);

        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 32,
                biHeight: -32,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0) else { return HICON::default() };
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, 32 * 32 * 4);
        for (d, p) in dst.chunks_exact_mut(4).zip(pm.pixels()) {
            let c = p.demultiply(); // icons want straight (not premultiplied) alpha
            d[0] = c.blue();
            d[1] = c.green();
            d[2] = c.red();
            d[3] = c.alpha();
        }
        let mask = CreateBitmap(32, 32, 1, 1, None);
        let icon = CreateIconIndirect(&ICONINFO { fIcon: BOOL(1), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color })
            .unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

pub unsafe fn create(start_minimized: bool) -> MainWindow {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let class = w!("SnapCapMain");
        TASKBAR_MSG.set(RegisterWindowMessageW(w!("TaskbarButtonCreated")));
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: hinst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as isize as _),
            ..Default::default()
        });

        let k = GetDpiForSystem() as f32 / 96.0;
        let style = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut r = RECT { left: 0, top: 0, right: (400.0 * k) as i32, bottom: (254.0 * k) as i32 };
        let _ = AdjustWindowRectEx(&mut r, style, false, WINDOW_EX_STYLE(0));
        let (ww, wh) = (r.right - r.left, r.bottom - r.top);
        let x = (GetSystemMetrics(SM_CXSCREEN) - ww) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - wh) / 3;
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("SnapCap"), style, x, y, ww, wh, None, None, Some(hinst.into()), None)
            .expect("create SnapCap window");
        set_window_icons(hwnd);

        // Start tucked away in the taskbar (e.g. when launched at sign-in) or visible as normal.
        // Never steal focus from a game either way.
        let _ = ShowWindow(hwnd, if start_minimized { SW_SHOWMINNOACTIVE } else { SW_SHOWNOACTIVATE });

        let taskbar = CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER)
            .ok()
            .filter(|t| t.HrInit().is_ok());
        MainWindow {
            hwnd,
            taskbar,
            taskbar_ready: false,
            rec_icon: make_rec_icon(),
            title: String::new(),
            status: String::new(),
            note: String::new(),
            hints: String::new(),
            rec_btn: String::new(),
            badge: None,
            dx11_shown: false,
        }
    }
}

impl MainWindow {
    /// The taskbar told us its button exists, so the badge can be (re)applied.
    pub fn taskbar_created(&mut self) {
        self.taskbar_ready = true;
        self.badge = None;
    }

    /// Shows or hides the "Restart in DX11" button (only touches Windows when it changes).
    pub unsafe fn set_dx11(&mut self, show: bool) {
        unsafe {
            if self.dx11_shown == show {
                return;
            }
            self.dx11_shown = show;
            if let Ok(b) = GetDlgItem(Some(self.hwnd), BTN_DX11) {
                let _ = ShowWindow(b, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
            }
        }
    }

    unsafe fn set_text(&self, id: i32, text: &str) {
        unsafe {
            let t = wide(text);
            let hw = if id == 0 { self.hwnd } else { GetDlgItem(Some(self.hwnd), id).unwrap_or_default() };
            let _ = SetWindowTextW(hw, PCWSTR(t.as_ptr()));
        }
    }

    /// Refreshes title, taskbar badge, status text and buttons; only touches what changed.
    pub unsafe fn update(&mut self, s: &Status) {
        unsafe {
            let secs = |e: u64| if e >= 3600 { format!("{}:{:02}:{:02}", e / 3600, e / 60 % 60, e % 60) } else { format!("{:02}:{:02}", e / 60, e % 60) };
            let (title, status, rec_btn) = if s.recording {
                (
                    format!("SnapCap \u{2014} REC {}", secs(s.elapsed)),
                    format!("\u{25CF} Recording {} \u{00B7} {} \u{00B7} {} FPS", s.name, secs(s.elapsed), s.fps),
                    "Stop recording",
                )
            } else if s.name.is_empty() {
                ("SnapCap".to_string(), "Ready".to_string(), "Start recording")
            } else {
                ("SnapCap".to_string(), format!("Ready \u{00B7} {} \u{00B7} {} FPS", s.name, s.fps), "Start recording")
            };
            if title != self.title {
                self.set_text(0, &title);
                self.title = title;
            }
            if status != self.status {
                self.set_text(ST_STATUS, &status);
                self.status = status;
            }
            if s.note != self.note {
                self.set_text(ST_NOTE, s.note);
                self.note = s.note.to_string();
            }
            if s.hints != self.hints {
                self.set_text(ST_HINTS, s.hints);
                self.hints = s.hints.to_string();
            }
            if rec_btn != self.rec_btn {
                self.set_text(BTN_REC, rec_btn);
                self.rec_btn = rec_btn.to_string();
            }
            if self.badge != Some(s.recording) && self.taskbar_ready {
                if let Some(t) = &self.taskbar {
                    let _ = if s.recording {
                        t.SetOverlayIcon(self.hwnd, self.rec_icon, w!("Recording"))
                    } else {
                        t.SetOverlayIcon(self.hwnd, HICON::default(), PCWSTR::null())
                    };
                }
                self.badge = Some(s.recording);
            }
        }
    }
}

unsafe fn child(parent: HWND, class: PCWSTR, text: &str, style: u32, (x, y, w_, h): (i32, i32, i32, i32), id: i32, k: f32) -> HWND {
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

unsafe fn create_controls(hwnd: HWND) {
    unsafe {
        let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96);
        let k = dpi as f32 / 96.0;
        let font = CreateFontW(
            -((9.0 * dpi as f32 / 72.0).round() as i32),
            0, 0, 0, 400, 0, 0, 0,
            FONT_CHARSET(1),
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            FONT_QUALITY(5),
            0,
            w!("Segoe UI"),
        );
        FONT.set(font.0 as isize);

        let (stat, btn, tab) = (w!("STATIC"), w!("BUTTON"), WS_TABSTOP.0);
        child(hwnd, stat, "Starting\u{2026}", 0, (16, 14, 368, 22), ST_STATUS, k);
        child(hwnd, stat, "", 0, (16, 40, 368, 38), ST_NOTE, k);
        child(hwnd, btn, "Start recording", tab | 1 /*BS_DEFPUSHBUTTON*/, (16, 88, 180, 32), BTN_REC, k);
        child(hwnd, btn, "Settings\u{2026}", tab, (204, 88, 180, 32), BTN_SETTINGS, k);
        child(hwnd, btn, "Open recordings folder", tab, (16, 128, 180, 32), BTN_FOLDER, k);
        child(hwnd, btn, "Quit SnapCap", tab, (204, 128, 180, 32), BTN_QUIT, k);
        let dx = child(hwnd, btn, "Restart game in DX11 (enables the overlay)", tab, (16, 168, 368, 32), BTN_DX11, k);
        let _ = ShowWindow(dx, SW_HIDE); // only shown for Unreal games running Direct3D 12
        child(hwnd, stat, "", 0, (16, 214, 368, 34), ST_HINTS, k);
    }
}

/// Asks "close the game and start it again with -dx11?" and, on Yes, does it. Shown on top of
/// the game (this is reached from a hotkey while the game has focus). Must be called with the
/// app *not* borrowed: the message box runs its own message loop and the timer needs the app.
pub unsafe fn confirm_restart(owner: Option<HWND>) {
    unsafe {
        let Some(Some(name)) = with_app(|app| app.dx11_prompt()) else { return };
        let text = wide(&format!(
            "Close {name} and start it again with -dx11 (Direct3D 11)?\n\n\
             Unsaved progress in the game will be lost, and not every game supports this switch."
        ));
        let r = MessageBoxW(
            owner,
            PCWSTR(text.as_ptr()),
            w!("Restart in DX11"),
            MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND,
        );
        if r == IDYES {
            with_app(|app| app.restart_dx11());
        }
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let tb = TASKBAR_MSG.get();
        if tb != 0 && msg == tb {
            with_app(|app| app.mw.taskbar_created());
            return LRESULT(0);
        }
        match msg {
            WM_CREATE => {
                create_controls(hwnd);
                LRESULT(0)
            }
            WM_COMMAND => {
                let (id, code) = ((wp.0 & 0xFFFF) as i32, ((wp.0 >> 16) & 0xFFFF) as u32);
                if code == 0 {
                    match id {
                        BTN_REC => {
                            with_app(|app| app.toggle_record());
                        }
                        BTN_SETTINGS => {
                            with_app(|app| app.open_settings());
                        }
                        BTN_FOLDER => {
                            if let Some(dir) = with_app(|app| app.cfg.out_dir.clone()) {
                                let _ = std::fs::create_dir_all(&dir);
                                let d = wide(&dir.to_string_lossy());
                                ShellExecuteW(Some(hwnd), w!("open"), PCWSTR(d.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
                            }
                        }
                        BTN_QUIT => {
                            with_app(|app| app.quit());
                        }
                        BTN_DX11 => confirm_restart(Some(hwnd)),
                        _ => {}
                    }
                }
                LRESULT(0)
            }
            // The X button / taskbar "Close window" quits SnapCap (stopping any recording cleanly).
            WM_CLOSE => {
                if with_app(|app| app.quit()).is_none() {
                    PostQuitMessage(0);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
