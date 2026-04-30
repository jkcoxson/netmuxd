// Jackson Coxson

use crate::manager::ManagerRequest;
use crate::pairing_file::PairingFileFinder;
use crate::{config::NetmuxdConfig, manager::ManagerSender};
use log::{debug, warn};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::net::IpAddr;

const SERVICE_NAME: &str = "apple-mobdev2";
const SERVICE_PROTOCOL: &str = "tcp";

pub async fn discover(sender: ManagerSender, config: NetmuxdConfig) {
    // mdns-sd expects the fully-qualified service type with a trailing '.';
    // downstream consumers expect the form without it.
    let browse_type = format!("_{}._{}.local.", SERVICE_NAME, SERVICE_PROTOCOL);
    let service_name = format!("_{}._{}.local", SERVICE_NAME, SERVICE_PROTOCOL);
    log::info!("Starting mDNS discovery for {browse_type} with mdns-sd");

    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            log::error!("Failed to create mDNS daemon: {e}");
            return;
        }
    };
    let receiver = match daemon.browse(&browse_type) {
        Ok(r) => r,
        Err(e) => {
            log::error!("Failed to start mDNS browse: {e}");
            return;
        }
    };

    let mut pairing_file_finder = PairingFileFinder::new(&config);

    while let Ok(event) = receiver.recv_async().await {
        let resolved = match event {
            ServiceEvent::ServiceResolved(info) => info,
            _ => continue,
        };
        debug!(
            "Resolved service: fullname={} addrs={:?}",
            resolved.fullname, resolved.addresses
        );

        let addr = match pick_address(&resolved) {
            Some(a) => a,
            None => {
                warn!(
                    "Resolved mDNS service has no usable address: {}",
                    resolved.fullname
                );
                continue;
            }
        };

        // iOS 26.4+: match by Bonjour TXT record (identifier + authTag HMACs).
        let identifier = resolved
            .get_property_val("identifier")
            .and_then(|v| v)
            .map(|b| b.to_vec());
        let auth_tags: Vec<Vec<u8>> = resolved
            .get_properties()
            .iter()
            .filter(|p| {
                let k = p.key();
                k == "authTag" || k.starts_with("authTag#")
            })
            .filter_map(|p| p.val().map(|b| b.to_vec()))
            .collect();

        let mut udid: Option<String> = None;
        if let Some(ident) = &identifier
            && !auth_tags.is_empty()
        {
            let refs: Vec<&[u8]> = auth_tags.iter().map(|v| v.as_slice()).collect();
            udid = pairing_file_finder.find_udid_from_txt(ident, &refs).await;
        }

        // iOS < 26.4 fallback: parse MAC out of the instance name (`<MAC>@<id>.…`).
        if udid.is_none()
            && let Some((mac_addr, _)) = resolved.fullname.split_once('@')
            && let Ok(u) = pairing_file_finder
                .get_udid_from_mac(mac_addr.to_string())
                .await
        {
            udid = Some(u);
        }

        let udid = match udid {
            Some(u) => u,
            None => {
                debug!(
                    "No paired device matched service {} (identifier={}, authTags={})",
                    resolved.fullname,
                    identifier.is_some(),
                    auth_tags.len()
                );
                continue;
            }
        };

        if sender
            .send(ManagerRequest::discovered_device(
                udid,
                addr,
                service_name.clone(),
                "Network".to_string(),
            ))
            .await
            .is_err()
        {
            debug!("Failed to send discovered device to manager, closing");
            break;
        }
    }
}

fn pick_address(resolved: &mdns_sd::ResolvedService) -> Option<IpAddr> {
    // Prefer IPv4 to preserve the existing behaviour; fall back to any address.
    resolved
        .addresses
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| resolved.addresses.iter().next())
        .map(|a| a.to_ip_addr())
}
