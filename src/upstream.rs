// Jackson Coxson
//
// Forwarding to an upstream usbmuxd when running in shim mode. The shim adds
// network devices of its own but defers USB devices and most request/response
// traffic to the real muxer pointed at by `--upstream-usbmuxd`.

use idevice::{
    ReadWrite,
    usbmuxd::{RawPacket, UsbmuxdAddr, server::UsbmuxdServerResponse},
};
use std::{collections::BTreeSet, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Open a fresh connection to the upstream muxer.
pub async fn connect(addr: &UsbmuxdAddr) -> Result<Box<dyn ReadWrite>, String> {
    tokio::time::timeout(REQUEST_TIMEOUT, addr.to_socket())
        .await
        .map_err(|_| "connect to upstream usbmuxd timed out".to_owned())?
        .map_err(|e| format!("connect to upstream usbmuxd: {e:?}"))
}

/// Read one usbmuxd frame (16-byte header + body) as raw bytes.
pub async fn read_frame<R: AsyncRead + Unpin + ?Sized>(sock: &mut R) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; 16];
    sock.read_exact(&mut header).await?;
    let size = u32::from_le_bytes(header[..4].try_into().expect("16-byte header")) as usize;
    if !(16..=MAX_FRAME_SIZE).contains(&size) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "upstream frame size outside 16 bytes..16 MiB",
        ));
    }
    let mut buf = vec![0u8; size];
    buf[..16].copy_from_slice(&header);
    sock.read_exact(&mut buf[16..]).await?;
    Ok(buf)
}

/// Forward a single request to upstream and return its single response frame.
///
/// The frame is returned rather than written to the client so the caller can
/// fall back to handling the request locally if upstream is unreachable. Used
/// for the request/response messages the shim doesn't special-case (ReadBUID,
/// ReadPairRecord, SavePairRecord, and any unknown type).
pub async fn forward_to_upstream(addr: &UsbmuxdAddr, request: &[u8]) -> Result<Vec<u8>, String> {
    exchange(addr, request, REQUEST_TIMEOUT).await
}

async fn exchange(
    addr: &UsbmuxdAddr,
    request: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    if request.len() < 16 {
        return Err("request smaller than its header".into());
    }
    tokio::time::timeout(timeout, async {
        let mut up = connect(addr).await?;
        up.write_all(request)
            .await
            .map_err(|e| format!("write to upstream: {e:?}"))?;
        let response = read_frame(&mut *up)
            .await
            .map_err(|e| format!("read from upstream: {e:?}"))?;
        if response[12..16] != request[12..16] {
            return Err("upstream response tag mismatch".into());
        }
        Ok(response)
    })
    .await
    .map_err(|_| "upstream request timed out".to_owned())?
}

/// A Listen stream can remain idle indefinitely; only its handshake is timed.
pub async fn listen(addr: &UsbmuxdAddr) -> Result<Box<dyn ReadWrite>, String> {
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        let mut up = connect(addr).await?;
        let mut dictionary = plist::Dictionary::new();
        dictionary.insert("MessageType".into(), "Listen".into());
        let request: Vec<u8> = RawPacket::new(dictionary, 1, 8, 0).into();
        up.write_all(&request)
            .await
            .map_err(|e| format!("write Listen: {e}"))?;
        let response = read_frame(&mut *up)
            .await
            .map_err(|e| format!("read Listen: {e}"))?;
        let packet =
            RawPacket::try_from(response.as_slice()).map_err(|_| "invalid Listen response")?;
        if packet.tag != 0
            || packet
                .plist
                .get("MessageType")
                .and_then(plist::Value::as_string)
                != Some("Result")
            || packet
                .plist
                .get("Number")
                .and_then(plist::Value::as_unsigned_integer)
                != Some(0)
        {
            return Err("upstream rejected Listen".to_owned());
        }
        Ok(up)
    })
    .await
    .map_err(|_| "upstream Listen handshake timed out".to_owned())?
}

/// Bound the per-client event backlog. On an upstream restart, explicitly
/// detach every previously advertised device before accepting its new IDs.
pub fn spawn_listener(addr: UsbmuxdAddr) -> (mpsc::Receiver<Vec<u8>>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        let mut backoff = Duration::from_millis(250);
        loop {
            let mut attached = BTreeSet::new();
            match listen(&addr).await {
                Ok(mut stream) => {
                    backoff = Duration::from_millis(250);
                    while let Ok(frame) = read_frame(&mut *stream).await {
                        let Ok(packet) = RawPacket::try_from(frame.as_slice()) else {
                            break;
                        };
                        let Some(id) = packet
                            .plist
                            .get("DeviceID")
                            .and_then(plist::Value::as_unsigned_integer)
                            .and_then(|id| u32::try_from(id).ok())
                        else {
                            break;
                        };
                        match packet
                            .plist
                            .get("MessageType")
                            .and_then(plist::Value::as_string)
                        {
                            Some("Attached") => {
                                attached.insert(id);
                            }
                            Some("Detached") => {
                                attached.remove(&id);
                            }
                            _ => break,
                        }
                        if tx.send(frame).await.is_err() {
                            return;
                        }
                    }
                }
                Err(error) => log::warn!("Upstream Listen failed: {error}"),
            }
            for id in attached {
                let frame = UsbmuxdServerResponse::Detached(id.into())
                    .into_packet(0)
                    .into();
                if tx.send(frame).await.is_err() {
                    return;
                }
            }
            tokio::select! {
                _ = tx.closed() => return,
                _ = tokio::time::sleep(backoff) => {},
            }
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    });
    (rx, task)
}

/// Forward the client's verbatim `ListDevices` request to upstream, then return
/// a response whose `DeviceList` is the upstream list with `network_devices`
/// appended. Preserves every property upstream reports for its USB devices.
pub async fn list_devices_merged(
    addr: &UsbmuxdAddr,
    request: &[u8],
    network_devices: Vec<plist::Value>,
    tag: u32,
) -> Result<Vec<u8>, String> {
    let frame = forward_to_upstream(addr, request).await?;

    let parsed = RawPacket::try_from(frame.as_slice())
        .map_err(|_| "could not parse upstream ListDevices response".to_string())?;
    let mut list = match parsed.plist.get("DeviceList") {
        Some(plist::Value::Array(a)) => a.clone(),
        _ => return Err("upstream ListDevices response had no DeviceList array".into()),
    };
    list.extend(network_devices);

    Ok(UsbmuxdServerResponse::DeviceList(list)
        .into_packet(tag)
        .into())
}
