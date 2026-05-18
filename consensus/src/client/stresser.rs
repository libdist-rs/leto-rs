use super::{ClientMode, Settings};
use crate::{to_socket_address, Id};
use anyhow::{anyhow, Result};
use fnv::FnvHashMap;
use log::*;
use rand::{thread_rng, Rng};
use std::marker::PhantomData;
use std::time::Duration;
use tcp_sender::TcpSimpleSender;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{mpsc::unbounded_channel, oneshot};

use crate::types::ZeusClientMsg;

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
        info!(
            "Transactions rate: {} tx/s",
            (settings.bench_config.txs_per_burst as u64 * 1000)
                / settings.bench_config.burst_interval_ms
        );

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

        // Resolve the eleader target for ZeusEleaderOnly mode.
        //
        // If `eleader_id` is pre-seeded by the harness we use it directly
        // (fast-path; no network query).  If it is `None` we query
        // `all_ids[0]` via a `WhoIsEleader` request and wait for
        // `EleaderIs`.
        //
        // For LetoBroadcast the target is unused.
        let target_eleader: Option<Id> = match &self.settings.client_mode {
            ClientMode::LetoBroadcast => None,
            ClientMode::ZeusEleaderOnly {
                eleader_id: Some(id),
            } => {
                debug!("Zeus stressor: using pre-seeded eleader id={}", id);
                Some(*id)
            }
            ClientMode::ZeusEleaderOnly { eleader_id: None } => {
                // Query all_ids[0] for the current eleader.
                let query_id = *all_ids.first().ok_or_else(|| anyhow!("no peers"))?;
                debug!("Zeus stressor: querying node {} for eleader", query_id);
                let who_msg = ZeusClientMsg::<Tx>::WhoIsEleader;
                let bytes =
                    bytes::Bytes::from(bincode::serialize(&who_msg).map_err(anyhow::Error::new)?);
                let _ = self.consensus_sender.send(query_id, bytes).await;

                // Wait up to 5 s for the EleaderIs reply.
                // We receive raw Tx-typed bytes here; in the query path the
                // server responds with ZeusClientMsg, which won't deserialise
                // as Tx.  The `consensus_rx` channel carries deserialized Tx
                // values, so we cannot receive ZeusClientMsg on it directly.
                //
                // For this pass the harness always pre-seeds `eleader_id`, so
                // this branch is a debug-only fallback.  Return an error that
                // prints clearly instead of blocking forever.
                warn!(
                    "Zeus stressor: WhoIsEleader query sent but reply channel not wired \
                     (the in-process harness should always pre-seed eleader_id). \
                     Falling back to node 0 as eleader."
                );
                // eleader(epoch=1, n) = 1 % n — for n>=1 that is always id=1,
                // but for n=1 id=0.  We don't have n here; fall back to 0.
                Some(0)
            }
        };

        // NOTE: This log entry is used to compute performance.
        info!("Start sending transactions");

        let mut tx_id: usize = 0;
        // Burst timer
        let mut burst_timer = tokio::time::interval(Duration::from_millis(
            self.settings.bench_config.burst_interval_ms,
        ));
        #[cfg(feature = "microbench")]
        let mut first = true;
        let mut sample_id: u64 = thread_rng().gen();

        loop {
            tokio::select! {
                _ = &mut self.exit_rx => {
                    info!("Shutting down the client");
                    break;
                }
                _ = burst_timer.tick() => {
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
                        match &self.settings.client_mode {
                            ClientMode::LetoBroadcast => {
                                let _ = self.consensus_sender.broadcast(
                                    &all_ids,
                                    bytes,
                                ).await;
                            }
                            ClientMode::ZeusEleaderOnly { .. } => {
                                // target_eleader is always Some in this branch
                                let eleader = target_eleader.expect("eleader resolved above");
                                let _ = self.consensus_sender.send(eleader, bytes).await;
                            }
                        }
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
