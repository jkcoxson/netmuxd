//! `IdeviceProvider` adapter for the `UsbMuxHandle`.
//!
//! The provider works on both native and `wasm32-unknown-unknown`: on wasm
//! the underlying mux is driven by WebUSB through the user's nusb fork; on
//! native it can be driven by nusb's bulk endpoints directly.

use std::{future::Future, pin::Pin};

use idevice::{Idevice, IdeviceError, pairing_file::PairingFile, provider::IdeviceProvider};

use crate::usb::mux::UsbMuxHandle;

/// Provider that opens device connections through a [`UsbMuxHandle`] and
/// hands out a cached pairing file.
#[derive(Clone, Debug)]
pub struct UsbMuxProvider {
    mux: UsbMuxHandle,
    pairing_file: PairingFile,
    label: String,
}

impl UsbMuxProvider {
    pub fn new(mux: UsbMuxHandle, pairing_file: PairingFile, label: impl Into<String>) -> Self {
        Self {
            mux,
            pairing_file,
            label: label.into(),
        }
    }
}

impl IdeviceProvider for UsbMuxProvider {
    fn connect(
        &self,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = Result<Idevice, IdeviceError>> + Send>> {
        let mux = self.mux.clone();
        let label = self.label.clone();
        Box::pin(async move {
            let stream = mux.connect(port).await.map_err(IdeviceError::Socket)?;
            Ok(Idevice::new(Box::new(stream), label))
        })
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn get_pairing_file(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<PairingFile, IdeviceError>> + Send>> {
        let pairing_file = self.pairing_file.clone();
        Box::pin(async move { Ok(pairing_file) })
    }
}
