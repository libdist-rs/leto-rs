use super::Proposal;
use mempool::Transaction;
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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ClientMsg<Tx> {
    NewTx(Tx),
}

impl<Tx> network::Message for ClientMsg<Tx> 
where 
    Tx: Transaction,
{}