/// Zeus: FuncMakeAttestation and FuncAttestationValid.
///
/// Zeus: OnSignatureRoundPropose calls `make_attestation()` instead of
/// `create_block()` to construct the sig-chain payload.
///
/// FuncAttestationValid has tri-state semantics:
///   - `Valid`          — attestation is fully admitted.
///   - `Invalid`        — cryptographically invalid; never park.
///   - `Parked(hash)`   — well-formed but pinned DataBlock absent from store;
///     parked under `hash` in `pendingAttestations`.
///
/// Change C: `AttestationEnvelope` now carries a hash+height+epoch reference
/// to the pinned data block.  `make_attestation` reads the current
/// data-chain head metadata from `data_chain` and `data_block_store`.
/// `attestation_valid` looks up the block by hash; absence returns
/// `Parked(data_block_hash)` and triggers the catch-up channel (§8.6).
use crate::{
    server::zeus::{
        chain_state::{conflicts_data_prefix, data_chain_valid},
        Zeus,
    },
    types::{Attestation, AttestationEnvelope, AttestationSig, ZeusMsg},
    Id, Round,
};
use anyhow::Result;
use crypto::hash::Hash;
use log::*;
use serde::Serialize;
use std::marker::PhantomData;

/// Tri-state result of `FuncAttestationValid`.
#[derive(Debug)]
pub enum AttestationValidity<Tx> {
    Valid,
    Invalid,
    /// Parked: the pinned DataBlock identified by this hash is absent from the
    /// store.  The caller should park the containing message under this hash.
    Parked(crate::server::zeus::chain_state::DataBlockHash<Tx>),
}

impl<Tx> Zeus<Tx>
where
    Tx: crate::types::Transaction,
{
    // -----------------------------------------------------------------------
    // Zeus: FuncMakeAttestation (zeus.tex §FuncMakeAttestation)
    // -----------------------------------------------------------------------

    /// Constructs an attestation at signature-chain round `r_s` pointing at
    /// the current data-chain head.  Called by the sig-chain leader.
    ///
    /// Change C: reads hash + height + epoch from `data_chain` state rather
    /// than cloning the full head block.  `make_attestation` no longer needs
    /// the full block in scope.
    ///
    /// Post-epoch-advance note: after `advance_to_epoch(e+1)` truncates
    /// `data_chain.head_hash` to `H(D*)`, the new eleader's first attestation
    /// pins `H(D*)`.  In-flight sig-chain proposals from epoch `e` that arrive
    /// after the advance are rejected by `attestation_valid` →
    /// `conflicts_data_prefix`: their pinned block is above `D*.h`, causing
    /// `conflicts_data_prefix` to return `true`.  No separate purge of
    /// prior-epoch attestations is needed.
    // PAPER-TODO(zeus-view-change): confirm attestation-rejection semantics
    // post-eleader-change in the paper's liveness argument.
    ///
    /// Zeus: OnSignatureRoundPropose — replaces canonical Leto's mempool-pop.
    pub fn make_attestation(
        &self,
        r_s: Round,
    ) -> Result<Attestation<Tx>>
    where
        Tx: Clone + Serialize,
    {
        // Read current data-chain head metadata.
        let data_block_hash = self.data_chain.head_hash.clone();
        let data_block_height = self.data_chain.head_height;
        // Epoch of the head block: look it up in the store.  Fallback to 0 for
        // genesis (always in store).
        let data_block_epoch = self.data_block_db.epoch_of(&data_block_hash).unwrap_or(0);

        // h_p^(s) = H(head(lockChain)) — we use the sig-chain highest hash
        // h_p^(s) = H(head(lockChain)) — derive from sig-chain highest hash.
        // SAFETY: Hash<T> is a [u8;32] newtype over its phantom T.  We
        // reinterpret the sig-chain element hash as an AttestationEnvelope hash
        // because the sig-chain stores Attestation<Tx> elements; the phantom
        // type change is safe since the underlying bytes are identical.
        let parent_hash_s: Hash<AttestationEnvelope<Tx>> = unsafe {
            let src: crate::server::zeus::SigElementHash<Tx> = self.sig_chain_state.highest_hash();
            std::mem::transmute(src)
        };

        let envelope = AttestationEnvelope {
            epoch_s: self.signature_epoch,
            round_s: r_s,
            data_block_hash,
            data_block_height,
            data_block_epoch,
            parent_hash_s,
        };

        let env_hash = Hash::ser_and_hash(&envelope);
        let raw = self.crypto_system.secret.sign(env_hash.as_ref())?;

        Ok(Attestation::new(
            envelope,
            AttestationSig {
                raw,
                signer: self.my_id,
                _phantom: PhantomData,
            },
        ))
    }

    // -----------------------------------------------------------------------
    // Zeus: FuncAttestationValid (zeus.tex §FuncAttestationValid)
    // -----------------------------------------------------------------------

    /// Validates an attestation as a sig-chain block-element.
    ///
    /// Tri-state: `Valid`, `Invalid`, or `Parked(hash)`.
    /// The `parked_msg` argument is the containing `ZeusMsg` to park if needed.
    ///
    /// Change C: the pinned data block is looked up in `data_block_store` by
    /// `att.envelope.data_block_hash`.  On a store miss the function parks
    /// the message and returns `Parked(data_block_hash)`.  The `Parked` branch
    /// is now exercised more often than before (every time a receiver is
    /// slightly behind the eleader when an attestation arrives); the existing
    /// `pending_attestations` + `on_data_response` drain machinery handles it.
    pub fn attestation_valid(
        &mut self,
        att: &Attestation<Tx>,
        parked_msg: ZeusMsg<Tx>,
    ) -> AttestationValidity<Tx>
    where
        Tx: Clone + Serialize + PartialEq,
    {
        // 1. Cryptographic verification of the sig-chain leader's signature.
        let signer_id = att.sig_s.signer;
        let pk = match self.crypto_system.system.get(&signer_id) {
            Some(pk) => pk,
            None => {
                warn!("attestation_valid: unknown signer id {}", signer_id);
                return AttestationValidity::Invalid;
            }
        };
        let env_hash = Hash::ser_and_hash(&att.envelope);
        if !pk.verify(env_hash.as_ref(), &att.sig_s.raw) {
            warn!(
                "attestation_valid: sig verification failed for signer={} round={}",
                signer_id, att.envelope.round_s
            );
            return AttestationValidity::Invalid;
        }

        // 2. Epoch gate on the pinned data block using the epoch carried in the
        //    envelope (no store lookup required). Per FuncAttestationValid
        //    §FuncDataBlockValid(A.D_h, A.D_h.e): we invoke FuncDataBlockValid with
        //    e_cur = A.D_h.e, so the stale-epoch branch (block_epoch < e_cur) trivially
        //    passes.  The only non-trivial condition is future-epoch without an
        //    admitted change QC. TODO(zeus-view-change): once multi-epoch is wired, add
        //    the full gate here.
        // (epoch field carried for future gating — no check needed in steady state)

        // 3. Check that the pinned DataBlock is in the store (or is genesis). Genesis:
        //    height==0 && epoch==0.
        let is_genesis = att.envelope.data_block_height == 0 && att.envelope.data_block_epoch == 0;
        let d_hash = &att.envelope.data_block_hash;

        if !is_genesis && !self.data_block_db.contains(d_hash) {
            // Park the message under the missing hash.
            debug!(
                "attestation_valid: parking under missing data block {:?}",
                d_hash
            );
            self.pending_attestations
                .entry(d_hash.clone())
                .or_default()
                .push(parked_msg);
            // Emit a DataRequest — handled by caller which broadcasts it.
            return AttestationValidity::Parked(d_hash.clone());
        }

        // 4. Chain-level validity of the pinned data chain. Under the eager-admission
        //    invariant, store membership IS chain validity: every block in
        //    data_block_store has a valid parent link. Step 3 above already confirmed
        //    d_h is in the store (or is genesis). This O(1) call re-confirms membership
        //    as the invariant check.
        if !data_chain_valid(d_hash, &self.data_block_db) {
            // Unreachable in steady-state: step 3 parks on store-miss.
            // Guard defensively.
            debug!("attestation_valid: data_chain_valid false (defensive guard)");
            self.pending_attestations
                .entry(d_hash.clone())
                .or_default()
                .push(parked_msg);
            return AttestationValidity::Parked(d_hash.clone());
        }

        // 5. Committed-prefix conflict check. Drive the walk from the pinned block's
        //    resident metadata (no payload load needed).
        if let Some(m) = self.data_block_db.meta(d_hash) {
            let (d_height, d_parent) = (m.height, m.parent_hash.clone());
            if conflicts_data_prefix(
                d_hash,
                d_height,
                &d_parent,
                &self.commit_lock,
                &self.data_block_db,
            ) {
                warn!("attestation_valid: conflicts with committed prefix");
                return AttestationValidity::Invalid;
            }
        } else if !is_genesis {
            // Should be unreachable after step 3/4, but guard defensively.
            warn!("attestation_valid: block missing after chain-valid check (defensive)");
            return AttestationValidity::Invalid;
        }
        // Genesis never conflicts.

        AttestationValidity::Valid
    }

    // -----------------------------------------------------------------------
    // Helper: leader for a given sig-chain round (canonical Leto round-robin)
    // -----------------------------------------------------------------------

    /// Returns the sig-chain leader ID for round `r`.
    /// Reuses the `LeaderContext` logic: leader = (r - 1) % n mapping via
    /// the leader history.  For Zeus steady-state the leader schedule is
    /// identical to Leto's.
    pub fn sig_leader_for_round(
        &self,
        r: Round,
    ) -> Id {
        let n = self.settings.committee_config.num_nodes();
        let _ = n; // avoid unused warning; signer-based check is done in verify step
        ((r as usize).saturating_sub(1) % self.settings.committee_config.num_nodes()) as Id
    }
}
