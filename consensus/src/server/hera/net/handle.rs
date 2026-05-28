//! Application-facing facade over [`Network`].
//!
//! [`Network`] hands out a fresh [`Connection`] (with a new outbound channel)
//! each time a peer (re)connects. `HeraNet` hides that churn behind a stable
//! `send`/`broadcast` API by keeping a `peer_id -> current outbound sender` map
//! that a background router task updates on every (re)connection, and by
//! funnelling every peer's inbound frames into one aggregate channel for the
//! consensus loop.
//!
//! **Per-plane channel discipline (non-negotiable):**
//! - Sig plane: `HeraNet::spawn_sig` — unbounded per-peer outbox, never drop. A
//!   dropped SigPropose/Blame/BlameQC has no per-message retransmit; the
//!   sig-chain wedges at large n. Unbounded is memory-safe because volume is
//!   O(n)/round and the sig actor is never flooded.
//! - Data plane: `HeraNet::spawn_data` — bounded(1000) per-peer outbox,
//!   try_send-drop on full. The O(n²) data flood is loss-tolerant
//!   (DataRequest/DataResponse catch-up); shedding prevents OOM.
//!
//! Both planes use `HeraNet::spawn_with_mode` under the hood and get the
//! same persistent-connection management (jittered reconnect, ping/pong, etc.).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use fnv::FnvHashMap;
use log::warn;
use tokio::sync::mpsc;

use crate::Id;

use super::network::{ChannelMode, Network, OutboundSender, CHANNEL_CAPACITY};

/// If the intra-node data-intake send (per-peer pump → actor aggregate) blocks
/// at least this long, log it: the data actor is draining slower than the mesh
/// is delivering — a slow-drain liveness signal.
const BACKPRESSURE_STALL_WARN: Duration = Duration::from_millis(50);

/// Log one outbox-shed `warn!` per this many drops (rate-limit so a persistently
/// slow/dead peer cannot flood the log while still surfacing the liveness issue).
const DROP_LOG_EVERY: u64 = 256;

type PeerSenders = Arc<Mutex<FnvHashMap<usize, OutboundSender>>>;

/// PROFILE INSTRUMENTATION: cumulative count of outbound frames dropped because
/// the per-peer bounded(1000) channel was full. Incremented on every
/// try_send failure in HeraNet::send. Readable via
/// HeraNet::outbound_dropped_total().
static OUTBOUND_DROPPED_TOTAL: AtomicU64 = AtomicU64::new(0);

/// PROFILE INSTRUMENTATION: cumulative count of successful outbound sends.
static OUTBOUND_SENT_TOTAL: AtomicU64 = AtomicU64::new(0);

/// PROFILE INSTRUMENTATION: cumulative count of frames NOT sent because the peer
/// was not in the connected map (data-mesh gap; no retransmit). Distinct from
/// OUTBOUND_DROPPED_TOTAL (outbox-full shed).
static OUTBOUND_NOTCONN_TOTAL: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct HeraNet {
    peers: PeerSenders,
    /// Count of distinct peers that have connected at least once. Lets the
    /// protocol wait for the mesh to form before broadcasting its bootstrap
    /// proposal (a proposal sent before the mesh is up is dropped to
    /// not-yet-connected peers, with no retransmission -- wedging large n).
    connected: Arc<AtomicUsize>,
}

impl HeraNet {
    /// Spawn the sig-plane transport (unbounded per-peer outbox, never drop).
    ///
    /// Use for SigPropose/Blame/BlameQC/SigElement messages. Volume is
    /// O(n)/round and the sig actor is never flooded, so unbounded is
    /// memory-safe.
    pub fn spawn_sig(
        addresses: Vec<SocketAddr>,
        our_id: Id,
        local_addr: SocketAddr,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        Self::spawn_with_mode(addresses, our_id, local_addr, ChannelMode::Unbounded)
    }

    /// Spawn the data-plane transport (bounded(1000) per-peer outbox,
    /// try_send-drop).
    ///
    /// Use for DataPropose/DataRequest/DataResponse. The O(n²) data flood is
    /// loss-tolerant (refetchable). Shedding prevents OOM.
    pub fn spawn_data(
        addresses: Vec<SocketAddr>,
        our_id: Id,
        local_addr: SocketAddr,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        Self::spawn_with_mode(
            addresses,
            our_id,
            local_addr,
            ChannelMode::Bounded(CHANNEL_CAPACITY),
        )
    }

    /// Spawn the transport. `addresses` is indexed by authority id (0..n);
    /// `our_id` indexes into it; `local_addr` binds the inbound listener.
    /// `mode` selects per-peer outbox discipline. Synchronous:
    /// must be called from within a tokio runtime.
    pub fn spawn_with_mode(
        addresses: Vec<SocketAddr>,
        our_id: Id,
        local_addr: SocketAddr,
        mode: ChannelMode,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let mut network = Network::from_socket_addresses(&addresses, our_id, local_addr, mode);
        let peers: PeerSenders = Arc::new(Mutex::new(FnvHashMap::default()));
        let connected = Arc::new(AtomicUsize::new(0));
        let (inbound_tx, inbound_rx) = mpsc::channel::<Bytes>(CHANNEL_CAPACITY);

        let peers_router = peers.clone();
        let connected_router = connected.clone();
        tokio::spawn(async move {
            let conn_rx = network.connection_receiver();
            while let Some(conn) = conn_rx.recv().await {
                let peer_id = conn.peer_id;
                // Replace any prior sender for this peer with the new one; count
                // the first connection from each distinct peer.
                let first_time = peers_router
                    .lock()
                    .unwrap()
                    .insert(peer_id, conn.sender)
                    .is_none();
                if first_time {
                    connected_router.fetch_add(1, Ordering::Relaxed);
                }
                // Pump this connection's inbound frames into the aggregate
                // channel until the connection dies (receiver closes).
                let inbound_tx = inbound_tx.clone();
                let mut receiver = conn.receiver;
                tokio::spawn(async move {
                    while let Some(frame) = receiver.recv().await {
                        if inbound_tx.send(frame).await.is_err() {
                            break; // consensus side gone
                        }
                    }
                });
            }
        });

        (Self { peers, connected }, inbound_rx)
    }

    /// Spawn the **data-plane** transport with **per-peer processing**.
    ///
    /// Unlike `spawn_with_mode` (one shared inbound channel of raw `Bytes`),
    /// this runs `processor` inside each peer's inbound-pump task, so the
    /// expensive part (bincode deserialize + ed25519 verify) **parallelizes
    /// across peers/cores** instead of serializing on one task — mirroring
    /// Mysticeti's per-connection verification (`net_sync.rs`). Each pump
    /// forwards `processor(frame) == Some(item)` onto a bounded aggregate
    /// channel with `try_send` (data-plane shedding: drop on full). The outbox
    /// stays bounded(1000) try_send-drop (data-plane discipline).
    ///
    /// `processor` is shared (`Arc`) across all pumps, so it must be
    /// `Fn + Send + Sync + 'static` (it reads the immutable pubkey set by `&`).
    pub fn spawn_data_processed<T, F>(
        addresses: Vec<SocketAddr>,
        our_id: Id,
        local_addr: SocketAddr,
        out_cap: usize,
        outbox_cap: usize,
        processor: F,
    ) -> (Self, mpsc::Receiver<T>)
    where
        T: Send + 'static,
        F: Fn(Bytes) -> Option<T> + Send + Sync + 'static,
    {
        let mut network = Network::from_socket_addresses(
            &addresses,
            our_id,
            local_addr,
            ChannelMode::Bounded(outbox_cap),
        );
        let peers: PeerSenders = Arc::new(Mutex::new(FnvHashMap::default()));
        let connected = Arc::new(AtomicUsize::new(0));
        let (out_tx, out_rx) = mpsc::channel::<T>(out_cap);
        let processor = Arc::new(processor);

        let peers_router = peers.clone();
        let connected_router = connected.clone();
        tokio::spawn(async move {
            let conn_rx = network.connection_receiver();
            while let Some(conn) = conn_rx.recv().await {
                let peer_id = conn.peer_id;
                let first_time = peers_router
                    .lock()
                    .unwrap()
                    .insert(peer_id, conn.sender)
                    .is_none();
                if first_time {
                    connected_router.fetch_add(1, Ordering::Relaxed);
                }
                // Per-peer verify+forward task: `processor` (deserialize +
                // verify) runs here, so it parallelizes across peers/cores.
                let out_tx = out_tx.clone();
                let processor = processor.clone();
                let mut receiver = conn.receiver;
                tokio::spawn(async move {
                    while let Some(frame) = receiver.recv().await {
                        if let Some(item) = processor(frame) {
                            // Intra-node backpressure: block (do NOT drop) when
                            // the actor is behind. A blocked pump stops draining
                            // this peer's socket → TCP backpressure to the sender,
                            // which sheds at its OWN outbox (inter-node). The rule
                            // is: block within a node, shed between nodes — so a
                            // straggler never wedges, it just falls behind and
                            // catches up via refetch.
                            let send_start = Instant::now();
                            match out_tx.send(item).await {
                                Ok(()) => {
                                    let waited = send_start.elapsed();
                                    if waited >= BACKPRESSURE_STALL_WARN {
                                        warn!(
                                            "HeraNet: data intake backpressured {} ms on peer {} \
                                             (verified-block aggregate saturated — actor draining slowly)",
                                            waited.as_millis(),
                                            peer_id
                                        );
                                    }
                                }
                                Err(_closed) => break, // data actor gone
                            }
                        }
                    }
                });
            }
        });

        (Self { peers, connected }, out_rx)
    }

    /// Best-effort send to one peer. Returns `true` if the frame was enqueued,
    /// `false` if it was shed (bounded outbox full) or the peer is not yet
    /// connected — so callers that know the message type can log a typed drop.
    /// - For sig plane (unbounded): always enqueues.
    /// - For data plane (bounded): drops on a full channel (inter-node shed).
    pub async fn send(
        &self,
        id: Id,
        bytes: Bytes,
    ) -> bool {
        let outcome = {
            let guard = self.peers.lock().unwrap();
            // Can't hold the lock across await; send under the lock.
            // OutboundSender is not Clone. For sig plane this is a non-blocking
            // unbounded send; for data plane try_send is also non-blocking.
            guard.get(&id).map(|s| s.try_send(bytes))
        };
        match outcome {
            Some(true) => {
                OUTBOUND_SENT_TOTAL.fetch_add(1, Ordering::Relaxed);
                true
            }
            Some(false) => {
                // Inter-node shed: the bounded outbox to this peer is full.
                // Log the FIRST drop (n==1) — even one lost block can strand a
                // node — then rate-limit so a persistently-slow peer can't spam.
                let n = OUTBOUND_DROPPED_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 || n % DROP_LOG_EVERY == 0 {
                    warn!(
                        "HeraNet: SHED data frame to peer {} (outbox full); cumulative drops={}",
                        id, n
                    );
                }
                false
            }
            // Peer not in the connected map: silent no-op (no per-peer channel
            // exists). This is the data-plane analogue of a lost frame — and it
            // has NO retransmit. Log first + rate-limited so a data-mesh-not-yet-
            // formed gap (which strands a gate with no drop/can't-serve) is visible.
            None => {
                let n = OUTBOUND_NOTCONN_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 || n % DROP_LOG_EVERY == 0 {
                    warn!(
                        "HeraNet: data frame to peer {} NOT SENT (peer not connected; no retransmit); cumulative={}",
                        id, n
                    );
                }
                false
            }
        }
    }

    /// Best-effort broadcast to the given peers. Returns the number of peers the
    /// frame was shed to (outbox full / not connected) — 0 in the happy path.
    pub async fn broadcast(
        &self,
        peers: &[Id],
        bytes: Bytes,
    ) -> usize {
        let mut shed = 0usize;
        for peer in peers {
            if !self.send(*peer, bytes.clone()).await {
                shed += 1;
            }
        }
        shed
    }

    /// Number of distinct peers that have connected at least once.
    pub fn connected_peers(&self) -> usize {
        self.connected.load(Ordering::Relaxed)
    }

    /// PROFILE INSTRUMENTATION: snapshot the cumulative outbound-drop counter.
    pub fn outbound_dropped_total() -> u64 {
        OUTBOUND_DROPPED_TOTAL.load(Ordering::Relaxed)
    }

    /// PROFILE INSTRUMENTATION: snapshot the cumulative outbound-sent counter.
    pub fn outbound_sent_total() -> u64 {
        OUTBOUND_SENT_TOTAL.load(Ordering::Relaxed)
    }
}
