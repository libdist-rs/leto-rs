use crate::smr::{SimpleData, SimpleTx};
use mempool::Batch;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc::UnboundedReceiver;

#[derive(Debug, Clone)]
pub struct LevelMetrics {
    pub target_rate: u64,
    pub actual_tps: f64,
    pub actual_bps: f64,
    pub batches_committed: u64,
    pub txs_committed: u64,
    pub bytes_committed: u64,
    pub duration_secs: f64,
    pub status: Status,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Ok,
    Degraded,
    Saturated,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Ok => write!(f, "OK"),
            Status::Degraded => write!(f, "DEGRADED"),
            Status::Saturated => write!(f, "SATURATED"),
        }
    }
}

#[derive(Default)]
struct MetricsInner {
    batches_committed: u64,
    txs_committed: u64,
    bytes_committed: u64,
    level_start: Option<Instant>,
}

pub struct MetricsCollector {
    inner: Arc<Mutex<MetricsInner>>,
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(MetricsInner::default())),
        }
    }

    /// Spawn a task that drains the commit channel and accumulates metrics.
    /// Only call this for one node (e.g., node 0) to avoid double-counting.
    pub fn register_commit_receiver(
        &self,
        mut rx: UnboundedReceiver<Arc<Batch<SimpleTx<SimpleData>>>>,
    ) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            while let Some(batch) = rx.recv().await {
                let tx_count = batch.payload.len() as u64;
                let byte_size = bincode::serialized_size(&*batch).unwrap_or(0);
                let mut state = inner.lock().unwrap();
                state.batches_committed += 1;
                state.txs_committed += tx_count;
                state.bytes_committed += byte_size;
            }
        });
    }

    /// Reset counters and start timing for a new level.
    pub fn reset_level(&self) {
        let mut state = self.inner.lock().unwrap();
        state.batches_committed = 0;
        state.txs_committed = 0;
        state.bytes_committed = 0;
        state.level_start = Some(Instant::now());
    }

    /// Snapshot the current metrics for the just-finished level.
    pub fn snapshot(&self, target_rate: u64) -> LevelMetrics {
        let state = self.inner.lock().unwrap();
        let elapsed = state
            .level_start
            .map(|s| s.elapsed().as_secs_f64())
            .unwrap_or(1.0)
            .max(0.001);

        let actual_tps = state.txs_committed as f64 / elapsed;
        let actual_bps = state.bytes_committed as f64 / elapsed;

        let ratio = if target_rate > 0 {
            actual_tps / target_rate as f64
        } else {
            1.0
        };

        let status = if ratio >= 0.9 {
            Status::Ok
        } else if ratio >= 0.5 {
            Status::Degraded
        } else {
            Status::Saturated
        };

        LevelMetrics {
            target_rate,
            actual_tps,
            actual_bps,
            batches_committed: state.batches_committed,
            txs_committed: state.txs_committed,
            bytes_committed: state.bytes_committed,
            duration_secs: elapsed,
            status,
        }
    }
}
