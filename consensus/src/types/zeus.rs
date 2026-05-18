/// Zeus data-plane types.
///
/// Wire-format / hashing contract (from zeus.tex §8 locked decisions):
///   - `DataBlockEnvelope` = (epoch, height, payload, parent_hash).
///     `Hash<DataBlock>` = `Hash::ser_and_hash(&block.envelope)` — never
///     includes `sig`.  Cached after first call via `OnceCell`.
///   - `AttestationEnvelope` = (epoch_s, round_s, data_block_hash,
///     data_block_height, data_block_epoch, parent_hash_s). `Hash<Attestation>`
///     = `Hash::ser_and_hash(&att.envelope)` — never includes `sig_s`.  Cached
///     after first call via `OnceCell`.
///   - Wire-format change (Change C): `AttestationEnvelope` now carries a
///     hash+height+epoch reference to the pinned data block instead of the full
///     `DataBlock` value.  `attestation_valid` looks up the block in the store;
///     absence returns `Parked(hash)` triggering the catch-up channel (zeus.tex
///     §8.6).  The paper requires this interpretation: §8.6 says `A.D_h ∉
///     dataBlockStore` is reachable, which is only possible if `A.D_h` is a
///     reference, not an embedded value.
///   - Genesis: `DataBlock` at height 0, epoch 0, all-zero parent hash.
///     `H(dataGenesis)` is a fixed sentinel computed at startup via
///     `DataBlock::genesis()`.
use crate::{Id, Round};
use crypto::hash::Hash;
use net_common::Message;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::marker::PhantomData;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// DataBlock types
// ---------------------------------------------------------------------------

/// The signable content of a data block.  `Hash<DataBlock>` is derived from
/// this envelope only (the leader signature is excluded from the hash).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct DataBlockEnvelope<Tx> {
    /// Data-plane epoch (current_epoch in Zeus Init).
    pub epoch: u64,
    /// Height in the data chain (genesis = 0).
    pub height: u64,
    /// Application payload (batch of transactions).  Arc so that cloning a
    /// DataBlock is O(1) after the initial allocation.  Bincode serializes
    /// Arc<T> as T; deserialization wraps into a fresh Arc.
    pub payload: Arc<Vec<Tx>>,
    /// Hash of the parent `DataBlock` envelope (all-zeros for genesis).
    pub parent_hash: Hash<DataBlockEnvelope<Tx>>,
}

/// A data block: an envelope plus the eleader's signature over that envelope.
///
/// `cached_hash` is skipped during serialization/deserialization; it is
/// populated lazily on the first call to `hash()` and never recomputed.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DataBlock<Tx> {
    pub envelope: DataBlockEnvelope<Tx>,
    /// Signature by `eleader(envelope.epoch)` over `envelope`.
    pub sig: DataBlockSig<Tx>,
    /// Lazily-computed canonical hash of the envelope.  Lock-free via
    /// `OnceCell`; safe to clone across threads because the value is
    /// immutable once set.
    #[serde(skip, default = "OnceCell::new")]
    pub(crate) cached_hash: OnceCell<Hash<DataBlockEnvelope<Tx>>>,
}

impl<Tx> PartialEq for DataBlock<Tx>
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

impl<Tx> Eq for DataBlock<Tx> where Tx: Serialize + Clone {}

/// Signature type alias — the signed content is the envelope, the signer id
/// is carried inline in this struct.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct DataBlockSig<Tx> {
    pub raw: Vec<u8>,
    pub signer: Id,
    pub _phantom: PhantomData<Tx>,
}

impl<Tx> DataBlock<Tx>
where
    Tx: Serialize + Clone,
{
    /// Canonical hash of a data block = hash of the envelope only.
    ///
    /// Computed once and cached.  Subsequent calls are a single atomic load.
    pub fn hash(&self) -> &Hash<DataBlockEnvelope<Tx>> {
        self.cached_hash
            .get_or_init(|| Hash::ser_and_hash(&self.envelope))
    }

    /// Construct the genesis data block (height 0, epoch 0, empty payload,
    /// all-zero parent hash, empty signature).
    pub fn genesis() -> Self {
        Self {
            envelope: DataBlockEnvelope {
                epoch: 0,
                height: 0,
                payload: Arc::new(Vec::new()),
                parent_hash: Hash::EMPTY_HASH,
            },
            sig: DataBlockSig {
                raw: Vec::new(),
                signer: 0,
                _phantom: PhantomData,
            },
            cached_hash: OnceCell::new(),
        }
    }

    /// True iff this is the genesis block (height 0, epoch 0).
    pub fn is_genesis(&self) -> bool {
        self.envelope.height == 0 && self.envelope.epoch == 0
    }
}

// ---------------------------------------------------------------------------
// Attestation types
// ---------------------------------------------------------------------------

/// The signable content of an attestation.
///
/// Change C: carries a hash+height+epoch reference to the pinned data block
/// instead of the full `DataBlock` value.  This reduces the wire size from
/// ~O(payload) to ~80 bytes per attestation.
///
/// `A = (e_s, r_s, H(D_h), D_h.h, D_h.e, h_p_s)` per zeus.tex
/// §FuncMakeAttestation. The `data_block_height` and `data_block_epoch` fields
/// are carried so that `attestation_valid` can perform the epoch gate and
/// ordering checks without a store lookup (which may return `Parked`).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct AttestationEnvelope<Tx> {
    /// Signature-chain epoch (e_s).  Steady-state = 1.
    /// TODO(zeus-view-change): incremented on sig-chain view change.
    pub epoch_s: u64,
    /// Signature-chain round (r_s).
    pub round_s: Round,
    /// Hash of the pinned data block (H(D_h)).
    pub data_block_hash: Hash<DataBlockEnvelope<Tx>>,
    /// Height of the pinned data block (D_h.h).
    /// Carried for store-lookup ordering and prefix-commit checks.
    pub data_block_height: u64,
    /// Epoch of the pinned data block (D_h.e).
    /// Carried for the epoch gate in `FuncDataBlockValid`.
    pub data_block_epoch: u64,
    /// Hash of the sig-chain head at the time of proposal (h_p^(s)).
    pub parent_hash_s: Hash<AttestationEnvelope<Tx>>,
}

/// An attestation: envelope + the sig-chain leader's signature.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Attestation<Tx> {
    pub envelope: AttestationEnvelope<Tx>,
    /// Signature by `rleader(envelope.round_s)` over `envelope`.
    pub sig_s: AttestationSig<Tx>,
    /// Lazily-computed canonical hash of the envelope.
    #[serde(skip, default = "OnceCell::new")]
    cached_hash: OnceCell<Hash<AttestationEnvelope<Tx>>>,
}

impl<Tx> PartialEq for Attestation<Tx>
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

impl<Tx> Eq for Attestation<Tx> where Tx: Serialize + Clone {}

/// Signature type alias for attestation signatures.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct AttestationSig<Tx> {
    pub raw: Vec<u8>,
    pub signer: Id,
    pub _phantom: PhantomData<Tx>,
}

impl<Tx> Attestation<Tx>
where
    Tx: Serialize + Clone,
{
    /// Canonical hash of an attestation = hash of the envelope only.
    ///
    /// Computed once and cached.
    pub fn hash(&self) -> &Hash<AttestationEnvelope<Tx>> {
        self.cached_hash
            .get_or_init(|| Hash::ser_and_hash(&self.envelope))
    }

    /// Convenience: height of the pinned data block.
    pub fn data_height(&self) -> u64 {
        self.envelope.data_block_height
    }

    /// Convenience constructor used in tests and internal construction.
    pub fn new(
        envelope: AttestationEnvelope<Tx>,
        sig_s: AttestationSig<Tx>,
    ) -> Self {
        Self {
            envelope,
            sig_s,
            cached_hash: OnceCell::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// net_common::Message impls (required for mempool::Transaction blanket)
// ---------------------------------------------------------------------------

impl<Tx> Message for Attestation<Tx>
where
    Self: serde::de::DeserializeOwned,
{
    type DeserializationError = Box<bincode::ErrorKind>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::DeserializationError> {
        bincode::deserialize(bytes)
    }
}

impl<Tx> Message for DataBlock<Tx>
where
    Self: serde::de::DeserializeOwned,
{
    type DeserializationError = Box<bincode::ErrorKind>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::DeserializationError> {
        bincode::deserialize(bytes)
    }
}

// ---------------------------------------------------------------------------
// consensus Transaction impl for Attestation<Tx>
// Attestation<Tx> satisfies the consensus Transaction trait automatically
// via the blanket impl in tx.rs (Serialize + DeserializeOwned + Debug +
// Send + Sync + Clone + 'static + Message + Unpin).
// ---------------------------------------------------------------------------

// The consensus Transaction trait adds Unpin.  Attestation<Tx> is Unpin
// by default (all fields are Unpin for any Tx: Unpin).  We add an explicit
// impl here for clarity in the non-benchmark case.
#[cfg(not(feature = "benchmark"))]
impl<Tx> crate::types::Transaction for Attestation<Tx> where Tx: crate::types::Transaction {}

#[cfg(feature = "benchmark")]
impl<Tx> crate::types::Transaction for Attestation<Tx>
where
    Tx: crate::types::Transaction,
{
    fn is_sample(&self) -> bool {
        // Attestations are not sampled transactions.
        false
    }

    fn get_id(&self) -> u64 {
        0
    }
}
