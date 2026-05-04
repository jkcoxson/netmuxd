// Jackson Coxson
//
// Driver-binding installer for the `netmuxd install` subcommand. Wraps
// libwdi to bind `libusb0.sys` (libusb-win32 kernel driver) to every
// Apple iOS device. Without this binding,
// `usbccgp.sys` owns the parent and libusbK can't issue
// SetConfiguration.
//
// We diverge from libwdi's default behavior in one important way: we
// supply our own INF rather than let libwdi auto-generate one per
// device. The auto-generated INF only declares ONE hardware ID (the
// VID+PID of the device libwdi was passed), which means a phone that
// later enumerates as a different PID in our range needs another
// `install` run. Our hand-written INF lists every PID in
// `0x1290..=0x12AF` so the install sticks for the whole device class.
//
// libwdi's `external_inf=TRUE` mode is what makes this work: libwdi
// still extracts the embedded driver binaries (libusb0.sys + the
// libusb0 user-mode DLLs), still tokenizes our INF, still generates
// and signs the .cat against it, and still imports a self-signed CA
// into Trusted Publishers. We just provide the INF text.
//
// ARM64 caveat: libwdi's self-signed CA is rejected by
// Windows on ARM64. Test mode (`bcdedit /set testsigning on`) unblocks
// development; production deployment to ARM64 needs Microsoft
// attestation signing of the package.

use std::ffi::{CStr, CString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;

mod ffi;

use crate::usb::{APPLE_VID, PID_RANGE_HIGH, PID_RANGE_LOW};

const VENDOR_NAME: &str = "Apple Inc.";
const INF_NAME: &str = "netmuxd_libusb0.inf";
const CAT_NAME: &str = "netmuxd_libusb0.cat";

// libusb0's "default device" GUID.
const DEVICE_INTERFACE_GUID: &str = "{20343A29-6DA1-4DB8-8A3C-16E774057BF5}";

#[derive(Debug)]
enum DeviceOutcome {
    /// libwdi successfully bound the driver to a connected device.
    Installed,
    /// Connected device was already on libusb0; no action needed.
    AlreadyBound,
    Failed(String),
}

pub fn run_install() -> i32 {
    unsafe {
        ffi::wdi_set_log_level(ffi::WDI_LOG_LEVEL_INFO);
    }

    if std::env::var_os("NETMUXD_DEBUG_DEVLIST").is_some()
        && let Err(e) = dump_libwdi_device_list()
    {
        eprintln!("Device list dump failed: {e}");
    }

    println!("netmuxd install: scanning for iOS devices...");

    let candidates = match enumerate_apple_devices() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to enumerate USB devices: {e}");
            return 1;
        }
    };

    if candidates.is_empty() {
        eprintln!("No iOS device plugged in. With our self-signed INF, Windows' driver-match",);
        eprintln!("ranking picks Apple's WHQL-signed `appleusb.inf` over ours, so we can't",);
        eprintln!("win on first plug-in via staging alone. The device must be present so",);
        eprintln!("libwdi can `UpdateDriverForPlugAndPlayDevices` with INSTALLFLAG_FORCE,",);
        eprintln!("bypassing ranking. Plug an iPhone in (mode 0/3) and re-run. If iTunes /",);
        eprintln!("Apple Mobile Device Support is installed, uninstall it first.",);
        eprintln!("Reboot after.",);
        return 2;
    }

    println!("Currently-connected matches (will be rebound):");
    for (vid, pid, desc, current_driver) in &candidates {
        println!(
            "  {desc}: VID=0x{vid:04x} PID=0x{pid:04x} (current: {})",
            current_driver.as_deref().unwrap_or("<none>"),
        );
    }

    let prepare_path = match prepare_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to set up libwdi extract directory: {e}");
            return 1;
        }
    };

    if let Err(e) = write_libusb0_inf(&prepare_path) {
        eprintln!("Failed to write INF template: {e}");
        return 1;
    }

    let outcomes = match install_each(&prepare_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Install pass failed: {e}");
            return 1;
        }
    };

    let mut installed = 0usize;
    let mut already = 0usize;
    let mut failed = 0usize;
    for (label, outcome) in outcomes {
        match outcome {
            DeviceOutcome::Installed => {
                println!("  [OK]   {label}");
                installed += 1;
            }
            DeviceOutcome::AlreadyBound => {
                println!("  [skip] {label}: already on libusb0");
                already += 1;
            }
            DeviceOutcome::Failed(msg) => {
                eprintln!("  [FAIL] {label}: {msg}");
                failed += 1;
            }
        }
    }

    println!("\nDone. {installed} device(s) bound, {already} already bound, {failed} failed.",);

    if failed > 0 { 1 } else { 0 }
}

/// Generate the unsigned driver-package files (INF + .cat with file
/// hashes + libusb-win32 driver binaries) into a user-specified
/// directory, ready for signing with a real cert. Skips libwdi's
/// self-signing pass so the .cat is left unsigned for the caller to
/// sign with their own toolchain.
pub fn run_export(out_dir: &str) -> i32 {
    unsafe {
        ffi::wdi_set_log_level(ffi::WDI_LOG_LEVEL_INFO);
    }

    let path = PathBuf::from(out_dir);
    if let Err(e) = fs::create_dir_all(&path) {
        eprintln!("Failed to create output dir {path:?}: {e}");
        return 1;
    }
    let prepare_path = match path.to_str() {
        Some(s) => s.to_string(),
        None => {
            eprintln!("Output dir path is not valid UTF-8: {path:?}");
            return 1;
        }
    };

    println!("netmuxd export-driver: writing unsigned package to {prepare_path}");

    if let Err(e) = write_libusb0_inf(&prepare_path) {
        eprintln!("Failed to write INF template: {e}");
        return 1;
    }

    if let Err(e) = unsafe { prepare_export(&prepare_path) } {
        eprintln!("libwdi prepare failed: {e}");
        return 1;
    }

    println!();
    println!("Wrote unsigned driver package:");
    if let Ok(entries) = fs::read_dir(&prepare_path) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let kind = if entry.path().is_dir() { "<dir>" } else { "" };
            println!("  {kind:<5} {}", name.to_string_lossy());
        }
    }
    0
}

pub fn run_uninstall() -> i32 {
    println!("netmuxd uninstall: scanning Windows INF directory...");

    let inf_dir = std::env::var_os("WINDIR")
        .map(|s| PathBuf::from(s).join("INF"))
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\INF"));

    let entries = match fs::read_dir(&inf_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Failed to read {inf_dir:?}: {e}");
            eprintln!("Re-run from an elevated terminal — reading the INF directory and");
            eprintln!("running `pnputil /delete-driver` both require administrator rights.");
            return 1;
        }
    };

    let mut matches: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        if !(lower.starts_with("oem") && lower.ends_with(".inf")) {
            continue;
        }
        let Some(content) = read_inf_text(&path) else {
            continue;
        };
        if content.contains("netmuxd_libusb0") {
            matches.push(name.to_string());
        }
    }

    if matches.is_empty() {
        println!("No netmuxd-installed driver packages found in {inf_dir:?}.");
        return 0;
    }

    println!("Found {} package(s) to remove:", matches.len());
    for name in &matches {
        println!("  {name}");
    }

    let mut failed = 0usize;
    for name in &matches {
        let output = Command::new("pnputil")
            .args(["/delete-driver", name, "/uninstall", "/force"])
            .output();
        match output {
            Ok(o) if o.status.success() => {
                println!("  [OK]   {name} removed");
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let stdout = String::from_utf8_lossy(&o.stdout);
                eprintln!(
                    "  [FAIL] {name}: pnputil exit {}",
                    o.status.code().unwrap_or(-1),
                );
                let combined = format!("{stdout}\n{stderr}");
                for line in combined.lines().filter(|l| !l.trim().is_empty()) {
                    eprintln!("         {line}");
                }
                failed += 1;
            }
            Err(e) => {
                eprintln!("  [FAIL] {name}: failed to run pnputil: {e}");
                failed += 1;
            }
        }
    }

    let removed = matches.len() - failed;
    println!("\nDone. {removed} removed, {failed} failed.");

    if failed > 0 { 1 } else { 0 }
}

fn read_inf_text(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let utf16: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(String::from_utf16_lossy(&utf16))
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        let utf16: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        Some(String::from_utf16_lossy(&utf16))
    } else {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn dump_libwdi_device_list() -> Result<(), String> {
    println!("--- libwdi device list (NETMUXD_DEBUG_DEVLIST) ---");
    with_device_list(|head| {
        if head.is_null() {
            println!("  (empty list)");
            return Ok(());
        }
        for dev in DeviceIter::new(head) {
            unsafe {
                let vid = (*dev).vid;
                let pid = (*dev).pid;
                let mi = (*dev).mi;
                let is_comp = (*dev).is_composite;
                let desc = cstr_to_string((*dev).desc).unwrap_or_else(|| "<no desc>".into());
                let drv = cstr_to_string((*dev).driver);
                let dev_id = cstr_to_string((*dev).device_id);
                let hw_id = cstr_to_string((*dev).hardware_id);
                println!(
                    "  VID=0x{vid:04x} PID=0x{pid:04x} mi={mi} is_composite={is_comp} drv={:?}",
                    drv,
                );
                println!("    desc={desc}");
                println!("    device_id={:?}", dev_id);
                println!("    hardware_id={:?}", hw_id);
            }
        }
        Ok(())
    })?;
    println!("--- end ---\n");
    Ok(())
}

type EnumerationReturn = Vec<(u16, u16, String, Option<String>)>;
fn enumerate_apple_devices() -> Result<EnumerationReturn, String> {
    let mut out = Vec::new();
    with_device_list(|head| {
        for dev in DeviceIter::new(head) {
            unsafe {
                if !is_target_composite_parent(dev) {
                    continue;
                }
                let desc = cstr_to_string((*dev).desc).unwrap_or_else(|| "<no desc>".into());
                let drv = cstr_to_string((*dev).driver);
                out.push(((*dev).vid, (*dev).pid, desc, drv));
            }
        }
        Ok(())
    })?;
    Ok(out)
}

/// Drive the install pass.
///
/// 1. For every connected matching device that isn't already on
///    libusb0, call `wdi_install_driver` so it's bound immediately.
///    The first such call does the slow work
///    — cat sign, cert import, file copy to driver store; subsequent
///    calls reuse the staged package.
/// 2. If no `wdi_install_driver` call fired in step 1 (because nothing
///    was connected, or all connected matches were already on
///    libusb0), do *one* call against a synthesized anchor device so
///    libwdi's installer.exe falls through to `SetupCopyOEMInfU` and
///    stages the package for future plug-ins. Without this, an
///    "install with no iPhone connected" run would never actually put
///    our INF in the driver store.
fn install_each(prepare_path: &str) -> Result<Vec<(String, DeviceOutcome)>, String> {
    unsafe {
        prepare_once(prepare_path)?;
    }

    let mut results = Vec::new();
    with_device_list(|head| {
        for dev in DeviceIter::new(head) {
            unsafe {
                if !is_target_composite_parent(dev) {
                    continue;
                }
                let vid = (*dev).vid;
                let pid = (*dev).pid;
                let label = format!(
                    "VID=0x{vid:04x} PID=0x{pid:04x} ({})",
                    cstr_to_string((*dev).desc).unwrap_or_else(|| "<no desc>".into()),
                );
                if let Some(drv) = cstr_to_string((*dev).driver)
                    && drv.eq_ignore_ascii_case("libusb0")
                {
                    results.push((label, DeviceOutcome::AlreadyBound));
                    continue;
                }
                let outcome = match install_only(dev, prepare_path) {
                    Ok(()) => DeviceOutcome::Installed,
                    Err(msg) => DeviceOutcome::Failed(msg),
                };
                results.push((label, outcome));
            }
        }
        Ok(())
    })?;
    Ok(results)
}

/// Run `wdi_prepare_driver` once for the whole install pass: extract
/// embedded driver binaries, tokenize our INF (no-op since it has no
/// `#TOKEN#` markers), generate a self-signed cert, and sign the
/// `.cat`.
unsafe fn prepare_once(prepare_path: &str) -> Result<(), String> {
    let path_c = CString::new(prepare_path).map_err(|e| e.to_string())?;
    let inf_full = format!("{prepare_path}\\{INF_NAME}");
    let inf_c = CString::new(inf_full).map_err(|e| e.to_string())?;
    let vendor_c = CString::new(VENDOR_NAME).map_err(|e| e.to_string())?;

    let mut synth = SynthDevice::new()?;
    let mut prep = ffi::wdi_options_prepare_driver {
        driver_type: ffi::WDI_LIBUSB0,
        vendor_name: vendor_c.as_ptr(),
        device_guid: ptr::null(),
        disable_cat: ffi::FALSE,
        disable_signing: ffi::FALSE,
        cert_subject: ptr::null(),
        use_wcid_driver: ffi::FALSE,
        external_inf: ffi::TRUE,
    };
    let rc = unsafe {
        ffi::wdi_prepare_driver(
            synth.as_mut_ptr(),
            path_c.as_ptr(),
            inf_c.as_ptr(),
            &mut prep,
        )
    };
    if rc != ffi::WDI_SUCCESS {
        return Err(format!("wdi_prepare_driver: {}", wdi_err(rc)));
    }
    Ok(())
}

/// Like `prepare_once`, but with `disable_signing=TRUE` so libwdi
/// generates the .cat (with the right file hashes) but leaves it
/// unsigned. Used by `run_export` so the caller can sign the .cat
/// themselves with a real cert.
unsafe fn prepare_export(prepare_path: &str) -> Result<(), String> {
    let path_c = CString::new(prepare_path).map_err(|e| e.to_string())?;
    let inf_full = format!("{prepare_path}\\{INF_NAME}");
    let inf_c = CString::new(inf_full).map_err(|e| e.to_string())?;
    let vendor_c = CString::new(VENDOR_NAME).map_err(|e| e.to_string())?;

    let mut synth = SynthDevice::new()?;
    let mut prep = ffi::wdi_options_prepare_driver {
        driver_type: ffi::WDI_LIBUSB0,
        vendor_name: vendor_c.as_ptr(),
        device_guid: ptr::null(),
        disable_cat: ffi::FALSE,
        disable_signing: ffi::TRUE,
        cert_subject: ptr::null(),
        use_wcid_driver: ffi::FALSE,
        external_inf: ffi::TRUE,
    };
    let rc = unsafe {
        ffi::wdi_prepare_driver(
            synth.as_mut_ptr(),
            path_c.as_ptr(),
            inf_c.as_ptr(),
            &mut prep,
        )
    };
    if rc != ffi::WDI_SUCCESS {
        return Err(format!("wdi_prepare_driver: {}", wdi_err(rc)));
    }
    Ok(())
}

/// Run `wdi_install_driver` for one device. Assumes `prepare_once`
/// has already run against `prepare_path`. If `dev` has a present
/// matching hardware ID, libwdi's elevated installer.exe binds it
/// directly via `UpdateDriverForPlugAndPlayDevices`.
unsafe fn install_only(dev: *mut ffi::wdi_device_info, prepare_path: &str) -> Result<(), String> {
    let path_c = CString::new(prepare_path).map_err(|e| e.to_string())?;
    let inf_full = format!("{prepare_path}\\{INF_NAME}");
    let inf_c = CString::new(inf_full).map_err(|e| e.to_string())?;

    let mut install = ffi::wdi_options_install_driver {
        hWnd: ptr::null_mut(),
        install_filter_driver: ffi::FALSE,
        // 5-minute timeout for any concurrent SetupAPI installs to
        // settle. libwdi blocks here until they're done.
        pending_install_timeout: 300_000,
    };
    let rc = unsafe { ffi::wdi_install_driver(dev, path_c.as_ptr(), inf_c.as_ptr(), &mut install) };
    if rc != ffi::WDI_SUCCESS {
        return Err(format!("wdi_install_driver: {}", wdi_err(rc)));
    }
    Ok(())
}

struct SynthDevice {
    info: ffi::wdi_device_info,
    _desc: CString,
    _driver: CString,
    _device_id: CString,
    _hardware_id: CString,
}

impl SynthDevice {
    fn new() -> Result<Self, String> {
        let desc = CString::new("Apple iPhone-class device (libwdi staging anchor)")
            .map_err(|e| e.to_string())?;
        let driver = CString::new("").map_err(|e| e.to_string())?;
        let id = format!("USB\\VID_{:04X}&PID_{:04X}", APPLE_VID, PID_RANGE_LOW);
        let device_id = CString::new(id.clone()).map_err(|e| e.to_string())?;
        let hardware_id = CString::new(id).map_err(|e| e.to_string())?;

        let info = ffi::wdi_device_info {
            next: ptr::null_mut(),
            vid: APPLE_VID,
            pid: PID_RANGE_LOW,
            is_composite: ffi::TRUE,
            mi: 0,
            desc: desc.as_ptr() as *mut _,
            driver: driver.as_ptr() as *mut _,
            device_id: device_id.as_ptr() as *mut _,
            hardware_id: hardware_id.as_ptr() as *mut _,
            compatible_id: ptr::null_mut(),
            upper_filter: ptr::null_mut(),
            driver_version: 0,
        };

        Ok(Self {
            info,
            _desc: desc,
            _driver: driver,
            _device_id: device_id,
            _hardware_id: hardware_id,
        })
    }

    fn as_mut_ptr(&mut self) -> *mut ffi::wdi_device_info {
        &mut self.info
    }
}

fn prepare_path() -> Result<String, String> {
    let mut p: PathBuf = std::env::temp_dir();
    p.push(format!("netmuxd-libwdi-install-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).map_err(|e| format!("create_dir_all({:?}): {e}", p))?;
    p.to_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("non-UTF-8 temp path: {p:?}"))
}

/// Write `netmuxd_libusb0.inf` into `prepare_path` covering every
/// hardware ID in our PID range. libwdi's `wdi_tokenize_file` will
/// pass the file through and look for `#TOKEN#` markers.
fn write_libusb0_inf(prepare_path: &str) -> std::io::Result<()> {
    let path = PathBuf::from(prepare_path).join(INF_NAME);
    fs::write(&path, build_libusb0_inf())
}

fn build_libusb0_inf() -> String {
    let mut strings_pids = String::new();
    let mut devices_default = String::new();
    let mut devices_nt = String::new();
    let mut devices_amd64 = String::new();
    let mut devices_arm64 = String::new();
    for pid in PID_RANGE_LOW..=PID_RANGE_HIGH {
        let key = format!("DeviceName_{pid:04X}");
        let hw_id = format!("USB\\VID_{:04X}&PID_{:04X}", APPLE_VID, pid);
        strings_pids.push_str(&format!(
            "{key} = \"Apple iPhone-class device (libusb-win32) PID 0x{pid:04X}\"\n",
        ));
        devices_default.push_str(&format!("%{key}% = LIBUSB_WIN32_DEV, {hw_id}\n"));
        devices_nt.push_str(&format!("%{key}% = LIBUSB_WIN32_DEV.NT, {hw_id}\n"));
        devices_amd64.push_str(&format!("%{key}% = LIBUSB_WIN32_DEV.NTAMD64, {hw_id}\n"));
        devices_arm64.push_str(&format!("%{key}% = LIBUSB_WIN32_DEV.NTARM64, {hw_id}\n"));
    }

    format!(
        r#"; netmuxd_libusb0.inf — auto-generated by `netmuxd install`.
; Covers Apple VID 0x{vid:04X}, PID 0x{lo:04X}..=0x{hi:04X}.
; Adapted from libwdi/libusb0.inf.in (libusb-win32 LGPL).
[Strings]
VendorName = "{vendor}"
SourceName = "Apple iPhone (libusb-win32) Install Disk"
DeviceGUID = "{guid}"
{strings_pids}
[Version]
Signature   = "$Windows NT$"
Class       = "libusb-win32 devices"
ClassGuid   = {{EB781AAF-9C70-4523-A5DF-642A87ECA567}}
Provider    = "libusb-win32"
CatalogFile = {cat}
DriverVer   = 01/01/2026, 1.5.0.0

[ClassInstall32]
Addreg = libusb_class_install_add_reg

[libusb_class_install_add_reg]
HKR,,,0,"libusb-win32 devices"
HKR,,Icon,,-20

[Manufacturer]
%VendorName% = Devices, NT, NTAMD64, NTARM64

[SourceDisksNames]
1 = %SourceName%

[SourceDisksFiles.x86]
libusb0.sys     = 1,x86
libusb0_x86.dll = 1,x86

[SourceDisksFiles.amd64]
libusb0.sys     = 1,amd64
libusb0.dll     = 1,amd64
libusb0_x86.dll = 1,x86

[SourceDisksFiles.arm64]
libusb0.sys     = 1,arm64
libusb0.dll     = 1,arm64

[DestinationDirs]
libusb_files_sys       = 10,system32\drivers
libusb_files_dll       = 10,system32
libusb_files_dll_wow64 = 10,syswow64
libusb_files_dll_x86   = 10,system32

[libusb_files_sys]
libusb0.sys

[libusb_files_dll]
libusb0.dll

[libusb_files_dll_x86]
libusb0.dll, libusb0_x86.dll

[libusb_files_dll_wow64]
libusb0.dll, libusb0_x86.dll

[LIBUSB_WIN32_DEV.NT]
CopyFiles = libusb_files_sys, libusb_files_dll_x86

[LIBUSB_WIN32_DEV.NTAMD64]
CopyFiles = libusb_files_sys, libusb_files_dll, libusb_files_dll_wow64

[LIBUSB_WIN32_DEV.NTARM64]
CopyFiles = libusb_files_sys, libusb_files_dll

[LIBUSB_WIN32_DEV.NT.HW]
DelReg = libusb_del_reg_hw
AddReg = libusb_add_reg_hw

[LIBUSB_WIN32_DEV.NTAMD64.HW]
DelReg = libusb_del_reg_hw
AddReg = libusb_add_reg_hw

[LIBUSB_WIN32_DEV.NTARM64.HW]
DelReg = libusb_del_reg_hw
AddReg = libusb_add_reg_hw

[LIBUSB_WIN32_DEV.NT.Services]
AddService = libusb0, 0x00000002, libusb_add_service

[LIBUSB_WIN32_DEV.NTAMD64.Services]
AddService = libusb0, 0x00000002, libusb_add_service

[LIBUSB_WIN32_DEV.NTARM64.Services]
AddService = libusb0, 0x00000002, libusb_add_service

[libusb_del_reg_hw]
HKR,,LowerFilters
HKR,,UpperFilters

[libusb_add_reg_hw]
HKR,,SurpriseRemovalOK,0x00010001,1
HKR,,DeviceInterfaceGUIDs,0x00010000,%DeviceGUID%

[libusb_add_service]
DisplayName   = "libusb-win32 - Kernel Driver 01/01/2026 1.5.0.0"
ServiceType   = 1
StartType     = 3
ErrorControl  = 0
ServiceBinary = %12%\libusb0.sys

[Devices]
{devices_default}
[Devices.NT]
{devices_nt}
[Devices.NTAMD64]
{devices_amd64}
[Devices.NTARM64]
{devices_arm64}
"#,
        vid = APPLE_VID,
        lo = PID_RANGE_LOW,
        hi = PID_RANGE_HIGH,
        vendor = VENDOR_NAME,
        guid = DEVICE_INTERFACE_GUID,
        cat = CAT_NAME,
    )
}

// --- helpers -----------------------------------------------------------

fn is_apple_mux(vid: u16, pid: u16) -> bool {
    vid == APPLE_VID && (PID_RANGE_LOW..=PID_RANGE_HIGH).contains(&pid)
}

unsafe fn is_target_composite_parent(dev: *mut ffi::wdi_device_info) -> bool {
    if dev.is_null() {
        return false;
    }
    unsafe { is_apple_mux((*dev).vid, (*dev).pid) && (*dev).is_composite == ffi::FALSE }
}

fn with_device_list<F>(f: F) -> Result<(), String>
where
    F: FnOnce(*mut ffi::wdi_device_info) -> Result<(), String>,
{
    let mut opts = ffi::wdi_options_create_list {
        list_all: ffi::TRUE,
        list_hubs: ffi::TRUE,
        trim_whitespaces: ffi::TRUE,
    };
    let mut head: *mut ffi::wdi_device_info = ptr::null_mut();
    let rc = unsafe { ffi::wdi_create_list(&mut head, &mut opts) };
    if rc == ffi::WDI_ERROR_NO_DEVICE {
        return f(ptr::null_mut());
    }
    if rc != ffi::WDI_SUCCESS {
        return Err(format!("wdi_create_list: {}", wdi_err(rc)));
    }
    let result = f(head);
    unsafe {
        let _ = ffi::wdi_destroy_list(head);
    }
    result
}

struct DeviceIter {
    next: *mut ffi::wdi_device_info,
}

impl DeviceIter {
    fn new(head: *mut ffi::wdi_device_info) -> Self {
        Self { next: head }
    }
}

impl Iterator for DeviceIter {
    type Item = *mut ffi::wdi_device_info;
    fn next(&mut self) -> Option<*mut ffi::wdi_device_info> {
        if self.next.is_null() {
            return None;
        }
        let cur = self.next;
        unsafe {
            self.next = (*cur).next;
        }
        Some(cur)
    }
}

fn wdi_err(code: std::os::raw::c_int) -> String {
    unsafe {
        let p = ffi::wdi_strerror(code);
        if p.is_null() {
            return format!("wdi error {code}");
        }
        match CStr::from_ptr(p).to_str() {
            Ok(s) => format!("{s} ({code})"),
            Err(_) => format!("wdi error {code} (non-UTF-8 message)"),
        }
    }
}

fn cstr_to_string(p: *const std::os::raw::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe {
        match CStr::from_ptr(p).to_str() {
            Ok(s) if !s.is_empty() => Some(s.to_string()),
            _ => None,
        }
    }
}
