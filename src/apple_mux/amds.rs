// Jackson Coxson
//
// Find and terminate Apple Mobile Device Service (AMDS).

#![cfg(target_os = "windows")]

use std::io;

use log::{info, warn};

use super::ffi;

// Process image names to terminate, matched case-insensitively.
const AMDS_PROCESS_NAMES: &[&str] = &["AppleMobileDeviceService.exe"];

const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
const PROCESS_TERMINATE: u32 = 0x0001;
const SYNCHRONIZE: u32 = 0x0010_0000;
const WAIT_TIMEOUT_MS: u32 = 5000;

#[repr(C)]
struct ProcessEntry32W {
    dw_size: u32,
    cnt_usage: u32,
    th32_process_id: u32,
    th32_default_heap_id: usize,
    th32_module_id: u32,
    cnt_threads: u32,
    th32_parent_process_id: u32,
    pc_pri_class_base: i32,
    dw_flags: u32,
    sz_exe_file: [u16; 260],
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> ffi::Handle;
    fn Process32FirstW(snapshot: ffi::Handle, entry: *mut ProcessEntry32W) -> ffi::Bool;
    fn Process32NextW(snapshot: ffi::Handle, entry: *mut ProcessEntry32W) -> ffi::Bool;
    fn OpenProcess(access: u32, inherit: ffi::Bool, pid: u32) -> ffi::Handle;
    fn TerminateProcess(process: ffi::Handle, exit_code: u32) -> ffi::Bool;
    fn WaitForSingleObject(handle: ffi::Handle, milliseconds: u32) -> u32;
}

/// Snapshot the running processes, terminate any that match AMDS, and return
/// how many were killed. Blocking Win32 calls; run from a blocking context.
pub fn kill_amds() -> u32 {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if ffi::is_invalid(snapshot) {
        warn!(
            "kill-amds: CreateToolhelp32Snapshot failed: {}",
            io::Error::last_os_error()
        );
        return 0;
    }

    let mut entry: ProcessEntry32W = unsafe { std::mem::zeroed() };
    entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;

    let mut killed = 0u32;
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) };
    while ok != ffi::FALSE {
        let name = exe_name(&entry.sz_exe_file);
        if AMDS_PROCESS_NAMES
            .iter()
            .any(|n| n.eq_ignore_ascii_case(&name))
        {
            let pid = entry.th32_process_id;
            if terminate(pid) {
                info!("kill-amds: terminated {name} (pid {pid})");
                killed += 1;
            } else {
                warn!(
                    "kill-amds: failed to terminate {name} (pid {pid}): {}",
                    io::Error::last_os_error()
                );
            }
        }
        ok = unsafe { Process32NextW(snapshot, &mut entry) };
    }

    unsafe { ffi::CloseHandle(snapshot) };
    killed
}

fn terminate(pid: u32) -> bool {
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, ffi::FALSE, pid) };
    if ffi::is_invalid(handle) {
        return false;
    }
    let ok = unsafe { TerminateProcess(handle, 1) };
    if ok != ffi::FALSE {
        unsafe { WaitForSingleObject(handle, WAIT_TIMEOUT_MS) };
    }
    unsafe { ffi::CloseHandle(handle) };
    ok != ffi::FALSE
}

/// Decode the NUL-terminated UTF-16 image name from a `PROCESSENTRY32W`.
fn exe_name(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
