use crate::SimpleTx;
use consensus::{client::MockTx, Id};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ExtraData {
    pub tag: usize,
    pub sample: bool,
    pub source: Id,
}

impl ExtraData {
    pub fn new(
        tag: usize,
        source: Id,
        sample: bool,
    ) -> Self {
        Self { tag, source, sample }
    }
}

// Generates a mock transaction with this Id
impl<Data> MockTx for SimpleTx<Data>
where
    Data: crate::Data,
{
    fn mock_transaction(
        tx_id: usize,
        client_id: Id,
        tx_size: usize,
        sample: bool,
    ) -> Self {
        let data = Data::with_payload(&vec![0; tx_size-33]);
        let extra_data = ExtraData::new(
            tx_id, 
            client_id,
            sample,
        );
        SimpleTx {
            data,
            extra: bincode::serialize(&extra_data).unwrap(),
        }
    }
}
