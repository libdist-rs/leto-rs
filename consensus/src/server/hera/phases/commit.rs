/// Hera commit projection.
///
/// Parallel to `zeus/phases/commit.rs::ZeusCommitContext` but carries
/// `MultiAttestation<Tx>` instead of `Attestation<Tx>` as the committed
/// payload.
///
/// The commit rule is identical to Zeus: `(n+f+1)/2` unique proposers in the
/// sliding window.  When a sig-chain element commits, the main task's
/// `on_committed_attestation` receives the `MultiAttestation` and walks each
/// author's sub-chain from the committed watermark to the attested head height,
/// emitting txs in author-id-ascending order.
///
/// `HeraCommitContext` wraps the same `ChainState<MultiAttestation<Tx>>` that
/// the main `Hera<Tx>` task uses.  The commit background task is separate so
/// the main event loop is not blocked by the chain-walk (which touches
/// storage).
use crate::{
    server::hera::{SigChainState, SigElement, SigElementHash},
    types::{hera::MultiAttestation, Transaction},
    Id, START_ID,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use linked_hash_map::LinkedHashMap;
use log::*;
use mempool::Batch;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// Messages sent to the Hera commit task.
pub enum HeraCommitMsg<Tx> {
    EndRound {
        round_element_hash: SigElementHash<Tx>,
        round_element: Arc<SigElement<Tx>>,
    },
}

/// A committed multi-attestation notification sent from the commit task to the
/// main Hera task.
pub struct HeraCommittedAttestation<Tx> {
    /// The sig-chain round at which this element committed.
    pub sig_round: u64,
    /// The best (highest-max-head-height) MultiAttestation from the committed
    /// sig-chain element.
    pub attestation: MultiAttestation<Tx>,
}

/// Hera commit context: same sliding-window commit rule as Zeus.
pub struct HeraCommitContext<Tx> {
    pub(crate) tx_inner: UnboundedSender<HeraCommitMsg<Tx>>,
    /// Receive committed attestations back from the commit task.
    pub(crate) rx_committed: UnboundedReceiver<HeraCommittedAttestation<Tx>>,
}

impl<Tx> HeraCommitContext<Tx>
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
        let (tx_committed, rx_committed) = unbounded_channel::<HeraCommittedAttestation<Tx>>();

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
                error!("Hera commit context shut down: {}", e);
            }
        });

        Self {
            tx_inner,
            rx_committed,
        }
    }

    async fn run(
        mut sig_chain_state: SigChainState<Tx>,
        tx_committed: UnboundedSender<HeraCommittedAttestation<Tx>>,
        mut rx_inner: UnboundedReceiver<HeraCommitMsg<Tx>>,
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
                .ok_or_else(|| anyhow!("Hera commit context: inner channel closed"))?;

            match msg {
                HeraCommitMsg::EndRound {
                    round_element_hash,
                    round_element,
                } => {
                    if round_element.proposal.round() == 0 {
                        continue;
                    }

                    debug!(
                        "Hera commit: EndRound round={} head_hash={:?} queue_len={} \
                         unique_proposers={}",
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
                                error!("Hera commit: missing parent element");
                                break;
                            }
                            Err(e) => {
                                error!("Hera commit: error reading parent: {}", e);
                                break;
                            }
                        };
                        head_hash = head.proposal.block().parent_hash();
                    }

                    if !connected {
                        debug!(
                            "Hera commit: FORK detected, replacing queue (was {}, now {})",
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

                    // Commit elements.
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
                        let atts: &[MultiAttestation<Tx>] = &element.batch.payload;

                        // Pick the attestation with the highest max-head-height.
                        let best_att: Option<MultiAttestation<Tx>> =
                            atts.iter().max_by_key(|a| a.max_head_height()).cloned();

                        if let Some(attestation) = best_att {
                            debug!(
                                "Hera commit: firing sig_round={} heads={} blames={}",
                                sig_round,
                                attestation.envelope.heads.len(),
                                attestation.envelope.blames.len(),
                            );
                            let _ = tx_committed.send(HeraCommittedAttestation {
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

impl<Tx> Hera<Tx>
where
    Tx: Transaction,
{
    /// Hera: try_commit — sends the current sig-chain head to the commit task.
    pub fn try_commit(&mut self) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        let msg = HeraCommitMsg::EndRound {
            round_element_hash: self.sig_chain_state.highest_hash(),
            round_element: self.sig_chain_state.highest_chain(),
        };
        self.hera_commit_ctx
            .tx_inner
            .send(msg)
            .map_err(anyhow::Error::new)
    }

    /// Hera: `on_committed_attestation` — called from the main select loop
    /// when `HeraCommittedAttestation` arrives.
    ///
    /// For each `DataHead` in `att.envelope.heads`, walk that author's
    /// sub-chain from `committed_heights[author] + 1` to `head.height` and
    /// emit all blocks' txs via `tx_data_commit`.  Authors are iterated in
    /// ascending id order for determinism.
    ///
    /// Updates `multi_data_chain.committed_heights[author]` to `head.height`
    /// after emission.
    pub(crate) fn on_committed_attestation(
        &mut self,
        committed: HeraCommittedAttestation<Tx>,
    ) where
        Tx: Clone + Serialize,
    {
        let att = committed.attestation;

        // Debug assertion: heads + blames must cover exactly n authors with no
        // duplicates.
        #[cfg(debug_assertions)]
        {
            let n = self.settings.committee_config.num_nodes();
            let total = att.envelope.heads.len() + att.envelope.blames.len();
            debug_assert_eq!(
                total,
                n,
                "Hera invariant: heads.len() + blames.len() must equal n; \
                 got heads={} blames={} n={}",
                att.envelope.heads.len(),
                att.envelope.blames.len(),
                n
            );
            // No author should appear in both.
            let head_authors: std::collections::HashSet<Id> =
                att.envelope.heads.iter().map(|h| h.author).collect();
            for blamed in &att.envelope.blames {
                debug_assert!(
                    !head_authors.contains(blamed),
                    "Hera invariant: author {} appears in both heads and blames",
                    blamed
                );
            }
        }

        // Track max heads size for test hook.
        let heads_len = att.envelope.heads.len();
        let prev = self
            .max_committed_heads_len
            .load(std::sync::atomic::Ordering::Relaxed);
        if heads_len > prev {
            self.max_committed_heads_len
                .store(heads_len, std::sync::atomic::Ordering::Relaxed);
        }

        // Sort heads by author id for determinism.
        let mut sorted_heads = att.envelope.heads.clone();
        sorted_heads.sort_by_key(|h| h.author);

        for head in &sorted_heads {
            let author = head.author;
            let to_height = head.height;
            let from_height = self.multi_data_chain.committed_height(author);

            if to_height <= from_height {
                continue;
            }

            // Walk the sub-chain from from_height+1 to to_height.
            let blocks = self
                .multi_data_chain
                .walk_range(&head.hash, from_height, to_height);

            // Debug: verify parent-hash chain continuity.
            #[cfg(debug_assertions)]
            {
                for i in 1..blocks.len() {
                    debug_assert_eq!(
                        blocks[i].envelope.parent_hash,
                        *blocks[i - 1].hash(),
                        "Hera commit: chain broken at author={} height={}",
                        author,
                        blocks[i].envelope.height
                    );
                }
            }

            for block in &blocks {
                let payload = std::sync::Arc::clone(&block.envelope.payload);
                if !payload.is_empty() {
                    if self.emit_dp {
                        self.committed_tx_count += payload.len() as u64;
                    }
                    let payload_owned: Vec<Tx> = (*payload).clone();
                    // Notify batcher of committed round.
                    let _ = self.tx_consensus_to_batcher.send(
                        crate::server::BatcherConsensusMsg::Committed {
                            batch: Batch {
                                payload: payload_owned.clone(),
                            },
                            round: block.envelope.height,
                        },
                    );
                    if self
                        .tx_data_commit
                        .send(Arc::new(Batch {
                            payload: payload_owned,
                        }))
                        .is_err()
                    {
                        log::error!("Hera: tx_data_commit closed");
                        return;
                    }
                }
            }

            // Update committed watermark.
            self.multi_data_chain
                .committed_heights
                .insert(author, to_height);
        }
    }
}

use crate::server::hera::Hera;
