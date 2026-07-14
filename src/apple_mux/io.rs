// Jackson Coxson

#![cfg(target_os = "windows")]

use std::ffi::c_void;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::task::{JoinError, JoinHandle};

use super::device::{DeviceHandle, ioctl_sync};
use super::ffi;

const READ_CHUNK: usize = 0x8000;

pub struct AppleMuxReader {
    handle: Arc<DeviceHandle>,
    pipe: u8,
    pending: Option<JoinHandle<io::Result<Vec<u8>>>>,
    leftover: Vec<u8>,
    leftover_off: usize,
}

impl AppleMuxReader {
    pub(crate) fn new(handle: Arc<DeviceHandle>, pipe: u8) -> Self {
        Self {
            handle,
            pipe,
            pending: None,
            leftover: Vec::new(),
            leftover_off: 0,
        }
    }

    fn spawn_read(&self, want: usize) -> JoinHandle<io::Result<Vec<u8>>> {
        let handle = self.handle.clone();
        let code = ffi::ioctl_read_pipe(self.pipe);
        let cap = want.clamp(1, READ_CHUNK);
        tokio::task::spawn_blocking(move || -> io::Result<Vec<u8>> {
            let mut buf = vec![0u8; cap];
            // The read IOCTL uses the buffer as both in and out (mirrors
            // Apple's Usbmuxio_ReadPipe_SyncF).
            let n = unsafe {
                ioctl_sync(
                    handle.raw(),
                    code,
                    buf.as_ptr() as *const c_void,
                    buf.len() as u32,
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as u32,
                )?
            };
            buf.truncate(n as usize);
            Ok(buf)
        })
    }
}

impl AsyncRead for AppleMuxReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        loop {
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

            if let Some(fut) = &mut me.pending {
                match Pin::new(fut).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(res) => {
                        me.pending = None;
                        match unwrap_join(res) {
                            Ok(data) => {
                                if data.is_empty() {
                                    continue;
                                }
                                me.leftover = data;
                                me.leftover_off = 0;
                            }
                            Err(e) => return Poll::Ready(Err(e)),
                        }
                    }
                }
            } else {
                me.pending = Some(me.spawn_read(buf.remaining()));
            }
        }
    }
}

impl Drop for AppleMuxReader {
    fn drop(&mut self) {
        abort_pipe(&self.handle, self.pipe);
    }
}

pub struct AppleMuxWriter {
    handle: Arc<DeviceHandle>,
    pipe: u8,
    max_packet: u16,
    pending: Option<JoinHandle<io::Result<usize>>>,
    pending_len: usize,
}

impl AppleMuxWriter {
    pub(crate) fn new(handle: Arc<DeviceHandle>, pipe: u8, max_packet: u16) -> Self {
        Self {
            handle,
            pipe,
            max_packet,
            pending: None,
            pending_len: 0,
        }
    }

    fn spawn_write(&self, bytes: Vec<u8>) -> JoinHandle<io::Result<usize>> {
        let handle = self.handle.clone();
        let code = ffi::ioctl_write_pipe(self.pipe);
        let max_packet = self.max_packet as usize;
        tokio::task::spawn_blocking(move || -> io::Result<usize> {
            let len = bytes.len();
            let n = unsafe {
                ioctl_sync(
                    handle.raw(),
                    code,
                    bytes.as_ptr() as *const c_void,
                    len as u32,
                    bytes.as_ptr() as *mut c_void,
                    len as u32,
                )?
            };
            if max_packet > 0 && len > 0 && len.is_multiple_of(max_packet) {
                unsafe {
                    ioctl_sync(
                        handle.raw(),
                        code,
                        bytes.as_ptr() as *const c_void,
                        0,
                        bytes.as_ptr() as *mut c_void,
                        0,
                    )?;
                }
            }
            Ok(n as usize)
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

impl AsyncWrite for AppleMuxWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let me = self.get_mut();
        if me.pending.is_some() {
            return me.poll_pending(cx);
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        me.pending_len = buf.len();
        me.pending = Some(me.spawn_write(buf.to_vec()));
        me.poll_pending(cx)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut().poll_pending(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl Drop for AppleMuxWriter {
    fn drop(&mut self) {
        abort_pipe(&self.handle, self.pipe);
    }
}

/// Best-effort pipe abort (unblocks an in-flight blocking transfer).
fn abort_pipe(handle: &Arc<DeviceHandle>, pipe: u8) {
    unsafe {
        let _ = ioctl_sync(
            handle.raw(),
            ffi::ioctl_abort_pipe(pipe),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
        );
    }
}

fn unwrap_join<T>(res: Result<io::Result<T>, JoinError>) -> io::Result<T> {
    match res {
        Ok(inner) => inner,
        Err(je) => Err(io::Error::other(je)),
    }
}
