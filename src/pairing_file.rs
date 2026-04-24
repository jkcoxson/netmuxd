// Jackson Coxson

use std::{collections::HashMap, path::PathBuf};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use idevice::{pairing_file::PairingFile, IdeviceError};
use log::{debug, info, trace, warn};
use sha2::{Sha256, Sha512};
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
            .filter_map(|t| decode_auth_tag(t))
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
            let hk = Hkdf::<Sha512>::new(None, host_id);
            let mut key = [0u8; 32];
            if hk.expand(&[], &mut key).is_err() {
                continue;
            }
            let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key).ok()?;
            mac.update(identifier);
            let tag = mac.finalize().into_bytes();
            let expected = &tag[..8];
            if decoded_tags.iter().any(|d| d == expected) {
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
                    _ => {
                        debug!("Plist did not contain UDID");
                        None
                    }
                };

                let udid = if let Some(udid) = udid {
                    udid
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
                    self.host_ids.insert(stem.clone(), host_id.as_bytes().to_vec());
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

/// Decode an `authTag` TXT value to its 8-byte form.
///
/// Bonjour TXT values are raw bytes; the `authTag` entries carry base64-encoded
/// 8-byte HMAC truncations. MobileDevice trims ASCII whitespace before decoding
/// (see `_EVP_DecodeBlock` site in `AMDIsTXTRecordForUDID`). Anything that
/// doesn't decode to exactly 8 bytes is rejected.
fn decode_auth_tag(raw: &[u8]) -> Option<[u8; 8]> {
    let trimmed = raw
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map(|start| {
            let end = raw
                .iter()
                .rposition(|b| !b.is_ascii_whitespace())
                .map(|i| i + 1)
                .unwrap_or(raw.len());
            &raw[start..end]
        })
        .unwrap_or(&[][..]);
    let decoded = B64.decode(trimmed).ok()?;
    decoded.as_slice().try_into().ok()
}
