// jkcoxson

use std::{collections::HashMap, io::Read, net::IpAddr, path::PathBuf};

use plist_plus::Plist;
use tokio::sync::mpsc::UnboundedSender;

pub struct CentralData {
    pub devices: HashMap<String, Device>,
    pub last_index: u64,
    pub last_interface_index: u64,
    plist_storage: String,
    known_mac_addresses: HashMap<String, String>,
}

pub struct Device {
    pub connection_type: String,
    pub device_id: u64,
    pub service_name: String,
    pub interface_index: u64,
    pub network_address: IpAddr,
    pub serial_number: String,
    pub heartbeat_handle: Option<UnboundedSender<()>>,
}

impl CentralData {
    pub fn new(plist_storage: Option<String>) -> CentralData {
        let plist_storage = if plist_storage.is_some() {
            plist_storage.unwrap()
        } else {
            match std::env::consts::OS {
                "macos" => "/var/db/lockdown",
                "linux" => "/var/lib/lockdown",
                "windows" => "C:/ProgramData/Apple/Lockdown",
                _ => panic!("Unsupported OS, specify a path"),
            }
            .to_string()
        };
        CentralData {
            devices: HashMap::new(),
            last_index: 0,
            last_interface_index: 0,
            plist_storage,
            known_mac_addresses: HashMap::new(),
        }
    }
    pub fn add_device(
        &mut self,
        udid: String,
        ip_address: String,
        service_name: String,
        connection_type: String,
    ) {
        if self.devices.contains_key(&udid) {
            return;
        }
        let network_address = ip_address.parse().unwrap();
        self.last_index += 1;
        self.last_interface_index += 1;
        let dev = Device {
            connection_type,
            device_id: self.last_index,
            service_name,
            interface_index: self.last_interface_index,
            network_address,
            serial_number: udid.clone(),
            heartbeat_handle: None,
        };
        self.devices.insert(udid, dev);
    }
    pub fn remove_device(&mut self, udid: String) {
        if !self.devices.contains_key(&udid) {
            return;
        }
        if let Some(handle) = &self.devices.get(&udid).unwrap().heartbeat_handle {
            handle.send(()).unwrap();
        }
        self.devices.remove(&udid);
    }
    pub fn get_pairing_record(&self, udid: String) -> Result<Vec<u8>, ()> {
        let path = PathBuf::from(self.plist_storage.clone()).join(format!("{}.plist", udid));
        println!("Reading pair data from {:?}", path);
        if !path.exists() {
            println!("No pairing record found for {}", udid);
            return Err(());
        }
        // Read the file
        let mut file = std::fs::File::open(path).unwrap();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        Ok(contents)
    }
    pub fn get_buid(&self) -> Result<String, ()> {
        let path = PathBuf::from(self.plist_storage.clone()).join("SystemConfiguration.plist");
        println!("Reading BUID data from {:?}", path);
        if !path.exists() {
            println!("No BUID found");
            return Err(());
        }
        // Read the file to a string
        let mut file = std::fs::File::open(path).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();

        // Parse the string into a plist
        let plist = Plist::from_xml(contents).unwrap();
        let buid = plist.dict_get_item("SystemBUID")?.get_string_val()?;
        Ok(buid)
    }

    pub fn get_udid(&mut self, mac: String) -> Result<String, ()> {
        if let Some(udid) = self.known_mac_addresses.get(&mac) {
            return Ok(udid.to_string());
        }
        // Iterate through all files in the plist storage, loading them into memory
        let path = PathBuf::from(self.plist_storage.clone());
        for entry in std::fs::read_dir(path).unwrap() {
            println!("Unwrapping...");
            let entry = entry.unwrap();
            println!("Reading pair data from {:?}", entry.path());
            let path = entry.path();
            if path.is_file() {
                let mut file = std::fs::File::open(path.clone()).unwrap();
                let mut contents = String::new();
                let plist = match file.read_to_string(&mut contents) {
                    Ok(_) => Plist::from_xml(contents).unwrap(),
                    Err(e) => {
                        println!("Error reading file: {:?}", e);
                        let mut buf = vec![];
                        file.read_to_end(&mut buf).unwrap();
                        match Plist::from_memory(buf) {
                            Ok(plist) => plist,
                            Err(_) => {
                                println!("Error reading file");
                                continue;
                            }
                        }
                    }
                };
                let mac_addr = match plist.dict_get_item("WiFiMACAddress") {
                    Ok(item) => match item.get_string_val() {
                        Ok(val) => val,
                        Err(_) => continue,
                    },
                    Err(_) => continue,
                };
                println!("Adding {} to known mac addresses", mac_addr);
                self.known_mac_addresses.insert(
                    mac_addr,
                    path.file_stem().unwrap().to_string_lossy().to_string(),
                );
            }
        }
        if let Some(udid) = self.known_mac_addresses.get(&mac) {
            return Ok(udid.to_string());
        }
        Err(())
    }
}

impl Device {
    pub fn new(
        connection_type: String,
        device_id: u64,
        service_name: String,
        interface_index: u64,
        network_address: IpAddr,
        serial_number: String,
        handle: Option<UnboundedSender<()>>,
    ) -> Device {
        Device {
            connection_type,
            device_id,
            service_name,
            interface_index,
            network_address,
            serial_number,
            heartbeat_handle: handle,
        }
    }
}

impl TryFrom<Plist> for Device {
    type Error = ();

    fn try_from(plist: Plist) -> Result<Self, Self::Error> {
        let connection_type = plist.dict_get_item("ConnectionType")?.get_string_val()?;
        let device_id = plist.dict_get_item("DeviceID")?.get_uint_val()?;
        let service_name = plist
            .dict_get_item("EscapedFullServiceName")?
            .get_string_val()?;
        let interface_index = plist.dict_get_item("InterfaceIndex")?.get_uint_val()?;

        // Parse the network address
        let network_address = plist.dict_get_item("NetworkAddress")?.get_data_val()?;
        let mut data = vec![];
        for i in 0..network_address.len() {
            data.push(network_address[i] as u8);
        }
        let network_address;
        // Determine if the data is IPv4 or IPv6
        match data[1] {
            0x02 => {
                // IPv4
                let mut ip_addr = [0u8; 4];
                ip_addr.copy_from_slice(&data[4..8]);
                network_address = IpAddr::from(ip_addr);
            }
            0x1E => {
                // IPv6
                let mut ip_addr = [0u8; 16];
                ip_addr.copy_from_slice(&data[7..23]);
                network_address = IpAddr::from(ip_addr);
            }
            _ => return Err(()),
        }

        let serial_number = plist.dict_get_item("SerialNumber")?.get_string_val()?;

        Ok(Device {
            connection_type,
            device_id,
            service_name,
            interface_index,
            network_address,
            serial_number,
            heartbeat_handle: None,
        })
    }
}

impl TryFrom<&Device> for Plist {
    type Error = ();

    fn try_from(device: &Device) -> Result<Self, Self::Error> {
        let mut p = Plist::new_dict();
        p.dict_set_item("ConnectionType", device.connection_type.clone().into())?;
        p.dict_set_item("DeviceID", device.device_id.into())?;
        p.dict_set_item("EscapedFullServiceName", device.service_name.clone().into())?;
        p.dict_set_item("InterfaceIndex", device.interface_index.into())?;

        // Reassemble the network address back into bytes
        let mut data = [0u8; 152];
        match device.network_address {
            IpAddr::V4(ip_addr) => {
                data[0] = 10;
                data[1] = 0x02;
                data[2] = 0x00;
                data[3] = 0x00;
                let mut i = 4;
                for byte in ip_addr.octets() {
                    data[i] = byte;
                    i += 1;
                }
            }
            IpAddr::V6(ip_addr) => {
                data[0] = 28;
                data[1] = 0x1E;
                data[2] = 0x00;
                data[3] = 0x00;
                data[4] = 0x00;
                data[5] = 0x00;
                data[6] = 0x00;
                let mut i = 16;
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
        p.dict_set_item("NetworkAddress", Plist::new_data(&data))?;

        p.dict_set_item("SerialNumber", device.serial_number.clone().into())?;
        Ok(p)
    }
}
