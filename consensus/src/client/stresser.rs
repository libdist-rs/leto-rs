use std::marker::PhantomData;
use std::time::Duration;

use super::Settings;
use crate::types::{self};
use crate::{to_socket_address, Id};
use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use fnv::FnvHashMap;
use futures_util::SinkExt;
use log::*;
use network::NetSender;
use network::{
    plaintcp::{TcpReceiver, TcpSimpleSender},
    Acknowledgement,
};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedSender},
    oneshot,
};

/// This is a client implementation that stresses the BFT-system
pub struct Stressor<Tx> {
    id: Id,
    exit_rx: oneshot::Receiver<()>,
    settings: Settings,
    consensus_sender: TcpSimpleSender<Id, Tx, Acknowledgement>,
    consensus_rx: UnboundedReceiver<Tx>,
    _x: PhantomData<Tx>,
}


impl<Tx> Stressor<Tx> 
where
    Tx: super::MockTx,
{
    pub fn spawn(
        my_id: Id,
        settings: Settings,
    ) -> Result<oneshot::Sender<()>> {
        let (exit_tx, exit_rx) = oneshot::channel();

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
        debug!("Using servers: {:?}", peer_map);
        let consensus_sender = TcpSimpleSender::<Id, Tx, Acknowledgement>::with_peers(peer_map);

        // Networking setup
        let (consensus_tx, consensus_rx) = unbounded_channel();
        let my_addr = to_socket_address(
            "0.0.0.0", 
            settings.port
        )?;
        TcpReceiver::spawn(
            my_addr, 
            Handler::<Tx>::new(consensus_tx)
        );

        // Start the client
        tokio::spawn(async move {
            Self {
                id: my_id,
                exit_rx,
                settings,
                consensus_sender,
                consensus_rx,
                _x: PhantomData,
            }.run().await
        });
        Ok(exit_tx)
    }

    async fn run(&mut self)-> Result<()>
    {
        // Get stress settings
        let burst_tx = self.settings.bench_config.txs_per_burst;
        let tx_size = self.settings.bench_config.tx_size;
        let all_ids = self.settings.consensus_config.get_all_ids();

        // Start the client
        let mut tx_id: usize = 0;
        // Burst timer
        let mut burst_timer = tokio::time::interval(Duration::from_millis(
            self.settings.bench_config.burst_interval_ms,
        ));
        loop {
            tokio::select! {
                _ = &mut self.exit_rx => {
                    info!("Shutting down the client");
                    break;
                }
                _ = burst_timer.tick() => {
                    // Time to send a burst of transactions
                    // Send `burst_tx` transactions every interval
                    for _i in 0..burst_tx {
                        let tx = Tx::mock_transaction(tx_id, self.id, tx_size);
                        tx_id = tx_id + 1;
                        self.consensus_sender.broadcast(
                            tx,
                            &all_ids, // SendAll
                        ).await;
                    }
                }
                confirmation = self.consensus_rx.recv() => {
                    info!("Received a confirmation message: {:?}", confirmation);
                    // TODO: Handle tx confirmation
                }
            }
        };
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Handler<Tx> {
    tx: UnboundedSender<Tx>,
}

impl<Tx> Handler<Tx> {
    pub fn new(tx: UnboundedSender<Tx>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl<Tx> network::Handler<Acknowledgement, Tx> for Handler<Tx>
where
    Tx: types::Transaction,
{
    async fn dispatch(
        &self,
        msg: Tx,
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
