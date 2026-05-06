// Jackson Coxson

#[cfg(unix)]
use std::{fs, os::unix::prelude::PermissionsExt};

use netmuxd::{
    config::NetmuxdConfig,
    daemon,
    manager::{self, ListenerEvent, ManagerRequest, ManagerSender, new_manager_thread},
    mdns,
    pairing_file::PairingFileFinder,
    raw_packet::{self, RawPacket},
};

#[cfg(target_os = "windows")]
use netmuxd::{libusbk, libwdi};

trait AsyncReadWrite: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + ?Sized> AsyncReadWrite for T {}
use log::{error, info, trace, warn};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::oneshot::channel,
};

#[tokio::main]
async fn main() {
    // `install` / `uninstall` / `export-driver` are one-shot driver-
    // management subcommands that don't run the daemon.
    #[cfg(target_os = "windows")]
    match std::env::args().nth(1).as_deref() {
        Some("install") => std::process::exit(libwdi::run_install()),
        Some("uninstall") => std::process::exit(libwdi::run_uninstall()),
        Some("export-driver") => {
            let out = std::env::args().nth(2).unwrap_or_else(|| {
                eprintln!("Usage: netmuxd export-driver <output-dir>");
                std::process::exit(2);
            });
            std::process::exit(libwdi::run_export(&out));
        }
        _ => {}
    }

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

    if config.use_usb {
        let manager_sender = manager_sender.clone();
        let config = config.clone();
        tokio::spawn(async move {
            daemon::discover(manager_sender, config).await;
            error!("USB discovery stopped");
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

async fn handle_stream(
    mut socket: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    manager_sender: ManagerSender,
    pairing_file_finder: PairingFileFinder,
) {
    tokio::spawn(async move {
        // 16 MiB cap on a single packet to avoid unbounded allocations from a
        // misbehaving client. usbmuxd packets are normally a few KiB.
        const MAX_PACKET_SIZE: usize = 16 * 1024 * 1024;

        loop {
            trace!("Waiting for data from client...");
            // Read the 16-byte header (size, version, message, tag).
            let mut header = [0u8; 16];
            if let Err(e) = socket.read_exact(&mut header).await {
                trace!("Header read ended: {e:?}");
                return;
            }

            let packet_size =
                u32::from_le_bytes(header[0..4].try_into().expect("16-byte header")) as usize;
            if packet_size < 16 || packet_size > MAX_PACKET_SIZE {
                warn!("Bogus packet size from client: {packet_size}");
                return;
            }

            // Pull the rest of the packet body.
            let mut buffer = vec![0u8; packet_size];
            buffer[..16].copy_from_slice(&header);
            if packet_size > 16 {
                if let Err(e) = socket.read_exact(&mut buffer[16..]).await {
                    warn!(
                        "Failed reading packet body ({} bytes): {e:?}",
                        packet_size - 16
                    );
                    return;
                }
            }
            let buffer = &mut buffer;

            let parsed: raw_packet::RawPacket = match buffer.try_into() {
                Ok(p) => p,
                Err(_) => {
                    warn!("Could not parse packet");
                    return;
                }
            };
            trace!("Recv'd plist: {parsed:#?}");

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
                            request_type: manager::ManagerRequestType::DiscoveredNetworkDevice {
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
                                connection_type: None,
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
                    println!("{}", plist_macro::pretty_print_dictionary(&res));

                    let res = RawPacket::new(res, 1, 8, parsed.tag);
                    let res: Vec<u8> = res.into();
                    if let Err(e) = socket.write_all(&res).await {
                        warn!("Failed to send response to client: {e:}");
                        return;
                    }

                    continue;
                }
                "Listen" => {
                    run_listen(socket, manager_sender, parsed.tag).await;
                    return;
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
                "SavePairRecord" => {
                    let pair_record_data = match parsed.plist.get("PairRecordData") {
                        Some(plist::Value::Data(d)) => d.clone(),
                        _ => {
                            warn!("SavePairRecord did not contain PairRecordData");
                            return;
                        }
                    };

                    let udid = match parsed.plist.get("PairRecordID") {
                        Some(plist::Value::String(u)) => Some(u.clone()),
                        _ => None,
                    };
                    let udid = match udid {
                        Some(u) => u,
                        None => {
                            let device_id = match parsed.plist.get("DeviceID") {
                                Some(plist::Value::Integer(d)) => match d.as_unsigned() {
                                    Some(d) => d,
                                    None => {
                                        warn!("DeviceID is not unsigned");
                                        return;
                                    }
                                },
                                _ => {
                                    warn!("SavePairRecord missing both PairRecordID and DeviceID");
                                    return;
                                }
                            };
                            let (tx, rx) = tokio::sync::oneshot::channel();
                            if let Err(e) = manager_sender
                                .send(ManagerRequest {
                                    request_type:
                                        manager::ManagerRequestType::GetDeviceConnection {
                                            id: device_id,
                                            response: tx,
                                        },
                                    response: None,
                                })
                                .await
                            {
                                error!("Manager thread is stopped: {e:?}");
                                return;
                            }
                            match rx.await {
                                Ok(Some(c)) => c.serial_number,
                                Ok(None) => {
                                    warn!("No device with id {device_id}");
                                    return;
                                }
                                Err(e) => {
                                    error!("Manager did not respond: {e:?}");
                                    return;
                                }
                            }
                        }
                    };

                    let path = std::path::PathBuf::from(pairing_file_finder.plist_storage())
                        .join(format!("{udid}.plist"));
                    if let Some(parent) = path.parent() {
                        let _ = tokio::fs::create_dir_all(parent).await;
                    }

                    let result_number: u32 = match tokio::fs::write(&path, &pair_record_data).await
                    {
                        Ok(()) => {
                            info!("Saved pair record for {udid} to {path:?}");
                            0
                        }
                        Err(e) => {
                            warn!("Failed to write pair record {path:?}: {e:?}");
                            1
                        }
                    };

                    let mut p = plist::Dictionary::new();
                    p.insert("MessageType".into(), "Result".into());
                    p.insert("Number".into(), result_number.into());
                    let res: Vec<u8> = RawPacket::new(p, 1, 8, parsed.tag).into();
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
                        Some(plist::Value::Integer(p)) => {
                            if let Some(u) = p.as_unsigned() {
                                u as u16
                            } else if let Some(s) = p.as_signed() {
                                s as u16
                            } else {
                                warn!("PortNumber is not an integer");
                                return;
                            }
                        }
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

                    let connection_port = connection_port.to_be();

                    info!("Client is establishing connection to port {connection_port}");
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    if let Err(e) = manager_sender
                        .send(ManagerRequest {
                            request_type: manager::ManagerRequestType::GetDeviceConnection {
                                id: device_id,
                                response: tx,
                            },
                            response: None,
                        })
                        .await
                    {
                        log::error!("Manager thread is stopped: {e:?}");
                        return;
                    }

                    let lookup = match rx.await {
                        Ok(Some(l)) => l,
                        Ok(None) => {
                            warn!("No device with id {device_id}");
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
                        Err(e) => {
                            log::error!("Manager thread did not respond: {e:?}");
                            return;
                        }
                    };

                    type Upstream = Box<dyn AsyncReadWrite>;
                    let upstream: Result<Upstream, String> = match lookup.connection_type.as_str() {
                        "Network" => match lookup.network_address {
                            Some(addr) => {
                                match tokio::net::TcpStream::connect((addr, connection_port)).await
                                {
                                    Ok(s) => Ok(Box::new(s) as Upstream),
                                    Err(e) => Err(format!("tcp connect: {e:?}")),
                                }
                            }
                            None => Err("network device missing address".to_string()),
                        },
                        "USB" => match lookup.usb {
                            Some(handle) => match handle.connect(connection_port).await {
                                Ok(s) => Ok(Box::new(s) as Upstream),
                                Err(e) => Err(format!("usb connect: {e:?}")),
                            },
                            None => Err("usb device missing handle".into()),
                        },
                        other => Err(format!("unknown connection type {other}")),
                    };

                    let mut upstream = match upstream {
                        Ok(s) => s,
                        Err(e) => {
                            error!(
                                "Unable to connect to device {device_id} port {connection_port}: {e}"
                            );
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
                    };

                    let mut p = plist::Dictionary::new();
                    p.insert("MessageType".into(), "Result".into());
                    p.insert("Number".into(), 0.into());
                    let res = RawPacket::new(p, 1, 8, parsed.tag);
                    let res: Vec<u8> = res.into();
                    if let Err(e) = socket.write_all(&res).await {
                        warn!("Failed to send response to client: {e:?}");
                        return;
                    }

                    let (kill, killed) = channel();
                    manager_sender
                        .send(ManagerRequest {
                            request_type: manager::ManagerRequestType::OpenSocket {
                                device_id,
                                kill,
                            },
                            response: None,
                        })
                        .await
                        .expect("Manager is dead");

                    tokio::select! {
                        _ = killed => {
                            info!("Bidirectional stream stopped via heartbeat failure");
                        }
                        e = tokio::io::copy_bidirectional(&mut *upstream, &mut socket) => {
                            info!("Bidirectional stream stopped: {e:?}");
                        }
                    }
                    return;
                }
                _ => {
                    warn!("Unknown packet type");
                }
            }
        }
    });
}

async fn run_listen<S>(mut socket: S, manager_sender: ManagerSender, listen_tag: u32)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut p = plist::Dictionary::new();
    p.insert("MessageType".into(), "Result".into());
    p.insert("Number".into(), 0.into());
    let res: Vec<u8> = RawPacket::new(p, 1, 8, listen_tag).into();
    if let Err(e) = socket.write_all(&res).await {
        warn!("Failed to send Listen Result: {e:?}");
        return;
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ListenerEvent>();
    if let Err(e) = manager_sender
        .send(ManagerRequest {
            request_type: manager::ManagerRequestType::Subscribe { listener: tx },
            response: None,
        })
        .await
    {
        error!("Failed to subscribe listener: {e:?}");
        return;
    }

    let (mut reader, mut writer) = tokio::io::split(socket);
    let mut sink = [0u8; 256];
    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { return; };
                let plist = match event {
                    ListenerEvent::Attached(p) => p,
                    ListenerEvent::Detached(id) => {
                        let mut p = plist::Dictionary::new();
                        p.insert("MessageType".into(), "Detached".into());
                        p.insert("DeviceID".into(), id.into());
                        p
                    }
                };
                let bytes: Vec<u8> = RawPacket::new(plist, 1, 8, 0).into();
                if let Err(e) = writer.write_all(&bytes).await {
                    info!("Listener write failed, dropping: {e:?}");
                    return;
                }
            }
            r = reader.read(&mut sink) => {
                match r {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        }
    }
}
