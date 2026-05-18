/// Zeus stress-test binary.
///
/// Shares all modules with the Leto stress-test via path-referenced includes.
#[path = "../config.rs"]
mod config;
#[path = "../load_driver.rs"]
mod load_driver;
#[path = "../metrics.rs"]
mod metrics;
#[path = "../report.rs"]
mod report;
#[path = "../smr.rs"]
mod smr;
#[path = "../zeus_harness.rs"]
mod zeus_harness;

use anyhow::Result;
use clap::Parser;
use config::StressTestConfig;
use load_driver::LoadDriver;
use metrics::{LevelMetrics, MetricsCollector, Status};
use std::time::Duration;
use zeus_harness::ZeusNodeHarness;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let config = StressTestConfig::parse();
    let load_levels = config.load_levels();

    println!("===============================================================");
    println!("  ZEUS BFT STRESS TEST");
    println!("===============================================================");
    println!(
        "  Nodes: {}, Faults: {}",
        config.num_nodes, config.num_crash_faults
    );
    println!(
        "  Load levels: {} -> {} tx/s (step {})",
        config.load_start, config.load_max, config.load_step
    );
    println!(
        "  Duration/level: {}s, Warmup: {}s",
        config.duration_per_level_secs, config.warmup_secs
    );
    println!("===============================================================");
    println!();

    println!(
        "[*] Spawning {} Zeus consensus nodes...",
        config.num_nodes - config.num_crash_faults
    );
    let metrics = MetricsCollector::new();
    let harness = ZeusNodeHarness::spawn_nodes(&config, &metrics)?;

    let convergence_secs = (config.num_nodes as u64).max(5);
    println!("[*] Waiting {}s for node convergence...", convergence_secs);
    tokio::time::sleep(Duration::from_secs(convergence_secs)).await;
    println!("[*] Convergence period complete.");
    println!();

    let mut results: Vec<LevelMetrics> = Vec::new();
    let mut consecutive_saturated = 0u32;

    for (level_idx, &target_rate) in load_levels.iter().enumerate() {
        println!(
            "[*] Level {} / {}: target {} tx/s",
            level_idx + 1,
            load_levels.len(),
            target_rate,
        );

        // Zeus: pre-seed the eleader id so the stressor skips the WhoIsEleader query.
        // eleader(epoch=1, n) = 1 % n.
        let eleader_id = (1usize % config.num_nodes) as consensus::Id;
        let client_mode = consensus::client::ClientMode::ZeusEleaderOnly {
            eleader_id: Some(eleader_id),
        };
        let driver = LoadDriver::start_load(&config, target_rate, client_mode)?;

        if config.warmup_secs > 0 {
            println!("    Warming up for {}s...", config.warmup_secs);
            tokio::time::sleep(Duration::from_secs(config.warmup_secs)).await;
        }

        metrics.reset_level();
        println!("    Measuring for {}s...", config.duration_per_level_secs);
        tokio::time::sleep(Duration::from_secs(config.duration_per_level_secs)).await;

        let level_metrics = metrics.snapshot(target_rate);
        println!(
            "    Result: {:.0} tx/s ({:.0} B/s), {} batches, status: {}",
            level_metrics.actual_tps,
            level_metrics.actual_bps,
            level_metrics.batches_committed,
            level_metrics.status,
        );

        driver.stop();
        tokio::time::sleep(Duration::from_secs(2)).await;

        if level_metrics.status == Status::Saturated {
            consecutive_saturated += 1;
        } else {
            consecutive_saturated = 0;
        }
        results.push(level_metrics);

        if consecutive_saturated >= 2 {
            println!();
            println!("[!] System saturated for 2 consecutive levels. Stopping early.");
            break;
        }
    }

    println!();
    println!("[*] Shutting down Zeus nodes...");
    harness.shutdown();
    tokio::time::sleep(Duration::from_secs(2)).await;

    report::print_report(&config, &results);

    Ok(())
}
