/// Hera multi-author data-chain state.
///
/// Parallel to `zeus/chain_state/data_chain.rs::DataChainState`, but
/// `head_hash` / `head_height` / `committed_height` become per-author maps
/// keyed by `Id`.
///
/// Every node is the stable leader of its own data sub-chain.  This structure
/// tracks, for each author:
///   - The current admitted tip (hash + height).
///   - The highest height that has been committed (via a committed sig-block).
///   - A `pending_data_blocks` park map for out-of-order arrivals — holds
///     **child metadata** (hash + height), not full bodies.  Bodies live in the
///     data actor's plain in-memory `block_store`.
///
/// ## Memory model
/// Block **bodies** are stored by the data actor in a plain
/// `FnvHashMap<DataBlockHash<Tx>, DataBlock<Tx>>`, GC'd every 40 commits.
/// This struct holds only small per-author metadata.
///
/// ## Admission (synchronous, metadata-only)
/// The data actor inserts the body into `block_store` FIRST, then calls
/// `admit_metadata`. No store reads occur during admission:
///   (a) In-order: advance head to `(height, hash)`, publish ArcSwap, bridge
///       via `pending_data_blocks` if a parked child is now unlocked.
///   (b) Gap / unknown parent: push `(hash, height)` into
///       `pending_data_blocks[parent_hash]`, return `Parked(parent_hash)`.
///   (c) height <= head_height: `Duplicate`.
///
/// ## walk_range (synchronous, block_store-backed)
/// Follows `parent_hash` backward from the tip, reads each body from the
/// `block_store` hashmap, reverses to ascending order.  Only called on commit
/// (low-volume).  The caller passes `&FnvHashMap<...>` so this struct does
/// not need to hold it.
use crate::{
    types::{DataBlock, DataBlockEnvelope},
    Id,
};
use arc_swap::ArcSwap;
use crypto::hash::Hash;
use fnv::FnvHashMap;
use log::debug;
use serde::Serialize;
use std::sync::Arc;

/// Storage-key type for Hera data blocks: a content-addressed hash of the
/// `DataBlockEnvelope<Tx>`.  Canonical definition lives here; callers import
/// it via the `chain_state` re-export.
pub type DataBlockHash<Tx> = Hash<DataBlockEnvelope<Tx>>;

/// RocksDB key for a Hera data block: prefix byte `b'D'` followed by the
/// 32-byte hash.  Used by the data actor (write on admission) and the
/// consensus actor (notify_read in GateFut) so `Storage::notify_read` can
/// gate proposal processing on block arrival without disk I/O on the hot path
/// (the store's in-memory obligation map resolves the notify before RocksDB
/// flushes).
pub fn data_block_key<Tx>(hash: &DataBlockHash<Tx>) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + hash.as_ref().len());
    key.push(b'D');
    key.extend_from_slice(hash.as_ref());
    key
}

/// Lock-free snapshot of an author's current admitted head.
///
/// Published by the data actor via `ArcSwap::store` after every successful
/// `admit_metadata`. Read by the consensus actor (via `load_full`) when
/// building a `SigPropose` multi-attestation. Stale reads are safe: sub-chains
/// are append-only and attesting a slightly-old head costs at most one round of
/// throughput, never safety.
///
/// Using a single `ArcSwap<Arc<DataHeadSnapshot>>` instead of two separate
/// atomics prevents torn reads (new height paired with old hash would reference
/// a nonexistent block, parking attestations indefinitely).
#[derive(Clone, Debug)]
pub struct DataHeadSnapshot<Tx> {
    pub epoch: u64,
    pub height: u64,
    pub hash: DataBlockHash<Tx>,
}

/// Result of a metadata-only admission attempt.
#[derive(Debug)]
pub enum AdmitResult {
    /// Block extends author's head; head advanced.
    Extended,
    /// Out-of-order block whose parent was not yet the head; block's metadata
    /// was parked, then a pending chain was unlocked (bridge).
    Bridge,
    /// Block's parent hash is unknown; child metadata parked.
    Parked(DataBlockHash<u8>), // phantom-erased for caller compatibility
    /// Block already known (idempotent).
    Duplicate,
    /// Structural check failed (unknown author, genesis-parent invariant,
    /// etc.).
    Invalid,
}

/// Typed result of `admit_metadata`, carrying the missing parent hash in the
/// `Parked` case so the caller can emit a targeted `DataRequest`.
#[derive(Debug)]
pub enum AdmitTypedResult<Tx> {
    Extended,
    Bridge,
    /// Parent missing; the missing parent hash is carried here.
    Parked(DataBlockHash<Tx>),
    Duplicate,
    Invalid,
}

/// Per-author data-chain state for Hera.
///
/// Bodies are **not** stored here — they live in `DataBlockDB`.  Only small
/// metadata is kept in RAM.
pub struct MultiAuthorDataChainState<Tx> {
    /// Current admitted tip hash per author.
    pub heads: FnvHashMap<Id, DataBlockHash<Tx>>,
    /// Current admitted tip height per author.
    pub heights: FnvHashMap<Id, u64>,
    /// Highest height committed (via sig-block) per author.
    pub committed_heights: FnvHashMap<Id, u64>,
    /// Park map: missing parent_hash → Vec of (child_hash, child_height).
    /// Holds child **metadata only** — bodies are in DataBlockDB.
    ///
    /// Capacity-capped to `MAX_PENDING` entries total; if a permanently-missing
    /// parent causes unbounded growth, the oldest entries are evicted (the
    /// block is re-requested via the hint/catch-up path when needed).
    pub pending_data_blocks: FnvHashMap<DataBlockHash<Tx>, Vec<(DataBlockHash<Tx>, u64)>>,
    /// Lock-free per-author head snapshots. Published (via ArcSwap::store)
    /// by the data actor after each successful admission; loaded (via
    /// ArcSwap::load_full) by the consensus actor when building SigPropose.
    /// Shared Arc so both actors can hold a reference without copying the map.
    pub head_snapshots: Arc<FnvHashMap<Id, ArcSwap<Arc<DataHeadSnapshot<Tx>>>>>,
}

/// Maximum total parked-child entries across all authors before a coarse evict.
const MAX_PENDING: usize = 4096;

impl<Tx> MultiAuthorDataChainState<Tx>
where
    Tx: Serialize + Clone + 'static,
{
    /// Initialize with genesis state for all authors.
    pub fn new(all_ids: &[Id]) -> Self {
        let genesis_hash = DataBlock::<Tx>::genesis().hash().clone();

        let mut heads = FnvHashMap::default();
        let mut heights = FnvHashMap::default();
        let mut committed_heights = FnvHashMap::default();
        let mut head_snapshots_inner = FnvHashMap::default();

        for &id in all_ids {
            heads.insert(id, genesis_hash.clone());
            heights.insert(id, 0u64);
            committed_heights.insert(id, 0u64);
            head_snapshots_inner.insert(
                id,
                ArcSwap::new(Arc::new(Arc::new(DataHeadSnapshot {
                    epoch: 0,
                    height: 0,
                    hash: genesis_hash.clone(),
                }))),
            );
        }

        Self {
            heads,
            heights,
            committed_heights,
            pending_data_blocks: FnvHashMap::default(),
            head_snapshots: Arc::new(head_snapshots_inner),
        }
    }

    /// Publish the current admitted head for `author` into the lock-free
    /// ArcSwap slot. Called internally after every successful admission.
    pub fn publish_head(
        &self,
        author: Id,
        epoch: u64,
    ) {
        if let (Some(snap_slot), Some(&height), Some(hash)) = (
            self.head_snapshots.get(&author),
            self.heights.get(&author),
            self.heads.get(&author),
        ) {
            let snap = Arc::new(Arc::new(DataHeadSnapshot {
                epoch,
                height,
                hash: hash.clone(),
            }));
            snap_slot.store(snap);
        }
    }

    /// Read the current head snapshot for `author`. Returns a consistent
    /// (epoch, height, hash) triple even if the data actor is writing
    /// concurrently. The read is stale-OK: monotonic heads, safe to attest a
    /// slightly-old head.
    pub fn load_head_snapshot(
        &self,
        author: Id,
    ) -> Option<Arc<DataHeadSnapshot<Tx>>> {
        self.head_snapshots
            .get(&author)
            .map(|slot| (**slot.load()).clone())
    }

    /// Returns the current head hash for the given author.
    pub fn head_hash(
        &self,
        author: Id,
    ) -> Option<&DataBlockHash<Tx>> {
        self.heads.get(&author)
    }

    /// Returns the current head height for the given author.
    pub fn head_height(
        &self,
        author: Id,
    ) -> u64 {
        *self.heights.get(&author).unwrap_or(&0)
    }

    /// Returns the committed height watermark for the given author.
    pub fn committed_height(
        &self,
        author: Id,
    ) -> u64 {
        *self.committed_heights.get(&author).unwrap_or(&0)
    }

    /// Metadata-only admission — **synchronous, allocation-light**.
    ///
    /// Pre-conditions:
    ///   - Caller has verified `block.sig` before calling.
    ///   - Caller has already inserted the block body into `DataBlockDB` before
    ///     calling, so `walk_range` can read it later.
    ///
    /// Only the four scalars `(hash, height, parent_hash, author, epoch)` are
    /// used — no body access.
    pub fn admit_metadata(
        &mut self,
        hash: DataBlockHash<Tx>,
        height: u64,
        parent_hash: DataBlockHash<Tx>,
        author: Id,
        epoch: u64,
    ) -> AdmitTypedResult<Tx>
    where
        Tx: PartialEq + std::fmt::Debug,
    {
        // Idempotent: if this hash is already the head or below it, treat as
        // duplicate.
        let head_height = self.head_height(author);
        if height <= head_height {
            return AdmitTypedResult::Duplicate;
        }

        // Author must be known.
        if !self.heads.contains_key(&author) {
            debug!(
                "Hera: admit_metadata: unknown author {} for block height={}",
                author, height
            );
            return AdmitTypedResult::Invalid;
        }

        let head_hash = self.heads[&author].clone();

        if parent_hash == head_hash {
            // -------------------------------------------------------------------
            // Case (a): extends current head directly.
            // -------------------------------------------------------------------
            self.heads.insert(author, hash.clone());
            self.heights.insert(author, height);
            self.publish_head(author, epoch);

            // Bridge: while a parked child whose parent is the new head exists,
            // advance the head to that child.
            self.bridge_pending(author, epoch);

            debug!(
                "Hera: admit_metadata case(a): author={} height={}",
                author, height
            );
            AdmitTypedResult::Extended
        } else {
            // -------------------------------------------------------------------
            // Case (c): parent not yet the head → park metadata.
            // -------------------------------------------------------------------
            let genesis_hash = DataBlock::<Tx>::genesis().hash().clone();
            if parent_hash == genesis_hash && height <= head_height + 1 {
                debug!(
                    "Hera: admit_metadata: genesis-parent block at height={} but \
                     head={}, dropping",
                    height, head_height
                );
                return AdmitTypedResult::Invalid;
            }

            debug!(
                "Hera: admit_metadata case(c): author={} height={} parent missing, parking",
                author, height
            );

            // Evict if the pending map is saturated.
            let total: usize = self.pending_data_blocks.values().map(|v| v.len()).sum();
            if total >= MAX_PENDING {
                self.pending_data_blocks.clear();
                debug!("Hera: admit_metadata: pending_data_blocks evicted (over MAX_PENDING)");
            }

            self.pending_data_blocks
                .entry(parent_hash.clone())
                .or_default()
                .push((hash, height));

            AdmitTypedResult::Parked(parent_hash)
        }
    }

    /// Walk `pending_data_blocks` forward from author's new head, advancing
    /// the head for each parked child whose parent is now the head.
    fn bridge_pending(
        &mut self,
        author: Id,
        epoch: u64,
    ) {
        loop {
            let current_head = match self.heads.get(&author) {
                Some(h) => h.clone(),
                None => break,
            };
            let parked = match self.pending_data_blocks.remove(&current_head) {
                Some(v) if !v.is_empty() => v,
                _ => break,
            };
            let (child_hash, child_height) = parked[0].clone();
            self.heads.insert(author, child_hash);
            self.heights.insert(author, child_height);
            self.publish_head(author, epoch);
        }
    }

    /// Walk author's chain from `from_height + 1` to `to_height` (inclusive),
    /// reading block bodies from `block_store` (plain hashmap, synchronous) and
    /// returning them in ascending height order.
    ///
    /// **Synchronous** — no disk I/O, no await. Only called on commit
    /// (low-volume path).
    pub fn walk_range(
        &mut self,
        block_store: &FnvHashMap<DataBlockHash<Tx>, DataBlock<Tx>>,
        tip_hash: &DataBlockHash<Tx>,
        from_height: u64,
        to_height: u64,
    ) -> Vec<DataBlock<Tx>>
    where
        Tx: Clone + std::fmt::Debug,
    {
        if to_height <= from_height {
            return Vec::new();
        }

        let genesis = DataBlock::<Tx>::genesis();
        let genesis_hash = genesis.hash().clone();

        let mut chain: Vec<DataBlock<Tx>> = Vec::new();
        let mut current_hash = tip_hash.clone();

        loop {
            // Resolve genesis separately so it is always available.
            let block = if current_hash == genesis_hash {
                genesis.clone()
            } else {
                match block_store.get(&current_hash) {
                    Some(b) => b.clone(),
                    None => {
                        log::warn!("Hera: walk_range: block not found for {:?}", current_hash);
                        break;
                    }
                }
            };

            let h = block.envelope.height;
            if h <= from_height {
                break;
            }
            let parent = block.envelope.parent_hash.clone();
            chain.push(block);
            if h == 0 || h <= from_height + 1 {
                break;
            }
            current_hash = parent;
        }

        // chain is in descending order; reverse to ascending.
        chain.reverse();
        // Filter to only heights in (from_height, to_height].
        chain.retain(|b| b.envelope.height > from_height && b.envelope.height <= to_height);
        chain
    }
}
