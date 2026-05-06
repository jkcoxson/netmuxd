// Jackson Coxson
//
// Safe wrappers around the libusbK device-list and per-device handle.

#![cfg(target_os = "windows")]

use std::ffi::CStr;
use std::io;
use std::ptr;
use std::sync::Arc;

use super::ffi;
use super::io::{LibusbkReader, LibusbkWriter};

// --- Handle wrapper ----------------------------------------------------

/// Owned `KUSB_HANDLE`. `Drop` calls `UsbK_Free`. Shared between the
/// reader/writer pipe wrappers via `Arc`, since libusbK supports
/// concurrent calls on different pipes against a single device handle.
pub(crate) struct DeviceHandle {
    raw: ffi::KUSB_HANDLE,
}

// libusbK's documentation explicitly supports concurrent ReadPipe /
// WritePipe on different pipes from different threads against a single
// `KUSB_HANDLE`. The DLL serializes internally where needed.
unsafe impl Send for DeviceHandle {}
unsafe impl Sync for DeviceHandle {}

impl DeviceHandle {
    pub(crate) fn raw(&self) -> ffi::KUSB_HANDLE {
        self.raw
    }
}

impl Drop for DeviceHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = ffi::UsbK_Free(self.raw);
        }
    }
}

// --- Device ------------------------------------------------------------

/// A live libusbK device handle.
pub struct Device {
    handle: Arc<DeviceHandle>,
}

impl Device {
    /// Open the device described by `info`. The `KLST_DEVINFO_HANDLE`
    /// is owned by the parent `DeviceList` and must outlive this call,
    /// but does **not** need to outlive the resulting `Device`.
    pub fn open(info: &DeviceInfo) -> io::Result<Self> {
        let mut h: ffi::KUSB_HANDLE = ptr::null_mut();
        let ok = unsafe { ffi::UsbK_Init(&mut h, info.raw) };
        if ok == ffi::FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle: Arc::new(DeviceHandle { raw: h }),
        })
    }

    pub fn set_configuration(&self, configuration_number: u8) -> io::Result<()> {
        let ok = unsafe { ffi::UsbK_SetConfiguration(self.handle.raw, configuration_number) };
        if ok == ffi::FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn get_configuration(&self) -> io::Result<u8> {
        let mut n: u8 = 0;
        let ok = unsafe { ffi::UsbK_GetConfiguration(self.handle.raw, &mut n) };
        if ok == ffi::FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(n)
    }

    /// Claim an interface by its `bInterfaceNumber`.
    pub fn claim_interface(&self, number: u8) -> io::Result<()> {
        let ok = unsafe { ffi::UsbK_ClaimInterface(self.handle.raw, number, ffi::FALSE) };
        if ok == ffi::FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn release_interface(&self, number: u8) -> io::Result<()> {
        let ok = unsafe { ffi::UsbK_ReleaseInterface(self.handle.raw, number, ffi::FALSE) };
        if ok == ffi::FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Read a USB descriptor.
    pub fn get_descriptor(
        &self,
        descriptor_type: u8,
        index: u8,
        language_id: u16,
        buffer: &mut [u8],
    ) -> io::Result<usize> {
        let mut transferred: u32 = 0;
        let ok = unsafe {
            ffi::UsbK_GetDescriptor(
                self.handle.raw,
                descriptor_type,
                index,
                language_id,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut transferred,
            )
        };
        if ok == ffi::FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(transferred as usize)
    }

    /// Split off async pipe wrappers for the given bulk endpoints.
    pub fn pipes(
        &self,
        ep_in: u8,
        ep_out: u8,
        ep_out_max_packet: u16,
    ) -> (LibusbkReader, LibusbkWriter) {
        (
            LibusbkReader::new(self.handle.clone(), ep_in),
            LibusbkWriter::new(self.handle.clone(), ep_out, ep_out_max_packet),
        )
    }

    pub fn set_short_packet_terminate(&self, pipe_id: u8, enable: bool) -> io::Result<()> {
        let value: u8 = if enable { 1 } else { 0 };
        let ok = unsafe {
            ffi::UsbK_SetPipePolicy(
                self.handle.raw,
                pipe_id,
                ffi::SHORT_PACKET_TERMINATE,
                std::mem::size_of::<u8>() as u32,
                &value as *const u8 as *const std::ffi::c_void,
            )
        };
        if ok == ffi::FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

// --- DeviceList & iterator --------------------------------------------

/// A snapshot of libusbK-visible USB devices. Frees the underlying
/// list on drop.
pub struct DeviceList {
    raw: ffi::KLST_HANDLE,
}

unsafe impl Send for DeviceList {}

impl DeviceList {
    /// Enumerate currently-connected libusbK devices.
    pub fn new() -> io::Result<Self> {
        let mut raw: ffi::KLST_HANDLE = ptr::null_mut();
        let ok = unsafe { ffi::LstK_Init(&mut raw, ffi::KLST_FLAG_NONE) };
        if ok == ffi::FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { raw })
    }

    /// Iterate device entries. The yielded `DeviceInfo` borrows from
    /// `self`; do not retain it past the list's lifetime.
    pub fn iter(&self) -> DeviceIter<'_> {
        unsafe {
            ffi::LstK_MoveReset(self.raw);
        }
        DeviceIter { list: self }
    }
}

impl Drop for DeviceList {
    fn drop(&mut self) {
        unsafe {
            let _ = ffi::LstK_Free(self.raw);
        }
    }
}

/// Borrowed view onto a `KLST_DEVINFO`. Cheap to copy; the underlying
/// pointer is stable for the lifetime of the parent `DeviceList`.
#[derive(Copy, Clone)]
pub struct DeviceInfo<'a> {
    raw: ffi::KLST_DEVINFO_HANDLE,
    _list: std::marker::PhantomData<&'a DeviceList>,
}

#[allow(dead_code)]
impl<'a> DeviceInfo<'a> {
    pub fn vid(&self) -> u16 {
        unsafe { (*self.raw).Common.Vid as u16 }
    }
    pub fn pid(&self) -> u16 {
        unsafe { (*self.raw).Common.Pid as u16 }
    }
    /// Composite-interface number from the device ID string. `-1` for
    /// non-composite parents.
    pub fn mi(&self) -> i32 {
        unsafe { (*self.raw).Common.MI }
    }
    pub fn bus_number(&self) -> i32 {
        unsafe { (*self.raw).BusNumber }
    }
    pub fn device_address(&self) -> i32 {
        unsafe { (*self.raw).DeviceAddress }
    }
    pub fn serial_number(&self) -> Option<String> {
        unsafe { read_cstr(&(*self.raw).SerialNumber) }
    }
    pub fn device_id(&self) -> Option<String> {
        unsafe { read_cstr(&(*self.raw).DeviceID) }
    }
    pub fn instance_id(&self) -> Option<String> {
        unsafe { read_cstr(&(*self.raw).Common.InstanceID) }
    }
    pub fn device_path(&self) -> Option<String> {
        unsafe { read_cstr(&(*self.raw).DevicePath) }
    }
    pub(crate) fn raw_handle(&self) -> ffi::KLST_DEVINFO_HANDLE {
        self.raw
    }
}

/// Read a fixed-length C string slot into a Rust `String`. Returns
/// `None` if the field is empty or contains non-UTF-8.
unsafe fn read_cstr(buf: &[u8]) -> Option<String> {
    let cstr = unsafe { CStr::from_ptr(buf.as_ptr() as *const i8) };
    match cstr.to_str() {
        Ok(s) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    }
}

pub struct DeviceIter<'a> {
    list: &'a DeviceList,
}

impl<'a> Iterator for DeviceIter<'a> {
    type Item = DeviceInfo<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let mut info: ffi::KLST_DEVINFO_HANDLE = ptr::null_mut();
        let ok = unsafe { ffi::LstK_MoveNext(self.list.raw, &mut info) };
        if ok == ffi::FALSE || info.is_null() {
            return None;
        }
        Some(DeviceInfo {
            raw: info,
            _list: std::marker::PhantomData,
        })
    }
}
