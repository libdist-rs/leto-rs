use crate::{Id, Round};

#[derive(Debug)]
pub struct RoundContext {
    current_round: Round,
    current_leader: Id,
}

impl RoundContext {
    pub fn new(
        current_round: Round,
        current_leader: Id,
    ) -> Self {
        Self {
            current_round,
            current_leader,
        }
    }

    pub fn round(&self) -> Round {
        self.current_round
    }

    pub fn leader(&self) -> Id {
        self.current_leader
    }

    // TODO: Implement update and leader history handling
    pub fn update(&mut self) {
        // self.current_round + 1.into();
    }
}
