use crate::{
    server::{ChainState, Leto},
    types::{Element, Transaction},
    Id, Round, start_id,
};
use anyhow::{anyhow, Context, Result};
use crypto::hash::Hash;
use linked_hash_map::LinkedHashMap;
use log::*;
use mempool::Batch;
use std::sync::Arc;
use storage::rocksdb::Storage;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

#[derive(Debug)]
pub enum CommitMsg<Tx> {
    EndRound {
        // This is the highest chain hash
        round_element_hash: Hash<Element<Id, Tx, Round>>,
        // This is the highest chain as of this round
        round_element: Arc<Element<Id, Tx, Round>>,
    },
}

pub struct CommitContext<Tx> {
    tx_inner: UnboundedSender<CommitMsg<Tx>>,
}

impl<Tx> CommitContext<Tx> {
    pub fn spawn(
        store: Storage,
        tx_commit: UnboundedSender<Arc<Batch<Tx>>>,
        num_nodes: usize,
        num_faults: usize,
    ) -> Self
    where
        Tx: Transaction,
    {
        let (tx_inner, rx_inner) = unbounded_channel();
        tokio::spawn(async move {
            if let Err(e) = Self::run(store, tx_commit, rx_inner, num_nodes, num_faults).await {
                error!("Commit Helper shut down: {}", e);
            }
        });
        Self { tx_inner }
    }

    /// The chain validity will be ensured while checking the proposal
    /// We are guaranteed that this chain is available on disk and that it
    /// satisfies all the properties of chain validity We just need to
    /// - go back up to last committed block,
    /// - on the way check if any block got (n+t+1)/2 UCR votes
    async fn run(
        store: Storage,
        tx_commit: UnboundedSender<Arc<Batch<Tx>>>,
        mut rx_inner: UnboundedReceiver<CommitMsg<Tx>>,
        num_nodes: usize,
        num_faults: usize,
    ) -> Result<()>
    where
        Tx: Transaction,
    {
        // Our DB Accessor
        let mut chain_state = ChainState::<Tx>::new(store);
        chain_state.genesis_setup().await?;

        // Genesis constants
        let genesis_element = Arc::new(Element::<Id, Tx, Round>::genesis(start_id()));
        let genesis_element_hash = Hash::ser_and_hash(genesis_element.as_ref());

        // Commit Length
        let commit_len = (num_nodes + num_faults + 1) / 2;

        // Tracking variables
        let mut highest_committed_element = genesis_element.clone();
        let mut highest_committed_hash = Hash::ser_and_hash(highest_committed_element.as_ref());
        let mut commit_queue = LinkedHashMap::<
            Hash<Element<Id, Tx, Round>>,
            Arc<Element<Id, Tx, Round>>,
        >::with_capacity(commit_len);
        commit_queue.insert(genesis_element_hash.clone(), genesis_element);
        let mut unique_proposers = LinkedHashMap::<Id, usize>::default();
        loop {
            tokio::select! {
                msg = rx_inner.recv() => {
                    let msg = msg.ok_or_else(||
                        anyhow!("Shutting down commit helper")
                    )?;
                    match msg {
                        CommitMsg::EndRound {
                            round_element_hash,
                            round_element,
                        } => {
                            let mut head = round_element;
                            let mut head_hash = round_element_hash;
                            let mut local_queue = LinkedHashMap::<Hash<Element<Id, Tx, Round>>, Arc<Element<Id, Tx, Round>>>::with_capacity(commit_len);
                            let mut connected_to_commit_queue = false;
                            let mut local_unique_proposers = LinkedHashMap::<Id, usize>::default();
                            while head.proposal.round() > highest_committed_element.proposal.round()
                            {
                                // We connected to the back of the commit queue
                                if let Some((hash, _)) = commit_queue.back()
                                {
                                    if hash == &head_hash {
                                        info!("Connected to commit queue");
                                        connected_to_commit_queue = true;
                                        break;
                                    }
                                }
                                // If we reached the highest committed block, skip
                                if head_hash == highest_committed_hash {
                                    info!("Connected to the highest committed block");
                                    break;
                                }
                                // Update local queue
                                *local_unique_proposers
                                    .entry(head.auth.get_id())
                                    .or_insert(0)
                                    += 1;
                                local_queue.insert(
                                    head_hash.clone(),
                                    head,
                                );
                                // Update head
                                head = match chain_state.get_element(head_hash).await {
                                    Err(e) => {
                                        error!("Error reading parent: {}", e);
                                        break;
                                    },
                                    Ok(None) => {
                                        error!("Could not find parent");
                                        break;
                                    }
                                    Ok(Some(parent)) => Arc::new(parent),
                                };
                                head_hash = head.proposal.block().parent_hash();
                            }
                            // Local queue contains a chain that connects to:
                            // (a) the highest committed block [f branch]
                            // (b) the genesis [first n rounds]
                            // (c) commit queue [others including crash only]
                            if !connected_to_commit_queue {
                                info!("Replacing commit queue");
                                let _ = std::mem::replace(
                                    &mut commit_queue,
                                    local_queue
                                );
                                let _ = std::mem::replace(
                                    &mut unique_proposers,
                                    local_unique_proposers,
                                );
                            } else {
                                info!("Extending commit queue");
                                commit_queue.extend(local_queue);
                                unique_proposers.extend(local_unique_proposers);
                            }
                            // Commit logic
                            while unique_proposers.len() > commit_len {
                                // Pop and commit
                                let (hash, element) = commit_queue.pop_back()
                                    .expect("Must be unwrappable");
                                let id = element.auth.get_id();
                                let count = unique_proposers
                                    .get_mut(&id)
                                    .expect("Must be unwrappable");
                                assert!(*count > 0usize);
                                *count -= 1;
                                if *count == 0 {
                                    unique_proposers.remove(&id);
                                }
                                // Commit element
                                tx_commit.send(
                                    Arc::new(element.batch.clone())
                                ).map_err(anyhow::Error::new)?;
                                highest_committed_hash = hash;
                                highest_committed_element = element;
                            }
                        },
                    }
                },
            }
        }
    }
}

impl<Tx> Leto<Tx> {
    pub async fn try_commit(&mut self) -> Result<()>
    where
        Tx: Transaction,
    {
        // Let the commit context know
        let commit_helper_msg = CommitMsg::EndRound {
            round_element_hash: self.chain_state.highest_hash(),
            round_element: self.chain_state.highest_chain(),
        };
        self.commit_ctx
            .tx_inner
            .send(commit_helper_msg)
            .map_err(anyhow::Error::new)
            .context("Error sending msg to commit helper")
    }
}

pub struct DummyCommitSink {}

impl DummyCommitSink {
    pub fn spawn<Tx>(mut rx_inner: UnboundedReceiver<Arc<Batch<Tx>>>)
    where
        Tx: Transaction,
    {
        tokio::spawn(async move {
            while let Some(batch) = rx_inner.recv().await {
                // Process the batch of transactions
                info!("Committed batch: {:?}", batch);
            }
        });
    }
}
