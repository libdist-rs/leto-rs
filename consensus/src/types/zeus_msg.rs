/// Zeus wire-format message enum.
///
/// Sig-plane variants mirror their Leto `ProtocolMsg` counterparts (with
/// attestations instead of raw batches).  Data-plane adds `DataPropose`,
/// `DataRequest`, `DataResponse`.  `ProtocolMsg` is kept untouched.
use super::{
    Attestation, AttestationEnvelope, Certificate, DataBlock, DataBlockEnvelope, EleaderBlame,
    EleaderChangeQC, Proposal, Request, Response, Signature,
};
use crate::{Id, Round};
use crypto::hash::Hash;
use net_common::Message;
use serde::{Deserialize, Serialize};

/// Zeus protocol messages.
///
/// Sig-plane messages carry `Attestation<Tx>` payloads where Leto carried
/// `Batch<Tx>`.  The underlying sig-chain structure (proposals, blame,
/// blame-QC) is otherwise identical to canonical Leto.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ZeusMsg<Tx> {
    // -----------------------------------------------------------------------
    // Sig-plane messages  (canonical Leto, attestation substitution)
    // -----------------------------------------------------------------------
    /// Zeus: OnSignatureRoundPropose — sig-chain leader proposes an
    /// attestation. Mirrors `ProtocolMsg::Propose` with `Attestation<Tx>`
    /// as the block payload.
    SigPropose {
        proposal: Proposal<Id, Attestation<Tx>, Round>,
        auth: Signature<Id, Proposal<Id, Attestation<Tx>, Round>>,
        attestation: Attestation<Tx>,
        sender: Id,
    },

    /// Zeus: relay of a SigPropose to the next leader.
    SigRelay {
        proposal: Proposal<Id, Attestation<Tx>, Round>,
        auth: Signature<Id, Proposal<Id, Attestation<Tx>, Round>>,
        att_hash: Hash<AttestationEnvelope<Tx>>,
        sender: Id,
    },

    /// Zeus: OnSignatureBlame — blame for the current sig-chain round.
    SigBlame {
        round: Round,
        auth: Signature<Id, Round>,
    },

    /// Zeus: BlameQC for sig-chain.
    SigBlameQC {
        round: Round,
        qc: Certificate<Id, Round>,
    },

    // -----------------------------------------------------------------------
    // Sig-plane sync (mirrors Leto BatchRequest/BatchResponse/ElementRequest/ElementResponse)
    // -----------------------------------------------------------------------
    AttestationRequest {
        source: Id,
        request: Request<Attestation<Tx>>,
    },
    AttestationResponse {
        response: Response<Attestation<Tx>>,
    },
    SigElementRequest {
        source: Id,
        request: Request<super::Element<Id, Attestation<Tx>, Round>>,
    },
    SigElementResponse {
        response: Response<super::Element<Id, Attestation<Tx>, Round>>,
    },

    // -----------------------------------------------------------------------
    // Data-plane messages
    // -----------------------------------------------------------------------
    /// Zeus: OnEleaderPropose — eleader broadcasts a new data block.
    DataPropose {
        block: DataBlock<Tx>,
        sender: Id,
    },

    /// Zeus: OnDataRequest — request a data block by hash from peers.
    DataRequest {
        target_hash: Hash<DataBlockEnvelope<Tx>>,
        source: Id,
    },

    /// Zeus: OnDataResponse — peer responds with a data block.
    DataResponse {
        block: DataBlock<Tx>,
    },

    // -----------------------------------------------------------------------
    // Data-plane eleader-change messages
    // -----------------------------------------------------------------------
    /// Zeus: OnEleaderBlame — a node blames the current eleader for silence
    /// or equivocation.
    EleaderBlame(EleaderBlame<Tx>),

    /// Zeus: OnEleaderChangeQC — a node multicasts a formed eleader-change QC
    /// (t+1 distinct-signer blames).
    EleaderChangeQC(EleaderChangeQC<Tx>),
}

impl<Tx> Message for ZeusMsg<Tx>
where
    Self: serde::de::DeserializeOwned,
{
    type DeserializationError = Box<bincode::ErrorKind>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::DeserializationError> {
        bincode::deserialize(bytes)
    }
}
