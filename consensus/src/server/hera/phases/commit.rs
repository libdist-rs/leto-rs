/// Hera commit projection.
///
/// The commit rule is identical to Zeus: `(n+f+1)/2` unique proposers in the
/// sliding window. When a sig-chain element commits, the consensus actor's
/// `on_committed_attestation` forwards the committed attestation to the data
/// actor via the unbounded `tx_commit_emit` channel; the data actor walks
/// ranges, emits txs, and GCs block_store below the committed watermark.
///
/// `HeraCommitContext` wraps a second `ChainState<MultiAttestation<Tx>>`
/// that runs in a background task so the sig actor event loop is not blocked
/// by the chain-walk (which touches storage).
use crate::{
    server::hera::{SigChainState, SigElement, SigElementHash},
    types::{hera::MultiAttestation, Transaction},
    Id, START_ID,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use linked_hash_map::LinkedHashMap;
use log::*;
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
/// consensus actor.
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

        // Commit quorum: minimum unique proposers in the sliding window required
        // to commit a sig-chain element.  Safety-correct formula: (n+f+1)/2.
        #[allow(clippy::manual_div_ceil)]
        let commit_len: usize = (num_nodes + num_faults + 1) / 2;

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
            crate::server::hera::core::INFLIGHT_COMMIT_TX_INNER
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

            match msg {
                HeraCommitMsg::EndRound {
                    round_element_hash,
                    round_element,
                } => {
                    if round_element.proposal.round() == 0 {
                        continue;
                    }

                    let walk_round = round_element.proposal.round();
                    debug!(
                        "Hera commit: EndRound round={} head_hash={:?} queue_len={} \
                         unique_proposers={}",
                        walk_round,
                        &round_element_hash,
                        commit_queue.len(),
                        unique_proposers.len(),
                    );

                    let mut local_queue =
                        LinkedHashMap::<SigElementHash<Tx>, Arc<SigElement<Tx>>>::with_capacity(
                            commit_len,
                        );
                    let mut local_unique_proposers = LinkedHashMap::<Id, usize>::default();
                    let mut connected = false;

                    // FIX 1: Derive the parent hash from the in-memory element
                    // WITHOUT a storage read, eliminating the wasted first read
                    // that was present in the old loop.  Insert the current head
                    // once and immediately step to the parent.
                    //
                    // We carry `Option<Arc<SigElement<Tx>>>` so the loop can
                    // detect the "moved out / broke early" case without a borrow.
                    let mut cur: Option<Arc<SigElement<Tx>>> = Some(round_element);
                    let mut head_hash = round_element_hash;

                    loop {
                        let head = match cur.take() {
                            Some(h) => h,
                            None => break,
                        };
                        if head.proposal.round() <= highest_committed_element.proposal.round() {
                            break;
                        }
                        if let Some((h, _)) = commit_queue.back() {
                            if h == &head_hash {
                                connected = true;
                                break;
                            }
                        }
                        if head_hash == highest_committed_hash {
                            break;
                        }

                        // Record this element.
                        *local_unique_proposers
                            .entry(head.auth.get_id())
                            .or_insert(0) += 1;
                        // Extract parent hash from in-memory element — no read.
                        let parent_hash = head.proposal.block().parent_hash();
                        local_queue.insert(head_hash, head);

                        // FIX 1: Consult in-memory maps before hitting storage.
                        // The fast path (connected chain) does ~0 storage reads.
                        head_hash = parent_hash.clone();
                        if let Some(e) = local_queue.get(&parent_hash) {
                            cur = Some(Arc::clone(e));
                            continue;
                        }
                        if let Some(e) = commit_queue.get(&parent_hash) {
                            cur = Some(Arc::clone(e));
                            continue;
                        }

                        // Fallback: read from storage.
                        let fetched = sig_chain_state.get_element(parent_hash.clone()).await;
                        match fetched {
                            Ok(Some(e)) => cur = Some(Arc::new(e)),
                            Ok(None) => {
                                error!("Hera commit: missing parent element");
                                break;
                            }
                            Err(e) => {
                                error!("Hera commit: error reading parent: {}", e);
                                break;
                            }
                        }
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

                        let best_att: Option<MultiAttestation<Tx>> =
                            atts.iter().max_by_key(|a| a.max_head_height()).cloned();

                        if let Some(attestation) = best_att {
                            debug!(
                                "Hera commit: firing sig_round={} heads={} blames={}",
                                sig_round,
                                attestation.envelope.heads.len(),
                                attestation.envelope.blames.len(),
                            );
                            if tx_committed
                                .send(HeraCommittedAttestation {
                                    sig_round,
                                    attestation,
                                })
                                .is_ok()
                            {
                                crate::server::hera::core::INFLIGHT_COMMITTED
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }

                        highest_committed_hash = hash;
                        highest_committed_element = element;
                    }
                }
            }
        }
    }
}

use crate::server::hera::Hera;

impl<Tx> Hera<Tx>
where
    Tx: Transaction,
{
    /// Send the current sig-chain head to the commit task.
    pub fn try_commit(&mut self) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        let msg = HeraCommitMsg::EndRound {
            round_element_hash: self.sig_chain_state.highest_hash(),
            round_element: self.sig_chain_state.highest_chain(),
        };
        let result = self
            .hera_commit_ctx
            .tx_inner
            .send(msg)
            .map_err(anyhow::Error::new);
        if result.is_ok() {
            crate::server::hera::core::INFLIGHT_COMMIT_TX_INNER
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        result
    }
}
