use crypto::hash::Hash;
use mempool::BatchHash;
use serde::{Deserialize, Serialize};

use super::Element;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Block<Id, Tx, Round> {
    batch_hash: BatchHash<Tx>,
    prev_hash: Hash<Element<Id, Tx, Round>>,
}

impl<Id, Tx, Round> Block<Id, Tx, Round> {
    pub const fn new(
        batch_hash: BatchHash<Tx>,
        prev_hash: Hash<Element<Id, Tx, Round>>,
    ) -> Self {
        Self {
            batch_hash,
            prev_hash,
        }
    }

    pub fn parent_hash(&self) -> Hash<Element<Id, Tx, Round>> {
        self.prev_hash.clone()
    }

    pub fn batch_hash(&self) -> &BatchHash<Tx> {
        &self.batch_hash
    } 
}
