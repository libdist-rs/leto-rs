/// Hera sig-plane handlers.
///
/// Mirrors `zeus/phases/sig.rs` with `MultiAttestation<Tx>` as the payload.
/// The sig-chain protocol (propose / blame / blame-QC / advance-round) is
/// structurally identical to Zeus/Leto.
///
/// Key difference from Zeus: there is no `rleader_waiting_fresh` gate.
/// Every sig-chain round entry immediately triggers a proposal if this node
/// is the rleader, because every node is continuously producing data blocks
/// and there is always something fresh to attest.
use crate::{
    server::hera::Hera,
    types::{
        hera::MultiAttestation, Block, Certificate, HeraMsg, Proposal, Request, Response,
        Signature, Transaction,
    },
    Id, Round,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use log::*;
use mempool::Batch;
use serde::Serialize;
use std::sync::Arc;

impl<Tx> Hera<Tx>
where
    Tx: Transaction,
{
    // -----------------------------------------------------------------------
    // Hera: OnSignatureRoundPropose (sig-chain leader fires)
    // -----------------------------------------------------------------------

    /// Called when this node is the sig-chain leader and should propose a
    /// `MultiAttestation` for the current round.
    ///
    /// Unlike Zeus, there is no freshness gate: Hera always proposes
    /// immediately on round entry (every node continuously produces data and
    /// there is always something to attest).
    pub async fn handle_new_sig_round(&mut self) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let r = self.round_state.round();

        debug!(
            "Hera: sig-chain leader proposing multi-attestation for round {}",
            r
        );

        let attestation = self.make_multi_attestation(r)?;

        // Wrap attestation in a sig-chain Block.
        let att_hash = attestation.hash().clone();
        let batch_hash: mempool::BatchHash<MultiAttestation<Tx>> = unsafe {
            let src: crypto::hash::Hash<crate::types::hera::MultiAttestationEnvelope<Tx>> =
                att_hash;
            std::mem::transmute(src)
        };
        let prev_hash = self.sig_chain_state.highest_hash();
        let block = Block::<Id, MultiAttestation<Tx>, Round>::new(batch_hash, prev_hash);
        let proposal = Proposal::new(block, r, None);

        let prop_hash = Hash::ser_and_hash(&proposal);
        let auth = Signature::<Id, Proposal<Id, MultiAttestation<Tx>, Round>>::new(
            prop_hash,
            self.my_id,
            &self.crypto_system.secret,
        )?;

        let msg = HeraMsg::SigPropose {
            proposal: proposal.clone(),
            auth: auth.clone(),
            attestation: attestation.clone(),
            sender: self.my_id,
        };

        let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
        let _ = self
            .consensus_net
            .broadcast(&self.broadcast_peers, bytes)
            .await;

        // Yield so other Tokio tasks can deliver pending network messages
        // before the loopback cascade continues.
        tokio::task::yield_now().await;

        // Loopback.
        self.tx_msg_loopback
            .send(HeraMsg::SigPropose {
                proposal,
                auth,
                attestation,
                sender: self.my_id,
            })
            .map_err(anyhow::Error::new)?;

        // Update prev_attested_heights after the proposal is sent.
        // Committed heights are tracked separately; here we record what we just
        // attested so the NEXT proposal's freshness check is correct.
        let all_ids = self.settings.committee_config.get_all_ids();
        for id in all_ids {
            let h = self.multi_data_chain.head_height(id);
            *self.prev_attested_heights.entry(id).or_insert(0) = h;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Hera: OnSignaturePropose (all nodes receive SigPropose)
    // -----------------------------------------------------------------------

    pub async fn handle_sig_proposal(
        &mut self,
        proposal: Proposal<Id, MultiAttestation<Tx>, Round>,
        auth: Signature<Id, Proposal<Id, MultiAttestation<Tx>, Round>>,
        attestation: MultiAttestation<Tx>,
        _sender: Id,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let cur_round = self.round_state.round();
        if proposal.round() != cur_round {
            warn!(
                "Hera: sig proposal for wrong round {}, current {}",
                proposal.round(),
                cur_round
            );
            return Ok(());
        }

        let leader = self.sig_leader_context.leader();
        if leader != self.my_id {
            let pk = self
                .crypto_system
                .system
                .get(&leader)
                .ok_or_else(|| anyhow!("Unknown sig leader {}", leader))?;
            let prop_hash = Hash::ser_and_hash(&proposal);
            auth.verify(&prop_hash, &leader, pk)?;
        }

        // FuncMultiAttestationValid.
        let parked_msg = HeraMsg::SigPropose {
            proposal: proposal.clone(),
            auth: auth.clone(),
            attestation: attestation.clone(),
            sender: _sender,
        };
        match self.multi_attestation_valid(&attestation, parked_msg) {
            super::attestation::MultiAttestationValidity::Valid => {}
            super::attestation::MultiAttestationValidity::Invalid => {
                warn!("Hera: multi-attestation invalid in sig proposal");
                return Ok(());
            }
            super::attestation::MultiAttestationValidity::Parked(h) => {
                debug!("Hera: attestation parked waiting for {:?}", h);
                self.broadcast_data_request(h).await?;
                return Ok(());
            }
        }

        // Fetch a missing PARENT sig-element BEFORE agreeing, so the committer
        // never walks into a gap. A node that advanced past round r via a
        // blame-QC never stored round r's element; a round r+1 proposal then
        // references it as parent. Park this proposal and fetch the parent;
        // `on_sig_element_response` re-drives it once the parent arrives.
        let parent_hash = proposal.block().parent_hash();
        match self.sig_chain_state.get_element(parent_hash.clone()).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                debug!(
                    "Hera: sig proposal round={} parked for missing parent element {:?}",
                    proposal.round(),
                    parent_hash
                );
                let parked = HeraMsg::SigPropose {
                    proposal: proposal.clone(),
                    auth: auth.clone(),
                    attestation: attestation.clone(),
                    sender: _sender,
                };
                self.pending_sig_proposals
                    .entry(parent_hash.clone())
                    .or_default()
                    .push(parked);
                self.broadcast_sig_element_request(parent_hash).await?;
                return Ok(());
            }
            Err(e) => {
                warn!("Hera: error reading parent sig-element: {}", e);
                return Ok(());
            }
        }

        self.on_correct_sig_proposal(proposal, auth, attestation)
            .await
    }

    async fn on_correct_sig_proposal(
        &mut self,
        proposal: Proposal<Id, MultiAttestation<Tx>, Round>,
        auth: Signature<Id, Proposal<Id, MultiAttestation<Tx>, Round>>,
        attestation: MultiAttestation<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        // Relay to next leader.
        self.relay_sig_proposal(proposal.clone(), auth.clone())
            .await?;

        // Update sig-chain state.
        let batch = Batch {
            payload: vec![attestation],
        };
        self.sig_chain_state
            .update_highest_chain(proposal, auth, batch)
            .await?;

        // Advance sig-chain round.
        self.advance_sig_round().await
    }

    async fn relay_sig_proposal(
        &mut self,
        proposal: Proposal<Id, MultiAttestation<Tx>, Round>,
        auth: Signature<Id, Proposal<Id, MultiAttestation<Tx>, Round>>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        let next_leader = self.sig_leader_context.next_leader();
        if next_leader == self.my_id {
            return Ok(());
        }
        let att_hash: crypto::hash::Hash<crate::types::hera::MultiAttestationEnvelope<Tx>> = unsafe {
            let bh: mempool::BatchHash<MultiAttestation<Tx>> =
                proposal.block().batch_hash().clone();
            std::mem::transmute(bh)
        };
        let relay = HeraMsg::SigRelay {
            proposal,
            auth,
            att_hash,
            sender: self.my_id,
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&relay).map_err(anyhow::Error::new)?);
        let _ = self.consensus_net.send(next_leader, bytes).await;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Sig-element catch-up (mirrors the data-plane DataRequest/DataResponse).
    // -----------------------------------------------------------------------

    /// Broadcast a request for a missing sig-chain element by hash.
    pub(crate) async fn broadcast_sig_element_request(
        &mut self,
        target_hash: crate::server::hera::core::SigElementHash<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        // Dedupe: skip if a fetch for this element is already outstanding.
        if !self.pending_sig_element_requests.insert(target_hash.clone()) {
            return Ok(());
        }
        let msg = HeraMsg::<Tx>::SigElementRequest {
            source: self.my_id,
            request: Request::new(target_hash),
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
        let _ = self
            .consensus_net
            .broadcast(&self.broadcast_peers, bytes)
            .await;
        Ok(())
    }

    /// Serve a peer's request for a sig-chain element we hold.
    pub async fn on_sig_element_request(
        &mut self,
        target_hash: crate::server::hera::core::SigElementHash<Tx>,
        source: Id,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        if let Ok(Some(element)) = self.sig_chain_state.get_element(target_hash.clone()).await {
            let msg = HeraMsg::<Tx>::SigElementResponse {
                response: Response::new(target_hash, element),
            };
            let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
            let _ = self.consensus_net.send(source, bytes).await;
        }
        Ok(())
    }

    /// Admit a fetched sig-chain element and re-drive any proposals parked on
    /// it. The element is content-addressed: we store it under its true hash
    /// (`ser_and_hash`) and only re-drive proposals that referenced THAT hash,
    /// so a peer cannot substitute a different element. If the fetched element's
    /// own parent is also missing, fetch it too (fills a chain of gaps).
    pub async fn on_sig_element_response(
        &mut self,
        element: crate::server::hera::core::SigElement<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let element_hash: crate::server::hera::core::SigElementHash<Tx> =
            Hash::ser_and_hash(&element);

        // Accept only elements we actually want: one a proposal is parked on,
        // or one we requested (incl. recursively-fetched ancestors). The hash
        // is content-derived, so a peer cannot substitute a different element.
        let wanted = self.pending_sig_proposals.contains_key(&element_hash)
            || self.pending_sig_element_requests.contains(&element_hash);
        if !wanted {
            return Ok(());
        }
        self.pending_sig_element_requests.remove(&element_hash);

        // Recurse if this element's own parent is missing (fills a gap chain).
        let parent_hash = element.proposal.block().parent_hash();
        let parent_present = matches!(
            self.sig_chain_state.get_element(parent_hash.clone()).await,
            Ok(Some(_))
        );

        self.sig_chain_state
            .write_element(Arc::new(element))
            .await?;

        if !parent_present {
            self.broadcast_sig_element_request(parent_hash).await?;
        }

        self.drain_pending_sig_proposals(element_hash);
        Ok(())
    }

    /// Re-queue sig proposals parked on `element_hash` via the loopback channel.
    pub(crate) fn drain_pending_sig_proposals(
        &mut self,
        element_hash: crate::server::hera::core::SigElementHash<Tx>,
    ) {
        let to_replay = self
            .pending_sig_proposals
            .remove(&element_hash)
            .unwrap_or_default();
        for parked_msg in to_replay {
            debug!("Hera: re-queuing parked sig proposal via loopback");
            if self.tx_msg_loopback.send(parked_msg).is_err() {
                warn!("Hera: loopback channel closed; dropping parked sig proposal");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Advance sig-chain round
    // -----------------------------------------------------------------------

    pub async fn advance_sig_round(&mut self) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        self.sig_leader_context.advance_round();
        self.round_state
            .advance_round(&mut self.timer, &mut self.timer_enabled);
        self.try_commit()?;

        let new_round = self.round_state.round();
        debug!("Hera: sig-chain advancing to round {}", new_round);

        if self.sig_leader_context.leader() == self.my_id {
            if let Err(e) = self.handle_new_sig_round().await {
                error!("Hera: handle_new_sig_round after advance: {}", e);
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Blame handling (identical to Zeus)
    // -----------------------------------------------------------------------

    pub async fn handle_sig_blame(
        &mut self,
        blame_round: Round,
        auth: Signature<Id, Round>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        debug!("Hera: sig blame for round {}", blame_round);

        if self.sig_chain_state.highest_chain().proposal.round() == blame_round {
            return Ok(());
        }
        if self.round_state.got_qc {
            return Ok(());
        }

        #[allow(clippy::manual_div_ceil)]
        let qc_len = (self.settings.committee_config.num_nodes()
            + self.settings.committee_config.num_faults()
            + 1)
            / 2;

        let origin = auth.get_id();
        let blame_hash = Hash::ser_and_hash(&blame_round);
        let pk = self
            .crypto_system
            .system
            .get(&origin)
            .ok_or_else(|| anyhow!("Unknown blamer {}", origin))?;
        auth.verify_without_id_check(&blame_hash, pk)?;

        self.round_state.blame_map.insert(auth.get_id(), auth);
        if self.round_state.blame_map.len() != qc_len {
            return Ok(());
        }

        self.round_state.got_qc = true;
        let blame_map = std::mem::take(&mut self.round_state.blame_map);
        let mut qc = Certificate::empty();
        for (_, a) in blame_map {
            qc.add(a);
        }
        self.handle_sig_blame_qc(blame_round, qc).await
    }

    pub async fn on_sig_blame_qc(
        &mut self,
        blame_round: Round,
        qc: Certificate<Id, Round>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        if self.round_state.got_qc {
            return Ok(());
        }
        let blame_hash = Hash::ser_and_hash(&blame_round);
        qc.verify(&blame_hash, &self.crypto_system.system)?;
        self.handle_sig_blame_qc(blame_round, qc).await
    }

    async fn handle_sig_blame_qc(
        &mut self,
        blame_round: Round,
        qc: Certificate<Id, Round>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        self.round_state.got_qc = true;

        let pmsg = HeraMsg::<Tx>::SigBlameQC {
            round: blame_round,
            qc: qc.clone(),
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&pmsg).map_err(anyhow::Error::new)?);
        let _ = self
            .consensus_net
            .broadcast(&self.broadcast_peers, bytes)
            .await;

        self.sig_chain_state.add_qc(blame_round, qc);
        self.advance_sig_round().await
    }

    // -----------------------------------------------------------------------
    // Round timeout (OnSignatureTimeout)
    // -----------------------------------------------------------------------

    pub async fn on_round_timeout(&mut self) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        self.round_state.disable_timer(&mut self.timer_enabled);

        let blame_msg = self.round_state.round();
        let blame_hash = Hash::ser_and_hash(&blame_msg);
        let auth = Signature::<Id, Round>::new(blame_hash, self.my_id, &self.crypto_system.secret)?;
        let pmsg = HeraMsg::<Tx>::SigBlame {
            round: blame_msg,
            auth,
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&pmsg).map_err(anyhow::Error::new)?);
        let _ = self
            .consensus_net
            .broadcast(&self.broadcast_peers, bytes)
            .await;

        self.tx_msg_loopback
            .send(pmsg)
            .map_err(anyhow::Error::new)?;
        Ok(())
    }
}
