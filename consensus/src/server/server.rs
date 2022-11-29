use super::{Leto, Settings};
use crate::{to_socket_address, Id, KeyConfig};
use anyhow::{anyhow, Result};
use fnv::FnvHashMap;
use mempool::MempoolMsg;
use network::{plaintcp::TcpSimpleSender, Acknowledgement};
use std::{marker::PhantomData, net::SocketAddr, path::PathBuf};
use storage::rocksdb::Storage;
use tokio::sync::{mpsc::unbounded_channel, oneshot};

/// This is the server that runs the protocol
pub struct Server<Transaction> {
    _x: PhantomData<Transaction>,
}

pub fn get_mempool_peers(
    my_id: Id,
    settings: &Settings,
) -> Result<FnvHashMap<Id, SocketAddr>> {
    let mut map = FnvHashMap::default();
    for i in 0..settings.consensus_config.num_nodes() {
        let id: Id = i.into();
        if id != my_id {
            let party = settings
                .consensus_config
                .get(&id)
                .ok_or(anyhow!("Id not found"))?;
            let ip_str = &party.mempool_address;
            let addr = to_socket_address(ip_str, party.mempool_port)?;
            map.insert(id, addr.into());
        }
    }
    Ok(map)
}

pub fn get_consensus_peers(
    my_id: Id,
    settings: &Settings,
) -> Result<FnvHashMap<Id, SocketAddr>> {
    let mut map = FnvHashMap::default();
    for i in 0..settings.consensus_config.num_nodes() {
        let id: Id = i.into();
        if id != my_id {
            let party = settings
                .consensus_config
                .get(&id)
                .ok_or(anyhow!("Id not found"))?;
            let ip_str = &party.consensus_address;
            let addr = to_socket_address(ip_str, party.consensus_port)?;
            map.insert(id, addr.into());
        }
    }
    Ok(map)
}

impl<Transaction> Server<Transaction>
where
    Transaction: crate::types::Transaction,
{
    pub fn spawn(
        my_id: Id,
        all_ids: Vec<Id>,
        crypto_system: KeyConfig,
        settings: Settings,
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
                .ok_or(anyhow::anyhow!("Invalid path for storage"))?,
        )?;

        // Create the mempool
        let me = settings
            .consensus_config
            .get(&my_id)
            .ok_or(anyhow!("My Id is not present in the config"))?;
        let mempool_peers = get_mempool_peers(my_id, &settings)?;
        let mempool_net =
            TcpSimpleSender::<Id, MempoolMsg<Id, Transaction>, Acknowledgement>::with_peers(
                mempool_peers,
            );
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
        Leto::<Transaction>::spawn(
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
        )?;
        Ok(exit_tx)
    }
}
