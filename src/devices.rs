// jkcoxson

use std::net::IpAddr;

#[derive(Debug, Clone)]
pub struct MuxerDevice {
    // Universal types
    pub connection_type: String,
    pub device_id: u64,
    pub interface_index: u64,
    pub serial_number: String,

    // Network types
    pub network_address: Option<IpAddr>,
    pub service_name: Option<String>,

    // USB types
    pub connection_speed: Option<u64>,
    pub location_id: Option<u64>,
    pub product_id: Option<u64>,
}

impl From<&MuxerDevice> for plist::Dictionary {
    fn from(device: &MuxerDevice) -> Self {
        let mut p = plist::Dictionary::new();
        p.insert(
            "ConnectionType".into(),
            device.connection_type.clone().into(),
        );
        p.insert("DeviceID".into(), device.device_id.into());
        if device.connection_type == "Network" {
            p.insert(
                "EscapedFullServiceName".into(),
                device
                    .service_name
                    .clone()
                    .expect("Network device, but no service name")
                    .into(),
            );
        }
        p.insert("InterfaceIndex".into(), device.interface_index.into());

        // Reassemble the network address back into bytes
        if device.connection_type == "Network" {
            let mut data = [0u8; 152];
            match device
                .network_address
                .expect("Network device, but no address")
            {
                IpAddr::V4(ip_addr) => {
                    data[0] = 0x02;
                    data[1] = 0x00;
                    data[2] = 0x00;
                    data[3] = 0x00;
                    let mut i = 4;
                    for byte in ip_addr.octets() {
                        data[i] = byte;
                        i += 1;
                    }
                }
                IpAddr::V6(ip_addr) => {
                    data[0] = 0x1E;
                    data[1] = 0x00;
                    data[2] = 0x00;
                    data[3] = 0x00;
                    data[4] = 0x00;
                    data[5] = 0x00;
                    data[6] = 0x00;
                    data[7] = 0x00;
                    let mut i = 8;
                    for byte in ip_addr.octets() {
                        data[i] = byte;
                        i += 1;
                    }
                }
            }
            // Start from the back and fill with zeros
            let mut i = data.len() - 2;
            while i > 0 {
                if data[i] != 0 {
                    break;
                }
                data[i] = 0;
                i -= 1;
            }
            p.insert("NetworkAddress".into(), plist::Value::Data(data.to_vec()));
        }

        p.insert("SerialNumber".into(), device.serial_number.clone().into());
        p
    }
}
