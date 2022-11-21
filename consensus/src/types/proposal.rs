use super::Block;
use network::Message;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Proposal<Transaction, Round> {
    block: Block<Transaction>,
    round: Round,
}

impl<Transaction, Round> Proposal<Transaction, Round> {
    pub fn new(
        block: Block<Transaction>,
        round: Round,
    ) -> Self {
        Self { block, round }
    }
}

impl<Transaction, Round> Message for Proposal<Transaction, Round>
where
    Transaction: super::Transaction,
    Round: Message,
{
}
