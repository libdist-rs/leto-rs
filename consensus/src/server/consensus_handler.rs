use super::ProtocolMsg;
use async_trait::async_trait;
use futures_util::SinkExt;
use network::Acknowledgement;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub struct Handler {
    consensus_tx: UnboundedSender<ProtocolMsg>,
}

impl Handler {
    pub fn new(consensus_tx: UnboundedSender<ProtocolMsg>) -> Self {
        Self { consensus_tx }
    }
}

#[async_trait]
impl network::Handler<Acknowledgement, ProtocolMsg> for Handler {
    async fn dispatch(
        &self,
        msg: ProtocolMsg,
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
