use crypto::hash::Hash;
use futures_util::Stream;
use linked_hash_map::LinkedHashMap;
use mempool::{Batch, Transaction};
use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::Interval;

/// Txpool holds the transactions and releases them when it is time
/// DONE (in RRBatcher) Implement propose once for a round strategy
#[derive(Debug)]
pub struct Txpool<Tx> {
    linked_hash_map: LinkedHashMap<Hash<Tx>, (Tx, /* Size of the Tx */ usize)>,
    current_size: usize,
    batch_size: usize,
    timer: Interval,
}

impl<Tx> Stream for Txpool<Tx>
where
    Tx: Transaction,
{
    type Item = Batch<Tx>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        // If timer has expired, make a batch
        if let Poll::Ready(_) = self.timer.poll_tick(cx) {
            return Poll::Ready(Some(self.as_mut().make_batch()));
        }
        // If we have enough, make a transaction
        if self.current_size > self.batch_size {
            return Poll::Ready(Some(self.as_mut().make_batch()));
        }
        Poll::Pending
    }
}

impl<Tx> Txpool<Tx>
where
    Tx: Transaction,
{
    /// Creates a new transaction pool
    pub fn new(
        batch_size: usize,
        batch_timeout: Duration,
    ) -> Self {
        Self {
            linked_hash_map: LinkedHashMap::new(),
            current_size: 0,
            batch_size,
            timer: tokio::time::interval(batch_timeout),
        }
    }

    /// Adds a transaction to the transaction pool
    pub fn add_tx(
        &mut self,
        tx: Tx,
        tx_size: usize,
    ) {
        let hash = Hash::ser_and_hash(&tx);
        self.current_size += tx_size;
        self.linked_hash_map.insert(hash, (tx, tx_size));
    }

    /// Removes all the transactions from the transaction pool if they exist
    pub fn clear_batch(
        &mut self,
        batch: Batch<Tx>,
    ) {
        for tx in batch.payload {
            let hash = Hash::ser_and_hash(&tx);
            if let Some((_, tx_size)) = self.linked_hash_map.remove(&hash) {
                self.current_size -= tx_size;
            }
        }
    }

    /// Attempts to make a batch with <= batch_size transactions
    pub fn make_batch(&mut self) -> Batch<Tx> {
        let mut current_batch_size = 0;
        let mut payload = Vec::new();
        // Stop if
        // (1) We collect enough transactions, then stop
        // (2) We run out of transactions
        while current_batch_size < self.batch_size && // Collected enough
            !self.linked_hash_map.is_empty()
        // Pool is not empty
        {
            if let Some((_, (tx, tx_size))) = self.linked_hash_map.pop_front() {
                payload.push(tx);
                current_batch_size += tx_size;
                self.current_size -= tx_size;
            }
        }
        self.timer.reset();
        Batch { payload }
    }

    /// Checks whether there are sufficient transactions in the pool to make a
    /// batch
    pub fn ready(&self) -> bool {
        self.current_size > self.batch_size
    }
}
