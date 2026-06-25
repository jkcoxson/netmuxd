// Jackson Coxson
// Reference: vendor/libwdi/include/libwdi.h.

#![cfg(target_os = "windows")]
#![allow(
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::upper_case_acronyms
)]

use std::os::raw::{c_char, c_int, c_void};

// --- Windows base types ------------------------------------------------

pub type BOOL = i32;
pub type UINT32 = u32;
pub type UINT64 = u64;
pub type HWND = *mut c_void;

pub const TRUE: BOOL = 1;
pub const FALSE: BOOL = 0;

// --- libwdi enums ------------------------------------------------------

// wdi_driver_type
pub const WDI_WINUSB: c_int = 0;
pub const WDI_LIBUSB0: c_int = 1;
pub const WDI_LIBUSBK: c_int = 2;
pub const WDI_CDC: c_int = 3;
pub const WDI_USER: c_int = 4;

// wdi_log_level
pub const WDI_LOG_LEVEL_DEBUG: c_int = 0;
pub const WDI_LOG_LEVEL_INFO: c_int = 1;
pub const WDI_LOG_LEVEL_WARNING: c_int = 2;
pub const WDI_LOG_LEVEL_ERROR: c_int = 3;
pub const WDI_LOG_LEVEL_NONE: c_int = 4;

// wdi_error
pub const WDI_SUCCESS: c_int = 0;
pub const WDI_ERROR_IO: c_int = -1;
pub const WDI_ERROR_INVALID_PARAM: c_int = -2;
pub const WDI_ERROR_ACCESS: c_int = -3;
pub const WDI_ERROR_NO_DEVICE: c_int = -4;
pub const WDI_ERROR_NOT_FOUND: c_int = -5;
pub const WDI_ERROR_BUSY: c_int = -6;
pub const WDI_ERROR_TIMEOUT: c_int = -7;
pub const WDI_ERROR_OVERFLOW: c_int = -8;
pub const WDI_ERROR_PENDING_INSTALLATION: c_int = -9;
pub const WDI_ERROR_INTERRUPTED: c_int = -10;
pub const WDI_ERROR_RESOURCE: c_int = -11;
pub const WDI_ERROR_NOT_SUPPORTED: c_int = -12;
pub const WDI_ERROR_EXISTS: c_int = -13;
pub const WDI_ERROR_USER_CANCEL: c_int = -14;
pub const WDI_ERROR_NEEDS_ADMIN: c_int = -15;
pub const WDI_ERROR_WOW64: c_int = -16;
pub const WDI_ERROR_INF_SYNTAX: c_int = -17;
pub const WDI_ERROR_CAT_MISSING: c_int = -18;
pub const WDI_ERROR_UNSIGNED: c_int = -19;
pub const WDI_ERROR_OTHER: c_int = -99;

// --- libwdi structs ----------------------------------------------------

#[repr(C)]
pub struct wdi_device_info {
    pub next: *mut wdi_device_info,
    pub vid: u16,
    pub pid: u16,
    pub is_composite: BOOL,
    pub mi: u8,
    pub desc: *mut c_char,
    pub driver: *mut c_char,
    pub device_id: *mut c_char,
    pub hardware_id: *mut c_char,
    pub compatible_id: *mut c_char,
    pub upper_filter: *mut c_char,
    pub driver_version: UINT64,
}

#[repr(C)]
pub struct wdi_options_create_list {
    pub list_all: BOOL,
    pub list_hubs: BOOL,
    pub trim_whitespaces: BOOL,
}

#[repr(C)]
pub struct wdi_options_prepare_driver {
    pub driver_type: c_int,
    pub vendor_name: *const c_char,
    pub device_guid: *const c_char,
    pub disable_cat: BOOL,
    pub disable_signing: BOOL,
    pub cert_subject: *const c_char,
    pub use_wcid_driver: BOOL,
    pub external_inf: BOOL,
}

#[repr(C)]
pub struct wdi_options_install_driver {
    pub hWnd: HWND,
    pub install_filter_driver: BOOL,
    pub pending_install_timeout: UINT32,
}

// --- DLL imports -------------------------------------------------------

#[link(name = "libwdi", kind = "static")]
unsafe extern "system" {
    pub fn wdi_create_list(
        list: *mut *mut wdi_device_info,
        options: *mut wdi_options_create_list,
    ) -> c_int;

    pub fn wdi_destroy_list(list: *mut wdi_device_info) -> c_int;

    pub fn wdi_prepare_driver(
        device_info: *mut wdi_device_info,
        path: *const c_char,
        inf_name: *const c_char,
        options: *mut wdi_options_prepare_driver,
    ) -> c_int;

    pub fn wdi_install_driver(
        device_info: *mut wdi_device_info,
        path: *const c_char,
        inf_name: *const c_char,
        options: *mut wdi_options_install_driver,
    ) -> c_int;

    pub fn wdi_strerror(errcode: c_int) -> *const c_char;
    pub fn wdi_set_log_level(level: c_int) -> c_int;
}
