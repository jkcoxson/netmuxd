// Jackson Coxson
// When I original wrote netmuxd, I was a naive high school student
// and placed everything in an Arc<Muxtex<>>. While it has its uses,
// I much prefer the channel-runner paradigm for multithreaded programs.

use std::{collections::HashMap, net::IpAddr};

use crossfire::{AsyncRx, MAsyncTx, mpmc::unbounded_async};
use log::debug;
use tokio::sync::oneshot::Sender;

use crate::{
    config::NetmuxdConfig, devices::MuxerDevice, heartbeat::heartbeat,
    pairing_file::PairingFileFinder, usb_mux::UsbMuxHandle,
};

pub type ManagerSender = MAsyncTx<ManagerRequest>;
pub type ManagerReceiver = AsyncRx<ManagerRequest>;

pub struct ManagerRequest {
    pub request_type: ManagerRequestType,
    pub response: Option<Sender<plist::Dictionary>>,
}

pub enum ManagerRequestType {
    DiscoveredNetworkDevice {
        udid: String,
        network_address: IpAddr,
        service_name: String,
        connection_type: String,
    },
    DiscoveredUsbDevice {
        udid: String,
        location_id: u64,
        product_id: u64,
        speed: u64,
        handle: UsbMuxHandle,
    },
    DeferredMuxerAdd {
        device: MuxerDevice,
        response: Option<Sender<plist::Dictionary>>,
    },
    RemoveDevice {
        udid: String,
        connection_type: Option<String>,
    },
    ListDevices,
    GetDeviceConnection {
        id: u64,
        response: tokio::sync::oneshot::Sender<Option<DeviceConnection>>,
    },
    HeartbeatFailed {
        udid: String,
    },
    OpenSocket {
        device_id: u64,
        kill: Sender<()>,
    },
}

#[derive(Clone)]
pub struct DeviceConnection {
    pub connection_type: String,
    pub network_address: Option<IpAddr>,
    pub usb: Option<UsbMuxHandle>,
}

impl ManagerRequest {
    pub fn discovered_device(
        udid: String,
        network_address: IpAddr,
        service_name: String,
        connection_type: String,
    ) -> Self {
        Self {
            request_type: ManagerRequestType::DiscoveredNetworkDevice {
                udid,
                network_address,
                service_name,
                connection_type,
            },
            response: None,
        }
    }
    pub fn heartbeat_failed(udid: String) -> Self {
        Self {
            request_type: ManagerRequestType::HeartbeatFailed { udid },
            response: None,
        }
    }
}

/// Find the device_id for a (udid, connection_type) pair, if any
fn find_device_id(
    devices: &HashMap<u64, MuxerDevice>,
    udid: &str,
    connection_type: &str,
) -> Option<u64> {
    devices
        .iter()
        .find(|(_, d)| d.serial_number == udid && d.connection_type == connection_type)
        .map(|(id, _)| *id)
}

fn drop_entry(
    id: u64,
    devices: &mut HashMap<u64, MuxerDevice>,
    usb_handles: &mut HashMap<u64, UsbMuxHandle>,
    open_sockets: &mut HashMap<u64, Vec<Sender<()>>>,
) -> Option<UsbMuxHandle> {
    devices.remove(&id);
    if let Some(l) = open_sockets.remove(&id) {
        for s in l {
            let _ = s.send(());
        }
    }
    usb_handles.remove(&id)
}

pub fn new_manager_thread(config: &NetmuxdConfig) -> ManagerSender {
    let (manager_sender, manager_recv) = new_channel_pair();
    let to_return = manager_sender.clone();
    let config = config.clone();
    let pairing_file_finder = PairingFileFinder::new(&config);

    let mut devices: HashMap<u64, MuxerDevice> = HashMap::new();
    let mut usb_handles: HashMap<u64, UsbMuxHandle> = HashMap::new();
    let mut open_sockets: HashMap<u64, Vec<Sender<()>>> = HashMap::new();
    let mut last_index: u64 = 1;
    let mut last_interface_index: u64 = 1;

    tokio::task::spawn(async move {
        loop {
            let message = match manager_recv.recv().await {
                Ok(m) => m,
                Err(_) => {
                    debug!("All senders are closed, stopping manager thread");
                    break;
                }
            };
            match message.request_type {
                ManagerRequestType::DiscoveredNetworkDevice {
                    udid,
                    network_address,
                    service_name,
                    connection_type,
                } => {
                    if find_device_id(&devices, &udid, &connection_type).is_some() {
                        continue;
                    }
                    let pairing_file = match pairing_file_finder.get_pairing_record(&udid).await {
                        Ok(p) => p,
                        Err(e) => {
                            debug!("Failed to get pairing record: {e:?}");
                            continue;
                        }
                    };

                    let device = MuxerDevice {
                        connection_type,
                        device_id: last_index,
                        interface_index: last_interface_index,
                        serial_number: udid.clone(),
                        network_address: Some(network_address),
                        service_name: Some(service_name),
                        connection_speed: None,
                        location_id: None,
                        product_id: None,
                    };
                    last_index = last_index.wrapping_add(1);
                    last_interface_index = last_interface_index.wrapping_add(1);

                    if config.use_heartbeat {
                        heartbeat(
                            device,
                            message.response,
                            pairing_file,
                            manager_sender.clone(),
                        )
                        .await;

                        continue;
                    }

                    devices.insert(device.device_id, device);
                    if let Some(response) = message.response {
                        response
                            .send(idevice::plist!(dict {
                            "Result": 1,
                            }))
                            .ok();
                    }
                }
                ManagerRequestType::DiscoveredUsbDevice {
                    udid,
                    location_id,
                    product_id,
                    speed,
                    handle,
                } => {
                    if let Some(id) = find_device_id(&devices, &udid, "USB") {
                        // Replace the handle but keep the device entry.
                        usb_handles.insert(id, handle);
                        continue;
                    }
                    let device = MuxerDevice {
                        connection_type: "USB".into(),
                        device_id: last_index,
                        interface_index: last_interface_index,
                        serial_number: udid.clone(),
                        network_address: None,
                        service_name: None,
                        connection_speed: Some(speed),
                        location_id: Some(location_id),
                        product_id: Some(product_id),
                    };
                    println!("Adding USB device {udid}");
                    devices.insert(last_index, device);
                    usb_handles.insert(last_index, handle);
                    last_index = last_index.wrapping_add(1);
                    last_interface_index = last_interface_index.wrapping_add(1);
                }
                ManagerRequestType::DeferredMuxerAdd { device, response } => {
                    println!("Adding network device {}", device.serial_number);
                    devices.insert(device.device_id, device);
                    if let Some(response) = response {
                        response
                            .send(idevice::plist!(dict {
                            "Result": 1,
                            }))
                            .ok();
                    }
                }
                ManagerRequestType::RemoveDevice {
                    udid,
                    connection_type,
                } => {
                    let ids: Vec<u64> = devices
                        .iter()
                        .filter(|(_, d)| {
                            d.serial_number == udid
                                && connection_type
                                    .as_deref()
                                    .map(|ct| d.connection_type == ct)
                                    .unwrap_or(true)
                        })
                        .map(|(id, _)| *id)
                        .collect();
                    for id in ids {
                        if let Some(h) =
                            drop_entry(id, &mut devices, &mut usb_handles, &mut open_sockets)
                        {
                            h.shutdown().await;
                        }
                    }
                }
                ManagerRequestType::ListDevices => {
                    if let Some(response) = message.response {
                        let mut device_list = Vec::new();
                        for d in devices.values() {
                            let to_push = idevice::plist!(dict {
                                "DeviceID": d.device_id,
                                "MessageType": "Attached",
                                "Properties": plist::Value::Dictionary(d.into()),
                            });
                            device_list.push(plist::Value::Dictionary(to_push));
                        }
                        response
                            .send(idevice::plist!(dict {
                                "DeviceList": device_list
                            }))
                            .ok();
                    }
                }
                ManagerRequestType::GetDeviceConnection { id, response } => {
                    let lookup = devices.get(&id).map(|d| DeviceConnection {
                        connection_type: d.connection_type.clone(),
                        network_address: d.network_address,
                        usb: usb_handles.get(&id).cloned(),
                    });
                    let _ = response.send(lookup);
                }
                ManagerRequestType::HeartbeatFailed { udid } => {
                    if let Some(id) = find_device_id(&devices, &udid, "Network") {
                        drop_entry(id, &mut devices, &mut usb_handles, &mut open_sockets);
                    }
                }
                ManagerRequestType::OpenSocket { device_id, kill } => {
                    open_sockets.entry(device_id).or_default().push(kill);
                }
            }
        }
    });

    to_return
}

fn new_channel_pair() -> (ManagerSender, ManagerReceiver) {
    let (t, r) = unbounded_async();
    (t.into(), r.into())
}
