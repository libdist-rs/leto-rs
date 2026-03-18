mod config;
mod harness;
mod load_driver;
mod metrics;
mod report;
mod smr;

use anyhow::Result;
use clap::Parser;
use config::StressTestConfig;
use harness::NodeHarness;
use load_driver::LoadDriver;
use metrics::{LevelMetrics, MetricsCollector, Status};
use std::time::Duration;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .format_timestamp_millis()
    .init();

    let config = StressTestConfig::parse();
    let load_levels = config.load_levels();

    println!("===============================================================");
    println!("  LETO BFT STRESS TEST");
    println!("===============================================================");
    println!("  Nodes: {}, Faults: {}", config.num_nodes, config.num_crash_faults);
    println!("  Load levels: {} -> {} tx/s (step {})", config.load_start, config.load_max, config.load_step);
    println!("  Duration/level: {}s, Warmup: {}s", config.duration_per_level_secs, config.warmup_secs);
    println!("===============================================================");
    println!();

    // 1. Spawn consensus nodes
    println!("[*] Spawning {} consensus nodes...", config.num_nodes - config.num_crash_faults);
    let metrics = MetricsCollector::new();
    let harness = NodeHarness::spawn_nodes(&config, &metrics)?;

    // 2. Wait for nodes to connect
    let convergence_secs = (config.num_nodes as u64).max(5);
    println!("[*] Waiting {}s for node convergence...", convergence_secs);
    tokio::time::sleep(Duration::from_secs(convergence_secs)).await;
    println!("[*] Convergence period complete.");
    println!();

    // 3. Run load levels
    let mut results: Vec<LevelMetrics> = Vec::new();
    let mut consecutive_saturated = 0u32;

    for (level_idx, &target_rate) in load_levels.iter().enumerate() {
        println!(
            "[*] Level {} / {}: target {} tx/s",
            level_idx + 1,
            load_levels.len(),
            target_rate,
        );

        // Start stressors
        let driver = LoadDriver::start_load(&config, target_rate)?;

        // Warmup
        if config.warmup_secs > 0 {
            println!("    Warming up for {}s...", config.warmup_secs);
            tokio::time::sleep(Duration::from_secs(config.warmup_secs)).await;
        }

        // Reset metrics and measure
        metrics.reset_level();
        println!("    Measuring for {}s...", config.duration_per_level_secs);
        tokio::time::sleep(Duration::from_secs(config.duration_per_level_secs)).await;

        // Snapshot metrics
        let level_metrics = metrics.snapshot(target_rate);
        println!(
            "    Result: {:.0} tx/s ({:.0} B/s), {} batches, status: {}",
            level_metrics.actual_tps,
            level_metrics.actual_bps,
            level_metrics.batches_committed,
            level_metrics.status,
        );

        // Stop stressors
        driver.stop();
        // Brief pause between levels for cleanup
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Track saturation
        if level_metrics.status == Status::Saturated {
            consecutive_saturated += 1;
        } else {
            consecutive_saturated = 0;
        }

        results.push(level_metrics);

        // Early exit if saturated for 2 consecutive levels
        if consecutive_saturated >= 2 {
            println!();
            println!("[!] System saturated for 2 consecutive levels. Stopping early.");
            break;
        }
    }

    // 4. Shutdown
    println!();
    println!("[*] Shutting down nodes...");
    harness.shutdown();
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 5. Print report
    report::print_report(&config, &results);

    Ok(())
}
