// Jackson Coxson
//
// Minimal Win32 FFI for the Apple-mux WinUSB backend.
//
// On Windows with Apple's "Apple Devices" / Mobile Device Support driver
// package installed, the iOS mux interface (MI_01) is bound to in-box
// WinUSB.sys with Apple's UMDF filter `AppleUsbFilter.dll` on top. That
// filter exposes a custom `DeviceIoControl` surface (device type 0x22)
// that we drive directly. We open Apple's registered device interface
// `{664be590-54bd-4964-8a8c-6cd1314f6dc2}` and speak its IOCTL protocol.

#![cfg(target_os = "windows")]

use std::ffi::c_void;

pub type Handle = *mut c_void;
pub type Bool = i32;
pub const FALSE: Bool = 0;

#[inline]
pub fn is_invalid(h: Handle) -> bool {
    h as isize == -1 || h.is_null()
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// Registered by AppleUsbFilter.dll
pub const APPLE_MUX_INTERFACE_GUID: Guid = Guid {
    data1: 0x664be590,
    data2: 0x54bd,
    data3: 0x4964,
    data4: [0x8a, 0x8c, 0x6c, 0xd1, 0x31, 0x4f, 0x6d, 0xc2],
};

// CreateFile
pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const FILE_SHARE_READ: u32 = 0x1;
pub const FILE_SHARE_WRITE: u32 = 0x2;
pub const OPEN_EXISTING: u32 = 3;
pub const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;

// SetupAPI enumeration
pub const DIGCF_PRESENT: u32 = 0x2;
pub const DIGCF_DEVICEINTERFACE: u32 = 0x10;

// error codes
pub const ERROR_IO_PENDING: u32 = 997;
pub const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
pub const ERROR_NO_MORE_ITEMS: u32 = 259;

#[repr(C)]
pub struct SpDeviceInterfaceData {
    pub cb_size: u32,
    pub interface_class_guid: Guid,
    pub flags: u32,
    pub reserved: usize,
}

#[repr(C)]
pub struct Overlapped {
    pub internal: usize,
    pub internal_high: usize,
    pub offset: u32,
    pub offset_high: u32,
    pub h_event: Handle,
}

impl Default for Overlapped {
    fn default() -> Self {
        Self {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            h_event: std::ptr::null_mut(),
        }
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        sa: *mut c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    pub fn CloseHandle(h: Handle) -> Bool;
    pub fn DeviceIoControl(
        h: Handle,
        code: u32,
        in_buf: *const c_void,
        in_size: u32,
        out_buf: *mut c_void,
        out_size: u32,
        returned: *mut u32,
        overlapped: *mut Overlapped,
    ) -> Bool;
    pub fn GetOverlappedResult(
        h: Handle,
        overlapped: *mut Overlapped,
        transferred: *mut u32,
        wait: Bool,
    ) -> Bool;
    pub fn CreateEventW(sa: *mut c_void, manual: Bool, initial: Bool, name: *const u16) -> Handle;
}

#[link(name = "setupapi")]
unsafe extern "system" {
    pub fn SetupDiGetClassDevsW(
        guid: *const Guid,
        enumerator: *const u16,
        hwnd: Handle,
        flags: u32,
    ) -> Handle;
    pub fn SetupDiEnumDeviceInterfaces(
        devinfo: Handle,
        devinfo_data: *mut c_void,
        guid: *const Guid,
        index: u32,
        iface: *mut SpDeviceInterfaceData,
    ) -> Bool;
    pub fn SetupDiGetDeviceInterfaceDetailW(
        devinfo: Handle,
        iface: *mut SpDeviceInterfaceData,
        detail: *mut c_void,
        detail_size: u32,
        required: *mut u32,
        devinfo_data: *mut c_void,
    ) -> Bool;
    pub fn SetupDiDestroyDeviceInfoList(devinfo: Handle) -> Bool;
}

// Apple mux IOCTL codes
// (device type 0x22, reversed from AppleMobileDeviceService_main.dll `Usbmuxio_*`)

/// Control transfer: in = 8-byte setup packet (+ OUT data); out = 8 + IN data.
pub const IOCTL_CONTROL_TRANSFER: u32 = 0x2200A0;
/// Device init / power (0x24-byte in & out). Sent right after open. Required.
pub const IOCTL_INIT: u32 = 0x2200A8;
/// Get pipe properties (0x18-byte in & out; in[0]=1-based pipe index;
/// out[6]=bEndpointAddress, out[4..6]=wMaxPacketSize).
pub const IOCTL_GET_PIPE_PROPERTIES: u32 = 0x2200A4;
/// Get device serial / UDID (0x100-byte in & out; ASCII out).
pub const IOCTL_GET_SERIAL: u32 = 0x2200AC;

/// Bulk read from pipe `p`: `0x220000 | ((p<<2)+0x20) | METHOD_OUT_DIRECT(2)`.
/// (p=1 -> 0x220026, p=2 -> 0x22002A)
#[inline]
pub fn ioctl_read_pipe(pipe: u8) -> u32 {
    0x220000 | (((pipe as u32) << 2) + 0x20) | 2
}
/// Bulk write to pipe `p`: `0x220000 | ((p<<2)+0x40) | METHOD_IN_DIRECT(1)`.
/// NOTE the write base is +0x40, distinct from read's +0x20.
/// (p=1 -> 0x220045, p=2 -> 0x220049)
#[inline]
pub fn ioctl_write_pipe(pipe: u8) -> u32 {
    0x220000 | (((pipe as u32) << 2) + 0x40) | 1
}
/// Abort pipe `p`: `0x220000 | ((p<<2)+0x60)` (preceded by CancelIo in Apple's code).
#[inline]
pub fn ioctl_abort_pipe(pipe: u8) -> u32 {
    0x220000 | (((pipe as u32) << 2) + 0x60)
}
