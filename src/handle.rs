// jkcoxson
// Handle raw packets

use crate::{central_data::CentralData, raw_packet::RawPacket, response_builder};
use plist_plus::Plist;
use std::sync::Arc;
use tokio::{io::AsyncWriteExt, net::TcpStream, sync::Mutex};

/// Handles usbmuxd's requests
pub async fn cope(
    packet: RawPacket,
    mut stream: TcpStream,
    data: Arc<Mutex<CentralData>>,
) -> Result<(), ()> {
    let packet_type = packet
        .plist
        .clone()
        .dict_get_item("MessageType")?
        .get_string_val()?;
    match packet_type.as_str() {
        "ListDevices" => {
            println!("Getting a list of devices");
            let res = RawPacket::new(response_builder::list_devices(data).await, 1, 8, packet.tag);
            println!("Sending: {:?}", res);
            let res: Vec<u8> = res.into();
            stream.write_all(&res).await.unwrap();
        }
        "Listen" => {
            // noop
            // This is basically libusbmuxd saying "uh hello, where's my response?"
        }
        "ReadPairRecord" => {
            println!("Reading pair data");
            let lock = data.lock().await;
            let pair_file = match lock.get_pairing_record(
                packet
                    .plist
                    .dict_get_item("PairRecordID")?
                    .get_string_val()?,
            ) {
                Ok(pair_file) => pair_file,
                Err(_) => {
                    println!("Error getting pairing record");
                    return Ok(());
                }
            };

            let mut p = Plist::new_dict();
            p.dict_set_item("PairRecordData", pair_file.into()).unwrap();

            let res = RawPacket::new(p, 1, 8, packet.tag);
            println!("Sending: {:?}", res);
            let res: Vec<u8> = res.into();
            stream.write_all(&res).await.unwrap();
        }
        "ReadBUID" => {
            println!("Reading BUID");
            let lock = data.lock().await;
            let buid = lock.get_buid()?;

            let mut p = Plist::new_dict();
            p.dict_set_item("BUID", buid.into()).unwrap();

            let res = RawPacket::new(p, 1, 8, packet.tag);
            println!("Sending: {:?}", res);
            let res: Vec<u8> = res.into();
            stream.write_all(&res).await.unwrap();
        }
        _ => {
            println!("Unknown packet type: {}", packet_type);
        }
    }
    Ok(())
}

/// Handles netmuxd specific requests
pub async fn instruction(
    packet: RawPacket,
    mut stream: TcpStream,
    data: Arc<Mutex<CentralData>>,
) -> Result<(), ()> {
    println!("Getting message type");
    let packet_type = packet
        .plist
        .clone()
        .dict_get_item("MessageType")?
        .get_string_val()?;
    match packet_type.as_str() {
        "AddDevice" => {
            println!("here 1: {:?}", packet.plist);
            let connection_type = packet
                .plist
                .clone()
                .dict_get_item("ConnectionType")?
                .get_string_val()?;
            println!("here 2");
            let service_name = packet
                .plist
                .clone()
                .dict_get_item("ServiceName")?
                .get_string_val()?;
            println!("here 3");
            let ip_address = packet
                .plist
                .clone()
                .dict_get_item("IPAddress")?
                .get_string_val()?;
            println!("here 4");
            let udid = packet
                .plist
                .clone()
                .dict_get_item("DeviceID")?
                .get_string_val()?;
            let mut central_data = data.lock().await;
            central_data.add_device(udid, ip_address, service_name, connection_type);

            let mut p = Plist::new_dict();
            p.dict_set_item("Result", "OK".into())?;
            let res: Vec<u8> = RawPacket::new(p, 1, 8, packet.tag).into();
            stream.write_all(&res).await.unwrap();
        }
        _ => {
            println!("Unknown packet type: {}", packet_type);
        }
    }
    Ok(())
}
