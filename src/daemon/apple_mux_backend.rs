// Jackson Coxson

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, trace, warn};
use tokio::sync::{Mutex, oneshot};

use crate::apple_mux::{AppleMuxReader, AppleMuxWriter, Device, enumerate_paths};
use crate::config::NetmuxdConfig;
use crate::manager::ManagerSender;
use crate::pairing_file::PairingFileFinder;
use crate::usb::mux::{self, UsbMuxHandle};

use super::{DeviceMeta, connect_device, send_remove};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub(super) async fn run(sender: ManagerSender, config: NetmuxdConfig) {
    let pairing_file_finder = PairingFileFinder::new(&config);
    // Map interface path -> UDID. The path is stable for a physical
    // connection (its instance id changes across replug), so it's a
    // good hotplug key.
    let known: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));

    loop {
        let paths = match tokio::task::spawn_blocking(enumerate_paths).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                warn!("apple_mux enumerate failed: {e:?}");
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
            Err(e) => {
                warn!("apple_mux enumerate task panicked: {e:?}");
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };

        let current: HashSet<String> = paths.iter().cloned().collect();
        let active: HashSet<String> = known.lock().await.keys().cloned().collect();

        for path in paths {
            if active.contains(&path) {
                continue;
            }
            handle_connected(
                path,
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

async fn handle_connected(
    path: String,
    sender: ManagerSender,
    pairing_file_finder: PairingFileFinder,
    known: Arc<Mutex<HashMap<String, String>>>,
) {
    let product_id = parse_pid(&path).unwrap_or(0) as u64;
    let location_id = stable_hash(&path);
    let speed: u64 = 0;

    // Open + init + read serial are blocking USB I/O; run in one task.
    let opened = tokio::task::spawn_blocking({
        let path = path.clone();
        move || -> io::Result<(Device, String, u8, u8, u16)> {
            let device = Device::open(&path)?;
            // Init is required, the handle is unusable if it fails.
            device.init()?;
            let serial = device.serial()?;
            // Map the two bulk pipes to read (IN) / write (OUT) by
            // endpoint direction rather than assuming descriptor order.
            // Keep the OUT pipe's max-packet size: the writer needs it to
            // append a terminating ZLP on max-packet-multiple transfers.
            let (in1, mp1) = device.pipe_properties(1)?;
            let (in2, mp2) = device.pipe_properties(2)?;
            let (read_pipe, write_pipe, write_max_packet) = match (in1, in2) {
                (true, false) => (1u8, 2u8, mp2),
                (false, true) => (2u8, 1u8, mp1),
                _ => {
                    return Err(io::Error::other(
                        "mux interface: expected exactly one IN and one OUT bulk pipe",
                    ));
                }
            };
            Ok((device, serial, read_pipe, write_pipe, write_max_packet))
        }
    })
    .await;

    let (device, raw_udid, read_pipe, write_pipe, write_max_packet) = match opened {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            warn!("Failed to open apple_mux device {path}: {e:?}");
            return;
        }
        Err(e) => {
            warn!("apple_mux open task panicked: {e:?}");
            return;
        }
    };

    debug!("apple_mux device: pid=0x{product_id:04x} serial={raw_udid} path={path}");

    let (reader, writer): (AppleMuxReader, AppleMuxWriter) =
        device.pipes(read_pipe, write_pipe, write_max_packet);
    drop(device); // reader/writer hold their own Arc to the handle.

    let (exit_tx, exit_rx) = oneshot::channel::<u64>();
    let handle: UsbMuxHandle = mux::spawn(0, raw_udid.clone(), reader, writer, exit_tx);

    let map_udid = connect_device(
        &sender,
        &pairing_file_finder,
        &known,
        path.clone(),
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
        k.insert(path.clone(), map_udid);
    }

    let known = known.clone();
    let sender = sender.clone();
    let key = path.clone();
    tokio::spawn(async move {
        let _ = exit_rx.await;
        let removed = { known.lock().await.remove(&key) };
        if let Some(udid) = removed {
            trace!("USB mux task for {udid} exited");
            send_remove(&sender, udid).await;
        }
    });
}

/// Extract the `pid_XXXX` hex value from an interface path.
fn parse_pid(path: &str) -> Option<u16> {
    let lower = path.to_ascii_lowercase();
    let i = lower.find("pid_")? + 4;
    let hex: String = lower[i..].chars().take(4).collect();
    u16::from_str_radix(&hex, 16).ok()
}

fn stable_hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}
