use super::Leto;
use anyhow::{Result, anyhow};
use mempool::BatchHash;

impl<Tx> Leto<Tx> {
    pub fn handle_new_batch(&mut self, batch_hash: BatchHash<Tx>) -> Result<()> {
        todo!();
    }
}