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

/// `ClientMsg` are messages sent between the client and the servers.
///
/// # Batch hashing convention
///
/// `BatchConfirmation` is keyed by `Hash<Vec<Tx>>` computed as
/// `Hash::ser_and_hash(&vec_of_txs)` — bincode-serialise the `Vec<Tx>` and
/// SHA-256 the result.  Both the client (at send time) and the server (at
/// commit time) must use exactly this expression so the keys match.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ClientMsg<Tx> {
    /// Client → server: submit a new transaction (legacy single-tx path).
    ///
    /// `reply_to` is the `SocketAddr` of the client's confirmation listener
    /// (a `TcpReceiver<ClientMsg<Tx>>` bound to a known port).  The server
    /// records `H(tx) → reply_to` and sends `Confirmation(H(tx))` there after
    /// commit.
    NewTx {
        tx: Tx,
        reply_to: std::net::SocketAddr,
    },
    /// Server → client: the transaction with this hash was committed
    /// (legacy single-tx path).
    Confirmation(Hash<Tx>),

    /// Client → server: submit a batch of transactions.
    ///
    /// `reply_to` is the `SocketAddr` of the client's confirmation listener.
    /// The server computes `batch_hash = Hash::ser_and_hash(&batch)`, records
    /// `batch_hash → reply_to`, and sends `BatchConfirmation(batch_hash)` on
    /// commit.
    NewBatch {
        batch: Vec<Tx>,
        reply_to: std::net::SocketAddr,
    },
    /// Server → client: the batch with this hash was committed.
    ///
    /// Hash is `Hash::ser_and_hash(&vec_of_txs)` over the original `Vec<Tx>`.
    BatchConfirmation(Hash<Vec<Tx>>),
}

/// `ZeusClientMsg` are messages between clients and Zeus servers.
///
/// Parallel to `ClientMsg` but carries the Zeus-specific `WhoIsEleader` /
/// `EleaderIs` discovery round-trip.  Zeus and Leto share the stressor but
/// use distinct client-message types, mirroring how `ZeusMsg` / `ProtocolMsg`
/// diverge on the server side.
///
/// # Batch hashing convention
///
/// Same as `ClientMsg`: `Hash::ser_and_hash(&vec_of_txs)`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ZeusClientMsg<Tx> {
    /// Client → any server: "who is the current eleader?"
    WhoIsEleader,
    /// Server → client: "the current eleader for epoch `epoch` is `id`".
    EleaderIs { id: Id, epoch: u64 },
    /// Client → eleader: submit a new transaction (legacy single-tx path).
    ///
    /// `reply_to` carries the client's confirmation listener address (see
    /// `ClientMsg::NewTx`).
    NewTx {
        tx: Tx,
        reply_to: std::net::SocketAddr,
    },
    /// Eleader → client: confirmation that a transaction was committed
    /// (legacy single-tx path).
    Confirmation(Hash<Tx>),

    /// Client → eleader: submit a batch of transactions.
    ///
    /// `reply_to` is the `SocketAddr` of the client's confirmation listener.
    NewBatch {
        batch: Vec<Tx>,
        reply_to: std::net::SocketAddr,
    },
    /// Eleader → client: the batch with this hash was committed.
    ///
    /// Hash is `Hash::ser_and_hash(&vec_of_txs)` over the original `Vec<Tx>`.
    BatchConfirmation(Hash<Vec<Tx>>),
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
