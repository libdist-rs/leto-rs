use std::fmt::{self, Debug, Display};

use network::Message;
use serde::{Deserialize, Serialize};

pub trait Data: Message {
    fn with_payload(data: &[u8]) -> Self;
}

/// Naive implementation of data
#[derive(Serialize, Deserialize, Clone)]
pub struct SimpleData {
    tx: Vec<u8>,
}

impl Data for SimpleData {
    fn with_payload(data: &[u8]) -> Self {
        Self { tx: data.to_vec() }
    }
}

impl Debug for SimpleData {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "{}", base64::encode(&self.tx))
    }
}

impl Display for SimpleData {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "{}", base64::encode(&self.tx))
    }
}
impl network::Message for SimpleData {}
