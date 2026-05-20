use crate::Id;

pub trait Transaction: mempool::Transaction + Unpin {
    /// The client that originated this transaction.
    fn client_id(&self) -> Id;

    /// Per-client monotonically increasing sequence number.
    fn nonce(&self) -> u64;

    /// Returns true if this tx is a benchmark sample tx.
    /// Default: false (non-benchmark impls don't sample).
    fn is_sample(&self) -> bool {
        false
    }

    /// Returns the benchmark sample id.
    /// Default: 0.
    fn get_id(&self) -> u64 {
        0
    }
}
