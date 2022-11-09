use super::Proposal;
use network::Identifier;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ProtocolMsg<Id, Data, Round> {
    Propose { prop: Proposal<Id, Data, Round> },
    Relay {},
    Blame {},
}

impl<Id, Data, Round> network::Message for ProtocolMsg<Id, Data, Round>
where
    Id: Identifier,
    Data: network::Message,
    Round: network::Message,
{
}
