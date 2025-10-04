// Jackson Coxson

use idevice::{heartbeat::HeartbeatClient, lockdown::LockdownClient, Idevice};
use log::{debug, info, warn};
use std::net::SocketAddr;
use tokio::sync::oneshot::Sender;

use crate::{
    devices::MuxerDevice,
    manager::{ManagerRequest, ManagerSender},
};

pub async fn heartbeat(
    device: MuxerDevice,
    response: Option<Sender<plist::Dictionary>>,
    pairing_file: idevice::pairing_file::PairingFile,
    sender: ManagerSender,
) {
    debug!("Spawning heartbeat for {device:?}");
    tokio::spawn(async move {
        let udid = device.serial_number.clone();
        let socket = SocketAddr::new(
            device.network_address.unwrap(),
            LockdownClient::LOCKDOWND_PORT,
        );

        let socket = match tokio::net::TcpStream::connect(socket).await {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to connect to lockdown port: {e:?}");
                if let Some(response) = response {
                    response
                        .send(idevice::plist!(dict {
                        "Result": 0,
                        }))
                        .ok();
                }
                return;
            }
        };

        let socket = Box::new(socket);
        let idevice = Idevice::new(socket, "netmuxd");

        let mut lockdown_client = LockdownClient { idevice };
        if let Err(e) = lockdown_client.start_session(&pairing_file).await {
            warn!("Failed to start lockdown session: {e:?}");
            if let Some(response) = response {
                response
                    .send(idevice::plist!(dict {
                    "Result": 0,
                    }))
                    .ok();
            }
            return;
        }

        let (port, _) = match lockdown_client
            .start_service("com.apple.mobile.heartbeat")
            .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!("Failed to start heartbeat service: {e:?}");
                if let Some(response) = response {
                    response
                        .send(idevice::plist!(dict {
                        "Result": 0,
                        }))
                        .ok();
                }
                return;
            }
        };

        let socket = SocketAddr::new(device.network_address.unwrap(), port);
        let socket = match tokio::net::TcpStream::connect(socket).await {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to connect to heartbeat port: {e:?}");
                if let Some(response) = response {
                    response
                        .send(idevice::plist!(dict {
                        "Result": 0,
                        }))
                        .ok();
                }
                return;
            }
        };

        let socket = Box::new(socket);
        let mut idevice = Idevice::new(socket, "heartbeat_client");

        if let Err(e) = idevice.start_session(&pairing_file).await {
            warn!("Failed to wrap heartbeat client in TLS: {e:?}");
            if let Some(response) = response {
                response
                    .send(idevice::plist!(dict {
                    "Result": 0,
                    }))
                    .ok();
            }
            return;
        }

        let mut interval = 10;
        let mut heartbeat_client = HeartbeatClient { idevice };

        // now that we successfully created the heartbeat client, we can send the deferred add
        sender
            .send(ManagerRequest {
                request_type: crate::manager::ManagerRequestType::DeferredMuxerAdd {
                    device,
                    response,
                },
                response: None,
            })
            .await
            .ok();

        loop {
            match heartbeat_client.get_marco(interval + 5).await {
                Ok(i) => {
                    interval = i;
                }
                Err(e) => {
                    info!("Heartbeat recv failed: {:?}", e);
                    sender
                        .send(ManagerRequest::heartbeat_failed(udid.clone()))
                        .await
                        .ok();
                    break;
                }
            }
            if let Err(e) = heartbeat_client.send_polo().await {
                info!("Heartbeat send failed: {:?}", e);
                sender
                    .send(ManagerRequest::heartbeat_failed(udid.clone()))
                    .await
                    .ok();
                break;
            }
            if sender.is_disconnected() {
                break;
            }
        }
    });
}
