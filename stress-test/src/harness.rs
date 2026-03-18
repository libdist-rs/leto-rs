use crate::config::{build_server_settings, StressTestConfig};
use crate::metrics::MetricsCollector;
use crate::smr::{SimpleData, SimpleTx};
use anyhow::Result;
use consensus::{server::Server, Id, KeyConfig};
use crypto::Algorithm;
use mempool::Batch;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::{mpsc::unbounded_channel, oneshot};

pub struct NodeHarness {
    exit_senders: Vec<oneshot::Sender<()>>,
    /// Extra commit receivers for non-metrics nodes (kept alive so channels don't close)
    _drain_tasks: Vec<tokio::task::JoinHandle<()>>,
    pub temp_dir: TempDir,
}

impl NodeHarness {
    pub fn spawn_nodes(
        config: &StressTestConfig,
        metrics: &MetricsCollector,
    ) -> Result<Self> {
        let temp_dir = TempDir::new()?;
        let settings = build_server_settings(config, temp_dir.path())?;
        let crypto_keys = KeyConfig::generate(Algorithm::ED25519, config.num_nodes)?;
        let all_ids: Vec<Id> = (0..config.num_nodes).collect();

        let mut exit_senders = Vec::new();
        let mut drain_tasks = Vec::new();
        let nodes_to_spawn = config.num_nodes - config.num_crash_faults;

        for id in 0..nodes_to_spawn {
            let (tx_commit, rx_commit) = unbounded_channel::<Arc<Batch<SimpleTx<SimpleData>>>>();

            if id == 0 {
                // Node 0's commits feed the metrics collector
                metrics.register_commit_receiver(rx_commit);
            } else {
                // Drain other nodes' commit channels to prevent backpressure
                let handle = tokio::spawn(async move {
                    let mut rx = rx_commit;
                    while rx.recv().await.is_some() {}
                });
                drain_tasks.push(handle);
            }

            let exit_tx = Server::<SimpleTx<SimpleData>>::spawn(
                id,
                all_ids.clone(),
                crypto_keys[id].clone(),
                settings.clone(),
                tx_commit,
            )?;
            exit_senders.push(exit_tx);
        }

        Ok(Self {
            exit_senders,
            _drain_tasks: drain_tasks,
            temp_dir,
        })
    }

    pub fn shutdown(self) {
        for sender in self.exit_senders {
            let _ = sender.send(());
        }
        // temp_dir is dropped here, cleaning up RocksDB files
    }
}
