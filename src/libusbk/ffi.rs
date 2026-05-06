// Jackson Coxson
//
// Reference: vendor/libusbK/include/libusbk.h. The DLL exports use the
// stdcall convention (`KUSB_API` -> `WINAPI`). On x86_64 Windows there is
// only one calling convention so `extern "system"` is correct; it also
// stays correct if we ever build for x86.

#![cfg(target_os = "windows")]
#![allow(non_snake_case, non_camel_case_types, dead_code)]
#![allow(clippy::upper_case_acronyms)]

use std::os::raw::c_void;

// --- Windows base types ------------------------------------------------

pub type BOOL = i32;
pub type UCHAR = u8;
pub type USHORT = u16;
pub type UINT = u32;
pub type INT = i32;
pub type HANDLE = *mut c_void;

pub const TRUE: BOOL = 1;
pub const FALSE: BOOL = 0;

// Minimal OVERLAPPED. We only ever pass a *mut OVERLAPPED that the
// caller has already initialized; we don't read its fields from Rust.
#[repr(C)]
pub struct OVERLAPPED {
    pub internal: usize,
    pub internal_high: usize,
    pub offset: u32,
    pub offset_high: u32,
    pub h_event: HANDLE,
}
pub type LPOVERLAPPED = *mut OVERLAPPED;

// WINUSB_SETUP_PACKET (8 bytes, packed). Same wire layout as the
// standard USB control setup packet.
#[repr(C, packed)]
#[derive(Copy, Clone, Default)]
pub struct WINUSB_SETUP_PACKET {
    pub RequestType: UCHAR,
    pub Request: UCHAR,
    pub Value: USHORT,
    pub Index: USHORT,
    pub Length: USHORT,
}

// --- libusbK opaque handles --------------------------------------------

pub type KLIB_HANDLE = *mut c_void;
pub type KUSB_HANDLE = KLIB_HANDLE;
pub type KLST_HANDLE = KLIB_HANDLE;

pub const KLST_STRING_MAX_LEN: usize = 256;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct KLST_DEV_COMMON_INFO {
    pub Vid: INT,
    pub Pid: INT,
    pub MI: INT,
    pub InstanceID: [u8; KLST_STRING_MAX_LEN],
}

// KLST_SYNC_FLAG is `enum`, but C enums under MSVC are always `int`. We
// only ever read the field, never compare against named values directly
// from Rust, so an `i32` is safe.
pub type KLST_SYNC_FLAG = i32;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct KLST_DEVINFO {
    pub Common: KLST_DEV_COMMON_INFO,
    pub DriverID: INT,
    pub DeviceInterfaceGUID: [u8; KLST_STRING_MAX_LEN],
    pub DeviceID: [u8; KLST_STRING_MAX_LEN],
    pub ClassGUID: [u8; KLST_STRING_MAX_LEN],
    pub Mfg: [u8; KLST_STRING_MAX_LEN],
    pub DeviceDesc: [u8; KLST_STRING_MAX_LEN],
    pub Service: [u8; KLST_STRING_MAX_LEN],
    pub SymbolicLink: [u8; KLST_STRING_MAX_LEN],
    pub DevicePath: [u8; KLST_STRING_MAX_LEN],
    pub LUsb0FilterIndex: INT,
    pub Connected: BOOL,
    pub SyncFlags: KLST_SYNC_FLAG,
    pub BusNumber: INT,
    pub DeviceAddress: INT,
    pub SerialNumber: [u8; KLST_STRING_MAX_LEN],
}

pub type KLST_DEVINFO_HANDLE = *mut KLST_DEVINFO;

// KLST_FLAG bits we may pass to LstK_Init.
pub type KLST_FLAG = u32;
pub const KLST_FLAG_NONE: KLST_FLAG = 0;
pub const KLST_FLAG_INCLUDE_RAWGUID: KLST_FLAG = 0x0001;
pub const KLST_FLAG_INCLUDE_DISCONNECT: KLST_FLAG = 0x0002;

// --- DLL imports -------------------------------------------------------

#[link(name = "libusbK", kind = "dylib")]
unsafe extern "system" {
    // Process-wide context. Optional; the DLL auto-initializes on first
    // use. We expose the symbols anyway so callers can be explicit.
    pub fn LibK_Context_Init(Heap: HANDLE, Reserved: *mut c_void) -> BOOL;
    pub fn LibK_Context_Free();

    // Device list.
    pub fn LstK_Init(DeviceList: *mut KLST_HANDLE, Flags: KLST_FLAG) -> BOOL;
    pub fn LstK_Free(DeviceList: KLST_HANDLE) -> BOOL;
    pub fn LstK_MoveReset(DeviceList: KLST_HANDLE);
    pub fn LstK_MoveNext(DeviceList: KLST_HANDLE, DeviceInfo: *mut KLST_DEVINFO_HANDLE) -> BOOL;
    pub fn LstK_Current(DeviceList: KLST_HANDLE, DeviceInfo: *mut KLST_DEVINFO_HANDLE) -> BOOL;
    pub fn LstK_Count(DeviceList: KLST_HANDLE, Count: *mut UINT) -> BOOL;

    // Per-device handle lifecycle.
    pub fn UsbK_Init(InterfaceHandle: *mut KUSB_HANDLE, DevInfo: KLST_DEVINFO_HANDLE) -> BOOL;
    pub fn UsbK_Free(InterfaceHandle: KUSB_HANDLE) -> BOOL;

    // Configuration / interface selection.
    pub fn UsbK_SetConfiguration(InterfaceHandle: KUSB_HANDLE, ConfigurationNumber: UCHAR) -> BOOL;
    pub fn UsbK_GetConfiguration(
        InterfaceHandle: KUSB_HANDLE,
        ConfigurationNumber: *mut UCHAR,
    ) -> BOOL;
    pub fn UsbK_ClaimInterface(
        InterfaceHandle: KUSB_HANDLE,
        NumberOrIndex: UCHAR,
        IsIndex: BOOL,
    ) -> BOOL;
    pub fn UsbK_ReleaseInterface(
        InterfaceHandle: KUSB_HANDLE,
        NumberOrIndex: UCHAR,
        IsIndex: BOOL,
    ) -> BOOL;

    // Descriptors and control transfers.
    pub fn UsbK_GetDescriptor(
        InterfaceHandle: KUSB_HANDLE,
        DescriptorType: UCHAR,
        Index: UCHAR,
        LanguageID: USHORT,
        Buffer: *mut UCHAR,
        BufferLength: UINT,
        LengthTransferred: *mut UINT,
    ) -> BOOL;
    pub fn UsbK_ControlTransfer(
        InterfaceHandle: KUSB_HANDLE,
        SetupPacket: WINUSB_SETUP_PACKET,
        Buffer: *mut UCHAR,
        BufferLength: UINT,
        LengthTransferred: *mut UINT,
        Overlapped: LPOVERLAPPED,
    ) -> BOOL;

    // Bulk pipes.
    pub fn UsbK_ReadPipe(
        InterfaceHandle: KUSB_HANDLE,
        PipeID: UCHAR,
        Buffer: *mut UCHAR,
        BufferLength: UINT,
        LengthTransferred: *mut UINT,
        Overlapped: LPOVERLAPPED,
    ) -> BOOL;
    pub fn UsbK_WritePipe(
        InterfaceHandle: KUSB_HANDLE,
        PipeID: UCHAR,
        Buffer: *const UCHAR,
        BufferLength: UINT,
        LengthTransferred: *mut UINT,
        Overlapped: LPOVERLAPPED,
    ) -> BOOL;
    pub fn UsbK_ResetPipe(InterfaceHandle: KUSB_HANDLE, PipeID: UCHAR) -> BOOL;
    pub fn UsbK_AbortPipe(InterfaceHandle: KUSB_HANDLE, PipeID: UCHAR) -> BOOL;
    pub fn UsbK_FlushPipe(InterfaceHandle: KUSB_HANDLE, PipeID: UCHAR) -> BOOL;

    pub fn UsbK_SetPipePolicy(
        InterfaceHandle: KUSB_HANDLE,
        PipeID: UCHAR,
        PolicyType: UINT,
        ValueLength: UINT,
        Value: *const c_void,
    ) -> BOOL;
}

pub const SHORT_PACKET_TERMINATE: UINT = 0x01;

// Layout sanity checks. KUSB_SETUP_PACKET is documented as 8 bytes; if
// our packed mirror drifts we want a build-time error, not a runtime
// USB stall.
const _: () = {
    if core::mem::size_of::<WINUSB_SETUP_PACKET>() != 8 {
        panic!("WINUSB_SETUP_PACKET must be exactly 8 bytes");
    }
};
