use super::Leto;
use crate::{
    types::{self, Block, Proposal, ProtocolMsg, Signature},
    Id, Round,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use log::*;
use mempool::BatchHash;

impl<Transaction> Leto<Transaction>
where
    Transaction: types::Transaction,
{
    pub fn handle_proposal(
        &mut self,
        prop: Proposal<Transaction, Round>,
        auth: Signature<Id, Proposal<Transaction, Round>>,
    ) -> Result<()>
    where
        Transaction: types::Transaction,
    {
        debug!("Got a proposal: {:?}", prop);
        // TODO: Check correct leader
        // TODO: Check if the parent is known
        if auth.id != self.my_id {
            // Check signature
            let proposal_hash = Hash::ser_and_hash(&prop);
            let leader = self.round_context.leader();
            auth.verify(
                &proposal_hash,
                &leader,
                self.crypto_system
                    .system
                    .get(&leader)
                    .ok_or(anyhow!("Unknown signer for proposal"))?,
            )?;
        }
        Ok(())
    }

    pub fn handle_blame(
        &mut self,
        blame_round: Round,
        auth: Signature<Id, Round>,
    ) -> Result<()>
    where
        Transaction: types::Transaction,
    {
        debug!("Got a blame for round {} from {}", blame_round, auth.id);
        todo!();
    }

    pub async fn handle_new_batch(
        &mut self,
        batch_hash: BatchHash<Transaction>,
    ) -> Result<()>
    where
        Transaction: types::Transaction,
    {
        debug!("Got a batch hash: {}", batch_hash);

        // Create proposal
        let block = Block::new(batch_hash.clone());
        let round = self.round_context.round();
        let proposal = Proposal::new(block, round);

        // Create sig
        let prop_hash = Hash::ser_and_hash(&proposal);
        let auth = Signature::new(prop_hash, self.my_id, &self.crypto_system.secret)?;
        
        // Create protocol msg
        let msg = ProtocolMsg
            ::<Id, Transaction, Round>
            ::Propose { 
                proposal, 
                auth,
        };

        // Broadcast message
        let _ = self
            .consensus_net
            .broadcast(&self.broadcast_peers, msg.clone())
            .await;
        
        // Send to loopback
        self.tx_msg_loopback
            .send(msg)
            .map_err(anyhow::Error::new)
        // Ok(())
    }
}
