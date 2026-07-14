// Jackson Coxson
//
// Windows backend that rides Apple's installed WinUSB + UMDF-filter
// driver stack for the iOS mux, instead of binding our own driver
// (libusbK/libwdi).

#![cfg(target_os = "windows")]

pub mod amds;
pub mod device;
pub mod ffi;
pub mod io;

pub use device::{Device, enumerate_paths};
pub use io::{AppleMuxReader, AppleMuxWriter};
