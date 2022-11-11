use network::Message;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Transaction<Data> {
    pub data: Data,
    /// Extra data for future extensions
    pub extra: Vec<u8>,
}

impl<Data> Message for Transaction<Data> where Data: Message {}
impl<Data> mempool::Transaction for Transaction<Data> where Data: Message {}

/// Naive implementation of data
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Data {
    tx: Vec<u8>,
}

impl network::Message for Data {}
