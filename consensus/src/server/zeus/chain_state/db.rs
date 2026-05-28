//! DB-backed data-block store for Zeus.
//!
//! Replaces the unbounded in-memory `DataBlockStore` (a plain `FnvHashMap`).
//! During a crash-fault sig-round stall the eleader keeps admitting data blocks
//! while commits are frozen; holding every full `DataBlock` (each carrying an
//! `Arc<Vec<Tx>>` payload) in RAM grew the map to ~3.7 GB and OOM-killed the
//! eleader on 4 GiB hosts.
//!
//! `DataBlockDB` is a **write-back** store, so steady-state commit incurs no
//! disk I/O at all (which is what kept peak throughput high):
//!   - a **resident metadata index** (`DataBlockMeta`:
//!     epoch/height/parent/hash), one small entry per admitted block, answers
//!     the synchronous membership / chain-validity / conflict-walk / epoch
//!     queries without a DB hit;
//!   - a **bounded in-memory cache** of full blocks is the primary payload
//!     store. `insert` only touches RAM — it does NOT write through to disk.
//!   - on eviction (cache over budget), a block is **dropped for free if it has
//!     already been emitted** by the committer (`height <= emitted_high`), and
//!     **spilled to RocksDB only if not yet emitted** (the uncommitted backlog
//!     that piles up during a crash-fault stall).
//!
//! So in steady state — commits keep up, the oldest cached blocks are already
//! emitted — eviction just frees memory and nothing hits disk. Only a stall
//! (commits frozen, backlog grows past the budget) spills payloads to disk,
//! bounding RAM. The committer emits resident blocks directly and reads only
//! spilled ones back (off the consensus loop). `contains(h)` via the metadata
//! index never depends on the payload's location.
//!
//! NOTE: the metadata index still grows with admitted-block count (~80 B/entry)
//! — orders of magnitude smaller than payloads; spilling it too is a possible
//! future extension.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use fnv::FnvHashMap;
use serde::{de::DeserializeOwned, Serialize};
use storage::rocksdb::Storage;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use super::DataBlockHash;
use crate::server::ChainDB;
use crate::types::{DataBlock, DataBlockEnvelope};

/// A block handed to the background committer for emission. In steady state the
/// block is still resident in the cache and is passed by value (no disk); only
/// blocks that were spilled to disk during a stall are passed by hash, and the
/// committer reads those back off the consensus loop.
pub enum CommitItem<Tx> {
    Resident(DataBlock<Tx>),
    Spilled(DataBlockHash<Tx>),
}

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
    fn of(
        block: &DataBlock<Tx>,
        hash: DataBlockHash<Tx>,
    ) -> Self {
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
    /// Hands (key, serialized-payload) to a background writer task. Used ONLY
    /// to spill not-yet-emitted blocks to disk on eviction (during a stall);
    /// steady-state inserts never write here.
    write_tx: UnboundedSender<(Vec<u8>, Vec<u8>)>,
    /// Highest emitted (committed + delivered) data-block height, advanced by
    /// the background committer. Eviction drops blocks at or below this for
    /// free (the committer is done with them) and spills the rest to disk.
    emitted_high: Arc<AtomicU64>,
    /// Resident metadata for every admitted block (including genesis).
    meta: FnvHashMap<DataBlockHash<Tx>, DataBlockMeta<Tx>>,
    /// Bounded resident cache of full blocks, keyed by hash. Each entry stores
    /// the block and its serialized byte size. FIFO (insertion-order) eviction
    /// keyed on a cumulative byte budget, not a block count.
    cache: FnvHashMap<DataBlockHash<Tx>, (DataBlock<Tx>, usize)>,
    order: VecDeque<DataBlockHash<Tx>>,
    /// Cumulative serialized bytes of the blocks currently resident in `cache`.
    resident_bytes: usize,
    /// Eviction budget in bytes; evict oldest until `resident_bytes <=
    /// cap_bytes`.
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
    /// Convenience constructor with a private `emitted_high` (starts at 0, so
    /// all evictions spill). Used by unit tests; production uses
    /// `with_emitted_high` to share the committer's watermark.
    pub fn new(store: Storage) -> Self {
        Self::with_byte_budget(
            store,
            DEFAULT_DATA_BLOCK_CACHE_BYTES,
            Arc::new(AtomicU64::new(0)),
        )
    }

    /// Production constructor: shares the committer's `emitted_high` watermark
    /// so eviction can drop already-emitted blocks for free.
    pub fn with_emitted_high(
        store: Storage,
        emitted_high: Arc<AtomicU64>,
    ) -> Self {
        Self::with_byte_budget(store, DEFAULT_DATA_BLOCK_CACHE_BYTES, emitted_high)
    }

    pub fn with_byte_budget(
        store: Storage,
        cap_bytes: usize,
        emitted_high: Arc<AtomicU64>,
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
            emitted_high,
            meta,
            cache: FnvHashMap::default(),
            order: VecDeque::new(),
            resident_bytes: 0,
            cap_bytes: cap_bytes.max(1),
            genesis,
            genesis_hash,
        }
    }

    /// Synchronous, cache-only fetch (no disk). Used by the commit walk on the
    /// consensus loop: a hit yields the block to hand to the committer by
    /// value; a miss means the block was spilled to disk and the committer must
    /// read it back. Never touches RocksDB, so it never blocks the loop.
    pub fn cache_get(
        &self,
        h: &DataBlockHash<Tx>,
    ) -> Option<DataBlock<Tx>> {
        if *h == self.genesis_hash {
            return Some(self.genesis.clone());
        }
        self.cache.get(h).map(|(b, _)| b.clone())
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
        // Not in cache: it was either spilled to disk (read it back) or dropped
        // after emission (gone — only reachable by a hopelessly-behind peer,
        // who needs full sync, not a point fetch). Use a NON-blocking read so a
        // dropped block returns None instead of parking forever. The committer
        // uses a separate blocking read only for blocks it knows were spilled.
        match self
            .db
            .read_as::<DataBlockEnvelope<Tx>, DataBlock<Tx>>(h)
            .await?
        {
            Some(block) => Ok(Some(block)),
            None => Ok(None),
        }
    }

    /// Admit a block: record metadata and place it in the resident cache.
    /// Write-back — this does NOT touch disk. A block only reaches RocksDB if
    /// it is later evicted while still un-emitted (see `cache_put`).
    pub async fn insert(
        &mut self,
        block: DataBlock<Tx>,
    ) -> Result<()> {
        let h = block.hash().clone();
        self.meta
            .entry(h.clone())
            .or_insert_with(|| DataBlockMeta::of(&block, h.clone()));
        // Approximate resident size by the payload-bearing envelope; exact
        // serialized bytes aren't needed for a memory budget.
        let size = (block.envelope.payload.len() + 1) * 64;
        self.cache_put(h, block, size);
        Ok(())
    }

    /// Insert into the byte-bounded cache, evicting oldest blocks (FIFO) until
    /// within `cap_bytes`. An evicted block is **dropped for free** if it has
    /// already been emitted (`height <= emitted_high`), and **spilled to disk**
    /// otherwise (the un-emitted backlog during a stall). So steady state —
    /// where the oldest cached blocks are already emitted — never writes to
    /// disk. The just-inserted block is kept (we never evict below one entry).
    fn cache_put(
        &mut self,
        h: DataBlockHash<Tx>,
        block: DataBlock<Tx>,
        size: usize,
    ) {
        match self.cache.insert(h.clone(), (block, size)) {
            Some((_, old_size)) => {
                self.resident_bytes = self.resident_bytes + size - old_size;
            }
            None => {
                self.order.push_back(h);
                self.resident_bytes += size;
            }
        }
        while self.resident_bytes > self.cap_bytes && self.cache.len() > 1 {
            let old = match self.order.pop_front() {
                Some(o) => o,
                None => break,
            };
            if let Some((evicted, sz)) = self.cache.remove(&old) {
                self.resident_bytes = self.resident_bytes.saturating_sub(sz);
                let emitted = self.emitted_high.load(Ordering::Relaxed);
                if evicted.envelope.height > emitted {
                    // Not yet emitted — the committer will need it later. Spill
                    // to disk (non-blocking) so it can read it back.
                    if let Ok(bytes) = bincode::serialize(&evicted) {
                        let _ = self.write_tx.send((old.to_vec(), bytes));
                    }
                }
                // else: already emitted — drop it; nobody needs the payload.
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

    fn block(
        height: u64,
        parent: DataBlockHash<u32>,
    ) -> DataBlock<u32> {
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

    /// Fill past the budget with 10 blocks; `first` is evicted from the cache.
    async fn fill_and_evict(emitted_high_val: u64) -> (DataBlockDB<u32>, Vec<DataBlockHash<u32>>) {
        let emitted = Arc::new(AtomicU64::new(emitted_high_val));
        // size per block = (payload.len()+1)*64 = (4+1)*64 = 320; budget ~2.
        let mut db = DataBlockDB::<u32>::with_byte_budget(temp_store(), 650, emitted);
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
        assert!(db.resident_bytes() <= 650, "cache stays within byte budget");
        assert!(db.cached_blocks() < 10, "eviction occurred");
        (db, hashes)
    }

    #[tokio::test]
    async fn unemitted_eviction_spills_and_reloads() {
        // emitted_high = 0 → every evicted block is un-emitted → spilled to disk.
        let (mut db, hashes) = fill_and_evict(0).await;
        let first = &hashes[0];
        assert!(db.contains(first), "metadata survives eviction");
        // The spill write is async; let the writer task drain, then read back.
        let mut reloaded = None;
        for _ in 0..2000 {
            if let Some(b) = db.get(first).await.unwrap() {
                reloaded = Some(b);
                break;
            }
            tokio::task::yield_now().await;
        }
        let reloaded = reloaded.expect("spilled block reloads from disk");
        assert_eq!(reloaded.envelope.height, 1);
        assert_eq!(reloaded.hash(), first);
    }

    #[tokio::test]
    async fn emitted_eviction_is_dropped() {
        // emitted_high above all heights → every evicted block is dropped (no
        // disk write); a later get returns None (payload is gone).
        let (mut db, hashes) = fill_and_evict(1000).await;
        let first = &hashes[0];
        assert!(db.contains(first), "metadata still tracks the block");
        // Give any (erroneous) writer activity a chance, then confirm it's gone.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(
            db.get(first).await.unwrap().is_none(),
            "emitted + evicted block is dropped, not on disk"
        );
    }

    #[tokio::test]
    async fn missing_hash_returns_none() {
        let mut db = DataBlockDB::<u32>::new(temp_store());
        let phantom = block(99, DataBlockHash::<u32>::EMPTY_HASH);
        assert!(!db.contains(phantom.hash()));
        assert!(db.get(phantom.hash()).await.unwrap().is_none());
    }
}
