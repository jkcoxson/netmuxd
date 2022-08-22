// jkcoxson

use std::sync::Arc;
#[cfg(unix)]
use std::{fs, os::unix::prelude::PermissionsExt};

use devices::SharedDevices;
use log::{error, info, warn};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
};

use crate::handle::cope;

mod devices;
mod handle;
mod heartbeat;
mod mdns;
mod raw_packet;
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

async fn handle_stream(
    mut socket: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    data: Arc<Mutex<SharedDevices>>,
) {
    tokio::spawn(async move {
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

        if let Ok(Some(to_send)) = cope(parsed, data).await {
            if to_send.is_empty() {
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
                        return;
                    }
                }
            }
            socket.write_all(&to_send).await.unwrap();
        }
    });
}
