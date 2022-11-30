use crate::types::{self, ProtocolMsg};
use async_trait::async_trait;
use futures_util::SinkExt;
use network::{Acknowledgement, Identifier, Message};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub struct Handler<Id, Tx, Round> {
    consensus_tx: UnboundedSender<ProtocolMsg<Id, Tx, Round>>,
}

impl<Id, Tx, Round> Handler<Id, Tx, Round> {
    pub fn new(consensus_tx: UnboundedSender<ProtocolMsg<Id, Tx, Round>>) -> Self {
        Self { consensus_tx }
    }
}

#[async_trait]
impl<Id, Tx, Round> network::Handler<Acknowledgement, ProtocolMsg<Id, Tx, Round>>
    for Handler<Id, Tx, Round>
where
    Id: Identifier,
    Tx: types::Transaction,
    Round: mempool::Round + Message,
{
    async fn dispatch(
        &self,
        msg: ProtocolMsg<Id, Tx, Round>,
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
