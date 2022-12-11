use crypto::hash::Hash;
use mempool::BatchHash;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Block<Tx> {
    batch_hash: BatchHash<Tx>,
    prev_hash: Hash<Block<Tx>>,
}

impl<Tx> Block<Tx> {
    pub const fn new(
        batch_hash: BatchHash<Tx>,
        prev_hash: Hash<Self>,
    ) -> Self {
        Self {
            batch_hash,
            prev_hash,
        }
    }

    pub(crate) fn parent_hash(&self) -> Hash<Block<Tx>> {
        self.prev_hash.clone()
    }
}
