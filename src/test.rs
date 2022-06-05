// jkcoxson

use rusb::{Device, UsbContext};

const APPLE_VENDOR_ID: u16 = 0x05ac;

#[tokio::main]
async fn main() {
    let context = rusb::Context::new().unwrap();

    // let devices = context.devices().unwrap();
    // for device in devices.iter() {
    //     device.
    // }

    let reg: Result<rusb::Registration<rusb::Context>, rusb::Error> = rusb::HotplugBuilder::new()
        .enumerate(true)
        .register(&context, Box::new(Handler {}));

    let _reg = Some(reg.unwrap());

    loop {
        context.handle_events(None).unwrap();
    }
}

struct Handler;

impl<T: UsbContext> rusb::Hotplug<T> for Handler {
    fn device_arrived(&mut self, device: Device<T>) {
        // println!("Device added: {:?}", device);

        let desc = device.device_descriptor().unwrap();
        if desc.vendor_id() == APPLE_VENDOR_ID {
            println!("iDevice plugged in!");
        }
    }

    fn device_left(&mut self, _: Device<T>) {
        println!("Device removed");
    }
}
