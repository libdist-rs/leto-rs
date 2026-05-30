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
use std::time::Instant;

impl<Tx> Hera<Tx>
where
    Tx: Transaction,
{
    // -----------------------------------------------------------------------
    // Hera: OnSignatureRoundPropose (sig-chain leader fires)
    // -----------------------------------------------------------------------

    /// Called when this node is the sig-chain leader and a new sig-chain round
    /// begins.  When proposal pacing is active (`propose_interval > 0`) and
    /// the last proposal was sent less than `propose_interval` ago, the
    /// proposal is deferred: `pending_propose_round` is set to the current
    /// round and `Ok(())` is returned without sending anything.  The
    /// `pace_timer` select arm fires `try_fire_pending_propose` once the
    /// interval elapses, which calls `do_propose` directly (no re-check).
    ///
    /// When pacing is off (default, `propose_interval == 0`), this is
    /// byte-for-byte the previous behavior.
    pub async fn handle_new_sig_round(&mut self) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let r = self.round_state.round();

        // --- Proposal pacing gate ---
        // Only active when propose_interval > 0 (env HERA_PROPOSE_INTERVAL_MS).
        if !self.propose_interval.is_zero() {
            if let Some(last) = self.last_propose_at {
                if last.elapsed() < self.propose_interval {
                    // Too soon — defer this proposal.
                    debug!(
                        "Hera[n{}]: PACE-DEFER round={} (elapsed={:?} < interval={:?})",
                        self.my_id,
                        r,
                        last.elapsed(),
                        self.propose_interval,
                    );
                    self.pending_propose_round = Some(r);
                    return Ok(());
                }
            }
        }

        // Update last_propose_at and clear any pending flag before sending.
        self.last_propose_at = Some(Instant::now());
        self.pending_propose_round = None;

        self.do_propose(r).await
    }

    /// Unconditionally build and broadcast a SigPropose for round `r`.
    /// Called by `handle_new_sig_round` (immediate path) and
    /// `try_fire_pending_propose` (deferred path).  Callers are responsible
    /// for updating `last_propose_at` / `pending_propose_round` before
    /// calling this function.
    pub(crate) async fn do_propose(
        &mut self,
        r: Round,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        // STALL DIAGNOSTIC: log every proposal so a stalled round can be
        // correlated to whether its leader actually proposed (and when).
        info!("Hera[n{}]: PROPOSE round={}", self.my_id, r);

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
        let peers = self.broadcast_peers.clone();
        let results = self.consensus_net.broadcast(&peers, bytes).await;
        for (peer, result) in peers.iter().zip(results.into_iter()) {
            if let Ok(h) = result {
                self.push_cancel_handler(*peer, h);
            }
        }

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
        crate::server::hera::core::INFLIGHT_LOOPBACK
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Update prev_attested_heights after the proposal is sent.
        // Read from the lock-free ArcSwap head_snapshots (published by data actor).
        // Stale reads are safe: attesting a slightly-old head costs one round.
        let all_ids = self.settings.committee_config.get_all_ids();
        for id in all_ids {
            let h = self
                .head_snapshots
                .get(&id)
                .map(|slot| (**slot.load()).height)
                .unwrap_or(0);
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
        sender: Id,
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

        // FuncMultiAttestationValid — sig verify only (synchronous).
        match self.multi_attestation_sig_valid(&attestation) {
            super::attestation::SigAttestationValidity::Valid => {}
            super::attestation::SigAttestationValidity::Invalid => {
                warn!("Hera: multi-attestation sig invalid in sig proposal");
                return Ok(());
            }
        }

        // Agreement is DECOUPLED from data availability (spec invariant 1): the
        // proposer only references data blocks it holds, so we agree on the
        // leader signature + parent sig-element and fetch the data lazily for
        // commit. This is what prevents a node momentarily missing one data
        // block from stranding (it can neither agree nor, alone, blame it out).
        //
        // Fire non-blocking fetch-from-SENDER hints (invariant 2) for each
        // referenced head, so the data is present by commit time. The data actor
        // skips heads already in its block_store. `sender` is the proposer/
        // relayer — a guaranteed holder of every referenced head by invariant 1.
        for h in attestation
            .envelope
            .heads
            .iter()
            .filter(|h| h.height > 0 || h.epoch > 0)
        {
            let _ = self.tx_fetch_hint.send((h.hash.clone(), sender));
        }

        // The PARENT sig-element must be present before we agree, so the
        // committer never walks into a chain gap. If missing, park and request
        // it from the SENDER (invariant 2) — the proposer built on it, so it has
        // it; never the data author.
        let parent_hash = proposal.block().parent_hash();
        match self.sig_chain_state.get_element(parent_hash.clone()).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                let our_highest = self.sig_chain_state.highest_chain().proposal.round();
                let proposal_round = proposal.round();
                info!(
                    "Hera[n{}]: PARKED sig proposal round={} for missing parent \
                     (req from sender {} our_highest={})",
                    self.my_id, proposal_round, sender, our_highest
                );
                let parked = HeraMsg::SigPropose {
                    proposal: proposal.clone(),
                    auth: auth.clone(),
                    attestation: attestation.clone(),
                    sender, // preserve the real sender for re-driven fetches
                };
                self.pending_sig_proposals
                    .entry(parent_hash.clone())
                    .or_default()
                    .push(parked);
                // If more than one element is missing, a ranged request
                // fills the gap in one RTT; otherwise fall back to single.
                let parent_round = proposal_round.saturating_sub(1);
                if parent_round > our_highest + 1 {
                    crate::server::hera::core::BACKFILL_REQUESTS.fetch_add(
                        (parent_round - our_highest) as i64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    let msg = HeraMsg::<Tx>::SigElementRangeRequest {
                        source: self.my_id,
                        from_round: our_highest + 1,
                        to_round: parent_round,
                    };
                    let bytes =
                        bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
                    if let Ok(h) = self.consensus_net.send(sender, bytes).await {
                        self.push_cancel_handler(sender, h);
                    }
                } else {
                    self.request_sig_element_from(parent_hash, sender).await?;
                }
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
        let r = proposal.round();
        let batch = Batch {
            payload: vec![attestation],
        };
        self.sig_chain_state
            .update_highest_chain(proposal, auth, batch)
            .await?;
        info!("Hera[n{}]: AGREED round={} -> advancing", self.my_id, r);

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
        if let Ok(h) = self.consensus_net.send(next_leader, bytes).await {
            self.push_cancel_handler(next_leader, h);
        }
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
        if !self
            .pending_sig_element_requests
            .insert(target_hash.clone())
        {
            return Ok(());
        }
        let msg = HeraMsg::<Tx>::SigElementRequest {
            source: self.my_id,
            request: Request::new(target_hash),
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
        let peers = self.broadcast_peers.clone();
        let results = self.consensus_net.broadcast(&peers, bytes).await;
        for (peer, result) in peers.iter().zip(results.into_iter()) {
            if let Ok(h) = result {
                self.push_cancel_handler(*peer, h);
            }
        }
        Ok(())
    }

    /// Request a missing sig-chain element from a specific `holder`
    /// (request-from-sender, invariant 2): the node that referenced the element
    /// to us provably has it (invariant 1). Falls back to a broadcast if the
    /// holder is unknown (`holder == 0` sentinel) or is ourselves.
    pub(crate) async fn request_sig_element_from(
        &mut self,
        target_hash: crate::server::hera::core::SigElementHash<Tx>,
        holder: Id,
    ) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        if holder == self.my_id {
            return self.broadcast_sig_element_request(target_hash).await;
        }
        // Dedupe: skip if a fetch for this element is already outstanding.
        if !self
            .pending_sig_element_requests
            .insert(target_hash.clone())
        {
            return Ok(());
        }
        let msg = HeraMsg::<Tx>::SigElementRequest {
            source: self.my_id,
            request: Request::new(target_hash),
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
        if let Ok(h) = self.consensus_net.send(holder, bytes).await {
            self.push_cancel_handler(holder, h);
        }
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
            if let Ok(h) = self.consensus_net.send(source, bytes).await {
                self.push_cancel_handler(source, h);
            }
        }
        Ok(())
    }

    /// Admit a fetched sig-chain element and re-drive any proposals parked on
    /// it. The element is content-addressed: we store it under its true hash
    /// (`ser_and_hash`) and only re-drive proposals that referenced THAT hash,
    /// so a peer cannot substitute a different element. If the fetched
    /// element's own parent is also missing, fetch it too (fills a chain of
    /// gaps).
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
        let element_round = element.proposal.round(); // capture before move
        let parent_present = matches!(
            self.sig_chain_state.get_element(parent_hash.clone()).await,
            Ok(Some(_))
        );

        self.sig_chain_state
            .write_element(Arc::new(element))
            .await?;

        if !parent_present {
            // The parent is missing. Rather than requesting one element at a
            // time, request a range: from just above our current highest to
            // the parent's round (parent_round = element_round - 1).
            let our_highest = self.sig_chain_state.highest_chain().proposal.round();
            let parent_round = element_round.saturating_sub(1);
            if parent_round > our_highest + 1 {
                // Multiple ancestors missing — use range request.
                self.broadcast_sig_range_request(our_highest + 1, parent_round)
                    .await?;
            } else {
                // At most one ancestor missing — single-element fallback.
                self.broadcast_sig_element_request(parent_hash).await?;
            }
        }

        self.drain_pending_sig_proposals(element_hash);
        Ok(())
    }

    /// Re-queue sig proposals parked on `element_hash` via the loopback
    /// channel.
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
            if self.tx_msg_loopback.send(parked_msg).is_ok() {
                crate::server::hera::core::INFLIGHT_LOOPBACK
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else {
                warn!("Hera: loopback channel closed; dropping parked sig proposal");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Ranged sig-element backfill (O(1) RTT for a K-element gap).
    // -----------------------------------------------------------------------

    /// Broadcast a SigElementRangeRequest covering rounds `[from_round,
    /// to_round]`. The first respondent that holds the range fills the gap;
    /// subsequent responses are deduplicated by
    /// `on_sig_element_range_response`.
    pub(crate) async fn broadcast_sig_range_request(
        &mut self,
        from_round: crate::Round,
        to_round: crate::Round,
    ) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        if from_round > to_round {
            return Ok(());
        }
        crate::server::hera::core::BACKFILL_REQUESTS.fetch_add(
            (to_round - from_round + 1) as i64,
            std::sync::atomic::Ordering::Relaxed,
        );
        let msg = HeraMsg::<Tx>::SigElementRangeRequest {
            source: self.my_id,
            from_round,
            to_round,
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
        let peers = self.broadcast_peers.clone();
        let results = self.consensus_net.broadcast(&peers, bytes).await;
        for (peer, result) in peers.iter().zip(results.into_iter()) {
            if let Ok(h) = result {
                self.push_cancel_handler(*peer, h);
            }
        }
        Ok(())
    }

    /// Serve a peer's SigElementRangeRequest. Walk backward from our
    /// `highest_chain` collecting elements whose round is in `[from, to]`,
    /// capped at `MAX_RANGE_RESPONSE`. Elements are sent in ascending round
    /// order (ancestor-first) so the receiver can store them left-to-right
    /// and satisfy parent-present checks.
    pub async fn on_sig_element_range_request(
        &mut self,
        source: Id,
        from_round: crate::Round,
        to_round: crate::Round,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        use crate::server::hera::core::MAX_RANGE_RESPONSE;
        let highest = self.sig_chain_state.highest_chain();
        if highest.proposal.round() < from_round {
            // We don't have anything in range; skip.
            return Ok(());
        }
        let mut elements: Vec<crate::server::hera::core::SigElement<Tx>> = Vec::new();
        let mut cur_hash = self.sig_chain_state.highest_hash();
        let mut cur_element = highest;
        loop {
            let r = cur_element.proposal.round();
            if r < from_round {
                break;
            }
            if r <= to_round {
                elements.push((*cur_element).clone());
            }
            if elements.len() >= MAX_RANGE_RESPONSE {
                break;
            }
            if r == 0 {
                break; // reached genesis
            }
            let parent_hash = cur_element.proposal.block().parent_hash();
            if parent_hash == cur_hash {
                break; // circular / genesis guard
            }
            cur_hash = parent_hash.clone();
            match self.sig_chain_state.get_element(parent_hash).await {
                Ok(Some(e)) => cur_element = Arc::new(e),
                Ok(None) => break, // we don't have this ancestor
                Err(_) => break,
            }
        }
        if elements.is_empty() {
            return Ok(());
        }
        // Reverse to ascending round order (ancestor-first).
        elements.reverse();
        let msg = HeraMsg::<Tx>::SigElementRangeResponse { elements };
        let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
        if let Ok(h) = self.consensus_net.send(source, bytes).await {
            self.push_cancel_handler(source, h);
        }
        Ok(())
    }

    /// Admit a batch of sig-chain elements delivered by a
    /// SigElementRangeResponse. Stores elements in the received order
    /// (ancestor-first) so each element's parent is already stored when
    /// the next one is written. Retries any proposals parked on elements
    /// within the batch.
    pub async fn on_sig_element_range_response(
        &mut self,
        elements: Vec<crate::server::hera::core::SigElement<Tx>>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        crate::server::hera::core::BACKFILL_RESPONSES
            .fetch_add(elements.len() as i64, std::sync::atomic::Ordering::Relaxed);
        for element in elements {
            let element_hash: crate::server::hera::core::SigElementHash<Tx> =
                Hash::ser_and_hash(&element);
            // Only store elements we want (parked on or explicitly requested)
            // or that correspond to known gaps. Accept if: it's in pending
            // proposals, we issued a request for it, or its round is ≤ our
            // current round (covering catch-up range fills).
            let cur_round = self.round_state.round();
            let wanted = self.pending_sig_proposals.contains_key(&element_hash)
                || self.pending_sig_element_requests.contains(&element_hash)
                || element.proposal.round() <= cur_round;
            if !wanted {
                continue;
            }
            // Remove from pending requests if present.
            self.pending_sig_element_requests.remove(&element_hash);
            // Write to storage (idempotent: re-writing an existing element is a no-op).
            self.sig_chain_state
                .write_element(Arc::new(element))
                .await?;
            // Drain any proposals parked on this hash.
            self.drain_pending_sig_proposals(element_hash);
        }
        Ok(())
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
        debug!(
            "Hera[n{}]: sig-chain advancing to round {}",
            self.my_id, new_round
        );

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
        highest_round: Round,
        highest_hash: crate::server::hera::core::SigElementHash<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
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
        // Track the highest chain reported across this round's blames (the next
        // leader extends it). `origin` is a holder of that element (invariant 1).
        let better = match &self.round_state.blame_best {
            Some((r, _)) => highest_round > *r,
            None => true,
        };
        if better {
            self.round_state.blame_best = Some((highest_round, highest_hash));
        }
        info!(
            "Hera[n{}]: BLAME-RX round={} from={} map={}/{} cur={} reported_highest={}",
            self.my_id,
            blame_round,
            origin,
            self.round_state.blame_map.len(),
            qc_len,
            self.round_state.round(),
            highest_round,
        );
        if self.round_state.blame_map.len() < qc_len {
            return Ok(());
        }

        info!(
            "Hera[n{}]: BLAME-QC formed for round {} ({} blames)",
            self.my_id, blame_round, qc_len
        );
        self.round_state.got_qc = true;
        let blame_map = std::mem::take(&mut self.round_state.blame_map);
        let mut qc = Certificate::empty();
        for (_, a) in blame_map {
            qc.add(a);
        }
        let (best_round, best_hash) = self
            .round_state
            .blame_best
            .clone()
            .unwrap_or_else(|| (Round::MIN, self.sig_chain_state.highest_hash()));
        self.handle_sig_blame_qc(blame_round, qc, best_round, best_hash)
            .await
    }

    pub async fn on_sig_blame_qc(
        &mut self,
        blame_round: Round,
        qc: Certificate<Id, Round>,
        highest_round: Round,
        highest_hash: crate::server::hera::core::SigElementHash<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        info!(
            "Hera[n{}]: BLAMEQC-RX round={} cur={} got_qc={} reported_highest={}",
            self.my_id,
            blame_round,
            self.round_state.round(),
            self.round_state.got_qc,
            highest_round,
        );
        if self.round_state.got_qc {
            return Ok(());
        }
        let blame_hash = Hash::ser_and_hash(&blame_round);
        qc.verify(&blame_hash, &self.crypto_system.system)?;
        self.handle_sig_blame_qc(blame_round, qc, highest_round, highest_hash)
            .await
    }

    async fn handle_sig_blame_qc(
        &mut self,
        blame_round: Round,
        qc: Certificate<Id, Round>,
        highest_round: Round,
        highest_hash: crate::server::hera::core::SigElementHash<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        self.round_state.got_qc = true;

        let pmsg = HeraMsg::<Tx>::SigBlameQC {
            round: blame_round,
            qc: qc.clone(),
            highest_round,
            highest_hash: highest_hash.clone(),
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&pmsg).map_err(anyhow::Error::new)?);
        let peers = self.broadcast_peers.clone();
        let results = self.consensus_net.broadcast(&peers, bytes).await;
        for (peer, result) in peers.iter().zip(results.into_iter()) {
            if let Ok(h) = result {
                self.push_cancel_handler(*peer, h);
            }
        }

        self.sig_chain_state.add_qc(blame_round, qc);

        // Forward catch-up of chain DATA: if the quorum's reported highest is
        // ahead of ours, fetch the missing range in a single round-trip rather
        // than one-element-per-RTT. The gap is (our_highest+1 .. reported_highest).
        let our_highest_round = self.sig_chain_state.highest_chain().proposal.round();
        if highest_round > our_highest_round {
            if let Ok(None) = self.sig_chain_state.get_element(highest_hash.clone()).await {
                if highest_round > our_highest_round + 1 {
                    // Large gap: request the whole range at once.
                    self.broadcast_sig_range_request(our_highest_round + 1, highest_round)
                        .await?;
                } else {
                    // Small gap (1 element): fall back to single-element request.
                    self.broadcast_sig_element_request(highest_hash).await?;
                }
            }
        }

        info!(
            "Hera[n{}]: BLAMEQC-ADVANCE past round={}",
            self.my_id, blame_round
        );
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
        // STALL DIAGNOSTIC: which round timed out, who its leader was, and how far
        // the chain got. Correlate across nodes: did `leader` ever log PROPOSE for
        // this round? If not, the leader fell behind (didn't reach this round in
        // time); if yes, the proposal didn't propagate/agree.
        info!(
            "Hera[n{}]: STALL-TIMEOUT round={} leader={} highest_chained={}",
            self.my_id,
            blame_msg,
            self.sig_leader_context.leader(),
            self.sig_chain_state.highest_chain().proposal.round(),
        );
        let blame_hash = Hash::ser_and_hash(&blame_msg);
        let auth = Signature::<Id, Round>::new(blame_hash, self.my_id, &self.crypto_system.secret)?;
        // Carry our highest sig-chain element so the next leader can extend the
        // true highest chain (it fetches the element from a blamer if missing —
        // request-from-sender, invariant 2).
        let highest_round = self.sig_chain_state.highest_chain().proposal.round();
        let highest_hash = self.sig_chain_state.highest_hash();
        let pmsg = HeraMsg::<Tx>::SigBlame {
            round: blame_msg,
            auth,
            highest_round,
            highest_hash,
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&pmsg).map_err(anyhow::Error::new)?);
        let peers = self.broadcast_peers.clone();
        let results = self.consensus_net.broadcast(&peers, bytes).await;
        for (peer, result) in peers.iter().zip(results.into_iter()) {
            if let Ok(h) = result {
                self.push_cancel_handler(*peer, h);
            }
        }

        self.tx_msg_loopback
            .send(pmsg)
            .map_err(anyhow::Error::new)?;
        crate::server::hera::core::INFLIGHT_LOOPBACK
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}
