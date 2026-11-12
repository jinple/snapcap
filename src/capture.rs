//! Screen capture (DXGI Desktop Duplication) -> H.264/MP4 (Media Foundation).
//! Captures the desktop region of the target window so the overlay is baked in.
//! No external dependencies (no ffmpeg); uses the GPU encoder when available.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HANDLE, HMODULE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::System::Threading::{CreateWaitableTimerExW, INFINITE, SetWaitableTimer, WaitForSingleObject};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::{Interface, PCWSTR, Result};

use crate::overlay::Style;

pub const WM_FRAME: u32 = 0x8000 + 1; // WM_APP + 1

#[derive(Default)]
pub struct Shared {
    pub recording: AtomicBool,
    pub stop: AtomicBool,
    /// Latest message for the control window (set when a recording finishes or fails).
    pub note: std::sync::Mutex<Option<String>>,
    /// Draw the overlay into recorded frames ("Show overlay in recordings").
    pub burn: AtomicBool,
    /// Overlay font-size percentages (from settings), used when burning in.
    pub counter_pct: AtomicU32,
    pub label_pct: AtomicU32,
    /// Measured on-screen update rate of the target's monitor, in FPS * 10.
    pub fps10: AtomicU32,
    /// Real game FPS * 10 from the in-game hook (0 = hook not running).
    pub hook_fps10: AtomicU32,
    /// Whole seconds recorded so far (wall clock).
    pub elapsed: AtomicU64,
    /// Point (screen coords) on the monitor being watched.
    pub watch_x: AtomicI32,
    pub watch_y: AtomicI32,
    /// Index the *next* captured frame will have. The overlay shows this number,
    /// so the frame with index N has "N" burned into it.
    pub frame: AtomicU64,
}

pub struct Job {
    pub rect: RECT,
    pub fps: u32,
    pub bitrate_mbps: u32,
    pub path: std::path::PathBuf,
    pub overlay: isize,
    /// Process whose audio to capture (0 = none, e.g. the desktop).
    pub pid: u32,
    /// App name shown in the burned-in overlay.
    pub name: String,
    /// Game process whose in-game hook should supply overlay-free frames (0 = use the
    /// screen duplication instead).
    pub hook_pid: u32,
    /// Encode on the GPU (the "Encode recordings on the GPU" setting). Falls back to the CPU
    /// path by itself if the GPU setup isn't supported.
    pub gpu: bool,
}

use crate::hook_client::protocol::{Header, MAGIC, MAP_SIZE, frame_name, map_name};
use windows::Win32::System::Memory::{FILE_MAP_ALL_ACCESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile};

/// Frames delivered by the in-game hook: a copy of the game's back buffer taken *before*
/// the overlay is drawn, so the recording can omit the overlay even though it's visible live.
struct HookSrc {
    hdr: *mut Header,
    tex: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    w: u32,
    h: u32,
    fmt: DXGI_FORMAT,
    /// back buffer is R8G8B8A8; the encoder wants BGRA, so swap on the CPU
    rgba: bool,
    /// back buffer is R10G10B10A2 (10-bit); convert to 8-bit BGRA on the CPU
    r10: bool,
}

impl Drop for HookSrc {
    fn drop(&mut self) {
        unsafe {
            std::ptr::write_volatile(&raw mut (*self.hdr).want_capture, 0);
            let _ = UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: self.hdr as *mut _ });
        }
    }
}

unsafe fn acquire_key(m: &IDXGIKeyedMutex, ms: u32) -> bool {
    unsafe { (Interface::vtable(m).AcquireSync)(Interface::as_raw(m), 0, ms) == windows::core::HRESULT(0) }
}

unsafe fn open_hook_src(device: &ID3D11Device, pid: u32) -> std::result::Result<HookSrc, String> {
    unsafe {
        let name: Vec<u16> = map_name(pid).encode_utf16().chain(Some(0)).collect();
        let h = OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(name.as_ptr())).map_err(|e| format!("no hook mapping ({e})"))?;
        let view = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, MAP_SIZE);
        if view.Value.is_null() {
            return Err("can't map the hook block".into());
        }
        let hdr = view.Value as *mut Header;
        if std::ptr::read_volatile(&raw const (*hdr).magic) != MAGIC {
            return Err("bad hook block".into());
        }
        std::ptr::write_volatile(&raw mut (*hdr).want_capture, 1);
        std::ptr::write_volatile(&raw mut (*hdr).want_frame, 1);
        // wait for the hook to deliver its first clean frame
        let t = Instant::now();
        while std::ptr::read_volatile(&raw const (*hdr).frame_seq) == 0 {
            if t.elapsed() > Duration::from_millis(2500) {
                std::ptr::write_volatile(&raw mut (*hdr).want_capture, 0);
                return Err("hook delivered no frames".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let dev1: ID3D11Device1 = device.cast().map_err(|e| e.to_string())?;
        let fname: Vec<u16> = frame_name(pid).encode_utf16().chain(Some(0)).collect();
        let tex: ID3D11Texture2D = dev1
            .OpenSharedResourceByName(PCWSTR(fname.as_ptr()), 0x8000_0000)
            .map_err(|e| format!("can't open the shared frame ({e})"))?;
        let mutex: IDXGIKeyedMutex = tex.cast().map_err(|e| e.to_string())?;
        let mut d = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut d);
        let rgba = matches!(
            d.Format,
            DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB | DXGI_FORMAT_R8G8B8A8_TYPELESS
        );
        let bgra = matches!(
            d.Format,
            DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB | DXGI_FORMAT_B8G8R8A8_TYPELESS
        );
        let r10 = matches!(d.Format, DXGI_FORMAT_R10G10B10A2_UNORM | DXGI_FORMAT_R10G10B10A2_TYPELESS);
        if !rgba && !bgra && !r10 {
            std::ptr::write_volatile(&raw mut (*hdr).want_capture, 0);
            return Err(format!("unsupported back buffer format {}", d.Format.0));
        }
        Ok(HookSrc { hdr, tex, mutex, w: d.Width, h: d.Height, fmt: d.Format, rgba, r10 })
    }
}

thread_local! { static STAGE: std::cell::Cell<&'static str> = const { std::cell::Cell::new("starting") }; }

/// Remembers what the recorder is doing, so a failure says *where* it happened.
fn stage(s: &'static str) {
    STAGE.set(s);
}

pub fn run(job: Job, shared: Arc<Shared>) {
    let res = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
        timeBeginPeriod(1);
        let r = record(&job, &shared);
        timeEndPeriod(1);
        let _ = MFShutdown();
        CoUninitialize();
        r
    };
    match res {
        Ok(n) => {
            println!("saved {n} frames -> {}", job.path.display());
            let file = job.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            if let Ok(mut note) = shared.note.lock() {
                *note = Some(format!("Saved {file} ({n} frames)"));
            }
        }
        Err(e) => {
            eprintln!("recording failed while {}: {e}", STAGE.get());
            if let Ok(mut note) = shared.note.lock() {
                *note = Some(format!("Recording failed while {}: {e}", STAGE.get()));
            }
        }
    }
    shared.recording.store(false, Ordering::SeqCst);
    notify(&job);
}

/// High-resolution sleeper: coarse wait on a hi-res waitable timer, then spin.
/// (`thread::sleep` can round up to ~15.6ms on Windows 11, which cost us frames.)
struct Waiter(HANDLE);

impl Waiter {
    fn new() -> Self {
        // CREATE_WAITABLE_TIMER_HIGH_RESOLUTION = 2, TIMER_ALL_ACCESS = 0x1F0003
        let h = unsafe { CreateWaitableTimerExW(None, windows::core::PCWSTR::null(), 2, 0x1F0003) };
        Self(h.unwrap_or_default())
    }

    fn until(&self, t: Instant) {
        loop {
            let now = Instant::now();
            if now >= t {
                return;
            }
            let left = t - now;
            if left > Duration::from_micros(1500) {
                let coarse = left - Duration::from_micros(1000);
                if self.0.is_invalid() {
                    std::thread::sleep(coarse);
                } else {
                    let due = -((coarse.as_nanos() / 100) as i64);
                    unsafe {
                        if SetWaitableTimer(self.0, &due, 0, None, None, false).is_ok() {
                            WaitForSingleObject(self.0, INFINITE);
                        }
                    }
                }
            } else {
                std::hint::spin_loop();
            }
        }
    }
}

/// Counts desktop updates (= frames the app presented, capped by refresh rate)
/// and publishes a rate twice a second.
struct Meter {
    acc: u32,
    since: Instant,
}

impl Meter {
    fn new() -> Self {
        Self { acc: 0, since: Instant::now() }
    }
    fn add(&mut self, n: u32, _s: &Shared, _o: isize) {
        self.acc += n;
    }
    fn tick(&mut self, s: &Shared, overlay: isize) {
        let dt = self.since.elapsed();
        if dt >= Duration::from_millis(500) {
            let fps10 = (self.acc as f64 / dt.as_secs_f64() * 10.0).round() as u32;
            s.fps10.store(fps10, Ordering::Relaxed);
            self.acc = 0;
            self.since = Instant::now();
            let _ = overlay;
        }
    }
}

/// Idle-time watcher: duplicates the target's monitor (no pixel copies) just to
/// count how often the screen updates, so the overlay can show a live FPS.
pub fn monitor(shared: Arc<Shared>, overlay: isize) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut cur: Option<(RECT, ID3D11Device, IDXGIOutputDuplication)> = None;
        let mut meter = Meter::new();
        loop {
            if shared.recording.load(Ordering::Relaxed) {
                cur = None; // the recorder takes over metering
                std::thread::sleep(Duration::from_millis(100));
                meter = Meter::new();
                continue;
            }
            // The in-game hook already reports the game's exact FPS, so don't duplicate the
            // screen (which makes DWM copy every frame of the game) just to estimate it.
            if shared.hook_fps10.load(Ordering::Relaxed) > 0 {
                cur = None;
                std::thread::sleep(Duration::from_millis(250));
                meter = Meter::new();
                continue;
            }
            let (x, y) = (shared.watch_x.load(Ordering::Relaxed), shared.watch_y.load(Ordering::Relaxed));
            let wanted = find_output(x, y);
            let same = match (&cur, &wanted) {
                (Some((r, _, _)), Ok((_, _, w))) => {
                    (r.left, r.top, r.right, r.bottom) == (w.left, w.top, w.right, w.bottom)
                }
                _ => false,
            };
            if !same {
                cur = None;
                let Ok((adapter, output, rect)) = wanted else {
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                };
                let mut device = None;
                let ok = D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_FLAG(0),
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                )
                .is_ok();
                match (ok, device) {
                    (true, Some(dev)) => match duplicate(&output, &dev) {
                        Ok(d) => cur = Some((rect, dev, d)),
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                    },
                    _ => {
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                }
                meter = Meter::new();
            }
            let dup = &cur.as_ref().unwrap().2;
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match dup.AcquireNextFrame(100, &mut info, &mut res) {
                Ok(()) => {
                    meter.add(info.AccumulatedFrames, &shared, overlay);
                    let _ = dup.ReleaseFrame();
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
                Err(_) => {
                    cur = None;
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            let before = shared.fps10.load(Ordering::Relaxed);
            meter.tick(&shared, overlay);
            if shared.fps10.load(Ordering::Relaxed) != before {
                notify_raw(overlay);
            }
        }
    }
}

fn notify_raw(overlay: isize) {
    unsafe {
        let _ = PostMessageW(Some(HWND(overlay as _)), WM_FRAME, WPARAM(0), LPARAM(0));
    }
}

fn notify(job: &Job) {
    unsafe {
        let _ = PostMessageW(Some(HWND(job.overlay as _)), WM_FRAME, WPARAM(0), LPARAM(0));
    }
}

unsafe fn find_output(cx: i32, cy: i32) -> Result<(IDXGIAdapter1, IDXGIOutput, RECT)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut first = None;
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            ai += 1;
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                oi += 1;
                let r = output.GetDesc()?.DesktopCoordinates;
                if first.is_none() {
                    first = Some((adapter.clone(), output.clone(), r));
                }
                if cx >= r.left && cx < r.right && cy >= r.top && cy < r.bottom {
                    return Ok((adapter, output, r));
                }
            }
        }
        first.ok_or_else(|| windows::core::Error::from(DXGI_ERROR_NOT_FOUND))
    }
}

unsafe fn duplicate(output: &IDXGIOutput, device: &ID3D11Device) -> Result<IDXGIOutputDuplication> {
    unsafe { output.cast::<IDXGIOutput1>()?.DuplicateOutput(device) }
}

unsafe fn record(job: &Job, shared: &Shared) -> Result<u64> {
    unsafe {
        let r = job.rect;
        stage("finding the monitor");
        let (adapter, output, out) = find_output((r.left + r.right) / 2, (r.top + r.bottom) / 2)?;
        stage("creating the capture device");

        let mut device = None;
        let mut ctx = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
        let device = device.unwrap();
        let ctx = ctx.unwrap();

        // Prefer overlay-free frames straight from the game (in-game hook), if it's running.
        stage("opening the in-game frame source");
        let hook = if job.hook_pid != 0 {
            match open_hook_src(&device, job.hook_pid) {
                Ok(s) => {
                    eprintln!("recording from the in-game hook ({}x{}, overlay-free)", s.w, s.h);
                    Some(s)
                }
                Err(e) => {
                    eprintln!("in-game capture unavailable ({e}); using screen capture");
                    None
                }
            }
        } else {
            None
        };

        // Screen duplication is only needed when the hook isn't supplying frames; it can fail on
        // its own in exclusive fullscreen, so don't make it a hard requirement in hook mode.
        let (mut dup, mon_w, mon_h) = if hook.is_some() {
            (None, (out.right - out.left) as u32, (out.bottom - out.top) as u32)
        } else {
            stage("duplicating the screen");
            let d = duplicate(&output, &device)?;
            let md = d.GetDesc().ModeDesc;
            (Some(d), md.Width, md.Height)
        };

        // Crop region (relative to the monitor), even-sized for H.264.
        let (x0, y0, w, h) = if let Some(hs) = hook.as_ref() {
            (0, 0, (hs.w & !1).max(2), (hs.h & !1).max(2))
        } else {
            let x0 = (r.left.max(out.left) - out.left).max(0) as u32;
            let y0 = (r.top.max(out.top) - out.top).max(0) as u32;
            let x1 = ((r.right.min(out.right) - out.left).max(0) as u32).min(mon_w);
            let y1 = ((r.bottom.min(out.bottom) - out.top).max(0) as u32).min(mon_h);
            (x0, y0, ((x1.saturating_sub(x0)) & !1).max(2), ((y1.saturating_sub(y0)) & !1).max(2))
        };
        let stage_fmt = hook.as_ref().map(|s| s.fmt).unwrap_or(DXGI_FORMAT_B8G8R8A8_UNORM);

        stage("creating capture textures");
        let tex_desc = |w, h, usage, cpu: D3D11_CPU_ACCESS_FLAG, fmt: DXGI_FORMAT| D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: fmt,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: usage,
            BindFlags: 0,
            CPUAccessFlags: cpu.0 as u32,
            MiscFlags: 0,
        };
        let mut desktop = None;
        let mut desktop_desc = tex_desc(mon_w, mon_h, D3D11_USAGE_DEFAULT, D3D11_CPU_ACCESS_FLAG(0), DXGI_FORMAT_B8G8R8A8_UNORM);
        // the GPU converter reads it (video processor inputs want render-target + shader bind flags)
        desktop_desc.BindFlags = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
        device.CreateTexture2D(&desktop_desc, None, Some(&mut desktop))?;
        let desktop = desktop.unwrap();
        // A small ring of staging textures: each frame is *read* two ticks after its copy was
        // issued, so the CPU never stalls waiting on a GPU that's busy rendering the game.
        const RING: u64 = 3;
        let mut stagings: Vec<ID3D11Texture2D> = Vec::new();
        for _ in 0..RING {
            let mut s = None;
            device.CreateTexture2D(
                &tex_desc(w, h, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ, stage_fmt),
                None,
                Some(&mut s),
            )?;
            stagings.push(s.unwrap());
        }
        let mut issued: u64 = 0;
        let mut cap_t = [Instant::now(); RING as usize];
        let src_box = D3D11_BOX { left: x0, top: y0, front: 0, right: x0 + w, bottom: y0 + h, back: 1 };

        let mut audio = if job.pid != 0 {
            match crate::audio::start(job.pid) {
                Ok(a) => Some(a),
                Err(e) => {
                    eprintln!("audio unavailable ({e}); recording video only");
                    None
                }
            }
        } else {
            None
        };
        // GPU path: the video processor converts frames to NV12 on the GPU and the encoder takes
        // the textures directly (no readback, no CPU conversion). SNAPCAP_CPU=1 forces the old
        // CPU path; it's also the fallback if anything here isn't supported.
        let mut gpu: Option<crate::gpuenc::GpuConv> = None;
        if job.gpu && std::env::var_os("SNAPCAP_CPU").is_none() {
            stage("setting up GPU conversion");
            let src_tex = hook.as_ref().map(|s| &s.tex).unwrap_or(&desktop);
            let src_size = hook.as_ref().map(|s| (s.w, s.h)).unwrap_or((mon_w, mon_h));
            let step = std::cell::Cell::new("creating the video processor");
            let attempt = (|| -> Result<crate::gpuenc::GpuConv> {
                let mut g = crate::gpuenc::GpuConv::new(&device, job.fps, (x0, y0, w, h), src_size)?;
                step.set("reading the source texture");
                g.set_source(src_tex)?;
                // a trial conversion, so an unsupported format falls back to the CPU path up front
                step.set("trial conversion");
                match hook.as_ref() {
                    Some(hs) => {
                        if !acquire_key(&hs.mutex, 200) {
                            return Err(windows::Win32::Foundation::E_FAIL.into());
                        }
                        let r = g.convert();
                        let _ = hs.mutex.ReleaseSync(0);
                        r?;
                    }
                    None => {
                        g.convert()?;
                    }
                }
                Ok(g)
            })();
            match attempt {
                Ok(g) => gpu = Some(g),
                Err(e) => eprintln!("GPU conversion unavailable while {} ({e}); using the CPU path", step.get()),
            }
        }
        if std::env::var_os("SNAPCAP_STATS").is_some() {
            eprintln!("video path: {}", if gpu.is_some() { "GPU (video processor -> NV12)" } else { "CPU (readback + convert)" });
        }
        stage("creating the MP4 encoder");
        let (sink, stream, astream) = create_writer(job, w, h, audio.is_some(), gpu.as_ref().map(|g| &g.mgr))?;
        // shared recording clock: starts only once audio + encoder are ready
        let t0 = Instant::now();
        if let Some(a) = audio.as_ref() {
            a.begin(t0);
        }

        let interval = Duration::from_secs_f64(1.0 / job.fps as f64);
        let sample_dur = 10_000_000i64 / job.fps as i64;
        let row = (w * 4) as usize;
        let mut have_frame = false;
        let mut idx: u64 = 0;
        shared.frame.store(0, Ordering::SeqCst);
        let mut next = Instant::now();
        let mut stats = [0f64; 3];
        let mut slow = 0u32;
        let (mut late_ms, mut late_max, mut acq_ms, mut acq_max) = (0f64, 0f64, 0f64, 0f64);
        let renderer = crate::overlay::Renderer::new().ok();
        let burn_scale = ((h as f32 / 1080.0).clamp(0.6, 2.0) * 20.0).round() / 20.0;
        let waiter = Waiter::new();
        let begun = Instant::now();
        let mut first_at: Option<(Instant, Duration)> = None;
        let mut meter = Meter::new();
        let mut last_notify = Instant::now();
        let mut burn_cache: Option<((u32, u64, u32), tiny_skia::Pixmap)> = None;

        while !shared.stop.load(Ordering::Relaxed) {
            waiter.until(next);
            let t_wake = Instant::now();
            late_ms += (t_wake - next).as_secs_f64() * 1e3;
            late_max = late_max.max((t_wake - next).as_secs_f64() * 1e3);

            // The burned-in overlay picture: re-render only when it changes (~15x/s) and, on the
            // GPU path, upload it only then.
            let burn_on = renderer.is_some() && shared.burn.load(Ordering::Relaxed);
            let mut ovl_changed = false;
            if let (Some(r), true) = (renderer.as_ref(), burn_on) {
                let tt = t0.elapsed();
                let hf = shared.hook_fps10.load(Ordering::Relaxed);
                let fps = ((if hf > 0 { hf } else { shared.fps10.load(Ordering::Relaxed) }) + 5) / 10;
                let style = Style {
                    fps,
                    elapsed: tt.as_secs(),
                    recording: true,
                    counter_scale: shared.counter_pct.load(Ordering::Relaxed) as f32 / 100.0,
                    label_scale: shared.label_pct.load(Ordering::Relaxed) as f32 / 100.0,
                    hints: &[],
                    name: &job.name,
                    time: tt.as_secs_f32(),
                };
                let key = (fps, tt.as_secs(), (tt.as_secs_f32() * 15.0) as u32);
                if burn_cache.as_ref().is_none_or(|(k, _)| *k != key) {
                    burn_cache = Some((key, r.render(&style, burn_scale)));
                    ovl_changed = true;
                }
            }
            if let Some(g) = gpu.as_mut() {
                g.overlay_on = burn_on;
                if let (true, true, Some((_, pm))) = (burn_on, ovl_changed, burn_cache.as_ref()) {
                    let m = (18.0 * burn_scale) as i32;
                    if let Err(e) = g.set_overlay(pm.data(), pm.width(), pm.height(), m, m) {
                        eprintln!("overlay upload failed: {e}");
                    }
                }
            }

            // In-game hook source: grab its latest overlay-free frame.
            let mut issued_now = false;
            let mut gpu_tex: Option<ID3D11Texture2D> = None;
            if let Some(hs) = hook.as_ref() {
                if acquire_key(&hs.mutex, 100) {
                    if let Some(g) = gpu.as_ref() {
                        // convert on the GPU while we hold the game's frame
                        match g.convert() {
                            Ok(t) => gpu_tex = Some(t),
                            Err(e) => {
                                let _ = hs.mutex.ReleaseSync(0);
                                return Err(e);
                            }
                        }
                    } else {
                        ctx.CopySubresourceRegion(
                            &stagings[(issued % RING) as usize],
                            0,
                            0,
                            0,
                            0,
                            &hs.tex,
                            0,
                            Some(&D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 }),
                        );
                    }
                    let _ = hs.mutex.ReleaseSync(0);
                    // ask the hook for the next clean frame (one per recorded frame)
                    std::ptr::write_volatile(&raw mut (*hs.hdr).want_frame, 1);
                    have_frame = true;
                    issued_now = true;
                }
            }

            // pull the newest desktop image, if there is one
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            let timeout = if have_frame { 0 } else { 250 };
            if hook.is_none() {
            match dup.as_ref().unwrap().AcquireNextFrame(timeout, &mut info, &mut res) {
                Ok(()) => {
                    meter.add(info.AccumulatedFrames, shared, job.overlay);
                    if let Some(res) = res.as_ref() {
                        if let Ok(tex) = res.cast::<ID3D11Texture2D>() {
                            ctx.CopyResource(&desktop, &tex);
                            have_frame = true;
                        }
                    }
                    let _ = dup.as_ref().unwrap().ReleaseFrame();
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    // mode change / fullscreen transition: re-create the duplication
                    let mut ok = false;
                    for _ in 0..20 {
                        std::thread::sleep(Duration::from_millis(100));
                        if let Ok(d) = duplicate(&output, &device) {
                            let m = d.GetDesc().ModeDesc;
                            if (m.Width, m.Height) != (mon_w, mon_h) {
                                break;
                            }
                            dup = Some(d);
                            ok = true;
                            break;
                        }
                    }
                    if !ok {
                        break;
                    }
                    next = Instant::now();
                    continue;
                }
                Err(e) => {
                    let _ = sink.Finalize();
                    return Err(e);
                }
            }
            }
            let a_ms = t_wake.elapsed().as_secs_f64() * 1e3;
            acq_ms += a_ms;
            acq_max = acq_max.max(a_ms);
            if !have_frame {
                next = Instant::now();
                continue;
            }
            if hook.is_some() && !issued_now {
                // the hook hasn't produced a new clean frame yet; try again next tick
                next += interval;
                continue;
            }

            if hook.is_none() {
                if let Some(g) = gpu.as_ref() {
                    gpu_tex = Some(g.convert()?);
                } else {
                    ctx.CopySubresourceRegion(&stagings[(issued % RING) as usize], 0, 0, 0, 0, &desktop, 0, Some(&src_box));
                }
            }
            let t_a = Instant::now();
            let (sample, t_b, t_c) = if let Some(tex) = gpu_tex.take() {
                // GPU path: the converted NV12 texture goes straight to the encoder.
                let g = gpu.as_ref().unwrap();
                let s = g.sample(&tex, (t_a - t0).as_nanos() as i64 / 100, sample_dur)?;
                let t = Instant::now();
                (s, t, t)
            } else {
            cap_t[(issued % RING) as usize] = t_a;
            issued += 1;
            if issued < RING {
                // pipeline warm-up: nothing old enough to read yet
                next += interval;
                continue;
            }
            // read the oldest slot (issued two ticks ago), which the GPU finished long since
            let rslot = (issued % RING) as usize;
            let t_cap = cap_t[rslot];
            let staging = &stagings[rslot];
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            stage("reading a captured frame");
            ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let t_b = Instant::now();

            stage("allocating a video buffer");
            let buf = MFCreateMemoryBuffer(row as u32 * h)?;
            let mut dst = std::ptr::null_mut();
            buf.Lock(&mut dst, None, None)?;
            let src = mapped.pData as *const u8;
            let swap_rb = hook.as_ref().is_some_and(|s| s.rgba);
            let ten_bit = hook.as_ref().is_some_and(|s| s.r10);
            for y in 0..h as usize {
                let s = src.add(y * mapped.RowPitch as usize);
                let d = dst.add(y * row);
                if ten_bit {
                    // R10G10B10A2 (r in the low bits) -> 8-bit BGRA, opaque
                    let (s32, d32) = (s as *const u32, d as *mut u32);
                    for x in 0..w as usize {
                        let v = s32.add(x).read_unaligned();
                        let (r, g, b) = ((v & 0x3FF) >> 2, ((v >> 10) & 0x3FF) >> 2, ((v >> 20) & 0x3FF) >> 2);
                        d32.add(x).write_unaligned(0xFF00_0000 | (r << 16) | (g << 8) | b);
                    }
                } else if swap_rb {
                    // game back buffer is RGBA, the encoder wants BGRA
                    let (s32, d32) = (s as *const u32, d as *mut u32);
                    for x in 0..w as usize {
                        let v = s32.add(x).read_unaligned();
                        d32.add(x).write_unaligned((v & 0xFF00_FF00) | ((v & 0xFF) << 16) | ((v >> 16) & 0xFF));
                    }
                } else {
                    std::ptr::copy_nonoverlapping(s, d, row);
                }
            }
            // Burn the overlay in ourselves (software). This works even in exclusive
            // fullscreen, where Windows won't composite our overlay window at all.
            if let (true, Some((_, pm))) = (burn_on, burn_cache.as_ref()) {
                let m = (18.0 * burn_scale) as usize;
                blend(dst, row, w as usize, h as usize, pm, m, m);
            }
            buf.Unlock()?;
            ctx.Unmap(staging, 0);
            buf.SetCurrentLength(row as u32 * h)?;
            let t_c = Instant::now();

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            // real capture time, so video stays in sync with audio (no nominal-fps drift)
            sample.SetSampleTime((t_cap - t0).as_nanos() as i64 / 100)?;
            sample.SetSampleDuration(sample_dur)?;
            (sample, t_b, t_c)
            };
            stage("encoding a frame");
            sink.WriteSample(stream, &sample)?;
            if let (Some(a), Some(s)) = (audio.as_ref(), astream) {
                drain_audio(&sink, s, a)?;
            }
            shared.elapsed.store(t0.elapsed().as_secs(), Ordering::Relaxed);
            let t_d = Instant::now();
            stats[0] += (t_b - t_a).as_secs_f64();
            stats[1] += (t_c - t_b).as_secs_f64();
            stats[2] += (t_d - t_c).as_secs_f64();
            if t_d - t_a > interval {
                slow += 1;
            }

            meter.tick(shared, job.overlay);
            if idx == 0 {
                first_at = Some((Instant::now(), t_d - t_a));
            }
            idx += 1;
            shared.frame.store(idx, Ordering::Relaxed);
            // Wake the UI at ~15 Hz, not once per frame: each wake re-renders the overlay.
            if last_notify.elapsed() >= Duration::from_millis(66) {
                last_notify = Instant::now();
                notify(job);
            }

            next += interval;
            let now = Instant::now();
            if now > next + interval {
                next = now; // fell behind: don't try to burst-catch-up
            }
        }

        let loop_end = t0.elapsed();
        if let Some(a) = audio.as_mut() {
            a.stop();
            if std::env::var_os("SNAPCAP_STATS").is_some() {
                eprintln!("video loop ended at {:?}, audio joined at {:?}", loop_end, t0.elapsed());
            }
            if let Some(s) = astream {
                drain_audio(&sink, s, a)?;
            }
        }
        stage("finishing the file");
        sink.Finalize()?;
        if std::env::var_os("SNAPCAP_STATS").is_some() && idx > 0 {
            let n = idx as f64;
            eprintln!(
                "wake lateness avg {:.2} max {:.2} ms | acquire avg {:.2} max {:.2} ms",
                late_ms / n, late_max, acq_ms / n, acq_max
            );
            if let Some((t, _)) = first_at {
                eprintln!(
                    "startup to first frame {:.0} ms; steady-state {:.1} fps over {} frames",
                    (t - begun).as_secs_f64() * 1e3,
                    (n - 1.0) / t.elapsed().as_secs_f64().max(1e-9),
                    idx
                );
            }
            eprintln!(
                "{w}x{h}: avg ms/frame  copy+map {:.2}  memcpy {:.2}  WriteSample {:.2}  | frames over budget {slow}/{idx}",
                stats[0] / n * 1e3,
                stats[1] / n * 1e3,
                stats[2] / n * 1e3
            );
        }
        Ok(idx)
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// Alpha-blends a premultiplied-RGBA pixmap onto a top-down BGRA frame.
unsafe fn blend(dst: *mut u8, row: usize, w: usize, h: usize, pm: &tiny_skia::Pixmap, ox: usize, oy: usize) {
    unsafe {
        let (pw, ph) = (pm.width() as usize, pm.height() as usize);
        let src = pm.data();
        for y in 0..ph.min(h.saturating_sub(oy)) {
            let d_row = dst.add((oy + y) * row + ox * 4);
            for x in 0..pw.min(w.saturating_sub(ox)) {
                let s = &src[(y * pw + x) * 4..(y * pw + x) * 4 + 4];
                let a = s[3] as u32;
                if a == 0 {
                    continue;
                }
                let d = d_row.add(x * 4);
                if a == 255 {
                    *d = s[2];
                    *d.add(1) = s[1];
                    *d.add(2) = s[0];
                } else {
                    let inv = 255 - a;
                    *d = (s[2] as u32 + *d as u32 * inv / 255).min(255) as u8;
                    *d.add(1) = (s[1] as u32 + *d.add(1) as u32 * inv / 255).min(255) as u8;
                    *d.add(2) = (s[0] as u32 + *d.add(2) as u32 * inv / 255).min(255) as u8;
                }
            }
        }
    }
}

unsafe fn drain_audio(sink: &IMFSinkWriter, stream: u32, a: &crate::audio::Audio) -> Result<()> {
    unsafe {
        while let Ok(c) = a.rx.try_recv() {
            let buf = MFCreateMemoryBuffer(c.pcm.len() as u32)?;
            let mut dst = std::ptr::null_mut();
            buf.Lock(&mut dst, None, None)?;
            std::ptr::copy_nonoverlapping(c.pcm.as_ptr(), dst, c.pcm.len());
            buf.Unlock()?;
            buf.SetCurrentLength(c.pcm.len() as u32)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(c.time)?;
            s.SetSampleDuration(c.dur)?;
            sink.WriteSample(stream, &s)?;
        }
        Ok(())
    }
}

unsafe fn create_writer(
    job: &Job,
    w: u32,
    h: u32,
    with_audio: bool,
    gpu_mgr: Option<&IMFDXGIDeviceManager>,
) -> Result<(IMFSinkWriter, u32, Option<u32>)> {
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 3)?;
        let attrs = attrs.unwrap();
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        if let Some(m) = gpu_mgr {
            // lets the (hardware) encoder read our GPU textures directly
            attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, m)?;
        }

        let wide: Vec<u16> = job.path.to_string_lossy().encode_utf16().chain(Some(0)).collect();
        let sink = MFCreateSinkWriterFromURL(PCWSTR(wide.as_ptr()), None, &attrs)?;

        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, job.bitrate_mbps * 1_000_000)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
        out.SetUINT64(&MF_MT_FRAME_RATE, pack(job.fps, 1))?;
        out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        if gpu_mgr.is_some() {
            // the GPU converter produces BT.709, studio-range NV12: say so, so players decode it right
            out.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            out.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            out.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            out.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        }
        let stream = sink.AddStream(&out)?;

        let inp = MFCreateMediaType()?;
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
        inp.SetUINT64(&MF_MT_FRAME_RATE, pack(job.fps, 1))?;
        inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        if gpu_mgr.is_some() {
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            inp.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            inp.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            inp.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            inp.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        } else {
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            // positive stride == top-down, which matches what the CPU path hands us
            inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, (w * 4) as u32)?;
        }
        sink.SetInputMediaType(stream, &inp, None)?;

        let mut astream = None;
        if with_audio {
            let (rate, ch) = (crate::audio::RATE, crate::audio::CHANNELS);
            let aout = MFCreateMediaType()?;
            aout.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            aout.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            aout.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            aout.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
            aout.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, ch)?;
            aout.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?; // 192 kbps
            aout.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            aout.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
            let s = sink.AddStream(&aout)?;

            let ain = MFCreateMediaType()?;
            ain.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            ain.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            ain.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            ain.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
            ain.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, ch)?;
            ain.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, ch * 2)?;
            ain.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * ch * 2)?;
            sink.SetInputMediaType(s, &ain, None)?;
            astream = Some(s);
        }
        sink.BeginWriting()?;
        Ok((sink, stream, astream))
    }
}
