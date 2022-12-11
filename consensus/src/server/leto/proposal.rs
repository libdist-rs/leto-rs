use super::Leto;
use crate::{
    types::{self, Block, Proposal, ProtocolMsg, Signature},
    Id, Round, server::BatcherConsensusMsg as BCM,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use log::*;
use mempool::BatchHash;

impl<Tx> Leto<Tx>
where
    Tx: types::Transaction,
{
    /// A function to handle incoming proposals
    pub async fn handle_proposal(
        &mut self,
        proposal: Proposal<Tx, Round>,
        auth: Signature<Id, Proposal<Tx, Round>>,
    ) -> Result<()>
    where
        Tx: types::Transaction,
    {
        debug!("Got a proposal: {:?}", proposal);
        debug!("Proposal has sig: {:?}", auth);

        // Check if this proposal is for the correct round
        if proposal.round() < self.round_context.round() {
            // Ignore
            warn!(
                "Got an old proposal for round {} in {}",
                proposal.round(),
                self.round_context.round()
            );
            return Ok(());
        } else if proposal.round() > self.round_context.round() {
            // Handle future proposals
            warn!(
                "Got a future proposal for round {} in {}",
                proposal.round(),
                self.round_context.round()
            );
            self.round_context.queue_proposal(proposal, auth);
            return Ok(());
        }
        debug!("Got a proposal for the correct round");

        // Check if the parent is known
        let parent_hash = proposal.block().parent_hash();
        trace!("Querying parent hash: {:?}", parent_hash);
        let parent = self.chain_state.parent(parent_hash).await?;
        if let None = parent {
            warn!("Parent not found for prop: {:?}", proposal);
            // TODO: Handle unknown parent
            // self
            //    .tx_consensus_to_mem
            //    .send(
            //         ConsensusMempoolMsg::UnknownBatch(
            //             self.my_id,
            //             vec![parent_hash]
            //         )
            //     );
            // TODO: For now, return
            return Ok(());
        }

        debug!("Parent identified for the current proposal");

        // Check signature
        let proposal_hash = Hash::ser_and_hash(&proposal);
        let leader = self.leader_context.leader();
        if leader != self.my_id {
            // Check correct leader
            auth.verify(
                &proposal_hash,
                &leader,
                self.crypto_system
                    .system
                    .get(&leader)
                    .ok_or(anyhow!("Unknown signer for proposal"))?,
            )?;
        }

        /* WE NOW HAVE A CORRECT PROPOSAL */
        self.on_correct_proposal(proposal, auth).await
    }

    pub async fn on_correct_proposal(
        &mut self,
        proposal: Proposal<Tx, Round>,
        auth: Signature<Id, Proposal<Tx, Round>>,
    ) -> Result<()> {
        // Send the proposal to the next leader and wait for an ack from them
        self.relay_proposal(proposal.clone(), auth.clone()).await?;

        debug!("Relaying finished");

        // Update the chain state (Will write this proposal to the disk)
        self.chain_state
            .update_highest_chain(proposal, auth)
            .await?;

        // TODO: Let the mempool know that we can clear these transactions
        // self.tx_consensus_to_batcher
        //     .send(BCM::OptimisticClear { batch: () });

        // Advance the round
        self.advance_round()
    }

    /// A function that will propose the new batch to all the servers
    pub async fn handle_new_batch(
        &mut self,
        batch_hash: BatchHash<Tx>,
    ) -> Result<()>
    where
        Tx: types::Transaction,
    {
        debug!("Got a batch hash: {}", batch_hash);
        debug!(
            "Server {} proposing for round {} as leader {}",
            self.my_id,
            self.round_context.round(),
            self.leader_context.leader()
        );

        // Create proposal
        let prev_hash = self.chain_state.highest_block_hash();
        let block = Block::new(batch_hash.clone(), prev_hash);
        let round = self.round_context.round();
        let proposal = Proposal::new(block, round);

        // Create sig
        let prop_hash = Hash::ser_and_hash(&proposal);
        let auth = Signature::new(prop_hash, self.my_id, &self.crypto_system.secret)?;

        // Create protocol msg
        let msg = ProtocolMsg::<Id, Tx, Round>::Propose {
            proposal: proposal.clone(),
            auth: auth.clone(),
        };

        // Broadcast message
        let handlers = self.consensus_net
            .broadcast(&self.broadcast_peers, msg.clone())
            .await;

        self.cancel_handlers
            .entry(self.round_context.round())
            .or_insert_with(Vec::new)
            .extend(handlers);

        // Send to loopback
        // self.tx_msg_loopback.send(msg).map_err(anyhow::Error::new)
        if let Err(e) = self.handle_proposal(proposal, auth).await {
            error!("Error handling my own proposal: {}", e);
        }

        Ok(())
    }
}
