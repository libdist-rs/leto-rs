/// Nonce-dedup integration test.
///
/// Spawns 4 Leto nodes in-process with a single Stressor client in
/// LetoBroadcast mode.  Collects every committed transaction on node 0
/// for a fixed observation window and asserts:
///
///   1. No (client_id, nonce) pair appears in two distinct committed batches
///      (the primary correctness signal for the nonce-keyed mempool).
///   2. `DP[Throughput]` ≈ offered rate (≤ 2× offered), NOT n× offered.
///      (Measured as committed tx count / window duration.)
///
/// Port range: 13000–13999 (distinct from crash-fault range 11000–12999
/// and launch-test range 6000–9999).
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    client::{self, Stressor},
    server::{BenchConfig, Config, Party, Server, Settings, StorageConfig},
    Id, KeyConfig, Round, SimpleData, SimpleTx,
};
use anyhow::Result;
use consensus::types::Transaction;
use crypto::Algorithm;
use fnv::FnvHashMap;
use mempool::Batch;
use tokio::sync::{mpsc::unbounded_channel, oneshot};

const NUM_NODES: usize = 4;
const CLIENT_ID: Id = NUM_NODES;
const BASE_CONSENSUS_PORT: u16 = 13000;
const BASE_MEMPOOL_PORT: u16 = 13200;
const BASE_CLIENT_PORT: u16 = 13400;
const BASE_CONSENSUS_CLIENT_PORT: u16 = 13600;

// Offered rate per client × 1 client = total offered.
const TX_PER_BURST: usize = 20;
const BURST_INTERVAL_MS: u64 = 50;
// Offered rate = TX_PER_BURST / BURST_INTERVAL_MS * 1000 = 400 tx/s
const OFFERED_TX_PER_SEC: u64 = (TX_PER_BURST as u64 * 1000) / BURST_INTERVAL_MS;

const OBSERVATION_SECS: u64 = 15;
const WARMUP_SECS: u64 = 4;
// Allow up to 2× offered: accounts for the timer-triggered batch that may
// include some txs duplicated across the first round before the pool is
// seeded.  Pre-fix this would be ~4× for n=4.
const MAX_RATIO: f64 = 2.0;

fn build_server_settings(db_dir: &std::path::Path) -> Settings {
    let mut parties: FnvHashMap<Id, Party> = FnvHashMap::default();
    for id in 0..NUM_NODES {
        parties.insert(
            id,
            Party {
                id,
                consensus_address: "127.0.0.1".to_string(),
                consensus_port: BASE_CONSENSUS_PORT + id as u16,
                mempool_address: "127.0.0.1".to_string(),
                mempool_port: BASE_MEMPOOL_PORT + id as u16,
                client_port: BASE_CLIENT_PORT + id as u16,
                consensus_client_port: BASE_CONSENSUS_CLIENT_PORT + id as u16,
            },
        );
    }
    Settings {
        committee_config: Config { parties },
        mempool_config: mempool::Config::<Round>::default(),
        storage: StorageConfig {
            base: db_dir.to_string_lossy().to_string(),
            prefix: "nonce-dedup-db".to_string(),
        },
        bench_config: BenchConfig {
            batch_size: 50_000,
            batch_timeout: Duration::from_millis(200),
            delay_in_ms: 200,
            eleader_pipeline_depth: 16,
            data_timer_duration_ms: 1000,
            bench_emit_window_secs: 5,
            bench_metrics_node: 0,
        },
    }
}

fn build_client_settings() -> client::Settings {
    let mut client_parties: FnvHashMap<Id, client::Party> = FnvHashMap::default();
    for id in 0..NUM_NODES {
        client_parties.insert(
            id,
            client::Party {
                id,
                address: "127.0.0.1".to_string(),
                port: BASE_CONSENSUS_CLIENT_PORT + id as u16,
                confirmation_port: 0,
            },
        );
    }
    client::Settings {
        bench_config: client::Bench {
            burst_interval_ms: BURST_INTERVAL_MS,
            tx_size: 256,
            txs_per_burst: TX_PER_BURST,
            bench_emit_window_secs: 5,
            emit_dp: false,
        },
        consensus_config: client::Config {
            parties: client_parties,
        },
        client_mode: client::ClientMode::LetoBroadcast,
        my_confirmation_address: "0.0.0.0".to_string(),
        my_confirmation_port: 0,
    }
}

#[tokio::test]
async fn test_nonce_dedup_no_replay() -> Result<()> {
    // Collect all committed (client_id, nonce) pairs from node 0.
    let seen: Arc<Mutex<HashMap<(Id, u64), usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let tx_count: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));

    let db_dir = {
        let mut p = std::env::temp_dir();
        p.push(format!("leto-nonce-dedup-{}", std::process::id()));
        std::fs::create_dir_all(&p)?;
        p
    };

    let server_settings = build_server_settings(&db_dir);
    let client_settings = build_client_settings();
    let crypto_keys = KeyConfig::generate(Algorithm::ED25519, NUM_NODES)?;
    let all_ids: Vec<Id> = (0..NUM_NODES).collect();

    let mut exit_senders: Vec<oneshot::Sender<()>> = Vec::new();
    let mut drain_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    for id in 0..NUM_NODES {
        let (tx_commit, rx_commit) = unbounded_channel::<Arc<Batch<SimpleTx<SimpleData>>>>();

        if id == 0 {
            let seen_clone = seen.clone();
            let tx_count_clone = tx_count.clone();
            tokio::spawn(async move {
                let mut rx = rx_commit;
                while let Some(batch) = rx.recv().await {
                    let mut s = seen_clone.lock().unwrap();
                    let mut c = tx_count_clone.lock().unwrap();
                    for tx in &batch.payload {
                        let key = (tx.client_id(), tx.nonce());
                        *s.entry(key).or_insert(0) += 1;
                        *c += 1;
                    }
                }
            });
        } else {
            let h = tokio::spawn(async move {
                let mut rx = rx_commit;
                while rx.recv().await.is_some() {}
            });
            drain_handles.push(h);
        }

        let exit_tx = Server::<SimpleTx<SimpleData>>::spawn(
            id,
            all_ids.clone(),
            crypto_keys[id].clone(),
            server_settings.clone(),
            tx_commit,
        )?;
        exit_senders.push(exit_tx);
    }

    let client_exit_tx = Stressor::<SimpleTx<SimpleData>>::spawn(CLIENT_ID, client_settings)?;

    // Warmup
    tokio::time::sleep(Duration::from_secs(WARMUP_SECS)).await;
    // Zero counters after warmup to get a clean measurement window.
    {
        let mut s = seen.lock().unwrap();
        s.clear();
        *tx_count.lock().unwrap() = 0;
    }

    let window_start = Instant::now();
    tokio::time::sleep(Duration::from_secs(OBSERVATION_SECS)).await;
    let elapsed = window_start.elapsed().as_secs_f64();

    // Shut down
    let _ = client_exit_tx.send(());
    for s in exit_senders {
        let _ = s.send(());
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = std::fs::remove_dir_all(&db_dir);

    let seen = seen.lock().unwrap();
    let total_committed = *tx_count.lock().unwrap();

    // --- Assertion 1: no (client, nonce) committed more than once ---
    let duplicates: Vec<_> = seen.iter().filter(|(_, &count)| count > 1).collect();
    assert!(
        duplicates.is_empty(),
        "Found {} (client, nonce) pairs committed more than once: {:?}",
        duplicates.len(),
        &duplicates[..duplicates.len().min(5)],
    );

    // --- Assertion 2: committed tx/s ≤ MAX_RATIO × offered ---
    let actual_tps = total_committed as f64 / elapsed;
    let offered_tps = OFFERED_TX_PER_SEC as f64;
    println!(
        "[nonce-dedup] offered={:.0} tx/s, committed={:.0} tx/s, ratio={:.2}",
        offered_tps,
        actual_tps,
        actual_tps / offered_tps,
    );
    assert!(
        actual_tps <= offered_tps * MAX_RATIO,
        "DP[Throughput] inflated: committed {:.0} tx/s > {:.0}× offered {:.0} tx/s. \
         Pre-fix this would be ~{}× for n={}.",
        actual_tps,
        MAX_RATIO,
        offered_tps,
        NUM_NODES,
        NUM_NODES,
    );

    Ok(())
}
