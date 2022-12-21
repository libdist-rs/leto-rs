use std::net::SocketAddr;
use fnv::{FnvHashMap, FnvHashSet};
use futures_util::{stream::FuturesUnordered, StreamExt, FutureExt};
use mempool::{BatchHash, Batch};
use network::{plaintcp::TcpSimpleSender, Acknowledgement, NetSender};
use serde::de::DeserializeOwned;
use storage::rocksdb::Storage;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use crate::{types::{ProtocolMsg, Transaction, Request, Proposal, Signature, Element}, Round, Id, server::{ChainDB, Leto}};
use super::SyncMsg;
use anyhow::{Result, Context};
use log::*;
use crypto::hash::Hash;

pub struct SyncHelper<Tx> {
    my_id: Id, 
    /// A channel to receive messages from the outside
    rx_from_out: UnboundedReceiver<SyncMsg<Tx>>, 
    /// Our storage
    db: ChainDB, 
    /// A channel to tell the outside world that this message is synced and ready
    tx_outer: UnboundedSender<ProtocolMsg<Id, Tx, Round>>,
    /// Networking
    network: TcpSimpleSender<Id, ProtocolMsg<Id, Tx, Round>, Acknowledgement>,
    /// The current round
    current_round: Round,
    /// Pending batches
    pending_relays: FnvHashMap<(Round, Id), FnvHashSet<BatchHash<Tx>>>,
    /// Pending chain delivery
    #[allow(clippy::type_complexity)]
    pending_elements: FnvHashMap<(Round, Id), FnvHashSet<Hash<Element<Id, Tx, Round>>>>,
}

impl<Tx> SyncHelper<Tx> 
where 
    Tx: Transaction,
{
    pub fn new(
        my_id: usize, 
        rx_from_out: UnboundedReceiver<SyncMsg<Tx>>, 
        store: Storage, 
        consensus_peers: FnvHashMap<Id, SocketAddr>, 
        tx_outer: UnboundedSender<ProtocolMsg<Id, Tx, Round>>
    ) -> Self {
        let network = TcpSimpleSender::with_peers(consensus_peers);
        Self {
            rx_from_out,
            my_id, 
            db: ChainDB::new(store),
            tx_outer,
            network,
            current_round: Leto::<Tx>::INITIAL_ROUND,
            pending_relays: FnvHashMap::default(),
            pending_elements: FnvHashMap::default(),
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut relays_waiting = FuturesUnordered::new();
        let mut elements_waiting = FuturesUnordered::new();

        loop {
            if let Err(e) = tokio::select! {
                Some(request) = self.rx_from_out.recv() => {
                    match request {
                        SyncMsg::DeliverBatchOnly(
                            req_hash, 
                            sender, 
                            proposal, 
                            auth
                        ) => {
                            // Check if we are already waiting for this
                            let round_map = self.pending_relays
                                .entry((self.current_round, sender))
                                .or_insert_with(FnvHashSet::default);
                            if round_map.contains(&req_hash) {
                                continue;
                            }
                            round_map.insert(req_hash.clone());
                            
                            // Request batch from the sender
                            let req_msg = ProtocolMsg::BatchRequest{
                                source: self.my_id,
                                request: Request::new(req_hash.clone()),
                            };
                            self.network
                                .send(sender, req_msg)
                                .await;

                            // Add future that will be complete once this batch is loaded
                            relays_waiting.push(
                                Self::on_deliver_batch(
                                    self.db.clone(),
                                    req_hash,
                                    proposal,
                                    auth,
                                    sender,
                                ).boxed()
                            )
                        },
                        SyncMsg::DeliverParentOnly(
                            parent_hash,
                            sender,
                            proposal,
                            auth,
                            batch,
                        ) => {
                            let round_map = self.pending_elements
                                .entry((self.current_round, sender))
                                .or_insert_with(FnvHashSet::default);
                            if round_map.contains(&parent_hash) {
                                continue;
                            }
                            round_map.insert(parent_hash.clone());

                            // Request parent from the sender
                            let req_msg = ProtocolMsg::ElementRequest {
                                source: self.my_id,
                                request: Request::new(parent_hash.clone()),
                            };
                            self.network
                                .send(sender, req_msg)
                                .await;

                            // Add future that will be complete once this batch is loaded
                            elements_waiting.push(
                                Self::on_deliver_parent(
                                    self.db.clone(),
                                    batch,
                                    proposal,
                                    auth,
                                    sender,
                                ).boxed()
                            )
                        },
                        SyncMsg::DeliverParentAndBatch(
                            batch_hash,
                            sender, 
                            proposal,
                            auth,
                        ) => {
                            // Add future that will be complete once this batch is loaded
                            elements_waiting.push(
                                Self::on_deliver_parent_and_batch(
                                    self.db.clone(),
                                    batch_hash.clone(),
                                    proposal,
                                    auth,
                                    sender,
                                ).boxed()
                            );
                            // Send messages appropriately
                            let relay_map = self.pending_relays
                                .entry((self.current_round, sender))
                                .or_default();
                            if !relay_map.contains(&batch_hash) {
                                relay_map.insert(batch_hash.clone());
                                let msg = ProtocolMsg::BatchRequest{
                                    request: Request::new(batch_hash),
                                    source: self.my_id,
                                };
                                self.network.send(sender, msg).await;
                            }
                        },
                        SyncMsg::AdvanceRound(new_round) => {
                            // Ensure we are advancing forward
                            if new_round < self.current_round {
                                continue;
                            }
                            // GC
                            self.pending_relays
                                .retain(|(round, _), _| *round >= new_round);
                            self.pending_elements
                                .retain(|(round, _), _| *round >= new_round);
                            // Update the round
                            self.current_round = new_round;
                        }
                    };
                    Ok(())
                },
                Some(msg_res) = relays_waiting.next() => {
                    match msg_res {
                        Ok(msg) => {
                            // TODO: Clear waiting
                            // self.pending_relays
                            //     .entry((self.current_round, ));

                            self.tx_outer.send(msg)
                                .context("Failed to notify read")
                        },
                        Err(e) => Err(e),
                    }
                }
                Some(msg_res) = elements_waiting.next() => {
                    match msg_res {
                        Ok(msg) => {
                            // TODO: Clear waiting
                            // self.pending_relays
                            //     .entry((self.current_round, ));

                            self.tx_outer.send(msg)
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

    async fn on_deliver_batch(
        mut db: ChainDB,
        batch_hash: BatchHash<Tx>,
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        sender: Id,
    ) -> Result<ProtocolMsg<Id, Tx, Round>>
    where
        Tx: DeserializeOwned,
    {
        let batch = db.notify_read(batch_hash).await?;

        // Now write this proposal so its children can be delivered
        db.write(Element::new(
            proposal.clone(), 
            auth.clone(), 
            batch.clone(),
        )).await?;

        // Deliver message
        Ok(ProtocolMsg::Propose{ 
            proposal, 
            auth, 
            batch, 
            sender
        })
    }

    async fn on_deliver_parent_and_batch(
        mut db: ChainDB,
        batch_hash: BatchHash<Tx>,
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        sender: Id,
    ) -> Result<ProtocolMsg<Id, Tx, Round>>
    where
        Tx: DeserializeOwned,
    {
        let batch = db.notify_read(batch_hash).await?;
        let _ = db.notify_read(proposal.block().parent_hash()).await?;

        // Now write this proposal so its children can be delivered
        db.write(Element::new(
            proposal.clone(), 
            auth.clone(), 
            batch.clone(),
        )).await?;

        // Deliver message
        Ok(ProtocolMsg::Propose{ 
            proposal, 
            auth, 
            batch, 
            sender
        })
    }


    async fn on_deliver_parent(
        mut db: ChainDB,
        batch: Batch<Tx>,
        proposal: Proposal<Id, Tx, Round>,
        auth: Signature<Id, Proposal<Id, Tx, Round>>,
        sender: Id,
    ) -> Result<ProtocolMsg<Id, Tx, Round>>
    where
        Tx: DeserializeOwned,
    {
        let _ = db.notify_read(proposal.block().parent_hash()).await?;

        // Now write this proposal so its children can be delivered
        db.write(Element::new(
            proposal.clone(), 
            auth.clone(), 
            batch.clone(),
        )).await?;

        // Deliver message
        Ok(ProtocolMsg::Propose{ 
            proposal, 
            auth, 
            batch, 
            sender
        })
    }
}