// Jackson Coxson
//
// libusbK user-mode DLL bindings

pub mod device;
pub mod ffi;
pub mod io;

pub use device::{Device, DeviceList};
pub use io::{LibusbkReader, LibusbkWriter};

pub fn dll_available() -> bool {
    use std::ffi::OsStr;
    use std::os::raw::c_void;
    use std::os::windows::ffi::OsStrExt;

    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
    }

    let wide: Vec<u16> = OsStr::new("libusbK.dll")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `wide` is a valid NUL-terminated UTF-16 string for the
    // duration of the call.
    !unsafe { LoadLibraryW(wide.as_ptr()) }.is_null()
}
