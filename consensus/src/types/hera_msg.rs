use super::hera::{MultiAttestation, MultiAttestationEnvelope};
/// Hera wire-format message enum.
///
/// Mirrors `ZeusMsg<Tx>` (zeus_msg.rs) variant-for-variant with two changes:
///   (a) Sig-plane variants (`SigPropose`, `SigRelay`, `AttestationRequest`,
///       `AttestationResponse`, `SigElementRequest`, `SigElementResponse`)
///       carry `MultiAttestation<Tx>` instead of `Attestation<Tx>`.
///   (b) `EleaderBlame` / `EleaderChangeQC` variants are dropped entirely —
///       Hera has no single eleader, so there is nothing to blame.
///
/// Data-plane variants (`DataPropose`, `DataRequest`, `DataResponse`) are
/// structurally identical to Zeus's and carry `DataBlock<Tx>`.
use super::{
    Certificate, DataBlock, DataBlockEnvelope, Element, Proposal, Request, Response, Signature,
};
use crate::{Id, Round};
use crypto::hash::Hash;
use net_common::Message;
use serde::{Deserialize, Serialize};

/// Hera protocol messages.
///
/// Sig-plane messages carry `MultiAttestation<Tx>` payloads.  Data-plane
/// messages carry `DataBlock<Tx>` payloads, identical to Zeus.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum HeraMsg<Tx> {
    // -----------------------------------------------------------------------
    // Sig-plane messages
    // -----------------------------------------------------------------------
    /// Hera: sig-chain leader proposes a MultiAttestation.
    SigPropose {
        proposal: Proposal<Id, MultiAttestation<Tx>, Round>,
        auth: Signature<Id, Proposal<Id, MultiAttestation<Tx>, Round>>,
        attestation: MultiAttestation<Tx>,
        sender: Id,
    },

    /// Hera: relay of a SigPropose to the next leader.
    SigRelay {
        proposal: Proposal<Id, MultiAttestation<Tx>, Round>,
        auth: Signature<Id, Proposal<Id, MultiAttestation<Tx>, Round>>,
        att_hash: Hash<MultiAttestationEnvelope<Tx>>,
        sender: Id,
    },

    /// Hera: OnSignatureBlame — blame for the current sig-chain round.
    SigBlame {
        round: Round,
        auth: Signature<Id, Round>,
    },

    /// Hera: BlameQC for sig-chain.
    SigBlameQC {
        round: Round,
        qc: Certificate<Id, Round>,
    },

    // -----------------------------------------------------------------------
    // Sig-plane sync
    // -----------------------------------------------------------------------
    AttestationRequest {
        source: Id,
        request: Request<MultiAttestation<Tx>>,
    },
    AttestationResponse {
        response: Response<MultiAttestation<Tx>>,
    },
    SigElementRequest {
        source: Id,
        request: Request<Element<Id, MultiAttestation<Tx>, Round>>,
    },
    SigElementResponse {
        response: Response<Element<Id, MultiAttestation<Tx>, Round>>,
    },

    // -----------------------------------------------------------------------
    // Data-plane messages
    // -----------------------------------------------------------------------
    /// Hera: a node broadcasts its own data block (every node is a proposer).
    DataPropose { block: DataBlock<Tx>, sender: Id },

    /// Hera: request a data block by hash from peers.
    DataRequest {
        target_hash: Hash<DataBlockEnvelope<Tx>>,
        source: Id,
    },

    /// Hera: peer responds with a data block.
    DataResponse { block: DataBlock<Tx> },
}

impl<Tx> Message for HeraMsg<Tx>
where
    Self: serde::de::DeserializeOwned,
{
    type DeserializationError = Box<bincode::ErrorKind>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::DeserializationError> {
        bincode::deserialize(bytes)
    }
}
