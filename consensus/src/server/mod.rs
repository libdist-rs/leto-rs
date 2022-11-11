mod server;
pub use server::*;

mod settings;
pub use settings::*;

mod consensus_handler;
pub use consensus_handler::*;

#[cfg(test)]
mod test;

use crate::{types, Id, Round};
pub type ProtocolMsg = types::ProtocolMsg<Id, types::Data, Round>;
