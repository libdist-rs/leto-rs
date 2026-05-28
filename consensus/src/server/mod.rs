use std::sync::OnceLock;

mod core;
pub use self::core::*;

mod chain_db;
pub use chain_db::*;

/// Cancel-handler GC depth (in rounds).  When a protocol's sig-chain is at
/// round `r`, cancel handlers for messages sent at rounds `< r -
/// GC_DEPTH_ROUNDS` are dropped, which signals the per-peer
/// `tcp-reliable-sender` connection task to skip those messages (via
/// `cancel_handler.is_closed()` check in libnet's `connection.rs:200-207`).
///
/// Initialized to `4 * n` at the first protocol `spawn`.  `4 * n` gives the
/// protocol ample time for slow-but-alive peers to catch up while bounding
/// per-peer cancel-handler memory at ~`4n * messages_per_round * peers *
/// sizeof(oneshot::Receiver)` ≈ KB-scale for typical committees.
///
/// The `OnceLock` is intentional: a single binary runs a single protocol
/// instance with a fixed `n`, so the first-set value is correct for the
/// lifetime of the process.  Subsequent calls to `init_gc_depth_rounds` are
/// no-ops.
pub static GC_DEPTH_ROUNDS: OnceLock<u64> = OnceLock::new();

pub fn init_gc_depth_rounds(n: usize) {
    let _ = GC_DEPTH_ROUNDS.set((4 * n) as u64);
}

/// Read the GC depth, falling back to 16 if `init_gc_depth_rounds` has not
/// been called yet (defensive — should never happen because every protocol's
/// `spawn` initializes it).
pub fn gc_depth_rounds() -> u64 {
    *GC_DEPTH_ROUNDS.get().unwrap_or(&16)
}

mod settings;
pub use settings::*;

mod consensus_handler;
pub use consensus_handler::*;

mod rr_batcher;
pub use rr_batcher::*;

mod tx_pool;
pub use tx_pool::*;

mod leto;
pub use leto::*;

pub mod zeus;
pub use zeus::{Zeus, ZeusServer};

pub mod hera;
pub use hera::{Hera, HeraServer};
