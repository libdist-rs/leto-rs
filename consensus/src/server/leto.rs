use crate::{Id, Round, types::ProtocolMsg, to_socket_address};
use log::*;
use mempool::{BatchHash, Transaction, Batch, ConsensusMempoolMsg};
use network::{Message, plaintcp::{TcpReceiver, TcpReliableSender}, Acknowledgement};
use storage::rocksdb::Storage;
use tokio::sync::{oneshot, mpsc::{UnboundedReceiver, unbounded_channel, UnboundedSender}};
use anyhow::{anyhow, Result};
use super::{Handler, get_consensus_peers, Parameters, RRBatcher};

pub struct Leto<Data, Tx> {
    my_id: Id,
    exit_rx: oneshot::Receiver<()>,
    rx_mem_to_consensus: UnboundedReceiver<BatchHash<Tx>>,
    rx_net_to_consensus: UnboundedReceiver<ProtocolMsg<Id, Data, Round>>,
    store: Storage,
}

impl<Data, Tx> Leto<Data, Tx> 
{
    pub const INITIAL_LEADER: Id = Id::START;
    pub const INITIAL_ROUND: Round = Round::START;
}

impl<Data, Tx> Leto<Data, Tx> 
where 
    Tx: Transaction,
    Data: Message,
{
    pub fn spawn(
        my_id: Id,
        settings: super::Settings,
        store: Storage,
        exit_rx: oneshot::Receiver<()>,
        rx_mem_to_consensus: UnboundedReceiver<BatchHash<Tx>>,
        rx_mem_to_batcher: UnboundedReceiver<(Tx, usize)>,
        tx_processor: UnboundedSender<Batch<Tx>>,
        tx_consensus_to_mem: UnboundedSender<ConsensusMempoolMsg<Id, Round, Tx>>,
    ) -> Result<()>
    {
        let me = settings
            .consensus_config
            .get(&my_id)
            .ok_or(anyhow!("My Id is not present in the config"))?;
        let consensus_addr = to_socket_address(
            "0.0.0.0", 
            me.consensus_port
        )?;

        let (tx_net_to_consensus, rx_net_to_consensus) = unbounded_channel();

        // Start receiver for consensus messages
        TcpReceiver::spawn(
            consensus_addr, 
            Handler::new(tx_net_to_consensus)
        );

        // Start outgoing connections
        let consensus_peers = get_consensus_peers(my_id, &settings)?;
        let consensus_net = TcpReliableSender::<Id, ProtocolMsg<Id, Data, Round>, Acknowledgement>::with_peers(consensus_peers);

        // Start the batcher
        let (tx_consensus_to_batcher, rx_consensus_to_batcher) = unbounded_channel();
        let batching_params = Parameters::new(
            my_id.clone(), 
            Leto::<Data, Tx>::INITIAL_LEADER, 
            settings.bench_config.batch_size, 
            settings.bench_config.batch_timeout
        );
        RRBatcher::<Id, Tx>::spawn(
            batching_params, 
            rx_mem_to_batcher, 
            rx_consensus_to_batcher,
            tx_processor,
        )?;

        tokio::spawn(async move {
            let res = Leto { 
                my_id,
                exit_rx,
                rx_mem_to_consensus,
                rx_net_to_consensus,
                store,
            }.run().await;
            if let Err(e) = res {
                error!("Consensus error: {}", e);
            }
        });
        Ok(())
    }

    async fn run(
        &mut self,
    ) -> Result<()> 
    {
        info!("Starting the server");
        loop {
            tokio::select! {
                _ = &mut self.exit_rx => {
                    info!("Termination signal received by the server. Exiting.");
                    break;
                }
                // Handle batch ready messages from the mempool
                batch_hash = self.rx_mem_to_consensus.recv() => {
                    let batch_hash = batch_hash.ok_or(
                        anyhow!("Mempool processor has shut down")
                    )?;
                    info!("Got a batch hash: {}", batch_hash);
                    // TODO: Further processing
                }
                // Receive consensus messages from others
                msg = self.rx_net_to_consensus.recv() => {
                    info!("Got a consensus message: {:?}", msg);
                }
            }
        }
        info!("Server is shutting down!");
        Ok(())
    }
}