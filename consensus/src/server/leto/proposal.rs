use super::Leto;
use crate::{
    types::{self, Block, Proposal, ProtocolMsg, Signature},
    Id, Round,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use log::*;
use mempool::BatchHash;

impl<Tx> Leto<Tx>
where
    Tx: types::Transaction,
{
    pub async fn handle_proposal(
        &mut self,
        prop: Proposal<Tx, Round>,
        auth: Signature<Id, Proposal<Tx, Round>>,
    ) -> Result<()>
    where
        Tx: types::Transaction,
    {
        debug!("Got a proposal: {:?}", prop);
        // TODO: Check if the parent is known
        let parent_hash = prop.block().parent_hash();
        trace!("Querying parent hash: {:?}", parent_hash);
        let parent = self.chain_state.parent(parent_hash).await?;
        if let None = parent {
            warn!("Parent not found for prop: {:?}", prop);
            // TODO: Handle unknown parent
            // self._tx_consensus_to_mem.send(ConsensusMempoolMsg::UnknownBatch(, ));
            return Ok(());
            // TODO: For now, return
        }
        debug!("Parent identified for the current proposal");
        // TODO: Check if this proposal is for the correct round
        if prop.round() < self.round_context.round() {
            // Ignore
            warn!(
                "Got an old proposal for round {} in {}",
                prop.round(),
                self.round_context.round()
            );
            return Ok(());
        } else if prop.round() > self.round_context.round() {
            // Handle future proposals
            warn!(
                "Got a future proposal for round {} in {}",
                prop.round(),
                self.round_context.round()
            );
            self.round_context.queue_proposal(prop);
            return Ok(());
        }
        // Check signature
        let proposal_hash = Hash::ser_and_hash(&prop);
        // Check correct leader
        let leader = self.round_context.leader();
        if leader != self.my_id {
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
        debug!("Got a blame for round {} from {}", blame_round, auth.id);
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
        let prev_hash = self.chain_state.highest_block_hash();
        let block = Block::new(batch_hash.clone(), prev_hash);
        let round = self.round_context.round();
        let proposal = Proposal::new(block, round);

        // Create sig
        let prop_hash = Hash::ser_and_hash(&proposal);
        let auth = Signature::new(prop_hash, self.my_id, &self.crypto_system.secret)?;

        // Create protocol msg
        let msg = ProtocolMsg::<Id, Tx, Round>::Propose { proposal, auth };

        // Broadcast message
        let handlers = self
            .consensus_net
            .broadcast(&self.broadcast_peers, msg.clone())
            .await;

        // Wait for n-f servers to get the message before proceeding
        self.quorum_waiter.wait(handlers).await?;

        // Send to loopback
        self.tx_msg_loopback.send(msg).map_err(anyhow::Error::new)
        // Ok(())
    }
}
