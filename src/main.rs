// jkcoxson

use std::sync::Arc;
#[cfg(unix)]
use std::{fs, os::unix::prelude::PermissionsExt};

use crate::raw_packet::RawPacket;
use devices::SharedDevices;
use log::{error, info, warn};
use plist_plus::Plist;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
};

mod devices;
mod heartbeat;
mod mdns;
mod raw_packet;
#[cfg(feature = "usb")]
mod usb;

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

    #[cfg(unix)]
    let mut use_unix = true;

    let mut use_mdns = true;
    #[cfg(feature = "usb")]
    let mut use_usb = false;

    // Loop through args
    let mut i = 0;
    while i < std::env::args().len() {
        match std::env::args().nth(i).unwrap().as_str() {
            "-p" | "--port" => {
                port = std::env::args().nth(i + 1).unwrap().parse::<i32>().unwrap();
                i += 2;
            }
            "--host" => {
                host = Some(std::env::args().nth(i + 1).unwrap().to_string());
                i += 2;
            }
            "--plist-storage" => {
                plist_storage = Some(std::env::args().nth(i + 1).unwrap());
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
            #[cfg(feature = "usb")]
            "--enable-usb" => {
                use_usb = true;
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
                #[cfg(unix)]
                println!("  --disable-unix");
                println!("  --enable-mdns");
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

    let data = Arc::new(Mutex::new(devices::SharedDevices::new(plist_storage)));
    info!("Created new central data");
    let data_clone = data.clone();
    #[cfg(feature = "usb")]
    let usb_data = data.clone();

    if let Some(host) = host.clone() {
        let tcp_data = data.clone();
        tokio::spawn(async move {
            let data = tcp_data;
            // Create TcpListener
            let listener = tokio::net::TcpListener::bind(format!("{}:{}", host, port))
                .await
                .unwrap();

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
            let listener = tokio::net::UnixListener::bind("/var/run/usbmuxd").unwrap();
            // Change the permission of the socket
            info!("Changing permissions of socket");
            fs::set_permissions("/var/run/usbmuxd", fs::Permissions::from_mode(0o666)).unwrap();

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

    #[cfg(feature = "usb")]
    if use_usb {
        usb::start_listener(usb_data);
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
    Connect,
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
            let size = match socket.read(&mut buf).await {
                Ok(s) => s,
                Err(_) => {
                    return;
                }
            };
            if size == 0 {
                info!("Unix size is zero, closing connection");
                return;
            }

            let buffer = &mut buf[0..size].to_vec();
            if size == 16 {
                info!("Only read the header, pulling more bytes");
                // Get the number of bytes to pull
                let packet_size = &buffer[0..4];
                let packet_size = u32::from_le_bytes(packet_size.try_into().unwrap());
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

            match current_directions {
                Directions::None => {
                    // Handle the packet
                    let packet_type = parsed
                        .plist
                        .clone()
                        .dict_get_item("MessageType")
                        .unwrap()
                        .get_string_val()
                        .unwrap();

                    match packet_type.as_str() {
                        //////////////////////////////
                        // netmuxd specific packets //
                        //////////////////////////////
                        "AddDevice" => {
                            let connection_type = parsed
                                .plist
                                .clone()
                                .dict_get_item("ConnectionType")
                                .unwrap()
                                .get_string_val()
                                .unwrap();
                            let service_name = parsed
                                .plist
                                .clone()
                                .dict_get_item("ServiceName")
                                .unwrap()
                                .get_string_val()
                                .unwrap();
                            let ip_address = parsed
                                .plist
                                .clone()
                                .dict_get_item("IPAddress")
                                .unwrap()
                                .get_string_val()
                                .unwrap();
                            let udid = parsed
                                .plist
                                .clone()
                                .dict_get_item("DeviceID")
                                .unwrap()
                                .get_string_val()
                                .unwrap();
                            let mut central_data = data.lock().await;
                            heartbeat::heartbeat(
                                udid.clone(),
                                ip_address.clone().parse().unwrap(),
                                data.clone(),
                            );
                            central_data.add_network_device(
                                udid,
                                ip_address.parse().unwrap(),
                                service_name,
                                connection_type,
                                data.clone(),
                            );

                            let mut p = Plist::new_dict();
                            p.dict_set_item("Result", "OK".into()).unwrap();
                            let res: Vec<u8> = RawPacket::new(p, 1, 8, parsed.tag).into();
                            socket.write_all(&res).await.unwrap();

                            // No more further communication for this packet
                            return;
                        }
                        //////////////////////////////
                        // usbmuxd protocol packets //
                        //////////////////////////////
                        "ListDevices" => {
                            let data = data.lock().await;
                            let mut device_list = Plist::new_array();
                            for i in &data.devices {
                                let mut to_push = Plist::new_dict();
                                to_push
                                    .dict_set_item("DeviceID", Plist::new_uint(i.1.device_id))
                                    .unwrap();
                                to_push
                                    .dict_set_item("MessageType", "Attached".into())
                                    .unwrap();
                                to_push
                                    .dict_set_item("Properties", i.1.try_into().unwrap())
                                    .unwrap();

                                device_list.array_append_item(to_push).unwrap();
                            }
                            let mut upper = Plist::new_dict();
                            upper.dict_set_item("DeviceList", device_list).unwrap();
                            let res = RawPacket::new(upper, 1, 8, parsed.tag);
                            let res: Vec<u8> = res.into();
                            socket.write_all(&res).await.unwrap();

                            // No more further communication for this packet
                            return;
                        }
                        "Listen" => {
                            // The full functionality of this is not implemented. We will just maintain the connection.
                            current_directions = Directions::Listen;
                        }
                        "ReadPairRecord" => {
                            let lock = data.lock().await;
                            let pair_file = match lock.get_pairing_record(
                                parsed
                                    .plist
                                    .dict_get_item("PairRecordID")
                                    .unwrap()
                                    .get_string_val()
                                    .unwrap(),
                            ) {
                                Ok(pair_file) => pair_file,
                                Err(_) => {
                                    // Unimplemented
                                    return;
                                }
                            };

                            let mut p = Plist::new_dict();
                            p.dict_set_item("PairRecordData", pair_file.into()).unwrap();

                            let res = RawPacket::new(p, 1, 8, parsed.tag);
                            let res: Vec<u8> = res.into();
                            socket.write_all(&res).await.unwrap();

                            // No more further communication for this packet
                            return;
                        }
                        "ReadBUID" => {
                            let lock = data.lock().await;
                            let buid = lock.get_buid().unwrap();

                            let mut p = Plist::new_dict();
                            p.dict_set_item("BUID", buid.into()).unwrap();

                            let res = RawPacket::new(p, 1, 8, parsed.tag);
                            let res: Vec<u8> = res.into();
                            socket.write_all(&res).await.unwrap();

                            // No more further communication for this packet
                            return;
                        }
                        "Connect" => {
                            current_directions = Directions::Connect;
                        }
                        _ => {
                            warn!("Unknown packet type");
                        }
                    }
                }
                Directions::Connect => todo!(),
                Directions::Listen => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });
}
