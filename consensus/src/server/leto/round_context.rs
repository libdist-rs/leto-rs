use std::{
    pin::Pin,
    task::{Context, Poll},
};

use fnv::FnvHashMap;
use futures_util::Stream;

use crate::{types::Proposal, Id, Round};

#[derive(Debug)]
pub struct RoundContext<Tx> {
    current_round: Round,
    current_leader: Id,
    proposals_ready: FnvHashMap<Round, Vec<Proposal<Tx, Round>>>,
}

impl<Tx> RoundContext<Tx> {
    pub fn new(
        current_round: Round,
        current_leader: Id,
    ) -> Self {
        Self {
            current_round,
            current_leader,
            proposals_ready: FnvHashMap::default(),
        }
    }

    pub fn round(&self) -> Round {
        self.current_round
    }

    pub fn leader(&self) -> Id {
        self.current_leader
    }

    // Implement update and leader history handling
    pub fn update(&mut self) {
        self.current_round += 1.into();
    }

    // Whether we have any messages for the current round received earlier
    pub fn msgs_ready(&self) -> bool {
        self.proposals_ready.contains_key(&self.current_round)
    }

    pub fn queue_proposal(
        &mut self,
        prop: Proposal<Tx, Round>,
    ) -> () {
        self.proposals_ready
            .entry(prop.round())
            .or_insert(Vec::new())
            .push(prop);
    }
}

impl<Tx> Stream for RoundContext<Tx> {
    type Item = Proposal<Tx, Round>;

    fn poll_next(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        todo!()
    }
}
