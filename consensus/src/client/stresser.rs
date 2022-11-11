use crate::{Id, to_socket_address, client::ClientMsg};
use anyhow::Result;
use async_trait::async_trait;
use fnv::FnvHashMap;
use anyhow::anyhow;
use futures_util::SinkExt;
use network::{Acknowledgement, plaintcp::{TcpSimpleSender, TcpReceiver}};
use tokio::sync::{mpsc::{unbounded_channel, UnboundedSender}, oneshot};
use super::Settings;
use log::*;

/// This is a client implementation that stresses the BFT-system
pub struct Stressor {
    pub id: Id,
}

/*
 * TODO: Implement ConsensusHandler
 * TODO: Implement Client
 * TODO: Use the args
 */

impl Stressor {
    pub fn spawn(
        _my_id: Id,
        settings: Settings,
    ) -> Result<oneshot::Sender<()>> {
        let mut peer_map = FnvHashMap::default();
        // These are all server Ids
        for id in settings.consensus_config.get_all_ids() {
            let party = settings.consensus_config
                    .get(&id)
                    .ok_or(anyhow!("Unknown party [Possibly corrupt settings]"))?;
            let consensus_addr = to_socket_address(
                &party.consensus_address, 
                party.client_port
            )?;
            peer_map.insert(
                id, 
                consensus_addr
            );
        }
        
        let (consensus_tx, consensus_rx) = unbounded_channel();
        let my_addr = to_socket_address("0.0.0.0", settings.port)?;
        TcpReceiver::spawn(
            my_addr, 
            Handler::new(consensus_tx)
        );
        let (exit_tx, mut exit_rx) = oneshot::channel();
        let consensus_sender = TcpSimpleSender::<Id, ClientMsg, Acknowledgement>::with_peers(peer_map);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut exit_rx => {
                        info!("Shutting down the client");
                        break;
                    }
                }
            }
        });
        Ok(exit_tx)
    }
}

#[derive(Debug, Clone)]
struct Handler {
    tx: UnboundedSender<ClientMsg>,
}

impl Handler {
    pub fn new(tx: UnboundedSender<ClientMsg>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl network::Handler<Acknowledgement, ClientMsg> for Handler {
    async fn dispatch(
        &self, 
        msg: ClientMsg, 
        writer: &mut network::Writer<Acknowledgement>,
    ) 
    {
        // Forward the message
        self.tx
            .send(msg)
            .expect("Failed to send message to the consensus channel");

        // Acknowledge
        writer
            .send(Acknowledgement::Pong)
            .await
            .expect("Failed to send an acknowledgement");
        
    }
}