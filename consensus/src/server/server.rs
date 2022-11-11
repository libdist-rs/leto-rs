use super::{Handler, Settings};
use crate::{Id, Transaction, to_socket_address};
use anyhow::{anyhow, Result};
use fnv::FnvHashMap;
use log::*;
use mempool::MempoolMsg;
use network::{
    plaintcp::{TcpReceiver, TcpSimpleSender},
    Acknowledgement,
};
use std::{
    net::SocketAddr,
    path::PathBuf,
};
use storage::rocksdb::Storage;
use tokio::sync::{mpsc::unbounded_channel, oneshot};

/// This is the server that runs the protocol
pub struct Server {}

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

impl Server {
    pub fn spawn(
        my_id: Id,
        all_ids: Vec<Id>,
        settings: Settings,
    ) -> anyhow::Result<oneshot::Sender<()>> {
        let (exit_tx, mut exit_rx) = oneshot::channel();
        let me = settings
            .consensus_config
            .get(&my_id)
            .ok_or(anyhow!("My Id is not present in the config"))?;
        let path = {
            let mut path = PathBuf::new();
            path.push(&settings.storage.base);
            let file_name = format!("{}-{}", settings.storage.prefix, my_id);
            path.set_file_name(file_name);
            path.set_extension("db");
            path
        };
        let _store = Storage::new(
            path.to_str()
                .ok_or(anyhow::anyhow!("Invalid path for storage"))?,
        )?;
        let mempool_peers = get_mempool_peers(my_id, &settings)?;
        let mempool_net =
            TcpSimpleSender::<Id, MempoolMsg<Id, Transaction>, Acknowledgement>::with_peers(
                mempool_peers,
            );
        let (tx_mem_to_consensus, mut rx_mem_to_consensus) = unbounded_channel();
        // TODO: Use this sender when implementing
        let (_tx_consensus_to_mem, rx_consensus_to_mem) = unbounded_channel();
        let (tx_processor, rx_processor) = unbounded_channel();
        let (tx_batcher, mut rx_batcher) = unbounded_channel();
        let (consensus_net_tx, mut consensus_net_rx) = unbounded_channel();
        let mempool_addr = to_socket_address("0.0.0.0", me.mempool_port)?;
        let consensus_addr = to_socket_address("0.0.0.0", me.consensus_port)?;
        let client_addr = to_socket_address("0.0.0.0", me.client_port)?;

        // Start receiver for consensus messages
        TcpReceiver::spawn(consensus_addr, Handler::new(consensus_net_tx));

        // Start the mempool
        mempool::Mempool::spawn(
            my_id,
            all_ids,
            settings.mempool_config,
            _store,
            mempool_net,
            rx_consensus_to_mem,
            tx_batcher,
            tx_processor,
            rx_processor,
            tx_mem_to_consensus,
            mempool_addr,
            client_addr,
        );

        tokio::spawn(async move {
            info!("Starting the server");
            loop {
                tokio::select! {
                    _ = &mut exit_rx => {
                        info!("Termination signal received. Shutting down server.");
                        break;
                    }
                    // Handle batch ready messages from the mempool
                    batch = rx_batcher.recv() => {
                        info!("Got a batch: {:?}", batch);
                    }
                    // Handle outputs from the mempool
                    mem_msg = rx_mem_to_consensus.recv() => {
                        info!("Got a message from the mempool: {:?}", mem_msg);
                    }
                    // Receive consensus messages from others
                    msg = consensus_net_rx.recv() => {
                        info!("Got a consensus message: {:?}", msg);
                    }
                }
            }
            info!("Server is shutting down!")
        });
        Ok(exit_tx)
    }
}
