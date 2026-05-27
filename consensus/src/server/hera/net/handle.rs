//! Application-facing facade over [`Network`].
//!
//! [`Network`] hands out a fresh [`Connection`] (with a new outbound channel)
//! each time a peer (re)connects. `HeraNet` hides that churn behind a stable
//! `send`/`broadcast` API by keeping a `peer_id -> current outbound sender` map
//! that a background router task updates on every (re)connection, and by
//! funnelling every peer's inbound frames into one aggregate channel for the
//! consensus loop.
//!
//! Sends use `try_send`: a full per-peer channel means that peer is behind, so
//! the frame is **dropped** rather than blocking the consensus loop. This is
//! the shedding property that lets the cluster survive hera's flood -- the
//! redundant data plane tolerates drops, and the sig-chain keeps flowing
//! because the transport never melts.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use fnv::FnvHashMap;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::Id;

use super::network::{Network, CHANNEL_CAPACITY};

type PeerSenders = Arc<Mutex<FnvHashMap<usize, mpsc::Sender<Bytes>>>>;

#[derive(Clone)]
pub struct HeraNet {
    peers: PeerSenders,
}

impl HeraNet {
    /// Spawn the transport. `addresses` is indexed by authority id (0..n);
    /// `our_id` indexes into it; `local_addr` binds the inbound listener.
    /// Returns the facade and the aggregate inbound-frame receiver. Synchronous:
    /// must be called from within a tokio runtime.
    pub fn spawn(
        addresses: Vec<SocketAddr>,
        our_id: Id,
        local_addr: SocketAddr,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let mut network = Network::from_socket_addresses(&addresses, our_id, local_addr);
        let peers: PeerSenders = Arc::new(Mutex::new(FnvHashMap::default()));
        let (inbound_tx, inbound_rx) = mpsc::channel::<Bytes>(CHANNEL_CAPACITY);

        let peers_router = peers.clone();
        tokio::spawn(async move {
            let conn_rx = network.connection_receiver();
            while let Some(conn) = conn_rx.recv().await {
                let peer_id = conn.peer_id;
                // Replace any prior sender for this peer with the new one.
                peers_router.lock().unwrap().insert(peer_id, conn.sender);
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

        (Self { peers }, inbound_rx)
    }

    /// Best-effort send to one peer. Drops on a full or stale channel.
    pub async fn send(&self, id: Id, bytes: Bytes) {
        let sender = self.peers.lock().unwrap().get(&id).cloned();
        let Some(sender) = sender else { return };
        match sender.try_send(bytes) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // Peer is behind; shed the frame rather than block consensus.
            }
            Err(TrySendError::Closed(_)) => {
                // Stale connection; the router will install a fresh sender.
                self.peers.lock().unwrap().remove(&id);
            }
        }
    }

    /// Best-effort broadcast to the given peers.
    pub async fn broadcast(&self, peers: &[Id], bytes: Bytes) {
        for peer in peers {
            self.send(*peer, bytes.clone()).await;
        }
    }
}
