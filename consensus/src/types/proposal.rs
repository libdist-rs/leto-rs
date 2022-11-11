use super::{Block, Signature};
use network::{Identifier, Message};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Proposal<Id, Data, Round> {
    block: Block<Data>,
    round: Round,
    sig: Signature<Id, Block<Data>>,
}

impl<Id, Data, Round> Message for Proposal<Id, Data, Round>
where
    Id: Identifier,
    Data: Message,
    Round: Message,
{
}
