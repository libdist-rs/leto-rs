/// Hera core struct and main event loop.
///
/// Mirrors `zeus/core.rs` with these key differences:
///   - Every node runs its own data sub-chain (no single eleader).
///   - The sig-plane carries `MultiAttestation<Tx>` payloads.
///   - No eleader blame / eleader change logic.
///   - No `rleader_waiting_fresh` state.
///   - `my_height` / `my_last_hash` track this node's own proposed tip.
///   - `prev_attested_heights` tracks, per-author, the height last attested in
///     a sig-block proposal from this node.
use crate::{
    server::{BatcherConsensusMsg, ChainState, LeaderContext, Parameters, RRBatcher, Settings},
    types::{hera::MultiAttestation, Element, HeraMsg, Signature, Transaction},
    Id, KeyConfig, Round, START_ID,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use fnv::{FnvHashMap, FnvHashSet};
use log::*;
use mempool::Batch;
use serde::Serialize;
use std::net::SocketAddr;
use std::{collections::VecDeque, sync::Arc};
use storage::rocksdb::Storage;
// Hera broadcasts its data/sig messages all-to-all, which melts libnet's
// reconnect-storm-prone senders at large n and wedges the sig-chain. The
// HeraNet transport (ported from mysticeti) keeps one persistent connection per
// peer pair with jittered reconnect backoff and bounded drop-on-full channels,
// so the flood is shed instead of melting the mesh. See hera::net.
use super::net::HeraNet;
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tokio::time::{interval, Interval};

use super::chain_state::{DataBlockHash, MultiAuthorDataChainState};
use super::phases::HeraCommitContext;

/// Data-plane messages (the high-volume all-to-all flood). Everything else
/// (sig proposals, blames, blame-QCs, sig-element catch-up) is treated as
/// priority sig-plane traffic.
fn is_data_plane<Tx>(msg: &HeraMsg<Tx>) -> bool {
    matches!(
        msg,
        HeraMsg::DataPropose { .. } | HeraMsg::DataRequest { .. } | HeraMsg::DataResponse { .. }
    )
}

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
// Park map for attestations awaiting a missing data block.
// ---------------------------------------------------------------------------
pub type PendingAttestations<Tx> = FnvHashMap<DataBlockHash<Tx>, Vec<HeraMsg<Tx>>>;

// Park map for sig proposals awaiting a missing PARENT sig-element. A node
// that advanced a round via blame-QC never stored that round's element; a
// later proposal then references a parent it lacks. We fetch the parent
// (SigElementRequest) before agreeing, so the committer never sees a gap.
pub type PendingSigProposals<Tx> = FnvHashMap<SigElementHash<Tx>, Vec<HeraMsg<Tx>>>;

// ---------------------------------------------------------------------------
// ZeusRoundState equivalent for Hera (same structure, HeraMsg type)
// ---------------------------------------------------------------------------

/// Minimal round-ordering buffer for Hera sig-plane messages.
pub struct HeraRoundState<Tx> {
    current_round: Round,
    msg_buf: VecDeque<HeraMsg<Tx>>,
    future_msgs: FnvHashMap<Round, VecDeque<HeraMsg<Tx>>>,
    pub blame_map: FnvHashMap<Id, Signature<Id, Round>>,
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

    pub fn disable_timer(
        &self,
        timer_enabled: &mut bool,
    ) {
        *timer_enabled = false;
    }
}

// ---------------------------------------------------------------------------
// Hera struct
// ---------------------------------------------------------------------------

pub struct Hera<Tx> {
    // ------------------------------------------------------------------
    // Static
    // ------------------------------------------------------------------
    pub(crate) my_id: Id,
    pub(crate) crypto_system: KeyConfig,
    pub(crate) broadcast_peers: Vec<Id>,
    pub(crate) settings: Settings,
    pub(crate) consensus_net: HeraNet,

    // ------------------------------------------------------------------
    // Channels
    // ------------------------------------------------------------------
    pub(crate) exit_rx: oneshot::Receiver<()>,
    /// Sig-plane (consensus) inbound — drained with priority over the data
    /// plane so the leader's low-volume but critical SigPropose/blame/blame-QC
    /// messages are never buried behind the all-to-all data flood (which
    /// desynchronizes the round state machine and wedges the sig-chain at large
    /// n -- see hera-n61-stall-diagnosis).
    pub(crate) rx_sig_net: UnboundedReceiver<HeraMsg<Tx>>,
    /// Data-plane inbound — lowest priority.
    pub(crate) rx_data_net: UnboundedReceiver<HeraMsg<Tx>>,
    pub(crate) rx_msg_loopback: UnboundedReceiver<HeraMsg<Tx>>,
    pub(crate) tx_msg_loopback: UnboundedSender<HeraMsg<Tx>>,
    pub(crate) tx_consensus_to_batcher: UnboundedSender<BatcherConsensusMsg<Id, Tx>>,
    pub(crate) rx_data_batch: UnboundedReceiver<Batch<Tx>>,
    #[allow(dead_code)]
    pub(crate) tx_data_commit: UnboundedSender<Arc<Batch<Tx>>>,

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
    // Data-plane state
    // ------------------------------------------------------------------
    /// Current data-plane epoch.
    pub(crate) current_epoch: u64,
    /// Per-author data-chain state (all n sub-chains).
    pub(crate) multi_data_chain: MultiAuthorDataChainState<Tx>,
    /// Park map: missing data hash → parked HeraMsg entries.
    pub(crate) pending_attestations: PendingAttestations<Tx>,
    /// Park map: missing parent sig-element hash → parked SigPropose entries.
    pub(crate) pending_sig_proposals: PendingSigProposals<Tx>,
    /// Sig-element hashes we have an outstanding fetch for. Lets us accept a
    /// recursively-fetched ancestor (which has no parked proposal of its own)
    /// and dedupe in-flight requests.
    pub(crate) pending_sig_element_requests: FnvHashSet<SigElementHash<Tx>>,

    // ------------------------------------------------------------------
    // This node's own sub-chain state
    // ------------------------------------------------------------------
    /// Height of the most recently proposed block by this node.
    pub(crate) my_height: u64,
    /// Hash of the most recently proposed block by this node.
    pub(crate) my_last_hash: Option<DataBlockHash<Tx>>,
    /// Per-author highest height attested in the last SigPropose from this
    /// node.
    pub(crate) prev_attested_heights: FnvHashMap<Id, u64>,

    // ------------------------------------------------------------------
    // DP[Throughput] emission
    // ------------------------------------------------------------------
    pub(crate) committed_tx_count: u64,
    pub(crate) bench_emit_interval: tokio::time::Interval,
    pub(crate) emit_dp: bool,
    pub(crate) bench_emit_window_secs: f64,
    /// Per-window latency samples (ms).  Populated in `on_committed_attestation`
    /// for txs that carry a `hera_timestamp_ns()`; consumed by the emission
    /// tick and cleared.  Capacity-bounded to avoid memory blow-up at very high
    /// commit rates (we down-sample after the cap).
    pub(crate) latency_samples_ms: Vec<u64>,

    // ------------------------------------------------------------------
    // Test hook: max heads len across all committed attestations
    // ------------------------------------------------------------------
    pub(crate) max_committed_heads_len: Arc<std::sync::atomic::AtomicUsize>,

    // ------------------------------------------------------------------
    // Self-pacing: cumulative count of THIS node's own txs that have
    // committed (bumped in on_committed_attestation for author == my_id).
    // The load generator pauses when (generated - this) exceeds a cap, so
    // load creation tracks commit progress instead of flooding the network.
    // ------------------------------------------------------------------
    pub(crate) my_committed_txs: Arc<std::sync::atomic::AtomicU64>,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

impl<Tx> Hera<Tx> {
    pub const INITIAL_LEADER: Id = START_ID;
    pub const INITIAL_ROUND: Round = 0;
    pub const STEADY_STATE_SIG_EPOCH: u64 = 1;
    pub const INITIAL_DATA_EPOCH: u64 = 1;
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
        my_committed_txs: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug + 'static,
    {
        crate::server::init_gc_depth_rounds(settings.committee_config.num_nodes());
        let me = settings
            .committee_config
            .get(&my_id)
            .ok_or_else(|| anyhow!("My Id {} not in config", my_id))?;
        let consensus_addr = crate::to_socket_address("0.0.0.0", me.consensus_port)?;

        // Mysticeti-style persistent-connection transport (replaces libnet's
        // reconnect-storm-prone TcpSimpleSender/TcpReceiver). One connection
        // per peer pair, jittered reconnect backoff, bounded drop-on-full
        // channels -- so hera's all-to-all flood is shed instead of melting the
        // mesh and wedging the sig-chain (see hera::net).
        let num_nodes = settings.committee_config.num_nodes();
        let mut net_addresses: Vec<SocketAddr> = Vec::with_capacity(num_nodes);
        for id in 0..num_nodes {
            let party = settings
                .committee_config
                .get(&id)
                .ok_or_else(|| anyhow!("Id {} not in config", id))?;
            net_addresses
                .push(crate::to_socket_address(&party.consensus_address, party.consensus_port)?);
        }
        let (consensus_net, mut inbound_rx) =
            HeraNet::spawn(net_addresses, my_id, consensus_addr);

        // Deserialize inbound frames into HeraMsg and split into two priority
        // classes: sig-plane (consensus) and data-plane. The run loop drains the
        // sig channel before the data channel so a data flood can't bury the
        // critical sig messages.
        let (tx_sig_net, rx_sig_net) = unbounded_channel();
        let (tx_data_net, rx_data_net) = unbounded_channel();
        tokio::spawn(async move {
            while let Some(frame) = inbound_rx.recv().await {
                match bincode::deserialize::<HeraMsg<Tx>>(&frame) {
                    Ok(msg) => {
                        let tx = if is_data_plane(&msg) {
                            &tx_data_net
                        } else {
                            &tx_sig_net
                        };
                        if tx.send(msg).is_err() {
                            break;
                        }
                    }
                    Err(e) => log::warn!("Hera: failed to deserialize inbound frame: {e}"),
                }
            }
        });

        // Batcher: every node is always the leader of its own sub-chain.
        let (tx_consensus_to_batcher, rx_consensus_to_batcher) = unbounded_channel();
        let batching_params = Parameters::new(
            my_id,
            my_id, // initial_leader = self (always)
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

        let num_nodes = settings.committee_config.num_nodes();
        let num_faults = settings.committee_config.num_faults();

        let hera_commit_ctx = HeraCommitContext::<Tx>::spawn(
            SigChainState::new(store.clone()),
            num_nodes,
            num_faults,
        );

        let all_peers_except_me: Vec<Id> =
            all_peers.iter().filter(|x| *x != &my_id).cloned().collect();

        let (tx_msg_loopback, rx_msg_loopback) = unbounded_channel();

        let all_ids = settings.committee_config.get_all_ids();
        let multi_data_chain = MultiAuthorDataChainState::<Tx>::new(&all_ids);

        let emit_dp = my_id == settings.bench_config.bench_metrics_node;
        let bench_emit_window_secs = settings.bench_config.bench_emit_window_secs.max(1) as f64;
        let bench_emit_interval = interval(std::time::Duration::from_secs(
            settings.bench_config.bench_emit_window_secs.max(1),
        ));

        let protocol = Hera::<Tx> {
            my_id,
            crypto_system,
            broadcast_peers: all_peers_except_me,
            exit_rx,
            rx_sig_net,
            rx_data_net,
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
            hera_commit_ctx,
            round_state: HeraRoundState::new(num_nodes),
            // Round (blame) timer. Defaults to 4*delay_in_ms; override with
            // HERA_ROUND_TIMER_MS to tune the blame timeout during experiments.
            timer: interval(std::time::Duration::from_millis(
                std::env::var("HERA_ROUND_TIMER_MS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(4 * settings.bench_config.delay_in_ms),
            )),
            timer_enabled: true,
            signature_epoch: Hera::<Tx>::STEADY_STATE_SIG_EPOCH,
            current_epoch: Hera::<Tx>::INITIAL_DATA_EPOCH,
            multi_data_chain,
            pending_attestations: FnvHashMap::default(),
            pending_sig_proposals: FnvHashMap::default(),
            pending_sig_element_requests: FnvHashSet::default(),
            my_height: 0,
            my_last_hash: None,
            prev_attested_heights: FnvHashMap::default(),
            committed_tx_count: 0,
            latency_samples_ms: Vec::with_capacity(8192),
            bench_emit_interval,
            emit_dp,
            bench_emit_window_secs,
            max_committed_heads_len,
            my_committed_txs,
            settings,
        };

        tokio::spawn(async move {
            let mut p = protocol;
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

        // Startup gate: HeraNet establishes connections lazily (active/passive
        // handshake + jittered 1-5s backoff). The bootstrap sig proposal is
        // broadcast immediately, so without a gate it is dropped to peers whose
        // connection is not yet up -- and the leader never re-proposes, which
        // wedges the cluster at large n (the blame path cannot reconverge once
        // nodes desynchronize). Wait until a quorum of peers has connected
        // before proposing, capped so f crashed peers cannot block liveness.
        {
            let num_nodes = self.settings.committee_config.num_nodes();
            let num_faults = self.settings.committee_config.num_faults();
            // Quorum of *peers* (excluding self): n - f - 1.
            let want = (num_nodes - num_faults).saturating_sub(1);
            // Cap so f crashed peers cannot block startup forever. Override with
            // HERA_STARTUP_GATE_CAP_MS during experiments (default 30s).
            let cap_ms = std::env::var("HERA_STARTUP_GATE_CAP_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30_000);
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_millis(cap_ms);
            while self.consensus_net.connected_peers() < want
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            info!(
                "Hera: startup gate cleared ({} of {} peers connected)",
                self.consensus_net.connected_peers(),
                want
            );
        }

        self.timer.reset();
        self.bench_emit_interval.tick().await;

        // Prime batcher: every node is always the leader of its own sub-chain.
        let _ = self
            .tx_consensus_to_batcher
            .send(BatcherConsensusMsg::NewRound {
                leader: self.my_id,
                round: self.my_height + 1,
            });

        // If this node is the first sig-chain leader, kick off the first proposal.
        if self.sig_leader_context.leader() == self.my_id {
            if let Err(e) = self.handle_new_sig_round().await {
                error!("Hera: initial handle_new_sig_round: {}", e);
            }
        }

        loop {
            tokio::select! {
                // Biased: branches are tried top-to-bottom, so all control and
                // sig-plane traffic is drained before the data plane. This keeps
                // the leader's SigPropose/blame/blame-QC out from behind the
                // all-to-all data flood, which otherwise desynchronizes the
                // round state machine and wedges the sig-chain at large n.
                biased;

                // Exit
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

                // Committed attestation feedback from HeraCommitContext.
                committed = self.hera_commit_ctx.rx_committed.recv() => {
                    if let Some(c) = committed {
                        self.on_committed_attestation(c);
                    } else {
                        warn!("Hera main: rx_committed closed");
                    }
                }

                // Loopback (this node's own blames/relays re-entering).
                msg = self.rx_msg_loopback.recv() => {
                    let msg = msg.ok_or_else(|| anyhow!("Hera: loopback closed"))?;
                    if let Err(e) = self.dispatch(msg).await {
                        error!("Hera: loopback dispatch: {}", e);
                    }
                }

                // Priority sig-plane network intake.
                msg = self.rx_sig_net.recv() => {
                    let msg = msg.ok_or_else(|| anyhow!("Hera: sig net closed"))?;
                    if let Err(e) = self.dispatch(msg).await {
                        error!("Hera: sig net dispatch: {}", e);
                    }
                }

                // Round-ordered sig-plane messages (already buffered).
                _ = async {}, if self.round_state.is_ready() => {
                    if let Some(msg) = self.round_state.pop_msg() {
                        if let Err(e) = self.handle_sig_ordered(msg).await {
                            error!("Hera: handle_sig_ordered: {}", e);
                        }
                    }
                }

                // Data batches from this node's own batcher (data plane).
                batch = self.rx_data_batch.recv() => {
                    let batch = batch.ok_or_else(|| anyhow!("Hera: data batcher shut down"))?;
                    if let Err(e) = self.on_self_propose(batch).await {
                        error!("Hera: on_self_propose: {}", e);
                    }
                }

                // Data-plane network intake — lowest priority.
                msg = self.rx_data_net.recv() => {
                    let msg = msg.ok_or_else(|| anyhow!("Hera: data net closed"))?;
                    if let Err(e) = self.dispatch(msg).await {
                        error!("Hera: data net dispatch: {}", e);
                    }
                }

                // DP[Throughput] + DP[Latency] emission.
                _ = self.bench_emit_interval.tick(), if self.emit_dp => {
                    // Heartbeat: lets a normal (Info-level) run show whether the
                    // sig-chain is advancing rounds (rate vs wedge) without the
                    // huge debug logs. round climbs but committed stays 0 ⇒
                    // commit-logic issue; round stuck ⇒ wedge.
                    info!(
                        "Hera HB: round={} highest_chained={} connected={} committed_window={}",
                        self.round_state.round(),
                        self.sig_chain_state.highest_chain().proposal.round(),
                        self.consensus_net.connected_peers(),
                        self.committed_tx_count,
                    );
                    eprintln!(
                        "DP[Throughput]: {}",
                        self.committed_tx_count as f64 / self.bench_emit_window_secs
                    );
                    if !self.latency_samples_ms.is_empty() {
                        // Median (50th percentile).  Cheap to compute, robust to
                        // tail outliers from warmup / GC.  Parser at
                        // scripts/orchestrator/parse.py averages across windows.
                        self.latency_samples_ms.sort_unstable();
                        let mid = self.latency_samples_ms.len() / 2;
                        let median_ms = self.latency_samples_ms[mid];
                        eprintln!("DP[Latency]: {}", median_ms);
                        self.latency_samples_ms.clear();
                    }
                    self.committed_tx_count = 0;
                }
            }
        }
        info!("Hera: shut down.");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Top-level dispatch
    // -----------------------------------------------------------------------

    pub(crate) async fn dispatch(
        &mut self,
        msg: HeraMsg<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        match msg {
            // Data-plane — direct.
            HeraMsg::DataPropose { block, sender } => self.on_data_propose(block, sender).await,
            HeraMsg::DataRequest {
                target_hash,
                source,
            } => self.on_data_request(target_hash, source).await,
            HeraMsg::DataResponse { block } => self.on_data_response(block).await,

            // Sig-plane — route through round ordering.
            HeraMsg::SigPropose { ref proposal, .. } => {
                let r = proposal.round();
                self.round_state.enqueue(msg, r);
                Ok(())
            }
            HeraMsg::SigBlame { round, .. } | HeraMsg::SigBlameQC { round, .. } => {
                let r = round;
                self.round_state.enqueue(msg, r);
                Ok(())
            }

            // Sig-element catch-up — handled directly (not round-ordered), like
            // the data-plane DataRequest/DataResponse. Lets a node fetch a parent
            // sig-element it missed (e.g. advanced past a round via blame-QC)
            // before agreeing on a proposal that extends it.
            HeraMsg::SigElementRequest { source, request } => {
                self.on_sig_element_request(request.request_hash().clone(), source)
                    .await
            }
            HeraMsg::SigElementResponse { response } => {
                self.on_sig_element_response(response.response()).await
            }

            // Remaining sync variants — no-op for now (not wired for Hera v1).
            _ => {
                debug!("Hera: unhandled msg variant in dispatch");
                Ok(())
            }
        }
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
            HeraMsg::SigBlame { round, auth } => self.handle_sig_blame(round, auth).await,
            HeraMsg::SigBlameQC { round, qc } => self.on_sig_blame_qc(round, qc).await,
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
}

impl<Tx> std::fmt::Debug for Hera<Tx> {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("Hera")
            .field("my_id", &self.my_id)
            .field("current_epoch", &self.current_epoch)
            .field("signature_epoch", &self.signature_epoch)
            .finish()
    }
}
