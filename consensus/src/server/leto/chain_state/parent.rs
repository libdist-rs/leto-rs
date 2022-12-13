use std::sync::Arc;

use super::{ChainState, DBData};
use crate::{
    types::{Element, Proposal},
    Id, Round,
};
use anyhow::Result;
use crypto::hash::Hash;
use serde::de::DeserializeOwned;

pub type ParentData<Tx> = DBData<Proposal<Id, Tx, Round>>;

impl<Tx> ChainState<Tx>
where
    Tx: DeserializeOwned,
{
    pub fn highest_hash(&self) -> Hash<Element<Id, Tx, Round>> {
        self.highest_chain_hash.clone()
    }

    pub fn highest_chain(&self) -> Arc<Element<Id, Tx, Round>> {
        self.highest_chain_element.clone()
    }

    pub async fn get_element(
        &mut self,
        element_hash: Hash<Element<Id, Tx, Round>>,
    ) -> Result<Option<Element<Id, Tx, Round>>> {
        match self.store.read(element_hash.to_vec()).await? {
            None => Ok(None),
            Some(raw) => {
                let element = bincode::deserialize::<Element<Id, Tx, Round>>(&raw)
                    .map_err(anyhow::Error::new)?;
                Ok(Some(element))
            }
        }
    }
}
