use crate::{
    types::{Certificate, ProtocolMsg, Signature, Transaction},
    Id, Round, server::Leto,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use log::*;

impl<Tx> Leto<Tx> {
    pub async fn on_round_timeout(&mut self) -> Result<()>
    where
        Tx: Transaction,
    {
        // Disable more timeouts for the same round until we advance
        self.round_context.disable_blame_timers(&mut self.timer_enabled);

        // Construct blame message
        let blame_msg = self.round_context.round();
        let blame_msg_hash = Hash::ser_and_hash(&blame_msg);
        let auth = Signature::new(blame_msg_hash, self.my_id, &self.crypto_system.secret)?;
        let pmsg = ProtocolMsg::<Id, Tx, Round>::Blame {
            round: blame_msg,
            auth,
        };

        // Broadcast to all the servers
        let handlers = self
            .consensus_net
            .broadcast(&self.broadcast_peers, pmsg.clone())
            .await;
        self.round_context.add_handlers(handlers);

        // Loopback
        self.tx_msg_loopback
            .send(pmsg)
            .map_err(anyhow::Error::new)?;

        Ok(())
    }

    /// Handle blame is called on receiving a blame message
    /// Cases to be careful about:
    /// - A blame can be called after handling a proposal for the current round
    ///   (occurs due to the future scheduling)
    /// - A blame can be called for the future round
    /// - Normal blame message
    pub async fn handle_blame(
        &mut self,
        blame_round: Round,
        auth: Signature<Id, Round>,
    ) -> Result<()>
    where
        Tx: Transaction,
    {
        debug!("Got a blame message for round {}: {}", blame_round, auth);

        // Handle the case where we received a proposal for the current round and a
        // blame We will prefer the proposal over the blame
        if self.chain_state.highest_chain().proposal.round() == blame_round {
            warn!("Got a blame and a proposal for round: {}", blame_round);
            return Ok(());
        }

        // Handle the case where we already received a QC
        let qc_len = (self.settings.committee_config.num_nodes()
            + self.settings.committee_config.num_faults()
            + 1)
            / 2;
        if self.round_context.got_qc {
            return Ok(());
        }

        // Check the signature
        let origin = auth.get_id();
        let blame_msg_hash = Hash::ser_and_hash(&blame_round);
        let pk = self
            .crypto_system
            .system
            .get(&origin)
            .ok_or_else(|| anyhow!("Got blame from unknown id: {}", origin))?;
        auth.verify_without_id_check(&blame_msg_hash, pk)?;

        // Add signature to the set
        self.round_context.blame_map.insert(auth.get_id(), auth);

        // Only if we have qc_len # of signatures, move to the next step
        if self.round_context.blame_map.len() != qc_len {
            return Ok(());
        }
        // Move on to on_blame QC
        self.round_context.got_qc = true;

        // Extract the certificate
        let mut qc = Certificate::empty();
        let blame_map = std::mem::take(&mut self.round_context.blame_map);
        for (_, auth) in blame_map {
            qc.add(auth);
        }

        // By-pass checks
        self.handle_blame_qc(blame_round, qc).await
    }

    /// On getting a blame QC we broadcast the QC to all the servers
    /// and move to the next round
    /// Handle
    /// - calling once for a round
    /// - stale rounds
    pub async fn on_blame_qc(
        &mut self,
        blame_round: Round,
        qc: Certificate<Id, Round>,
    ) -> Result<()>
    where
        Tx: Transaction,
    {
        debug!("Got a blame QC message for round {}: {:?}", blame_round, qc);

        // Filter re-entry
        if self.round_context.got_qc {
            debug!("Already got QC for this round");
            return Ok(());
        }

        // Check validity of the QC
        let blame_hash = Hash::ser_and_hash(&blame_round);
        qc.verify(&blame_hash, &self.crypto_system.system)?;

        // Got a correct QC, move on
        self.handle_blame_qc(blame_round, qc).await
    }

    pub async fn handle_blame_qc(
        &mut self,
        blame_round: Round,
        qc: Certificate<Id, Round>,
    ) -> Result<()>
    where
        Tx: Transaction,
    {
        // Prevent re-entry
        self.round_context.got_qc = true;

        debug!("Got a correct blame QC");

        // Construct BlameQC msg
        let pmsg = ProtocolMsg::<Id, Tx, Round>::BlameQC {
            round: blame_round,
            qc: qc.clone(),
        };

        // Broadcast the certificate
        let cancel_handlers = self
            .consensus_net
            .broadcast(&self.broadcast_peers, pmsg)
            .await;
        self.round_context.add_handlers(cancel_handlers);

        // Update chain state
        self.chain_state.add_qc(blame_round, qc);

        // Advance round
        self.advance_round().await
    }
}
