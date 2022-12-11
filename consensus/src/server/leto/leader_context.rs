use crate::{Id, Round};
use crypto::hash::Hash;
use fnv::FnvHashSet;
use rand::{rngs::StdRng, seq::IteratorRandom, SeedableRng};
use std::collections::VecDeque;

#[derive(Debug)]
pub struct LeaderContext {
    rng: StdRng,
    current_round: Round,
    elligible: FnvHashSet<Id>,
    history: VecDeque<Id>,

    // Caches
    current_leader: Id,
    next_leader: Id,
}

impl LeaderContext {
    pub const SEED: &str = "LETO-PROTOCOL";
}

pub fn hash_to_seed<T>(hash: &Hash<T>) -> [u8; 32] {
    let seed_hash = hash.to_vec();
    let mut seed: [u8; 32] = [0u8; 32];
    for i in 0..32 {
        seed[i] = seed_hash[i];
    }
    seed
}

impl LeaderContext {
    /// Returns the leader of the current round
    pub fn leader(&self) -> Id {
        self.current_leader
    }

    /// Returns the leader of the next round
    pub fn next_leader(&self) -> Id {
        self.next_leader
    }

    /// Update the round
    pub fn advance_round(&mut self) {
        // Update the round
        self.current_round += 1.into();

        // Update the current leader
        self.current_leader = self.next_leader;

        // The (n+t+1)/2th old leader is now elligible
        let new_elligible_server = self.history.pop_back().unwrap();
        self.elligible.insert(new_elligible_server);

        // Current leader is no longer elligible
        self.elligible.remove(&self.current_leader);

        // Add current leader to the front of the history
        self.history.push_front(self.current_leader.clone());

        // Update the next leader using randomness
        self.next_leader = self.elligible.iter().choose(&mut self.rng).unwrap().clone();
    }
}

impl LeaderContext {
    pub fn new(
        mut all_ids: Vec<Id>,
        num_faults: usize,
    ) -> Self {
        let seed_hash = Hash::<&str>::do_hash(Self::SEED.as_bytes());
        let seed = hash_to_seed(&seed_hash);

        let num_nodes = all_ids.len();
        let replacement_limit = (num_nodes + num_faults + 1) / 2;
        let mut history = VecDeque::with_capacity(replacement_limit);
        for _ in 0..replacement_limit {
            let leader = all_ids.pop().unwrap();
            history.push_front(leader);
        }

        let elligible = all_ids.into_iter().map(|id| id).collect();

        let mut ctx = Self {
            rng: StdRng::from_seed(seed),
            current_leader: Id::START,
            current_round: Round::START,
            elligible,
            history,
            next_leader: Id::START,
        };

        ctx.advance_round();
        ctx.current_round = Round::START;
        ctx
    }
}

#[test]
fn test_leader_ctx() {
    let (n, f) = (34usize, 10);

    let ids: Vec<Id> = (1..n).into_iter().map(|i| i.into()).collect();
    let mut ctx = LeaderContext::new(ids.clone(), 10);
    let mut ctx2 = LeaderContext::new(ids, 10);
    ctx.advance_round();
    ctx2.advance_round();

    // Check consistency
    assert_eq!(ctx.current_leader, ctx2.current_leader);
    assert_eq!(ctx.next_leader, ctx2.next_leader);
    assert_eq!(ctx.elligible, ctx2.elligible);
    assert_eq!(ctx.history, ctx2.history);

    // Check correctness
    assert_eq!(ctx.history.len(), (n + f + 1) / 2);
    assert_eq!(ctx.elligible.len(), n - ((n + f + 1) / 2));
}
