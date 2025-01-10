// Jackson Coxson
// Stand-alone binary to add devices to netmuxd

use std::io::{Read, Write};

mod raw_packet;

const SERVICE_NAME: &str = "apple-mobdev2";
const SERVICE_PROTOCOL: &str = "tcp";

trait ReadWrite: Read + Write + std::fmt::Debug {}
impl<T: Read + Write + std::fmt::Debug> ReadWrite for T {}

fn main() {
    // Read the command line arguments
    let args = std::env::args().collect::<Vec<String>>();
    if args.len() < 3 {
        println!("Usage: add_device <udid> <ip>");
        return;
    }
    let udid = &args[1];
    let ip = &args[2];
    let mut request = plist::Dictionary::new();
    request.insert("MessageType".into(), "AddDevice".into());
    request.insert("ConnectionType".into(), "Network".into());
    request.insert(
        "ServiceName".into(),
        format!("_{}._{}.local", SERVICE_NAME, SERVICE_PROTOCOL).into(),
    );
    request.insert("IPAddress".into(), ip.to_string().into());
    request.insert("DeviceID".into(), udid.as_str().into());

    let request = raw_packet::RawPacket::new(request, 69, 69, 69);
    let request: Vec<u8> = request.into();

    // Connect to the socket
    let socket_address =
        std::env::var("USBMUXD_SOCKET_ADDRESS").unwrap_or("/var/run/usbmuxd".to_string());

    let mut stream: Box<dyn ReadWrite> = if socket_address.starts_with('/') {
        Box::new(
            std::os::unix::net::UnixStream::connect(socket_address)
                .expect("Unable to connect to unix socket"),
        )
    } else {
        Box::new(
            std::net::TcpStream::connect(socket_address).expect("Unable to connect to TCP socket"),
        )
    };
    stream.write_all(&request).unwrap();

    let mut buf = Vec::new();
    let size = stream.read_to_end(&mut buf).unwrap();

    let buffer = &mut buf[0..size].to_vec();
    if size == 16 {
        let packet_size = &buffer[0..4];
        let packet_size = u32::from_le_bytes(packet_size.try_into().unwrap());
        // Pull the rest of the packet
        let mut packet = vec![0; packet_size as usize];
        let _ = stream.read(&mut packet).unwrap();
        // Append the packet to the buffer
        buffer.append(&mut packet);
    }

    let parsed: raw_packet::RawPacket = buffer.try_into().unwrap();
    match parsed.plist.get("Result") {
        Some(plist::Value::Integer(r)) => {
            if r.as_unsigned().unwrap() == 1 {
                println!("Success");
            } else {
                println!("Failure");
            }
        }
        _ => {
            println!("Failure");
        }
    }
}
