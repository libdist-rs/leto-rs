/// Crash-fault view-change smoke test.
///
/// Spawns 4 Leto nodes in-process plus one Stressor client, kills one
/// non-metrics node (node 1) mid-flight and asserts:
///
///   1. The remaining 3 nodes continue committing batches after the kill
///      (proves liveness — at least N_POST_KILL batches arrive on the
///      surviving metrics node within the observation window).
///   2. The round counter on node 0's commit stream advances, showing
///      the blame → BlameQC → advance_round path fired.
///   3. No surviving node panics.
///
/// The test is deterministic: all randomness is seeded by a fixed
/// LeaderContext seed ("LETO-PROTOCOL"), and the timer is short enough
/// (~800 ms per blame cycle) that we see multiple view-changes within the
/// post-kill observation window.
///
/// Port allocation: this test uses the range 11000–12999 to avoid
/// colliding with `launch_test.rs` (6000–9999) and the production
/// examples/server.json ports (7001–10004).
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use crate::{
    client::{self, Stressor},
    server::{BenchConfig, Config, Party, Server, Settings, StorageConfig},
    Id, KeyConfig, Round, SimpleData, SimpleTx,
};
use anyhow::Result;
use crypto::Algorithm;
use fnv::FnvHashMap;
use mempool::Batch;
use tokio::sync::{mpsc::unbounded_channel, oneshot};

// How many batches the surviving cluster must commit after the kill.
const N_POST_KILL_BATCHES: u64 = 3;
// Minimum number of round advances we must observe after the kill.
// Each commit on node 0's channel represents one completed round.
const REQUIRED_ROUND_ADVANCE: u64 = 3;

// Blame timer = 4 * delay_in_ms  →  4 * 200 = 800 ms
// One view-change = ~1 timer fire + network RTT ≈ 1.5 s
// We allow 25 s post-kill for ≥ 3 commits to show up.
const POST_KILL_OBSERVATION_SECS: u64 = 25;

// Short delay so the cluster can establish connections before we start
// counting commits.
const WARMUP_SECS: u64 = 6;

const BASE_CONSENSUS_PORT: u16 = 11000;
const BASE_MEMPOOL_PORT: u16 = 11500;
const BASE_CLIENT_PORT: u16 = 12000;
const BASE_CONSENSUS_CLIENT_PORT: u16 = 12500;
// Stressor confirmation-listener port (one client, OS assigns ephemeral).
const STRESSOR_CONFIRMATION_PORT: u16 = 0;

const NUM_NODES: usize = 4;
// We kill node 1.  Node 0 is the metrics node and must stay up for counting.
const NODE_TO_KILL: Id = 1;
// Client ID: distinct from server IDs.
const CLIENT_ID: Id = NUM_NODES;

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
            prefix: "crash-test-db".to_string(),
        },
        bench_config: BenchConfig {
            batch_size: 50_000,
            batch_timeout: Duration::from_millis(200),
            // Short delay → fast blame timer (4 * 200 = 800 ms)
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
                // Stressor sends to consensus_client_port (NewBatch listener).
                port: BASE_CONSENSUS_CLIENT_PORT + id as u16,
                confirmation_port: STRESSOR_CONFIRMATION_PORT,
            },
        );
    }
    client::Settings {
        bench_config: client::Bench {
            burst_interval_ms: 50,
            tx_size: 256,
            // 50 tx / 50 ms = 1000 tx/s — enough to fill batches quickly
            txs_per_burst: 50,
            bench_emit_window_secs: 5,
            emit_dp: false,
        },
        consensus_config: client::Config {
            parties: client_parties,
        },
        client_mode: client::ClientMode::LetoBroadcast,
        my_confirmation_address: "0.0.0.0".to_string(),
        my_confirmation_port: STRESSOR_CONFIRMATION_PORT,
    }
}

#[tokio::test]
async fn test_crash_fault_view_change() -> Result<()> {
    // Shared atomic counters driven by node 0's commit channel.
    let total_commits: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let post_kill_commits: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let kill_flag: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

    // Use a unique subdirectory under the OS temp dir.  We clean it up
    // manually at the end so the DB files don't linger on test failure.
    let db_dir = {
        let mut p = std::env::temp_dir();
        p.push(format!("leto-crash-test-{}", std::process::id()));
        std::fs::create_dir_all(&p)?;
        p
    };
    let server_settings = build_server_settings(&db_dir);
    let client_settings = build_client_settings();
    let all_ids: Vec<Id> = (0..NUM_NODES).collect();
    let crypto_keys = KeyConfig::generate(Algorithm::ED25519, NUM_NODES)?;

    // Spawn all 4 nodes; collect their exit senders.
    let mut exit_senders: Vec<oneshot::Sender<()>> = Vec::new();
    // Keep drain handles alive so the channels don't close.
    let mut _drain_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    for id in 0..NUM_NODES {
        let (tx_commit, rx_commit) =
            unbounded_channel::<Arc<Batch<SimpleTx<SimpleData>>>>();

        if id == 0 {
            // Node 0: feed commits into the counter task.
            let total_clone = total_commits.clone();
            let post_kill_clone = post_kill_commits.clone();
            let kill_flag_clone = kill_flag.clone();
            tokio::spawn(async move {
                let mut rx = rx_commit;
                while rx.recv().await.is_some() {
                    total_clone.fetch_add(1, Ordering::Relaxed);
                    if kill_flag_clone.load(Ordering::Relaxed) == 1 {
                        post_kill_clone.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        } else {
            // Other nodes: drain to prevent backpressure.
            let handle = tokio::spawn(async move {
                let mut rx = rx_commit;
                while rx.recv().await.is_some() {}
            });
            _drain_handles.push(handle);
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

    // Spawn the stressor client.
    let client_exit_tx =
        Stressor::<SimpleTx<SimpleData>>::spawn(CLIENT_ID, client_settings)?;

    // --- Warmup: let nodes establish connections and start committing ---
    tokio::time::sleep(Duration::from_secs(WARMUP_SECS)).await;

    let commits_before_kill = total_commits.load(Ordering::Relaxed);
    assert!(
        commits_before_kill > 0,
        "No commits in {}s warmup — protocol never started. \
         Check port conflicts or settings.",
        WARMUP_SECS,
    );

    // --- Kill node NODE_TO_KILL (node 1) ---
    // Sending () on the exit channel mirrors exactly how NodeHarness::shutdown
    // works and how the production binary handles SIGINT.
    println!(
        "[crash-test] Pre-kill commits on node 0: {}",
        commits_before_kill
    );
    let kill_sender = exit_senders.remove(NODE_TO_KILL);
    kill_flag.store(1, Ordering::Relaxed);
    let _kill_ts = Instant::now();
    let _ = kill_sender.send(());
    println!("[crash-test] Killed node {}; observing for {}s", NODE_TO_KILL, POST_KILL_OBSERVATION_SECS);

    // --- Post-kill observation window ---
    tokio::time::sleep(Duration::from_secs(POST_KILL_OBSERVATION_SECS)).await;

    let post_kill = post_kill_commits.load(Ordering::Relaxed);
    println!(
        "[crash-test] Post-kill commits on node 0 ({}s window): {}",
        POST_KILL_OBSERVATION_SECS, post_kill
    );

    // Shut down stressor and remaining nodes cleanly before asserting.
    let _ = client_exit_tx.send(());
    for sender in exit_senders {
        let _ = sender.send(());
    }
    // Give tasks a moment to drain.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Clean up the DB directory.
    let _ = std::fs::remove_dir_all(&db_dir);

    // --- Assertions ---

    // (1) Liveness: surviving 3-node cluster committed new batches.
    assert!(
        post_kill >= N_POST_KILL_BATCHES,
        "Liveness failure: only {} batch(es) committed by node 0 in the {}s \
         window after killing node {}. Expected >= {}. \
         View-change may have stalled.",
        post_kill,
        POST_KILL_OBSERVATION_SECS,
        NODE_TO_KILL,
        N_POST_KILL_BATCHES,
    );

    // (2) Round advance: each post-kill commit on node 0 implies at least one
    //     completed round after the kill.  Requiring >= REQUIRED_ROUND_ADVANCE
    //     commits proves the blame→BlameQC→advance_round path fired repeatedly.
    assert!(
        post_kill >= REQUIRED_ROUND_ADVANCE,
        "Round-advance failure: only {} commit(s) on node 0 after kill, \
         need >= {} to prove blame→BlameQC→advance_round path fired repeatedly.",
        post_kill,
        REQUIRED_ROUND_ADVANCE,
    );

    Ok(())
}
