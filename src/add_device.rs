// Jackson Coxson
// Stand-alone binary to add devices to netmuxd

use plist_plus::Plist;
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
    let mut request = Plist::new_dict();
    request
        .dict_set_item("MessageType", Plist::new_string("AddDevice"))
        .unwrap();
    request
        .dict_set_item("ConnectionType", Plist::new_string("Network"))
        .unwrap();
    request
        .dict_set_item(
            "ServiceName",
            Plist::new_string(format!("_{}._{}.local", SERVICE_NAME, SERVICE_PROTOCOL).as_str()),
        )
        .unwrap();
    request
        .dict_set_item("IPAddress", Plist::new_string(ip.as_str()))
        .unwrap();
    request
        .dict_set_item("DeviceID", Plist::new_string(udid.as_str()))
        .unwrap();

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
}
