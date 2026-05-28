/// Hera top-level server (full mempool + Hera consensus + self-load).
///
/// Mirrors `zeus/server.rs` with these differences:
///   - No `consensus_client_addr` socket binding.
///   - No `run_zeus_client_batch_listener` or `run_confirmation_router`.
///   - If `TPS > 0` env var, spawns `load_gen::spawn` to drive the batcher
///     internally.
///   - The mempool is still spawned so the mempool sync protocol (for future
///     use) is available; its client listener will sit idle since no external
///     clients connect.
///   - A shared `Arc<AtomicUsize>` is passed to `Hera::spawn` so the smoke test
///     can observe the maximum heads-per-committed-attestation.
///   - Two oneshot exits: one for the consensus actor, one for the data actor.
use crate::{server::Settings, types::Transaction, Id, KeyConfig};
use anyhow::{anyhow, Result};
use log::info;
use mempool::{Batch, MempoolMsg};
use serde::Serialize;
use std::{marker::PhantomData, path::PathBuf, sync::Arc};
use storage::rocksdb::Storage;
use tcp_sender::TcpSimpleSender;
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedSender},
    oneshot,
};

use super::Hera;

pub struct HeraServer<Tx> {
    _x: PhantomData<Tx>,
}

impl<Tx> HeraServer<Tx>
where
    Tx: Transaction,
{
    /// Spawn the Hera server.
    ///
    /// Returns `(exit_tx, max_committed_heads_len)`. Send `()` to `exit_tx`
    /// to shut down both the consensus and data actors.
    pub fn spawn_with_factory<F>(
        my_id: Id,
        all_ids: Vec<Id>,
        crypto_system: KeyConfig,
        settings: Settings,
        tx_commit: UnboundedSender<Arc<Batch<Tx>>>,
        tx_factory: Option<F>,
    ) -> Result<(oneshot::Sender<()>, Arc<std::sync::atomic::AtomicUsize>)>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug + 'static,
        F: Fn(Id, u64, u128) -> Tx + Send + 'static,
    {
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
        let mempool_addr = crate::to_socket_address("0.0.0.0", me.mempool_port)?;
        let client_addr = crate::to_socket_address("0.0.0.0", me.client_port)?;

        info!("HeraServer booted on {}", me.mempool_address);

        let (_tx_consensus_to_mem, rx_consensus_to_mem) = unbounded_channel();
        let (tx_mem_to_consensus, _rx_mem_to_consensus) = unbounded_channel();
        let (tx_mem_to_batcher, rx_mem_to_batcher) = unbounded_channel::<(Tx, usize)>();
        let (tx_processor, rx_processor) = unbounded_channel();

        let max_committed_heads_len = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Chain-lead rate-limit cap.  Must be >= 1 (see liveness invariant in
        // data_actor.rs: a cap of 0 would freeze the node's own data sub-chain
        // by preventing it from ever proposing a block above its committed head).
        let max_chain_lead: u64 = std::env::var("HERA_MAX_CHAIN_LEAD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(super::data_actor::DEFAULT_MAX_CHAIN_LEAD)
            .max(1);

        // Shared atomics: data actor writes, load generator reads.
        // Initialized to 0 (genesis): my_height starts at 0 and
        // committed_height starts at 0 before any commit lands.
        let my_height_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let my_committed_height_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));

        mempool::Mempool::spawn(
            my_id,
            all_ids.clone(),
            settings.mempool_config.clone(),
            store.clone(),
            mempool_net,
            rx_consensus_to_mem,
            tx_mem_to_batcher.clone(),
            tx_processor.clone(),
            rx_processor,
            tx_mem_to_consensus,
            mempool_addr,
            client_addr,
        );

        let tps: usize = std::env::var("TPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if tps > 0 {
            if let Some(factory) = tx_factory {
                super::load_gen::spawn(
                    my_id,
                    tps,
                    tx_mem_to_batcher.clone(),
                    factory,
                    Arc::clone(&my_height_atomic),
                    Arc::clone(&my_committed_height_atomic),
                    max_chain_lead,
                );
                info!("HeraServer: self-load generator spawned at TPS={}", tps);
            } else {
                info!(
                    "HeraServer: TPS={} but no factory provided — no self-load",
                    tps
                );
            }
        }

        // One exit oneshot for the consensus actor. The data actor exits
        // naturally when the consensus actor drops its cross-plane senders
        // (tx_fetch_hint, tx_commit_emit), which closes the data actor's recv channels.
        let (exit_tx, exit_rx) = oneshot::channel::<()>();

        Hera::<Tx>::spawn(
            my_id,
            crypto_system,
            all_ids,
            settings,
            store,
            exit_rx,
            rx_mem_to_batcher,
            tx_commit,
            Arc::clone(&max_committed_heads_len),
            Arc::clone(&my_height_atomic),
            Arc::clone(&my_committed_height_atomic),
            max_chain_lead,
        )?;

        Ok((exit_tx, max_committed_heads_len))
    }

    /// Convenience wrapper: spawn without a self-load factory.
    pub fn spawn(
        my_id: Id,
        all_ids: Vec<Id>,
        crypto_system: KeyConfig,
        settings: Settings,
        tx_commit: UnboundedSender<Arc<Batch<Tx>>>,
    ) -> Result<(oneshot::Sender<()>, Arc<std::sync::atomic::AtomicUsize>)>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug + 'static,
    {
        Self::spawn_with_factory::<fn(Id, u64, u128) -> Tx>(
            my_id,
            all_ids,
            crypto_system,
            settings,
            tx_commit,
            None,
        )
    }
}
