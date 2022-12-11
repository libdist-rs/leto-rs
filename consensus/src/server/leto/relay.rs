use super::Leto;
use crate::{
    types::{Proposal, ProtocolMsg, Signature, Transaction},
    Id, Round,
};
use anyhow::Result;
use log::*;

impl<Tx> Leto<Tx>
where
    Tx: Transaction,
{
    pub async fn relay_proposal(
        &mut self,
        proposal: Proposal<Tx, Round>,
        auth: Signature<Id, Proposal<Tx, Round>>,
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
        let relay_msg = ProtocolMsg::Relay { proposal, auth };

        // If I am not the next leader, send real message
        let handler = self.consensus_net.send(next_leader, relay_msg).await;
        self.cancel_handlers
            .entry(self.round_context.round())
            .or_insert_with(Vec::new)
            .push(handler);

        Ok(())
    }
}
