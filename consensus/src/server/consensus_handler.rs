use crate::types::ProtocolMsg;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub struct Handler<Id, Tx, Round> {
    consensus_tx: UnboundedSender<ProtocolMsg<Id, Tx, Round>>,
}

impl<Id, Tx, Round> Handler<Id, Tx, Round> {
    pub fn new(consensus_tx: UnboundedSender<ProtocolMsg<Id, Tx, Round>>) -> Self {
        Self { consensus_tx }
    }

    pub fn dispatch(&self, msg: ProtocolMsg<Id, Tx, Round>) {
        self.consensus_tx
            .send(msg)
            .expect("Failed to send message to the consensus channel");
    }
}
