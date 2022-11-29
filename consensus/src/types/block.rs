use mempool::BatchHash;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Block<Tx> {
    batch_hash: BatchHash<Tx>,
}

impl<Tx> Block<Tx> {
    pub fn new(batch_hash: BatchHash<Tx>) -> Self { Self { batch_hash } }
}
