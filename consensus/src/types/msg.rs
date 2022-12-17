use super::{Certificate, Proposal, Request, Response, Signature};
use crypto::hash::Hash;
use mempool::{Batch, BatchHash};
use network::Identifier;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ProtocolMsg<Id, Tx, Round> {
    Propose {
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch: Batch<Tx>,
    },
    Relay {
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch_hash: BatchHash<Tx>,
        sender: Id,
    },
    Blame {
        round: Round,
        auth: Signature<Id, Round>,
    },
    BlameQC {
        round: Round,
        qc: Certificate<Id, Round>,
    },
    BatchRequest {
        source: Id,
        request: Request<Batch<Tx>>,
    },
    BatchResponse {
        response: Response<Batch<Tx>>,
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
