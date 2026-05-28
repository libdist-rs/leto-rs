//! Persistent-connection transport for hera, generic over raw [`Bytes`] frames.
//!
//! Why this exists: libnet's `TcpSimpleSender` reconnects on every send error
//! with zero backoff, so under hera's all-to-all data flood the peers' accept
//! backlog saturates, the kernel RSTs new SYNs ("Connection refused"), and the
//! sender immediately re-dials -- a self-reinforcing connection storm that
//! drops the sig-chain's low-volume but critical messages and wedges the
//! protocol at round 2 (see memory: hera-n61-stall-diagnosis).
//!
//! This transport instead keeps **one persistent connection per peer pair**:
//! - an active/passive handshake so only the lower-id side dials (no dueling
//!   connects); the dialer announces its authority id so the listener routes
//!   the socket to the right peer worker (no fragile source-address matching),
//! - reconnect only when the single connection dies, with a jittered 1--5s
//!   backoff (no hammering),
//! - **bounded (1000) channels** so overload is shed by dropping (the caller
//!   uses `try_send`) instead of erroring the socket,
//! - a 30s ping/pong that breaks a connection whose RTT exceeds a configured
//!   ceiling, so a silently-stuck peer frees its slot.
//!
//! Structurally ported from `mysticeti-core/src/network.rs`; the Prometheus
//! metrics and typed `NetworkMessage` enum are removed (frames are opaque
//! `Bytes`), and mysticeti's source-address matching is replaced by an
//! id-in-handshake so the transport works identically whether peers share an
//! IP (local tests) or not (AWS).

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::{select, select_all, Either};
use futures_util::FutureExt;
use rand::prelude::ThreadRng;
use rand::Rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::runtime::Handle;
use tokio::select;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::Receiver;
use tokio::task::JoinHandle;
use tokio::time::Instant;

const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Default per-peer outbound channel depth for the data plane (full ⇒ caller
/// drops, shedding). The data transport now derives its outbox cap from the
/// chain-lead window W (see `core.rs` / `spawn_data_processed`), so this is only
/// a fallback for `spawn_data`/`spawn_sig`; the sig plane is unbounded anyway.
pub(crate) const CHANNEL_CAPACITY: usize = 1_000;

/// Raw per-connection inbound staging buffer (frames straight off the socket,
/// before deserialize+verify). Uses a *blocking* send (`handle_read_stream`),
/// so a full buffer backpressures the socket read rather than dropping —
/// modest is fine.
pub(crate) const INBOUND_CHANNEL_CAPACITY: usize = 512;

/// Per-plane channel discipline for the per-peer outbox.
///
/// - `Bounded(cap)`: bounded mpsc; caller uses `try_send` and drops on full.
///   Used for the data plane — high-volume, loss-tolerant (refetchable).
/// - `Unbounded`: unbounded mpsc; caller uses blocking `send`. Used for the sig
///   plane — low-volume, loss-intolerant (a dropped SigPropose wedges liveness
///   with no per-message retransmit).
#[derive(Clone, Copy, Debug)]
pub enum ChannelMode {
    Bounded(usize),
    Unbounded,
}
/// Break a connection whose measured RTT exceeds this (a stuck peer frees its
/// slot so a fresh connection can be established).
const MAX_LATENCY: Duration = Duration::from_secs(5);
const MAX_SIZE: u32 = 16 * 1024 * 1024;
const ACTIVE_HANDSHAKE: u64 = 0xFEFE0000;
const PASSIVE_HANDSHAKE: u64 = 0x0000AEAE;
/// Bound the time a freshly-accepted socket has to announce its id, so a slow
/// or malicious dialer cannot pin the listener.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Outbound sender, either bounded (data plane, try_send+drop) or unbounded
/// (sig plane, reliable send, never drop).
pub enum OutboundSender {
    Bounded(mpsc::Sender<Bytes>),
    Unbounded(mpsc::UnboundedSender<Bytes>),
}

impl OutboundSender {
    /// Try to send a frame. For bounded senders drops on full (data plane
    /// shedding). For unbounded senders always succeeds (sig plane must not
    /// drop).
    pub fn try_send(
        &self,
        bytes: Bytes,
    ) -> bool {
        match self {
            OutboundSender::Bounded(s) => s.try_send(bytes).is_ok(),
            OutboundSender::Unbounded(s) => s.send(bytes).is_ok(),
        }
    }
}

/// Handed to the application each time a (re)connection to a peer succeeds.
/// `sender` is the current outbound channel for `peer_id`; `receiver` yields
/// inbound frames from that peer.
pub struct Connection {
    pub peer_id: usize,
    pub sender: OutboundSender,
    pub receiver: mpsc::Receiver<Bytes>,
}

pub struct Network {
    connection_receiver: mpsc::Receiver<Connection>,
    stop: Option<mpsc::Sender<()>>,
    server_handle: Option<JoinHandle<()>>,
}

impl Network {
    /// `addresses` is indexed by authority id (0..n); `our_id` indexes into it;
    /// `local_addr` is the bind address for the inbound listener. `mode`
    /// controls per-peer outbox discipline (bounded+drop for data plane,
    /// unbounded for sig plane). Synchronous: must be called from within a
    /// tokio runtime (it spawns the server and per-peer worker tasks).
    pub fn from_socket_addresses(
        addresses: &[SocketAddr],
        our_id: usize,
        local_addr: SocketAddr,
        mode: ChannelMode,
    ) -> Self {
        assert!(
            our_id < addresses.len(),
            "our_id {our_id} >= address count {}",
            addresses.len()
        );
        let mut worker_senders: HashMap<usize, mpsc::UnboundedSender<TcpStream>> = HashMap::new();
        let handle = Handle::current();
        let (connection_sender, connection_receiver) = mpsc::channel(16);
        for (id, address) in addresses.iter().enumerate() {
            if id == our_id {
                continue;
            }
            let (sender, receiver) = mpsc::unbounded_channel();
            worker_senders.insert(id, sender);
            handle.spawn(
                Worker {
                    peer: *address,
                    peer_id: id,
                    our_id,
                    connection_sender: connection_sender.clone(),
                    active_immediately: id < our_id,
                    channel_mode: mode,
                }
                .run(receiver),
            );
        }
        let (stop, rx_stop) = mpsc::channel(1);
        let server_handle = handle.spawn(async move {
            Server {
                local_addr,
                worker_senders: Arc::new(worker_senders),
            }
            .run(rx_stop)
            .await
        });
        Self {
            connection_receiver,
            stop: Some(stop),
            server_handle: Some(server_handle),
        }
    }

    pub fn connection_receiver(&mut self) -> &mut mpsc::Receiver<Connection> {
        &mut self.connection_receiver
    }

    #[allow(dead_code)]
    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            stop.send(()).await.ok();
        }
        if let Some(handle) = self.server_handle.take() {
            handle.await.ok();
        }
    }
}

struct Server {
    local_addr: SocketAddr,
    worker_senders: Arc<HashMap<usize, mpsc::UnboundedSender<TcpStream>>>,
}

impl Server {
    async fn run(
        self,
        mut stop: Receiver<()>,
    ) {
        // Build the listener with SO_REUSEADDR so a fast restart can rebind a
        // port still in TIME_WAIT (otherwise local re-runs panic with AddrInUse).
        let socket = match self.local_addr {
            SocketAddr::V4(_) => TcpSocket::new_v4(),
            SocketAddr::V6(_) => TcpSocket::new_v6(),
        }
        .expect("create TCP socket");
        socket
            .set_reuseaddr(true)
            .expect("set SO_REUSEADDR on listener");
        socket
            .bind(self.local_addr)
            .expect("Failed to bind to local socket");
        let server = socket.listen(1024).expect("listen on bound socket");
        loop {
            tokio::select! {
                result = server.accept() => {
                    let (socket, remote) = result.expect("accept failed");
                    // Read the dialer's announced id off-thread so a slow peer
                    // cannot stall the accept loop.
                    let workers = self.worker_senders.clone();
                    tokio::spawn(async move {
                        match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_active_handshake(socket)).await {
                            Ok(Ok((socket, peer_id))) => {
                                if let Some(sender) = workers.get(&peer_id) {
                                    sender.send(socket).ok();
                                } else {
                                    log::warn!("Inbound connection from unknown peer id {peer_id} ({remote})");
                                }
                            }
                            Ok(Err(e)) => log::warn!("Handshake read failed from {remote}: {e}"),
                            Err(_) => log::warn!("Handshake timed out from {remote}"),
                        }
                    });
                }
                _ = stop.recv() => {
                    log::info!("Shutting down hera network");
                    return;
                }
            }
        }
    }
}

/// Read the active side's handshake: magic word followed by its authority id.
async fn read_active_handshake(mut socket: TcpStream) -> io::Result<(TcpStream, usize)> {
    let magic = socket.read_u64().await?;
    if magic != ACTIVE_HANDSHAKE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad active handshake magic: {magic:#x}"),
        ));
    }
    let peer_id = socket.read_u64().await? as usize;
    Ok((socket, peer_id))
}

struct Worker {
    peer: SocketAddr,
    peer_id: usize,
    our_id: usize,
    connection_sender: mpsc::Sender<Connection>,
    active_immediately: bool,
    channel_mode: ChannelMode,
}

impl Worker {
    async fn run(
        self,
        mut receiver: mpsc::UnboundedReceiver<TcpStream>,
    ) -> Option<()> {
        let initial_delay = if self.active_immediately {
            Duration::ZERO
        } else {
            sample_delay(Duration::from_secs(1)..Duration::from_secs(5))
        };
        let mut work = self.connect_and_handle(initial_delay).boxed();
        loop {
            match select(work, receiver.recv().boxed()).await {
                Either::Left((_work, _receiver)) => {
                    // The connection died; reconnect after a jittered backoff
                    // (this is what stops the per-error reconnect storm).
                    let delay = sample_delay(Duration::from_secs(1)..Duration::from_secs(5));
                    work = self.connect_and_handle(delay).boxed();
                }
                Either::Right((received, _work)) => {
                    if let Some(socket) = received {
                        log::debug!("Replaced connection for {}", self.peer_id);
                        work = self.handle_passive_stream(socket).boxed();
                    } else {
                        return None; // server terminated
                    }
                }
            }
        }
    }

    async fn connect_and_handle(
        &self,
        delay: Duration,
    ) -> io::Result<()> {
        // critical to avoid a race between active and passive connections
        tokio::time::sleep(delay).await;
        let mut stream = loop {
            match TcpStream::connect(self.peer).await {
                Ok(stream) => break stream,
                Err(_err) => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        };
        stream.set_nodelay(true)?;
        stream.write_u64(ACTIVE_HANDSHAKE).await?;
        stream.write_u64(self.our_id as u64).await?;
        let handshake = stream.read_u64().await?;
        if handshake != PASSIVE_HANDSHAKE {
            log::warn!("Invalid passive handshake: {handshake}");
            return Ok(());
        }
        let Some(connection) = self.make_connection().await else {
            return Ok(());
        };
        Self::handle_stream(stream, connection).await
    }

    async fn handle_passive_stream(
        &self,
        mut stream: TcpStream,
    ) -> io::Result<()> {
        // The dialer's magic + id were already consumed by the server before it
        // routed the socket here; we only reply with the passive handshake.
        stream.set_nodelay(true)?;
        stream.write_u64(PASSIVE_HANDSHAKE).await?;
        let Some(connection) = self.make_connection().await else {
            return Ok(());
        };
        Self::handle_stream(stream, connection).await
    }

    async fn handle_stream(
        stream: TcpStream,
        connection: WorkerConnection,
    ) -> io::Result<()> {
        let WorkerConnection {
            sender,
            receiver,
            peer_id,
        } = connection;
        log::debug!("Connected to {peer_id}");
        let (reader, writer) = stream.into_split();
        let (pong_sender, pong_receiver) = mpsc::channel(150);
        let write_fut = Self::handle_write_stream(writer, receiver, pong_receiver).boxed();
        let read_fut = Self::handle_read_stream(reader, sender, pong_sender).boxed();
        let (r, ..) = select_all([write_fut, read_fut]).await;
        log::debug!("Disconnected from {peer_id}");
        r
    }

    async fn handle_write_stream(
        mut writer: OwnedWriteHalf,
        mut receiver: mpsc::UnboundedReceiver<Bytes>,
        mut pong_receiver: mpsc::Receiver<i64>,
    ) -> io::Result<()> {
        let start = Instant::now();
        let mut ping_deadline = start + PING_INTERVAL;
        loop {
            select! {
                _ = tokio::time::sleep_until(ping_deadline) => {
                    ping_deadline += PING_INTERVAL;
                    let ping_time = start.elapsed().as_micros() as i64;
                    assert!(ping_time > 0);
                    writer.write_all(&encode_ping(ping_time)).await?;
                }
                received = pong_receiver.recv() => {
                    // Embedded ping-pong RTT: a positive ping is echoed back
                    // negated as a pong; a negative value lets us recover our
                    // original send time and compute RTT.
                    let Some(ping) = received else { return Ok(()) };
                    if ping == 0 {
                        log::warn!("Invalid ping: {ping}");
                        return Ok(());
                    }
                    if ping > 0 {
                        match ping.checked_neg() {
                            Some(pong) => writer.write_all(&encode_ping(pong)).await?,
                            None => { log::warn!("Invalid ping: {ping}"); return Ok(()); }
                        }
                    } else {
                        match ping.checked_neg().and_then(|n| u64::try_from(n).ok()) {
                            Some(our_ping) => {
                                let time = start.elapsed().as_micros() as u64;
                                match time.checked_sub(our_ping) {
                                    Some(delay) => {
                                        let d = Duration::from_micros(delay);
                                        if d >= MAX_LATENCY {
                                            log::warn!("High latency connection: {d:?}. Breaking.");
                                            return Ok(());
                                        }
                                    }
                                    None => {
                                        log::warn!("Invalid ping {ping} > current time {time}");
                                        return Ok(());
                                    }
                                }
                            }
                            None => { log::warn!("Invalid pong: {ping}"); return Ok(()); }
                        }
                    }
                }
                received = receiver.recv() => {
                    let Some(frame) = received else { return Ok(()) };
                    writer.write_u32(frame.len() as u32).await?;
                    writer.write_all(&frame).await?;
                }
            }
        }
    }

    async fn handle_read_stream(
        mut stream: OwnedReadHalf,
        sender: mpsc::Sender<Bytes>,
        pong_sender: mpsc::Sender<i64>,
    ) -> io::Result<()> {
        let mut buf = vec![0u8; MAX_SIZE as usize].into_boxed_slice();
        loop {
            let size = stream.read_u32().await?;
            if size > MAX_SIZE {
                log::warn!("Invalid size: {size}");
                return Ok(());
            }
            if size == 0 {
                // ping/pong frame
                let buf = &mut buf[..PING_SIZE - 4];
                let read = stream.read_exact(buf).await?;
                assert_eq!(read, buf.len());
                let pong = decode_ping(buf);
                match pong_sender.try_reserve() {
                    Ok(permit) => permit.send(pong),
                    Err(TrySendError::Full(_)) => log::error!("Pong channel saturated; dropping"),
                    Err(TrySendError::Closed(_)) => return Ok(()),
                }
                continue;
            }
            let buf = &mut buf[..size as usize];
            let read = stream.read_exact(buf).await?;
            assert_eq!(read, buf.len());
            // Inbound frame; block-on-full here applies backpressure to this
            // peer's socket read (the read side is not the flood path).
            if sender.send(Bytes::copy_from_slice(buf)).await.is_err() {
                return Ok(());
            }
        }
    }

    async fn make_connection(&self) -> Option<WorkerConnection> {
        // Inbound: always a bounded channel (backpressure on the socket read).
        let (network_in_sender, network_in_receiver) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
        // Outbound: bounded for data plane (try_send+drop), unbounded for sig.
        let (outbound_sender, write_receiver) = match self.channel_mode {
            ChannelMode::Bounded(cap) => {
                let (tx, rx_bounded) = mpsc::channel::<Bytes>(cap);
                // Bridge: forward the bounded rx into an unbounded channel so
                // handle_write_stream can use a single UnboundedReceiver type.
                let (ub_tx, ub_rx) = mpsc::unbounded_channel::<Bytes>();
                let ub_tx_fwd = ub_tx.clone();
                tokio::spawn(async move {
                    let mut rx = rx_bounded;
                    while let Some(frame) = rx.recv().await {
                        if ub_tx_fwd.send(frame).is_err() {
                            break;
                        }
                    }
                });
                (OutboundSender::Bounded(tx), ub_rx)
            }
            ChannelMode::Unbounded => {
                let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
                (OutboundSender::Unbounded(tx), rx)
            }
        };
        let connection = Connection {
            peer_id: self.peer_id,
            sender: outbound_sender,
            receiver: network_in_receiver,
        };
        self.connection_sender.send(connection).await.ok()?;
        Some(WorkerConnection {
            sender: network_in_sender,
            receiver: write_receiver,
            peer_id: self.peer_id,
        })
    }
}

struct WorkerConnection {
    /// Inbound: frames from the TCP stream sent to the aggregate inbound
    /// channel.
    sender: mpsc::Sender<Bytes>,
    /// Outbound: frames to write to the TCP stream. Always UnboundedReceiver;
    /// the bounded/unbounded discipline is enforced at the OutboundSender
    /// level.
    receiver: mpsc::UnboundedReceiver<Bytes>,
    peer_id: usize,
}

fn sample_delay(range: Range<Duration>) -> Duration {
    ThreadRng::default().gen_range(range)
}

const PING_SIZE: usize = 12;
fn encode_ping(message: i64) -> [u8; PING_SIZE] {
    let mut m = [0u8; PING_SIZE];
    m[4..].copy_from_slice(&message.to_le_bytes());
    m
}

fn decode_ping(message: &[u8]) -> i64 {
    let mut m = [0u8; 8];
    m.copy_from_slice(message);
    i64::from_le_bytes(m)
}
