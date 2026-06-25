// Jackson Coxson
//
// Tokio AsyncRead / AsyncWrite over libusbK bulk pipes. The DLL is
// blocking-only without an OVERLAPPED + IOCP setup, so we move each
// transfer to a `spawn_blocking` thread.
//
// Cancellation: dropping a reader/writer calls `UsbK_AbortPipe`, which
// causes the in-flight blocking call to return with
// `ERROR_OPERATION_ABORTED`. The detached `spawn_blocking` task drops
// its `Arc<DeviceHandle>` and exits.

#![cfg(target_os = "windows")]

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::task::{JoinError, JoinHandle};

use super::device::DeviceHandle;
use super::ffi;

const READ_CHUNK: usize = 16384;

// --- LibusbkReader -----------------------------------------------------

pub struct LibusbkReader {
    handle: Arc<DeviceHandle>,
    pipe_id: u8,
    /// In-flight blocking ReadPipe, if any.
    pending: Option<JoinHandle<io::Result<Vec<u8>>>>,
    /// Bytes received but not yet copied into the caller's buf.
    leftover: Vec<u8>,
    leftover_off: usize,
}

impl LibusbkReader {
    pub(crate) fn new(handle: Arc<DeviceHandle>, pipe_id: u8) -> Self {
        Self {
            handle,
            pipe_id,
            pending: None,
            leftover: Vec::new(),
            leftover_off: 0,
        }
    }

    fn spawn_read(&self) -> JoinHandle<io::Result<Vec<u8>>> {
        let handle = self.handle.clone();
        let pipe_id = self.pipe_id;
        tokio::task::spawn_blocking(move || -> io::Result<Vec<u8>> {
            let mut buf = vec![0u8; READ_CHUNK];
            let mut transferred: u32 = 0;
            let ok = unsafe {
                ffi::UsbK_ReadPipe(
                    handle.raw(),
                    pipe_id,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut transferred,
                    std::ptr::null_mut(),
                )
            };
            if ok == ffi::FALSE {
                return Err(io::Error::last_os_error());
            }
            buf.truncate(transferred as usize);
            Ok(buf)
        })
    }
}

impl AsyncRead for LibusbkReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        loop {
            // 1. Drain any leftover bytes from a prior chunk.
            let n_leftover = me.leftover.len() - me.leftover_off;
            if n_leftover > 0 {
                let n = n_leftover.min(buf.remaining());
                buf.put_slice(&me.leftover[me.leftover_off..me.leftover_off + n]);
                me.leftover_off += n;
                if me.leftover_off == me.leftover.len() {
                    me.leftover.clear();
                    me.leftover_off = 0;
                }
                return Poll::Ready(Ok(()));
            }

            // 2. Poll an in-flight transfer.
            if let Some(fut) = &mut me.pending {
                match Pin::new(fut).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(res) => {
                        me.pending = None;
                        match unwrap_join(res) {
                            Ok(data) => {
                                if data.is_empty() {
                                    // Zero-byte read: treat as a no-op
                                    // and try again rather than EOF.
                                    continue;
                                }
                                me.leftover = data;
                                me.leftover_off = 0;
                                // Loop to copy.
                            }
                            Err(e) => return Poll::Ready(Err(e)),
                        }
                    }
                }
            } else {
                // 3. No in-flight transfer; kick one off.
                me.pending = Some(me.spawn_read());
            }
        }
    }
}

impl Drop for LibusbkReader {
    fn drop(&mut self) {
        // Unblock the worker thread, if any. The detached task will
        // see ERROR_OPERATION_ABORTED, drop its Arc, and terminate.
        unsafe {
            let _ = ffi::UsbK_AbortPipe(self.handle.raw(), self.pipe_id);
        }
    }
}

// --- LibusbkWriter -----------------------------------------------------

pub struct LibusbkWriter {
    handle: Arc<DeviceHandle>,
    pipe_id: u8,
    max_packet: u16,
    /// In-flight blocking WritePipe, if any.
    pending: Option<JoinHandle<io::Result<usize>>>,
    /// Number of bytes the caller submitted for the in-flight write.
    /// We report this back to the caller as "written" once the
    /// transfer succeeds (libusbK only completes a write when the
    /// full buffer has been sent or an error occurs).
    pending_len: usize,
}

impl LibusbkWriter {
    pub(crate) fn new(handle: Arc<DeviceHandle>, pipe_id: u8, max_packet: u16) -> Self {
        Self {
            handle,
            pipe_id,
            max_packet,
            pending: None,
            pending_len: 0,
        }
    }

    fn spawn_write(&self, bytes: Vec<u8>) -> JoinHandle<io::Result<usize>> {
        let handle = self.handle.clone();
        let pipe_id = self.pipe_id;
        let mps = self.max_packet as usize;
        tokio::task::spawn_blocking(move || -> io::Result<usize> {
            let mut transferred: u32 = 0;
            let ok = unsafe {
                ffi::UsbK_WritePipe(
                    handle.raw(),
                    pipe_id,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    &mut transferred,
                    std::ptr::null_mut(),
                )
            };
            if ok == ffi::FALSE {
                return Err(io::Error::last_os_error());
            }

            // If the transfer was an exact multiple of MPS, the device
            // can't tell where it ends and the next one begins. Send a
            // zero-length packet so the next write starts a fresh USB
            // transfer. Pass a non-null pointer to a stack byte even
            // though length=0 — some libusbK builds reject a NULL buffer
            // pointer regardless of length.
            if mps > 0 && !bytes.is_empty() && bytes.len() % mps == 0 {
                let dummy: u8 = 0;
                let mut zlp_transferred: u32 = 0;
                let ok = unsafe {
                    ffi::UsbK_WritePipe(
                        handle.raw(),
                        pipe_id,
                        &dummy as *const u8,
                        0,
                        &mut zlp_transferred,
                        std::ptr::null_mut(),
                    )
                };
                if ok == ffi::FALSE {
                    return Err(io::Error::last_os_error());
                }
            }

            Ok(transferred as usize)
        })
    }

    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let Some(fut) = &mut self.pending else {
            return Poll::Ready(Ok(0));
        };
        match Pin::new(fut).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(res) => {
                let n = self.pending_len;
                self.pending = None;
                self.pending_len = 0;
                match unwrap_join(res) {
                    Ok(_) => Poll::Ready(Ok(n)),
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
        }
    }
}

impl AsyncWrite for LibusbkWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let me = self.get_mut();
        // If a write is already in flight we cannot start another;
        // wait for it. The caller will be re-awoken when it finishes.
        if me.pending.is_some() {
            return me.poll_pending(cx);
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        me.pending_len = buf.len();
        me.pending = Some(me.spawn_write(buf.to_vec()));
        // Try a synchronous poll — usually returns Pending immediately,
        // but registers the waker so we get notified.
        me.poll_pending(cx)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        match me.poll_pending(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // USB bulk pipes don't have a graceful shutdown; flushing the
        // last in-flight transfer is the most we can do.
        self.poll_flush(cx)
    }
}

impl Drop for LibusbkWriter {
    fn drop(&mut self) {
        unsafe {
            let _ = ffi::UsbK_AbortPipe(self.handle.raw(), self.pipe_id);
        }
    }
}

// --- helpers -----------------------------------------------------------

fn unwrap_join<T>(res: Result<io::Result<T>, JoinError>) -> io::Result<T> {
    match res {
        Ok(inner) => inner,
        Err(je) => Err(io::Error::other(je)),
    }
}
