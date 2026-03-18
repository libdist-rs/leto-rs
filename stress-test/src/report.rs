use crate::config::StressTestConfig;
use crate::metrics::{LevelMetrics, Status};

pub fn print_report(config: &StressTestConfig, results: &[LevelMetrics]) {
    println!();
    println!("===============================================================");
    println!("  LETO BFT STRESS TEST RESULTS");
    println!("===============================================================");
    println!(
        "  Nodes: {}  |  Faults: {}  |  Tx Size: {} B  |  Batch: {} B",
        config.num_nodes, config.num_crash_faults, config.tx_size, config.batch_size,
    );
    println!(
        "  Delay: {} ms  |  Duration/Level: {} s  |  Clients: {}",
        config.delay_in_ms, config.duration_per_level_secs, config.num_clients,
    );
    println!("---------------------------------------------------------------");
    println!(
        "  {:<5} | {:>11} | {:>11} | {:>12} | {:>8} | {}",
        "Level", "Target tx/s", "Actual tx/s", "Bytes/s", "Batches", "Status"
    );
    println!(
        "  {:-<5}-+-{:-<11}-+-{:-<11}-+-{:-<12}-+-{:-<8}-+-{:-<10}",
        "", "", "", "", "", ""
    );

    for (i, m) in results.iter().enumerate() {
        println!(
            "  {:<5} | {:>11} | {:>11.0} | {:>12.0} | {:>8} | {}",
            i + 1,
            m.target_rate,
            m.actual_tps,
            m.actual_bps,
            m.batches_committed,
            m.status,
        );
    }

    println!("===============================================================");

    // Find breaking point
    let last_ok = results
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| m.status == Status::Ok);

    if let Some((idx, m)) = last_ok {
        println!(
            "  BREAKING POINT: ~{} tx/s (level {} last healthy)",
            m.target_rate,
            idx + 1,
        );
    } else if !results.is_empty() {
        println!("  BREAKING POINT: system was never healthy at offered rates");
    }

    // Peak throughput
    if let Some(peak) = results
        .iter()
        .max_by(|a, b| a.actual_tps.partial_cmp(&b.actual_tps).unwrap())
    {
        println!(
            "  Peak throughput: {:.0} tx/s at offered rate {} tx/s",
            peak.actual_tps, peak.target_rate,
        );
    }

    println!("===============================================================");
    println!();
}
