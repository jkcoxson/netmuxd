// Jackson Coxson

#[cfg(unix)]
use std::{fs, os::unix::prelude::PermissionsExt};
use std::{net::IpAddr, str::FromStr};

use crate::{
    config::NetmuxdConfig,
    manager::{ManagerRequest, ManagerSender, new_manager_thread},
    pairing_file::PairingFileFinder,
    raw_packet::RawPacket,
};
use log::{debug, error, info, trace, warn};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::oneshot::channel,
};

mod config;
mod devices;
mod heartbeat;
mod manager;
mod mdns;
mod pairing_file;
mod raw_packet;

#[tokio::main]
async fn main() {
    println!("Starting netmuxd");

    env_logger::init();
    info!("Logger initialized");

    let config = NetmuxdConfig::collect();
    info!("Collected arguments, proceeding");

    let manager_sender = new_manager_thread(&config);

    if let Some(host) = config.host.clone() {
        let manager_sender = manager_sender.clone();
        let pairing_file_finder = PairingFileFinder::new(&config);
        tokio::spawn(async move {
            // Create TcpListener
            let listener = tokio::net::TcpListener::bind(format!("{}:{}", host, config.port))
                .await
                .expect("Unable to bind to TCP listener");

            println!("Listening on {}:{}", host, config.port);
            #[cfg(unix)]
            println!(
                "WARNING: Running in host mode will not work unless you are running a daemon in unix mode as well"
            );
            loop {
                let (socket, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => {
                        warn!("Error accepting connection");
                        continue;
                    }
                };

                handle_stream(socket, manager_sender.clone(), pairing_file_finder.clone()).await;
            }
        });
    }

    #[cfg(unix)]
    if config.use_unix {
        let manager_sender = manager_sender.clone();
        let pairing_file_finder = PairingFileFinder::new(&config);
        tokio::spawn(async move {
            // Delete old Unix socket
            info!("Deleting old Unix socket");
            std::fs::remove_file("/var/run/usbmuxd").unwrap_or_default();
            // Create UnixListener
            info!("Binding to new Unix socket");
            let listener = tokio::net::UnixListener::bind("/var/run/usbmuxd")
                .expect("Unable to bind to unix socket");
            // Change the permission of the socket
            info!("Changing permissions of socket");
            fs::set_permissions("/var/run/usbmuxd", fs::Permissions::from_mode(0o666))
                .expect("Unable to set socket file permissions");

            println!("Listening on /var/run/usbmuxd");

            loop {
                let (socket, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => {
                        warn!("Error accepting connection");
                        continue;
                    }
                };

                handle_stream(socket, manager_sender.clone(), pairing_file_finder.clone()).await;
            }
        });
    }

    if config.use_mdns {
        let local = tokio::task::LocalSet::new();
        let manager_sender = manager_sender.clone();
        local.spawn_local(async move {
            mdns::discover(manager_sender.clone(), config).await;
            error!("mDNS discovery stopped, how the heck did you break this");
        });
        local.await;
        error!("mDNS discovery stopped");
        std::process::exit(1);
    } else {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    }
}

enum Directions {
    None,
    Listen,
}

async fn handle_stream(
    mut socket: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    manager_sender: ManagerSender,
    pairing_file_finder: PairingFileFinder,
) {
    tokio::spawn(async move {
        let mut current_directions = Directions::None;

        loop {
            // Wait for a message from the client
            let mut buf = [0; 1024];
            trace!("Waiting for data from client...");
            let size = match socket.read(&mut buf).await {
                Ok(s) => s,
                Err(_) => {
                    return;
                }
            };
            trace!("Recv'd {size} bytes");
            if size == 0 {
                debug!("Received size is zero, closing connection");
                return;
            }

            let buffer = &mut buf[0..size].to_vec();
            if size == 16 {
                info!("Only read the header, pulling more bytes");
                // Get the number of bytes to pull
                let packet_size = &buffer[0..4];
                let packet_size = match packet_size.try_into() {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("Failed to read packet size: {e:?}");
                        return;
                    }
                };
                let packet_size = u32::from_le_bytes(packet_size);
                info!("Packet size: {}", packet_size);
                // Pull the rest of the packet
                let mut packet = vec![0; packet_size as usize];
                let size = match socket.read(&mut packet).await {
                    Ok(s) => s,
                    Err(_) => {
                        return;
                    }
                };
                if size == 0 {
                    info!("Size was zero");
                    return;
                }
                // Append the packet to the buffer
                buffer.append(&mut packet);
            }

            let parsed: raw_packet::RawPacket = match buffer.try_into() {
                Ok(p) => p,
                Err(_) => {
                    warn!("Could not parse packet");
                    return;
                }
            };
            trace!("Recv'd plist: {parsed:#?}");

            match current_directions {
                Directions::None => {
                    // Handle the packet
                    let packet_type = match parsed.plist.get("MessageType") {
                        Some(plist::Value::String(p)) => p,
                        _ => {
                            warn!("Packet didn't contain MessageType");
                            return;
                        }
                    };

                    trace!("usbmuxd client sent {packet_type}");

                    match packet_type.as_str() {
                        //////////////////////////////
                        // netmuxd specific packets //
                        //////////////////////////////
                        "AddDevice" => {
                            let connection_type = match parsed.plist.get("ConnectionType") {
                                Some(plist::Value::String(c)) => c,
                                _ => {
                                    warn!("Packet didn't contain ConnectionType");
                                    return;
                                }
                            };
                            let service_name = match parsed.plist.get("ServiceName") {
                                Some(plist::Value::String(s)) => s,
                                _ => {
                                    warn!("Packet didn't contain ServiceName");
                                    return;
                                }
                            };

                            let ip_address = match parsed.plist.get("IPAddress") {
                                Some(plist::Value::String(ip)) => ip,
                                _ => {
                                    warn!("Packet didn't contain IPAddress");
                                    return;
                                }
                            };

                            let ip_address = match ip_address.parse() {
                                Ok(i) => i,
                                Err(_) => {
                                    warn!("Bad IP requested: {ip_address}");
                                    return;
                                }
                            };

                            let udid = match parsed.plist.get("DeviceID") {
                                Some(plist::Value::String(u)) => u,
                                _ => {
                                    warn!("Packet didn't contain DeviceID");
                                    return;
                                }
                            };

                            let (tx, rx) = channel();
                            if let Err(e) = manager_sender
                                .send(ManagerRequest {
                                    request_type:
                                        manager::ManagerRequestType::DiscoveredNetworkDevice {
                                            udid: udid.clone(),
                                            network_address: ip_address,
                                            service_name: service_name.to_string(),
                                            connection_type: connection_type.to_string(),
                                        },
                                    response: Some(tx),
                                })
                                .await
                            {
                                log::error!("Failed to send to manager: {e:?}, stopping!");
                                return;
                            }
                            let res = match rx.await {
                                Ok(r) => r,
                                Err(e) => {
                                    log::error!("Failed to recv manager response: {e:?}");
                                    return;
                                }
                            };

                            let res: Vec<u8> = RawPacket::new(res, 1, 8, parsed.tag).into();
                            if let Err(e) = socket.write_all(&res).await {
                                warn!("Failed to send back success message: {e:?}");
                            }

                            // No more further communication for this packet
                            return;
                        }
                        "RemoveDevice" => {
                            let udid = match parsed.plist.get("DeviceID") {
                                Some(plist::Value::String(u)) => u,
                                _ => {
                                    warn!("Packet didn't contain DeviceID");
                                    return;
                                }
                            };

                            manager_sender
                                .send(ManagerRequest {
                                    request_type: manager::ManagerRequestType::RemoveDevice {
                                        udid: udid.to_string(),
                                    },
                                    response: None,
                                })
                                .await
                                .ok();

                            return;
                        }

                        //////////////////////////////
                        // usbmuxd protocol packets //
                        //////////////////////////////
                        "ListDevices" => {
                            let (tx, rx) = channel();
                            if let Err(e) = manager_sender
                                .send(ManagerRequest {
                                    request_type: manager::ManagerRequestType::ListDevices,
                                    response: Some(tx),
                                })
                                .await
                            {
                                log::error!("Manager channel is closed: {e:?}");
                            }
                            let res = match rx.await {
                                Ok(r) => r,
                                Err(e) => {
                                    log::error!("Did not recv manager response: {e:?}");
                                    return;
                                }
                            };
                            println!("{}", idevice::pretty_print_dictionary(&res));

                            let res = RawPacket::new(res, 1, 8, parsed.tag);
                            let res: Vec<u8> = res.into();
                            if let Err(e) = socket.write_all(&res).await {
                                warn!("Failed to send response to client: {e:}");
                                return;
                            }

                            continue;
                        }
                        "Listen" => {
                            // The full functionality of this is not implemented. We will just maintain the connection.
                            current_directions = Directions::Listen;
                        }
                        "ReadPairRecord" => {
                            let pair_file = match pairing_file_finder
                                .get_pairing_record(match parsed.plist.get("PairRecordID") {
                                    Some(plist::Value::String(p)) => p,
                                    _ => {
                                        warn!("Request did not contain PairRecordID");
                                        return;
                                    }
                                })
                                .await
                            {
                                Ok(pair_file) => pair_file,
                                Err(_) => {
                                    // Unimplemented
                                    return;
                                }
                            };

                            let pair_file = match pair_file.serialize() {
                                Ok(p) => p,
                                Err(e) => {
                                    log::error!("Failed to serialize pair record: {e:?}");
                                    return;
                                }
                            };

                            let mut p = plist::Dictionary::new();
                            p.insert("PairRecordData".into(), plist::Value::Data(pair_file));

                            let res = RawPacket::new(p, 1, 8, parsed.tag);
                            let res: Vec<u8> = res.into();
                            if let Err(e) = socket.write_all(&res).await {
                                warn!("Failed to send response to client: {e:?}");
                                return;
                            }

                            continue;
                        }
                        "ReadBUID" => {
                            let buid = match pairing_file_finder.get_buid().await {
                                Ok(b) => b,
                                Err(e) => {
                                    log::error!("Failed to get buid: {e:?}");
                                    return;
                                }
                            };

                            let mut p = plist::Dictionary::new();
                            p.insert("BUID".into(), buid.into());

                            let res = RawPacket::new(p, 1, 8, parsed.tag);
                            let res: Vec<u8> = res.into();
                            if let Err(e) = socket.write_all(&res).await {
                                warn!("Failed to send response to client: {e:?}");
                                return;
                            }

                            continue;
                        }
                        "Connect" => {
                            let connection_port = match parsed.plist.get("PortNumber") {
                                Some(plist::Value::Integer(p)) => match p.as_unsigned() {
                                    Some(p) => p,
                                    None => {
                                        warn!("PortNumber is not unsigned!");
                                        return;
                                    }
                                },
                                _ => {
                                    warn!("Packet didn't contain PortNumber");
                                    return;
                                }
                            };

                            let device_id = match parsed.plist.get("DeviceID") {
                                Some(plist::Value::Integer(d)) => match d.as_unsigned() {
                                    Some(d) => d,
                                    None => {
                                        warn!("DeviceID is not unsigned!");
                                        return;
                                    }
                                },
                                _ => {
                                    warn!("Packet didn't contain DeviceID");
                                    return;
                                }
                            };

                            let connection_port = connection_port as u16;
                            let connection_port = connection_port.to_be();

                            info!("Client is establishing connection to port {connection_port}");
                            let (tx, rx) = channel();
                            if let Err(e) = manager_sender
                                .send(ManagerRequest {
                                    request_type:
                                        manager::ManagerRequestType::GetDeviceNetworkAddress {
                                            id: device_id,
                                        },
                                    response: Some(tx),
                                })
                                .await
                            {
                                log::error!("Manager thread is stopped: {e:?}");
                                return;
                            }

                            let res = match rx.await {
                                Ok(r) => r,
                                Err(e) => {
                                    log::error!("Manager thread did not respond: {e:?}");
                                    return;
                                }
                            };

                            if let Some(address) = res.get("address").and_then(|x| x.as_string())
                                && let Some(udid) = res.get("udid").and_then(|x| x.as_string())
                            {
                                info!("Connecting to device {}", device_id);
                                let network_address = IpAddr::from_str(address);

                                match network_address {
                                    Ok(ip) => {
                                        match tokio::net::TcpStream::connect((ip, connection_port))
                                            .await
                                        {
                                            Ok(mut stream) => {
                                                let mut p = plist::Dictionary::new();
                                                p.insert("MessageType".into(), "Result".into());
                                                p.insert("Number".into(), 0.into());

                                                let res = RawPacket::new(p, 1, 8, parsed.tag);
                                                let res: Vec<u8> = res.into();
                                                if let Err(e) = socket.write_all(&res).await {
                                                    warn!(
                                                        "Failed to send response to client: {e:?}"
                                                    );
                                                    return;
                                                }

                                                let (kill, killed) = channel();
                                                manager_sender
                                                    .send(ManagerRequest {
                                                        request_type: manager::ManagerRequestType::OpenSocket { udid: udid.to_string() , kill },
                                                        response: None,
                                                    })
                                                    .await
                                                    .expect("Manager is dead");

                                                tokio::select! {
                                                    _ = killed => {
                                                        info!("Bidirectional stream stopped via heartbeat failure");
                                                    }
                                                    e = tokio::io::copy_bidirectional(&mut stream, &mut socket) => {
                                                        info!("Bidirectional stream stopped: {e:?}");

                                                    }
                                                }
                                                return;
                                            }
                                            Err(e) => {
                                                error!(
                                                    "Unable to connect to device {device_id} port {connection_port}: {e:?}"
                                                );
                                                let mut p = plist::Dictionary::new();
                                                p.insert("MessageType".into(), "Result".into());
                                                p.insert("Number".into(), 1.into());

                                                let res = RawPacket::new(p, 1, 8, parsed.tag);
                                                let res: Vec<u8> = res.into();
                                                if let Err(e) = socket.write_all(&res).await {
                                                    warn!(
                                                        "Failed to send response to client: {e:?}"
                                                    );
                                                }

                                                continue;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Invalid network address: {e:?}");
                                        return;
                                    }
                                }
                            } else {
                                let mut p = plist::Dictionary::new();
                                p.insert("MessageType".into(), "Result".into());
                                p.insert("Number".into(), 1.into());

                                let res = RawPacket::new(p, 1, 8, parsed.tag);
                                let res: Vec<u8> = res.into();
                                if let Err(e) = socket.write_all(&res).await {
                                    warn!("Failed to send response to client: {e:?}");
                                }

                                continue;
                            }
                        }
                        _ => {
                            warn!("Unknown packet type");
                        }
                    }
                }
                Directions::Listen => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });
}
