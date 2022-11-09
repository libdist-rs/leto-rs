use anyhow::Result;
use fnv::FnvHashMap;
use mempool::MempoolMsg;
use network::{plaintcp::TcpSimpleSender, Acknowledgement};
use std::{
    net::{SocketAddr, SocketAddrV4},
    path::PathBuf,
};
use storage::rocksdb::Storage;
use tokio::sync::{mpsc::unbounded_channel, oneshot};

use crate::types::Transaction;

use super::{Id, Settings};
// use super::Config;

/// This is the server that runs the protocol
pub struct Server {}

fn to_socket_address(
    ip_str: &str,
    port_str: &str,
) -> Result<SocketAddr> {
    let addr = SocketAddrV4::new(ip_str.parse()?, port_str.parse()?);
    Ok(addr.into())
}

pub fn get_mempool_peers(
    my_id: Id,
    settings: &Settings,
) -> Result<FnvHashMap<Id, SocketAddr>> {
    let mut map = FnvHashMap::default();
    for i in 0..settings.consensus_config.num_nodes {
        let id: Id = i.into();
        if id != my_id {
            let (ip_str, port_str) = settings.consensus_config.mempool_addresses[&i].clone();
            let addr = to_socket_address(&ip_str, &port_str)?;
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
        exit: oneshot::Receiver<()>,
    ) -> anyhow::Result<()> {
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
        let (tx_mem_to_consensus, rx_mem_to_consensus) = unbounded_channel();
        let (tx_consensus_to_mem, rx_consensus_to_mem) = unbounded_channel();
        let (tx_processor, rx_processor) = unbounded_channel();
        let (tx_batcher, rx_batcher) = unbounded_channel();
        let mempool_addr = to_socket_address(
            "0.0.0.0",
            &settings.consensus_config.mempool_port.to_string(),
        )?;
        let client_addr = to_socket_address(
            "0.0.0.0",
            &settings.consensus_config.consensus_port.to_string(),
        )?;
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
            log::info!("Starting the server");
            tokio::select! {
                _ = exit => {
                    log::info!("Termination signal received. Shutting down server.");
                }
            }
            log::info!("Server is shutting down!")
        });
        Ok(())
    }
}
