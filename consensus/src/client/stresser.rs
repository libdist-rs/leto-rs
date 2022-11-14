use std::time::Duration;

use super::Settings;
use crate::{client::ClientMsg, to_socket_address, types::Data, Id, Transaction};
use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use fnv::FnvHashMap;
use futures_util::SinkExt;
use log::*;
use network::{
    plaintcp::{TcpReceiver, TcpSimpleSender},
    Acknowledgement, NetSender,
};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedSender},
    oneshot,
};

/// This is a client implementation that stresses the BFT-system
pub struct Stressor {
    pub id: Id,
}

/*
 * TODO: Implement ConsensusHandler
 * TODO: Implement Client
 * TODO: Use the args
 * TODO: Add data size
 */

// Generates a mock transaction with this Id
fn mock_transaction(id: usize) -> Transaction {
    let data = Data::new(id.to_le_bytes().to_vec());
    Transaction {
        data,
        extra: vec![],
    }
}

impl Stressor {
    pub fn spawn(
        _my_id: Id,
        settings: Settings,
    ) -> Result<oneshot::Sender<()>> {
        let mut peer_map = FnvHashMap::default();
        // These are all server Ids
        let all_ids = settings.consensus_config.get_all_ids();
        for id in &all_ids {
            let party = settings
                .consensus_config
                .get(id)
                .ok_or(anyhow!("Unknown party [Possibly corrupt settings]"))?;
            let consensus_addr = to_socket_address(&party.address, party.port)?;
            peer_map.insert(id.clone(), consensus_addr);
        }

        let burst = settings.bench_config.txs_per_burst;
        let my_addr = to_socket_address("0.0.0.0", settings.port)?;

        let (consensus_tx, mut consensus_rx) = unbounded_channel();
        TcpReceiver::spawn(my_addr, Handler::new(consensus_tx));
        let (exit_tx, mut exit_rx) = oneshot::channel();
        let mut consensus_sender =
            TcpSimpleSender::<Id, Transaction, Acknowledgement>::with_peers(peer_map);
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_millis(
                settings.bench_config.burst_interval_ms,
            ));
            let mut test_id: usize = 0;
            loop {
                tokio::select! {
                    _ = &mut exit_rx => {
                        info!("Shutting down the client");
                        break;
                    }
                    _ = timer.tick() => {
                        // Time to send a burst of transactions
                        // TODO: Send X every interval
                        for _i in 0..burst {
                            let tx = mock_transaction(test_id);
                            test_id = test_id + 1;
                            consensus_sender.broadcast(
                                tx,
                                &all_ids, // SendAll
                            ).await;
                        }
                    }
                    confirmation = consensus_rx.recv() => {
                        info!("Received a confirmation message: {:?}", confirmation);
                    }
                }
            }
        });
        Ok(exit_tx)
    }
}

#[derive(Debug, Clone)]
struct Handler {
    tx: UnboundedSender<ClientMsg>,
}

impl Handler {
    pub fn new(tx: UnboundedSender<ClientMsg>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl network::Handler<Acknowledgement, ClientMsg> for Handler {
    async fn dispatch(
        &self,
        msg: ClientMsg,
        writer: &mut network::Writer<Acknowledgement>,
    ) {
        // Forward the message
        self.tx
            .send(msg)
            .expect("Failed to send message to the consensus channel");

        // Acknowledge
        writer
            .send(Acknowledgement::Pong)
            .await
            .expect("Failed to send an acknowledgement");
    }
}
