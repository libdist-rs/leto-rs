use std::{time::Duration, cmp::Ordering, collections::VecDeque, pin::Pin, task::{self, Poll}};
use super::Leto;
use crate::{
    server::BatcherConsensusMsg as BCM,
    types::{Certificate, Proposal, ProtocolMsg, Signature, Transaction},
    Id, Round,
};
use anyhow::Result;
use fnv::FnvHashMap;
use futures_util::Stream;
use log::*;
use mempool::Batch;
use network::{plaintcp::CancelHandler, Acknowledgement};
use tokio::time::{interval, Interval};

type PropMsg<Id, Tx, Round> = (
    Proposal<Id, Tx, Round>,
    Signature<Id, Proposal<Id, Tx, Round>>,
    Batch<Tx>,
    Id,
);

type BlameMsg<Id, Round> = (Round, Signature<Id, Round>);

type BlameQCMsg<Id, Round> = (Round, Certificate<Id, Round>);

#[derive(Debug)]
pub enum Event<Tx> {
    Timeout,
    Msg(ProtocolMsg<Id, Tx, Round>),
}

#[derive(Debug)]
pub struct RoundContext<Tx> {
    /// Track the current round
    current_round: Round,

    /// A buffer of messages that are available for the current round
    msg_buf: FnvHashMap<Round, VecDeque<ProtocolMsg<Id, Tx, Round>>>,

    /// Track the proposals that are ready for handling in the current round
    proposals_ready: FnvHashMap<Round, Vec<PropMsg<Id, Tx, Round>>>,

    /// Track the blame messages that are ready to be handled in the current
    /// round
    blame_ready: FnvHashMap<Round, Vec<BlameMsg<Id, Round>>>,

    /// Track the blame QC messages that are ready to be handled in the current
    /// round
    blame_qc_ready: FnvHashMap<Round, Vec<BlameQCMsg<Id, Round>>>,

    /// A collection of cancel handlers for messages which are undergoing
    /// transmission
    cancel_handlers: FnvHashMap<Round, Vec<CancelHandler<Acknowledgement>>>,

    /// The timeout for the current round
    timer: Interval,
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
            blame_ready: FnvHashMap::default(),
            blame_qc_ready: FnvHashMap::default(),
            cancel_handlers: FnvHashMap::default(),
            num_nodes,
            timer: interval(4 * delay),
            timer_enabled: true,
            blame_map: FnvHashMap::default(),
            got_qc: false,
            msg_buf: FnvHashMap::default(),
        }
    }

    /// Resets the timer
    pub fn init(&mut self) {
        self.timer.reset();
    }

    /// Returns the current round
    pub fn round(&self) -> Round {
        self.current_round
    }

    /// Track the following handler
    pub fn add_handler(&mut self, handler: CancelHandler<Acknowledgement>) {
        self.cancel_handlers
            .entry(self.current_round)
            .or_default()
            .push(handler);
    }

    /// Track the following send handlers
    pub fn add_handlers(&mut self, handlers: Vec<CancelHandler<Acknowledgement>>) {
        self.cancel_handlers
            .entry(self.current_round)
            .or_default()
            .extend(handlers);
    }

    /// Move forward by one round
    pub fn advance_round(&mut self) -> Result<()> {
        self.current_round += 1;

        // Reset msg buf
        self.msg_buf.clear();

        // GC too old cancel handlers
        self.cancel_handlers
            .retain(|round, _| gc_cancel_handlers(*round, self.current_round, self.num_nodes));
        
        // GC old round messages
        self.msg_buf
            .retain(|r, _| r >= &self.current_round);

        // Reset timers
        self.timer.reset();
        self.timer_enabled = true;

        // Reset QC for the current round
        self.blame_map.clear();
        self.got_qc = false;

        // Re-schedule suspended messages
        // Process the propose messages from the new current round first
        if let Some(msgs) = self.propose_msgs() {
            for (prop, auth, batch, sender) in msgs {
                let pmsg = ProtocolMsg::Propose {
                    proposal: prop,
                    auth,
                    batch,
                    sender,
                };
                self.msg_buf
                    .entry(self.current_round)
                    .or_default()
                    .push_back(pmsg);
            }
        }

        // Process the relay messages from the new current round first
        if let Some(msgs) = self.blame_qc_msgs() {
            for (round, qc) in msgs {
                let pmsg = ProtocolMsg::<Id, Tx, Round>::BlameQC { round, qc };
                self.msg_buf
                    .entry(self.current_round)
                    .or_default()
                    .push_back(pmsg);
            }
        }

        // Process the blame messages from the new current round second
        if let Some(msgs) = self.blame_msgs() {
            for (round, auth) in msgs {
                let pmsg = ProtocolMsg::<Id, Tx, Round>::Blame { round, auth };
                self.msg_buf
                    .entry(self.current_round)
                    .or_default()
                    .push_back(pmsg);
            }
        }

        Ok(())
    }

    pub fn sync(
        &mut self, 
        msg: ProtocolMsg<Id, Tx, Round>,
    ) -> Result<()> 
    {
        let msg_round = match &msg {
            ProtocolMsg::Propose { proposal, ..} => proposal.round(),
            ProtocolMsg::Blame { round, ..} => *round,
            ProtocolMsg::BlameQC { round, ..} => *round,
            ProtocolMsg::Relay {..} => unreachable!(),
            ProtocolMsg::BatchRequest {..} => unreachable!(),
            ProtocolMsg::BatchResponse {..} => unreachable!(),
            ProtocolMsg::ElementRequest {..} => unreachable!(),
            ProtocolMsg::ElementResponse {..} => unreachable!(),
        };
        match msg_round.cmp(&self.current_round) {
            Ordering::Less => {
                debug!("Got an old message for round {} in round {}", 
                    msg_round, 
                    self.current_round,
                );
                return Ok(());
            },
            Ordering::Greater => {
                match msg {
                    ProtocolMsg::Propose { 
                        proposal, 
                        auth, 
                        batch, 
                        sender 
                    } => self.queue_proposal(proposal, auth, batch, sender),
                    ProtocolMsg::Blame { 
                        round, 
                        auth 
                    } => self.queue_blame(round, auth),
                    ProtocolMsg::BlameQC { 
                        round, 
                        qc 
                    } => self.queue_blame_qc(round, qc),
                    ProtocolMsg::Relay {..} => unreachable!(),
                    ProtocolMsg::BatchRequest {..} => unreachable!(),
                    ProtocolMsg::BatchResponse {..} => unreachable!(),
                    ProtocolMsg::ElementRequest {..} => unreachable!(),
                    ProtocolMsg::ElementResponse {..} => unreachable!(),
                }
            },
            Ordering::Equal => {
                self.msg_buf
                    .entry(self.current_round)
                    .or_default()
                    .push_back(msg);
            },
        };
        Ok(())
    }

    /// All propose messages for the current round
    pub fn propose_msgs(&mut self) -> Option<Vec<PropMsg<Id, Tx, Round>>> {
        self.proposals_ready.remove(&self.current_round)
    }

    /// All blame messages for the current round
    pub fn blame_msgs(&mut self) -> Option<Vec<BlameMsg<Id, Round>>> {
        self.blame_ready.remove(&self.current_round)
    }

    /// All blame QC messages for the current round
    pub fn blame_qc_msgs(&mut self) -> Option<Vec<BlameQCMsg<Id, Round>>> {
        self.blame_qc_ready.remove(&self.current_round)
    }

    fn queue_proposal(
        &mut self,
        prop: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch: Batch<Tx>,
        sender: Id,
    ) {
        self.proposals_ready
            .entry(prop.round())
            .or_insert(Vec::new())
            .push((prop, auth, batch, sender));
    }

    fn queue_blame(
        &mut self,
        round: Round,
        auth: Signature<Id, Round>,
    ) {
        self.blame_ready
            .entry(round)
            .or_insert_with(Vec::new)
            .push((round, auth));
    }

    fn queue_blame_qc(
        &mut self,
        round: Round,
        qc: Certificate<Id, Round>,
    ) {
        self.blame_qc_ready
            .entry(round)
            .or_insert_with(Vec::new)
            .push((round, qc));
    }
}

impl<Tx> Stream for RoundContext<Tx>
where
    Tx: Transaction,
{
    type Item = Event<Tx>;

    fn poll_next(
        mut self: Pin<&mut Self>, 
        cx: &mut task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let rnd = self.as_ref().current_round;
        let buf = self.msg_buf
            .entry(rnd)
            .or_default();
        if buf.is_empty() {
            if self.timer_enabled && self.timer.poll_tick(cx).is_ready() {
                return Poll::Ready(Some(Event::Timeout));
            }
            return Poll::Pending;
        }
        let msg = buf.pop_front()
            .unwrap();
        Poll::Ready(Some(Event::Msg(msg)))
    }
}

impl<Tx> Leto<Tx>
where
    Tx: Transaction,
{
    pub async fn advance_round(&mut self) -> Result<()> {
        // Update the leaders
        self.leader_context.advance_round();

        // Update the round
        self.round_context.advance_round()?;

        // Clear the waiting_hashes for the relay messages
        self.synchronizer.advance_round(self.round_context.round())?;

        // Try committing
        self.try_commit().await?;

        // Let the batcher know that we are in a new round
        let batcher_msg = BCM::NewRound {
            leader: self.leader_context.leader(),
        };
        self.tx_consensus_to_batcher.send(batcher_msg)?;

        debug!("Advancing to round {}", self.round_context.round());
        debug!("Using new leader: {}", self.leader_context.leader());
        Ok(())
    }
}

