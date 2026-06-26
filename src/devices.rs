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
        p.insert("InterfaceIndex".into(), device.interface_index.into());
        p.insert("SerialNumber".into(), device.serial_number.clone().into());

        match device.connection_type.as_str() {
            "Network" => {
                p.insert(
                    "EscapedFullServiceName".into(),
                    device
                        .service_name
                        .clone()
                        .expect("Network device, but no service name")
                        .into(),
                );

                let bsd_sockaddr = cfg!(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "netbsd",
                    target_os = "openbsd",
                    target_os = "dragonfly",
                ));

                let mut data = [0u8; 128];
                match device
                    .network_address
                    .expect("Network device, but no address")
                {
                    IpAddr::V4(ip_addr) => {
                        if bsd_sockaddr {
                            data[0] = 0x10; // sa_len = sizeof(sockaddr_in) = 16
                            data[1] = 0x02; // sa_family = AF_INET
                        } else {
                            // sa_family is a u16 starting at byte 0.
                            data[0] = 0x02;
                            data[1] = 0x00;
                        }
                        // bytes 2..4 = port, left zero
                        data[4..8].copy_from_slice(&ip_addr.octets());
                    }
                    IpAddr::V6(ip_addr) => {
                        if bsd_sockaddr {
                            data[0] = 0x1C; // sa_len = sizeof(sockaddr_in6) = 28
                            data[1] = 0x1E; // sa_family = AF_INET6 (BSD = 30)
                        } else {
                            // Linux AF_INET6 = 10.
                            data[0] = 0x0A;
                            data[1] = 0x00;
                        }
                        // bytes 2..4 = port, 4..8 = flowinfo, all zero
                        data[8..24].copy_from_slice(&ip_addr.octets());
                        // bytes 24..28 = scope_id, left zero
                    }
                }
                p.insert("NetworkAddress".into(), plist::Value::Data(data.to_vec()));
            }
            "USB" => {
                if let Some(speed) = device.connection_speed {
                    p.insert("ConnectionSpeed".into(), speed.into());
                }
                if let Some(location) = device.location_id {
                    p.insert("LocationID".into(), location.into());
                }
                if let Some(pid) = device.product_id {
                    p.insert("ProductID".into(), pid.into());
                }
            }
            _ => {}
        }

        p
    }
}
