/// Zeus data-plane handlers.
///
/// Implements:
///   - `on_eleader_propose`: eleader proposes a new DataBlock.
///   - `on_data_propose`: all nodes handle incoming DataPropose.
///   - `on_data_request`: Zeus OnDataRequest handler.
///   - `on_data_response`: Zeus OnDataResponse handler with park-drain.
///
/// Reference: zeus.tex §data-plane handlers (lines 319+).
use crate::{
    server::zeus::{
        chain_state::{conflicts_data_prefix, eleader, epoch_gate_valid},
        Zeus,
    },
    types::{DataBlock, DataBlockEnvelope, DataBlockSig, Transaction, ZeusMsg},
    Id,
};
use anyhow::Result;
use crypto::hash::Hash;
use log::*;
use mempool::Batch;
use once_cell::sync::OnceCell;
use serde::Serialize;
use std::marker::PhantomData;
use std::sync::Arc;

impl<Tx> Zeus<Tx>
where
    Tx: Transaction,
{
    // -----------------------------------------------------------------------
    // Eleader propose loop (zeus.tex §OnEleaderPropose / FuncMakeDataBlock)
    // -----------------------------------------------------------------------

    /// Zeus: eleader proposes a new DataBlock.
    ///
    /// Called when `self.is_eleader()` and the RRBatcher has a batch ready.
    /// Constructs a DataBlock extending the in-flight proposed tip (not the
    /// admitted head) and multicasts it.
    ///
    /// Pipelining: up to `eleader_pipeline_depth` (W) blocks may be in-flight
    /// simultaneously.  In-flight count is
    /// `eleader_proposed_height − data_chain.head_height`.  When the window
    /// is full the batch is dropped; `on_data_propose` will re-prime the
    /// batcher via `NewRound` when the next block is admitted.
    ///
    /// Parent-hash chaining: the envelope's parent_hash is set to
    /// `last_proposed_hash` (the hash of the most-recently proposed block),
    /// not `data_chain.head_hash` (the admitted head), so the in-flight chain
    /// is self-consistent even when multiple blocks are outstanding.
    ///
    /// The eleader's own admission is deferred via `tx_msg_loopback` so the
    /// select loop can immediately interleave the next proposal (from the next
    /// batch already sealed by the batcher) with admission of this one.  A
    /// `NewRound` is sent to the batcher right after updating the pipeline
    /// counters — before admission — so the batcher seals block N+1 in parallel
    /// with the local admission of block N.  This is the key change that
    /// removes the old one-in-flight serialization: `NewRound` no longer
    /// waits for `on_data_propose` to complete.
    pub async fn on_eleader_propose(
        &mut self,
        batch: Batch<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        // ----------------------------------------------------------------
        // Pipeline gate: drop if window is full.
        // ----------------------------------------------------------------
        let in_flight = self
            .eleader_proposed_height
            .saturating_sub(self.data_chain.head_height);
        if in_flight >= self.eleader_pipeline_depth as u64 {
            debug!(
                "Zeus: eleader window full (in_flight={} >= depth={}), dropping batch",
                in_flight, self.eleader_pipeline_depth
            );
            // on_data_propose will re-prime the batcher via NewRound when the
            // next block is admitted and the window drops below depth.
            return Ok(());
        }

        // ----------------------------------------------------------------
        // Determine parent hash and height for this proposal.
        // ----------------------------------------------------------------
        let (parent_hash, height) = if let Some(ref lph) = self.last_proposed_hash {
            // Steady-state pipeline: chain off the last proposed tip, not the
            // admitted head.  This keeps the in-flight chain hash-linked even
            // when W > 1 blocks are outstanding.
            let h = self.eleader_proposed_height + 1;
            (lph.clone(), h)
        } else {
            // Epoch entry (first proposal in this epoch, `last_proposed_hash`
            // is `None`): extend from the latest committed attestation's data
            // head, per the strict extension rule (zeus.tex §OnEleaderPropose).
            //
            // Post-`advance_to_epoch`: `data_chain.head_hash` has already been
            // truncated to `H(D*)` and `last_proposed_hash` was reset to `None`.
            // `latest_committed_attestation` will return the same D*, so the
            // new eleader's first block's parent is `H(D*)` — enforcing the
            // strict extension rule transitively via the truncation.
            // No separate gate is needed here.
            //
            // TODO(zeus-view-change): enforce strict extension here — RESOLVED.
            // The truncation in `advance_to_epoch` (core.rs) makes the head IS
            // H(D*), so the parent chain here is trivially correct.
            let d_ext = {
                let d_star_opt = crate::server::zeus::chain_state::latest_committed_attestation(
                    &self.commit_lock,
                );
                match d_star_opt {
                    None => DataBlock::<Tx>::genesis(),
                    Some(a) => {
                        // Look up the pinned block by its hash.
                        let pinned_hash = &a.envelope.data_block_hash;
                        self.data_block_store
                            .get(pinned_hash)
                            .cloned()
                            .unwrap_or_else(DataBlock::<Tx>::genesis)
                    }
                }
            };
            (d_ext.hash().clone(), d_ext.envelope.height + 1)
        };

        debug!(
            "Zeus: on_eleader_propose: in_flight={} next_height={} parent={:?}",
            in_flight, height, parent_hash,
        );

        let payload = Arc::new(batch.payload);
        let envelope = DataBlockEnvelope {
            epoch: self.current_epoch,
            height,
            payload,
            parent_hash,
        };

        // Sign the envelope.
        let env_hash = Hash::ser_and_hash(&envelope);
        let raw = self.crypto_system.secret.sign(env_hash.as_ref())?;

        let block = DataBlock {
            envelope,
            sig: DataBlockSig {
                raw,
                signer: self.my_id,
                _phantom: PhantomData,
            },
            cached_hash: OnceCell::new(),
        };

        info!(
            "Zeus: eleader proposing data block height={} epoch={} in_flight={}",
            block.envelope.height,
            block.envelope.epoch,
            in_flight + 1,
        );

        // Update pipeline state BEFORE broadcasting (idempotent on error).
        self.eleader_proposed_height = block.envelope.height;
        self.last_proposed_hash = Some(block.hash().clone());

        // ----------------------------------------------------------------
        // Prime the batcher for the next block immediately.
        // ----------------------------------------------------------------
        if in_flight + 1 < self.eleader_pipeline_depth as u64 {
            let n = self.settings.committee_config.num_nodes();
            let _ =
                self.tx_consensus_to_batcher
                    .send(crate::server::BatcherConsensusMsg::NewRound {
                        leader: crate::server::zeus::chain_state::eleader(self.current_epoch, n),
                    });
        }

        // ----------------------------------------------------------------
        // Broadcast to peers.
        // ----------------------------------------------------------------
        let msg = ZeusMsg::DataPropose {
            block: block.clone(),
            sender: self.my_id,
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
        let results = self
            .consensus_net
            .broadcast(&self.broadcast_peers, bytes)
            .await;
        let handlers: Vec<_> = results.into_iter().filter_map(|r| r.ok()).collect();
        self.round_state.add_handlers(handlers);

        // ----------------------------------------------------------------
        // Defer own admission via loopback.
        // ----------------------------------------------------------------
        let loopback_msg = ZeusMsg::DataPropose {
            block,
            sender: self.my_id,
        };
        if self.tx_msg_loopback.send(loopback_msg).is_err() {
            warn!("Zeus: loopback channel closed; own admission dropped");
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Zeus: OnEleaderPropose — receive and validate a DataBlock
    // -----------------------------------------------------------------------

    /// Zeus: OnEleaderPropose handler (all nodes).
    ///
    /// Validates the incoming DataBlock and, if valid, admits it via the
    /// three-case eager-invariant admission:
    ///
    ///   Case (a) — fast path: block extends the current head.
    ///   Case (b) — bridge: parent is in store but is not the head.
    ///   Case (c) — parked: parent not in store; emit DataRequest.
    ///
    /// Reference: zeus.tex lines 340–390.
    pub async fn on_data_propose(
        &mut self,
        block: DataBlock<Tx>,
        sender: Id,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let n = self.settings.committee_config.num_nodes();
        let expected_eleader = eleader(block.envelope.epoch, n);

        // 1. Check sender is the correct eleader for this block's epoch.
        if sender != expected_eleader {
            debug!(
                "Zeus: data propose from non-eleader {} (expected {}), epoch {}",
                sender, expected_eleader, block.envelope.epoch
            );
            return Ok(());
        }

        // 2. Verify eleader signature on the envelope.
        if !self.verify_data_block_sig(&block) {
            warn!(
                "Zeus: data block signature verification failed from sender={} epoch={} height={}",
                sender, block.envelope.epoch, block.envelope.height
            );
            return Ok(());
        }

        // 3. Epoch gate (single-block validity).
        if !epoch_gate_valid(&block, self.current_epoch, &self.admitted_change_qcs) {
            debug!(
                "Zeus: data block epoch gate failed: block.epoch={}, cur={}",
                block.envelope.epoch, self.current_epoch
            );
            return Ok(());
        }

        let block_hash = block.hash().clone();
        debug!(
            "Zeus: data block hash={:?} parent_hash={:?} chain_head={:?}",
            block_hash, block.envelope.parent_hash, self.data_chain.head_hash
        );

        // 4. Idempotent: skip if already in store.
        if self.data_block_store.contains_key(&block_hash) {
            return Ok(());
        }

        // 5. Equivocation check. If child_of[parent_hash] is already set to a
        //    *different* hash, the eleader has signed two distinct children of the same
        //    parent.  Algorithm/zeus.tex lines 409-417.
        if let Some(existing_child_hash) = self
            .data_chain
            .child_of
            .get(&block.envelope.parent_hash)
            .cloned()
        {
            if existing_child_hash != block_hash {
                warn!(
                    "Zeus: equivocation detected at parent {:?} (existing child {:?}, new {:?})",
                    block.envelope.parent_hash, existing_child_hash, block_hash
                );
                // Look up the existing block to build the blame payload.
                if let Some(existing_block) =
                    self.data_block_store.get(&existing_child_hash).cloned()
                {
                    let epoch = block.envelope.epoch;
                    let height = block.envelope.height;
                    match self.make_equivocation_blame(epoch, height, existing_block, block) {
                        Ok(blame) => {
                            let msg = ZeusMsg::EleaderBlame(blame.clone());
                            let bytes = bytes::Bytes::from(
                                bincode::serialize(&msg).map_err(anyhow::Error::new)?,
                            );
                            let results = self
                                .consensus_net
                                .broadcast(&self.broadcast_peers, bytes)
                                .await;
                            let handlers: Vec<_> =
                                results.into_iter().filter_map(|r| r.ok()).collect();
                            self.round_state.add_handlers(handlers);

                            // Self-insert.
                            self.eleader_blames.entry(epoch).or_default().push(blame);

                            self.try_form_change_qc(epoch).await?;
                        }
                        Err(e) => {
                            warn!("Zeus: equivocation blame construction failed: {}", e);
                        }
                    }
                }
                // Do not admit the equivocating block (Algorithm/zeus.tex line 415-416).
                return Ok(());
            }
            // Same child already recorded — duplicate arrival; idempotent.
        }

        // 6. Committed-prefix conflict check.
        if conflicts_data_prefix(&block, &self.commit_lock, &self.data_block_store) {
            warn!("Zeus: data block conflicts with committed prefix");
            return Ok(());
        }

        // 7. Three-case admission.
        self.admit_data_block(block).await?;

        // Process rleader wakeup if signaled by admit_data_block or its drains.
        if self.rleader_wakeup_pending {
            self.rleader_wakeup_pending = false;
            if let Err(e) = self.handle_new_sig_round().await {
                error!("Zeus: handle_new_sig_round from data propose wakeup: {}", e);
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Three-case admission (shared between on_data_propose and on_data_response)
    // -----------------------------------------------------------------------

    /// Admits a data block that has already passed signature, epoch-gate,
    /// idempotency, equivocation, and conflict checks.
    ///
    /// Cases:
    ///   (a) parent_hash == head_hash        → fast path, advance head.
    ///   (b) parent in store, not head       → bridge insert, drain head.
    ///   (c) parent not in store             → park, emit DataRequest.
    async fn admit_data_block(
        &mut self,
        block: DataBlock<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let block_hash = block.hash().clone();
        let parent_hash = block.envelope.parent_hash.clone();

        if parent_hash == self.data_chain.head_hash {
            // ---------------------------------------------------------------
            // Case (a): fast path — block directly extends the current head.
            // ---------------------------------------------------------------
            self.insert_and_advance(block, block_hash, parent_hash);

            // Drain any pending blocks whose parent is now the new head.
            self.drain_pending_data_blocks().await?;

            // Drain pending attestations for the new head.
            self.drain_pending_attestations(self.data_chain.head_hash.clone());
        } else if self.data_block_store.contains_key(&parent_hash) {
            // ---------------------------------------------------------------
            // Case (b): bridge — parent is in store but is not the head.
            // Insert without advancing head; then walk child_of forward from
            // the current head until there is no next child.
            // ---------------------------------------------------------------
            self.insert_non_head(block, block_hash, parent_hash);

            // Drain pending blocks whose parent is now admitted.
            self.drain_pending_data_blocks().await?;

            // Walk child_of forward from head to advance it as far as possible.
            self.advance_head_via_child_of();

            // After advancing, drain attestations for the new head.
            self.drain_pending_attestations(self.data_chain.head_hash.clone());
        } else {
            // ---------------------------------------------------------------
            // Case (c): parked — parent is not yet in the store.
            // ---------------------------------------------------------------
            debug!(
                "Zeus: data block height={} parent not in store ({:?}); parking",
                block.envelope.height, parent_hash
            );

            // Don't request genesis — we always hold it.
            let genesis_hash = DataBlock::<Tx>::genesis().hash().clone();
            if parent_hash == genesis_hash {
                // Parent IS genesis but genesis is always in store — this path
                // should not be reachable in a correct implementation; log and drop.
                warn!(
                    "Zeus: case (c) reached with genesis parent hash; \
                     genesis should always be in store. Dropping block."
                );
                return Ok(());
            }

            self.data_chain
                .pending_data_blocks
                .entry(parent_hash.clone())
                .or_default()
                .push(block);

            self.broadcast_data_request(parent_hash).await?;
        }

        Ok(())
    }

    /// Insert a block that extends the current head; advance head and populate
    /// `child_of`.  Does NOT drain — caller handles draining.
    fn insert_and_advance(
        &mut self,
        block: DataBlock<Tx>,
        block_hash: crate::server::zeus::chain_state::DataBlockHash<Tx>,
        parent_hash: crate::server::zeus::chain_state::DataBlockHash<Tx>,
    ) where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let old_head_hash = self.data_chain.head_hash.clone();
        self.data_block_store
            .insert(block_hash.clone(), block.clone());
        self.data_chain
            .child_of
            .insert(parent_hash, block_hash.clone());
        self.data_chain.head_hash = block_hash;
        self.data_chain.head_height = block.envelope.height;
        self.last_seen_data_block = block.clone();

        info!(
            "Zeus: admitted data block height={} epoch={} head_hash={:?}",
            block.envelope.height, block.envelope.epoch, self.data_chain.head_hash
        );

        // Update latest_eleader_block (monotone).
        let admitted_height = block.envelope.height;
        let current_latest_h = self
            .latest_eleader_block
            .as_ref()
            .map(|b| b.envelope.height)
            .unwrap_or(0);
        if admitted_height > current_latest_h {
            self.latest_eleader_block = Some(block);
        }

        self.post_admit_side_effects(admitted_height, old_head_hash);
    }

    /// Insert a block whose parent is in the store but is not the head.
    /// Does NOT advance head — caller calls `advance_head_via_child_of` after.
    fn insert_non_head(
        &mut self,
        block: DataBlock<Tx>,
        block_hash: crate::server::zeus::chain_state::DataBlockHash<Tx>,
        parent_hash: crate::server::zeus::chain_state::DataBlockHash<Tx>,
    ) where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        self.data_block_store
            .insert(block_hash.clone(), block.clone());
        self.data_chain.child_of.insert(parent_hash, block_hash);

        info!(
            "Zeus: bridge-admitted data block height={} epoch={} (non-head)",
            block.envelope.height, block.envelope.epoch
        );
    }

    /// Walk `child_of` forward from the current head as far as possible,
    /// advancing `head_hash` and `head_height` at each step.
    ///
    /// Updates `latest_eleader_block` and `last_seen_data_block` for each
    /// newly-advanced head.  Called after a bridge (case b) or pending-drain
    /// insert to finalize head advancement.
    fn advance_head_via_child_of(&mut self)
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        loop {
            let next_hash = match self.data_chain.child_of.get(&self.data_chain.head_hash) {
                Some(h) => h.clone(),
                None => break,
            };
            let next_block = match self.data_block_store.get(&next_hash) {
                Some(b) => b.clone(),
                None => {
                    // child_of points to a hash not yet in the store — should
                    // not happen under the invariant, but guard defensively.
                    break;
                }
            };
            let old_head = self.data_chain.head_hash.clone();
            self.data_chain.head_hash = next_hash;
            self.data_chain.head_height = next_block.envelope.height;
            self.last_seen_data_block = next_block.clone();

            info!(
                "Zeus: head advanced to height={} epoch={} hash={:?}",
                next_block.envelope.height, next_block.envelope.epoch, self.data_chain.head_hash
            );

            let admitted_height = next_block.envelope.height;
            let current_latest_h = self
                .latest_eleader_block
                .as_ref()
                .map(|b| b.envelope.height)
                .unwrap_or(0);
            if admitted_height > current_latest_h {
                self.latest_eleader_block = Some(next_block);
            }

            self.post_admit_side_effects(admitted_height, old_head);
        }
    }

    /// Drain `pending_data_blocks` for the current head hash, re-admitting
    /// each parked child through the three-case logic.
    ///
    /// Uses an iterative approach (not recursive) to avoid async recursion.
    /// May loop multiple times if draining triggers further case-(a) admits
    /// that in turn unblock more parked children.
    async fn drain_pending_data_blocks(&mut self) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        // We iterate as long as new blocks are being admitted; each admission
        // may unblock children for the newly-advanced head.
        loop {
            let current_head = self.data_chain.head_hash.clone();
            let parked = self
                .data_chain
                .pending_data_blocks
                .remove(&current_head)
                .unwrap_or_default();

            if parked.is_empty() {
                break;
            }

            for child_block in parked {
                Box::pin(self.admit_data_block(child_block)).await?;
            }
        }
        Ok(())
    }

    /// Drain `pending_attestations` for the given hash via the loopback
    /// channel. Identical to the existing loopback-replay idiom.
    fn drain_pending_attestations(
        &mut self,
        hash: crate::server::zeus::chain_state::DataBlockHash<Tx>,
    ) {
        let to_replay = self.pending_attestations.remove(&hash).unwrap_or_default();
        for parked_msg in to_replay {
            debug!("Zeus: re-queuing parked attestation msg via loopback");
            if self.tx_msg_loopback.send(parked_msg).is_err() {
                warn!("Zeus: loopback channel closed; dropping parked msg");
            }
        }
    }

    /// Side effects that fire on every successful head admission.
    ///
    /// Extracted to avoid repetition across insert_and_advance and
    /// advance_head_via_child_of.
    fn post_admit_side_effects(
        &mut self,
        admitted_height: u64,
        _old_head_hash: crate::server::zeus::chain_state::DataBlockHash<Tx>,
    ) where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        // TimerData reset (zeus.tex Def 8.4)
        {
            use crate::server::zeus::core::DataTimerKind;
            let timer_kind = self.data_timer.as_ref().map(|(_, k)| *k);
            match timer_kind {
                Some(DataTimerKind::TimerDataRoundEntry) => {
                    self.disarm_data_timer();
                }
                Some(DataTimerKind::RleaderWaitingFresh) => {}
                None => {}
            }
        }

        // Window-full recovery: if we are the eleader and the window just
        // dropped to zero, re-prime the batcher.
        if self.is_eleader() {
            let in_flight = self
                .eleader_proposed_height
                .saturating_sub(self.data_chain.head_height);
            if in_flight == 0 {
                let n = self.settings.committee_config.num_nodes();
                let _ = self.tx_consensus_to_batcher.send(
                    crate::server::BatcherConsensusMsg::NewRound {
                        leader: crate::server::zeus::chain_state::eleader(self.current_epoch, n),
                    },
                );
            }
        }

        // Rleader wakeup: if this node is the current rleader and is waiting
        // for a fresh block, the fresh block just arrived — jump into the
        // propose path.
        {
            use crate::server::zeus::core::DataTimerKind;
            if self.rleader_waiting_fresh
                && self.sig_leader_context.leader() == self.my_id
                && admitted_height > self.last_attested_data_height
            {
                debug!(
                    "Zeus: rleader wakeup: fresh block height={} arrived; proposing",
                    admitted_height
                );
                if matches!(
                    self.data_timer.as_ref().map(|(_, k)| k),
                    Some(DataTimerKind::RleaderWaitingFresh)
                ) {
                    self.disarm_data_timer();
                }
                self.rleader_waiting_fresh = false;
                self.rleader_wakeup_pending = true;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Zeus: OnDataRequest
    // -----------------------------------------------------------------------

    /// Zeus: OnDataRequest — respond to a peer's request for a data block by
    /// hash.
    ///
    /// Reference: zeus.tex lines 495–503.
    pub async fn on_data_request(
        &mut self,
        target_hash: crate::server::zeus::chain_state::DataBlockHash<Tx>,
        source: Id,
    ) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        if let Some(block) = self.data_block_store.get(&target_hash).cloned() {
            let msg = ZeusMsg::<Tx>::DataResponse { block };
            let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
            if let Ok(h) = self.consensus_net.send(source, bytes).await {
                self.round_state.add_handler(h);
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Zeus: OnDataResponse — re-enter three-case admission
    // -----------------------------------------------------------------------

    /// Zeus: OnDataResponse — validate a received DataBlock and re-enter the
    /// three-case admission.  If the response's parent is also missing, this
    /// cascades into another DataRequest (case c), bounded by the depth from
    /// the response back to the nearest in-store ancestor.
    ///
    /// The bridge-drain (case b) plus the `pending_data_blocks` drain inside
    /// `admit_data_block` will unwind chains of catch-up blocks in one pass.
    ///
    /// Reference: zeus.tex lines 505–540.
    pub async fn on_data_response(
        &mut self,
        block: DataBlock<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let h_target = block.hash().clone();

        // Idempotent: if we already have this block, ignore.
        if self.data_block_store.contains_key(&h_target) {
            return Ok(());
        }

        // Validate (catch-up: pass D.epoch as e_cur to bypass stale-epoch check).
        if !self.verify_data_block_sig(&block) {
            warn!("Zeus: OnDataResponse: signature invalid");
            return Ok(());
        }
        if !epoch_gate_valid(&block, block.envelope.epoch, &self.admitted_change_qcs) {
            warn!("Zeus: OnDataResponse: epoch gate failed");
            return Ok(());
        }

        // Equivocation check (same as on_data_propose).
        if let Some(existing_child) = self.data_chain.child_of.get(&block.envelope.parent_hash) {
            if existing_child != &h_target {
                warn!(
                    "Zeus: OnDataResponse: equivocation at parent {:?}",
                    block.envelope.parent_hash
                );
                return Ok(());
            }
        }

        // Re-enter the three-case admission.
        self.admit_data_block(block).await?;

        // Process rleader wakeup if signaled by admit_data_block.
        if self.rleader_wakeup_pending {
            self.rleader_wakeup_pending = false;
            if let Err(e) = self.handle_new_sig_round().await {
                error!(
                    "Zeus: handle_new_sig_round from data response wakeup: {}",
                    e
                );
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Helper: verify eleader signature on a DataBlock
    // -----------------------------------------------------------------------

    pub fn verify_data_block_sig(
        &self,
        block: &DataBlock<Tx>,
    ) -> bool
    where
        Tx: Serialize,
    {
        let n = self.settings.committee_config.num_nodes();
        let expected_eleader = eleader(block.envelope.epoch, n);
        let pk = match self.crypto_system.system.get(&expected_eleader) {
            Some(pk) => pk,
            None => return false,
        };
        // Use cached hash — no re-serialization on repeated calls.
        let env_hash = block.hash();
        pk.verify(env_hash.as_ref(), &block.sig.raw)
    }
}
