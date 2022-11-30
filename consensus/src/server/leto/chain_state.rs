use std::fmt;
use super::Leto;
use crate::types::Block;
use anyhow::Result;
use crypto::hash::Hash;
use log::*;
use serde::{de::DeserializeOwned, Serialize};
use storage::rocksdb::Storage;

pub struct ChainState<Tx> {
    store: Storage,
    highest_block_hash: Hash<Block<Tx>>,
}

impl<Tx> ChainState<Tx> {
    pub fn highest_block_hash(&self) -> Hash<Block<Tx>> {
        self.highest_block_hash.clone()
    }

    pub fn store(&mut self) -> &mut Storage {
        &mut self.store
    }
}

impl<Tx> ChainState<Tx>
where
    Tx: Serialize,
{
    /// Returns the chainstate using the genesis block
    pub fn new(store: Storage) -> Self {
        let genesis_hash = Hash::ser_and_hash(&Leto::<Tx>::GENESIS_BLOCK);
        Self {
            highest_block_hash: genesis_hash,
            store,
        }
    }

}

impl<Tx> ChainState<Tx>
where 
    Tx: Serialize + fmt::Debug,
{
    pub async fn genesis_setup(&mut self) -> Result<()> {
        let serialized_genesis = bincode::serialize(&Leto::<Tx>::GENESIS_BLOCK)?;
        trace!("Writing genesis block: {:?} with hash: {:?} and serialized form: {:?}", 
            Leto::<Tx>::GENESIS_BLOCK,
            self.highest_block_hash,
            serialized_genesis,
        );
        self.store
            .write(
                self.highest_block_hash.to_vec(),
                serialized_genesis.clone(),
            )
            .await;
        // let (sender, receiver) = oneshot::channel();
        let res = self.store.notify_read(self.highest_block_hash.to_vec()).await?;
        assert_eq!(res, serialized_genesis);
        Ok(())
    }
}

impl<Tx> ChainState<Tx>
where
    Tx: DeserializeOwned,
{
    pub async fn parent(
        &mut self,
        parent_hash: Hash<Block<Tx>>,
    ) -> Result<Option<Block<Tx>>> {
        let result = self.store
            .read(parent_hash.to_vec())
            .await?;
        trace!("Got {:?} from reading parent hash", result);
        if let Some(raw) = result {
            return Ok(Some(
                bincode::deserialize(&raw)
                    .map_err(anyhow::Error::new)?
            ));
        }
        Ok(None)
    }
}
