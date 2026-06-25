// Jackson Coxson
//
// Forwarding to an upstream usbmuxd when running in shim mode. The shim adds
// network devices of its own but defers USB devices and most request/response
// traffic to the real muxer pointed at by `--upstream-usbmuxd`.

use idevice::{
    ReadWrite,
    usbmuxd::{RawPacket, UsbmuxdAddr, server::UsbmuxdServerResponse},
};
use log::warn;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Open a fresh connection to the upstream muxer.
pub async fn connect(addr: &UsbmuxdAddr) -> Result<Box<dyn ReadWrite>, String> {
    addr.to_socket()
        .await
        .map_err(|e| format!("connect to upstream usbmuxd: {e:?}"))
}

/// Read one usbmuxd frame (16-byte header + body) as raw bytes.
pub async fn read_frame<R: AsyncRead + Unpin + ?Sized>(sock: &mut R) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; 16];
    sock.read_exact(&mut header).await?;
    let size = u32::from_le_bytes(header[..4].try_into().expect("16-byte header")) as usize;
    if size < 16 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "upstream frame smaller than its header",
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
    let mut up = connect(addr).await?;
    up.write_all(request)
        .await
        .map_err(|e| format!("write to upstream: {e:?}"))?;
    read_frame(&mut *up)
        .await
        .map_err(|e| format!("read from upstream: {e:?}"))
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
    let mut up = connect(addr).await?;
    up.write_all(request)
        .await
        .map_err(|e| format!("write ListDevices to upstream: {e:?}"))?;
    let frame = read_frame(&mut *up)
        .await
        .map_err(|e| format!("read ListDevices from upstream: {e:?}"))?;

    let parsed = RawPacket::try_from(frame.as_slice())
        .map_err(|_| "could not parse upstream ListDevices response".to_string())?;
    let mut list = match parsed.plist.get("DeviceList") {
        Some(plist::Value::Array(a)) => a.clone(),
        _ => {
            warn!("upstream ListDevices response had no DeviceList array");
            Vec::new()
        }
    };
    list.extend(network_devices);

    Ok(UsbmuxdServerResponse::DeviceList(list)
        .into_packet(tag)
        .into())
}
