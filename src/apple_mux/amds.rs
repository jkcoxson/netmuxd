// Jackson Coxson
//
// Find and terminate Apple Mobile Device Service (AMDS).

#![cfg(target_os = "windows")]

use std::io;
use std::ptr;

use log::{info, warn};

use super::ffi;

// Process image names to terminate, matched case-insensitively.
const AMDS_PROCESS_NAMES: &[&str] = &["AppleMobileDeviceService.exe"];

const AMDS_SERVICE_NAME: &str = "Apple Mobile Device Service";

const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
const PROCESS_TERMINATE: u32 = 0x0001;
const SYNCHRONIZE: u32 = 0x0010_0000;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const WAIT_TIMEOUT_MS: u32 = 5000;

// Service Control Manager access rights.
const SC_MANAGER_CONNECT: u32 = 0x0001;
const SERVICE_START: u32 = 0x0010;
const SERVICE_QUERY_STATUS: u32 = 0x0004;
const ERROR_SERVICE_ALREADY_RUNNING: i32 = 1056;

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
    fn QueryFullProcessImageNameW(
        handle: ffi::Handle,
        flags: u32,
        buffer: *mut u16,
        size: *mut u32,
    ) -> ffi::Bool;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenSCManagerW(machine: *const u16, database: *const u16, access: u32) -> ffi::Handle;
    fn OpenServiceW(scm: ffi::Handle, name: *const u16, access: u32) -> ffi::Handle;
    fn StartServiceW(service: ffi::Handle, num_args: u32, args: *const *const u16) -> ffi::Bool;
    fn CloseServiceHandle(handle: ffi::Handle) -> ffi::Bool;
}

/// Snapshot the running processes, terminate any that match AMDS, and return
/// the full executable paths of the ones we killed (so they can later be
/// relaunched via [`restart_amds`]). Blocking Win32 calls; run from a blocking
/// context.
pub fn kill_amds() -> Vec<String> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if ffi::is_invalid(snapshot) {
        warn!(
            "kill-amds: CreateToolhelp32Snapshot failed: {}",
            io::Error::last_os_error()
        );
        return Vec::new();
    }

    let mut entry: ProcessEntry32W = unsafe { std::mem::zeroed() };
    entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;

    let mut paths: Vec<String> = Vec::new();
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) };
    while ok != ffi::FALSE {
        let name = exe_name(&entry.sz_exe_file);
        if AMDS_PROCESS_NAMES
            .iter()
            .any(|n| n.eq_ignore_ascii_case(&name))
        {
            let pid = entry.th32_process_id;
            match terminate(pid) {
                Some(path) => {
                    info!("kill-amds: terminated {name} (pid {pid})");
                    match path {
                        Some(p) if !paths.contains(&p) => paths.push(p),
                        Some(_) => {} // duplicate path, already recorded
                        None => warn!(
                            "kill-amds: couldn't read image path for {name} (pid {pid}); \
                             it won't be restarted on exit"
                        ),
                    }
                }
                None => warn!(
                    "kill-amds: failed to terminate {name} (pid {pid}): {}",
                    io::Error::last_os_error()
                ),
            }
        }
        ok = unsafe { Process32NextW(snapshot, &mut entry) };
    }

    unsafe { ffi::CloseHandle(snapshot) };
    paths
}

pub fn restart_amds(paths: &[String]) {
    match start_amds_service() {
        ServiceStart::Started | ServiceStart::AlreadyRunning => return,
        ServiceStart::NoService => {
            warn!(
                "restart-amds: service \"{AMDS_SERVICE_NAME}\" not found; \
                 falling back to relaunching the killed exe(s)"
            );
        }
        ServiceStart::Failed => {
            warn!("restart-amds: could not start service; falling back to relaunching the exe(s)");
        }
    }

    if paths.is_empty() {
        warn!("restart-amds: nothing to relaunch");
        return;
    }
    for path in paths {
        match std::process::Command::new(path).spawn() {
            Ok(child) => info!("restart-amds: relaunched {path} (pid {})", child.id()),
            Err(e) => warn!("restart-amds: failed to relaunch {path}: {e}"),
        }
    }
}

enum ServiceStart {
    Started,
    AlreadyRunning,
    /// The service isn't installed on this machine.
    NoService,
    /// The service exists but couldn't be started (e.g. not elevated).
    Failed,
}

fn start_amds_service() -> ServiceStart {
    let scm = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) };
    if ffi::is_invalid(scm) {
        warn!(
            "restart-amds: OpenSCManager failed: {}",
            io::Error::last_os_error()
        );
        return ServiceStart::Failed;
    }

    let name = wide(AMDS_SERVICE_NAME);
    let service = unsafe { OpenServiceW(scm, name.as_ptr(), SERVICE_START | SERVICE_QUERY_STATUS) };
    if ffi::is_invalid(service) {
        let err = io::Error::last_os_error();
        unsafe { CloseServiceHandle(scm) };
        // 1060 = ERROR_SERVICE_DOES_NOT_EXIST
        return if err.raw_os_error() == Some(1060) {
            ServiceStart::NoService
        } else {
            warn!("restart-amds: OpenService(\"{AMDS_SERVICE_NAME}\") failed: {err}");
            ServiceStart::Failed
        };
    }

    let ok = unsafe { StartServiceW(service, 0, ptr::null()) };
    let result = if ok != ffi::FALSE {
        info!("restart-amds: started service \"{AMDS_SERVICE_NAME}\"");
        ServiceStart::Started
    } else {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_SERVICE_ALREADY_RUNNING) {
            info!("restart-amds: service \"{AMDS_SERVICE_NAME}\" already running");
            ServiceStart::AlreadyRunning
        } else {
            warn!("restart-amds: StartService(\"{AMDS_SERVICE_NAME}\") failed: {err}");
            ServiceStart::Failed
        }
    };

    unsafe { CloseServiceHandle(service) };
    unsafe { CloseServiceHandle(scm) };
    result
}

/// Encode a `&str` as a NUL-terminated UTF-16 buffer for the wide Win32 APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn terminate(pid: u32) -> Option<Option<String>> {
    let handle = unsafe {
        OpenProcess(
            PROCESS_TERMINATE | SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            ffi::FALSE,
            pid,
        )
    };
    if ffi::is_invalid(handle) {
        return None;
    }
    // Read the image path before killing — it's gone once the process exits.
    let path = full_image_path(handle);
    let ok = unsafe { TerminateProcess(handle, 1) };
    if ok != ffi::FALSE {
        unsafe { WaitForSingleObject(handle, WAIT_TIMEOUT_MS) };
    }
    unsafe { ffi::CloseHandle(handle) };
    if ok != ffi::FALSE { Some(path) } else { None }
}

fn full_image_path(handle: ffi::Handle) -> Option<String> {
    // Extended-length path limit; QueryFullProcessImageNameW writes the char
    // count actually used back into `size`.
    let mut buf = vec![0u16; 32768];
    let mut size = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size) };
    if ok == ffi::FALSE || size == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..size as usize]))
}

/// Decode the NUL-terminated UTF-16 image name from a `PROCESSENTRY32W`.
fn exe_name(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
