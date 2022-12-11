use crate::server::{
    get_consensus_peers, BatcherConsensusMsg, Handler, Parameters, RRBatcher, Settings,
};
use crate::types::Block;
use crate::{
    to_socket_address,
    types::{self, ProtocolMsg},
    Id, KeyConfig, Round,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use fnv::FnvHashMap;
use log::*;
use mempool::{Batch, BatchHash, ConsensusMempoolMsg};
use network::plaintcp::CancelHandler;
use network::{
    plaintcp::{TcpReceiver, TcpReliableSender},
    Acknowledgement,
};
use storage::rocksdb::Storage;
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    oneshot,
};

use super::{ChainState, LeaderContext, QuorumWaiter, RoundContext};

pub struct Leto<Tx> {
    // Static state
    pub(crate) my_id: Id,
    /// Crypto Keys
    pub(crate) crypto_system: KeyConfig,
    /// A collection of all peers except `my_id` for use during broadcast
    pub(crate) broadcast_peers: Vec<Id>,
    /// Settings
    pub(crate) _settings: Settings,

    // Channel handlers
    pub(crate) exit_rx: oneshot::Receiver<()>,
    pub(crate) rx_mem_to_consensus: UnboundedReceiver<BatchHash<Tx>>,
    pub(crate) rx_net_to_consensus: UnboundedReceiver<ProtocolMsg<Id, Tx, Round>>,
    pub(crate) consensus_net: TcpReliableSender<Id, ProtocolMsg<Id, Tx, Round>, Acknowledgement>,
    pub(crate) _tx_consensus_to_mem: UnboundedSender<ConsensusMempoolMsg<Id, Round, Tx>>,
    pub(crate) tx_consensus_to_batcher: UnboundedSender<BatcherConsensusMsg<Id, Tx>>,
    pub(crate) tx_msg_loopback: UnboundedSender<ProtocolMsg<Id, Tx, Round>>,
    pub(crate) rx_msg_loopback: UnboundedReceiver<ProtocolMsg<Id, Tx, Round>>,
    pub(crate) cancel_handlers: FnvHashMap<Round, Vec<CancelHandler>>,

    /* Helpers */
    /// This object is used to wait for n-f messages to be delivered
    pub(crate) _quorum_waiter: QuorumWaiter,

    /* Variable State */
    /// Round State
    pub(crate) round_context: RoundContext<Tx>,
    /// Leader State
    pub(crate) leader_context: LeaderContext,
    /// Chain state
    pub(crate) chain_state: ChainState<Tx>,
}

impl<Tx> Leto<Tx> {
    pub const INITIAL_LEADER: Id = Id::START;
    pub const INITIAL_ROUND: Round = Round::START;
    pub const GENESIS_BLOCK: Block<Tx> =
        Block::<Tx>::new(BatchHash::<Tx>::EMPTY_HASH, Hash::<Block<Tx>>::EMPTY_HASH);
}

impl<Tx> Leto<Tx>
where
    Tx: types::Transaction,
{
    pub fn spawn(
        my_id: Id,
        crypto_system: KeyConfig,
        all_peers: Vec<Id>,
        settings: Settings,
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
        let consensus_addr = to_socket_address("0.0.0.0", me.consensus_port)?;

        let (tx_net_to_consensus, rx_net_to_consensus) = unbounded_channel();

        // Start receiver for consensus messages
        TcpReceiver::<Acknowledgement, ProtocolMsg<Id, Tx, Round>, _>::spawn(
            consensus_addr,
            Handler::<Id, Tx, Round>::new(tx_net_to_consensus),
        );

        // Start outgoing connections
        let consensus_peers = get_consensus_peers(my_id, &settings)?;
        let consensus_net =
            TcpReliableSender::<Id, ProtocolMsg<Id, Tx, Round>, Acknowledgement>::with_peers(
                consensus_peers,
            );

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

        let all_peers_except_me = all_peers.into_iter().filter(|x| x != &my_id).collect();

        let (tx_msg_loopback, rx_msg_loopback) = unbounded_channel();
        tokio::spawn(async move {
            let res = Leto::<Tx> {
                my_id,
                crypto_system,
                broadcast_peers: all_peers_except_me,
                exit_rx,
                rx_mem_to_consensus,
                rx_net_to_consensus,
                consensus_net,
                _tx_consensus_to_mem: tx_consensus_to_mem,
                tx_consensus_to_batcher,
                round_context: RoundContext::new(Round::START),
                leader_context: LeaderContext::new(
                    settings.consensus_config.get_all_ids(),
                    settings.consensus_config.num_faults,
                ),
                tx_msg_loopback,
                rx_msg_loopback,
                // num_nodes - num_faults - 1
                // Here, -1 is because we always deliver messages to ourselves using the loopback
                // channel
                _quorum_waiter: QuorumWaiter::new(
                    settings.consensus_config.num_nodes()
                        - settings.consensus_config.num_faults
                        - 1,
                ),
                chain_state: ChainState::new(store),
                _settings: settings,
                cancel_handlers: FnvHashMap::default(),
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

        // Setup genesis context
        self.chain_state.genesis_setup().await?;

        // Start the protocol loop
        loop {
            tokio::select! {
                exit_val = &mut self.exit_rx => {
                    let _ = exit_val.map_err(anyhow::Error::new)?;
                    info!("Termination signal received by the server. Exiting.");
                    break
                }
                // Handle batch ready messages from the mempool if I am the leader
                batch_hash = self.rx_mem_to_consensus.recv(), if
                    self.leader_context.leader() == self.my_id
                => {
                    let batch_hash = batch_hash.ok_or(
                        anyhow!("Mempool processor has shut down")
                    )?;
                    if let Err(e) = self.handle_new_batch(batch_hash).await {
                        error!("Error handling new batch: {}", e);
                    }
                }
                // Receive consensus messages from loopback
                msg = self.rx_msg_loopback.recv() => {
                    let msg = msg.ok_or(
                        anyhow!("Loopback layer has closed")
                    )?;
                    info!("Got a consensus message from loopback: {:?}", msg);
                    if let Err(e) = self.handle_msg(msg).await {
                        error!("Error handling consensus message from loopback: {}", e);
                    }
                }
                // Receive consensus messages from others
                msg = self.rx_net_to_consensus.recv() => {
                    let msg = msg.ok_or(
                        anyhow!("Networking layer has closed")
                    )?;
                    info!("Got a consensus message from the network: {:?}", msg);
                    if let Err(e) = self.handle_msg(msg).await {
                        error!("Error handling consensus message: {}", e);
                    }
                }
            };
        }
        info!("Server is shutting down!");
        Ok(())
    }

    async fn handle_msg(
        &mut self,
        msg: ProtocolMsg<Id, Tx, Round>,
    ) -> Result<()> {
        match msg {
            ProtocolMsg::Propose { proposal, auth } => self.handle_proposal(proposal, auth).await,
            ProtocolMsg::Relay { proposal, auth } => self.handle_proposal(proposal, auth).await,
            ProtocolMsg::Blame { round, auth } => self.handle_blame(round, auth),
        }
    }
}
