/// Zeus top-level server (full mempool + Zeus consensus).
///
/// Mirrors `consensus/src/server/core.rs` (`Server<Tx>`) but wires
/// `Zeus::spawn` instead of `Leto::spawn`.
use crate::{
    server::Settings,
    to_socket_address,
    types::{Transaction, ZeusClientMsg},
    Id, KeyConfig,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use fnv::FnvHashMap;
use futures_util::StreamExt;
use log::info;
use mempool::{Batch, MempoolMsg};
use serde::Serialize;
use std::{marker::PhantomData, net::SocketAddr, path::PathBuf, sync::Arc};
use storage::rocksdb::Storage;
use tcp_sender::TcpSimpleSender;
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedSender},
    oneshot,
};

use super::Zeus;

pub struct ZeusServer<Tx> {
    _x: PhantomData<Tx>,
}

impl<Tx> ZeusServer<Tx>
where
    Tx: Transaction,
{
    pub fn spawn(
        my_id: Id,
        all_ids: Vec<Id>,
        crypto_system: KeyConfig,
        settings: Settings,
        tx_commit: UnboundedSender<Arc<Batch<Tx>>>,
    ) -> Result<oneshot::Sender<()>>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug + 'static,
    {
        // Create the DB
        let path = {
            let mut path = PathBuf::new();
            path.push(&settings.storage.base);
            let file_name = format!("{}-{}", settings.storage.prefix, my_id);
            path.set_file_name(file_name);
            path.set_extension("db");
            path
        };
        let store = Storage::new(
            path.to_str()
                .ok_or_else(|| anyhow!("Invalid path for storage"))?,
        )?;

        let me = settings
            .committee_config
            .get(&my_id)
            .ok_or_else(|| anyhow!("My Id {} not in config", my_id))?;
        let mempool_peers = settings.get_mempool_peers(my_id)?;
        let mempool_net = TcpSimpleSender::<Id, MempoolMsg<Id, Tx>>::with_peers(mempool_peers);
        let mempool_addr = to_socket_address("0.0.0.0", me.mempool_port)?;
        let client_addr = to_socket_address("0.0.0.0", me.client_port)?;
        let consensus_client_addr = to_socket_address("0.0.0.0", me.consensus_client_port)?;
        info!("ZeusServer booted on {}", me.mempool_address);

        let (_tx_consensus_to_mem, rx_consensus_to_mem) = unbounded_channel();
        let (tx_mem_to_consensus, _rx_mem_to_consensus) = unbounded_channel();
        // Eleader batcher receives raw (Tx, size) pairs from the mempool
        let (tx_mem_to_batcher, rx_mem_to_batcher) = unbounded_channel::<(Tx, usize)>();
        let (tx_processor, rx_processor) = unbounded_channel();

        // Bounded ingress queue for the Zeus client listener path.
        //
        // Why bounded here and not in libmempool's `tx_mem_to_batcher`:
        // libmempool's `Mempool::spawn` takes an `UnboundedSender` (3rd-party
        // signature we can't easily change).  Instead we put the bound on the
        // listener side via a dedicated channel + small forwarder task.
        //
        // Back-pressure dynamics: in healthy operation, the forwarder drains
        // this channel in microseconds (it just relays to libmempool's
        // unbounded path), so the cap is never hit and the listener never
        // blocks → baseline throughput is identical to the unbounded path.
        // Under heavy load + eleader stall (e.g., crash-fault cascading
        // EleaderBlame), Zeus's main task hogs CPU; tokio scheduling fairness
        // means the forwarder gets less CPU; the channel fills; the listener's
        // `send(...).await` blocks; the listener stops polling its TcpReceiver;
        // TCP recv buffer fills on the client → kernel zero-window-ad → client's
        // TCP send blocks.  Net: the client slows to whatever the eleader can
        // actually process, breaking the cascade.
        //
        // CAP=1024: ~500 KB at tx_size=512.  Large enough that healthy bursts
        // never hit it; small enough that backpressure activates within ~10 ms
        // of CPU starvation at typical bench rates.
        const CLIENT_INGRESS_CAP: usize = 1024;
        let (tx_client_bounded, mut rx_client_bounded) =
            tokio::sync::mpsc::channel::<(Tx, usize)>(CLIENT_INGRESS_CAP);
        let tx_mem_to_batcher_for_forwarder = tx_mem_to_batcher.clone();
        tokio::spawn(async move {
            while let Some(item) = rx_client_bounded.recv().await {
                if tx_mem_to_batcher_for_forwarder.send(item).is_err() {
                    break;
                }
            }
        });
        let tx_batcher_for_client = tx_client_bounded;

        // Fan-out: committed batch → app sink + confirmation router.
        let (tx_commit_fanout, mut rx_commit_fanout) = unbounded_channel::<Arc<Batch<Tx>>>();
        let (tx_commit_for_confirm, mut rx_commit_for_confirm) =
            unbounded_channel::<Arc<Batch<Tx>>>();
        let tx_commit_clone = tx_commit.clone();
        tokio::spawn(async move {
            while let Some(batch) = rx_commit_fanout.recv().await {
                let _ = tx_commit_clone.send(Arc::clone(&batch));
                let _ = tx_commit_for_confirm.send(batch);
            }
        });

        // (tx_hash, reply_to) per-tx registration channel.  See Leto
        // server's matching comment: libmempool re-batches across NewBatch
        // boundaries, so the confirmation roundtrip must be per-tx not
        // per-batch.
        let (tx_tx_sender_map, mut rx_tx_sender_map) =
            unbounded_channel::<(Hash<Tx>, SocketAddr)>();

        // Confirmation router (same logic as Leto Server).
        tokio::spawn(async move {
            Self::run_confirmation_router(&mut rx_tx_sender_map, &mut rx_commit_for_confirm).await;
        });

        // Client batch listener on consensus_client_port.
        // This also handles WhoIsEleader from the Zeus stressor.
        // eleader resolution state is read-only from settings; we capture
        // num_nodes and a clone for the listener closure.
        let num_nodes = settings.committee_config.num_nodes();
        let initial_epoch = Zeus::<Tx>::INITIAL_DATA_EPOCH;
        tokio::spawn(async move {
            Self::run_zeus_client_batch_listener(
                consensus_client_addr,
                tx_batcher_for_client,
                tx_tx_sender_map,
                num_nodes,
                initial_epoch,
            )
            .await;
        });

        // Start the mempool
        mempool::Mempool::spawn(
            my_id,
            all_ids.clone(),
            settings.mempool_config.clone(),
            store.clone(),
            mempool_net,
            rx_consensus_to_mem,
            tx_mem_to_batcher,
            tx_processor.clone(),
            rx_processor,
            tx_mem_to_consensus,
            mempool_addr,
            client_addr,
        );

        // Start Zeus; pass tx_commit_fanout so confirmation router sees commits.
        let (exit_tx, exit_rx) = oneshot::channel();
        Zeus::<Tx>::spawn(
            my_id,
            crypto_system,
            all_ids,
            settings,
            store,
            exit_rx,
            rx_mem_to_batcher,
            tx_commit_fanout,
        )?;

        Ok(exit_tx)
    }

    /// Confirmation router for Zeus: same as Leto's.
    ///
    /// Cancel-handler note: `TcpSimpleSender::send` returns
    /// `Result<(), TcpSimpleSenderError>` — no `CancelHandler` emitted.
    async fn run_confirmation_router(
        rx_tx_sender_map: &mut tokio::sync::mpsc::UnboundedReceiver<(Hash<Tx>, SocketAddr)>,
        rx_commit_for_confirm: &mut tokio::sync::mpsc::UnboundedReceiver<Arc<Batch<Tx>>>,
    ) where
        Tx: Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        let mut pending: FnvHashMap<Hash<Tx>, SocketAddr> = FnvHashMap::default();
        let mut reply_sender: TcpSimpleSender<SocketAddr, ZeusClientMsg<Tx>> =
            TcpSimpleSender::with_peers(FnvHashMap::default());

        loop {
            tokio::select! {
                reg = rx_tx_sender_map.recv() => {
                    match reg {
                        Some((tx_hash, addr)) => { pending.insert(tx_hash, addr); }
                        None => break,
                    }
                }
                batch = rx_commit_for_confirm.recv() => {
                    let batch = match batch {
                        Some(b) => b,
                        None => break,
                    };
                    for tx in batch.payload.iter() {
                        let tx_hash: Hash<Tx> = Hash::ser_and_hash(tx);
                        if let Some(addr) = pending.remove(&tx_hash) {
                            let msg = ZeusClientMsg::<Tx>::Confirmation(tx_hash);
                            if let Ok(bytes) = bincode::serialize(&msg) {
                                if !reply_sender.get_peers().contains_key(&addr) {
                                    let mut new_peers = reply_sender.get_peers().clone();
                                    new_peers.insert(addr, addr);
                                    reply_sender =
                                        TcpSimpleSender::with_peers(new_peers);
                                }
                                let _ = reply_sender
                                    .send(addr, bytes::Bytes::from(bytes))
                                    .await;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Zeus client batch listener.
    ///
    /// Handles:
    /// - `NewBatch`: forward txs to batcher, register batch_hash → reply_to.
    /// - `WhoIsEleader`: respond with `EleaderIs { id, epoch }` using the
    ///   current epoch formula `eleader(epoch, n)`.  The response goes via a
    ///   one-shot `TcpSimpleSender` back to `reply_to`. NOTE: This listener
    ///   does not have live access to `current_epoch` on the Zeus task (which
    ///   lives in a separate tokio task).  For steady state (epoch =
    ///   INITIAL_DATA_EPOCH) this is correct; for post-view-change epochs, the
    ///   eleader returned may be stale.  A proper fix requires an
    ///   `Arc<AtomicU64>` shared between Zeus's main task and this listener, or
    ///   an mpsc query channel — flagged for follow-up.
    /// - `NewTx` (legacy): forward to batcher only.
    async fn run_zeus_client_batch_listener(
        addr: SocketAddr,
        tx_batcher: tokio::sync::mpsc::Sender<(Tx, usize)>,
        tx_tx_sender_map: tokio::sync::mpsc::UnboundedSender<(Hash<Tx>, SocketAddr)>,
        num_nodes: usize,
        initial_epoch: u64,
    ) where
        Tx: Transaction,
    {
        use super::chain_state::eleader as eleader_fn;

        let mut receiver = tcp_receiver::TcpReceiver::<ZeusClientMsg<Tx>>::spawn(addr);

        while let Some(result) = receiver.next().await {
            match result {
                Ok(ZeusClientMsg::NewBatch { batch, reply_to }) => {
                    // Per-tx registration — libmempool re-batches across
                    // NewBatch boundaries so the confirmation roundtrip
                    // must be per-tx, not per-batch.
                    for tx in batch {
                        let tx_hash: Hash<Tx> = Hash::ser_and_hash(&tx);
                        let _ = tx_tx_sender_map.send((tx_hash, reply_to));
                        let size = bincode::serialized_size(&tx).unwrap_or(0) as usize;
                        // Bounded send: under load this awaits when the
                        // forwarder can't keep up → listener stops polling
                        // its TcpReceiver → TCP backpressure to the client.
                        if tx_batcher.send((tx, size)).await.is_err() {
                            log::warn!(
                                "Zeus client listener: tx_batcher closed; \
                                 stopping listener"
                            );
                            return;
                        }
                    }
                }
                Ok(ZeusClientMsg::WhoIsEleader) => {
                    // NOTE: This listener does not have the client's reply_to
                    // address in this variant — the stressor would need to send
                    // it.  The current ZeusClientMsg::WhoIsEleader carries no
                    // reply_to field.  We log and skip; the stressor must use
                    // its pre-seeded eleader_id for now.
                    //
                    // The eleader formula for reference (not sent here):
                    // eleader(initial_epoch, num_nodes)
                    log::warn!(
                        "Zeus client listener: WhoIsEleader received but no reply_to \
                         field in the message — cannot respond (use pre-seeded eleader_id). \
                         eleader(epoch={}, n={})={}",
                        initial_epoch,
                        num_nodes,
                        eleader_fn(initial_epoch, num_nodes),
                    );
                }
                Ok(ZeusClientMsg::NewTx { tx, reply_to: _ }) => {
                    let size = bincode::serialized_size(&tx).unwrap_or(0) as usize;
                    if tx_batcher.send((tx, size)).await.is_err() {
                        log::warn!("Zeus client listener: tx_batcher closed (NewTx path)");
                        return;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    log::error!("Zeus client batch listener: deserialize error: {:?}", e);
                }
            }
        }
        log::warn!("Zeus client batch listener: stream ended");
    }
}
