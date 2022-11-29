mod settings;
pub use settings::*;

mod stresser;
pub use stresser::*;

use crate::Id;


pub trait MockTx: crate::types::Transaction {
    fn mock_transaction(
        tx_id: usize, 
        client_id: Id,
        tx_size: usize,
    ) -> Self;
}