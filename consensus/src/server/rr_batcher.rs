use super::Txpool;
use crate::{types::Transaction, Round};
use anyhow::{anyhow, Result};
use log::*;
use mempool::Batch;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Messages sent from the consensus engine to the batcher.
#[derive(Debug)]
pub enum BatcherConsensusMsg<Id, Tx> {
    /// Entering a new round: the batcher may now propose if it is the leader.
    NewRound { leader: Id, round: Round },
    /// A proposal carrying `batch` was admitted at `round`.
    /// Idempotent if (client,nonce) pairs are already InFlight.
    Proposed { batch: Batch<Tx>, round: Round },
    /// The batch committed at `round` on the canonical chain.
    Committed { batch: Batch<Tx>, round: Round },
    /// The chain switched away from `rounds`; orphan those InFlight entries.
    Rollback { rounds: Vec<Round> },
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
    /// The current consensus round (needed to tag make_batch calls)
    current_round: Round,
    /// Have we proposed in this round?
    proposed: bool,
    /// A channel to receive transactions from the client listener
    rx_incoming_tx: UnboundedReceiver<(Tx, usize)>,
    /// A channel to receive messages from the consensus engine
    rx_incoming_consensus: UnboundedReceiver<BatcherConsensusMsg<Id, Tx>>,
    /// A channel to output sealed batches to the proposer
    tx_outgoing_batch: UnboundedSender<Batch<Tx>>,
    /// The in-memory mempool
    pool: Txpool<Tx>,
}

impl<Id, Tx> RRBatcher<Id, Tx>
where
    Id: std::fmt::Debug + Clone + Eq + std::hash::Hash + Send + Sync + 'static,
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
                current_round: 0,
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
            // Poll the timer / ready flag without holding a borrow across await.
            let can_propose = self.my_id == self.current_leader && !self.proposed;

            tokio::select! {
                // Timer or size threshold: propose when we are the leader and
                // haven't proposed yet this round.
                _ = self.pool.tick_timer(), if can_propose => {
                    debug!("Proposing a batch (timer/size)");
                    let batch = self.pool.make_batch(self.current_round);
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
                    // If ready to propose, fire immediately.
                    if can_propose && self.pool.ready() {
                        debug!("Proposing a batch (ready)");
                        let batch = self.pool.make_batch(self.current_round);
                        self.propose(batch)?;
                    }
                },
                msg_from_consensus = self.rx_incoming_consensus.recv() => {
                    let msg = msg_from_consensus.ok_or_else(||
                        anyhow!(
                            "Incoming msg channel has closed for the batcher. Terminating."
                        )
                    )?;
                    match msg {
                        BatcherConsensusMsg::NewRound { leader, round } => {
                            self.current_leader = leader;
                            self.current_round = round;
                            self.proposed = false;
                            self.pool.reset_timer();
                            // If we are the new leader and already have enough,
                            // propose immediately.
                            self.try_propose()?;
                        },
                        BatcherConsensusMsg::Proposed { batch, round } => {
                            self.pool.admit_proposal(&batch, round);
                        },
                        BatcherConsensusMsg::Committed { batch, round } => {
                            self.pool.commit(&batch, round);
                        },
                        BatcherConsensusMsg::Rollback { rounds } => {
                            self.pool.rollback(&rounds);
                        },
                    }
                }
            }
        }
    }

    /// Seals and sends a batch to the proposer task.
    fn propose(
        &mut self,
        batch: Batch<Tx>,
    ) -> Result<()> {
        self.proposed = true;
        self.tx_outgoing_batch
            .send(batch)
            .map_err(anyhow::Error::new)
    }

    /// Proposes immediately if there are enough buffered transactions.
    fn try_propose(&mut self) -> Result<()> {
        if self.my_id == self.current_leader && !self.proposed && self.pool.ready() {
            let batch = self.pool.make_batch(self.current_round);
            self.propose(batch)
        } else {
            Ok(())
        }
    }
}
