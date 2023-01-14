use crate::SimpleTx;
use consensus::{client::MockTx, Id};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ExtraData {
    pub source: Id,
    pub sample: bool,
    pub sample_id: u64,
    pub tag: usize,
}

impl ExtraData {
    pub fn new(
        tag: usize,
        source: Id,
        sample: bool,
        sample_id: u64,
    ) -> Self {
        Self { source, sample, sample_id, tag }
    }
}

// Generates a mock transaction with this Id
impl<Data> MockTx for SimpleTx<Data>
where
    Data: crate::Data,
{
    const HEADER_SIZE: usize = 33;

    fn mock_transaction(
        tx_id: usize,
        client_id: Id,
        tx_size: usize,
        sample: bool,
        sample_id: u64,
    ) -> Self {
        let data = Data::with_payload(&vec![0; tx_size-Self::HEADER_SIZE]);
        let extra_data = ExtraData::new(
            tx_id, 
            client_id,
            sample,
            sample_id,
        );
        SimpleTx {
            data,
            extra: bincode::serialize(&extra_data).unwrap(),
        }
    }
}
