/// Zeus data-chain state.
///
/// Implements:
///   - `DataChainState<Tx>` — tracks `head_hash`, `head_height`, `child_of`,
///     and `pending_data_blocks` for eager-invariant admission.
///   - `DataBlockStore<Tx>` — FnvHashMap keyed by
///     `Hash<DataBlockEnvelope<Tx>>`.
///   - `PendingAttestations<Tx>` — park map per zeus.tex §FuncAttestationValid.
///   - `data_block_valid(D, e_cur)` — validates a single block (no chain walk).
///   - `data_chain_valid(hash)` — O(1) store-membership lookup.
///   - `conflicts_data_prefix(D, commit_lock, store)` — checks committed prefix
///     safety.
///   - `latest_committed_attestation(commit_lock)` — highest-height pinned A.
use crate::types::{
    Attestation, BlamePayload, BlameReason, DataBlock, DataBlockEnvelope, EleaderBlame,
    EleaderChangeQC, ZeusMsg, TAG_ELEADER_BLAME, TAG_REASON_EQUIVOCATION, TAG_REASON_SILENCE,
};
// EleaderBlameSigned is referenced only as a phantom type for Hash in
// blame_signed_bytes. The actual Hash type used internally is Hash<BlameSigned>
// (local struct).
use super::DataBlockDB;
use crypto::hash::Hash;
use fnv::FnvHashMap;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

pub type DataBlockHash<Tx> = Hash<DataBlockEnvelope<Tx>>;

/// Lightweight chain-head state (design decision #8: no Vec<DataBlock>).
///
/// Invariant: every block in the companion `data_block_store` has been
/// confirmed to have a valid eleader signature, its `parent_hash` links to
/// another in-store block (or to genesis), and it is the unique child of its
/// parent (no equivocation admitted).  Chain-membership is therefore
/// structural: `data_chain_valid(hash)` reduces to `contains_key(hash)`.
#[derive(Debug, Clone)]
pub struct DataChainState<Tx> {
    pub head_hash: DataBlockHash<Tx>,
    pub head_height: u64,
    /// parent_hash → child_hash for every admitted block.
    ///
    /// Populated on every successful insertion into `data_block_store`.
    /// Used for:
    ///   - O(1) equivocation check (two distinct children of same parent).
    ///   - Head-advance drain in case (b) admission (bridge path).
    pub child_of: FnvHashMap<DataBlockHash<Tx>, DataBlockHash<Tx>>,
    /// Blocks parked because their `parent_hash` is not yet in the store.
    ///
    /// Keyed by the missing `parent_hash`.  Mirrored after
    /// `pendingAttestations`. Drained on every successful admission.
    pub pending_data_blocks: FnvHashMap<DataBlockHash<Tx>, Vec<DataBlock<Tx>>>,
}


/// Park map: missing-data-hash → parked (attestation-containing) ZeusMsg
/// entries. On `OnDataResponse` drain, each entry is re-dispatched to the
/// appropriate handler.
pub type PendingAttestations<Tx> = FnvHashMap<DataBlockHash<Tx>, Vec<ZeusMsg<Tx>>>;

/// Admitted eleader-change QC map (epoch → actual QC).
///
/// Keyed by epoch `e`.  A `Some(QC)` entry means the epoch-e eleader has been
/// formally replaced; `epoch e+1` blocks are therefore admitted.
/// The genesis pre-admission (epoch 0 → epoch 1) stores a sentinel with an
/// empty blames vec — see `Zeus::spawn`.
pub type AdmittedChangeQCs<Tx> = FnvHashMap<u64, EleaderChangeQC<Tx>>;

impl<Tx> DataChainState<Tx>
where
    Tx: Serialize + Clone,
{
    pub fn genesis() -> Self {
        let genesis_block = DataBlock::<Tx>::genesis();
        Self {
            head_hash: genesis_block.hash().clone(),
            head_height: 0,
            child_of: FnvHashMap::default(),
            pending_data_blocks: FnvHashMap::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// data_block_valid (zeus.tex §FuncDataBlockValid)
// ---------------------------------------------------------------------------

/// Validates a single data block `D` against `e_cur`.
///
/// Does NOT check parent linkage — that is `data_chain_valid`'s job.
///
/// Steady-state callers pass `self.current_epoch`.
/// Catch-up callers (`on_data_response`) pass `D.envelope.epoch`.
///
/// Cryptographic verification of the eleader signature is intentionally
/// left to the caller (via `verify_data_block_sig`) because the crypto
/// system is not stored here. This function only handles the epoch-gating
/// logic.
pub fn epoch_gate_valid<Tx>(
    block: &DataBlock<Tx>,
    e_cur: u64,
    admitted_change_qcs: &AdmittedChangeQCs<Tx>,
) -> bool
where
    Tx: Serialize + Clone,
{
    let block_epoch = block.envelope.epoch;
    if block_epoch < e_cur {
        // Stale-epoch data block.
        return false;
    }
    if block_epoch > e_cur && !admitted_change_qcs.contains_key(&(block_epoch - 1)) {
        // Future-epoch data block without an admitted eleader-change QC.
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// data_chain_valid (zeus.tex §FuncDataChainValid)
// ---------------------------------------------------------------------------

/// O(1) chain-validity check: store membership IS chain validity under the
/// eager-admission invariant.
///
/// Every block admitted to `data_block_store` has been confirmed to have a
/// valid eleader signature and a `parent_hash` pointing to another in-store
/// block (or genesis).  No full ancestor walk is required.
///
/// Callers pass the hash of the block whose chain validity they need to
/// confirm.  For genesis (always in the store) this trivially returns `true`.
pub fn data_chain_valid<Tx>(
    hash: &DataBlockHash<Tx>,
    db: &DataBlockDB<Tx>,
) -> bool {
    db.contains(hash)
}

// ---------------------------------------------------------------------------
// conflicts_data_prefix (zeus.tex §FuncConflictsDataPrefix)
// ---------------------------------------------------------------------------

/// Returns `true` iff adopting `D` as data-chain head would conflict with a
/// signature-chain-committed attestation's pinned prefix.
///
/// `commit_lock` is the set of committed attestations (highest-height wins).
///
/// Change C: attestations now carry `data_block_hash` + `data_block_height`
/// rather than an embedded `DataBlock`.  The ancestor walk uses only the
/// resident metadata index (`parent_hash`/`height`/cached `hash`), so it stays
/// synchronous and never touches RocksDB.  If a pinned block's ancestor is
/// absent from the index (catch-up gap) the check conservatively returns
/// `true`.
/// The candidate head is identified by `(d_hash, d_height, d_parent)` — its
/// own hash, height, and parent hash — rather than a full `DataBlock`, so
/// callers can drive the walk from the resident metadata index without loading
/// any payload.
pub fn conflicts_data_prefix<Tx>(
    d_hash: &DataBlockHash<Tx>,
    d_height: u64,
    d_parent: &DataBlockHash<Tx>,
    commit_lock: &[(u64, Attestation<Tx>)],
    db: &DataBlockDB<Tx>,
) -> bool {
    for (_r_s, att) in commit_lock {
        let pinned_height = att.envelope.data_block_height;
        let pinned_hash = &att.envelope.data_block_hash;

        if pinned_height <= d_height {
            // pinned block should be an ancestor of d: walk d's parent chain
            // via the metadata index down to `pinned_height`.
            let mut cur_hash = d_hash.clone();
            let mut cur_height = d_height;
            let mut cur_parent = d_parent.clone();
            while cur_height > pinned_height {
                match db.meta(&cur_parent) {
                    Some(m) => {
                        cur_hash = m.hash.clone();
                        cur_height = m.height;
                        cur_parent = m.parent_hash.clone();
                    }
                    None => return true, // Missing ancestor — conservative reject
                }
            }
            // Now cur_height == pinned_height; check the ancestor hash matches.
            if cur_hash != *pinned_hash {
                return true;
            }
        } else {
            // pinned_height > d.height: adopting d would orphan a committed prefix.
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// latest_committed_attestation (zeus.tex §FuncLatestCommittedAttestation)
// ---------------------------------------------------------------------------

/// Returns a reference to the attestation in `commit_lock` that pins the
/// highest-height data prefix, or `None` if no committed attestations exist.
pub fn latest_committed_attestation<Tx>(
    commit_lock: &[(u64, Attestation<Tx>)]
) -> Option<&Attestation<Tx>>
where
    Tx: Serialize + Clone,
{
    commit_lock
        .iter()
        .max_by_key(|(_r_s, att)| att.envelope.data_block_height)
        .map(|(_, att)| att)
}

// ---------------------------------------------------------------------------
// eleader function (zeus.tex §FuncEleader)
// ---------------------------------------------------------------------------

/// Returns the eleader ID for epoch `e` with `n` total nodes.
/// `eleader(e) = (e mod n) + 1`  but we use 0-based IDs, so `e % n`.
pub fn eleader(
    epoch: u64,
    num_nodes: usize,
) -> crate::Id {
    (epoch as usize % num_nodes) as crate::Id
}

// ---------------------------------------------------------------------------
// EleaderBlame / EleaderChangeQC validity
// (Algorithm/zeus.tex §FuncEleaderBlameValid lines 213-237,
//  §FuncEleaderChangeQCValid lines 239-250)
// ---------------------------------------------------------------------------

/// Serializable signing tuple for a blame message.
///
/// Signed content per Algorithm/zeus.tex line 215:
///   `(TAG_ELEADER_BLAME, epoch, reason_tag, payload_bytes)`
///
/// We encode this as a 4-tuple of raw bytes so that bincode produces a
/// deterministic, stable layout for signing/verification.
#[derive(serde::Serialize)]
struct BlameSigned {
    tag: u8,
    epoch: u64,
    reason_tag: u8,
    payload_bytes: Vec<u8>,
}

/// Encode the payload bytes for a silence blame: `last_seen_height` as
/// little-endian u64.
fn silence_payload_bytes(last_seen_height: u64) -> Vec<u8> {
    last_seen_height.to_le_bytes().to_vec()
}

/// Encode the payload bytes for an equivocation blame:
/// `H(block_a.envelope) || H(block_b.envelope)` (64 bytes total).
fn equivocation_payload_bytes<Tx>(
    block_a: &DataBlock<Tx>,
    block_b: &DataBlock<Tx>,
) -> Vec<u8>
where
    Tx: Serialize + Clone,
{
    let mut bytes = Vec::with_capacity(64);
    bytes.extend_from_slice(block_a.hash().as_ref());
    bytes.extend_from_slice(block_b.hash().as_ref());
    bytes
}

/// Build the signed digest bytes for an eleader blame.
///
/// Returns the 32-byte hash digest as a `Vec<u8>` so callers can pass it to
/// `pk.verify(digest, sig_bytes)` without any phantom-type friction.  The
/// hash covers `(TAG_ELEADER_BLAME, epoch, reason_tag, payload_bytes)`.
pub fn blame_signed_bytes<Tx>(
    epoch: u64,
    reason: &BlameReason,
    payload: &BlamePayload<Tx>,
) -> Vec<u8>
where
    Tx: Serialize + Clone,
{
    let (reason_tag, payload_bytes) = match reason {
        BlameReason::Silence => {
            let h = match payload {
                BlamePayload::Silence { last_seen_height } => *last_seen_height,
                _ => 0,
            };
            (TAG_REASON_SILENCE, silence_payload_bytes(h))
        }
        BlameReason::Equivocation => {
            let (ba, bb) = match payload {
                BlamePayload::Equivocation {
                    block_a, block_b, ..
                } => (block_a, block_b),
                _ => panic!("mismatched BlameReason/BlamePayload"),
            };
            (TAG_REASON_EQUIVOCATION, equivocation_payload_bytes(ba, bb))
        }
    };
    let signing_tuple = BlameSigned {
        tag: TAG_ELEADER_BLAME,
        epoch,
        reason_tag,
        payload_bytes,
    };
    Hash::<BlameSigned>::ser_and_hash(&signing_tuple)
        .as_ref()
        .to_vec()
}

/// Validates a single eleader blame per Algorithm/zeus.tex lines 213-237.
///
/// Checks:
///   1. Signer's signature over `(TAG_ELEADER_BLAME, epoch, reason_tag,
///      payload_bytes)`.
///   2. For Silence: always true after sig check (threshold provides non-faulty
///      guarantee).
///   3. For Equivocation: both `block_a` and `block_b` are signed by the
///      expected eleader at the same `(epoch, height)` and are distinct.
pub fn eleader_blame_valid<Tx>(
    blame: &EleaderBlame<Tx>,
    pk_map: &fnv::FnvHashMap<crate::Id, crypto::PublicKey>,
    expected_eleader_id: crate::Id,
) -> bool
where
    Tx: Serialize + Clone,
{
    // 1. Verify the signer's sig over the blame tuple.
    let signer_pk = match pk_map.get(&blame.signer) {
        Some(pk) => pk,
        None => return false,
    };
    let digest = blame_signed_bytes(blame.epoch, &blame.reason, &blame.payload);
    if !signer_pk.verify(&digest, &blame.sig_raw) {
        return false;
    }

    // 2/3. Reason-specific checks.
    match &blame.reason {
        BlameReason::Silence => {
            // Silence is not self-verifying beyond the signature; threshold
            // on QC formation provides the non-faulty-blamer guarantee.
            true
        }
        BlameReason::Equivocation => {
            let (height, block_a, block_b) = match &blame.payload {
                BlamePayload::Equivocation {
                    height,
                    block_a,
                    block_b,
                } => (height, block_a, block_b),
                _ => return false,
            };
            // Blocks must be distinct.
            if block_a.hash() == block_b.hash() {
                return false;
            }
            // Both must be at the claimed height.
            if block_a.envelope.height != *height || block_b.envelope.height != *height {
                return false;
            }
            // Both must carry valid eleader signatures on their envelopes.
            let eleader_pk = match pk_map.get(&expected_eleader_id) {
                Some(pk) => pk,
                None => return false,
            };
            // DataBlock.hash() returns Hash<DataBlockEnvelope<Tx>>; as_ref() is &[u8].
            if !eleader_pk.verify(block_a.hash().as_ref(), &block_a.sig.raw) {
                return false;
            }
            if !eleader_pk.verify(block_b.hash().as_ref(), &block_b.sig.raw) {
                return false;
            }
            true
        }
    }
}

/// Validates an eleader-change QC per Algorithm/zeus.tex lines 239-250.
///
/// Checks:
///   1. Distinct signer count >= `num_faults + 1`.
///   2. For each blame: `blame.epoch == qc.epoch` AND `eleader_blame_valid`.
pub fn eleader_change_qc_valid<Tx>(
    qc: &EleaderChangeQC<Tx>,
    pk_map: &fnv::FnvHashMap<crate::Id, crypto::PublicKey>,
    num_faults: usize,
    num_nodes: usize,
) -> bool
where
    Tx: Serialize + Clone,
{
    let expected_eleader_id = eleader(qc.epoch, num_nodes);
    // Count distinct signers.
    let mut seen: std::collections::HashSet<crate::Id> = std::collections::HashSet::new();
    for blame in &qc.blames {
        if blame.epoch != qc.epoch {
            return false;
        }
        if !eleader_blame_valid(blame, pk_map, expected_eleader_id) {
            return false;
        }
        seen.insert(blame.signer);
    }
    seen.len() > num_faults
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AttestationSig, DataBlock};
    use crypto::hash::Hash;
    use std::marker::PhantomData;
    use std::sync::Arc;

    type Tx = u32;

    fn make_block(
        epoch: u64,
        height: u64,
        parent_hash: DataBlockHash<Tx>,
        payload: Vec<Tx>,
    ) -> DataBlock<Tx> {
        use crate::types::{DataBlockEnvelope, DataBlockSig};
        DataBlock {
            envelope: DataBlockEnvelope {
                epoch,
                height,
                payload: Arc::new(payload),
                parent_hash,
            },
            sig: DataBlockSig {
                raw: vec![],
                signer: 0,
                _phantom: PhantomData,
            },
            cached_hash: once_cell::sync::OnceCell::new(),
        }
    }

    fn make_attestation(
        epoch_s: u64,
        round_s: u64,
        data_block_hash: DataBlockHash<Tx>,
        data_block_height: u64,
        data_block_epoch: u64,
    ) -> Attestation<Tx> {
        use crate::types::AttestationEnvelope;
        Attestation::new(
            AttestationEnvelope {
                epoch_s,
                round_s,
                data_block_hash,
                data_block_height,
                data_block_epoch,
                parent_hash_s: Hash::EMPTY_HASH,
            },
            AttestationSig {
                raw: vec![],
                signer: 0,
                _phantom: PhantomData,
            },
        )
    }

    #[test]
    fn genesis_is_genesis() {
        let g = DataBlock::<Tx>::genesis();
        assert!(g.is_genesis());
        assert_eq!(g.envelope.height, 0);
        assert_eq!(g.envelope.epoch, 0);
    }

    #[test]
    fn hash_is_cached() {
        let g = DataBlock::<Tx>::genesis();
        // Two calls should return the same pointer (same OnceCell value).
        let h1 = g.hash();
        let h2 = g.hash();
        assert_eq!(h1, h2);
        // Pointer equality confirms the value was not recomputed.
        assert!(std::ptr::eq(h1, h2));
    }

    fn temp_store() -> storage::rocksdb::Storage {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("zeus-datachain-test-{nanos}-{n}.db"));
        storage::rocksdb::Storage::new(path.to_str().unwrap()).expect("open rocksdb")
    }

    #[tokio::test]
    async fn chain_valid_o1_absent() {
        // data_chain_valid (O(1)) returns false for a block not admitted.
        let db = DataBlockDB::<Tx>::new(temp_store());
        let genesis = DataBlock::<Tx>::genesis();
        let block1 = make_block(1, 1, genesis.hash().clone(), vec![]);
        let block1_hash = block1.hash().clone();
        // block1 not inserted — should return false.
        assert!(!data_chain_valid(&block1_hash, &db));
    }

    #[tokio::test]
    async fn chain_valid_o1_present() {
        // data_chain_valid (O(1)) returns true once block is admitted.
        let mut db = DataBlockDB::<Tx>::new(temp_store());
        let genesis = DataBlock::<Tx>::genesis();
        let block1 = make_block(1, 1, genesis.hash().clone(), vec![]);
        let block1_hash = block1.hash().clone();
        db.insert(block1).await.unwrap();
        assert!(data_chain_valid(&block1_hash, &db));
    }

    #[tokio::test]
    async fn no_conflict_for_genesis_commit() {
        let db = DataBlockDB::<Tx>::new(temp_store());
        let genesis = DataBlock::<Tx>::genesis();
        let genesis_hash = genesis.hash().clone();
        let att = make_attestation(1, 1, genesis_hash.clone(), 0, 0);
        let commit_lock = vec![(1u64, att)];

        // d == genesis should not conflict (equal height, same block hash).
        assert!(!conflicts_data_prefix(
            &genesis_hash,
            genesis.envelope.height,
            &genesis.envelope.parent_hash,
            &commit_lock,
            &db,
        ));
    }

    #[test]
    fn bincode_arc_roundtrip() {
        use crate::types::DataBlockEnvelope;
        // Verify that Arc<Vec<Tx>> survives a bincode round-trip.
        let env = DataBlockEnvelope::<u32> {
            epoch: 1,
            height: 2,
            payload: Arc::new(vec![10u32, 20, 30]),
            parent_hash: Hash::EMPTY_HASH,
        };
        let bytes = bincode::serialize(&env).expect("serialize");
        let env2: DataBlockEnvelope<u32> = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(env.epoch, env2.epoch);
        assert_eq!(env.height, env2.height);
        assert_eq!(*env.payload, *env2.payload);
    }

    #[test]
    fn eleader_round_robin() {
        assert_eq!(eleader(0, 4), 0);
        assert_eq!(eleader(1, 4), 1);
        assert_eq!(eleader(4, 4), 0);
        assert_eq!(eleader(5, 4), 1);
    }
}
