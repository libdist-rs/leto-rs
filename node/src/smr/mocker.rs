use crate::SimpleTx;
use consensus::{client::MockTx, Id};
use serde::{Deserialize, Serialize};

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

// Generates a mock transaction with this Id
impl<Data> MockTx for SimpleTx<Data>
where
    Data: crate::Data,
{
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
        let data = Data::with_payload(&vec![0; payload_size]);
        let extra_data = ExtraData::new(sample, sample_id);
        SimpleTx {
            data,
            source: client_id,
            nonce: tx_id as u64,
            extra: bincode::serialize(&extra_data).unwrap(),
        }
    }
}
