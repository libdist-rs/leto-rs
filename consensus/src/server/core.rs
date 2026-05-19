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
        let (tx_batch_sender_map, mut rx_batch_sender_map) =
            unbounded_channel::<(Hash<Vec<Tx>>, SocketAddr)>();

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
        // Receives committed batches and (batch_hash → SocketAddr) registrations;
        // sends BatchConfirmation back to the client.
        tokio::spawn(async move {
            Self::run_confirmation_router(&mut rx_batch_sender_map, &mut rx_commit_for_confirm)
                .await;
        });

        // Spawn the client batch listener on consensus_client_port.
        tokio::spawn(async move {
            Self::run_client_batch_listener::<Tx>(
                consensus_client_addr,
                tx_batcher_for_client,
                tx_batch_sender_map,
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
    /// send `BatchConfirmation` to the recorded client address.
    ///
    /// Cancel-handler note: `TcpSimpleSender::send` returns
    /// `Result<(), TcpSimpleSenderError>` (not a `CancelHandler`), so no
    /// handler tracking is required here.
    async fn run_confirmation_router(
        rx_batch_sender_map: &mut tokio::sync::mpsc::UnboundedReceiver<(Hash<Vec<Tx>>, SocketAddr)>,
        rx_commit_for_confirm: &mut tokio::sync::mpsc::UnboundedReceiver<Arc<Batch<Tx>>>,
    ) where
        Tx: Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        // Map: batch_hash → client SocketAddr
        let mut pending: FnvHashMap<Hash<Vec<Tx>>, SocketAddr> = FnvHashMap::default();
        // TcpSimpleSender keyed by SocketAddr for unreliable one-shot replies.
        let mut reply_sender: TcpSimpleSender<SocketAddr, ClientMsg<Tx>> =
            TcpSimpleSender::with_peers(FnvHashMap::default());

        loop {
            tokio::select! {
                reg = rx_batch_sender_map.recv() => {
                    match reg {
                        Some((hash, addr)) => { pending.insert(hash, addr); }
                        None => break,
                    }
                }
                batch = rx_commit_for_confirm.recv() => {
                    let batch = match batch {
                        Some(b) => b,
                        None => break,
                    };
                    let batch_hash: Hash<Vec<Tx>> =
                        Hash::ser_and_hash(&batch.payload);
                    if let Some(addr) = pending.remove(&batch_hash) {
                        let msg = ClientMsg::<Tx>::BatchConfirmation(batch_hash);
                        if let Ok(bytes) = bincode::serialize(&msg) {
                            // Ensure the peer map knows this address.
                            if !reply_sender.get_peers().contains_key(&addr) {
                                let mut new_peers = reply_sender.get_peers().clone();
                                new_peers.insert(addr, addr);
                                reply_sender = TcpSimpleSender::with_peers(new_peers);
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

    /// Listen on `consensus_client_port` for `ClientMsg<Tx>`.
    /// On `NewBatch`: forward each tx to the batcher and register the batch
    /// hash → reply_to in the confirmation router.
    async fn run_client_batch_listener<T>(
        addr: SocketAddr,
        tx_batcher: tokio::sync::mpsc::UnboundedSender<(T, usize)>,
        tx_batch_sender_map: tokio::sync::mpsc::UnboundedSender<(Hash<Vec<T>>, SocketAddr)>,
    ) where
        T: Transaction,
    {
        let mut receiver = tcp_receiver::TcpReceiver::<ClientMsg<T>>::spawn(addr);
        while let Some(result) = receiver.next().await {
            match result {
                Ok(ClientMsg::NewBatch { batch, reply_to }) => {
                    let batch_hash: Hash<Vec<T>> = Hash::ser_and_hash(&batch);
                    // Register hash → reply_to before forwarding txs.
                    let _ = tx_batch_sender_map.send((batch_hash, reply_to));
                    // Inject each tx into the batcher.
                    for tx in batch {
                        let size = bincode::serialized_size(&tx).unwrap_or(0) as usize;
                        let _ = tx_batcher.send((tx, size));
                    }
                }
                Ok(ClientMsg::NewTx { tx, reply_to: _ }) => {
                    // Legacy single-tx path — forward to batcher only.
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
