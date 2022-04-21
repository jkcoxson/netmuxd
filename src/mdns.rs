// jkcoxson

use crate::{
    central_data::{CentralData, Device},
    heartbeat,
};
use log::info;
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

use futures_util::{pin_mut, stream::StreamExt};
use mdns::{Record, RecordKind};
use std::net::IpAddr;

const SERVICE_NAME: &'static str = "_apple-mobdev2._tcp.local";

pub async fn discover(data: Arc<Mutex<CentralData>>) {
    println!("Starting mDNS discovery");
    let stream = mdns::discover::all(SERVICE_NAME, Duration::from_secs(5))
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
                if i.name.contains(SERVICE_NAME) && i.name.contains("@") {
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
                    service_name: SERVICE_NAME.to_string(),
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

fn to_ip_addr(record: &Record) -> Option<IpAddr> {
    match record.kind {
        RecordKind::A(addr) => Some(addr.into()),
        RecordKind::AAAA(addr) => Some(addr.into()),
        _ => None,
    }
}
