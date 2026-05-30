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
    ///
    /// Carries the blamer's highest known sig-chain element so the next leader
    /// can extend the true highest chain (it asks a blamer for the element if
    /// it does not have it — request-from-sender). `highest_hash` is the
    /// element hash; `highest_round` is that element's round (genesis =
    /// Round::MIN).
    SigBlame {
        round: Round,
        auth: Signature<Id, Round>,
        highest_round: Round,
        highest_hash: Hash<Element<Id, MultiAttestation<Tx>, Round>>,
    },

    /// Hera: BlameQC for sig-chain.
    ///
    /// Carries the maximum highest-chain reference among the quorum of blames
    /// that formed the QC, so a node that only receives the QC (did not collect
    /// the individual blames) also learns which chain the new leader must
    /// extend.
    SigBlameQC {
        round: Round,
        qc: Certificate<Id, Round>,
        highest_round: Round,
        highest_hash: Hash<Element<Id, MultiAttestation<Tx>, Round>>,
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

    /// Hera: ranged sig-element catch-up request.
    ///
    /// The requester holds elements up to (but not including) `from_round` and
    /// needs all elements up through `to_round` to close the gap.  The
    /// responder returns up to `MAX_RANGE_RESPONSE` elements in ancestor-first
    /// order (lowest round first) that it holds in the requested range.
    SigElementRangeRequest {
        source: Id,
        from_round: Round,
        to_round: Round,
    },

    /// Hera: response to a SigElementRangeRequest.
    ///
    /// Carries elements in ascending round order (lowest first) so the receiver
    /// can store them in order and satisfy parent-present checks for each.
    SigElementRangeResponse {
        elements: Vec<Element<Id, MultiAttestation<Tx>, Round>>,
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

    /// Hera: peer responds with a data block. `responder` is the node serving
    /// the block (a guaranteed holder); the requester fetches a missing parent
    /// from `responder` (request-from-sender), never the possibly-dead author.
    DataResponse { block: DataBlock<Tx>, responder: Id },
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
