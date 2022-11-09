use std::{
    fmt::{Display, Formatter},
    marker::PhantomData,
    ops::{Deref, DerefMut},
};

use crypto::hash::ser_and_hash;
use serde::{Deserialize, Serialize};

#[derive(Hash, PartialEq, Default, Eq, Clone, Ord, PartialOrd, Deserialize, Serialize)]
pub struct Hash<T> {
    pub(crate) hash: crypto::hash::Hash,
    _x: PhantomData<T>,
}

impl<T> Display for Hash<T> {
    fn fmt(
        &self,
        f: &mut Formatter<'_>,
    ) -> std::fmt::Result {
        write!(f, "{}", base64::encode(&self.hash))
    }
}

impl<T> std::fmt::Debug for Hash<T> {
    fn fmt(
        &self,
        f: &mut Formatter<'_>,
    ) -> std::fmt::Result {
        write!(f, "{}", base64::encode(&self.hash))
    }
}

impl<T> From<&T> for Hash<T>
where
    T: Serialize,
{
    /// Convert any serializable into hash
    ///
    /// Note: Uses bincode for serialization
    fn from(data: &T) -> Self {
        let inner_hash = ser_and_hash(data);
        Self::from_raw_hash(inner_hash)
    }
}

impl<T> Deref for Hash<T> {
    type Target = crypto::hash::Hash;

    fn deref(&self) -> &Self::Target {
        &self.hash
    }
}

impl<T> DerefMut for Hash<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.hash
    }
}

impl<T> Hash<T> {
    /// Converts a raw hash into a type associated hash
    pub fn from_raw_hash(raw: [u8; 32]) -> Self {
        Self {
            hash: raw,
            _x: PhantomData,
        }
    }
}
