// jkcoxson
// Handle raw packets

use crate::{central_data::CentralData, heartbeat, raw_packet::RawPacket};
use log::info;
use plist_plus::Plist;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Handles usbmuxd's requests
pub async fn cope(packet: RawPacket, data: Arc<Mutex<CentralData>>) -> Result<Option<Vec<u8>>, ()> {
    let packet_type = packet
        .plist
        .clone()
        .dict_get_item("MessageType")?
        .get_string_val()?;
    info!("Got packet type: {:?}", packet_type);
    match packet_type.as_str() {
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
            let res = RawPacket::new(upper, 1, 8, packet.tag);
            let res: Vec<u8> = res.into();
            return Ok(Some(res));
        }
        "Listen" => {
            // Instruct netmuxd to not drop this connection until it drops from the other side
            return Ok(Some(Vec::new()));
        }
        "ReadPairRecord" => {
            let lock = data.lock().await;
            let pair_file = match lock.get_pairing_record(
                packet
                    .plist
                    .dict_get_item("PairRecordID")?
                    .get_string_val()?,
            ) {
                Ok(pair_file) => pair_file,
                Err(_) => {
                    return Ok(None);
                }
            };

            let mut p = Plist::new_dict();
            p.dict_set_item("PairRecordData", pair_file.into()).unwrap();

            let res = RawPacket::new(p, 1, 8, packet.tag);
            let res: Vec<u8> = res.into();
            return Ok(Some(res));
        }
        "ReadBUID" => {
            let lock = data.lock().await;
            let buid = lock.get_buid()?;

            let mut p = Plist::new_dict();
            p.dict_set_item("BUID", buid.into()).unwrap();

            let res = RawPacket::new(p, 1, 8, packet.tag);
            let res: Vec<u8> = res.into();
            return Ok(Some(res));
        }
        _ => {
            println!("Unknown packet type: {}", packet_type);
        }
    }
    Ok(None)
}

/// Handles netmuxd specific requests
pub async fn instruction(
    packet: RawPacket,
    data: Arc<Mutex<CentralData>>,
) -> Result<Option<Vec<u8>>, ()> {
    let packet_type = packet
        .plist
        .clone()
        .dict_get_item("MessageType")?
        .get_string_val()?;
    match packet_type.as_str() {
        "AddDevice" => {
            info!("Adding manual device");
            let connection_type = packet
                .plist
                .clone()
                .dict_get_item("ConnectionType")?
                .get_string_val()?;
            let service_name = packet
                .plist
                .clone()
                .dict_get_item("ServiceName")?
                .get_string_val()?;
            let ip_address = packet
                .plist
                .clone()
                .dict_get_item("IPAddress")?
                .get_string_val()?;
            let udid = packet
                .plist
                .clone()
                .dict_get_item("DeviceID")?
                .get_string_val()?;
            let mut central_data = data.lock().await;
            heartbeat::heartbeat(
                udid.clone(),
                ip_address.clone().parse().unwrap(),
                data.clone(),
            );
            central_data.add_device(udid, ip_address, service_name, connection_type);

            let mut p = Plist::new_dict();
            p.dict_set_item("Result", "OK".into())?;
            let res: Vec<u8> = RawPacket::new(p, 1, 8, packet.tag).into();
            return Ok(Some(res));
        }
        _ => {
            println!("Unknown packet type: {}", packet_type);
        }
    }
    Ok(None)
}
