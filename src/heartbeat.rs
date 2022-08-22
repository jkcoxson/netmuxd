// jkcoxson

use log::{error, info};
use rusty_libimobiledevice::{idevice, services::heartbeat::HeartbeatClient};
use std::{
    net::IpAddr,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::UnboundedSender;

use crate::devices::SharedDevices;

pub fn heartbeat(
    udid: String,
    ip_addr: IpAddr,
    data: Arc<tokio::sync::Mutex<SharedDevices>>,
) -> UnboundedSender<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let pls_stop = Arc::new(Mutex::new(false));
    let pls_stop_clone = pls_stop.clone();
    tokio::task::spawn_blocking(move || {
        let device = idevice::Device::new(udid.clone(), Some(ip_addr), 0);
        let hb_client = match HeartbeatClient::new(&device, "netmuxd".to_string()) {
            Ok(hb_client) => hb_client,
            Err(e) => {
                error!(
                    "Failed to create heartbeat client for udid {}: {:?}",
                    udid, e
                );
                tokio::spawn(async move {
                    remove_from_data(data, udid).await;
                });
                return;
            }
        };

        let mut heartbeat_tries = 0;
        loop {
            match hb_client.receive(10000) {
                Ok(plist) => match hb_client.send(plist) {
                    Ok(_) => {
                        heartbeat_tries = 0;
                    }
                    Err(_) => {
                        tokio::spawn(async move {
                            remove_from_data(data, udid).await;
                        });
                        return;
                    }
                },
                Err(e) => {
                    heartbeat_tries += 1;
                    if heartbeat_tries > 5 {
                        info!("Heartbeat failed for {}: {:?}", udid, e);
                        tokio::spawn(async move {
                            remove_from_data(data, udid).await;
                        });
                        break;
                    }
                }
            }
            if *pls_stop.lock().unwrap() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        rx.recv().await;
        *pls_stop_clone.lock().unwrap() = true;
    });
    tx
}

pub async fn remove_from_data(data: Arc<tokio::sync::Mutex<SharedDevices>>, udid: String) {
    println!("Removing {}", udid);
    let mut data = data.lock().await;
    data.remove_device(udid);
}
