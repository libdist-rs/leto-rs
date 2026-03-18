use consensus::client::MockTx;
use consensus::types::Transaction;
use consensus::Id;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug, Display};
use base64::{Engine as _, engine::general_purpose};

// --- Data trait + SimpleData (from node/src/smr/data.rs) ---

pub trait Data: Serialize + serde::de::DeserializeOwned + Send + Sync + std::fmt::Debug + Clone + Unpin + 'static {
    fn with_payload(data: &[u8]) -> Self;
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SimpleData {
    tx: Vec<u8>,
}

impl Data for SimpleData {
    fn with_payload(data: &[u8]) -> Self {
        Self { tx: data.to_vec() }
    }
}

impl Debug for SimpleData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = general_purpose::STANDARD.encode(&self.tx);
        write!(f, "{}", encoded)
    }
}

impl Display for SimpleData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = general_purpose::STANDARD.encode(&self.tx);
        write!(f, "{}", &encoded)
    }
}

// --- SimpleTx (from node/src/smr/tx.rs) ---

#[derive(Serialize, Deserialize, Clone)]
pub struct SimpleTx<D> {
    pub data: D,
    pub extra: Vec<u8>,
}

impl<D: Debug> Debug for SimpleTx<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = general_purpose::STANDARD.encode(&self.extra);
        write!(f, "Tx [{:?}, {}]", self.data, &encoded)
    }
}

impl<D: Debug> Display for SimpleTx<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = general_purpose::STANDARD.encode(&self.extra);
        write!(f, "Tx [{:?}, {}]", self.data, &encoded)
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
    #[cfg(feature = "benchmark")]
    fn is_sample(&self) -> bool {
        let extra_data: ExtraData = bincode::deserialize(&self.extra)
            .expect("Failed to deserialize");
        extra_data.sample
    }

    #[cfg(feature = "benchmark")]
    fn get_id(&self) -> u64 {
        let extra_data: ExtraData = bincode::deserialize(&self.extra)
            .expect("Failed to deserialize");
        extra_data.sample_id
    }
}

// --- ExtraData + MockTx impl (from node/src/smr/mocker.rs) ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ExtraData {
    pub source: Id,
    pub sample: bool,
    pub sample_id: u64,
    pub tag: usize,
}

impl ExtraData {
    pub fn new(tag: usize, source: Id, sample: bool, sample_id: u64) -> Self {
        Self { source, sample, sample_id, tag }
    }
}

impl<D: Data> MockTx for SimpleTx<D> {
    const HEADER_SIZE: usize = 33;

    fn mock_transaction(
        tx_id: usize,
        client_id: Id,
        tx_size: usize,
        sample: bool,
        sample_id: u64,
    ) -> Self {
        let data = D::with_payload(&vec![0; tx_size - Self::HEADER_SIZE]);
        let extra_data = ExtraData::new(tx_id, client_id, sample, sample_id);
        SimpleTx {
            data,
            extra: bincode::serialize(&extra_data).unwrap(),
        }
    }
}
