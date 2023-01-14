#[cfg(not(feature = "benchmark"))]
pub trait Transaction: mempool::Transaction + Unpin {}

#[cfg(feature = "benchmark")]
pub trait Transaction: mempool::Transaction + Unpin {
    fn is_sample(&self) -> bool;
    fn get_id(&self) -> u64;
}

