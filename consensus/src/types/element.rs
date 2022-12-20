use std::marker::PhantomData;
use super::{Block, Proposal, Signature};
use crypto::hash::Hash;
use mempool::Batch;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Element<Id, Tx, Round> {
    /// The proposal
    pub proposal: Proposal<Id, Tx, Round>,
    /// The signature on `self.proposal`
    pub auth: Signature<Id, Proposal<Id, Tx, Round>>,
    /// The batch referred in `self.proposal.block.batch_hash`
    pub batch: Batch<Tx>,
}

impl<Id, Tx, Round> Element<Id, Tx, Round> {
    pub fn new(
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch: Batch<Tx>,
    ) -> Self {
        Self {
            proposal,
            auth,
            batch,
        }
    }

    pub fn genesis(initial_leader: Id) -> Self 
    where 
        Round: mempool::Round,
        Id: network::Identifier,
    {
        // let
        Self {
            proposal: Proposal {
                block: Block::new(Hash::EMPTY_HASH, Hash::EMPTY_HASH),
                qc: None,
                round: Round::MIN,
            },
            auth: Signature {
                raw: Vec::new(),
                id: initial_leader,
                _x: PhantomData,
            },
            batch: Batch {
                payload: Vec::new(),
            },
        }
    }
}
