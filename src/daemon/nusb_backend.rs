// Jackson Coxson
//
// nusb-based USB enumeration / hotplug / per-device wiring. Used on
// every platform except Windows. On Windows the libusbK backend takes
// over because nusb's WinUSB backend cannot call SetConfiguration on a
// composite device.

#![cfg(not(target_os = "windows"))]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use log::{debug, info, trace, warn};
use nusb::DeviceId;
use nusb::descriptors::TransferType;
use nusb::hotplug::HotplugEvent;
use nusb::transfer::{Bulk, ControlIn, ControlType, Direction, In, Out, Recipient};
use tokio::sync::Mutex;
use tokio::sync::oneshot;

use crate::config::NetmuxdConfig;
use crate::manager::ManagerSender;
use crate::pairing_file::PairingFileFinder;
use crate::usb::mux::{self, UsbMuxHandle};

use super::{
    APPLE_VID, DeviceMeta, INTERFACE_CLASS, INTERFACE_PROTOCOL, INTERFACE_SUBCLASS, PID_RANGE_HIGH,
    PID_RANGE_LOW, connect_device, send_remove,
};

const READER_BUF: usize = 16384;
const WRITER_BUF: usize = 16384;

// Apple vendor-specific USB request: switch the device into a mode
// that exposes a particular set of configurations. See
// https://theapplewiki.com/wiki/IOS_USB_device_modes
const APPLE_VEND_SET_MODE: u8 = 0x52;

// Mode 3 (iOS 10.3+): PTP + Apple Mobile Device + NCM. Triggers a USB
// reconnect, after which hotplug re-fires with the mux interface
// available.
const TARGET_MODE: u16 = 3;

pub(super) async fn run(sender: ManagerSender, config: NetmuxdConfig) {
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
                    send_remove(&sender, udid).await;
                }
            }
        }
    }

    warn!("USB hotplug stream ended");
}

// if MacOS does its silly little "oh you want to connect this device" thing
// it doesn't let you claim right away
const CLAIM_INTERFACE_RETRIES: u32 = 5;
const CLAIM_INTERFACE_RETRY_DELAY: Duration = Duration::from_secs(1);

async fn claim_interface_with_retry(
    device: &nusb::Device,
    interface_number: u8,
    serial: &Option<String>,
) -> Result<nusb::Interface, nusb::Error> {
    let mut attempt = 0;
    loop {
        match device.detach_and_claim_interface(interface_number).await {
            Ok(i) => return Ok(i),
            Err(e) if e.kind() == nusb::ErrorKind::Busy && attempt < CLAIM_INTERFACE_RETRIES => {
                attempt += 1;
                debug!(
                    "claim_interface({interface_number}) busy for {serial:?}, retrying ({attempt}/{CLAIM_INTERFACE_RETRIES}): {e:?}",
                );
                tokio::time::sleep(CLAIM_INTERFACE_RETRY_DELAY).await;
            }
            Err(e) => return Err(e),
        }
    }
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
    let serial = info.serial_number().map(|s| {
        s.trim_matches(|c: char| c == '\0' || c.is_whitespace())
            .to_string()
    });
    let location_id = device_location_id(&info);
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
            // mode 3 (mux + NCM, iOS 10.3+); mode 3 triggers a USB
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

    let interface =
        match claim_interface_with_retry(&device, target.interface_number, &serial).await {
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

    // We always need the mux task running before we can either pair
    // (which talks to lockdown over the mux) or register the device
    let (exit_tx, exit_rx) = oneshot::channel::<u64>();
    let handle: UsbMuxHandle = mux::spawn(0, raw_udid.clone(), reader, writer, exit_tx);

    let map_udid = connect_device(
        &sender,
        &pairing_file_finder,
        &known,
        id,
        handle,
        raw_udid.clone(),
        DeviceMeta {
            location_id,
            product_id,
            speed,
        },
    )
    .await;
    {
        let mut k = known.lock().await;
        k.insert(id, map_udid);
    }

    // When the mux task exits (USB error / device gone), clean up.
    let known = known.clone();
    let sender = sender.clone();
    tokio::spawn(async move {
        let _ = exit_rx.await;
        let removed = { known.lock().await.remove(&id) };
        if let Some(udid) = removed {
            trace!("USB mux task for {udid} exited");
            send_remove(&sender, udid).await;
        }
    });
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

/// Best-effort numeric location identifier for the device, used only
/// for the LocationID field clients see in `ListDevices`. macOS has a
/// real IOKit location ID; on other platforms we synthesize a stable
/// number from the bus + port chain so each plug-in gets a distinct
/// value within a single netmuxd run.
fn device_location_id(info: &nusb::DeviceInfo) -> u64 {
    #[cfg(target_os = "macos")]
    {
        info.location_id() as u64
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut acc: u64 = 0;
        for &b in info.port_chain() {
            acc = (acc << 4) | (b as u64 & 0xf);
        }
        acc
    }
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
