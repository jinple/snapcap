//! Shared-memory protocol between SnapCap (main process) and the in-game hook DLL.
//! This file is compiled into both crates (`#[path]`), so keep it dependency-free.
//!
//! The block is a named file mapping `Local\SnapCapHook_<game pid>`:
//!   [ Header | ... padding ... | overlay pixels (premultiplied BGRA, w*h*4) ]

pub const MAGIC: u32 = 0x314B_4353; // "SCK1"
pub const MAX_W: usize = 1400;
pub const MAX_H: usize = 700;
pub const PIXELS_OFFSET: usize = 256;
pub const MAP_SIZE: usize = PIXELS_OFFSET + MAX_W * MAX_H * 4;

#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub version: u32,
    /// GetTickCount64() of SnapCap's last heartbeat. The hook goes idle if this is stale.
    pub main_tick: u64,
    // ---- SnapCap -> hook ----
    /// 1 = draw the overlay.
    pub enable: u32,
    /// Bumped whenever the pixels change.
    pub seq: u32,
    pub w: u32,
    pub h: u32,
    /// Top-left position of the overlay inside the game's back buffer.
    pub x: i32,
    pub y: i32,
    /// 1 = the recorder wants pre-overlay frames from the hook.
    pub want_capture: u32,
    pub _pad0: u32,
    // ---- hook -> SnapCap ----
    pub hook_pid: u32,
    /// DXGI_FORMAT of the game's back buffer.
    pub bb_format: u32,
    /// GetTickCount64() of the hook's last Present.
    pub hook_tick: u64,
    /// Number of Present calls seen (SnapCap derives the real game FPS from this).
    pub presents: u64,
    /// Frames on which the overlay was actually drawn.
    pub drawn: u64,
    pub bb_w: u32,
    pub bb_h: u32,
    /// Bumped each time a fresh pre-overlay frame was copied for the recorder.
    pub frame_seq: u64,
}

pub fn map_name(pid: u32) -> String {
    format!("Local\\SnapCapHook_{pid}")
}

/// Name of the shared (keyed-mutex) texture holding the latest pre-overlay frame.
#[allow(dead_code)]
pub fn frame_name(pid: u32) -> String {
    format!("Local\\SnapCapFrame_{pid}")
}
