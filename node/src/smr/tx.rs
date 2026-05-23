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

    /// Hera self-load convention: the binary's make_tx writes a u128 little-
    /// endian timestamp at payload bytes 16..32. We decode it back here so the
    /// Hera commit loop can compute end-to-end latency without coupling the
    /// generic consensus crate to SimpleData.
    fn hera_timestamp_ns(&self) -> Option<u128> {
        let bytes = bincode::serialize(&self.data).ok()?;
        // SimpleData is `{ tx: Vec<u8> }`. bincode serializes Vec<u8> as
        // length-prefix (8 bytes for usize on 64-bit) + raw bytes.  Skip the
        // length prefix to land at the actual payload bytes.
        let payload = bytes.get(8..)?;
        if payload.len() < 32 {
            return None;
        }
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&payload[16..32]);
        Some(u128::from_le_bytes(buf))
    }
}
