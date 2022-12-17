use super::Txpool;
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use log::*;
use mempool::{Batch, Transaction};
use network::Identifier;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Messages sent back and forth between the consensus and the batcher
#[derive(Debug)]
pub enum BatcherConsensusMsg<Id, Tx> {
    /// On entering a new round, notify the batcher that it may be its turn to
    /// propose
    NewRound { leader: Id },
    /// On committing a batch, clear the batch
    Commit { batch: Batch<Tx> },
    /// Clear this batch so that future proposers don't propose the same batch
    /// in their turn if not committed
    OptimisticClear { batch: Batch<Tx> },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Parameters<Id> {
    pub my_id: Id,
    pub initial_leader: Id,
    pub batch_size: usize,
    pub batch_timeout: Duration,
}

impl<Id> Parameters<Id> {
    pub fn new(
        my_id: Id,
        initial_leader: Id,
        batch_size: usize,
        batch_timeout: Duration,
    ) -> Self {
        Self {
            my_id,
            initial_leader,
            batch_size,
            batch_timeout,
        }
    }
}

/// An implementation of the Round-Robin batcher
#[derive(Debug)]
pub struct RRBatcher<Id, Tx> {
    /// The ID of this server
    my_id: Id,
    /// The ID of the current leader
    current_leader: Id,
    /// Have we proposed in this round?
    proposed: bool,
    /// A channel to receive messages from the mempool
    rx_incoming_tx: UnboundedReceiver<(Tx, usize)>,
    /// A channel to receive messages from the consensus
    rx_incoming_consensus: UnboundedReceiver<BatcherConsensusMsg<Id, Tx>>,
    /// A channel to output batches
    tx_outgoing_batch: UnboundedSender<Batch<Tx>>,
    /// The in-memory mempool
    pool: Txpool<Tx>,
}

impl<Id, Tx> RRBatcher<Id, Tx>
where
    Id: Identifier,
    Tx: Transaction,
{
    pub fn spawn(
        params: Parameters<Id>,
        rx_incoming_tx: UnboundedReceiver<(Tx, usize)>,
        rx_incoming_consensus: UnboundedReceiver<BatcherConsensusMsg<Id, Tx>>,
        tx_outgoing_batch: UnboundedSender<Batch<Tx>>,
    ) -> Result<()> {
        tokio::spawn(async move {
            let res = Self {
                my_id: params.my_id,
                current_leader: params.initial_leader,
                proposed: false,
                rx_incoming_tx,
                rx_incoming_consensus,
                tx_outgoing_batch,
                pool: Txpool::new(params.batch_size, params.batch_timeout),
            }
            .run()
            .await;
            if let Err(e) = res {
                error!("RR-Batcher terminated with {}", e);
            }
        });
        Ok(())
    }

    async fn run(&mut self) -> Result<()> {
        debug!(
            "My id: {:?}, Current leader: {:?}",
            self.my_id, self.current_leader
        );
        loop {
            tokio::select! {
                batch = &mut self.pool.next(), if self.my_id == self.current_leader &&
                    !self.proposed => {
                    // Make a batch even if we have insufficient transactions
                    debug!("Proposing a batch");
                    let batch = batch.ok_or_else(||
                        anyhow!("Failed to get a batch")
                    )?;
                    self.propose(batch)?;
                },
                tx = self.rx_incoming_tx.recv() => {
                    let (tx, tx_size) = tx.ok_or_else(||
                        anyhow!(
                            "Incoming transaction channel has closed for the batcher. Terminating."
                        )
                    )?;
                    trace!("Got a transaction: {:?}", tx);
                    self.pool.add_tx(tx, tx_size);
                },
                msg_from_consensus = self.rx_incoming_consensus.recv() => {
                    let msg_from_consensus = msg_from_consensus.ok_or_else(||
                        anyhow!(
                            "Incoming msg channel has closed for the batcher. Terminating."
                        )
                    )?;
                    match msg_from_consensus {
                        BatcherConsensusMsg::NewRound { leader } => {
                            self.current_leader = leader;
                            self.proposed = false;
                            // Check if we can propose and propose
                            self.try_propose()?;
                        },
                        BatcherConsensusMsg::Commit { batch } => {
                            // Clear in-memory mempool
                            self.pool.clear_batch(batch);
                        },
                        BatcherConsensusMsg::OptimisticClear { batch } => {
                            // Clear in-memory mempool
                            self.pool.clear_batch(batch);
                        }
                    }
                }
            }
        }
    }

    /// Will convert the in-memory mempool into a batch
    /// If insufficient transactions are present, then a smaller (possibly)
    /// empty batch is created
    ///
    /// Can throw errors if the sending fails
    fn propose(
        &mut self,
        batch: Batch<Tx>,
    ) -> Result<()> {
        self.proposed = true;
        self.tx_outgoing_batch
            .send(batch)
            .map_err(anyhow::Error::new)
    }

    /// Checks if we can propose, and proposes if we can
    /// Called the round changes so that the new leader can immediately propose
    fn try_propose(&mut self) -> Result<()> {
        if self.pool.ready() {
            let batch = self.pool.make_batch();
            self.propose(batch)
        } else {
            Ok(())
        }
    }
}
