// jkcoxson

use std::{collections::HashMap, io::Read, net::IpAddr, path::PathBuf, sync::Arc};

use log::{error, info, trace, warn};
use plist_plus::{error::PlistError, Plist};
use tokio::sync::{mpsc::UnboundedSender, Mutex};

use crate::heartbeat;

pub struct SharedDevices {
    pub devices: HashMap<String, MuxerDevice>,
    pub last_index: u64,
    pub last_interface_index: u64,
    plist_storage: String,
    known_mac_addresses: HashMap<String, String>,
    paired_udids: Vec<String>,
}

pub struct MuxerDevice {
    // Universal types
    pub connection_type: String,
    pub device_id: u64,
    pub interface_index: u64,
    pub serial_number: String,

    // Network types
    pub network_address: Option<IpAddr>,
    pub heartbeat_handle: Option<UnboundedSender<()>>,
    pub service_name: Option<String>,

    // USB types
    pub connection_speed: Option<u64>,
    pub location_id: Option<u64>,
    pub product_id: Option<u64>,
}

impl SharedDevices {
    pub fn new(plist_storage: Option<String>) -> Self {
        let plist_storage = if let Some(plist_storage) = plist_storage {
            info!("Plist storage specified, ensure the environment is aware");
            plist_storage
        } else {
            info!("Using system default plist storage");
            match std::env::consts::OS {
                "macos" => "/var/db/lockdown",
                "linux" => "/var/lib/lockdown",
                "windows" => "C:/ProgramData/Apple/Lockdown",
                _ => panic!("Unsupported OS, specify a path"),
            }
            .to_string()
        };

        // Make sure the directory exists
        if std::fs::read_dir(&plist_storage).is_err() {
            // Create the directory
            std::fs::create_dir(&plist_storage).expect("Unable to create plist storage folder");
            info!("Created plist storage!");
            error!("You are missing a system configuration file. Run usbmuxd to create one.")
        } else {
            trace!("Plist storage exists");
        }

        Self {
            devices: HashMap::new(),
            last_index: 0,
            last_interface_index: 0,
            plist_storage,
            known_mac_addresses: HashMap::new(),
            paired_udids: Vec::new(),
        }
    }
    pub fn add_network_device(
        &mut self,
        udid: String,
        network_address: IpAddr,
        service_name: String,
        connection_type: String,
        data: Arc<Mutex<Self>>,
    ) {
        if self.devices.contains_key(&udid) {
            trace!("Device has already been added, skipping");
            return;
        }
        self.last_index += 1;
        self.last_interface_index += 1;

        let handle = heartbeat::heartbeat(udid.to_string(), network_address, data);

        let dev = MuxerDevice {
            connection_type,
            device_id: self.last_index,
            service_name: Some(service_name),
            interface_index: self.last_interface_index,
            network_address: Some(network_address),
            serial_number: udid.clone(),
            heartbeat_handle: Some(handle),
            connection_speed: None,
            location_id: None,
            product_id: None,
        };
        info!("Adding device: {:?}", udid);
        self.devices.insert(udid, dev);
    }

    #[cfg(feature = "usb")]
    pub fn add_usb_device(&mut self, udid: String, _data: Arc<Mutex<Self>>) {
        self.last_index += 1;
        self.last_interface_index += 1;

        let dev = MuxerDevice {
            connection_type: "USB".to_string(),
            device_id: self.last_index,
            service_name: None,
            interface_index: self.last_interface_index,
            network_address: None,
            serial_number: udid,
            heartbeat_handle: None,
            connection_speed: None,
            location_id: None,
            product_id: None,
        };

        info!("Adding device: {:?}", dev.serial_number);
        self.devices.insert(dev.serial_number.clone(), dev);
    }

    pub fn remove_device(&mut self, udid: String) {
        if !self.devices.contains_key(&udid) {
            warn!("Device isn't in the muxer, skipping");
            return;
        }
        info!("Removing device: {:?}", udid);
        let _ = &self
            .devices
            .get(&udid)
            .unwrap()
            .heartbeat_handle
            .as_ref()
            .unwrap()
            .send(())
            .unwrap();
        self.devices.remove(&udid);
    }
    pub fn get_pairing_record(&self, udid: String) -> Result<Vec<u8>, ()> {
        let path = PathBuf::from(self.plist_storage.clone()).join(format!("{}.plist", udid));
        if !path.exists() {
            warn!("No pairing record found for device: {:?}", udid);
            return Err(());
        }
        // Read the file
        info!("Reading pairing record for device: {:?}", udid);
        let mut file = std::fs::File::open(path).unwrap();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        Ok(contents)
    }
    pub fn get_buid(&self) -> Result<String, PlistError> {
        let path = PathBuf::from(self.plist_storage.clone()).join("SystemConfiguration.plist");
        if !path.exists() {
            error!("No SystemConfiguration.plist found!");
            return Err(PlistError::Unknown);
        }
        // Read the file to a string
        info!("Reading SystemConfiguration.plist");
        let mut file = std::fs::File::open(path).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();

        // Parse the string into a plist
        info!("Parsing SystemConfiguration.plist");
        let plist = Plist::from_xml(contents).unwrap();
        let buid = plist.dict_get_item("SystemBUID")?.get_string_val()?;
        Ok(buid)
    }

    pub fn update_cache(&mut self) {
        // Iterate through all files in the plist storage, loading them into memory
        trace!("Updating plist cache");
        let path = PathBuf::from(self.plist_storage.clone());
        for entry in std::fs::read_dir(path).expect("Plist storage is unreadable!!") {
            let entry = entry.unwrap();
            let path = entry.path();
            trace!("Attempting to read {:?}", path);
            if path.is_file() {
                let mut file = std::fs::File::open(&path).unwrap();
                let mut contents = String::new();
                let plist = match file.read_to_string(&mut contents) {
                    Ok(_) => Plist::from_xml(contents).unwrap(),
                    Err(e) => {
                        warn!("Error reading file: {:?}", e);
                        let mut buf = vec![];
                        file.read_to_end(&mut buf).unwrap();
                        match Plist::from_memory(buf) {
                            Ok(plist) => plist,
                            Err(_) => {
                                trace!("Could not read plist to memory");
                                continue;
                            }
                        }
                    }
                };
                let mac_addr = match plist.clone().dict_get_item("WiFiMACAddress") {
                    Ok(item) => match item.get_string_val() {
                        Ok(val) => val,
                        Err(_) => {
                            warn!("Could not get string value of WiFiMACAddress");
                            continue;
                        }
                    },
                    Err(_) => {
                        warn!("Plist did not contain WiFiMACAddress");
                        continue;
                    }
                };
                let udid = match plist.clone().dict_get_item("UDID") {
                    Ok(item) => match item.get_string_val() {
                        Ok(val) => Some(val),
                        Err(_) => {
                            warn!("Could not get string value of UDID");
                            None
                        }
                    },
                    Err(_) => {
                        warn!("Plist did not contain UDID");
                        None
                    }
                };

                let udid = if let Some(udid) = udid {
                    udid
                } else {
                    // Use the file name as the UDID
                    // This won't be reached because the SystemConfiguration doesn't have a WiFiMACAddress
                    // This is just used as a last resort, but might not be correct so we'll pass a warning
                    warn!("Using the file name as the UDID");
                    match path.file_name() {
                        Some(f) => {
                            f.to_str().unwrap().split('.').collect::<Vec<&str>>()[0].to_string()
                        }
                        None => {
                            trace!("File had no name");
                            continue;
                        }
                    }
                };

                self.known_mac_addresses.insert(
                    mac_addr,
                    path.file_stem().unwrap().to_string_lossy().to_string(),
                );
                if self.paired_udids.contains(&udid) {
                    trace!("Cache already contained this UDID");
                    continue;
                }
                trace!("Adding {} to plist cache", udid);
                self.paired_udids.push(udid);
            }
        }
    }

    pub fn get_udid_from_mac(&mut self, mac: String) -> Result<String, ()> {
        info!("Getting UDID for MAC: {:?}", mac);
        if let Some(udid) = self.known_mac_addresses.get(&mac) {
            info!("Found UDID: {:?}", udid);
            return Ok(udid.to_string());
        } else {
            trace!("No UDID found for {:?} in cache, re-caching...", mac);
        }
        self.update_cache();

        if let Some(udid) = self.known_mac_addresses.get(&mac) {
            info!("Found UDID: {:?}", udid);
            return Ok(udid.to_string());
        }
        trace!("No UDID found after a re-cache");
        Err(())
    }

    #[cfg(feature = "usb")]
    pub fn check_udid(&mut self, udid: String) -> bool {
        if self.paired_udids.contains(&udid) {
            return true;
        }
        self.update_cache();
        self.paired_udids.contains(&udid)
    }
}

impl TryFrom<&MuxerDevice> for Plist {
    type Error = PlistError;

    fn try_from(device: &MuxerDevice) -> Result<Self, Self::Error> {
        let mut p = Plist::new_dict();
        p.dict_set_item("ConnectionType", device.connection_type.clone().into())?;
        p.dict_set_item("DeviceID", device.device_id.into())?;
        if device.connection_type == "Network" {
            p.dict_set_item(
                "EscapedFullServiceName",
                device.service_name.clone().unwrap().into(),
            )?;
        }
        p.dict_set_item("InterfaceIndex", device.interface_index.into())?;

        // Reassemble the network address back into bytes
        if device.connection_type == "Network" {
            let mut data = [0u8; 152];
            match device.network_address.unwrap() {
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
        }

        p.dict_set_item("SerialNumber", device.serial_number.clone().into())?;
        Ok(p)
    }
}
