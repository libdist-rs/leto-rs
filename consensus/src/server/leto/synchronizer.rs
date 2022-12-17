use crate::{
    types::{Proposal, ProtocolMsg, Request, Response, Signature, Transaction},
    Id, Round,
};
use anyhow::{anyhow, Context, Result};
use crypto::hash::Hash;
use fnv::{FnvHashMap, FnvHashSet};
use futures_util::{stream::FuturesUnordered, StreamExt};
use log::*;
use mempool::{Batch, BatchHash};
use network::{plaintcp::TcpSimpleSender, Acknowledgement, NetSender};
use serde::de::DeserializeOwned;
use std::{fmt::Debug, net::SocketAddr};
use storage::rocksdb::Storage;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use super::{Leto, RelayMsg};

#[derive(Debug)]
pub enum SynchronizerInMsg<Tx> {
    RequestBatch(
        /* batch_hash */ BatchHash<Tx>,
        /* The person who sent this unknown batch hash */ Id,
        // Proposal<Tx, Round>,
    ),
}

type SInMsg<Tx> = SynchronizerInMsg<Tx>;
type SOutMsg<Tx> = SynchronizerOutMsg<Tx>;

#[derive(Debug)]
pub enum SynchronizerOutMsg<Tx> {
    ResponseBatch(Batch<Tx>),
}

pub struct Synchronizer<Tx> {
    store: Storage,
    inner_channel: UnboundedSender<SInMsg<Tx>>,

    /// Track the relay messages for which we are waiting
    relay_waiting: FnvHashMap<BatchHash<Tx>, FnvHashMap<Id, RelayMsg<Id, Tx, Round>>>,
}

impl<Tx> Synchronizer<Tx> {
    pub fn new(
        store: Storage,
        my_id: Id,
        tx_outer: UnboundedSender<SOutMsg<Tx>>,
        consensus_peers: FnvHashMap<Id, SocketAddr>,
    ) -> Self
    where
        Tx: Transaction,
    {
        let (tx_from_out, rx_from_out) = unbounded_channel();

        tokio::spawn(Self::run(
            my_id,
            rx_from_out,
            store.clone(),
            consensus_peers,
            tx_outer,
        ));

        Self {
            store,
            inner_channel: tx_from_out,
            relay_waiting: FnvHashMap::default(),
        }
    }

    async fn run(
        my_id: Id,
        mut rx: UnboundedReceiver<SynchronizerInMsg<Tx>>,
        store: Storage,
        consensus_peers: FnvHashMap<Id, SocketAddr>,
        tx_outer: UnboundedSender<SynchronizerOutMsg<Tx>>,
    ) -> Result<()>
    where
        Tx: Transaction,
    {
        let mut network =
            TcpSimpleSender::<_, ProtocolMsg<Id, Tx, Round>, Acknowledgement>::with_peers(
                consensus_peers,
            );

        let mut waiting = FuturesUnordered::new();
        let mut pending = FnvHashSet::default();

        loop {
            if let Err(e) = tokio::select! {
                Some(request) = rx.recv() => {
                    match request {
                        SynchronizerInMsg::RequestBatch(req_hash, sender) => {
                            let req_msg = ProtocolMsg::BatchRequest{
                                source: my_id,
                                request: Request::new(req_hash.clone()),
                            };
                            network.send(sender, req_msg)
                                .await;
                            if pending.contains(&req_hash) {
                                continue;
                            }
                            pending.insert(req_hash.clone());
                            waiting.push(
                                Self::wait_for_batch(
                                    store.clone(),
                                    req_hash
                                )
                            )
                        },
                    };
                    Ok(())
                },
                Some(batch) = waiting.next() => {
                    match batch {
                        Ok(b) => {
                            tx_outer.send(SOutMsg::ResponseBatch(b))
                                .map_err(anyhow::Error::new)
                                .context("Failed to notify read")
                        },
                        Err(e) => Err(e),
                    }
                }
            } {
                error!("Error: {}", e);
            }
        }
    }

    async fn wait_for_batch(
        mut store: Storage,
        req_hash: BatchHash<Tx>,
    ) -> Result<Batch<Tx>>
    where
        Tx: DeserializeOwned,
    {
        match store.notify_read(req_hash.to_vec()).await {
            Ok(serialized) => {
                let batch =
                    bincode::deserialize::<Batch<Tx>>(&serialized).map_err(anyhow::Error::new)?;
                Ok(batch)
            }
            Err(e) => Err(e),
        }
    }

    /// Public API:
    /// The protocol asks the synchronizer for help with an unknown batch
    pub async fn on_unknown_batch(
        &mut self,
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        batch_hash: BatchHash<Tx>,
        source: Id, // Whoever sent this unknown hash
    ) -> Result<()>
    where
        Tx: Transaction,
    {
        // Check if duplicate relay for the same batch_hash
        let retry_map = self
            .relay_waiting
            .entry(batch_hash.clone())
            .or_insert_with(FnvHashMap::default);
        if retry_map.contains_key(&source) {
            warn!("Duplicate relay received from the same sender: {}", source);
            return Ok(());
        }
        retry_map.insert(source, (proposal, auth, batch_hash.clone(), source));
        // Send request for help
        self.inner_channel
            .send(SInMsg::RequestBatch(batch_hash, source))
            .map_err(anyhow::Error::new)
            .context("Error sending message to Synchronizer")
    }

    /// Public API
    /// The protocol lets the synchronizer know that we received some
    /// synchronized response
    pub async fn on_batch_response(
        &mut self,
        response: Response<Batch<Tx>>,
    ) -> Result<()>
    where
        Tx: Transaction,
    {
        let batch_hash = response.request_hash().clone();
        let serialized = bincode::serialize(&response.response())
            .map_err(anyhow::Error::new)
            .context("Failed to serialize received batch response")?;
        if batch_hash != Hash::do_hash(&serialized) {
            return Err(anyhow!("Got an invalid response for a batch request"));
        }
        // Write to DB, so that notify_read will complete and will notify via
        // rx_synchronizer in the protocol
        self.store.write(batch_hash.to_vec(), serialized).await;
        Ok(())
    }

    /// Advance the round of the synchronizer
    pub fn advance_round(&mut self) {
        self.relay_waiting.clear();
    }
}

impl<Tx> Leto<Tx> {
    /// When a requested batch is ready, this function tries to process the
    /// relay message
    pub async fn on_batch_ready(
        &mut self,
        batch: Batch<Tx>,
    ) -> Result<()>
    where
        Tx: Transaction,
    {
        let batch_hash = Hash::ser_and_hash(&batch);
        let waiting = match self.synchronizer.relay_waiting.remove(&batch_hash) {
            None => return Ok(()),
            Some(map) => map,
        };
        // Pick any one and move on
        for (_, (proposal, auth, batch_hash, source2)) in waiting {
            self.tx_msg_loopback.send(ProtocolMsg::Relay {
                proposal,
                auth,
                batch_hash,
                sender: source2,
            })?;
        }

        Ok(())
    }
}
