use anyhow::Result;
use clap::Parser;
use consensus::{
    client,
    server::{self, BenchConfig, StorageConfig},
    Id, Round,
};
use fnv::FnvHashMap;
use std::{path::Path, time::Duration};

#[derive(Parser, Debug, Clone)]
#[command(name = "stress-test", about = "Leto BFT load/throughput stress test")]
pub struct StressTestConfig {
    /// Total number of consensus nodes
    #[arg(long, default_value_t = 4)]
    pub num_nodes: usize,

    /// Number of crash faults (nodes not spawned)
    #[arg(long, default_value_t = 0)]
    pub num_crash_faults: usize,

    /// Transaction payload size in bytes
    #[arg(long, default_value_t = 512)]
    pub tx_size: usize,

    /// Batch size threshold in bytes
    #[arg(long, default_value_t = 500_000)]
    pub batch_size: usize,

    /// Max batch delay in milliseconds
    #[arg(long, default_value_t = 1000)]
    pub batch_timeout_ms: u64,

    /// Network delay parameter (Delta) in milliseconds
    #[arg(long, default_value_t = 500)]
    pub delay_in_ms: u64,

    /// Starting load rate in tx/s
    #[arg(long, default_value_t = 1000)]
    pub load_start: u64,

    /// Load increment per level in tx/s
    #[arg(long, default_value_t = 5000)]
    pub load_step: u64,

    /// Maximum load rate to attempt in tx/s
    #[arg(long, default_value_t = 200_000)]
    pub load_max: u64,

    /// Duration per load level in seconds
    #[arg(long, default_value_t = 30)]
    pub duration_per_level_secs: u64,

    /// Warmup duration before measuring in seconds
    #[arg(long, default_value_t = 5)]
    pub warmup_secs: u64,

    /// Number of client stressors
    #[arg(long, default_value_t = 1)]
    pub num_clients: usize,

    /// Base port for consensus network
    #[arg(long, default_value_t = 18000)]
    pub base_consensus_port: u16,

    /// Base port for mempool network
    #[arg(long, default_value_t = 19000)]
    pub base_mempool_port: u16,

    /// Base port for client network
    #[arg(long, default_value_t = 20000)]
    pub base_client_port: u16,
}

impl StressTestConfig {
    /// Generate the sequence of load levels to test
    pub fn load_levels(&self) -> Vec<u64> {
        let mut levels = Vec::new();
        let mut rate = self.load_start;
        while rate <= self.load_max {
            levels.push(rate);
            rate += self.load_step;
        }
        if levels.is_empty() {
            levels.push(self.load_start);
        }
        levels
    }
}

pub fn build_server_settings(
    config: &StressTestConfig,
    temp_dir: &Path,
) -> Result<server::Settings> {
    let mut parties = FnvHashMap::default();
    for id in 0..config.num_nodes {
        parties.insert(
            id,
            server::Party {
                id,
                consensus_address: "127.0.0.1".to_string(),
                consensus_port: config.base_consensus_port + id as u16,
                mempool_address: "127.0.0.1".to_string(),
                mempool_port: config.base_mempool_port + id as u16,
                client_port: config.base_client_port + id as u16,
            },
        );
    }

    let mempool_config = mempool::Config::<Round>::default();
    let storage_config = StorageConfig {
        base: temp_dir.to_string_lossy().to_string(),
        prefix: "stress-db".to_string(),
    };
    let bench_config = BenchConfig {
        batch_size: config.batch_size,
        batch_timeout: Duration::from_millis(config.batch_timeout_ms),
        delay_in_ms: config.delay_in_ms,
    };

    Ok(server::Settings {
        committee_config: server::Config { parties },
        mempool_config,
        storage: storage_config,
        bench_config,
    })
}

pub fn build_client_settings(
    config: &StressTestConfig,
    target_rate: u64,
) -> client::Settings {
    let rate_per_client = target_rate / config.num_clients.max(1) as u64;
    let burst_interval_ms: u64 = 50;
    let txs_per_burst = ((rate_per_client * burst_interval_ms) / 1000).max(1) as usize;

    let mut client_parties = FnvHashMap::default();
    for id in 0..config.num_nodes {
        client_parties.insert(
            id as Id,
            client::Party {
                id,
                address: "127.0.0.1".to_string(),
                port: config.base_client_port + id as u16,
            },
        );
    }

    client::Settings {
        bench_config: client::Bench {
            burst_interval_ms,
            tx_size: config.tx_size,
            txs_per_burst,
        },
        consensus_config: client::Config {
            parties: client_parties,
        },
    }
}
