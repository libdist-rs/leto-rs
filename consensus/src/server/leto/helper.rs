use crate::{
    types::{ProtocolMsg, Request, Response, Transaction, Element},
    Id, Round,
};
use anyhow::Result;
use fnv::FnvHashMap;
use log::*;
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
        loop {
            if let Err(e) = tokio::select! {
                Some(help) = self.rx_requests.recv() => {
                    match help {
                        HelperRequest::BatchRequest(source, req) => {
                            let req_hash = req.request_hash().clone();
                            match self.handle_request(source, req).await {
                                Ok(Some(batch)) => {
                                    let response_msg = ProtocolMsg::BatchResponse{
                                        response: Response::new(
                                            req_hash,
                                            batch,
                                        )
                                    };
                                    self.network.send(source, response_msg).await;
                                    Ok(())
                                },
                                Ok(None) => Ok(()),
                                Err(e) => Err(e),
                            }
                        },
                        HelperRequest::ElementRequest(source, req) => {
                            let req_hash = req.request_hash().clone();
                            match self.handle_request(source, req).await {
                                Ok(Some(element)) => {
                                    let response_msg = ProtocolMsg::ElementResponse{
                                        response: Response::new(
                                            req_hash,
                                            element,
                                        )
                                    };
                                    self.network.send(source, response_msg).await;
                                    Ok(())
                                },
                                Ok(None) => Ok(()),
                                Err(e) => Err(e),
                            }
                        }
                    }
                }
            } {
                error!("Error helping with request: {}", e);
            }
        }
    }

    async fn handle_request<T>(
        &mut self, 
        source: Id, 
        req: Request<T>,
    ) -> Result<Option<T>> 
    where 
        T: DeserializeOwned,
    {
        let req_hash = req.request_hash().clone();
        let key = req_hash.to_vec();
        self.db
            .read(req_hash)
            .await
    }
}