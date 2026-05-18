/// Zeus sig-plane handlers.
///
/// Zeus: OnSignatureRoundPropose, OnSignatureBlame, OnSignatureBlameQC.
/// These are canonical Leto handlers with the substitution that block-elements
/// carry `Attestation<Tx>` instead of arbitrary mempool payloads.
///
/// Reference: zeus.tex §sig-plane handlers (lines 555+).
use crate::{
    server::zeus::{core::DataTimerKind, Zeus},
    types::{Attestation, Block, Certificate, Proposal, Signature, Transaction, ZeusMsg},
    Id, Round,
};
use anyhow::{anyhow, Result};
use crypto::hash::Hash;
use log::*;
use mempool::Batch;
use serde::Serialize;

impl<Tx> Zeus<Tx>
where
    Tx: Transaction,
{
    // -----------------------------------------------------------------------
    // Zeus: OnSignatureRoundPropose (sig-chain leader fires)
    // -----------------------------------------------------------------------

    /// Called when this node is the sig-chain leader and should propose an
    /// attestation for the current round.
    ///
    /// Zeus: OnSignatureRoundPropose — substitutes canonical Leto's
    /// mempool-pop with `FuncMakeAttestation`.
    ///
    /// Attestation-freshness rule: only propose if `latest_eleader_block` has
    /// a height strictly greater than `last_attested_data_height`.  If no fresh
    /// block is available, arm the TimerData and wait.  When a fresh block
    /// arrives (`on_data_propose`), it will call back into this function.
    pub async fn handle_new_sig_round(&mut self) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let r = self.round_state.round();

        // --- Freshness check (attestation-freshness rule) ---
        let latest_h = self
            .latest_eleader_block
            .as_ref()
            .map(|b| b.envelope.height)
            .unwrap_or(0);

        if latest_h <= self.last_attested_data_height {
            // No fresh block available; arm the timer and enter waiting state.
            debug!(
                "Zeus: rleader round {}: no fresh block \
                 (latest_h={} <= last_attested={}); waiting",
                r, latest_h, self.last_attested_data_height
            );
            self.rleader_waiting_fresh = true;
            self.arm_data_timer(DataTimerKind::RleaderWaitingFresh);
            return Ok(());
        }

        // Fresh block available — proceed with the propose.
        self.rleader_waiting_fresh = false;
        self.disarm_data_timer();

        debug!(
            "Zeus: sig-chain leader proposing attestation for round {} \
             (latest_h={} last_attested={})",
            r, latest_h, self.last_attested_data_height
        );

        // Zeus: FuncMakeAttestation — pin the highest-height admitted block,
        // not the data_chain head, so that bursts (multiple data blocks landing
        // between attestations) are covered by the prefix-commit projection.
        //
        // We override `make_attestation`'s internal use of `data_chain.head_hash`
        // by temporarily ensuring `latest_eleader_block` is the data chain head.
        // Since `latest_eleader_block` tracks admitted blocks (set in
        // `on_data_propose`), and `data_chain.head_hash` is updated there too,
        // the two are in sync: `latest_eleader_block` == the block at head_hash.
        // `make_attestation` reads `data_chain.head_hash` from the store, which
        // is already the latest admitted block.  No override needed.
        let attestation = self.make_attestation(r)?;

        // Wrap attestation in a sig-chain Block (batch_hash = H(attestation envelope),
        // parent = sig-chain head hash)
        let att_hash = attestation.hash().clone();
        // Re-interpret att_hash as BatchHash<Attestation<Tx>>.
        // SAFETY: Hash<T> is a [u8;32] newtype over a phantom T.  Both
        // Hash<AttestationEnvelope<Tx>> and BatchHash<Attestation<Tx>>
        // alias Hash<_>; the underlying bytes are identical.
        let batch_hash: mempool::BatchHash<Attestation<Tx>> = unsafe {
            let src: crypto::hash::Hash<crate::types::AttestationEnvelope<Tx>> = att_hash;
            std::mem::transmute(src)
        };
        let prev_hash = self.sig_chain_state.highest_hash();
        let block = Block::<Id, Attestation<Tx>, Round>::new(batch_hash, prev_hash);
        let proposal = Proposal::new(block, r, None);

        let prop_hash = Hash::ser_and_hash(&proposal);
        let auth = Signature::<Id, Proposal<Id, Attestation<Tx>, Round>>::new(
            prop_hash,
            self.my_id,
            &self.crypto_system.secret,
        )?;

        let msg = ZeusMsg::SigPropose {
            proposal: proposal.clone(),
            auth: auth.clone(),
            attestation: attestation.clone(),
            sender: self.my_id,
        };

        // Serialize once for broadcast
        let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
        let results = self
            .consensus_net
            .broadcast(&self.broadcast_peers, bytes)
            .await;
        let handlers: Vec<_> = results.into_iter().filter_map(|r| r.ok()).collect();
        self.round_state.add_handlers(handlers);

        // Yield once before pushing to loopback so that TCP IO tasks on
        // other Tokio worker threads can deliver pending network messages
        // (other nodes' proposals / blame) before this node's loopback
        // cascade continues.  Without this yield, a single node can race
        // through thousands of sig-chain rounds via the
        // loopback → is_ready → advance → loopback cycle while other
        // nodes' proposals sit in the TCP receive buffer unprocessed,
        // causing multi-round divergence and blame-driven stalls.
        tokio::task::yield_now().await;

        // Loopback
        self.tx_msg_loopback
            .send(ZeusMsg::SigPropose {
                proposal,
                auth,
                attestation,
                sender: self.my_id,
            })
            .map_err(anyhow::Error::new)?;

        // Record the height we just attested so the next call to
        // handle_new_sig_round can enforce the freshness rule.
        self.last_attested_data_height = latest_h;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Zeus: OnSignaturePropose (all nodes receive SigPropose)
    // -----------------------------------------------------------------------

    /// Zeus: OnSignaturePropose — canonical Leto OnPropose with
    /// FuncAttestationValid added to FuncChainValid.
    pub async fn handle_sig_proposal(
        &mut self,
        proposal: Proposal<Id, Attestation<Tx>, Round>,
        auth: Signature<Id, Proposal<Id, Attestation<Tx>, Round>>,
        attestation: Attestation<Tx>,
        _sender: Id,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        // Zeus: OnSignatureRoundPropose — check correct round
        let cur_round = self.round_state.round();
        if proposal.round() != cur_round {
            // Future/past round — round context already handled buffering.
            warn!(
                "Zeus: sig proposal for wrong round {}, current {}",
                proposal.round(),
                cur_round
            );
            return Ok(());
        }

        // Check leader
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

        // Zeus: FuncAttestationValid (tri-state)
        let msg_for_park = ZeusMsg::SigPropose {
            proposal: proposal.clone(),
            auth: auth.clone(),
            attestation: attestation.clone(),
            sender: _sender,
        };
        match self.attestation_valid(&attestation, msg_for_park) {
            super::attestation::AttestationValidity::Valid => {}
            super::attestation::AttestationValidity::Invalid => {
                warn!("Zeus: attestation invalid in sig proposal");
                return Ok(());
            }
            super::attestation::AttestationValidity::Parked(h) => {
                // Emit a DataRequest for the missing block
                debug!("Zeus: attestation parked waiting for {:?}", h);
                self.broadcast_data_request(h).await?;
                return Ok(());
            }
        }

        // Valid proposal — admit
        self.on_correct_sig_proposal(proposal, auth, attestation)
            .await
    }

    async fn on_correct_sig_proposal(
        &mut self,
        proposal: Proposal<Id, Attestation<Tx>, Round>,
        auth: Signature<Id, Proposal<Id, Attestation<Tx>, Round>>,
        attestation: Attestation<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        // Relay to next leader
        self.relay_sig_proposal(proposal.clone(), auth.clone())
            .await?;

        // Update sig-chain state
        let batch = Batch {
            payload: vec![attestation],
        };
        self.sig_chain_state
            .update_highest_chain(proposal, auth, batch)
            .await?;

        // Advance sig-chain round
        self.advance_sig_round().await?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Zeus: relay SigPropose to next sig-chain leader
    // -----------------------------------------------------------------------

    async fn relay_sig_proposal(
        &mut self,
        proposal: Proposal<Id, Attestation<Tx>, Round>,
        auth: Signature<Id, Proposal<Id, Attestation<Tx>, Round>>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        let next_leader = self.sig_leader_context.next_leader();
        if next_leader == self.my_id {
            return Ok(());
        }
        // Re-interpret the batch_hash as an attestation envelope hash.
        // SAFETY: BatchHash<Attestation<Tx>> and Hash<AttestationEnvelope<Tx>>
        // are both Hash<_> newtypes sharing the same [u8;32] layout.
        let att_hash: crypto::hash::Hash<crate::types::AttestationEnvelope<Tx>> = unsafe {
            let bh: mempool::BatchHash<Attestation<Tx>> = proposal.block().batch_hash().clone();
            std::mem::transmute(bh)
        };
        let relay = ZeusMsg::SigRelay {
            proposal,
            auth,
            att_hash,
            sender: self.my_id,
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&relay).map_err(anyhow::Error::new)?);
        if let Ok(h) = self.consensus_net.send(next_leader, bytes).await {
            self.round_state.add_handler(h);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Zeus: advance sig-chain round
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
        info!("Zeus: sig-chain advancing to round {}", new_round);

        // Per-node TimerData (zeus.tex Def 8.4): arm on every sig-chain round
        // entry.  If the rleader immediately proposes (fresh block available),
        // `handle_new_sig_round` will disarm it.  Otherwise it stays armed.
        // The RleaderWaitingFresh path in handle_new_sig_round replaces this
        // timer's kind when the rleader enters the waiting state.
        self.arm_data_timer(DataTimerKind::TimerDataRoundEntry);

        // If we are the new sig-chain leader, queue a propose trigger.
        if self.sig_leader_context.leader() == self.my_id {
            if let Err(e) = self.handle_new_sig_round().await {
                error!("Zeus: handle_new_sig_round after advance: {}", e);
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Zeus: OnSignatureBlame (canonical Leto OnBlame)
    // -----------------------------------------------------------------------

    pub async fn handle_sig_blame(
        &mut self,
        blame_round: Round,
        auth: Signature<Id, Round>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        debug!("Zeus: sig blame for round {}", blame_round);

        // Ignore if we already have a proposal for this round
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

    /// Zeus: BlameQC received from network
    pub async fn on_sig_blame_qc(
        &mut self,
        blame_round: Round,
        qc: Certificate<Id, Round>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        debug!("Zeus: sig blame QC for round {}", blame_round);
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

        let pmsg = ZeusMsg::<Tx>::SigBlameQC {
            round: blame_round,
            qc: qc.clone(),
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&pmsg).map_err(anyhow::Error::new)?);
        let results = self
            .consensus_net
            .broadcast(&self.broadcast_peers, bytes)
            .await;
        let handlers: Vec<_> = results.into_iter().filter_map(|r| r.ok()).collect();
        self.round_state.add_handlers(handlers);

        self.sig_chain_state.add_qc(blame_round, qc);
        self.advance_sig_round().await
    }

    // -----------------------------------------------------------------------
    // Zeus: OnSignatureTimeout (canonical Leto OnTimeout)
    // -----------------------------------------------------------------------

    pub async fn on_round_timeout(&mut self) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        self.round_state.disable_timer(&mut self.timer_enabled);

        let blame_msg = self.round_state.round();
        let blame_hash = Hash::ser_and_hash(&blame_msg);
        let auth = Signature::<Id, Round>::new(blame_hash, self.my_id, &self.crypto_system.secret)?;
        let pmsg = ZeusMsg::<Tx>::SigBlame {
            round: blame_msg,
            auth,
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&pmsg).map_err(anyhow::Error::new)?);
        let results = self
            .consensus_net
            .broadcast(&self.broadcast_peers, bytes)
            .await;
        let handlers: Vec<_> = results.into_iter().filter_map(|r| r.ok()).collect();
        self.round_state.add_handlers(handlers);

        self.tx_msg_loopback
            .send(pmsg)
            .map_err(anyhow::Error::new)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Helper: broadcast a DataRequest for a missing data block
    // -----------------------------------------------------------------------

    pub(crate) async fn broadcast_data_request(
        &mut self,
        target_hash: crate::server::zeus::chain_state::DataBlockHash<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        let msg = ZeusMsg::<Tx>::DataRequest {
            target_hash,
            source: self.my_id,
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
        let results = self
            .consensus_net
            .broadcast(&self.broadcast_peers, bytes)
            .await;
        let handlers: Vec<_> = results.into_iter().filter_map(|r| r.ok()).collect();
        self.round_state.add_handlers(handlers);
        Ok(())
    }
}
