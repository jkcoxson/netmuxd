// jkcoxson

use std::{collections::HashMap, io::Read, net::IpAddr, path::PathBuf, sync::Arc};

use log::{debug, info, trace, warn};
use tokio::{
    io::AsyncReadExt,
    sync::{oneshot::Sender, Mutex},
};

use crate::heartbeat;

pub struct SharedDevices {
    pub devices: HashMap<String, MuxerDevice>,
    pub last_index: u64,
    pub last_interface_index: u64,
    plist_storage: String,
    use_heartbeat: bool,
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
    pub heartbeat_handle: Option<Sender<()>>,
    pub service_name: Option<String>,

    // USB types
    pub connection_speed: Option<u64>,
    pub location_id: Option<u64>,
    pub product_id: Option<u64>,
}

impl SharedDevices {
    pub async fn new(plist_storage: Option<String>, use_heartbeat: bool) -> Self {
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
        if tokio::fs::read_dir(&plist_storage).await.is_err() {
            // Create the directory
            tokio::fs::create_dir(&plist_storage)
                .await
                .expect("Unable to create plist storage folder");
            info!("Created plist storage!");
        } else {
            trace!("Plist storage exists");
        }

        Self {
            devices: HashMap::new(),
            last_index: 0,
            last_interface_index: 0,
            plist_storage,
            use_heartbeat,
            known_mac_addresses: HashMap::new(),
            paired_udids: Vec::new(),
        }
    }
    pub async fn add_network_device(
        &mut self,
        udid: String,
        network_address: IpAddr,
        service_name: String,
        connection_type: String,
        data: Arc<Mutex<Self>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.devices.contains_key(&udid) {
            trace!("Device has already been added, skipping");
            return Ok(());
        }
        self.last_index += 1;
        self.last_interface_index += 1;
        let pairing_file = self.get_pairing_record(udid.clone()).await?;
        let pairing_file = idevice::pairing_file::PairingFile::from_bytes(&pairing_file)?;

        let handle = if self.use_heartbeat {
            Some(heartbeat::heartbeat(network_address, udid.clone(), pairing_file, data).await?)
        } else {
            None
        };

        let dev = MuxerDevice {
            connection_type,
            device_id: self.last_index,
            service_name: Some(service_name),
            interface_index: self.last_interface_index,
            network_address: Some(network_address),
            serial_number: udid.clone(),
            heartbeat_handle: handle,
            connection_speed: None,
            location_id: None,
            product_id: None,
        };
        info!("Adding device: {:?}", udid);
        self.devices.insert(udid, dev);
        Ok(())
    }

    pub fn get_device_by_id(&self, id: u64) -> Option<&MuxerDevice> {
        self.devices.values().find(|x| x.device_id == id)
    }

    pub fn remove_device(&mut self, udid: &String) {
        if !self.devices.contains_key(udid) {
            warn!("Device isn't in the muxer, skipping");
            return;
        }
        info!("Removing device: {:?}", udid);
        match self.devices.remove(udid) {
            Some(d) => match d.heartbeat_handle {
                Some(h) => {
                    if let Err(e) = h.send(()) {
                        warn!("Couldn't send kill signal to heartbeat thread: {e:?}");
                    }
                }
                None => {
                    warn!("Heartbeat handle option has none, can't kill");
                }
            },
            None => {
                warn!("No heartbeat handle found for device");
            }
        };
    }
    pub async fn get_pairing_record(&self, udid: String) -> Result<Vec<u8>, std::io::Error> {
        let path = PathBuf::from(self.plist_storage.clone()).join(format!("{}.plist", udid));
        info!("Attempting to read pairing file: {path:?}");
        if !path.exists() {
            warn!("No pairing record found for device: {:?}", udid);
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No pairing record for the device",
            ));
        }
        // Read the file
        info!("Reading pairing record for device: {:?}", udid);
        let mut file = tokio::fs::File::open(path).await?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).await?;
        Ok(contents)
    }
    pub async fn get_buid(&self) -> Result<String, std::io::Error> {
        let path = PathBuf::from(self.plist_storage.clone()).join("SystemConfiguration.plist");
        if !path.exists() {
            info!("No SystemConfiguration.plist found, generating BUID");
            warn!("The SystemConfiguration.plist generated by netmuxd is incomplete for other muxers. Delete if using usbmuxd or another muxer.");
            let mut new_plist = plist::Dictionary::new();
            let new_udid = uuid::Uuid::new_v4();
            new_plist.insert("SystemBUID".into(), new_udid.to_string().into());
            let f = std::fs::File::create_new(&path)?;
            plist::to_writer_xml(f, &new_plist).unwrap();
        }
        // Read the file to a string
        debug!("Reading SystemConfiguration.plist");
        let mut file = std::fs::File::open(path)?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)?;

        // Parse the string into a plist
        debug!("Parsing SystemConfiguration.plist");
        let plist = match plist::from_bytes::<plist::Dictionary>(&contents) {
            Ok(p) => p,
            Err(e) => {
                log::error!("Failed to parse plist: {e:?}");
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "unable to parse plist",
                ));
            }
        };
        match plist.get("SystemBUID") {
            Some(plist::Value::String(b)) => Ok(b.to_owned()),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Plist did not contain SystemBUID",
            )),
        }
    }

    pub async fn update_cache(&mut self) {
        // Iterate through all files in the plist storage, loading them into memory
        trace!("Updating plist cache");
        let path = PathBuf::from(self.plist_storage.clone());
        for entry in std::fs::read_dir(path).expect("Plist storage is unreadable!!") {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!("Unable to read entry in plist storage: {e:?}");
                    continue;
                }
            };
            let path = entry.path();
            trace!("Attempting to read {:?}", path);
            if path.is_file() {
                let mut file = match tokio::fs::File::open(&path).await {
                    Ok(f) => f,
                    Err(e) => {
                        warn!("Unable to read plist storage entry to memory: {e:?}");
                        continue;
                    }
                };
                let mut contents = Vec::new();
                let plist: plist::Dictionary = match file.read_to_end(&mut contents).await {
                    Ok(_) => match plist::from_bytes(&contents) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("Unable to parse entry file to plist: {e:?}");
                            continue;
                        }
                    },
                    Err(e) => {
                        trace!("Could not read plist to memory: {e:?}");
                        continue;
                    }
                };
                let mac_addr = match plist.get("WiFiMACAddress") {
                    Some(plist::Value::String(m)) => m,
                    _ => {
                        warn!("Could not get string value of WiFiMACAddress");
                        continue;
                    }
                };
                let udid = match plist.get("UDID") {
                    Some(plist::Value::String(u)) => Some(u),
                    _ => {
                        warn!("Plist did not contain UDID");
                        None
                    }
                };

                let udid = if let Some(udid) = udid {
                    udid.to_owned()
                } else {
                    // Use the file name as the UDID
                    // This won't be reached because the SystemConfiguration doesn't have a WiFiMACAddress
                    // This is just used as a last resort, but might not be correct so we'll pass a warning
                    warn!("Using the file name as the UDID");
                    match path.file_name() {
                        Some(f) => match f.to_str() {
                            Some(f) => f.split('.').collect::<Vec<&str>>()[0].to_string(),
                            None => {
                                warn!("Failed to get entry file name string");
                                continue;
                            }
                        },
                        None => {
                            trace!("File had no name");
                            continue;
                        }
                    }
                };

                let stem = match path.file_stem() {
                    Some(s) => s,
                    None => {
                        warn!("Failed to get file stem for entry");
                        continue;
                    }
                };

                self.known_mac_addresses
                    .insert(mac_addr.to_owned(), stem.to_string_lossy().to_string());
                if self.paired_udids.contains(&udid) {
                    trace!("Cache already contained this UDID");
                    continue;
                }
                trace!("Adding {} to plist cache", udid);
                self.paired_udids.push(udid);
            }
        }
    }

    pub async fn get_udid_from_mac(&mut self, mac: String) -> Result<String, ()> {
        info!("Getting UDID for MAC: {:?}", mac);
        if let Some(udid) = self.known_mac_addresses.get(&mac) {
            info!("Found UDID: {:?}", udid);
            return Ok(udid.to_string());
        } else {
            trace!("No UDID found for {:?} in cache, re-caching...", mac);
        }
        self.update_cache().await;

        if let Some(udid) = self.known_mac_addresses.get(&mac) {
            info!("Found UDID: {:?}", udid);
            return Ok(udid.to_string());
        }
        trace!("No UDID found after a re-cache");
        Err(())
    }
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
