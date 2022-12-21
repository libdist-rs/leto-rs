use crate::{
    types::{Certificate, Element, Proposal, Signature, Transaction},
    Id, Round, start_id,
};
use anyhow::Result;
use crypto::hash::Hash;
use fnv::FnvHashMap;
use mempool::{Batch, BatchHash};
use serde::{de::DeserializeOwned, Serialize};
use std::{fmt, sync::Arc};
use storage::rocksdb::Storage;

use super::ChainDB;

pub struct ChainState<Tx> {
    pub(super) db: ChainDB,
    pub(super) highest_chain_hash: Hash<Element<Id, Tx, Round>>,
    pub(super) highest_chain_element: Arc<Element<Id, Tx, Round>>,
    pub(super) qc_map: FnvHashMap<Round, Certificate<Id, Round>>,
}

impl<Tx> ChainState<Tx>
where
    Tx: DeserializeOwned,
{
    pub async fn get_batch(
        &mut self,
        batch_hash: BatchHash<Tx>,
    ) -> Result<Option<Batch<Tx>>> 
    where 
        Tx: Transaction,
    {
        self.db.read(batch_hash).await
    }
}

impl<Tx> ChainState<Tx>
where
    Tx: Transaction,
{
    /// Returns the chainstate using the genesis block
    pub fn new(store: Storage) -> Self {
        let genesis_element = Element::genesis(start_id());
        let genesis_hash = Hash::ser_and_hash(&genesis_element);
        Self {
            highest_chain_hash: genesis_hash,
            highest_chain_element: Arc::new(genesis_element),
            db: ChainDB::new(store),
            qc_map: FnvHashMap::default(),
        }
    }

    /// Update the highest known chain
    pub async fn update_highest_chain(
        &mut self,
        prop: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch: Batch<Tx>,
    ) -> Result<()>
    {
        // Write chain element to the disk
        let chain_element = Arc::new(Element::new(prop, auth, batch));
        self.write_element(chain_element.clone()).await?;

        // Update highest state
        self.highest_chain_hash = Hash::ser_and_hash(&chain_element);
        self.highest_chain_element = chain_element;

        Ok(())
    }

    pub fn add_qc(
        &mut self,
        blame_round: Round,
        qc: Certificate<Id, Round>,
    ) {
        self.qc_map.insert(blame_round, qc);
    }

    pub fn get_qc(
        &mut self,
        round: &Round,
    ) -> Option<Certificate<Id, Round>> {
        self.qc_map.get(round).cloned()
    }

    pub async fn write_element(
        &mut self,
        chain_element: Arc<Element<Id, Tx, Round>>,
    ) -> Result<()> {
        let element: Element<Id, Tx, Round> = chain_element.as_ref().clone();
        self.db.write::<Element<Id, Tx, Round>>(element).await
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
        self.write_element(self.highest_chain_element.clone())
            .await?;
        self
            .db
            .notify_read(self.highest_chain_hash.clone())
            .await?;
        Ok(())
    }
}
