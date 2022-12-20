use consensus::types::Transaction;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug, Display};

#[derive(Serialize, Deserialize, Clone)]
pub struct SimpleTx<Data> {
    pub data: Data,
    /// Extra data for future extensions
    pub extra: Vec<u8>,
}

impl<Data> Debug for SimpleTx<Data>
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

impl<Data> Display for SimpleTx<Data>
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

impl<Data> Transaction for SimpleTx<Data> where Data: crate::Data {}
