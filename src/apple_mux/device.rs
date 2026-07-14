// Jackson Coxson
//
// Enumerate + open Apple's mux device interface and drive its custom
// IOCTL protocol.

#![cfg(target_os = "windows")]

use std::ffi::c_void;
use std::io;
use std::ptr;
use std::sync::Arc;

use super::ffi;
use super::io::{AppleMuxReader, AppleMuxWriter};

/// Return the openable paths of all present mux interfaces, each already
/// suffixed with the `\MUX1` reference string required by the gate.
pub fn enumerate_paths() -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    let guid = ffi::APPLE_MUX_INTERFACE_GUID;
    let devinfo = unsafe {
        ffi::SetupDiGetClassDevsW(
            &guid,
            ptr::null(),
            ptr::null_mut(),
            ffi::DIGCF_PRESENT | ffi::DIGCF_DEVICEINTERFACE,
        )
    };
    if ffi::is_invalid(devinfo) {
        return Err(io::Error::last_os_error());
    }

    let mut index = 0u32;
    loop {
        let mut iface = ffi::SpDeviceInterfaceData {
            cb_size: std::mem::size_of::<ffi::SpDeviceInterfaceData>() as u32,
            interface_class_guid: guid,
            flags: 0,
            reserved: 0,
        };
        let ok = unsafe {
            ffi::SetupDiEnumDeviceInterfaces(devinfo, ptr::null_mut(), &guid, index, &mut iface)
        };
        if ok == ffi::FALSE {
            // ERROR_NO_MORE_ITEMS ends the loop; anything else is a real error.
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(ffi::ERROR_NO_MORE_ITEMS as i32) {
                break;
            }
            unsafe { ffi::SetupDiDestroyDeviceInfoList(devinfo) };
            return Err(e);
        }
        index += 1;

        // First call sizes the detail buffer.
        let mut required = 0u32;
        unsafe {
            ffi::SetupDiGetDeviceInterfaceDetailW(
                devinfo,
                &mut iface,
                ptr::null_mut(),
                0,
                &mut required,
                ptr::null_mut(),
            );
        }
        if required < 6 {
            continue;
        }
        // 8-byte-aligned backing store; cbSize is 8 on 64-bit (4 on 32-bit).
        let mut buf = vec![0u64; (required as usize).div_ceil(8)];
        let detail = buf.as_mut_ptr() as *mut c_void;
        let cb_size: u32 = if cfg!(target_pointer_width = "64") {
            8
        } else {
            6
        };
        unsafe { ptr::write(detail as *mut u32, cb_size) };
        let mut got = 0u32;
        let ok = unsafe {
            ffi::SetupDiGetDeviceInterfaceDetailW(
                devinfo,
                &mut iface,
                detail,
                required,
                &mut got,
                ptr::null_mut(),
            )
        };
        if ok == ffi::FALSE {
            continue;
        }
        // DevicePath (wide, NUL-terminated) begins right after cbSize (offset 4).
        let path_ptr = unsafe { (detail as *const u8).add(4) as *const u16 };
        let base = unsafe { read_wide(path_ptr) };
        if !base.is_empty() {
            out.push(format!("{base}\\MUX1"));
        }
    }

    unsafe { ffi::SetupDiDestroyDeviceInfoList(devinfo) };
    Ok(out)
}

unsafe fn read_wide(mut p: *const u16) -> String {
    let mut units = Vec::new();
    unsafe {
        while *p != 0 {
            units.push(*p);
            p = p.add(1);
        }
    }
    String::from_utf16_lossy(&units)
}

pub(crate) struct DeviceHandle {
    raw: ffi::Handle,
}

// The handle is used for concurrent overlapped DeviceIoControl on
// separate pipes from separate blocking threads; each call carries its
// own OVERLAPPED + event, so this is sound.
unsafe impl Send for DeviceHandle {}
unsafe impl Sync for DeviceHandle {}

impl DeviceHandle {
    pub(crate) fn raw(&self) -> ffi::Handle {
        self.raw
    }
}

impl Drop for DeviceHandle {
    fn drop(&mut self) {
        unsafe {
            ffi::CloseHandle(self.raw);
        }
    }
}

pub struct Device {
    handle: Arc<DeviceHandle>,
}

impl Device {
    /// Open a mux interface by its full `\MUX1`-suffixed path (from
    /// `enumerate_paths`).
    pub fn open(path: &str) -> io::Result<Self> {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let h = unsafe {
            ffi::CreateFileW(
                wide.as_ptr(),
                ffi::GENERIC_READ | ffi::GENERIC_WRITE,
                ffi::FILE_SHARE_READ | ffi::FILE_SHARE_WRITE,
                ptr::null_mut(),
                ffi::OPEN_EXISTING,
                ffi::FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if ffi::is_invalid(h) {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle: Arc::new(DeviceHandle { raw: h }),
        })
    }

    /// Apple sends this immediately after open (IOCTL 0x2200A8, 0x24-byte
    /// in/out)
    pub fn init(&self) -> io::Result<()> {
        let mut buf = [0u8; 0x24];
        unsafe {
            ioctl_sync(
                self.handle.raw,
                ffi::IOCTL_INIT,
                buf.as_ptr() as *const c_void,
                buf.len() as u32,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn control_transfer(
        &self,
        dir_in: bool,
        req_type: u8,
        recipient: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
    ) -> io::Result<usize> {
        let w_len = data.len() as u16;
        // in = 8-byte setup packet (+ OUT data appended); out = 8 + IN data.
        let mut buf = vec![0u8; 8 + data.len()];
        buf[0] = ((dir_in as u8) << 7) | ((req_type & 0x3) << 5) | (recipient & 0x1f);
        buf[1] = request;
        buf[2..4].copy_from_slice(&value.to_le_bytes());
        buf[4..6].copy_from_slice(&index.to_le_bytes());
        buf[6..8].copy_from_slice(&w_len.to_le_bytes());
        if !dir_in {
            buf[8..].copy_from_slice(data);
        }
        let in_size = 8u32;
        let returned = unsafe {
            ioctl_sync(
                self.handle.raw,
                ffi::IOCTL_CONTROL_TRANSFER,
                buf.as_ptr() as *const c_void,
                in_size,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
            )?
        };
        if dir_in && returned as usize >= 8 {
            let n = (returned as usize - 8).min(data.len());
            data[..n].copy_from_slice(&buf[8..8 + n]);
            Ok(n)
        } else {
            Ok(0)
        }
    }

    /// Query a 1-based pipe index. Returns `(is_bulk_in, max_packet)`.
    /// Used to map the two bulk endpoints to read (IN) / write (OUT)
    /// rather than assuming an order.
    pub fn pipe_properties(&self, index: u32) -> io::Result<(bool, u16)> {
        let mut buf = [0u8; 0x18];
        buf[0..4].copy_from_slice(&index.to_le_bytes());
        unsafe {
            ioctl_sync(
                self.handle.raw,
                ffi::IOCTL_GET_PIPE_PROPERTIES,
                buf.as_ptr() as *const c_void,
                buf.len() as u32,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
            )?;
        }
        let addr = buf[6];
        let max_packet = u16::from_le_bytes([buf[4], buf[5]]);
        Ok((addr & 0x80 != 0, max_packet))
    }

    /// Read the device UDID via IOCTL 0x2200AC (ASCII).
    pub fn serial(&self) -> io::Result<String> {
        let mut buf = [0u8; 0x100];
        let n = unsafe {
            ioctl_sync(
                self.handle.raw,
                ffi::IOCTL_GET_SERIAL,
                buf.as_ptr() as *const c_void,
                buf.len() as u32,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
            )?
        } as usize;
        let n = n.min(buf.len());
        let end = buf[..n].iter().position(|&b| b == 0).unwrap_or(n);
        let s = String::from_utf8_lossy(&buf[..end]).into_owned();
        if s.is_empty() {
            Err(io::Error::other("empty serial from 0x2200AC"))
        } else {
            Ok(s)
        }
    }

    pub fn pipes(
        &self,
        read_pipe: u8,
        write_pipe: u8,
        write_max_packet: u16,
    ) -> (AppleMuxReader, AppleMuxWriter) {
        (
            AppleMuxReader::new(self.handle.clone(), read_pipe),
            AppleMuxWriter::new(self.handle.clone(), write_pipe, write_max_packet),
        )
    }
}

pub(crate) unsafe fn ioctl_sync(
    handle: ffi::Handle,
    code: u32,
    in_buf: *const c_void,
    in_size: u32,
    out_buf: *mut c_void,
    out_size: u32,
) -> io::Result<u32> {
    // Manual-reset event, initially non-signaled.
    let event = unsafe { ffi::CreateEventW(ptr::null_mut(), 1, 0, ptr::null()) };
    if ffi::is_invalid(event) {
        return Err(io::Error::last_os_error());
    }
    let mut ov = ffi::Overlapped {
        h_event: event,
        ..Default::default()
    };
    let mut returned = 0u32;
    let ok = unsafe {
        ffi::DeviceIoControl(
            handle,
            code,
            in_buf,
            in_size,
            out_buf,
            out_size,
            &mut returned,
            &mut ov,
        )
    };
    let result = if ok == ffi::FALSE {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(ffi::ERROR_IO_PENDING as i32) {
            let g = unsafe { ffi::GetOverlappedResult(handle, &mut ov, &mut returned, 1) };
            if g == ffi::FALSE {
                Err(io::Error::last_os_error())
            } else {
                Ok(returned)
            }
        } else {
            Err(e)
        }
    } else {
        Ok(returned)
    };
    unsafe { ffi::CloseHandle(event) };
    result
}
