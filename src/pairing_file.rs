// Jackson Coxson

use std::{collections::HashMap, path::PathBuf};

use idevice::{IdeviceError, pairing_file::PairingFile};
use log::{debug, info, trace, warn};
use tokio::io::AsyncReadExt;

use crate::config::NetmuxdConfig;

#[derive(Clone, Debug)]
pub struct PairingFileFinder {
    plist_storage: String,
    // Legacy MAC-based lookup (iOS < 26.4)
    known_mac_addresses: HashMap<String, String>,
    // TXT-based lookup
    host_ids: HashMap<String, Vec<u8>>,
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
            host_ids: HashMap::new(),
            paired_udids: Vec::new(),
        }
    }

    pub fn plist_storage(&self) -> &str {
        &self.plist_storage
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

    /// iOS 26.4+ lookup: match a Bonjour TXT record against a paired device.
    ///
    /// Algorithm (mirrors MobileDevice's `AMDIsTXTRecordForUDID`):
    ///   K        = HKDF-SHA512(ikm = HostID_utf8, salt = "", info = "", L = 32)
    ///   expected = HMAC-SHA256(K, identifier)[0..8]
    ///   match    = any auth_tag whose base64-decoded first 8 bytes equal `expected`.
    ///
    /// `identifier` is the raw TXT value (CFData in the original, bytes here).
    /// `auth_tags` are the raw TXT values for `authTag`, `authTag#0`, `authTag#1`, etc
    /// base64-encoded 8-byte tags; this function decodes them.
    pub async fn find_udid_from_txt(
        &mut self,
        identifier: &[u8],
        auth_tags: &[&[u8]],
    ) -> Option<String> {
        if auth_tags.is_empty() {
            return None;
        }
        // Decode all tags up front (they're independent of the candidate HostID).
        let decoded_tags: Vec<[u8; 8]> = auth_tags
            .iter()
            .filter_map(|t| idevice::mdns::decode_auth_tag(t))
            .collect();
        if decoded_tags.is_empty() {
            debug!("TXT record had authTag(s) but none decoded to 8 bytes");
            return None;
        }

        if let Some(udid) = self.match_txt(identifier, &decoded_tags) {
            return Some(udid);
        }
        trace!("No UDID matched TXT record in cache, re-caching...");
        self.update_cache().await;
        self.match_txt(identifier, &decoded_tags)
    }

    fn match_txt(&self, identifier: &[u8], decoded_tags: &[[u8; 8]]) -> Option<String> {
        for (udid, host_id) in &self.host_ids {
            let expected = idevice::mdns::derive_auth_tag(host_id, identifier);
            if decoded_tags.contains(&expected) {
                info!("TXT record matched UDID {}", udid);
                return Some(udid.clone());
            }
        }
        None
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
                let udid = match plist.get("UDID") {
                    Some(plist::Value::String(u)) => Some(u.clone()),
                    _ => None,
                };

                let udid = if let Some(udid) = udid {
                    udid
                } else {
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
                    Some(s) => s.to_string_lossy().to_string(),
                    None => {
                        warn!("Failed to get file stem for entry");
                        continue;
                    }
                };

                // Legacy: index by WiFiMACAddress if present (iOS < 26.4 devices).
                if let Some(plist::Value::String(mac)) = plist.get("WiFiMACAddress") {
                    self.known_mac_addresses.insert(mac.clone(), stem.clone());
                }

                // iOS 26.4+: index by HostID for TXT-based matching.
                if let Some(plist::Value::String(host_id)) = plist.get("HostID") {
                    self.host_ids
                        .insert(stem.clone(), host_id.as_bytes().to_vec());
                } else {
                    debug!("Plist {stem:?} has no HostID; TXT-based lookup will skip it");
                }
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

    pub async fn remove_pairing_record(&self, udid: &str) -> std::io::Result<()> {
        let path = PathBuf::from(self.plist_storage.clone()).join(format!("{udid}.plist"));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn get_buid(&self) -> Result<String, std::io::Error> {
        let (_, buid) = self.get_host_identity().await?;
        Ok(buid)
    }

    /// Returns the local (HostID, SystemBUID) used when pairing with
    /// new devices. Reads `SystemConfiguration.plist` from
    /// `plist_storage` and lazily creates either field if missing,
    /// writing the file back so other muxers see the same identity.
    pub async fn get_host_identity(&self) -> Result<(String, String), std::io::Error> {
        let path = PathBuf::from(self.plist_storage.clone()).join("SystemConfiguration.plist");

        let mut plist = if path.exists() {
            let mut file = tokio::fs::File::open(&path).await?;
            let mut contents = Vec::new();
            file.read_to_end(&mut contents).await?;
            plist::from_bytes::<plist::Dictionary>(&contents).unwrap_or_else(|e| {
                warn!("Failed to parse SystemConfiguration.plist ({e:?}), regenerating");
                plist::Dictionary::new()
            })
        } else {
            info!("No SystemConfiguration.plist found, generating one");
            warn!(
                "The SystemConfiguration.plist generated by netmuxd is incomplete for other muxers. Delete if using usbmuxd or another muxer."
            );
            plist::Dictionary::new()
        };

        let mut dirty = false;

        let host_id = match plist.get("HostID").and_then(|v| v.as_string()) {
            Some(s) => s.to_string(),
            None => {
                let new_id = uuid::Uuid::new_v4().to_string().to_uppercase();
                plist.insert("HostID".into(), new_id.clone().into());
                dirty = true;
                new_id
            }
        };

        let system_buid = match plist.get("SystemBUID").and_then(|v| v.as_string()) {
            Some(s) => s.to_string(),
            None => {
                let new_id = uuid::Uuid::new_v4().to_string().to_uppercase();
                plist.insert("SystemBUID".into(), new_id.clone().into());
                dirty = true;
                new_id
            }
        };

        if dirty {
            debug!("Persisting SystemConfiguration.plist with new identity field(s)");
            // Best-effort write; if it fails the caller still gets
            // the in-memory identity for this session.
            if let Some(parent) = path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let mut buf = Vec::new();
            if let Err(e) = plist::to_writer_xml(&mut buf, &plist) {
                warn!("Failed to serialize SystemConfiguration.plist: {e:?}");
            } else if let Err(e) = tokio::fs::write(&path, &buf).await {
                warn!("Failed to write SystemConfiguration.plist: {e:?}");
            }
        }

        Ok((host_id, system_buid))
    }
}
