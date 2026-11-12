//! SnapCap - featherweight gameplay recorder.
//!
//! Default keys (rebindable in the settings window):
//!   F9   start / stop recording
//!   F10  show / hide the overlay
//!   F8   settings
//!   F11  quit

// A GUI app: no console window. (When started from a terminal we attach to it, so --help and
// the status messages still print there; see `attach_console`.)
#![windows_subsystem = "windows"]

mod audio;
mod capture;
mod gpuenc;
mod hook_client;
mod main_window;
mod overlay;
mod relaunch;
mod settings;
mod settings_ui;

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Instant;

use capture::{Job, Shared, WM_FRAME};
use hook_client::HookClient;
use overlay::{Renderer, Style};
use settings::{ACTION_NAMES, Settings, key_name};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PWSTR, w};

// hotkey ids are index + 1 into `Settings::keys`
const ID_REC: i32 = 1;
const ID_OVERLAY: i32 = 2;
const ID_SETTINGS: i32 = 3;
const ID_QUIT: i32 = 4;
/// F7: "restart the game in DX11". Registered only while an Unreal game on Direct3D 12 is focused.
const ID_DX11: i32 = 5;
const TIMER: usize = 1;
/// WDA_EXCLUDEFROMCAPTURE: the window is visible on screen but absent from captures.
const WDA_EXCLUDEFROMCAPTURE: u32 = 0x11;

#[derive(Clone, PartialEq)]
struct Target {
    hwnd: isize,
    pid: u32,
    rect: (i32, i32, i32, i32),
    name: String,
}

struct Config {
    fps: u32,
    bitrate: u32,
    out_dir: std::path::PathBuf,
    overlay: bool,
    /// Start with the control window minimized to the taskbar (used by the start-with-Windows shortcut).
    start_minimized: bool,
}

struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    bits: *mut u8,
    w: u32,
    h: u32,
}

pub(crate) struct App {
    cfg: Config,
    pub(crate) settings: Settings,
    hints: Vec<String>,
    mw: main_window::MainWindow,
    /// Last thing worth telling the user (file saved, error, hook status), shown in the window.
    note: String,
    last_hook_status: String,
    /// A true exclusive-fullscreen D3D app is running: no window can draw over it.
    exclusive: bool,
    hook: HookClient,
    last_excl_check: Instant,
    last_topmost: Instant,
    renderer: Renderer,
    shared: Arc<Shared>,
    hwnd: HWND,
    dib: Option<Dib>,
    target: Option<Target>,
    locked: bool,
    worker: Option<JoinHandle<()>>,
    t0: Instant,
    last_key: Option<(u32, bool, Option<Target>, i32, bool, u64)>,
    /// Process we last checked for "Unreal game on Direct3D 12", when, and the restart plan if so.
    dx12_pid: u32,
    dx12_checked: Instant,
    dx12_plan: Option<relaunch::Plan>,
    /// The F7 hotkey for the restart is currently registered (and advertised in the overlay).
    dx11_hotkey: bool,
    dx11_wanted: bool,
}

thread_local! { pub(crate) static APP: RefCell<Option<App>> = const { RefCell::new(None) }; }

/// A GUI-subsystem exe has no console of its own. If we were started from a terminal, hook
/// stdout/stderr up to it (unless they were already redirected), so command-line use still works.
fn attach_console() {
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows::Win32::System::Console::*;
    unsafe {
        let valid = |h: windows::core::Result<HANDLE>| h.is_ok_and(|h| !h.is_invalid() && !h.0.is_null());
        if valid(GetStdHandle(STD_OUTPUT_HANDLE)) {
            return; // already redirected/inherited
        }
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return; // launched from Explorer/Start: no console, and that's the point
        }
        for std in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            if let Ok(f) = CreateFileW(w!("CONOUT$"), 0x4000_0000, FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None) {
                let _ = SetStdHandle(std, f);
            }
        }
    }
}

fn main() {
    attach_console();
    let cfg = parse_args();
    let renderer = match Renderer::new() {
        Ok(r) => r,
        Err(e) => return eprintln!("error: {e}"),
    };
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let hinst = GetModuleHandleW(None).unwrap();
        let class = w!("SnapCapOverlay");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        });
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!("SnapCap"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .expect("create overlay window");

        let shared = Arc::new(Shared::default());
        {
            let (s, h) = (shared.clone(), hwnd.0 as isize);
            std::thread::spawn(move || capture::monitor(s, h));
        }
        let settings = Settings::load();
        let mw = main_window::create(cfg.start_minimized);
        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                mw,
                note: String::new(),
                last_hook_status: String::new(),
                cfg,
                settings,
                hints: Vec::new(),
                exclusive: false,
                hook: HookClient::new(),
                last_excl_check: Instant::now(),
                last_topmost: Instant::now(),
                renderer,
                shared,
                hwnd,
                dib: None,
                target: None,
                locked: false,
                worker: None,
                t0: Instant::now(),
                last_key: None,
                dx12_pid: 0,
                dx12_checked: Instant::now(),
                dx12_plan: None,
                dx11_hotkey: false,
                dx11_wanted: false,
            })
        });
        SetTimer(Some(hwnd), TIMER, 66, None); // ~15 Hz is plenty for status/overlay upkeep

        APP.with(|a| {
            if let Some(app) = a.borrow_mut().as_mut() {
                app.update_hints();
                app.apply_visuals();
                app.register_keys();
                println!("SnapCap running.  {}", app.hints.join("   "));
            }
        });

        if std::env::args().any(|a| a == "--selftest") {
            let h = hwnd.0 as isize;
            std::thread::spawn(move || {
                let press = |id: i32| {
                    let _ = PostMessageW(Some(HWND(h as _)), WM_HOTKEY, WPARAM(id as usize), LPARAM(0));
                };
                std::thread::sleep(std::time::Duration::from_secs(2));
                press(ID_REC);
                std::thread::sleep(std::time::Duration::from_secs(4));
                press(ID_REC);
                std::thread::sleep(std::time::Duration::from_secs(2));
                press(ID_QUIT);
            });
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = DispatchMessageW(&msg);
        }
    }
}

fn parse_args() -> Config {
    let mut cfg = Config {
        fps: 60,
        bitrate: 16,
        out_dir: std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into()))
            .join("Videos")
            .join("SnapCap"),
        overlay: true,
        start_minimized: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fps" => cfg.fps = args.next().and_then(|v| v.parse().ok()).unwrap_or(60).clamp(1, 240),
            "--bitrate" => cfg.bitrate = args.next().and_then(|v| v.parse().ok()).unwrap_or(16).clamp(1, 200),
            "--out" => {
                if let Some(v) = args.next() {
                    cfg.out_dir = v.into()
                }
            }
            "--no-overlay" => cfg.overlay = false,
            "--minimized" => cfg.start_minimized = true,
            "--selftest" => {}
            _ => {
                println!(
                    "SnapCap - featherweight recorder\n\n  --fps N        capture rate (default 60)\n  --bitrate N    Mbps (default 16)\n  --out DIR      output folder (default %USERPROFILE%\\Videos\\SnapCap)\n  --no-overlay   start with the overlay hidden\n  --minimized    start minimized to the taskbar\n\nDefault hotkeys: F9 record/stop, F10 overlay, F8 settings, F11 quit.\nPress the settings key to change font sizes and keybinds."
                );
                std::process::exit(0);
            }
        }
    }
    cfg
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER | WM_FRAME => {
            APP.with(|a| {
                if let Some(app) = a.borrow_mut().as_mut() {
                    unsafe { app.tick() }
                }
            });
            LRESULT(0)
        }
        WM_HOTKEY if wp.0 as i32 == ID_DX11 => {
            // outside the APP borrow: the confirmation box runs a nested message loop
            unsafe { main_window::confirm_restart(None) };
            LRESULT(0)
        }
        WM_HOTKEY => {
            APP.with(|a| {
                if let Some(app) = a.borrow_mut().as_mut() {
                    unsafe {
                        match wp.0 as i32 {
                            ID_REC => app.toggle_record(),
                            ID_OVERLAY => {
                                app.cfg.overlay = !app.cfg.overlay;
                                app.apply_visuals();
                            }
                            ID_SETTINGS => app.open_settings(),
                            ID_QUIT => app.quit(),
                            _ => {}
                        }
                    }
                }
            });
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

impl App {
    fn recording(&self) -> bool {
        self.shared.recording.load(Ordering::Relaxed) && !self.shared.stop.load(Ordering::Relaxed)
    }

    /// Stops any recording cleanly (so the file is finished and playable), then exits.
    fn quit(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        unsafe { PostQuitMessage(0) };
    }

    fn update_hints(&mut self) {
        self.hints = (0..4)
            .map(|i| format!("{}: {}", key_name(self.settings.keys[i]), ACTION_NAMES[i]))
            .collect();
    }

    unsafe fn unregister_all(&self) {
        unsafe {
            for id in 1..=4 {
                let _ = UnregisterHotKey(Some(self.hwnd), id);
            }
        }
    }

    /// (Re)registers every global hotkey; returns the names of actions that failed.
    unsafe fn register_now(&self) -> Vec<&'static str> {
        unsafe {
            self.unregister_all();
            let mut failed = Vec::new();
            for (i, k) in self.settings.keys.iter().enumerate() {
                if RegisterHotKey(Some(self.hwnd), i as i32 + 1, HOT_KEY_MODIFIERS(k.mods) | MOD_NOREPEAT, k.vk)
                    .is_err()
                {
                    failed.push(ACTION_NAMES[i]);
                }
            }
            failed
        }
    }

    /// Normal operation: hotkeys live. (While the settings window is open they're
    /// released, otherwise Windows would swallow the keys you're trying to bind.)
    pub(crate) fn register_keys(&mut self) {
        if settings_ui::is_open() {
            return;
        }
        let failed = unsafe { self.register_now() };
        for f in failed {
            eprintln!("warning: couldn't register the \"{f}\" hotkey (in use by another app?)");
        }
    }

    pub(crate) fn set_keys(&mut self, keys: [settings::Key; 4]) -> Result<(), String> {
        if let Some(e) = Settings::validate_keys(&keys) {
            return Err(e);
        }
        let old = self.settings.keys;
        self.settings.keys = keys;
        let failed = unsafe { self.register_now() };
        if !failed.is_empty() {
            self.settings.keys = old;
            unsafe { self.unregister_all() };
            return Err(format!("Couldn't bind: {} (that key combo is already used by another app).", failed.join(", ")));
        }
        self.update_hints();
        self.last_key = None;
        Ok(())
    }

    /// Font sizes / capture-visibility changed: apply and redraw.
    pub(crate) fn apply_visuals(&mut self) {
        unsafe {
            // The live window is never part of a capture; if the user wants the overlay in
            // recordings, the recorder draws it into the frames itself (works in exclusive
            // fullscreen too, where this window can't be shown at all).
            let _ = SetWindowDisplayAffinity(self.hwnd, WINDOW_DISPLAY_AFFINITY(WDA_EXCLUDEFROMCAPTURE));
            let s = &self.shared;
            s.burn.store(self.settings.overlay_in_rec && self.cfg.overlay, Ordering::Relaxed);
            s.counter_pct.store(self.settings.counter_pct, Ordering::Relaxed);
            s.label_pct.store(self.settings.label_pct, Ordering::Relaxed);
            self.last_key = None;
            self.tick();
        }
    }

    /// Polls whether a true exclusive-fullscreen Direct3D app is running.
    unsafe fn check_exclusive(&mut self) {
        unsafe {
            if self.last_excl_check.elapsed().as_millis() < 500 {
                return;
            }
            self.last_excl_check = Instant::now();
            let now = windows::Win32::UI::Shell::SHQueryUserNotificationState()
                .is_ok_and(|s| s == windows::Win32::UI::Shell::QUNS_RUNNING_D3D_FULL_SCREEN);
            if now != self.exclusive {
                self.exclusive = now;
                if now {
                    if self.settings.hook_enabled {
                        eprintln!("exclusive fullscreen detected: using the in-game overlay hook (Direct3D 11 only).");
                    } else {
                        eprintln!(
                            "exclusive fullscreen detected: Windows won't show a normal overlay window over it. \
                             Tick \"In-game overlay for exclusive fullscreen\" in settings (F8) to draw it inside \
                             the game, or use Borderless in the game."
                        );
                    }
                }
            }
        }
    }

    unsafe fn open_settings(&mut self) {
        unsafe {
            self.unregister_all();
            settings_ui::open(&self.settings);
        }
    }

    unsafe fn toggle_record(&mut self) {
        unsafe {
            if self.shared.recording.load(Ordering::SeqCst) {
                self.shared.stop.store(true, Ordering::SeqCst);
                println!("stopping...");
                return;
            }
            if self.worker.is_some() {
                return; // previous take is still finalizing
            }
            let Some(t) = self.target.clone() else {
                self.note = "Nothing to record: no window in focus.".into();
                return println!("nothing to record (no focused window)");
            };
            let _ = std::fs::create_dir_all(&self.cfg.out_dir);
            let stamp = timestamp();
            let safe: String = t
                .name
                .chars()
                .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                .collect();
            let path = self.cfg.out_dir.join(format!("{safe}_{stamp}.mp4"));
            println!("recording {} -> {}", t.name, path.display());

            self.locked = true;
            self.shared.stop.store(false, Ordering::SeqCst);
            self.shared.frame.store(0, Ordering::SeqCst);
            self.shared.recording.store(true, Ordering::SeqCst);
            let job = Job {
                rect: RECT { left: t.rect.0, top: t.rect.1, right: t.rect.2, bottom: t.rect.3 },
                fps: self.cfg.fps,
                bitrate_mbps: self.cfg.bitrate,
                path,
                overlay: self.hwnd.0 as isize,
                pid: t.pid,
                name: t.name.clone(),
                hook_pid: if self.settings.hook_enabled && t.pid != 0 && self.hook.alive(t.pid) { t.pid } else { 0 },
                gpu: self.settings.gpu_encode,
            };
            let shared = self.shared.clone();
            self.worker = Some(std::thread::spawn(move || capture::run(job, shared)));
            self.last_key = None;
            self.tick();
        }
    }

    /// Name of the game the "Restart in DX11" button would act on (None = nothing to do now).
    fn dx11_prompt(&self) -> Option<String> {
        if self.recording() {
            return None;
        }
        self.dx12_plan.as_ref().map(|p| p.name.clone())
    }

    /// Closes the game and starts it again with `-dx11` (off the UI thread: it waits for exit).
    fn restart_dx11(&mut self) {
        let (Some(plan), Some(t)) = (self.dx12_plan.take(), self.target.clone()) else { return };
        self.note = format!("Restarting {} with -dx11\u{2026}", plan.name);
        let shared = self.shared.clone();
        std::thread::spawn(move || {
            let msg = match relaunch::restart(&plan, t.hwnd) {
                Ok(()) => format!("Restarted {} with -dx11. Focus it to enable the overlay.", plan.name),
                Err(e) => format!("Restart failed: {e}"),
            };
            if let Ok(mut n) = shared.note.lock() {
                *n = Some(msg);
            }
        });
    }

    /// Spots Unreal games running Direct3D 12 and shows/hides the "Restart in DX11" button.
    unsafe fn update_dx11_button(&mut self, pid: u32) {
        unsafe {
            if pid == 0 || self.recording() {
                self.dx12_pid = 0;
                if pid == 0 {
                    self.dx12_plan = None;
                }
            } else if self.dx12_pid != pid || (self.dx12_plan.is_none() && self.dx12_checked.elapsed().as_secs() >= 5) {
                // modules load shortly after launch, so a "no" is re-checked every few seconds
                let first = self.dx12_pid != pid;
                self.dx12_pid = pid;
                self.dx12_checked = Instant::now();
                self.dx12_plan = if self.hook.dx12_only(pid) { relaunch::plan(pid) } else { None };
                if first && self.dx12_plan.is_some() {
                    self.note = "This game renders with Direct3D 12, so the in-game overlay can't be used. \
                                 'Restart game in DX11' relaunches it with -dx11."
                        .into();
                }
            }
            let want = self.dx12_plan.is_some() && !self.recording();
            self.mw.set_dx11(want);
            if want != self.dx11_wanted {
                self.dx11_wanted = want;
                self.dx11_hotkey = if want {
                    // The game has focus and covers our window, so offer the restart in the
                    // overlay with a hotkey. (If F7 is taken, the window's button still works.)
                    RegisterHotKey(Some(self.hwnd), ID_DX11, MOD_NOREPEAT, VK_F7.0 as u32).is_ok()
                } else {
                    let _ = UnregisterHotKey(Some(self.hwnd), ID_DX11);
                    false
                };
                self.last_key = None; // redraw the overlay with/without the hint
            }
        }
    }

    unsafe fn tick(&mut self) {
        unsafe {
            // reap a finished worker
            if self.worker.as_ref().is_some_and(|h| h.is_finished()) {
                let _ = self.worker.take().unwrap().join();
                self.shared.stop.store(false, Ordering::SeqCst);
                self.locked = false;
            }
            self.check_exclusive();
            // Messages for the control window: a finished/failed recording, or why the hook wasn't used.
            if let Some(n) = self.shared.note.lock().ok().and_then(|mut n| n.take()) {
                self.note = n;
            }
            if self.hook.status != self.last_hook_status {
                self.last_hook_status = self.hook.status.clone();
                // Routine refusals (a browser or other non-game has focus) aren't worth a message.
                if self.last_hook_status.starts_with("hook not injected") && !self.last_hook_status.contains("never injected") {
                    self.note = format!("In-game hook: {}", self.last_hook_status.trim_start_matches("hook not injected: "));
                }
            }
            // Games often grab HWND_TOPMOST themselves; keep re-asserting ours so the
            // overlay doesn't end up underneath a fullscreen-optimized game window.
            // (Throttled: tick() also runs for every recorded frame, and re-asserting z-order
            // that often makes DWM churn over the game and slows it down.)
            if self.cfg.overlay && self.target.is_some() && self.last_topmost.elapsed().as_millis() >= 500 {
                self.last_topmost = Instant::now();
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            if !self.locked {
                if let Some(t) = foreground_target(self.hwnd) {
                    self.target = Some(t);
                }
            }
            self.update_dx11_button(self.target.as_ref().map_or(0, |t| t.pid));
            let Some(target) = self.target.clone() else {
                let hints = self.hints.join("  \u{00B7}  ");
                let (rec, elapsed) = (self.recording(), self.shared.elapsed.load(Ordering::Relaxed));
                self.mw.update(&main_window::Status { recording: rec, elapsed, name: "", fps: 0, note: &self.note, hints: &hints });
                return;
            };

            // In-game hook (opt-in, exclusive fullscreen D3D11 only): engage it, keep it
            // alive, and use the game's real Present rate for the FPS readout.
            let limit = self.settings.fps_limit;
            let overlay_hook = self.settings.hook_enabled;
            let mut hook_alive = false;
            let mut game_fps10 = 0;
            if target.pid != 0 && (overlay_hook || limit > 0 || self.hook.has(target.pid)) {
                // Load the hook for the overlay (exclusive fullscreen only) or to cap frames
                // (any time a limit is set).
                if (overlay_hook && self.exclusive) || limit > 0 {
                    self.hook.ensure(target.pid);
                }
                self.hook.heartbeat(target.pid);
                hook_alive = self.hook.alive(target.pid);
                if hook_alive {
                    game_fps10 = self.hook.fps10(target.pid);
                }
            }
            // The cap applies to whatever is focused; any other hooked app runs unlimited.
            self.hook.apply_limit(target.pid, if hook_alive { limit } else { 0 });
            // Only draw the overlay through the hook if the user asked for that.
            let hook_live = hook_alive && overlay_hook;
            if hook_alive && self.last_excl_check.elapsed().as_millis() < 20 && std::env::var_os("SNAPCAP_STATS").is_some() {
                eprintln!("game {} measured {:.1} fps (limit {})", target.name, game_fps10 as f32 / 10.0, limit);
            }
            self.shared.hook_fps10.store(game_fps10, Ordering::Relaxed);

            let rec = self.recording();
            let time = self.t0.elapsed().as_secs_f32();
            let f10 = if game_fps10 > 0 { game_fps10 } else { self.shared.fps10.load(Ordering::Relaxed) };
            let fps = (f10 + 5) / 10;
            let elapsed = if rec { self.shared.elapsed.load(Ordering::Relaxed) } else { 0 };
            {
                let hints = self.hints.join("  \u{00B7}  ");
                self.mw.update(&main_window::Status { recording: rec, elapsed, name: &target.name, fps, note: &self.note, hints: &hints });
            }
            self.shared.watch_x.store((target.rect.0 + target.rect.2) / 2, Ordering::Relaxed);
            self.shared.watch_y.store((target.rect.1 + target.rect.3) / 2, Ordering::Relaxed);
            let pulse_bucket = if rec { (time * 15.0) as i32 } else { 0 };
            let key = (fps, rec, Some(target.clone()), pulse_bucket, self.cfg.overlay, elapsed);
            if self.last_key.as_ref() == Some(&key) {
                return;
            }
            self.last_key = Some(key);

            if !self.cfg.overlay {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
                if hook_live {
                    self.hook.set_enabled(target.pid, false);
                }
                return;
            }

            let height = (target.rect.3 - target.rect.1) as f32;
            let scale = ((height / 1080.0).clamp(0.6, 2.0) * 20.0).round() / 20.0;
            let cs = self.settings.counter_pct as f32 / 100.0;
            let ls = self.settings.label_pct as f32 / 100.0;
            let (cw, ch) = Renderer::canvas(scale, cs.max(ls));
            if self.dib.as_ref().is_none_or(|d| d.w != cw || d.h != ch) {
                self.dib = Some(make_dib(cw, ch));
            }
            let dib = self.dib.as_ref().unwrap();
            // The overlay canvas fits four legend lines: when the DX11 offer is showing, it takes
            // the top line and the last one (Quit) makes room for it.
            let mut hint_lines = self.hints.clone();
            if self.dx11_hotkey {
                hint_lines.truncate(3);
                hint_lines.insert(0, "F7: Restart game in DX11".to_string());
            }
            let style = Style {
                fps,
                elapsed,
                recording: rec,
                counter_scale: cs,
                label_scale: ls,
                hints: &hint_lines,
                name: &target.name,
                time,
            };
            let pm = self.renderer.render(&style, scale);

            // tiny-skia: premultiplied RGBA  ->  layered window: premultiplied BGRA
            let src = pm.data();
            let dst = std::slice::from_raw_parts_mut(dib.bits, src.len());
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = s[3];
            }

            let margin = (18.0 * scale) as i32;
            if hook_live {
                let bytes = std::slice::from_raw_parts(dib.bits, (cw * ch * 4) as usize);
                self.hook.publish(target.pid, bytes, cw, ch, margin, margin);
                // Once the hook is really drawing inside the game, our own window is redundant.
                // Until then (or if the hook can't draw) keep showing the normal overlay.
                if self.hook.drawing(target.pid) {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                    return;
                }
            }
            let pos = POINT { x: target.rect.0 + margin, y: target.rect.1 + margin };
            let size = SIZE { cx: cw as i32, cy: ch as i32 };
            let src_pt = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&pos),
                Some(&size),
                Some(dib.dc),
                Some(&src_pt),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }
}

unsafe fn make_dib(w: u32, h: u32) -> Dib {
    unsafe {
        let dc = CreateCompatibleDC(None);
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bmp = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
        SelectObject(dc, bmp.into());
        Dib { dc, bmp, bits: bits as *mut u8, w, h }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.dc);
            let _ = DeleteObject(self.bmp.into());
        }
    }
}

/// The window we should be recording: whatever is focused (not ourselves).
unsafe fn foreground_target(own: HWND) -> Option<Target> {
    unsafe {
        let focused = GetForegroundWindow();
        let mut fpid = 0u32;
        if !focused.0.is_null() {
            GetWindowThreadProcessId(focused, Some(&mut fpid));
        }
        if focused == own || (fpid != 0 && fpid == GetCurrentProcessId()) {
            return None; // our own settings window: keep the previous target
        }
        // What the user is actually looking at beats what has keyboard focus: if a
        // fullscreen window is on top of the focused one, that's the target.
        let fg = find_fullscreen_above(focused).unwrap_or(focused);
        if fg.0.is_null() || IsIconic(fg).as_bool() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        let mut cls = [0u16; 64];
        let n = GetClassNameW(fg, &mut cls) as usize;
        let class = String::from_utf16_lossy(&cls[..n]);
        let desktop = matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd");

        let mut r = RECT::default();
        if desktop {
            let mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            if GetMonitorInfoW(mon, &mut mi).as_bool() {
                r = mi.rcMonitor;
            }
        } else if DwmGetWindowAttribute(
            fg,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut _ as *mut _,
            size_of::<RECT>() as u32,
        )
        .is_err()
        {
            GetWindowRect(fg, &mut r).ok()?;
        }
        if r.right - r.left < 64 || r.bottom - r.top < 64 {
            return None;
        }
        let name = if desktop { "Desktop".to_string() } else { process_name(pid) };
        // the desktop shell's audio is not "the app", so don't capture explorer.exe
        let pid = if desktop { 0 } else { pid };
        Some(Target { hwnd: fg.0 as isize, pid, rect: (r.left, r.top, r.right, r.bottom), name })
    }
}

struct Scan {
    stop: HWND,
    own_pid: u32,
    found: Option<HWND>,
}

unsafe extern "system" fn scan_cb(h: HWND, lp: LPARAM) -> BOOL {
    unsafe {
        let s = &mut *(lp.0 as *mut Scan);
        if h == s.stop {
            return FALSE; // reached the focused window: nothing fullscreen above it
        }
        if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            return TRUE;
        }
        // skip click-through / tool / no-activate windows: other apps' overlays
        // (GPU overlays, Game Bar, our own) often span the whole monitor
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        if ex & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0 | WS_EX_TRANSPARENT.0) != 0 {
            return TRUE;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
        if cloaked != 0 {
            return TRUE;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == s.own_pid {
            return TRUE;
        }
        let mut cls = [0u16; 64];
        let n = GetClassNameW(h, &mut cls) as usize;
        let class = String::from_utf16_lossy(&cls[..n]);
        if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
            return TRUE;
        }
        let mut r = RECT::default();
        if GetWindowRect(h, &mut r).is_err() {
            return TRUE;
        }
        let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            let m = mi.rcMonitor;
            if r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom {
                s.found = Some(h);
                return FALSE;
            }
        }
        TRUE
    }
}

/// Topmost fullscreen window that sits above `focused` in z-order (EnumWindows walks
/// top to bottom, so we stop as soon as we reach the focused window).
unsafe fn find_fullscreen_above(focused: HWND) -> Option<HWND> {
    unsafe {
        let mut s = Scan { stop: focused, own_pid: GetCurrentProcessId(), found: None };
        let _ = EnumWindows(Some(scan_cb), LPARAM(&mut s as *mut Scan as isize));
        s.found
    }
}

unsafe fn process_name(pid: u32) -> String {
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let mut buf = [0u16; 520];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
            let _ = CloseHandle(h);
            if ok.is_ok() {
                let p = String::from_utf16_lossy(&buf[..len as usize]);
                if let Some(stem) = std::path::Path::new(&p).file_stem() {
                    return stem.to_string_lossy().into_owned();
                }
            }
        }
        "Unknown".into()
    }
}

fn timestamp() -> String {
    // local-ish timestamp without pulling in a date crate
    let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}
