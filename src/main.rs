// jkcoxson

use std::sync::Arc;
#[cfg(unix)]
use std::{fs, os::unix::prelude::PermissionsExt};

use crate::raw_packet::RawPacket;
use devices::SharedDevices;
use log::{debug, error, info, trace, warn};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
};

mod devices;
mod heartbeat;
mod mdns;
mod raw_packet;

#[tokio::main]
async fn main() {
    println!("Starting netmuxd");

    env_logger::init();
    info!("Logger initialized");

    let mut port = 27015;
    #[cfg(unix)]
    let mut host = None;
    #[cfg(windows)]
    let mut host = Some("localhost".to_string());
    let mut plist_storage = None;
    let mut use_heartbeat = true;

    #[cfg(unix)]
    let mut use_unix = true;

    let mut use_mdns = true;

    // Loop through args
    let mut i = 0;
    while i < std::env::args().len() {
        match std::env::args().nth(i).unwrap().as_str() {
            "-p" | "--port" => {
                port = std::env::args()
                    .nth(i + 1)
                    .expect("port flag passed without number")
                    .parse::<i32>()
                    .expect("port isn't a number");
                i += 2;
            }
            "--host" => {
                host = Some(
                    std::env::args()
                        .nth(i + 1)
                        .expect("host flag passed without host")
                        .to_string(),
                );
                i += 2;
            }
            "--plist-storage" => {
                plist_storage = Some(
                    std::env::args()
                        .nth(i + 1)
                        .expect("flag passed without value"),
                );
                i += 1;
            }
            #[cfg(unix)]
            "--disable-unix" => {
                use_unix = false;
                i += 1;
            }
            "--disable-mdns" => {
                use_mdns = false;
                i += 1;
            }
            "--disable-heartbeat" => {
                use_heartbeat = false;
                i += 1;
            }
            "-h" | "--help" => {
                println!("netmuxd - a network multiplexer");
                println!("Usage:");
                println!("  netmuxd [options]");
                println!("Options:");
                println!("  -p, --port <port>");
                println!("  --host <host>");
                println!("  --plist-storage <path>");
                println!("  --disable-heartbeat");
                #[cfg(unix)]
                println!("  --disable-unix");
                println!("  --disable-mdns");
                #[cfg(feature = "usb")]
                println!("  --enable-usb  (unusable for now)");
                println!("  -h, --help");
                println!("  --about");
                println!("\n\nSet RUST_LOG to info, debug, warn, error, or trace to see more logs. Default is error.");
                std::process::exit(0);
            }
            "--about" => {
                println!("netmuxd - a network multiplexer");
                println!("Copyright (c) 2020 Jackson Coxson");
                println!("Licensed under the MIT License");
            }
            _ => {
                i += 1;
            }
        }
    }
    info!("Collected arguments, proceeding");

    let data = Arc::new(Mutex::new(
        devices::SharedDevices::new(plist_storage, use_heartbeat).await,
    ));
    info!("Created new central data");
    let data_clone = data.clone();

    if let Some(host) = host.clone() {
        let tcp_data = data.clone();
        tokio::spawn(async move {
            let data = tcp_data;
            // Create TcpListener
            let listener = tokio::net::TcpListener::bind(format!("{}:{}", host, port))
                .await
                .expect("Unable to bind to TCP listener");

            println!("Listening on {}:{}", host, port);
            #[cfg(unix)]
            println!("WARNING: Running in host mode will not work unless you are running a daemon in unix mode as well");
            loop {
                let (socket, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => {
                        warn!("Error accepting connection");
                        continue;
                    }
                };

                handle_stream(socket, data.clone()).await;
            }
        });
    }

    #[cfg(unix)]
    if use_unix {
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

                handle_stream(socket, data.clone()).await;
            }
        });
    }

    if use_mdns {
        let local = tokio::task::LocalSet::new();
        local.spawn_local(async move {
            mdns::discover(data_clone).await;
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
    data: Arc<Mutex<SharedDevices>>,
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

                            let udid = match parsed.plist.get("DeviceID") {
                                Some(plist::Value::String(u)) => u,
                                _ => {
                                    warn!("Packet didn't contain DeviceID");
                                    return;
                                }
                            };

                            let mut central_data = data.lock().await;
                            let ip_address = match ip_address.parse() {
                                Ok(i) => i,
                                Err(_) => {
                                    warn!("Bad IP requested: {ip_address}");
                                    return;
                                }
                            };
                            let res = match central_data
                                .add_network_device(
                                    udid.to_owned(),
                                    ip_address,
                                    service_name.to_owned(),
                                    connection_type.to_owned(),
                                    data.clone(),
                                )
                                .await
                            {
                                Ok(_) => 1,
                                Err(e) => {
                                    error!("Failed to add requested device to muxer: {e:?}");
                                    0
                                }
                            };

                            let mut p = plist::Dictionary::new();
                            p.insert("Result".into(), res.into());
                            let res: Vec<u8> = RawPacket::new(p, 1, 8, parsed.tag).into();
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

                            let mut central_data = data.lock().await;
                            central_data.remove_device(udid);
                            return;
                        }
                        //////////////////////////////
                        // usbmuxd protocol packets //
                        //////////////////////////////
                        "ListDevices" => {
                            let data = data.lock().await;
                            let mut device_list = Vec::new();
                            for i in &data.devices {
                                let mut to_push = plist::Dictionary::new();
                                to_push.insert("DeviceID".into(), i.1.device_id.into());
                                to_push.insert("MessageType".into(), "Attached".into());
                                to_push.insert(
                                    "Properties".into(),
                                    plist::Value::Dictionary(i.1.into()),
                                );

                                device_list.push(plist::Value::Dictionary(to_push));
                            }
                            let mut upper = plist::Dictionary::new();
                            upper.insert("DeviceList".into(), plist::Value::Array(device_list));
                            let res = RawPacket::new(upper, 1, 8, parsed.tag);
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
                            let lock = data.lock().await;
                            let pair_file = match lock
                                .get_pairing_record(match parsed.plist.get("PairRecordID") {
                                    Some(plist::Value::String(p)) => p.to_owned(),
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
                            std::mem::drop(lock);

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
                            let lock = data.lock().await;
                            let buid = match lock.get_buid().await {
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
                            let central_data = data.lock().await;
                            if let Some(device) = central_data.get_device_by_id(device_id) {
                                let network_address = device.network_address;
                                let device_id = device.device_id;
                                std::mem::drop(central_data);

                                info!("Connecting to device {}", device_id);

                                match network_address {
                                    Some(ip) => {
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

                                                if let Err(e) = tokio::io::copy_bidirectional(
                                                    &mut stream,
                                                    &mut socket,
                                                )
                                                .await
                                                {
                                                    info!("Bidirectional stream stopped: {e:?}");
                                                }
                                                continue;
                                            }
                                            Err(e) => {
                                                error!("Unable to connect to device {device_id} port {connection_port}: {e:?}");
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
                                    None => {
                                        unimplemented!()
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
