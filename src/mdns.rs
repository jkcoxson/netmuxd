// jkcoxson

use crate::{
    central_data::{CentralData, Device},
    heartbeat,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

use futures_util::{pin_mut, stream::StreamExt};
use mdns::{Record, RecordKind};
use std::net::IpAddr;

const SERVICE_NAME: &'static str = "_apple-mobdev2._tcp.local";

pub async fn discover(data: Arc<Mutex<CentralData>>) {
    println!("Starting mDNS discovery");
    let stream = mdns::discover::all(SERVICE_NAME, Duration::from_secs(2))
        .unwrap()
        .listen();
    pin_mut!(stream);

    while let Some(Ok(response)) = stream.next().await {
        let addr = response.records().filter_map(self::to_ip_addr).next();

        if let Some(addr) = addr {
            println!("Found iDevice at {}", addr);
            let mut mac_addr = None;
            for i in response.records() {
                if i.name.contains(SERVICE_NAME) && i.name.contains("@") {
                    mac_addr = Some(i.name.split("@").collect::<Vec<&str>>()[0]);
                    println!("Found mac address: {}", mac_addr.unwrap());
                }
            }

            // Look through paired devices for mac address
            if mac_addr.is_none() {
                println!("No mac address found, skipping");
                continue;
            }
            let mac_addr = mac_addr.unwrap();
            let mut lock = data.lock().await;
            if let Ok(udid) = lock.get_udid(mac_addr.to_string()) {
                println!("Found udid: {}", udid);
                let handle = heartbeat::heartbeat(udid.to_string(), addr, data.clone());
                let device = Device {
                    connection_type: "Network".to_string(),
                    device_id: 0,
                    service_name: SERVICE_NAME.to_string(),
                    interface_index: 0,
                    network_address: addr,
                    serial_number: udid.to_string(),
                    heartbeat_handle: Some(handle),
                };
                lock.devices.insert(udid.clone(), device);
            } else {
                println!("No udid found, skipping");
            }
        }
    }
}

fn to_ip_addr(record: &Record) -> Option<IpAddr> {
    match record.kind {
        RecordKind::A(addr) => Some(addr.into()),
        RecordKind::AAAA(addr) => Some(addr.into()),
        _ => None,
    }
}
