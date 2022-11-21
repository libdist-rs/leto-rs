use mempool::BatchHash;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Block<Transaction> {
    tx_hash: BatchHash<Transaction>,
}

impl<Transaction> Block<Transaction> {
    pub fn new(tx_hash: BatchHash<Transaction>) -> Self {
        Self { tx_hash }
    }
}
