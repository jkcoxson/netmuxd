// Jackson Coxson
//
// Windows USB backend, built on `crate::libusbk`. nusb's WinUSB backend
// can't call SetConfiguration on Apple's composite devices, so we go
// through `libusbK.dll` instead.
//
// libusbK has no native hotplug; we poll `LstK_Init` on a 2-second
// cadence and diff against the previous snapshot.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, trace, warn};
use tokio::sync::Mutex;
use tokio::sync::oneshot;

use crate::config::NetmuxdConfig;
use crate::libusbk::{Device, DeviceList, LibusbkReader, LibusbkWriter};
use crate::manager::ManagerSender;
use crate::pairing_file::PairingFileFinder;
use crate::usb_mux::{self, UsbMuxHandle};

use super::{
    APPLE_VID, INTERFACE_CLASS, INTERFACE_PROTOCOL, INTERFACE_SUBCLASS, PID_RANGE_HIGH,
    PID_RANGE_LOW, pair_via_usb, register_with_manager, resolve_paired_udid, send_remove,
};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

// Standard USB descriptor types. (Mirrors `USB_DESCRIPTOR_TYPE_*` in
// the libusbK header but we don't want to plumb every variant
// through the FFI module just for this parser.)
const USB_DESC_DEVICE: u8 = 0x01;
const USB_DESC_CONFIG: u8 = 0x02;
const USB_DESC_INTERFACE: u8 = 0x04;
const USB_DESC_ENDPOINT: u8 = 0x05;

const ENDPOINT_DIR_IN: u8 = 0x80;
const ENDPOINT_TYPE_BULK: u8 = 0x02;
const ENDPOINT_TYPE_MASK: u8 = 0x03;

// --- entry point -------------------------------------------------------

pub(super) async fn run(sender: ManagerSender, config: NetmuxdConfig) {
    let pairing_file_finder = PairingFileFinder::new(&config);

    // Map device-instance ID -> UDID. The Windows DeviceID string is
    // stable across the lifetime of a single physical connection.
    let known: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));

    loop {
        let candidates = match scan().await {
            Ok(c) => c,
            Err(e) => {
                warn!("libusbK device scan failed: {e:?}");
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };

        let current: HashSet<String> = candidates.iter().map(|c| c.device_id.clone()).collect();
        let active: HashSet<String> = known.lock().await.keys().cloned().collect();

        for cand in candidates {
            if active.contains(&cand.device_id) {
                continue;
            }
            handle_connected(
                cand,
                sender.clone(),
                pairing_file_finder.clone(),
                known.clone(),
            )
            .await;
        }

        for stale in active.difference(&current) {
            let udid = { known.lock().await.remove(stale) };
            if let Some(udid) = udid {
                info!("USB device {udid} disconnected");
                send_remove(&sender, udid).await;
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// --- enumeration -------------------------------------------------------

#[derive(Clone, Debug)]
struct Candidate {
    /// Windows DeviceID instance string. Stable per physical
    /// connection; used as the key for hotplug diffing.
    device_id: String,
    serial: Option<String>,
    pid: u16,
    bus_number: i32,
    device_address: i32,
}

async fn scan() -> io::Result<Vec<Candidate>> {
    tokio::task::spawn_blocking(|| -> io::Result<Vec<Candidate>> {
        let list = DeviceList::new()?;
        let mut out = Vec::new();
        for info in list.iter() {
            if !is_apple_mux(info.vid(), info.pid()) {
                continue;
            }
            let Some(device_id) = info.device_id() else {
                continue;
            };
            out.push(Candidate {
                device_id,
                serial: info.serial_number(),
                pid: info.pid(),
                bus_number: info.bus_number(),
                device_address: info.device_address(),
            });
        }
        Ok(out)
    })
    .await
    .map_err(io::Error::other)?
}

fn is_apple_mux(vid: u16, pid: u16) -> bool {
    vid == APPLE_VID && (PID_RANGE_LOW..=PID_RANGE_HIGH).contains(&pid)
}

// --- per-device wiring -------------------------------------------------

async fn handle_connected(
    cand: Candidate,
    sender: ManagerSender,
    pairing_file_finder: PairingFileFinder,
    known: Arc<Mutex<HashMap<String, String>>>,
) {
    let device_id = cand.device_id.clone();
    let serial = cand.serial.clone();
    let location_id = synthesize_location_id(&cand);
    let product_id = cand.pid as u64;
    // libusbK only reports high vs full/low speed via
    // UsbK_QueryDeviceInformation, and the LocationID/Speed fields in
    // ListDevices are informational only.
    let speed: u64 = 0;

    debug!(
        "USB device candidate: pid=0x{:04x} serial={:?} location_id=0x{:x}",
        cand.pid, serial, location_id,
    );

    // Open + descriptor walk + set_configuration + claim are all
    // synchronous USB I/O, some of which (set_configuration) does a
    // bus reset. Run the chain in one blocking task.
    let opened = tokio::task::spawn_blocking({
        let device_id = device_id.clone();
        move || -> io::Result<Option<(Device, MuxTarget)>> {
            let list = DeviceList::new()?;
            let info = list
                .iter()
                .find(|i| i.device_id().as_deref() == Some(device_id.as_str()));
            let Some(info) = info else {
                return Ok(None);
            };
            let device = Device::open(&info)?;
            let Some(target) = find_mux_target(&device)? else {
                return Ok(None);
            };
            // SetConfiguration only really works against libusb0.sys;
            // libusbK.sys/WinUSB.sys emulate it as a no-op-or-fail.
            // Skip the call when we're already on the desired config.
            let current_cfg = device.get_configuration().unwrap_or(0);
            if current_cfg != target.config_value {
                device.set_configuration(target.config_value)?;
            }
            device.claim_interface(target.interface_number)?;
            Ok(Some((device, target)))
        }
    })
    .await;

    let (device, target) = match opened {
        Ok(Ok(Some(t))) => t,
        Ok(Ok(None)) => {
            warn!("Device {device_id} not found in re-scan or has no mux interface");
            return;
        }
        Ok(Err(e)) => {
            warn!("Failed to open libusbK device {serial:?}: {e:?}");
            return;
        }
        Err(e) => {
            warn!("libusbK open task panicked: {e:?}");
            return;
        }
    };

    let (reader, writer): (LibusbkReader, LibusbkWriter) =
        device.pipes(target.ep_in, target.ep_out);
    drop(device); // Reader/writer hold their own Arcs to the handle.

    let raw_udid = match serial.clone() {
        Some(s) => s,
        None => {
            warn!("USB device has no serial; skipping");
            return;
        }
    };

    let existing_udid = resolve_paired_udid(&pairing_file_finder, &raw_udid).await;

    // The mux task must be running before we can pair (which talks to
    // lockdown over the mux) or register the device.
    let (exit_tx, exit_rx) = oneshot::channel::<u64>();
    let handle: UsbMuxHandle = usb_mux::spawn(0, raw_udid.clone(), reader, writer, exit_tx);

    let registered_udid = match existing_udid {
        Some(udid) => {
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
                location_id, cand.pid,
            );
            Some(udid)
        }
        None => {
            info!(
                "No pairing record for {raw_udid}; starting pair flow (tap Trust on the device when prompted)"
            );
            let pairing_finder = pairing_file_finder.clone();
            let handle_for_pair = handle.clone();
            let sender_for_pair = sender.clone();
            let known_for_pair = known.clone();
            let raw_udid_for_pair = raw_udid.clone();
            let key_for_pair = device_id.clone();
            tokio::spawn(async move {
                match pair_via_usb(&pairing_finder, &handle_for_pair, &raw_udid_for_pair).await {
                    Ok(udid) => {
                        info!("Successfully paired {udid}");
                        {
                            let mut k = known_for_pair.lock().await;
                            if k.contains_key(&key_for_pair) {
                                k.insert(key_for_pair, udid.clone());
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
            None
        }
    };

    let map_udid = registered_udid.unwrap_or_else(|| raw_udid.clone());
    {
        let mut k = known.lock().await;
        k.insert(device_id.clone(), map_udid);
    }

    let known = known.clone();
    let sender = sender.clone();
    let key = device_id.clone();
    tokio::spawn(async move {
        let _ = exit_rx.await;
        let removed = { known.lock().await.remove(&key) };
        if let Some(udid) = removed {
            trace!("USB mux task for {udid} exited");
            send_remove(&sender, udid).await;
        }
    });
}

fn synthesize_location_id(cand: &Candidate) -> u64 {
    ((cand.bus_number as u64) << 32) | (cand.device_address as u64 & 0xffff_ffff)
}

// --- descriptor walking ------------------------------------------------

#[derive(Copy, Clone, Debug)]
struct MuxTarget {
    config_value: u8,
    interface_number: u8,
    ep_in: u8,
    ep_out: u8,
}

fn find_mux_target(device: &Device) -> io::Result<Option<MuxTarget>> {
    let mut dev_desc = [0u8; 18];
    let n = device.get_descriptor(USB_DESC_DEVICE, 0, 0, &mut dev_desc)?;
    if n < 18 {
        return Ok(None);
    }
    let num_configs = dev_desc[17];

    // Highest configuration first, mirroring the nusb path and
    // usbmuxd's preference for mux + NCM (config 4) over mux-only
    // (config 3).
    for cfg_idx in (0..num_configs).rev() {
        // First read the 9-byte config header to learn wTotalLength.
        let mut head = [0u8; 9];
        let n = device.get_descriptor(USB_DESC_CONFIG, cfg_idx, 0, &mut head)?;
        if n < 9 {
            continue;
        }
        let total_len = u16::from_le_bytes([head[2], head[3]]) as usize;
        let cfg_value = head[5];
        if total_len < 9 {
            continue;
        }

        // Now read the full config descriptor blob (config + all
        // interfaces + all endpoints, concatenated).
        let mut blob = vec![0u8; total_len];
        let n = device.get_descriptor(USB_DESC_CONFIG, cfg_idx, 0, &mut blob)?;
        if n < total_len {
            continue;
        }

        if let Some(t) = scan_config_blob(&blob, cfg_value) {
            return Ok(Some(t));
        }
    }
    Ok(None)
}

/// Walk a config descriptor blob looking for an interface with our
/// class/sub/proto signature that exposes both a bulk-IN and a bulk-OUT
/// endpoint. We treat every INTERFACE descriptor as a separate alt
/// setting; the first match wins.
fn scan_config_blob(blob: &[u8], cfg_value: u8) -> Option<MuxTarget> {
    let mut off = 0;
    let mut intf_num: Option<u8> = None;
    let mut matches = false;
    let mut ep_in: Option<u8> = None;
    let mut ep_out: Option<u8> = None;

    let finalize =
        |intf_num: Option<u8>, ep_in: Option<u8>, ep_out: Option<u8>| -> Option<MuxTarget> {
            let (Some(num), Some(i), Some(o)) = (intf_num, ep_in, ep_out) else {
                return None;
            };
            Some(MuxTarget {
                config_value: cfg_value,
                interface_number: num,
                ep_in: i,
                ep_out: o,
            })
        };

    while off + 2 <= blob.len() {
        let len = blob[off] as usize;
        let dtype = blob[off + 1];
        if len < 2 || off + len > blob.len() {
            break;
        }
        let desc = &blob[off..off + len];

        match dtype {
            USB_DESC_INTERFACE => {
                if matches && let Some(t) = finalize(intf_num, ep_in, ep_out) {
                    return Some(t);
                }
                ep_in = None;
                ep_out = None;
                if len >= 9 {
                    intf_num = Some(desc[2]);
                    let class = desc[5];
                    let sub = desc[6];
                    let proto = desc[7];
                    matches = class == INTERFACE_CLASS
                        && sub == INTERFACE_SUBCLASS
                        && proto == INTERFACE_PROTOCOL;
                } else {
                    intf_num = None;
                    matches = false;
                }
            }
            USB_DESC_ENDPOINT => {
                if matches && len >= 7 {
                    let addr = desc[2];
                    let attrs = desc[3];
                    if attrs & ENDPOINT_TYPE_MASK == ENDPOINT_TYPE_BULK {
                        if addr & ENDPOINT_DIR_IN != 0 {
                            ep_in = Some(addr);
                        } else {
                            ep_out = Some(addr);
                        }
                    }
                }
            }
            _ => {}
        }
        off += len;
    }

    if matches {
        return finalize(intf_num, ep_in, ep_out);
    }
    None
}
