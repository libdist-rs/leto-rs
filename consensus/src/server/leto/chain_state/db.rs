use serde::{Serialize, de::DeserializeOwned};
use storage::rocksdb::Storage;
use anyhow::{Result, Context};
use crypto::hash::Hash;

#[derive(Clone)]
pub struct ChainDB {
    store: Storage,
}

impl ChainDB {
    pub fn new(store: Storage) -> Self { Self { store } }

    pub async fn read<T>(
        &mut self,
        hash: Hash<T>,
    ) -> Result<Option<T>>
    where 
        T: DeserializeOwned,
    {
        match self.store.read(hash.to_vec()).await? {
            Some(serialized) => bincode::deserialize::<T>(&serialized)
                .map(Some)
                .context("Failed to deserialize batch"),
            None => Ok(None),
        }
    }

    pub async fn notify_read<T>(
        &mut self,
        hash: Hash<T>,
    ) -> Result<T>
    where 
        T: Serialize + DeserializeOwned,
    {
        self.store
            .notify_read(hash.to_vec())
            .await
            .and_then(|serialized| {
                bincode::deserialize::<T>(&serialized)
                    .map_err(anyhow::Error::new)
            })
    }

    pub async fn write<T>(
        &mut self,
        val: T,
    ) -> Result<()>
    where 
        T: Serialize,
    {
        let serialized = bincode::serialize(&val)?;
        let val_hash: Hash<T> = Hash::do_hash(&serialized);
        self.store.write(val_hash.to_vec(), serialized).await;
        Ok(())
    }

    pub async fn write_serialized<T>(
        &mut self,
        hash: Hash<T>,
        serialized: Vec<u8>,
    ) -> Result<()>
    where 
        T: Serialize,
    {
        self.store.write(
            hash.to_vec(), 
            serialized,
        ).await;
        Ok(())
    }
}

