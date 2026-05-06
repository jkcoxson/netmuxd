// Jackson Coxson
//
// A Rust port of the relevant parts of usbmuxd/src/device.c.
// iOS devices expose a single bulk-in/bulk-out pipe;
// on top of that pipe usbmuxd implements a TCP-emulation
// protocol that lets multiple "connections" be multiplexed to
// different services on the device. Each Connect from a client maps
// to a virtual TCP connection.

use std::collections::HashMap;
use std::io;

use log::{debug, info, trace, warn};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

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
    /// Our internal half of the duplex pair. Reader half lives in a
    /// pump task that forwards bytes onto the data channel.
    write_half: Option<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
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
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin,
{
    // The handshake runs in v1 framing (8-byte header, no magic);
    // we transition to v2 once the device acknowledges VERSION.
    let mut state = MuxState::default();
    send_version(&mut writer, &mut state, 2, 0).await?;

    // Wait for the device's version response (still v1).
    let pkt = read_packet(&mut reader, &mut state).await?;
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
    send_raw(&mut writer, &mut state, Proto::Setup, &[], &[0x07], true).await?;

    let mut connections: HashMap<u16, Connection> = HashMap::new();
    let mut next_sport: u16 = 1;

    // Internal channel: each per-connection pump task forwards user
    // writes here as (sport, bytes). None means the user closed the
    // duplex (we should send FIN/RST and tear down).
    let (tx_data, mut rx_data) = mpsc::channel::<(u16, Option<Vec<u8>>)>(64);

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
                            write_half: None,
                        };
                        if let Err(e) = send_tcp(&mut writer, &mut state, &conn, tcp_flags::SYN, &[]).await {
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
                        if let Err(e) = send_tcp(&mut writer, &mut state, conn, tcp_flags::ACK, &bytes).await {
                            warn!("dev={device_id} send_tcp failed for sport={sport}: {e:?}");
                            teardown(&mut connections, sport);
                            continue;
                        }
                        let conn = connections.get_mut(&sport).unwrap();
                        conn.tx_seq = conn.tx_seq.wrapping_add(bytes.len() as u32);
                    }
                    None => {
                        // User closed: send RST and clean up.
                        let _ = send_tcp(&mut writer, &mut state, conn, tcp_flags::RST, &[]).await;
                        teardown(&mut connections, sport);
                    }
                }
            }
            res = read_packet(&mut reader, &mut state) => {
                let pkt = match res {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("dev={device_id} read failure, exiting: {e:?}");
                        break;
                    }
                };
                if let Err(e) = handle_incoming(
                    device_id,
                    &pkt,
                    &mut connections,
                    &mut writer,
                    &mut state,
                    &tx_data,
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
            let _ = send_tcp(&mut writer, &mut state, conn, tcp_flags::RST, &[]).await;
        }
        teardown(&mut connections, sport);
    }
    Ok(())
}

fn teardown(connections: &mut HashMap<u16, Connection>, sport: u16) {
    if let Some(mut conn) = connections.remove(&sport) {
        conn.state = ConnState::Dead;
        // Dropping write_half closes the duplex from our side, which
        // the user observes as EOF on read. The pump task will see
        // the duplex close on its read and exit.
        drop(conn.write_half.take());
        if let Some(reply) = conn.connect_reply.take() {
            let _ = reply.send(Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "device refused or closed connection",
            )));
        }
    }
}

async fn handle_incoming<W>(
    device_id: u64,
    pkt: &[u8],
    connections: &mut HashMap<u16, Connection>,
    writer: &mut W,
    state: &mut MuxState,
    tx_data: &mpsc::Sender<(u16, Option<Vec<u8>>)>,
) -> io::Result<()>
where
    W: AsyncWrite + Send + Unpin,
{
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
                        write_half: None,
                    };
                    let _ = send_tcp(writer, state, &anon, tcp_flags::RST, &[]).await;
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
                    send_tcp(writer, state, conn, tcp_flags::ACK, &[]).await?;
                    conn.state = ConnState::Connected;

                    // Build the duplex pair, hand one half to the user,
                    // keep the other half here. Spawn a pump task that
                    // reads from the user-side reader and forwards as
                    // (sport, bytes) to the main loop.
                    let (user_side, our_side) = tokio::io::duplex(DUPLEX_BUF);
                    let (mut our_read, our_write) = tokio::io::split(our_side);
                    conn.write_half = Some(our_write);
                    let sport = conn.sport;
                    let tx_data = tx_data.clone();
                    crate::spawn(async move {
                        let mut buf = vec![0u8; MAX_PAYLOAD];
                        loop {
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
                    if let Some(reply) = conn.connect_reply.take() {
                        let _ = reply.send(Ok(user_side));
                    }
                    info!("dev={device_id} sport={our_sport} -> dport={our_dport} connected");
                } else {
                    if let Some(reply) = conn.connect_reply.take() {
                        let _ = reply.send(Err(io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            format!("device refused (flags=0x{:x})", th.flags),
                        )));
                    }
                    teardown(connections, our_sport);
                }
            } else if conn.state == ConnState::Connected {
                if th.flags & tcp_flags::RST != 0 {
                    info!("dev={device_id} sport={our_sport} dport={our_dport} reset by device");
                    teardown(connections, our_sport);
                } else if !payload.is_empty() {
                    // Forward to user.
                    let len = payload.len() as u32;
                    if let Some(w) = conn.write_half.as_mut()
                        && let Err(e) = w.write_all(payload).await
                    {
                        warn!("dev={device_id} sport={our_sport} user-side write failed: {e:?}");
                        let _ = send_tcp(writer, state, conn, tcp_flags::RST, &[]).await;
                        teardown(connections, our_sport);
                        return Ok(());
                    }

                    conn.tx_ack = conn.tx_ack.wrapping_add(len);
                    send_tcp(writer, state, conn, tcp_flags::ACK, &[]).await?;
                }
                // Pure ACK with no payload: nothing to do.
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
                    _ => debug!("dev={device_id} CONTROL kind={kind} ({} bytes)", rest.len()),
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

async fn read_packet<R>(reader: &mut R, state: &mut MuxState) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Send + Unpin,
{
    let header_size = mux_header_size(state.version);
    // Read the first 8 bytes (protocol + length)
    let mut head8 = [0u8; V1_HEADER_SIZE];
    reader.read_exact(&mut head8).await?;
    let length = u32::from_be_bytes(head8[4..8].try_into().unwrap()) as usize;
    if length < header_size || length > USB_MTU {
        return Err(io::Error::other(format!(
            "implausible mux packet length {length} (header_size={header_size})"
        )));
    }
    let mut pkt = vec![0u8; length];
    pkt[0..V1_HEADER_SIZE].copy_from_slice(&head8);
    if length > V1_HEADER_SIZE {
        reader.read_exact(&mut pkt[V1_HEADER_SIZE..]).await?;
    }
    if state.version >= 2 {
        state.rx_seq = u16::from_be_bytes(pkt[14..16].try_into().unwrap());
    }
    trace!(
        "read mux pkt: len={length} version={} header_size={header_size}",
        state.version
    );
    Ok(pkt)
}

async fn send_version<W>(
    writer: &mut W,
    state: &mut MuxState,
    major: u32,
    minor: u32,
) -> io::Result<()>
where
    W: AsyncWrite + Send + Unpin,
{
    // The initial VERSION exchange happens with state.version == 0, so
    // send_raw will write an 8-byte v1 header.
    let mut payload = [0u8; 12];
    payload[0..4].copy_from_slice(&major.to_be_bytes());
    payload[4..8].copy_from_slice(&minor.to_be_bytes());
    send_raw(writer, state, Proto::Version, &payload, &[], false).await
}

async fn send_tcp<W>(
    writer: &mut W,
    state: &mut MuxState,
    conn: &Connection,
    flags: u8,
    payload: &[u8],
) -> io::Result<()>
where
    W: AsyncWrite + Send + Unpin,
{
    let mut hdr = [0u8; TCP_HEADER_SIZE];
    hdr[0..2].copy_from_slice(&conn.sport.to_be_bytes());
    hdr[2..4].copy_from_slice(&conn.dport.to_be_bytes());
    hdr[4..8].copy_from_slice(&conn.tx_seq.to_be_bytes());
    hdr[8..12].copy_from_slice(&conn.tx_ack.to_be_bytes());
    hdr[12] = (TCP_HEADER_SIZE as u8 / 4) << 4;
    hdr[13] = flags;
    hdr[14..16].copy_from_slice(&((RX_WINDOW >> 8) as u16).to_be_bytes());
    // checksum + urgent ptr left zero (the device doesn't validate)
    send_raw(writer, state, Proto::Tcp, &hdr, payload, false).await
}

async fn send_raw<W>(
    writer: &mut W,
    state: &mut MuxState,
    proto: Proto,
    header: &[u8],
    payload: &[u8],
    reset_seq: bool,
) -> io::Result<()>
where
    W: AsyncWrite + Send + Unpin,
{
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
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}
