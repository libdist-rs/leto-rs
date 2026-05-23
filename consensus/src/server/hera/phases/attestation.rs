/// Hera: FuncMakeMultiAttestation and FuncMultiAttestationValid.
///
/// `make_multi_attestation` is called by the sig-chain leader to construct a
/// `MultiAttestation` payload referencing all author sub-chain heads.
///
/// `multi_attestation_valid` has tri-state semantics:
///   - `Valid`       — fully verified.
///   - `Invalid`     — cryptographically invalid; never park.
///   - `Parked(hash)` — well-formed but a referenced DataBlock is absent from
///     the local store; park the containing message under the missing hash.
use crate::{
    server::hera::{chain_state::DataBlockHash, Hera},
    types::{
        hera::{DataHead, MultiAttestation, MultiAttestationEnvelope},
        AttestationSig, DataBlock, HeraMsg, Transaction,
    },
    Id, Round,
};
use anyhow::Result;
use crypto::hash::Hash;
use log::{debug, warn};
use serde::Serialize;
use std::marker::PhantomData;

/// Tri-state result of `FuncMultiAttestationValid`.
#[derive(Debug)]
pub enum MultiAttestationValidity<Tx> {
    Valid,
    Invalid,
    /// The referenced DataBlock identified by this hash is absent from the
    /// local store.  The caller should park the containing message under this
    /// hash and emit a DataRequest.
    Parked(DataBlockHash<Tx>),
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
    /// For each author `i` in the committee:
    ///   - If `multi_data_chain.heights[i] > prev_attested_heights[i]`, include
    ///     a `DataHead { author: i, hash, height, epoch }`.
    ///   - Otherwise, push `i` to `blames`.
    ///
    /// The `prev_attested_heights` map tracks, per-author, the highest height
    /// that was included in a head in a previous sig-block proposal from this
    /// node.  This ensures the freshness rule: we only attest new data.
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
        sorted_ids.sort_unstable(); // deterministic order

        for id in sorted_ids {
            let current_height = self.multi_data_chain.head_height(id);
            let prev_attested = self.prev_attested_heights.get(&id).copied().unwrap_or(0);

            if current_height > prev_attested {
                let head_hash = match self.multi_data_chain.head_hash(id) {
                    Some(h) => h.clone(),
                    None => {
                        blames.push(id);
                        continue;
                    }
                };
                let epoch = self
                    .multi_data_chain
                    .get_block(&head_hash)
                    .map(|b| b.envelope.epoch)
                    .unwrap_or(self.current_epoch);
                heads.push(DataHead {
                    author: id,
                    hash: head_hash,
                    height: current_height,
                    epoch,
                });
            } else {
                blames.push(id);
            }
        }

        // Derive parent_hash_s from sig-chain highest hash.
        // SAFETY: Hash<T> is a [u8;32] newtype over a phantom T.  We
        // reinterpret the sig-chain element hash as a MultiAttestationEnvelope
        // hash because the underlying bytes are identical.
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
    // Hera: FuncMultiAttestationValid
    // -----------------------------------------------------------------------

    /// Validates an incoming `MultiAttestation`.
    ///
    /// Checks:
    ///   1. Cryptographic signature by the expected rleader.
    ///   2. For each `head` in `att.envelope.heads`, the referenced block must
    ///      be in the local store.  If any is missing, park under that hash and
    ///      return `Parked(missing_hash)`.
    ///
    /// The `parked_msg` is stored if we need to park.
    pub fn multi_attestation_valid(
        &mut self,
        att: &MultiAttestation<Tx>,
        parked_msg: HeraMsg<Tx>,
    ) -> MultiAttestationValidity<Tx>
    where
        Tx: Clone + Serialize + PartialEq,
    {
        // 1. Verify sig by the signer.
        let signer_id = att.sig_s.signer;
        let pk = match self.crypto_system.system.get(&signer_id) {
            Some(pk) => pk,
            None => {
                warn!("multi_attestation_valid: unknown signer {}", signer_id);
                return MultiAttestationValidity::Invalid;
            }
        };
        let env_hash = Hash::ser_and_hash(&att.envelope);
        if !pk.verify(env_hash.as_ref(), &att.sig_s.raw) {
            warn!(
                "multi_attestation_valid: sig failed for signer={} round={}",
                signer_id, att.envelope.round_s
            );
            return MultiAttestationValidity::Invalid;
        }

        // 2. Check each head's data block is in the local store.
        for head in &att.envelope.heads {
            let is_genesis = head.height == 0 && head.epoch == 0;
            if is_genesis {
                continue;
            }
            if !self.multi_data_chain.block_store.contains_key(&head.hash) {
                debug!(
                    "multi_attestation_valid: parking under missing block \
                     author={} height={} hash={:?}",
                    head.author, head.height, head.hash
                );
                self.pending_attestations
                    .entry(head.hash.clone())
                    .or_default()
                    .push(parked_msg);
                return MultiAttestationValidity::Parked(head.hash.clone());
            }
        }

        MultiAttestationValidity::Valid
    }
}

impl<Tx> Hera<Tx>
where
    Tx: Transaction,
{
    /// Helper: verify the author's signature on a `DataBlock`.
    /// The signer must match `block.sig.signer` (per-author, not
    /// epoch-derived).
    pub fn verify_data_block_sig(
        &self,
        block: &DataBlock<Tx>,
    ) -> bool
    where
        Tx: Serialize,
    {
        let author = block.sig.signer;
        let pk = match self.crypto_system.system.get(&author) {
            Some(pk) => pk,
            None => return false,
        };
        let env_hash = block.hash();
        pk.verify(env_hash.as_ref(), &block.sig.raw)
    }

    /// Helper: broadcast a `DataRequest` for a missing block hash.
    pub async fn broadcast_data_request(
        &mut self,
        target_hash: DataBlockHash<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        let msg = HeraMsg::<Tx>::DataRequest {
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
