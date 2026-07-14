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

use std::collections::HashMap;
use std::sync::Arc;

use idevice::{Idevice, IdeviceError, services::lockdown::LockdownClient};
use log::{info, warn};
use tokio::sync::Mutex;

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
mod apple_mux_backend;
#[cfg(all(target_os = "windows", feature = "libusbk"))]
mod libusbk_backend;
#[cfg(not(target_os = "windows"))]
mod nusb_backend;

pub(crate) const PID_RANGE_LOW: u16 = *PID_RANGE.start();
pub(crate) const PID_RANGE_HIGH: u16 = *PID_RANGE.end();

pub fn usb_available(apple_mux: bool) -> bool {
    #[cfg(target_os = "windows")]
    {
        // The apple_mux backend (the default) rides Apple's installed WinUSB
        // driver and needs no libusbK.dll of ours.
        if apple_mux {
            return true;
        }
        #[cfg(feature = "libusbk")]
        {
            crate::libusbk::dll_available()
        }
        #[cfg(not(feature = "libusbk"))]
        {
            false
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = apple_mux;
        true
    }
}

/// Entry point. Dispatches to the platform's USB backend.
pub async fn discover(sender: ManagerSender, config: NetmuxdConfig) {
    #[cfg(not(target_os = "windows"))]
    nusb_backend::run(sender, config).await;
    #[cfg(target_os = "windows")]
    {
        // Default: the apple_mux backend (rides Apple's installed WinUSB
        // stack, no driver of our own). `--libusbk` opts into the legacy
        // libusbK backend, when compiled in.
        if config.apple_mux {
            apple_mux_backend::run(sender, config).await;
        } else {
            #[cfg(feature = "libusbk")]
            libusbk_backend::run(sender, config).await;
            #[cfg(not(feature = "libusbk"))]
            {
                let _ = (sender, config);
                log::error!(
                    "--libusbk was requested but this build has no libusbK backend compiled in"
                );
            }
        }
    }
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

fn synthesize_udid(raw: &str) -> String {
    if raw.len() == 24 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        format!("{}-{}", &raw[..8], &raw[8..])
    } else {
        raw.to_string()
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

    match lockdown.idevice.get_type().await {
        Ok(ty) if ty != "com.apple.mobile.lockdown" => {
            info!("{raw_udid} is running '{ty}' (restore mode); exposing without pairing");
            return Ok(synthesize_udid(raw_udid));
        }
        Ok(_) => {}
        Err(e) => {
            warn!("QueryType failed for {raw_udid}: {e:?}; attempting to pair anyway");
        }
    }

    // Ask the device for its canonical UDID
    let canonical_udid = match lockdown.get_value(Some("UniqueDeviceID"), None).await {
        Ok(v) => v
            .as_string()
            .map(|s| s.to_string())
            .ok_or_else(|| "UniqueDeviceID not a string".to_string())?,
        Err(e) => {
            // Fallback: synthesize the dashed form from the raw 24-char serial.
            warn!("GetValue(UniqueDeviceID) failed: {e:?}; using synthesized UDID");
            synthesize_udid(raw_udid)
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

enum RecordCheck {
    Valid,
    Stale(String),
    Unknown(String),
}

async fn check_pairing_record(
    pairing_finder: &PairingFileFinder,
    handle: &UsbMuxHandle,
    udid: &str,
) -> RecordCheck {
    let pairing_file = match pairing_finder.get_pairing_record(&udid.to_string()).await {
        Ok(p) => p,
        Err(e) => return RecordCheck::Unknown(format!("read pairing record: {e:?}")),
    };

    let stream = match handle.connect(LockdownClient::LOCKDOWND_PORT).await {
        Ok(s) => s,
        Err(e) => return RecordCheck::Unknown(format!("usb connect to lockdown: {e:?}")),
    };

    let idevice = Idevice::new(Box::new(stream), "netmuxd-preflight");
    let mut lockdown = LockdownClient { idevice };

    match lockdown.idevice.get_type().await {
        Ok(ty) if ty != "com.apple.mobile.lockdown" => {
            return RecordCheck::Unknown(format!(
                "device is running '{ty}' (restore mode); skipping validation"
            ));
        }
        Ok(_) => {}
        Err(e) => {
            return RecordCheck::Unknown(format!("QueryType failed: {e:?}"));
        }
    }

    match lockdown.start_session(&pairing_file).await {
        Ok(_legacy) => RecordCheck::Valid,
        Err(IdeviceError::InvalidHostID) => {
            RecordCheck::Stale("device does not recognize this host's pairing".to_string())
        }
        Err(IdeviceError::Rustls(e)) => {
            RecordCheck::Stale(format!("TLS handshake failed with stored certs: {e:?}"))
        }
        Err(e) => RecordCheck::Unknown(format!("{e:?}")),
    }
}

/// The bits of `ListDevices` info a backend has on hand at connect
/// time but that `connect_device` only ever threads through unchanged.
pub(crate) struct DeviceMeta {
    pub location_id: u64,
    pub product_id: u64,
    pub speed: u64,
}

pub(crate) async fn connect_device<K>(
    sender: &ManagerSender,
    pairing_file_finder: &PairingFileFinder,
    known: &Arc<Mutex<HashMap<K, String>>>,
    key: K,
    handle: UsbMuxHandle,
    raw_udid: String,
    meta: DeviceMeta,
) -> String
where
    K: Eq + std::hash::Hash + Clone + Send + 'static,
{
    let DeviceMeta {
        location_id,
        product_id,
        speed,
    } = meta;

    let existing_udid = resolve_paired_udid(pairing_file_finder, &raw_udid).await;

    let validated_udid = match existing_udid {
        Some(udid) => match check_pairing_record(pairing_file_finder, &handle, &udid).await {
            RecordCheck::Valid => Some(udid),
            RecordCheck::Stale(reason) => {
                warn!("Pairing record for {udid} is stale ({reason}); removing and re-pairing");
                if let Err(e) = pairing_file_finder.remove_pairing_record(&udid).await {
                    warn!("Failed to remove stale pairing record for {udid}: {e:?}");
                }
                None
            }
            RecordCheck::Unknown(reason) => {
                warn!(
                    "Could not validate pairing record for {udid} ({reason}); trusting it anyway"
                );
                Some(udid)
            }
        },
        None => None,
    };

    match validated_udid {
        Some(udid) => {
            register_with_manager(sender, udid.clone(), handle, location_id, product_id, speed)
                .await;
            info!(
                "Registered USB device {udid} (location_id=0x{location_id:x}, pid=0x{product_id:04x})"
            );
            udid
        }
        None => {
            info!(
                "No valid pairing record for {raw_udid}; starting pair flow (tap Trust on the device when prompted)"
            );
            let pairing_finder = pairing_file_finder.clone();
            let handle_for_pair = handle.clone();
            let sender_for_pair = sender.clone();
            let known_for_pair = known.clone();
            let raw_udid_for_pair = raw_udid.clone();
            tokio::spawn(async move {
                match pair_via_usb(&pairing_finder, &handle_for_pair, &raw_udid_for_pair).await {
                    Ok(udid) => {
                        info!("Successfully paired {udid}");
                        {
                            let mut k = known_for_pair.lock().await;
                            if k.contains_key(&key) {
                                k.insert(key, udid.clone());
                            }
                        }
                        register_with_manager(
                            &sender_for_pair,
                            udid,
                            handle_for_pair,
                            location_id,
                            product_id,
                            speed,
                        )
                        .await;
                    }
                    Err(e) => {
                        warn!("Pairing failed for {raw_udid_for_pair}: {e:?}");
                        handle_for_pair.shutdown().await;
                    }
                }
            });
            raw_udid
        }
    }
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
