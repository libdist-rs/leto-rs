//! Content-addressed RocksDB wrapper shared across protocols.
//!
//! `ChainDB` maps a typed `Hash<T>` to the bincode-serialized `T` in the
//! underlying `storage::rocksdb::Storage`.  It is protocol-agnostic: Leto uses
//! it for its chain/batch store, and Zeus's `DataBlockDB` uses it to spill
//! data-block payloads to disk.  `notify_read` blocks until a key is written,
//! which is how a node waits for a batch/block another peer will deliver.
use anyhow::{Context, Result};
use crypto::hash::Hash;
use serde::{de::DeserializeOwned, Serialize};
use storage::rocksdb::Storage;

#[derive(Clone)]
pub struct ChainDB {
    store: Storage,
}

impl ChainDB {
    pub fn new(store: Storage) -> Self {
        Self { store }
    }

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
                bincode::deserialize::<T>(&serialized).map_err(anyhow::Error::new)
            })
    }

    /// Read a value of type `V` stored under a key hash of a (possibly
    /// different) type `K`.  `read` ties the key-hash type to the value type;
    /// this variant decouples them, which Zeus's `DataBlockDB` needs because it
    /// stores a full `DataBlock` keyed by its envelope hash
    /// (`Hash<DataBlockEnvelope>`).
    pub async fn read_as<K, V>(
        &mut self,
        hash: &Hash<K>,
    ) -> Result<Option<V>>
    where
        V: DeserializeOwned,
    {
        match self.store.read(hash.to_vec()).await? {
            Some(serialized) => bincode::deserialize::<V>(&serialized)
                .map(Some)
                .context("Failed to deserialize value"),
            None => Ok(None),
        }
    }

    /// Like `read_as`, but blocks until the key is written if it is not yet
    /// present (delegates to `Storage::notify_read`). Used when a value is
    /// known to have been admitted and handed to an async writer, but may not
    /// have hit disk yet — the caller must only invoke this for keys that will
    /// be written, otherwise it parks forever.
    pub async fn notify_read_as<K, V>(
        &mut self,
        hash: &Hash<K>,
    ) -> Result<V>
    where
        V: DeserializeOwned,
    {
        let serialized = self.store.notify_read(hash.to_vec()).await?;
        bincode::deserialize::<V>(&serialized).context("Failed to deserialize value")
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
        self.store.write(hash.to_vec(), serialized).await;
        Ok(())
    }
}
