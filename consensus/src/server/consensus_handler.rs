use crate::types::{self, ProtocolMsg};
use async_trait::async_trait;
use futures_util::SinkExt;
use network::{Acknowledgement, Identifier, Message};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub struct Handler<Id, Transaction, Round> {
    consensus_tx: UnboundedSender<ProtocolMsg<Id, Transaction, Round>>,
}

impl<Id, Transaction, Round> Handler<Id, Transaction, Round> {
    pub fn new(
        consensus_tx: UnboundedSender<ProtocolMsg<Id, Transaction, Round>>
    ) -> Self 
    {
        Self { consensus_tx }
    }
}

#[async_trait]
impl<Id, Transaction, Round> network::Handler<Acknowledgement, ProtocolMsg<Id, Transaction, Round>>
    for Handler<Id, Transaction, Round>
where
    Id: Identifier,
    Transaction: types::Transaction,
    Round: mempool::Round + Message,
{
    async fn dispatch(
        &self,
        msg: ProtocolMsg<Id, Transaction, Round>,
        writer: &mut network::Writer<Acknowledgement>,
    ) {
        // Forward the message
        self.consensus_tx
            .send(msg)
            .expect("Failed to send message to the consensus channel");

        // Acknowledge
        writer
            .send(Acknowledgement::Pong)
            .await
            .expect("Failed to send an acknowledgement");
    }
}
