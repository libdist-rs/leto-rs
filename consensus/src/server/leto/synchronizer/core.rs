use crate::{
    types::{Proposal, ProtocolMsg, Response, Signature, Transaction, Element},
    Id, Round, server::{HelperRequest, ChainDB},
};
use anyhow::{Context, Result};
use crypto::hash::Hash;
use fnv::FnvHashMap;
use futures_util::Stream;
use mempool::{Batch, BatchHash};
use serde::Serialize;
use std::{fmt::Debug, net::SocketAddr, pin::Pin, task::{self, Poll}};
use storage::rocksdb::Storage;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use super::SyncHelper;

pub(super) type SyncMsg<Tx> = SynchronizerMsg<Tx>;

#[derive(Debug)]
pub enum SynchronizerMsg<Tx> {
    /// Sending this variant signals that only the batch needs syncing
    DeliverBatchOnly(
        /// Hash of the batch
        BatchHash<Tx>, 
        /// The person who sent this unknown batch hash
        Id, 
        // The message that needs this batch to be resolved
        Proposal<Id, Tx, Round>,
        Signature<Id, Proposal<Id, Tx, Round>>,
    ),
    /// Sending this variant signals that only the parent needs syncing
    DeliverParentOnly(
        /// Hash of the element
        Hash<Element<Id, Tx, Round>>,
        /// The person who sent this unknown batch hash
        Id, 
        // The message that needs this batch to be resolved
        Proposal<Id, Tx, Round>,
        Signature<Id, Proposal<Id, Tx, Round>>,
        Batch<Tx>,
    ),
    DeliverParentAndBatch(
        /// Hash of the batch
        BatchHash<Tx>, 
        /// The person who sent this unknown batch hash
        Id, 
        /// The message that needs this batch to be resolved
        Proposal<Id, Tx, Round>,
        Signature<Id, Proposal<Id, Tx, Round>>,
    ),
    AdvanceRound(Round),
}

pub struct Synchronizer<Tx> {
    /// The storage
    db: ChainDB,
    /// The channel used to communicate with the synchronizer thread
    inner_channel: UnboundedSender<SyncMsg<Tx>>,
    /// The channel used to tell the caller that this message is synced and ready
    tx_outer: UnboundedSender<ProtocolMsg<Id, Tx, Round>>,
    /// The channel used to forward requests to the helper
    tx_helper: UnboundedSender<HelperRequest<Tx>>,
    /// This channel contains the synchronized messages
    rx_outer: UnboundedReceiver<ProtocolMsg<Id, Tx, Round>>,
}

impl<Tx> Synchronizer<Tx> {
    pub fn new(
        store: Storage,
        my_id: Id,
        tx_helper: UnboundedSender<HelperRequest<Tx>>,
        consensus_peers: FnvHashMap<Id, SocketAddr>,
    ) -> Self
    where
        Tx: Transaction,
    {
        let (tx_from_out, rx_from_out) = unbounded_channel();

        let (tx_outer, rx_outer) = unbounded_channel();

        let store_clone = store.clone();
        let tx_outer_clone = tx_outer.clone();
        tokio::spawn(async move {
            SyncHelper::<Tx>::new(
                my_id,
                rx_from_out,
                store_clone,
                consensus_peers,
                tx_outer_clone,
            ).run()
            .await
        });

        Self {
            db: ChainDB::new(store),
            inner_channel: tx_from_out,
            tx_outer,
            rx_outer,
            tx_helper,
        }
    }

    /// The protocol lets the synchronizer know that we received some
    /// synchronized response
    async fn on_response<T>(
        &mut self,
        response: Response<T>,
    ) -> Result<()>
    where
        T: Serialize,
    {
        self.db
            .write(response.response())
            .await?;
        Ok(())
    }

    /// Advance the round of the synchronizer
    pub fn advance_round(&mut self, new_round: Round) -> Result<()> 
    where 
        Tx: Transaction,
    {
        self.inner_channel
            .send(SyncMsg::AdvanceRound(new_round))
            .context("Error advancing the round for the synchronizer")
    }

    /// This function synchronizes the propose message
    async fn sync_propose_msg(
        &mut self,
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch: Batch<Tx>,
        sender: Id,
    ) -> Result<()> 
    where 
        Tx: Transaction,
    {
        // Write the batch, so that some messages can be resolved
        self.db.write(batch.clone()).await?;

        // Check if we know the parent
        let parent_hash = proposal
            .block()
            .parent_hash(); 
        match self.db
            .read(parent_hash.clone())
            .await? {
            Some(..) => {
                // Write the proposal so that messages that are dependent on this are resolved
                self.db.write(Element::new(
                    proposal.clone(),
                    auth.clone(),
                    batch.clone(),
                )).await?;
                let msg = ProtocolMsg::Propose { 
                    proposal,
                    auth, 
                    batch,
                    sender,
                };
                self.tx_outer
                    .send(msg)
                    .context("Failed to send synced propose")
            },
            None => {
                self.inner_channel.send(SyncMsg::DeliverParentOnly(
                    parent_hash, 
                    sender,
                    proposal, 
                    auth, 
                    batch,
                )).context("Failed to send sync request")
            },
        }
    }

    /// This function synchronizes the relay message and converts it into a propose message
    async fn sync_relay_msg(
        &mut self,
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch_hash: BatchHash<Tx>,
        sender: Id,
    ) -> Result<()> 
    where   
        Tx: Transaction,
    {
        // Check if we know the batch
        let batch_opt = self.db
            .read(batch_hash.clone())
            .await?;
        // if batch_opt.is_none() {
        // }
        let parent_hash = proposal.block().parent_hash(); 
        let parent_opt = self.db
            .read(parent_hash.clone())
            .await?;
        if batch_opt.is_none() && parent_opt.is_none() {
            self.inner_channel.send(SyncMsg::DeliverParentAndBatch(
                batch_hash, 
                sender, 
                proposal.clone(), 
                auth.clone(),
            )).context("Failed to send to sync helper")
        } else if parent_opt.is_none() {
            self.inner_channel
                .send(SyncMsg::DeliverParentOnly(
                    parent_hash, 
                    sender, 
                    proposal,
                    auth,
                    batch_opt.unwrap(),
                )).context("Failed to send to sync helper")
        } else if batch_opt.is_none() {
            self.inner_channel.send(SyncMsg::DeliverBatchOnly(
                batch_hash, 
                sender, 
                proposal.clone(), 
                auth.clone(),
            )).context("Failed to send to sync helper")
        } else {
            self.tx_outer
                .send(ProtocolMsg::Propose { 
                    proposal, 
                    auth, 
                    batch: batch_opt.unwrap(), 
                    sender,
                }).context("Failed to send synced propose to tx outer")
        }
    }

    /// This function takes a message and returns it after ensuring that all the hashes are available
    pub async fn sync_msg(
        &mut self, 
        msg: ProtocolMsg<Id, Tx, Round>
    ) -> Result<()>
    where 
        Tx: Transaction,
    {
        match msg {
            ProtocolMsg::Propose { 
                proposal, 
                auth, 
                batch ,
                sender
            } => self.sync_propose_msg(
                proposal, 
                auth, 
                batch,
                sender,
            ).await,
            ProtocolMsg::Relay { 
                proposal, 
                auth, 
                batch_hash, 
                sender 
            } => self.sync_relay_msg(
                proposal,
                auth,
                batch_hash,
                sender,
            ).await,
            // Synchronizer messages
            ProtocolMsg::BatchRequest { 
                source, 
                request 
            } => self.tx_helper
                .send(HelperRequest::BatchRequest(
                    source, 
                    request
                )).context("Error sending request batch"),
            ProtocolMsg::BatchResponse { 
                response 
            } => self.on_response(response).await,
            ProtocolMsg::ElementRequest { 
                source, 
                request 
            } => self.tx_helper
                .send(HelperRequest::ElementRequest(
                    source,
                    request,
                )).context("Error sending request element"),
            ProtocolMsg::ElementResponse { 
                response 
            } => self.on_response(response).await,
            // By-pass all the remaining messages
            ProtocolMsg::Blame {..} => self.tx_outer
                .send(msg)
                .context("Error by-passing blame in synchronizer"),
            ProtocolMsg::BlameQC {..} => self.tx_outer
                .send(msg)
                .context("Error by-passing blameQC in synchronizer"),
        }
    }
}

/// Implement stream so we can just call `synchronizer.next()` instead of exposing the internals using channels
impl<Tx> Stream for Synchronizer<Tx> 
where
    Tx: Transaction,
{
    type Item = ProtocolMsg<Id, Tx, Round>;

    fn poll_next(
        mut self: Pin<&mut Self>, 
        cx: &mut task::Context<'_>
    ) -> Poll<Option<Self::Item>> {
        self.rx_outer.poll_recv(cx)
    }
}