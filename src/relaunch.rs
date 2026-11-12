//! "Restart in DX11": Unreal games render with Direct3D 12 by default, which the in-game hook
//! can't use. Most Unreal builds accept `-dx11`, so for those we offer to close the game and
//! start it again with that switch (same exe, same arguments, plus `-dx11`).

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows::Win32::Foundation::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
use windows::core::PWSTR;

#[repr(C)]
struct UnicodeString {
    length: u16,
    max: u16,
    buffer: *mut u16,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationProcess(h: HANDLE, class: u32, info: *mut core::ffi::c_void, len: u32, ret: *mut u32) -> i32;
}

/// How to relaunch a game: what to start, with which arguments, and which processes to close first.
#[derive(Clone)]
pub struct Plan {
    pub exe: PathBuf,
    /// The original command line minus the exe (what follows argv[0]).
    pub rest: String,
    /// Processes to close, game first, then any launcher stub that spawned it.
    pub pids: Vec<u32>,
    pub name: String,
}

unsafe fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Option<HANDLE> {
    unsafe { OpenProcess(access, false, pid).ok() }
}

unsafe fn image_path(h: HANDLE) -> Option<PathBuf> {
    unsafe {
        let mut buf = [0u16; 1040];
        let mut len = buf.len() as u32;
        QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

/// ProcessCommandLineInformation (60): the command line, as a UNICODE_STRING plus its characters.
unsafe fn command_line(h: HANDLE) -> Option<String> {
    unsafe {
        let mut buf = vec![0u16; 8192];
        let mut ret = 0u32;
        let st = NtQueryInformationProcess(h, 60, buf.as_mut_ptr() as *mut _, (buf.len() * 2) as u32, &mut ret);
        if st < 0 {
            return None;
        }
        let us = &*(buf.as_ptr() as *const UnicodeString);
        if us.buffer.is_null() {
            return None;
        }
        Some(String::from_utf16_lossy(std::slice::from_raw_parts(us.buffer, us.length as usize / 2)))
    }
}

unsafe fn parent_of(pid: u32) -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut parent = None;
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                if e.th32ProcessID == pid {
                    parent = Some(e.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        parent
    }
}

fn is_unreal(exe: &Path) -> bool {
    let p = exe.to_string_lossy().to_ascii_lowercase();
    p.contains("\\binaries\\win64\\") || exe.parent().is_some_and(|d| d.join("Engine").is_dir())
}

fn rest_of(cmd: &str) -> String {
    let c = cmd.trim_start();
    let end = if c.starts_with('"') { c[1..].find('"').map(|i| i + 2).unwrap_or(c.len()) } else { c.find(' ').unwrap_or(c.len()) };
    c[end..].trim().to_string()
}

/// Works out how to relaunch `pid` with `-dx11`; `None` if it isn't an Unreal game or already
/// has the switch (then it isn't something this button can fix).
pub unsafe fn plan(pid: u32) -> Option<Plan> {
    unsafe {
        let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let exe = image_path(h);
        let cmd = command_line(h);
        let _ = CloseHandle(h);
        let (mut exe, mut cmd) = (exe?, cmd?);
        let mut pids = vec![pid];

        // Unreal builds often start through a stub exe of the same name; relaunch that one.
        if let Some(pp) = parent_of(pid).filter(|&p| p != 0 && p != pid) {
            if let Some(ph) = open(pp, PROCESS_QUERY_LIMITED_INFORMATION) {
                let pexe = image_path(ph);
                let pcmd = command_line(ph);
                let _ = CloseHandle(ph);
                if let (Some(pexe), Some(pcmd)) = (pexe, pcmd) {
                    if pexe.file_name() == exe.file_name() {
                        pids.push(pp);
                        exe = pexe;
                        cmd = pcmd;
                    }
                }
            }
        }
        if !is_unreal(&exe) || cmd.to_ascii_lowercase().contains("-dx11") {
            return None;
        }
        let name = exe.file_stem()?.to_string_lossy().into_owned();
        Some(Plan { exe, rest: rest_of(&cmd), pids, name })
    }
}

/// Closes the game (politely first, then by force) and starts it again with `-dx11`.
/// Blocks for a few seconds: run it off the UI thread.
pub fn restart(plan: &Plan, game_hwnd: isize) -> Result<(), String> {
    unsafe {
        let _ = PostMessageW(Some(HWND(game_hwnd as _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        for (i, &pid) in plan.pids.iter().enumerate() {
            let Some(h) = open(pid, PROCESS_SYNCHRONIZE | PROCESS_TERMINATE) else { continue };
            let wait = if i == 0 { 8000 } else { 3000 };
            if WaitForSingleObject(h, wait) != WAIT_OBJECT_0 {
                let _ = TerminateProcess(h, 0);
                let _ = WaitForSingleObject(h, 3000);
            }
            let _ = CloseHandle(h);
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    let args = format!("{} -dx11", plan.rest);
    std::process::Command::new(&plan.exe)
        .current_dir(plan.exe.parent().unwrap_or(Path::new(".")))
        .raw_arg(args.trim())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("couldn't start {}: {e}", plan.exe.display()))
}
