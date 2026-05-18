use super::{Certificate, Element, Proposal, Request, Response, Signature};
use crate::Id;
use crypto::hash::Hash;
use mempool::{Batch, BatchHash};
use net_common::Message;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ProtocolMsg<Id, Tx, Round> {
    Propose {
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch: Batch<Tx>,
        sender: Id,
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
    // Request-response messages
    BatchRequest {
        source: Id,
        request: Request<Batch<Tx>>,
    },
    BatchResponse {
        response: Response<Batch<Tx>>,
    },
    ElementRequest {
        source: Id,
        request: Request<Element<Id, Tx, Round>>,
    },
    ElementResponse {
        response: Response<Element<Id, Tx, Round>>,
    },
}

/// `ClientMsg` are messages sent between the client and the servers
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ClientMsg<Tx> {
    NewTx(Tx),
    Confirmation(Hash<Tx>),
}

/// `ZeusClientMsg` are messages between clients and Zeus servers.
///
/// Parallel to `ClientMsg` but carries the Zeus-specific `WhoIsEleader` /
/// `EleaderIs` discovery round-trip.  Zeus and Leto share the stressor but
/// use distinct client-message types, mirroring how `ZeusMsg` / `ProtocolMsg`
/// diverge on the server side.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ZeusClientMsg<Tx> {
    /// Client → any server: "who is the current eleader?"
    WhoIsEleader,
    /// Server → client: "the current eleader for epoch `epoch` is `id`".
    EleaderIs { id: Id, epoch: u64 },
    /// Client → eleader: submit a new transaction.
    NewTx(Tx),
    /// Eleader → client: confirmation that a transaction was committed.
    Confirmation(Hash<Tx>),
}

impl<Id, Tx, Round> Message for ProtocolMsg<Id, Tx, Round>
where
    Self: serde::de::DeserializeOwned,
{
    type DeserializationError = Box<bincode::ErrorKind>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::DeserializationError> {
        bincode::deserialize(bytes)
    }
}

impl<Tx> Message for ClientMsg<Tx>
where
    Self: serde::de::DeserializeOwned,
{
    type DeserializationError = Box<bincode::ErrorKind>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::DeserializationError> {
        bincode::deserialize(bytes)
    }
}

impl<Tx> Message for ZeusClientMsg<Tx>
where
    Self: serde::de::DeserializeOwned,
{
    type DeserializationError = Box<bincode::ErrorKind>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::DeserializationError> {
        bincode::deserialize(bytes)
    }
}
