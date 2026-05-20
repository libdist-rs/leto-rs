use crate::{types::Transaction, Id, Round};
use fnv::{FnvHashMap, FnvHashSet};
use mempool::Batch;
use std::{collections::BTreeMap, time::Duration};
use tokio::time::Interval;

type Nonce = u64;
type TxKey = (Id, Nonce);

struct InflightEntry<Tx> {
    tx: Tx,
    size: usize,
    round: Round,
}

/// Nonce-keyed per-tx state machine replacing the hash-keyed `LinkedHashMap`.
///
/// Per-tx states:
///   Unknown   – no storage
///   Mineable  – in `mineable` BTreeMap, eligible for batching
///   InFlight  – in `inflight` HashMap, tagged with the round it was proposed
///   Replayed  – dropped on the floor at `add_tx`
///
/// Replicated state: `high_committed_nonce[client]` – max nonce committed on
/// the canonical chain for that client.  O(#clients), not O(#committed txs).
#[derive(Debug)]
pub struct Txpool<Tx> {
    /// Mineable transactions sorted by (client_id, nonce).
    /// BTreeMap iteration is in key order, giving deterministic, fair batches.
    mineable: BTreeMap<TxKey, (Tx, usize)>,
    mineable_bytes: usize,

    /// In-flight transactions indexed by (client_id, nonce).
    inflight: FnvHashMap<TxKey, InflightEntry<Tx>>,
    /// Secondary index: round → set of (client_id, nonce) in that round.
    inflight_by_round: FnvHashMap<Round, FnvHashSet<TxKey>>,

    /// High-water mark per client: max nonce committed on the canonical chain.
    high_committed_nonce: FnvHashMap<Id, Nonce>,

    batch_size: usize,
    timer: Interval,
}

impl<Tx> std::fmt::Debug for InflightEntry<Tx> {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        write!(
            f,
            "InflightEntry {{ size: {}, round: {} }}",
            self.size, self.round
        )
    }
}

impl<Tx> Txpool<Tx>
where
    Tx: Transaction,
{
    /// Creates a new transaction pool.
    pub fn new(
        batch_size: usize,
        batch_timeout: Duration,
    ) -> Self {
        Self {
            mineable: BTreeMap::new(),
            mineable_bytes: 0,
            inflight: FnvHashMap::default(),
            inflight_by_round: FnvHashMap::default(),
            high_committed_nonce: FnvHashMap::default(),
            batch_size,
            timer: tokio::time::interval(batch_timeout),
        }
    }

    /// Adds a client-submitted transaction.
    ///
    /// Dropped silently if:
    ///   - nonce ≤ high_committed_nonce[client] (replay / late RTO / Byzantine)
    ///   - (client, nonce) is already InFlight (proposal-direct LAN race)
    ///
    /// Otherwise inserted into Mineable; last-write-wins on equivocation.
    pub fn add_tx(
        &mut self,
        tx: Tx,
        size: usize,
    ) {
        let (c, n) = (tx.client_id(), tx.nonce());
        if n <= *self.high_committed_nonce.get(&c).unwrap_or(&0) {
            return; // replay / late RTO / Byzantine client
        }
        if self.inflight.contains_key(&(c, n)) {
            return; // proposal-direct race — already in InFlight, no-op
        }
        // Insert into Mineable; adjust byte counter for overwrites (equivocation
        // case: last-write-wins, same (c,n) key).
        if let Some((_, old_sz)) = self.mineable.insert((c, n), (tx, size)) {
            self.mineable_bytes -= old_sz;
        }
        self.mineable_bytes += size;

        debug_assert!(self.key_sets_disjoint());
    }

    /// Notifies the pool that `batch` has been proposed at `round`.
    ///
    /// For each tx:
    ///   - If stale (n ≤ hi[c]): silent skip (Byzantine proposal policy (a)).
    ///   - If already InFlight: idempotent no-op (Trace 8 loopback).
    ///   - If in Mineable: promote to InFlight(round).
    ///   - If Unknown (proposal arrived before client — Trace 2): insert
    ///     directly into InFlight(round).
    pub fn admit_proposal(
        &mut self,
        batch: &Batch<Tx>,
        round: Round,
    ) {
        let by_round = self.inflight_by_round.entry(round).or_default();
        for tx in &batch.payload {
            let (c, n) = (tx.client_id(), tx.nonce());
            if n <= *self.high_committed_nonce.get(&c).unwrap_or(&0) {
                // Byzantine or stale proposal — silent skip (policy (a)).
                continue;
            }
            if self.inflight.contains_key(&(c, n)) {
                // Already InFlight (loopback / duplicate proposal) — idempotent.
                continue;
            }
            let size = if let Some((_, sz)) = self.mineable.remove(&(c, n)) {
                self.mineable_bytes -= sz;
                sz
            } else {
                // Proposal arrived before the client copy (Trace 2).
                bincode::serialized_size(tx).unwrap_or(0) as usize
            };
            self.inflight.insert(
                (c, n),
                InflightEntry {
                    tx: tx.clone(),
                    size,
                    round,
                },
            );
            by_round.insert((c, n));
        }

        debug_assert!(self.key_sets_disjoint());
    }

    /// Pops up to `batch_size` bytes from Mineable, promoting them to
    /// InFlight(round).  Called by the proposer only.
    pub fn make_batch(
        &mut self,
        round: Round,
    ) -> Batch<Tx> {
        let mut payload = Vec::new();
        let mut batch_bytes = 0usize;
        let by_round = self.inflight_by_round.entry(round).or_default();

        while batch_bytes < self.batch_size {
            let key = match self.mineable.keys().next().copied() {
                Some(k) => k,
                None => break,
            };
            let (c, n) = key;
            let (tx, size) = self.mineable.remove(&key).unwrap();
            self.mineable_bytes -= size;
            batch_bytes += size;
            payload.push(tx.clone());
            self.inflight
                .insert((c, n), InflightEntry { tx, size, round });
            by_round.insert((c, n));
        }

        self.reset_timer();
        debug_assert!(self.key_sets_disjoint());
        Batch { payload }
    }

    /// Advances `high_committed_nonce` for every tx in the batch and GCs
    /// stale entries from Mineable and InFlight.
    pub fn commit(
        &mut self,
        batch: &Batch<Tx>,
        round: Round,
    ) {
        // 1. Advance high-water marks; remove committed (c,n) from both buckets.
        let mut touched: FnvHashSet<Id> = FnvHashSet::default();
        for tx in &batch.payload {
            let (c, n) = (tx.client_id(), tx.nonce());
            touched.insert(c);

            // Remove from InFlight (removes from inflight_by_round index too).
            if let Some(e) = self.inflight.remove(&(c, n)) {
                if let Some(set) = self.inflight_by_round.get_mut(&e.round) {
                    set.remove(&(c, n));
                }
            }
            // Remove from Mineable (idempotent if not there).
            if let Some((_, sz)) = self.mineable.remove(&(c, n)) {
                self.mineable_bytes -= sz;
            }

            let cur = self.high_committed_nonce.entry(c).or_insert(0);
            if n > *cur {
                *cur = n;
            }
        }

        // 2. GC Mineable entries below the new high-water marks (range scan).
        for c in &touched {
            let hi = *self.high_committed_nonce.get(c).unwrap();
            // Collect keys in range [(*c, 0), (*c, hi)] that are stale.
            let stale: Vec<TxKey> = self
                .mineable
                .range((*c, 0)..=(*c, hi))
                .map(|(k, _)| *k)
                .collect();
            for k in stale {
                if let Some((_, sz)) = self.mineable.remove(&k) {
                    self.mineable_bytes -= sz;
                }
            }

            // 3. InFlight stragglers below hi[c] are GC'd lazily in
            //    admit_proposal and rollback; bounded by commit_len in-flight
            //    rounds.
        }

        // Clean up the by_round index entry for the committed round if empty.
        // Do NOT unconditionally remove it: non-committed InFlight txs at the
        // same round (e.g. from a conflicting proposal) need the index intact
        // so rollback() can find and GC them.
        if let Some(set) = self.inflight_by_round.get(&round) {
            if set.is_empty() {
                self.inflight_by_round.remove(&round);
            }
        }

        debug_assert!(self.key_sets_disjoint());
    }

    /// Returns orphaned InFlight entries from `rounds` to Mineable
    /// (chain-switch).
    ///
    /// - Subsumed orphan (n ≤ hi[c]): a different proposal at this round
    ///   committed a higher nonce from c → drop.
    /// - Otherwise: return to Mineable so the tx can be re-proposed.
    pub fn rollback(
        &mut self,
        rounds: &[Round],
    ) {
        for r in rounds {
            if let Some(set) = self.inflight_by_round.remove(r) {
                for (c, n) in set {
                    let hi = *self.high_committed_nonce.get(&c).unwrap_or(&0);
                    if n <= hi {
                        // Subsumed orphan — already committed under a higher nonce.
                        self.inflight.remove(&(c, n));
                        continue;
                    }
                    if let Some(e) = self.inflight.remove(&(c, n)) {
                        self.mineable.insert((c, n), (e.tx, e.size));
                        self.mineable_bytes += e.size;
                    }
                }
            }
        }

        debug_assert!(self.key_sets_disjoint());
    }

    /// Resets the proposal timer.
    pub fn reset_timer(&mut self) {
        self.timer.reset();
    }

    /// Returns true when there are enough bytes in Mineable to fill a batch.
    pub fn ready(&self) -> bool {
        self.mineable_bytes > self.batch_size
    }

    /// Awaitable timer tick — resolves when the batch timeout fires.
    /// Used in `RRBatcher::run`'s `select!` branch.
    pub async fn tick_timer(&mut self) {
        self.timer.tick().await;
    }

    // ----- debug_assert! invariants -----

    #[allow(dead_code)]
    fn key_sets_disjoint(&self) -> bool {
        for key in self.mineable.keys() {
            if self.inflight.contains_key(key) {
                return false;
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Unit tests for the nonce-keyed state machine.
// Each test drives the four ops in sequence and asserts the end-state.
// Tests are named after the race traces in todo.org.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    // Minimal test transaction.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct TestTx {
        client: Id,
        nonce: u64,
        payload: u8,
    }

    impl TestTx {
        fn new(
            client: Id,
            nonce: u64,
        ) -> Self {
            Self {
                client,
                nonce,
                payload: 0,
            }
        }
    }

    impl net_common::Message for TestTx {
        type DeserializationError = Box<bincode::ErrorKind>;
        fn from_bytes(bytes: &[u8]) -> Result<Self, Self::DeserializationError> {
            bincode::deserialize(bytes)
        }
    }

    impl crate::types::Transaction for TestTx {
        fn client_id(&self) -> Id {
            self.client
        }
        fn nonce(&self) -> u64 {
            self.nonce
        }
    }

    fn pool() -> Txpool<TestTx> {
        Txpool::new(1024 * 1024, Duration::from_secs(60))
    }

    fn batch(txs: Vec<TestTx>) -> Batch<TestTx> {
        Batch { payload: txs }
    }

    // ----- Trace 1: common case (client copy arrives first) ----------------
    #[tokio::test]
    async fn trace1_common_case() {
        let mut p = pool();
        let tx = TestTx::new(1, 5);
        // t1: add_tx — 5 > hi[1]=0 → Mineable
        p.add_tx(tx.clone(), 10);
        assert!(p.mineable.contains_key(&(1, 5)));
        assert_eq!(p.mineable_bytes, 10);

        // t2: admit_proposal — Mineable → InFlight(r=1)
        p.admit_proposal(&batch(vec![tx.clone()]), 1);
        assert!(!p.mineable.contains_key(&(1, 5)));
        assert!(p.inflight.contains_key(&(1, 5)));
        assert_eq!(p.inflight_by_round[&1].len(), 1);

        // t3: commit — hi[1]:=5; (1,5) removed
        p.commit(&batch(vec![tx.clone()]), 1);
        assert!(!p.inflight.contains_key(&(1, 5)));
        assert_eq!(*p.high_committed_nonce.get(&1).unwrap(), 5);
    }

    // ----- Trace 2: proposal arrives before client (LAN race) --------------
    #[tokio::test]
    async fn trace2_proposal_before_client() {
        let mut p = pool();
        let tx = TestTx::new(1, 5);

        // t1: admit_proposal first — tx Unknown → InFlight directly
        p.admit_proposal(&batch(vec![tx.clone()]), 1);
        assert!(!p.mineable.contains_key(&(1, 5)));
        assert!(p.inflight.contains_key(&(1, 5)));

        // t2: add_tx arrives late — already InFlight → NO-OP
        p.add_tx(tx.clone(), 10);
        assert!(!p.mineable.contains_key(&(1, 5)));
        assert!(p.inflight.contains_key(&(1, 5)));
        assert_eq!(p.mineable_bytes, 0);
    }

    // ----- Trace 3: late client after commit (RTO) -------------------------
    #[tokio::test]
    async fn trace3_late_client_after_commit() {
        let mut p = pool();
        let tx = TestTx::new(1, 5);

        // t0: proposal admitted
        p.admit_proposal(&batch(vec![tx.clone()]), 1);
        // t1: commit — hi[1]:=5
        p.commit(&batch(vec![tx.clone()]), 1);
        assert_eq!(*p.high_committed_nonce.get(&1).unwrap(), 5);

        // t2: late add_tx — 5 ≤ hi[1]=5 → DROPPED
        p.add_tx(tx.clone(), 10);
        assert!(!p.mineable.contains_key(&(1, 5)));
        assert_eq!(p.mineable_bytes, 0);
    }

    // ----- Trace 4: chain switch orphans round r ---------------------------
    #[tokio::test]
    async fn trace4_chain_switch_rollback() {
        let mut p = pool();
        let tx = TestTx::new(1, 5);
        p.add_tx(tx.clone(), 10);

        // Proposer pops into InFlight(r=1)
        let _batch = p.make_batch(1);
        assert!(p.inflight.contains_key(&(1, 5)));
        assert!(!p.mineable.contains_key(&(1, 5)));

        // Chain switch — rollback round 1
        p.rollback(&[1]);
        // 5 > hi[1]=0 → returned to Mineable
        assert!(p.mineable.contains_key(&(1, 5)));
        assert!(!p.inflight.contains_key(&(1, 5)));
        assert_eq!(p.mineable_bytes, 10);
    }

    // ----- Trace 5: view change commits different proposal at same round ----
    //
    // Per the design, InFlight stragglers below hi[c] are GC'd *lazily* in
    // admit_proposal and rollback, not eagerly in commit.  After commit,
    // (1,5) may still sit in inflight; it will be dropped when rollback or
    // admit_proposal is called for the round that held it.
    #[tokio::test]
    async fn trace5_different_proposal_at_same_round() {
        let mut p = pool();
        let tx5 = TestTx::new(1, 5);
        let tx7 = TestTx::new(1, 7);

        // (1,5) promoted via P1 at round 2 (not the same round as commit).
        p.add_tx(tx5.clone(), 10);
        p.admit_proposal(&batch(vec![tx5.clone()]), 2);
        assert!(p.inflight.contains_key(&(1, 5)));

        // Canonical chain commits P2 with (1,7) at round 2.
        // hi[1] := 7; GC sweeps (1,5) from Mineable (nothing to sweep there).
        p.commit(&batch(vec![tx7.clone()]), 2);
        assert_eq!(*p.high_committed_nonce.get(&1).unwrap(), 7);
        // (1,5) may still be in inflight (lazy GC); not in Mineable.
        assert!(!p.mineable.contains_key(&(1, 5)));

        // Rollback of round 2 triggers lazy GC of (1,5): 5 ≤ hi[1]=7 → dropped.
        p.rollback(&[2]);
        assert!(!p.inflight.contains_key(&(1, 5)));
        assert!(!p.mineable.contains_key(&(1, 5)));
    }

    // ----- Trace 6: Byzantine client replays old nonce ---------------------
    #[tokio::test]
    async fn trace6_byzantine_replay() {
        let mut p = pool();
        // Establish hi[1] = 10
        let tx10 = TestTx::new(1, 10);
        p.add_tx(tx10.clone(), 10);
        p.admit_proposal(&batch(vec![tx10.clone()]), 1);
        p.commit(&batch(vec![tx10.clone()]), 1);
        assert_eq!(*p.high_committed_nonce.get(&1).unwrap(), 10);

        // Byzantine client sends nonce 3
        let tx3 = TestTx::new(1, 3);
        p.add_tx(tx3, 10);
        assert!(!p.mineable.contains_key(&(1, 3)));
        assert_eq!(p.mineable_bytes, 0);
    }

    // ----- Trace 8: own loopback at proposer L -----------------------------
    #[tokio::test]
    async fn trace8_proposer_loopback() {
        let mut p = pool();
        let tx = TestTx::new(1, 5);
        p.add_tx(tx.clone(), 10);

        // make_batch moves (1,5) to InFlight(r=1)
        let b = p.make_batch(1);
        assert_eq!(b.payload.len(), 1);
        assert!(p.inflight.contains_key(&(1, 5)));

        // Loopback: admit_proposal called again with same batch — idempotent
        p.admit_proposal(&b, 1);
        assert!(p.inflight.contains_key(&(1, 5)));
        assert!(!p.mineable.contains_key(&(1, 5)));
        // Still only one entry, not duplicated
        assert_eq!(p.inflight_by_round[&1].len(), 1);
    }

    // ----- Trace 9: equivocation (same (c,n), different payload) -----------
    #[tokio::test]
    async fn trace9_equivocation_last_write_wins() {
        let mut p = pool();
        let tx_a = TestTx {
            client: 1,
            nonce: 5,
            payload: 0xAA,
        };
        let tx_b = TestTx {
            client: 1,
            nonce: 5,
            payload: 0xBB,
        };

        p.add_tx(tx_a.clone(), 10);
        assert_eq!(p.mineable_bytes, 10);

        // Second write with same (c,n): overwrites.
        p.add_tx(tx_b.clone(), 10);
        assert_eq!(p.mineable.len(), 1);
        assert_eq!(p.mineable_bytes, 10); // not doubled

        // The stored tx is the last one written.
        let stored = &p.mineable[&(1, 5)].0;
        assert_eq!(stored.payload, 0xBB);
    }

    // ----- GC: commit sweeps stale Mineable entries -------------------------
    #[tokio::test]
    async fn gc_sweeps_stale_mineable() {
        let mut p = pool();
        // Pre-populate: (1,1), (1,2), (1,3) all Mineable.
        for n in 1u64..=3 {
            p.add_tx(TestTx::new(1, n), 10);
        }
        assert_eq!(p.mineable.len(), 3);
        assert_eq!(p.mineable_bytes, 30);

        // Commit (1,3) — hi[1] := 3; GC should remove (1,1) and (1,2) from Mineable.
        p.commit(&batch(vec![TestTx::new(1, 3)]), 1);
        assert!(!p.mineable.contains_key(&(1, 1)));
        assert!(!p.mineable.contains_key(&(1, 2)));
        assert!(!p.mineable.contains_key(&(1, 3)));
        assert_eq!(p.mineable_bytes, 0);
        assert_eq!(*p.high_committed_nonce.get(&1).unwrap(), 3);
    }

    // ----- Rollback subsumed orphan (Trace 5 variant) ----------------------
    #[tokio::test]
    async fn rollback_subsumed_orphan_dropped() {
        let mut p = pool();
        let tx5 = TestTx::new(1, 5);
        let tx7 = TestTx::new(1, 7);

        // (1,5) in InFlight(r=1), (1,7) also in InFlight(r=1) via admit
        p.add_tx(tx5.clone(), 10);
        p.add_tx(tx7.clone(), 10);
        p.admit_proposal(&batch(vec![tx5.clone(), tx7.clone()]), 1);

        // Commit (1,7) at round 1 — hi[1]:=7
        p.commit(&batch(vec![tx7.clone()]), 1);
        assert_eq!(*p.high_committed_nonce.get(&1).unwrap(), 7);

        // Now rollback round 1 (shouldn't exist in inflight_by_round after commit,
        // but test rollback path for a different round where (1,5) might linger
        // if it were in a different round).
        // Re-test with a fresh scenario: (1,5) in InFlight(r=2), then hi[1]:=7.
        let mut p2 = pool();
        p2.add_tx(tx5.clone(), 10);
        p2.admit_proposal(&batch(vec![tx5.clone()]), 2);
        // Simulate commit at round 3 advancing hi[1] past 5.
        p2.commit(&batch(vec![tx7.clone()]), 3);
        // Rollback round 2 — (1,5) is subsumed (5 ≤ 7) → drop, not return to Mineable.
        p2.rollback(&[2]);
        assert!(!p2.mineable.contains_key(&(1, 5)));
        assert!(!p2.inflight.contains_key(&(1, 5)));
    }
}
