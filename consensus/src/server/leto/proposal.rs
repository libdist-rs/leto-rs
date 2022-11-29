use crate::{
    types::{self, Block, Proposal, ProtocolMsg, Signature},
    Id, Round, server::QuorumWaiter,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use futures_util::{stream::FuturesUnordered, StreamExt};
use log::*;
use mempool::{BatchHash, wait};
use super::Leto;

impl<Tx> Leto<Tx>
where
    Tx: types::Transaction,
{
    pub fn handle_proposal(
        &mut self,
        prop: Proposal<Tx, Round>,
        auth: Signature<Id, Proposal<Tx, Round>>,
    ) -> Result<()>
    where
        Tx: types::Transaction,
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
        Tx: types::Transaction,
    {
        debug!("Got a blame for round {} from {}", 
            blame_round, 
            auth.id
        );
        todo!();
    }

    pub async fn handle_new_batch(
        &mut self,
        batch_hash: BatchHash<Tx>,
    ) -> Result<()>
    where
        Tx: types::Transaction,
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
            ::<Id, Tx, Round>
            ::Propose { 
                proposal, 
                auth,
        };

        // Broadcast message
        let handlers = self
            .consensus_net
            .broadcast(&self.broadcast_peers, msg.clone())
            .await;
        
        let quorum_waiter = QuorumWaiter::new(
            self.settings.consensus_config.num_nodes()-self.settings.consensus_config.num_faults
        );
        quorum_waiter.wait(handlers).await?;
        // let mut wait_stream = FuturesUnordered::new();
        // for handler in handlers {
        //     // wait_stream.push(handler);
        //     // TODO: Use Quorum waiter
        //     let _ack = handler.await?;
        // }
        
        // Send to loopback
        self.tx_msg_loopback
            .send(msg)
            .map_err(anyhow::Error::new)
        // Ok(())
    }
}
