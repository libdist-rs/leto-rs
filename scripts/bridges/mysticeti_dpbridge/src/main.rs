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

    /// Exact Prometheus series name carrying the cumulative
    /// committed-tx counter.
    ///
    /// Mysticeti increments `latency_s_count` (the count component of
    /// the end-to-end-latency histogram) once per committed tx, so it
    /// IS the committed-tx counter under that name.  Pass the literal
    /// series name (including any `_count` / `_total` suffix) — not
    /// the histogram base.
    #[arg(long, default_value = "latency_s_count")]
    throughput_metric: String,

    /// Metric name carrying the commit-latency histogram.
    /// Mysticeti uses `latency_s` (seconds; histogram).
    /// We read the p50 quantile bucket; pass --latency-quantile to
    /// override.
    #[arg(long, default_value = "latency_s")]
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

/// Find a counter, gauge, or histogram-count by name; returns the sum
/// across labels.
///
/// Counter / Gauge / Untyped are matched directly on the series name.
/// Histograms are matched on either:
///   - the bare histogram name (`latency_s`) → use the histogram's
///     sample count (i.e. count of observations, which is what
///     Mysticeti increments per committed tx);
///   - the explicit `<name>_count` series (the caller spelled it out)
///     → same: extract from any Histogram whose bare name matches the
///     `_count`-stripped requested name.
///
/// This handles prometheus-parse's API choice to bundle histogram
/// count/sum/buckets into a single `Value::Histogram` sample rather
/// than emitting them as separate counter series.
fn find_counter(scrape: &Scrape, name: &str) -> Option<f64> {
    let histogram_target = name.strip_suffix("_count").unwrap_or(name);
    let mut sum = 0.0f64;
    let mut found = false;
    for sample in &scrape.samples {
        match &sample.value {
            Value::Counter(v) | Value::Gauge(v) | Value::Untyped(v) => {
                if sample.metric == name {
                    sum += *v;
                    found = true;
                }
            }
            Value::Histogram(hist_samples) => {
                if sample.metric == histogram_target {
                    // Histogram total observation count = highest +Inf bucket
                    // OR equivalently the last cumulative bucket count.  The
                    // prometheus-parse HistogramCount type exposes the +Inf
                    // bucket as the last entry; sum() over that gives the count.
                    if let Some(last) = hist_samples.last() {
                        sum += last.count;
                        found = true;
                    }
                }
            }
            _ => {}
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
/// prometheus-parse bundles a histogram's buckets into a single
/// `Value::Histogram(Vec<HistogramCount>)` sample, where each entry
/// is `{less_than: f64, count: f64}` (cumulative count ≤ less_than).
/// We aggregate across all label combinations matching `metric`, walk
/// buckets in ascending order, and find the smallest bucket whose
/// cumulative count exceeds `quantile × total`.
///
/// Assumes the histogram unit is seconds; converts to ms in the return.
fn extract_latency_ms(scrape: &Scrape, metric: &str, quantile: f64) -> Option<f64> {
    // Aggregate buckets across all label sets that match this metric.
    // Map le → summed-count.
    let mut bucket_sum: std::collections::BTreeMap<u64, f64> = std::collections::BTreeMap::new();
    let mut total: f64 = 0.0;
    let mut found = false;
    for sample in &scrape.samples {
        if sample.metric != metric {
            continue;
        }
        if let Value::Histogram(entries) = &sample.value {
            found = true;
            for entry in entries {
                // BTreeMap doesn't support f64 keys; encode as bits.
                let key = entry.less_than.to_bits();
                *bucket_sum.entry(key).or_insert(0.0) += entry.count;
            }
            // The total observation count is the highest cumulative bucket.
            if let Some(last) = entries.last() {
                total += last.count;
            }
        }
    }
    if !found || total <= 0.0 {
        return None;
    }
    let target = quantile * total;
    for (key, count) in &bucket_sum {
        if *count >= target {
            let le = f64::from_bits(*key);
            return Some(le * 1000.0);
        }
    }
    // Fallback: top bucket
    bucket_sum
        .keys()
        .next_back()
        .map(|key| f64::from_bits(*key) * 1000.0)
}
