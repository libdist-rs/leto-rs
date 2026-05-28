/// Hera data actor: owns the data plane and its event loop.
///
/// Separated from the consensus (sig) actor so the O(n²) data-block flood
/// never enters the sig event loop (which would bury the low-volume but
/// liveness-critical SigPropose/Blame/BlameQC under the data tide).
///
/// ## Ownership
/// - `MultiAuthorDataChainState`: metadata (heads, pending_data_blocks). Block
///   **bodies** live in a plain `FnvHashMap<DataBlockHash<Tx>, DataBlock<Tx>>`
///   (pure in-memory, no disk spill, GC every 40 commits).
/// - `store`: clone of the shared RocksDB `Storage`. On every valid block
///   admission, `store.write(data_block_key(hash), serialized_block)` is issued
///   so the block's existence is visible to the consensus actor's
///   `Storage::notify_read(data_block_key(hash))` gating path.  The store task
///   resolves notify_read obligations synchronously on write (before RocksDB
///   flush), so gating has no disk-I/O latency on the hot path.
/// - Batcher input (`rx_data_batch`): produces self-proposed blocks.
/// - Data network (`data_net`): binds the data port; receives DataPropose,
///   DataRequest, DataResponse from peers.
/// - My sub-chain state: `my_height`, `my_last_hash`, `current_epoch`.
///
/// ## Cross-plane edges (both unbounded — sig-plane discipline)
/// - `rx_fetch_hint` ← consensus: (hash, author) hint to fetch a missing block.
/// - `rx_commit_emit` ← consensus: `{author→height}` set to commit.
///
/// ## Block admission
/// On a valid block (self-propose, DataPropose, DataResponse):
///   1. `block_store.insert(hash, block)` — synchronous hashmap insert.
///   2. `store.write(data_block_key(hash), bytes)` — triggers notify_read
///      resolution.
///   3. `multi_data_chain.admit_metadata(...)` — advances chain metadata.
///
/// ## GC
/// Every 40 commit events, `gc_block_store()` drops every block whose height
/// is at or below the committed watermark for its author (already emitted;
/// a hopelessly-behind peer would need full sync, not a point fetch).
///
/// ## Poll order (high→low priority, biased select)
/// 1. Exit.
/// 2. Commit emission (rx_commit_emit) — low-volume, liveness-critical.
/// 3. Fetch hint (rx_fetch_hint) — low-volume.
/// 4. Self-propose batch (rx_data_batch) — medium volume.
/// 5. Verified data blocks (recv_many(K), last) — high volume, shedding OK.
use crate::{
    server::{
        hera::chain_state::{
            data_block_key, AdmitTypedResult, DataBlockHash, MultiAuthorDataChainState,
        },
        BatcherConsensusMsg,
    },
    types::{DataBlock, DataBlockEnvelope, DataBlockSig, HeraMsg, Transaction},
    Id, KeyConfig,
};
use anyhow::{anyhow, Result};
use bytes::Bytes;
use crypto::hash::Hash;
use fnv::{FnvHashMap, FnvHashSet};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use log::*;
use mempool::Batch;
use once_cell::sync::OnceCell;
use serde::Serialize;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering as AOrdering};
use std::sync::Arc;
use storage::rocksdb::Storage;
use tcp_reliable_sender::{CancelHandler, TcpReliableSender};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Chain-lead rate-limit
// ---------------------------------------------------------------------------

/// Default maximum number of uncommitted data blocks this node may have ahead
/// of its committed head.  Enforced as a hard cap in `on_self_propose` and a
/// soft pause in the load generator.
///
/// Must be >= 1: a cap of 0 would prevent proposing ANY block (the node's
/// head is always equal to its committed head after commit, so lead == 0 means
/// even height committed_height+1 would be blocked).  Read from env
/// `HERA_MAX_CHAIN_LEAD` at spawn time; clamped to 1 in `Hera::spawn`.
/// Default chain-lead window W. Small by design: this is the flow-control
/// window, not the throughput knob (batch_size carries throughput). It must be
/// >= the per-author blocks produced within one commit-feedback latency (BDP)
/// to keep the pipe full, and the verified-block aggregate is sized to n*W, so
/// keeping W small keeps that buffer cheap. Tunable via `HERA_MAX_CHAIN_LEAD`.
pub const DEFAULT_MAX_CHAIN_LEAD: u64 = 64;

// ---------------------------------------------------------------------------
// Cross-plane message types
// ---------------------------------------------------------------------------

/// Sent from the consensus actor to the data actor: committed `{author→height}`
/// set from a committed sig-chain element.  The data actor walks the range,
/// emits txs, and advances committed_heights.
pub struct CommitEmit<Tx> {
    pub heads: Vec<crate::types::hera::DataHead<Tx>>,
    pub sig_round: u64,
}

// ---------------------------------------------------------------------------
// HeraDataActor
// ---------------------------------------------------------------------------

/// Drain budget for `recv_many` — limits how many data blocks are processed
/// per select iteration so control channels are not starved.
const DATA_DRAIN_BUDGET: usize = 64;

/// Cumulative count of DataRequests we could not serve (requested hash absent
/// from block_store) — a data-availability gap, distinct from a transit drop.
static CANT_SERVE_TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// GC the block store every N commit events.
const GC_EVERY_N_COMMITS: u64 = 40;

pub struct HeraDataActor<Tx> {
    // ------------------------------------------------------------------
    // Static
    // ------------------------------------------------------------------
    pub my_id: Id,
    pub crypto_system: KeyConfig,
    pub broadcast_peers: Vec<Id>,
    pub current_epoch: u64,
    /// Data-plane reliable sender (retransmits, reconnects, per-message ACK).
    pub data_net: TcpReliableSender<Id, HeraMsg<Tx>>,

    // ------------------------------------------------------------------
    // n-f ack gate (data-chain flow control)
    // ------------------------------------------------------------------
    /// Quorum size for the receive-ack gate: a proposer waits until >= n-f
    /// nodes (self + n-f-1 peers) have received its current block before
    /// proposing the next (window = 1). = num_nodes - num_faults.
    pub n_minus_t: usize,
    /// True when the previous self-proposed block has been received by >= n-f
    /// nodes (or there is no outstanding block). Gates `on_self_propose`.
    pub can_propose: bool,
    /// Count of peer ACKs collected for the current outstanding block.
    pub acks_for_current: usize,
    /// CancelHandlers (ACK oneshots) for the current outstanding block; each
    /// resolves when that peer's TcpReceiver acked receipt.
    pub awaiting_acks: FuturesUnordered<CancelHandler>,

    // ------------------------------------------------------------------
    // Channels
    // ------------------------------------------------------------------
    #[allow(dead_code)]
    pub _exit_placeholder: Option<oneshot::Receiver<()>>,
    /// Data-plane inbound from the reliable receiver (after ed25519 verify in
    /// the forwarding task).
    pub rx_verified_blocks: UnboundedReceiver<HeraMsg<Tx>>,
    /// Own batcher output: self-proposed data batches.
    pub rx_data_batch: UnboundedReceiver<Batch<Tx>>,
    /// Cross-plane: fetch hint from consensus — (hash, author) to fetch if
    /// missing. Fire-and-forget; no reply.
    pub rx_fetch_hint: UnboundedReceiver<(DataBlockHash<Tx>, Id)>,
    /// Cross-plane: committed set from consensus actor (unbounded).
    pub rx_commit_emit: UnboundedReceiver<CommitEmit<Tx>>,
    /// Downstream: emit committed tx batches (unbounded, as today).
    pub tx_data_commit: UnboundedSender<Arc<Batch<Tx>>>,
    /// Batcher control (tell the batcher which rounds committed).
    pub tx_consensus_to_batcher: UnboundedSender<BatcherConsensusMsg<Id, Tx>>,

    // ------------------------------------------------------------------
    // Component 1: plain in-memory body store
    // ------------------------------------------------------------------
    /// Full data-block bodies, keyed by content-hash.  GC'd every
    /// `GC_EVERY_N_COMMITS` commit events — entries whose height is at or
    /// below the committed watermark for their author are dropped.
    pub block_store: FnvHashMap<DataBlockHash<Tx>, DataBlock<Tx>>,

    // ------------------------------------------------------------------
    // Component 2: shared RocksDB store (for notify_read gating)
    // ------------------------------------------------------------------
    /// Clone of the shared `Storage` handle.  On every valid block admission,
    /// `store.write(data_block_key(hash), serialized_block)` is issued so that
    /// the consensus actor's `store.notify_read(data_block_key(hash))` GateFut
    /// resolves (the store task wakes obligations synchronously on write,
    /// before the RocksDB flush).
    pub store: Storage,

    // ------------------------------------------------------------------
    // Data-plane state
    // ------------------------------------------------------------------
    pub multi_data_chain: MultiAuthorDataChainState<Tx>,
    /// In-flight data-block fetches (dedup).
    pub pending_data_requests: FnvHashSet<DataBlockHash<Tx>>,

    // ------------------------------------------------------------------
    // Own sub-chain state
    // ------------------------------------------------------------------
    pub my_height: u64,
    pub my_last_hash: Option<DataBlockHash<Tx>>,

    // ------------------------------------------------------------------
    // GC counter
    // ------------------------------------------------------------------
    /// Incremented on every commit event; triggers `gc_block_store` every
    /// `GC_EVERY_N_COMMITS` events.
    pub commit_event_count: u64,

    // ------------------------------------------------------------------
    // DP[Throughput] emission
    // ------------------------------------------------------------------
    pub committed_tx_count: u64,
    pub bench_emit_interval: tokio::time::Interval,
    pub emit_dp: bool,
    pub bench_emit_window_secs: f64,
    pub latency_samples_ms: Vec<u64>,

    // ------------------------------------------------------------------
    // Test hook
    // ------------------------------------------------------------------
    pub max_committed_heads_len: Arc<std::sync::atomic::AtomicUsize>,

    // ------------------------------------------------------------------
    // Chain-lead rate-limit (hard cap in on_self_propose)
    // ------------------------------------------------------------------
    /// Maximum number of uncommitted blocks this node may be ahead of its
    /// committed head.  Read from env `HERA_MAX_CHAIN_LEAD` (default 1000,
    /// clamped to >= 1).  Must be >= 1 for liveness: a cap of 0 would
    /// prevent the node from ever proposing a new block (the committed head
    /// and proposed head would coincide immediately after commit, so every
    /// propose attempt would see lead == 0 >= 0 and be skipped, wedging the
    /// chain permanently).
    pub max_chain_lead: u64,
    /// Shared with the load generator: data actor stores `my_height` here
    /// after each successful self-propose so the load generator can read the
    /// current proposed-tip height without locking.
    pub my_height_atomic: Arc<AtomicU64>,
    /// Shared with the load generator: data actor stores
    /// `committed_heights[my_id]` here in `on_commit_emit` so the load
    /// generator can read the committed watermark without locking.
    pub my_committed_height_atomic: Arc<AtomicU64>,
}

impl<Tx> HeraDataActor<Tx>
where
    Tx: Transaction,
{
    /// Run the data actor event loop.
    pub async fn run(&mut self) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug + serde::de::DeserializeOwned + 'static,
    {
        info!("HeraDataActor: starting (node {})", self.my_id);
        self.bench_emit_interval.tick().await; // consume first immediate tick

        // Prime own batcher.
        let _ = self
            .tx_consensus_to_batcher
            .send(BatcherConsensusMsg::NewRound {
                leader: self.my_id,
                round: self.my_height + 1,
            });

        let mut data_buf: Vec<HeraMsg<Tx>> = Vec::with_capacity(DATA_DRAIN_BUDGET);

        loop {
            tokio::select! {
                biased;

                // 1. Commit emission from consensus (low-volume, liveness-critical).
                commit = self.rx_commit_emit.recv() => {
                    let commit = commit.ok_or_else(|| anyhow!("rx_commit_emit closed"))?;
                    self.on_commit_emit(commit);
                }

                // 2. Fetch hint from consensus (fire-and-forget).
                hint = self.rx_fetch_hint.recv() => {
                    let (hash, author) = hint.ok_or_else(|| anyhow!("rx_fetch_hint closed"))?;
                    self.request_data(hash, author).await;
                }

                // 3. n-f ack gate: count peer ACKs for the outstanding block.
                //    When >= n-f-1 peers (plus self) have received it, release
                //    the self-propose gate. Polled above self-propose so the gate
                //    opens promptly. Disabled when no block is outstanding.
                ack = self.awaiting_acks.next(), if !self.awaiting_acks.is_empty() => {
                    if matches!(ack, Some(Ok(Ok(_)))) {
                        self.acks_for_current += 1;
                        // self counts as 1, so n-f-1 peer ACKs suffice.
                        if self.acks_for_current + 1 >= self.n_minus_t {
                            self.can_propose = true;
                            self.awaiting_acks.clear();
                        }
                    }
                }

                // 4. Self-propose batch — gated: only when the previous block has
                //    been received by >= n-f nodes (window = 1).
                batch = self.rx_data_batch.recv(), if self.can_propose => {
                    let batch = batch.ok_or_else(|| anyhow!("rx_data_batch closed"))?;
                    if let Err(e) = self.on_self_propose(batch).await {
                        error!("HeraDataActor: on_self_propose: {e}");
                    }
                }

                // 5. Verified data blocks — last, drained in batches (bounded budget).
                n = self.rx_verified_blocks.recv_many(&mut data_buf, DATA_DRAIN_BUDGET) => {
                    if n == 0 {
                        return Err(anyhow!("rx_verified_blocks closed"));
                    }
                    for msg in data_buf.drain(..) {
                        if let Err(e) = self.dispatch_data(msg).await {
                            error!("HeraDataActor: dispatch_data: {e}");
                        }
                    }
                }

                // DP[Throughput] + DP[Latency] emission.
                _ = self.bench_emit_interval.tick(), if self.emit_dp => {
                    eprintln!(
                        "DP[Throughput]: {}",
                        self.committed_tx_count as f64 / self.bench_emit_window_secs
                    );
                    if !self.latency_samples_ms.is_empty() {
                        self.latency_samples_ms.sort_unstable();
                        let mid = self.latency_samples_ms.len() / 2;
                        let median_ms = self.latency_samples_ms[mid];
                        eprintln!("DP[Latency]: {}", median_ms);
                        self.latency_samples_ms.clear();
                    }
                    // Liveness/flow telemetry: inbound queue depth, n-f ack-gate
                    // state (can_propose + acks collected for the outstanding
                    // block), outstanding refetches, and resident block bodies.
                    info!(
                        "HeraDataActor: inbound_depth={} can_propose={} acks={}/{} pending_reqs={} block_store={}",
                        self.rx_verified_blocks.len(),
                        self.can_propose,
                        self.acks_for_current,
                        self.n_minus_t.saturating_sub(1),
                        self.pending_data_requests.len(),
                        self.block_store.len(),
                    );
                    self.committed_tx_count = 0;
                }
            }
        }

        #[allow(unreachable_code)]
        {
            info!("HeraDataActor: shut down.");
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // Self-propose
    // -----------------------------------------------------------------------

    pub async fn on_self_propose(
        &mut self,
        batch: Batch<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        if batch.payload.is_empty() {
            return Ok(());
        }

        // Hard chain-lead cap: if this node's proposed tip is already
        // max_chain_lead blocks ahead of its committed head, skip this batch.
        // The node's current tip (my_height) remains attestable by the sig
        // chain, so once that head is committed committed_heights[my_id]
        // advances and the cap releases — no permanent stall.
        let committed_height = self.multi_data_chain.committed_height(self.my_id);
        let lead = self.my_height.saturating_sub(committed_height);
        if lead >= self.max_chain_lead {
            debug!(
                "HeraDataActor: chain-lead cap: my_height={} committed={} lead={} >= max={}, dropping batch",
                self.my_height, committed_height, lead, self.max_chain_lead
            );
            return Ok(());
        }

        let height = self.my_height + 1;
        let parent_hash = self
            .my_last_hash
            .clone()
            .unwrap_or_else(|| DataBlock::<Tx>::genesis().hash().clone());

        let payload = Arc::new(batch.payload);
        let envelope = DataBlockEnvelope {
            epoch: self.current_epoch,
            height,
            payload,
            parent_hash,
        };

        let env_hash = Hash::ser_and_hash(&envelope);
        let raw = self.crypto_system.secret.sign(env_hash.as_ref())?;

        let block = DataBlock {
            envelope,
            sig: DataBlockSig {
                raw,
                signer: self.my_id,
                _phantom: PhantomData,
            },
            cached_hash: OnceCell::new(),
        };

        debug!(
            "HeraDataActor: self-propose height={} author={}",
            block.envelope.height, self.my_id
        );

        self.my_height = block.envelope.height;
        self.my_height_atomic
            .store(self.my_height, AOrdering::Relaxed);
        self.my_last_hash = Some(block.hash().clone());

        // Prime batcher for next block.
        let _ = self
            .tx_consensus_to_batcher
            .send(BatcherConsensusMsg::NewRound {
                leader: self.my_id,
                round: self.my_height + 1,
            });

        // Broadcast to peers.
        let msg = HeraMsg::DataPropose {
            block: block.clone(),
            sender: self.my_id,
        };
        let bytes = Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
        let results = self.data_net.broadcast(&self.broadcast_peers, bytes).await;

        // Arm the n-f ack gate for this block: collect the ACK oneshots and
        // block further self-proposals until >= n-f nodes (self + n-f-1 peers)
        // have received it (window = 1). Reliable delivery guarantees the ACKs
        // arrive as long as <= f peers are faulty.
        self.awaiting_acks.clear();
        self.acks_for_current = 0;
        for r in results {
            if let Ok(h) = r {
                self.awaiting_acks.push(h);
            }
        }
        // Stay open only if no peer ACK is required (degenerate tiny n).
        self.can_propose = self.n_minus_t <= 1;

        let block_hash = block.hash().clone();
        let block_height = block.envelope.height;
        let block_parent = block.envelope.parent_hash.clone();
        let block_epoch = block.envelope.epoch;
        let block_author = block.sig.signer;

        // Insert body into block_store, write to shared store, then admit metadata.
        // Own blocks are pre-trusted (we just signed them).
        self.admit_block(
            block,
            block_hash,
            block_height,
            block_parent,
            block_author,
            block_epoch,
        );

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Dispatch inbound data messages
    // -----------------------------------------------------------------------

    async fn dispatch_data(
        &mut self,
        msg: HeraMsg<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        match msg {
            HeraMsg::DataPropose { block, sender } => self.on_data_propose(block, sender).await,
            HeraMsg::DataRequest {
                target_hash,
                source,
            } => self.on_data_request(target_hash, source).await,
            HeraMsg::DataResponse { block } => self.on_data_response(block).await,
            _ => {
                debug!("HeraDataActor: unexpected msg in dispatch_data");
                Ok(())
            }
        }
    }

    // -----------------------------------------------------------------------
    // DataPropose from peers (already sig-verified by the inbound pump task)
    // -----------------------------------------------------------------------

    pub async fn on_data_propose(
        &mut self,
        block: DataBlock<Tx>,
        sender: Id,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let author = block.sig.signer;
        if sender != author {
            debug!(
                "HeraDataActor: on_data_propose: sender={} != author={}, continuing",
                sender, author
            );
        }

        // The block arrived — clear any outstanding fetch for it (dedup).
        self.pending_data_requests.remove(block.hash());

        let block_hash = block.hash().clone();
        let block_height = block.envelope.height;
        let block_parent = block.envelope.parent_hash.clone();
        let block_epoch = block.envelope.epoch;

        let missing_parent = self.admit_block(
            block,
            block_hash,
            block_height,
            block_parent,
            author,
            block_epoch,
        );

        if let Some(missing_hash) = missing_parent {
            debug!(
                "HeraDataActor: data block parked; requesting parent {:?} from author {}",
                missing_hash, author
            );
            self.request_data(missing_hash, author).await;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // DataRequest / DataResponse
    // -----------------------------------------------------------------------

    pub async fn on_data_request(
        &mut self,
        target_hash: DataBlockHash<Tx>,
        source: Id,
    ) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        let genesis = DataBlock::<Tx>::genesis();
        let block = if *genesis.hash() == target_hash {
            Some(genesis)
        } else {
            self.block_store.get(&target_hash).cloned()
        };

        match block {
            Some(b) => {
                let msg = HeraMsg::<Tx>::DataResponse { block: b };
                match bincode::serialize(&msg) {
                    Ok(bytes) => {
                        if let Err(e) = self.data_net.send(source, Bytes::from(bytes)).await {
                            debug!(
                                "HeraDataActor: DataResponse to {} not sent ({e}); \
                                 requester will retry on its next grace tick",
                                source
                            );
                        }
                    }
                    Err(e) => warn!("HeraDataActor: serialize DataResponse: {e}"),
                }
            }
            None => {
                // The requester is gating on a block we were asked to serve but
                // do NOT hold — a genuine availability gap (not a transit drop).
                // Log first + rate-limited so a stuck gate is visible.
                let n = CANT_SERVE_TOTAL.fetch_add(1, AOrdering::Relaxed) + 1;
                if n == 1 || n % 256 == 0 {
                    warn!(
                        "HeraDataActor: cannot serve DataRequest from {} (hash not in block_store); \
                         cumulative={}",
                        source, n
                    );
                }
            }
        }
        Ok(())
    }

    pub async fn on_data_response(
        &mut self,
        block: DataBlock<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let h_target = block.hash().clone();
        self.pending_data_requests.remove(&h_target);

        let block_height = block.envelope.height;
        let block_parent = block.envelope.parent_hash.clone();
        let block_epoch = block.envelope.epoch;
        let author = block.sig.signer;

        // Height <= head is duplicate — skip.
        if block_height <= self.multi_data_chain.head_height(author) {
            return Ok(());
        }

        let missing_parent = self.admit_block(
            block,
            h_target,
            block_height,
            block_parent,
            author,
            block_epoch,
        );

        if let Some(missing_hash) = missing_parent {
            self.request_data(missing_hash, author).await;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Core admission helper (synchronous — hashmap insert)
    // -----------------------------------------------------------------------

    /// Insert body into `block_store`, write to shared store (wakes
    /// notify_read GateFuts), then admit metadata.  Returns
    /// `Some(missing_parent_hash)` if the block was parked (parent not yet
    /// admitted).
    fn admit_block(
        &mut self,
        block: DataBlock<Tx>,
        hash: DataBlockHash<Tx>,
        height: u64,
        parent_hash: DataBlockHash<Tx>,
        author: Id,
        epoch: u64,
    ) -> Option<DataBlockHash<Tx>>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        // 1. Body → plain hashmap (synchronous, allocation-light, no disk).
        self.block_store.insert(hash.clone(), block.clone());

        // 2. Write to the shared store — wakes any consensus actor GateFuts that are
        //    parked on notify_read(data_block_key(hash)).  The store task resolves
        //    obligations synchronously on Write before touching RocksDB, so there is no
        //    disk-I/O latency on the gating path.
        let key = data_block_key(&hash);
        let mut store = self.store.clone();
        match bincode::serialize(&block) {
            Ok(bytes) => {
                tokio::spawn(async move {
                    store.write(key, bytes).await;
                });
            }
            Err(e) => warn!("HeraDataActor: admit_block serialize: {e}"),
        }

        // 3. Metadata admission.
        match self
            .multi_data_chain
            .admit_metadata(hash, height, parent_hash, author, epoch)
        {
            AdmitTypedResult::Extended | AdmitTypedResult::Bridge => None,
            AdmitTypedResult::Parked(missing_hash) => Some(missing_hash),
            AdmitTypedResult::Duplicate => {
                debug!("HeraDataActor: duplicate block, ignoring");
                None
            }
            AdmitTypedResult::Invalid => {
                debug!("HeraDataActor: invalid block, ignoring");
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // Request a missing data block point-to-point from its author, deduped.
    // -----------------------------------------------------------------------

    pub async fn request_data(
        &mut self,
        target_hash: DataBlockHash<Tx>,
        author: Id,
    ) where
        Tx: Clone + Serialize,
    {
        // Already admitted — nothing to fetch (avoids requesting a block we hold;
        // the gate's notify_read will resolve from the buffered block directly).
        if self.block_store.contains_key(&target_hash) {
            return;
        }
        // Dedup WITHOUT inserting yet: mark pending only on a successful send, so
        // a send that fails (peer not in map) is retried on the next grace-tick
        // hint instead of being pinned forever by the dedup set.
        if self.pending_data_requests.contains(&target_hash) {
            return; // already outstanding (reliable delivery in progress)
        }
        let msg = HeraMsg::<Tx>::DataRequest {
            target_hash: target_hash.clone(),
            source: self.my_id,
        };
        match bincode::serialize(&msg) {
            Ok(bytes) => match self.data_net.send(author, Bytes::from(bytes)).await {
                Ok(_handler) => {
                    // Reliable delivery in progress — dedup until the block lands.
                    // Drop the ACK handler (we don't gate catch-up on it).
                    self.pending_data_requests.insert(target_hash);
                }
                Err(e) => {
                    debug!(
                        "HeraDataActor: DataRequest to author {} not sent ({e}); \
                         will retry on next grace tick: {:?}",
                        author, target_hash
                    );
                }
            },
            Err(e) => warn!("HeraDataActor: serialize DataRequest: {e}"),
        }
    }

    // -----------------------------------------------------------------------
    // GC: drop block bodies that are safely below each author's committed height.
    // -----------------------------------------------------------------------

    fn gc_block_store(&mut self)
    where
        Tx: Clone,
    {
        let committed_heights = &self.multi_data_chain.committed_heights;
        self.block_store.retain(|_hash, block| {
            let author = block.sig.signer;
            let committed = committed_heights.get(&author).copied().unwrap_or(0);
            // Keep if not yet committed for this author.
            block.envelope.height > committed
        });
        debug!(
            "HeraDataActor: gc_block_store: {} blocks remaining",
            self.block_store.len()
        );
    }

    // -----------------------------------------------------------------------
    // Commit emission (cross-plane: from consensus actor) — synchronous
    // -----------------------------------------------------------------------

    fn on_commit_emit(
        &mut self,
        commit: CommitEmit<Tx>,
    ) where
        Tx: Clone + Serialize + std::fmt::Debug,
    {
        // Track max heads size for test hook.
        let heads_len = commit.heads.len();
        let prev = self
            .max_committed_heads_len
            .load(std::sync::atomic::Ordering::Relaxed);
        if heads_len > prev {
            self.max_committed_heads_len
                .store(heads_len, std::sync::atomic::Ordering::Relaxed);
        }

        // Sort heads by author id for determinism.
        let mut sorted_heads = commit.heads.clone();
        sorted_heads.sort_by_key(|h| h.author);

        for head in &sorted_heads {
            let author = head.author;
            let to_height = head.height;
            let from_height = self.multi_data_chain.committed_height(author);

            if to_height <= from_height {
                continue;
            }

            // walk_range reads bodies from block_store (synchronous).
            let blocks = self.multi_data_chain.walk_range(
                &self.block_store,
                &head.hash,
                from_height,
                to_height,
            );

            for block in &blocks {
                let payload = Arc::clone(&block.envelope.payload);
                if !payload.is_empty() {
                    if self.emit_dp {
                        self.committed_tx_count += payload.len() as u64;
                        if self.latency_samples_ms.len() < 8192 {
                            let now_ns = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos())
                                .unwrap_or(0);
                            for tx in payload.iter() {
                                if let Some(send_ns) = tx.hera_timestamp_ns() {
                                    if send_ns > 0 && now_ns >= send_ns {
                                        let lat_ms = ((now_ns - send_ns) / 1_000_000) as u64;
                                        self.latency_samples_ms.push(lat_ms);
                                        if self.latency_samples_ms.len() >= 8192 {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let payload_owned: Vec<Tx> = (*payload).clone();
                    let _ = self
                        .tx_consensus_to_batcher
                        .send(BatcherConsensusMsg::Committed {
                            batch: Batch {
                                payload: payload_owned.clone(),
                            },
                            round: block.envelope.height,
                        });
                    if self
                        .tx_data_commit
                        .send(Arc::new(Batch {
                            payload: payload_owned,
                        }))
                        .is_err()
                    {
                        error!("HeraDataActor: tx_data_commit closed");
                        return;
                    }
                }
            }

            self.multi_data_chain
                .committed_heights
                .insert(author, to_height);

            // Advance the shared committed-height atomic so the load generator
            // can observe that the cap has relaxed without locking.
            if author == self.my_id {
                self.my_committed_height_atomic
                    .store(to_height, AOrdering::Relaxed);
            }
        }

        // GC block_store every GC_EVERY_N_COMMITS commit events.
        self.commit_event_count += 1;
        if self.commit_event_count % GC_EVERY_N_COMMITS == 0 {
            self.gc_block_store();
        }

        // GC pending_data_requests: entries are cleared on block arrival, but a
        // block that never arrives would linger. Coarse reset if it grows large
        // (a re-hint re-requests as needed).
        const MAX_REQ: usize = 4096;
        if self.pending_data_requests.len() > MAX_REQ {
            self.pending_data_requests.clear();
        }
    }
}
