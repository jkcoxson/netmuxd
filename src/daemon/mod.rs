// Jackson Coxson
//
// USB enumeration + hotplug + per-device wiring. The platform-agnostic
// surface lives here; the actual enumeration/I/O backend is selected at
// compile time:
//
//   - non-Windows: nusb (`nusb_backend`).
//   - Windows:     libusbK via `crate::libusbk` (`libusbk_backend`).
//
// Both backends end up calling the helpers below to pair (if needed)
// and register the device with the manager.

use idevice::{Idevice, services::lockdown::LockdownClient};
use log::{info, warn};

use crate::config::NetmuxdConfig;
use crate::manager::{ManagerRequest, ManagerRequestType, ManagerSender};
use crate::pairing_file::PairingFileFinder;
use crate::usb::mux::UsbMuxHandle;

// Re-export the Apple-specific constants the daemon backends share with
// the reusable `crate::usb::apple` helpers, so backends keep using
// `super::*` without each maintaining its own copy.
pub(crate) use crate::usb::apple::{
    APPLE_VID, MUX_INTERFACE_CLASS as INTERFACE_CLASS,
    MUX_INTERFACE_PROTOCOL as INTERFACE_PROTOCOL, MUX_INTERFACE_SUBCLASS as INTERFACE_SUBCLASS,
    PID_RANGE,
};

// Backend modules.
#[cfg(target_os = "windows")]
mod libusbk_backend;
#[cfg(not(target_os = "windows"))]
mod nusb_backend;

pub(crate) const PID_RANGE_LOW: u16 = *PID_RANGE.start();
pub(crate) const PID_RANGE_HIGH: u16 = *PID_RANGE.end();

pub fn usb_available() -> bool {
    #[cfg(target_os = "windows")]
    {
        crate::libusbk::dll_available()
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}

/// Entry point. Dispatches to the platform's USB backend.
pub async fn discover(sender: ManagerSender, config: NetmuxdConfig) {
    #[cfg(not(target_os = "windows"))]
    nusb_backend::run(sender, config).await;
    #[cfg(target_os = "windows")]
    libusbk_backend::run(sender, config).await;
}

// --- shared helpers ----------------------------------------------------

pub(crate) async fn register_with_manager(
    sender: &ManagerSender,
    udid: String,
    handle: UsbMuxHandle,
    location_id: u64,
    product_id: u64,
    speed: u64,
) {
    if let Err(e) = sender
        .send(ManagerRequest {
            request_type: ManagerRequestType::DiscoveredUsbDevice {
                udid: udid.clone(),
                location_id,
                product_id,
                speed,
                handle,
            },
            response: None,
        })
        .await
    {
        warn!("Failed to forward discovered USB device {udid}: {e:?}");
    }
}

/// Run the lockdown Pair flow over the USB mux. Blocks while the
/// device displays the Trust prompt to the user. On success, writes
/// the pairing record to disk and returns the canonical UDID we used
/// as the filename.
pub(crate) async fn pair_via_usb(
    pairing_finder: &PairingFileFinder,
    handle: &UsbMuxHandle,
    raw_udid: &str,
) -> Result<String, String> {
    let stream = handle
        .connect(LockdownClient::LOCKDOWND_PORT)
        .await
        .map_err(|e| format!("usb connect to lockdown: {e:?}"))?;

    let idevice = Idevice::new(Box::new(stream), "netmuxd-pair");
    let mut lockdown = LockdownClient { idevice };

    // Ask the device for its canonical UDID
    let canonical_udid = match lockdown.get_value(Some("UniqueDeviceID"), None).await {
        Ok(v) => v
            .as_string()
            .map(|s| s.to_string())
            .ok_or_else(|| "UniqueDeviceID not a string".to_string())?,
        Err(e) => {
            // Fallback: synthesize the dashed form from the raw 24-char serial.
            warn!("GetValue(UniqueDeviceID) failed: {e:?}; using synthesized UDID");
            if raw_udid.len() == 24 && raw_udid.chars().all(|c| c.is_ascii_hexdigit()) {
                format!("{}-{}", &raw_udid[..8], &raw_udid[8..])
            } else {
                raw_udid.to_string()
            }
        }
    };

    let (host_id, system_buid) = pairing_finder
        .get_host_identity()
        .await
        .map_err(|e| format!("read host identity: {e:?}"))?;

    info!("Calling lockdown.pair() for {canonical_udid} (waiting for user trust)");
    let mut pairing_file = lockdown
        .pair(host_id, system_buid, None)
        .await
        .map_err(|e| format!("lockdown pair: {e:?}"))?;
    pairing_file.udid = Some(canonical_udid.clone());

    let bytes = pairing_file
        .serialize()
        .map_err(|e| format!("serialize pairing file: {e:?}"))?;
    let path = std::path::PathBuf::from(pairing_finder.plist_storage())
        .join(format!("{canonical_udid}.plist"));
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    tokio::fs::write(&path, &bytes)
        .await
        .map_err(|e| format!("write {path:?}: {e:?}"))?;

    Ok(canonical_udid)
}

pub(crate) async fn resolve_paired_udid(finder: &PairingFileFinder, raw: &str) -> Option<String> {
    let mut candidates: Vec<String> = vec![raw.to_string()];
    if raw.len() == 24 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        candidates.push(format!("{}-{}", &raw[..8], &raw[8..]));
    }
    for udid in candidates {
        if finder.get_pairing_record(&udid).await.is_ok() {
            return Some(udid);
        }
    }
    None
}

/// Sentinel sent over the device-removal channel for both backends.
pub(crate) async fn send_remove(sender: &ManagerSender, udid: String) {
    let _ = sender
        .send(ManagerRequest {
            request_type: ManagerRequestType::RemoveDevice {
                udid,
                connection_type: Some("USB".into()),
            },
            response: None,
        })
        .await;
}
