use super::Leto;
use crate::{
    server::BatcherConsensusMsg as BCM,
    types::{self, Block, Proposal, ProtocolMsg, Signature},
    Id, Round,
};
use anyhow::{anyhow, Context, Result};
use crypto::hash::Hash;
use log::*;
use mempool::{Batch, BatchHash};

impl<Tx> Leto<Tx>
where
    Tx: types::Transaction,
{
    /// A function to handle incoming proposals
    pub async fn handle_proposal(
        &mut self,
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch: Batch<Tx>,
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
            self.round_context.queue_proposal(proposal, auth, batch);
            return Ok(());
        }
        debug!("Got a proposal for the correct round");

        // Check if the parent is known
        let parent_hash = proposal.block().parent_hash();
        trace!("Querying parent hash: {:?}", parent_hash);
        let parent = self.chain_state.get_element(parent_hash).await?;
        if let None = parent {
            warn!("Parent not found for prop: {:?}", proposal);
            // TODO: Handle unknown parent
            // NOTE: This should never trigger in our experimental settings
            // self
            //    .tx_consensus_to_mem
            //    .send(
            //         ConsensusMempoolMsg::UnknownBatch(
            //             self.my_id,
            //             vec![parent_hash]
            //         )
            //     );
            // TODO: For now, return
            unreachable!("This case should never occur in our experiments");
        }
        let parent = parent.unwrap();
        let is_proposal_valid = {
            let mut start = parent.proposal.round() + 1.into();
            let mut idx = 0usize;
            let mut status = true;
            let qc_len = (self.settings.consensus_config.num_nodes()
                + self.settings.consensus_config.num_faults
                + 1)
                / 2;
            while proposal.round() != start && status {
                // Check QC for round#: start
                if proposal.qc().is_none() {
                    status = false;
                    break;
                }
                let round_hash = Hash::ser_and_hash(&start);
                let res = proposal
                    .qc()
                    .as_ref()
                    .and_then(|qc_vec| {
                        if qc_vec[idx].unique_len() != qc_len {
                            return Some(false);
                        }
                        if !qc_vec[idx]
                            .verify(&round_hash, &self.crypto_system.system)
                            .is_ok()
                        {
                            return Some(false);
                        }
                        Some(true)
                    })
                    .expect("Invariant must hold; must be unwrappable");
                if res == false {
                    status = false;
                    break;
                }
                if idx > self.settings.consensus_config.num_faults {
                    status = false;
                    break;
                }
                start += 1.into();
                idx += 1;
            }
            status
        };
        if !is_proposal_valid {
            warn!("Got an invalid chain after qc check");
            error!("Unimplemented QC check");
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
        self.on_correct_proposal(proposal, auth, batch).await
    }

    pub async fn on_correct_proposal(
        &mut self,
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch: Batch<Tx>,
    ) -> Result<()> {
        // Get batch hash
        let batch_hash = Hash::ser_and_hash(&batch);

        // Send the proposal to the next leader and wait for an ack from them
        self.relay_proposal(proposal.clone(), auth.clone(), batch_hash)
            .await?;

        debug!("Relaying finished");

        // Update the chain state (Will write this proposal to the disk)
        self.chain_state
            .update_highest_chain(proposal, auth, batch.clone())
            .await?;

        // Let the mempool know that we can clear these transactions
        self.tx_consensus_to_batcher
            .send(BCM::OptimisticClear { batch })
            .map_err(anyhow::Error::new)
            .context("Error while sending optimistic clear")?;

        // Advance the round
        self.advance_round().await
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
        let prev_hash = self.chain_state.highest_hash();
        let block = Block::new(batch_hash.clone(), prev_hash);
        let round = self.round_context.round();
        let proposal = Proposal::new(block, round, None);

        // Create sig
        let prop_hash = Hash::ser_and_hash(&proposal);
        let auth = Signature::new(prop_hash, self.my_id, &self.crypto_system.secret)?;

        // Create protocol msg
        let batch = self
            .chain_state
            .get_batch(batch_hash)
            .await?
            .ok_or(anyhow!(
                "Implementation Bug: Expected proposer to have his batch in his own DB"
            ))?;

        let msg = ProtocolMsg::<Id, Tx, Round>::Propose {
            proposal: proposal.clone(),
            auth: auth.clone(),
            batch: batch.clone(),
        };

        // Broadcast message
        let handlers = self
            .consensus_net
            .broadcast(&self.broadcast_peers, msg.clone())
            .await;
        self.round_context
            .cancel_handlers
            .entry(self.round_context.round())
            .or_insert_with(Vec::new)
            .extend(handlers);

        // Loopback
        if let Err(e) = self.handle_proposal(proposal, auth, batch).await {
            error!("Error handling my own proposal: {}", e);
        }

        Ok(())
    }
}
