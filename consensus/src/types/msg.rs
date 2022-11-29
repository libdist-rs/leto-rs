use crypto::hash::Hash;
use super::{Proposal, Signature};
use mempool::Transaction;
use network::Identifier;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ProtocolMsg<Id, Tx, Round> {
    Propose { 
        prop: Proposal<Tx, Round>,
        auth: Signature<Id, Proposal<Tx, Round>>,
    },
    Relay {
        prop: Proposal<Tx, Round>,
        auth: Signature<Id, Proposal<Tx, Round>>,
    },
    Blame {},
}

impl<Id, Tx, Round> network::Message for ProtocolMsg<Id, Tx, Round>
where
    Id: Identifier,
    Tx: Transaction,
    Round: network::Message,
{}

/// `ClientMsg` are messages sent between the client and the servers
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ClientMsg<Tx> {
    NewTx(Tx),
    Confirmation(Hash<Tx>),
}

impl<Tx> network::Message for ClientMsg<Tx> where Tx: Transaction {}
