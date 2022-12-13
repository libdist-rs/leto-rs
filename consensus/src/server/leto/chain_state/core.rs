use crate::{
    types::{Element, Proposal, Signature, Transaction},
    Id, Round,
};
use anyhow::{Context, Result};
use crypto::hash::Hash;
use mempool::{Batch, BatchHash};
use serde::{de::DeserializeOwned, Serialize};
use std::{fmt, sync::Arc};
use storage::rocksdb::Storage;

pub struct ChainState<Tx> {
    pub(super) store: Storage,
    pub(super) highest_chain_hash: Hash<Element<Id, Tx, Round>>,
    pub(super) highest_chain_element: Arc<Element<Id, Tx, Round>>,
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

impl<Tx> ChainState<Tx>
where
    Tx: Serialize,
{
    /// Returns the chainstate using the genesis block
    pub fn new(store: Storage) -> Self {
        let genesis_element = Element::genesis(0.into(), 0.into());
        let genesis_hash = Hash::ser_and_hash(&genesis_element);
        Self {
            highest_chain_hash: genesis_hash,
            highest_chain_element: Arc::new(genesis_element),
            store,
        }
    }

    /// Update the highest known chain
    pub async fn update_highest_chain(
        &mut self,
        prop: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch: Batch<Tx>,
    ) -> Result<()>
    where
        Tx: Transaction,
    {
        // Write chain element to the disk
        let chain_element = Arc::new(Element::new(prop, auth, batch));
        self.write_element(chain_element.clone()).await?;

        // Update highest state
        self.highest_chain_hash = Hash::ser_and_hash(&chain_element);
        self.highest_chain_element = chain_element;

        Ok(())
    }

    pub async fn write_element(
        &mut self,
        chain_element: Arc<Element<Id, Tx, Round>>,
    ) -> Result<()> {
        // Index chain element hash -> chain_element (used in blocks in the prev_hash
        // field)
        let chain_element_serialized = bincode::serialize(chain_element.as_ref())?;
        let chain_element_hash = Hash::<Element<Id, Tx, Round>>::do_hash(&chain_element_serialized);
        self.store
            .write(chain_element_hash.to_vec(), chain_element_serialized)
            .await;

        // Index batch_hash -> Batch (used when relay messages are received)
        let batch_serialized = bincode::serialize(&chain_element.batch)?;
        let batch_hash = Hash::<Batch<Tx>>::do_hash(&batch_serialized);
        self.store
            .write(batch_hash.to_vec(), batch_serialized)
            .await;

        // Done
        Ok(())
    }
}

impl<Tx> ChainState<Tx>
where
    Tx: Serialize + fmt::Debug,
{
    pub async fn genesis_setup(&mut self) -> Result<()>
    where
        Tx: Transaction,
    {
        // Write the genesis elements
        let serialized = bincode::serialize(self.highest_chain_element.as_ref())?;
        self.write_element(self.highest_chain_element.clone())
            .await?;
        let res = self
            .store
            .notify_read(self.highest_chain_hash.to_vec())
            .await?;
        assert_eq!(res, serialized);
        Ok(())
    }
}
