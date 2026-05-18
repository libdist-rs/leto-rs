/// Zeus top-level server (full mempool + Zeus consensus).
///
/// Mirrors `consensus/src/server/core.rs` (`Server<Tx>`) but wires
/// `Zeus::spawn` instead of `Leto::spawn`.
use crate::{server::Settings, to_socket_address, Id, KeyConfig};
use anyhow::{anyhow, Result};
use log::info;
use mempool::{Batch, MempoolMsg};
use std::{marker::PhantomData, path::PathBuf, sync::Arc};
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
    Tx: crate::types::Transaction,
{
    pub fn spawn(
        my_id: Id,
        all_ids: Vec<Id>,
        crypto_system: KeyConfig,
        settings: Settings,
        tx_commit: UnboundedSender<Arc<Batch<Tx>>>,
    ) -> Result<oneshot::Sender<()>>
    where
        Tx: Clone + serde::Serialize + PartialEq + std::fmt::Debug + 'static,
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
        info!("ZeusServer booted on {}", me.mempool_address);

        let (_tx_consensus_to_mem, rx_consensus_to_mem) = unbounded_channel();
        let (tx_mem_to_consensus, _rx_mem_to_consensus) = unbounded_channel();
        // Eleader batcher receives raw (Tx, size) pairs from the mempool
        let (tx_mem_to_batcher, rx_mem_to_batcher) = unbounded_channel();
        let (tx_processor, rx_processor) = unbounded_channel();

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

        // Start Zeus
        let (exit_tx, exit_rx) = oneshot::channel();
        Zeus::<Tx>::spawn(
            my_id,
            crypto_system,
            all_ids,
            settings,
            store,
            exit_rx,
            rx_mem_to_batcher,
            tx_commit,
        )?;

        Ok(exit_tx)
    }
}
