// jkcoxson

use crate::devices::SharedDevices;
use log::info;
use std::net::IpAddr;
use std::any::Any;
use std::sync::{Arc, Mutex as StdMut};
use std::time::Duration;

use tokio::sync::Mutex;

#[cfg(not(feature = "zeroconf"))]
use {
    futures_util::{pin_mut, stream::StreamExt},
    mdns::{Record, RecordKind},
};

#[cfg(feature = "zeroconf")]
use {
    zeroconf::prelude::*,
    zeroconf::{MdnsBrowser, ServiceDiscovery, ServiceType},
};

const SERVICE_NAME: &str = "apple-mobdev2";
const SERVICE_PROTOCOL: &str = "tcp";

#[derive(Default, Debug, Clone)]
pub struct Context {
    name: String,
    address: String,
}

#[cfg(feature = "zeroconf")]
fn on_service_discovered(result: zeroconf::Result<ServiceDiscovery>, _context: Option<Arc<dyn Any>>,) {
        if let Ok(service) = result {
            info!("Service discovered: {:?}", service);
            let mut context = _context.as_ref().unwrap().downcast_ref::<Arc<StdMut<Context>>>().unwrap().lock().unwrap();
            context.name = String::from(service.name());
            context.address = String::from(service.address());
        }
}

#[cfg(feature = "zeroconf")]
pub async fn discover(data: Arc<Mutex<SharedDevices>>) {
    let service_name = format!("_{}._{}.local", SERVICE_NAME, SERVICE_PROTOCOL);
    println!("Starting mDNS discovery for {} with zeroconf", service_name);

    let mut browser = MdnsBrowser::new(ServiceType::new(SERVICE_NAME, SERVICE_PROTOCOL).unwrap());

    browser.set_service_discovered_callback(Box::new(on_service_discovered));
    browser.set_context(Box::new(Arc::new(StdMut::new(Context { name : String::from(""), address : String::from("")}))));

    let event_loop = browser.browse_services().unwrap();
    loop {
       event_loop.poll(Duration::from_secs(0)).unwrap();

       let context = browser.context().unwrap().downcast_ref::<Arc<StdMut<Context>>>().unwrap().lock().unwrap();
       let name = context.name.clone();
       let address = context.address.clone();
       info!("Name = {} ; Address = {}", name ,address); 

       if !name.contains("@") {
               continue;
       }
 
       let addr = match address.clone() {
               addr if addr.contains(":") => IpAddr::V6(addr.parse().unwrap()),
               addr => IpAddr::V4(addr.parse().unwrap()),
       };

       let mac_addr = name.split("@").collect::<Vec<&str>>()[0];
       let mut lock = data.lock().await;

       if let Ok(udid) = lock.get_udid_from_mac(mac_addr.to_string()) {
           if lock.devices.contains_key(&udid) {
               info!("Device has already been added to muxer, skipping");
               return;
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
