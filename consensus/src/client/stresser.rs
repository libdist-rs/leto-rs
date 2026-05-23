use super::{ClientMode, Settings};
use crate::{to_socket_address, Id};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use fnv::FnvHashMap;
use futures_util::StreamExt;
use log::*;
use rand::{thread_rng, Rng};
use std::marker::PhantomData;
use std::time::{Duration, Instant};
use tcp_broadcast::TcpBroadcastSender;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{mpsc::unbounded_channel, oneshot};

use crate::types::{ClientMsg, ZeusClientMsg};

/// This is a client implementation that stresses the BFT-system
pub struct Stressor<Tx> {
    id: Id,
    exit_rx: oneshot::Receiver<()>,
    settings: Settings,
    /// Sends raw serialized message bytes to all servers.  The phantom type is
    /// `Vec<u8>` because we serialize `ClientMsg`/`ZeusClientMsg` manually and
    /// pass raw `Bytes` — the phantom is not used on the wire.
    ///
    /// Switched from `TcpSimpleSender` to `TcpBroadcastSender` so that
    /// `broadcast_with_faults` can return after the BFT quorum of `n - t`
    /// peers has accepted the message, dropping pending sends to slow/dead
    /// peers via the worker-side cancel flag.  This is what unblocks the
    /// client when `--crashes N` removes some nodes.
    consensus_sender: TcpBroadcastSender<Id, Vec<u8>>,
    /// Receives committed per-tx hash `Hash<Tx>` from the confirmation listener
    /// task (server emits `Confirmation(Hash<Tx>)` per committed tx).
    /// The listener deserializes the server reply and forwards the hash here.
    confirmation_rx: UnboundedReceiver<Hash<Tx>>,
    _x: PhantomData<Tx>,
}

impl<Tx> Stressor<Tx>
where
    Tx: super::MockTx,
{
    pub fn spawn(
        my_id: Id,
        settings: Settings,
    ) -> Result<oneshot::Sender<()>> {
        // NOTE: Used for benchmarking
        info!("Transactions size: {} B", settings.bench_config.tx_size);
        info!(
            "Transactions rate: {} tx/s",
            (settings.bench_config.txs_per_burst as u64 * 1000)
                / settings.bench_config.burst_interval_ms
        );

        let (exit_tx, exit_rx) = oneshot::channel();

        // Build the peer map: server Id → consensus_client_port address.
        // The stressor sends `NewBatch` to the server's `consensus_client_port`.
        let mut peer_map: FnvHashMap<Id, std::net::SocketAddr> = FnvHashMap::default();
        let all_ids = settings.consensus_config.get_all_ids();
        for id in &all_ids {
            let party = settings
                .consensus_config
                .get(id)
                .ok_or_else(|| anyhow!("Unknown party [{}]", id))?;
            let consensus_addr = to_socket_address(&party.address, party.port)?;
            peer_map.insert(*id, consensus_addr);
        }
        debug!("Using servers: {:?}", peer_map);

        // `consensus_sender` is typed with `Vec<u8>` phantom — we serialize
        // manually and pass raw `Bytes`.
        let consensus_sender = TcpBroadcastSender::<Id, Vec<u8>>::with_peers(peer_map);

        // Bind confirmation listener on my_confirmation_address:my_confirmation_port.
        // Port 0 lets the OS pick a free ephemeral port (for in-process harness).
        let my_addr = to_socket_address(
            &settings.my_confirmation_address,
            settings.my_confirmation_port,
        )?;
        let (confirmation_tx, confirmation_rx) = unbounded_channel::<Hash<Tx>>();

        // Spawn a mode-aware confirmation listener task.
        // The listener knows the client mode and uses the correct message type.
        // Confirmation listener: server emits `Confirmation(Hash<Tx>)` per
        // committed tx (not per batch — libmempool re-batches across NewBatch
        // boundaries so per-batch hashes never match server-side).
        match &settings.client_mode {
            ClientMode::LetoBroadcast => {
                let tx = confirmation_tx;
                tokio::spawn(async move {
                    let mut receiver = tcp_receiver::TcpReceiver::<ClientMsg<Tx>>::spawn(my_addr);
                    while let Some(result) = receiver.next().await {
                        match result {
                            Ok(ClientMsg::Confirmation(h)) => {
                                if tx.send(h).is_err() {
                                    break;
                                }
                            }
                            Ok(ClientMsg::BatchConfirmation(_)) => {
                                // Legacy per-batch path — unused; ignore.
                            }
                            Ok(_) => {}
                            Err(e) => {
                                debug!("Leto confirmation receiver: decode error: {:?}", e);
                            }
                        }
                    }
                });
            }
            ClientMode::ZeusEleaderOnly { .. } => {
                let tx = confirmation_tx;
                tokio::spawn(async move {
                    let mut receiver =
                        tcp_receiver::TcpReceiver::<ZeusClientMsg<Tx>>::spawn(my_addr);
                    while let Some(result) = receiver.next().await {
                        match result {
                            Ok(ZeusClientMsg::Confirmation(h)) => {
                                if tx.send(h).is_err() {
                                    break;
                                }
                            }
                            Ok(ZeusClientMsg::BatchConfirmation(_)) => {
                                // Legacy per-batch path — unused; ignore.
                            }
                            Ok(_) => {}
                            Err(e) => {
                                debug!("Zeus confirmation receiver: decode error: {:?}", e);
                            }
                        }
                    }
                });
            }
        }

        // Start the client
        tokio::spawn(async move {
            Self {
                id: my_id,
                exit_rx,
                settings,
                consensus_sender,
                confirmation_rx,
                _x: PhantomData,
            }
            .run()
            .await
        });
        Ok(exit_tx)
    }

    async fn run(&mut self) -> Result<()> {
        // Get stress settings
        let burst_tx = self.settings.bench_config.txs_per_burst;
        let tx_size = self.settings.bench_config.tx_size;
        let all_ids = self.settings.consensus_config.get_all_ids();
        let emit_dp = self.settings.bench_config.emit_dp;
        let window_secs = self.settings.bench_config.bench_emit_window_secs.max(1);

        // Confirmation listener address included in NewBatch so the server
        // knows where to send BatchConfirmation.
        let my_confirmation_addr: std::net::SocketAddr = to_socket_address(
            &self.settings.my_confirmation_address,
            self.settings.my_confirmation_port,
        )?;

        // Resolve the eleader target for ZeusEleaderOnly mode.
        let target_eleader: Option<Id> = match &self.settings.client_mode {
            ClientMode::LetoBroadcast => None,
            ClientMode::ZeusEleaderOnly {
                eleader_id: Some(id),
            } => {
                debug!("Zeus stressor: using pre-seeded eleader id={}", id);
                Some(*id)
            }
            ClientMode::ZeusEleaderOnly { eleader_id: None } => {
                warn!(
                    "Zeus stressor: WhoIsEleader query sent but reply channel not wired \
                     (the in-process harness should always pre-seed eleader_id). \
                     Falling back to node 0 as eleader."
                );
                Some(0)
            }
        };

        // NOTE: This log entry is used to compute performance.
        info!("Start sending transactions");

        let mut tx_id: usize = 0;
        // Burst timer
        let mut burst_timer = tokio::time::interval(Duration::from_millis(
            self.settings.bench_config.burst_interval_ms,
        ));
        #[cfg(feature = "microbench")]
        let mut first = true;
        let mut sample_id: u64 = thread_rng().gen();

        // DP[Latency] state: batch_hash → send Instant.
        let mut send_ts: FnvHashMap<Hash<Tx>, Instant> = FnvHashMap::default();
        // Latency samples (microseconds) collected in the current window.
        let mut latency_hist: Vec<u64> = Vec::new();
        let mut emit_interval = tokio::time::interval(Duration::from_secs(window_secs));
        // Drop the immediate first tick so the first window is a full window.
        emit_interval.tick().await;

        loop {
            tokio::select! {
                _ = &mut self.exit_rx => {
                    info!("Shutting down the client");
                    break;
                }
                _ = burst_timer.tick() => {
                    // Build one batch of `burst_tx` transactions.
                    let mut batch: Vec<Tx> = Vec::with_capacity(burst_tx);
                    for i in 0..burst_tx {
                        let tx = Tx::mock_transaction(
                            tx_id,
                            self.id,
                            tx_size,
                            i == 0,
                            sample_id,
                        );
                        #[cfg(feature = "benchmark")]
                        {
                            if i == 0 {
                                info!("Sending sample transaction {}", sample_id);
                            }
                        }
                        #[cfg(feature = "microbench")]
                        {
                            if first {
                                info!(
                                    "Tx size: {}",
                                    bincode::serialized_size(&tx)?,
                                );
                                first = false;
                            }
                        }
                        batch.push(tx);
                        tx_id += 1;
                    }

                    // Per-tx send-time tracking — server emits a Confirmation
                    // per committed tx, looked up against this map.  All txs
                    // in a burst share the same Instant; resolution loss is
                    // bounded by the OS clock granularity (negligible vs.
                    // network/commit latency).
                    let now = Instant::now();
                    for tx in &batch {
                        let h: Hash<Tx> = Hash::ser_and_hash(tx);
                        send_ts.insert(h, now);
                    }

                    match &self.settings.client_mode {
                        ClientMode::LetoBroadcast => {
                            let msg = ClientMsg::<Tx>::NewBatch {
                                batch,
                                reply_to: my_confirmation_addr,
                            };
                            let bytes = bytes::Bytes::from(
                                bincode::serialize(&msg).map_err(anyhow::Error::new)?,
                            );
                            // BFT quorum: f = (n - 1) / 3.  Wait for n - f peers
                            // to accept the message; cancel pending sends to the
                            // (up to f) slow/dead peers.  Per-tx Confirmations
                            // from any n - f alive servers drive forward progress.
                            let n = all_ids.len();
                            let fault_threshold = n.saturating_sub(1) / 3;
                            let _ = self.consensus_sender
                                .broadcast_with_faults(&all_ids, bytes, fault_threshold)
                                .await;
                        }
                        ClientMode::ZeusEleaderOnly { .. } => {
                            let eleader =
                                target_eleader.expect("eleader resolved above");
                            let msg = ZeusClientMsg::<Tx>::NewBatch {
                                batch,
                                reply_to: my_confirmation_addr,
                            };
                            let bytes = bytes::Bytes::from(
                                bincode::serialize(&msg).map_err(anyhow::Error::new)?,
                            );
                            // Single-target enqueue.  `send` returns immediately
                            // with a CancelHandle; we drop the handle here, which
                            // is the "fire and forget" path — the worker will
                            // deliver if the eleader is reachable, or the per-peer
                            // mpsc fills and subsequent sends drop on the floor
                            // (best-effort).  If the eleader is dead, Zeus's
                            // sig-chain eleader-blame mechanism handles recovery;
                            // the client should re-resolve `target_eleader` after
                            // a view-change (TODO: not yet wired).
                            let _ = self.consensus_sender.send(eleader, bytes);
                        }
                    }

                    sample_id += 1;
                }
                tx_hash = self.confirmation_rx.recv() => {
                    let h = match tx_hash {
                        Some(h) => h,
                        None => break,
                    };
                    if let Some(sent) = send_ts.remove(&h) {
                        latency_hist.push(sent.elapsed().as_micros() as u64);
                    }
                }
                _ = emit_interval.tick(), if emit_dp => {
                    if !latency_hist.is_empty() {
                        let med = median_us(&latency_hist) as f64 / 1000.0;
                        eprintln!("DP[Latency]: {}", med);
                        latency_hist.clear();
                    }
                }
            }
        }
        Ok(())
    }
}

/// Returns the median of a non-empty slice of microsecond values.
fn median_us(hist: &[u64]) -> u64 {
    let mut v = hist.to_vec();
    v.sort_unstable();
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2
    } else {
        v[mid]
    }
}
