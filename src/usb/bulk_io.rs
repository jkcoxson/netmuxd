//! `tokio::io::AsyncRead` / `AsyncWrite` adapters over `nusb` bulk endpoints.
//!
//! These work on every `nusb` backend (including the WebUSB backend) without
//! depending on `nusb`'s `tokio` feature, which is gated to native targets in
//! upstream `nusb`.
//!
//! Wire the result into [`crate::usb::mux::spawn`].

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use nusb::Endpoint;
use nusb::transfer::{Buffer, In, Out};
pub use nusb::transfer::Bulk;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Default per-IN-transfer buffer size. Apple's mux pipe accepts up to
/// `3 * 16384` bytes per transfer (`USB_MTU`). 16 KiB is a balanced default
/// for short-message protocols like usbmuxd's control plane.
pub const DEFAULT_TRANSFER_SIZE: usize = 16 * 1024;

/// Default cap on a single OUT submission. The mux task already chunks user
/// payloads to `USB_MTU` minus headers, so this only matters as a safety net.
pub const DEFAULT_MAX_OUT: usize = 64 * 1024;

/// Wraps an `Endpoint<Bulk, In>` as `AsyncRead`.
#[derive(Debug)]
pub struct BulkReader {
    ep: Endpoint<Bulk, In>,
    transfer_size: usize,
    in_flight_target: usize,
    /// Currently-being-drained completed buffer, with read offset and end.
    drain: Option<(Buffer, usize, usize)>,
}

impl BulkReader {
    /// Wrap `ep` as an `AsyncRead`. `transfer_size` is rounded up to the
    /// endpoint's max packet size.
    pub fn new(ep: Endpoint<Bulk, In>, transfer_size: usize) -> Self {
        let mp = ep.max_packet_size();
        let transfer_size = transfer_size.div_ceil(mp).max(1) * mp;
        Self {
            ep,
            transfer_size,
            in_flight_target: 1,
            drain: None,
        }
    }

    /// Number of concurrent IN transfers to keep submitted. Higher values
    /// improve streaming throughput. Defaults to 1.
    pub fn with_in_flight(mut self, n: usize) -> Self {
        self.in_flight_target = n.max(1);
        self
    }

    fn submit_until_target(&mut self) {
        while self.ep.pending() < self.in_flight_target {
            self.ep.submit(Buffer::new(self.transfer_size));
        }
    }
}

impl AsyncRead for BulkReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            if let Some((b, off, end)) = &mut this.drain {
                let avail = &b[*off..*end];
                let n = avail.len().min(buf.remaining());
                if n > 0 {
                    buf.put_slice(&avail[..n]);
                    *off += n;
                }
                if *off >= *end {
                    this.drain = None;
                }
                return Poll::Ready(Ok(()));
            }
            this.submit_until_target();
            let comp = ready!(this.ep.poll_next_complete(cx));
            if let Err(e) = comp.status {
                return Poll::Ready(Err(io::Error::other(format!("usb in: {e:?}"))));
            }
            if comp.actual_len == 0 {
                // Empty packet — nothing to deliver, loop and try again.
                continue;
            }
            this.drain = Some((comp.buffer, 0, comp.actual_len));
        }
    }
}

/// Wraps an `Endpoint<Bulk, Out>` as `AsyncWrite`.
///
/// Single transfer in flight at a time. The previous transfer's completion is
/// drained at the start of each `poll_write` call before accepting new bytes.
#[derive(Debug)]
pub struct BulkWriter {
    ep: Endpoint<Bulk, Out>,
    max_out: usize,
    in_flight: bool,
}

impl BulkWriter {
    pub fn new(ep: Endpoint<Bulk, Out>) -> Self {
        Self {
            ep,
            max_out: DEFAULT_MAX_OUT,
            in_flight: false,
        }
    }

    /// Cap on a single OUT submission. Defaults to 64 KiB.
    pub fn with_max_out(mut self, max: usize) -> Self {
        self.max_out = max.max(1);
        self
    }

    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.in_flight {
            return Poll::Ready(Ok(()));
        }
        let comp = ready!(self.ep.poll_next_complete(cx));
        self.in_flight = false;
        match comp.status {
            Ok(()) => Poll::Ready(Ok(())),
            Err(e) => Poll::Ready(Err(io::Error::other(format!("usb out: {e:?}")))),
        }
    }
}

impl AsyncWrite for BulkWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        ready!(self.as_mut().poll_drain(cx))?;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let n = buf.len().min(self.max_out);
        let mut out = Buffer::new(n);
        out.extend_from_slice(&buf[..n]);
        self.ep.submit(out);
        self.in_flight = true;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.as_mut().poll_drain(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.as_mut().poll_drain(cx)
    }
}
