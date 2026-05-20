use base64::{engine::general_purpose, Engine as _};
use consensus::{types::Transaction, Id};
use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug, Display};

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct SimpleTx<Data> {
    pub data: Data,
    /// Client that originated this tx.
    pub source: Id,
    /// Per-client monotonically increasing sequence number.
    pub nonce: u64,
    /// Extra data for benchmark sampling (sample flag + sample_id only).
    pub extra: Vec<u8>,
}

impl<Data> Debug for SimpleTx<Data>
where
    Data: Debug,
{
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        let encoded = general_purpose::STANDARD.encode(&self.extra);
        write!(
            f,
            "Tx [{:?}, src={}, n={}, {}]",
            self.data, self.source, self.nonce, &encoded
        )
    }
}

impl<Data> Display for SimpleTx<Data>
where
    Data: Debug,
{
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        let encoded = general_purpose::STANDARD.encode(&self.extra);
        write!(
            f,
            "Tx [{:?}, src={}, n={}, {}]",
            self.data, self.source, self.nonce, &encoded
        )
    }
}

impl<Data> net_common::Message for SimpleTx<Data>
where
    Self: serde::de::DeserializeOwned,
{
    type DeserializationError = Box<bincode::ErrorKind>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::DeserializationError> {
        bincode::deserialize(bytes)
    }
}

impl<Data> Transaction for SimpleTx<Data>
where
    Data: crate::Data,
{
    fn client_id(&self) -> Id {
        self.source
    }

    fn nonce(&self) -> u64 {
        self.nonce
    }

    fn is_sample(&self) -> bool {
        #[cfg(feature = "benchmark")]
        {
            use crate::ExtraData;
            let e: ExtraData = bincode::deserialize(&self.extra).expect("Failed to deserialize");
            e.sample
        }
        #[cfg(not(feature = "benchmark"))]
        {
            false
        }
    }

    fn get_id(&self) -> u64 {
        #[cfg(feature = "benchmark")]
        {
            use crate::ExtraData;
            let e: ExtraData = bincode::deserialize(&self.extra).expect("Failed to deserialize");
            e.sample_id
        }
        #[cfg(not(feature = "benchmark"))]
        {
            0
        }
    }
}
