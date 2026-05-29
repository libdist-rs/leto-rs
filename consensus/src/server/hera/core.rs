/// Hera consensus (sig-plane) actor.
///
/// Owns the sig chain and drives sig-chain rounds. The data plane lives in a
/// separate `HeraDataActor` (see `data_actor.rs`) so the O(n²) data flood
/// never enters this event loop.
///
/// ## Cross-plane edges (unbounded — sig-plane discipline, never drop)
/// - `tx_fetch_hint` → data actor: (hash, holder) hint to fetch a missing block
///   from `holder` (the sig-proposal sender — a guaranteed holder by invariant 1).
/// - `tx_commit_emit` → data actor: committed `{author→height}` set.
///
/// ## Agreement is decoupled from data availability (spec invariant 1)
/// A sig proposal references only data the proposer holds, so agreement depends
/// on the leader signature + the parent sig-element — NOT on having the data
/// blocks locally. We fire non-blocking fetch-from-sender hints for referenced
/// heads (so the data is present by commit time) and agree immediately. This is
/// what prevents a node momentarily missing a data block from stranding. The
/// data actor consumes the blocks at commit and lags gracefully if any are late.
///
/// ## Head reads (lock-free, stale-OK)
/// When building a SigPropose, reads per-author
/// `ArcSwap<Arc<DataHeadSnapshot>>` from `multi_data_chain.head_snapshots` (the
/// data actor publishes after each admission). Stale reads cost one round of
/// throughput, never safety.
use crate::{
    server::{
        hera::{
            chain_state::MultiAuthorDataChainState,
            data_actor::{CommitEmit, HeraDataActor},
            phases::HeraCommitContext,
        },
        ChainState, LeaderContext, Parameters, RRBatcher, Settings,
    },
    types::{hera::MultiAttestation, Element, HeraMsg, Proposal, Signature, Transaction},
    Id, KeyConfig, Round, START_ID,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use fnv::{FnvHashMap, FnvHashSet};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use log::*;
use mempool::Batch;
use serde::Serialize;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering as AOrdering};
use std::sync::Arc;
use storage::rocksdb::Storage;
use tcp_reliable_sender::{CancelHandler, TcpReliableSender};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tokio::time::{interval, Interval};

// ---------------------------------------------------------------------------
// PROFILE INSTRUMENTATION: in-flight depth counters for unbounded channels.
// ---------------------------------------------------------------------------
/// Depth of rx_sig_net (net → consensus actor, sig-plane).
pub(crate) static INFLIGHT_SIG_NET: AtomicI64 = AtomicI64::new(0);
/// Depth of rx_msg_loopback (sig-plane self-deliver).
pub(crate) static INFLIGHT_LOOPBACK: AtomicI64 = AtomicI64::new(0);
/// Depth of HeraCommitContext tx_inner (consensus → commit task).
pub(crate) static INFLIGHT_COMMIT_TX_INNER: AtomicI64 = AtomicI64::new(0);
/// Depth of rx_committed (commit task → consensus actor).
pub(crate) static INFLIGHT_COMMITTED: AtomicI64 = AtomicI64::new(0);

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

/// Sig-chain element: Leto `Element` with `MultiAttestation<Tx>` payload.
pub type SigElement<Tx> = Element<Id, MultiAttestation<Tx>, Round>;
/// Sig-chain element hash.
pub type SigElementHash<Tx> = Hash<SigElement<Tx>>;
/// Sig-chain ChainState.
pub type SigChainState<Tx> = ChainState<MultiAttestation<Tx>>;
/// Sig-chain LeaderContext.
pub type SigLeaderContext = LeaderContext;

// ---------------------------------------------------------------------------
// Park map for sig proposals awaiting a missing PARENT sig-element.
// ---------------------------------------------------------------------------
pub type PendingSigProposals<Tx> = FnvHashMap<SigElementHash<Tx>, Vec<HeraMsg<Tx>>>;

// ---------------------------------------------------------------------------
// HeraRoundState
// ---------------------------------------------------------------------------

/// Minimal round-ordering buffer for Hera sig-plane messages.
pub struct HeraRoundState<Tx> {
    current_round: Round,
    msg_buf: VecDeque<HeraMsg<Tx>>,
    future_msgs: FnvHashMap<Round, VecDeque<HeraMsg<Tx>>>,
    pub blame_map: FnvHashMap<Id, Signature<Id, Round>>,
    /// Highest sig-chain `(round, element-hash)` reported across the blames
    /// collected for the current round. The next leader extends this (the true
    /// highest chain among the quorum), fetching the element from a blamer if it
    /// does not have it (request-from-sender). Reset whenever blame_map clears.
    pub blame_best: Option<(Round, SigElementHash<Tx>)>,
    pub got_qc: bool,
    #[allow(dead_code)]
    num_nodes: usize,
}

impl<Tx> HeraRoundState<Tx> {
    pub fn new(num_nodes: usize) -> Self {
        Self {
            current_round: Round::MIN + 1,
            msg_buf: VecDeque::new(),
            future_msgs: FnvHashMap::default(),
            blame_map: FnvHashMap::default(),
            blame_best: None,
            got_qc: false,
            num_nodes,
        }
    }

    pub fn round(&self) -> Round {
        self.current_round
    }

    pub fn is_ready(&self) -> bool {
        !self.msg_buf.is_empty()
    }

    pub fn pop_msg(&mut self) -> Option<HeraMsg<Tx>> {
        self.msg_buf.pop_front()
    }

    pub fn enqueue(
        &mut self,
        msg: HeraMsg<Tx>,
        msg_round: Round,
    ) {
        match msg_round.cmp(&self.current_round) {
            std::cmp::Ordering::Less => {
                debug!("HeraRoundState: dropping old round {} msg", msg_round);
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
        self.blame_best = None;
        self.got_qc = false;
        if let Some(msgs) = self.future_msgs.remove(&self.current_round) {
            for m in msgs {
                self.msg_buf.push_back(m);
            }
        }
        self.future_msgs.retain(|r, _| r >= &self.current_round);
        timer.reset();
        *timer_enabled = true;
    }

    /// Forward catch-up: jump `current_round` directly to `target` (> current).
    /// Used when a *verified* SigPropose / SigBlameQC proves the cluster is
    /// already at a higher round, so this node should rejoin the frontier rather
    /// than strand in `future_msgs`. Stale current-round messages are dropped;
    /// any messages already buffered for `target` are released for processing.
    pub fn fast_forward(
        &mut self,
        target: Round,
        timer: &mut Interval,
        timer_enabled: &mut bool,
    ) {
        if target <= self.current_round {
            return;
        }
        self.current_round = target;
        self.blame_map.clear();
        self.blame_best = None;
        self.got_qc = false;
        // Old current-round messages are now stale (round < target); drop them.
        self.msg_buf.clear();
        if let Some(msgs) = self.future_msgs.remove(&self.current_round) {
            for m in msgs {
                self.msg_buf.push_back(m);
            }
        }
        self.future_msgs.retain(|r, _| r >= &self.current_round);
        timer.reset();
        *timer_enabled = true;
    }

    pub fn disable_timer(
        &self,
        timer_enabled: &mut bool,
    ) {
        *timer_enabled = false;
    }
}

// ---------------------------------------------------------------------------
// Hera consensus actor struct
// ---------------------------------------------------------------------------

/// Maximum number of pending cancel handlers retained per peer.
/// When the queue exceeds this limit the oldest handler is popped (dropped),
/// cancelling that stale in-flight message.  1024 >> a few rounds of sig
/// messages, so the backlog to a late-connecting peer drains before the cap.
pub const MAX_CANCEL_HANDLERS_PER_PEER: usize = 1024;

/// Maximum round gap this node will fast-forward in a single forward-catch-up
/// step. Bounds the deterministic leader-rotation replay (and the gap to fill
/// via sig-element fetch), preventing a Byzantine-inflated round number from
/// forcing an unbounded replay. Normal lag is a few rounds; this is generous.
const MAX_SIG_CATCHUP: Round = 4096;

pub struct Hera<Tx> {
    // ------------------------------------------------------------------
    // Static
    // ------------------------------------------------------------------
    pub(crate) my_id: Id,
    pub(crate) crypto_system: KeyConfig,
    pub(crate) broadcast_peers: Vec<Id>,
    pub(crate) settings: Settings,
    /// Sig-plane reliable sender (retransmits, reconnects; queued per peer).
    pub(crate) consensus_net: TcpReliableSender<Id, HeraMsg<Tx>>,
    /// Per-peer bounded cancel-handler queue.
    /// Retains up to MAX_CANCEL_HANDLERS_PER_PEER handlers per peer;
    /// drops oldest on overflow (cancels the stale in-flight message).
    pub(crate) cancel_handlers: FnvHashMap<Id, VecDeque<CancelHandler>>,

    // ------------------------------------------------------------------
    // Channels
    // ------------------------------------------------------------------
    pub(crate) exit_rx: oneshot::Receiver<()>,
    /// Sig-plane inbound (unbounded, never drop).
    pub(crate) rx_sig_net: UnboundedReceiver<HeraMsg<Tx>>,
    /// Sig-plane self-deliver loopback (unbounded).
    pub(crate) rx_msg_loopback: UnboundedReceiver<HeraMsg<Tx>>,
    pub(crate) tx_msg_loopback: UnboundedSender<HeraMsg<Tx>>,

    /// Cross-plane: send (hash, author) fetch hint to data actor
    /// (fire-and-forget).
    pub(crate) tx_fetch_hint: UnboundedSender<(super::chain_state::DataBlockHash<Tx>, Id)>,
    /// Cross-plane: send committed {author→height} to data actor (unbounded).
    pub(crate) tx_commit_emit: UnboundedSender<CommitEmit<Tx>>,

    // ------------------------------------------------------------------
    // Sig-plane state
    // ------------------------------------------------------------------
    pub(crate) sig_chain_state: SigChainState<Tx>,
    pub(crate) sig_leader_context: SigLeaderContext,
    pub(crate) hera_commit_ctx: HeraCommitContext<Tx>,
    pub(crate) round_state: HeraRoundState<Tx>,
    pub(crate) timer: Interval,
    pub(crate) timer_enabled: bool,
    pub(crate) signature_epoch: u64,

    // ------------------------------------------------------------------
    // Data heads (read-only, from shared ArcSwap map in multi_data_chain)
    // ------------------------------------------------------------------
    pub(crate) head_snapshots:
        Arc<fnv::FnvHashMap<Id, arc_swap::ArcSwap<Arc<super::chain_state::DataHeadSnapshot<Tx>>>>>,
    /// Per-author highest height attested in the last SigPropose from this
    /// node.
    pub(crate) prev_attested_heights: FnvHashMap<Id, u64>,

    // ------------------------------------------------------------------
    // Pending sig proposals park map
    // ------------------------------------------------------------------
    pub(crate) pending_sig_proposals: PendingSigProposals<Tx>,
    pub(crate) pending_sig_element_requests: FnvHashSet<SigElementHash<Tx>>,

    // ------------------------------------------------------------------
    // Heartbeat (info-level round/commit progress)
    // ------------------------------------------------------------------
    pub(crate) bench_emit_interval: tokio::time::Interval,
    pub(crate) emit_dp: bool,
}

// ---------------------------------------------------------------------------
// Constants + cancel-handler bookkeeping
// ---------------------------------------------------------------------------

impl<Tx> Hera<Tx> {
    pub const INITIAL_LEADER: Id = START_ID;
    pub const INITIAL_ROUND: Round = 0;
    pub const STEADY_STATE_SIG_EPOCH: u64 = 1;
    pub const INITIAL_DATA_EPOCH: u64 = 1;

    /// Push a cancel handler onto the per-peer bounded queue.
    ///
    /// If the queue length now exceeds `MAX_CANCEL_HANDLERS_PER_PEER`,
    /// pop_front (drop) the oldest handler — cancelling that stale message.
    /// Recent (liveness-relevant) handlers are retained; memory is bounded
    /// per peer even for a permanently-dead peer.
    pub(crate) fn push_cancel_handler(
        &mut self,
        peer: Id,
        h: CancelHandler,
    ) {
        let q = self.cancel_handlers.entry(peer).or_default();
        q.push_back(h);
        if q.len() > MAX_CANCEL_HANDLERS_PER_PEER {
            q.pop_front();
        }
    }
}

// ---------------------------------------------------------------------------
// spawn() + run()
// ---------------------------------------------------------------------------

impl<Tx> Hera<Tx>
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
        max_committed_heads_len: Arc<std::sync::atomic::AtomicUsize>,
        my_height_atomic: Arc<std::sync::atomic::AtomicU64>,
        my_committed_height_atomic: Arc<std::sync::atomic::AtomicU64>,
        max_chain_lead: u64,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug + 'static,
    {
        crate::server::init_gc_depth_rounds(settings.committee_config.num_nodes());

        let me = settings
            .committee_config
            .get(&my_id)
            .ok_or_else(|| anyhow!("My Id {} not in config", my_id))?;

        // ---------------------------------------------------------------
        // Two physical planes: sig uses tcp-reliable-sender/tcp-receiver;
        // data uses HeraNet (bounded per-peer outbox, try_send-drop).
        // ---------------------------------------------------------------
        let consensus_addr = crate::to_socket_address("0.0.0.0", me.consensus_port)?;
        let data_addr = crate::to_socket_address("0.0.0.0", me.data_port)?;

        let num_nodes = settings.committee_config.num_nodes();
        let mut data_addresses: Vec<SocketAddr> = Vec::with_capacity(num_nodes);
        for id in 0..num_nodes {
            let party = settings
                .committee_config
                .get(&id)
                .ok_or_else(|| anyhow!("Id {} not in config", id))?;
            data_addresses.push(crate::to_socket_address(
                &party.consensus_address,
                party.data_port,
            )?);
        }

        // Sig plane: TcpReliableSender (retransmits, reconnects) outbound.
        let sig_peers = settings.get_consensus_peers(my_id)?;
        let consensus_net = TcpReliableSender::<Id, HeraMsg<Tx>>::with_peers(sig_peers);

        // Sig plane: TcpReceiver inbound — forward deserialized msgs to rx_sig_net.
        let (tx_sig_net, rx_sig_net) = unbounded_channel::<HeraMsg<Tx>>();
        let mut sig_receiver = tcp_receiver::TcpReceiver::<HeraMsg<Tx>>::spawn(consensus_addr);
        tokio::spawn(async move {
            while let Some(result) = sig_receiver.next().await {
                match result {
                    Ok(msg) => {
                        INFLIGHT_SIG_NET.fetch_add(1, AOrdering::Relaxed);
                        if tx_sig_net.send(msg).is_err() {
                            break;
                        }
                    }
                    Err(e) => log::warn!("Hera sig: receive error: {e}"),
                }
            }
        });

        // ---------------------------------------------------------------
        // Data plane: reliable sender + receiver (same transport as the sig
        // plane). DataPropose is paced by an n-f ack gate in the data actor
        // (propose height h+1 only once >= n-f nodes received h), so the data
        // volume is low and reliable delivery is affordable. Reliable delivery
        // means every block reaches >= n-f nodes, so a referenced head is
        // always available and the consensus gate (notify_read) never stalls on
        // a lost block. DataRequest/DataResponse remain as a reconnect backstop.
        // ---------------------------------------------------------------
        let _ = data_addresses; // (data peer map built from settings below)
        let data_peers = settings.get_data_peers(my_id)?;
        let data_net = TcpReliableSender::<Id, HeraMsg<Tx>>::with_peers(data_peers);

        // Data-plane inbound: TcpReceiver → verify (ed25519) in the forwarding
        // task (off the actor loop) → rx_verified. The n-f ack gate keeps the
        // inbound rate O(n)/round so a single verify task is not a bottleneck.
        let crypto_for_verify = crypto_system.clone();
        let (tx_verified, rx_verified) = unbounded_channel::<HeraMsg<Tx>>();
        let mut data_receiver = tcp_receiver::TcpReceiver::<HeraMsg<Tx>>::spawn(data_addr);
        tokio::spawn(async move {
            while let Some(result) = data_receiver.next().await {
                match result {
                    Ok(msg) => {
                        let ok = match &msg {
                            HeraMsg::DataPropose { block, .. }
                            | HeraMsg::DataResponse { block, .. } => {
                                verify_data_block_sig(&crypto_for_verify, block)
                            }
                            HeraMsg::DataRequest { .. } => true,
                            _ => false,
                        };
                        if ok && tx_verified.send(msg).is_err() {
                            break; // data actor gone
                        }
                    }
                    Err(e) => log::warn!("Hera data: receive error: {e}"),
                }
            }
        });

        // ---------------------------------------------------------------
        // Batcher
        // ---------------------------------------------------------------
        let (tx_consensus_to_batcher, rx_consensus_to_batcher) = unbounded_channel();
        let batching_params = Parameters::new(
            my_id,
            my_id,
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

        let num_faults = settings.committee_config.num_faults();

        // ---------------------------------------------------------------
        // HeraCommitContext (sig-chain commit task)
        // ---------------------------------------------------------------
        let hera_commit_ctx = HeraCommitContext::<Tx>::spawn(
            SigChainState::new(store.clone()),
            num_nodes,
            num_faults,
        );

        let all_peers_except_me: Vec<Id> =
            all_peers.iter().filter(|x| *x != &my_id).cloned().collect();

        // Sig-plane loopback (unbounded, sig-plane discipline).
        let (tx_msg_loopback, rx_msg_loopback) = unbounded_channel();

        let all_ids = settings.committee_config.get_all_ids();
        // MultiAuthorDataChainState is owned by the data actor; we only
        // need the shared head_snapshots for the consensus actor.
        let multi_data_chain = MultiAuthorDataChainState::<Tx>::new(&all_ids);
        let head_snapshots = Arc::clone(&multi_data_chain.head_snapshots);

        // ---------------------------------------------------------------
        // Shared store for gating: the data actor writes
        // data_block_key(hash) on admission; the consensus actor calls
        // store.notify_read(data_block_key(hash)) in each GateFut.
        // Both actors hold a clone of the same Storage handle (same channel).
        // ---------------------------------------------------------------

        // ---------------------------------------------------------------
        // Cross-plane channels (unbounded — sig-plane discipline).
        // ---------------------------------------------------------------
        let (tx_fetch_hint, rx_fetch_hint) =
            unbounded_channel::<(super::chain_state::DataBlockHash<Tx>, Id)>();
        let (tx_commit_emit, rx_commit_emit) = unbounded_channel::<CommitEmit<Tx>>();

        let emit_dp = my_id == settings.bench_config.bench_metrics_node;
        let bench_emit_window_secs = settings.bench_config.bench_emit_window_secs.max(1) as f64;
        let bench_emit_interval = interval(std::time::Duration::from_secs(
            settings.bench_config.bench_emit_window_secs.max(1),
        ));

        // ---------------------------------------------------------------
        // Data actor
        // ---------------------------------------------------------------
        let data_actor = HeraDataActor::<Tx> {
            my_id,
            crypto_system: crypto_system.clone(),
            broadcast_peers: all_peers_except_me.clone(),
            current_epoch: Hera::<Tx>::INITIAL_DATA_EPOCH,
            data_net,
            n_minus_t: num_nodes.saturating_sub(num_faults),
            can_propose: true,
            acks_for_current: 0,
            awaiting_acks: FuturesUnordered::new(),
            _exit_placeholder: None,
            rx_verified_blocks: rx_verified,
            rx_data_batch,
            rx_fetch_hint,
            rx_commit_emit,
            tx_data_commit: tx_data_commit.clone(),
            tx_consensus_to_batcher: tx_consensus_to_batcher.clone(),
            block_store: fnv::FnvHashMap::default(),
            multi_data_chain,
            pending_data_requests: fnv::FnvHashMap::default(),
            my_height: 0,
            my_last_hash: None,
            commit_event_count: 0,
            committed_tx_count: 0,
            bench_emit_interval: interval(std::time::Duration::from_secs(
                settings.bench_config.bench_emit_window_secs.max(1),
            )),
            emit_dp,
            bench_emit_window_secs,
            latency_samples_ms: Vec::with_capacity(8192),
            max_committed_heads_len: Arc::clone(&max_committed_heads_len),
            max_chain_lead,
            my_height_atomic: Arc::clone(&my_height_atomic),
            my_committed_height_atomic: Arc::clone(&my_committed_height_atomic),
        };

        tokio::spawn(async move {
            let mut da = data_actor;
            if let Err(e) = da.run().await {
                error!("HeraDataActor error: {}", e);
            }
        });

        // ---------------------------------------------------------------
        // Consensus (sig) actor
        // ---------------------------------------------------------------
        let consensus_actor = Hera::<Tx> {
            my_id,
            crypto_system,
            broadcast_peers: all_peers_except_me,
            exit_rx,
            rx_sig_net,
            consensus_net,
            cancel_handlers: FnvHashMap::default(),
            tx_msg_loopback,
            rx_msg_loopback,
            tx_fetch_hint,
            tx_commit_emit,
            sig_chain_state: SigChainState::new(store.clone()),
            sig_leader_context: SigLeaderContext::new(
                settings.committee_config.get_all_ids(),
                num_faults,
            ),
            hera_commit_ctx,
            round_state: HeraRoundState::new(num_nodes),
            timer: interval(std::time::Duration::from_millis(
                std::env::var("HERA_ROUND_TIMER_MS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(4 * settings.bench_config.delay_in_ms),
            )),
            timer_enabled: true,
            signature_epoch: Hera::<Tx>::STEADY_STATE_SIG_EPOCH,
            head_snapshots,
            prev_attested_heights: FnvHashMap::default(),
            pending_sig_proposals: FnvHashMap::default(),
            pending_sig_element_requests: FnvHashSet::default(),
            bench_emit_interval,
            emit_dp,
            settings,
        };

        tokio::spawn(async move {
            let mut p = consensus_actor;
            if let Err(e) = p.run().await {
                error!("Hera consensus error: {}", e);
            }
        });

        Ok(())
    }

    async fn run(&mut self) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug + 'static,
    {
        info!("Hera: starting (node {})", self.my_id);
        self.sig_chain_state.genesis_setup().await?;
        // No startup gate needed: TcpReliableSender queues and retransmits
        // until each peer connects, so the bootstrap proposal is reliably
        // delivered even to late-connecting peers.

        // PROFILE INSTRUMENTATION: independent monitor task.
        {
            let my_id = self.my_id;
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
                loop {
                    ticker.tick().await;
                    let sig_net = INFLIGHT_SIG_NET.load(AOrdering::Relaxed);
                    let loopback = INFLIGHT_LOOPBACK.load(AOrdering::Relaxed);
                    let tx_inner = INFLIGHT_COMMIT_TX_INNER.load(AOrdering::Relaxed);
                    let committed = INFLIGHT_COMMITTED.load(AOrdering::Relaxed);
                    #[cfg(target_os = "linux")]
                    let rss_kb: i64 = {
                        std::fs::read_to_string("/proc/self/status")
                            .ok()
                            .and_then(|s| {
                                s.lines()
                                    .find(|l| l.starts_with("VmRSS:"))
                                    .and_then(|l| l.split_whitespace().nth(1))
                                    .and_then(|v| v.parse().ok())
                            })
                            .unwrap_or(-1)
                    };
                    #[cfg(not(target_os = "linux"))]
                    let rss_kb: i64 = -1;
                    eprintln!(
                        "CHANDEPTH: node={} sig_net={} loopback={} \
                         commit_tx_inner={} rx_committed={} rss_kb={}",
                        my_id, sig_net, loopback, tx_inner, committed, rss_kb,
                    );
                }
            });
        }

        self.timer.reset();
        self.bench_emit_interval.tick().await;

        // If this node is the first sig-chain leader, kick off the first proposal.
        if self.sig_leader_context.leader() == self.my_id {
            if let Err(e) = self.handle_new_sig_round().await {
                error!("Hera: initial handle_new_sig_round: {}", e);
            }
        }

        loop {
            tokio::select! {
                biased;

                // Exit.
                exit_val = &mut self.exit_rx => {
                    exit_val.map_err(anyhow::Error::new)?;
                    info!("Hera: exit signal.");
                    break;
                }

                // Round timer (blame) — must fire on time regardless of load.
                _ = self.timer.tick(), if self.timer_enabled => {
                    if let Err(e) = self.on_round_timeout().await {
                        error!("Hera: on_round_timeout: {}", e);
                    }
                }

                // Committed attestation from HeraCommitContext.
                committed = self.hera_commit_ctx.rx_committed.recv() => {
                    INFLIGHT_COMMITTED.fetch_sub(1, AOrdering::Relaxed);
                    if let Some(c) = committed {
                        self.on_committed_attestation(c);
                    } else {
                        warn!("Hera main: rx_committed closed");
                    }
                }

                // Sig-plane self-deliver loopback (unbounded).
                msg = self.rx_msg_loopback.recv() => {
                    INFLIGHT_LOOPBACK.fetch_sub(1, AOrdering::Relaxed);
                    let msg = msg.ok_or_else(|| anyhow!("Hera: loopback closed"))?;
                    if let Err(e) = self.dispatch(msg).await {
                        error!("Hera: loopback dispatch: {}", e);
                    }
                }

                // Sig-plane network intake (unbounded, never drop).
                msg = self.rx_sig_net.recv() => {
                    INFLIGHT_SIG_NET.fetch_sub(1, AOrdering::Relaxed);
                    let msg = msg.ok_or_else(|| anyhow!("Hera: sig net closed"))?;
                    if let Err(e) = self.dispatch(msg).await {
                        error!("Hera: sig net dispatch: {}", e);
                    }
                }

                // Round-ordered sig-plane messages (buffered).
                _ = async {}, if self.round_state.is_ready() => {
                    if let Some(msg) = self.round_state.pop_msg() {
                        if let Err(e) = self.handle_sig_ordered(msg).await {
                            error!("Hera: handle_sig_ordered: {}", e);
                        }
                    }
                }

                // Heartbeat (info-level round/commit progress).
                _ = self.bench_emit_interval.tick(), if self.emit_dp => {
                    info!(
                        "Hera HB: round={} highest_chained={}",
                        self.round_state.round(),
                        self.sig_chain_state.highest_chain().proposal.round(),
                    );
                    let sig_net = INFLIGHT_SIG_NET.load(AOrdering::Relaxed);
                    let loopback = INFLIGHT_LOOPBACK.load(AOrdering::Relaxed);
                    let tx_inner = INFLIGHT_COMMIT_TX_INNER.load(AOrdering::Relaxed);
                    let committed_depth = INFLIGHT_COMMITTED.load(AOrdering::Relaxed);
                    info!(
                        "CHAN_DEPTH: sig_net={} loopback={} commit_tx_inner={} rx_committed={}",
                        sig_net, loopback, tx_inner, committed_depth,
                    );
                }
            }
        }
        info!("Hera: shut down.");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Top-level dispatch (sig-plane only)
    // -----------------------------------------------------------------------

    pub(crate) async fn dispatch(
        &mut self,
        msg: HeraMsg<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        match msg {
            // Sig-plane — route through round ordering.
            HeraMsg::SigPropose {
                ref proposal,
                ref auth,
                ..
            } => {
                let r = proposal.round();
                // Forward catch-up: a SigPropose for a round ahead of us proves
                // (once its leader signature checks out) that the cluster has
                // moved on. Fast-forward to r instead of stranding it in
                // future_msgs; the parent sig-element chain is filled by the
                // existing fetch path when the proposal is processed.
                if r > self.round_state.round() && self.verify_future_proposal(proposal, auth, r) {
                    self.catch_up_to(r);
                }
                self.round_state.enqueue(msg, r);
                Ok(())
            }
            HeraMsg::SigBlameQC { round, ref qc, .. } => {
                // A valid blame-QC for `round` is unforgeable proof the cluster
                // blamed `round` and advanced. Catch up to `round` so the normal
                // handler advances us to round+1.
                if round > self.round_state.round() {
                    let blame_hash = Hash::ser_and_hash(&round);
                    if qc.verify(&blame_hash, &self.crypto_system.system).is_ok() {
                        info!(
                            "Hera[n{}]: CATCH-UP to round {} via valid SigBlameQC (was {})",
                            self.my_id,
                            round,
                            self.round_state.round()
                        );
                        self.catch_up_to(round);
                    }
                }
                self.round_state.enqueue(msg, round);
                Ok(())
            }
            HeraMsg::SigBlame { round, .. } => {
                self.round_state.enqueue(msg, round);
                Ok(())
            }

            // Sig-element catch-up — handled directly (not round-ordered).
            HeraMsg::SigElementRequest { source, request } => {
                self.on_sig_element_request(request.request_hash().clone(), source)
                    .await
            }
            HeraMsg::SigElementResponse { response } => {
                self.on_sig_element_response(response.response()).await
            }

            // Data-plane messages must NOT arrive on the sig transport.
            HeraMsg::DataPropose { .. }
            | HeraMsg::DataRequest { .. }
            | HeraMsg::DataResponse { .. } => {
                debug!("Hera: data msg on sig transport — ignoring (bug in caller)");
                Ok(())
            }

            _ => {
                debug!("Hera: unhandled msg variant in dispatch");
                Ok(())
            }
        }
    }

    // -----------------------------------------------------------------------
    // Forward catch-up (rejoin the frontier instead of stranding)
    // -----------------------------------------------------------------------

    /// Verify that `proposal` (for round `r` > our current round) carries a
    /// valid signature from round `r`'s legitimate leader. The leader rotation
    /// is a deterministic stateful PRNG walk, so we replay it on a CLONE of the
    /// live leader context (advancing `r - cur` steps) to learn round `r`'s
    /// leader without mutating live state. Bounded by `MAX_SIG_CATCHUP` so a
    /// Byzantine-inflated round number cannot force an unbounded replay.
    fn verify_future_proposal(
        &self,
        proposal: &Proposal<Id, MultiAttestation<Tx>, Round>,
        auth: &Signature<Id, Proposal<Id, MultiAttestation<Tx>, Round>>,
        r: Round,
    ) -> bool
    where
        Tx: Clone + Serialize,
    {
        let cur = self.round_state.round();
        if r <= cur || r - cur > MAX_SIG_CATCHUP {
            return false;
        }
        let mut ctx = self.sig_leader_context.clone();
        for _ in 0..(r - cur) {
            ctx.advance_round();
        }
        let leader = ctx.leader();
        let Some(pk) = self.crypto_system.system.get(&leader) else {
            return false;
        };
        let prop_hash = Hash::ser_and_hash(proposal);
        if auth.verify(&prop_hash, &leader, pk).is_ok() {
            info!(
                "Hera[n{}]: CATCH-UP to round {} via valid SigPropose (was {})",
                self.my_id, r, cur
            );
            true
        } else {
            warn!(
                "Hera[n{}]: FUTURE-PROP-REJECT round={} (cur={}) computed_leader={} sig_invalid",
                self.my_id, r, cur, leader
            );
            false
        }
    }

    /// Fast-forward both the round buffer and the (lock-step) leader context to
    /// `target`. Caller must have already validated that `target` is justified
    /// (verified proposal or blame-QC) and within `MAX_SIG_CATCHUP`.
    fn catch_up_to(
        &mut self,
        target: Round,
    ) {
        let cur = self.round_state.round();
        if target <= cur || target - cur > MAX_SIG_CATCHUP {
            return;
        }
        for _ in 0..(target - cur) {
            self.sig_leader_context.advance_round();
        }
        self.round_state
            .fast_forward(target, &mut self.timer, &mut self.timer_enabled);
    }

    async fn handle_sig_ordered(
        &mut self,
        msg: HeraMsg<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        match msg {
            HeraMsg::SigPropose {
                proposal,
                auth,
                attestation,
                sender,
            } => {
                self.handle_sig_proposal(proposal, auth, attestation, sender)
                    .await
            }
            HeraMsg::SigBlame {
                round,
                auth,
                highest_round,
                highest_hash,
            } => {
                self.handle_sig_blame(round, auth, highest_round, highest_hash)
                    .await
            }
            HeraMsg::SigBlameQC {
                round,
                qc,
                highest_round,
                highest_hash,
            } => {
                self.on_sig_blame_qc(round, qc, highest_round, highest_hash)
                    .await
            }
            _ => {
                debug!("Hera: unexpected msg in handle_sig_ordered");
                Ok(())
            }
        }
    }

    /// Returns the current rleader id.
    pub fn rleader(&self) -> Id {
        self.sig_leader_context.leader()
    }

    /// Emit committed {author→height} set to the data actor via unbounded
    /// tx_commit_emit channel (sig-plane discipline, never drop).
    pub(crate) fn on_committed_attestation(
        &mut self,
        committed: super::phases::HeraCommittedAttestation<Tx>,
    ) where
        Tx: Clone + Serialize,
    {
        let commit = CommitEmit {
            heads: committed.attestation.envelope.heads.clone(),
            sig_round: committed.sig_round,
        };
        if self.tx_commit_emit.send(commit).is_err() {
            error!("Hera: tx_commit_emit closed — data actor gone");
        }
    }
}

impl<Tx> std::fmt::Debug for Hera<Tx> {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("Hera")
            .field("my_id", &self.my_id)
            .field("signature_epoch", &self.signature_epoch)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Per-peer verify helper (used in the inbound pump task)
// ---------------------------------------------------------------------------

/// Verify a DataBlock's author signature. Used in the data-plane inbound pump
/// task (off the actor loop) so verification parallelizes across peers/cores.
fn verify_data_block_sig<Tx>(
    crypto_system: &KeyConfig,
    block: &crate::types::DataBlock<Tx>,
) -> bool
where
    Tx: serde::Serialize + Clone,
{
    let author = block.sig.signer;
    let pk = match crypto_system.system.get(&author) {
        Some(pk) => pk,
        None => return false,
    };
    let env_hash = block.hash();
    pk.verify(env_hash.as_ref(), &block.sig.raw)
}
