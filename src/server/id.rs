use serde::{Deserialize, Serialize};
use std::fmt::{self, Display};

#[derive(Debug, Serialize, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Copy, Clone)]
pub struct Id(usize);

impl network::Message for Id {}
impl network::Identifier for Id {}

impl From<usize> for Id {
    fn from(i: usize) -> Self {
        Self(i)
    }
}

impl Display for Id {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        self.0.fmt(f)
    }
}
