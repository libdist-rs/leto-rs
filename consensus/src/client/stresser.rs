use super::Settings;
use crate::{to_socket_address, Id};
use anyhow::anyhow;
use anyhow::Result;
use fnv::FnvHashMap;
use log::*;
use rand::{thread_rng, Rng};
use std::marker::PhantomData;
use std::time::Duration;
use tcp_sender::TcpSimpleSender;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{
    mpsc::unbounded_channel,
    oneshot,
};

/// This is a client implementation that stresses the BFT-system
pub struct Stressor<Tx> {
    id: Id,
    exit_rx: oneshot::Receiver<()>,
    settings: Settings,
    consensus_sender: TcpSimpleSender<Id, Tx>,
    consensus_rx: UnboundedReceiver<Tx>,
    _x: PhantomData<Tx>,
}

impl<Tx> Stressor<Tx>
where
    Tx: super::MockTx,
{
    pub fn spawn(
        my_id: Id,
        settings: Settings,
    ) -> Result<oneshot::Sender<()>> {

        // NOTE: Used for benchmarking
        info!("Transactions size: {} B", settings.bench_config.tx_size);
        info!("Transactions rate: {} tx/s", (settings.bench_config.txs_per_burst as u64 * 1000)/settings.bench_config.burst_interval_ms);

        let (exit_tx, exit_rx) = oneshot::channel();

        let mut peer_map = FnvHashMap::default();
        // These are all server Ids
        let all_ids = settings.consensus_config.get_all_ids();
        for id in &all_ids {
            let party = settings
                .consensus_config
                .get(id)
                .ok_or_else(|| anyhow!("Unknown party [{}]", id))?;
            let consensus_addr = to_socket_address(&party.address, party.port)?;
            peer_map.insert(*id, consensus_addr);
        }
        debug!("Using servers: {:?}", peer_map);
        let consensus_sender = TcpSimpleSender::<Id, Tx>::with_peers(peer_map);

        // Networking setup
        let (consensus_tx, consensus_rx) = unbounded_channel();
        let my_addr = to_socket_address("0.0.0.0", 0)?; // Random available port
        let mut receiver = tcp_receiver::TcpReceiver::<Tx>::spawn(my_addr);
        // Spawn a forwarding task
        tokio::spawn(async move {
            use futures_util::StreamExt;
            while let Some(Ok(msg)) = receiver.next().await {
                if consensus_tx.send(msg).is_err() {
                    break;
                }
            }
        });

        // Start the client
        tokio::spawn(async move {
            Self {
                id: my_id,
                exit_rx,
                settings,
                consensus_sender,
                consensus_rx,
                _x: PhantomData,
            }
            .run()
            .await
        });
        Ok(exit_tx)
    }

    async fn run(&mut self) -> Result<()> {
        // Get stress settings
        let burst_tx = self.settings.bench_config.txs_per_burst;
        let tx_size = self.settings.bench_config.tx_size;
        let all_ids = self.settings.consensus_config.get_all_ids();

        // Start the client
        let mut tx_id: usize = 0;
        // Burst timer
        let mut burst_timer = tokio::time::interval(Duration::from_millis(
            self.settings.bench_config.burst_interval_ms,
        ));
        #[cfg(feature = "microbench")]
        let mut first = true;
        let mut sample_id: u64 = thread_rng().gen();

        // NOTE: This log entry is used to compute performance.
        info!("Start sending transactions");

        loop {
            tokio::select! {
                _ = &mut self.exit_rx => {
                    info!("Shutting down the client");
                    break;
                }
                _ = burst_timer.tick() => {
                    // Time to send a burst of transactions
                    // Send `burst_tx` transactions every interval
                    for i in 0..burst_tx {
                        let tx = Tx::mock_transaction(
                            tx_id,
                            self.id,
                            tx_size,
                            i == 0,
                            sample_id,
                        );
                        #[cfg(feature = "benchmark")]
                        {
                            if i == 0 {
                                info!("Sending sample transaction {}", sample_id);
                            }
                        }
                        #[cfg(feature = "microbench")]
                        {
                            if first {
                                info!(
                                    "Tx size: {}",
                                    bincode::serialized_size(&tx)?,
                                );
                                first = false;
                            }
                        }
                        let bytes = bytes::Bytes::from(bincode::serialize(&tx).unwrap());
                        let _ = self.consensus_sender.broadcast(
                            &all_ids,
                            bytes,
                        ).await;
                        tx_id += 1;
                    }
                    sample_id += 1;
                }
                confirmation = self.consensus_rx.recv() => {
                    info!("Received a confirmation message: {:?}", confirmation);
                    // TODO: Handle tx confirmation
                }
            }
        }
        Ok(())
    }
}
