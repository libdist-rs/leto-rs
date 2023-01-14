use crate::{
    server::{BatcherConsensusMsg as BCM,Leto},
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
    /// TODO: Handle proposals received right after the blame QC
    /// TODO: Handle proposing by extending QCs
    pub async fn handle_proposal(
        &mut self,
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch: Batch<Tx>,
    ) -> Result<()>
    where
        Tx: types::Transaction,
    {
        debug!(
            "Got a proposal: {:?} in round {}", 
            proposal, 
            self.round_context.round(),
        );

        // Get parent
        let parent_hash = proposal.block().parent_hash();
        trace!("Querying parent hash: {:?}", parent_hash);
        let parent = self.chain_state
            .get_element(parent_hash)
            .await?
            .unwrap();
        
        // Check proposal validity
        let is_proposal_valid = {
            let mut start = parent.proposal.round() + 1;
            let mut idx = 0usize;
            let mut status = true;
            let qc_len = (self.settings.committee_config.num_nodes()
                + self.settings.committee_config.num_faults()
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
                    .map(|qc_vec| {
                        if qc_vec[idx].unique_len() != qc_len {
                            return false;
                        }
                        if qc_vec[idx]
                            .verify(&round_hash, &self.crypto_system.system)
                            .is_err()
                        {
                            return false;
                        }
                        true
                    })
                    .unwrap_or(false);
                if !res {
                    status = false;
                    break;
                }
                if idx > self.settings.committee_config.num_faults() {
                    status = false;
                    break;
                }
                start += 1;
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
                    .ok_or_else(|| anyhow!("Unknown signer for proposal"))?,
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
        let qc = {
            let end = self.chain_state.highest_chain().proposal.round();
            let mut start = self.round_context.round() - 1;
            let mut qc_vec = Vec::new();
            while start > end {
                qc_vec.push(
                    self.chain_state
                        .get_qc(&start)
                        .expect("Expected qc for this round"),
                );
                start -= 1;
            }
            if qc_vec.is_empty() {
                None
            } else {
                Some(qc_vec)
            }
        };
        let round = self.round_context.round();
        let proposal = Proposal::new(block, round, qc);

        #[cfg(feature = "benchmark")]
        {
            // NOTE: This log entry is used to compute performance.
            info!("Created B{} -> {:?}", 
                proposal.round(), 
                proposal.block().batch_hash(),
            );
        }

        // Create sig
        let prop_hash = Hash::ser_and_hash(&proposal);
        let auth = Signature::new(prop_hash, self.my_id, &self.crypto_system.secret)?;

        // Create protocol msg
        let batch = self
            .chain_state
            .get_batch(batch_hash)
            .await?
            .ok_or_else(|| {
                anyhow!("Implementation Bug: Expected proposer to have his batch in his own DB")
            })?;

        info!("Proposing a batch with {} tx", batch.payload.len());

        let msg = ProtocolMsg::<Id, Tx, Round>::Propose {
            proposal: proposal.clone(),
            auth: auth.clone(),
            batch: batch.clone(),
            sender: self.my_id,
        };

        // Broadcast message
        let handlers = self
            .consensus_net
            .broadcast(&self.broadcast_peers, msg.clone())
            .await;
        self.round_context
            .add_handlers(handlers);

        // Loopback
        if let Err(e) = self.handle_proposal(proposal, auth, batch).await {
            error!("Error handling my own proposal: {}", e);
        }

        Ok(())
    }
}
