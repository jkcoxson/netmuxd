//! netmuxd — usbmuxd-protocol server and library
//!
//! Library surface:
//! - [`usb`]: reusable USB transport + protocol code that works on both
//!   native targets and `wasm32-unknown-unknown` (over WebUSB via the
//!   user's `nusb` fork). Includes the `usbmuxd`-v2 wire protocol, generic
//!   `AsyncRead`/`AsyncWrite` adapters over `nusb` bulk endpoints, and
//!   Apple-specific open-and-claim helpers.
//! - [`raw_packet`], [`devices`]: usbmuxd protocol packet codec and the
//!   device-list plist shape served by the daemon.
//!
//! The bundled `netmuxd`, `passthrough`, and `add_device` binaries
//! consume this library plus the native-only [`daemon`] orchestration
//! (USB enumeration / hotplug / pair / manager).

pub mod devices;
pub mod raw_packet;
pub mod usb;

#[cfg(not(target_arch = "wasm32"))]
pub mod heartbeat;
#[cfg(not(target_arch = "wasm32"))]
pub mod manager;

#[cfg(not(target_arch = "wasm32"))]
pub mod config;
#[cfg(not(target_arch = "wasm32"))]
pub mod daemon;
#[cfg(not(target_arch = "wasm32"))]
pub mod mdns;
#[cfg(not(target_arch = "wasm32"))]
pub mod pairing_file;

#[cfg(all(target_os = "windows", not(target_arch = "wasm32")))]
pub mod libusbk;
#[cfg(all(target_os = "windows", not(target_arch = "wasm32")))]
pub mod libwdi;

/// Spawn a `'static + Send` future on whatever executor is current.
///
/// On native this is `tokio::spawn`. On wasm32 this is
/// `wasm_bindgen_futures::spawn_local`, which doesn't require Send but
/// accepts Send futures fine.
pub(crate) fn spawn<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::spawn(fut);
    }
    #[cfg(target_arch = "wasm32")]
    {
        wasm_bindgen_futures::spawn_local(fut);
    }
}
