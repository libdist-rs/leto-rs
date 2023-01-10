use consensus::types::Transaction;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug, Display};
use base64::{Engine as _, engine::general_purpose};

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
        let encoded = general_purpose::STANDARD_NO_PAD.encode(&self.extra);
        write!(f, "Tx [{:?}, {}]", self.data, &encoded)
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
        let encoded = general_purpose::STANDARD_NO_PAD.encode(&self.extra);
        write!(f, "Tx [{:?}, {}]", self.data, &encoded)
    }
}

impl<Data> Transaction for SimpleTx<Data> where Data: crate::Data {}
