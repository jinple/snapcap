//! SnapCap's side of the in-game hook: injects `snapcap_hook.dll` into a game (only when
//! asked to), owns the shared-memory block, publishes overlay pixels and reads back the
//! game's real FPS from the hook's Present counter.

#[path = "../hook/src/protocol.rs"]
pub mod protocol;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use windows::Win32::Foundation::*;
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::*;
use windows::Win32::System::ProcessStatus::{K32EnumProcessModulesEx, K32GetModuleBaseNameW, LIST_MODULES_64BIT};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::*;
use windows::core::{BOOL, PCWSTR, s, w};

use protocol::*;

struct Map {
    _handle: HANDLE,
    view: *mut u8,
}

pub struct HookClient {
    maps: std::collections::HashMap<u32, Map>,
    tried: HashSet<u32>,
    dll: PathBuf,
    // FPS derivation from the hook's Present counter
    last_presents: u64,
    last_at: Instant,
    fps10: u32,
    pub status: String,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

impl HookClient {
    pub fn new() -> Self {
        let dll = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("snapcap_hook.dll")))
            .unwrap_or_else(|| PathBuf::from("snapcap_hook.dll"));
        Self {
            maps: Default::default(),
            tried: HashSet::new(),
            dll,
            last_presents: 0,
            last_at: Instant::now(),
            fps10: 0,
            status: String::new(),
        }
    }

    unsafe fn hdr(&self, pid: u32) -> Option<*mut Header> {
        self.maps.get(&pid).map(|m| m.view as *mut Header)
    }

    /// Creates the shared block for `pid` (once) and, if it passes the safety checks,
    /// injects the hook DLL (once per process). Returns a short status message.
    pub unsafe fn ensure(&mut self, pid: u32) {
        unsafe {
            if !self.maps.contains_key(&pid) {
                let name = wide(&map_name(pid));
                let Ok(h) = CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, MAP_SIZE as u32, PCWSTR(name.as_ptr()))
                else {
                    return;
                };
                let view = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, MAP_SIZE);
                if view.Value.is_null() {
                    let _ = CloseHandle(h);
                    return;
                }
                let hdr = view.Value as *mut Header;
                std::ptr::write_bytes(view.Value as *mut u8, 0, size_of::<Header>());
                (*hdr).magic = MAGIC;
                (*hdr).version = 1;
                self.maps.insert(pid, Map { _handle: h, view: view.Value as *mut u8 });
            }
            if self.tried.insert(pid) {
                self.status = match self.inject(pid) {
                    Ok(()) => "hook injected".into(),
                    Err(e) => format!("hook not injected: {e}"),
                };
                eprintln!("in-game overlay: {}", self.status);
            }
        }
    }

    pub fn has(&self, pid: u32) -> bool {
        self.maps.contains_key(&pid)
    }

    /// Frame cap for the focused app (`target`); every other hooked app is set to unlimited.
    pub unsafe fn apply_limit(&self, target: u32, limit: u32) {
        unsafe {
            for (&pid, m) in &self.maps {
                let h = m.view as *mut Header;
                let want = if pid == target { limit } else { 0 };
                if std::ptr::read_volatile(&raw const (*h).fps_limit) != want {
                    std::ptr::write_volatile(&raw mut (*h).fps_limit, want);
                }
            }
        }
    }

    /// Heartbeat so the hook knows SnapCap is alive (it goes idle if this stops).
    pub unsafe fn heartbeat(&self, pid: u32) {
        unsafe {
            if let Some(h) = self.hdr(pid) {
                std::ptr::write_volatile(&raw mut (*h).main_tick, GetTickCount64());
            }
        }
    }

    /// True while the hook inside `pid` is running and presenting frames.
    pub unsafe fn alive(&self, pid: u32) -> bool {
        unsafe {
            let Some(h) = self.hdr(pid) else { return false };
            let hp = std::ptr::read_volatile(&raw const (*h).hook_pid);
            let tick = std::ptr::read_volatile(&raw const (*h).hook_tick);
            hp == pid && GetTickCount64().saturating_sub(tick) < 1500
        }
    }

    /// Hands the hook a new overlay image to draw at (x, y) in the game's back buffer.
    pub unsafe fn publish(&self, pid: u32, bgra: &[u8], w: u32, h: u32, x: i32, y: i32) {
        unsafe {
            let Some(hdr) = self.hdr(pid) else { return };
            if w as usize > MAX_W || h as usize > MAX_H || bgra.len() < (w * h * 4) as usize {
                return;
            }
            let base = hdr as *mut u8;
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), base.add(PIXELS_OFFSET), (w * h * 4) as usize);
            std::ptr::write_volatile(&raw mut (*hdr).w, w);
            std::ptr::write_volatile(&raw mut (*hdr).h, h);
            std::ptr::write_volatile(&raw mut (*hdr).x, x);
            std::ptr::write_volatile(&raw mut (*hdr).y, y);
            let seq = std::ptr::read_volatile(&raw const (*hdr).seq);
            std::ptr::write_volatile(&raw mut (*hdr).seq, seq.wrapping_add(1));
            std::ptr::write_volatile(&raw mut (*hdr).enable, 1);
        }
    }

    pub unsafe fn set_enabled(&self, pid: u32, on: bool) {
        unsafe {
            if let Some(h) = self.hdr(pid) {
                std::ptr::write_volatile(&raw mut (*h).enable, on as u32);
            }
        }
    }

    /// Real game FPS (x10) from the hook's Present counter; 0 if the hook isn't running.
    pub unsafe fn fps10(&mut self, pid: u32) -> u32 {
        unsafe {
            let Some(h) = self.hdr(pid) else { return 0 };
            if !self.alive(pid) {
                self.fps10 = 0;
                return 0;
            }
            let dt = self.last_at.elapsed().as_secs_f64();
            if dt >= 0.5 {
                let p = std::ptr::read_volatile(&raw const (*h).presents);
                if p >= self.last_presents && self.last_presents != 0 {
                    self.fps10 = ((p - self.last_presents) as f64 / dt * 10.0).round() as u32;
                }
                self.last_presents = p;
                self.last_at = Instant::now();
            }
            self.fps10
        }
    }

    unsafe fn inject(&self, pid: u32) -> Result<(), String> {
        unsafe {
            if !self.dll.exists() {
                return Err(format!("{} not found next to snapcap.exe", self.dll.display()));
            }
            let access = PROCESS_CREATE_THREAD
                | PROCESS_QUERY_INFORMATION
                | PROCESS_VM_OPERATION
                | PROCESS_VM_WRITE
                | PROCESS_VM_READ;
            let proc = OpenProcess(access, false, pid).map_err(|e| format!("can't open the game process ({e}); is it running as admin?"))?;
            let res = self.inject_into(proc);
            let _ = CloseHandle(proc);
            res
        }
    }

    unsafe fn inject_into(&self, proc: HANDLE) -> Result<(), String> {
        unsafe {
            // 64-bit games only (our DLL is 64-bit)
            let mut wow = BOOL(0);
            let _ = IsWow64Process(proc, &mut wow);
            if wow.as_bool() {
                return Err("32-bit game: not supported".into());
            }

            // Look at what the game has loaded: refuse anti-cheat, and require plain D3D11.
            let mut mods = [HMODULE::default(); 1024];
            let mut needed = 0u32;
            if K32EnumProcessModulesEx(proc, mods.as_mut_ptr(), (mods.len() * size_of::<HMODULE>()) as u32, &mut needed, LIST_MODULES_64BIT.0)
                .as_bool()
            {
                let n = (needed as usize / size_of::<HMODULE>()).min(mods.len());
                let (mut d3d11, mut d3d12) = (false, false);
                for m in &mods[..n] {
                    let mut buf = [0u16; 260];
                    let len = K32GetModuleBaseNameW(proc, Some(*m), &mut buf) as usize;
                    let name = String::from_utf16_lossy(&buf[..len]).to_ascii_lowercase();
                    if ["easyanticheat", "beclient", "battleye", "vgk", "eaanticheat", "faceit", "xigncode", "npggnt", "gameguard", "mhyprot", "ricochet"]
                        .iter()
                        .any(|b| name.contains(b))
                    {
                        return Err(format!("anti-cheat detected ({name}); refusing to inject"));
                    }
                    d3d11 |= name == "d3d11.dll";
                    d3d12 |= name == "d3d12.dll";
                }
                if d3d12 {
                    return Err("Direct3D 12 game: only Direct3D 11 is supported so far".into());
                }
                if !d3d11 {
                    return Err("game doesn't use Direct3D 11 (Vulkan/OpenGL/other): not supported".into());
                }
            }

            let path = wide(&self.dll.to_string_lossy());
            let bytes = path.len() * 2;
            let remote = VirtualAllocEx(proc, None, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
            if remote.is_null() {
                return Err("VirtualAllocEx failed".into());
            }
            let mut written = 0usize;
            WriteProcessMemory(proc, remote, path.as_ptr() as *const _, bytes, Some(&mut written)).map_err(|e| e.to_string())?;
            let k32 = GetModuleHandleW(w!("kernel32.dll")).map_err(|e| e.to_string())?;
            let ll = GetProcAddress(k32, s!("LoadLibraryW")).ok_or("no LoadLibraryW")?;
            let start: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32 = std::mem::transmute(ll);
            let th = CreateRemoteThread(proc, None, 0, Some(start), Some(remote), 0, None).map_err(|e| e.to_string())?;
            WaitForSingleObject(th, 5000);
            let _ = CloseHandle(th);
            let _ = VirtualFreeEx(proc, remote, 0, MEM_RELEASE);
            Ok(())
        }
    }
}
