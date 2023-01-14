mod settings;
pub use settings::*;

mod stresser;
pub use stresser::*;

use crate::Id;

pub trait MockTx: crate::types::Transaction {
    const HEADER_SIZE: usize;

    fn mock_transaction(
        tx_id: usize,
        client_id: Id,
        tx_size: usize,
        sample: bool,
        sample_id: u64,
    ) -> Self;
}
