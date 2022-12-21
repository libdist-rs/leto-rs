use std::sync::Arc;
use std::time::Duration;
use crate::server::{Settings, BatcherConsensusMsg, Handler, Parameters, RRBatcher, Event};
use crate::start_id;
use crate::types::Transaction;
use crate::{
    to_socket_address,
    types::ProtocolMsg,
    Id, KeyConfig, Round,
};
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
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
use super::{
    ChainState, CommitContext, Helper, LeaderContext, RoundContext, Synchronizer,
};

pub struct Leto<Tx> {
    // Static state
    pub(crate) my_id: Id,
    /// Crypto Keys
    pub(crate) crypto_system: KeyConfig,
    /// A collection of all peers except `my_id` for use during broadcast
    pub(crate) broadcast_peers: Vec<Id>,
    /// Settings
    pub(crate) settings: Settings,
    /// Network abstraction
    pub(crate) consensus_net: TcpReliableSender<Id, ProtocolMsg<Id, Tx, Round>, Acknowledgement>,

    // Channel handlers
    /// A signal to exit the protocol
    pub(crate) exit_rx: oneshot::Receiver<()>,
    /// We get messages from the mempool here
    /// In particular, we get batch hashes when we are the leaders
    pub(crate) rx_mem_to_consensus: UnboundedReceiver<BatchHash<Tx>>,
    /// We get messages from the network here
    pub(crate) rx_net_to_consensus: UnboundedReceiver<ProtocolMsg<Id, Tx, Round>>,
    /// We get messages from ourselves (loopback) here
    pub(crate) rx_msg_loopback: UnboundedReceiver<ProtocolMsg<Id, Tx, Round>>,
    /// We send messages to the batcher here
    /// We tell the batcher to clear a batch when committing or receiving a
    /// proposal, and notify it when we end a round
    pub(crate) tx_consensus_to_batcher: UnboundedSender<BatcherConsensusMsg<Id, Tx>>,
    /// A loopback channel to re-schedule suspended messages
    pub(crate) tx_msg_loopback: UnboundedSender<ProtocolMsg<Id, Tx, Round>>,

    /// TODO: Use this (Currently, we do not need it)
    pub(crate) _tx_consensus_to_mem: UnboundedSender<ConsensusMempoolMsg<Id, Round, Tx>>,

    /* Helpers */
    /// Helps get unknown batch hashes, blocks, etc
    pub(crate) synchronizer: Synchronizer<Tx>,
    /// Helps us with committing
    pub(crate) commit_ctx: CommitContext<Tx>,

    /* Variable State */
    /// Round State
    pub(crate) round_context: RoundContext<Tx>,
    /// Leader State
    pub(crate) leader_context: LeaderContext,
    /// Chain state
    pub(crate) chain_state: ChainState<Tx>,
}

impl<Tx> Leto<Tx> {
    pub const INITIAL_LEADER: Id = start_id();
    pub const INITIAL_ROUND: Round = 0;
}

impl<Tx> Leto<Tx>
where
    Tx: Transaction,
{
    #[allow(clippy::too_many_arguments)]
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
        tx_commit: UnboundedSender<Arc<Batch<Tx>>>,
    ) -> Result<()> 
    {
        let me = settings
            .committee_config
            .get(&my_id)
            .ok_or_else(|| anyhow!("My Id {} is not present in the config", my_id))?;
        let consensus_addr = to_socket_address("0.0.0.0", me.consensus_port)?;

        let (tx_net_to_consensus, rx_net_to_consensus) = unbounded_channel();

        // Start receiver for consensus messages
        TcpReceiver::<Acknowledgement, ProtocolMsg<Id, Tx, Round>, _>::spawn(
            consensus_addr,
            Handler::<Id, Tx, Round>::new(tx_net_to_consensus),
        );

        // Start outgoing connections
        let consensus_peers = settings.get_consensus_peers(my_id)?;
        let consensus_net =
            TcpReliableSender::<Id, ProtocolMsg<Id, Tx, Round>, Acknowledgement>::with_peers(
                consensus_peers.clone(),
            );

        // Start the helper
        let (tx_helper, rx_helper) = unbounded_channel();
        Helper::<Tx>::spawn(store.clone(), rx_helper, consensus_peers.clone());

        // Start the synchronizer
        let synchronizer =
            Synchronizer::<Tx>::new(
                store.clone(), 
                my_id, 
                tx_helper,
                consensus_peers,
            );

        // Start the batcher
        let (tx_consensus_to_batcher, rx_consensus_to_batcher) = unbounded_channel();
        let batching_params = Parameters::new(
            my_id,
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

        // Start the commit context
        let commit_ctx = CommitContext::spawn(
            store.clone(),
            tx_commit,
            settings.committee_config.num_nodes(),
            settings.committee_config.num_faults(),
        );

        let all_peers_except_me = all_peers.into_iter().filter(|x| x != &my_id).collect();

        let (tx_msg_loopback, rx_msg_loopback) = unbounded_channel();
        tokio::spawn(async move {
            let mut protocol = Leto::<Tx> {
                my_id,
                crypto_system,
                broadcast_peers: all_peers_except_me,
                exit_rx,
                rx_mem_to_consensus,
                rx_net_to_consensus,
                consensus_net,
                _tx_consensus_to_mem: tx_consensus_to_mem,
                tx_consensus_to_batcher,
                round_context: RoundContext::new(
                    settings.committee_config.num_nodes(),
                    Duration::from_millis(settings.bench_config.delay_in_ms),
                ),
                leader_context: LeaderContext::new(
                    settings.committee_config.get_all_ids(),
                    settings.committee_config.num_faults(),
                ),
                tx_msg_loopback,
                rx_msg_loopback,
                chain_state: ChainState::new(store),
                settings,
                synchronizer,
                commit_ctx,
            };
            if let Err(e) = protocol.run().await {
                error!("Consensus error: {}", e);
            }
        });
        Ok(())
    }

    async fn run(&mut self) -> Result<()> 
    where 
        Tx: Transaction,
    {
        info!("Starting the server");

        // Setup genesis context
        self.chain_state.genesis_setup().await?;

        // Reset the timer
        self.round_context.init();

        // Start the protocol loop
        loop {
            tokio::select! {
                // Receive exit handlers
                exit_val = &mut self.exit_rx => {
                    exit_val.map_err(anyhow::Error::new)?;
                    info!("Termination signal received by the server. Exiting.");
                    break
                }
                // Handle batch ready messages from the mempool when I am the leader
                batch_hash = self.rx_mem_to_consensus.recv(), if
                    self.leader_context.leader() == self.my_id
                => {
                    let batch_hash = batch_hash.ok_or_else(||
                        anyhow!("Mempool processor has shut down")
                    )?;
                    if let Err(e) = self.handle_new_batch(batch_hash).await {
                        error!("Error handling new batch: {}", e);
                    }
                }
                // Receive delivery synchronized messages
                // Forward to round synchronizer
                sync_help = &mut self.synchronizer.next() => {
                    let synced_msg = sync_help.ok_or_else(||
                        anyhow!("Synchronizer channel closed")
                    )?;
                    if let Err(e) = self.round_context.sync(synced_msg) {
                        error!("Error handling a synced message: {}", e);
                    }
                }
                // Receive round synchronized and delivery synchronized messages
                // Forward to generic msg handler which will forward to the corresponding handler
                rnd_help = &mut self.round_context.next() => {
                    let synced_msg = rnd_help.ok_or_else(||
                        anyhow!("Round context error")
                    )?;
                    let res = match synced_msg {
                        Event::Timeout => self.on_round_timeout().await,
                        Event::Msg(m) => self.handle_msg(m).await,
                    };
                    if let Err(e) = res {
                        error!("Error handling a round synced message: {}", e);
                    }
                }
                // Receive consensus messages from loopback
                // Loopback messages are always delivery synchronized 
                // Hence, forward them to the round synchronizer
                msg = self.rx_msg_loopback.recv() => {
                    let msg = msg.ok_or_else(||
                        anyhow!("Loopback layer has closed")
                    )?;
                    info!("Got a consensus message from loopback: {:?}", msg);
                    if let Err(e) = self.round_context.sync(msg) {
                        error!("Error handling consensus message from loopback: {}", e);
                    }
                }
                // Receive consensus messages from others
                // Forward to Delivery synchronizer
                msg = self.rx_net_to_consensus.recv() => {
                    let msg = msg.ok_or_else(||
                        anyhow!("Networking layer has closed")
                    )?;
                    info!("Got a consensus message from the network: {:?}", msg);
                    if let Err(e) = self.synchronizer.sync_msg(msg).await {
                        error!("Error handling consensus message: {}", e);
                    }
                }
            };
        }
        info!("Server is shutting down!");
        Ok(())
    }

    /// Handles synced messages
    async fn handle_msg(
        &mut self,
        msg: ProtocolMsg<Id, Tx, Round>,
    ) -> Result<()> {
        debug!(
            "RD: Got msg {:?} in round {}",
            msg,
            self.round_context.round(),
        );

        match msg {
            ProtocolMsg::Propose {
                proposal,
                auth,
                batch,
                ..
            } => self.handle_proposal(proposal, auth, batch).await,
            ProtocolMsg::Blame { 
                round, 
                auth 
            } => self.handle_blame(round, auth).await,
            ProtocolMsg::BlameQC { 
                round, 
                qc 
            } => self.on_blame_qc(round, qc).await,
            // We should never see the other messages
            ProtocolMsg::Relay {..} => unreachable!(),
            ProtocolMsg::BatchRequest {..} => unreachable!(),
            ProtocolMsg::BatchResponse {..} => unreachable!(),
            ProtocolMsg::ElementRequest {..} => unreachable!(),
            ProtocolMsg::ElementResponse {..} => unreachable!(),
        }
    }
}
