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
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use super::DataBlockHash;
use crate::server::ChainDB;
use crate::types::{DataBlock, DataBlockEnvelope};

/// Default resident full-block cache budget, in bytes of serialized payload.
///
/// Sized as a small fraction of the (4 GiB) bench hosts so the cache cannot be
/// the OOM driver, while comfortably covering the steady-state working set
/// (recent tips + the commit-prefix window). NOTE: a *block-count* cap is the
/// wrong unit here — at `tx_size=512`, `batch_size≈1000` a block is ~0.5 MB, so
/// a 4096-block cap would be a 2 GB ceiling that never evicts in a stall. The
/// budget is in bytes precisely so it is robust to block size.
pub const DEFAULT_DATA_BLOCK_CACHE_BYTES: usize = 128 * 1024 * 1024;

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
    /// RocksDB-backed payload store (content-addressed by envelope hash). Used
    /// only for READS on the consensus task; writes go through `write_tx`.
    db: ChainDB,
    /// Hands (key, serialized-payload) to a background writer task so the
    /// consensus task never awaits a disk write. The writer drains this and
    /// persists to the same RocksDB; `get` uses `notify_read` to wait for a
    /// not-yet-flushed block, so correctness holds despite the async write.
    write_tx: UnboundedSender<(Vec<u8>, Vec<u8>)>,
    /// Resident metadata for every admitted block (including genesis).
    meta: FnvHashMap<DataBlockHash<Tx>, DataBlockMeta<Tx>>,
    /// Bounded resident cache of full blocks, keyed by hash. Each entry stores
    /// the block and its serialized byte size. FIFO (insertion-order) eviction
    /// keyed on a cumulative byte budget, not a block count.
    cache: FnvHashMap<DataBlockHash<Tx>, (DataBlock<Tx>, usize)>,
    order: VecDeque<DataBlockHash<Tx>>,
    /// Cumulative serialized bytes of the blocks currently resident in `cache`.
    resident_bytes: usize,
    /// Eviction budget in bytes; evict oldest until `resident_bytes <= cap_bytes`.
    cap_bytes: usize,
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

    /// Bytes of full blocks currently resident in the cache (excludes genesis
    /// and the metadata index). Used by tests/metrics to confirm the bound.
    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// Number of full blocks currently resident in the cache.
    pub fn cached_blocks(&self) -> usize {
        self.cache.len()
    }
}

impl<Tx> DataBlockDB<Tx>
where
    Tx: Serialize + DeserializeOwned + Clone + 'static,
{
    pub fn new(store: Storage) -> Self {
        Self::with_byte_budget(store, DEFAULT_DATA_BLOCK_CACHE_BYTES)
    }

    pub fn with_byte_budget(
        store: Storage,
        cap_bytes: usize,
    ) -> Self {
        let genesis = DataBlock::<Tx>::genesis();
        let genesis_hash = genesis.hash().clone();
        let mut meta = FnvHashMap::default();
        meta.insert(
            genesis_hash.clone(),
            DataBlockMeta::of(&genesis, genesis_hash.clone()),
        );

        // Background writer: persists payloads off the consensus task. `insert`
        // hands (key, value) here without awaiting; this task absorbs the
        // RocksDB write latency (and any libstorage actor backpressure) so the
        // consensus loop is never blocked on disk.
        let (write_tx, mut write_rx) = unbounded_channel::<(Vec<u8>, Vec<u8>)>();
        let mut writer_store = store.clone();
        tokio::spawn(async move {
            while let Some((key, value)) = write_rx.recv().await {
                writer_store.write(key, value).await;
            }
        });

        Self {
            db: ChainDB::new(store),
            write_tx,
            meta,
            cache: FnvHashMap::default(),
            order: VecDeque::new(),
            resident_bytes: 0,
            cap_bytes: cap_bytes.max(1),
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
        // Not admitted locally → genuinely absent. (Must short-circuit before
        // notify_read, which would park forever for a key never written.)
        if !self.meta.contains_key(h) {
            return Ok(None);
        }
        if let Some((b, _)) = self.cache.get(h) {
            return Ok(Some(b.clone()));
        }
        // Admitted but evicted from the cache. It has been, or will shortly be,
        // persisted by the background writer; `notify_read` returns immediately
        // if already on disk, otherwise waits for the pending write to land.
        let block: DataBlock<Tx> = self
            .db
            .notify_read_as::<DataBlockEnvelope<Tx>, DataBlock<Tx>>(h)
            .await?;
        let size = bincode::serialized_size(&block).unwrap_or(0) as usize;
        self.cache_put(h.clone(), block.clone(), size);
        Ok(Some(block))
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
        let size = serialized.len();
        // Hand the payload to the background writer — non-blocking, no disk
        // await on the consensus loop. (Send only fails if the writer task is
        // gone, i.e. shutdown; safe to ignore.)
        let _ = self.write_tx.send((h.to_vec(), serialized));
        self.cache_put(h, block, size);
        Ok(())
    }

    /// Insert into the byte-bounded cache, evicting oldest blocks (FIFO) until
    /// the cumulative serialized size is within `cap_bytes`. The just-inserted
    /// block is never evicted in the same call (we keep at least one entry), so
    /// an immediate read-back hits the cache. We keep insertion order rather
    /// than true LRU — Zeus's access pattern is append-heavy, so the
    /// most-recently-admitted window (what the hot walks touch) stays resident.
    fn cache_put(
        &mut self,
        h: DataBlockHash<Tx>,
        block: DataBlock<Tx>,
        size: usize,
    ) {
        match self.cache.insert(h.clone(), (block, size)) {
            Some((_, old_size)) => {
                // Replaced an existing entry; adjust the byte total in place.
                self.resident_bytes = self.resident_bytes + size - old_size;
            }
            None => {
                self.order.push_back(h);
                self.resident_bytes += size;
            }
        }
        while self.resident_bytes > self.cap_bytes && self.cache.len() > 1 {
            match self.order.pop_front() {
                Some(old) => {
                    if let Some((_, sz)) = self.cache.remove(&old) {
                        self.resident_bytes = self.resident_bytes.saturating_sub(sz);
                    }
                }
                None => break,
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
        // Measure one block's serialized size, then set a byte budget that
        // holds only ~2 blocks so inserting 10 forces eviction. The evicted
        // blocks must still be retrievable from RocksDB and remain members.
        let g = DataBlock::<u32>::genesis();
        let probe = block(1, g.hash().clone());
        let block_bytes = bincode::serialized_size(&probe).unwrap() as usize;
        let budget = block_bytes * 2 + 1;

        let mut db = DataBlockDB::<u32>::with_byte_budget(temp_store(), budget);
        let mut parent = g.hash().clone();
        let mut hashes = Vec::new();
        for height in 1..=10u64 {
            let b = block(height, parent.clone());
            let h = b.hash().clone();
            db.insert(b).await.unwrap();
            hashes.push(h.clone());
            parent = h;
        }
        // The byte budget is respected and eviction happened (not all 10 fit).
        assert!(db.resident_bytes() <= budget, "cache stays within byte budget");
        assert!(db.cached_blocks() < 10, "eviction occurred");
        // The first block is long evicted from the resident cache...
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
