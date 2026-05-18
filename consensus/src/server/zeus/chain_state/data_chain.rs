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
use crate::types::{Attestation, DataBlock, DataBlockEnvelope, ZeusMsg};
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

/// The actual block store keyed by `H(envelope)`.
pub type DataBlockStore<Tx> = FnvHashMap<DataBlockHash<Tx>, DataBlock<Tx>>;

/// Park map: missing-data-hash → parked (attestation-containing) ZeusMsg
/// entries. On `OnDataResponse` drain, each entry is re-dispatched to the
/// appropriate handler.
pub type PendingAttestations<Tx> = FnvHashMap<DataBlockHash<Tx>, Vec<ZeusMsg<Tx>>>;

/// Admitted eleader-change QC map (epoch → QC present flag for steady-state).
/// TODO(zeus-view-change): replace bool with the actual QC type.
pub type AdmittedChangeQCs = FnvHashMap<u64, bool>;

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
    admitted_change_qcs: &AdmittedChangeQCs,
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
        // TODO(zeus-view-change): admitted_change_qcs[block_epoch - 1] should be
        // Some(QC).
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
    store: &DataBlockStore<Tx>,
) -> bool
where
    Tx: Serialize + Clone,
{
    store.contains_key(hash)
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
/// rather than an embedded `DataBlock`.  The conflict check looks up the
/// pinned block in `store` by hash; if the block is absent (catch-up gap)
/// the check conservatively returns `true`.
pub fn conflicts_data_prefix<Tx>(
    d: &DataBlock<Tx>,
    commit_lock: &[(u64, Attestation<Tx>)],
    store: &DataBlockStore<Tx>,
) -> bool
where
    Tx: Serialize + Clone + PartialEq,
{
    for (_r_s, att) in commit_lock {
        let pinned_height = att.envelope.data_block_height;
        let pinned_hash = &att.envelope.data_block_hash;

        if pinned_height <= d.envelope.height {
            // pinned block should be an ancestor of d: walk d's parent chain.
            let mut d_prime = d.clone();
            while d_prime.envelope.height > pinned_height {
                let parent_hash = d_prime.envelope.parent_hash.clone();
                match store.get(&parent_hash) {
                    Some(p) => d_prime = p.clone(),
                    None => return true, // Missing ancestor — conservative reject
                }
            }
            // Now d_prime.height == pinned_height; check its hash matches.
            if d_prime.hash() != pinned_hash {
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

    #[test]
    fn chain_valid_o1_absent() {
        // data_chain_valid (O(1)) returns false for a block not in the store.
        let genesis = DataBlock::<Tx>::genesis();
        let g_hash = genesis.hash().clone();
        let block1 = make_block(1, 1, g_hash.clone(), vec![]);
        let block1_hash = block1.hash().clone();

        let mut store: DataBlockStore<Tx> = FnvHashMap::default();
        store.insert(g_hash, genesis);
        // block1 not inserted — should return false.
        assert!(!data_chain_valid(&block1_hash, &store));
    }

    #[test]
    fn chain_valid_o1_present() {
        // data_chain_valid (O(1)) returns true once block is in the store.
        let genesis = DataBlock::<Tx>::genesis();
        let g_hash = genesis.hash().clone();
        let block1 = make_block(1, 1, g_hash.clone(), vec![]);
        let block1_hash = block1.hash().clone();

        let mut store: DataBlockStore<Tx> = FnvHashMap::default();
        store.insert(g_hash, genesis);
        store.insert(block1_hash.clone(), block1);
        assert!(data_chain_valid(&block1_hash, &store));
    }

    #[test]
    fn no_conflict_for_genesis_commit() {
        let genesis = DataBlock::<Tx>::genesis();
        let genesis_hash = genesis.hash().clone();
        let att = make_attestation(1, 1, genesis_hash, 0, 0);

        let commit_lock = vec![(1u64, att)];
        let store: DataBlockStore<Tx> = FnvHashMap::default();

        // d == genesis should not conflict (equal height, same block hash)
        assert!(!conflicts_data_prefix(&genesis, &commit_lock, &store));
    }

    #[test]
    fn bincode_arc_roundtrip() {
        use crate::types::{DataBlockEnvelope, DataBlockSig};
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
