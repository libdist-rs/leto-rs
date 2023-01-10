use network::Message;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug, Display};
use base64::{Engine as _, engine::general_purpose};

pub trait Data: Message + Unpin {
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
        let encoded = general_purpose::STANDARD_NO_PAD
            .encode(&self.tx);
        write!(f, "{}", encoded)
    }
}

impl Display for SimpleData {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        let encoded = general_purpose::STANDARD_NO_PAD
            .encode(&self.tx);
        write!(f, "{}", &encoded)
    }
}
