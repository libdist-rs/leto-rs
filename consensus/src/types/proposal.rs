use super::Block;
use network::Message;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Proposal<Tx, Round> {
    block: Block<Tx>,
    round: Round,
}

impl<Tx, Round> Proposal<Tx, Round> {}

impl<Tx, Round> Proposal<Tx, Round>
where
    Round: Clone,
{
    pub fn round(&self) -> Round {
        self.round.clone()
    }
}

impl<Tx, Round> Proposal<Tx, Round> {
    pub fn new(
        block: Block<Tx>,
        round: Round,
    ) -> Self {
        Self { block, round }
    }

    pub fn block(&self) -> &Block<Tx> {
        &self.block
    }
}

impl<Tx, Round> Message for Proposal<Tx, Round>
where
    Tx: super::Transaction,
    Round: Message,
{
}
