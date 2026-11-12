//! SnapCap in-game overlay hook (Direct3D 11).
//!
//! Loaded into a game on request by SnapCap. It patches IDXGISwapChain::Present so that,
//! right before each frame is presented, it draws SnapCap's overlay into the game's own
//! back buffer. That is the only way to show anything over exclusive fullscreen, where
//! Windows won't composite any other window on top of the game.
//!
//! Safety notes: every D3D11 call is recorded on a *deferred* context and replayed with
//! RestoreContextState=TRUE, so the game's render state is untouched. Anything that goes
//! wrong just disables the overlay for that frame; it never panics across the FFI edge.

#![allow(clippy::missing_safety_doc)]

#[path = "protocol.rs"]
mod protocol;

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows::Win32::System::Memory::*;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, GetCurrentProcessId, SetWaitableTimer, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_OVERLAPPED};
use windows::core::{BOOL, HRESULT, Interface, PCSTR, w};

use protocol::*;

// Volatile accessors for the shared header (SnapCap writes it from another process).
macro_rules! rd {
    ($h:expr, $f:ident) => {
        std::ptr::read_volatile(&raw const (*$h).$f)
    };
}
macro_rules! wr {
    ($h:expr, $f:ident, $v:expr) => {
        std::ptr::write_volatile(&raw mut (*$h).$f, $v)
    };
}

static ORIG_PRESENT: AtomicUsize = AtomicUsize::new(0);
static ORIG_PRESENT1: AtomicUsize = AtomicUsize::new(0);
static VIEW: AtomicUsize = AtomicUsize::new(0); // mapped shared block (0 = not open)
static LAST_OPEN_TRY: AtomicUsize = AtomicUsize::new(0);

struct Gfx {
    dev_raw: usize,
    ctx: ID3D11DeviceContext,
    dctx: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    blend: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    sampler: ID3D11SamplerState,
    cb: ID3D11Buffer,
    tex: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, u32, u32)>,
    /// Pre-overlay copy of the back buffer, shared with the recorder.
    shared: Option<SharedFrame>,
    last_seq: u32,
    logged: bool,
}

struct SharedFrame {
    tex: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    handle: HANDLE, // keeps the *named* shared object alive
    w: u32,
    h: u32,
    fmt: i32,
}

impl Drop for SharedFrame {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}
// COM interfaces are free-threaded for our usage (we only touch them under the mutex).
unsafe impl Send for Gfx {}

static GFX: Mutex<Option<Gfx>> = Mutex::new(None);

fn log(msg: &str) {
    use std::io::Write;
    if let Some(dir) = std::env::var_os("TEMP") {
        let p = std::path::Path::new(&dir).join("snapcap_hook.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "[pid {}] {msg}", unsafe { GetCurrentProcessId() });
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(hinst: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == 1 {
        // DLL_PROCESS_ATTACH
        unsafe {
            let _ = DisableThreadLibraryCalls(hinst);
        }
        std::thread::spawn(|| {
            let r = std::panic::catch_unwind(|| unsafe { install() });
            match r {
                Ok(Ok(())) => log("hook installed"),
                Ok(Err(e)) => log(&format!("install failed: {e}")),
                Err(_) => log("install panicked"),
            }
        });
    }
    TRUE
}

/// Creates a throw-away D3D11 device + swap chain just to learn where dxgi.dll keeps the
/// swap-chain vtable, then patches `Present` / `Present1` in it. (All swap chains of that
/// class share the vtable, including the game's.)
unsafe fn install() -> windows::core::Result<()> {
    unsafe {
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), w!("STATIC"), w!("snapcap-dummy"), WS_OVERLAPPED, 0, 0, 8, 8, None, None, None, None)?;
        let desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: 8,
                Height: 8,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ..Default::default()
            },
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 1,
            OutputWindow: hwnd,
            Windowed: TRUE,
            SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
            ..Default::default()
        };
        let mut sc: Option<IDXGISwapChain> = None;
        let mut dev: Option<ID3D11Device> = None;
        D3D11CreateDeviceAndSwapChain(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&desc),
            Some(&mut sc),
            Some(&mut dev),
            None,
            None,
        )?;
        let sc = sc.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        let vtbl = *(sc.as_raw() as *const *mut usize);
        patch(vtbl.add(8), present_hook as *const () as usize, &ORIG_PRESENT);
        patch(vtbl.add(22), present1_hook as *const () as usize, &ORIG_PRESENT1);
        drop(sc);
        drop(dev);
        let _ = DestroyWindow(hwnd);
        Ok(())
    }
}

unsafe fn patch(slot: *mut usize, new: usize, orig: &AtomicUsize) {
    unsafe {
        if *slot == new {
            return;
        }
        let mut old = PAGE_PROTECTION_FLAGS(0);
        if VirtualProtect(slot as *const c_void, size_of::<usize>(), PAGE_READWRITE, &mut old).is_ok() {
            orig.store(*slot, Ordering::SeqCst);
            *slot = new;
            let mut tmp = PAGE_PROTECTION_FLAGS(0);
            let _ = VirtualProtect(slot as *const c_void, size_of::<usize>(), old, &mut tmp);
        }
    }
}

type PresentFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT;
type Present1Fn = unsafe extern "system" fn(*mut c_void, u32, u32, *const c_void) -> HRESULT;

thread_local! { static IN_HOOK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

unsafe extern "system" fn present_hook(this: *mut c_void, sync: u32, flags: u32) -> HRESULT {
    unsafe {
        guarded(this);
        limit_frame();
        let f: PresentFn = std::mem::transmute(ORIG_PRESENT.load(Ordering::SeqCst));
        f(this, sync, flags)
    }
}

unsafe extern "system" fn present1_hook(this: *mut c_void, sync: u32, flags: u32, p: *const c_void) -> HRESULT {
    unsafe {
        guarded(this);
        limit_frame();
        let f: Present1Fn = std::mem::transmute(ORIG_PRESENT1.load(Ordering::SeqCst));
        f(this, sync, flags, p)
    }
}

// ---- frame limiter ------------------------------------------------------------------------
// Paces Present calls to the cap SnapCap wrote into the shared header. We wait *before* the
// real Present, so frames leave at an even rate. Anything stale or odd just means "no limit".

static NEXT_PRESENT: AtomicI64 = AtomicI64::new(0); // QPC ticks when the next Present may go
static TIMER: AtomicUsize = AtomicUsize::new(0); // high-resolution waitable timer handle
static QPC_FREQ: AtomicI64 = AtomicI64::new(0);

unsafe fn qpc() -> i64 {
    unsafe {
        let mut v = 0i64;
        let _ = QueryPerformanceCounter(&mut v);
        v
    }
}

unsafe fn limit_frame() {
    let _ = std::panic::catch_unwind(|| unsafe {
        let h = header();
        if h.is_null() {
            return;
        }
        let lim = rd!(h, fps_limit);
        // SnapCap gone? Never keep throttling a game nobody is controlling any more.
        if lim == 0 || GetTickCount64().saturating_sub(rd!(h, main_tick)) > 2000 {
            NEXT_PRESENT.store(0, Ordering::Relaxed);
            return;
        }
        let mut freq = QPC_FREQ.load(Ordering::Relaxed);
        if freq == 0 {
            let _ = QueryPerformanceFrequency(&mut freq);
            QPC_FREQ.store(freq, Ordering::Relaxed);
        }
        let interval = freq / lim.clamp(1, 1000) as i64;
        let now = qpc();
        let target = NEXT_PRESENT.load(Ordering::Relaxed);
        let go_at = if target == 0 || now >= target { now } else { wait_until(target, freq); target };
        // If we ran late, restart the cadence from now instead of bursting to catch up.
        NEXT_PRESENT.store(go_at + interval, Ordering::Relaxed);
    });
}

/// Coarse wait on a high-resolution waitable timer, then spin for the last ~1.5 ms.
unsafe fn wait_until(target: i64, freq: i64) {
    unsafe {
        let mut timer = TIMER.load(Ordering::Relaxed);
        if timer == 0 {
            // CREATE_WAITABLE_TIMER_HIGH_RESOLUTION = 2, TIMER_ALL_ACCESS = 0x1F0003
            timer = CreateWaitableTimerExW(None, windows::core::PCWSTR::null(), 2, 0x1F0003)
                .map(|h| h.0 as usize)
                .unwrap_or(usize::MAX);
            TIMER.store(timer, Ordering::Relaxed);
        }
        loop {
            let now = qpc();
            if now >= target {
                return;
            }
            let left_us = (target - now) * 1_000_000 / freq;
            if left_us > 1500 {
                let coarse_100ns = -((left_us - 1000) * 10);
                if timer != usize::MAX
                    && SetWaitableTimer(HANDLE(timer as *mut c_void), &coarse_100ns, 0, None, None, false).is_ok()
                {
                    WaitForSingleObject(HANDLE(timer as *mut c_void), 100);
                } else {
                    std::thread::sleep(std::time::Duration::from_micros((left_us - 1000) as u64));
                }
            } else {
                std::hint::spin_loop();
            }
        }
    }
}

unsafe fn guarded(this: *mut c_void) {
    if IN_HOOK.get() {
        return;
    }
    IN_HOOK.set(true);
    let t0 = unsafe { qpc() };
    let _ = std::panic::catch_unwind(|| unsafe { on_present(this) });
    let dt = unsafe { qpc() } - t0;
    IN_HOOK.set(false);
    // Cheap self-timing: every ~5 s write how long the hook's own work took per Present.
    static SUM: AtomicI64 = AtomicI64::new(0);
    static MAX: AtomicI64 = AtomicI64::new(0);
    static N: AtomicI64 = AtomicI64::new(0);
    static SINCE: AtomicI64 = AtomicI64::new(0);
    SUM.fetch_add(dt, Ordering::Relaxed);
    MAX.fetch_max(dt, Ordering::Relaxed);
    N.fetch_add(1, Ordering::Relaxed);
    let mut freq = QPC_FREQ.load(Ordering::Relaxed);
    if freq == 0 {
        let _ = unsafe { QueryPerformanceFrequency(&mut freq) };
        QPC_FREQ.store(freq, Ordering::Relaxed);
    }
    let freq = freq.max(1);
    let since = SINCE.load(Ordering::Relaxed);
    if since == 0 {
        SINCE.store(t0, Ordering::Relaxed);
    } else if (t0 - since) > freq * 5 {
        let n = N.swap(0, Ordering::Relaxed).max(1);
        let us = |t: i64| t as f64 * 1e6 / freq as f64;
        let (ovl, rec, lim) = {
            let h = header();
            if h.is_null() { (9, 9, 0) } else { (rd!(h, enable), rd!(h, want_capture), rd!(h, fps_limit)) }
        };
        log(&format!(
            "t={}s overlay={} recording={} limit={} | {:.1} presents/s, hook cost avg {:.0} us max {:.0} us",
            GetTickCount64() / 1000 % 100000,
            ovl,
            rec,
            lim,
            n as f64 * freq as f64 / (t0 - since) as f64,
            us(SUM.swap(0, Ordering::Relaxed)) / n as f64,
            us(MAX.swap(0, Ordering::Relaxed)),
        ));
        SINCE.store(t0, Ordering::Relaxed);
    }
}

unsafe fn header() -> *mut Header {
    unsafe {
        let v = VIEW.load(Ordering::Relaxed);
        if v != 0 {
            return v as *mut Header;
        }
        // (re)try opening SnapCap's mapping at most once a second
        let now = GetTickCount64() as usize;
        if now.wrapping_sub(LAST_OPEN_TRY.load(Ordering::Relaxed)) < 1000 {
            return std::ptr::null_mut();
        }
        LAST_OPEN_TRY.store(now, Ordering::Relaxed);
        let name: Vec<u16> = map_name(GetCurrentProcessId()).encode_utf16().chain(Some(0)).collect();
        let Ok(h) = OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, windows::core::PCWSTR(name.as_ptr())) else {
            return std::ptr::null_mut();
        };
        let view = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, MAP_SIZE);
        if view.Value.is_null() {
            return std::ptr::null_mut();
        }
        let hdr = view.Value as *mut Header;
        if std::ptr::read_volatile(&raw const (*hdr).magic) != MAGIC {
            return std::ptr::null_mut();
        }
        VIEW.store(view.Value as usize, Ordering::Relaxed);
        hdr
    }
}

const HLSL: &str = r#"
cbuffer C : register(b0) { float4 rect; float4 flags; };
struct V { float4 p : SV_Position; float2 uv : TEXCOORD0; };
V vs(uint id : SV_VertexID) {
    float2 uv = float2(id & 1, id >> 1);
    V o;
    o.p = float4(lerp(rect.x, rect.z, uv.x), lerp(rect.y, rect.w, uv.y), 0, 1);
    o.uv = uv;
    return o;
}
Texture2D t : register(t0);
SamplerState s : register(s0);
float4 ps(V i) : SV_Target {
    float4 c = t.Sample(s, i.uv);
    if (flags.x > 0.5 && c.a > 0) {          // sRGB target: our pixels are already sRGB
        float3 u = pow(abs(c.rgb / c.a), 2.2);
        c.rgb = u * c.a;
    }
    return c;
}
"#;

unsafe fn compile(entry: &[u8], target: &[u8]) -> windows::core::Result<ID3DBlob> {
    unsafe {
        let mut code: Option<ID3DBlob> = None;
        let mut err: Option<ID3DBlob> = None;
        let r = D3DCompile(
            HLSL.as_ptr() as *const c_void,
            HLSL.len(),
            PCSTR::null(),
            None,
            None::<&ID3DInclude>,
            PCSTR(entry.as_ptr()),
            PCSTR(target.as_ptr()),
            0,
            0,
            &mut code,
            Some(&mut err),
        );
        if let Err(e) = r {
            if let Some(b) = err {
                let msg = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
                log(&format!("shader error: {}", String::from_utf8_lossy(msg)));
            }
            return Err(e);
        }
        code.ok_or_else(|| windows::core::Error::from(E_FAIL))
    }
}

unsafe fn make_gfx(dev: &ID3D11Device) -> windows::core::Result<Gfx> {
    unsafe {
        let ctx = dev.GetImmediateContext()?;
        let mut dctx: Option<ID3D11DeviceContext> = None;
        dev.CreateDeferredContext(0, Some(&mut dctx))?;
        let dctx = dctx.ok_or_else(|| windows::core::Error::from(E_FAIL))?;

        let vsb = compile(b"vs\0", b"vs_4_0\0")?;
        let psb = compile(b"ps\0", b"ps_4_0\0")?;
        let bytes = |b: &ID3DBlob| std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
        let mut vs = None;
        dev.CreateVertexShader(bytes(&vsb), None, Some(&mut vs))?;
        let mut ps = None;
        dev.CreatePixelShader(bytes(&psb), None, Some(&mut ps))?;

        let mut bd = D3D11_BLEND_DESC::default();
        bd.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: TRUE,
            SrcBlend: D3D11_BLEND_ONE,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: 0x0F,
        };
        let mut blend = None;
        dev.CreateBlendState(&bd, Some(&mut blend))?;

        let rd_ = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: TRUE,
            ..Default::default()
        };
        let mut raster = None;
        dev.CreateRasterizerState(&rd_, Some(&mut raster))?;

        let sd = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler = None;
        dev.CreateSamplerState(&sd, Some(&mut sampler))?;

        let cbd = D3D11_BUFFER_DESC {
            ByteWidth: 32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let mut cb = None;
        dev.CreateBuffer(&cbd, None, Some(&mut cb))?;

        let e = || windows::core::Error::from(E_FAIL);
        Ok(Gfx {
            dev_raw: dev.as_raw() as usize,
            ctx,
            dctx,
            vs: vs.ok_or_else(e)?,
            ps: ps.ok_or_else(e)?,
            blend: blend.ok_or_else(e)?,
            raster: raster.ok_or_else(e)?,
            sampler: sampler.ok_or_else(e)?,
            cb: cb.ok_or_else(e)?,
            tex: None,
            shared: None,
            last_seq: 0,
            logged: false,
        })
    }
}

unsafe fn make_shared(dev: &ID3D11Device, bd: &D3D11_TEXTURE2D_DESC) -> windows::core::Result<SharedFrame> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: bd.Width,
            Height: bd.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: bd.Format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            // render-target bind too: the recorder's GPU video processor wants it on its input
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32,
        };
        let mut t = None;
        dev.CreateTexture2D(&desc, None, Some(&mut t))?;
        let tex = t.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        let res1: IDXGIResource1 = tex.cast()?;
        let name: Vec<u16> = frame_name(GetCurrentProcessId()).encode_utf16().chain(Some(0)).collect();
        // DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE
        let handle = res1.CreateSharedHandle(None, 0x8000_0001, windows::core::PCWSTR(name.as_ptr()))?;
        let mutex: IDXGIKeyedMutex = tex.cast()?;
        Ok(SharedFrame { tex, mutex, handle, w: bd.Width, h: bd.Height, fmt: bd.Format.0 })
    }
}

/// AcquireSync that distinguishes real success from WAIT_TIMEOUT (which windows-rs
/// reports as Ok because it's a non-negative HRESULT).
unsafe fn acquire(m: &IDXGIKeyedMutex, ms: u32) -> bool {
    unsafe { (Interface::vtable(m).AcquireSync)(Interface::as_raw(m), 0, ms) == HRESULT(0) }
}

/// Only plain 8-bit RGBA/BGRA back buffers (UNORM / sRGB / typeless) are handled.
fn rtv_format(f: DXGI_FORMAT) -> Option<(DXGI_FORMAT, bool)> {
    match f {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_TYPELESS => Some((DXGI_FORMAT_R8G8B8A8_UNORM, false)),
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_TYPELESS => Some((DXGI_FORMAT_B8G8R8A8_UNORM, false)),
        // 10-bit SDR back buffers (Unreal Engine's default): blending our 8-bit overlay into them is fine.
        DXGI_FORMAT_R10G10B10A2_UNORM | DXGI_FORMAT_R10G10B10A2_TYPELESS => Some((DXGI_FORMAT_R10G10B10A2_UNORM, false)),
        DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => Some((DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, true)),
        DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => Some((DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, true)),
        _ => None,
    }
}

unsafe fn on_present(this: *mut c_void) {
    unsafe {
        let h = header();
        if h.is_null() {
            return;
        }
        let now = GetTickCount64();
        wr!(h, hook_pid, GetCurrentProcessId());
        wr!(h, hook_tick, now);
        wr!(h, presents, rd!(h, presents) + 1);

        // SnapCap gone / frozen? Then stay out of the way.
        if now.saturating_sub(rd!(h, main_tick)) > 2000 {
            return;
        }
        let want_cap = rd!(h, want_capture) == 1;
        let (w, hh) = (rd!(h, w), rd!(h, h));
        let draw = rd!(h, enable) == 1 && w > 0 && hh > 0 && w as usize <= MAX_W && hh as usize <= MAX_H;
        if !draw && !want_cap {
            return;
        }

        let Some(sc) = IDXGISwapChain::from_raw_borrowed(&this) else { return };
        // Fails for Direct3D 12 / other swap chains, which we simply ignore.
        let Ok(dev) = sc.GetDevice::<ID3D11Device>() else { return };
        let Ok(bb) = sc.GetBuffer::<ID3D11Texture2D>(0) else { return };
        let mut bd = D3D11_TEXTURE2D_DESC::default();
        bb.GetDesc(&mut bd);
        wr!(h, bb_w, bd.Width);
        wr!(h, bb_h, bd.Height);
        wr!(h, bb_format, bd.Format.0 as u32);
        {
            // one-time diagnostics: what is the game presenting?
            static LOGGED: AtomicI64 = AtomicI64::new(0);
            if LOGGED.fetch_add(1, Ordering::Relaxed) == 0 {
                log(&format!(
                    "first present: back buffer {}x{} fmt {} (supported: {}), overlay enable={} {}x{}, want_capture={}",
                    bd.Width, bd.Height, bd.Format.0, rtv_format(bd.Format).is_some(), rd!(h, enable), w, hh, want_cap
                ));
            }
        }
        let Some((rtv_fmt, srgb)) = rtv_format(bd.Format) else { return };

        // keep the shared-frame resources and the draw resources under one lock
        let mut guard = match GFX.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if guard.as_ref().is_none_or(|g| g.dev_raw != dev.as_raw() as usize) {
            *guard = None;
            match make_gfx(&dev) {
                Ok(g) => *guard = Some(g),
                Err(e) => {
                    log(&format!("gfx init failed: {e}"));
                    return;
                }
            }
        }
        let g = guard.as_mut().unwrap();

        // --- hand the recorder a clean (overlay-free) copy of this frame ---
        if want_cap {
            let stale = g.shared.as_ref().is_none_or(|s| s.w != bd.Width || s.h != bd.Height || s.fmt != bd.Format.0);
            if stale {
                g.shared = None; // release the old name before reusing it
                match make_shared(&dev, &bd) {
                    Ok(s) => g.shared = Some(s),
                    Err(e) => log(&format!("shared frame failed: {e}")),
                }
            }
            if let Some(s) = g.shared.as_ref() {
                // Only copy when the recorder asked for a frame: this copy goes through a keyed
                // mutex, which flushes the game's GPU pipeline, so doing it on every Present
                // (e.g. 144/s) tanks the frame rate without loading the GPU.
                if rd!(h, want_frame) == 1 && acquire(&s.mutex, 0) {
                    g.ctx.CopyResource(&s.tex, &bb);
                    let _ = s.mutex.ReleaseSync(0);
                    wr!(h, want_frame, 0);
                    wr!(h, frame_seq, rd!(h, frame_seq) + 1);
                }
            }
        } else if g.shared.is_some() {
            g.shared = None;
        }
        if !draw {
            return;
        }

        // --- upload the overlay pixels when they changed (or the size did) ---
        let need_tex = g.tex.as_ref().is_none_or(|t| t.2 != w || t.3 != hh);
        if need_tex {
            let td = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: hh,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut t = None;
            if dev.CreateTexture2D(&td, None, Some(&mut t)).is_err() {
                return;
            }
            let Some(t) = t else { return };
            let mut srv = None;
            if dev.CreateShaderResourceView(&t, None, Some(&mut srv)).is_err() {
                return;
            }
            let Some(srv) = srv else { return };
            g.tex = Some((t, srv, w, hh));
            g.last_seq = 0;
        }
        let (tex, srv, _, _) = g.tex.as_ref().unwrap();
        let seq = rd!(h, seq);
        if seq != g.last_seq {
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            if g.ctx.Map(tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m)).is_ok() {
                let src = (h as *const u8).add(PIXELS_OFFSET);
                for y in 0..hh as usize {
                    std::ptr::copy_nonoverlapping(
                        src.add(y * w as usize * 4),
                        (m.pData as *mut u8).add(y * m.RowPitch as usize),
                        w as usize * 4,
                    );
                }
                g.ctx.Unmap(tex, 0);
                g.last_seq = seq;
            }
        }

        // --- constants: where the quad goes, in NDC ---
        let (x, y) = (rd!(h, x) as f32, rd!(h, y) as f32);
        let (bw, bh) = (bd.Width as f32, bd.Height as f32);
        let ndc = |px: f32, tot: f32| px / tot * 2.0 - 1.0;
        let c: [f32; 8] = [
            ndc(x, bw),
            -ndc(y, bh),
            ndc(x + w as f32, bw),
            -ndc(y + hh as f32, bh),
            if srgb { 1.0 } else { 0.0 },
            0.0,
            0.0,
            0.0,
        ];
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        if g.ctx.Map(&g.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m)).is_ok() {
            std::ptr::copy_nonoverlapping(c.as_ptr() as *const u8, m.pData as *mut u8, 32);
            g.ctx.Unmap(&g.cb, 0);
        }

        // --- draw, recorded on the deferred context and replayed with state restore ---
        let rtv_desc = D3D11_RENDER_TARGET_VIEW_DESC {
            Format: rtv_fmt,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut rtv = None;
        if dev.CreateRenderTargetView(&bb, Some(&rtv_desc), Some(&mut rtv)).is_err() {
            return;
        }
        let Some(rtv) = rtv else { return };

        let d = &g.dctx;
        d.OMSetRenderTargets(Some(&[Some(rtv)]), None);
        d.RSSetViewports(Some(&[D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: bw,
            Height: bh,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]));
        d.RSSetState(&g.raster);
        d.OMSetBlendState(&g.blend, None, 0xFFFF_FFFF);
        d.IASetInputLayout(None);
        d.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
        d.VSSetShader(&g.vs, None);
        d.PSSetShader(&g.ps, None);
        d.VSSetConstantBuffers(0, Some(&[Some(g.cb.clone())]));
        d.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
        d.PSSetSamplers(0, Some(&[Some(g.sampler.clone())]));
        d.Draw(4, 0);
        let mut cl: Option<ID3D11CommandList> = None;
        if d.FinishCommandList(false, Some(&mut cl)).is_ok() {
            if let Some(cl) = cl {
                g.ctx.ExecuteCommandList(&cl, true); // true = restore the game's state
                wr!(h, drawn, rd!(h, drawn) + 1);
                if !g.logged {
                    g.logged = true;
                    log(&format!("first overlay draw ok ({}x{} on {}x{} fmt {})", w, hh, bd.Width, bd.Height, bd.Format.0));
                }
            }
        }
    }
}
