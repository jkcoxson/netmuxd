// jkcoxson

const APPLE_VENDOR_ID: u32 = 0x05ac;

#[tokio::main]
async fn main() {
    for device in rusb::devices().unwrap().iter() {
        let desc = device.device_descriptor().unwrap();

        println!("{:?}", desc);

        // Get the Device Handle
        let handle = device.open().unwrap();
    }
}
