use crate::{Id, Round, types::ProtocolMsg, to_socket_address};
use log::*;
use mempool::{BatchHash, Transaction, Batch, ConsensusMempoolMsg};
use network::{plaintcp::{TcpReceiver, TcpReliableSender}, Acknowledgement};
use storage::rocksdb::Storage;
use tokio::sync::{oneshot, mpsc::{UnboundedReceiver, unbounded_channel, UnboundedSender}};
use anyhow::{anyhow, Result};
use super::{Handler, get_consensus_peers, Parameters, RRBatcher, BatcherConsensusMsg};

pub struct Leto<Tx> {
    my_id: Id,
    exit_rx: oneshot::Receiver<()>,
    rx_mem_to_consensus: UnboundedReceiver<BatchHash<Tx>>,
    rx_net_to_consensus: UnboundedReceiver<ProtocolMsg<Id, Tx, Round>>,
    store: Storage,
    tx_consensus_to_mem: UnboundedSender<ConsensusMempoolMsg<Id, Round, Tx>>,
    consensus_net: TcpReliableSender<Id, ProtocolMsg<Id, Tx, Round>, Acknowledgement>,
    tx_consensus_to_batcher: UnboundedSender<BatcherConsensusMsg<Id, Tx>>,

    /// State
    round_ctx: RoundContext,
}

impl<Tx> Leto<Tx> 
{
    pub const INITIAL_LEADER: Id = Id::START;
    pub const INITIAL_ROUND: Round = Round::START;
}

mod proposal;
pub use proposal::*;

mod round_context;
pub use round_context::*;

impl<Tx> Leto<Tx> 
where 
    Tx: Transaction,
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
        TcpReceiver::<Acknowledgement, ProtocolMsg<Id, Tx, Round>, _>::spawn(
            consensus_addr, 
            Handler::<Id, Tx, Round>::new(tx_net_to_consensus)
        );

        // Start outgoing connections
        let consensus_peers = get_consensus_peers(my_id, &settings)?;
        let consensus_net = TcpReliableSender::<Id, ProtocolMsg<Id, Tx, Round>, Acknowledgement>::with_peers(consensus_peers);

        // Start the batcher
        let (tx_consensus_to_batcher, rx_consensus_to_batcher) = unbounded_channel();
        let batching_params = Parameters::new(
            my_id.clone(), 
            Leto::<Tx>::INITIAL_LEADER, 
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
            let res = Leto::<Tx> { 
                my_id,
                exit_rx,
                rx_mem_to_consensus,
                rx_net_to_consensus,
                store,
                tx_consensus_to_mem,
                consensus_net,
                tx_consensus_to_batcher,
                round_ctx: RoundContext::default(),
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
                    debug!("Got a batch hash: {}", batch_hash);
                    // Further processing (propose)
                    self.handle_new_batch(batch_hash)?;
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