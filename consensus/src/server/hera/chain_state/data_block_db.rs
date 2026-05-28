//! Write-back data-block cache for Hera.
//!
//! This is a port of `zeus/chain_state/db.rs::DataBlockDB` adapted for Hera's
//! data model (`DataBlock`/`DataBlockEnvelope`/`DataBlockHash` are the same
//! crate types, but Hera's `DataBlockHash` alias lives here).  The Zeus copy
//! is left untouched; the temporary duplication is intentional while both
//! protocols coexist.  If a third protocol needs this, it should be extracted
//! into a shared module.
//!
//! See `zeus/chain_state/db.rs` for the full design rationale.  Short summary:
//!   - Resident metadata index (`meta`, ~80 B/entry) answers all synchronous
//!     membership/walk queries without hitting disk.
//!   - Byte-bounded FIFO cache of full blocks; `insert` is synchronous,
//!     allocation-light, never touches disk.
//!   - On eviction: drop for free if `height <= emitted_high`, else spill to
//!     disk via a background writer channel.
//!   - `get` (async) checks cache, then RocksDB; `contains` / `meta` /
//!     `epoch_of` are sync and cache-only.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use fnv::FnvHashMap;
use serde::{de::DeserializeOwned, Serialize};
use storage::rocksdb::Storage;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::server::ChainDB;
use crate::types::{DataBlock, DataBlockEnvelope};
use crypto::hash::Hash;

/// Storage-key type for Hera data blocks: a content-addressed hash of the
/// `DataBlockEnvelope<Tx>`.  Defined here so both `DataBlockDB` and
/// `MultiAuthorDataChainState` can import it from `super`.
pub type DataBlockHash<Tx> = Hash<DataBlockEnvelope<Tx>>;

/// Default resident full-block cache budget in bytes.
/// Same constant as Zeus; see zeus/chain_state/db.rs for sizing rationale.
pub const DEFAULT_DATA_BLOCK_CACHE_BYTES: usize = 128 * 1024 * 1024;

/// Always-resident, lightweight per-block metadata.
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
    /// RocksDB-backed payload store.  Used only for reads on the consensus
    /// task; writes go through `write_tx`.
    db: ChainDB,
    /// Background writer: hands (key, bytes) pairs off the consensus loop.
    write_tx: UnboundedSender<(Vec<u8>, Vec<u8>)>,
    /// Highest emitted data-block height — advanced by the data actor after
    /// each `on_commit_emit`.  Eviction drops blocks at or below this for free.
    emitted_high: Arc<AtomicU64>,
    /// Resident metadata for every admitted block (including genesis).
    meta: FnvHashMap<DataBlockHash<Tx>, DataBlockMeta<Tx>>,
    /// Bounded resident cache of full blocks.
    cache: FnvHashMap<DataBlockHash<Tx>, (DataBlock<Tx>, usize)>,
    order: VecDeque<DataBlockHash<Tx>>,
    /// Running byte total of resident cache (excludes genesis + metadata).
    resident_bytes: usize,
    /// Cache capacity in bytes.
    cap_bytes: usize,
    /// Pinned genesis block — always resident, never evicted.
    genesis: DataBlock<Tx>,
    genesis_hash: DataBlockHash<Tx>,
}

// ---------------------------------------------------------------------------
// Synchronous, metadata-only queries (no Tx bound needed).
// ---------------------------------------------------------------------------

impl<Tx> DataBlockDB<Tx> {
    /// True iff the block has been admitted.
    pub fn contains(
        &self,
        h: &DataBlockHash<Tx>,
    ) -> bool {
        self.meta.contains_key(h)
    }

    /// Resident metadata for an admitted block.
    pub fn meta(
        &self,
        h: &DataBlockHash<Tx>,
    ) -> Option<&DataBlockMeta<Tx>> {
        self.meta.get(h)
    }

    /// Epoch of an admitted block.
    pub fn epoch_of(
        &self,
        h: &DataBlockHash<Tx>,
    ) -> Option<u64> {
        self.meta.get(h).map(|m| m.epoch)
    }

    /// Bytes currently resident in the cache (for tests/metrics).
    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// Number of full blocks currently resident in the cache.
    pub fn cached_blocks(&self) -> usize {
        self.cache.len()
    }
}

// ---------------------------------------------------------------------------
// Full-block access (requires Serialize + DeserializeOwned + Clone).
// ---------------------------------------------------------------------------

impl<Tx> DataBlockDB<Tx>
where
    Tx: Serialize + DeserializeOwned + Clone + 'static,
{
    /// Convenience constructor with a private `emitted_high` (starts at 0).
    /// Used by unit tests; production code calls `with_emitted_high`.
    pub fn new(store: Storage) -> Self {
        Self::with_byte_budget(
            store,
            DEFAULT_DATA_BLOCK_CACHE_BYTES,
            Arc::new(AtomicU64::new(0)),
        )
    }

    /// Production constructor: shares the committer's `emitted_high` watermark.
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

    /// Cache-only fetch (no disk).  Used by the commit walk: a hit yields the
    /// block by value; a miss means it was spilled to disk.
    pub fn cache_get(
        &self,
        h: &DataBlockHash<Tx>,
    ) -> Option<DataBlock<Tx>> {
        if *h == self.genesis_hash {
            return Some(self.genesis.clone());
        }
        self.cache.get(h).map(|(b, _)| b.clone())
    }

    /// Fetch a full block: cache → RocksDB.  Returns `Ok(None)` only for a
    /// hash never admitted locally.
    pub async fn get(
        &mut self,
        h: &DataBlockHash<Tx>,
    ) -> Result<Option<DataBlock<Tx>>> {
        if *h == self.genesis_hash {
            return Ok(Some(self.genesis.clone()));
        }
        if !self.meta.contains_key(h) {
            return Ok(None);
        }
        if let Some((b, _)) = self.cache.get(h) {
            return Ok(Some(b.clone()));
        }
        // Not in cache: spilled to disk or dropped after emission.
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
    /// Write-back — does NOT touch disk on the hot path.
    pub async fn insert(
        &mut self,
        block: DataBlock<Tx>,
    ) -> Result<()> {
        let h = block.hash().clone();
        self.meta
            .entry(h.clone())
            .or_insert_with(|| DataBlockMeta::of(&block, h.clone()));
        // Approximate byte size from payload length; exact serialization cost
        // is not needed for a memory budget.
        let size = (block.envelope.payload.len() + 1) * 64;
        self.cache_put(h, block, size);
        Ok(())
    }

    /// Insert into the byte-bounded cache, evicting oldest blocks until within
    /// `cap_bytes`.  An evicted block is **dropped for free** if already
    /// emitted (`height <= emitted_high`) and **spilled to disk**
    /// otherwise.
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
                    // Not yet emitted — spill to disk so walk_range can reload.
                    if let Ok(bytes) = bincode::serialize(&evicted) {
                        let _ = self.write_tx.send((old.to_vec(), bytes));
                    }
                }
                // else: already emitted — drop; nobody needs this payload.
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
        let path = std::env::temp_dir().join(format!("hera-dbdb-test-{nanos}-{n}.db"));
        Storage::new(path.to_str().unwrap()).expect("open rocksdb")
    }

    fn block(
        height: u64,
        parent: DataBlockHash<u32>,
    ) -> DataBlock<u32> {
        use crate::types::DataBlockSig;
        use std::marker::PhantomData;
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
        assert!(db.contains(&gh));
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
        let gg = db.get(g.hash()).await.unwrap().expect("genesis present");
        assert!(gg.is_genesis());
    }

    #[tokio::test]
    async fn missing_hash_returns_none() {
        let mut db = DataBlockDB::<u32>::new(temp_store());
        let phantom = block(99, DataBlockHash::<u32>::EMPTY_HASH);
        assert!(!db.contains(phantom.hash()));
        assert!(db.get(phantom.hash()).await.unwrap().is_none());
    }
}
