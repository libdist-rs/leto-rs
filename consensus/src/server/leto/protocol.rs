use std::sync::Arc;
use std::time::Duration;

use crate::server::{
    get_consensus_peers, BatcherConsensusMsg, Handler, Parameters, RRBatcher, Settings,
};
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

use super::{
    ChainState, CommitContext, Helper, HelperRequest, LeaderContext, QuorumWaiter, RoundContext,
    Synchronizer, SynchronizerOutMsg,
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
    /// We get messages from the synchronizer here
    /// The synchronizer tells us when we get responses for unknown batches,
    /// blocks, etc.
    pub(crate) rx_synchronizer_to_consensus: UnboundedReceiver<SynchronizerOutMsg<Tx>>,

    /// We send messages to the batcher here
    /// We tell the batcher to clear a batch when committing or receiving a
    /// proposal, and notify it when we end a round
    pub(crate) tx_consensus_to_batcher: UnboundedSender<BatcherConsensusMsg<Id, Tx>>,
    /// A loopback channel to re-schedule suspended messages
    pub(crate) tx_msg_loopback: UnboundedSender<ProtocolMsg<Id, Tx, Round>>,
    /// We respond to other server's help requests for unknown hashes here
    pub(crate) tx_helper: UnboundedSender<HelperRequest<Tx>>,

    /// TODO: Use this (Currently, we do not need it)
    pub(crate) _tx_consensus_to_mem: UnboundedSender<ConsensusMempoolMsg<Id, Round, Tx>>,

    /* Helpers */
    /// This object is used to wait for n-f messages to be delivered
    pub(crate) _quorum_waiter: QuorumWaiter,
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
        settings: Settings,
        store: Storage,
        exit_rx: oneshot::Receiver<()>,
        rx_mem_to_consensus: UnboundedReceiver<BatchHash<Tx>>,
        rx_mem_to_batcher: UnboundedReceiver<(Tx, usize)>,
        tx_processor: UnboundedSender<Batch<Tx>>,
        tx_consensus_to_mem: UnboundedSender<ConsensusMempoolMsg<Id, Round, Tx>>,
        tx_commit: UnboundedSender<Arc<Batch<Tx>>>,
    ) -> Result<()> {
        let me = settings
            .committee_config
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
                consensus_peers.clone(),
            );

        // Start the helper
        let (tx_helper, rx_helper) = unbounded_channel();
        Helper::<Tx>::spawn(store.clone(), rx_helper, consensus_peers.clone());

        // Start the synchronizer
        let (tx_from_sync, rx_from_sync) = unbounded_channel();
        let synchronizer =
            Synchronizer::<Tx>::new(store.clone(), my_id, tx_from_sync, consensus_peers);

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
                // num_nodes - num_faults - 1
                // Here, -1 is because we always deliver messages to ourselves using the loopback
                // channel
                _quorum_waiter: QuorumWaiter::new(
                    settings.committee_config.num_nodes()
                        - settings.committee_config.num_faults()
                        - 1,
                ),
                chain_state: ChainState::new(store),
                settings,
                rx_synchronizer_to_consensus: rx_from_sync,
                synchronizer,
                tx_helper,
                commit_ctx,
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

        // Reset the timer
        self.round_context.timer.reset();

        // Start the protocol loop
        loop {
            tokio::select! {
                // Receive exit handlers
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
                // On timeout for a round
                _ = self.round_context.timer.tick(), if self.round_context.timer_enabled => {
                    warn!("Round {} timing out", self.round_context.round());
                    if let Err(e) = self.on_round_timeout().await {
                        error!("Error handling round timeout: {}", e);
                    }
                }
                // Receive synchronized messages from others
                sync_help = self.rx_synchronizer_to_consensus.recv() => {
                    let sync_msg = sync_help.ok_or(
                        anyhow!("Synchronizer channel closed")
                    )?;
                    if let Err(e) = match sync_msg {
                        SynchronizerOutMsg::ResponseBatch(resp_batch) => self.on_batch_ready(resp_batch).await,
                    } {
                        error!("Error handling a ready batch: {}", e);
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
            ProtocolMsg::Propose {
                proposal,
                auth,
                batch,
            } => self.handle_proposal(proposal, auth, batch).await,
            ProtocolMsg::Relay {
                proposal,
                auth,
                batch_hash,
                sender,
            } => self.handle_relay(proposal, auth, batch_hash, sender).await,
            ProtocolMsg::Blame { 
                round, 
                auth 
            } => self.handle_blame(round, auth).await,
            ProtocolMsg::BlameQC { 
                round, 
                qc 
            } => self.on_blame_qc(round, qc).await,
            // Use the helper
            ProtocolMsg::BatchRequest { source, request } => {
                self.on_batch_request(source, request).await
            }
            // Use the synchronizer
            ProtocolMsg::BatchResponse { response } => {
                self.synchronizer.on_batch_response(response).await
            }
        }
    }
}
