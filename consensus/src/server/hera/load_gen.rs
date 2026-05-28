use crate::Id;
/// Hera internal load generator (Mysticeti-style).
///
/// Spawns a tokio task that emits `TPS` transactions per second into the
/// batcher channel.  The load is evenly distributed across 100 ms windows
/// (10 bursts/s).  Each tx embeds a u64 nanosecond timestamp in its payload
/// for latency measurement.
///
/// Used by `HeraServer::spawn` when the `TPS` env var is > 0.
/// The concrete `Tx` type is `node::SimpleTx<node::SimpleData>` — fixed at
/// binary level, not library level.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{interval, Duration};

/// Spawn the internal tx generator for a Hera node.
///
/// - `my_id`: this node's id, embedded in each generated tx as `source`.
/// - `tps`: target transactions per second.
/// - `tx_to_batcher`: sender into the batcher's `(Tx, usize)` channel.
/// - `make_tx`: closure that produces a `Tx` given `(my_id, nonce, now_ns)`.
///   The closure must be `Send + 'static`.
/// - `my_height`: the data actor's current proposed-tip height (updated after
///   each successful self-propose).
/// - `my_committed_height`: the data actor's committed watermark for this node
///   (updated in `on_commit_emit`).
/// - `max_chain_lead`: soft pause threshold — generation is suppressed this
///   tick when `my_height - my_committed_height >= max_chain_lead`. Matches the
///   data actor's hard cap so txs are not generated into blocks that will be
///   dropped anyway.
pub fn spawn<Tx, F>(
    my_id: Id,
    tps: usize,
    tx_to_batcher: UnboundedSender<(Tx, usize)>,
    make_tx: F,
    my_height: Arc<AtomicU64>,
    my_committed_height: Arc<AtomicU64>,
    max_chain_lead: u64,
) where
    Tx: serde::Serialize + Send + 'static,
    F: Fn(Id, u64, u128) -> Tx + Send + 'static,
{
    if tps == 0 {
        return;
    }
    // Round up so we don't starve slow windows.
    let txs_per_100ms = tps.saturating_add(9) / 10;

    tokio::spawn(async move {
        let mut tick = interval(Duration::from_millis(100));
        let mut nonce: u64 = 0;
        loop {
            tick.tick().await;

            // Soft pause gate: if this node's data sub-chain is already at
            // (or beyond) the chain-lead cap, skip this window so the generator
            // does not produce txs that the data actor will discard anyway.
            // Re-checked every 100 ms; resumes automatically as commits advance
            // my_committed_height and the lead drops below max_chain_lead.
            let height = my_height.load(Ordering::Relaxed);
            let committed = my_committed_height.load(Ordering::Relaxed);
            if height.saturating_sub(committed) >= max_chain_lead {
                continue;
            }

            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            for _ in 0..txs_per_100ms {
                let tx = make_tx(my_id, nonce, now_ns);
                let size = bincode::serialized_size(&tx).unwrap_or(0) as usize;
                if tx_to_batcher.send((tx, size)).is_err() {
                    return; // channel closed — server shutting down
                }
                nonce += 1;
            }
        }
    });
}
