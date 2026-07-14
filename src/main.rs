// Jackson Coxson

#[cfg(unix)]
use std::{fs, os::unix::prelude::PermissionsExt};

use idevice::{
    IdeviceError,
    usbmuxd::{
        RawPacket, UsbmuxdAddr,
        errors::UsbmuxdError,
        server::{UsbmuxdServerRequest, UsbmuxdServerResponse},
    },
};
use netmuxd::{
    config::NetmuxdConfig,
    daemon,
    manager::{
        self, ListenerEvent, ManagerRequest, ManagerSender, SHIM_NETWORK_ID_BASE,
        new_manager_thread,
    },
    mdns,
    pairing_file::PairingFileFinder,
    upstream,
};

#[cfg(all(target_os = "windows", feature = "libusbk"))]
use netmuxd::libwdi;

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
    #[cfg(all(target_os = "windows", feature = "libusbk"))]
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

    #[cfg(target_os = "windows")]
    let killed_amds_paths: Vec<String> = if config.kill_amds {
        match tokio::task::spawn_blocking(netmuxd::apple_mux::amds::kill_amds).await {
            Ok(paths) => {
                if paths.is_empty() {
                    info!("--kill-amds: no AppleMobileDeviceService process found");
                } else {
                    info!(
                        "--kill-amds: terminated {} AppleMobileDeviceService process(es)",
                        paths.len()
                    );
                }
                paths
            }
            Err(e) => {
                warn!("--kill-amds task panicked: {e:?}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    #[cfg(target_os = "windows")]
    if config.restart_amds_on_exit {
        let paths = killed_amds_paths.clone();
        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            println!("Shutting down; restarting AMDS");
            netmuxd::apple_mux::amds::restart_amds(&paths);
            std::process::exit(0);
        });
    }

    let manager_sender = new_manager_thread(&config);

    if let Some(host) = config.host.clone() {
        let manager_sender = manager_sender.clone();
        let pairing_file_finder = PairingFileFinder::new(&config);
        let upstream = config.upstream.clone();
        tokio::spawn(async move {
            // Create TcpListener
            let listener = tokio::net::TcpListener::bind(format!("{}:{}", host, config.port))
                .await
                .expect("Unable to bind to TCP listener");

            println!("Listening on {}:{}", host, config.port);
            #[cfg(unix)]
            if upstream.is_none() {
                println!(
                    "WARNING: Running in host mode will not work unless you are running a daemon in unix mode as well"
                );
            }
            loop {
                let (socket, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => {
                        warn!("Error accepting connection");
                        continue;
                    }
                };

                handle_stream(
                    socket,
                    manager_sender.clone(),
                    pairing_file_finder.clone(),
                    upstream.clone(),
                )
                .await;
            }
        });
    }

    #[cfg(unix)]
    if config.use_unix {
        let manager_sender = manager_sender.clone();
        let pairing_file_finder = PairingFileFinder::new(&config);
        let upstream = config.upstream.clone();
        let socket_path = config.socket_path.clone();

        // Refuse to bind (and thereby delete) the upstream's own socket.
        if let Some(UsbmuxdAddr::UnixSocket(up)) = &upstream
            && *up == socket_path
        {
            panic!(
                "--socket-path ({socket_path}) is the same as the upstream usbmuxd socket; the shim must listen on a different path"
            );
        }

        tokio::spawn(async move {
            // Delete old Unix socket
            info!("Deleting old Unix socket");
            std::fs::remove_file(&socket_path).unwrap_or_default();
            // Create UnixListener
            info!("Binding to new Unix socket");
            let listener = tokio::net::UnixListener::bind(&socket_path)
                .expect("Unable to bind to unix socket");
            // Change the permission of the socket
            info!("Changing permissions of socket");
            fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o666))
                .expect("Unable to set socket file permissions");

            println!("Listening on {socket_path}");

            loop {
                let (socket, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => {
                        warn!("Error accepting connection");
                        continue;
                    }
                };

                handle_stream(
                    socket,
                    manager_sender.clone(),
                    pairing_file_finder.clone(),
                    upstream.clone(),
                )
                .await;
            }
        });
    }

    if config.use_usb {
        if daemon::usb_available(config.apple_mux) {
            let manager_sender = manager_sender.clone();
            let config = config.clone();
            tokio::spawn(async move {
                daemon::discover(manager_sender, config).await;
                error!("USB discovery stopped");
            });
        } else {
            warn!(
                "USB is enabled but the libusbK backend is unavailable (--libusbk was passed but \
                 libusbK.dll was not found, or this build has no libusbK support); continuing \
                 without USB. Network/mDNS devices are unaffected. Drop --libusbk to use the default \
                 Apple-driver backend, or place libusbK.dll next to netmuxd.exe."
            );
        }
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

#[cfg(target_os = "windows")]
async fn wait_for_shutdown_signal() {
    use tokio::signal::windows;
    let mut ctrl_c = match windows::ctrl_c() {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to install Ctrl+C handler: {e:?}");
            std::future::pending::<()>().await;
            return;
        }
    };
    let mut ctrl_close = match windows::ctrl_close() {
        Ok(s) => s,
        Err(e) => {
            // Fall back to Ctrl+C only.
            warn!("Failed to install console-close handler: {e:?}");
            ctrl_c.recv().await;
            return;
        }
    };
    tokio::select! {
        _ = ctrl_c.recv() => {}
        _ = ctrl_close.recv() => {}
    }
}

async fn handle_stream(
    mut socket: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    manager_sender: ManagerSender,
    pairing_file_finder: PairingFileFinder,
    upstream: Option<UsbmuxdAddr>,
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
            if !(16..=MAX_PACKET_SIZE).contains(&packet_size) {
                warn!("Bogus packet size from client: {packet_size}");
                return;
            }

            // Pull the rest of the packet body.
            let mut buffer = vec![0u8; packet_size];
            buffer[..16].copy_from_slice(&header);
            if packet_size > 16
                && let Err(e) = socket.read_exact(&mut buffer[16..]).await
            {
                warn!(
                    "Failed reading packet body ({} bytes): {e:?}",
                    packet_size - 16
                );
                return;
            }
            let parsed: RawPacket = match RawPacket::try_from(&mut buffer) {
                Ok(p) => p,
                Err(_) => {
                    warn!("Could not parse packet");
                    return;
                }
            };
            trace!("Recv'd plist: {parsed:#?}");

            // Decode the standard usbmuxd requests via idevice. netmuxd's own
            // AddDevice/RemoveDevice extensions aren't part of the standard
            // protocol, so they surface as `UnknownMessageType` and are
            // dispatched separately below.
            let request = match UsbmuxdServerRequest::decode(&parsed) {
                Ok(r) => r,
                Err(IdeviceError::Usbmuxd(UsbmuxdError::UnknownMessageType(message_type))) => {
                    match message_type.as_str() {
                        //////////////////////////////
                        // netmuxd specific packets //
                        //////////////////////////////
                        "AddDevice" => {
                            handle_add_device(&mut socket, &manager_sender, &parsed).await;
                            return;
                        }
                        "RemoveDevice" => {
                            handle_remove_device(&manager_sender, &parsed).await;
                            return;
                        }
                        other => {
                            // Forward anything we don't model to the upstream
                            // muxer when in shim mode; otherwise it's unknown.
                            // There's no local equivalent, so a dead upstream
                            // just means we can't answer it.
                            if let Some(addr) = upstream.as_ref() {
                                match upstream::forward_to_upstream(addr, &buffer).await {
                                    Ok(frame) => {
                                        if let Err(e) = socket.write_all(&frame).await {
                                            warn!("Failed to send response to client: {e:?}");
                                            return;
                                        }
                                    }
                                    Err(e) => warn!("Failed forwarding {other} to upstream: {e}"),
                                }
                                continue;
                            }
                            warn!("Unknown packet type: {other}");
                            continue;
                        }
                    }
                }
                Err(e) => {
                    warn!("Malformed usbmuxd request: {e}");
                    return;
                }
            };

            trace!("usbmuxd client sent {request:?}");

            match request {
                //////////////////////////////
                // usbmuxd protocol packets //
                //////////////////////////////
                UsbmuxdServerRequest::ListDevices => {
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

                    let out: Vec<u8> = if let Some(addr) = upstream.as_ref() {
                        // Shim mode: ask upstream for its (USB) device list and
                        // append our network devices to it.
                        let network = match res.get("DeviceList") {
                            Some(plist::Value::Array(a)) => a.clone(),
                            _ => Vec::new(),
                        };
                        match upstream::list_devices_merged(addr, &buffer, network, parsed.tag)
                            .await
                        {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                // Upstream is unreachable: serve just our
                                // network devices instead of dropping the
                                // client.
                                warn!(
                                    "Upstream ListDevices failed ({e}); serving network devices only"
                                );
                                RawPacket::new(res, 1, 8, parsed.tag).into()
                            }
                        }
                    } else {
                        println!("{}", plist_macro::pretty_print_dictionary(&res));
                        RawPacket::new(res, 1, 8, parsed.tag).into()
                    };
                    if let Err(e) = socket.write_all(&out).await {
                        warn!("Failed to send response to client: {e:}");
                        return;
                    }

                    continue;
                }
                UsbmuxdServerRequest::Listen => {
                    run_listen(socket, manager_sender, parsed.tag, upstream).await;
                    return;
                }
                UsbmuxdServerRequest::ReadPairRecord { pair_record_id } => {
                    // Forward to upstream when possible; fall back to our own
                    // lockdown storage if it's unreachable.
                    if let Some(addr) = upstream.as_ref() {
                        match upstream::forward_to_upstream(addr, &buffer).await {
                            Ok(frame) => {
                                if let Err(e) = socket.write_all(&frame).await {
                                    warn!("Failed to send response to client: {e:?}");
                                    return;
                                }
                                continue;
                            }
                            Err(e) => warn!(
                                "Upstream ReadPairRecord failed ({e}); reading local pairing storage"
                            ),
                        }
                    }
                    let pair_file = match pairing_file_finder
                        .get_pairing_record(&pair_record_id)
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

                    let res: Vec<u8> = UsbmuxdServerResponse::PairRecord(pair_file)
                        .into_packet(parsed.tag)
                        .into();
                    if let Err(e) = socket.write_all(&res).await {
                        warn!("Failed to send response to client: {e:?}");
                        return;
                    }

                    continue;
                }
                UsbmuxdServerRequest::SavePairRecord {
                    pair_record_id,
                    device_id,
                    pair_record_data,
                } => {
                    // Forward to upstream when possible; fall back to writing
                    // our own lockdown storage if it's unreachable.
                    if let Some(addr) = upstream.as_ref() {
                        match upstream::forward_to_upstream(addr, &buffer).await {
                            Ok(frame) => {
                                if let Err(e) = socket.write_all(&frame).await {
                                    warn!("Failed to send response to client: {e:?}");
                                    return;
                                }
                                continue;
                            }
                            Err(e) => warn!(
                                "Upstream SavePairRecord failed ({e}); writing local pairing storage"
                            ),
                        }
                    }
                    let udid = match pair_record_id {
                        Some(u) => u,
                        None => {
                            let device_id = match device_id {
                                Some(d) => d,
                                None => {
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

                    let res: Vec<u8> = UsbmuxdServerResponse::Result(result_number)
                        .into_packet(parsed.tag)
                        .into();
                    if let Err(e) = socket.write_all(&res).await {
                        warn!("Failed to send response to client: {e:?}");
                        return;
                    }

                    continue;
                }
                UsbmuxdServerRequest::ReadBuid => {
                    // Forward to upstream when possible; fall back to our own
                    // host identity if it's unreachable.
                    if let Some(addr) = upstream.as_ref() {
                        match upstream::forward_to_upstream(addr, &buffer).await {
                            Ok(frame) => {
                                if let Err(e) = socket.write_all(&frame).await {
                                    warn!("Failed to send response to client: {e:?}");
                                    return;
                                }
                                continue;
                            }
                            Err(e) => {
                                warn!("Upstream ReadBUID failed ({e}); using local host identity")
                            }
                        }
                    }
                    let buid = match pairing_file_finder.get_buid().await {
                        Ok(b) => b,
                        Err(e) => {
                            log::error!("Failed to get buid: {e:?}");
                            return;
                        }
                    };

                    let res: Vec<u8> = UsbmuxdServerResponse::Buid(buid)
                        .into_packet(parsed.tag)
                        .into();
                    if let Err(e) = socket.write_all(&res).await {
                        warn!("Failed to send response to client: {e:?}");
                        return;
                    }

                    continue;
                }
                UsbmuxdServerRequest::Connect { device_id, port } => {
                    info!("Client is establishing connection to port {port}");

                    // In shim mode, a DeviceID below the network base belongs to
                    // the upstream muxer: hand the whole connection to it. The
                    // upstream's Result and the tunnel both flow back over the
                    // splice, so we don't reply ourselves.
                    if let Some(addr) = upstream.as_ref()
                        && device_id < SHIM_NETWORK_ID_BASE
                    {
                        let mut up = match upstream::connect(addr).await {
                            Ok(u) => u,
                            Err(e) => {
                                error!("Failed to reach upstream for Connect: {e}");
                                let res: Vec<u8> = UsbmuxdServerResponse::Result(1)
                                    .into_packet(parsed.tag)
                                    .into();
                                let _ = socket.write_all(&res).await;
                                continue;
                            }
                        };
                        if let Err(e) = up.write_all(&buffer).await {
                            error!("Failed to forward Connect to upstream: {e:?}");
                            let res: Vec<u8> = UsbmuxdServerResponse::Result(1)
                                .into_packet(parsed.tag)
                                .into();
                            let _ = socket.write_all(&res).await;
                            continue;
                        }
                        if let Err(e) = tokio::io::copy_bidirectional(&mut *up, &mut socket).await {
                            info!("Upstream proxied stream stopped: {e:?}");
                        }
                        return;
                    }

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
                            let res: Vec<u8> = UsbmuxdServerResponse::Result(1)
                                .into_packet(parsed.tag)
                                .into();
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
                                match tokio::net::TcpStream::connect((addr, port)).await {
                                    Ok(s) => Ok(Box::new(s) as Upstream),
                                    Err(e) => Err(format!("tcp connect: {e:?}")),
                                }
                            }
                            None => Err("network device missing address".to_string()),
                        },
                        "USB" => match lookup.usb {
                            Some(handle) => match handle.connect(port).await {
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
                            error!("Unable to connect to device {device_id} port {port}: {e}");
                            let res: Vec<u8> = UsbmuxdServerResponse::Result(1)
                                .into_packet(parsed.tag)
                                .into();
                            if let Err(e) = socket.write_all(&res).await {
                                warn!("Failed to send response to client: {e:?}");
                            }
                            continue;
                        }
                    };

                    let res: Vec<u8> = UsbmuxdServerResponse::Result(0)
                        .into_packet(parsed.tag)
                        .into();
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
            }
        }
    });
}

/// netmuxd extension: register a network device the client discovered itself.
async fn handle_add_device(
    socket: &mut (impl AsyncWrite + Unpin),
    manager_sender: &ManagerSender,
    parsed: &RawPacket,
) {
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
}

/// netmuxd extension: drop a device the client previously added.
async fn handle_remove_device(manager_sender: &ManagerSender, parsed: &RawPacket) {
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
}

async fn run_listen<S>(
    mut socket: S,
    manager_sender: ManagerSender,
    listen_tag: u32,
    upstream: Option<UsbmuxdAddr>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let res: Vec<u8> = UsbmuxdServerResponse::Result(0)
        .into_packet(listen_tag)
        .into();
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

    // In shim mode, also relay the upstream muxer's attach/detach frames so the
    // client sees its USB devices alongside our network devices. The pump
    // reconnects with backoff if upstream drops; the guard aborts it when this
    // Listen session ends.
    let (mut upstream_rx, _upstream_guard) = match upstream {
        Some(addr) => {
            let (rx, guard) = spawn_upstream_listen(addr);
            (Some(rx), Some(guard))
        }
        None => (None, None),
    };

    let (mut reader, mut writer) = tokio::io::split(socket);
    let mut sink = [0u8; 256];
    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { return; };
                let response = match event {
                    ListenerEvent::Attached(p) => UsbmuxdServerResponse::Attached(p),
                    ListenerEvent::Detached(id) => UsbmuxdServerResponse::Detached(id),
                };
                let bytes: Vec<u8> = response.into_packet(0).into();
                if let Err(e) = writer.write_all(&bytes).await {
                    info!("Listener write failed, dropping: {e:?}");
                    return;
                }
            }
            // Relay upstream attach/detach frames verbatim.
            frame = next_upstream(&mut upstream_rx) => {
                match frame {
                    Some(frame) => {
                        if let Err(e) = writer.write_all(&frame).await {
                            info!("Listener write (upstream) failed, dropping: {e:?}");
                            return;
                        }
                    }
                    None => {
                        warn!("Upstream Listen stream ended; relaying network devices only");
                        upstream_rx = None;
                    }
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

/// A spawned task that is aborted when this guard is dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Spawn a task that relays the upstream muxer's `Listen` attach/detach frames
/// into the returned channel, reconnecting with capped backoff whenever the
/// upstream stream drops. The returned guard aborts the task when dropped (when
/// the client's Listen session ends).
fn spawn_upstream_listen(
    addr: UsbmuxdAddr,
) -> (tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>, AbortOnDrop) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let handle = tokio::spawn(upstream_listen_pump(addr, tx));
    (rx, AbortOnDrop(handle))
}

async fn upstream_listen_pump(addr: UsbmuxdAddr, tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) {
    use std::time::Duration;
    const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
    const MAX_BACKOFF: Duration = Duration::from_secs(5);

    let mut backoff = INITIAL_BACKOFF;
    loop {
        match open_upstream_listen(&addr).await {
            Ok(mut up) => {
                info!("Upstream Listen stream connected");
                backoff = INITIAL_BACKOFF;
                loop {
                    match upstream::read_frame(&mut *up).await {
                        Ok(frame) => {
                            // Stop for good once the client's Listen session
                            // ends (the receiver was dropped).
                            if tx.send(frame).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            warn!("Upstream Listen stream dropped ({e:?}); reconnecting");
                            break;
                        }
                    }
                }
            }
            Err(e) => warn!("Upstream Listen connect failed ({e}); retrying in {backoff:?}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Open a connection to upstream, send `Listen`, and consume the handshake
/// `Result` frame so only attach/detach frames remain.
async fn open_upstream_listen(addr: &UsbmuxdAddr) -> Result<Box<dyn idevice::ReadWrite>, String> {
    let mut up = upstream::connect(addr).await?;
    let mut p = plist::Dictionary::new();
    p.insert("MessageType".into(), "Listen".into());
    let listen_pkt: Vec<u8> = RawPacket::new(p, 1, 8, 0).into();
    up.write_all(&listen_pkt)
        .await
        .map_err(|e| format!("write Listen to upstream: {e:?}"))?;
    upstream::read_frame(&mut *up)
        .await
        .map_err(|e| format!("read upstream Listen result: {e:?}"))?;
    Ok(up)
}

/// Await the next upstream frame, or never resolve when there is no upstream
/// subscription (so the `select!` branch stays idle in non-shim mode).
async fn next_upstream(
    rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
) -> Option<Vec<u8>> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}
