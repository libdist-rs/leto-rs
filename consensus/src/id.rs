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

impl TryFrom<String> for Id {
    type Error = anyhow::Error;
    // forward it to the &str implementation
    fn try_from(value: String) -> anyhow::Result<Self> {
        Self::try_from(value.as_str())
    }
}

impl<'a> TryFrom<&'a str> for Id {
    type Error = anyhow::Error;
    fn try_from(value: &'a str) -> anyhow::Result<Self> {
        let usize_val: usize = value.parse()?;
        Ok(usize_val.into())
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
