// jkcoxson

use log::{error, warn};
use rusb::UsbContext;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::devices::SharedDevices;

const APPLE_VENDOR_ID: u16 = 0x05ac;

pub fn start_listener(data: Arc<Mutex<SharedDevices>>) {
    let context = rusb::Context::new().unwrap();
    let reg: Result<rusb::Registration<rusb::Context>, rusb::Error> = rusb::HotplugBuilder::new()
        .enumerate(true)
        .register(&context, Box::new(Handler { data }));

    tokio::task::spawn_blocking(move || {
        let _reg = Some(reg.unwrap());
        loop {
            match context.handle_events(None) {
                Ok(_) => {}
                Err(e) => {
                    error!("Error handling USB events: {:?}", e);
                    break;
                }
            }
        }
    });
}

struct Handler {
    #[allow(dead_code)]
    data: Arc<Mutex<SharedDevices>>,
}

impl<T: UsbContext> rusb::Hotplug<T> for Handler {
    fn device_arrived(&mut self, device: rusb::Device<T>) {
        let desc = device.device_descriptor().unwrap();
        if desc.vendor_id() == APPLE_VENDOR_ID {
            println!("iDevice plugged in!");
            let handle = device.open().unwrap();

            // Get the device's serial number
            let langs = handle
                .read_languages(std::time::Duration::from_secs(3))
                .unwrap();
            let serial_number = handle
                .read_serial_number_string(langs[0], &desc, std::time::Duration::from_secs(1))
                .unwrap();

            let serial_number = match serial_number.len() {
                0x28 => {
                    let mut s = serial_number[..8].to_string();
                    s.push('-');
                    s.push_str(&serial_number[8..]);
                    s
                }
                _ => {
                    warn!("Serial number is unexpected length: {}", serial_number);
                    return;
                }
            };

            println!("Serial number: {}", serial_number);
            let serial_number = serial_number.trim().to_string();
            let serial_number = serial_number.replace('\0', "");

            // Determine if the device is paired
            let data = self.data.clone();
            tokio::spawn(async move {
                let cloned_data = data.clone();
                let mut d = cloned_data.lock().await;
                if d.check_udid(serial_number.clone()) {
                    println!("Device is paired!");
                    d.add_usb_device(serial_number, data);
                } else {
                    todo!()
                    // Try and pair the device
                }
            });
        }
    }

    fn device_left(&mut self, _: rusb::Device<T>) {
        println!("Device removed");

        // Remove device by serial number
    }
}
