//! Persistent-connection transport for hera's consensus plane, ported from
//! mysticeti to replace libnet's reconnect-storm-prone `TcpSimpleSender`.
//! See `network.rs` for the rationale and `handle.rs` for the facade.

mod handle;
mod network;

pub use handle::HeraNet;
