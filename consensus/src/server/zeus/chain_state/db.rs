//! DB-backed data-block store for Zeus.
//!
//! Replaces the unbounded in-memory `DataBlockStore` (a plain `FnvHashMap`).
//! During a crash-fault sig-round stall the eleader keeps admitting data blocks
//! while commits are frozen; holding every full `DataBlock` (each carrying an
//! `Arc<Vec<Tx>>` payload) in RAM grew the map to ~3.7 GB and OOM-killed the
//! eleader on 4 GiB hosts.
//!
//! `DataBlockDB` bounds resident memory by splitting storage:
//!   - a **resident metadata index** (`DataBlockMeta`: epoch/height/parent/hash),
//!     one small entry per admitted block, that answers the synchronous
//!     membership / chain-validity / equivocation queries without a DB hit;
//!   - **payloads written through to RocksDB** (via [`ChainDB`]), keyed by the
//!     block's envelope hash;
//!   - a **bounded resident cache** of full blocks so hot reads avoid the DB.
//!
//! Eviction is capacity-driven (insertion-order / FIFO), not commit-driven:
//! during a stall nothing commits, so uncommitted payloads must be evictable
//! too. Genesis is pinned and never evicted. Because `insert` is write-through,
//! any block whose metadata is resident is guaranteed present in RocksDB, so a
//! later `get` always succeeds (`contains(h) ⇒ get(h).await == Some`).
//!
//! NOTE: the metadata index itself still grows with the number of admitted
//! blocks (~80 bytes/entry). That is orders of magnitude smaller than the
//! payloads and is acceptable for the target workloads; spilling metadata to
//! disk too is a possible future extension if a pathological stall demands it.
use std::collections::VecDeque;

use anyhow::Result;
use fnv::FnvHashMap;
use serde::{de::DeserializeOwned, Serialize};
use storage::rocksdb::Storage;

use super::DataBlockHash;
use crate::server::ChainDB;
use crate::types::{DataBlock, DataBlockEnvelope};

/// Default resident full-block cache capacity (number of blocks).
pub const DEFAULT_DATA_BLOCK_CACHE_CAP: usize = 4096;

/// Always-resident, lightweight per-block metadata.
///
/// Carries everything the synchronous read paths need: membership
/// (`data_chain_valid`), the parent-link walk in `conflicts_data_prefix`, the
/// pinned-block epoch in `make_attestation`, and height checks in
/// `advance_to_epoch`. The cached `hash` lets the conflict walk compare hashes
/// without loading the full block.
#[derive(Debug, Clone)]
pub struct DataBlockMeta<Tx> {
    pub epoch: u64,
    pub height: u64,
    pub parent_hash: DataBlockHash<Tx>,
    pub hash: DataBlockHash<Tx>,
}

impl<Tx> DataBlockMeta<Tx> {
    fn of(block: &DataBlock<Tx>, hash: DataBlockHash<Tx>) -> Self {
        Self {
            epoch: block.envelope.epoch,
            height: block.envelope.height,
            parent_hash: block.envelope.parent_hash.clone(),
            hash,
        }
    }
}

pub struct DataBlockDB<Tx> {
    /// RocksDB-backed payload store (content-addressed by envelope hash).
    db: ChainDB,
    /// Resident metadata for every admitted block (including genesis).
    meta: FnvHashMap<DataBlockHash<Tx>, DataBlockMeta<Tx>>,
    /// Bounded resident cache of full blocks. FIFO (insertion-order) eviction.
    cache: FnvHashMap<DataBlockHash<Tx>, DataBlock<Tx>>,
    order: VecDeque<DataBlockHash<Tx>>,
    cap: usize,
    /// Pinned genesis block — always resident, never evicted, never on disk.
    genesis: DataBlock<Tx>,
    genesis_hash: DataBlockHash<Tx>,
}

// Synchronous, metadata-only queries. These touch only the resident `meta`
// index, so they need no `Tx` bound — callers (the sig/conflict paths) stay
// synchronous and don't have to carry `DeserializeOwned`.
impl<Tx> DataBlockDB<Tx> {
    /// True iff the block has been admitted (chain-membership == validity).
    pub fn contains(
        &self,
        h: &DataBlockHash<Tx>,
    ) -> bool {
        self.meta.contains_key(h)
    }

    /// Resident metadata for an admitted block, if any.
    pub fn meta(
        &self,
        h: &DataBlockHash<Tx>,
    ) -> Option<&DataBlockMeta<Tx>> {
        self.meta.get(h)
    }

    /// Epoch of an admitted block, if any.
    pub fn epoch_of(
        &self,
        h: &DataBlockHash<Tx>,
    ) -> Option<u64> {
        self.meta.get(h).map(|m| m.epoch)
    }
}

impl<Tx> DataBlockDB<Tx>
where
    Tx: Serialize + DeserializeOwned + Clone + 'static,
{
    pub fn new(store: Storage) -> Self {
        Self::with_capacity(store, DEFAULT_DATA_BLOCK_CACHE_CAP)
    }

    pub fn with_capacity(
        store: Storage,
        cap: usize,
    ) -> Self {
        let genesis = DataBlock::<Tx>::genesis();
        let genesis_hash = genesis.hash().clone();
        let mut meta = FnvHashMap::default();
        meta.insert(
            genesis_hash.clone(),
            DataBlockMeta::of(&genesis, genesis_hash.clone()),
        );
        Self {
            db: ChainDB::new(store),
            meta,
            cache: FnvHashMap::default(),
            order: VecDeque::new(),
            cap: cap.max(1),
            genesis,
            genesis_hash,
        }
    }

    // ----------------------------------------------------------------------
    // Asynchronous, full-block access (cache → RocksDB).
    // ----------------------------------------------------------------------

    /// Fetch a full block. Genesis is served from the pinned copy; otherwise
    /// the resident cache is consulted, then RocksDB (reloading into cache on a
    /// hit). Returns `Ok(None)` only for a hash never admitted locally.
    pub async fn get(
        &mut self,
        h: &DataBlockHash<Tx>,
    ) -> Result<Option<DataBlock<Tx>>> {
        if *h == self.genesis_hash {
            return Ok(Some(self.genesis.clone()));
        }
        if let Some(b) = self.cache.get(h) {
            return Ok(Some(b.clone()));
        }
        match self
            .db
            .read_as::<DataBlockEnvelope<Tx>, DataBlock<Tx>>(h)
            .await?
        {
            Some(block) => {
                self.cache_put(h.clone(), block.clone());
                Ok(Some(block))
            }
            None => Ok(None),
        }
    }

    /// Admit a block: record metadata (always), write the payload through to
    /// RocksDB, and place it in the resident cache. Idempotent — RocksDB is
    /// content-addressed and the metadata entry is keyed by hash.
    pub async fn insert(
        &mut self,
        block: DataBlock<Tx>,
    ) -> Result<()> {
        let h = block.hash().clone();
        self.meta
            .entry(h.clone())
            .or_insert_with(|| DataBlockMeta::of(&block, h.clone()));
        let serialized = bincode::serialize(&block)?;
        self.db
            .write_serialized::<DataBlockEnvelope<Tx>>(h.clone(), serialized)
            .await?;
        self.cache_put(h, block);
        Ok(())
    }

    /// Insert into the bounded cache with FIFO eviction. No-op reorder on a
    /// repeat insert (we keep insertion order, not true LRU — Zeus's access
    /// pattern is append-heavy, so the most-recently-admitted window stays
    /// resident, which is what the hot walks touch).
    fn cache_put(
        &mut self,
        h: DataBlockHash<Tx>,
        block: DataBlock<Tx>,
    ) {
        if self.cache.insert(h.clone(), block).is_none() {
            self.order.push_back(h);
            while self.cache.len() > self.cap {
                match self.order.pop_front() {
                    Some(old) => {
                        self.cache.remove(&old);
                    }
                    None => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> Storage {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("zeus-dbdb-test-{nanos}-{n}.db"));
        Storage::new(path.to_str().unwrap()).expect("open rocksdb")
    }

    fn block(height: u64, parent: DataBlockHash<u32>) -> DataBlock<u32> {
        use crate::types::DataBlockSig;
        use std::marker::PhantomData;
        use std::sync::Arc;
        DataBlock {
            envelope: DataBlockEnvelope {
                epoch: 1,
                height,
                payload: Arc::new(vec![height as u32; 4]),
                parent_hash: parent,
            },
            sig: DataBlockSig {
                raw: Vec::new(),
                signer: 0,
                _phantom: PhantomData,
            },
            cached_hash: Default::default(),
        }
    }

    #[tokio::test]
    async fn genesis_resident_and_membership() {
        let db = DataBlockDB::<u32>::new(temp_store());
        let g = DataBlock::<u32>::genesis();
        let gh = g.hash().clone();
        assert!(db.contains(&gh), "genesis must be a member");
        assert_eq!(db.epoch_of(&gh), Some(0));
    }

    #[tokio::test]
    async fn insert_then_get_and_contains() {
        let mut db = DataBlockDB::<u32>::new(temp_store());
        let g = DataBlock::<u32>::genesis();
        let b = block(1, g.hash().clone());
        let h = b.hash().clone();
        db.insert(b.clone()).await.unwrap();
        assert!(db.contains(&h));
        assert_eq!(db.meta(&h).unwrap().height, 1);
        let got = db.get(&h).await.unwrap().expect("present");
        assert_eq!(got.hash(), &h);
        // genesis still served from the pinned copy
        let gg = db.get(g.hash()).await.unwrap().expect("genesis present");
        assert!(gg.is_genesis());
    }

    #[tokio::test]
    async fn eviction_reloads_from_disk_and_contains_survives() {
        // Tiny cache forces eviction; the evicted block must still be
        // retrievable from RocksDB and remain a member.
        let mut db = DataBlockDB::<u32>::with_capacity(temp_store(), 2);
        let g = DataBlock::<u32>::genesis();
        let mut parent = g.hash().clone();
        let mut hashes = Vec::new();
        for height in 1..=10u64 {
            let b = block(height, parent.clone());
            let h = b.hash().clone();
            db.insert(b).await.unwrap();
            hashes.push(h.clone());
            parent = h;
        }
        // The first block is long evicted from the resident cache (cap 2)...
        let first = &hashes[0];
        assert!(db.contains(first), "metadata survives eviction");
        // ...but get() reloads it from disk.
        let reloaded = db.get(first).await.unwrap().expect("reload from disk");
        assert_eq!(reloaded.envelope.height, 1);
        assert_eq!(reloaded.hash(), first, "contains ⇒ get == Some");
    }

    #[tokio::test]
    async fn missing_hash_returns_none() {
        let mut db = DataBlockDB::<u32>::new(temp_store());
        let phantom = block(99, DataBlockHash::<u32>::EMPTY_HASH);
        assert!(!db.contains(phantom.hash()));
        assert!(db.get(phantom.hash()).await.unwrap().is_none());
    }
}
