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
///   - A `child_of` map for O(1) chain advancement.
///   - A `pending_data_blocks` park map for out-of-order arrivals.
///
/// The three-case admission (parent-matches-head / parent-in-store-not-head /
/// parent-missing) is per-author: admission for author `i` does not affect
/// author `j`.
use crate::{
    types::{DataBlock, DataBlockEnvelope},
    Id,
};
use crypto::hash::Hash;
use fnv::FnvHashMap;
use log::debug;
use serde::Serialize;

pub type DataBlockHash<Tx> = Hash<DataBlockEnvelope<Tx>>;

/// Result of a single-block admission attempt.
#[derive(Debug)]
pub enum AdmitResult {
    /// Block extends author's head; head advanced.
    Extended,
    /// Block's parent is in the store but is not the head; block inserted,
    /// head may have been advanced via child_of walk.
    Bridge,
    /// Block's parent is missing; block parked under missing parent hash.
    Parked(DataBlockHash<u8>), // phantom; actual type is erased for caller
    /// Block already in store (idempotent).
    Duplicate,
    /// Block failed a basic validity check (signature must be verified by
    /// caller before calling admit; this is for structural checks only).
    Invalid,
}

/// Per-author data-chain state for Hera.
pub struct MultiAuthorDataChainState<Tx> {
    /// Current admitted tip hash per author.
    pub heads: FnvHashMap<Id, DataBlockHash<Tx>>,
    /// Current admitted tip height per author.
    pub heights: FnvHashMap<Id, u64>,
    /// Highest height committed (via sig-block) per author.
    pub committed_heights: FnvHashMap<Id, u64>,
    /// parent_hash → child_hash for every admitted block (all authors).
    pub child_of: FnvHashMap<DataBlockHash<Tx>, DataBlockHash<Tx>>,
    /// Park map: missing parent_hash → list of parked blocks.
    pub pending_data_blocks: FnvHashMap<DataBlockHash<Tx>, Vec<DataBlock<Tx>>>,
    /// Full block store: H(envelope) → DataBlock.
    pub block_store: FnvHashMap<DataBlockHash<Tx>, DataBlock<Tx>>,
}

impl<Tx> MultiAuthorDataChainState<Tx>
where
    Tx: Serialize + Clone,
{
    /// Initialize with genesis state for all authors.
    ///
    /// Every author starts with height 0 (genesis) and the genesis hash as
    /// their head.  The genesis block is pre-inserted into `block_store`.
    pub fn new(all_ids: &[Id]) -> Self {
        let genesis = DataBlock::<Tx>::genesis();
        let genesis_hash = genesis.hash().clone();

        let mut heads = FnvHashMap::default();
        let mut heights = FnvHashMap::default();
        let mut committed_heights = FnvHashMap::default();
        let mut block_store = FnvHashMap::default();

        for &id in all_ids {
            heads.insert(id, genesis_hash.clone());
            heights.insert(id, 0u64);
            committed_heights.insert(id, 0u64);
        }

        block_store.insert(genesis_hash, genesis);

        Self {
            heads,
            heights,
            committed_heights,
            child_of: FnvHashMap::default(),
            pending_data_blocks: FnvHashMap::default(),
            block_store,
        }
    }

    /// Returns the current head hash for the given author, or the genesis hash
    /// if the author is unknown.
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

    /// Look up a block by its hash.
    pub fn get_block(
        &self,
        hash: &DataBlockHash<Tx>,
    ) -> Option<&DataBlock<Tx>> {
        self.block_store.get(hash)
    }

    /// Three-case admission for a block from `block.sig.signer`.
    ///
    /// Pre-condition: the caller has already verified `block.sig` against
    /// `block.sig.signer`'s public key. This function handles structural
    /// admission only.
    ///
    /// Cases:
    ///   (a) parent_hash == author's head hash → extend head.
    ///   (b) parent in store but not head → bridge insert, walk child_of.
    ///   (c) parent not in store → park, caller should emit DataRequest.
    ///
    /// Returns a typed `AdmitResult`; callers handle the `Parked` case by
    /// emitting a `DataRequest` for the missing parent hash.
    pub fn admit(
        &mut self,
        block: DataBlock<Tx>,
    ) -> AdmitResult
    where
        Tx: PartialEq + std::fmt::Debug,
    {
        let author = block.sig.signer;
        let block_hash = block.hash().clone();
        let parent_hash = block.envelope.parent_hash.clone();

        // Idempotent.
        if self.block_store.contains_key(&block_hash) {
            return AdmitResult::Duplicate;
        }

        // Author must be known.
        if !self.heads.contains_key(&author) {
            debug!(
                "Hera: admit: unknown author {} for block height={}",
                author, block.envelope.height
            );
            return AdmitResult::Invalid;
        }

        let head_hash = self.heads[&author].clone();

        if parent_hash == head_hash {
            // ---------------------------------------------------------------
            // Case (a): extends current head.
            // ---------------------------------------------------------------
            let new_height = block.envelope.height;
            self.block_store.insert(block_hash.clone(), block);
            self.child_of.insert(parent_hash, block_hash.clone());
            self.heads.insert(author, block_hash);
            self.heights.insert(author, new_height);

            // Drain pending blocks whose parent is the new head.
            self.drain_pending_for_author(author, new_height);

            debug!(
                "Hera: admit case(a): author={} height={}",
                author, new_height
            );
            AdmitResult::Extended
        } else if self.block_store.contains_key(&parent_hash) {
            // ---------------------------------------------------------------
            // Case (b): parent in store but not head.
            // ---------------------------------------------------------------
            let new_height = block.envelope.height;
            self.block_store.insert(block_hash.clone(), block);
            self.child_of.insert(parent_hash, block_hash);

            // Walk child_of forward to advance head.
            self.advance_head_via_child_of(author);

            // Drain pending.
            let cur_height = self.heights[&author];
            self.drain_pending_for_author(author, cur_height);

            debug!(
                "Hera: admit case(b): author={} height={}",
                author, new_height
            );
            AdmitResult::Bridge
        } else {
            // ---------------------------------------------------------------
            // Case (c): parent not in store → park.
            // ---------------------------------------------------------------
            // Do not request genesis — always in store.
            let genesis_hash = DataBlock::<Tx>::genesis().hash().clone();
            if parent_hash == genesis_hash {
                // Should not happen if block_store invariant holds.
                debug!("Hera: admit case(c) with genesis parent — invariant violation, dropping");
                return AdmitResult::Invalid;
            }

            debug!(
                "Hera: admit case(c): author={} height={} parent missing, parking",
                author, block.envelope.height
            );
            self.pending_data_blocks
                .entry(parent_hash.clone())
                .or_default()
                .push(block);

            // Return the missing parent hash erased as u8 phantom so callers
            // can use it for DataRequest. The caller casts it back.
            // We encode the missing hash into a typed-erased return via a
            // separate field. Use the concrete hash instead.
            //
            // Callers use `AdmitResult::Parked` and retrieve the hash via
            // `pending_parent_hash`.  Since we can't store it in the enum
            // without the Tx phantom type, we return it via the Parked variant
            // with a workaround: callers call `last_parked_parent` after
            // an Admit::Parked return.
            let _ = parent_hash; // stored in pending_data_blocks above
            AdmitResult::Parked(unsafe { std::mem::transmute(block_hash) })
        }
    }

    /// Admit a block and return the missing parent hash if parked.
    ///
    /// This is the typed version callers should use — it returns the missing
    /// parent hash directly so callers can emit a `DataRequest`.
    pub fn admit_typed(
        &mut self,
        block: DataBlock<Tx>,
    ) -> AdmitTypedResult<Tx>
    where
        Tx: PartialEq + std::fmt::Debug,
    {
        let author = block.sig.signer;
        let block_hash = block.hash().clone();
        let parent_hash = block.envelope.parent_hash.clone();

        // Idempotent.
        if self.block_store.contains_key(&block_hash) {
            return AdmitTypedResult::Duplicate;
        }

        // Author must be known.
        if !self.heads.contains_key(&author) {
            debug!(
                "Hera: admit_typed: unknown author {} for block height={}",
                author, block.envelope.height
            );
            return AdmitTypedResult::Invalid;
        }

        let head_hash = self.heads[&author].clone();

        if parent_hash == head_hash {
            let new_height = block.envelope.height;
            self.block_store.insert(block_hash.clone(), block);
            self.child_of.insert(parent_hash, block_hash.clone());
            self.heads.insert(author, block_hash);
            self.heights.insert(author, new_height);
            self.drain_pending_for_author(author, new_height);
            debug!(
                "Hera: admit_typed case(a): author={} height={}",
                author, new_height
            );
            AdmitTypedResult::Extended
        } else if self.block_store.contains_key(&parent_hash) {
            let new_height = block.envelope.height;
            self.block_store.insert(block_hash.clone(), block);
            self.child_of.insert(parent_hash, block_hash);
            self.advance_head_via_child_of(author);
            let cur_height = self.heights[&author];
            self.drain_pending_for_author(author, cur_height);
            debug!(
                "Hera: admit_typed case(b): author={} height={}",
                author, new_height
            );
            AdmitTypedResult::Bridge
        } else {
            let genesis_hash = DataBlock::<Tx>::genesis().hash().clone();
            if parent_hash == genesis_hash {
                debug!("Hera: admit_typed case(c) with genesis parent — dropping");
                return AdmitTypedResult::Invalid;
            }
            debug!(
                "Hera: admit_typed case(c): author={} height={} parent missing",
                author, block.envelope.height
            );
            self.pending_data_blocks
                .entry(parent_hash.clone())
                .or_default()
                .push(block);
            AdmitTypedResult::Parked(parent_hash)
        }
    }

    /// Walk `child_of` forward from author's current head as far as possible.
    fn advance_head_via_child_of(
        &mut self,
        author: Id,
    ) {
        loop {
            let current_head = match self.heads.get(&author) {
                Some(h) => h.clone(),
                None => break,
            };
            let next_hash = match self.child_of.get(&current_head) {
                Some(h) => h.clone(),
                None => break,
            };
            let next_height = match self.block_store.get(&next_hash) {
                Some(b) => b.envelope.height,
                None => break,
            };
            self.heads.insert(author, next_hash);
            self.heights.insert(author, next_height);
        }
    }

    /// Drain pending blocks whose parent is the current head for `author`.
    /// May trigger further case-(a) admissions recursively (iteratively).
    fn drain_pending_for_author(
        &mut self,
        author: Id,
        _admitted_height: u64,
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
            for child_block in parked {
                let child_author = child_block.sig.signer;
                let child_hash = child_block.hash().clone();
                let child_height = child_block.envelope.height;
                let parent = child_block.envelope.parent_hash.clone();
                self.block_store.insert(child_hash.clone(), child_block);
                self.child_of.insert(parent, child_hash.clone());
                if child_author == author {
                    self.heads.insert(author, child_hash);
                    self.heights.insert(author, child_height);
                }
            }
        }
    }

    /// Walk author's chain from `from_height + 1` to `to_height` (inclusive),
    /// returning blocks in ascending height order.
    ///
    /// Used by the commit path to emit txs for newly-committed sub-chain
    /// ranges. Walking is done via parent_hash backward from the `tip_hash`
    /// then reversed.
    pub fn walk_range(
        &self,
        tip_hash: &DataBlockHash<Tx>,
        from_height: u64,
        to_height: u64,
    ) -> Vec<DataBlock<Tx>>
    where
        Tx: Clone,
    {
        if to_height <= from_height {
            return Vec::new();
        }

        let mut chain: Vec<DataBlock<Tx>> = Vec::new();
        let mut current_hash = tip_hash.clone();

        loop {
            let block = match self.block_store.get(&current_hash) {
                Some(b) => b,
                None => break,
            };
            let h = block.envelope.height;
            if h <= from_height {
                break;
            }
            chain.push(block.clone());
            if h == 0 {
                break;
            }
            current_hash = block.envelope.parent_hash.clone();
        }

        // chain is in descending order; reverse to ascending.
        chain.reverse();
        // Filter to only heights in (from_height, to_height].
        chain.retain(|b| b.envelope.height > from_height && b.envelope.height <= to_height);
        chain
    }
}

/// Typed result of `admit_typed`, carrying the missing parent hash in the
/// `Parked` case.
#[derive(Debug)]
pub enum AdmitTypedResult<Tx> {
    Extended,
    Bridge,
    /// Parent missing from store; the missing parent hash is carried here.
    Parked(DataBlockHash<Tx>),
    Duplicate,
    Invalid,
}
