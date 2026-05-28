//! Shared in-memory block-presence index for Hera cross-actor gating.
//!
//! Replaces the `Storage::notify_read(data_block_key(hash))` gating path in
//! `core.rs::push_gating_future`.  The old path funneled every gating wait
//! through libstorage's single serial store task (a known throughput wall).
//! `BlockPresence` is a `std::sync::Mutex`-guarded in-memory map that
//! resolves gates in microseconds without any disk I/O.
//!
//! ## Wait / insert protocol
//! - **data actor** (on admission): `presence.insert(hash, author, height)` —
//!   synchronous; marks the block present and drains any registered waiters.
//! - **consensus actor** (in GateFut): `presence.wait(hash).await` — resolves
//!   immediately if the block is already present; otherwise registers a oneshot
//!   and awaits it.  The `std::sync::Mutex` is dropped BEFORE the `.await`
//!   (critical: never hold a std Mutex across an await point).
//!
//! ## GC
//! After advancing `committed_heights[author]` in `on_commit_emit`, call
//! `presence.gc(author, new_height)` to remove entries for that author at or
//! below `new_height`.  Committed blocks will never be gated on again, so
//! removing them is safe and keeps the index bounded.
use std::sync::{Arc, Mutex};

use fnv::FnvHashMap;
use tokio::sync::oneshot;

use super::DataBlockHash;
use crate::Id;

struct Inner<Tx> {
    /// Present blocks: hash → (author, height) for GC.
    present: FnvHashMap<DataBlockHash<Tx>, (Id, u64)>,
    /// Pending waiters per hash.
    waiters: FnvHashMap<DataBlockHash<Tx>, Vec<oneshot::Sender<()>>>,
}

pub struct BlockPresence<Tx> {
    inner: Mutex<Inner<Tx>>,
}

impl<Tx> BlockPresence<Tx> {
    /// Create a new, empty presence index wrapped in an `Arc`.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                present: FnvHashMap::default(),
                waiters: FnvHashMap::default(),
            }),
        })
    }

    /// Mark `hash` as present (admitted by the data actor).
    ///
    /// Drains any registered waiters for this hash by sending `()` on each
    /// oneshot.  Called on EVERY valid block admission (self-propose,
    /// DataPropose, DataResponse) — right when the body is inserted into
    /// `DataBlockDB`.
    pub fn insert(
        &self,
        hash: DataBlockHash<Tx>,
        author: Id,
        height: u64,
    ) {
        let mut g = self.inner.lock().expect("BlockPresence lock poisoned");
        g.present.insert(hash.clone(), (author, height));
        if let Some(senders) = g.waiters.remove(&hash) {
            for tx in senders {
                // Ignore send errors: the waiter may have been dropped (e.g.
                // blame fired and the round advanced).
                let _ = tx.send(());
            }
        }
    }

    /// Wait until `hash` is present.
    ///
    /// If already present: returns immediately (no allocation).
    /// Otherwise: registers a oneshot sender under the lock, drops the lock,
    /// then awaits the receiver.  The lock is never held across an `.await`.
    pub async fn wait(
        &self,
        hash: DataBlockHash<Tx>,
    ) {
        // --- lock scope: check presence and optionally register a waiter ---
        let rx = {
            let mut g = self.inner.lock().expect("BlockPresence lock poisoned");
            if g.present.contains_key(&hash) {
                // Fast path: already admitted — no allocation, no await.
                return;
            }
            let (tx, rx) = oneshot::channel::<()>();
            g.waiters.entry(hash).or_default().push(tx);
            rx
            // MutexGuard `g` dropped here (end of block).
        };
        // --- lock released; now safe to await ---
        // Ignore the error: the sender is dropped if the data actor shuts
        // down or blame fired; either way this gate should unblock.
        let _ = rx.await;
    }

    /// Remove all presence entries for `author` at height <=
    /// `committed_height`.
    ///
    /// Also removes any lingering stale waiters for those hashes (they will
    /// never be used: committed blocks are never gated on again).
    pub fn gc(
        &self,
        author: Id,
        committed_height: u64,
    ) {
        let mut g = self.inner.lock().expect("BlockPresence lock poisoned");
        // Collect hashes to remove.
        let to_remove: Vec<DataBlockHash<Tx>> = g
            .present
            .iter()
            .filter(|(_, &(a, h))| a == author && h <= committed_height)
            .map(|(k, _)| k.clone())
            .collect();
        for h in to_remove {
            g.present.remove(&h);
            // Drop any lingering waiters for this hash.  Receivers will get
            // an `Err(RecvError)` on `.await`, which `wait` ignores.
            g.waiters.remove(&h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DataBlock, DataBlockEnvelope, DataBlockSig};
    use std::marker::PhantomData;

    fn mk_hash(height: u64) -> DataBlockHash<u32> {
        let b = DataBlock::<u32> {
            envelope: DataBlockEnvelope {
                epoch: 1,
                height,
                payload: std::sync::Arc::new(vec![height as u32]),
                parent_hash: DataBlock::<u32>::genesis().hash().clone(),
            },
            sig: DataBlockSig {
                raw: Vec::new(),
                signer: 0,
                _phantom: PhantomData,
            },
            cached_hash: Default::default(),
        };
        b.hash().clone()
    }

    #[tokio::test]
    async fn wait_resolves_immediately_when_present() {
        let p = BlockPresence::<u32>::new();
        let h = mk_hash(1);
        p.insert(h.clone(), 0, 1);
        // Should return without blocking.
        tokio::time::timeout(std::time::Duration::from_millis(10), p.wait(h))
            .await
            .expect("should resolve immediately");
    }

    #[tokio::test]
    async fn wait_unblocks_on_insert() {
        let p = BlockPresence::<u32>::new();
        let h = mk_hash(2);
        let p2 = Arc::clone(&p);
        let hc = h.clone();
        let waiter = tokio::spawn(async move { p2.wait(hc).await });
        tokio::task::yield_now().await;
        p.insert(h, 0, 2);
        tokio::time::timeout(std::time::Duration::from_millis(50), waiter)
            .await
            .expect("timeout")
            .expect("task panicked");
    }

    #[tokio::test]
    async fn gc_removes_committed_entries() {
        let p = BlockPresence::<u32>::new();
        let h1 = mk_hash(1);
        let h2 = mk_hash(2);
        p.insert(h1.clone(), 0, 1);
        p.insert(h2.clone(), 0, 2);
        p.gc(0, 1);
        {
            let g = p.inner.lock().unwrap();
            assert!(!g.present.contains_key(&h1), "h1 should be GC'd");
            assert!(g.present.contains_key(&h2), "h2 should remain");
        }
    }
}
