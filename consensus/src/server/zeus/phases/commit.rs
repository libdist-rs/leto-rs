/// Zeus commit projection.
///
/// When the sig-chain commit rule fires ((n+f+1)/2 unique proposers in the
/// sliding window), the Zeus commit context identifies the committed
/// attestation (highest data height from the committed sig-chain element) and
/// sends it back to the main Zeus task via `ZeusCommittedAttestation`.
///
/// The main task (`on_committed_attestation`) performs the prefix walk over the
/// data-block store (Def 8.7: A.D_h.h ≥ h commits all data blocks 1..H) and
/// emits payloads in ascending height order via `tx_data_commit`.  This keeps
/// the data-block store access on the single-threaded main task, avoiding any
/// shared-state complexity.
///
/// Change C: `Attestation` no longer embeds a full `DataBlock`; it carries
/// `data_block_hash` + `data_block_height`.  The commit context uses
/// `data_block_height` for ordering.  The prefix walk in
/// `on_committed_attestation` looks up the pinned block by `data_block_hash`
/// from `data_block_store`.
use crate::{
    server::{
        zeus::{
            chain_state::{CommitItem, DataBlockHash},
            SigChainState, SigElement, SigElementHash, Zeus,
        },
        BatcherConsensusMsg, ChainDB,
    },
    types::{DataBlock, DataBlockEnvelope, Transaction},
    Id, START_ID,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use linked_hash_map::LinkedHashMap;
use log::*;
use mempool::Batch;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// Messages sent to the Zeus commit task.
pub enum ZeusCommitMsg<Tx> {
    EndRound {
        round_element_hash: SigElementHash<Tx>,
        round_element: Arc<SigElement<Tx>>,
    },
}

/// Committed attestation notification sent from the commit task back to the
/// main Zeus task so that `commit_lock` and prefix emission stay up to date.
pub struct ZeusCommittedAttestation<Tx> {
    /// The sig-chain round at which this attestation was committed.
    pub sig_round: u64,
    /// The highest-data-height attestation from the committed sig-chain
    /// element.
    pub attestation: crate::types::Attestation<Tx>,
}

/// Zeus commit context: spawns a background task that tracks the Leto
/// lock-of-lock commit rule on the sig-chain and notifies the main task of
/// newly committed attestations.
pub struct ZeusCommitContext<Tx> {
    pub(crate) tx_inner: UnboundedSender<ZeusCommitMsg<Tx>>,
    /// Receive committed attestations back from the commit task.
    pub(crate) rx_committed: tokio::sync::mpsc::UnboundedReceiver<ZeusCommittedAttestation<Tx>>,
}

impl<Tx> ZeusCommitContext<Tx>
where
    Tx: Transaction,
{
    pub fn spawn(
        sig_chain_state: SigChainState<Tx>,
        num_nodes: usize,
        num_faults: usize,
    ) -> Self
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug + 'static,
    {
        let (tx_inner, rx_inner) = unbounded_channel();
        let (tx_committed, rx_committed) =
            tokio::sync::mpsc::unbounded_channel::<ZeusCommittedAttestation<Tx>>();
        tokio::spawn(async move {
            if let Err(e) = Self::run(
                sig_chain_state,
                tx_committed,
                rx_inner,
                num_nodes,
                num_faults,
            )
            .await
            {
                error!("Zeus commit context shut down: {}", e);
            }
        });
        Self {
            tx_inner,
            rx_committed,
        }
    }

    async fn run(
        mut sig_chain_state: SigChainState<Tx>,
        tx_committed: tokio::sync::mpsc::UnboundedSender<ZeusCommittedAttestation<Tx>>,
        mut rx_inner: UnboundedReceiver<ZeusCommitMsg<Tx>>,
        num_nodes: usize,
        num_faults: usize,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug + 'static,
    {
        sig_chain_state.genesis_setup().await?;

        let genesis_element = Arc::new(SigElement::<Tx>::genesis(START_ID));
        let genesis_hash = Hash::ser_and_hash(genesis_element.as_ref());

        #[allow(clippy::manual_div_ceil)]
        let commit_len = (num_nodes + num_faults + 1) / 2;

        let mut highest_committed_element = genesis_element.clone();
        let mut highest_committed_hash = genesis_hash.clone();

        let mut unique_proposers = LinkedHashMap::<Id, usize>::default();
        unique_proposers.insert(genesis_element.auth.get_id(), 1);

        let mut commit_queue =
            LinkedHashMap::<SigElementHash<Tx>, Arc<SigElement<Tx>>>::with_capacity(commit_len);
        commit_queue.insert(genesis_hash, genesis_element);

        loop {
            let msg = rx_inner
                .recv()
                .await
                .ok_or_else(|| anyhow!("Zeus commit context: inner channel closed"))?;

            match msg {
                ZeusCommitMsg::EndRound {
                    round_element_hash,
                    round_element,
                } => {
                    if round_element.proposal.round() == 0 {
                        continue;
                    }

                    debug!(
                        "Zeus commit: EndRound round={} head_hash={:?} queue_len={} unique_proposers={}",
                        round_element.proposal.round(),
                        &round_element_hash,
                        commit_queue.len(),
                        unique_proposers.len(),
                    );

                    let mut head = round_element;
                    let mut head_hash = round_element_hash;
                    let mut local_queue =
                        LinkedHashMap::<SigElementHash<Tx>, Arc<SigElement<Tx>>>::with_capacity(
                            commit_len,
                        );
                    let mut local_unique_proposers = LinkedHashMap::<Id, usize>::default();
                    let mut connected = false;

                    while head.proposal.round() > highest_committed_element.proposal.round() {
                        if let Some((h, _)) = commit_queue.back() {
                            if h == &head_hash {
                                connected = true;
                                break;
                            }
                        }
                        if head_hash == highest_committed_hash {
                            break;
                        }
                        *local_unique_proposers
                            .entry(head.auth.get_id())
                            .or_insert(0) += 1;
                        local_queue.insert(head_hash.clone(), head);

                        head = match sig_chain_state.get_element(head_hash).await {
                            Ok(Some(e)) => Arc::new(e),
                            Ok(None) => {
                                error!("Zeus commit: missing parent element");
                                break;
                            }
                            Err(e) => {
                                error!("Zeus commit: error reading parent: {}", e);
                                break;
                            }
                        };
                        head_hash = head.proposal.block().parent_hash();
                    }

                    if !connected {
                        debug!(
                            "Zeus commit: FORK detected, replacing queue (was {}, now {})",
                            commit_queue.len(),
                            local_queue.len(),
                        );
                        let _ = std::mem::replace(&mut commit_queue, local_queue);
                        let _ = std::mem::replace(&mut unique_proposers, local_unique_proposers);
                    } else {
                        commit_queue.extend(local_queue);
                        for (id, n) in local_unique_proposers {
                            *unique_proposers.entry(id).or_insert(0) += n;
                        }
                    }

                    // Commit elements: fire ZeusCommittedAttestation for each committed
                    // sig-chain element.  The main task does the data-prefix walk and
                    // payload emission (zeus.tex Def 8.7).
                    while unique_proposers.len() >= commit_len {
                        let (hash, element) = commit_queue
                            .pop_front()
                            .expect("commit_queue non-empty by invariant");
                        let id = element.auth.get_id();
                        let count = unique_proposers.get_mut(&id).expect("must exist");
                        *count -= 1;
                        if *count == 0 {
                            unique_proposers.remove(&id);
                        }

                        let sig_round = element.proposal.round();
                        let att = &element.batch.payload;

                        // Pick the attestation with the highest pinned data height.
                        // Change C: use data_block_height instead of data_block.envelope.height.
                        let best_att: Option<crate::types::Attestation<Tx>> = att
                            .iter()
                            .max_by_key(|a| a.envelope.data_block_height)
                            .cloned();

                        if let Some(attestation) = best_att {
                            debug!(
                                "Zeus commit: firing sig_round={} pinned data_height={}",
                                sig_round, attestation.envelope.data_block_height
                            );
                            // Ignore send errors: main task may have exited
                            let _ = tx_committed.send(ZeusCommittedAttestation {
                                sig_round,
                                attestation,
                            });
                        }

                        highest_committed_hash = hash;
                        highest_committed_element = element;
                    }
                }
            }
        }
    }
}

impl<Tx> Zeus<Tx>
where
    Tx: Transaction,
{
    /// Zeus: OnSigCommit — called when the sig-chain advance_round fires.
    /// Sends the current sig-chain head to the commit context.
    pub fn try_commit(&mut self) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        let msg = ZeusCommitMsg::EndRound {
            round_element_hash: self.sig_chain_state.highest_hash(),
            round_element: self.sig_chain_state.highest_chain(),
        };
        self.zeus_commit_ctx
            .tx_inner
            .send(msg)
            .map_err(anyhow::Error::new)
    }
}

impl<Tx> Zeus<Tx>
where
    Tx: Transaction,
{
    /// Zeus: prefix commit projection (zeus.tex Def 8.7).
    ///
    /// Called from the main select loop when `ZeusCommittedAttestation`
    /// arrives. Looks up the pinned block by `data_block_hash` in the
    /// data-block store, then walks backward from the pinned height H down
    /// to `zeus_committed_high + 1`, collecting blocks, and emits payloads
    /// via `tx_data_commit` in **ascending** height order.
    ///
    /// Change C: the attestation carries `data_block_hash` +
    /// `data_block_height` instead of an embedded block.  We look up the
    /// block before walking.
    ///
    /// If a block in the walk is missing from the store (catch-up race), the
    /// contiguous tail that is present is emitted and a warning is logged.
    /// The gap will be picked up on the next commit cycle.
    pub(crate) fn on_committed_attestation(
        &mut self,
        committed: ZeusCommittedAttestation<Tx>,
    ) {
        let new_height = committed.attestation.envelope.data_block_height;

        // Update commit_lock watermark (monotonic) — needed by conflict checks.
        self.commit_lock
            .retain(|(_, a)| a.envelope.data_block_height >= new_height);
        let already_covered = self
            .commit_lock
            .iter()
            .any(|(_, a)| a.envelope.data_block_height >= new_height);
        if !already_covered {
            debug!(
                "Zeus: commit_lock updated: sig_round={} data_height={}",
                committed.sig_round, new_height
            );
            self.commit_lock
                .push((committed.sig_round, committed.attestation));
        }

        // Nothing new to commit (genesis, or already at/above this height).
        if new_height <= self.zeus_committed_high {
            return;
        }

        // Forward-walk `child_of` from the last-committed cursor up to
        // `new_height`, collecting the ordered block hashes. This is metadata
        // only — child links + heights, all resident — so the consensus loop
        // never touches a payload or the disk here. The unique-child invariant
        // (equivocation is rejected at admit) guarantees a single chain, so we
        // never commit two blocks for one height. If a child link or its
        // metadata is missing (block not admitted yet), we stop and defer the
        // remainder to a later commit cycle.
        let mut to_commit: Vec<CommitItem<Tx>> = Vec::new();
        let mut cur = self.last_committed_hash.clone();
        let mut last_hash: Option<DataBlockHash<Tx>> = None;
        let mut last_height = self.zeus_committed_high;
        loop {
            let child = match self.data_chain.child_of.get(&cur) {
                Some(c) => c.clone(),
                None => break,
            };
            let child_height = match self.data_block_db.meta(&child) {
                Some(m) => m.height,
                None => break,
            };
            // Resident → hand the block over by value (steady state, no disk);
            // evicted → hand the hash and let the committer read it off-loop.
            let item = match self.data_block_db.cache_get(&child) {
                Some(b) => CommitItem::Resident(b),
                None => CommitItem::Spilled(child.clone()),
            };
            to_commit.push(item);
            last_height = child_height;
            last_hash = Some(child.clone());
            cur = child;
            if child_height >= new_height {
                break;
            }
        }

        let last = match last_hash {
            Some(h) => h,
            None => return, // nothing newly committable yet — defer
        };

        // Advance the watermark + cursor to the last collected block.
        self.zeus_committed_high = last_height;
        self.last_committed_hash = last;

        // Hand the ordered items to the background committer (non-blocking):
        // resident blocks emit straight from memory; spilled ones are read back
        // off the consensus loop.
        if self.commit_tx.send(to_commit).is_err() {
            error!("Zeus: committer channel closed");
        }
    }

    /// Spawn the background committer. The consensus loop hands it ordered
    /// data-block hashes (already past the committed watermark); it reads each
    /// payload from the data store off the consensus loop and emits it to the
    /// app sink (`tx_data_commit`) and the batcher (replay/nonce tracking),
    /// incrementing the shared committed-tx counter for DP[Throughput].
    pub(crate) fn spawn_committer(
        mut rx: UnboundedReceiver<Vec<CommitItem<Tx>>>,
        mut reader: ChainDB,
        tx_data_commit: UnboundedSender<Arc<Batch<Tx>>>,
        tx_consensus_to_batcher: UnboundedSender<BatcherConsensusMsg<Id, Tx>>,
        committed_tx_count: Arc<AtomicU64>,
        emitted_high: Arc<AtomicU64>,
        emit_dp: bool,
    ) {
        tokio::spawn(async move {
            while let Some(items) = rx.recv().await {
                for item in items {
                    let block: DataBlock<Tx> = match item {
                        CommitItem::Resident(b) => b,
                        CommitItem::Spilled(h) => {
                            // Spilled to disk during a stall. notify_read returns
                            // immediately if flushed, else waits for the write.
                            match reader
                                .notify_read_as::<DataBlockEnvelope<Tx>, DataBlock<Tx>>(&h)
                                .await
                            {
                                Ok(b) => b,
                                Err(e) => {
                                    warn!("Zeus committer: spilled read failed: {}", e);
                                    continue;
                                }
                            }
                        }
                    };
                    let height = block.envelope.height;
                    let payload = block.envelope.payload;
                    if !payload.is_empty() {
                        info!("Zeus-committed height {} with {} txs", height, payload.len());
                        if emit_dp {
                            committed_tx_count
                                .fetch_add(payload.len() as u64, Ordering::Relaxed);
                        }
                        let payload_owned: Vec<Tx> = (*payload).clone();
                        let _ = tx_consensus_to_batcher.send(BatcherConsensusMsg::Committed {
                            batch: Batch {
                                payload: payload_owned.clone(),
                            },
                            round: height,
                        });
                        if tx_data_commit
                            .send(Arc::new(Batch {
                                payload: payload_owned,
                            }))
                            .is_err()
                        {
                            error!("Zeus committer: tx_data_commit closed");
                            return;
                        }
                    } else {
                        debug!("Zeus: committed empty data block at height {}", height);
                    }
                    // Advance the emitted watermark so the store may now drop
                    // this block (and everything below it) from RAM on eviction.
                    emitted_high.fetch_max(height, Ordering::Relaxed);
                }
            }
        });
    }
}
