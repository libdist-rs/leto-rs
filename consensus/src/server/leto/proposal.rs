use crate::types::{Block, Proposal};

use super::Leto;
use anyhow::{Result, anyhow};
use mempool::BatchHash;

impl<Tx> Leto<Tx> {
    pub fn handle_new_batch(&mut self, batch_hash: BatchHash<Tx>) -> Result<()> {
        let block = Block::new(batch_hash);
        let prop = Proposal::new(
            block, 
            self.round_ctx.round()
        );
        // let auth = self.c
        todo!();
    }
}