// jkcoxson
// Passes packets from a a TCP stream to the unix socket for analysis.

use colored::Colorize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod raw_packet;

#[tokio::main]
async fn main() {
    let mut port = 27015;
    let mut host = "127.0.0.1".to_string();

    // Loop through args
    let mut i = 0;
    while i < std::env::args().len() {
        match std::env::args().nth(i).unwrap().as_str() {
            "-p" | "--port" => {
                port = std::env::args().nth(i + 1).unwrap().parse::<i32>().unwrap();
                i += 2;
            }
            "-h" | "--host" => {
                host = std::env::args().nth(i + 1).unwrap().to_string();
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    // Create TcpListener
    let listener = tokio::net::TcpListener::bind(format!("{}:{}", host, port))
        .await
        .unwrap();

    println!("Listening on {}:{}", host, port);

    loop {
        let (mut socket, _) = match listener.accept().await {
            Ok(s) => {
                println!("Accepted connection");
                s
            }
            Err(e) => {
                println!("Error accepting connection: {}", e);
                continue;
            }
        };

        // Dial the unix socket
        let mut unix_socket = match tokio::net::UnixStream::connect("/var/run/usbmuxd").await {
            Ok(s) => s,
            Err(e) => {
                println!("Error connecting to unix socket: {}", e);
                continue;
            }
        };

        tokio::spawn(async move {
            let mut tcp_buffer = [0; 16384];
            let mut unix_buffer = [0; 16384];
            let size = socket.read(&mut tcp_buffer).await;
            if size.is_ok() {
                let size = size.unwrap();
                if size == 0 {
                    println!("Client disconnected");
                } else {
                    println!("Received {} bytes", size);
                    let buffer = &tcp_buffer[0..size];
                    let parsed = raw_packet::RawPacket::from(buffer);
                    println!("{}", format!("{:?}", parsed).blue());
                    unix_socket.write_all(buffer).await.unwrap();
                }
            }
            let size = unix_socket.read(&mut unix_buffer).await;
            if size.is_ok() {
                let size = size.unwrap();
                if size == 0 {
                    println!("Unix socket disconnected");
                } else {
                    println!("Received {} bytes", size);
                    let buffer = &unix_buffer[0..size];
                    let parsed = raw_packet::RawPacket::from(buffer);
                    println!("{}", format!("{:?}", parsed).green());
                    socket.write_all(buffer).await.unwrap();
                }
            }
        });
    }
}
