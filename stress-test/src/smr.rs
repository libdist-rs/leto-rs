use base64::{engine::general_purpose, Engine as _};
use consensus::client::MockTx;
use consensus::types::Transaction;
use consensus::Id;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug, Display};

// --- Data trait + SimpleData (from node/src/smr/data.rs) ---

pub trait Data:
    Serialize + serde::de::DeserializeOwned + Send + Sync + std::fmt::Debug + Clone + Unpin + 'static
{
    fn with_payload(data: &[u8]) -> Self;
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct SimpleData {
    tx: Vec<u8>,
}

impl Data for SimpleData {
    fn with_payload(data: &[u8]) -> Self {
        Self { tx: data.to_vec() }
    }
}

impl Debug for SimpleData {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        let encoded = general_purpose::STANDARD.encode(&self.tx);
        write!(f, "{}", encoded)
    }
}

impl Display for SimpleData {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        let encoded = general_purpose::STANDARD.encode(&self.tx);
        write!(f, "{}", &encoded)
    }
}

// --- SimpleTx (from node/src/smr/tx.rs) ---

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct SimpleTx<D> {
    pub data: D,
    /// Client that originated this tx.
    pub source: Id,
    /// Per-client monotonically increasing sequence number.
    pub nonce: u64,
    /// Extra data for benchmark sampling (sample flag + sample_id only).
    pub extra: Vec<u8>,
}

impl<D: Debug> Debug for SimpleTx<D> {
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

impl<D: Debug> Display for SimpleTx<D> {
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

impl<D> net_common::Message for SimpleTx<D>
where
    Self: serde::de::DeserializeOwned,
{
    type DeserializationError = Box<bincode::ErrorKind>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::DeserializationError> {
        bincode::deserialize(bytes)
    }
}

impl<D: Data> Transaction for SimpleTx<D> {
    fn client_id(&self) -> Id {
        self.source
    }

    fn nonce(&self) -> u64 {
        self.nonce
    }

    fn is_sample(&self) -> bool {
        #[cfg(feature = "benchmark")]
        {
            let extra_data: ExtraData =
                bincode::deserialize(&self.extra).expect("Failed to deserialize");
            extra_data.sample
        }
        #[cfg(not(feature = "benchmark"))]
        {
            false
        }
    }

    fn get_id(&self) -> u64 {
        #[cfg(feature = "benchmark")]
        {
            let extra_data: ExtraData =
                bincode::deserialize(&self.extra).expect("Failed to deserialize");
            extra_data.sample_id
        }
        #[cfg(not(feature = "benchmark"))]
        {
            0
        }
    }
}

// --- ExtraData + MockTx impl (from node/src/smr/mocker.rs) ---

/// Benchmark-only metadata embedded in `extra`.  Only sample flag and
/// sample_id remain here; `source` and `nonce` are now top-level fields.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ExtraData {
    pub sample: bool,
    pub sample_id: u64,
}

impl ExtraData {
    pub fn new(
        sample: bool,
        sample_id: u64,
    ) -> Self {
        Self { sample, sample_id }
    }
}

impl<D: Data> MockTx for SimpleTx<D> {
    // Wire layout (bincode):
    //   data field    = tx_size - HEADER_SIZE bytes of payload
    //   source (Id = usize): 8 B
    //   nonce  (u64):        8 B
    //   extra  (Vec<u8>):    4 B len prefix + ExtraData contents
    // HEADER_SIZE covers the fixed-size non-data portions so that the
    // caller can size `data` to hit exactly `tx_size` on the wire.
    // Id (usize) = 8 B, nonce (u64) = 8 B → 16 B additional over the
    // previous layout; old HEADER_SIZE was 33 B, new = 49 B.
    const HEADER_SIZE: usize = 49;

    fn mock_transaction(
        tx_id: usize,
        client_id: Id,
        tx_size: usize,
        sample: bool,
        sample_id: u64,
    ) -> Self {
        let payload_size = tx_size.saturating_sub(Self::HEADER_SIZE);
        let data = D::with_payload(&vec![0; payload_size]);
        let extra_data = ExtraData::new(sample, sample_id);
        SimpleTx {
            data,
            source: client_id,
            nonce: tx_id as u64,
            extra: bincode::serialize(&extra_data).unwrap(),
        }
    }
}
