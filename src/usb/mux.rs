// Jackson Coxson
//
// A Rust port of the relevant parts of usbmuxd/src/device.c.
// iOS devices expose a single bulk-in/bulk-out pipe;
// on top of that pipe usbmuxd implements a TCP-emulation
// protocol that lets multiple "connections" be multiplexed to
// different services on the device. Each Connect from a client maps
// to a virtual TCP connection.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use log::{debug, info, trace, warn};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, oneshot};

// v1 frames have only the 8-byte protocol/length header; v2 adds the
// magic and tx/rx seq, bringing the header to 16 bytes. The very first
// VERSION exchange uses v1 (the C reference code starts with
// dev->version=0 which means an 8-byte header), and we transition to
// v2 once we've parsed the device's VERSION reply.
const V1_HEADER_SIZE: usize = 8;
const V2_HEADER_SIZE: usize = 16;
const MUX_MAGIC: u32 = 0xfeedface;
const TCP_HEADER_SIZE: usize = 20;
const USB_MTU: usize = 3 * 16384;
const MAX_PAYLOAD: usize = USB_MTU - V2_HEADER_SIZE - TCP_HEADER_SIZE;
const RX_WINDOW: u32 = 131072;
const DUPLEX_BUF: usize = 65536;

// throttle the stream a bit
const TX_HIGH_WATER: usize = 1024 * 1024;
const TX_LOW_WATER: usize = 256 * 1024;

fn mux_header_size(version: u8) -> usize {
    if version < 2 {
        V1_HEADER_SIZE
    } else {
        V2_HEADER_SIZE
    }
}

#[repr(u32)]
#[derive(Clone, Copy)]
enum Proto {
    Version = 0,
    Control = 1,
    Setup = 2,
    Tcp = 6,
}

mod tcp_flags {
    pub const SYN: u8 = 0x02;
    pub const RST: u8 = 0x04;
    pub const ACK: u8 = 0x10;
}

/// Handle held by the device-discovery layer to talk to a per-device
/// mux task.
#[derive(Clone, Debug)]
pub struct UsbMuxHandle {
    cmd: mpsc::Sender<Command>,
}

enum Command {
    Connect {
        port: u16,
        reply: oneshot::Sender<io::Result<tokio::io::DuplexStream>>,
    },
    Shutdown,
}

impl UsbMuxHandle {
    /// Open a virtual TCP connection to `port` on the device. The
    /// returned stream is a duplex pipe; bytes written to it are
    /// forwarded to the device, and bytes received from the device
    /// are readable from it.
    pub async fn connect(&self, port: u16) -> io::Result<tokio::io::DuplexStream> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Command::Connect { port, reply: tx })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "mux task gone"))?;
        rx.await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "mux task dropped reply"))?
    }

    pub async fn shutdown(&self) {
        let _ = self.cmd.send(Command::Shutdown).await;
    }
}

/// State for a single virtual connection.
struct Connection {
    sport: u16,
    dport: u16,
    state: ConnState,
    /// Sequence number we assign to bytes we send.
    tx_seq: u32,
    /// Last ack we sent back to the device (tracks bytes we've consumed).
    tx_ack: u32,
    /// The seq the device last sent (pre-payload).
    rx_seq: u32,
    /// Last ack from the device
    rx_ack: u32,
    /// Window size advertised by the device.
    rx_win: u32,
    /// Pending oneshot for the original Connect call (resolved on SYN/ACK or RST).
    connect_reply: Option<oneshot::Sender<io::Result<tokio::io::DuplexStream>>>,
    rx_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    rx_buffered: u32,
    /// User bytes waiting on device-side TCP window space. Flushed in
    /// chunks bounded by `MAX_PAYLOAD` and the device's advertised
    /// window each time `rx_ack`/`rx_win` move.
    pending_tx: VecDeque<u8>,
    client_closed: bool,
    tx_pause: Arc<AtomicBool>,
    tx_resume: Arc<Notify>,
}

#[derive(PartialEq)]
enum ConnState {
    Connecting,
    Connected,
    Dead,
}

/// Spawn a per-device mux task. Returns a handle for opening
/// connections, plus a oneshot that fires when the task exits (used
/// by the manager to drop the device).
///
/// `reader` and `writer` are the device's bulk-in / bulk-out
/// endpoints, abstracted as `AsyncRead` / `AsyncWrite`. On macOS /
/// Linux these come from nusb (`EndpointRead<Bulk>` /
/// `EndpointWrite<Bulk>`); on Windows they'll come from a
/// libusbK-backed wrapper. The protocol code itself is transport-
/// agnostic.
pub fn spawn<R, W>(
    device_id: u64,
    serial: String,
    reader: R,
    writer: W,
    on_exit: oneshot::Sender<u64>,
) -> UsbMuxHandle
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let handle = UsbMuxHandle { cmd: cmd_tx };

    crate::spawn(async move {
        if let Err(e) = run(device_id, &serial, reader, writer, cmd_rx).await {
            warn!("USB mux task for device {device_id} ({serial}) exited: {e:?}");
        } else {
            info!("USB mux task for device {device_id} ({serial}) exited cleanly");
        }
        let _ = on_exit.send(device_id);
    });

    handle
}

async fn run<R, W>(
    device_id: u64,
    serial: &str,
    mut reader: R,
    mut writer: W,
    mut cmd_rx: mpsc::Receiver<Command>,
) -> io::Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    // The handshake runs in v1 framing (8-byte header, no magic);
    // we transition to v2 once the device acknowledges VERSION.
    let mut state = MuxState::default();

    // write outside of the tokio::select
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    crate::spawn(async move {
        while let Some(buf) = write_rx.recv().await {
            if let Err(e) = writer.write_all(&buf).await {
                warn!("dev={device_id} writer task write failed: {e:?}");
                break;
            }
            // Flush per frame so the ZLP / transfer-end framing lands on mux
            // packet boundaries (the device parses one packet per transfer).
            if let Err(e) = writer.flush().await {
                warn!("dev={device_id} writer task flush failed: {e:?}");
                break;
            }
        }
    });

    send_version(&write_tx, &mut state, 2, 0).await?;

    // Wait for the device's version response (still v1).
    let pkt = read_frame(&mut reader).await?;
    let parsed = parse_header(&pkt, state.version)?;
    if parsed.protocol != Proto::Version as u32 {
        return Err(io::Error::other(format!(
            "expected VERSION reply, got proto {}",
            parsed.protocol
        )));
    }
    let payload = &pkt[mux_header_size(state.version)..];
    if payload.len() < 12 {
        return Err(io::Error::other("VERSION payload too short"));
    }
    let major = u32::from_be_bytes(payload[0..4].try_into().unwrap());
    let minor = u32::from_be_bytes(payload[4..8].try_into().unwrap());
    if major != 2 {
        return Err(io::Error::other(format!(
            "unsupported mux version {major}.{minor}"
        )));
    }
    info!("Device {device_id} ({serial}) negotiated mux v{major}.{minor}");
    state.version = 2;

    // SETUP packet kicks the device into mux mode (v2 framing, resets seq).
    send_raw(&write_tx, &mut state, Proto::Setup, &[], &[0x07], true).await?;

    let (pkt_tx, mut pkt_rx) = mpsc::channel::<io::Result<Vec<u8>>>(16);
    let (_shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    crate::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                res = read_frame(&mut reader) => {
                    let stop = res.is_err();
                    if pkt_tx.send(res).await.is_err() {
                        break;
                    }
                    if stop {
                        break;
                    }
                }
            }
        }
    });

    let mut connections: HashMap<u16, Connection> = HashMap::new();
    let mut next_sport: u16 = 1;

    // Internal channel: each per-connection pump task forwards user
    // writes here as (sport, bytes). None means the user closed the
    // duplex (we should send FIN/RST and tear down).
    let (tx_data, mut rx_data) = mpsc::channel::<(u16, Option<Vec<u8>>)>(64);

    let (drain_tx, mut drain_rx) = mpsc::unbounded_channel::<(u16, u32)>();

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(Command::Connect { port, reply }) => {
                        let sport = next_sport;
                        next_sport = next_sport.wrapping_add(1).max(1);
                        debug!("dev={device_id} opening sport={sport} dport={port}");

                        let mut conn = Connection {
                            sport,
                            dport: port,
                            state: ConnState::Connecting,
                            tx_seq: 0,
                            tx_ack: 0,
                            rx_seq: 0,
                            rx_ack: 0,
                            rx_win: 0,
                            connect_reply: Some(reply),
                            rx_tx: None,
                            rx_buffered: 0,
                            pending_tx: VecDeque::new(),
                            client_closed: false,
                            tx_pause: Arc::new(AtomicBool::new(false)),
                            tx_resume: Arc::new(Notify::new()),
                        };
                        if let Err(e) = send_tcp(&write_tx, &mut state, &conn, tcp_flags::SYN, &[]).await {
                            let _ = conn.connect_reply.take().unwrap().send(Err(e));
                            continue;
                        }
                        connections.insert(sport, conn);
                    }
                    Some(Command::Shutdown) | None => {
                        debug!("dev={device_id} shutting down mux task");
                        break;
                    }
                }
            }
            data = rx_data.recv() => {
                let Some((sport, payload)) = data else { break; };
                let Some(conn) = connections.get_mut(&sport) else { continue; };
                if conn.state != ConnState::Connected {
                    continue;
                }
                match payload {
                    Some(bytes) => {
                        conn.pending_tx.extend(bytes);
                        if let Err(e) =
                            flush_pending(&write_tx, &mut state, conn).await
                        {
                            warn!("dev={device_id} send_tcp failed for sport={sport}: {e:?}");
                            teardown(&mut connections, sport);
                            continue;
                        }
                        if conn.pending_tx.len() > TX_HIGH_WATER {
                            conn.tx_pause.store(true, Ordering::Release);
                        }
                    }
                    None => {
                        conn.client_closed = true;
                        if let Err(e) = flush_pending(&write_tx, &mut state, conn).await {
                            warn!("dev={device_id} flush on close failed sport={sport}: {e:?}");
                            let _ = send_tcp(&write_tx, &mut state, conn, tcp_flags::RST, &[]).await;
                            teardown(&mut connections, sport);
                        } else if tx_drained(conn) {
                            let _ = send_tcp(&write_tx, &mut state, conn, tcp_flags::RST, &[]).await;
                            teardown(&mut connections, sport);
                        }
                    }
                }
            }
            drained = drain_rx.recv() => {
                let Some((sport, n)) = drained else { continue; };
                if let Some(conn) = connections.get_mut(&sport) {
                    let before = RX_WINDOW.saturating_sub(conn.rx_buffered);
                    conn.rx_buffered = conn.rx_buffered.saturating_sub(n);
                    let after = RX_WINDOW.saturating_sub(conn.rx_buffered);
                    if conn.state == ConnState::Connected
                        && before < MAX_PAYLOAD as u32
                        && after >= MAX_PAYLOAD as u32
                        && let Err(e) = send_tcp(&write_tx, &mut state, conn, tcp_flags::ACK, &[]).await
                    {
                        warn!("dev={device_id} window-update ACK failed sport={sport}: {e:?}");
                    }
                }
            }
            res = pkt_rx.recv() => {
                let pkt = match res {
                    Some(Ok(p)) => p,
                    Some(Err(e)) => {
                        warn!("dev={device_id} read failure, exiting: {e:?}");
                        break;
                    }
                    None => {
                        debug!("dev={device_id} reader task ended");
                        break;
                    }
                };
                if state.version >= 2 && pkt.len() >= V2_HEADER_SIZE {
                    state.rx_seq = u16::from_be_bytes(pkt[14..16].try_into().unwrap());
                }
                if let Err(e) = handle_incoming(
                    device_id,
                    &pkt,
                    &mut connections,
                    &write_tx,
                    &mut state,
                    &tx_data,
                    &drain_tx,
                ).await {
                    warn!("dev={device_id} packet handler error: {e:?}");
                }
            }
        }
    }

    // Best-effort RST for all open connections.
    let sports: Vec<u16> = connections.keys().copied().collect();
    for sport in sports {
        if let Some(conn) = connections.get(&sport) {
            let _ = send_tcp(&write_tx, &mut state, conn, tcp_flags::RST, &[]).await;
        }
        teardown(&mut connections, sport);
    }
    Ok(())
}

fn tx_drained(conn: &Connection) -> bool {
    conn.pending_tx.is_empty()
}

fn teardown(connections: &mut HashMap<u16, Connection>, sport: u16) {
    if let Some(mut conn) = connections.remove(&sport) {
        conn.state = ConnState::Dead;
        drop(conn.rx_tx.take());
        if let Some(reply) = conn.connect_reply.take() {
            let _ = reply.send(Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "device refused or closed connection",
            )));
        }
    }
}

async fn handle_incoming(
    device_id: u64,
    pkt: &[u8],
    connections: &mut HashMap<u16, Connection>,
    write_tx: &mpsc::UnboundedSender<Vec<u8>>,
    state: &mut MuxState,
    tx_data: &mpsc::Sender<(u16, Option<Vec<u8>>)>,
    drain_tx: &mpsc::UnboundedSender<(u16, u32)>,
) -> io::Result<()> {
    let hdr = parse_header(pkt, state.version)?;
    let header_size = mux_header_size(state.version);

    match hdr.protocol {
        p if p == Proto::Tcp as u32 => {
            if pkt.len() < header_size + TCP_HEADER_SIZE {
                return Err(io::Error::other("TCP packet too short"));
            }
            let th = parse_tcp(&pkt[header_size..header_size + TCP_HEADER_SIZE])?;
            let payload = &pkt[header_size + TCP_HEADER_SIZE..];

            // Device's th.src_port is what it sees as our destination
            // (i.e. the dport on our side); th.dst_port is our sport.
            let our_sport = th.dst_port;
            let our_dport = th.src_port;

            let Some(conn) = connections.get_mut(&our_sport) else {
                if th.flags & tcp_flags::RST == 0 {
                    debug!(
                        "dev={device_id} no connection for incoming {}->{}, sending RST",
                        our_dport, our_sport
                    );
                    let anon = Connection {
                        sport: our_sport,
                        dport: our_dport,
                        state: ConnState::Dead,
                        tx_seq: 0,
                        tx_ack: th.seq,
                        rx_seq: 0,
                        rx_ack: 0,
                        rx_win: 0,
                        connect_reply: None,
                        rx_tx: None,
                        rx_buffered: 0,
                        pending_tx: VecDeque::new(),
                        client_closed: false,
                        tx_pause: Arc::new(AtomicBool::new(false)),
                        tx_resume: Arc::new(Notify::new()),
                    };
                    let _ = send_tcp(write_tx, state, &anon, tcp_flags::RST, &[]).await;
                }
                return Ok(());
            };

            conn.rx_seq = th.seq;
            conn.rx_ack = th.ack;
            conn.rx_win = (th.window as u32) << 8;

            if conn.state == ConnState::Connecting {
                if th.flags == (tcp_flags::SYN | tcp_flags::ACK) {
                    conn.tx_seq = conn.tx_seq.wrapping_add(1);
                    conn.tx_ack = conn.tx_ack.wrapping_add(1);
                    send_tcp(write_tx, state, conn, tcp_flags::ACK, &[]).await?;
                    conn.state = ConnState::Connected;

                    let (user_side, our_side) = tokio::io::duplex(DUPLEX_BUF);
                    let (mut our_read, mut our_write) = tokio::io::split(our_side);
                    let (rx_tx, mut rx_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                    conn.rx_tx = Some(rx_tx);
                    let sport = conn.sport;

                    // don't overwhelm the device, throttle
                    let tx_data = tx_data.clone();
                    let tx_pause = conn.tx_pause.clone();
                    let tx_resume = conn.tx_resume.clone();
                    crate::spawn(async move {
                        let mut buf = vec![0u8; MAX_PAYLOAD];
                        loop {
                            while tx_pause.load(Ordering::Acquire) {
                                tx_resume.notified().await;
                            }
                            match our_read.read(&mut buf).await {
                                Ok(0) | Err(_) => {
                                    let _ = tx_data.send((sport, None)).await;
                                    return;
                                }
                                Ok(n) => {
                                    if tx_data
                                        .send((sport, Some(buf[..n].to_vec())))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                        }
                    });

                    // Forwarder: device -> user, reporting drained bytes.
                    let drain_tx = drain_tx.clone();
                    crate::spawn(async move {
                        while let Some(chunk) = rx_rx.recv().await {
                            let n = chunk.len() as u32;
                            if our_write.write_all(&chunk).await.is_err() {
                                break;
                            }
                            if drain_tx.send((sport, n)).is_err() {
                                break;
                            }
                        }
                        // Dropping our_write closes the duplex
                    });

                    if let Some(reply) = conn.connect_reply.take() {
                        let _ = reply.send(Ok(user_side));
                    }
                    info!("dev={device_id} sport={our_sport} -> dport={our_dport} connected");
                } else {
                    if let Some(reply) = conn.connect_reply.take() {
                        let reason = String::from_utf8_lossy(payload);
                        let _ = reply.send(Err(io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            format!(
                                "device refused (flags=0x{:x}, reason: {})",
                                th.flags,
                                reason.trim_end()
                            ),
                        )));
                    }
                    teardown(connections, our_sport);
                }
            } else if conn.state == ConnState::Connected {
                if th.flags & tcp_flags::RST != 0 {
                    let reason = String::from_utf8_lossy(payload);
                    warn!(
                        "dev={device_id} sport={our_sport} dport={our_dport} reset by device \
                         (reason: {})",
                        reason.trim_end()
                    );
                    teardown(connections, our_sport);
                } else if th.flags != tcp_flags::ACK {
                    warn!(
                        "dev={device_id} sport={our_sport} dport={our_dport} unexpected flags \
                         0x{:x}, closing",
                        th.flags
                    );
                    teardown(connections, our_sport);
                } else {
                    if !payload.is_empty() {
                        let len = payload.len() as u32;
                        let delivered = conn
                            .rx_tx
                            .as_ref()
                            .is_some_and(|tx| tx.send(payload.to_vec()).is_ok());
                        if !delivered {
                            warn!("dev={device_id} sport={our_sport} user-side gone; resetting");
                            let _ = send_tcp(write_tx, state, conn, tcp_flags::RST, &[]).await;
                            teardown(connections, our_sport);
                            return Ok(());
                        }

                        conn.rx_buffered = conn.rx_buffered.saturating_add(len);
                        conn.tx_ack = conn.tx_ack.wrapping_add(len);
                        // The ACK carries the now-shrunk window (see send_tcp).
                        send_tcp(write_tx, state, conn, tcp_flags::ACK, &[]).await?;
                    }
                    // The packet (payload-bearing or pure ACK) updated
                    // rx_ack / rx_win — try to flush any user bytes that
                    // were waiting on window space.
                    flush_pending(write_tx, state, conn).await?;

                    // finish writing before closing
                    if conn.client_closed && tx_drained(conn) {
                        send_tcp(write_tx, state, conn, tcp_flags::RST, &[]).await?;
                        teardown(connections, our_sport);
                    }
                }
            }
        }
        p if p == Proto::Control as u32 => {
            let payload = &pkt[header_size..];
            if let Some((&kind, rest)) = payload.split_first() {
                match kind {
                    3 => warn!(
                        "dev={device_id} CONTROL ERROR: {}",
                        String::from_utf8_lossy(rest)
                    ),
                    5 => warn!(
                        "dev={device_id} CONTROL WARN: {}",
                        String::from_utf8_lossy(rest)
                    ),
                    7 => info!(
                        "dev={device_id} CONTROL INFO: {}",
                        String::from_utf8_lossy(rest)
                    ),
                    _ => warn!(
                        "dev={device_id} CONTROL kind={kind} ({} bytes): {}",
                        rest.len(),
                        String::from_utf8_lossy(rest)
                    ),
                }
            }
        }
        p if p == Proto::Version as u32 => {
            warn!("dev={device_id} unexpected VERSION packet after handshake");
        }
        other => {
            warn!("dev={device_id} unknown protocol {other}");
        }
    }
    Ok(())
}

#[derive(Default)]
struct MuxState {
    /// 0 before the version handshake completes, 2 afterwards.
    version: u8,
    tx_seq: u16,
    rx_seq: u16,
}

struct ParsedHeader {
    protocol: u32,
}

fn parse_header(pkt: &[u8], version: u8) -> io::Result<ParsedHeader> {
    let header_size = mux_header_size(version);
    if pkt.len() < header_size {
        return Err(io::Error::other("mux header too short"));
    }
    let protocol = u32::from_be_bytes(pkt[0..4].try_into().unwrap());
    // usbmuxd writes magic = 0xfeedface on every outbound v2 packet
    // but does not validate the incoming magic. Devices have been
    // observed to send other constants (e.g. 0xfaceface). Trust the
    // protocol/length fields and don't reject on magic mismatch, idk
    Ok(ParsedHeader { protocol })
}

struct ParsedTcp {
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
}

fn parse_tcp(buf: &[u8]) -> io::Result<ParsedTcp> {
    if buf.len() < TCP_HEADER_SIZE {
        return Err(io::Error::other("TCP header too short"));
    }
    Ok(ParsedTcp {
        src_port: u16::from_be_bytes(buf[0..2].try_into().unwrap()),
        dst_port: u16::from_be_bytes(buf[2..4].try_into().unwrap()),
        seq: u32::from_be_bytes(buf[4..8].try_into().unwrap()),
        ack: u32::from_be_bytes(buf[8..12].try_into().unwrap()),
        flags: buf[13],
        window: u16::from_be_bytes(buf[14..16].try_into().unwrap()),
    })
}

async fn read_frame<R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Send + Unpin,
{
    // Read the first 8 bytes (protocol + length).
    let mut head8 = [0u8; V1_HEADER_SIZE];
    reader.read_exact(&mut head8).await?;
    let protocol = u32::from_be_bytes(head8[0..4].try_into().unwrap());
    let length = u32::from_be_bytes(head8[4..8].try_into().unwrap()) as usize;
    if !(V2_HEADER_SIZE..=USB_MTU).contains(&length) {
        let mut tail = [0u8; 32];
        let n = match tokio::time::timeout(
            std::time::Duration::from_millis(50),
            reader.read(&mut tail),
        )
        .await
        {
            Ok(Ok(n)) => n,
            _ => 0,
        };
        return Err(io::Error::other(format!(
            "implausible mux packet length {length} (protocol={protocol:#010x}, \
             head={head8:02x?}, next_{n}={:02x?})",
            &tail[..n],
        )));
    }
    let mut pkt = vec![0u8; length];
    pkt[0..V1_HEADER_SIZE].copy_from_slice(&head8);
    if length > V1_HEADER_SIZE {
        reader.read_exact(&mut pkt[V1_HEADER_SIZE..]).await?;
    }
    trace!("read mux frame: len={length}");
    Ok(pkt)
}

async fn send_version(
    write_tx: &mpsc::UnboundedSender<Vec<u8>>,
    state: &mut MuxState,
    major: u32,
    minor: u32,
) -> io::Result<()> {
    // The initial VERSION exchange happens with state.version == 0, so
    // send_raw will write an 8-byte v1 header.
    let mut payload = [0u8; 12];
    payload[0..4].copy_from_slice(&major.to_be_bytes());
    payload[4..8].copy_from_slice(&minor.to_be_bytes());
    send_raw(write_tx, state, Proto::Version, &payload, &[], false).await
}

async fn flush_pending(
    write_tx: &mpsc::UnboundedSender<Vec<u8>>,
    state: &mut MuxState,
    conn: &mut Connection,
) -> io::Result<()> {
    while !conn.pending_tx.is_empty() {
        let inflight = conn.tx_seq.wrapping_sub(conn.rx_ack);
        if inflight >= conn.rx_win {
            break;
        }
        let available = (conn.rx_win - inflight) as usize;
        let chunk_len = conn.pending_tx.len().min(MAX_PAYLOAD).min(available);
        if chunk_len == 0 {
            break;
        }
        let chunk: Vec<u8> = conn.pending_tx.drain(..chunk_len).collect();
        send_tcp(write_tx, state, conn, tcp_flags::ACK, &chunk).await?;
        conn.tx_seq = conn.tx_seq.wrapping_add(chunk_len as u32);
    }
    // Resume the reader pump once we've drained below the low watermark, so the
    // client can push more (it was paused when `pending_tx` filled up).
    if conn.tx_pause.load(Ordering::Acquire) && conn.pending_tx.len() < TX_LOW_WATER {
        conn.tx_pause.store(false, Ordering::Release);
        conn.tx_resume.notify_one();
    }
    Ok(())
}

async fn send_tcp(
    write_tx: &mpsc::UnboundedSender<Vec<u8>>,
    state: &mut MuxState,
    conn: &Connection,
    flags: u8,
    payload: &[u8],
) -> io::Result<()> {
    let mut hdr = [0u8; TCP_HEADER_SIZE];
    hdr[0..2].copy_from_slice(&conn.sport.to_be_bytes());
    hdr[2..4].copy_from_slice(&conn.dport.to_be_bytes());
    hdr[4..8].copy_from_slice(&conn.tx_seq.to_be_bytes());
    hdr[8..12].copy_from_slice(&conn.tx_ack.to_be_bytes());
    hdr[12] = (TCP_HEADER_SIZE as u8 / 4) << 4;
    hdr[13] = flags;
    // Advertise the receive space still free on this connection so a slow
    // consumer throttles the device on this stream alone (0 => pause). The
    // window is a 256-byte-granular u16, matching how we read the device's.
    let win = RX_WINDOW.saturating_sub(conn.rx_buffered);
    hdr[14..16].copy_from_slice(&((win >> 8) as u16).to_be_bytes());
    // checksum + urgent ptr left zero (the device doesn't validate)
    send_raw(write_tx, state, Proto::Tcp, &hdr, payload, false).await
}

async fn send_raw(
    write_tx: &mpsc::UnboundedSender<Vec<u8>>,
    state: &mut MuxState,
    proto: Proto,
    header: &[u8],
    payload: &[u8],
    reset_seq: bool,
) -> io::Result<()> {
    let header_size = mux_header_size(state.version);
    let total = header_size + header.len() + payload.len();
    if total > USB_MTU {
        return Err(io::Error::other(format!("packet too large: {total}")));
    }
    if reset_seq && state.version >= 2 {
        state.tx_seq = 0;
        state.rx_seq = 0xFFFF;
    }
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(&(proto as u32).to_be_bytes());
    buf.extend_from_slice(&(total as u32).to_be_bytes());
    if state.version >= 2 {
        buf.extend_from_slice(&MUX_MAGIC.to_be_bytes());
        buf.extend_from_slice(&state.tx_seq.to_be_bytes());
        buf.extend_from_slice(&state.rx_seq.to_be_bytes());
        state.tx_seq = state.tx_seq.wrapping_add(1);
    }
    buf.extend_from_slice(header);
    buf.extend_from_slice(payload);
    write_tx
        .send(buf)
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer task gone"))?;
    Ok(())
}
