// Jackson Coxson
//
// USB enumeration + hotplug. Watches for Apple iOS devices over USB,
// switches them into mux mode, claims the right interface, and hands
// the bulk endpoints off to a per-device usb_mux task. Disconnects
// signal the manager to drop the device.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use idevice::{Idevice, services::lockdown::LockdownClient};
use log::{debug, info, trace, warn};
use nusb::DeviceId;
use nusb::descriptors::TransferType;
use nusb::hotplug::HotplugEvent;
use nusb::transfer::{Bulk, ControlIn, ControlType, Direction, In, Out, Recipient};
use tokio::sync::Mutex;
use tokio::sync::oneshot;

use crate::config::NetmuxdConfig;
use crate::manager::{ManagerRequest, ManagerRequestType, ManagerSender};
use crate::pairing_file::PairingFileFinder;
use crate::usb_mux::{self, UsbMuxHandle};

const APPLE_VID: u16 = 0x05ac;
const PID_RANGE_LOW: u16 = 0x1290;
const PID_RANGE_HIGH: u16 = 0x12af;

const INTERFACE_CLASS: u8 = 0xff;
const INTERFACE_SUBCLASS: u8 = 0xfe;
const INTERFACE_PROTOCOL: u8 = 0x02;

// Apple vendor-specific USB request: switch the device into a mode
// that exposes a particular set of configurations. See
// https://theapplewiki.com/wiki/IOS_USB_device_modes
const APPLE_VEND_SET_MODE: u8 = 0x52;

// Mode 3 (iOS 10.3+): PTP + Apple Mobile Device + NCM. Triggers a USB
// reconnect, after which hotplug re-fires with the mux interface
// available.
const TARGET_MODE: u16 = 3;

const READER_BUF: usize = 16384;
const WRITER_BUF: usize = 16384;

pub async fn discover(sender: ManagerSender, config: NetmuxdConfig) {
    let pairing_file_finder = PairingFileFinder::new(&config);

    // Map nusb DeviceId -> UDID, so we can issue RemoveDevice on
    // disconnect events (which only carry the DeviceId).
    let known: Arc<Mutex<HashMap<DeviceId, String>>> = Arc::new(Mutex::new(HashMap::new()));

    // Start the hotplug stream first so we don't miss events that
    // arrive between the initial enumeration and now.
    let mut watch = match nusb::watch_devices() {
        Ok(w) => w,
        Err(e) => {
            warn!("Failed to start USB hotplug watch: {e:?}");
            return;
        }
    };

    // Initial scan.
    match nusb::list_devices().await {
        Ok(iter) => {
            for info in iter {
                if !is_apple_mux(&info) {
                    continue;
                }
                handle_connected(
                    info,
                    sender.clone(),
                    pairing_file_finder.clone(),
                    known.clone(),
                )
                .await;
            }
        }
        Err(e) => warn!("USB list_devices failed: {e:?}"),
    }

    while let Some(event) = watch.next().await {
        match event {
            HotplugEvent::Connected(info) => {
                if !is_apple_mux(&info) {
                    continue;
                }
                handle_connected(
                    info,
                    sender.clone(),
                    pairing_file_finder.clone(),
                    known.clone(),
                )
                .await;
            }
            HotplugEvent::Disconnected(id) => {
                let udid = { known.lock().await.remove(&id) };
                if let Some(udid) = udid {
                    info!("USB device {udid} disconnected");
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
            }
        }
    }

    warn!("USB hotplug stream ended");
}

fn is_apple_mux(info: &nusb::DeviceInfo) -> bool {
    info.vendor_id() == APPLE_VID
        && info.product_id() >= PID_RANGE_LOW
        && info.product_id() <= PID_RANGE_HIGH
}

async fn handle_connected(
    info: nusb::DeviceInfo,
    sender: ManagerSender,
    pairing_file_finder: PairingFileFinder,
    known: Arc<Mutex<HashMap<DeviceId, String>>>,
) {
    let id = info.id();
    let serial = info.serial_number().map(|s| s.to_string());
    let location_id = info.location_id() as u64;
    let product_id = info.product_id() as u64;
    let speed = speed_to_bps(info.speed());

    debug!(
        "USB device candidate: vid=0x{:04x} pid=0x{:04x} serial={:?} location_id=0x{:x}",
        info.vendor_id(),
        info.product_id(),
        serial,
        location_id
    );

    let device = match info.open().await {
        Ok(d) => d,
        Err(e) => {
            warn!("Failed to open USB device {serial:?}: {e:?}");
            return;
        }
    };

    // Walk configurations from highest to lowest, looking for one
    // with our class/sub/proto interface and two bulk endpoints.
    let target = match find_mux_target(&device) {
        Some(t) => t,
        None => {
            // Device is in a mode (e.g. mode 5 / NCM Direct on
            // iOS 17+) where the mux interface isn't exposed. Send
            // the vendor SET_MODE control transfer to switch into
            // mode 3 (mux + NCM, iOS 10.3+). Mode 3 triggers a USB
            // reconnect, so we drop this handle and let hotplug
            // re-fire the Connected event with the new configs.
            info!(
                "No usbmux interface on {serial:?}; switching to mode {TARGET_MODE} via SET_MODE",
            );
            match device
                .control_in(
                    ControlIn {
                        control_type: ControlType::Vendor,
                        recipient: Recipient::Device,
                        request: APPLE_VEND_SET_MODE,
                        value: 0,
                        index: TARGET_MODE,
                        length: 1,
                    },
                    Duration::from_secs(2),
                )
                .await
            {
                Ok(resp) => {
                    debug!("SET_MODE {TARGET_MODE} on {serial:?} returned {:?}", resp);
                }
                Err(e) => {
                    debug!("SET_MODE {TARGET_MODE} on {serial:?} errored: {e:?}");
                }
            }
            return;
        }
    };

    let active_value = device
        .active_configuration()
        .ok()
        .map(|c| c.configuration_value());
    if active_value != Some(target.config_value) {
        debug!(
            "Setting device {serial:?} configuration: {:?} -> {}",
            active_value, target.config_value
        );
        if let Err(e) = device.set_configuration(target.config_value).await {
            warn!(
                "set_configuration({}) failed for {serial:?}: {e:?}",
                target.config_value
            );
            return;
        }
    }

    let interface = match device
        .detach_and_claim_interface(target.interface_number)
        .await
    {
        Ok(i) => i,
        Err(e) => {
            warn!(
                "claim_interface({}) failed for {serial:?}: {e:?}",
                target.interface_number
            );
            return;
        }
    };

    let ep_in = match interface.endpoint::<Bulk, In>(target.ep_in) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                "Failed to open bulk-in endpoint 0x{:02x}: {e:?}",
                target.ep_in
            );
            return;
        }
    };
    let ep_out = match interface.endpoint::<Bulk, Out>(target.ep_out) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                "Failed to open bulk-out endpoint 0x{:02x}: {e:?}",
                target.ep_out
            );
            return;
        }
    };

    let reader = ep_in.reader(READER_BUF).with_num_transfers(4);
    let writer = ep_out.writer(WRITER_BUF).with_num_transfers(4);

    let raw_udid = match serial.clone() {
        Some(s) => s,
        None => {
            warn!("USB device has no serial; skipping");
            return;
        }
    };

    let existing_udid = resolve_paired_udid(&pairing_file_finder, &raw_udid).await;

    // We always need the mux task running before we can either pair
    // (which talks to lockdown over the mux) or register the device
    let (exit_tx, exit_rx) = oneshot::channel::<u64>();
    let handle: UsbMuxHandle = usb_mux::spawn(0, raw_udid.clone(), reader, writer, exit_tx);

    let registered_udid = match existing_udid {
        Some(udid) => {
            // We already trust this device — register immediately.
            register_with_manager(
                &sender,
                udid.clone(),
                handle.clone(),
                location_id,
                product_id,
                speed,
            )
            .await;
            info!(
                "Registered USB device {udid} (location_id=0x{:x}, pid=0x{:04x})",
                location_id,
                info.product_id()
            );
            Some(udid)
        }
        None => {
            // No pairing record yet, make one
            info!(
                "No pairing record for {raw_udid}; starting pair flow (tap Trust on the device when prompted)"
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
                        // Replace the placeholder raw_udid in `known`
                        // with the canonical form so disconnect
                        // cleanup matches what we register below.
                        {
                            let mut k = known_for_pair.lock().await;
                            if k.contains_key(&id) {
                                k.insert(id, udid.clone());
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
            // We don't yet have a confirmed UDID. Register raw_udid
            // in `known` as a placeholder. The pair task replaces
            // it on success, and disconnect cleanup uses whatever's
            // current.
            None
        }
    };

    let map_udid = registered_udid.unwrap_or_else(|| raw_udid.clone());
    {
        let mut k = known.lock().await;
        k.insert(id, map_udid);
    }

    // When the mux task exits (USB error / device gone), clean up.
    // We look up the UDID from `known` at exit time so we use whatever
    // was registered last. Pairing may have replaced the raw serial
    // with the canonical UDID after the entry was first inserted.
    let known = known.clone();
    let sender = sender.clone();
    tokio::spawn(async move {
        let _ = exit_rx.await;
        let removed = { known.lock().await.remove(&id) };
        if let Some(udid) = removed {
            trace!("USB mux task for {udid} exited");
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
    });
}

async fn register_with_manager(
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
async fn pair_via_usb(
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
        .pair(host_id, system_buid)
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

struct MuxTarget {
    config_value: u8,
    interface_number: u8,
    ep_in: u8,
    ep_out: u8,
}

fn find_mux_target(device: &nusb::Device) -> Option<MuxTarget> {
    // Try configurations highest-numbered first, mirroring usbmuxd.
    let mut configs: Vec<_> = device.configurations().collect();
    configs.sort_by_key(|c| std::cmp::Reverse(c.configuration_value()));

    for cfg in configs {
        for intf in cfg.interfaces() {
            for alt in intf.alt_settings() {
                if alt.class() != INTERFACE_CLASS
                    || alt.subclass() != INTERFACE_SUBCLASS
                    || alt.protocol() != INTERFACE_PROTOCOL
                {
                    continue;
                }
                let mut ep_in = None;
                let mut ep_out = None;
                for ep in alt.endpoints() {
                    if ep.transfer_type() != TransferType::Bulk {
                        continue;
                    }
                    match ep.direction() {
                        Direction::In => ep_in = Some(ep.address()),
                        Direction::Out => ep_out = Some(ep.address()),
                    }
                }
                if let (Some(ep_in), Some(ep_out)) = (ep_in, ep_out) {
                    return Some(MuxTarget {
                        config_value: cfg.configuration_value(),
                        interface_number: alt.interface_number(),
                        ep_in,
                        ep_out,
                    });
                }
            }
        }
    }
    None
}

async fn resolve_paired_udid(finder: &PairingFileFinder, raw: &str) -> Option<String> {
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

fn speed_to_bps(speed: Option<nusb::Speed>) -> u64 {
    use nusb::Speed::*;
    match speed {
        Some(Low) => 1_500_000,
        Some(Full) => 12_000_000,
        Some(High) => 480_000_000,
        Some(Super) => 5_000_000_000,
        Some(SuperPlus) => 10_000_000_000,
        _ => 0,
    }
}
