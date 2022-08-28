// jkcoxson

use crate::devices::SharedDevices;
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

const SERVICE_NAME: &str = "apple-mobdev2";
const SERVICE_PROTOCOL: &str = "tcp";

#[cfg(feature = "zeroconf")]
pub async fn discover(data: Arc<Mutex<SharedDevices>>) {
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
            if let Ok(udid) = lock.get_udid_from_mac(mac_addr.to_string()) {
                if lock.devices.contains_key(&udid) {
                    info!("Device has already been added to muxer, skipping");
                    continue;
                }
                println!("Adding device {}", udid);

                lock.add_network_device(
                    udid,
                    addr,
                    service_name.clone(),
                    "Network".to_string(),
                    data.clone(),
                )
            }
        }
    }
}

#[cfg(not(feature = "zeroconf"))]
pub async fn discover(data: Arc<Mutex<SharedDevices>>) {
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
                if let RecordKind::A(addr4) = i.kind {
                    addr = std::net::IpAddr::V4(addr4)
                }
                if i.name.contains(&service_name) && i.name.contains('@') {
                    mac_addr = Some(i.name.split('@').collect::<Vec<&str>>()[0]);
                }
            }

            // Look through paired devices for mac address
            if mac_addr.is_none() {
                continue;
            }
            let mac_addr = mac_addr.unwrap();
            let mut lock = data.lock().await;
            if let Ok(udid) = lock.get_udid_from_mac(mac_addr.to_string()) {
                if lock.devices.contains_key(&udid) {
                    info!("Device has already been added to muxer, skipping");
                    continue;
                }
                println!("Adding device {}", udid);

                lock.add_network_device(
                    udid,
                    addr,
                    service_name.clone(),
                    "Network".to_string(),
                    data.clone(),
                )
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
