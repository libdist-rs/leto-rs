use super::{Leto, Settings};
use crate::{to_socket_address, Id, KeyConfig};
use anyhow::anyhow;
use mempool::{Batch, MempoolMsg};
use network::{plaintcp::TcpSimpleSender, Acknowledgement};
use std::{marker::PhantomData, path::PathBuf, sync::Arc};
use storage::rocksdb::Storage;
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
    Tx: crate::types::Transaction,
{
    pub fn spawn(
        my_id: Id,
        all_ids: Vec<Id>,
        crypto_system: KeyConfig,
        settings: Settings,
        tx_commit: UnboundedSender<Arc<Batch<Tx>>>,
    ) -> anyhow::Result<oneshot::Sender<()>> {
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
        let mempool_net =
            TcpSimpleSender::<Id, MempoolMsg<Id, Tx>, Acknowledgement>::with_peers(mempool_peers);
        let mempool_addr = to_socket_address("0.0.0.0", me.mempool_port)?;
        let client_addr = to_socket_address("0.0.0.0", me.client_port)?;

        // A channel for the consensus to communicate with the mempool
        let (tx_consensus_to_mem, rx_consensus_to_mem) = unbounded_channel();
        // A channel for the mempool to communicate with the consensus
        let (tx_mem_to_consensus, rx_mem_to_consensus) = unbounded_channel();
        // A channel for the mempool to communicate with the batcher
        let (tx_mem_to_batcher, rx_mem_to_batcher) = unbounded_channel();
        // The mempool creates a processor
        // The tx_processor is used so that the consensus can send to the processor
        // The rx_processor is used by the mempool to hand-over to the processor
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
            tx_commit,
        )?;
        Ok(exit_tx)
    }
}
