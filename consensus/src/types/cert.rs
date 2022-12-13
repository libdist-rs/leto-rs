use super::Signature;
use anyhow::Result;
use crypto::hash::Hash;
use crypto::PublicKey;
use fnv::{FnvHashMap, FnvHashSet};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Certificate<Id, T> {
    raw_sigs: Vec<Signature<Id, T>>,
}

impl<Id, T> Certificate<Id, T>
where
    Id: Eq + std::hash::Hash,
{
    pub fn verify(
        &self,
        msg_hash: &Hash<T>,
        pk_map: &FnvHashMap<Id, PublicKey>,
    ) -> Result<()> {
        // Check individual signatures
        for sig in &self.raw_sigs {
            let pk = pk_map
                .get(&sig.id)
                .ok_or(anyhow::Error::msg("Unknown Id"))?;
            sig.verify_without_id_check(msg_hash, pk)?;
        }
        Ok(())
    }
}

impl<Id, T> Certificate<Id, T> {
    pub fn len(&self) -> usize {
        self.raw_sigs.len()
    }

    /// Creates an empty certificate to add signatures to
    pub fn empty() -> Self {
        Self {
            raw_sigs: Vec::new(),
        }
    }

    /// Adds a signature into the certificate
    pub fn add(&mut self, sig: Signature<Id, T>) {
        self.raw_sigs.push(sig);
    }
}

impl<Id, T> Certificate<Id, T>
where
    Id: Eq + std::hash::Hash,
{
    /// Returns the number of unique signatures on the message
    pub fn unique_len(&self) -> usize {
        let mut set = FnvHashSet::default();
        for sig in &self.raw_sigs {
            set.insert(&sig.id);
        }
        set.len()
    }
}

impl<Id, Inner> Extend<Signature<Id, Inner>> for Certificate<Id, Inner> {
    fn extend<T: IntoIterator<Item = Signature<Id, Inner>>>(
        &mut self,
        iter: T,
    ) {
        self.raw_sigs.extend(iter);
    }
}

impl<Id, T> From<Signature<Id, T>> for Certificate<Id, T> {
    fn from(sig: Signature<Id, T>) -> Self {
        Self {
            raw_sigs: vec![sig],
        }
    }
}

impl<Id, T> IntoIterator for Certificate<Id, T> {
    type Item = Signature<Id, T>;

    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.raw_sigs.into_iter()
    }
}
