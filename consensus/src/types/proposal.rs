use super::{Block, Certificate};
use network::Message;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Proposal<Id, Tx, Round> {
    pub(super) block: Block<Id, Tx, Round>,
    pub(super) qc: Option<Vec<Certificate<Id, Round>>>,
    pub(super) round: Round,
}

impl<Id, Tx, Round> Proposal<Id, Tx, Round> {}

impl<Id, Tx, Round> Proposal<Id, Tx, Round>
where
    Round: Clone,
{
    pub fn round(&self) -> Round {
        self.round.clone()
    }

    pub fn qc(&self) -> &Option<Vec<Certificate<Id, Round>>> {
        &self.qc
    }
}

impl<Id, Tx, Round> Proposal<Id, Tx, Round> {
    pub fn new(
        block: Block<Id, Tx, Round>,
        round: Round,
        qc: Option<Vec<Certificate<Id, Round>>>,
    ) -> Self {
        Self { block, round, qc }
    }

    pub fn block(&self) -> &Block<Id, Tx, Round> {
        &self.block
    }
}

impl<Id, Tx, Round> Message for Proposal<Id, Tx, Round>
where
    Tx: super::Transaction,
    Round: Message,
    Id: network::Identifier,
{
}
