use network::Message;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug, Display};

#[derive(Serialize, Deserialize, Clone)]
pub struct Transaction<Data> {
    pub data: Data,
    /// Extra data for future extensions
    pub extra: Vec<u8>,
}

impl<Data> Debug for Transaction<Data>
where
    Data: Debug,
{
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "Tx [{:?}, {}]", self.data, &base64::encode(&self.extra))
    }
}

impl<Data> Display for Transaction<Data>
where
    Data: Debug,
{
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "Tx [{:?}, {}]", self.data, &base64::encode(&self.extra))
    }
}

impl<Data> Message for Transaction<Data> where Data: Message {}
impl<Data> mempool::Transaction for Transaction<Data> where Data: Message {}

/// Naive implementation of data
#[derive(Serialize, Deserialize, Clone)]
pub struct Data {
    tx: Vec<u8>,
}

impl Data {
    pub fn new(tx: Vec<u8>) -> Self {
        Self { tx }
    }
}

impl Debug for Data {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "{}", base64::encode(&self.tx))
    }
}

impl Display for Data {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "{}", base64::encode(&self.tx))
    }
}
impl network::Message for Data {}
