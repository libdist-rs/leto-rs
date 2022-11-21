use super::{Proposal, Signature};
use crypto::hash::Hash;
use network::Identifier;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ProtocolMsg<Id, Transaction, Round> {
    Propose {
        proposal: Proposal<Transaction, Round>,
        auth: Signature<Id, Proposal<Transaction, Round>>,
    },
    Relay {
        proposal: Proposal<Transaction, Round>,
        auth: Signature<Id, Proposal<Transaction, Round>>,
    },
    Blame {
        round: Round,
        auth: Signature<Id, Round>,
    },
}

impl<Id, Transaction, Round> network::Message for ProtocolMsg<Id, Transaction, Round>
where
    Id: Identifier,
    Transaction: super::Transaction,
    Round: network::Message,
{
}

/// `ClientMsg` are messages sent between the client and the servers
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ClientMsg<Tx> {
    NewTx(Tx),
    Confirmation(Hash<Tx>),
}

impl<Tx> network::Message for ClientMsg<Tx> where Tx: super::Transaction {}
