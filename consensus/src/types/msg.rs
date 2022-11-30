use super::{Proposal, Signature};
use crypto::hash::Hash;
use network::Identifier;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ProtocolMsg<Id, Tx, Round> {
    Propose {
        proposal: Proposal<Tx, Round>,
        auth: Signature<Id, Proposal<Tx, Round>>,
    },
    Relay {
        proposal: Proposal<Tx, Round>,
        auth: Signature<Id, Proposal<Tx, Round>>,
    },
    Blame {
        round: Round,
        auth: Signature<Id, Round>,
    },
}

impl<Id, Tx, Round> network::Message for ProtocolMsg<Id, Tx, Round>
where
    Id: Identifier,
    Tx: super::Transaction,
    Round: network::Message + mempool::Round,
{
}

/// `ClientMsg` are messages sent between the client and the servers
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ClientMsg<Tx> {
    NewTx(Tx),
    Confirmation(Hash<Tx>),
}

impl<Tx> network::Message for ClientMsg<Tx> where Tx: super::Transaction {}
