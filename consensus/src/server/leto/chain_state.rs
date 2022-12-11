use super::Leto;
use crate::{
    types::{Block, Proposal, Signature},
    Id, Round,
};
use anyhow::{anyhow, Context, Result};
use crypto::hash::Hash;
use log::*;
use mempool::{Batch, BatchHash};
use serde::{de::DeserializeOwned, Serialize};
use std::fmt;
use storage::rocksdb::Storage;

pub struct ChainElement<Tx> {
    key: Hash<Block<Tx>>,
    value: (Proposal<Tx, Round>, Signature<Id, Proposal<Tx, Round>>),
}

impl<Tx> ChainElement<Tx>
where
    Tx: Serialize,
{
    pub fn new(
        prop: Proposal<Tx, Round>,
        auth: Signature<Id, Proposal<Tx, Round>>,
    ) -> Self {
        let key = Hash::ser_and_hash(prop.block());
        Self {
            key,
            value: (prop, auth),
        }
    }

    pub async fn write(
        &self,
        store: &mut Storage,
    ) -> Result<()> {
        let key_vec = self.key.to_vec();
        let value_vec = bincode::serialize(&self.value)?;
        Ok(store.write(key_vec, value_vec).await)
    }
}

pub struct ChainState<Tx> {
    store: Storage,
    highest_block_hash: Hash<Block<Tx>>,
}

impl<Tx> ChainState<Tx>
where
    Tx: DeserializeOwned,
{
    pub async fn get_batch(
        &mut self,
        batch_hash: BatchHash<Tx>,
    ) -> Result<Option<Batch<Tx>>> {
        match self.store.read(batch_hash.to_vec()).await? {
            Some(serialized) => bincode::deserialize::<Batch<Tx>>(&serialized)
                .map_err(anyhow::Error::new)
                .map(Some)
                .context("Failed to deserialize batch"),
            None => Ok(None),
        }
    }
}

impl<Tx> ChainState<Tx> {
    pub fn highest_block_hash(&self) -> Hash<Block<Tx>> {
        self.highest_block_hash.clone()
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

    /// Update the highest known chain
    pub async fn update_highest_chain(
        &mut self,
        prop: Proposal<Tx, Round>,
        auth: Signature<Id, Proposal<Tx, Round>>,
    ) -> Result<()> {
        // Write proposal and signature to the disk
        let chain_element = ChainElement::new(prop, auth);
        chain_element.write(&mut self.store).await?;

        // Update highest state
        self.highest_block_hash = chain_element.key;

        Ok(())
    }

    pub async fn write_batch(
        &mut self,
        batch: Batch<Tx>,
    ) -> Result<()> {
        let value = bincode::serialize(&batch)
            .map_err(anyhow::Error::new)
            .context("Failed to write batch")?;
        let key = Hash::<Batch<Tx>>::do_hash(&value);

        // Write to DB
        self.store.write(key.to_vec(), value).await;

        Ok(())
    }
}

impl<Tx> ChainState<Tx>
where
    Tx: Serialize + fmt::Debug,
{
    pub async fn genesis_setup(&mut self) -> Result<()> {
        let serialized_genesis = bincode::serialize(&Leto::<Tx>::GENESIS_BLOCK)?;
        trace!(
            "Writing genesis block: {:?} with hash: {:?} and serialized form: {:?}",
            Leto::<Tx>::GENESIS_BLOCK,
            self.highest_block_hash,
            serialized_genesis,
        );
        self.store
            .write(self.highest_block_hash.to_vec(), serialized_genesis.clone())
            .await;
        // let (sender, receiver) = oneshot::channel();
        let res = self
            .store
            .notify_read(self.highest_block_hash.to_vec())
            .await?;
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
        let result = self.store.read(parent_hash.to_vec()).await?;
        trace!("Got {:?} from reading parent hash", result);
        if let Some(raw) = result {
            return Ok(Some(
                bincode::deserialize(&raw).map_err(anyhow::Error::new)?,
            ));
        }
        Ok(None)
    }
}
