/// Hera data-plane handlers.
///
/// Every node is the stable leader of its own data sub-chain.  This module
/// implements:
///   - `on_self_propose`: called when this node's own batcher emits a batch.
///     Builds a DataBlock (author=self), signs it, broadcasts it, and admits it
///     locally.
///   - `on_data_propose`: handles incoming DataPropose from peers.  Validates
///     the author's signature (signer must equal `block.sig.signer`), then
///     admits via `multi_data_chain.admit_typed`.
///   - `on_data_request`: responds to a peer's DataRequest with the stored
///     block.
///   - `on_data_response`: re-enters admission for a received block.
///
/// No eleader gate: any node can propose at any time.
use crate::{
    server::hera::{chain_state::AdmitTypedResult, Hera},
    types::{DataBlock, DataBlockEnvelope, DataBlockSig, HeraMsg, Transaction},
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

impl<Tx> Hera<Tx>
where
    Tx: Transaction,
{
    // -----------------------------------------------------------------------
    // Self-propose: this node's batcher emits a batch
    // -----------------------------------------------------------------------

    /// Build, sign, broadcast, and locally admit a new data block for this
    /// node's own sub-chain.
    ///
    /// Called when `rx_data_batch` yields a batch.  The block extends
    /// `my_last_hash` (this node's last proposed block hash) at height
    /// `my_height + 1`.  The batcher is re-primed immediately after so the
    /// pipeline stays full.
    pub async fn on_self_propose(
        &mut self,
        batch: Batch<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        // Drop empty batches.
        if batch.payload.is_empty() {
            return Ok(());
        }

        let height = self.my_height + 1;
        let parent_hash = self
            .my_last_hash
            .clone()
            .unwrap_or_else(|| DataBlock::<Tx>::genesis().hash().clone());

        let payload = Arc::new(batch.payload);
        let envelope = DataBlockEnvelope {
            epoch: self.current_epoch,
            height,
            payload,
            parent_hash,
        };

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

        debug!(
            "Hera: self-propose data block height={} author={}",
            block.envelope.height, self.my_id
        );

        // Update this node's proposed tip.
        self.my_height = block.envelope.height;
        self.my_last_hash = Some(block.hash().clone());

        // Prime batcher for next block.
        let _ = self
            .tx_consensus_to_batcher
            .send(crate::server::BatcherConsensusMsg::NewRound {
                leader: self.my_id,
                round: self.my_height + 1,
            });

        // Broadcast to peers.
        let msg = HeraMsg::DataPropose {
            block: block.clone(),
            sender: self.my_id,
        };
        let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
        let _ = self
            .consensus_net
            .broadcast(&self.broadcast_peers, bytes)
            .await;

        // Locally admit via loopback so the select loop can interleave network
        // messages with the admission.
        let loopback = HeraMsg::DataPropose {
            block,
            sender: self.my_id,
        };
        if self.tx_msg_loopback.send(loopback).is_err() {
            warn!("Hera: loopback channel closed; own admission dropped");
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Incoming DataPropose from peers
    // -----------------------------------------------------------------------

    /// Handle a `DataPropose` from any peer (or our own loopback).
    ///
    /// Validates the block's signature (signer must be `block.sig.signer`) and
    /// then admits via `multi_data_chain.admit_typed`.
    pub async fn on_data_propose(
        &mut self,
        block: DataBlock<Tx>,
        sender: Id,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let author = block.sig.signer;

        // Sanity: sender should match the embedded author.  Allow mismatch only
        // with a warning (a relay or test scenario might differ).
        if sender != author {
            debug!(
                "Hera: on_data_propose: sender={} != author={}; continuing",
                sender, author
            );
        }

        // Verify author's signature.
        if !self.verify_data_block_sig(&block) {
            warn!(
                "Hera: data block sig invalid from author={} height={}",
                author, block.envelope.height
            );
            return Ok(());
        }

        // Admit via multi-author chain.
        let result = self.multi_data_chain.admit_typed(block);
        match result {
            AdmitTypedResult::Extended | AdmitTypedResult::Bridge => {
                // Drain pending attestations for each newly-admitted head.
                let head_hash = self.multi_data_chain.head_hash(author).cloned();
                if let Some(h) = head_hash {
                    self.drain_pending_attestations(h);
                }
            }
            AdmitTypedResult::Parked(missing_hash) => {
                debug!(
                    "Hera: on_data_propose: block parked; requesting {:?}",
                    missing_hash
                );
                self.broadcast_data_request(missing_hash).await?;
            }
            AdmitTypedResult::Duplicate => {
                debug!("Hera: on_data_propose: duplicate block, ignoring");
            }
            AdmitTypedResult::Invalid => {
                debug!("Hera: on_data_propose: invalid block, ignoring");
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // DataRequest
    // -----------------------------------------------------------------------

    pub async fn on_data_request(
        &mut self,
        target_hash: crate::server::hera::chain_state::DataBlockHash<Tx>,
        source: Id,
    ) -> Result<()>
    where
        Tx: Clone + Serialize,
    {
        if let Some(block) = self.multi_data_chain.block_store.get(&target_hash).cloned() {
            let msg = HeraMsg::<Tx>::DataResponse { block };
            let bytes = bytes::Bytes::from(bincode::serialize(&msg).map_err(anyhow::Error::new)?);
            let _ = self.consensus_net.send(source, bytes).await;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // DataResponse
    // -----------------------------------------------------------------------

    pub async fn on_data_response(
        &mut self,
        block: DataBlock<Tx>,
    ) -> Result<()>
    where
        Tx: Clone + Serialize + PartialEq + std::fmt::Debug,
    {
        let h_target = block.hash().clone();

        // Idempotent.
        if self.multi_data_chain.block_store.contains_key(&h_target) {
            return Ok(());
        }

        // Validate sig.
        if !self.verify_data_block_sig(&block) {
            warn!("Hera: on_data_response: sig invalid");
            return Ok(());
        }

        let author = block.sig.signer;
        let result = self.multi_data_chain.admit_typed(block);
        match result {
            AdmitTypedResult::Extended | AdmitTypedResult::Bridge => {
                let head_hash = self.multi_data_chain.head_hash(author).cloned();
                if let Some(h) = head_hash {
                    self.drain_pending_attestations(h);
                }
            }
            AdmitTypedResult::Parked(missing_hash) => {
                self.broadcast_data_request(missing_hash).await?;
            }
            _ => {}
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Drain pending attestations (loopback replay)
    // -----------------------------------------------------------------------

    /// Drain `pending_attestations` for the given hash by re-queuing them via
    /// the loopback channel.
    pub(crate) fn drain_pending_attestations(
        &mut self,
        hash: crate::server::hera::chain_state::DataBlockHash<Tx>,
    ) {
        let to_replay = self.pending_attestations.remove(&hash).unwrap_or_default();
        for parked_msg in to_replay {
            debug!("Hera: re-queuing parked attestation msg via loopback");
            if self.tx_msg_loopback.send(parked_msg).is_err() {
                warn!("Hera: loopback channel closed; dropping parked msg");
            }
        }
    }
}
