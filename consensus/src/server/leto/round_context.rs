use std::time::Duration;

use super::Leto;
use crate::{
    server::BatcherConsensusMsg as BCM,
    types::{Certificate, Proposal, ProtocolMsg, Signature, Transaction},
    Id, Round,
};
use anyhow::Result;
use fnv::FnvHashMap;
use log::*;
use mempool::{Batch, BatchHash};
use network::{plaintcp::CancelHandler, Acknowledgement};
use tokio::time::{interval, Interval};

type PropMsg<Id, Tx, Round> = (
    Proposal<Id, Tx, Round>,
    Signature<Id, Proposal<Id, Tx, Round>>,
    Batch<Tx>,
);

pub type RelayMsg<Id, Tx, Round> = (
    Proposal<Id, Tx, Round>,
    Signature<Id, Proposal<Id, Tx, Round>>,
    BatchHash<Tx>,
    Id,
);

pub type BlameMsg<Id, Round> = (Round, Signature<Id, Round>);

pub type BlameQCMsg<Id, Round> = (Round, Certificate<Id, Round>);

#[derive(Debug)]
pub struct RoundContext<Tx> {
    /// Track the current round
    current_round: Round,

    /// Track the proposals that are ready for handling in the current round
    proposals_ready: FnvHashMap<Round, Vec<PropMsg<Id, Tx, Round>>>,

    /// Track the relay messages that are ready to be handled in the current
    /// round
    relay_ready: FnvHashMap<Round, Vec<RelayMsg<Id, Tx, Round>>>,

    /// Track the blame messages that are ready to be handled in the current
    /// round
    blame_ready: FnvHashMap<Round, Vec<BlameMsg<Id, Round>>>,

    /// Track the blame QC messages that are ready to be handled in the current
    /// round
    blame_qc_ready: FnvHashMap<Round, Vec<BlameQCMsg<Id, Round>>>,

    /// A collection of cancel handlers for messages which are undergoing
    /// transmission
    pub(crate) cancel_handlers: FnvHashMap<Round, Vec<CancelHandler<Acknowledgement>>>,

    /// The timeout for the current round
    pub(crate) timer: Interval,
    /// This value tells if we already timed out for the current round
    pub(crate) timer_enabled: bool,
    /// The QC for the current round
    pub(crate) blame_map: FnvHashMap<Id, Signature<Id, Round>>,
    /// This is used to indicate that we already extracted QC from blame_map
    pub(crate) got_qc: bool,

    // Cache
    num_nodes: usize,
}

/// Determine whether or not to retain the cancel handler for some message that
/// we are transmitting
///
/// If true, we will try some more
/// If false, we will stop the retransmission of the message to the servers and
/// move on
fn gc_cancel_handlers(
    handler_round: Round,
    current_round: Round,
    num_nodes: usize,
) -> bool {
    // current round < 2n; retain
    if current_round <= 2 * num_nodes as u64 {
        return true;
    }

    // If we are in round 2n+1 and handler is from round 0 then delete
    handler_round > current_round - (2 * num_nodes as u64)
}

impl<Tx> Leto<Tx>
where
    Tx: Transaction,
{
    pub async fn advance_round(&mut self) -> Result<()> {
        // Update the leaders
        self.leader_context.advance_round();

        // Clear the waiting_hashes for the relay messages
        self.synchronizer.advance_round();

        // Update the round
        self.round_context.advance_round();

        // Try committing
        self.try_commit().await?;

        // Let the batcher know that we are in a new round
        let batcher_msg = BCM::NewRound {
            leader: self.leader_context.leader(),
        };
        self.tx_consensus_to_batcher.send(batcher_msg)?;

        // Process the propose messages from the new current round first
        if let Some(msgs) = self.round_context.propose_msgs() {
            for (prop, auth, batch) in msgs {
                let pmsg = ProtocolMsg::Propose {
                    proposal: prop,
                    auth,
                    batch,
                };
                self.tx_msg_loopback.send(pmsg)?;
            }
        }

        // Process the relay messages from the new current round second
        if let Some(msgs) = self.round_context.relay_msgs() {
            for (prop, auth, batch_hash, sender) in msgs {
                let pmsg = ProtocolMsg::Relay {
                    proposal: prop,
                    auth,
                    batch_hash,
                    sender,
                };
                self.tx_msg_loopback.send(pmsg)?;
            }
        }

        // Process the relay messages from the new current round first
        if let Some(msgs) = self.round_context.blame_qc_msgs() {
            for (round, qc) in msgs {
                let pmsg = ProtocolMsg::<Id, Tx, Round>::BlameQC { round, qc };
                self.tx_msg_loopback.send(pmsg)?;
            }
        }

        // Process the blame messages from the new current round second
        if let Some(msgs) = self.round_context.blame_msgs() {
            for (round, auth) in msgs {
                let pmsg = ProtocolMsg::<Id, Tx, Round>::Blame { round, auth };
                self.tx_msg_loopback.send(pmsg)?;
            }
        }

        debug!("Advancing to round {}", self.round_context.round());
        debug!("Using new leader: {}", self.leader_context.leader());
        Ok(())
    }
}

impl<Tx> RoundContext<Tx>
where
    Tx: Transaction,
{
    /*
     * Leader generation procedure:
     * For every round, L = random(elligible)
     * elligible.remove(L)
     * oldest.push_front(L)
     * elligible.add(oldest.pop_back())
     */

    pub fn new(
        num_nodes: usize,
        delay: Duration,
    ) -> Self {
        Self {
            current_round: Round::MIN + 1,
            proposals_ready: FnvHashMap::default(),
            relay_ready: FnvHashMap::default(),
            blame_ready: FnvHashMap::default(),
            blame_qc_ready: FnvHashMap::default(),
            cancel_handlers: FnvHashMap::default(),
            num_nodes,
            timer: interval(4 * delay),
            timer_enabled: true,
            blame_map: FnvHashMap::default(),
            got_qc: false,
        }
    }

    pub fn round(&self) -> Round {
        self.current_round
    }

    pub fn advance_round(&mut self) {
        self.current_round += 1;

        // GC too old cancel handlers
        self.cancel_handlers
            .retain(|round, _| gc_cancel_handlers(*round, self.current_round, self.num_nodes));

        // Reset timers
        self.timer.reset();
        self.timer_enabled = true;

        // Reset QC for the current round
        self.blame_map.clear();
        self.got_qc = false;
    }

    /// All propose messages for the current round
    pub fn propose_msgs(&mut self) -> Option<Vec<PropMsg<Id, Tx, Round>>> {
        self.proposals_ready.remove(&self.current_round)
    }

    /// All relay messages for the current round
    pub fn relay_msgs(&mut self) -> Option<Vec<RelayMsg<Id, Tx, Round>>> {
        self.relay_ready.remove(&self.current_round)
    }

    /// All blame messages for the current round
    pub fn blame_msgs(&mut self) -> Option<Vec<BlameMsg<Id, Round>>> {
        self.blame_ready.remove(&self.current_round)
    }

    /// All blame QC messages for the current round
    pub fn blame_qc_msgs(&mut self) -> Option<Vec<BlameQCMsg<Id, Round>>> {
        self.blame_qc_ready.remove(&self.current_round)
    }

    pub fn queue_proposal(
        &mut self,
        prop: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch: Batch<Tx>,
    ) {
        self.proposals_ready
            .entry(prop.round())
            .or_insert(Vec::new())
            .push((prop, auth, batch));
    }

    pub fn queue_relay(
        &mut self,
        prop: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch_hash: BatchHash<Tx>,
        sender: Id,
    ) {
        self.relay_ready
            .entry(prop.round())
            .or_insert(Vec::new())
            .push((prop, auth, batch_hash, sender));
    }

    pub fn queue_blame(
        &mut self,
        round: Round,
        auth: Signature<Id, Round>,
    ) -> Result<()> {
        self.blame_ready
            .entry(round)
            .or_insert_with(Vec::new)
            .push((round, auth));
        Ok(())
    }

    pub fn queue_blame_qc(
        &mut self,
        round: Round,
        qc: Certificate<Id, Round>,
    ) -> Result<()> {
        self.blame_qc_ready
            .entry(round)
            .or_insert_with(Vec::new)
            .push((round, qc));
        Ok(())
    }
}
