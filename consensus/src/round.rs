use serde::{Deserialize, Serialize};
use std::fmt::{self, Display};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
pub struct Round(usize);

impl Default for Round {
    fn default() -> Self {
        Self(10)
    }
}

impl From<usize> for Round {
    fn from(n: usize) -> Self {
        Self(n)
    }
}

impl Display for Round {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::ops::Sub<Self> for Round {
    type Output = Self;

    fn sub(
        self,
        rhs: Self,
    ) -> Self::Output {
        Self(self.0.sub(rhs.0))
    }
}

impl mempool::Round for Round {
    const MIN: Self = Self(0);
}

impl network::Message for Round {}
