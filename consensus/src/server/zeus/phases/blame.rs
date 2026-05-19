/// Zeus data-plane eleader-blame and eleader-change-QC handlers.
///
/// Implements:
///   - `on_eleader_blame`: validate and accumulate an incoming eleader blame,
///     attempt QC formation via `try_form_change_qc`.
///   - `on_eleader_change_qc`: validate and admit an incoming eleader-change
///     QC, merge blames, and advance the epoch if needed.
///
/// Reference: Algorithm/zeus.tex §OnEleaderBlame (lines 454-484) and
/// §OnEleaderChangeQC (lines 487-517).
///
/// NOTE(harness): a multi-node in-process harness test for eleader-change
/// should be added as a follow-up dispatch.  It would:
///   1. Start N nodes.
///   2. Stall the eleader's data-block propose.
///   3. Wait for the silence-blame timer to fire.
///   4. Assert that (a) all nodes emit EleaderBlame(Silence), (b) a
///      EleaderChangeQC forms within ~3 timer durations, and (c) epoch
///      increments and the new eleader produces data blocks.
use crate::{
    server::zeus::{
        chain_state::{blame_signed_bytes, eleader, eleader_change_qc_valid},
        Zeus,
    },
    types::{BlamePayload, BlameReason, EleaderBlame, EleaderChangeQC, Transaction},
};
use anyhow::Result;
use log::*;
use serde::Serialize;

impl<Tx> Zeus<Tx>
where
    Tx: Transaction,
{
    // -----------------------------------------------------------------------
    // Zeus: OnEleaderBlame (Algorithm/zeus.tex lines 454-484)
    // -----------------------------------------------------------------------

    /// Validate and accumulate an incoming eleader blame, then attempt QC
    /// formation via `try_form_change_qc`.
    ///
    /// Idempotency: a blame from the same signer for the same epoch + reason
    /// combination is discarded on second arrival.
    pub async fn on_eleader_blame(
        &mut self,
        blame: EleaderBlame<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let n = self.settings.committee_config.num_nodes();
        let expected_eleader_id = eleader(blame.epoch, n);

        // Validate (Algorithm/zeus.tex line 456).
        if !crate::server::zeus::chain_state::eleader_blame_valid(
            &blame,
            &self.crypto_system.system,
            expected_eleader_id,
        ) {
            debug!(
                "Zeus: on_eleader_blame: invalid blame from signer={} epoch={}",
                blame.signer, blame.epoch
            );
            return Ok(());
        }

        // Idempotency: skip if we already have a blame from this signer for
        // this epoch with the same reason.
        {
            let existing = self.eleader_blames.entry(blame.epoch).or_default();
            let already_have = existing
                .iter()
                .any(|b| b.signer == blame.signer && b.reason == blame.reason);
            if already_have {
                debug!(
                    "Zeus: on_eleader_blame: duplicate blame from signer={} epoch={} reason={:?}",
                    blame.signer, blame.epoch, blame.reason
                );
                return Ok(());
            }
            existing.push(blame.clone());
        }

        info!(
            "Zeus: on_eleader_blame: admitted blame from signer={} epoch={} reason={:?}",
            blame.signer, blame.epoch, blame.reason
        );

        // Attempt QC formation (Algorithm/zeus.tex line 461).
        self.try_form_change_qc(blame.epoch).await
    }

    // -----------------------------------------------------------------------
    // Zeus: OnEleaderChangeQC (Algorithm/zeus.tex lines 487-517)
    // -----------------------------------------------------------------------

    /// Validate and admit an incoming eleader-change QC, merge blames, and
    /// advance the epoch if `current_epoch <= qc.epoch`.
    ///
    /// Idempotent: returns immediately if the QC for this epoch has already
    /// been admitted.
    pub async fn on_eleader_change_qc(
        &mut self,
        qc: EleaderChangeQC<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        // Idempotency (Algorithm/zeus.tex line 489).
        if self.admitted_change_qcs.contains_key(&qc.epoch) {
            debug!(
                "Zeus: on_eleader_change_qc: epoch {} already admitted",
                qc.epoch
            );
            return Ok(());
        }

        let n = self.settings.committee_config.num_nodes();
        let num_faults = self.settings.committee_config.num_faults();

        // Validate (Algorithm/zeus.tex line 491).
        if !eleader_change_qc_valid(&qc, &self.crypto_system.system, num_faults, n) {
            warn!(
                "Zeus: on_eleader_change_qc: invalid QC for epoch {}",
                qc.epoch
            );
            return Ok(());
        }

        // Admit (Algorithm/zeus.tex line 492).
        self.admitted_change_qcs.insert(qc.epoch, qc.clone());

        // Merge blames into local accumulator (Algorithm/zeus.tex line 493),
        // deduplicating by (signer, reason).
        {
            let local = self.eleader_blames.entry(qc.epoch).or_default();
            for b in qc.blames.iter() {
                let already = local
                    .iter()
                    .any(|l| l.signer == b.signer && l.reason == b.reason);
                if !already {
                    local.push(b.clone());
                }
            }
        }

        info!(
            "Zeus: on_eleader_change_qc: admitted QC for epoch {} ({} blames)",
            qc.epoch,
            qc.blames.len()
        );

        // Advance epoch if we are at or behind this epoch (Algorithm/zeus.tex
        // line 494).
        if self.current_epoch <= qc.epoch {
            self.advance_to_epoch(qc.epoch + 1).await?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Helper: build and sign an equivocation blame
    // -----------------------------------------------------------------------

    /// Construct and sign an equivocation blame for a pair of conflicting
    /// data blocks.
    ///
    /// Called from `on_data_propose` when a `child_of` collision is detected
    /// (Algorithm/zeus.tex lines 409-417).
    pub(crate) fn make_equivocation_blame(
        &self,
        epoch: u64,
        height: u64,
        block_a: crate::types::DataBlock<Tx>,
        block_b: crate::types::DataBlock<Tx>,
    ) -> Result<EleaderBlame<Tx>>
    where
        Tx: Clone + Serialize,
    {
        let payload = BlamePayload::Equivocation {
            height,
            block_a,
            block_b,
        };
        let digest = blame_signed_bytes(epoch, &BlameReason::Equivocation, &payload);
        let sig_raw = self.crypto_system.secret.sign(&digest)?;
        Ok(EleaderBlame {
            epoch,
            reason: BlameReason::Equivocation,
            payload,
            signer: self.my_id,
            sig_raw,
        })
    }
}

// Sig-plane attestation note:
//
// When sig-chain proposals from a prior epoch arrive after `advance_to_epoch`
// has already incremented `current_epoch`, they are rejected by
// `FuncConflictsDataPrefix` in `attestation_valid`: the old attestation's
// pinned block is above `D*.h`, so `conflicts_data_prefix` returns `true`.
// No explicit per-sig-chain-proposal purge is needed.
//
// PAPER-TODO(zeus-view-change): confirm this reasoning in the paper's
// liveness argument for attestation admission after eleader change.
//
// See also: `phases/sig.rs::make_attestation` — the new eleader's first
// attestation pins `data_chain.head_hash` which post-`advance_to_epoch` IS
// `H(D*)`.  No separate gate is needed; the truncation in `advance_to_epoch`
// enforces the strict-extension rule transitively.
