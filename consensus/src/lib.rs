pub mod client;
pub mod server;
pub mod types;

mod id;

pub use id::*;

mod round;
pub use round::*;

use anyhow::Result;
use std::net::{SocketAddr, SocketAddrV4};

pub fn to_socket_address(
    ip_str: &str,
    port: u16,
) -> Result<SocketAddr> {
    let addr = SocketAddrV4::new(ip_str.parse()?, port);
    Ok(addr.into())
}

pub type Transaction = types::Transaction<types::Data>;
