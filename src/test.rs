// jkcoxson
// Test file for reverse engineering Apple's usbmuxd

use plist_plus::Plist;
use tokio::io::AsyncWriteExt;

mod raw_packet;

#[tokio::main]
async fn main() {
    let mut p = Plist::new_dict();
    p.dict_set_item("MessageType", "AddDevice".into()).unwrap();
    p.dict_set_item("ConnectionType", "Network".into()).unwrap();
    p.dict_set_item("ServiceName", "lol".into()).unwrap();
    p.dict_set_item("IPAddress", "192.168.1.3".into()).unwrap();
    p.dict_set_item("DeviceID", "00008101-001E30590C08001E".into())
        .unwrap();
    let packet: Vec<u8> = raw_packet::RawPacket::new(p, 1, 69, 69).into();

    // Connect to netmuxd
    let mut stream = tokio::net::TcpStream::connect("127.0.0.1:27015")
        .await
        .unwrap();
    stream.write_all(&packet).await.unwrap();
}
