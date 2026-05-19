use super::{Leto, Settings};
use crate::{
    to_socket_address,
    types::{ClientMsg, Transaction},
    Id, KeyConfig,
};
use anyhow::anyhow;
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

/// This is the server that runs the protocol
pub struct Server<Tx> {
    _x: PhantomData<Tx>,
}

impl<Tx> Server<Tx>
where
    Tx: Transaction,
{
    pub fn spawn(
        my_id: Id,
        all_ids: Vec<Id>,
        crypto_system: KeyConfig,
        settings: Settings,
        tx_commit: UnboundedSender<Arc<Batch<Tx>>>,
    ) -> anyhow::Result<oneshot::Sender<()>> {
        #[cfg(feature = "benchmark")]
        settings.bench_log();

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
                .ok_or_else(|| anyhow!("Invalid path [{}] for storage", path.display()))?,
        )?;

        // Create the mempool
        let me = settings
            .committee_config
            .get(&my_id)
            .ok_or_else(|| anyhow!("My Id {} is not present in the config", my_id))?;
        let mempool_peers = settings.get_mempool_peers(my_id)?;
        let mempool_net = TcpSimpleSender::<Id, MempoolMsg<Id, Tx>>::with_peers(mempool_peers);
        let mempool_addr = to_socket_address("0.0.0.0", me.mempool_port)?;
        let client_addr = to_socket_address("0.0.0.0", me.client_port)?;
        let consensus_client_addr = to_socket_address("0.0.0.0", me.consensus_client_port)?;
        info!("Server booted on {}", me.mempool_address);

        // A channel for the consensus to communicate with the mempool
        let (tx_consensus_to_mem, rx_consensus_to_mem) = unbounded_channel();
        // A channel for the mempool to communicate with the consensus
        let (tx_mem_to_consensus, rx_mem_to_consensus) = unbounded_channel();
        // A channel for the mempool to communicate with the batcher
        let (tx_mem_to_batcher, rx_mem_to_batcher) = unbounded_channel::<(Tx, usize)>();
        // The mempool creates a processor
        // The tx_processor is used so that the consensus can send to the processor
        // The rx_processor is used by the mempool to hand-over to the processor
        let (tx_processor, rx_processor) = unbounded_channel();

        // Clone batcher sender so the client-batch listener can also inject txs.
        let tx_batcher_for_client = tx_mem_to_batcher.clone();

        // Channel: committed batch notifications → confirmation router.
        // The commit context will fan the committed batch to both tx_commit (app
        // sink) and tx_commit_for_confirm (confirmation router).
        let (tx_commit_for_confirm, mut rx_commit_for_confirm) =
            unbounded_channel::<Arc<Batch<Tx>>>();

        // Channel: client-batch-listener → confirmation router maps.
        // Carries (batch_hash, reply_to_addr) inserted by the batch listener.
        let (tx_tx_sender_map, mut rx_tx_sender_map) =
            unbounded_channel::<(Hash<Tx>, SocketAddr)>();

        // Fan-out channel: sit between CommitContext and the two downstream
        // consumers (tx_commit and tx_commit_for_confirm).
        let (tx_commit_fanout, mut rx_commit_fanout) = unbounded_channel::<Arc<Batch<Tx>>>();

        // Spawn the fan-out task.
        let tx_commit_clone = tx_commit.clone();
        tokio::spawn(async move {
            while let Some(batch) = rx_commit_fanout.recv().await {
                let _ = tx_commit_clone.send(Arc::clone(&batch));
                let _ = tx_commit_for_confirm.send(batch);
            }
        });

        // Spawn the confirmation router.
        // Receives committed batches and (tx_hash → SocketAddr) per-tx
        // registrations; sends Confirmation(Hash<Tx>) back per committed tx.
        tokio::spawn(async move {
            Self::run_confirmation_router(&mut rx_tx_sender_map, &mut rx_commit_for_confirm)
                .await;
        });

        // Spawn the client batch listener on consensus_client_port.
        tokio::spawn(async move {
            Self::run_client_batch_listener::<Tx>(
                consensus_client_addr,
                tx_batcher_for_client,
                tx_tx_sender_map,
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
            tx_processor.clone(), // A channel to send to the processor
            rx_processor,         // Because the mempool spawns the processor
            tx_mem_to_consensus,
            mempool_addr,
            client_addr,
        );

        // Start the Leto consensus protocol
        let (exit_tx, exit_rx) = oneshot::channel();
        Leto::<Tx>::spawn(
            my_id,
            crypto_system,
            all_ids,
            settings,
            store,
            exit_rx,
            rx_mem_to_consensus,
            rx_mem_to_batcher,
            tx_processor,
            tx_consensus_to_mem,
            tx_commit_fanout, // → fan-out task
        )?;

        Ok(exit_tx)
    }

    /// Receive `(batch_hash, reply_to)` registrations from the batch listener
    /// and committed-batch notifications from the commit fan-out.  On a match,
    /// send a per-tx `Confirmation(Hash<Tx>)` to each originating client
    /// for every committed transaction.
    ///
    /// Per-tx (not per-batch) because libmempool's batcher re-groups txs
    /// across NewBatch boundaries — the committed batch's hash never
    /// matches the client's original NewBatch hash.  Per-tx confirmations
    /// let the Stressor compute latency from `tx_hash → send_ts`.
    ///
    /// Cancel-handler note: `TcpSimpleSender::send` returns
    /// `Result<(), TcpSimpleSenderError>` (not a `CancelHandler`), so no
    /// handler tracking is required here.
    async fn run_confirmation_router(
        rx_tx_sender_map: &mut tokio::sync::mpsc::UnboundedReceiver<(Hash<Tx>, SocketAddr)>,
        rx_commit_for_confirm: &mut tokio::sync::mpsc::UnboundedReceiver<Arc<Batch<Tx>>>,
    ) where
        Tx: Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        // Map: per-tx hash → originating client SocketAddr.
        let mut pending: FnvHashMap<Hash<Tx>, SocketAddr> = FnvHashMap::default();
        // TcpSimpleSender keyed by SocketAddr for unreliable one-shot replies.
        let mut reply_sender: TcpSimpleSender<SocketAddr, ClientMsg<Tx>> =
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
                    // For each committed tx, look up its originating client
                    // and send a per-tx Confirmation.
                    for tx in batch.payload.iter() {
                        let tx_hash: Hash<Tx> = Hash::ser_and_hash(tx);
                        if let Some(addr) = pending.remove(&tx_hash) {
                            let msg = ClientMsg::<Tx>::Confirmation(tx_hash);
                            if let Ok(bytes) = bincode::serialize(&msg) {
                                if !reply_sender.get_peers().contains_key(&addr) {
                                    let mut new_peers = reply_sender.get_peers().clone();
                                    new_peers.insert(addr, addr);
                                    reply_sender =
                                        TcpSimpleSender::with_peers(new_peers);
                                }
                                // Unreliable send — drop errors (confirmation loss tolerable).
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

    /// Listen on `consensus_client_port` for `ClientMsg<Tx>`.
    /// On `NewBatch`: forward each tx to the batcher and register the batch
    /// hash → reply_to in the confirmation router.
    async fn run_client_batch_listener<T>(
        addr: SocketAddr,
        tx_batcher: tokio::sync::mpsc::UnboundedSender<(T, usize)>,
        tx_tx_sender_map: tokio::sync::mpsc::UnboundedSender<(Hash<T>, SocketAddr)>,
    ) where
        T: Transaction,
    {
        let mut receiver = tcp_receiver::TcpReceiver::<ClientMsg<T>>::spawn(addr);
        while let Some(result) = receiver.next().await {
            match result {
                Ok(ClientMsg::NewBatch { batch, reply_to }) => {
                    // Per-tx registration so confirmation_router can route a
                    // Confirmation(Hash<Tx>) back to each tx's originating
                    // client.  libmempool re-batches across NewBatch
                    // boundaries, so per-batch hash mapping would never hit.
                    for tx in batch {
                        let tx_hash: Hash<T> = Hash::ser_and_hash(&tx);
                        let _ = tx_tx_sender_map.send((tx_hash, reply_to));
                        let size = bincode::serialized_size(&tx).unwrap_or(0) as usize;
                        let _ = tx_batcher.send((tx, size));
                    }
                }
                Ok(ClientMsg::NewTx { tx, reply_to }) => {
                    let tx_hash: Hash<T> = Hash::ser_and_hash(&tx);
                    let _ = tx_tx_sender_map.send((tx_hash, reply_to));
                    let size = bincode::serialized_size(&tx).unwrap_or(0) as usize;
                    let _ = tx_batcher.send((tx, size));
                }
                Ok(_) => {}
                Err(e) => {
                    log::error!("Leto client batch listener: deserialize error: {:?}", e);
                }
            }
        }
        log::warn!("Leto client batch listener: stream ended");
    }
}
