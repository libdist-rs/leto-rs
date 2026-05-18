/// Zeus core struct and main event loop.
///
/// Mirrors `consensus/src/server/leto/core.rs`.  The sig-plane reuses Leto's
/// `ChainState`, `LeaderContext`, and `CommitContext` with `Attestation<Tx>`
/// as the payload type.  Zeus uses its own round-ordering logic instead of
/// `RoundContext<Tx>` (which is typed to `ProtocolMsg` internally).
use crate::{
    server::{BatcherConsensusMsg, ChainState, LeaderContext, Parameters, RRBatcher, Settings},
    types::{Attestation, DataBlock, Element, Signature, Transaction, ZeusMsg},
    Id, KeyConfig, Round, START_ID,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use fnv::FnvHashMap;
use futures_util::StreamExt;
use log::*;
use mempool::Batch;
use serde::Serialize;
use std::pin::Pin;
use std::{collections::VecDeque, sync::Arc};
use storage::rocksdb::Storage;
use tcp_reliable_sender::{CancelHandler, TcpReliableSender};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tokio::time::{interval, sleep, Interval, Sleep};

use super::chain_state::{AdmittedChangeQCs, DataBlockStore, DataChainState, PendingAttestations};
use super::phases::ZeusCommitContext;

// ---------------------------------------------------------------------------
// Type aliases for the sig-plane
// ---------------------------------------------------------------------------

/// Sig-chain element: Leto `Element` with `Attestation<Tx>` payload.
pub type SigElement<Tx> = Element<Id, Attestation<Tx>, Round>;
/// Sig-chain element hash.
pub type SigElementHash<Tx> = Hash<SigElement<Tx>>;
/// Sig-chain ChainState.
pub type SigChainState<Tx> = ChainState<Attestation<Tx>>;
/// Sig-chain LeaderContext.
pub type SigLeaderContext = LeaderContext;

// ---------------------------------------------------------------------------
// TimerData kind (zeus.tex Def 8.4)
// ---------------------------------------------------------------------------

/// Distinguishes the two contexts in which the data-side timer fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataTimerKind {
    /// Armed on sig-chain round entry by every node.
    /// Expiry → would-be-blame warn, no action beyond logging.
    TimerDataRoundEntry,
    /// Armed by the rleader when it wants to propose but has no fresh block.
    /// Expiry → would-be-blame warn, stay in waiting state.
    RleaderWaitingFresh,
}

// ---------------------------------------------------------------------------
// Simple per-round ordering context for Zeus sig-plane
// ---------------------------------------------------------------------------

/// Minimal round-ordering buffer for Zeus sig-plane messages.
/// Queues future-round messages and delivers them when the round advances.
pub struct ZeusRoundState<Tx> {
    current_round: Round,
    msg_buf: VecDeque<ZeusMsg<Tx>>,
    future_msgs: FnvHashMap<Round, VecDeque<ZeusMsg<Tx>>>,
    /// Blame signatures collected for the current round.
    pub blame_map: FnvHashMap<Id, Signature<Id, Round>>,
    pub got_qc: bool,
    pub cancel_handlers: Vec<CancelHandler>,
    #[allow(dead_code)]
    num_nodes: usize,
}

impl<Tx> ZeusRoundState<Tx> {
    pub fn new(num_nodes: usize) -> Self {
        Self {
            current_round: Round::MIN + 1,
            msg_buf: VecDeque::new(),
            future_msgs: FnvHashMap::default(),
            blame_map: FnvHashMap::default(),
            got_qc: false,
            cancel_handlers: Vec::new(),
            num_nodes,
        }
    }

    pub fn round(&self) -> Round {
        self.current_round
    }

    pub fn is_ready(&self) -> bool {
        !self.msg_buf.is_empty()
    }

    pub fn pop_msg(&mut self) -> Option<ZeusMsg<Tx>> {
        self.msg_buf.pop_front()
    }

    /// Enqueue or buffer a message.
    pub fn enqueue(
        &mut self,
        msg: ZeusMsg<Tx>,
        msg_round: Round,
    ) {
        match msg_round.cmp(&self.current_round) {
            std::cmp::Ordering::Less => {
                debug!("ZeusRoundState: dropping old round {} msg", msg_round);
            }
            std::cmp::Ordering::Greater => {
                self.future_msgs
                    .entry(msg_round)
                    .or_default()
                    .push_back(msg);
            }
            std::cmp::Ordering::Equal => {
                self.msg_buf.push_back(msg);
            }
        }
    }

    pub fn advance_round(
        &mut self,
        timer: &mut Interval,
        timer_enabled: &mut bool,
    ) {
        self.current_round += 1;
        self.blame_map.clear();
        self.got_qc = false;
        self.cancel_handlers.retain_mut(|h| {
            matches!(
                h.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            )
        });
        // Drain future messages for the new round
        if let Some(msgs) = self.future_msgs.remove(&self.current_round) {
            for m in msgs {
                self.msg_buf.push_back(m);
            }
        }
        // GC stale future msgs
        self.future_msgs.retain(|r, _| r >= &self.current_round);
        timer.reset();
        *timer_enabled = true;
    }

    pub fn add_handler(
        &mut self,
        h: CancelHandler,
    ) {
        self.cancel_handlers.push(h);
    }

    pub fn add_handlers(
        &mut self,
        hs: Vec<CancelHandler>,
    ) {
        self.cancel_handlers.extend(hs);
    }

    pub fn disable_timer(
        &self,
        timer_enabled: &mut bool,
    ) {
        *timer_enabled = false;
    }
}

// ---------------------------------------------------------------------------
// Zeus struct
// ---------------------------------------------------------------------------

pub struct Zeus<Tx> {
    // ------------------------------------------------------------------
    // Static
    // ------------------------------------------------------------------
    pub(crate) my_id: Id,
    pub(crate) crypto_system: KeyConfig,
    pub(crate) broadcast_peers: Vec<Id>,
    pub(crate) settings: Settings,
    pub(crate) consensus_net: TcpReliableSender<Id, ZeusMsg<Tx>>,

    // ------------------------------------------------------------------
    // Channels
    // ------------------------------------------------------------------
    pub(crate) exit_rx: oneshot::Receiver<()>,
    pub(crate) rx_net_to_consensus: UnboundedReceiver<ZeusMsg<Tx>>,
    pub(crate) rx_msg_loopback: UnboundedReceiver<ZeusMsg<Tx>>,
    pub(crate) tx_msg_loopback: UnboundedSender<ZeusMsg<Tx>>,
    /// Batcher control for eleader data-block batching.
    pub(crate) tx_consensus_to_batcher: UnboundedSender<BatcherConsensusMsg<Id, Tx>>,
    /// Data batch ready from RRBatcher → eleader loop.
    pub(crate) rx_data_batch: UnboundedReceiver<Batch<Tx>>,
    /// Data-tx commit output (kept alive to prevent channel close).
    #[allow(dead_code)]
    pub(crate) tx_data_commit: UnboundedSender<Arc<Batch<Tx>>>,

    // ------------------------------------------------------------------
    // Sig-plane state
    // ------------------------------------------------------------------
    pub(crate) sig_chain_state: SigChainState<Tx>,
    pub(crate) sig_leader_context: SigLeaderContext,
    pub(crate) zeus_commit_ctx: ZeusCommitContext<Tx>,
    pub(crate) round_state: ZeusRoundState<Tx>,
    pub(crate) timer: Interval,
    pub(crate) timer_enabled: bool,

    /// Signature-chain epoch.  Steady-state = 1.
    /// TODO(zeus-view-change): increment on sig-chain view change.
    pub(crate) signature_epoch: u64,

    // ------------------------------------------------------------------
    // Data-plane state
    // ------------------------------------------------------------------
    /// Data-plane epoch (distinct from signature_epoch).
    pub(crate) current_epoch: u64,
    /// Lightweight data-chain head (hash + height; no Vec<DataBlock>).
    pub(crate) data_chain: DataChainState<Tx>,
    /// Block store: `H(envelope)` → `DataBlock`.
    pub(crate) data_block_store: DataBlockStore<Tx>,
    /// Park map for attestations awaiting a missing data block.
    pub(crate) pending_attestations: PendingAttestations<Tx>,
    /// Admitted eleader-change QCs.
    /// TODO(zeus-view-change): replace bool value with actual QC type.
    pub(crate) admitted_change_qcs: AdmittedChangeQCs,
    /// Most-recently admitted data block.
    pub(crate) last_seen_data_block: DataBlock<Tx>,
    /// Committed attestations (sig_round, Attestation).
    pub(crate) commit_lock: Vec<(u64, Attestation<Tx>)>,
    // ------------------------------------------------------------------
    // TimerData state (zeus.tex Def 8.4)
    // ------------------------------------------------------------------
    /// Highest-height data block admitted from the current eleader.
    /// Updated on every successful `on_data_propose` admission.
    /// `None` until the first data block is admitted in this epoch.
    pub(crate) latest_eleader_block: Option<DataBlock<Tx>>,

    /// Data-block height pinned by the most-recent sig-chain proposal.
    /// Updated every time `handle_new_sig_round` successfully proposes.
    /// Initialized to 0 (genesis height) at startup.
    pub(crate) last_attested_data_height: u64,

    /// Pending data-side timer and its kind.
    /// `None` when no timer is armed.
    pub(crate) data_timer: Option<(Pin<Box<Sleep>>, DataTimerKind)>,

    /// True iff this node is the current rleader and is sitting in the
    /// "waiting for fresh eleader block" state (step 4 of the rule).
    pub(crate) rleader_waiting_fresh: bool,

    /// Set by `post_admit_side_effects` when a rleader-wakeup fires inside the
    /// `drain_pending_data_blocks` async recursion where calling
    /// `handle_new_sig_round` directly would cause E0733.  Cleared and acted
    /// on by the outermost `on_data_propose` / `on_data_response` caller.
    pub(crate) rleader_wakeup_pending: bool,

    /// Highest data-block height already emitted via `tx_data_commit`.
    ///
    /// Monotonically increasing.  The prefix-commit walk (zeus.tex Def 8.7)
    /// emits heights `(zeus_committed_high, H]` and updates this watermark.
    pub(crate) zeus_committed_high: u64,
    /// Highest height that the eleader has proposed.
    ///
    /// In-flight count = `eleader_proposed_height − data_chain.head_height`.
    /// The eleader may propose the next height only when in-flight <
    /// `eleader_pipeline_depth`.
    pub(crate) eleader_proposed_height: u64,
    /// Hash of the most-recently proposed data block.
    ///
    /// Used as the parent hash when proposing the next block, so the eleader
    /// chains off its own latest proposal rather than the admitted head.  This
    /// is essential for correctness when W > 1 blocks are in-flight: the
    /// admitted head lags behind the proposed tip.
    ///
    /// `None` until the eleader issues its first proposal in the current epoch.
    ///
    /// TODO(zeus-view-change): reset to `None` (and `eleader_proposed_height`
    /// to `data_chain.head_height`) on epoch/eleader change so the new eleader
    /// starts from the admitted head.
    pub(crate) last_proposed_hash: Option<super::chain_state::DataBlockHash<Tx>>,
    /// Maximum number of data blocks the eleader may have in-flight (proposed
    /// but not yet locally admitted).  Sourced from
    /// `settings.bench_config.eleader_pipeline_depth`.
    pub(crate) eleader_pipeline_depth: usize,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

impl<Tx> Zeus<Tx> {
    pub const INITIAL_LEADER: Id = START_ID;
    pub const INITIAL_ROUND: Round = 0;
    pub const STEADY_STATE_SIG_EPOCH: u64 = 1;
    pub const INITIAL_DATA_EPOCH: u64 = 1;
}

// ---------------------------------------------------------------------------
// spawn() + run()
// ---------------------------------------------------------------------------

impl<Tx> Zeus<Tx>
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
        rx_mem_to_batcher: UnboundedReceiver<(Tx, usize)>,
        tx_data_commit: UnboundedSender<Arc<Batch<Tx>>>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug + 'static,
    {
        let me = settings
            .committee_config
            .get(&my_id)
            .ok_or_else(|| anyhow!("My Id {} not in config", my_id))?;
        let consensus_addr = crate::to_socket_address("0.0.0.0", me.consensus_port)?;

        // Networking receiver
        let (tx_net_to_consensus, rx_net_to_consensus) = unbounded_channel();
        let mut receiver = tcp_receiver::TcpReceiver::<ZeusMsg<Tx>>::spawn(consensus_addr);
        tokio::spawn(async move {
            while let Some(Ok(msg)) = receiver.next().await {
                if tx_net_to_consensus.send(msg).is_err() {
                    break;
                }
            }
        });

        // Outgoing consensus connections
        let consensus_peers = settings.get_consensus_peers(my_id)?;
        let consensus_net = TcpReliableSender::<Id, ZeusMsg<Tx>>::with_peers(consensus_peers);

        // TODO(zeus-view-change): non-eleader nodes' tx_pool/batcher is unused in
        // steady state; consider gating construction on `my_id ==
        // eleader(epoch)` once eleader-change lands. Eleader data-block batcher
        let (tx_consensus_to_batcher, rx_consensus_to_batcher) = unbounded_channel();
        let batching_params = Parameters::new(
            my_id,
            Zeus::<Tx>::INITIAL_LEADER,
            settings.bench_config.batch_size,
            settings.bench_config.batch_timeout,
        );
        let (tx_data_batch, rx_data_batch) = unbounded_channel::<Batch<Tx>>();
        RRBatcher::<Id, Tx>::spawn(
            batching_params,
            rx_mem_to_batcher,
            rx_consensus_to_batcher,
            tx_data_batch,
        )?;

        // Zeus commit context (specialized: does not require Attestation<Tx>:
        // Transaction).
        // Payload emission is done by the main task's on_committed_attestation,
        // which has direct access to data_block_store for the prefix walk.
        let zeus_commit_ctx = ZeusCommitContext::<Tx>::spawn(
            SigChainState::new(store.clone()),
            settings.committee_config.num_nodes(),
            settings.committee_config.num_faults(),
        );

        // TODO(zeus-view-change): add a `zeus_client_port` field to `Party` and
        // spawn a `TcpReceiver<ZeusClientMsg<Tx>>` listener here to answer
        // `WhoIsEleader` queries from real clients.  The in-process harness
        // always pre-seeds `eleader_id` via `ClientMode::ZeusEleaderOnly`, so
        // this listener is not needed for the stress-test binary and is deferred
        // to avoid port-conflict with the mempool's client listener.

        let all_peers_except_me: Vec<Id> = all_peers.into_iter().filter(|x| x != &my_id).collect();

        let (tx_msg_loopback, rx_msg_loopback) = unbounded_channel();

        // Bootstrap genesis DataBlock store
        let genesis_data_block = DataBlock::<Tx>::genesis();
        let genesis_data_hash = genesis_data_block.hash().clone();
        let mut data_block_store: DataBlockStore<Tx> = FnvHashMap::default();
        data_block_store.insert(genesis_data_hash, genesis_data_block.clone());

        let num_nodes = settings.committee_config.num_nodes();
        let num_faults = settings.committee_config.num_faults();

        let protocol = Zeus::<Tx> {
            my_id,
            crypto_system,
            broadcast_peers: all_peers_except_me,
            exit_rx,
            rx_net_to_consensus,
            consensus_net,
            tx_consensus_to_batcher,
            rx_data_batch,
            tx_msg_loopback,
            rx_msg_loopback,
            tx_data_commit,
            sig_chain_state: SigChainState::new(store.clone()),
            sig_leader_context: SigLeaderContext::new(
                settings.committee_config.get_all_ids(),
                num_faults,
            ),
            zeus_commit_ctx,
            round_state: ZeusRoundState::new(num_nodes),
            timer: interval(std::time::Duration::from_millis(
                4 * settings.bench_config.delay_in_ms,
            )),
            timer_enabled: true,
            signature_epoch: Zeus::<Tx>::STEADY_STATE_SIG_EPOCH,
            current_epoch: Zeus::<Tx>::INITIAL_DATA_EPOCH,
            data_chain: DataChainState::genesis(),
            data_block_store,
            pending_attestations: FnvHashMap::default(),
            admitted_change_qcs: {
                // Pre-admit the epoch 0 → epoch 1 transition.
                // Epoch 1 is the initial data-plane epoch; it does not require
                // an eleader-change QC because the genesis is the neutral state.
                // TODO(zeus-view-change): subsequent epoch transitions (e → e+1 for e >= 1)
                // require an actual eleader-change QC before being admitted.
                let mut m = FnvHashMap::default();
                m.insert(0u64, true);
                m
            },
            last_seen_data_block: genesis_data_block,
            commit_lock: Vec::new(),
            latest_eleader_block: None,
            last_attested_data_height: 0,
            data_timer: None,
            rleader_waiting_fresh: false,
            rleader_wakeup_pending: false,
            zeus_committed_high: 0,
            eleader_proposed_height: 0,
            last_proposed_hash: None,
            eleader_pipeline_depth: settings.bench_config.eleader_pipeline_depth,
            settings,
        };

        tokio::spawn(async move {
            let mut p = protocol;
            if let Err(e) = p.run().await {
                error!("Zeus consensus error: {}", e);
            }
        });

        Ok(())
    }

    async fn run(&mut self) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug + 'static,
    {
        info!("Zeus: starting (node {})", self.my_id);
        self.sig_chain_state.genesis_setup().await?;
        self.timer.reset();

        // Arm the eleader's batcher with the correct initial eleader identity.
        // The batcher is constructed with initial_leader=0; the actual eleader
        // for epoch 1 is eleader(1, n). Send NewRound once at startup so the
        // batcher sets current_leader correctly before any batch is sealed.
        {
            let n = self.settings.committee_config.num_nodes();
            let initial_eleader = super::chain_state::eleader(self.current_epoch, n);
            let _ = self
                .tx_consensus_to_batcher
                .send(BatcherConsensusMsg::NewRound {
                    leader: initial_eleader,
                });
        }

        // Per-node TimerData on first sig-round entry (round 1 at startup).
        // Every node arms the 1s timer here; if no eleader block arrives before
        // it fires, on_data_timer_expired emits a would-be-blame warn.
        // The rleader additionally enters the waiting-for-fresh state so that
        // the on_data_propose rleader-wakeup path fires when the first eleader
        // block is admitted.
        self.arm_data_timer(DataTimerKind::TimerDataRoundEntry);
        if self.sig_leader_context.leader() == self.my_id {
            self.rleader_waiting_fresh = true;
        }

        loop {
            tokio::select! {
                // Exit
                exit_val = &mut self.exit_rx => {
                    exit_val.map_err(anyhow::Error::new)?;
                    info!("Zeus: exit signal.");
                    break;
                }

                // Eleader data-block propose loop
                batch = self.rx_data_batch.recv(), if self.is_eleader() => {
                    let batch = batch.ok_or_else(|| anyhow!("Zeus: data batcher shut down"))?;
                    if let Err(e) = self.on_eleader_propose(batch).await {
                        error!("Zeus: on_eleader_propose: {}", e);
                    }
                }

                // Round-ordered sig-plane messages
                _ = async {}, if self.round_state.is_ready() => {
                    if let Some(msg) = self.round_state.pop_msg() {
                        if let Err(e) = self.handle_sig_ordered(msg).await {
                            error!("Zeus: handle_sig_ordered: {}", e);
                        }
                    }
                }

                // Round timer (blame)
                _ = self.timer.tick(), if self.timer_enabled => {
                    if let Err(e) = self.on_round_timeout().await {
                        error!("Zeus: on_round_timeout: {}", e);
                    }
                }

                // Loopback
                msg = self.rx_msg_loopback.recv() => {
                    let msg = msg.ok_or_else(|| anyhow!("Zeus: loopback closed"))?;
                    if let Err(e) = self.dispatch(msg).await {
                        error!("Zeus: loopback dispatch: {}", e);
                    }
                }

                // Net
                msg = self.rx_net_to_consensus.recv() => {
                    let msg = msg.ok_or_else(|| anyhow!("Zeus: net closed"))?;
                    if let Err(e) = self.dispatch(msg).await {
                        error!("Zeus: net dispatch: {}", e);
                    }
                }

                // Committed attestation feedback from ZeusCommitContext
                committed = self.zeus_commit_ctx.rx_committed.recv() => {
                    if let Some(c) = committed {
                        self.on_committed_attestation(c);
                    }
                }

                // TimerData (zeus.tex Def 8.4): fires when no fresh eleader
                // block has arrived within data_timer_duration_ms of round entry
                // or the rleader entering the waiting-for-fresh-block state.
                _ = async {
                    if let Some((timer, _)) = &mut self.data_timer {
                        timer.as_mut().await
                    } else {
                        futures_util::future::pending::<()>().await
                    }
                } => {
                    if let Err(e) = self.on_data_timer_expired().await {
                        error!("Zeus: on_data_timer_expired: {}", e);
                    }
                }
            }
        }
        info!("Zeus: shut down.");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Top-level dispatch
    // -----------------------------------------------------------------------

    /// Dispatch: data-plane messages bypass ordering; sig-plane goes through
    /// the round-ordered buffer.
    pub(crate) async fn dispatch(
        &mut self,
        msg: ZeusMsg<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        match msg {
            // Data-plane — direct
            ZeusMsg::DataPropose { block, sender } => self.on_data_propose(block, sender).await,
            ZeusMsg::DataRequest {
                target_hash,
                source,
            } => self.on_data_request(target_hash, source).await,
            ZeusMsg::DataResponse { block } => self.on_data_response(block).await,

            // Sig-plane — route through round ordering
            ZeusMsg::SigPropose { ref proposal, .. } => {
                let r = proposal.round();
                self.round_state.enqueue(msg, r);
                Ok(())
            }
            ZeusMsg::SigBlame { round, .. } | ZeusMsg::SigBlameQC { round, .. } => {
                let r = round;
                self.round_state.enqueue(msg, r);
                Ok(())
            }

            // Sync/relay — no-op (synchronizer not wired for Zeus yet)
            _ => {
                debug!("Zeus: unhandled msg variant in dispatch");
                Ok(())
            }
        }
    }

    /// Process a round-ordered sig-plane message.
    async fn handle_sig_ordered(
        &mut self,
        msg: ZeusMsg<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        match msg {
            ZeusMsg::SigPropose {
                proposal,
                auth,
                attestation,
                sender,
            } => {
                self.handle_sig_proposal(proposal, auth, attestation, sender)
                    .await
            }
            ZeusMsg::SigBlame { round, auth } => self.handle_sig_blame(round, auth).await,
            ZeusMsg::SigBlameQC { round, qc } => self.on_sig_blame_qc(round, qc).await,
            _ => {
                debug!("Zeus: unexpected msg in handle_sig_ordered");
                Ok(())
            }
        }
    }

    // -----------------------------------------------------------------------
    // Predicates
    // -----------------------------------------------------------------------

    pub fn is_eleader(&self) -> bool {
        let n = self.settings.committee_config.num_nodes();
        super::chain_state::eleader(self.current_epoch, n) == self.my_id
    }

    /// Returns the current sig-chain rleader id.
    pub fn rleader(&self) -> crate::Id {
        self.sig_leader_context.leader()
    }

    /// Arms (or re-arms) the data-side timer with the configured duration.
    /// Drops any previously armed timer.
    pub(crate) fn arm_data_timer(
        &mut self,
        kind: DataTimerKind,
    ) {
        let dur =
            std::time::Duration::from_millis(self.settings.bench_config.data_timer_duration_ms);
        self.data_timer = Some((Box::pin(sleep(dur)), kind));
    }

    /// Disarms the data-side timer.
    pub(crate) fn disarm_data_timer(&mut self) {
        self.data_timer = None;
    }

    /// Called when the data-side timer fires.
    ///
    /// For `TimerDataRoundEntry`: any node — emit would-be-blame warn.
    /// For `RleaderWaitingFresh`: rleader — emit would-be-blame warn, stay
    /// waiting.
    pub(crate) async fn on_data_timer_expired(&mut self) -> anyhow::Result<()>
    where
        Tx: Clone + serde::Serialize + PartialEq + std::fmt::Debug,
    {
        let kind = match self.data_timer.take() {
            Some((_, k)) => k,
            None => return Ok(()),
        };

        let n = self.settings.committee_config.num_nodes();
        let eleader_id = super::chain_state::eleader(self.current_epoch, n);
        let r = self.round_state.round();

        match kind {
            DataTimerKind::TimerDataRoundEntry => {
                warn!(
                    "Zeus: TimerData expired in sig round {}; would-be-blame eleader {}",
                    r, eleader_id
                );
            }
            DataTimerKind::RleaderWaitingFresh => {
                warn!(
                    "Zeus: TimerData expired in sig round {}; would-be-blame eleader {} \
                     (rleader waiting for fresh block)",
                    r, eleader_id
                );
                // Stay in waiting state — re-arm so the timer can fire again if
                // another duration passes with no fresh block.
                self.arm_data_timer(DataTimerKind::RleaderWaitingFresh);
            }
        }

        Ok(())
    }
}

impl<Tx> std::fmt::Debug for Zeus<Tx> {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("Zeus")
            .field("my_id", &self.my_id)
            .field("current_epoch", &self.current_epoch)
            .field("signature_epoch", &self.signature_epoch)
            .finish()
    }
}
