// jkcoxson

use std::{fs, os::unix::prelude::PermissionsExt, sync::Arc};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};

use crate::handle::{cope, instruction};

mod central_data;
mod handle;
mod heartbeat;
mod mdns;
mod raw_packet;

#[tokio::main]
async fn main() {
    println!("Starting netmuxd");

    let mut port = 27015;
    let mut host = None;
    let mut plist_storage = None;

    // Loop through args
    let mut i = 0;
    while i < std::env::args().len() {
        match std::env::args().nth(i).unwrap().as_str() {
            "-p" | "--port" => {
                port = std::env::args().nth(i + 1).unwrap().parse::<i32>().unwrap();
                i += 2;
            }
            "-h" | "--host" => {
                host = Some(std::env::args().nth(i + 1).unwrap().to_string());
                i += 2;
            }
            "--plist-storage" => {
                plist_storage = Some(std::env::args().nth(i + 1).unwrap());
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    let data = Arc::new(Mutex::new(central_data::CentralData::new(plist_storage)));
    let data_clone = data.clone();
    tokio::spawn(async move {
        mdns::discover(data_clone).await;
        println!("mDNS discovery stopped, how the heck did you break this");
    });

    if let Some(host) = host {
        // Create TcpListener
        let listener = tokio::net::TcpListener::bind(format!("{}:{}", host, port))
            .await
            .unwrap();

        println!("Listening on {}:{}", host, port);
        println!("WARNING: Running in host mode will not work unless you are running a daemon in unix mode as well");
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => {
                    continue;
                }
            };
            let cloned_data = data.clone();
            // Wait for a message from the client
            let mut buf = [0; 1024];
            let size = match socket.read(&mut buf).await {
                Ok(s) => s,
                Err(_) => {
                    break;
                }
            };
            if size == 0 {
                break;
            }
            let buffer = &buf[0..size];

            let parsed: raw_packet::RawPacket = buffer.into();

            if parsed.message == 69 && parsed.tag == 69 {
                match instruction(parsed, cloned_data.clone()).await {
                    Ok(to_send) => {
                        if let Some(to_send) = to_send {
                            socket.write_all(&to_send).await.unwrap();
                        }
                    }
                    Err(_) => {}
                }
            } else {
                match cope(parsed, cloned_data).await {
                    Ok(to_send) => {
                        if let Some(to_send) = to_send {
                            socket.write_all(&to_send).await.unwrap();
                        }
                    }
                    Err(_) => {}
                }
            }
        }
    } else {
        // Delete old Unix socket
        std::fs::remove_file("/var/run/usbmuxd").unwrap_or_default();
        // Create UnixListener
        let listener = tokio::net::UnixListener::bind("/var/run/usbmuxd").unwrap();
        // Change the permission of the socket
        fs::set_permissions("/var/run/usbmuxd", fs::Permissions::from_mode(0o666)).unwrap();

        println!("Listening on /var/run/usbmuxd");

        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => {
                    println!("Error accepting connection");
                    continue;
                }
            };
            let cloned_data = data.clone();
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
                    return;
                }
                let buffer = &buf[0..size];

                let parsed: raw_packet::RawPacket = buffer.into();

                if parsed.message == 69 && parsed.tag == 69 {
                    match instruction(parsed, cloned_data.clone()).await {
                        Ok(to_send) => {
                            if let Some(to_send) = to_send {
                                socket.write_all(&to_send).await.unwrap();
                            }
                        }
                        Err(_) => {}
                    }
                } else {
                    match cope(parsed, cloned_data).await {
                        Ok(to_send) => {
                            if let Some(to_send) = to_send {
                                socket.write_all(&to_send).await.unwrap();
                            }
                        }
                        Err(_) => {}
                    }
                }
            });
        }
    }
}
