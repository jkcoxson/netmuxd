// jkcoxson

use crate::central_data::CentralData;
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
            let mut data = data.lock().await;
            if let Ok(udid) = data.get_udid(mac_addr.to_string()) {
                println!("Found udid: {}", udid);
                data.add_device(
                    udid,
                    addr.to_string(),
                    SERVICE_NAME.to_string(),
                    "Network".to_string(),
                );
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
