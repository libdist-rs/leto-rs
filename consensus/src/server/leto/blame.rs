use super::Leto;
use crate::{
    types::{self, Signature},
    Id, Round,
};
use anyhow::Result;
use log::debug;

impl<Tx> Leto<Tx> {
    pub fn handle_blame(
        &mut self,
        blame_round: Round,
        auth: Signature<Id, Round>,
    ) -> Result<()>
    where
        Tx: types::Transaction,
    {
        debug!(
            "Got a blame for round {} from {}",
            blame_round,
            auth.get_id()
        );
        todo!();
    }
}
