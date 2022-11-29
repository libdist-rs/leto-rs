use super::{get_consensus_peers, BatcherConsensusMsg, Handler, Parameters, RRBatcher};
use crate::{
    to_socket_address,
    types::{self, ProtocolMsg},
    Id, KeyConfig, Round,
};
use anyhow::{anyhow, Result};
use log::*;
use mempool::{Batch, BatchHash, ConsensusMempoolMsg};
use network::{
    plaintcp::{TcpReceiver, TcpReliableSender},
    Acknowledgement,
};
use storage::rocksdb::Storage;
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    oneshot,
};

mod proposal;
pub use proposal::*;

mod round_context;
pub use round_context::*;

pub struct Leto<Tx> {
    my_id: Id,
    /// Crypto Keys
    crypto_system: KeyConfig, 
    broadcast_peers: Vec<Id>, // cache
    exit_rx: oneshot::Receiver<()>,
    rx_mem_to_consensus: UnboundedReceiver<BatchHash<Tx>>,
    rx_net_to_consensus: UnboundedReceiver<ProtocolMsg<Id, Tx, Round>>,
    store: Storage,
    consensus_net: TcpReliableSender<
        Id, 
        ProtocolMsg<Id, Tx, Round>, 
        Acknowledgement
    >,
    tx_consensus_to_mem: UnboundedSender<ConsensusMempoolMsg<Id, Round, Tx>>,
    tx_consensus_to_batcher: UnboundedSender<BatcherConsensusMsg<Id, Tx>>,
    round_context: RoundContext,
    tx_msg_loopback: UnboundedSender<ProtocolMsg<Id, Tx, Round>>,
    rx_msg_loopback: UnboundedReceiver<ProtocolMsg<Id, Tx, Round>>,
}

impl<Tx> Leto<Tx> {
    pub const INITIAL_LEADER: Id = Id::START;
    pub const INITIAL_ROUND: Round = Round::START;
}

impl<Tx> Leto<Tx>
where
    Tx: types::Transaction,
{
    pub fn spawn(
        my_id: Id,
        crypto_system: KeyConfig,
        all_peers: Vec<Id>,
        settings: super::Settings,
        store: Storage,
        exit_rx: oneshot::Receiver<()>,
        rx_mem_to_consensus: UnboundedReceiver<BatchHash<Tx>>,
        rx_mem_to_batcher: UnboundedReceiver<(Tx, usize)>,
        tx_processor: UnboundedSender<Batch<Tx>>,
        tx_consensus_to_mem: UnboundedSender<ConsensusMempoolMsg<Id, Round, Tx>>,
    ) -> Result<()> {
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
        let consensus_net = TcpReliableSender::<
            Id,
            ProtocolMsg<Id, Tx, Round>,
            Acknowledgement,
        >::with_peers(consensus_peers);

        // Start the batcher
        let (tx_consensus_to_batcher, rx_consensus_to_batcher) = unbounded_channel();
        let batching_params = Parameters::new(
            my_id.clone(),
            Leto::<Tx>::INITIAL_LEADER,
            settings.bench_config.batch_size,
            settings.bench_config.batch_timeout,
        );
        RRBatcher::<Id, Tx>::spawn(
            batching_params,
            rx_mem_to_batcher,
            rx_consensus_to_batcher,
            tx_processor,
        )?;

        let all_peers_except_me = all_peers
            .into_iter()
            .filter(|x| x != &my_id)
            .collect();

        let (tx_msg_loopback, rx_msg_loopback) = unbounded_channel();
        tokio::spawn(async move {
            let res = Leto::<Tx> {
                my_id,
                crypto_system,
                broadcast_peers: all_peers_except_me,
                exit_rx,
                rx_mem_to_consensus,
                rx_net_to_consensus,
                store,
                consensus_net,
                tx_consensus_to_mem,
                tx_consensus_to_batcher,
                round_context: RoundContext::new(
                    Round::START, 
                    Id::START
                ),
                tx_msg_loopback,
                rx_msg_loopback,
            }
            .run()
            .await;
            if let Err(e) = res {
                error!("Consensus error: {}", e);
            }
        });
        Ok(())
    }

    async fn run(&mut self) -> Result<()> {
        info!("Starting the server");
        loop {
            let res = tokio::select! {
                exit_val = &mut self.exit_rx => {
                    let _ = exit_val.map_err(anyhow::Error::new)?;
                    info!("Termination signal received by the server. Exiting.");
                    break
                }
                // Handle batch ready messages from the mempool
                batch_hash = self.rx_mem_to_consensus.recv() => {
                    let batch_hash = batch_hash.ok_or(
                        anyhow!("Mempool processor has shut down")
                    )?;
                    self.handle_new_batch(batch_hash).await                
                }
                // Receive consensus messages from loopback
                msg = self.rx_msg_loopback.recv() => {
                    let msg = msg.ok_or(
                        anyhow!("Loopback layer has closed")
                    )?;
                    info!("Got a consensus message from loopback: {:?}", msg);
                    self.handle_msg(msg).await
                }
                // Receive consensus messages from others
                msg = self.rx_net_to_consensus.recv() => {
                    let msg = msg.ok_or(
                        anyhow!("Networking layer has closed")
                    )?;
                    info!("Got a consensus message from the network: {:?}", msg);
                    let res = self.handle_msg(msg).await;
                    res
                }
            };
            if let Err(e) = res {
                error!("Consensus error: {}", e);
            }
        }
        info!("Server is shutting down!");
        Ok(())
    }

    async fn handle_msg(
        &mut self,
        msg: ProtocolMsg<Id, Tx, Round>,
    ) -> Result<()>
    {
        match msg {
            ProtocolMsg::Propose { proposal, auth } => self.handle_proposal(proposal, auth),
            ProtocolMsg::Relay { proposal, auth } => self.handle_proposal(proposal, auth),
            ProtocolMsg::Blame { round, auth } => self.handle_blame(round, auth),
        }
    }
}
