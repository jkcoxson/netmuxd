// Jackson Coxson
// When I original wrote netmuxd, I was a naive high school student
// and placed everything in an Arc<Muxtex<>>. While it has its uses,
// I much prefer the channel-runner paradigm for multithreaded programs.

use std::{collections::HashMap, net::IpAddr};

use crossfire::{mpmc::unbounded_async, AsyncRx, MAsyncTx};
use log::debug;
use tokio::sync::oneshot::Sender;

use crate::{
    config::NetmuxdConfig, devices::MuxerDevice, heartbeat::heartbeat,
    pairing_file::PairingFileFinder,
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
    DeferredMuxerAdd {
        device: MuxerDevice,
        response: Option<Sender<plist::Dictionary>>,
    },
    RemoveDevice {
        udid: String,
    },
    ListDevices,
    GetDeviceNetworkAddress {
        id: u64,
    },
    HeartbeatFailed {
        udid: String,
    },
    OpenSocket {
        udid: String,
        kill: Sender<()>,
    },
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

/// Spinner thread
///
/// 1. Watches for new devices over mDNS, and starts a heartbeat for them
pub fn new_manager_thread(config: &NetmuxdConfig) -> ManagerSender {
    let (manager_sender, manager_recv) = new_channel_pair();
    let to_return = manager_sender.clone();
    let config = config.clone();
    let pairing_file_finder = PairingFileFinder::new(&config);

    let mut devices: HashMap<String, MuxerDevice> = HashMap::new();
    let mut last_index: u64 = 1;
    let mut last_interface_index: u64 = 1;
    let mut open_sockets: HashMap<String, Vec<Sender<()>>> = HashMap::new();

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
                    if devices.contains_key(&udid) {
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

                    if config.use_heartbeat {
                        // We will spawn the heartbeat in a new thread,
                        // and then the thread will send the deferred add
                        // if successful.
                        heartbeat(
                            device,
                            message.response,
                            pairing_file,
                            manager_sender.clone(),
                        )
                        .await;

                        continue;
                    }

                    devices.insert(udid.clone(), device);
                    last_index = last_index.wrapping_add(1);
                    last_interface_index = last_interface_index.wrapping_add(1);
                    if let Some(response) = message.response {
                        response
                            .send(idevice::plist!(dict {
                            "Result": 1,
                            }))
                            .ok();
                    }
                }
                ManagerRequestType::DeferredMuxerAdd { device, response } => {
                    println!("Adding device {}", device.serial_number);
                    devices.insert(device.serial_number.clone(), device);
                    if let Some(response) = response {
                        response
                            .send(idevice::plist!(dict {
                            "Result": 1,
                            }))
                            .ok();
                    }
                }
                ManagerRequestType::RemoveDevice { udid } => {
                    devices.remove(&udid);
                }
                ManagerRequestType::ListDevices => {
                    if let Some(response) = message.response {
                        let mut device_list = Vec::new();
                        for i in &devices {
                            let to_push = idevice::plist!(dict {
                                "DeviceID": i.1.device_id,
                                "MessageType": "Attached",
                                "Properties": plist::Value::Dictionary(i.1.into()),
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
                ManagerRequestType::GetDeviceNetworkAddress { id } => {
                    if let Some(response) = message.response {
                        if let Some(device) = devices
                            .values()
                            .find(|x| x.device_id == id && x.network_address.is_some())
                        {
                            response
                                .send(idevice::plist!(dict {
                                    "found": true,
                                    "address": device.network_address.unwrap().to_string(),
                                    "udid": device.serial_number.to_string(),
                                }))
                                .ok();
                        } else {
                            response.send(idevice::plist!(dict {"found": false})).ok();
                        }
                    }
                }
                ManagerRequestType::HeartbeatFailed { udid } => {
                    devices.remove(&udid);
                    if let Some(l) = open_sockets.remove(&udid) {
                        for s in l {
                            let _ = s.send(());
                        }
                    }
                }
                ManagerRequestType::OpenSocket { udid, kill } => {
                    match open_sockets.get_mut(&udid) {
                        Some(l) => l.push(kill),
                        None => {
                            let l = vec![kill];
                            open_sockets.insert(udid, l);
                        }
                    };
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
