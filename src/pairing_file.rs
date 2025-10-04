// Jackson Coxson

use std::{collections::HashMap, path::PathBuf};

use idevice::{pairing_file::PairingFile, IdeviceError};
use log::{debug, info, trace, warn};
use tokio::io::AsyncReadExt;

use crate::config::NetmuxdConfig;

#[derive(Clone, Debug)]
pub struct PairingFileFinder {
    plist_storage: String,
    known_mac_addresses: HashMap<String, String>,
    paired_udids: Vec<String>,
}

impl PairingFileFinder {
    pub fn new(config: &NetmuxdConfig) -> Self {
        Self {
            plist_storage: config.plist_storage.clone().unwrap_or(
                match std::env::consts::OS {
                    "macos" => "/var/db/lockdown",
                    "linux" => "/var/lib/lockdown",
                    "windows" => "C:/ProgramData/Apple/Lockdown",
                    _ => panic!("Unsupported OS, specify a path"),
                }
                .to_string(),
            ),
            known_mac_addresses: HashMap::new(),
            paired_udids: Vec::new(),
        }
    }

    pub async fn get_udid_from_mac(&mut self, mac: String) -> Result<String, ()> {
        debug!("Getting UDID for MAC: {:?}", mac);
        if let Some(udid) = self.known_mac_addresses.get(&mac) {
            debug!("Found UDID: {:?}", udid);
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
                        debug!("Could not get string value of WiFiMACAddress");
                        continue;
                    }
                };
                let udid = match plist.get("UDID") {
                    Some(plist::Value::String(u)) => Some(u),
                    _ => {
                        debug!("Plist did not contain UDID");
                        None
                    }
                };

                let udid = if let Some(udid) = udid {
                    udid.to_owned()
                } else {
                    debug!("Using the file name as the UDID");
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

    pub async fn get_pairing_record(&self, udid: &String) -> Result<PairingFile, IdeviceError> {
        let path = PathBuf::from(self.plist_storage.clone()).join(format!("{}.plist", udid));
        info!("Attempting to read pairing file: {path:?}");
        if !path.exists() {
            warn!("No pairing record found for device: {:?}", udid);
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No pairing record for the device",
            )
            .into());
        }
        // Read the file
        info!("Reading pairing record for device: {:?}", udid);
        let mut file = tokio::fs::File::open(path).await?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).await?;
        let p = PairingFile::from_bytes(&contents)?;
        Ok(p)
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
        let mut file = tokio::fs::File::open(path).await?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).await?;

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
}
