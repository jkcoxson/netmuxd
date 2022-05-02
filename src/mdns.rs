// jkcoxson

use crate::{
    central_data::{CentralData, Device},
    heartbeat,
};
use log::info;
use std::net::IpAddr;
use std::sync::Arc;

use tokio::sync::Mutex;

#[cfg(not(feature = "zeroconf"))]
use {
    futures_util::{pin_mut, stream::StreamExt},
    mdns::{Record, RecordKind},
    std::time::Duration,
};

#[cfg(feature = "zeroconf")]
use {
    zeroconf::prelude::*,
    zeroconf::{MdnsBrowser, ServiceType},
};

const SERVICE_NAME: &'static str = "apple-mobdev2";
const SERVICE_PROTOCOL: &'static str = "tcp";

#[cfg(feature = "zeroconf")]
pub async fn discover(data: Arc<Mutex<CentralData>>) {
    let service_name = format!("_{}._{}.local", SERVICE_NAME, SERVICE_PROTOCOL);
    println!("Starting mDNS discovery for {} with zeroconf", service_name);

    let mut browser = MdnsBrowser::new(ServiceType::new(SERVICE_NAME, SERVICE_PROTOCOL).unwrap());
    loop {
        let result = browser.browse_async().await;

        if let Ok(service) = result {
            info!("Service discovered: {:?}", service);
            let name = service.name();
            if !name.contains("@") {
                continue;
            }
            let addr = match service.address() {
                addr if addr.contains(":") => IpAddr::V6(addr.parse().unwrap()),
                addr => IpAddr::V4(addr.parse().unwrap()),
            };

            let mac_addr = name.split("@").collect::<Vec<&str>>()[0];
            let mut lock = data.lock().await;
            if let Ok(udid) = lock.get_udid(mac_addr.to_string()) {
                if lock.devices.contains_key(&udid) {
                    info!("Device has already been added to muxer, skipping");
                    continue;
                }
                println!("Adding device {}", udid);
                let handle = heartbeat::heartbeat(udid.to_string(), addr, data.clone());
                let device = Device {
                    connection_type: "Network".to_string(),
                    device_id: 200,
                    service_name: service_name.to_string(),
                    interface_index: 300,
                    network_address: addr,
                    serial_number: udid.to_string(),
                    heartbeat_handle: Some(handle),
                };
                lock.devices.insert(udid.clone(), device);
            }
        }
    }
}

#[cfg(not(feature = "zeroconf"))]
pub async fn discover(data: Arc<Mutex<CentralData>>) {
    let service_name = format!("_{}._{}.local", SERVICE_NAME, SERVICE_PROTOCOL);
    println!("Starting mDNS discovery for {} with mdns", service_name);

    let stream = mdns::discover::all(&service_name, Duration::from_secs(5))
        .unwrap()
        .listen();
    pin_mut!(stream);

    while let Some(Ok(response)) = stream.next().await {
        let addr = response.records().filter_map(self::to_ip_addr).next();

        if let Some(mut addr) = addr {
            let mut mac_addr = None;
            for i in response.records() {
                match i.kind {
                    RecordKind::A(addr4) => addr = std::net::IpAddr::V4(addr4),
                    _ => (),
                }
                if i.name.contains(&service_name) && i.name.contains("@") {
                    mac_addr = Some(i.name.split("@").collect::<Vec<&str>>()[0]);
                }
            }

            // Look through paired devices for mac address
            if mac_addr.is_none() {
                continue;
            }
            let mac_addr = mac_addr.unwrap();
            let mut lock = data.lock().await;
            if let Ok(udid) = lock.get_udid(mac_addr.to_string()) {
                if lock.devices.contains_key(&udid) {
                    info!("Device has already been added to muxer, skipping");
                    continue;
                }
                println!("Adding device {}", udid);
                let handle = heartbeat::heartbeat(udid.to_string(), addr, data.clone());
                let device = Device {
                    connection_type: "Network".to_string(),
                    device_id: 200,
                    service_name: service_name.to_string(),
                    interface_index: 300,
                    network_address: addr,
                    serial_number: udid.to_string(),
                    heartbeat_handle: Some(handle),
                };
                lock.devices.insert(udid.clone(), device);
            }
        }
    }
}

#[cfg(not(feature = "zeroconf"))]
fn to_ip_addr(record: &Record) -> Option<IpAddr> {
    match record.kind {
        RecordKind::A(addr) => Some(addr.into()),
        RecordKind::AAAA(addr) => Some(addr.into()),
        _ => None,
    }
}
