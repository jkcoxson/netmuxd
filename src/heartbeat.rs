// jkcoxson

use idevice::{heartbeat::HeartbeatClient, lockdownd::LockdowndClient, Idevice};
use log::{info, warn};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tokio::sync::oneshot::{error::TryRecvError, Sender};

use crate::devices::SharedDevices;

pub async fn heartbeat(
    ip_addr: IpAddr,
    udid: String,
    pairing_file: idevice::pairing_file::PairingFile,
    data: Arc<tokio::sync::Mutex<SharedDevices>>,
) -> Result<Sender<()>, Box<dyn std::error::Error>> {
    let (tx, mut rx) = tokio::sync::oneshot::channel();

    let socket = SocketAddr::new(ip_addr, LockdowndClient::LOCKDOWND_PORT);

    let socket = tokio::net::TcpStream::connect(socket).await?;
    let socket = Box::new(socket);
    let idevice = Idevice::new(socket, "netmuxd");

    let mut lockdown_client = LockdowndClient { idevice };
    lockdown_client.start_session(&pairing_file).await?;

    let (port, _) = lockdown_client
        .start_service("com.apple.mobile.heartbeat")
        .await?;

    let socket = SocketAddr::new(ip_addr, port);
    let socket = tokio::net::TcpStream::connect(socket).await?;
    let socket = Box::new(socket);
    let mut idevice = Idevice::new(socket, "heartbeat_client");

    idevice.start_session(&pairing_file).await?;

    tokio::spawn(async move {
        let mut interval = 10;
        let mut heartbeat_client = HeartbeatClient { idevice };
        loop {
            match heartbeat_client.get_marco(interval + 5).await {
                Ok(i) => {
                    interval = i;
                }
                Err(e) => {
                    info!("Heartbeat recv failed: {:?}", e);
                    tokio::spawn(async move {
                        remove_from_data(data, udid).await;
                    });
                    break;
                }
            }
            match rx.try_recv() {
                Ok(_) => {
                    info!("Heartbeat instructed to die")
                }
                Err(TryRecvError::Closed) => {
                    warn!("Heartbeat killer closed");
                    break;
                }
                _ => {}
            }
            if let Err(e) = heartbeat_client.send_polo().await {
                info!("Heartbeat send failed: {:?}", e);
                tokio::spawn(async move {
                    remove_from_data(data, udid).await;
                });
                return;
            }
        }
    });
    Ok(tx)
}

pub async fn remove_from_data(data: Arc<tokio::sync::Mutex<SharedDevices>>, udid: String) {
    println!("Removing {}", udid);
    let mut data = data.lock().await;
    data.remove_device(&udid);
}
