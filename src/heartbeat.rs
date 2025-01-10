// jkcoxson

use idevice::{heartbeat::HeartbeatClient, lockdownd::LockdowndClient, Idevice};
use log::info;
use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::UnboundedSender;

use crate::devices::SharedDevices;

pub fn heartbeat(
    ip_addr: IpAddr,
    udid: String,
    pairing_file: idevice::pairing_file::PairingFile,
    data: Arc<tokio::sync::Mutex<SharedDevices>>,
) -> Result<UnboundedSender<()>, Box<dyn std::error::Error>> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let pls_stop = Arc::new(Mutex::new(false));
    let pls_stop_clone = pls_stop.clone();

    let socket = SocketAddr::new(ip_addr, idevice::lockdownd::LOCKDOWND_PORT);

    let socket = std::net::TcpStream::connect(socket)?;
    let socket = Box::new(socket);
    let idevice = Idevice::new(socket, "netmuxd");

    let mut lockdown_client = LockdowndClient { idevice };
    lockdown_client.start_session(&pairing_file)?;

    let (port, _) = lockdown_client
        .start_service("com.apple.mobile.heartbeat")
        .unwrap();

    let socket = SocketAddr::new(ip_addr, port);
    let socket = std::net::TcpStream::connect(socket)?;
    let socket = Box::new(socket);
    let mut idevice = Idevice::new(socket, "heartbeat_client");

    idevice.start_session(&pairing_file)?;

    let mut heartbeat_client = HeartbeatClient { idevice };

    tokio::task::spawn_blocking(move || loop {
        if let Err(e) = heartbeat_client.get_marco() {
            info!("Heartbeat recv failed: {:?}", e);
            tokio::spawn(async move {
                remove_from_data(data, udid).await;
            });
            break;
        }
        if *pls_stop.lock().unwrap() {
            break;
        }
        if let Err(e) = heartbeat_client.send_polo() {
            info!("Heartbeat send failed: {:?}", e);
            tokio::spawn(async move {
                remove_from_data(data, udid).await;
            });
            return;
        }
    });
    tokio::spawn(async move {
        rx.recv().await;
        *pls_stop_clone.lock().unwrap() = true;
    });
    Ok(tx)
}

pub async fn remove_from_data(data: Arc<tokio::sync::Mutex<SharedDevices>>, udid: String) {
    println!("Removing {}", udid);
    let mut data = data.lock().await;
    data.remove_device(&udid);
}
