//! Mysticeti → `DP[…]` bridge.
//!
//! Scrapes a local Prometheus `/metrics` endpoint exposed by a running
//! Mysticeti node every `--interval-ms` and prints
//!
//!   DP[Throughput]: <f64>
//!   DP[Latency]: <f64>
//!
//! to stderr, matching the format the leto-rs orchestrator's parser
//! consumes (compatible with libapollo-rs `consensus::statistics` output).
//!
//! Run as a sidecar alongside each Mysticeti node; the orchestrator
//! concatenates this stderr with the Mysticeti binary's own log into
//! a single client.log for `orchestrator/parse.py`.
//!
//! No upstream Mysticeti modification required.

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use prometheus_parse::{Scrape, Value};
use std::time::{Duration, Instant};

/// Bridge config.
#[derive(Parser, Debug)]
#[command(
    name = "mysticeti-dpbridge",
    about = "Scrape Mysticeti's Prometheus and emit DP[…] on stderr"
)]
struct Args {
    /// URL of the Mysticeti node's Prometheus endpoint.
    #[arg(long, default_value = "http://127.0.0.1:1500/metrics")]
    metrics_url: String,

    /// Scrape interval in milliseconds.
    #[arg(long, default_value_t = 1000)]
    interval_ms: u64,

    /// Metric name carrying the cumulative committed-tx counter.
    /// Adjust if Mysticeti's metric is named differently in the
    /// build you pinned.
    #[arg(long, default_value = "committed_transactions_total")]
    throughput_metric: String,

    /// Metric name carrying the commit-latency histogram.
    /// We read the p50 quantile bucket; pass --latency-quantile to
    /// override.
    #[arg(long, default_value = "commit_latency_seconds")]
    latency_metric: String,

    /// Quantile to extract from the latency histogram (0.0–1.0).
    #[arg(long, default_value_t = 0.5)]
    latency_quantile: f64,

    /// Optional cap on bridge runtime in seconds; 0 = until SIGTERM.
    #[arg(long, default_value_t = 0)]
    max_secs: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;

    eprintln!(
        "mysticeti-dpbridge: scraping {} every {}ms (latency q={})",
        args.metrics_url, args.interval_ms, args.latency_quantile
    );

    let mut prev_count: Option<f64> = None;
    let mut prev_time: Option<Instant> = None;
    let started = Instant::now();

    loop {
        if args.max_secs > 0 && started.elapsed().as_secs() >= args.max_secs {
            eprintln!("mysticeti-dpbridge: max-secs reached, exiting");
            return Ok(());
        }

        match scrape(&client, &args.metrics_url) {
            Ok(scrape) => {
                let now = Instant::now();
                if let Some(throughput) = compute_throughput(
                    &scrape,
                    &args.throughput_metric,
                    prev_count,
                    prev_time,
                    now,
                ) {
                    eprintln!("DP[Throughput]: {throughput}");
                }
                if let Some(latency_ms) = extract_latency_ms(
                    &scrape,
                    &args.latency_metric,
                    args.latency_quantile,
                ) {
                    eprintln!("DP[Latency]: {latency_ms}");
                }
                // Update prev counters
                if let Some(cur) = find_counter(&scrape, &args.throughput_metric) {
                    prev_count = Some(cur);
                    prev_time = Some(now);
                }
            }
            Err(e) => {
                eprintln!("mysticeti-dpbridge: scrape error: {e:#}");
            }
        }

        std::thread::sleep(Duration::from_millis(args.interval_ms));
    }
}

fn scrape(client: &reqwest::blocking::Client, url: &str) -> Result<Scrape> {
    let body = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .text()?;
    let lines = body.lines().map(|s| Ok::<_, std::io::Error>(s.to_string()));
    Ok(Scrape::parse(lines).map_err(|e| anyhow!("prometheus parse: {e}"))?)
}

/// Find a counter or gauge by name; returns the sample sum across labels.
fn find_counter(scrape: &Scrape, name: &str) -> Option<f64> {
    let mut sum = 0.0f64;
    let mut found = false;
    for sample in &scrape.samples {
        if sample.metric == name {
            match sample.value {
                Value::Counter(v) | Value::Gauge(v) | Value::Untyped(v) => {
                    sum += v;
                    found = true;
                }
                _ => {}
            }
        }
    }
    if found {
        Some(sum)
    } else {
        None
    }
}

/// Compute tx/s from the delta of a cumulative counter over the
/// interval since the previous scrape.
fn compute_throughput(
    scrape: &Scrape,
    metric: &str,
    prev_count: Option<f64>,
    prev_time: Option<Instant>,
    now: Instant,
) -> Option<f64> {
    let current = find_counter(scrape, metric)?;
    let (prev_c, prev_t) = match (prev_count, prev_time) {
        (Some(c), Some(t)) => (c, t),
        _ => return None, // first scrape — no baseline
    };
    let delta = current - prev_c;
    let secs = now.duration_since(prev_t).as_secs_f64();
    if secs <= 0.0 {
        return None;
    }
    Some(delta / secs)
}

/// Extract a quantile from a Prometheus histogram, in milliseconds.
///
/// Mysticeti histograms expose `_bucket{le=...}` lines; we walk them
/// in ascending `le` order and find the smallest bucket whose
/// cumulative count exceeds `quantile × total`.
fn extract_latency_ms(scrape: &Scrape, metric: &str, quantile: f64) -> Option<f64> {
    let bucket_name = format!("{metric}_bucket");
    let count_name = format!("{metric}_count");
    let total = find_counter(scrape, &count_name)?;
    if total <= 0.0 {
        return None;
    }
    let mut buckets: Vec<(f64, f64)> = Vec::new();
    for sample in &scrape.samples {
        if sample.metric != bucket_name {
            continue;
        }
        let le_str = sample.labels.get("le")?;
        let le: f64 = le_str.parse().ok()?;
        let count = match sample.value {
            Value::Counter(v) | Value::Gauge(v) | Value::Untyped(v) => v,
            _ => continue,
        };
        buckets.push((le, count));
    }
    if buckets.is_empty() {
        return None;
    }
    buckets.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let target = quantile * total;
    for (le, count) in &buckets {
        if *count >= target {
            // Assume histogram unit is seconds; convert to ms.
            return Some(le * 1000.0);
        }
    }
    // Fallback: top bucket
    buckets.last().map(|(le, _)| le * 1000.0)
}
