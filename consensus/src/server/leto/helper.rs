use crate::{
    types::{ProtocolMsg, Request, Response, Transaction},
    Id, Round,
};
use anyhow::{anyhow, Result};
use fnv::FnvHashMap;
use log::*;
use mempool::Batch;
use network::{plaintcp::TcpSimpleSender, Acknowledgement, NetSender};
use std::net::SocketAddr;
use storage::rocksdb::Storage;
use tokio::sync::mpsc::UnboundedReceiver;

use super::Leto;

#[derive(Debug)]
pub enum HelperRequest<Tx> {
    BatchRequest(Id, Request<Batch<Tx>>),
}

pub struct Helper<Tx> {
    /// The persistent storage
    store: Storage,
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
                store,
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
                            let key = req_hash.to_vec();
                            let batch_opt = self.store
                                .read(key)
                                .await
                                .and_then(|res| {
                                    res.ok_or_else(|| anyhow!(
                                        "Unknown batch",
                                    ))
                                })
                                .and_then(|b| {
                                    bincode::deserialize::<Batch<Tx>>(&b)
                                        .map_err(anyhow::Error::new)
                                });
                            match batch_opt {
                                Ok(batch) => {
                                    let response_msg = ProtocolMsg::BatchResponse{
                                        response: Response::new(
                                            req_hash,
                                            batch,
                                        )
                                    };
                                    self.network.send(source, response_msg).await;
                                    Ok(())
                                },
                                Err(e) => Err(e),
                            }
                        },
                    }
                }
            } {
                error!("Error helping with request: {}", e);
            }
        }
    }
}

impl<Tx> Leto<Tx> {
    pub async fn on_batch_request(
        &mut self,
        source: Id,
        request: Request<Batch<Tx>>,
    ) -> Result<()>
    where
        Tx: Transaction,
    {
        let helper_msg = HelperRequest::BatchRequest(source, request);
        self.tx_helper.send(helper_msg).map_err(anyhow::Error::new)
    }
}
