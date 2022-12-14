use crate::{
    server::Leto,
    types::{Proposal, ProtocolMsg, Signature, Transaction},
    Id, Round,
};
use anyhow::Result;
use log::*;
use mempool::BatchHash;

impl<Tx> Leto<Tx>
where
    Tx: Transaction,
{
    pub async fn relay_proposal(
        &mut self,
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch_hash: BatchHash<Tx>,
    ) -> Result<()> {
        debug!("Relaying the proposal");

        // Get the leader for the next round
        let next_leader = self.leader_context.next_leader();

        // If I am the next leader, ignore
        if next_leader == self.my_id {
            debug!("Returning because I am the next leader");
            // self.tx_msg_loopback.send(relay_msg)?;
            return Ok(());
        }

        // Relay message to the next leader
        let relay_msg = ProtocolMsg::Relay {
            proposal,
            auth,
            batch_hash,
            sender: self.my_id,
        };

        // If I am not the next leader, send real message
        let handler = self.consensus_net.send(next_leader, relay_msg).await;
        self.round_context
            .add_handler(handler);

        Ok(())
    }

    // pub async fn handle_relay(
    //     &mut self,
    //     proposal: Proposal<Id, Tx, Round>,
    //     auth: Signature<Id, Proposal<Id, Tx, Round>>,
    //     batch_hash: BatchHash<Tx>,
    //     source: Id,
    // ) -> Result<()> {
    //     // Check rounds
    //     // We may have already moved on
    //     debug!("Got a relay proposal: {:?}", proposal);
    //     debug!("Relayed proposal has sig: {:?}", auth);

    //     // Check if this proposal is for the correct round
    //     match proposal.round().cmp(&self.round_context.round()) {
    //         std::cmp::Ordering::Less => {
    //             // Ignore
    //             warn!(
    //                 "Got an old proposal for round {} in {}",
    //                 proposal.round(),
    //                 self.round_context.round()
    //             );
    //             return Ok(());
    //         }
    //         std::cmp::Ordering::Greater => {
    //             // Handle future proposals
    //             warn!(
    //                 "Got a future proposal for round {} in {}",
    //                 proposal.round(),
    //                 self.round_context.round()
    //             );
    //             self.round_context
    //                 .queue_relay(proposal, auth, batch_hash, source);
    //             return Ok(());
    //         }
    //         _ => (),
    //     };
    //     debug!("Got a relay for the correct round");

    //     // // Check whether batch is known
    //     let batch = self
    //         .chain_state
    //         .get_batch(batch_hash.clone())
    //         .await?
    //         .ok_or_else(||
    //             anyhow!("Synchronizer did not sync the relay message")
    //         )?;
    //     //     .context("Error getting batch hash when processing a relay")?;
    //     // if batch_opt.is_none() {
    //     //     // Ask sender for the batch corresponding to this
    //     //     let pmsg = ProtocolMsg::BatchRequest {
    //     //         source: self.my_id,
    //     //         request: Request::new(batch_hash.clone()),
    //     //     };
    //     //     let handler = self.consensus_net.send(source, pmsg).await;
    //     //     self.round_context
    //     //         .cancel_handlers
    //     //         .entry(self.round_context.round())
    //     //         .or_insert_with(Vec::new)
    //     //         .push(handler);
    //     //     // Reschedule self
    //     //     return self
    //     //         .synchronizer
    //     //         .on_unknown_batch(proposal, auth, batch_hash, source)
    //     //         .await;
    //     // }
    //     self.handle_proposal(proposal, auth, batch).await
    // }
}
