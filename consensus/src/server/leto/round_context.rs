use super::Leto;
use crate::{
    server::BatcherConsensusMsg as BCM,
    types::{Proposal, ProtocolMsg, Signature},
    Id, Round,
};
use anyhow::Result;
use fnv::FnvHashMap;
use log::*;
use mempool::{Batch, BatchHash, Transaction};

type PropMsg<Id, Tx, Round> = (
    Proposal<Tx, Round>,
    Signature<Id, Proposal<Tx, Round>>,
    Batch<Tx>,
);

pub type RelayMsg<Id, Tx, Round> = (
    Proposal<Tx, Round>,
    Signature<Id, Proposal<Tx, Round>>,
    BatchHash<Tx>,
    Id,
);

#[derive(Debug)]
pub struct RoundContext<Tx> {
    /// Track the current round
    current_round: Round,

    /// Track the proposals that are ready for handling in the current round
    proposals_ready: FnvHashMap<Round, Vec<PropMsg<Id, Tx, Round>>>,

    /// Track the relay messages that are ready to be handled in the current
    /// round
    relay_ready: FnvHashMap<Round, Vec<RelayMsg<Id, Tx, Round>>>,
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
    let round2: Round = 2.into();
    let n: Round = num_nodes.into();

    // current round < 2n; retain
    if current_round <= round2 * n {
        return true;
    }

    // If we are in round 2n+1 and handler is from round 0 then delete
    handler_round > current_round - (round2 * n)
}

impl<Tx> Leto<Tx>
where
    Tx: Transaction,
{
    pub fn advance_round(&mut self) -> Result<()> {
        // Update the leaders
        self.leader_context.advance_round();

        // Update the round
        self.round_context.advance_round();

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

        // GC too old cancel handlers
        self.cancel_handlers.retain(|round, _| {
            gc_cancel_handlers(
                *round,
                self.round_context.round(),
                self._settings.consensus_config.num_nodes(),
            )
        });

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

    pub fn new(current_round: Round) -> Self {
        Self {
            current_round,
            proposals_ready: FnvHashMap::default(),
            relay_ready: FnvHashMap::default(),
        }
    }

    pub fn round(&self) -> Round {
        self.current_round
    }

    pub fn advance_round(&mut self) {
        self.current_round += 1.into();
    }

    /// All propose messages for the current round
    pub fn propose_msgs(&mut self) -> Option<Vec<PropMsg<Id, Tx, Round>>> {
        self.proposals_ready.remove(&self.current_round)
    }

    /// All relay messages for the current round
    pub fn relay_msgs(&mut self) -> Option<Vec<RelayMsg<Id, Tx, Round>>> {
        self.relay_ready.remove(&self.current_round)
    }

    pub fn queue_proposal(
        &mut self,
        prop: Proposal<Tx, Round>,
        auth: Signature<Id, Proposal<Tx, Round>>,
        batch: Batch<Tx>,
    ) -> () {
        self.proposals_ready
            .entry(prop.round())
            .or_insert(Vec::new())
            .push((prop, auth, batch));
    }

    pub fn queue_relay(
        &mut self,
        prop: Proposal<Tx, Round>,
        auth: Signature<Id, Proposal<Tx, Round>>,
        batch_hash: BatchHash<Tx>,
        sender: Id,
    ) -> () {
        self.relay_ready
            .entry(prop.round())
            .or_insert(Vec::new())
            .push((prop, auth, batch_hash, sender));
    }
}
