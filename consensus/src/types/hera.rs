/// Hera multi-plane types.
///
/// Hera is a multi-plane Zeus variant where every node is the stable leader
/// of its own data sub-chain. The sig-chain (rotating leader) carries a
/// `MultiAttestation<Tx>` payload that references all data sub-chain heads
/// the proposer has seen, with explicit blames for authors the proposer has
/// no fresh head from.
///
/// Wire-format contract:
///   - `MultiAttestationEnvelope` = (epoch_s, round_s, parent_hash_s, heads,
///     blames). `Hash<MultiAttestation>` = `Hash::ser_and_hash(&att.envelope)`.
///     Cached after first call via `OnceCell`.
///   - `DataBlock<Tx>` / `DataBlockEnvelope<Tx>` / `DataBlockSig<Tx>` /
///     `AttestationSig<Tx>` are reused AS-IS from zeus.rs.
///   - `DataHead<Tx>` carries the per-author sub-chain reference: author id,
///     hash of the tip `DataBlockEnvelope`, height, and epoch.
///   - Invariant: for every committed sig-block S_r, `heads.len() +
///     blames.len() == n` and no author appears in both. Enforced by
///     `debug_assert!` in the commit path.
use crate::Id;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};

// Hera uses Zeus's data-block types unchanged. They are available via the
// parent types module (which re-exports zeus::*). We import them here for
// use within this file.
use super::zeus::{AttestationSig, DataBlockEnvelope};

use crate::Round;
use crypto::hash::Hash;

// ---------------------------------------------------------------------------
// DataHead
// ---------------------------------------------------------------------------

/// A reference to one author's data sub-chain tip as seen by the proposer.
///
/// Included in `MultiAttestationEnvelope::heads` for each author the proposer
/// has fresh data from since the previous committed sig-block.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct DataHead<Tx> {
    /// Author (node id) of this sub-chain.
    pub author: Id,
    /// Hash of the tip `DataBlockEnvelope` (H(D_h) for this author).
    pub hash: Hash<DataBlockEnvelope<Tx>>,
    /// Height of the tip block.
    pub height: u64,
    /// Data-plane epoch of the tip block.
    pub epoch: u64,
}

// ---------------------------------------------------------------------------
// MultiAttestationEnvelope
// ---------------------------------------------------------------------------

/// The signable content of a multi-attestation.
///
/// Carried as the payload of sig-chain blocks in Hera.  Every node's sub-chain
/// tip that the proposer has seen is listed in `heads`; authors the proposer
/// has not received fresh data from are listed in `blames`.
///
/// Invariant: `heads.len() + blames.len() == n` (full committee coverage).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct MultiAttestationEnvelope<Tx> {
    /// Signature-chain epoch (steady-state = 1).
    pub epoch_s: u64,
    /// Signature-chain round.
    pub round_s: Round,
    /// Hash of the sig-chain head at proposal time (h_p^(s)).
    pub parent_hash_s: Hash<MultiAttestationEnvelope<Tx>>,
    /// Per-author sub-chain heads the proposer has fresh data from.
    pub heads: Vec<DataHead<Tx>>,
    /// Authors the proposer has no fresh data from since last committed
    /// sig-block that referenced them.
    pub blames: Vec<Id>,
}

// ---------------------------------------------------------------------------
// MultiAttestation
// ---------------------------------------------------------------------------

/// A multi-attestation: envelope + the sig-chain leader's signature.
///
/// Mirrors `Attestation<Tx>` (zeus.rs:204-232) with `MultiAttestationEnvelope`
/// as the signed content.  The `cached_hash` field is skipped during
/// serialization and populated lazily on the first call to `hash()`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MultiAttestation<Tx> {
    pub envelope: MultiAttestationEnvelope<Tx>,
    /// Signature by `rleader(envelope.round_s)` over `envelope`.
    /// Reuses Zeus's `AttestationSig<Tx>` — same wire format.
    pub sig_s: AttestationSig<Tx>,
    /// Lazily-computed canonical hash of the envelope.
    #[serde(skip, default = "OnceCell::new")]
    cached_hash: OnceCell<Hash<MultiAttestationEnvelope<Tx>>>,
}

impl<Tx> PartialEq for MultiAttestation<Tx>
where
    Tx: Serialize + Clone,
{
    fn eq(
        &self,
        other: &Self,
    ) -> bool {
        self.hash() == other.hash()
    }
}

impl<Tx> Eq for MultiAttestation<Tx> where Tx: Serialize + Clone {}

impl<Tx> MultiAttestation<Tx>
where
    Tx: Serialize + Clone,
{
    /// Canonical hash of a multi-attestation = hash of the envelope only.
    /// Computed once and cached.  Subsequent calls are a single atomic load.
    pub fn hash(&self) -> &Hash<MultiAttestationEnvelope<Tx>> {
        self.cached_hash
            .get_or_init(|| Hash::ser_and_hash(&self.envelope))
    }

    /// Convenience constructor.
    pub fn new(
        envelope: MultiAttestationEnvelope<Tx>,
        sig_s: AttestationSig<Tx>,
    ) -> Self {
        Self {
            envelope,
            sig_s,
            cached_hash: OnceCell::new(),
        }
    }

    /// Highest-height data head in this attestation, if any.
    pub fn max_head_height(&self) -> u64 {
        self.envelope
            .heads
            .iter()
            .map(|h| h.height)
            .max()
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// net_common::Message impl
// ---------------------------------------------------------------------------

impl<Tx> net_common::Message for MultiAttestation<Tx>
where
    Self: serde::de::DeserializeOwned,
{
    type DeserializationError = Box<bincode::ErrorKind>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::DeserializationError> {
        bincode::deserialize(bytes)
    }
}

// ---------------------------------------------------------------------------
// consensus Transaction impl for MultiAttestation<Tx>
//
// MultiAttestations are not user transactions.  Stub values satisfy trait
// bounds for the sig-plane infrastructure (ChainState, ZeusCommitContext, etc.)
// that requires the payload type to implement Transaction.
// ---------------------------------------------------------------------------

impl<Tx> crate::types::Transaction for MultiAttestation<Tx>
where
    Tx: crate::types::Transaction,
{
    fn client_id(&self) -> crate::Id {
        0
    }

    fn nonce(&self) -> u64 {
        0
    }

    fn is_sample(&self) -> bool {
        false
    }

    fn get_id(&self) -> u64 {
        0
    }
}
