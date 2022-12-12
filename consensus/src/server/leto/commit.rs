use super::Leto;
use crate::{
    types::{Block, Transaction},
    Round,
};
use anyhow::Result;
use log::*;
use mempool::Batch;
use storage::rocksdb::Storage;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

pub enum CommitMsg<Tx> {
    EndRound(Round, Tx),
}

pub struct CommitContext<Tx> {
    tx_inner: UnboundedSender<CommitMsg<Tx>>,
}

impl<Tx> CommitContext<Tx> {
    pub fn spawn(
        store: Storage,
        tx_commit: UnboundedSender<Batch<Tx>>,
    ) -> Self
    where
        Tx: Transaction,
    {
        let (tx_inner, rx_inner) = unbounded_channel();
        tokio::spawn(async move {
            if let Err(e) = Self::run(store, tx_commit, rx_inner).await {
                error!("Commit Helper shut down: {}", e);
            }
        });
        Self { tx_inner }
    }

    async fn run(
        store: Storage,
        tx_commit: UnboundedSender<Batch<Tx>>,
        mut rx_inner: UnboundedReceiver<CommitMsg<Tx>>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                msg = rx_inner.recv() => {
                    info!("Committing");
                },
            }
        }
    }
}

impl<Tx> Leto<Tx> {
    pub async fn try_commit(&mut self) -> Result<()> {
        // Go back (n+t+1)/2 rounds
        // let rounds_back: usize =
        // (self.settings.consensus_config.num_nodes()+1+self.settings.consensus_config.
        // num_faults)/2; self.commit_ctx

        todo!("Implement committing");
    }
}

pub struct DummyCommitSink {}

impl DummyCommitSink {
    pub fn spawn<Tx>(mut rx_inner: UnboundedReceiver<Batch<Tx>>)
    where
        Tx: Transaction,
    {
        tokio::spawn(async move {
            while let Some(_batch) = rx_inner.recv().await {
                // Process the batch of transactions
            }
        });
    }
}
