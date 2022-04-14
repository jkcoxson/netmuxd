// jkcoxson

use std::{
    net::IpAddr,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::UnboundedSender;

use crate::central_data::CentralData;

pub fn heartbeat(
    udid: String,
    ip_addr: IpAddr,
    data: Arc<tokio::sync::Mutex<CentralData>>,
) -> UnboundedSender<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let pls_stop = Arc::new(Mutex::new(false));
    let pls_stop_clone = pls_stop.clone();
    tokio::task::spawn_blocking(move || {
        println!("Starting heartbeat for {}", udid);
        let device =
            rusty_libimobiledevice::idevice::Device::new(udid.clone(), true, Some(ip_addr), 0)
                .unwrap();
        let hb_client = rusty_libimobiledevice::services::heartbeat::HeartbeatClient::new(
            &device,
            "yurmom".to_string(),
        )
        .unwrap();
        loop {
            match hb_client.receive(15000) {
                Ok(plist) => match hb_client.send(plist) {
                    Ok(_) => {}
                    Err(_) => {
                        tokio::spawn(async move {
                            remove_from_data(data, udid).await;
                        });
                        return;
                    }
                },
                Err(_) => {
                    println!("Failed to receive heartbeat, removing device");
                    tokio::spawn(async move {
                        remove_from_data(data, udid).await;
                    });
                    break;
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

pub async fn remove_from_data(data: Arc<tokio::sync::Mutex<CentralData>>, udid: String) {
    let mut data = data.lock().await;
    data.remove_device(udid);
}
