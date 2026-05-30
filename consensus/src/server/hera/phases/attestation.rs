/// Hera: FuncMakeMultiAttestation and FuncMultiAttestationValid.
///
/// `make_multi_attestation` is called by the sig-chain leader to construct a
/// `MultiAttestation` payload referencing all author sub-chain heads.
///
/// Split with the data-plane actor refactor:
/// - Sig verify (`multi_attestation_sig_valid`) stays in the consensus actor.
/// - Head-existence check is now gated via Storage `notify_read` futures (see
///   `core.rs::push_gating_future`). The consensus actor pushes a `GateFut` and
///   continues; the future resolves once all referenced blocks land in the
///   shared store. No cross-actor RPC involved.
use crate::{
    server::hera::Hera,
    types::{
        hera::{DataHead, MultiAttestation, MultiAttestationEnvelope},
        AttestationSig, HeraMsg, Transaction,
    },
    Id, Round,
};
use anyhow::Result;
use crypto::hash::Hash;
use log::warn;
use serde::Serialize;
use std::marker::PhantomData;
use std::sync::atomic::Ordering as AOrdering;

/// Result of the sig-only validation (`multi_attestation_sig_valid`).
#[derive(Debug)]
pub enum SigAttestationValidity {
    /// Cryptographic signature is valid.
    Valid,
    /// Signature invalid — drop, never park.
    Invalid,
}

impl<Tx> Hera<Tx>
where
    Tx: Transaction,
{
    // -----------------------------------------------------------------------
    // Hera: FuncMakeMultiAttestation
    // -----------------------------------------------------------------------

    /// Build a `MultiAttestation` for the current sig-chain round.
    ///
    /// Reads per-author head snapshots from the lock-free `ArcSwap` map
    /// published by the data actor. Stale reads are safe: attesting a
    /// slightly-old head costs one round of throughput, never safety.
    pub fn make_multi_attestation(
        &self,
        r_s: Round,
    ) -> Result<MultiAttestation<Tx>>
    where
        Tx: Clone + Serialize,
    {
        let n = self.settings.committee_config.num_nodes();
        let mut heads: Vec<DataHead<Tx>> = Vec::with_capacity(n);
        let mut blames: Vec<Id> = Vec::new();

        let all_ids = self.settings.committee_config.get_all_ids();
        let mut sorted_ids = all_ids;
        sorted_ids.sort_unstable();

        // DP_PROFILE: measure attestation lag — how stale are the attested
        // heads compared to the data actor's freshest locally-known head?
        // lag_sum = sum over committee of max(0, local_latest_height -
        // attested_height). A large value means the sig leader is attesting
        // stale data (the data actor is not publishing fresh heads fast enough
        // or the ArcSwap is behind).
        let mut attest_lag_sum: u64 = 0;

        for id in sorted_ids {
            let prev_attested = self.prev_attested_heights.get(&id).copied().unwrap_or(0);

            // Load from the lock-free ArcSwap — consistent (epoch, height, hash) snapshot.
            let snap = self
                .head_snapshots
                .get(&id)
                .map(|slot| (**slot.load()).clone());

            if let Some(snap) = snap {
                // Attest-lag for this author: local freshest vs what we will attest.
                let attesting_height = if snap.height > prev_attested {
                    snap.height
                } else {
                    prev_attested
                };
                // local freshest - what we are about to attest (≥ 0).
                attest_lag_sum += snap.height.saturating_sub(attesting_height);

                if snap.height > prev_attested {
                    heads.push(DataHead {
                        author: id,
                        hash: snap.hash.clone(),
                        height: snap.height,
                        epoch: snap.epoch,
                    });
                } else {
                    blames.push(id);
                }
            } else {
                blames.push(id);
            }
        }

        // Update global attest-lag counters (read by DP_PROFILE monitor).
        crate::server::hera::core::DP_ATTEST_LAG_SUM.fetch_add(attest_lag_sum, AOrdering::Relaxed);
        crate::server::hera::core::DP_ATTEST_COUNT.fetch_add(1, AOrdering::Relaxed);

        // Derive parent_hash_s from sig-chain highest hash.
        let parent_hash_s: Hash<MultiAttestationEnvelope<Tx>> = unsafe {
            let src = self.sig_chain_state.highest_hash();
            std::mem::transmute(src)
        };

        let envelope = MultiAttestationEnvelope {
            epoch_s: self.signature_epoch,
            round_s: r_s,
            parent_hash_s,
            heads,
            blames,
        };

        let env_hash = Hash::ser_and_hash(&envelope);
        let raw = self.crypto_system.secret.sign(env_hash.as_ref())?;

        Ok(MultiAttestation::new(
            envelope,
            AttestationSig {
                raw,
                signer: self.my_id,
                _phantom: PhantomData,
            },
        ))
    }

    // -----------------------------------------------------------------------
    // Hera: FuncMultiAttestationValid (sig check only)
    // -----------------------------------------------------------------------

    /// Validate the cryptographic signature of an incoming `MultiAttestation`.
    ///
    /// Only verifies the sig — head-existence is checked via Storage
    /// `notify_read` in the gating future (see `push_gating_future`).
    /// Returns `Valid` or `Invalid` (no `Parked` state here).
    pub fn multi_attestation_sig_valid(
        &self,
        att: &MultiAttestation<Tx>,
    ) -> SigAttestationValidity
    where
        Tx: Clone + Serialize + PartialEq,
    {
        let signer_id = att.sig_s.signer;
        let pk = match self.crypto_system.system.get(&signer_id) {
            Some(pk) => pk,
            None => {
                warn!("multi_attestation_sig_valid: unknown signer {}", signer_id);
                return SigAttestationValidity::Invalid;
            }
        };
        let env_hash = Hash::ser_and_hash(&att.envelope);
        if !pk.verify(env_hash.as_ref(), &att.sig_s.raw) {
            warn!(
                "multi_attestation_sig_valid: sig failed for signer={} round={}",
                signer_id, att.envelope.round_s
            );
            return SigAttestationValidity::Invalid;
        }
        SigAttestationValidity::Valid
    }
}

impl<Tx> Hera<Tx>
where
    Tx: Transaction,
{
    /// Helper: broadcast a `DataRequest` via the data network.
    /// Note: after the split, DataRequest is sent via the data actor's
    /// `request_data`. This helper is kept for sig-element catch-up
    /// which still goes through the consensus net.
    #[allow(dead_code)]
    pub async fn broadcast_data_request_via_sig(
        &mut self,
        target_hash: crate::server::hera::chain_state::DataBlockHash<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        let msg = HeraMsg::<Tx>::DataRequest {
            target_hash,
            source: self.my_id,
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
        let _ = self
            .consensus_net
            .broadcast(&self.broadcast_peers, bytes)
            .await;
        Ok(())
    }
}
