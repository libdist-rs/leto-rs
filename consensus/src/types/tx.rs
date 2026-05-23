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

    /// Hera self-load benchmark: returns the nanosecond timestamp embedded in
    /// the payload at tx-creation time, if any.  Default: `None` (tx not
    /// stamped — caller skips latency tracking).  See
    /// `consensus/src/server/hera/load_gen.rs` for the embedding convention
    /// (16 bytes at payload offset 16..32, little-endian u128).
    fn hera_timestamp_ns(&self) -> Option<u128> {
        None
    }
}
