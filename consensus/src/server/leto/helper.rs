use crate::{
    types::{ProtocolMsg, Request, Response, Transaction, Element},
    Id, Round,
};
use anyhow::Result;
use fnv::FnvHashMap;
use futures_util::{stream::FuturesUnordered, StreamExt};
use mempool::Batch;
use network::{plaintcp::TcpSimpleSender, Acknowledgement, NetSender};
use serde::de::DeserializeOwned;
use std::net::SocketAddr;
use storage::rocksdb::Storage;
use tokio::sync::mpsc::UnboundedReceiver;
use super::ChainDB;

#[derive(Debug)]
pub enum HelperRequest<Tx> {
    BatchRequest(Id, Request<Batch<Tx>>),
    ElementRequest(Id, Request<Element<Id, Tx, Round>>),
}

pub struct Helper<Tx> {
    /// The persistent storage
    db: ChainDB,
    /// Channel to handle requests from others
    rx_requests: UnboundedReceiver<HelperRequest<Tx>>,
    /// A simple sender to send responses to the sender
    network: TcpSimpleSender<Id, ProtocolMsg<Id, Tx, Round>, Acknowledgement>,
}

impl<Tx> Helper<Tx> {
    pub fn spawn(
        store: Storage,
        rx_requests: UnboundedReceiver<HelperRequest<Tx>>,
        consensus_peers: FnvHashMap<Id, SocketAddr>,
    ) where
        Tx: Transaction,
    {
        let network = TcpSimpleSender::with_peers(consensus_peers);

        tokio::spawn(async move {
            Self {
                db: ChainDB::new(store),
                rx_requests,
                network,
            }
            .run()
            .await
        });
    }

    async fn run(&mut self) -> Result<()>
    where
        Tx: Transaction,
    {
        let mut element_read_queue = FuturesUnordered::new();
        let mut batch_read_queue = FuturesUnordered::new();
        loop {
            tokio::select! {
                Some(help) = self.rx_requests.recv() => {
                    match help {
                        HelperRequest::BatchRequest(source, req) => {
                            batch_read_queue.push(
                                Self::handle_request(
                                    self.db.clone(), 
                                    source, 
                                    req,
                                )
                            );
                        },
                        HelperRequest::ElementRequest(source, req) => {
                            element_read_queue.push(
                                Self::handle_request(
                                    self.db.clone(), 
                                    source, 
                                    req,
                                )
                            );
                        }
                    }
                },
                Some(Ok(Some((source, req, batch)))) = batch_read_queue.next(), 
                    if !batch_read_queue.is_empty() => {
                    let response_msg = ProtocolMsg::BatchResponse{
                        response: Response::new(
                            req.request_hash().clone(),
                            batch,
                        )
                    };
                    self.network.send(source, response_msg).await;
                },
                Some(Ok(Some((source, req, element)))) = element_read_queue.next(), 
                if !element_read_queue.is_empty() => {
                    let response_msg = ProtocolMsg::ElementResponse{
                        response: Response::new(
                            req.request_hash().clone(),
                            element,
                        )
                    };
                    self.network.send(source, response_msg).await;
                }
            } 
        }
    }

    async fn handle_request<T>(
        mut db: ChainDB,
        source: Id,
        req: Request<T>,
    ) -> Result<Option<(Id, Request<T>, T)>> 
    where 
        T: DeserializeOwned,
    {
        let req_hash = req
            .request_hash()
            .clone();
        db
            .read(req_hash)
            .await
            .map(|opt| opt.map(|v| (source, req, v)))
    }
}