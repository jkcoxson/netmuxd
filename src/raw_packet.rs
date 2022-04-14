// jkcoxson

use plist_plus::Plist;

#[derive(Debug)]
pub struct RawPacket {
    pub size: u32,
    pub version: u32,
    pub message: u32,
    pub tag: u32,
    pub plist: Plist,
}

impl RawPacket {
    pub fn new(plist: Plist, version: u32, message: u32, tag: u32) -> RawPacket {
        let plist_bytes = plist.to_string();
        let plist_bytes = plist_bytes.as_bytes();
        let size = plist_bytes.len() as u32 + 16;
        RawPacket {
            size,
            version,
            message,
            tag,
            plist,
        }
    }
}

impl From<RawPacket> for Vec<u8> {
    fn from(raw_packet: RawPacket) -> Vec<u8> {
        let mut packet = vec![];
        packet.extend_from_slice(&raw_packet.size.to_le_bytes());
        packet.extend_from_slice(&raw_packet.version.to_le_bytes());
        packet.extend_from_slice(&raw_packet.message.to_le_bytes());
        packet.extend_from_slice(&raw_packet.tag.to_le_bytes());
        packet.extend_from_slice(raw_packet.plist.to_string().as_bytes());
        packet
    }
}

impl From<Vec<u8>> for RawPacket {
    fn from(packet: Vec<u8>) -> Self {
        let packet: &[u8] = &packet;
        packet.into()
    }
}

impl From<&[u8]> for RawPacket {
    fn from(packet: &[u8]) -> Self {
        let packet_size = &packet[0..4];
        let packet_size = u32::from_le_bytes(packet_size.try_into().unwrap());

        let packet_version = &packet[4..8];
        let packet_version = u32::from_le_bytes(packet_version.try_into().unwrap());

        let message = &packet[8..12];
        let message = u32::from_le_bytes(message.try_into().unwrap());

        let packet_tag = &packet[12..16];
        let packet_tag = u32::from_le_bytes(packet_tag.try_into().unwrap());

        let plist = &packet[16..packet_size as usize];

        let plist = Plist::from_xml(String::from_utf8_lossy(&plist).to_string()).unwrap();

        RawPacket {
            size: packet_size,
            version: packet_version,
            message: message,
            tag: packet_tag,
            plist: plist,
        }
    }
}
