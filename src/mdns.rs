// jkcoxson

use crate::{
    central_data::{CentralData, Device},
    heartbeat,
};
use log::info;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use zeroconf::prelude::*;
use zeroconf::{MdnsBrowser, ServiceType};

const SERVICE_NAME: &'static str = "apple-mobdev2";
const SERVICE_PROTOCOL: &'static str = "tcp";

pub async fn discover(data: Arc<Mutex<CentralData>>) {
    let service_name = format!("_{}._{}.local", SERVICE_NAME, SERVICE_PROTOCOL);
    println!("Starting mDNS discovery for {}", service_name);

    let mut browser = MdnsBrowser::new(ServiceType::new(SERVICE_NAME, SERVICE_PROTOCOL).unwrap());
    loop {
        let result = browser.browse_async().await;

        if let Ok(service) = result {
            println!("Service discovered: {:?}", service);
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
