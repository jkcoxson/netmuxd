// jkcoxson
// Test file for reverse engineering Apple's usbmuxd

use plist_plus::Plist;
use rusty_libimobiledevice::idevice;
use tokio::io::AsyncWriteExt;

mod raw_packet;

#[tokio::main]
async fn main() {
    // Get a list of devices from the muxer
    let device = idevice::get_device("00008101-001E30590C08001E".to_string()).unwrap();
    println!("{:?}", device.get_ip_address());
}
